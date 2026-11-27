//! Authentication, origin/CSRF validation, rate limiting, and audit logging for `mare.http.serve`.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use subtle::ConstantTimeEq;

use super::{ServeBody, plain_status_response};

const TOKEN_BYTES: usize = 32;
const TOKEN_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const TOKEN_LOCK_POLL: Duration = Duration::from_millis(15);
const SESSION_COOKIE_NAME: &str = "mare_session";
const MAX_COOKIE_SESSIONS: usize = 1024;
const MAX_FAILURE_ENTRIES: usize = 1024;
const FAILURE_ENTRY_TTL: Duration = Duration::from_secs(300);
const MAX_CONSECUTIVE_AUTH_FAILURES: u32 = 5;
const AUTH_LOCKOUT_DURATION: Duration = Duration::from_secs(30);

pub enum TokenSource {
    Disabled,
    Static(String),
    RuntimeFile { path: PathBuf },
}

struct FailureState {
    count: u32,
    last_seen: Instant,
    locked_until: Option<Instant>,
}

#[derive(Default)]
struct AuthInner {
    last_token: Option<String>,
    sessions: VecDeque<String>,
    failures: HashMap<IpAddr, FailureState>,
    aggregate_failure: Option<FailureState>,
}

pub struct AuthState {
    source: TokenSource,
    cookie_name: String,
    secure_cookie: bool,
    inner: Mutex<AuthInner>,
}

pub enum AuthOutcome {
    Allowed,
    Redirect(hyper::Response<ServeBody>),
    Denied(hyper::Response<ServeBody>),
}

pub(super) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// Generate a 32-byte OS-CSPRNG token encoded as unpadded base64url.
pub fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    rand::fill(&mut buf[..]);
    URL_SAFE_NO_PAD.encode(buf)
}

fn ensure_private_permissions(path: &Path, unix_mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path).map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
        if meta.permissions().mode() & 0o077 != 0 {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(unix_mode))
                .map_err(|e| format!("cannot restrict {}: {e}", path.display()))?;
        }
    }
    #[cfg(windows)]
    {
        let _ = unix_mode;
        if mare_shared::private::check(path).is_err() {
            mare_shared::private::restrict(path).map_err(|e| format!("cannot restrict {}: {e}", path.display()))?;
        }
    }
    Ok(())
}

fn ensure_private_parent(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        mare_shared::private::create_dir_all(parent)
            .map_err(|e| format!("cannot create token dir {}: {e}", parent.display()))?;
        ensure_private_permissions(parent, 0o700)?;
    }
    Ok(())
}

fn prepare_and_lock(
    path: &Path,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<mare_shared::private::LockGuard, String> {
    let lock_path = path.with_file_name(".token.lock");
    if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
        return Err(format!("cancelled waiting for token lock {}", lock_path.display()));
    }
    ensure_private_parent(path)?;
    mare_shared::private::lock_file_blocking(&lock_path, Some(timeout), TOKEN_LOCK_POLL, false, cancel).map_err(|e| {
        match e {
            mare_shared::private::LockError::Cancelled => {
                format!("cancelled waiting for token lock {}", lock_path.display())
            }
            mare_shared::private::LockError::TimedOut => {
                format!("timed out waiting for token lock {} after {}ms", lock_path.display(), timeout.as_millis())
            }
            mare_shared::private::LockError::Io(io) => {
                format!("cannot open/lock token lock {}: {io}", lock_path.display())
            }
        }
    })
}

/// Whether `token` is a canonical unpadded 32-byte base64url string (43 chars).
pub(super) fn is_valid_runtime_token(token: &str) -> bool {
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(token) else {
        return false;
    };
    decoded.len() == TOKEN_BYTES && URL_SAFE_NO_PAD.encode(&decoded) == token
}

/// Load the persisted host token at `path`, or atomically create a new `0600` 32-byte random token
/// under a cross-process lock: read if present, write only if absent.
pub fn ensure_host_token_at(path: &Path) -> Result<String, String> {
    ensure_host_token_at_with(path, TOKEN_LOCK_TIMEOUT, None)
}

pub(super) fn ensure_host_token_at_with(
    path: &Path,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<String, String> {
    let _lock = prepare_and_lock(path, timeout, cancel)?;
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            let trimmed = raw.trim();
            if !is_valid_runtime_token(trimmed) {
                return Err(format!(
                    "corrupt host token file {}: expected canonical 32-byte base64url token",
                    path.display()
                ));
            }
            ensure_private_permissions(path, 0o600)?;
            return Ok(trimmed.to_string());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("cannot read token file {}: {e}", path.display())),
    }
    let token = generate_token();
    mare_shared::paths::write_atomic(path, token.as_bytes(), mare_shared::paths::WriteMode::Private)
        .map_err(|e| format!("cannot write token file {}: {e}", path.display()))?;
    Ok(token)
}

fn token_rotation_channel() -> &'static tokio::sync::watch::Sender<u64> {
    static TX: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> = std::sync::OnceLock::new();
    TX.get_or_init(|| tokio::sync::watch::channel(0).0)
}

/// Subscribe to in-process host token rotations so idle WebSocket connections can close promptly.
pub fn subscribe_token_rotation() -> tokio::sync::watch::Receiver<u64> {
    token_rotation_channel().subscribe()
}

/// Generate and atomically persist a new `0600` 32-byte random host token at `path`.
pub fn rotate_host_token_at(path: &Path) -> Result<String, String> {
    rotate_host_token_at_with(path, TOKEN_LOCK_TIMEOUT, None)
}

pub(super) fn rotate_host_token_at_with(
    path: &Path,
    timeout: Duration,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<String, String> {
    let _lock = prepare_and_lock(path, timeout, cancel)?;
    let token = generate_token();
    mare_shared::paths::write_atomic(path, token.as_bytes(), mare_shared::paths::WriteMode::Private)
        .map_err(|e| format!("cannot write token file {}: {e}", path.display()))?;
    token_rotation_channel().send_modify(|seq| *seq = seq.wrapping_add(1));
    Ok(token)
}

/// Rotate the default host runtime token file (`<state>/host/token`), gated by `fs_host_protect`.
pub fn rotate_host_token() -> Result<(), String> {
    rotate_host_token_with_partial(false)
}

/// Rotate the default host runtime token file (`<state>/host/token`), allowing explicit
/// `fs_host_protect` partial acceptance when the active sandbox backend cannot hide it.
pub fn rotate_host_token_with_partial(accepted_partial: bool) -> Result<(), String> {
    crate::ossandbox::protected::producer_gate(accepted_partial)?;
    rotate_host_token_at(&mare_shared::paths::host_token_file()).map(|_| ())
}

fn read_runtime_token(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| is_valid_runtime_token(s))
}

impl AuthState {
    pub fn new(source: TokenSource) -> Result<Self, String> {
        Self::new_with_cancel(source, None)
    }

    pub(super) fn new_with_cancel(
        source: TokenSource,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<Self, String> {
        let initial = match &source {
            TokenSource::Disabled => None,
            TokenSource::Static(t) => Some(t.clone()),
            TokenSource::RuntimeFile { path } => Some(ensure_host_token_at_with(path, TOKEN_LOCK_TIMEOUT, cancel)?),
        };
        Ok(Self {
            source,
            cookie_name: SESSION_COOKIE_NAME.to_string(),
            secure_cookie: false,
            inner: Mutex::new(AuthInner { last_token: initial, ..Default::default() }),
        })
    }

    pub fn set_cookie_name(&mut self, name: String) {
        self.cookie_name = name;
    }

    pub fn set_secure_cookie(&mut self, secure: bool) {
        self.secure_cookie = secure;
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self.source, TokenSource::Disabled)
    }

    /// Return the current expected token string (for printing the initial startup banner only).
    pub fn current_token(&self) -> Option<String> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        self.current_expected_token(&mut inner)
    }

    /// Resolve the active expected token and clear in-memory cookie sessions if rotated.
    fn current_expected_token(&self, inner: &mut AuthInner) -> Option<String> {
        match &self.source {
            TokenSource::Disabled => None,
            TokenSource::Static(t) => Some(t.clone()),
            TokenSource::RuntimeFile { path } => {
                let current = read_runtime_token(path);
                if current != inner.last_token {
                    inner.sessions.clear();
                    inner.last_token = current.clone();
                }
                current
            }
        }
    }

    pub fn authenticate(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        query: &[(String, String)],
        is_ws_upgrade: bool,
        peer_addr: SocketAddr,
    ) -> AuthOutcome {
        self.authenticate_at_with_token(method, path, headers, query, is_ws_upgrade, peer_addr, Instant::now()).0
    }

    /// Authenticate a request and atomically return the exact token validated under the lock
    /// (`Some(token)` on [`AuthOutcome::Allowed`] when auth is enabled, `None` otherwise).
    pub fn authenticate_with_token(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        query: &[(String, String)],
        is_ws_upgrade: bool,
        peer_addr: SocketAddr,
    ) -> (AuthOutcome, Option<String>) {
        self.authenticate_at_with_token(method, path, headers, query, is_ws_upgrade, peer_addr, Instant::now())
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn authenticate_at(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        query: &[(String, String)],
        is_ws_upgrade: bool,
        peer_addr: SocketAddr,
        now: Instant,
    ) -> AuthOutcome {
        self.authenticate_at_with_token(method, path, headers, query, is_ws_upgrade, peer_addr, now).0
    }

    #[allow(clippy::too_many_arguments)]
    fn authenticate_at_with_token(
        &self,
        method: &Method,
        path: &str,
        headers: &HeaderMap,
        query: &[(String, String)],
        is_ws_upgrade: bool,
        peer_addr: SocketAddr,
        now: Instant,
    ) -> (AuthOutcome, Option<String>) {
        if !self.is_enabled() {
            return (AuthOutcome::Allowed, None);
        }

        let ip = peer_addr.ip();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let inner = &mut *inner;
        prune_expired_failures(&mut inner.failures, &mut inner.aggregate_failure, now);

        let locked_until = inner.failures.get(&ip).and_then(|st| st.locked_until).or_else(|| {
            (!inner.failures.contains_key(&ip) && inner.failures.len() >= MAX_FAILURE_ENTRIES)
                .then(|| inner.aggregate_failure.as_ref().and_then(|st| st.locked_until))
                .flatten()
        });
        if let Some(until) = locked_until
            && now < until
        {
            let retry_after = until.saturating_duration_since(now).as_secs().max(1);
            tracing::warn!(
                target: "mare::http_auth",
                remote = %peer_addr,
                method = %method,
                path = %path,
                retry_after,
                "http auth rate limited"
            );
            let mut resp = plain_status_response(StatusCode::TOO_MANY_REQUESTS, "too many failed auth attempts");
            if let Ok(val) = HeaderValue::from_str(&retry_after.to_string()) {
                resp.headers_mut().insert(http::header::RETRY_AFTER, val);
            }
            return (AuthOutcome::Denied(resp), None);
        }

        let Some(expected) = self.current_expected_token(inner) else {
            tracing::warn!(
                target: "mare::http_auth",
                remote = %peer_addr,
                method = %method,
                path = %path,
                reason = "missing_runtime_token",
                "http auth failed"
            );
            return (AuthOutcome::Denied(unauthorized_response()), None);
        };

        // 1. Authorization: Bearer <token>
        let mut authed_via: Option<&'static str> = None;
        if let Some(auth) = headers.get(http::header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
            let trimmed = auth.trim();
            if trimmed.len() > 7
                && trimmed[..7].eq_ignore_ascii_case("bearer ")
                && ct_eq(trimmed[7..].trim().as_bytes(), expected.as_bytes())
            {
                authed_via = Some("bearer");
            }
        }

        // 2. x-mare-token: <token>
        if authed_via.is_none()
            && let Some(tok) = headers.get("x-mare-token").and_then(|v| v.to_str().ok())
            && ct_eq(tok.trim().as_bytes(), expected.as_bytes())
        {
            authed_via = Some("header");
        }

        // 3. In-memory HttpOnly session cookie
        if authed_via.is_none()
            && let Some(session_id) = extract_session_cookie(headers, &self.cookie_name)
            && inner.sessions.iter().any(|s| ct_eq(s.as_bytes(), session_id.as_bytes()))
        {
            authed_via = Some("cookie");
        }

        if let Some(via) = authed_via {
            inner.failures.remove(&ip);
            if *method == Method::GET && !is_ws_upgrade && query.iter().any(|(k, _)| k == "token") {
                let location = strip_token_from_location(path, query);
                tracing::info!(
                    target: "mare::http_auth",
                    remote = %peer_addr,
                    method = %method,
                    path = %path,
                    auth_method = %format!("{via}_token_strip"),
                    "http auth succeeded"
                );
                return (AuthOutcome::Redirect(build_token_strip_redirect(&location, None, self.secure_cookie)), None);
            }
            tracing::info!(
                target: "mare::http_auth",
                remote = %peer_addr,
                method = %method,
                path = %path,
                auth_method = via,
                "http auth succeeded"
            );
            return (AuthOutcome::Allowed, Some(expected));
        }

        // 4. ?token= query parameter
        let token_params: Vec<&str> = query.iter().filter(|(k, _)| k == "token").map(|(_, v)| v.as_str()).collect();
        if !token_params.is_empty() {
            if token_params.len() == 1 && ct_eq(token_params[0].as_bytes(), expected.as_bytes()) {
                if is_ws_upgrade {
                    inner.failures.remove(&ip);
                    tracing::info!(
                        target: "mare::http_auth",
                        remote = %peer_addr,
                        method = %method,
                        path = %path,
                        auth_method = "ws_query",
                        "http auth succeeded"
                    );
                    return (AuthOutcome::Allowed, Some(expected));
                }
                if *method == Method::GET {
                    inner.failures.remove(&ip);
                    let session_id = generate_token();
                    if inner.sessions.len() >= MAX_COOKIE_SESSIONS {
                        inner.sessions.pop_front();
                    }
                    inner.sessions.push_back(session_id.clone());
                    let location = strip_token_from_location(path, query);
                    tracing::info!(
                        target: "mare::http_auth",
                        remote = %peer_addr,
                        method = %method,
                        path = %path,
                        auth_method = "query_cookie_exchange",
                        "http auth succeeded"
                    );
                    return (
                        AuthOutcome::Redirect(build_token_strip_redirect(
                            &location,
                            Some((&self.cookie_name, &session_id)),
                            self.secure_cookie,
                        )),
                        None,
                    );
                }
            }
            record_failure(&mut inner.failures, &mut inner.aggregate_failure, ip, now);
            tracing::warn!(
                target: "mare::http_auth",
                remote = %peer_addr,
                method = %method,
                path = %path,
                reason = "invalid_query_token",
                "http auth failed"
            );
            return (AuthOutcome::Denied(unauthorized_response()), None);
        }

        record_failure(&mut inner.failures, &mut inner.aggregate_failure, ip, now);
        tracing::warn!(
            target: "mare::http_auth",
            remote = %peer_addr,
            method = %method,
            path = %path,
            reason = "missing_or_invalid_token",
            "http auth failed"
        );
        (AuthOutcome::Denied(unauthorized_response()), None)
    }
}

fn failure_expired(st: &FailureState, now: Instant) -> bool {
    match st.locked_until {
        Some(until) => now >= until,
        None => now.saturating_duration_since(st.last_seen) >= FAILURE_ENTRY_TTL,
    }
}

fn prune_expired_failures(
    failures: &mut HashMap<IpAddr, FailureState>,
    aggregate: &mut Option<FailureState>,
    now: Instant,
) {
    failures.retain(|_, st| !failure_expired(st, now));
    if aggregate.as_ref().is_some_and(|st| failure_expired(st, now)) {
        *aggregate = None;
    }
}

fn bump_failure(st: &mut FailureState, now: Instant) {
    if st.locked_until.is_some_and(|until| now >= until) {
        st.count = 0;
        st.locked_until = None;
    }
    st.last_seen = now;
    st.count = st.count.saturating_add(1);
    if st.count >= MAX_CONSECUTIVE_AUTH_FAILURES {
        st.locked_until = Some(now + AUTH_LOCKOUT_DURATION);
    }
}

fn record_failure(
    failures: &mut HashMap<IpAddr, FailureState>,
    aggregate: &mut Option<FailureState>,
    ip: IpAddr,
    now: Instant,
) {
    prune_expired_failures(failures, aggregate, now);
    if failures.contains_key(&ip) || failures.len() < MAX_FAILURE_ENTRIES {
        let entry = failures.entry(ip).or_insert(FailureState { count: 0, last_seen: now, locked_until: None });
        bump_failure(entry, now);
    } else {
        let entry = aggregate.get_or_insert(FailureState { count: 0, last_seen: now, locked_until: None });
        bump_failure(entry, now);
    }
}

fn unauthorized_response() -> hyper::Response<ServeBody> {
    let mut resp = plain_status_response(StatusCode::UNAUTHORIZED, "unauthorized");
    resp.headers_mut().insert(http::header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

fn extract_session_cookie<'a>(headers: &'a HeaderMap, cookie_name: &str) -> Option<&'a str> {
    for val in headers.get_all(http::header::COOKIE) {
        let Ok(s) = val.to_str() else { continue };
        for segment in s.split(';') {
            let Some((name, value)) = segment.split_once('=') else { continue };
            if name.trim() == cookie_name {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed);
                }
            }
        }
    }
    None
}

fn strip_token_from_location(path: &str, query: &[(String, String)]) -> String {
    let remaining: Vec<(&str, &str)> =
        query.iter().filter(|(k, _)| k != "token").map(|(k, v)| (k.as_str(), v.as_str())).collect();
    if remaining.is_empty() {
        return path.to_string();
    }
    let qs = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(remaining).finish();
    format!("{path}?{qs}")
}

fn build_token_strip_redirect(
    location: &str,
    session: Option<(&str, &str)>,
    secure: bool,
) -> hyper::Response<ServeBody> {
    let mut builder = hyper::Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(http::header::CACHE_CONTROL, "no-store")
        .header("referrer-policy", "no-referrer");
    if let Ok(loc) = HeaderValue::from_str(location) {
        builder = builder.header(http::header::LOCATION, loc);
    }
    if let Some((cookie_name, session_id)) = session {
        let secure_attr = if secure { "; Secure" } else { "" };
        let cookie = format!("{cookie_name}={session_id}; Path=/; HttpOnly; SameSite=Strict{secure_attr}");
        if let Ok(sc) = HeaderValue::from_str(&cookie) {
            builder = builder.header(http::header::SET_COOKIE, sc);
        }
    }
    builder.body(super::empty_body()).unwrap_or_else(|_| plain_status_response(StatusCode::SEE_OTHER, ""))
}

fn normalize_origin(raw: &str) -> Option<(String, String, u16)> {
    let parsed = url::Url::parse(raw.trim()).ok()?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || (parsed.path() != "/" && !parsed.path().is_empty())
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let port = parsed.port_or_known_default()?;
    Some((scheme.to_string(), host, port))
}

fn is_same_authority_origin(origin: &str, host_header: &str, expected_scheme: Option<&str>) -> bool {
    let Some((scheme, origin_host, origin_port)) = normalize_origin(origin) else {
        return false;
    };
    if let Some(exp) = expected_scheme
        && !scheme.eq_ignore_ascii_case(exp)
    {
        return false;
    }
    let Ok(host_url) = url::Url::parse(&format!("{scheme}://{}", host_header.trim())) else {
        return false;
    };
    let Some(host_str) = host_url.host_str() else {
        return false;
    };
    let Some(host_port) = host_url.port_or_known_default() else {
        return false;
    };
    origin_host == host_str.to_ascii_lowercase() && origin_port == host_port
}

fn origin_matches_allowed(origin: &str, allowed: &[String]) -> bool {
    let Some(norm_origin) = normalize_origin(origin) else {
        return false;
    };
    allowed.iter().any(|entry| normalize_origin(entry).is_some_and(|norm_allowed| norm_allowed == norm_origin))
}

/// Validate `Sec-Fetch-Site` and `Origin` headers against the validated `Host` authority
/// . When `expected_scheme` is `Some`,
/// same-authority origins must also match that scheme (`"http"` vs `"https"`).
pub fn validate_request_origin(
    headers: &HeaderMap,
    host_header: &str,
    allowed_origins: &[String],
    peer_addr: SocketAddr,
    path: &str,
    expected_scheme: Option<&str>,
) -> Option<hyper::Response<ServeBody>> {
    for val in headers.get_all("sec-fetch-site") {
        if let Ok(s) = val.to_str()
            && s.trim().eq_ignore_ascii_case("cross-site")
        {
            tracing::warn!(
                target: "mare::http_auth",
                remote = %peer_addr,
                path = %path,
                reason = "sec_fetch_site_cross_site",
                "http request rejected by origin fence"
            );
            return Some(plain_status_response(StatusCode::FORBIDDEN, "cross-site request forbidden"));
        }
    }

    let mut origin_iter = headers.get_all(http::header::ORIGIN).iter();
    let origin_val = origin_iter.next()?;
    if origin_iter.next().is_some() {
        tracing::warn!(
            target: "mare::http_auth",
            remote = %peer_addr,
            path = %path,
            reason = "duplicate_origin",
            "http request rejected by origin fence"
        );
        return Some(plain_status_response(StatusCode::BAD_REQUEST, "duplicate Origin header"));
    }
    let Ok(origin_str) = origin_val.to_str() else {
        tracing::warn!(
            target: "mare::http_auth",
            remote = %peer_addr,
            path = %path,
            reason = "invalid_origin",
            "http request rejected by origin fence"
        );
        return Some(plain_status_response(StatusCode::FORBIDDEN, "invalid Origin header"));
    };
    let trimmed = origin_str.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("null") {
        tracing::warn!(
            target: "mare::http_auth",
            remote = %peer_addr,
            path = %path,
            reason = "opaque_origin",
            "http request rejected by origin fence"
        );
        return Some(plain_status_response(StatusCode::FORBIDDEN, "forbidden Origin header"));
    }
    if origin_matches_allowed(trimmed, allowed_origins)
        || is_same_authority_origin(trimmed, host_header, expected_scheme)
    {
        return None;
    }
    tracing::warn!(
        target: "mare::http_auth",
        remote = %peer_addr,
        path = %path,
        reason = "origin_mismatch",
        "http request rejected by origin fence"
    );
    Some(plain_status_response(StatusCode::FORBIDDEN, "forbidden Origin header"))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::*;

    fn status_of(outcome: AuthOutcome) -> StatusCode {
        match outcome {
            AuthOutcome::Allowed => StatusCode::OK,
            AuthOutcome::Redirect(r) | AuthOutcome::Denied(r) => r.status(),
        }
    }

    #[test]
    fn failure_table_caps_aggregate_rate_limits_and_expires_entries() {
        let auth = AuthState::new(TokenSource::Static("secret".into())).unwrap();
        let headers = HeaderMap::new();
        let t0 = Instant::now();

        // 1. Fill the per-IP failure table up to MAX_FAILURE_ENTRIES (1024) distinct IPs.
        for i in 0..MAX_FAILURE_ENTRIES {
            let ip = IpAddr::V4(Ipv4Addr::from(0x0a00_0001u32 + i as u32));
            let addr = SocketAddr::new(ip, 12345);
            assert_eq!(
                status_of(auth.authenticate_at(&Method::GET, "/", &headers, &[], false, addr, t0)),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(auth.inner.lock().unwrap().failures.len(), MAX_FAILURE_ENTRIES);

        // 2. Overflow IPs share the aggregate failure bucket and trigger 429 once threshold is reached.
        let t1 = t0 + Duration::from_secs(1);
        let overflow_ip1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 12345);
        let overflow_ip2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 12345);
        for _ in 0..MAX_CONSECUTIVE_AUTH_FAILURES {
            assert_eq!(
                status_of(auth.authenticate_at(&Method::GET, "/", &headers, &[], false, overflow_ip1, t1)),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(auth.inner.lock().unwrap().failures.len(), MAX_FAILURE_ENTRIES);

        // A different overflow IP is now rate-limited by the aggregate bucket.
        let denied = auth.authenticate_at(&Method::GET, "/", &headers, &[], false, overflow_ip2, t1);
        match denied {
            AuthOutcome::Denied(resp) => {
                assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
                assert!(resp.headers().contains_key(http::header::RETRY_AFTER));
            }
            _ => panic!("expected 429 Too Many Requests from aggregate bucket"),
        }

        // 3. After AUTH_LOCKOUT_DURATION (30s) but before FAILURE_ENTRY_TTL (300s),
        //    aggregate lockout clears while the 1024 entries remain.
        let t2 = t1 + AUTH_LOCKOUT_DURATION + Duration::from_secs(1);
        assert_eq!(
            status_of(auth.authenticate_at(&Method::GET, "/", &headers, &[], false, overflow_ip2, t2)),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(auth.inner.lock().unwrap().failures.len(), MAX_FAILURE_ENTRIES);

        // 4. After FAILURE_ENTRY_TTL (300s), expired entries are pruned so new IPs get their own buckets.
        let t3 = t2 + FAILURE_ENTRY_TTL + Duration::from_secs(1);
        for _ in 0..MAX_CONSECUTIVE_AUTH_FAILURES {
            assert_eq!(
                status_of(auth.authenticate_at(&Method::GET, "/", &headers, &[], false, overflow_ip1, t3)),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(auth.inner.lock().unwrap().failures.len(), 1);
        // overflow_ip1 is locked out, but overflow_ip2 is NOT locked out because they no longer share aggregate.
        assert_eq!(
            status_of(auth.authenticate_at(&Method::GET, "/", &headers, &[], false, overflow_ip1, t3)),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            status_of(auth.authenticate_at(&Method::GET, "/", &headers, &[], false, overflow_ip2, t3)),
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn ensure_and_rotate_respect_lock_timeout_and_cancel_while_contended() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("host").join("token");
        let existing = ensure_host_token_at(&token_path).unwrap();

        let lock_path = token_path.with_file_name(".token.lock");
        let held = mare_shared::private::open(
            std::fs::OpenOptions::new().create(true).truncate(false).write(true),
            &lock_path,
        )
        .unwrap();
        held.lock().unwrap();

        let err_ensure = ensure_host_token_at_with(&token_path, Duration::from_millis(40), None).unwrap_err();
        assert!(err_ensure.contains("timed out waiting for token lock"), "unexpected ensure error: {err_ensure}");

        let err_rotate = rotate_host_token_at_with(&token_path, Duration::from_millis(40), None).unwrap_err();
        assert!(err_rotate.contains("timed out waiting for token lock"), "unexpected rotate error: {err_rotate}");
        assert_eq!(std::fs::read_to_string(&token_path).unwrap(), existing);

        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let uncreated_path = dir.path().join("uncreated_host").join("token");
        let started = Instant::now();
        let err_cancel = ensure_host_token_at_with(&uncreated_path, Duration::from_secs(5), Some(&cancel)).unwrap_err();
        assert!(err_cancel.contains("cancelled waiting for token lock"), "unexpected cancel error: {err_cancel}");
        assert!(started.elapsed() < Duration::from_secs(1), "pre-cancelled lock wait should return immediately");
        assert!(!uncreated_path.parent().unwrap().exists(), "pre-cancelled call must not create parent directory");

        let cancel_mid = tokio_util::sync::CancellationToken::new();
        let trigger = cancel_mid.clone();
        let contended = mare_shared::private::testing::watch_contended(&lock_path);
        let waiter_path = token_path.clone();
        let waiter = std::thread::spawn(move || {
            rotate_host_token_at_with(&waiter_path, Duration::from_secs(5), Some(&cancel_mid))
        });
        assert!(
            contended.wait_blocking(Duration::from_secs(5)),
            "waiter did not enter contended WouldBlock loop within 5s"
        );
        trigger.cancel();
        let err_mid = waiter.join().unwrap().unwrap_err();
        assert!(err_mid.contains("cancelled waiting for token lock"), "unexpected mid-wait cancel error: {err_mid}");

        // Cancelling the waiter must not release the holder's lock.
        let probe = mare_shared::private::open(
            std::fs::OpenOptions::new().create(true).truncate(false).write(true),
            &lock_path,
        )
        .unwrap();
        assert!(matches!(probe.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        drop(probe);
        let still_held = ensure_host_token_at_with(&token_path, Duration::from_millis(30), None).unwrap_err();
        assert!(still_held.contains("timed out waiting for token lock"), "unexpected error while held: {still_held}");
        assert_eq!(std::fs::read_to_string(&token_path).unwrap(), existing);

        // Once the holder releases, subsequent callers acquire the lock immediately.
        drop(held);
        let recovered = ensure_host_token_at_with(&token_path, Duration::from_millis(50), None).unwrap();
        assert_eq!(recovered, existing);
        let rotated = rotate_host_token_at_with(&token_path, Duration::from_millis(50), None).unwrap();
        assert_ne!(rotated, existing);
    }

    #[test]
    fn authenticate_with_token_returns_exact_validated_token() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("host").join("token");
        let auth = AuthState::new(TokenSource::RuntimeFile { path: token_path.clone() }).unwrap();
        let tok1 = auth.current_token().unwrap();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 4444);

        let mut headers = HeaderMap::new();
        headers.insert(http::header::AUTHORIZATION, HeaderValue::from_str(&format!("Bearer {tok1}")).unwrap());
        let (outcome, validated) = auth.authenticate_with_token(&Method::GET, "/rpc", &headers, &[], true, addr);
        assert!(matches!(outcome, AuthOutcome::Allowed));
        assert_eq!(validated.as_deref(), Some(tok1.as_str()));

        // Subscribe before rotating and verify rotation wakes subscribers immediately and updates token.
        let rx = subscribe_token_rotation();
        let tok2 = rotate_host_token_at(&token_path).unwrap();
        assert_ne!(tok1, tok2);
        assert!(rx.has_changed().unwrap(), "in-process rotation must notify subscribers");

        // Old token is now denied with no validated token returned; new token returns tok2 atomically.
        let (denied_out, denied_tok) = auth.authenticate_with_token(&Method::GET, "/rpc", &headers, &[], true, addr);
        assert!(matches!(denied_out, AuthOutcome::Denied(_)));
        assert_eq!(denied_tok, None);

        let mut headers2 = HeaderMap::new();
        headers2.insert("x-mare-token", HeaderValue::from_str(&tok2).unwrap());
        let (ok2, validated2) = auth.authenticate_with_token(&Method::GET, "/rpc", &headers2, &[], true, addr);
        assert!(matches!(ok2, AuthOutcome::Allowed));
        assert_eq!(validated2.as_deref(), Some(tok2.as_str()));

        // Disabled auth returns Allowed with None.
        let disabled = AuthState::new(TokenSource::Disabled).unwrap();
        let (ok_dis, tok_dis) =
            disabled.authenticate_with_token(&Method::GET, "/rpc", &HeaderMap::new(), &[], true, addr);
        assert!(matches!(ok_dis, AuthOutcome::Allowed));
        assert_eq!(tok_dis, None);
    }
}
