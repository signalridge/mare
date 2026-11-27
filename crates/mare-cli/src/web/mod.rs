//! Host-level HTTP/WebSocket listener.
//!
//! Runs on the `HostManager` / `RpcRuntime` LocalSet, independent of any single session Lua VM,
//! reuses `mare_binding::http_serve::host_transport` auth/Host/Origin/cookie/rate-limit/accept
//! primitives, serves dynamic `webserver` routes and `web = { dir }` static assets, and bridges
//! WebSocket connections into `rpc::serve::serve_with_io`.

mod tls;
mod ws_rpc;
#[cfg(test)]
pub(crate) use ws_rpc::{OVERLOAD_CLOSE_TIMEOUT, WS_SEND_TIMEOUT, WS_SHUTDOWN_TIMEOUT};

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use mare_binding::http_serve::host_transport::{
    AuthOutcome, AuthState, ServeBody, TokenSource, accept_backoff_wait, acquire_permit, empty_body, full_body,
    is_loopback_host, is_loopback_ip, is_valid_loopback_host_header, is_websocket_upgrade_request,
    next_listener_cookie_name, parse_trusted_hosts, plain_status_response, validate_request_host,
    validate_request_origin,
};
use mare_host::{AssetResolve, WebListenerInfo, WebRouteRequest};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;

use crate::rpc::RpcRuntime;
use crate::rpc::serve::MAX_LINE;

const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CONNECTIONS: usize = 256;
const DEFAULT_HEADER_READ_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_WS_PING_INTERVAL_MS: u64 = 15_000;
const DEFAULT_WS_PONG_TIMEOUT_MS: u64 = 10_000;
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

static NEXT_LISTENER_INSTANCE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebTokenKind {
    RuntimeFile,
    Static,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WebListenerRecord {
    pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    start_time: Option<u64>,
    instance_id: u64,
    url: String,
    ws_url: String,
    tls: bool,
    token_source: WebTokenKind,
}

#[derive(Debug, PartialEq, Eq)]
struct ValidatedListenerUrl {
    base: String,
    ip: IpAddr,
}

#[derive(Debug, PartialEq, Eq)]
enum ListenerUrlError {
    Ipv6ZoneUnsupported,
    Invalid,
}

fn is_zone_scoped_ipv6_url(raw: &str) -> bool {
    let trimmed = raw.trim().trim_end_matches('/');
    let Some(rest) = trimmed.strip_prefix("http://[").or_else(|| trimmed.strip_prefix("https://[")) else {
        return false;
    };
    let Some((bracketed, after)) = rest.split_once("]:") else {
        return false;
    };
    let Ok(port) = after.parse::<u16>() else {
        return false;
    };
    if port == 0 {
        return false;
    }
    let Some((addr, zone)) = bracketed.split_once('%') else {
        return false;
    };
    !zone.is_empty() && addr.parse::<std::net::Ipv6Addr>().is_ok()
}

fn validate_listener_url(raw: &str) -> Result<ValidatedListenerUrl, ListenerUrlError> {
    if is_zone_scoped_ipv6_url(raw) {
        return Err(ListenerUrlError::Ipv6ZoneUnsupported);
    }
    let parsed = url::Url::parse(raw.trim()).map_err(|_| ListenerUrlError::Invalid)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ListenerUrlError::Invalid);
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || (parsed.path() != "/" && !parsed.path().is_empty())
    {
        return Err(ListenerUrlError::Invalid);
    }
    let port = parsed.port_or_known_default().ok_or(ListenerUrlError::Invalid)?;
    if port == 0 {
        return Err(ListenerUrlError::Invalid);
    }
    let ip = match parsed.host().ok_or(ListenerUrlError::Invalid)? {
        url::Host::Ipv4(v4) => IpAddr::V4(v4),
        url::Host::Ipv6(v6) => IpAddr::V6(v6),
        url::Host::Domain(_) => return Err(ListenerUrlError::Invalid),
    };
    if is_unspecified_ip(ip) {
        return Err(ListenerUrlError::Invalid);
    }
    Ok(ValidatedListenerUrl { base: format!("{}://{}", parsed.scheme(), SocketAddr::new(ip, port)), ip })
}

fn is_local_interface_ip(ip: IpAddr) -> bool {
    std::net::TcpListener::bind((ip, 0)).is_ok()
}

#[cfg(unix)]
fn verify_private_entry_meta(
    path: &Path,
    is_symlink: bool,
    type_ok: bool,
    uid: u32,
    me: u32,
    mode: u32,
) -> std::io::Result<()> {
    if is_symlink || !type_ok || uid != me || (mode & 0o077) != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} failed private ownership/mode check", path.display()),
        ));
    }
    Ok(())
}

fn verify_private_dir(dir: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = unsafe { libc::geteuid() };
        verify_private_entry_meta(dir, meta.file_type().is_symlink(), meta.is_dir(), meta.uid(), me, meta.mode())?;
    }
    #[cfg(not(unix))]
    {
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is not a real directory", dir.display()),
            ));
        }
        #[cfg(windows)]
        mare_shared::private::check(dir).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    }
    Ok(())
}

fn verify_private_file(path: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let me = unsafe { libc::geteuid() };
        verify_private_entry_meta(path, meta.file_type().is_symlink(), meta.is_file(), meta.uid(), me, meta.mode())?;
    }
    #[cfg(not(unix))]
    {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is not a regular file", path.display()),
            ));
        }
        #[cfg(windows)]
        mare_shared::private::check(path).map_err(|e| std::io::Error::new(std::io::ErrorKind::PermissionDenied, e))?;
    }
    Ok(())
}

fn ensure_private_web_dir(dir: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => mare_shared::private::create_dir_all(dir)?,
        Ok(_) => {}
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(dir)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is not a real directory", dir.display()),
            ));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    verify_private_dir(dir)
}

fn web_listener_record_path(pid: u32, instance_id: u64) -> PathBuf {
    mare_shared::paths::web_listeners_dir().join(format!("{pid}-{instance_id}.json"))
}

fn write_listener_record(path: &Path, record: &WebListenerRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        ensure_private_web_dir(parent)?;
    }
    let bytes = serde_json::to_vec(record).map_err(std::io::Error::other)?;
    mare_shared::paths::write_atomic(path, &bytes, mare_shared::paths::WriteMode::Private)
}

fn remove_listener_record(path: &Path, expected_instance_id: u64) {
    let Ok(raw) = std::fs::read_to_string(path) else { return };
    let Ok(rec) = serde_json::from_str::<WebListenerRecord>(&raw) else { return };
    if rec.pid == std::process::id() && rec.instance_id == expected_instance_id {
        let _ = std::fs::remove_file(path);
    }
}

fn scan_live_listener_records() -> Result<Vec<(PathBuf, WebListenerRecord)>, String> {
    let dir = mare_shared::paths::web_listeners_dir();
    match std::fs::symlink_metadata(&dir) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot stat listener directory {}: {e}", dir.display())),
    }
    verify_private_dir(&dir).map_err(|e| format!("listener directory {} failed security check: {e}", dir.display()))?;
    let rd = std::fs::read_dir(&dir).map_err(|e| format!("cannot read listener directory {}: {e}", dir.display()))?;
    let mut live = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if let Err(e) = verify_private_file(&path) {
            tracing::warn!(path = %path.display(), error = %e, "ignoring listener record failing ownership/mode check");
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(rec) = serde_json::from_str::<WebListenerRecord>(&raw) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if !mare_binding::peers::alive_with_start_time(rec.pid, rec.start_time) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        live.push((path, rec));
    }
    live.sort_by_key(|(_, rec)| (rec.pid, rec.instance_id));
    Ok(live)
}

/// Handle to the running host-level webserver listener.
pub(crate) struct HostWebServer {
    pub addr: SocketAddr,
    pub tls: bool,
    shutdown: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    rpc: Rc<RpcRuntime>,
    listener_record: Option<(PathBuf, u64)>,
}

impl HostWebServer {
    pub(crate) async fn shutdown(&mut self) {
        self.shutdown.cancel();
        self.rpc.manager.web().set_listener(WebListenerInfo::default());
        if let Some((path, instance_id)) = self.listener_record.take() {
            remove_listener_record(&path, instance_id);
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for HostWebServer {
    fn drop(&mut self) {
        if let Some((path, instance_id)) = self.listener_record.take() {
            remove_listener_record(&path, instance_id);
        }
    }
}

/// Print the authenticated `?token=` login URL(s) for currently running host webserver listener(s),
/// or exit non-zero when no valid live listener is active.
pub(crate) async fn web_url_cli() -> anyhow::Result<i32> {
    let live = match scan_live_listener_records() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("mare web url: {msg}");
            return Ok(1);
        }
    };
    if live.is_empty() {
        eprintln!("mare web url: no running host webserver found");
        return Ok(1);
    }

    let needs_runtime_token = live.iter().any(|(_, rec)| rec.token_source == WebTokenKind::RuntimeFile);
    let runtime_token = if needs_runtime_token {
        let token_path = mare_shared::paths::host_token_file();
        match std::fs::read_to_string(&token_path) {
            Ok(raw_tok) if !raw_tok.trim().is_empty() => Some(raw_tok.trim().to_string()),
            Ok(_) => {
                eprintln!("mare web url: host token file {} is empty", token_path.display());
                return Ok(1);
            }
            Err(e) => {
                eprintln!("mare web url: cannot read host token file {}: {e}", token_path.display());
                return Ok(1);
            }
        }
    } else {
        None
    };

    let mut urls = Vec::new();
    let mut static_urls = Vec::new();
    for (path, rec) in live {
        let validated = match validate_listener_url(&rec.url) {
            Ok(v) => v,
            Err(ListenerUrlError::Ipv6ZoneUnsupported) => {
                eprintln!(
                    "mare web url: listener in {} uses a zone-scoped IPv6 address (%zone), which standard URLs and browsers do not support; bind a non-link-local IP or authenticate with an HTTP header",
                    path.display()
                );
                continue;
            }
            Err(ListenerUrlError::Invalid) => {
                eprintln!("mare web url: ignoring invalid listener URL in {}", path.display());
                continue;
            }
        };
        if !is_local_interface_ip(validated.ip) {
            eprintln!("mare web url: ignoring listener record {} with non-local IP {}", path.display(), validated.ip);
            continue;
        }
        match rec.token_source {
            WebTokenKind::Static => {
                static_urls.push(validated.base);
            }
            WebTokenKind::RuntimeFile => {
                let Some(token) = runtime_token.as_deref() else { continue };
                urls.push(format!("{}/?token={token}", validated.base));
            }
        }
    }

    if !urls.is_empty() {
        for url in urls {
            println!("{url}");
        }
        return Ok(0);
    }
    if !static_urls.is_empty() {
        for url in static_urls {
            eprintln!(
                "mare web url: listener at {url} uses a static webserver.token not stored in the host token file; append ?token=<token>"
            );
        }
        return Ok(1);
    }
    eprintln!("mare web url: no valid running host webserver found");
    Ok(1)
}

struct WebConfig {
    host: String,
    port: u16,
    token_source: TokenSource,
    trusted_hosts: Vec<(String, Option<u16>)>,
    raw_trusted_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
    plain_http: bool,
    max_connections: usize,
    header_read_timeout: Duration,
    ws_opts: ws_rpc::WsTransportOptions,
    accept_partial_host_protect: bool,
}

#[derive(Clone)]
struct ServerCtx {
    rpc: Rc<RpcRuntime>,
    auth: Arc<AuthState>,
    trusted_hosts: Arc<Vec<(String, Option<u16>)>>,
    allowed_origins: Arc<Vec<String>>,
    is_loopback_bind: bool,
    has_tls: bool,
    header_read_timeout: Duration,
    ws_opts: ws_rpc::WsTransportOptions,
    shutdown: CancellationToken,
}

/// Whether the effective config or CLI flags request starting the host webserver.
pub(crate) fn should_start_webserver(config: &serde_json::Value, cli_web: bool, cli_listen: Option<&str>) -> bool {
    cli_web
        || cli_listen.is_some()
        || config.get("webserver").and_then(|w| w.get("enabled")).and_then(|v| v.as_bool()) == Some(true)
}

fn parse_listen_override(raw: &str) -> Result<(String, u16), String> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| format!("invalid --web-listen `{raw}`"))?;
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| format!("invalid --web-listen `{raw}`: missing :PORT"))?
            .parse::<u16>()
            .map_err(|_| format!("invalid port in --web-listen `{raw}`"))?;
        return Ok((host.to_string(), port));
    }
    let (host, port_str) =
        trimmed.rsplit_once(':').ok_or_else(|| format!("invalid --web-listen `{raw}`: expected HOST:PORT"))?;
    if host.contains(':') {
        return Err(format!("invalid --web-listen `{raw}`: wrap IPv6 hosts in [...]"));
    }
    let port = port_str.parse::<u16>().map_err(|_| format!("invalid port in --web-listen `{raw}`"))?;
    Ok((host.to_string(), port))
}

fn is_unspecified_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_unspecified() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_unspecified()),
    }
}

fn parse_concrete_ip_literal(host: &str) -> Option<IpAddr> {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    match h.split_once('%') {
        Some((addr, zone)) if !zone.is_empty() => addr.parse::<std::net::Ipv6Addr>().ok().map(IpAddr::V6),
        Some(_) => None,
        None => h.parse::<IpAddr>().ok(),
    }
}

fn is_wildcard_host(host: &str) -> bool {
    let h = host.trim().trim_start_matches('[').trim_end_matches(']');
    if h.is_empty() || h == "*" {
        return true;
    }
    parse_concrete_ip_literal(h).is_some_and(is_unspecified_ip)
}

fn append_bound_host_trust(
    trusted: &mut Vec<(String, Option<u16>)>,
    local_addr: SocketAddr,
    is_loopback_bind: bool,
    has_tls: bool,
) {
    if is_loopback_bind {
        return;
    }
    let ip_str = local_addr.ip().to_string();
    let port = local_addr.port();
    if !trusted.iter().any(|(h, p)| *h == ip_str && *p == Some(port)) {
        trusted.push((ip_str.clone(), Some(port)));
    }
    let default_port = if has_tls { 443 } else { 80 };
    if port == default_port && !trusted.iter().any(|(h, p)| *h == ip_str && p.is_none()) {
        trusted.push((ip_str, None));
    }
}

fn build_web_config(config: &serde_json::Value, cli_listen: Option<&str>) -> Result<WebConfig, String> {
    let ws = config.get("webserver").and_then(|v| v.as_object());
    let mut host = ws.and_then(|m| m.get("host")).and_then(|v| v.as_str()).unwrap_or("127.0.0.1").to_string();
    let mut port =
        ws.and_then(|m| m.get("port")).and_then(|v| v.as_u64()).and_then(|n| u16::try_from(n).ok()).unwrap_or(0);
    if let Some(raw) = cli_listen {
        let (h, p) = parse_listen_override(raw)?;
        host = h;
        port = p;
    }
    if is_wildcard_host(&host) {
        return Err(format!(
            "webserver.host `{host}` refuses wildcard bind; specify one concrete IP address or localhost"
        ));
    }
    let token_source = match ws.and_then(|m| m.get("token")) {
        None | Some(serde_json::Value::Null) => {
            TokenSource::RuntimeFile { path: mare_shared::paths::host_token_file() }
        }
        Some(serde_json::Value::Bool(false)) => {
            return Err("webserver.token = false is refused: host webserver requires token auth".into());
        }
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => TokenSource::Static(s.trim().to_string()),
        _ => return Err("webserver.token must be a non-empty string".into()),
    };
    let str_vec = |key: &str| -> Vec<String> {
        ws.and_then(|m| m.get(key))
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let raw_trusted_hosts = str_vec("trusted_hosts");
    let trusted_hosts = parse_trusted_hosts(&raw_trusted_hosts)?;
    let allowed_origins = str_vec("allowed_origins");
    let accept_partial = str_vec("accept_partial");
    let mut accept_partial_host_protect = false;
    for cap in &accept_partial {
        let caps = mare_binding::ossandbox::Cap::parse_accept(cap)?;
        if caps.contains(&mare_binding::ossandbox::Cap::FsHostProtect) {
            accept_partial_host_protect = true;
        }
    }
    let plain_http = ws.and_then(|m| m.get("plain_http")).and_then(|v| v.as_bool()).unwrap_or(false);
    let tls_cert =
        ws.and_then(|m| m.get("tls_cert")).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).map(PathBuf::from);
    let tls_key =
        ws.and_then(|m| m.get("tls_key")).and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()).map(PathBuf::from);
    if tls_cert.is_some() != tls_key.is_some() {
        return Err("webserver: tls_cert and tls_key must both be set when configuring TLS".into());
    }
    let u64_opt = |k: &str, default: u64| -> u64 {
        ws.and_then(|m| m.get(k)).and_then(|v| v.as_u64()).filter(|n| *n > 0).unwrap_or(default)
    };
    let max_connections = u64_opt("max_connections", DEFAULT_MAX_CONNECTIONS as u64) as usize;
    let header_read_timeout = Duration::from_millis(u64_opt("header_read_timeout_ms", DEFAULT_HEADER_READ_TIMEOUT_MS));
    let ws_max_message_bytes = u64_opt("ws_max_message_bytes", MAX_LINE as u64) as usize;
    let ws_max_frame_bytes =
        (u64_opt("ws_max_frame_bytes", ws_max_message_bytes as u64) as usize).min(ws_max_message_bytes);
    let mut ws_config = WebSocketConfig::default();
    ws_config.max_message_size = Some(ws_max_message_bytes);
    ws_config.max_frame_size = Some(ws_max_frame_bytes);
    let ws_opts = ws_rpc::WsTransportOptions {
        ws_config,
        ping_interval: Duration::from_millis(u64_opt("ws_ping_interval_ms", DEFAULT_WS_PING_INTERVAL_MS)),
        pong_timeout: Duration::from_millis(u64_opt("ws_pong_timeout_ms", DEFAULT_WS_PONG_TIMEOUT_MS)),
    };
    Ok(WebConfig {
        host,
        port,
        token_source,
        trusted_hosts,
        raw_trusted_hosts,
        allowed_origins,
        tls_cert,
        tls_key,
        plain_http,
        max_connections,
        header_read_timeout,
        ws_opts,
        accept_partial_host_protect,
    })
}

/// Start the host-level HTTP/WebSocket RPC listener on the current `LocalSet`.
pub(crate) async fn start_host_webserver(
    rpc: &Rc<RpcRuntime>,
    config: &serde_json::Value,
    cli_listen: Option<&str>,
) -> Result<HostWebServer, String> {
    let mut cfg = build_web_config(config, cli_listen)?;
    if matches!(cfg.token_source, TokenSource::RuntimeFile { .. }) {
        mare_binding::ossandbox::protected::producer_gate(cfg.accept_partial_host_protect)?;
    }
    let tls_acceptor = match (&cfg.tls_cert, &cfg.tls_key) {
        (Some(cert), Some(key)) => Some(tls::load_tls_acceptor(cert, key)?),
        _ => None,
    };
    let is_loopback_cfg =
        is_loopback_host(&cfg.host) || parse_concrete_ip_literal(&cfg.host).is_some_and(is_loopback_ip);
    if !is_loopback_cfg && tls_acceptor.is_none() {
        if !cfg.plain_http {
            return Err(format!(
                "refusing unencrypted non-loopback bind on `{}`: configure tls_cert/tls_key, or bind one concrete IP address literal with plain_http = true",
                cfg.host
            ));
        }
        if parse_concrete_ip_literal(&cfg.host).is_none() {
            return Err(format!(
                "refusing plain_http bind on hostname `{}`: non-loopback plain_http requires a concrete IPv4 or IPv6 address literal",
                cfg.host
            ));
        }
    }

    let listener = TcpListener::bind((cfg.host.as_str(), cfg.port))
        .await
        .map_err(|e| format!("cannot listen on {}:{}: {e}", cfg.host, cfg.port))?;
    let local_addr = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
    if is_unspecified_ip(local_addr.ip()) {
        return Err(format!("webserver bound address `{}` resolves to wildcard; refusing", local_addr.ip()));
    }
    let is_loopback_bind = is_loopback_ip(local_addr.ip());
    if !is_loopback_bind && tls_acceptor.is_none() && !cfg.plain_http {
        return Err(format!(
            "refusing unencrypted non-loopback bound address `{}`: configure tls_cert/tls_key, or bind one concrete IP address literal with plain_http = true",
            local_addr.ip()
        ));
    }
    if !is_loopback_bind && tls_acceptor.is_none() {
        tracing::warn!(
            target: "mare::web_audit",
            addr = %local_addr,
            "host webserver listening over plain HTTP on a non-loopback address"
        );
    }

    let (token_source_kind, banner_token_path) = match &cfg.token_source {
        TokenSource::RuntimeFile { path } => (WebTokenKind::RuntimeFile, Some(path.clone())),
        _ => (WebTokenKind::Static, None),
    };
    let mut auth = AuthState::new(cfg.token_source)?;
    auth.set_cookie_name(next_listener_cookie_name(local_addr.port()));
    let has_tls = tls_acceptor.is_some();
    auth.set_secure_cookie(has_tls);
    append_bound_host_trust(&mut cfg.trusted_hosts, local_addr, is_loopback_bind, has_tls);
    let auth = Arc::new(auth);

    let scheme = if has_tls { "https" } else { "http" };
    let ws_scheme = if has_tls { "wss" } else { "ws" };
    let url = format!("{scheme}://{local_addr}");
    let ws_url = format!("{ws_scheme}://{local_addr}/rpc");
    rpc.manager.web().set_listener(WebListenerInfo {
        running: true,
        url: Some(url.clone()),
        ws_url: Some(ws_url.clone()),
        host: Some(local_addr.ip().to_string()),
        port: Some(local_addr.port()),
        tls: has_tls,
        trusted_hosts: cfg.raw_trusted_hosts.clone(),
    });

    let instance_id = NEXT_LISTENER_INSTANCE_ID.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let record_path = web_listener_record_path(pid, instance_id);
    let record = WebListenerRecord {
        pid,
        start_time: mare_binding::peers::start_time(pid),
        instance_id,
        url: url.clone(),
        ws_url: ws_url.clone(),
        tls: has_tls,
        token_source: token_source_kind,
    };
    let listener_record = match write_listener_record(&record_path, &record) {
        Ok(()) => Some((record_path, instance_id)),
        Err(e) => {
            tracing::warn!(error = %e, path = %record_path.display(), "failed to write host webserver listener metadata");
            None
        }
    };

    eprintln!("mare webserver listening on {url}");
    eprintln!("  readyz: {url}/readyz");
    eprintln!("  ws rpc: {ws_url}");
    match &banner_token_path {
        Some(path) => {
            eprintln!("  auth: token file {} (run `mare web url` for login URL)", path.display());
        }
        None => {
            eprintln!("  auth: configured static token");
        }
    }

    let shutdown = CancellationToken::new();
    let ctx = ServerCtx {
        rpc: rpc.clone(),
        auth,
        trusted_hosts: Arc::new(cfg.trusted_hosts),
        allowed_origins: Arc::new(cfg.allowed_origins),
        is_loopback_bind,
        has_tls,
        header_read_timeout: cfg.header_read_timeout,
        ws_opts: cfg.ws_opts,
        shutdown: shutdown.clone(),
    };
    let conn_sem = Arc::new(Semaphore::new(cfg.max_connections));
    let shutdown_loop = shutdown.clone();
    let header_timeout = cfg.header_read_timeout;
    let task = mare_shared::txn_scope::spawn_local(async move {
        let mut accept_backoff = Duration::ZERO;
        loop {
            let Some(permit) = acquire_permit(&conn_sem, &shutdown_loop).await else {
                break;
            };
            let (stream, peer_addr) = tokio::select! {
                _ = shutdown_loop.cancelled() => break,
                res = listener.accept() => match res {
                    Ok(pair) => {
                        accept_backoff = Duration::ZERO;
                        pair
                    }
                    Err(e) => {
                        drop(permit);
                        tracing::warn!("host webserver accept error: {e}");
                        if !accept_backoff_wait(&mut accept_backoff, &shutdown_loop).await {
                            break;
                        }
                        continue;
                    }
                }
            };
            let permit = Arc::new(permit);
            let ctx_conn = ctx.clone();
            let tls_conn = tls_acceptor.clone();
            mare_shared::txn_scope::spawn_local(async move {
                if let Some(acceptor) = tls_conn {
                    let tls_res = tokio::select! {
                        _ = ctx_conn.shutdown.cancelled() => return,
                        res = tokio::time::timeout(header_timeout, acceptor.accept(stream)) => res,
                    };
                    match tls_res {
                        Ok(Ok(tls_stream)) => {
                            serve_http_conn(tls_stream, peer_addr, ctx_conn, header_timeout, permit).await
                        }
                        Ok(Err(e)) => tracing::debug!(remote = %peer_addr, error = %e, "tls handshake failed"),
                        Err(_) => tracing::debug!(remote = %peer_addr, "tls handshake timed out"),
                    }
                } else {
                    serve_http_conn(stream, peer_addr, ctx_conn, header_timeout, permit).await;
                }
            });
        }
    });

    Ok(HostWebServer { addr: local_addr, tls: has_tls, shutdown, task: Some(task), rpc: rpc.clone(), listener_record })
}

async fn serve_http_conn<S>(
    stream: S,
    peer_addr: SocketAddr,
    ctx: ServerCtx,
    header_timeout: Duration,
    permit: Arc<OwnedSemaphorePermit>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let ctx_svc = ctx.clone();
    let svc = service_fn(move |req: hyper::Request<Incoming>| {
        let ctx = ctx_svc.clone();
        let permit = permit.clone();
        async move { handle_http_request(req, peer_addr, ctx, permit).await }
    });
    let conn = http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(header_timeout)
        .serve_connection(TokioIo::new(stream), svc)
        .with_upgrades();
    tokio::pin!(conn);
    tokio::select! {
        res = &mut conn => {
            if let Err(e) = res {
                tracing::debug!(remote = %peer_addr, error = %e, "http connection ended");
            }
        }
        _ = ctx.shutdown.cancelled() => {
            conn.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, conn).await;
        }
    }
}

type BoxHttpResponse = Box<hyper::Response<ServeBody>>;

fn parse_query_pairs(raw: Option<&str>) -> Vec<(String, String)> {
    raw.map(|q| url::form_urlencoded::parse(q.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default()
}

fn collect_headers_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in headers {
        let name = k.as_str();
        if matches!(name, "authorization" | "proxy-authorization" | "x-mare-token" | "cookie") {
            continue;
        }
        if let Ok(s) = v.to_str() {
            map.entry(name.to_string())
                .and_modify(|existing| {
                    existing.push_str(", ");
                    existing.push_str(s);
                })
                .or_insert_with(|| s.to_string());
        }
    }
    map
}

async fn read_limited_body(
    incoming: Incoming,
    limit: usize,
    frame_timeout: Duration,
    total_timeout: Duration,
) -> Result<String, BoxHttpResponse> {
    let mut body = incoming;
    let mut buf = Vec::new();
    let total_deadline = tokio::time::Instant::now() + total_timeout;
    loop {
        let remaining = total_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Box::new(plain_status_response(StatusCode::REQUEST_TIMEOUT, "request body timed out")));
        }
        let step_timeout = frame_timeout.min(remaining);
        let next_frame = match tokio::time::timeout(step_timeout, body.frame()).await {
            Ok(Some(res)) => res,
            Ok(None) => break,
            Err(_) => {
                return Err(Box::new(plain_status_response(StatusCode::REQUEST_TIMEOUT, "request body timed out")));
            }
        };
        let frame = next_frame
            .map_err(|_| Box::new(plain_status_response(StatusCode::BAD_REQUEST, "failed reading request body")))?;
        if let Ok(data) = frame.into_data() {
            if buf.len().saturating_add(data.len()) > limit {
                return Err(Box::new(plain_status_response(StatusCode::PAYLOAD_TOO_LARGE, "request body too large")));
            }
            buf.extend_from_slice(&data);
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn asset_response(resolved: AssetResolve) -> hyper::Response<ServeBody> {
    match resolved {
        AssetResolve::NotMatched | AssetResolve::NotFound => plain_status_response(StatusCode::NOT_FOUND, "not found"),
        AssetResolve::MethodNotAllowed => plain_status_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
        AssetResolve::BadRequest(msg) => plain_status_response(StatusCode::BAD_REQUEST, msg),
        AssetResolve::TooLarge => plain_status_response(StatusCode::PAYLOAD_TOO_LARGE, "asset too large"),
        AssetResolve::File { content_type, bytes, head_only } => {
            let body = if head_only { empty_body() } else { full_body(Bytes::from(bytes)) };
            hyper::Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, content_type)
                .header("x-content-type-options", "nosniff")
                .header(http::header::CACHE_CONTROL, "no-store")
                .body(body)
                .unwrap_or_else(|_| plain_status_response(StatusCode::INTERNAL_SERVER_ERROR, "response error"))
        }
    }
}

#[cfg(test)]
thread_local! {
    static BEFORE_ASSET_READ: std::cell::RefCell<Option<Box<dyn FnOnce() + Send>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn before_next_asset_read(hook: impl FnOnce() + Send + 'static) {
    BEFORE_ASSET_READ.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn take_asset_read_hook() -> Option<Box<dyn FnOnce() + Send>> {
    BEFORE_ASSET_READ.with(|slot| slot.borrow_mut().take())
}

async fn resolve_static_asset(ctx: &ServerCtx, method: &str, path: &str) -> AssetResolve {
    match ctx.rpc.manager.web().prepare_asset_lookup(method, path) {
        Err(early) => early,
        Ok(prepared) => {
            let expected_asset_id = prepared.asset_id().to_string();
            #[cfg(test)]
            let hook = take_asset_read_hook();
            if let Some(host) = ctx.rpc.anchor_host()
                && let Ok(handle) = mare_binding::blocking::spawn(host.lua(), move || {
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook();
                    }
                    mare_host::web_registry::resolve_prepared_asset(prepared)
                })
            {
                match handle.await {
                    Ok(res) if ctx.rpc.manager.web().asset_still_active(path, &expected_asset_id) => res,
                    Ok(_) => AssetResolve::NotMatched,
                    Err(_) => AssetResolve::NotFound,
                }
            } else {
                AssetResolve::NotFound
            }
        }
    }
}

async fn handle_http_request(
    mut req: hyper::Request<Incoming>,
    peer_addr: SocketAddr,
    ctx: ServerCtx,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<hyper::Response<ServeBody>, hyper::Error> {
    let path = req.uri().path().to_string();
    let host_header = match validate_request_host(req.headers(), &ctx.trusted_hosts, peer_addr, &path) {
        Ok(h) => h.to_string(),
        Err(resp) => return Ok(*resp),
    };
    let expected_scheme = ctx.has_tls.then_some("https");
    if let Some(resp) =
        validate_request_origin(req.headers(), &host_header, &ctx.allowed_origins, peer_addr, &path, expected_scheme)
    {
        return Ok(resp);
    }

    if (*req.method() == Method::GET || *req.method() == Method::HEAD) && (path == "/readyz" || path == "/healthz") {
        let body = if *req.method() == Method::HEAD { empty_body() } else { full_body(r#"{"status":"ready"}"#) };
        return Ok(hyper::Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::CACHE_CONTROL, "no-store")
            .body(body)
            .unwrap_or_else(|_| plain_status_response(StatusCode::OK, "ready")));
    }

    let query = parse_query_pairs(req.uri().query());
    let is_ws_upgrade = is_websocket_upgrade_request(req.method(), req.headers());
    let (auth_outcome, initial_token) =
        ctx.auth.authenticate_with_token(req.method(), &path, req.headers(), &query, is_ws_upgrade, peer_addr);
    match auth_outcome {
        AuthOutcome::Allowed => {}
        AuthOutcome::Redirect(resp) | AuthOutcome::Denied(resp) => return Ok(resp),
    }

    if is_ws_upgrade {
        if path != "/rpc" && path != "/" {
            return Ok(plain_status_response(StatusCode::NOT_FOUND, "unknown websocket endpoint"));
        }
        let mut versions = req.headers().get_all("sec-websocket-version").iter();
        let valid_version =
            versions.next().and_then(|v| v.to_str().ok()).map(str::trim) == Some("13") && versions.next().is_none();
        if !valid_version {
            return Ok(plain_status_response(StatusCode::BAD_REQUEST, "unsupported Sec-WebSocket-Version"));
        }
        let mut keys = req.headers().get_all("sec-websocket-key").iter();
        let Some(ws_key) = keys.next().and_then(|v| v.to_str().ok()).map(str::trim).map(str::to_string) else {
            return Ok(plain_status_response(StatusCode::BAD_REQUEST, "missing Sec-WebSocket-Key"));
        };
        if keys.next().is_some()
            || !base64::engine::general_purpose::STANDARD.decode(ws_key.as_bytes()).is_ok_and(|raw| raw.len() == 16)
        {
            return Ok(plain_status_response(StatusCode::BAD_REQUEST, "invalid Sec-WebSocket-Key"));
        }
        let is_local_conn =
            is_loopback_ip(peer_addr.ip()) && is_valid_loopback_host_header(&host_header) && ctx.is_loopback_bind;
        let origin = req.headers().get(http::header::ORIGIN).and_then(|v| v.to_str().ok()).map(str::to_string);
        let on_upgrade = hyper::upgrade::on(&mut req);
        ws_rpc::spawn_ws_rpc(
            ctx.rpc.clone(),
            on_upgrade,
            ctx.ws_opts.clone(),
            is_local_conn,
            peer_addr,
            origin,
            ctx.auth.clone(),
            initial_token,
            ctx.shutdown.clone(),
            permit,
        );
        let accept = derive_accept_key(ws_key.as_bytes());
        return Ok(hyper::Response::builder()
            .status(StatusCode::SWITCHING_PROTOCOLS)
            .header(http::header::CONNECTION, "Upgrade")
            .header(http::header::UPGRADE, "websocket")
            .header("sec-websocket-accept", accept)
            .body(empty_body())
            .unwrap_or_else(|_| plain_status_response(StatusCode::INTERNAL_SERVER_ERROR, "upgrade response error")));
    }

    let method = req.method().as_str().to_string();
    if let Some(handler) = ctx.rpc.manager.web().match_route(&method, &path) {
        let headers_map = collect_headers_map(req.headers());
        let query_map: BTreeMap<String, String> = query.into_iter().collect();
        let total_timeout = ctx.header_read_timeout.saturating_mul(4);
        let body =
            match read_limited_body(req.into_body(), DEFAULT_MAX_BODY_BYTES, ctx.header_read_timeout, total_timeout)
                .await
            {
                Ok(b) => b,
                Err(resp) => return Ok(*resp),
            };
        let route_req = WebRouteRequest { method, path, query: query_map, headers: headers_map, body };
        return match handler(route_req).await {
            Ok(res) => {
                let status = StatusCode::from_u16(res.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                let mut builder = hyper::Response::builder().status(status);
                for (k, v) in res.headers {
                    if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(&v)) {
                        builder = builder.header(name, val);
                    }
                }
                Ok(builder
                    .body(full_body(res.body))
                    .unwrap_or_else(|_| plain_status_response(StatusCode::INTERNAL_SERVER_ERROR, "invalid response")))
            }
            Err(err) => {
                tracing::warn!(error = %err, "registered webserver route handler failed");
                Ok(plain_status_response(StatusCode::BAD_GATEWAY, "route handler failed"))
            }
        };
    }

    Ok(asset_response(resolve_static_asset(&ctx, &method, &path).await))
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use mare_binding::http_serve::host_transport::is_valid_host_header;

    use super::*;

    #[test]
    fn non_loopback_concrete_bind_adds_own_host_header_without_escalating_local_conn() {
        let mut trusted = Vec::new();
        let addr_v4 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 8443);
        append_bound_host_trust(&mut trusted, addr_v4, false, true);
        assert!(is_valid_host_header("192.168.1.5:8443", &trusted));
        assert!(!is_valid_host_header("192.168.1.5:9999", &trusted));
        assert!(!is_valid_host_header("evil.example:8443", &trusted));

        let mut trusted_443 = Vec::new();
        let addr_443 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 443);
        append_bound_host_trust(&mut trusted_443, addr_443, false, true);
        assert!(is_valid_host_header("192.168.1.5", &trusted_443));

        let mut trusted_v6 = Vec::new();
        let addr_v6 = SocketAddr::new(IpAddr::V6("fd00::1".parse::<Ipv6Addr>().unwrap()), 8443);
        append_bound_host_trust(&mut trusted_v6, addr_v6, false, true);
        assert!(is_valid_host_header("[fd00::1]:8443", &trusted_v6));

        // Concrete non-loopback bind must keep is_loopback_ip(local_addr) == false so is_local_conn stays false.
        assert!(!is_loopback_ip(addr_v4.ip()));
        assert!(!is_valid_loopback_host_header("192.168.1.5:8443"));
    }

    #[test]
    fn wildcard_check_rejects_ipv4_mapped_unspecified_and_headers_join_duplicates() {
        assert!(is_wildcard_host("0.0.0.0"));
        assert!(is_wildcard_host("[::]"));
        assert!(is_wildcard_host("[::ffff:0.0.0.0]"));
        assert!(!is_wildcard_host("127.0.0.1"));

        let mut hm = HeaderMap::new();
        hm.append("x-custom", HeaderValue::from_static("one"));
        hm.append("x-custom", HeaderValue::from_static("two"));
        hm.append("authorization", HeaderValue::from_static("Bearer secret"));
        hm.append("x-mare-token", HeaderValue::from_static("tok"));
        hm.append("cookie", HeaderValue::from_static("mare_auth=abc"));
        hm.append("proxy-authorization", HeaderValue::from_static("Basic xyz"));
        let collected = collect_headers_map(&hm);
        assert_eq!(collected.get("x-custom").map(String::as_str), Some("one, two"));
        for stripped in ["authorization", "x-mare-token", "cookie", "proxy-authorization"] {
            assert!(!collected.contains_key(stripped), "header {stripped} must be stripped");
        }
    }

    #[tokio::test]
    async fn slowloris_body_read_times_out_with_408() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let svc = service_fn(|req: hyper::Request<Incoming>| async move {
                let res = match read_limited_body(
                    req.into_body(),
                    DEFAULT_MAX_BODY_BYTES,
                    Duration::from_millis(60),
                    Duration::from_millis(240),
                )
                .await
                {
                    Ok(_) => plain_status_response(StatusCode::OK, "ok"),
                    Err(resp) => *resp,
                };
                Ok::<_, hyper::Error>(res)
            });
            let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), svc).await;
        });

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"POST /hook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: 100\r\n\r\npartial")
            .await
            .unwrap();
        let mut buf = vec![0u8; 1024];
        let n = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await.unwrap().unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.starts_with("HTTP/1.1 408"), "slowloris body must return 408: {resp}");
        drop(client);
        let _ = srv.await;
    }

    #[test]
    fn listener_url_zone_and_private_meta_validation() {
        assert_eq!(parse_concrete_ip_literal("192.0.2.10"), Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))));
        assert_eq!(parse_concrete_ip_literal("192.0.2.10%eth0"), None);
        assert!(parse_concrete_ip_literal("fe80::1%eth0").is_some());
        assert!(parse_concrete_ip_literal("[fe80::1%eth0]").is_some());
        assert_eq!(parse_concrete_ip_literal("fe80::1%"), None);

        assert_eq!(
            validate_listener_url("http://127.0.0.1:7432"),
            Ok(ValidatedListenerUrl { base: "http://127.0.0.1:7432".into(), ip: IpAddr::V4(Ipv4Addr::LOCALHOST) })
        );
        assert_eq!(
            validate_listener_url("http://127.0.0.1:80"),
            Ok(ValidatedListenerUrl { base: "http://127.0.0.1:80".into(), ip: IpAddr::V4(Ipv4Addr::LOCALHOST) })
        );
        assert_eq!(
            validate_listener_url("https://127.0.0.1:443"),
            Ok(ValidatedListenerUrl { base: "https://127.0.0.1:443".into(), ip: IpAddr::V4(Ipv4Addr::LOCALHOST) })
        );
        assert_eq!(
            validate_listener_url("http://[::1]:80"),
            Ok(ValidatedListenerUrl { base: "http://[::1]:80".into(), ip: IpAddr::V6(Ipv6Addr::LOCALHOST) })
        );
        assert_eq!(
            validate_listener_url("https://[::1]:443"),
            Ok(ValidatedListenerUrl { base: "https://[::1]:443".into(), ip: IpAddr::V6(Ipv6Addr::LOCALHOST) })
        );
        assert_eq!(validate_listener_url("http://127.0.0.1:0"), Err(ListenerUrlError::Invalid));
        assert_eq!(validate_listener_url("http://[fe80::1%eth0]:7432"), Err(ListenerUrlError::Ipv6ZoneUnsupported));
        assert_eq!(validate_listener_url("https://[fe80::1%3]:8443"), Err(ListenerUrlError::Ipv6ZoneUnsupported));
        assert_eq!(validate_listener_url("http://evil.example:7432"), Err(ListenerUrlError::Invalid));
        assert_eq!(validate_listener_url("http://127.0.0.1:7432/path"), Err(ListenerUrlError::Invalid));
        assert_eq!(validate_listener_url("http://0.0.0.0:7432"), Err(ListenerUrlError::Invalid));

        #[cfg(unix)]
        {
            let p = Path::new("/tmp/web");
            assert!(verify_private_entry_meta(p, false, true, 1000, 1000, 0o40700).is_ok());
            assert!(verify_private_entry_meta(p, false, true, 1000, 1000, 0o100600).is_ok());
            assert!(verify_private_entry_meta(p, true, true, 1000, 1000, 0o40700).is_err());
            assert!(verify_private_entry_meta(p, false, false, 1000, 1000, 0o40700).is_err());
            assert!(verify_private_entry_meta(p, false, true, 1001, 1000, 0o40700).is_err());
            assert!(verify_private_entry_meta(p, false, true, 1000, 1000, 0o40755).is_err());
            assert!(verify_private_entry_meta(p, false, true, 1000, 1000, 0o100644).is_err());
        }
    }
}
