// exec-ctx: not a tool entry (split from http_serve.rs; gated by exec_ctx::check at http_serve.rs:773)
use super::*;

pub(super) struct PumpIo<R, W> {
    pub(super) reader: R,
    pub(super) writer: W,
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for PumpIo<R, W> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for PumpIo<R, W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

pub(super) async fn wait_handler_abort(
    shutdown: &CancellationToken,
    req_cancel: &CancellationToken,
    upgrade_phase: &AtomicU8,
    ws_drained: &(AtomicBool, tokio::sync::Notify),
    close_grace: std::time::Duration,
) {
    tokio::select! {
        biased;
        _ = shutdown.cancelled() => {}
        _ = req_cancel.cancelled() => {
            if upgrade_phase.load(Ordering::Acquire) == UPGRADE_SUCCEEDED {
                let _ = tokio::time::timeout(close_grace, async {
                    loop {
                        let mut notified = std::pin::pin!(ws_drained.1.notified());
                        notified.as_mut().enable();
                        if ws_drained.0.load(Ordering::Acquire) {
                            break;
                        }
                        tokio::select! {
                            biased;
                            _ = shutdown.cancelled() => break,
                            _ = notified => {}
                        }
                    }
                })
                .await;
            }
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
        }
    }
}

pub(super) fn collect_headers_vec(headers: &HeaderMap) -> Vec<(String, String)> {
    let mut headers_vec: Vec<(String, String)> = Vec::new();
    for (k, v) in headers {
        let key = k.as_str().to_ascii_lowercase();
        if let Ok(val) = v.to_str() {
            if let Some((_, existing)) = headers_vec.iter_mut().find(|(ek, _)| ek == &key) {
                existing.push_str(", ");
                existing.push_str(val);
            } else {
                headers_vec.push((key, val.to_string()));
            }
        }
    }
    headers_vec
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PeerGone {
    /// FIN: the peer finished sending. For HTTP this is a half-close, not a disconnect.
    Fin,
    /// RST or another socket error: the peer is gone.
    Reset,
}

#[cfg(windows)]
pub(super) fn windows_peer_reset(sock: &TcpStream) -> bool {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        POLLERR, POLLNVAL, POLLRDNORM, POLLWRNORM, SIO_TCP_INFO, SOCKET, SOCKET_ERROR, TCP_INFO_v0, TCPSTATE_CLOSED,
        WSAECONNABORTED, WSAECONNRESET, WSAENOTCONN, WSAGetLastError, WSAIoctl, WSAPOLLFD, WSAPoll, send,
    };
    let raw = sock.as_raw_socket() as SOCKET;
    // Non-destructive WSAPoll: POLLERR/POLLNVAL surface an aborted/reset socket while leaving a
    // clean half-close (FIN) alive. Never put POLLHUP/POLLERR in `events` (Winsock rejects them
    // with WSAEINVAL) and never treat POLLHUP alone as a reset, as FIN can surface POLLHUP.
    let mut pfd = WSAPOLLFD { fd: raw, events: POLLRDNORM | POLLWRNORM, revents: 0 };
    // SAFETY: `pfd` points to one initialized `WSAPOLLFD` for the valid Winsock handle `raw`.
    let poll_rc = unsafe { WSAPoll(&mut pfd, 1, 0) };
    if poll_rc > 0 && (pfd.revents & (POLLERR | POLLNVAL)) != 0 {
        return true;
    }
    if poll_rc == SOCKET_ERROR {
        // SAFETY: reads thread-local Winsock error from the failed `WSAPoll` call above.
        if matches!(unsafe { WSAGetLastError() }, WSAECONNRESET | WSAECONNABORTED | WSAENOTCONN) {
            return true;
        }
    }

    // Zero-byte send: writes nothing on a live or FIN half-closed socket, while Winsock surfaces a
    // subsequent RST as WSAECONNRESET / WSAECONNABORTED / WSAENOTCONN even when AFD readiness and
    // recv(MSG_PEEK) keep reporting EOF.
    let probe_byte: u8 = 0;
    // SAFETY: `raw` is a valid Winsock stream socket and length is 0.
    if unsafe { send(raw, (&raw const probe_byte).cast(), 0, 0) } == SOCKET_ERROR {
        // SAFETY: reads thread-local Winsock error from the failed `send` call above.
        if matches!(unsafe { WSAGetLastError() }, WSAECONNRESET | WSAECONNABORTED | WSAENOTCONN) {
            return true;
        }
    }

    let version: u32 = 0;
    let mut info = TCP_INFO_v0::default();
    let mut returned: u32 = 0;
    // Query TCP state via SIO_TCP_INFO so CLOSE_WAIT stays alive while CLOSED indicates RST.
    // SAFETY: `sock` owns a valid TCP Winsock handle; in/out buffers match `SIO_TCP_INFO` v0.
    let rc = unsafe {
        WSAIoctl(
            raw,
            SIO_TCP_INFO,
            (&raw const version).cast(),
            std::mem::size_of::<u32>() as u32,
            (&raw mut info).cast(),
            std::mem::size_of::<TCP_INFO_v0>() as u32,
            &mut returned,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc == SOCKET_ERROR {
        // SAFETY: reads thread-local Winsock error from the failed WSAIoctl call above.
        return matches!(unsafe { WSAGetLastError() }, WSAECONNRESET | WSAECONNABORTED | WSAENOTCONN);
    }
    returned as usize >= std::mem::size_of::<i32>() && info.State == TCPSTATE_CLOSED
}

#[cfg(not(windows))]
fn windows_peer_reset(_sock: &TcpStream) -> bool {
    false
}

fn peer_socket_reset(sock: &TcpStream) -> bool {
    !matches!(sock.take_error(), Ok(None)) || windows_peer_reset(sock)
}

/// Resolves once the peer half-closed (FIN, unless `ignore_fin`) or reset the connection, even
/// while unread request bytes are still pending in the socket.
pub(super) async fn peer_gone(
    read_half: &tokio::net::tcp::OwnedReadHalf,
    ignore_fin: bool,
    #[cfg(test)] test_hooks: Option<&ws::ServeTestHooks>,
) -> PeerGone {
    let sock: &TcpStream = read_half.as_ref();
    loop {
        let ready = if ignore_fin {
            if peer_socket_reset(sock) {
                return PeerGone::Reset;
            }
            // Bound readiness waits after FIN so platforms that stop updating cached readiness
            // after EOF still poll `peer_socket_reset` on `PEER_PROBE_INTERVAL`.
            match tokio::time::timeout(
                PEER_PROBE_INTERVAL,
                sock.ready(tokio::io::Interest::READABLE | tokio::io::Interest::WRITABLE),
            )
            .await
            {
                Ok(res) => res,
                Err(_) => continue,
            }
        } else {
            sock.ready(tokio::io::Interest::READABLE | tokio::io::Interest::WRITABLE).await
        };
        match ready {
            Ok(r) if r.is_write_closed() || r.is_error() => return PeerGone::Reset,
            Ok(r) if r.is_read_closed() => {
                // A reset records a socket error (SO_ERROR) even while request bytes are still
                // pending; a FIN only yields pending bytes or a clean EOF.
                let mut probe = [0u8; 1];
                if peer_socket_reset(sock) || sock.peek(&mut probe).await.is_err() {
                    return PeerGone::Reset;
                }
                if !ignore_fin {
                    return PeerGone::Fin;
                }
                #[cfg(test)]
                if let Some(hooks) = test_hooks {
                    hooks.on_fin_ignored.notify_waiters();
                }
            }
            Ok(_) if ignore_fin => {
                // Once EOF has been read on Windows AFD, subsequent readiness may report WRITABLE
                // without READ_CLOSED even after a peer RST transitions CLOSE_WAIT -> CLOSED.
                if peer_socket_reset(sock) {
                    return PeerGone::Reset;
                }
            }
            Ok(_) => {}
            Err(_) => return PeerGone::Reset,
        }
        // Poll again later instead of spinning on cached readiness.
        // sleep-ok: moved verbatim from http_serve.rs; polls peer socket liveness during streaming
        tokio::time::sleep(PEER_PROBE_INTERVAL).await;
    }
}
