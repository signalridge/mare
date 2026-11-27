//! `mare.http.serve`: loopback-only HTTP/1.1, streaming/SSE, and WebSocket server primitive.
//!
//! Built on `hyper` (HTTP/1.1 + upgrades) and `tokio-tungstenite` (WebSocket framing).
//! Defaults to `127.0.0.1` (ephemeral port `0` if omitted), rejects non-loopback bind hosts,
//! and validates incoming `Host` headers against loopback names/IPs to prevent DNS rebinding.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::{Body, Frame, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use mlua::{Lua, MultiValue, Table, UserData, UserDataMethods, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_util::sync::CancellationToken;

use crate::task::opt_token;
use mare_shared::lua::{nil_err, rt_err};

#[path = "http_serve_host.rs"]
mod host;
#[path = "http_serve_stream.rs"]
mod stream;
#[path = "http_serve_support.rs"]
mod support;

pub use host::{is_loopback_host, is_valid_loopback_host_header};
use host::{is_loopback_ip, is_valid_host_header, parse_trusted_hosts};
use stream::{PeerGone, PumpIo, collect_headers_vec, peer_gone, wait_handler_abort};
pub(super) use support::CancelOnDrop;
use support::{
    AbortOnDrop, ReceiverStreamBody, accept_next, acquire_permit, empty_body, full_body, header_contains_token,
    parse_limit, plain_status_response, serve_with_binder,
};

/// Shared HTTP/WebSocket host-server building blocks reused by `mare-cli`'s host webserver.
pub mod host_transport {
    pub use super::auth::{AuthOutcome, AuthState, TokenSource, subscribe_token_rotation, validate_request_origin};
    pub use super::host::{
        is_loopback_host, is_loopback_ip, is_valid_host_header, is_valid_loopback_host_header, parse_host_header,
        parse_trusted_hosts,
    };
    pub use super::next_listener_cookie_name;
    pub use super::support::{
        accept_backoff_wait, acquire_permit, empty_body, full_body, header_contains_token, plain_status_response,
    };
    pub use super::{
        ACCEPT_BACKOFF_INITIAL_MS, ACCEPT_BACKOFF_MAX_MS, ServeBody, is_websocket_upgrade_request,
        validate_request_host,
    };
}

const DEFAULT_MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENCY: usize = 64;
const DEFAULT_MAX_CONNECTIONS: usize = 256;
const DEFAULT_CLOSE_GRACE_MS: u64 = 5000;
const MAX_CLOSE_GRACE_MS: u64 = 3_600_000;
const DEFAULT_BODY_STALL_MS: u64 = 60_000;
const MAX_BODY_STALL_MS: u64 = 3_600_000;
/// How often a stalled request-body pump re-checks the socket for a peer disconnect.
const PEER_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
const DEFAULT_HEADER_READ_TIMEOUT_MS: u64 = 30_000;
const MAX_HEADER_READ_TIMEOUT_MS: u64 = 3_600_000;
const DEFAULT_WS_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const DEFAULT_WS_MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const ACCEPT_BACKOFF_INITIAL_MS: u64 = 10;
pub const ACCEPT_BACKOFF_MAX_MS: u64 = 200;
const STREAM_QUEUE_CAPACITY: usize = 16;
const WS_QUEUE_CAPACITY: usize = 64;

const UPGRADE_NONE: u8 = 0;
const UPGRADE_IN_PROGRESS: u8 = 1;
const UPGRADE_SUCCEEDED: u8 = 2;
const UPGRADE_FAILED: u8 = 3;

pub type ServeBody = BoxBody<Bytes, std::io::Error>;

#[derive(Clone, Default)]
struct ActiveTasks {
    inner: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl ActiveTasks {
    fn push(&self, handle: JoinHandle<()>) {
        if let Ok(mut g) = self.inner.lock() {
            g.retain(|h| !h.is_finished());
            g.push(handle);
        }
    }

    fn drain(&self) -> Vec<JoinHandle<()>> {
        self.inner.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default()
    }

    #[cfg(test)]
    fn unfinished_count(&self) -> usize {
        self.inner.lock().map(|g| g.iter().filter(|h| !h.is_finished()).count()).unwrap_or(0)
    }
}

#[path = "http_serve_auth.rs"]
mod auth;
#[path = "http_serve_req.rs"]
mod req;
#[path = "http_serve_ws.rs"]
mod ws;
use auth::{AuthOutcome, AuthState, TokenSource, validate_request_origin};
pub use auth::{
    ensure_host_token_at, generate_token, rotate_host_token, rotate_host_token_at, rotate_host_token_with_partial,
};
use req::{HttpRequest, ReqBodyState, UpgradeState, build_lua_response};
pub use ws::ServerWs;

#[derive(Clone)]
struct ServeCtx {
    lua: Lua,
    ectx: Option<Table>,
    handler: mlua::Function,
    auth: Arc<AuthState>,
    allowed_origins: Arc<Vec<String>>,
    trusted_hosts: Arc<Vec<(String, Option<u16>)>>,
    max_body_bytes: usize,
    close_grace: std::time::Duration,
    body_stall: std::time::Duration,
    header_read_timeout: std::time::Duration,
    ws_config: WebSocketConfig,
    shutdown: CancellationToken,
    tasks: ActiveTasks,
    req_sem: Arc<Semaphore>,
    #[cfg(test)]
    test_hooks: Option<Arc<ws::ServeTestHooks>>,
}

#[derive(Clone)]
struct ConnCtx {
    peer_addr: SocketAddr,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    conn_cancel: CancellationToken,
    upgrade_phase: Arc<AtomicU8>,
    peer_eof: ws::PeerEof,
}

/// Validate and extract the HTTP `Host` header against loopback/trusted host rules.
pub fn validate_request_host<'a>(
    headers: &'a HeaderMap,
    trusted_hosts: &[(String, Option<u16>)],
    peer_addr: SocketAddr,
    path: &str,
) -> Result<&'a str, Box<hyper::Response<ServeBody>>> {
    let mut host_iter = headers.get_all(http::header::HOST).iter();
    let Some(host_val) = host_iter.next() else {
        tracing::warn!(target: "mare::http_auth", remote = %peer_addr, path = %path, reason = "missing_host", "http request rejected by host fence");
        return Err(Box::new(plain_status_response(StatusCode::FORBIDDEN, "missing Host header")));
    };
    if host_iter.next().is_some() {
        tracing::warn!(target: "mare::http_auth", remote = %peer_addr, path = %path, reason = "duplicate_host", "http request rejected by host fence");
        return Err(Box::new(plain_status_response(StatusCode::BAD_REQUEST, "duplicate Host header")));
    }
    let Ok(host_str) = host_val.to_str() else {
        tracing::warn!(target: "mare::http_auth", remote = %peer_addr, path = %path, reason = "invalid_host", "http request rejected by host fence");
        return Err(Box::new(plain_status_response(StatusCode::FORBIDDEN, "invalid Host header")));
    };
    if !is_valid_host_header(host_str, trusted_hosts) {
        tracing::warn!(target: "mare::http_auth", remote = %peer_addr, path = %path, reason = "forbidden_host", "http request rejected by host fence");
        return Err(Box::new(plain_status_response(StatusCode::FORBIDDEN, "forbidden Host header")));
    }
    Ok(host_str)
}

/// Return `true` when HTTP headers request a WebSocket upgrade.
pub fn is_websocket_upgrade_request(method: &http::Method, headers: &HeaderMap) -> bool {
    *method == http::Method::GET
        && header_contains_token(headers, http::header::CONNECTION.as_str(), "upgrade")
        && header_contains_token(headers, http::header::UPGRADE.as_str(), "websocket")
}

async fn handle_http_request(
    mut req: hyper::Request<Incoming>,
    ctx: ServeCtx,
    conn_ctx: ConnCtx,
) -> Result<hyper::Response<ServeBody>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let host_str = match validate_request_host(req.headers(), &ctx.trusted_hosts, conn_ctx.peer_addr, &path) {
        Ok(h) => h,
        Err(resp) => return Ok(*resp),
    };
    if let Some(resp) =
        validate_request_origin(req.headers(), host_str, &ctx.allowed_origins, conn_ctx.peer_addr, &path, None)
    {
        return Ok(resp);
    }

    let method = req.method().as_str().to_string();
    let raw_query = req.uri().query().map(|q| q.to_string());
    let query: Vec<(String, String)> = raw_query
        .as_deref()
        .map(|q| url::form_urlencoded::parse(q.as_bytes()).map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        .unwrap_or_default();

    let is_ws_upgrade = is_websocket_upgrade_request(req.method(), req.headers())
        && req.headers().get("sec-websocket-version").and_then(|v| v.to_str().ok()) == Some("13")
        && req.headers().contains_key("sec-websocket-key");

    match ctx.auth.authenticate(req.method(), &path, req.headers(), &query, is_ws_upgrade, conn_ctx.peer_addr) {
        AuthOutcome::Allowed => {}
        AuthOutcome::Redirect(resp) | AuthOutcome::Denied(resp) => return Ok(resp),
    }

    let headers_vec = collect_headers_vec(req.headers());

    let (on_upgrade, ws_key) = if is_ws_upgrade {
        let key = req.headers().get("sec-websocket-key").and_then(|v| v.to_str().ok()).map(|s| s.to_string());
        let upg = hyper::upgrade::on(&mut req);
        (Some(upg), key)
    } else {
        (None, None)
    };

    #[cfg(test)]
    if let Some(hooks) = &ctx.test_hooks
        && ctx.req_sem.available_permits() == 0
    {
        hooks.on_req_permit_wait.notify_waiters();
    }
    let Some(permit) = acquire_permit(&ctx.req_sem, &conn_ctx.conn_cancel).await else {
        return Ok(plain_status_response(StatusCode::SERVICE_UNAVAILABLE, "server closed"));
    };

    let incoming = req.into_body();
    let (resp_tx, resp_rx) = oneshot::channel::<hyper::Response<ServeBody>>();
    let upgrade_phase = conn_ctx.upgrade_phase;
    let ws_drained = Arc::new((AtomicBool::new(false), tokio::sync::Notify::new()));
    let upgrade_state = Arc::new(AsyncMutex::new(UpgradeState {
        on_upgrade,
        ws_key,
        ws_config: ctx.ws_config,
        resp_tx: Some(resp_tx),
        conn_permit: Some(conn_ctx.permit),
        upgrade_phase: upgrade_phase.clone(),
        conn_cancel: conn_ctx.conn_cancel.clone(),
        ws_drained: ws_drained.clone(),
        peer_eof: conn_ctx.peer_eof,
        ws_stall_limit: ctx.body_stall,
        #[cfg(test)]
        test_hooks: ctx.test_hooks.clone(),
    }));

    let req_cancel = conn_ctx.conn_cancel.child_token();
    let mut cancel_guard = CancelOnDrop::new(req_cancel.clone());
    let body_state = Arc::new(AsyncMutex::new(ReqBodyState {
        incoming: Some(incoming),
        max_body_bytes: ctx.max_body_bytes,
        read_bytes: 0,
    }));
    let req_ud = HttpRequest {
        method,
        path,
        raw_query,
        query,
        headers: headers_vec,
        cancel: req_cancel.clone(),
        tasks: ctx.tasks.clone(),
        body: body_state.clone(),
        upgrade: upgrade_state.clone(),
    };

    let lua_c = ctx.lua.clone();
    let ectx_c = ctx.ectx.clone();
    let handler_c = ctx.handler.clone();
    let tasks_c = ctx.tasks.clone();
    let shutdown_c = ctx.shutdown.clone();
    let close_grace = ctx.close_grace;
    let handler_handle = mare_shared::txn_scope::spawn_local(async move {
        let cancel_wait = req_cancel.clone();
        let res = tokio::select! {
            biased;
            _ = wait_handler_abort(&shutdown_c, &cancel_wait, &upgrade_phase, &ws_drained, close_grace) => {
                body_state.lock().await.incoming.take();
                let mut ug = upgrade_state.lock().await;
                ug.conn_permit.take();
                return;
            }
            r = mare_shared::exec_ctx::call_in::<Value>(&lua_c, ectx_c.clone(), handler_c, req_ud) => r,
        };
        body_state.lock().await.incoming.take();
        let resp = match res {
            Ok(val) => match build_lua_response(&lua_c, ectx_c, val, req_cancel, &tasks_c, permit) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("mare.http.serve response error: {e}");
                    plain_status_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
                }
            },
            Err(e) => {
                tracing::warn!("mare.http.serve handler error: {e}");
                plain_status_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error")
            }
        };
        let mut ug = upgrade_state.lock().await;
        ug.conn_permit.take();
        if let Some(tx) = ug.resp_tx.take() {
            let _ = tx.send(resp);
        }
    });
    ctx.tasks.push(handler_handle);

    let resp = tokio::select! {
        biased;
        _ = ctx.shutdown.cancelled() => plain_status_response(StatusCode::SERVICE_UNAVAILABLE, "server closed"),
        r = resp_rx => match r {
            Ok(r) if !ctx.shutdown.is_cancelled() => {
                cancel_guard.disarm();
                r
            }
            Ok(_) => plain_status_response(StatusCode::SERVICE_UNAVAILABLE, "server closed"),
            Err(_) => plain_status_response(StatusCode::INTERNAL_SERVER_ERROR, "internal server error"),
        }
    };
    Ok(resp)
}

async fn serve_stream(
    stream: TcpStream,
    peer_addr: SocketAddr,
    ctx: ServeCtx,
    conn_permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) {
    let shutdown = ctx.shutdown.clone();
    let conn_cancel = shutdown.child_token();
    let mut conn_guard = CancelOnDrop::new(conn_cancel.clone());
    let upgrade_phase = Arc::new(AtomicU8::new(UPGRADE_NONE));
    let (mut read_half, write_half) = stream.into_split();
    let (mut duplex_tx, duplex_rx) = tokio::io::duplex(64 * 1024);

    let peer_eof: ws::PeerEof = Arc::new((AtomicBool::new(false), tokio::sync::Notify::new()));
    let conn_cancel_pump = conn_cancel.clone();
    let upgrade_phase_pump = upgrade_phase.clone();
    let peer_eof_pump = peer_eof.clone();
    let body_stall = ctx.body_stall;
    #[cfg(test)]
    let test_hooks_pump = ctx.test_hooks.clone();
    let mut pump_guard = AbortOnDrop::new(mare_shared::txn_scope::spawn_local(async move {
        let mut buf = [0u8; 8192];
        let mut half_closed = false;
        loop {
            let n = tokio::select! {
                biased;
                _ = conn_cancel_pump.cancelled() => break,
                res = read_half.read(&mut buf) => match res {
                    res @ (Ok(0) | Err(_)) => {
                        // Only a read error (RST) is a disconnect. A clean EOF is a half-close: the
                        // client may still wait for the response, so let hyper finish the request
                        // (`half_close(true)`); a peer that fully closed is noticed when writing.
                        if res.is_err() && upgrade_phase_pump.load(Ordering::Acquire) != UPGRADE_SUCCEEDED {
                            conn_cancel_pump.cancel();
                        }
                        // Lets a backpressured WebSocket reader drain the bounded remainder and
                        // observe the end of the stream.
                        peer_eof_pump.0.store(true, Ordering::Release);
                        peer_eof_pump.1.notify_waiters();
                        half_closed = res.is_ok();
                        #[cfg(test)]
                        if let Some(hooks) = &test_hooks_pump
                            && half_closed
                        {
                            hooks.on_peer_eof_read.notify_waiters();
                        }
                        break;
                    }
                    Ok(n) => n,
                },
            };
            let mut written = 0;
            while written < n {
                let mut write_fut = std::pin::pin!(duplex_tx.write(&buf[written..n]));
                #[cfg(test)]
                if let Some(hooks) = &test_hooks_pump {
                    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                    if let std::task::Poll::Ready(res) = std::future::Future::poll(write_fut.as_mut(), &mut cx) {
                        match res {
                            Ok(0) | Err(_) => return,
                            Ok(w) => {
                                written += w;
                                continue;
                            }
                        }
                    }
                    hooks.on_duplex_stalled.notify_waiters();
                }
                // Normal backpressure: wait for the handler to drain the duplex. The connection is
                // only closed when the peer disconnects or the (configurable, generous) body stall
                // timeout expires; both paths cancel the connection explicitly, so request bytes are
                // never silently discarded.
                let mut stall = std::pin::pin!(tokio::time::sleep(body_stall));
                let mut fin_seen = false;
                loop {
                    tokio::select! {
                        biased;
                        _ = conn_cancel_pump.cancelled() => return,
                        res = &mut write_fut => match res {
                            Ok(0) | Err(_) => return,
                            Ok(w) => {
                                written += w;
                                break;
                            }
                        },
                        _ = &mut stall, if upgrade_phase_pump.load(Ordering::Acquire) != UPGRADE_SUCCEEDED => {
                            conn_cancel_pump.cancel();
                            return;
                        }
                        gone = peer_gone(
                            &read_half,
                            fin_seen,
                            #[cfg(test)]
                            test_hooks_pump.as_deref(),
                        ) => {
                            peer_eof_pump.0.store(true, Ordering::Release);
                            peer_eof_pump.1.notify_waiters();
                            if gone == PeerGone::Reset && upgrade_phase_pump.load(Ordering::Acquire) != UPGRADE_SUCCEEDED {
                                conn_cancel_pump.cancel();
                                return;
                            }
                            // FIN (half-close) or upgraded: keep delivering the pending bytes; only
                            // a later reset (or the stall timeout) tears the connection down.
                            fin_seen = true;
                            #[cfg(test)]
                            if let Some(hooks) = &test_hooks_pump {
                                hooks.on_peer_half_close.notify_waiters();
                            }
                        }
                    }
                }
            }
        }
        if half_closed && upgrade_phase_pump.load(Ordering::Acquire) != UPGRADE_SUCCEEDED {
            // Half-closed HTTP request: signal EOF to hyper, but keep watching the socket so a
            // later reset still cancels a handler that has not written anything yet.
            drop(duplex_tx);
            tokio::select! {
                biased;
                _ = conn_cancel_pump.cancelled() => {}
                gone = peer_gone(
                    &read_half,
                    true,
                    #[cfg(test)]
                    test_hooks_pump.as_deref(),
                ) => {
                    #[cfg(test)]
                    if let Some(hooks) = &test_hooks_pump
                        && gone == PeerGone::Reset
                    {
                        hooks.on_half_closed_reset.notify_waiters();
                    }
                    if gone == PeerGone::Reset && upgrade_phase_pump.load(Ordering::Acquire) != UPGRADE_SUCCEEDED {
                        conn_cancel_pump.cancel();
                    }
                }
            }
        }
    }));

    let tasks_c = ctx.tasks.clone();
    let conn_ctx = ConnCtx {
        peer_addr,
        permit: conn_permit,
        conn_cancel: conn_cancel.clone(),
        upgrade_phase: upgrade_phase.clone(),
        peer_eof,
    };
    #[cfg(test)]
    let write_half = ws::TestWriteHalf {
        inner: write_half,
        hooks: ctx.test_hooks.clone(),
        upgrade_phase: upgrade_phase.clone(),
        upgraded_phase_val: UPGRADE_SUCCEEDED,
    };
    let header_read_timeout = ctx.header_read_timeout;
    let svc = service_fn(move |req| handle_http_request(req, ctx.clone(), conn_ctx.clone()));
    let io = PumpIo { reader: duplex_rx, writer: write_half };
    let mut builder = http1::Builder::new();
    builder.half_close(true).timer(TokioTimer::new()).header_read_timeout(header_read_timeout);
    let mut conn = builder.serve_connection(TokioIo::new(io), svc).with_upgrades();
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {}
        _ = conn_cancel.cancelled() => {}
        res = &mut conn => {
            if let Err(e) = res {
                tracing::debug!("mare.http.serve connection ended: {e}");
            }
        }
    }
    let phase = upgrade_phase.load(Ordering::Acquire);
    if phase == UPGRADE_IN_PROGRESS || phase == UPGRADE_SUCCEEDED {
        conn_guard.disarm();
        if let Some(handle) = pump_guard.disarm() {
            tasks_c.push(handle);
        }
    }
}

/// Handle returned by `mare.http.serve`.
pub struct HttpServer {
    addr: SocketAddr,
    shutdown: CancellationToken,
    task: Arc<AsyncMutex<Option<JoinHandle<()>>>>,
    tasks: ActiveTasks,
}

#[cfg(test)]
async fn serve_with_hooks(lua: Lua, opts: Table, hooks: Arc<ws::ServeTestHooks>) -> mlua::Result<MultiValue> {
    serve_with_binder_and_hooks(
        lua,
        opts,
        |host, port| async move {
            let listener = TcpListener::bind((host.as_str(), port))
                .await
                .map_err(|e| format!("cannot listen on {host}:{port}: {e}"))?;
            let local_addr = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
            Ok((listener, local_addr))
        },
        mare_shared::paths::host_token_file(),
        Some(hooks),
    )
    .await
}

#[cfg(test)]
async fn serve_with_token_path(lua: Lua, opts: Table, token_path: PathBuf) -> mlua::Result<MultiValue> {
    serve_with_binder_and_hooks(
        lua,
        opts,
        |host, port| async move {
            let listener = TcpListener::bind((host.as_str(), port))
                .await
                .map_err(|e| format!("cannot listen on {host}:{port}: {e}"))?;
            let local_addr = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
            Ok((listener, local_addr))
        },
        token_path,
        None,
    )
    .await
}

fn parse_token_source(opts: &Table, default_token_path: PathBuf) -> mlua::Result<Result<TokenSource, String>> {
    match opts.get::<Value>("token")? {
        Value::Nil => Ok(Ok(TokenSource::RuntimeFile { path: default_token_path })),
        Value::Boolean(false) => Ok(Ok(TokenSource::Disabled)),
        Value::Boolean(true) => Ok(Err("token must be a non-empty string or false to disable".into())),
        Value::String(s) => {
            let s = s.to_str()?;
            if s.is_empty() {
                Ok(Err("token must be a non-empty string or false to disable".into()))
            } else {
                Ok(Ok(TokenSource::Static(s.to_string())))
            }
        }
        _ => Ok(Err("token must be a string or false".into())),
    }
}

async fn serve_with_binder_and_hooks<F, Fut>(
    lua: Lua,
    opts: Table,
    binder: F,
    default_token_path: PathBuf,
    #[cfg(test)] test_hooks: Option<Arc<ws::ServeTestHooks>>,
) -> mlua::Result<MultiValue>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: std::future::Future<Output = Result<(TcpListener, SocketAddr), String>>,
{
    let host: String = opts.get::<Option<String>>("host")?.unwrap_or_else(|| "127.0.0.1".into());
    let port: u16 = opts.get::<Option<u16>>("port")?.unwrap_or(0);
    let handler: mlua::Function = opts.get("handler")?;
    let max_body_bytes = opts.get::<Option<usize>>("max_body_bytes")?.unwrap_or(DEFAULT_MAX_BODY_BYTES);
    let max_concurrency = match parse_limit(&opts, "max_concurrency", DEFAULT_MAX_CONCURRENCY)? {
        Ok(n) => n,
        Err(msg) => return nil_err(&lua, msg),
    };
    let max_connections = match parse_limit(&opts, "max_connections", max_concurrency.max(DEFAULT_MAX_CONNECTIONS))? {
        Ok(n) => n,
        Err(msg) => return nil_err(&lua, msg),
    };
    let close_grace_ms = match opts.get::<Option<i64>>("close_grace_ms")? {
        Some(n) if n < 0 => return nil_err(&lua, "close_grace_ms must be non-negative"),
        Some(n) => u64::try_from(n).unwrap_or(u64::MAX).min(MAX_CLOSE_GRACE_MS),
        None => DEFAULT_CLOSE_GRACE_MS,
    };
    let body_stall_ms = match opts.get::<Option<i64>>("body_stall_ms")? {
        Some(n) if n < 0 => return nil_err(&lua, "body_stall_ms must be non-negative"),
        Some(n) => u64::try_from(n).unwrap_or(u64::MAX).min(MAX_BODY_STALL_MS),
        None => DEFAULT_BODY_STALL_MS,
    };
    let header_read_timeout_ms = match opts.get::<Option<i64>>("header_read_timeout_ms")? {
        Some(n) if n <= 0 => return nil_err(&lua, "header_read_timeout_ms must be at least 1"),
        Some(n) => u64::try_from(n).unwrap_or(u64::MAX).min(MAX_HEADER_READ_TIMEOUT_MS),
        None => DEFAULT_HEADER_READ_TIMEOUT_MS,
    };
    let ws_max_message_bytes = match parse_limit(&opts, "ws_max_message_bytes", DEFAULT_WS_MAX_MESSAGE_BYTES)? {
        Ok(n) => n,
        Err(msg) => return nil_err(&lua, msg),
    };
    let ws_max_frame_bytes = match parse_limit(&opts, "ws_max_frame_bytes", DEFAULT_WS_MAX_FRAME_BYTES)? {
        Ok(n) => n.min(ws_max_message_bytes),
        Err(msg) => return nil_err(&lua, msg),
    };
    let allowed_origins = opts.get::<Option<Vec<String>>>("allowed_origins")?.unwrap_or_default();
    let raw_trusted_hosts = opts.get::<Option<Vec<String>>>("trusted_hosts")?.unwrap_or_default();
    let trusted_hosts = match parse_trusted_hosts(&raw_trusted_hosts) {
        Ok(list) => list,
        Err(msg) => return nil_err(&lua, msg),
    };
    let accept_partial = opts.get::<Option<Vec<String>>>("accept_partial")?.unwrap_or_default();
    let mut accepted_host_protect = false;
    for cap_name in &accept_partial {
        match crate::ossandbox::Cap::parse_accept(cap_name) {
            Ok(caps) => {
                if caps.contains(&crate::ossandbox::Cap::FsHostProtect) {
                    accepted_host_protect = true;
                }
            }
            Err(msg) => return nil_err(&lua, msg),
        }
    }
    if !accepted_host_protect
        && let Some(policy) = crate::ossandbox::current(&lua)
        && policy.accept_partial.contains(&crate::ossandbox::Cap::FsHostProtect)
    {
        accepted_host_protect = true;
    }
    let cancel = crate::task::checked_token(&opts)?;
    let token_source = match parse_token_source(&opts, default_token_path)? {
        Ok(src) => src,
        Err(msg) => return nil_err(&lua, msg),
    };
    if matches!(token_source, TokenSource::RuntimeFile { .. })
        && let Err(msg) = crate::ossandbox::protected::producer_gate(accepted_host_protect)
    {
        return nil_err(&lua, msg);
    }
    if matches!(token_source, TokenSource::Static(_)) {
        static WARNED_STATIC_TOKEN: std::sync::Once = std::sync::Once::new();
        WARNED_STATIC_TOKEN.call_once(|| {
            tracing::warn!(
                "mare.http.serve: custom static token is not stored in <state>/host/ and its source file is NOT \
                 hidden from sandboxed processes by fs_host_protect"
            );
        });
    }
    if matches!(token_source, TokenSource::RuntimeFile { .. } | TokenSource::Static(_))
        && crate::ossandbox::current(&lua).is_some_and(|p| p.profile.is_some())
    {
        static WARNED_RAW_PROFILE: std::sync::Once = std::sync::Once::new();
        WARNED_RAW_PROFILE.call_once(|| {
            tracing::warn!(
                "mare.http.serve: a raw (custom) Seatbelt profile is configured; restricted processes launched with a \
                 raw profile before token creation are NOT claimed to be protected by fs_host_protect"
            );
        });
    }
    let mut auth =
        match tokio::task::spawn_blocking(move || AuthState::new_with_cancel(token_source, cancel.as_ref())).await {
            Ok(Ok(a)) => a,
            Ok(Err(msg)) => return nil_err(&lua, msg),
            Err(e) => return nil_err(&lua, format!("host token lock task failed: {e}")),
        };

    if !is_loopback_host(&host) {
        return nil_err(&lua, format!("mare.http.serve only binds loopback addresses, got '{host}'"));
    }

    let (listener, local_addr) = match binder(host, port).await {
        Ok(pair) => pair,
        Err(msg) => return nil_err(&lua, msg),
    };
    if !is_loopback_ip(local_addr.ip()) {
        return nil_err(&lua, format!("mare.http.serve bound non-loopback address {}", local_addr.ip()));
    }
    auth.set_cookie_name(next_listener_cookie_name(local_addr.port()));
    let auth = Arc::new(auth);

    let mut ws_config = WebSocketConfig::default();
    ws_config.max_message_size = Some(ws_max_message_bytes);
    ws_config.max_frame_size = Some(ws_max_frame_bytes);

    let shutdown = CancellationToken::new();
    let shutdown_loop = shutdown.clone();
    let tasks = ActiveTasks::default();
    let tasks_loop = tasks.clone();
    let conn_sem = Arc::new(Semaphore::new(max_connections));
    let ctx = ServeCtx {
        lua: lua.clone(),
        ectx: mare_shared::exec_ctx::current(&lua),
        handler,
        auth,
        allowed_origins: Arc::new(allowed_origins),
        trusted_hosts: Arc::new(trusted_hosts),
        max_body_bytes,
        close_grace: std::time::Duration::from_millis(close_grace_ms),
        body_stall: std::time::Duration::from_millis(body_stall_ms),
        header_read_timeout: std::time::Duration::from_millis(header_read_timeout_ms),
        ws_config,
        shutdown: shutdown.clone(),
        tasks: tasks.clone(),
        req_sem: Arc::new(Semaphore::new(max_concurrency)),
        #[cfg(test)]
        test_hooks: test_hooks.clone(),
    };

    let task = mare_shared::txn_scope::spawn_local(async move {
        let mut accept_backoff = std::time::Duration::ZERO;
        loop {
            let Some(conn_permit) = acquire_permit(&conn_sem, &shutdown_loop).await else {
                break;
            };
            let (stream, peer_addr) = tokio::select! {
                _ = shutdown_loop.cancelled() => break,
                res = accept_next(
                    &listener,
                    #[cfg(test)]
                    test_hooks.as_deref(),
                ) => match res {
                    Ok(pair) => {
                        accept_backoff = std::time::Duration::ZERO;
                        pair
                    }
                    Err(e) => {
                        drop(conn_permit);
                        tracing::warn!("mare.http.serve accept error: {e}");
                        if !support::accept_backoff_wait(&mut accept_backoff, &shutdown_loop).await {
                            break;
                        }
                        continue;
                    }
                }
            };
            let ctx_conn = ctx.clone();
            let conn_permit = Arc::new(conn_permit);
            let conn_handle = mare_shared::txn_scope::spawn_local(async move {
                serve_stream(stream, peer_addr, ctx_conn, conn_permit).await;
            });
            tasks_loop.push(conn_handle);
        }
    });

    let srv = HttpServer { addr: local_addr, shutdown, task: Arc::new(AsyncMutex::new(Some(task))), tasks };
    Ok(MultiValue::from_vec(vec![Value::UserData(lua.create_userdata(srv)?)]))
}

async fn serve(lua: Lua, opts: Table) -> mlua::Result<MultiValue> {
    serve_with_binder(lua, opts, |host, port| async move {
        let listener = TcpListener::bind((host.as_str(), port))
            .await
            .map_err(|e| format!("cannot listen on {host}:{port}: {e}"))?;
        let local_addr = listener.local_addr().map_err(|e| format!("local_addr: {e}"))?;
        Ok((listener, local_addr))
    })
    .await
}

pub fn next_listener_cookie_name(port: u16) -> String {
    static NEXT_LISTENER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let listener_id = NEXT_LISTENER_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("mare_session_{port}_{listener_id}_{:08x}", rand::random::<u32>())
}

pub fn install(lua: &Lua, http: &Table) -> mlua::Result<()> {
    http.set(
        "serve",
        lua.create_async_function(|lua, opts: Table| {
            let barrier = crate::exec_gate::check_unpoisoned(&lua);
            let host = opts.get::<Option<String>>("host").ok().flatten().unwrap_or_else(|| "127.0.0.1".into());
            let pre = crate::exec_ctx::check(&lua, "network", &host);
            async move {
                barrier?;
                pre?;
                serve(lua, opts).await
            }
        })?,
    )
}

#[cfg(test)]
#[path = "http_serve_sec_tests/mod.rs"]
mod sec_tests;
#[cfg(test)]
#[path = "http_serve_tests.rs"]
mod tests;
