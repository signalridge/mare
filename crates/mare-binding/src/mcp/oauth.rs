//! OAuth for streamable-HTTP MCP servers: the file-backed credential store and the loopback
//! browser login (Apache-2.0 / MIT).
//!
//! Credentials are keyed by server name **and** a hash of its URL:
//! when a server's URL changes the old token is never sent to the new
//! address.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationMetadata, AuthorizationMetadataSource, AuthorizationRequest,
    AuthorizationSession, CredentialRefreshGuard, CredentialStore, StoredCredentials,
};
use sha2::Digest;
use tokio::sync::{Mutex, OwnedMutexGuard, mpsc};

/// OAuth options of one server (`oauth = { ... }` in the config).
#[derive(Debug, Clone, Default)]
pub(crate) struct OAuthOpts {
    pub scopes: Vec<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    pub client_name: Option<String>,
    pub callback_port: Option<u16>,
    pub auth_server_metadata_url: Option<String>,
}

impl OAuthOpts {
    pub(crate) fn from_json(v: Option<&serde_json::Value>) -> Self {
        let Some(v) = v.and_then(|v| v.as_object()) else { return Self::default() };
        let s = |key: &str| v.get(key).and_then(|x| x.as_str()).filter(|x| !x.is_empty()).map(str::to_string);
        let scopes = match v.get("scopes") {
            Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
            Some(serde_json::Value::String(s)) => s.split_whitespace().map(str::to_string).collect(),
            _ => vec![],
        };
        Self {
            scopes,
            client_id: s("client_id"),
            client_secret: s("client_secret"),
            client_name: s("client_name"),
            callback_port: v.get("callback_port").and_then(|x| x.as_u64()).and_then(|p| u16::try_from(p).ok()),
            auth_server_metadata_url: s("auth_server_metadata_url"),
        }
    }
}

fn safe_name(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// `<state_dir>/mcp-oauth/<name>-<sha256(url)[..16]>.json`
pub(crate) fn oauth_file_in(state_dir: &Path, key: &str, url: &str) -> PathBuf {
    let h = sha2::Sha256::digest(url.trim_end_matches('/').as_bytes());
    let hex: String = h.iter().take(8).map(|b| format!("{b:02x}")).collect();
    state_dir.join("mcp-oauth").join(format!("{}-{hex}.json", safe_name(key)))
}

/// `<state>/mcp-oauth/<name>-<sha256(url)[..16]>.json`
pub fn oauth_file(key: &str, url: &str) -> PathBuf {
    oauth_file_in(&mare_shared::paths::state_dir(), key, url)
}

/// Forget the tokens of `key` at `url` under `state_dir`. True when any existed.
pub(crate) fn logout_in(state_dir: &Path, key: &str, url: Option<&str>) -> std::io::Result<bool> {
    let Some(u) = url else {
        return Ok(false);
    };
    match std::fs::remove_file(oauth_file_in(state_dir, key, u)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Forget the tokens of `key` at `url`. True when any existed.
pub(crate) fn logout(key: &str, url: Option<&str>) -> std::io::Result<bool> {
    logout_in(&mare_shared::paths::state_dir(), key, url)
}

const REFRESH_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const REFRESH_LOCK_POLL: Duration = Duration::from_millis(15);
const DISCOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(10);

fn oauth_lock_path(cred_path: &Path) -> PathBuf {
    let mut os = cred_path.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

async fn in_process_refresh_lock(cred_path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();
    let key = mare_shared::private::canonicalize_lock_key(cred_path.to_path_buf()).await;
    let map = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
    guard.retain(|_, v| v.strong_count() > 0);
    if let Some(existing) = guard.get(&key).and_then(Weak::upgrade) {
        return existing;
    }
    let lock = Arc::new(Mutex::new(()));
    guard.insert(key, Arc::downgrade(&lock));
    lock
}

struct RefreshLockGuard {
    // Drop OS file lock before releasing the in-process mutex.
    _file: mare_shared::private::LockGuard,
    _in_process: OwnedMutexGuard<()>,
}

pub struct FileCredentialStore(pub PathBuf);

impl FileCredentialStore {
    /// Save credentials using the production `CredentialStore::save` path.
    pub async fn save_bearer(&self, access_token: &str) -> Result<(), String> {
        let creds = serde_json::from_value::<StoredCredentials>(serde_json::json!({
            "client_id": "test-client",
            "token_response": {
                "access_token": access_token,
                "token_type": "bearer"
            }
        }))
        .map_err(|e| e.to_string())?;
        self.save(creds).await.map_err(|e| e.to_string())
    }

    /// Load credentials using the production `CredentialStore::load` path.
    pub async fn load_present(&self) -> Result<bool, String> {
        self.load().await.map(|c| c.is_some()).map_err(|e| e.to_string())
    }
}

impl FileCredentialStore {
    pub(crate) async fn acquire_refresh_guard_with(
        &self,
        timeout: Duration,
    ) -> Result<CredentialRefreshGuard, AuthError> {
        let lock_path = oauth_lock_path(&self.0);
        let deadline = tokio::time::Instant::now() + timeout;
        let mem_lock = in_process_refresh_lock(&self.0).await;
        let in_process = tokio::time::timeout_at(deadline, mem_lock.lock_owned()).await.map_err(|_| {
            AuthError::CredentialStoreError(format!(
                "timed out waiting for in-process OAuth refresh lock {}",
                lock_path.display()
            ))
        })?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let file_guard =
            mare_shared::private::lock_file_async(&lock_path, Some(remaining), REFRESH_LOCK_POLL, true, None)
                .await
                .map_err(|err| match err {
                    mare_shared::private::LockError::TimedOut => AuthError::CredentialStoreError(format!(
                        "timed out waiting for OAuth refresh lock {} after {}ms",
                        lock_path.display(),
                        timeout.as_millis()
                    )),
                    mare_shared::private::LockError::Cancelled => AuthError::CredentialStoreError(format!(
                        "OAuth refresh lock cancelled for {}",
                        lock_path.display()
                    )),
                    mare_shared::private::LockError::Io(e) => AuthError::CredentialStoreError(format!(
                        "cannot lock OAuth refresh lock {}: {e}",
                        lock_path.display()
                    )),
                })?;
        Ok(CredentialRefreshGuard::new(RefreshLockGuard { _file: file_guard, _in_process: in_process }))
    }
}

#[async_trait::async_trait]
impl CredentialStore for FileCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        match tokio::fs::read(&self.0).await {
            Ok(b) => serde_json::from_slice(&b).map(Some).map_err(|e| AuthError::InternalError(e.to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(AuthError::InternalError(e.to_string())),
        }
    }
    async fn save(&self, c: StoredCredentials) -> Result<(), AuthError> {
        let data = serde_json::to_vec_pretty(&c).map_err(|e| AuthError::InternalError(e.to_string()))?;
        let path = self.0.clone();
        tokio::task::spawn_blocking(move || write_private(&path, &data))
            .await
            .map_err(|e| AuthError::InternalError(e.to_string()))?
            .map_err(|e| AuthError::InternalError(e.to_string()))
    }
    async fn clear(&self) -> Result<(), AuthError> {
        match tokio::fs::remove_file(&self.0).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(AuthError::InternalError(e.to_string())),
        }
    }
    async fn acquire_refresh_guard(&self) -> Result<Option<CredentialRefreshGuard>, AuthError> {
        self.acquire_refresh_guard_with(REFRESH_LOCK_TIMEOUT).await.map(Some)
    }
}

/// Atomic 0600 write (random-named temp, 0600 from creation → fsync → rename), in a 0700 directory
/// (Windows: private DACLs, `mare_shared::private`)
pub(crate) fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(std::path::Path::new("."));
    mare_shared::private::create_dir_all(dir)?;
    mare_shared::paths::write_atomic(path, data, mare_shared::paths::WriteMode::Private)
}

fn is_loopback(url: &str) -> bool {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(crate::http::is_loopback_host)).unwrap_or(false)
}

/// The metadata document at a configured `auth_server_metadata_url` (used instead of discovery;
/// HTTPS unless loopback).
async fn fetch_metadata(url: &str, enforce_offline: bool) -> Result<AuthorizationMetadata, String> {
    check_endpoint("auth_server_metadata_url", url)?;
    if enforce_offline {
        crate::http::check_offline_url(url)?;
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        crate::http::check_offline_proxy_for_url_with(url, None, &env)?;
    }
    let http = crate::http::build_client(crate::http::base_builder()?)?;
    let resp = http
        .get(url)
        .send()
        .await
        .map_err(|e| mare_shared::redact::redact_str(&format!("authorization server metadata: {e}")))?;
    if !resp.status().is_success() {
        return Err(format!("authorization server metadata: HTTP {}", resp.status()));
    }
    resp.json::<AuthorizationMetadata>()
        .await
        .map_err(|e| mare_shared::redact::redact_str(&format!("authorization server metadata: {e}")))
}

/// One auth-param of a `WWW-Authenticate` value (`error="insufficient_scope"`, `scope=a`), by
/// case-insensitive name; quoted or bare token values.
pub(crate) fn auth_param(header: &str, name: &str) -> Option<String> {
    let mut rest = header;
    while !rest.is_empty() {
        let start = rest.find(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-')?;
        rest = &rest[start..];
        let key_end = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(rest.len());
        let key = &rest[..key_end];
        let after = rest[key_end..].trim_start();
        let Some(after) = after.strip_prefix('=') else {
            // the scheme name (`Bearer`) or a token68: skip it
            rest = &rest[key_end..];
            continue;
        };
        let after = after.trim_start();
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            out.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => out.push(c),
                }
            }
            (out, &q[end.min(q.len())..])
        } else {
            let end = after.find(|c: char| c == ',' || c.is_whitespace()).unwrap_or(after.len());
            (after[..end].to_string(), &after[end..])
        };
        if key.eq_ignore_ascii_case(name) {
            return Some(value);
        }
        rest = tail;
    }
    None
}

fn same_origin(a: &url::Url, b: &url::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str().map(str::to_ascii_lowercase) == b.host_str().map(str::to_ascii_lowercase)
        && a.port_or_known_default() == b.port_or_known_default()
}

/// An endpoint named by authorization server metadata (or the configured metadata URL) must be
/// `https`, or `http` on a loopback host: authorization codes, tokens and client secrets travel
/// to it, and the MCP authorization specification requires HTTPS for authorization server
/// endpoints.
pub(crate) fn check_endpoint(field: &str, raw: &str) -> Result<(), String> {
    let shown = || mare_shared::redact::redact_str(raw);
    let url = url::Url::parse(raw).map_err(|_| format!("{field} is not a URL: {}", shown()))?;
    let loopback = url.host_str().is_some_and(crate::http::is_loopback_host);
    match url.scheme() {
        "https" if url.host_str().is_some() => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(format!("{field} must use https (plain http only on loopback): {}", shown())),
    }
}

/// Refuse authorization server metadata mare cannot use safely. PKCE: OAuth 2.1 and the MCP
/// authorization specification require clients to refuse an authorization server that does not
/// advertise `code_challenge_methods_supported` (RFC 8414), and mare only sends `S256`
/// (RFC 7636); rmcp merely warns when the field is absent. Endpoints: see [`check_endpoint`];
/// rmcp checks no scheme here.
pub(crate) fn check_metadata(meta: &AuthorizationMetadata) -> Result<(), String> {
    match &meta.code_challenge_methods_supported {
        Some(methods) if methods.iter().any(|m| m == "S256") => {}
        Some(_) => return Err("the authorization server does not support PKCE S256".into()),
        None => {
            return Err("the authorization server metadata does not advertise PKCE S256 \
                 (code_challenge_methods_supported is missing)"
                .into());
        }
    }
    check_endpoint("authorization_endpoint", &meta.authorization_endpoint)?;
    check_endpoint("token_endpoint", &meta.token_endpoint)?;
    if let Some(r) = &meta.registration_endpoint {
        check_endpoint("registration_endpoint", r)?;
    }
    if let Some(r) = meta.additional_fields.get("revocation_endpoint") {
        check_endpoint("revocation_endpoint", r.as_str().unwrap_or_default())?;
    }
    Ok(())
}

fn validate_challenge_param<'a>(name: &str, value: &'a str) -> Result<&'a str, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c == '"' || c == '\\' || c == '\r' || c == '\n') {
        return Err(format!("invalid `{name}` parameter in OAuth WWW-Authenticate challenge"));
    }
    Ok(trimmed)
}

fn format_normalized_challenge(resource_metadata: Option<&str>, scope: Option<&str>) -> Result<Option<String>, String> {
    let mut parts = Vec::with_capacity(2);
    if let Some(url) = resource_metadata {
        let safe_url = validate_challenge_param("resource_metadata", url)?;
        parts.push(format!("resource_metadata=\"{safe_url}\""));
    }
    if let Some(sc) = scope {
        let safe_scope = validate_challenge_param("scope", sc)?;
        parts.push(format!("scope=\"{safe_scope}\""));
    }
    if parts.is_empty() { Ok(None) } else { Ok(Some(format!("Bearer {}", parts.join(", ")))) }
}

/// `rmcp` parses `WWW-Authenticate` via case-insensitive substring search for
/// `resource_metadata=` (both on the challenge passed to `resolve_metadata_from_challenge`
/// and when it re-probes the resource endpoint on fallback). Reject any header where such a
/// substring carries a cross-origin pointer before handing control to `rmcp`.
fn reject_rmcp_cross_origin_pointers(header: &str, base: &url::Url) -> Result<(), String> {
    let lower = header.to_ascii_lowercase();
    let needle = "resource_metadata=";
    let mut offset = 0;
    while let Some(pos) = lower[offset..].find(needle) {
        let val_start = offset + pos + needle.len();
        let rest = header[val_start..].trim_start();
        let leading_ws = header[val_start..].len() - rest.len();
        let (raw_val, consumed) = if let Some(q) = rest.strip_prefix('"') {
            let mut out = String::new();
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((i, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            out.push(n);
                        }
                    }
                    '"' => {
                        end = i + 1;
                        break;
                    }
                    c => out.push(c),
                }
            }
            (out, 1 + end.min(q.len()))
        } else {
            let end = rest.find(|c: char| c == ',' || c.is_whitespace()).unwrap_or(rest.len());
            (rest[..end].to_string(), end)
        };
        let trimmed = raw_val.trim();
        if !trimmed.is_empty() {
            let target = url::Url::parse(trimmed).or_else(|_| base.join(trimmed));
            if !target.is_ok_and(|t| same_origin(base, &t)) {
                return Err(format!(
                    "the MCP server's protected resource metadata is on another origin ({}); \
                     mare only follows metadata on the server's own origin",
                    mare_shared::redact::redact_str(trimmed)
                ));
            }
        }
        offset = val_start + leading_ws + consumed.max(1);
        if offset >= header.len() {
            break;
        }
    }
    Ok(())
}

/// Ask the server for its `WWW-Authenticate` challenge (a plain GET, no redirects, as rmcp's own
/// probe). A challenge whose `resource_metadata` points at another origin is refused: RFC 9728
/// lets the metadata live elsewhere, but rmcp then follows it to any public host, so a response
/// from the server would steer discovery (and the user's browser) to a host the server's
/// origin never vouched for. Returns a normalized Bearer challenge carrying only the validated
/// same-origin `resource_metadata` and/or `scope` parameters so downstream parsing cannot be
/// confused by lookalike parameters such as `x_resource_metadata=...`.
async fn probe_challenge(server_url: &str) -> Result<Option<String>, String> {
    let base = url::Url::parse(server_url).map_err(|e| format!("invalid MCP server URL: {e}"))?;
    let http = crate::http::build_client(
        crate::http::base_builder()?.redirect(reqwest::redirect::Policy::none()).timeout(DISCOVERY_PROBE_TIMEOUT),
    )?;
    let resp = http
        .get(base.clone())
        .header("MCP-Protocol-Version", "2024-11-05")
        .send()
        .await
        .map_err(|e| mare_shared::redact::redact_str(&format!("OAuth discovery: {e}")))?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(None);
    }
    let mut same_origin_challenge: Option<String> = None;
    let mut scope_only_challenge: Option<String> = None;
    for value in resp.headers().get_all(reqwest::header::WWW_AUTHENTICATE) {
        let Ok(value) = value.to_str() else { continue };
        reject_rmcp_cross_origin_pointers(value, &base)?;
        let scope = auth_param(value, "scope").filter(|s| !s.trim().is_empty());
        if let Some(pointer) = auth_param(value, "resource_metadata").filter(|p| !p.trim().is_empty()) {
            let target = url::Url::parse(pointer.trim()).or_else(|_| base.join(pointer.trim()));
            match target {
                Ok(t) if same_origin(&base, &t) => {
                    if same_origin_challenge.is_none() {
                        same_origin_challenge = format_normalized_challenge(Some(t.as_str()), scope.as_deref())?;
                    }
                }
                _ => {
                    return Err(format!(
                        "the MCP server's protected resource metadata is on another origin ({}); \
                         mare only follows metadata on the server's own origin",
                        mare_shared::redact::redact_str(&pointer)
                    ));
                }
            }
        } else if scope_only_challenge.is_none()
            && let Some(sc) = scope
        {
            scope_only_challenge = format_normalized_challenge(None, Some(&sc))?;
        }
    }
    Ok(same_origin_challenge.or(scope_only_challenge))
}

/// Resolve and check the authorization server metadata of `server_url`: the configured
/// `auth_server_metadata_url`, otherwise discovery (RFC 9728 protected resource metadata, then
/// RFC 8414 / OIDC at the server's origin). Endpoints rmcp synthesizes when the server publishes
/// nothing are refused: current MCP revisions require metadata discovery.
pub(crate) async fn discover(m: &mut AuthorizationManager, server_url: &str, opts: &OAuthOpts) -> Result<(), String> {
    let meta = if let Some(meta_url) = &opts.auth_server_metadata_url {
        fetch_metadata(meta_url, crate::http::offline_guard_active()).await?
    } else {
        let challenge = probe_challenge(server_url).await?;
        let res = m
            .resolve_metadata_from_challenge(challenge.as_deref())
            .await
            .map_err(|e| mare_shared::redact::redact_str(&e.to_string()))?;
        if res.source == AuthorizationMetadataSource::LegacyEndpointFallback {
            return Err("the MCP server publishes no OAuth metadata (no protected resource or authorization \
                 server metadata); set oauth.auth_server_metadata_url"
                .into());
        }
        res.metadata
    };
    check_metadata(&meta)?;
    m.set_metadata(meta);
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AuthManagerMode {
    Default,
    #[cfg(test)]
    ProductionPathNoOfflineGuard,
}

pub(crate) async fn auth_manager_with_mode(
    url: &str,
    key: &str,
    mode: AuthManagerMode,
) -> Result<AuthorizationManager, String> {
    let enforce_offline = match mode {
        AuthManagerMode::Default => crate::http::offline_guard_active(),
        #[cfg(test)]
        AuthManagerMode::ProductionPathNoOfflineGuard => false,
    };
    if enforce_offline {
        crate::http::check_offline_url(url)?;
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        crate::http::check_offline_proxy_for_url_with(url, None, &env)?;
    }
    // a local development server is on a private address by definition; otherwise rmcp's default
    // refuses metadata/token endpoints that resolve to private ranges
    let mut m = if is_loopback(url) {
        AuthorizationManager::new_allowing_private_addresses(url).await
    } else {
        AuthorizationManager::new(url).await
    }
    .map_err(|e| e.to_string())?;
    // Only swap in mare's offline-guarded reqwest client when the test offline guard is active;
    // in production, preserve rmcp's built-in MetadataAddressResolver and OAuthHttpRedirectPolicy::Stop.
    if enforce_offline {
        let http = crate::http::build_client(crate::http::base_builder()?)?;
        m.with_client(http).map_err(|e| e.to_string())?;
    }
    m.set_credential_store(FileCredentialStore(oauth_file(key, url)));
    Ok(m)
}

pub(crate) async fn auth_manager(url: &str, key: &str) -> Result<AuthorizationManager, String> {
    auth_manager_with_mode(url, key, AuthManagerMode::Default).await
}

// ---- the loopback callback --------------------------------------------------------------

/// What the browser sent to the redirect URI: the full URL, or the server's `error=`.
#[derive(Debug, PartialEq)]
pub(crate) enum Callback {
    Url(String),
    Error { message: String, state: Option<String> },
    Other,
}

fn query_param(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes()).find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

fn url_state_param(raw_url: &str) -> Option<String> {
    url::Url::parse(raw_url).ok().and_then(|u| u.query().and_then(|q| query_param(q, "state")))
}

/// Parse one HTTP request head (`GET /callback?code=… HTTP/1.1`). `base` is the redirect URI's
/// origin (`http://127.0.0.1:port`).
pub(crate) fn parse_callback(head: &str, base: &str, path_prefix: &str) -> Callback {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    let (Some(_method), Some(target)) = (parts.next(), parts.next()) else { return Callback::Other };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != path_prefix {
        return Callback::Other;
    }
    if let Some(err) = query_param(query, "error") {
        let desc = query_param(query, "error_description").map(|d| format!(": {d}")).unwrap_or_default();
        return Callback::Error {
            message: format!("the authorization server refused the login ({err}{desc})"),
            state: query_param(query, "state"),
        };
    }
    Callback::Url(format!("{base}{target}"))
}

/// Read a request head: until the blank line, at most 64 KiB, at most 10 s.
async fn read_head(sock: &mut tokio::net::TcpStream) -> std::io::Result<String> {
    use tokio::io::AsyncReadExt;
    const MAX: usize = 64 * 1024;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let read = async {
        loop {
            let n = sock.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.windows(2).any(|w| w == b"\n\n") || buf.len() >= MAX {
                break;
            }
        }
        Ok::<_, std::io::Error>(())
    };
    tokio::time::timeout(CALLBACK_READ_TIMEOUT, read)
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "callback request timed out"))??;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn respond(sock: &mut tokio::net::TcpStream, status: &str, body: &str) {
    use tokio::io::AsyncWriteExt;
    let _ = sock
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
}

/// A started login: the session and the callback listeners (127.0.0.1 and ::1).
type Pending = (AuthorizationSession, Vec<tokio::net::TcpListener>);

pub struct OAuthLogin {
    pub(crate) url: String,
    pub(crate) redirect: String,
    cred_path: PathBuf,
    state: Arc<Mutex<Option<Pending>>>,
    paste_tx: mpsc::UnboundedSender<String>,
    paste_rx: Arc<Mutex<mpsc::UnboundedReceiver<String>>>,
}

/// Start a login: listen on 127.0.0.1 and ::1 (same port; `callback_port` when configured),
/// discover (or take the configured) authorization server metadata, and build the URL to open.
pub(crate) async fn begin(server_url: String, key: String, opts: OAuthOpts) -> Result<OAuthLogin, String> {
    crate::http::check_offline_url(&server_url)?;
    let port = opts.callback_port.unwrap_or(0);
    let v4 = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
        mare_shared::redact::redact_str(&format!("cannot listen on 127.0.0.1:{port} for the OAuth callback: {e}"))
    })?;
    let port = v4.local_addr().map_err(|e| e.to_string())?.port();
    let mut listeners = vec![v4];
    if let Ok(v6) = tokio::net::TcpListener::bind(("::1", port)).await {
        listeners.push(v6);
    }
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let cred_path = oauth_file(&key, &server_url);
    let mut manager = auth_manager(&server_url, &key).await?;
    discover(&mut manager, &server_url, &opts).await?;
    let mut req = AuthorizationRequest::new(redirect.clone())
        .with_client_name(opts.client_name.clone().unwrap_or_else(|| "mare".into()))
        .with_initial_scope_selection();
    if !opts.scopes.is_empty() {
        req = req.with_scopes(opts.scopes.clone());
    }
    if let Some(id) = &opts.client_id {
        req = req.with_preregistered_client(id.clone());
    }
    if let Some(s) = &opts.client_secret {
        req = req.with_client_secret(s.clone());
    }
    let session = AuthorizationSession::new(manager, req)
        .await
        .map_err(|(_, e)| mare_shared::redact::redact_str(&e.to_string()))?;
    let url = session.get_authorization_url().to_string();
    let (paste_tx, paste_rx) = mpsc::unbounded_channel();
    Ok(OAuthLogin {
        url,
        redirect,
        cred_path,
        state: Arc::new(Mutex::new(Some((session, listeners)))),
        paste_tx,
        paste_rx: Arc::new(Mutex::new(paste_rx)),
    })
}

impl OAuthLogin {
    /// The redirected URL pasted by the user (browser on another machine, e.g. over SSH).
    pub(crate) fn paste(&self, url: String) {
        let _ = self.paste_tx.send(url);
    }

    /// Wait for the browser's redirect (either listener) or a pasted URL; exchange the code and
    /// store the tokens.
    pub(crate) async fn finish(&self, timeout: Duration) -> Result<(), String> {
        let Some((session, listeners)) = self.state.lock().await.take() else {
            return Err("this login was already finished".into());
        };
        let store = FileCredentialStore(self.cred_path.clone());
        let base = self.redirect.trim_end_matches("/callback").to_string();
        let expected_state = url_state_param(&self.url);
        let state_matches = |got: Option<&str>| match expected_state.as_deref() {
            Some(exp) => got == Some(exp),
            None => true,
        };
        let mut paste = self.paste_rx.lock().await;
        let wait = async {
            loop {
                let accept = async {
                    match listeners.as_slice() {
                        [a] => a.accept().await,
                        [a, b, ..] => tokio::select! { r = a.accept() => r, r = b.accept() => r },
                        [] => std::future::pending().await,
                    }
                };
                tokio::select! {
                    pasted = paste.recv() => {
                        let Some(pasted) = pasted else { continue };
                        let pasted = pasted.trim().to_string();
                        let (head_path, query) = match url::Url::parse(&pasted) {
                            Ok(u) => (u.path().to_string(), u.query().unwrap_or("").to_string()),
                            Err(e) => return Err(format!("not a URL: {e}")),
                        };
                        let target = if query.is_empty() { head_path.clone() } else { format!("{head_path}?{query}") };
                        match parse_callback(&format!("GET {target} HTTP/1.1"), &base, "/callback") {
                            Callback::Url(u) => {
                                let _guard = store
                                    .acquire_refresh_guard_with(REFRESH_LOCK_TIMEOUT)
                                    .await
                                    .map_err(|e| mare_shared::redact::redact_str(&e.to_string()))?;
                                return session
                                    .handle_callback_url(&u)
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| mare_shared::redact::redact_str(&e.to_string()));
                            }
                            Callback::Error { message, state } => {
                                if !state_matches(state.as_deref()) {
                                    return Err("OAuth callback state mismatch".into());
                                }
                                return Err(mare_shared::redact::redact_str(&message));
                            }
                            Callback::Other => return Err("the pasted URL isn't the /callback redirect".into()),
                        }
                    }
                    acc = accept => {
                        let (mut sock, _) = acc.map_err(|e| e.to_string())?;
                        let head = match read_head(&mut sock).await {
                            Ok(h) => h,
                            Err(_) => continue,
                        };
                        match parse_callback(&head, &base, "/callback") {
                            Callback::Other => {
                                respond(&mut sock, "404 Not Found", "").await;
                            }
                            Callback::Error { message, state } => {
                                // Validate OAuth `state` before terminating the listener so unrelated
                                // local requests to `/callback?error=...` cannot abort an active login.
                                if !state_matches(state.as_deref()) {
                                    respond(&mut sock, "400 Bad Request", "mare: invalid OAuth state").await;
                                    continue;
                                }
                                let e = mare_shared::redact::redact_str(&message);
                                respond(&mut sock, "400 Bad Request", &format!("mare: login failed — {e}")).await;
                                return Err(e);
                            }
                            Callback::Url(u) => {
                                if !state_matches(url_state_param(&u).as_deref()) {
                                    respond(&mut sock, "400 Bad Request", "mare: invalid OAuth state").await;
                                    continue;
                                }
                                let _guard = store
                                    .acquire_refresh_guard_with(REFRESH_LOCK_TIMEOUT)
                                    .await
                                    .map_err(|e| mare_shared::redact::redact_str(&e.to_string()))?;
                                let res = session
                                    .handle_callback_url(&u)
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| mare_shared::redact::redact_str(&e.to_string()));
                                let body = match &res {
                                    Ok(()) => "mare: logged in — you can close this window".to_string(),
                                    Err(e) => format!("mare: login failed — {e}"),
                                };
                                respond(&mut sock, if res.is_ok() { "200 OK" } else { "400 Bad Request" }, &body).await;
                                return res;
                            }
                        }
                    }
                }
            }
        };
        tokio::time::timeout(timeout, wait).await.map_err(|_| "timed out waiting for the browser login".to_string())?
    }
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "oauth_discovery_tests.rs"]
mod discovery_tests;
