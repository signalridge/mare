//! Server-side WebSocket connection for `mare.http.serve`.

use std::collections::VecDeque;
#[cfg(test)]
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use mlua::{Table, UserData, UserDataMethods};
#[cfg(test)]
use tokio::io::AsyncWrite;
use tokio::sync::{Mutex as AsyncMutex, Notify, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_util::sync::CancellationToken;

use super::{ActiveTasks, CancelOnDrop, WS_QUEUE_CAPACITY, opt_token, rt_err};

#[derive(Clone)]
enum InboundTerminal {
    Close(Option<CloseFrame>),
    Error(String),
    Closed,
}

struct InboundState {
    queue: VecDeque<Message>,
    terminal: Option<InboundTerminal>,
    needs_close_recv: bool,
}

#[cfg(test)]
#[derive(Default)]
pub(super) struct ServeTestHooks {
    pub(super) on_inbound_full: Notify,
    pub(super) on_inbound_push: Notify,
    pub(super) on_inbound_terminal: Notify,
    pub(super) on_inbound_backpressure: Notify,
    pub(super) block_ws_writes: AtomicBool,
    pub(super) fail_ws_writes: AtomicBool,
    pub(super) on_ws_write_pending: Notify,
    pub(super) write_waker: std::sync::Mutex<Option<Waker>>,
    pub(super) on_duplex_stalled: Notify,
    pub(super) on_peer_half_close: Notify,
    pub(super) on_peer_eof_read: Notify,
    /// A stall/half-close probe saw the FIN and deliberately did not treat it as a disconnect.
    pub(super) on_fin_ignored: Notify,
    /// The half-close watcher observed a reset.
    pub(super) on_half_closed_reset: Notify,
    pub(super) on_backpressure_stall_timeout: Notify,
    pub(super) on_req_permit_wait: Notify,
    pub(super) inject_accept_error: AtomicBool,
    pub(super) on_accept_error: Notify,
}

#[cfg(test)]
pub(super) struct TestWriteHalf<W> {
    pub(super) inner: W,
    pub(super) hooks: Option<Arc<ServeTestHooks>>,
    pub(super) upgrade_phase: Arc<std::sync::atomic::AtomicU8>,
    pub(super) upgraded_phase_val: u8,
}

#[cfg(test)]
impl<W: AsyncWrite + Unpin> AsyncWrite for TestWriteHalf<W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        if let Some(hooks) = &self.hooks
            && self.upgrade_phase.load(Ordering::Acquire) == self.upgraded_phase_val
        {
            if hooks.fail_ws_writes.load(Ordering::Acquire) {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected ws write failure",
                )));
            }
            if hooks.block_ws_writes.load(Ordering::Acquire) {
                if let Ok(mut g) = hooks.write_waker.lock() {
                    *g = Some(cx.waker().clone());
                }
                hooks.on_ws_write_pending.notify_waiters();
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if let Some(hooks) = &self.hooks
            && self.upgrade_phase.load(Ordering::Acquire) == self.upgraded_phase_val
        {
            if hooks.fail_ws_writes.load(Ordering::Acquire) {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected ws write failure",
                )));
            }
            if hooks.block_ws_writes.load(Ordering::Acquire) {
                if let Ok(mut g) = hooks.write_waker.lock() {
                    *g = Some(cx.waker().clone());
                }
                hooks.on_ws_write_pending.notify_waiters();
                return Poll::Pending;
            }
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Shared "peer finished sending" signal set by the connection read pump on TCP EOF/error.
pub(super) type PeerEof = Arc<(AtomicBool, Notify)>;

/// How the read task may wait for queue space (see [`push_inbound_data`]).
struct BackpressureWait<'a> {
    peer_eof: &'a (AtomicBool, Notify),
    cancel: &'a CancellationToken,
    /// Longest wait without consumer progress (`body_stall_ms`) before the connection is torn down.
    stall_limit: Duration,
}

/// Queue one inbound data frame, applying backpressure when the queue is full: the read task
/// waits for the consumer to make room and stops reading the socket meanwhile (TCP
/// backpressure). Once the peer has finished sending (`peer_eof`), the remaining input is bounded
/// by the connection's 64 KiB duplex buffer, so frames are queued past the capacity to let the
/// read task observe EOF/Close and report the disconnect. A Close or FIN stuck behind unread
/// data is not observable, so a wait without consumer progress for `stall_limit` gives up
/// (no 1008 close frame); this bounds how long a healthy connection may stay backpressured.
/// Returns false when cancelled or stalled.
async fn push_inbound_data(
    inbound: &AsyncMutex<InboundState>,
    in_notify: &Notify,
    space_notify: &Notify,
    wait: &BackpressureWait<'_>,
    #[cfg(test)] test_hooks: Option<&ServeTestHooks>,
    msg: Message,
) -> bool {
    let BackpressureWait { peer_eof, cancel, stall_limit } = *wait;
    let mut stall = std::pin::pin!(tokio::time::sleep(stall_limit));
    loop {
        let mut space = std::pin::pin!(space_notify.notified());
        space.as_mut().enable();
        let mut eof = std::pin::pin!(peer_eof.1.notified());
        eof.as_mut().enable();
        {
            let mut st = inbound.lock().await;
            if st.queue.len() < WS_QUEUE_CAPACITY || peer_eof.0.load(Ordering::Acquire) {
                st.queue.push_back(msg);
                #[cfg(test)]
                if let Some(hooks) = test_hooks {
                    hooks.on_inbound_push.notify_waiters();
                    if st.queue.len() == WS_QUEUE_CAPACITY {
                        hooks.on_inbound_full.notify_waiters();
                    }
                }
                in_notify.notify_one();
                return true;
            }
        }
        #[cfg(test)]
        if let Some(hooks) = test_hooks {
            hooks.on_inbound_backpressure.notify_waiters();
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return false,
            _ = space => {}
            _ = eof => {}
            _ = &mut stall => {
                #[cfg(test)]
                if let Some(hooks) = test_hooks {
                    hooks.on_backpressure_stall_timeout.notify_waiters();
                }
                return false;
            }
        }
    }
}

async fn set_inbound_terminal(
    inbound: &AsyncMutex<InboundState>,
    in_notify: &Notify,
    #[cfg(test)] test_hooks: Option<&ServeTestHooks>,
    term: InboundTerminal,
) {
    let mut st = inbound.lock().await;
    st.terminal.get_or_insert(term);
    in_notify.notify_waiters();
    #[cfg(test)]
    if let Some(hooks) = test_hooks {
        hooks.on_inbound_terminal.notify_waiters();
    }
}

/// Server-side WebSocket connection with bounded inbound/outbound queues for backpressure.
pub struct ServerWs {
    out_tx: Arc<AsyncMutex<Option<mpsc::Sender<Message>>>>,
    inbound: Arc<AsyncMutex<InboundState>>,
    in_notify: Arc<Notify>,
    space_notify: Arc<Notify>,
    /// Cancelled when the user-side handle is dropped, so a blocked write can be interrupted.
    handle_dropped: CancellationToken,
    recv_lock: Arc<AsyncMutex<()>>,
    closed: Arc<AtomicBool>,
    cancel: CancellationToken,
    drained: Arc<(AtomicBool, Notify)>,
}

/// Connection-scoped resources a WebSocket inherits from its upgraded HTTP connection.
pub(super) struct WsConn {
    /// Cancels the connection token once both WS tasks are gone.
    pub(super) cancel_guard: Arc<CancelOnDrop>,
    /// Connection permit held until both WS tasks finish.
    pub(super) permit: Arc<tokio::sync::OwnedSemaphorePermit>,
    /// Set by the connection read pump on peer EOF/error.
    pub(super) peer_eof: PeerEof,
    /// Longest inbound backpressure wait without consumer progress (`body_stall_ms`).
    pub(super) stall_limit: Duration,
}

impl ServerWs {
    pub(super) fn from_stream<S>(
        ws: WebSocketStream<S>,
        cancel: CancellationToken,
        conn: WsConn,
        tasks: &ActiveTasks,
        drained: Arc<(AtomicBool, Notify)>,
        #[cfg(test)] test_hooks: Option<Arc<ServeTestHooks>>,
    ) -> Self
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + 'static,
    {
        let WsConn { cancel_guard: conn_cancel_guard, permit: conn_permit, peer_eof, stall_limit } = conn;
        let handle_dropped = CancellationToken::new();
        let handle_dropped_w = handle_dropped.clone();
        let (mut sink, mut stream) = ws.split();
        let (out_tx, mut out_rx) = mpsc::channel::<Message>(WS_QUEUE_CAPACITY);
        let inbound = Arc::new(AsyncMutex::new(InboundState {
            queue: VecDeque::with_capacity(WS_QUEUE_CAPACITY),
            terminal: None,
            needs_close_recv: false,
        }));
        let in_notify = Arc::new(Notify::new());
        let closed = Arc::new(AtomicBool::new(false));
        let peer_closed_cleanly = Arc::new(AtomicBool::new(false));
        let space_notify = Arc::new(Notify::new());

        let closed_w = closed.clone();
        let cancel_w = cancel.clone();
        let peer_closed_w = peer_closed_cleanly.clone();
        let conn_permit_w = conn_permit.clone();
        let conn_cancel_w = conn_cancel_guard.clone();

        let w_handle = mare_shared::txn_scope::spawn_local(async move {
            let _conn_permit = conn_permit_w;
            let _conn_cancel_guard = conn_cancel_w;
            let mut close_cleanly = false;
            loop {
                let msg = tokio::select! {
                    biased;
                    _ = cancel_w.cancelled() => {
                        if peer_closed_w.load(Ordering::Acquire) {
                            close_cleanly = true;
                        }
                        break;
                    }
                    m = out_rx.recv() => match m {
                        Some(m) => m,
                        None => {
                            close_cleanly = true;
                            break;
                        }
                    }
                };
                let is_close = matches!(msg, Message::Close(_));
                let mut send = sink.send(msg);
                let send_ok = tokio::select! {
                    biased;
                    _ = cancel_w.cancelled() => {
                        if peer_closed_w.load(Ordering::Acquire) {
                            close_cleanly = true;
                        }
                        false
                    }
                    res = &mut send => res.is_ok(),
                    _ = handle_dropped_w.cancelled() => {
                        // Every user handle is gone: give an in-flight write a short bounded
                        // window, then stop instead of waiting forever on a non-reading peer.
                        matches!(tokio::time::timeout(Duration::from_millis(100), &mut send).await, Ok(Ok(())))
                    }
                };
                if !send_ok {
                    if peer_closed_w.load(Ordering::Acquire) {
                        close_cleanly = true;
                    }
                    break;
                }
                if is_close {
                    close_cleanly = true;
                    break;
                }
            }
            closed_w.store(true, Ordering::SeqCst);
            cancel_w.cancel();
            if close_cleanly {
                let _ = tokio::time::timeout(Duration::from_millis(100), sink.close()).await;
            }
        });
        tasks.push(w_handle);

        let closed_r = closed.clone();
        let cancel_r = cancel.clone();
        let inbound_r = inbound.clone();
        let in_notify_r = in_notify.clone();
        // Weak: the read task must not keep the outbound channel open, otherwise dropping every
        // user handle leaves the write task parked on `out_rx.recv()` and both tasks wait forever.
        let pong_tx = out_tx.downgrade();
        let drained_r = drained.clone();
        let space_notify_r = space_notify.clone();

        let r_handle = mare_shared::txn_scope::spawn_local(async move {
            let _conn_permit = conn_permit;
            let _conn_cancel_guard = conn_cancel_guard;
            #[cfg(test)]
            let hooks_ref = test_hooks.as_deref();
            loop {
                let item = tokio::select! {
                    biased;
                    _ = cancel_r.cancelled() => break,
                    it = stream.next() => it,
                };
                match item {
                    Some(Ok(Message::Ping(p))) => {
                        if let Some(tx) = pong_tx.upgrade() {
                            let _ = tx.try_send(Message::Pong(p));
                        }
                    }
                    Some(Ok(Message::Pong(_) | Message::Frame(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        peer_closed_cleanly.store(true, Ordering::Release);
                        set_inbound_terminal(
                            &inbound_r,
                            &in_notify_r,
                            #[cfg(test)]
                            hooks_ref,
                            InboundTerminal::Close(frame),
                        )
                        .await;
                        break;
                    }
                    Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => {
                        if !push_inbound_data(
                            &inbound_r,
                            &in_notify_r,
                            &space_notify_r,
                            &BackpressureWait { peer_eof: &peer_eof, cancel: &cancel_r, stall_limit },
                            #[cfg(test)]
                            hooks_ref,
                            msg,
                        )
                        .await
                        {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        set_inbound_terminal(
                            &inbound_r,
                            &in_notify_r,
                            #[cfg(test)]
                            hooks_ref,
                            InboundTerminal::Error(e.to_string()),
                        )
                        .await;
                        break;
                    }
                    None => {
                        set_inbound_terminal(
                            &inbound_r,
                            &in_notify_r,
                            #[cfg(test)]
                            hooks_ref,
                            InboundTerminal::Closed,
                        )
                        .await;
                        break;
                    }
                }
            }
            {
                let st = inbound_r.lock().await;
                if st.queue.is_empty() && st.terminal.is_none() && !st.needs_close_recv {
                    drained_r.0.store(true, Ordering::Release);
                    drained_r.1.notify_waiters();
                }
            }
            closed_r.store(true, Ordering::SeqCst);
            cancel_r.cancel();
        });
        tasks.push(r_handle);

        Self {
            out_tx: Arc::new(AsyncMutex::new(Some(out_tx))),
            inbound,
            in_notify,
            space_notify,
            handle_dropped,
            recv_lock: Arc::new(AsyncMutex::new(())),
            closed,
            cancel,
            drained,
        }
    }
}

impl Drop for ServerWs {
    fn drop(&mut self) {
        // The userdata is the only user-side handle; signal the write task so a `sink.send`
        // blocked on a non-reading peer does not keep the tasks and permits alive.
        self.handle_dropped.cancel();
    }
}

impl UserData for ServerWs {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_async_method("send", |lua, this, (data, opts): (mlua::LuaString, Option<Table>)| async move {
            let binary = opts.as_ref().and_then(|o| o.get::<Option<bool>>("binary").ok().flatten()).unwrap_or(false);
            let msg = if binary {
                Message::Binary(Bytes::copy_from_slice(&data.as_bytes()))
            } else {
                let text = data.to_str().map_err(|_| rt_err("websocket text must be valid UTF-8"))?;
                Message::Text(text.to_owned().into())
            };
            let tx = {
                let g = this.out_tx.lock().await;
                g.clone()
            };
            let Some(tx) = tx else {
                return Err(rt_err("transport: websocket is closed"));
            };
            let _ = lua;
            tx.send(msg).await.map_err(|_| rt_err("transport: websocket is closed"))?;
            Ok(())
        });

        m.add_async_method("recv", |lua, this, opts: Option<Table>| async move {
            let idle = opts
                .as_ref()
                .and_then(|o| o.get::<Option<u64>>("idle_ms").ok().flatten())
                .map(Duration::from_millis);
            let tok: Option<CancellationToken> = opt_token(&opts);
            let _guard = this.recv_lock.lock().await;
            loop {
                let mut notified = std::pin::pin!(this.in_notify.notified());
                notified.as_mut().enable();
                {
                    let mut st = this.inbound.lock().await;
                    if let Some(msg) = st.queue.pop_front() {
                        // Do not mark `drained` when popping the last queued frame; only mark
                        // drained once the consumer observes closure on the subsequent `recv()`.
                        st.needs_close_recv = true;
                        this.space_notify.notify_one();
                        return match msg {
                            Message::Text(t) => Ok((Some(lua.create_string(t.as_bytes())?), None, Some(false))),
                            Message::Binary(b) => Ok((Some(lua.create_string(&b)?), None, Some(true))),
                            _ => Ok((None, Some("closed".to_string()), None)),
                        };
                    }
                    if let Some(term) = st.terminal.take() {
                        this.closed.store(true, Ordering::SeqCst);
                        if st.queue.is_empty() {
                            st.needs_close_recv = false;
                            this.drained.0.store(true, Ordering::Release);
                            this.drained.1.notify_waiters();
                        }
                        return match term {
                            InboundTerminal::Close(frame) => {
                                let why = frame
                                    .map(|f| format!("closed ({}: {})", u16::from(f.code), f.reason))
                                    .unwrap_or_else(|| "closed".to_string());
                                Ok((None, Some(why), None))
                            }
                            InboundTerminal::Error(e) => Err(rt_err(format!("transport: websocket: {e}"))),
                            InboundTerminal::Closed => Ok((None, Some("closed".to_string()), None)),
                        };
                    }
                    if this.closed.load(Ordering::SeqCst) || this.cancel.is_cancelled() {
                        st.needs_close_recv = false;
                        this.closed.store(true, Ordering::SeqCst);
                        this.drained.0.store(true, Ordering::Release);
                        this.drained.1.notify_waiters();
                        return Ok((None, Some("closed".to_string()), None));
                    }
                }
                tokio::select! {
                    biased;
                    _ = notified => {}
                    _ = this.cancel.cancelled() => {}
                    _ = async { match &tok { Some(t) => t.cancelled().await, None => std::future::pending().await } } => {
                        return Err(rt_err("aborted"));
                    }
                    _ = async { match idle { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => {
                        return Err(rt_err("transport: idle timeout waiting for websocket"));
                    }
                }
            }
        });

        m.add_async_method("close", |_, this, opts: Option<Table>| async move {
            this.closed.store(true, Ordering::SeqCst);
            let tx = this.out_tx.lock().await.take();
            if let Some(tx) = tx {
                let code = opts.as_ref().and_then(|o| o.get::<Option<u16>>("code").ok().flatten()).unwrap_or(1000);
                let reason =
                    opts.as_ref().and_then(|o| o.get::<Option<String>>("reason").ok().flatten()).unwrap_or_default();
                let frame = CloseFrame { code: CloseCode::from(code), reason: reason.into() };
                let _ = tx.send(Message::Close(Some(frame))).await;
            }
            Ok(())
        });

        m.add_method("is_closed", |_, this, ()| Ok(this.closed.load(Ordering::SeqCst)));
    }
}
