//! The connection loop: stdio and peer entry points, bounded line reads, the request / response
//! select loop.

use super::dispatch::handle_with_activations;
use super::*;

pub async fn run(host: Arc<Host>) -> Result<i32> {
    let rpc = RpcRuntime::from_host(&host).await;
    run_manager(rpc).await
}

pub async fn run_manager(rpc: Rc<RpcRuntime>) -> Result<i32> {
    // with --peer the stdio session is also reachable over its socket
    // SIGTERM / SIGHUP and SIGINT (Ctrl+C in the host's terminal) all stop the running turn and
    // shut down normally, so plugins still see app/shutdown (headless does the same, headless.rs)
    let stop = async {
        tokio::select! {
            _ = crate::signals::terminate() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    };
    let stdout = tokio::fs::File::from_std(crate::clone_protocol_stdout()?);
    serve_manager(rpc, tokio::io::stdin(), stdout, true, None, stop).await;
    Ok(0)
}

/// Serve one RPC connection until the peer closes it (or `stop` resolves, e.g. on SIGTERM).
/// `primary`: `true` for the session's own stdio frontend (Owner principal + shutdown owner);
/// `false` for secondary peer-style connections.
pub async fn serve<R, W>(
    host: Arc<Host>,
    reader: R,
    writer: W,
    primary: bool,
    stop: impl std::future::Future<Output = ()>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let rpc = RpcRuntime::from_host(&host).await;
    serve_manager(rpc, reader, writer, primary, None, stop).await
}

pub async fn serve_manager<R, W>(
    rpc: Rc<RpcRuntime>,
    reader: R,
    writer: W,
    primary: bool,
    peer_pid: Option<u32>,
    stop: impl std::future::Future<Output = ()>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let principal = if primary { Principal::Owner { remote_admin: true } } else { Principal::Peer };
    serve_inner(rpc, reader, writer, principal, primary, peer_pid, stop).await
}

/// A peer socket connection: secondary, default methods only (the socket already
/// admitted only the same OS user). `peer_pid`: the connecting process from the socket
/// credentials, when known.
pub async fn serve_peer<R, W>(host: Arc<Host>, reader: R, writer: W, peer_pid: Option<u32>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let rpc = RpcRuntime::from_host(&host).await;
    serve_peer_manager(rpc, reader, writer, peer_pid).await
}

pub async fn serve_peer_manager<R, W>(rpc: Rc<RpcRuntime>, reader: R, writer: W, peer_pid: Option<u32>)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    serve_inner(rpc, reader, writer, Principal::Peer, false, peer_pid, std::future::pending()).await
}

/// Longest accepted request line (64 MiB: room for base64 images in a prompt). A longer line is
/// skipped up to its newline and answered with -32600, never buffered without limit.
pub(crate) const MAX_LINE: usize = 64 * 1024 * 1024;
/// Longest request line on a peer socket: its default methods carry short texts, so any process
/// of the user can't make the session buffer 64 MiB per line.
pub(crate) const PEER_MAX_LINE: usize = 1024 * 1024;

/// Requests handled concurrently; more are refused with `overloaded`.
const MAX_INFLIGHT: usize = 256;
/// Default callback timeout before a client updates it.
const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_millis(proto::DEFAULT_CALLBACK_TIMEOUT_MS);
/// Poll interval while draining active sessions after output loss without EOF.
const BUSY_DRAIN_POLL: Duration = Duration::from_millis(25);
/// Grace period waiting for cancelled in-flight RPC handlers on disconnect.
const DISCONNECT_INFLIGHT_GRACE: Duration = Duration::from_millis(4000);
/// Writer flush wait on normal disconnect vs slow-consumer overload
/// (must cover WS_SEND_TIMEOUT 5s + OVERLOAD_CLOSE_TIMEOUT 2s + WS_SHUTDOWN_TIMEOUT 250ms).
const WRITER_DRAIN_WAIT: Duration = Duration::from_millis(500);
const WRITER_OVERLOAD_DRAIN_WAIT: Duration = Duration::from_secs(8);

/// What reading one line or WebSocket text frame produced.
pub(crate) enum Line {
    Text(String),
    /// invalid UTF-8 bytes on the line
    InvalidUtf8,
    /// longer than [`MAX_LINE`] (the rest of it was discarded)
    TooLong,
    Eof,
    Error(std::io::Error),
}

pub(crate) trait RpcMessageReader {
    async fn next_message(&mut self, max: usize) -> Line;
}

struct BufLineReader<R> {
    reader: BufReader<R>,
}

impl<R: AsyncRead + Unpin> RpcMessageReader for BufLineReader<R> {
    async fn next_message(&mut self, max: usize) -> Line {
        next_line(&mut self.reader, max).await
    }
}

pub(crate) struct ChannelMessageReader {
    rx: mpsc::Receiver<Line>,
}

impl ChannelMessageReader {
    pub(crate) fn new(rx: mpsc::Receiver<Line>) -> Self {
        Self { rx }
    }
}

impl RpcMessageReader for ChannelMessageReader {
    async fn next_message(&mut self, _max: usize) -> Line {
        self.rx.recv().await.unwrap_or(Line::Eof)
    }
}

/// Read one line (without the trailing newline), at most `max` bytes.
async fn next_line<R: AsyncRead + Unpin>(r: &mut BufReader<R>, max: usize) -> Line {
    let mut buf = Vec::new();
    let mut too_long = false;
    loop {
        let chunk = match r.fill_buf().await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Line::Error(e),
        };
        if chunk.is_empty() {
            if too_long {
                return Line::TooLong;
            }
            if buf.is_empty() {
                return Line::Eof;
            }
            break;
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (chunk.len(), false),
        };
        if !too_long {
            if buf.len() + take > max + 2 {
                too_long = true;
                buf = Vec::new();
            } else {
                buf.extend_from_slice(&chunk[..take]);
            }
        }
        r.consume(take);
        if done {
            break;
        }
    }
    if too_long {
        return Line::TooLong;
    }
    while matches!(buf.last(), Some(b'\n' | b'\r')) {
        buf.pop();
    }
    if buf.len() > max {
        return Line::TooLong;
    }
    match String::from_utf8(buf) {
        Ok(s) => Line::Text(s),
        Err(_) => Line::InvalidUtf8,
    }
}

async fn write_output<W: AsyncWrite + Unpin>(mut writer: W, mut rx: mpsc::Receiver<OutLine>) -> std::io::Result<()> {
    while let Some(line) = rx.recv().await {
        writer.write_all(line.text.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    Ok(())
}

pub(super) async fn serve_inner<R, W>(
    rpc: Rc<RpcRuntime>,
    reader: R,
    writer: W,
    principal: Principal,
    owns_process_lifecycle: bool,
    peer_pid: Option<u32>,
    stop: impl std::future::Future<Output = ()>,
) where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::channel::<OutLine>(OUT_QUEUE);
    let writer = tokio::spawn(write_output(writer, out_rx));
    let reader = BufLineReader { reader: BufReader::new(reader) };
    serve_with_io(rpc, reader, out_tx, writer, None, principal, owns_process_lifecycle, false, peer_pid, stop).await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_with_io<R>(
    rpc: Rc<RpcRuntime>,
    mut reader: R,
    out_tx: mpsc::Sender<OutLine>,
    mut writer: tokio::task::JoinHandle<std::io::Result<()>>,
    ws_close_code: Option<Arc<std::sync::atomic::AtomicU16>>,
    principal: Principal,
    owns_process_lifecycle: bool,
    remote_transport: bool,
    peer_pid: Option<u32>,
    stop: impl std::future::Future<Output = ()>,
) where
    R: RpcMessageReader,
{
    let Some(host) = rpc.anchor_host() else { return };
    // single writer: protocol messages only
    let out_budget = Arc::new(tokio::sync::Semaphore::new(OUT_BYTES));

    let conn_id = mare_host::alloc_conn_id();
    let (event_barrier, event_barriers) = mpsc::channel(128);
    let is_owner = principal.is_owner();
    let conn = Rc::new(Conn {
        id: conn_id,
        rpc: rpc.clone(),
        host: host.clone(),
        out: out_tx.clone(),
        out_budget,
        out_slots: Arc::new(tokio::sync::Semaphore::new(OUT_QUEUE)),
        pending: RefCell::new(HashMap::new()),
        // Per-connection lifecycle token (cancelled on this connection's close/EOF), not a child
        // of host.cancel_token(): primary shutdown after aborting a peer-started turn must not
        // cancel the peer's committed prompt request before it delivers the `"aborted"` result.
        conn_cancel: tokio_util::sync::CancellationToken::new(),
        requests: RefCell::new(HashMap::new()),
        session_ctxs: RefCell::new(HashMap::new()),
        anchor_ctx: RefCell::new(None),
        global_registrations: RefCell::new(GlobalRegistrations::default()),
        reg_gate: Rc::new(tokio::sync::Semaphore::new(1)),
        next_id: Cell::new(1),
        caps: RefCell::new(proto::ClientCapabilities::default()),
        cap_revision: Cell::new(0),
        cap_gate: Rc::new(tokio::sync::Semaphore::new(1)),
        timeout: Cell::new(DEFAULT_CALLBACK_TIMEOUT),
        approval_timeout: Cell::new(None),
        default_filter: RefCell::new(EventFilter::default()),
        default_subscribe_all: Cell::new(is_owner),
        explicit_sessions: RefCell::new(std::collections::HashSet::new()),
        suppressed_subscriptions: RefCell::new(std::collections::HashSet::new()),
        deferred_subscriptions: RefCell::new(HashMap::new()),
        subscriptions: RefCell::new(HashMap::new()),
        tracking_failures: RefCell::new(HashMap::new()),
        sub_wake: Arc::new(tokio::sync::Notify::new()),
        handshake: Cell::new(HandshakeState::Uninitialized),
        shutdown_requested: Cell::new(false),
        shutdown: Cell::new(false),
        shutdown_notify: tokio::sync::Notify::new(),
        closed: Cell::new(false),
        overflowed: Cell::new(false),
        overflow_notify: tokio::sync::Notify::new(),
        ws_close_code: ws_close_code.clone(),
        event_barrier,
        output_lost: Cell::new(false),
        inflight: Cell::new(0),
        inflight_notify: tokio::sync::Notify::new(),
        principal,
        remote_transport,
        peer_pid,
    });
    rpc.connections.borrow_mut().insert(conn.id, Rc::downgrade(&conn));
    for rt in rpc.active_sessions() {
        if conn.ensure_session_ctx(&rt.session_id, &rt.host).await.is_err() {
            rpc.connections.borrow_mut().remove(&conn.id);
            return;
        }
    }
    if !is_owner
        && let Ok(rt) = rpc.anchor_session_for(&conn)
        && super::access::authorize(&conn, &rt.host, super::access::Op::EventsStream).is_allowed()
        && let Ok(ctx_id) = conn.ensure_session_ctx(&rt.session_id, &rt.host).await
    {
        let filter = conn.default_filter.borrow().clone();
        if let Ok((_, _, _, activation)) =
            conn.open_subscription_on(&rt.session_id, &rt.host, ctx_id, None, None, true, filter, false, false)
        {
            conn.activate_subscriptions(vec![activation]);
        }
    }
    let events = super::stream_pump::spawn_event_pump(&conn, &host, event_barriers);

    let max_line = if is_owner { MAX_LINE } else { PEER_MAX_LINE };
    let term = stop;
    tokio::pin!(term);
    let mut signaled = false;
    let mut input_closed = false;
    let mut writer_finished = false;
    loop {
        if conn.overflowed.get() {
            conn.output_lost.set(true);
            break;
        }
        let line = tokio::select! {
            r = reader.next_message(max_line) => match r {
                Line::Text(l) => l,
                Line::InvalidUtf8 => {
                    conn.send(&Message::error(None, proto::ErrorCode::ParseError, "parse error: invalid UTF-8"));
                    continue;
                }
                Line::Eof => {
                    input_closed = true;
                    break;
                }
                Line::TooLong => {
                    conn.send(&Message::error(
                        None,
                        proto::ErrorCode::InvalidRequest,
                        format!("request line longer than {} MiB", max_line / (1024 * 1024)),
                    ));
                    continue;
                }
                // A read error also closes the transport; report it for diagnostics.
                Line::Error(e) => {
                    tracing::warn!("rpc: reading the request stream failed: {e}");
                    if owns_process_lifecycle {
                        eprintln!("mare: rpc: reading stdin failed: {e}");
                    }
                    input_closed = true;
                    break;
                }
            },
            _ = conn.overflow_notify.notified() => {
                conn.output_lost.set(true);
                break;
            },
            _ = &mut writer => {
                writer_finished = true;
                conn.output_lost.set(true);
                break;
            },
            _ = &mut term => {
                signaled = true;
                if owns_process_lifecycle {
                    crate::signals::begin_forceable_shutdown(0);
                }
                break;
            }
            _ = conn.shutdown_notify.notified() => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        // Syntax first: a line that is not JSON is a parse error (-32700, id null). A JSON value
        // that is not a valid JSON-RPC 2.0 envelope is an invalid request (-32600), answered
        // with its id when one is readable (a request's single `id`); it is never processed.
        // Fast path: parse `Message` once on valid top-level JSON objects with version "2.0";
        // non-objects (e.g. positional arrays) and wrong-version/invalid frames go through the
        // diagnostic sequence so `-32700` vs `-32600`, duplicate keys, and readable request IDs
        // match exactly.
        let fast = if line.trim_start().starts_with('{') {
            match serde_json::from_str::<Message>(&line) {
                Ok(m) if m.jsonrpc == proto::JSONRPC => Ok(m),
                Ok(_) => Err(None),
                Err(e) => Err(Some(e)),
            }
        } else {
            Err(None)
        };
        let msg: Message = match fast {
            Ok(m) => m,
            Err(msg_err_opt) => {
                if let Err(e) = serde_json::from_str::<serde::de::IgnoredAny>(&line) {
                    // the whole line, first: `[` or `[1,` is a parse error, not a wrong type
                    conn.send(&Message::error(None, proto::ErrorCode::ParseError, format!("parse error: {e}")));
                    continue;
                }
                let members = match serde_json::from_str::<envelope::Members>(&line) {
                    Ok(members) => members,
                    Err(e) => {
                        conn.send(&Message::error(
                            None,
                            proto::ErrorCode::InvalidRequest,
                            format!("invalid request: {e}"),
                        ));
                        continue;
                    }
                };
                let request_id = members.request_id();
                if members.single("jsonrpc").and_then(J::as_str) != Some(proto::JSONRPC) {
                    conn.send(&Message::error(request_id, proto::ErrorCode::InvalidRequest, "jsonrpc must be \"2.0\""));
                    continue;
                }
                let err_detail = match msg_err_opt {
                    Some(e) => e.to_string(),
                    None => match serde_json::from_str::<Message>(&line) {
                        Err(e) => e.to_string(),
                        Ok(_) => "invalid request".to_string(),
                    },
                };
                if members.single("method").is_none()
                    && let Some(id_val) = members.single("id")
                    && let Ok(req_id) = serde_json::from_value::<RequestId>(id_val.clone())
                {
                    let key = match &req_id {
                        RequestId::String(s) => s.clone(),
                        RequestId::Number(n) => n.to_string(),
                    };
                    if let Some(request) = conn.pending.borrow_mut().remove(&key) {
                        let _ = request.reply.send(Err(proto::RpcError::core(
                            proto::ErrorCode::Internal,
                            format!("invalid callback reply: {err_detail}"),
                        )));
                    }
                }
                conn.send(&Message::error(
                    request_id,
                    proto::ErrorCode::InvalidRequest,
                    format!("invalid request: {err_detail}"),
                ));
                continue;
            }
        };
        let Message { id, method, params, result, error, .. } = msg;
        match (method, id) {
            // response to one of our callbacks
            (None, Some(id)) => {
                let key = match &id {
                    RequestId::String(s) => s.clone(),
                    RequestId::Number(n) => n.to_string(),
                };
                if let Some(request) = conn.pending.borrow_mut().remove(&key) {
                    let _ = request.reply.send(match (result, error) {
                        (Some(result), None) => Ok(result),
                        (None, Some(error)) => Err(error),
                        _ => Err(proto::RpcError::core(
                            proto::ErrorCode::Internal,
                            "callback reply requires exactly one of result or error",
                        )),
                    });
                } else if result.is_none() && error.is_none() {
                    conn.send(&Message::error(
                        Some(id),
                        proto::ErrorCode::InvalidRequest,
                        "neither a request nor a response",
                    ));
                }
            }
            // request: handled concurrently so callbacks can be answered meanwhile
            (Some(method), Some(id)) => {
                if conn.shutdown_requested.get() && method != methods::SHUTDOWN {
                    conn.send(&Message::error(Some(id), proto::ErrorCode::InvalidRequest, "server is shutting down"));
                    continue;
                }
                if conn.inflight.get() >= MAX_INFLIGHT {
                    conn.send(&Message::rpc_error(
                        Some(id),
                        proto::RpcError::coded(
                            "overloaded",
                            format!("server overloaded: {MAX_INFLIGHT} requests in flight; retry later"),
                        ),
                    ));
                    continue;
                }
                if conn.requests.borrow().contains_key(&id) {
                    conn.send(&Message::error(
                        Some(id),
                        proto::ErrorCode::InvalidRequest,
                        "duplicate in-flight request ID",
                    ));
                    continue;
                }
                let cancel = conn.conn_cancel.child_token();
                conn.requests.borrow_mut().insert(id.clone(), cancel.clone());
                conn.inflight.set(conn.inflight.get() + 1);
                let c = conn.clone();
                tokio::task::spawn_local(async move {
                    let guard = RequestGuard { connection: c.clone(), id: id.clone() };
                    let is_shutdown = method == methods::SHUTDOWN;
                    // A request that reaches the plugin tree waits out a reload in progress, so it
                    // never runs against a tree half reloaded (the kernel's reload barrier; the
                    // whole Host::reload_all holds it); cancelling the request ends the wait.
                    let settled = envelope::reload_settled(&c, &method, &params, cancel.clone());
                    let request = REQUEST_CANCEL.scope(cancel, async {
                        settled.await?;
                        Ok::<_, proto::RpcError>(handle_with_activations(&c, &method, &params).await)
                    });
                    let (mut outcome, activations) = match request.await {
                        Ok((res, acts)) => (res, acts),
                        Err(e) => (Err(e), Vec::new()),
                    };
                    if !c.closed.get() && c.flush_events().await {
                        c.refresh_post_flush_filter_outcome(&mut outcome, &activations);
                        let reply = match outcome {
                            Ok(v) => Message::response(id.clone(), v),
                            Err(e) => Message::rpc_error(Some(id.clone()), e),
                        };
                        c.send(&reply);
                        c.activate_subscriptions(activations);
                    }
                    if is_shutdown {
                        c.shutdown.set(true);
                        c.shutdown_notify.notify_one();
                    }
                    drop(guard);
                });
            }
            // notification from the host (unknown methods ignored for forward compatibility)
            (Some(method), None) => {
                if method == methods::CALLBACK_UPDATE
                    && let Ok(cu) = params_req::<proto::CallbackUpdateParams>(&params)
                {
                    services::handle_callback_update(&conn, cu);
                } else if method == methods::CANCEL_REQUEST
                    && let Some(value) = params.as_ref().and_then(|params| params.get("id"))
                    && let Ok(id) = serde_json::from_value::<RequestId>(value.clone())
                    && let Some(cancel) = conn.requests.borrow().get(&id)
                {
                    cancel.cancel();
                }
            }
            (None, None) => {
                conn.send(&Message::error(None, proto::ErrorCode::InvalidRequest, "neither a request nor a response"));
            }
        }
        if conn.shutdown.get() {
            break;
        }
        // let spawned handlers run (e.g. shutdown) before reading the next line
        tokio::task::yield_now().await;
        if conn.shutdown.get() {
            break;
        }
    }

    // No more replies can arrive after input EOF or connection loss.
    if conn.output_lost.get() || input_closed {
        conn.closed.set(true);
    }
    conn.conn_cancel.cancel();
    conn.cancel_pending(None, "disconnected");
    host.answerer.release(host.lua(), conn.id, "disconnected");
    for rt in rpc.active_sessions() {
        ui_bridge::detach_shell_and_restore_viewport(&rpc, conn.id, &rt).await;
        rt.host.answerer.release(rt.host.lua(), conn.id, "disconnected");
    }
    #[cfg(test)]
    tests::notify_disconnect_observed();
    conn.closed.set(true);
    rpc.connections.borrow_mut().remove(&conn.id);
    if !owns_process_lifecycle || conn.output_lost.get() || signaled || input_closed || conn.shutdown.get() {
        // Let cancelled pre-start request tasks observe closure before checking host state.
        tokio::task::yield_now().await;
    }
    #[cfg(test)]
    if input_closed {
        tests::notify_test_input_eof();
    }
    // Withdraw registrations/hooks and dispose each session's kernel ctx before waking any
    // pending reverse RPCs (`callback.call`, UI prompts), so callers observing the cancel cannot
    // race ahead of registration cleanup.
    let ctx_entries = conn.drain_all_ctxs();
    let mut deferred_callback_tokens = Vec::new();
    for (weak_h, ctx_id) in ctx_entries {
        if let Some(h) = weak_h.upgrade() {
            h.kernel.release_baselines_for_owner(ctx_id);
            super::tree_hooks::disconnect_host(&h, ctx_id).await;
            deferred_callback_tokens.extend(h.kernel.dispose_ctx_defer_callbacks(ctx_id).await);
        }
    }
    conn.cancel_pending(None, "disconnected");
    host.answerer.release(host.lua(), conn.id, "disconnected");
    for rt in rpc.active_sessions() {
        ui_bridge::detach_shell_and_restore_viewport(&rpc, conn.id, &rt).await;
        rt.host.answerer.release(rt.host.lua(), conn.id, "disconnected");
    }
    for token in deferred_callback_tokens {
        token.cancel();
    }
    // Release UI attachment, external-editor lock, and responder state immediately on disconnect
    // rather than holding them through `wait_conn_inflight`.
    ui_bridge::cleanup_conn(&conn).await;
    // Stdio input EOF/read failure closes the primary connection and stops active runs. A peer
    // disconnect only removes that peer. Output failure remains independently monitored below.
    if owns_process_lifecycle {
        let stop_sessions: Vec<Rc<SessionRuntime>> = rpc.active_sessions();
        if input_closed {
            // A disconnected client cannot receive drafts returned by agent.abort.
            // Use the transport-loss cancellation path so acknowledged inbox entries
            // remain durable and can be resumed by the next connection.
            for rt in &stop_sessions {
                let state = rt.host.state().await;
                if let Some(id) = state.get("turn_id").and_then(J::as_str) {
                    let _ = rt.host.abort_turn(id).await;
                }
            }
            if stop_sessions.is_empty() {
                let state = host.state().await;
                if let Some(id) = state.get("turn_id").and_then(J::as_str) {
                    let _ = host.abort_turn(id).await;
                }
            }
        } else if signaled || conn.shutdown.get() {
            for rt in &stop_sessions {
                let _ = rt.host.abort().await;
            }
            if stop_sessions.is_empty() {
                let _ = host.abort().await;
            }
        } else {
            loop {
                let mut any_busy = false;
                for rt in rpc.active_sessions() {
                    let state = rt.host.state().await;
                    let busy = match state.get("busy").and_then(J::as_bool) {
                        Some(busy) => busy,
                        None => rt.host.is_busy().await,
                    };
                    if !busy {
                        continue;
                    }
                    if conn.output_lost.get() {
                        if !rt.host.supports_turn_cancellation() {
                            tracing::warn!(
                                "rpc: active agent does not declare `cancel` capability; skipping turn cancellation during shutdown"
                            );
                            continue;
                        }
                        if let Some(id) = state.get("turn_id").and_then(J::as_str) {
                            any_busy = true;
                            let _ = rt.host.abort_turn(id).await;
                        } else {
                            // A replacement agent that cannot identify its active turn cannot be
                            // conditionally cancelled. Skip it instead of waiting forever.
                            tracing::error!("rpc: active agent did not provide a turn ID for cancellation");
                            continue;
                        }
                    } else {
                        any_busy = true;
                    }
                }
                if conn.inflight.get() == 0 && !any_busy {
                    break;
                }
                tokio::select! {
                    _ = &mut writer, if !writer_finished => {
                        writer_finished = true;
                        conn.output_lost.set(true);
                    }
                    _ = &mut term => {
                        for rt in rpc.active_sessions() {
                            let _ = rt.host.abort().await;
                        }
                        break;
                    }
                    _ = conn.overflow_notify.notified() => {
                        conn.output_lost.set(true);
                    }
                    _ = tokio::time::sleep(BUSY_DRAIN_POLL) => {}
                }
            }
        }
    }
    conn.cancel_pending(None, "disconnected");
    wait_conn_inflight(&conn, DISCONNECT_INFLIGHT_GRACE).await;
    conn.subscriptions.borrow_mut().clear();
    ui_bridge::cleanup_conn(&conn).await;
    if remote_transport {
        tracing::info!(
            target: "mare::web_audit",
            conn_id = conn.id.0,
            local_admin = conn.is_remote_admin(),
            "ws rpc session closed"
        );
    }
    events.abort();
    let _ = events.await;
    if owns_process_lifecycle {
        if rpc.shutdown_on_drop {
            let _ = rpc.manager.shutdown_all_no_abort().await;
            rpc.shutdown_fallback_host(None).await;
        } else {
            host.shutdown().await;
            rpc.shutdown_fallback_host(Some(&host)).await;
        }
    }
    let overflowed = conn.overflowed.get();
    if let Some(code) = &ws_close_code
        && overflowed
    {
        code.store(1013, std::sync::atomic::Ordering::Release);
    }
    drop(conn);
    drop(out_tx);
    if !writer_finished {
        // On slow-consumer overload over WebSocket, allow one in-flight WS frame send
        // (WS_SEND_TIMEOUT = 5s) + close frame flush to finish once the client drains rather than
        // aborting after 500ms. Raw stream transports have no close frame to flush, so bound them
        // to WRITER_DRAIN_WAIT instead of keeping a stalled peer's socket open for 8s.
        let wait = if overflowed && ws_close_code.is_some() { WRITER_OVERLOAD_DRAIN_WAIT } else { WRITER_DRAIN_WAIT };
        let writer_abort = writer.abort_handle();
        if tokio::time::timeout(wait, writer).await.is_err() {
            writer_abort.abort();
        }
    }
}

/// Wait for all cancelled in-flight request handlers on a disconnected connection to exit.
async fn wait_conn_inflight(conn: &Conn, grace: Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    while conn.inflight.get() > 0 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            tracing::warn!(inflight = conn.inflight.get(), "rpc: connection closed with stop_unconfirmed requests");
            break;
        }
        let notified = conn.inflight_notify.notified();
        if conn.inflight.get() == 0 {
            break;
        }
        if tokio::time::timeout(remaining, notified).await.is_err() {
            if conn.inflight.get() > 0 {
                tracing::warn!(inflight = conn.inflight.get(), "rpc: connection closed with stop_unconfirmed requests");
            }
            break;
        }
    }
}

#[cfg(test)]
#[path = "fs_proc_tests.rs"]
mod fs_proc_tests;

#[cfg(test)]
#[path = "resource_limit_tests.rs"]
mod resource_limit_tests;

#[cfg(test)]
#[path = "custom_messages_tests.rs"]
mod custom_messages_tests;

#[cfg(test)]
#[path = "admission_race_tests.rs"]
mod admission_race_tests;
#[cfg(test)]
#[path = "baseline_tests.rs"]
mod baseline_tests;

#[cfg(test)]
#[path = "approval_capability_tests.rs"]
mod approval_capability_tests;
#[cfg(test)]
#[path = "approval_timeout_tests.rs"]
mod approval_timeout_tests;
#[cfg(test)]
#[path = "hook_number_tests.rs"]
mod hook_number_tests;
#[cfg(test)]
#[path = "policy_hooks_nonpolicy_tests.rs"]
mod policy_hooks_nonpolicy_tests;
#[cfg(test)]
#[path = "policy_hooks_request_build_tests.rs"]
mod policy_hooks_request_build_tests;
#[cfg(test)]
#[path = "policy_hooks_tests.rs"]
mod policy_hooks_tests;
#[cfg(test)]
#[path = "service_refusal_tests.rs"]
mod service_refusal_tests;

#[cfg(test)]
#[path = "multi_session_tests.rs"]
mod multi_session_tests;

#[cfg(test)]
#[path = "ui_bridge_tests.rs"]
mod ui_bridge_tests;

#[cfg(test)]
#[path = "web_server_tests.rs"]
mod web_server_tests;

#[cfg(test)]
#[path = "serve_tests.rs"]
pub(super) mod tests;
