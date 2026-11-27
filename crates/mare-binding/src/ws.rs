//! `mare.http.websocket`: a client WebSocket for the Responses WebSocket transport
//! (Apache-2.0): one long connection per session, text frames are JSON events,
//! pings are answered, an idle watchdog and the cancel token bound every wait, and a failed
//! handshake reports its HTTP status so the caller can fall back to HTTP.
//!
//! TLS uses the same rustls + platform verifier as `mare.http` (system roots, plus
//! `MARE_CA_CERTIFICATE` / `SSL_CERT_FILE` / `SSL_CERT_DIR`). Inside a tool call with a restricted network the
//! connection is refused: WebSockets don't go through the shared authorization layer.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

use super::task::opt_token;
use mare_shared::lua::rt_err;
use mare_shared::redact::redact_str;

type Stream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The rustls client config: aws-lc-rs (the provider reqwest uses) + the platform verifier,
/// with the custom CA certificates of `crate::cacerts` added.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let extra = crate::cacerts::load_from_env()?;
    #[cfg(not(target_os = "android"))]
    let verifier = if extra.is_empty() {
        rustls_platform_verifier::Verifier::new(provider.clone())
    } else {
        rustls_platform_verifier::Verifier::new_with_extra_roots(extra, provider.clone())
    }
    .map_err(|e| e.to_string())?;
    #[cfg(target_os = "android")]
    let verifier = rustls_platform_verifier::Verifier::new(provider.clone()).map_err(|e| e.to_string())?;
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

fn filter_ws_addrs(
    host: &str,
    addrs: Vec<std::net::SocketAddr>,
    offline: bool,
) -> Result<Vec<std::net::SocketAddr>, String> {
    if !offline {
        return Ok(addrs);
    }
    let loopback: Vec<std::net::SocketAddr> = addrs.into_iter().filter(|sa| sa.ip().is_loopback()).collect();
    if loopback.is_empty() {
        return Err(format!("offline test guard: {host} resolved to no loopback address; tests stay off the network"));
    }
    Ok(loopback)
}

/// Connect: the handshake's HTTP status (when the server answered) is part of the error so the
/// caller can tell "not supported" (e.g. 404/426) from a network failure.
async fn connect(url: &str, headers: Vec<(String, String)>, timeout: Duration) -> Result<Stream, String> {
    crate::http::check_offline_url(url)?;
    let mut req = url.into_client_request().map_err(|e| redact_str(&format!("websocket: invalid url {url}: {e}")))?;
    for (k, v) in headers {
        let name =
            HeaderName::from_bytes(k.as_bytes()).map_err(|e| redact_str(&format!("websocket: header {k}: {e}")))?;
        let value = HeaderValue::from_str(&v).map_err(|e| redact_str(&format!("websocket: header {k}: {e}")))?;
        req.headers_mut().insert(name, value);
    }
    let host = req
        .uri()
        .host()
        .ok_or_else(|| redact_str(&format!("websocket: missing host in {url}")))?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = req.uri().port_u16().unwrap_or(if req.uri().scheme_str() == Some("wss") { 443 } else { 80 });
    let connector = if url.starts_with("wss:") { Some(Connector::Rustls(tls_config()?)) } else { None };
    let fut = async {
        let raw_addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
            .await
            .map_err(|e| redact_str(&format!("websocket: {e}")))?
            .collect();
        let addrs = filter_ws_addrs(&host, raw_addrs, crate::http::offline_guard_active())?;
        let tcp = TcpStream::connect(&addrs[..]).await.map_err(|e| redact_str(&format!("websocket: {e}")))?;
        let _ = tcp.set_nodelay(true);
        match tokio_tungstenite::client_async_tls_with_config(req, tcp, None, connector).await {
            Err(WsError::Http(resp)) => Err(format!("websocket: HTTP {} during the handshake", resp.status().as_u16())),
            Err(e) => Err(redact_str(&format!("websocket: {e}"))),
            Ok((s, _resp)) => Ok(s),
        }
    };
    match tokio::time::timeout(timeout, fut).await {
        Err(_) => Err(format!("websocket: connect timed out after {} ms", timeout.as_millis())),
        Ok(res) => res,
    }
}

/// One connection; methods serialize on the inner lock (one request stream at a time).
pub struct Ws {
    inner: Arc<AsyncMutex<Option<Stream>>>,
}

impl UserData for Ws {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        // send(text): one text frame
        m.add_async_method("send", |lua, this, text: String| async move {
            crate::exec_gate::check_unpoisoned(&lua)?;
            let mut g = this.inner.lock().await;
            let Some(s) = g.as_mut() else { return Err(rt_err("transport: websocket is closed")) };
            crate::exec_gate::check_unpoisoned(&lua)?;
            if let Err(e) = s.send(Message::Text(text.into())).await {
                *g = None;
                return Err(rt_err(redact_str(&format!("transport: websocket send: {e}"))));
            }
            Ok(())
        });
        // recv({idle_ms?, cancel?}) -> text | nil, reason: the next text frame. Pings are
        // answered; a close frame or EOF returns nil + reason (the connection is gone); the idle
        // watchdog raises a retryable `transport:` error, a cancel raises "aborted".
        m.add_async_method("recv", |_, this, opts: Option<Table>| async move {
            let idle = opts
                .as_ref()
                .and_then(|o| o.get::<Option<u64>>("idle_ms").ok().flatten())
                .map(Duration::from_millis);
            let tok: Option<CancellationToken> = opt_token(&opts);
            let mut g = this.inner.lock().await;
            loop {
                let Some(s) = g.as_mut() else { return Ok((None, Some("websocket is closed".to_string()))) };
                let next = s.next();
                let msg = tokio::select! {
                    m = next => m,
                    _ = async { match &tok { Some(t) => t.cancelled().await, None => std::future::pending().await } } => {
                        return Err(rt_err("aborted"));
                    }
                    _ = async { match idle { Some(d) => tokio::time::sleep(d).await, None => std::future::pending().await } } => {
                        *g = None;
                        return Err(rt_err("transport: idle timeout waiting for websocket"));
                    }
                };
                match msg {
                    Some(Ok(Message::Text(t))) => return Ok((Some(t.to_string()), None)),
                    Some(Ok(Message::Binary(b))) => return Ok((Some(String::from_utf8_lossy(&b).into_owned()), None)),
                    Some(Ok(Message::Ping(p))) => {
                        if let Err(e) = s.send(Message::Pong(p)).await {
                            *g = None;
                            return Err(rt_err(redact_str(&format!("transport: websocket pong: {e}"))));
                        }
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        *g = None;
                        let why = frame
                            .map(|f| redact_str(&format!("closed ({}: {})", u16::from(f.code), f.reason)))
                            .unwrap_or_else(|| "closed".to_string());
                        return Ok((None, Some(why)));
                    }
                    Some(Err(e)) => {
                        *g = None;
                        return Err(rt_err(redact_str(&format!("transport: websocket: {e}"))));
                    }
                    None => {
                        *g = None;
                        return Ok((None, Some("closed".to_string())));
                    }
                }
            }
        });
        m.add_async_method("close", |_, this, ()| async move {
            if let Some(mut s) = this.inner.lock().await.take() {
                let _ = s.close(None).await;
            }
            Ok(())
        });
        m.add_async_method("is_closed", |_, this, ()| async move { Ok(this.inner.lock().await.is_none()) });
    }
}

/// `mare.http.websocket { url, headers?, timeout_ms? }` → connection (raises on failure; the
/// message carries `HTTP <status>` when the handshake was answered).
pub fn install(lua: &Lua, http: &Table) -> mlua::Result<()> {
    http.set(
        "websocket",
        lua.create_async_function(|lua, opts: Table| {
            let barrier = crate::exec_gate::check_unpoisoned(&lua);
            let entry = (|| -> mlua::Result<String> {
                let url: String = opts.get("url")?;
                if let Some(active) = crate::exec_ctx::active(&lua)? {
                    if !matches!(&active.policy.network, crate::ossandbox::Network::Open) {
                        let host = url::Url::parse(&url)
                            .ok()
                            .and_then(|u| u.host_str().map(|h| h.trim_matches(['[', ']']).to_owned()))
                            .unwrap_or_else(|| "invalid WebSocket URL".to_string());
                        return Err(crate::exec_ctx::deny_network(
                            &lua,
                            &active,
                            &host,
                            "WebSockets need an open network",
                        ));
                    }
                    crate::exec_ctx::check_with(&lua, &active, "network", &url)?;
                }
                Ok(url)
            })();
            async move {
                barrier?;
                let url = entry?;
                let timeout = Duration::from_millis(opts.get::<Option<u64>>("timeout_ms")?.unwrap_or(15_000));
                let mut headers =
                    vec![("user-agent".to_string(), concat!("mare/", env!("CARGO_PKG_VERSION")).to_string())];
                if let Some(h) = opts.get::<Option<Table>>("headers")? {
                    for pair in h.pairs::<String, Value>() {
                        let (k, v) = pair?;
                        if let Value::String(s) = v {
                            headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&k));
                            headers.push((k, s.to_str()?.to_string()));
                        }
                    }
                }
                crate::exec_gate::check_unpoisoned(&lua)?;
                let s = connect(&url, headers, timeout).await.map_err(|e| rt_err(redact_str(&e)))?;
                Ok(Ws { inner: Arc::new(AsyncMutex::new(Some(s))) })
            }
        })?,
    )
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    #[tokio::test]
    async fn ws_connect_enforces_offline_guard_and_allows_loopback() {
        // Non-loopback hostnames and literal IPs must be rejected.
        for bad in ["ws://example.com:8080/ws", "ws://1.1.1.1:8080/ws", "wss://[2001:db8::1]:8080/ws"] {
            let err = connect(bad, vec![], Duration::from_secs(2)).await.unwrap_err();
            assert!(err.contains("offline test guard"), "expected offline guard for {bad}, got: {err}");
        }

        // Resolved non-loopback IP (e.g. poisoned *.localhost) must be rejected.
        let public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 8080);
        let loopback = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
        let err = filter_ws_addrs("evil.localhost", vec![public], true).unwrap_err();
        assert!(err.contains("offline test guard"), "expected offline guard for non-loopback resolution, got: {err}");
        assert_eq!(filter_ws_addrs("evil.localhost", vec![public, loopback], true).unwrap(), vec![loopback]);

        // Positive loopback case: real loopback WebSocket handshake succeeds.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let srv = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            if let Some(Ok(msg)) = ws.next().await {
                let _ = ws.send(msg).await;
            }
        });
        let mut client = connect(&format!("ws://127.0.0.1:{port}/"), vec![], Duration::from_secs(2)).await.unwrap();
        client.send(Message::Text("ping".into())).await.unwrap();
        let reply = client.next().await.unwrap().unwrap();
        assert_eq!(reply.into_text().unwrap(), "ping");
        srv.abort();
    }
}
