//! WebSocket JSON-RPC transport bridge.
//!
//! Enforces one JSON-RPC object per text frame, rejects binary frames with close code `1003`,
//! runs periodic server ping/pong liveness checks, closes with `1013` (best-effort) on slow-consumer
//! overload, closes with `4001` when the host token rotates, and disconnects without terminating
//! the host process (`owns_process_lifecycle = false`).

use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use mare_binding::http_serve::host_transport::{AuthState, subscribe_token_rotation};
use tokio::io::AsyncWriteExt;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_util::sync::CancellationToken;

use crate::rpc::serve::{ChannelMessageReader, Line, MAX_LINE, serve_with_io};
use crate::rpc::{OUT_QUEUE, OutLine, Principal, RpcRuntime};

const WS_CLOSE_OVERLOADED: u16 = 1013;
const WS_CLOSE_AUTH_REVOKED: u16 = 4001;
pub(crate) const OVERLOAD_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const WS_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const WS_SEND_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_PING_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_PONG_SENTINEL: Duration = Duration::from_secs(86400);

#[derive(Clone)]
pub(super) struct WsTransportOptions {
    pub ws_config: WebSocketConfig,
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
}

async fn send_close_best_effort<S>(
    ws: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    code: CloseCode,
    reason: &'static str,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let frame = CloseFrame { code, reason: reason.into() };
    let res = tokio::time::timeout(OVERLOAD_CLOSE_TIMEOUT, async {
        let send_res = ws.send(Message::Close(Some(frame))).await;
        let flush_res = ws.flush().await;
        send_res.and(flush_res)
    })
    .await;
    match res {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::debug!(close_code = u16::from(code), error = %e, "ws close frame not delivered"),
        Err(_) => tracing::debug!(close_code = u16::from(code), "ws close frame send timed out"),
    }
}

fn is_token_revoked(auth: &AuthState, initial_token: Option<&str>) -> bool {
    initial_token.is_some_and(|expected| auth.current_token().as_deref() != Some(expected))
}

enum WsSendOutcome {
    Sent,
    Shutdown,
    TimedOut,
    Failed,
}

async fn ws_send_bounded<S>(
    ws: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    msg: Message,
    shutdown: &CancellationToken,
) -> WsSendOutcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    tokio::select! {
        _ = shutdown.cancelled() => {
            send_close_best_effort(ws, CloseCode::Normal, "server shutting down").await;
            WsSendOutcome::Shutdown
        }
        res = tokio::time::timeout(WS_SEND_TIMEOUT, ws.send(msg)) => match res {
            Ok(Ok(())) => WsSendOutcome::Sent,
            Ok(Err(e)) => {
                tracing::debug!(error = %e, "ws write failed");
                WsSendOutcome::Failed
            }
            Err(_) => {
                tracing::warn!(target: "mare::web_audit", "ws outbound send timed out; closing slow peer");
                WsSendOutcome::TimedOut
            }
        }
    }
}

async fn ws_send_or_close<S>(
    ws: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    msg: Message,
    shutdown: &CancellationToken,
    inbound_tx: &mpsc::Sender<Line>,
) -> bool
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match ws_send_bounded(ws, msg, shutdown).await {
        WsSendOutcome::Sent => true,
        WsSendOutcome::TimedOut => {
            send_close_best_effort(ws, CloseCode::Again, "overloaded").await;
            let _ = inbound_tx.try_send(Line::Eof);
            false
        }
        WsSendOutcome::Shutdown | WsSendOutcome::Failed => false,
    }
}

async fn close_after_token_rotation<S>(
    sink: &mut futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    close_code_hint: &AtomicU16,
    inbound_tx: &mpsc::Sender<Line>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    tracing::info!(target: "mare::web_audit", reason = "token_rotated", "closing ws after token rotation");
    close_code_hint.store(WS_CLOSE_AUTH_REVOKED, Ordering::Release);
    send_close_best_effort(sink, CloseCode::Library(WS_CLOSE_AUTH_REVOKED), "token rotated").await;
    let _ = inbound_tx.try_send(Line::Eof);
}

struct WsPumpArgs<S> {
    ws_stream: WebSocketStream<S>,
    inbound_tx: mpsc::Sender<Line>,
    out_rx: mpsc::Receiver<OutLine>,
    close_code_hint: Arc<AtomicU16>,
    shutdown: CancellationToken,
    auth: Arc<AuthState>,
    initial_token: Option<String>,
    ping_interval: Duration,
    pong_timeout: Duration,
}

fn spawn_ws_pump<S>(args: WsPumpArgs<S>) -> tokio::task::JoinHandle<std::io::Result<()>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let WsPumpArgs {
        ws_stream,
        inbound_tx,
        mut out_rx,
        close_code_hint,
        shutdown,
        auth,
        initial_token,
        ping_interval,
        pong_timeout,
    } = args;
    tokio::spawn(async move {
        let (mut sink, mut stream) = ws_stream.split();
        let mut token_rot = subscribe_token_rotation();
        if is_token_revoked(&auth, initial_token.as_deref()) {
            close_after_token_rotation(&mut sink, &close_code_hint, &inbound_tx).await;
            return Ok(());
        }
        let mut ping_tick = tokio::time::interval(ping_interval.max(MIN_PING_INTERVAL));
        ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping_tick.tick().await;
        let mut waiting_pong = false;
        let pong_deadline = tokio::time::sleep(IDLE_PONG_SENTINEL);
        tokio::pin!(pong_deadline);

        loop {
            if close_code_hint.load(Ordering::Acquire) == WS_CLOSE_OVERLOADED {
                send_close_best_effort(&mut sink, CloseCode::Again, "overloaded").await;
                break;
            }
            tokio::select! {
                _ = shutdown.cancelled() => {
                    send_close_best_effort(&mut sink, CloseCode::Normal, "server shutting down").await;
                    break;
                }
                Ok(()) = token_rot.changed(), if initial_token.is_some() => {
                    if is_token_revoked(&auth, initial_token.as_deref()) {
                        close_after_token_rotation(&mut sink, &close_code_hint, &inbound_tx).await;
                        break;
                    }
                }
                maybe_out = out_rx.recv() => match maybe_out {
                    Some(line) => {
                        if is_token_revoked(&auth, initial_token.as_deref()) {
                            close_after_token_rotation(&mut sink, &close_code_hint, &inbound_tx).await;
                            break;
                        }
                        if close_code_hint.load(Ordering::Acquire) == WS_CLOSE_OVERLOADED {
                            send_close_best_effort(&mut sink, CloseCode::Again, "overloaded").await;
                            break;
                        }
                        let text = line.text.trim_end_matches('\n').to_string();
                        if !ws_send_or_close(&mut sink, Message::Text(text.into()), &shutdown, &inbound_tx).await {
                            break;
                        }
                    }
                    None => {
                        let code = match close_code_hint.load(Ordering::Acquire) {
                            WS_CLOSE_OVERLOADED => {
                                send_close_best_effort(&mut sink, CloseCode::Again, "overloaded").await;
                                break;
                            }
                            WS_CLOSE_AUTH_REVOKED => CloseCode::Library(WS_CLOSE_AUTH_REVOKED),
                            _ => CloseCode::Normal,
                        };
                        send_close_best_effort(&mut sink, code, "connection closed").await;
                        break;
                    }
                },
                maybe_in = stream.next() => match maybe_in {
                    Some(Ok(Message::Text(txt))) => {
                        if is_token_revoked(&auth, initial_token.as_deref()) {
                            close_after_token_rotation(&mut sink, &close_code_hint, &inbound_tx).await;
                            break;
                        }
                        if txt.len() > MAX_LINE {
                            let _ = inbound_tx.send(Line::TooLong).await;
                        } else if inbound_tx.send(Line::Text(txt.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        send_close_best_effort(&mut sink, CloseCode::Unsupported, "binary frames not supported").await;
                        let _ = inbound_tx.try_send(Line::Eof);
                        break;
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if is_token_revoked(&auth, initial_token.as_deref()) {
                            close_after_token_rotation(&mut sink, &close_code_hint, &inbound_tx).await;
                            break;
                        }
                        if !ws_send_or_close(&mut sink, Message::Pong(payload), &shutdown, &inbound_tx).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {
                        if is_token_revoked(&auth, initial_token.as_deref()) {
                            close_after_token_rotation(&mut sink, &close_code_hint, &inbound_tx).await;
                            break;
                        }
                        waiting_pong = false;
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        let _ = inbound_tx.try_send(Line::Eof);
                        break;
                    }
                    Some(Ok(Message::Frame(_))) => {}
                    Some(Err(WsError::Capacity(cap))) => {
                        tracing::debug!(error = %cap, "ws inbound message exceeded configured capacity");
                        send_close_best_effort(&mut sink, CloseCode::Size, "message too large").await;
                        let _ = inbound_tx.try_send(Line::Eof);
                        break;
                    }
                    Some(Err(WsError::Utf8(msg))) => {
                        tracing::debug!(error = %msg, "ws inbound frame contained invalid UTF-8");
                        send_close_best_effort(&mut sink, CloseCode::Invalid, "invalid UTF-8").await;
                        let _ = inbound_tx.try_send(Line::Eof);
                        break;
                    }
                    Some(Err(WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake))) => {
                        tracing::debug!("ws peer closed connection without closing handshake");
                        let _ = inbound_tx.try_send(Line::Eof);
                        break;
                    }
                    Some(Err(WsError::Protocol(proto))) => {
                        tracing::debug!(error = %proto, "ws protocol error");
                        send_close_best_effort(&mut sink, CloseCode::Protocol, "protocol error").await;
                        let _ = inbound_tx.try_send(Line::Error(std::io::Error::other(proto.to_string())));
                        break;
                    }
                    Some(Err(e)) => {
                        let _ = inbound_tx.try_send(Line::Error(std::io::Error::other(e.to_string())));
                        break;
                    }
                },
                _ = ping_tick.tick() => {
                    if close_code_hint.load(Ordering::Acquire) == WS_CLOSE_OVERLOADED {
                        send_close_best_effort(&mut sink, CloseCode::Again, "overloaded").await;
                        break;
                    }
                    if is_token_revoked(&auth, initial_token.as_deref()) {
                        close_after_token_rotation(&mut sink, &close_code_hint, &inbound_tx).await;
                        break;
                    }
                    if !waiting_pong {
                        if !ws_send_or_close(&mut sink, Message::Ping(Vec::new().into()), &shutdown, &inbound_tx).await {
                            break;
                        }
                        waiting_pong = true;
                        pong_deadline.as_mut().reset(tokio::time::Instant::now() + pong_timeout);
                    }
                }
                () = &mut pong_deadline, if waiting_pong => {
                    tracing::warn!(target: "mare::web_audit", "ws keepalive pong timed out; disconnecting");
                    send_close_best_effort(&mut sink, CloseCode::Policy, "ping timeout").await;
                    let _ = inbound_tx.try_send(Line::Eof);
                    break;
                }
            }
        }
        // Gracefully half-close TCP write direction so buffered Close frames reach Windows clients
        // ahead of FIN instead of risking a socket-drop RST.
        if let Ok(mut ws) = sink.reunite(stream) {
            let _ = tokio::time::timeout(WS_SHUTDOWN_TIMEOUT, ws.get_mut().shutdown()).await;
        }
        Ok(())
    })
}

/// Upgrade an authenticated HTTP request into a live WebSocket RPC session on the `LocalSet`.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_ws_rpc(
    rpc: Rc<RpcRuntime>,
    on_upgrade: hyper::upgrade::OnUpgrade,
    opts: WsTransportOptions,
    local_admin: bool,
    peer_addr: SocketAddr,
    origin: Option<String>,
    auth: Arc<AuthState>,
    initial_token: Option<String>,
    shutdown: CancellationToken,
    permit: Arc<OwnedSemaphorePermit>,
) {
    mare_shared::txn_scope::spawn_local(async move {
        let _permit = permit;
        let upgraded = match on_upgrade.await {
            Ok(u) => u,
            Err(e) => {
                tracing::debug!(remote = %peer_addr, error = %e, "ws upgrade failed");
                return;
            }
        };
        let mut ws_stream =
            WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(opts.ws_config)).await;
        if is_token_revoked(&auth, initial_token.as_deref()) {
            let _ = ws_stream
                .send(Message::Close(Some(CloseFrame {
                    code: CloseCode::Library(WS_CLOSE_AUTH_REVOKED),
                    reason: "host token rotated".into(),
                })))
                .await;
            let _ = ws_stream.close(None).await;
            return;
        }
        let (inbound_tx, inbound_rx) = mpsc::channel::<Line>(64);
        let (out_tx, out_rx) = mpsc::channel::<OutLine>(OUT_QUEUE);
        let close_code_hint = Arc::new(AtomicU16::new(1000));
        let writer = spawn_ws_pump(WsPumpArgs {
            ws_stream,
            inbound_tx,
            out_rx,
            close_code_hint: close_code_hint.clone(),
            shutdown: shutdown.clone(),
            auth,
            initial_token,
            ping_interval: opts.ping_interval,
            pong_timeout: opts.pong_timeout,
        });
        tracing::info!(
            target: "mare::web_audit",
            remote = %peer_addr,
            local_admin,
            ?origin,
            "ws rpc session opened"
        );
        let reader = ChannelMessageReader::new(inbound_rx);
        let stop = shutdown.cancelled_owned();
        serve_with_io(
            rpc,
            reader,
            out_tx,
            writer,
            Some(close_code_hint),
            Principal::Owner { remote_admin: local_admin },
            false,
            true,
            None,
            stop,
        )
        .await;
    });
}
