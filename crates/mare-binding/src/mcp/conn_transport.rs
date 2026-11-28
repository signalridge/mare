//! MCP transport setup (stdio in its own process group, streamable HTTP with headers / bearer /
//! OAuth / a custom CA), stderr tailing, and parallel prefetch of tools / resources / prompts.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use rmcp::ServiceExt;
use rmcp::model::PaginatedRequestParams;
#[allow(deprecated)]
use rmcp::model::Root;
use rmcp::service::{Peer, RoleClient};
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, InsufficientScopeError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};
use tokio_util::sync::CancellationToken;

/// Timeout for best-effort `DELETE` cleanup of a session abandoned mid-handshake.
const HANDSHAKE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct HandshakeSession {
    client: reqwest::Client,
    uri: Arc<str>,
    session_id: Arc<str>,
    auth: Option<String>,
    headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
}

struct HandshakeSlot {
    active: bool,
    session: Option<HandshakeSession>,
}

/// Sends a best-effort `DELETE` for a Streamable HTTP session obtained during `initialize` if
/// the handshake or initial prefetch is abandoned (error, retry, cancellation, or timeout).
pub(super) struct AbandonedHandshakeGuard {
    slot: Arc<StdMutex<HandshakeSlot>>,
}

impl AbandonedHandshakeGuard {
    pub(super) fn new() -> Self {
        Self { slot: Arc::new(StdMutex::new(HandshakeSlot { active: true, session: None })) }
    }

    fn slot(&self) -> Arc<StdMutex<HandshakeSlot>> {
        self.slot.clone()
    }

    pub(super) fn disarm(&self) {
        let mut g = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        g.active = false;
        g.session = None;
    }

    pub(super) async fn cleanup_now(&self) {
        let session = self.slot.lock().unwrap_or_else(|e| e.into_inner()).session.take();
        if let Some(s) = session {
            let _ = tokio::time::timeout(
                HANDSHAKE_CLEANUP_TIMEOUT,
                s.client.delete_session(s.uri, s.session_id, s.auth, s.headers),
            )
            .await;
        }
    }
}

impl Drop for AbandonedHandshakeGuard {
    fn drop(&mut self) {
        let session = self.slot.lock().unwrap_or_else(|e| e.into_inner()).session.take();
        if let Some(s) = session
            && let Ok(h) = tokio::runtime::Handle::try_current()
        {
            h.spawn(async move {
                let _ = tokio::time::timeout(
                    HANDSHAKE_CLEANUP_TIMEOUT,
                    s.client.delete_session(s.uri, s.session_id, s.auth, s.headers),
                )
                .await;
            });
        }
    }
}

fn is_initialize_request(message: &rmcp::model::ClientJsonRpcMessage) -> bool {
    matches!(
        message,
        rmcp::model::ClientJsonRpcMessage::Request(req)
            if matches!(&req.request, rmcp::model::ClientRequest::InitializeRequest(_))
    )
}

fn response_session_id(resp: &StreamableHttpPostResponse) -> Option<&str> {
    match resp {
        StreamableHttpPostResponse::Json(_, sid) | StreamableHttpPostResponse::Sse(_, sid) => sid.as_deref(),
        _ => None,
    }
}

/// Legacy Streamable HTTP MCP servers often reject expired or evicted sessions with HTTP 400
/// carrying an explicit missing/invalid/expired session message instead of HTTP 404.
fn is_explicit_session_expiry_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "no valid session id",
        "invalid session id",
        "invalid mcp-session-id",
        "missing session id",
        "missing mcp-session-id",
        "unknown session id",
        "unknown session",
        "session not found",
        "session has expired",
        "session expired",
        "expired session",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

fn normalize_legacy_session_expiry(
    result: Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>>,
    session_was_attached: bool,
    is_init: bool,
) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
    if !session_was_attached || is_init {
        return result;
    }
    match result {
        Ok(StreamableHttpPostResponse::Json(rmcp::model::ServerJsonRpcMessage::Error(ref err_msg), _))
            if is_explicit_session_expiry_message(&err_msg.error.message) =>
        {
            Err(StreamableHttpError::SessionExpired)
        }
        Err(StreamableHttpError::UnexpectedServerResponse(ref msg))
            if msg.contains("400") && is_explicit_session_expiry_message(msg) =>
        {
            Err(StreamableHttpError::SessionExpired)
        }
        other => other,
    }
}

/// Keep a server's reply to a client response out of the JSON-RPC dispatcher.
/// The guard sits below AuthClient so both authenticated and plain requests use it.
#[derive(Clone)]
struct ReplyGuard {
    client: reqwest::Client,
    reply_expired: Option<CancellationToken>,
    handshake_slot: Option<Arc<StdMutex<HandshakeSlot>>>,
}

impl ReplyGuard {
    #[cfg(test)]
    fn new(client: reqwest::Client) -> Self {
        Self { client, reply_expired: None, handshake_slot: None }
    }

    fn with_guards(
        client: reqwest::Client,
        reply_expired: CancellationToken,
        handshake_slot: Arc<StdMutex<HandshakeSlot>>,
    ) -> Self {
        Self { client, reply_expired: Some(reply_expired), handshake_slot: Some(handshake_slot) }
    }

    fn record_handshake_session(
        &self,
        uri: Arc<str>,
        session_id: &str,
        auth: Option<String>,
        headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) {
        let Some(slot) = &self.handshake_slot else { return };
        let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
        if g.active {
            g.session = Some(HandshakeSession {
                client: self.client.clone(),
                uri,
                session_id: Arc::from(session_id),
                auth,
                headers,
            });
        }
    }
}

impl StreamableHttpClient for ReplyGuard {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        if matches!(
            message,
            rmcp::model::ClientJsonRpcMessage::Response(_) | rmcp::model::ClientJsonRpcMessage::Error(_)
        ) {
            self.post_message_with_max_sse_event_size(uri, message, session_id, auth, headers, 0).await
        } else {
            let session_was_attached = session_id.is_some();
            let is_init = is_initialize_request(&message);
            let track = is_init.then(|| (uri.clone(), auth.clone(), headers.clone()));
            let resp = normalize_legacy_session_expiry(
                self.client.post_message(uri, message, session_id, auth, headers).await.map_err(reclassify_forbidden),
                session_was_attached,
                is_init,
            )?;
            if let Some((u, a, h)) = track
                && let Some(sid) = response_session_id(&resp)
            {
                self.record_handshake_session(u, sid, a, h);
            }
            Ok(resp)
        }
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: rmcp::model::ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth: Option<String>,
        headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
        max_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        use rmcp::model::ClientJsonRpcMessage;
        if !matches!(message, ClientJsonRpcMessage::Response(_) | ClientJsonRpcMessage::Error(_)) {
            let session_was_attached = session_id.is_some();
            let is_init = is_initialize_request(&message);
            let track = is_init.then(|| (uri.clone(), auth.clone(), headers.clone()));
            let resp = normalize_legacy_session_expiry(
                self.client
                    .post_message_with_max_sse_event_size(uri, message, session_id, auth, headers, max_size)
                    .await
                    .map_err(reclassify_forbidden),
                session_was_attached,
                is_init,
            )?;
            if let Some((u, a, h)) = track
                && let Some(sid) = response_session_id(&resp)
            {
                self.record_handshake_session(u, sid, a, h);
            }
            return Ok(resp);
        }
        let mut request =
            self.client.post(uri.as_ref()).header(reqwest::header::ACCEPT, "application/json, text/event-stream");
        if let Some(auth) = auth {
            request = request.bearer_auth(auth);
        }
        for (name, value) in headers {
            if matches!(name.as_str(), "accept" | "mcp-session-id" | "last-event-id") {
                return Err(StreamableHttpError::ReservedHeaderConflict(name.to_string()));
            }
            request = request.header(name, value);
        }
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header("mcp-session-id", session_id.as_ref());
        }
        let response = request.json(&message).send().await.map_err(StreamableHttpError::Client)?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED
            && let Some(header) = pick_www_authenticate(response.headers())?
        {
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(header)));
        }
        if status == reqwest::StatusCode::FORBIDDEN
            && let Some(header) = pick_www_authenticate(response.headers())?
        {
            let scope = super::oauth::auth_param(&header, "scope");
            return Err(reclassify_forbidden(StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
                header, scope,
            ))));
        }
        match status {
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT => Ok(StreamableHttpPostResponse::Accepted),
            reqwest::StatusCode::NOT_FOUND if session_was_attached => {
                if let Some(tok) = &self.reply_expired {
                    tok.cancel();
                }
                Err(StreamableHttpError::SessionExpired)
            }
            reqwest::StatusCode::BAD_REQUEST if session_was_attached => {
                let body = response.text().await.unwrap_or_default();
                if is_explicit_session_expiry_message(&body) {
                    if let Some(tok) = &self.reply_expired {
                        tok.cancel();
                    }
                    Err(StreamableHttpError::SessionExpired)
                } else {
                    Err(StreamableHttpError::UnexpectedServerResponse(
                        format!("MCP reply POST expected 202 or 204, got HTTP {status}").into(),
                    ))
                }
            }
            status => Err(StreamableHttpError::UnexpectedServerResponse(
                format!("MCP reply POST expected 202 or 204, got HTTP {status}").into(),
            )),
        }
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth: Option<String>,
        headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        if let Some(slot) = &self.handshake_slot {
            let mut g = slot.lock().unwrap_or_else(|e| e.into_inner());
            if g.session.as_ref().is_some_and(|s| s.session_id.as_ref() == session_id.as_ref()) {
                g.session = None;
            }
        }
        self.client.delete_session(uri, session_id, auth, headers).await.map_err(reclassify_forbidden)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        self.client.get_stream(uri, session_id, last_event_id, auth, headers).await.map_err(reclassify_forbidden)
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth: Option<String>,
        headers: HashMap<reqwest::header::HeaderName, reqwest::header::HeaderValue>,
        max_size: usize,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        self.client
            .get_stream_with_max_sse_event_size(uri, session_id, last_event_id, auth, headers, max_size)
            .await
            .map_err(reclassify_forbidden)
    }
}

/// rmcp reports every 403 that carries `WWW-Authenticate` as insufficient scope. Only a
/// challenge with `error="insufficient_scope"` asks for more scope (RFC 6750, and the MCP
/// authorization specification's step-up); any other 403 is a plain refusal and is reported
/// as one, so the user is not told to widen scopes that would not help.
fn reclassify_forbidden<E: std::error::Error + Send + Sync + 'static>(
    e: StreamableHttpError<E>,
) -> StreamableHttpError<E> {
    match e {
        StreamableHttpError::InsufficientScope(s)
            if super::oauth::auth_param(&s.www_authenticate_header, "error").as_deref()
                != Some("insufficient_scope") =>
        {
            StreamableHttpError::UnexpectedServerResponse(
                format!("HTTP 403 Forbidden ({})", redact_str(&s.www_authenticate_header)).into(),
            )
        }
        e => e,
    }
}

use super::conn_handler::{Handler, Service, mcp_log};
use crate::platform::{self, ProcessBackend};
use mare_shared::redact::redact_str;

/// tools/list, resources/list, prompts/list: at most this many pages;
/// a cursor loop ends as truncation, not a hang.
pub(crate) const MAX_PAGES: usize = 16;
/// Startup timeout (initialize + the first listings): default and cap.
pub(crate) const DEFAULT_STARTUP: Duration = Duration::from_secs(60);
pub(crate) const MAX_STARTUP: Duration = Duration::from_secs(300);
/// Streamable HTTP initialize retries.
const HTTP_RETRY_DELAYS_MS: [u64; 2] = [250, 1_000];
/// stderr lines kept per server (the `/mcp` tail) and the longest line kept.
const STDERR_LINES: usize = 50;
const STDERR_LINE_MAX: usize = 2000;

/// The parsed parts of a connect spec that matter on every (re)connect.
pub(crate) struct Spec {
    pub raw: serde_json::Value,
    pub name: String,
    pub startup: Duration,
}

impl Spec {
    pub(crate) fn parse(raw: serde_json::Value) -> Result<Self, String> {
        let name = raw.get("name").and_then(|v| v.as_str()).unwrap_or("mcp").to_string();
        let startup = raw
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_STARTUP)
            .min(MAX_STARTUP);
        let ty = raw.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if ty.eq_ignore_ascii_case("sse") {
            // rmcp 3.9 has no legacy SSE client transport: say so instead of failing quietly
            return Err(
                "the legacy SSE transport isn't supported (rmcp has no SSE client); most servers that document an \
                 SSE endpoint also serve streamable HTTP, commonly at /mcp instead of /sse"
                    .into(),
            );
        }
        if !matches!(ty, "" | "stdio" | "http" | "streamable-http" | "streamable_http") {
            return Err(format!("unknown MCP transport type {ty:?} (stdio, http)"));
        }
        if raw.get("command").and_then(|v| v.as_str()).is_none() && raw.get("url").and_then(|v| v.as_str()).is_none() {
            return Err("an MCP server needs `command` (stdio) or `url` (streamable HTTP)".into());
        }
        Ok(Self { raw, name, startup })
    }
    pub(super) fn str(&self, k: &str) -> Option<String> {
        self.raw.get(k).and_then(|v| v.as_str()).map(str::to_string)
    }
    pub(super) fn strs(&self, k: &str) -> Vec<String> {
        self.raw
            .get(k)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    }
}

#[allow(deprecated)]
pub(super) fn roots_of(spec: &Spec) -> Vec<Root> {
    spec.strs("roots")
        .into_iter()
        .filter_map(|p| {
            let uri = url::Url::from_directory_path(&p).ok()?.to_string();
            let name = std::path::Path::new(&p).file_name().map(|n| n.to_string_lossy().into_owned());
            serde_json::from_value(serde_json::json!({ "uri": uri, "name": name })).ok()
        })
        .collect()
}

/// Environment values: strings as they are, numbers and booleans converted; anything else is a
/// configuration error (never silently dropped).
pub(crate) fn env_value(k: &str, v: &serde_json::Value) -> Result<String, String> {
    match v {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        serde_json::Value::Bool(b) => Ok(b.to_string()),
        other => Err(format!("env.{k} must be a string, number or boolean (got {other})")),
    }
}

/// The child process of a stdio server; its whole process group (Windows: job) ends with it,
/// with the host or with mare ([`crate::trees::Tree`]), whether or not it was closed.
pub(crate) struct ChildProc {
    child: tokio::process::Child,
    tree: crate::trees::Tree,
}

impl ChildProc {
    pub(super) async fn stop(mut self) {
        // Run stop_tree in a detached task so a caller timeout cannot drop `self.tree`
        // mid-sequence (which would SIGKILL + unregister before `end_all_graceful` confirms).
        let handle = tokio::spawn(async move {
            let (_, confirmed) = crate::proc::stop_tree(&mut self.child, self.tree.pid(), Duration::from_secs(1)).await;
            // unconfirmed: keep it registered so shutdown still stops and reports it
            self.tree.settle(confirmed);
        });
        let _ = handle.await;
    }
}

/// Read stderr lines (bounded) into the tail and `mcp.log`.
fn pump_stderr(server: Arc<str>, mut err: tokio::process::ChildStderr, tail: Arc<StdMutex<VecDeque<String>>>) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 8192];
        let mut line: Vec<u8> = Vec::new();
        let emit = |bytes: &[u8]| {
            let mut s = String::from_utf8_lossy(bytes).trim_end_matches('\r').to_string();
            if s.len() > STDERR_LINE_MAX {
                let mut cut = STDERR_LINE_MAX;
                while !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                s.truncate(cut);
                s.push('…');
            }
            mcp_log(&server, "stderr", &s);
            let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
            t.push_back(redact_str(&s));
            while t.len() > STDERR_LINES {
                t.pop_front();
            }
        };
        loop {
            let n = match err.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                if b == b'\n' {
                    emit(&line);
                    line.clear();
                } else if line.len() < STDERR_LINE_MAX * 2 {
                    line.push(b);
                }
            }
        }
        if !line.is_empty() {
            emit(&line);
        }
    });
}

pub(super) fn is_retryable_http(e: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"(?i)HTTP (408|429|5\d\d)\b|status(?:: | code )(408|429|5\d\d)\b|error sending request|connection refused|connection reset")
            .expect("retry pattern")
    });
    RE.is_match(e)
}

pub(super) async fn spawn_stdio(
    spec: &Spec,
    handler: Handler,
    tail: &Arc<StdMutex<VecDeque<String>>>,
    owner: Option<crate::trees::Owner>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<(Service, ChildProc), String> {
    let cmd = spec.str("command").unwrap_or_default();
    let policy = crate::subenv::EnvPolicy { include: spec.strs("env_include"), exclude: spec.strs("env_exclude") };
    let mut env: Vec<(std::ffi::OsString, std::ffi::OsString)> = crate::subenv::scrubbed_parent_env(&policy);
    if let Some(e) = spec.raw.get("env").and_then(|e| e.as_object()) {
        for (k, v) in e {
            let v = env_value(k, v)?;
            env.retain(|(ek, _)| ek != k.as_str());
            env.push((k.into(), v.into()));
        }
    }
    #[cfg(windows)]
    let program = {
        use std::path::PathBuf;

        // `npx`, `uvx` … are.cmd shims: resolve them like a shell would
        let path = env.iter().find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case("PATH")).map(|(_, v)| v.clone());
        let cwd = spec.str("cwd").map(PathBuf::from).unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        which::which_in(&cmd, path, cwd).map(|p| p.into_os_string()).unwrap_or_else(|_| cmd.clone().into())
    };
    #[cfg(not(windows))]
    let program: std::ffi::OsString = cmd.clone().into();
    let mut c = tokio::process::Command::new(program);
    c.args(spec.strs("args"));
    if let Some(cwd) = spec.str("cwd") {
        c.current_dir(cwd);
    }
    c.env_clear();
    c.envs(env);
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    c.kill_on_drop(true);
    // its own process group (Windows: job), like every mare.proc child
    <platform::Process as ProcessBackend>::isolate(&mut c);
    let mut child = platform::spawn(&mut c).map_err(|e| format!("starting {cmd}: {e}"))?;
    // registered from the start: an initialize that fails or times out still ends the tree
    let tree = crate::trees::Tree::new(child.id(), owner);
    if let Some(err) = child.stderr.take() {
        pump_stderr(Arc::from(spec.name.as_str()), err, tail.clone());
    }
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        return Err(format!("starting {cmd}: no stdio pipes"));
    };
    let proc = ChildProc { child, tree };
    let tracker = handler.progress_tracker.clone();
    let transport = super::conn_handler::ProgressTrackingTransport::new(
        rmcp::transport::IntoTransport::into_transport((stdout, stdin)),
        tracker,
    );
    let closed = handler.closed.clone();
    let svc_fut = handler.serve(transport);
    let cancelled_fut = async {
        match cancel {
            Some(c) => tokio::select! {
                _ = closed.cancelled() => {},
                _ = c.cancelled() => {},
            },
            None => closed.cancelled().await,
        }
    };
    let svc = tokio::select! {
        biased;
        _ = cancelled_fut => {
            proc.stop().await;
            return Err("MCP connection cancelled".into());
        }
        res = svc_fut => res.map_err(|e| {
            let tail = tail.lock().unwrap_or_else(|e| e.into_inner());
            let last: Vec<&str> = tail.iter().rev().take(3).rev().map(String::as_str).collect();
            if last.is_empty() { e.to_string() } else { format!("{e} (stderr: {})", last.join(" | ")) }
        })?,
    };
    Ok((svc, proc))
}

pub(super) async fn http_client(spec: &Spec) -> Result<reqwest::Client, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(h) = spec.raw.get("headers").and_then(|h| h.as_object()) {
        for (k, v) in h {
            let val = env_value(k, v).map_err(|_| format!("header {k} must be a string"))?;
            let (Ok(name), Ok(val)) =
                (reqwest::header::HeaderName::from_bytes(k.as_bytes()), reqwest::header::HeaderValue::from_str(&val))
            else {
                return Err(format!("invalid header {k}"));
            };
            headers.insert(name, val);
        }
    }
    // the same client setup as mare.http: proxies, NO_PROXY, system + custom CAs;
    // restrict redirects to the server's origin so custom headers (e.g. X-API-Key
    // from `headers` / `env_http_headers`) never follow cross-origin hops.
    let mut b =
        crate::http::base_builder()?.redirect(crate::http::same_origin_redirect_policy()).default_headers(headers);
    if let Some(ca) = spec.raw.get("tls").and_then(|t| t.get("ca_bundle")).and_then(|v| v.as_str()) {
        let pem = std::fs::read(ca).map_err(|e| format!("tls.ca_bundle {ca}: {e}"))?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| format!("tls.ca_bundle {ca}: {e}"))?;
        if certs.is_empty() {
            return Err(format!("tls.ca_bundle {ca}: no certificates found"));
        }
        for c in certs {
            b = b.add_root_certificate(c);
        }
    }
    crate::http::build_client(b)
}

#[cfg(test)]
pub(super) async fn connect_http(spec: &Spec, handler: Handler, deadline: Instant) -> Result<Service, String> {
    let guard = AbandonedHandshakeGuard::new();
    let res = connect_http_with_guard(spec, handler, deadline, &guard).await;
    if res.is_ok() {
        guard.disarm();
    }
    res
}

pub(super) async fn connect_http_with_guard(
    spec: &Spec,
    handler: Handler,
    deadline: Instant,
    handshake_guard: &AbandonedHandshakeGuard,
) -> Result<Service, String> {
    let url = spec.str("url").unwrap_or_default();
    crate::http::check_offline_url(&url)?;
    let oauth = spec.raw.get("oauth").filter(|o| !o.is_null() && *o != &serde_json::Value::Bool(false));
    let mut attempt = 0;
    loop {
        let http = http_client(spec).await?;
        let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.clone());
        cfg.reinit_on_expired_session = true; // HTTP 404 on a session: re-initialize (rmcp)
        if let Some(t) = spec.str("bearer_token") {
            cfg = cfg.auth_header(t);
        }
        let reply_expired = CancellationToken::new();
        let reply_guard = ReplyGuard::with_guards(http, reply_expired.clone(), handshake_guard.slot());
        let res = if let Some(o) = oauth {
            let key = o.get("key").and_then(|k| k.as_str()).unwrap_or(&spec.name).to_string();
            let opts = super::oauth::OAuthOpts::from_json(Some(o));
            let not_logged_in = || format!("not logged in to this MCP server — run `/mcp login {}`", spec.name);
            let mut m = super::oauth::auth_manager(&url, &key).await?;
            if !super::oauth::FileCredentialStore(super::oauth::oauth_file(&key, &url)).load_present().await? {
                return Err(not_logged_in());
            }
            super::oauth::discover(&mut m, &url, &opts).await?;
            if !m.initialize_from_store().await.map_err(|e| e.to_string())? {
                return Err(not_logged_in());
            }
            let transport = StreamableHttpClientTransport::with_client(AuthClient::new(reply_guard, m), cfg);
            let tracked = super::conn_handler::ProgressTrackingTransport::new(
                rmcp::transport::IntoTransport::into_transport(transport),
                handler.progress_tracker.clone(),
            )
            .with_abort(reply_expired);
            handler.clone().serve(tracked).await.map_err(|e| e.to_string())
        } else {
            let transport = StreamableHttpClientTransport::with_client(reply_guard, cfg);
            let tracked = super::conn_handler::ProgressTrackingTransport::new(
                rmcp::transport::IntoTransport::into_transport(transport),
                handler.progress_tracker.clone(),
            )
            .with_abort(reply_expired);
            handler.clone().serve(tracked).await.map_err(|e| e.to_string())
        };
        match res {
            Ok(svc) => return Ok(svc),
            Err(e) if is_retryable_http(&e) && attempt < HTTP_RETRY_DELAYS_MS.len() => {
                handshake_guard.cleanup_now().await;
                let delay = Duration::from_millis(HTTP_RETRY_DELAYS_MS[attempt]);
                attempt += 1;
                if Instant::now() + delay >= deadline {
                    return Err(e);
                }
                mcp_log(&spec.name, "warn", &format!("initialize failed ({e}); retrying in {}ms", delay.as_millis()));
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                handshake_guard.cleanup_now().await;
                return Err(e);
            }
        }
    }
}

/// Listings of one connection (wire form), fetched in parallel right after initialize.
#[derive(Default, Clone)]
pub(crate) struct Listings {
    pub tools: serde_json::Value,
    pub resources: serde_json::Value,
    pub templates: serde_json::Value,
    pub prompts: serde_json::Value,
    pub truncated: Vec<&'static str>,
    pub tools_generation: u64,
}

macro_rules! list_all {
    ($peer:expr, $method:ident, $field:ident) => {
        async {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            let mut pages = 0usize;
            let mut truncated = false;
            loop {
                let r = $peer.$method(Some(PaginatedRequestParams::default().with_cursor(cursor))).await?;
                out.extend(r.$field);
                pages += 1;
                cursor = r.next_cursor;
                if cursor.is_none() {
                    break;
                }
                if pages >= MAX_PAGES {
                    truncated = true;
                    break;
                }
            }
            Ok::<_, rmcp::ServiceError>((serde_json::to_value(out).unwrap_or_default(), truncated))
        }
        .await
    };
}

async fn list_tools(peer: &Peer<RoleClient>) -> Result<(serde_json::Value, bool), rmcp::ServiceError> {
    list_all!(peer, list_tools, tools)
}

pub(super) async fn prefetch(svc: &Service, server: &str) -> Result<Listings, String> {
    let peer = svc.peer().clone();
    let caps = svc.peer_info().map(|i| serde_json::to_value(&i.capabilities).unwrap_or_default()).unwrap_or_default();
    let has = |k: &str| caps.get(k).is_some_and(|v| !v.is_null());
    let empty = || Ok::<_, String>((serde_json::json!([]), false));
    // Always request tools/list even when `capabilities.tools` is omitted (matching Codex
    // `codex-mcp/src/rmcp_client.rs:1374`), falling back to empty only if an unadvertised
    // server rejects `tools/list` with JSON-RPC `-32601 Method not found` (rmcp 3.12 default).
    let (tools, resources, templates, prompts) = tokio::join!(
        async {
            match list_tools(&peer).await {
                Ok(v) => Ok(v),
                Err(rmcp::ServiceError::McpError(ref err))
                    if !has("tools") && err.code == rmcp::model::ErrorCode::METHOD_NOT_FOUND =>
                {
                    mcp_log(server, "info", "tools/list returned method not found; capabilities.tools omitted");
                    empty()
                }
                Err(e) => Err(e.to_string()),
            }
        },
        async {
            if has("resources") {
                list_all!(peer, list_resources, resources).map_err(|e| e.to_string())
            } else {
                empty()
            }
        },
        async {
            if has("resources") {
                list_all!(peer, list_resource_templates, resource_templates).map_err(|e| e.to_string())
            } else {
                empty()
            }
        },
        async {
            if has("prompts") { list_all!(peer, list_prompts, prompts).map_err(|e| e.to_string()) } else { empty() }
        },
    );
    let (tools, t_trunc) = tools.map_err(|e| format!("tools/list: {e}"))?;
    let mut l = Listings { tools, ..Default::default() };
    if t_trunc {
        l.truncated.push("tools");
    }
    for (kind, r, slot) in [
        ("resources", resources, &mut l.resources),
        ("resource templates", templates, &mut l.templates),
        ("prompts", prompts, &mut l.prompts),
    ] {
        match r {
            Ok((v, trunc)) => {
                *slot = v;
                if trunc {
                    l.truncated.push(kind);
                }
            }
            Err(e) => {
                mcp_log(server, "warn", &format!("{kind}/list: {e}"));
                *slot = serde_json::json!([]);
            }
        }
    }
    for kind in &l.truncated {
        mcp_log(server, "warn", &format!("{kind}: stopped after {MAX_PAGES} pages"));
    }
    Ok(l)
}

fn pick_www_authenticate(
    headers: &reqwest::header::HeaderMap,
) -> Result<Option<String>, StreamableHttpError<reqwest::Error>> {
    let mut first = None;
    for value in headers.get_all(reqwest::header::WWW_AUTHENTICATE) {
        let s = value.to_str().map_err(|_| {
            StreamableHttpError::UnexpectedServerResponse("invalid www-authenticate header value".into())
        })?;
        if s.split_whitespace().next().is_some_and(|scheme| scheme.eq_ignore_ascii_case("bearer")) {
            return Ok(Some(s.to_string()));
        }
        if first.is_none() {
            first = Some(s.to_string());
        }
    }
    Ok(first)
}

#[cfg(test)]
mod www_authenticate_tests {
    use super::pick_www_authenticate;
    use reqwest::header::{HeaderMap, HeaderValue, WWW_AUTHENTICATE};

    #[test]
    fn pick_www_authenticate_scans_all_headers_and_prefers_bearer() {
        let mut headers = HeaderMap::new();
        headers.append(WWW_AUTHENTICATE, HeaderValue::from_static(r#"Basic realm="internal""#));
        headers.append(WWW_AUTHENTICATE, HeaderValue::from_static(r#"Bearer realm="mcp", scope="mcp:read""#));
        let picked = pick_www_authenticate(&headers).unwrap().expect("must find bearer header");
        assert!(picked.starts_with("Bearer "), "{picked}");
        assert_eq!(crate::mcp::oauth::auth_param(&picked, "scope").as_deref(), Some("mcp:read"));
    }
}

#[cfg(test)]
#[path = "conn_transport_tests.rs"]
mod tests;
