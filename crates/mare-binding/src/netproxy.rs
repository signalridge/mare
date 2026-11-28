//! Per-process domain-filtering proxy for sandboxed commands (Apache-2.0).
//!
//! One loopback port speaks both HTTP proxy (CONNECT + absolute-form requests) and SOCKS5
//! (sniffed by the first byte). The seatbelt profile lets the sandboxed process connect only to
//! this port; `HTTP(S)_PROXY`/`ALL_PROXY` point there. Every refusal is recorded so the caller
//! can report a structured `sandbox_denied = "network"` with the host. The host decision itself
//! (rules, approval, resolved-address checks, pinning) is the shared [`crate::netauth`] layer —
//! the same one `mare.http` uses.
//!
//! Plain-HTTP requests are parsed with `httparse` and checked whole: the `Host` header must
//! agree with the absolute-form URL (refuses a host mismatch, Apache-2.0)
//! and is rewritten to the checked authority; duplicate `Host` headers and
//! ambiguous body framing are refused; and each connection carries exactly one request
//! (`Connection: close`), so a keep-alive connection can't smuggle an unchecked second request
//! to another virtual host on the same address. No `Upgrade` over plain HTTP (use CONNECT).

use crate::netauth::{AuthTarget, ProxyRoute, is_local_proxy_host, normalize_host};
pub use crate::netauth::{NetApprover, NetAsk, NetAskOrigin, NetPolicy, approver, is_non_public, set_approver};

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

#[path = "netproxy_http.rs"]
mod http_proxy;

#[cfg(test)]
use http_proxy::{Body, Refusal, Request, forward_body, parse_request};
use http_proxy::{http, split_host_port};

/// Where the sandbox proxy sends allowed connections: straight to the checked addresses, or — when
/// mare itself runs behind a proxy (`HTTPS_PROXY` / `HTTP_PROXY` / `ALL_PROXY` with `http://`,
/// minus `NO_PROXY`, matched by reqwest's own `hyper_util` matcher) — through that upstream proxy.
#[derive(Clone)]
pub struct Upstream(Arc<hyper_util::client::proxy::matcher::Matcher>);

impl Upstream {
    /// mare's own environment.
    pub fn from_env() -> Self {
        Self(Arc::new(hyper_util::client::proxy::matcher::Matcher::from_env()))
    }
    /// An explicit upstream for both schemes (tests, config).
    pub fn fixed(proxy: &str, no_proxy: &str) -> Self {
        Self(Arc::new(
            hyper_util::client::proxy::matcher::Matcher::builder()
                .http(proxy.to_string())
                .https(proxy.to_string())
                .no(no_proxy.to_string())
                .build(),
        ))
    }
    fn for_target(&self, scheme: &str, host: &str, port: u16) -> Option<(String, Option<String>)> {
        let h = if host.contains(':') { format!("[{host}]") } else { host.to_string() };
        let uri: http::Uri = format!("{scheme}://{h}:{port}/").parse().ok()?;
        let i = self.0.intercept(&uri)?;
        // only HTTP upstreams; SOCKS / HTTPS-to-proxy upstreams are refused with a clear error
        let u = i.uri();
        let addr = format!("{}:{}", u.host()?, u.port_u16().unwrap_or(80));
        if u.scheme_str() != Some("http") {
            return Some((format!("unsupported:{}", u.scheme_str().unwrap_or("?")), None));
        }
        let auth = i.basic_auth().and_then(|v| v.to_str().ok()).map(str::to_string);
        Some((addr, auth))
    }
    /// Tunnel upstream policy (`CONNECT` / `SOCKS5`): never infer scheme from destination port.
    pub(super) fn for_tunnel(&self, host: &str, port: u16) -> Option<(String, Option<String>)> {
        self.for_target("https", host, port)
    }
    fn route_for_via(via: Option<&(String, Option<String>)>) -> ProxyRoute {
        let Some((proxy, _)) = via else {
            return ProxyRoute::Direct;
        };
        if proxy.starts_with("unsupported:") {
            return ProxyRoute::RemoteProxy;
        }
        let Ok((proxy_host, _)) = split_host_port(proxy, 80) else {
            return ProxyRoute::RemoteProxy;
        };
        if is_local_proxy_host(&proxy_host) { ProxyRoute::LocalProxy } else { ProxyRoute::RemoteProxy }
    }
}

/// Connect to `target_host:port` through an HTTP `CONNECT` proxy, sending `Proxy-Authorization`
/// when `auth` is present and reading the response head byte-by-byte so no tunnel payload is
/// consumed.
pub(crate) async fn connect_via_http_proxy(
    proxy: &str,
    auth: Option<&str>,
    target_host: &str,
    port: u16,
) -> std::io::Result<TcpStream> {
    let mut up = TcpStream::connect(proxy).await?;
    let host = target_host.trim_start_matches('[').trim_end_matches(']');
    let authority = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let auth_hdr = auth.map(|a| format!("Proxy-Authorization: {a}\r\n")).unwrap_or_default();
    let req = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n{auth_hdr}\r\n");
    up.write_all(req.as_bytes()).await?;
    // read the upstream's response head byte by byte (nothing of the tunnel may be consumed)
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if up.read(&mut b).await? == 0 || head.len() > 16 * 1024 {
            return Err(std::io::Error::other("upstream proxy closed the CONNECT"));
        }
        head.push(b[0]);
    }
    let status = String::from_utf8_lossy(&head);
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(std::io::Error::other(format!(
            "upstream proxy refused CONNECT {authority}: {}",
            status.lines().next().unwrap_or("")
        )));
    }
    Ok(up)
}

/// Connect through an HTTP `CONNECT` proxy using pre-authorized `addrs` (IP + port) in order,
/// so the upstream proxy cannot re-resolve or DNS-rebind the target.
pub(crate) async fn connect_via_http_proxy_pinned(
    proxy: &str,
    auth: Option<&str>,
    addrs: &[std::net::SocketAddr],
) -> std::io::Result<TcpStream> {
    if addrs.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "by-name upstream proxy routing is disabled when allow_local = false",
        ));
    }
    let mut last_err = None;
    for addr in addrs {
        let ip_str = addr.ip().to_string();
        match connect_via_http_proxy(proxy, auth, &ip_str, addr.port()).await {
            Ok(up) => return Ok(up),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no pinned addresses available")))
}

/// Connect to an allowed target: directly to the checked addresses, or via a CONNECT through
/// the upstream proxy. When `pin_via_ip` is true (`allow_local = false`), upstream CONNECT uses
/// the pinned IP address(es) from authorization so the proxy cannot re-resolve or rebind.
async fn connect_target(
    via: Option<(String, Option<String>)>,
    host: &str,
    port: u16,
    addrs: &[std::net::SocketAddr],
    pin_via_ip: bool,
) -> std::io::Result<TcpStream> {
    let Some((proxy, auth)) = via else { return TcpStream::connect(addrs).await };
    if let Some(s) = proxy.strip_prefix("unsupported:") {
        return Err(std::io::Error::other(format!("upstream proxy scheme {s} is not supported (use http://)")));
    }
    if !pin_via_ip {
        return connect_via_http_proxy(&proxy, auth.as_deref(), host, port).await;
    }
    connect_via_http_proxy_pinned(&proxy, auth.as_deref(), addrs).await
}

/// A running proxy; stops when the last handle drops.
pub struct ProxyHandle {
    port: u16,
    denied: Arc<Mutex<Vec<(String, String)>>>,
    task: tokio::task::AbortHandle,
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ProxyHandle {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Hosts refused so far, with reasons (first refusal first).
    pub fn denied(&self) -> Vec<(String, String)> {
        self.denied.lock().unwrap().clone()
    }

    pub fn env(&self) -> Vec<(String, String)> {
        let http = format!("http://127.0.0.1:{}", self.port);
        let socks = format!("socks5h://127.0.0.1:{}", self.port);
        let mut v = vec![];
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            v.push((k.to_string(), http.clone()));
        }
        for k in ["ALL_PROXY", "all_proxy"] {
            v.push((k.to_string(), socks.clone()));
        }
        for k in ["NO_PROXY", "no_proxy"] {
            v.push((k.to_string(), String::new()));
        }
        v
    }

    /// Bind synchronously (callable from sync Lua functions); must run inside a tokio runtime.
    pub fn start(policy: NetPolicy) -> std::io::Result<Self> {
        Self::start_in(policy, None)
    }

    /// Like [`Self::start`], binding inside `range` (inclusive) when given — the Windows enhanced
    /// tier's WFP filters only let the sandbox account reach those loopback ports.
    pub fn start_in(policy: NetPolicy, range: Option<(u16, u16)>) -> std::io::Result<Self> {
        Self::start_in_with_approver(policy, range, None)
    }

    /// Like [`Self::start_in`], binding network approval requests to `approver`.
    pub fn start_in_with_approver(
        policy: NetPolicy,
        range: Option<(u16, u16)>,
        approver: Option<NetApprover>,
    ) -> std::io::Result<Self> {
        Self::start_with_approver(policy, range, Some(Upstream::from_env()), approver)
    }

    /// Full form: `upstream` = where allowed connections go (None = always direct).
    pub fn start_with(
        policy: NetPolicy,
        range: Option<(u16, u16)>,
        upstream: Option<Upstream>,
    ) -> std::io::Result<Self> {
        Self::start_with_approver(policy, range, upstream, None)
    }

    /// Full form with explicit session/invocation network approver.
    pub fn start_with_approver(
        policy: NetPolicy,
        range: Option<(u16, u16)>,
        upstream: Option<Upstream>,
        approver: Option<NetApprover>,
    ) -> std::io::Result<Self> {
        let std_listener = match range {
            None => std::net::TcpListener::bind(("127.0.0.1", 0))?,
            Some((lo, hi)) => {
                (lo..=hi).find_map(|p| std::net::TcpListener::bind(("127.0.0.1", p)).ok()).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        format!("no free port in {lo}-{hi} for the sandbox proxy"),
                    )
                })?
            }
        };
        std_listener.set_nonblocking(true)?;
        let port = std_listener.local_addr()?.port();
        let listener = TcpListener::from_std(std_listener)?;
        let denied = Arc::new(Mutex::new(vec![]));
        let policy = Arc::new(policy);
        let d2 = denied.clone();
        let task = tokio::spawn(async move {
            // The proxy owns its client tasks too: dropping the handle closes their sockets and
            // releases pending approvals, instead of leaving detached connections alive.
            let mut clients = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((sock, _)) = accepted else { break };
                        let (p, d, u, a) = (policy.clone(), d2.clone(), upstream.clone(), approver.clone());
                        clients.spawn(async move { let _ = serve(sock, p, d, u, a).await; });
                    }
                    _ = clients.join_next(), if !clients.is_empty() => {}
                }
            }
        })
        .abort_handle();
        Ok(Self { port, denied, task })
    }
}

type Denied = Arc<Mutex<Vec<(String, String)>>>;

async fn serve(
    sock: TcpStream,
    policy: Arc<NetPolicy>,
    denied: Denied,
    up: Option<Upstream>,
    approver: Option<NetApprover>,
) -> std::io::Result<()> {
    let mut first = [0u8; 1];
    if sock.peek(&mut first).await? == 0 {
        return Ok(());
    }
    if first[0] == 0x05 {
        socks5(sock, &policy, &denied, up.as_ref(), approver.as_ref()).await
    } else {
        http(sock, &policy, &denied, up.as_ref(), approver.as_ref()).await
    }
}

fn record(denied: &Denied, host: &str, reason: &str) {
    tracing::info!(host, reason, "sandbox proxy denied");
    denied.lock().unwrap().push((normalize_host(host), reason.to_string()));
}

/// Write a short plain-text response and close (every proxy-generated response closes).
async fn respond(w: &mut (impl AsyncWriteExt + Unpin), status: &str, extra: &str, body: &str) -> std::io::Result<()> {
    let resp = format!(
        "HTTP/1.1 {status}\r\n{extra}content-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    w.write_all(resp.as_bytes()).await
}

async fn forbid(
    w: &mut (impl AsyncWriteExt + Unpin),
    denied: &Denied,
    host: &str,
    reason: &str,
) -> std::io::Result<()> {
    record(denied, host, reason);
    let body = format!("blocked by mare sandbox network policy: {reason}\n");
    respond(w, "403 Forbidden", "x-mare-sandbox: denied\r\n", &body).await
}

async fn socks5(
    mut sock: TcpStream,
    policy: &NetPolicy,
    denied: &Denied,
    upstream: Option<&Upstream>,
    approver: Option<&NetApprover>,
) -> std::io::Result<()> {
    // greeting: VER NMETHODS METHODS… → no auth
    let mut hdr = [0u8; 2];
    sock.read_exact(&mut hdr).await?;
    let mut methods = vec![0u8; hdr[1] as usize];
    sock.read_exact(&mut methods).await?;
    sock.write_all(&[5, 0]).await?;
    // request: VER CMD RSV ATYP ADDR PORT
    let mut req = [0u8; 4];
    sock.read_exact(&mut req).await?;
    let host = match req[3] {
        1 => {
            let mut a = [0u8; 4];
            sock.read_exact(&mut a).await?;
            Ipv4Addr::from(a).to_string()
        }
        3 => {
            let mut n = [0u8; 1];
            sock.read_exact(&mut n).await?;
            let mut d = vec![0u8; n[0] as usize];
            sock.read_exact(&mut d).await?;
            String::from_utf8_lossy(&d).into_owned()
        }
        4 => {
            let mut a = [0u8; 16];
            sock.read_exact(&mut a).await?;
            Ipv6Addr::from(a).to_string()
        }
        _ => return sock.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await,
    };
    let mut p = [0u8; 2];
    sock.read_exact(&mut p).await?;
    let port = u16::from_be_bytes(p);
    if req[1] != 1 {
        // only CONNECT
        return sock.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await;
    }
    let via = upstream.and_then(|u| u.for_tunnel(&host, port));
    let route = Upstream::route_for_via(via.as_ref());
    let target = match policy.authorize_target(&host, port, approver, route, policy.fake_ip).await {
        Ok(t) => t,
        Err(reason) => {
            record(denied, &host, &reason);
            // 0x02: connection not allowed by ruleset
            return sock.write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0]).await;
        }
    };
    let addrs = match &target {
        AuthTarget::Pinned(a) => a.as_slice(),
        AuthTarget::ViaLocalProxyByName => &[],
    };
    let Ok(mut up) = connect_target(via, &host, port, addrs, !policy.allows_local()).await else {
        return sock.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).await;
    };
    sock.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
    tokio::io::copy_bidirectional(&mut sock, &mut up).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fwd(head: &str) -> (String, u16, String, Body) {
        match parse_request(head.as_bytes()) {
            Ok(Request::Forward { host, port, head, body }) => (host, port, head, body),
            r => panic!("{r:?}"),
        }
    }

    #[test]
    fn every_request_form_names_one_checked_target() {
        assert_eq!(
            parse_request(b"CONNECT example.com:8443 HTTP/1.1\r\nHost: other\r\n\r\n"),
            Ok(Request::Connect { host: "example.com".into(), port: 8443 })
        );
        // absolute-form: Host rewritten to the URL authority, Connection forced to close, proxy and
        // hop-by-hop headers (and those listed in Connection) dropped
        let (h, p, head, body) =
            fwd("GET http://Example.com/a?b=1 HTTP/1.1\r\nHost: example.com:80\r\nConnection: keep-alive, X-Hop\r\n\
             X-Hop: 1\r\nProxy-Authorization: x\r\nAccept: */*\r\n\r\n");
        assert_eq!((h.as_str(), p, body), ("example.com", 80, Body::None));
        assert_eq!(head, "GET /a?b=1 HTTP/1.1\r\nHost: example.com\r\nAccept: */*\r\nConnection: close\r\n\r\n");
        // no Host header: the URL authority is sent
        let (_, p, head, _) = fwd("GET http://[::1]:8080/ HTTP/1.0\r\n\r\n");
        assert_eq!(p, 8080);
        assert!(head.starts_with("GET / HTTP/1.0\r\nHost: [::1]:8080\r\n"), "{head}");
        // origin-form: the Host header is the target
        let (h, p, _, _) = fwd("GET /x HTTP/1.1\r\nHost: api.test:81\r\n\r\n");
        assert_eq!((h.as_str(), p), ("api.test", 81));
    }

    #[test]
    fn fronting_duplicates_and_smuggling_are_refused() {
        // Host-header fronting: the URL is checked, so the Host must name the same authority
        assert!(matches!(
            parse_request(b"GET http://allowed.test/ HTTP/1.1\r\nHost: evil.test\r\n\r\n"),
            Err(Refusal::Mismatch { .. })
        ));
        assert!(matches!(
            parse_request(b"GET http://allowed.test/ HTTP/1.1\r\nHost: allowed.test:8080\r\n\r\n"),
            Err(Refusal::Mismatch { .. })
        ));
        for bad in [
            "GET http://a.test/ HTTP/1.1\r\nHost: a.test\r\nHost: b.test\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: a.test\r\nhost: b.test\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: a.test:bad\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: a.test:0\r\n\r\n",
            "CONNECT a.test:0 HTTP/1.1\r\nHost: a.test:0\r\n\r\n",
            "CONNECT [::1 HTTP/1.1\r\n\r\n",
            "POST http://a.test/ HTTP/1.1\r\nContent-Length: +5\r\n\r\n",
            "POST http://a.test/ HTTP/1.1\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n",
            "POST http://a.test/ HTTP/1.1\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\n",
            "POST http://a.test/ HTTP/1.1\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
            "GET https://a.test/ HTTP/1.1\r\n\r\n",
            "GET http://a.test/ HTTP/1.1\r\nBad Header\r\n\r\n",
        ] {
            assert!(matches!(parse_request(bad.as_bytes()), Err(Refusal::Bad(_))), "{bad}");
        }
        assert_eq!(fwd("POST http://a.test/ HTTP/1.1\r\nContent-Length: 5\r\n\r\n").3, Body::Length(5));
        assert_eq!(fwd("POST http://a.test/ HTTP/1.1\r\nTransfer-Encoding: Chunked\r\n\r\n").3, Body::Chunked);
    }

    #[tokio::test]
    async fn exactly_one_body_is_forwarded() {
        let mut src = BufReader::new(&b"3\r\nabc\r\n0\r\nX-T: 1\r\n\r\nGET http://evil.test/ HTTP/1.1\r\n\r\n"[..]);
        let mut out = Vec::new();
        forward_body(&mut src, &mut out, &Body::Chunked).await.unwrap();
        assert_eq!(out, b"3\r\nabc\r\n0\r\nX-T: 1\r\n\r\n");
        let mut src = BufReader::new(&b"helloGET / HTTP/1.1\r\n\r\n"[..]);
        let mut out = Vec::new();
        forward_body(&mut src, &mut out, &Body::Length(5)).await.unwrap();
        assert_eq!(out, b"hello");
        let mut src = BufReader::new(&b"hi"[..]);
        assert!(forward_body(&mut src, &mut Vec::new(), &Body::Length(5)).await.is_err());

        // Reject chunk-size u64::MAX + 2 overflow, '+' prefix, and missing CRLF.
        for bad_chunk in [
            &b"ffffffffffffffff\r\n"[..],
            &b"+3\r\nabc\r\n0\r\n\r\n"[..],
            &b"3\r\nabcXX0\r\n\r\n"[..],
            &b"3\nabc\n0\n\n"[..],
        ] {
            let mut src = BufReader::new(bad_chunk);
            assert!(forward_body(&mut src, &mut Vec::new(), &Body::Chunked).await.is_err());
        }
    }

    #[tokio::test]
    async fn fake_ip_http_forward_on_non_80_port_uses_same_http_proxy_for_auth_and_connect() {
        let _guard = crate::netauth::RESOLVER_TEST_LOCK.lock().await;
        crate::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "fake.test".to_string(),
            vec!["198.18.0.5".parse().unwrap()],
        )])));

        // Local HTTP_PROXY mock: expects CONNECT fake.test:8443 then GET /hello, replies "via-local".
        let local_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local_port = local_listener.local_addr().unwrap().port();
        let seen_connect = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_c = seen_connect.clone();
        let mock_task = tokio::spawn(async move {
            while let Ok((sock, _)) = local_listener.accept().await {
                let seen_c = seen_c.clone();
                tokio::spawn(async move {
                    let mut r = BufReader::new(sock);
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    seen_c.lock().unwrap().push(line.trim().to_string());
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).await.unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let mut sock = r.into_inner();
                    let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
                    let mut r = BufReader::new(sock);
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).await.unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let mut sock = r.into_inner();
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nvia-local")
                        .await;
                });
            }
        });

        let upstream = Upstream(Arc::new(
            hyper_util::client::proxy::matcher::Matcher::builder()
                .http(format!("http://127.0.0.1:{local_port}"))
                .https("http://remote.invalid:8080".to_string())
                .build(),
        ));
        let mut policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "fake.test" }, allow_local = true }"#).eval().unwrap();
            NetPolicy::from_lua(&t).unwrap()
        };
        policy.fake_ip = crate::netauth::FakeIpMode::Auto;
        let proxy = ProxyHandle::start_with(policy, None, Some(upstream.clone())).unwrap();

        // 1. Plain HTTP forward on port 8443: authorized via HTTP_PROXY (local), must connect via HTTP_PROXY (local).
        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s.write_all(b"GET http://fake.test:8443/hello HTTP/1.1\r\nHost: fake.test:8443\r\n\r\n").await.unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(
            resp.starts_with("HTTP/1.1 200 OK\r\n") && resp.ends_with("via-local"),
            "expected 200 via-local, got: {resp:?}"
        );
        assert!(
            seen_connect.lock().unwrap().iter().any(|l| l == "CONNECT fake.test:8443 HTTP/1.1"),
            "expected CONNECT to local HTTP_PROXY, got: {:?}",
            seen_connect.lock().unwrap()
        );
        assert!(proxy.denied().is_empty(), "unexpected denials: {:?}", proxy.denied());

        // 2. CONNECT on port 8443 with allow_local = false: uses HTTPS_PROXY (remote), so fake-IP must be refused (403).
        let mut strict_policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "fake.test" } }"#).eval().unwrap();
            NetPolicy::from_lua(&t).unwrap()
        };
        strict_policy.fake_ip = crate::netauth::FakeIpMode::Auto;
        let strict_proxy = ProxyHandle::start_with(strict_policy, None, Some(upstream)).unwrap();
        let mut s2 = TcpStream::connect(("127.0.0.1", strict_proxy.port())).await.unwrap();
        s2.write_all(b"CONNECT fake.test:8443 HTTP/1.1\r\nHost: fake.test:8443\r\n\r\n").await.unwrap();
        let mut resp2 = String::new();
        s2.read_to_string(&mut resp2).await.unwrap();
        assert!(resp2.starts_with("HTTP/1.1 403 Forbidden\r\n"), "expected 403 for remote HTTPS_PROXY, got: {resp2:?}");

        mock_task.abort();
        drop(strict_proxy);
        drop(proxy);
        crate::netauth::set_resolver_override(None);
    }

    #[tokio::test]
    async fn upstream_proxy_pins_authorized_ip_on_allow_local_false_preventing_dns_rebinding() {
        let _guard = crate::netauth::RESOLVER_TEST_LOCK.lock().await;
        crate::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "rebind.test".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        )])));

        // Private server on 127.0.0.1 representing an internal service reached only via DNS rebinding.
        let priv_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let priv_port = priv_listener.local_addr().unwrap().port();
        let priv_task = tokio::spawn(async move {
            while let Ok((sock, _)) = priv_listener.accept().await {
                tokio::spawn(async move {
                    let mut r = BufReader::new(sock);
                    let mut line = String::new();
                    let _ = r.read_line(&mut line).await;
                    let mut sock = r.into_inner();
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nprivate-secret")
                        .await;
                });
            }
        });

        // Upstream proxy: if given hostname `rebind.test`, rebinds to 127.0.0.1:priv_port; if given pinned IP, refuses.
        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = up_listener.local_addr().unwrap().port();
        let seen_connect = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_c = seen_connect.clone();
        let up_task = tokio::spawn(async move {
            while let Ok((sock, _)) = up_listener.accept().await {
                let seen_c = seen_c.clone();
                tokio::spawn(async move {
                    let mut r = BufReader::new(sock);
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let trimmed = line.trim().to_string();
                    seen_c.lock().unwrap().push(trimmed.clone());
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).await.unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                    }
                    let mut sock = r.into_inner();
                    let target = trimmed.split_whitespace().nth(1).unwrap_or("");
                    if target.starts_with("rebind.test:") {
                        if let Ok(mut priv_sock) = TcpStream::connect(("127.0.0.1", priv_port)).await {
                            let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
                            let _ = tokio::io::copy_bidirectional(&mut sock, &mut priv_sock).await;
                        }
                    } else {
                        let _ = sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    }
                });
            }
        });

        let policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "rebind.test" } }"#).eval().unwrap();
            NetPolicy::from_lua(&t).unwrap()
        };
        let upstream = Upstream::fixed(&format!("http://127.0.0.1:{up_port}"), "");
        let proxy = ProxyHandle::start_with(policy, None, Some(upstream)).unwrap();

        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        let req = format!("GET http://rebind.test:{priv_port}/ HTTP/1.1\r\nHost: rebind.test:{priv_port}\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(
            !resp.contains("private-secret"),
            "upstream proxy must not rebind hostname to private server: {resp:?}"
        );
        assert_eq!(
            seen_connect.lock().unwrap().as_slice(),
            [format!("CONNECT 93.184.216.34:{priv_port} HTTP/1.1")],
            "upstream proxy must receive pinned IP, not hostname"
        );

        priv_task.abort();
        up_task.abort();
        drop(proxy);
        crate::netauth::set_resolver_override(None);
    }

    #[tokio::test]
    async fn local_upstream_proxy_rejects_mixed_fake_ipv4_and_ula_when_allow_local_false() {
        let _guard = crate::netauth::RESOLVER_TEST_LOCK.lock().await;
        crate::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "mixed-fake.test".to_string(),
            vec!["198.18.0.5".parse().unwrap(), "fd00::1".parse().unwrap()],
        )])));

        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = up_listener.local_addr().unwrap().port();
        let seen_connect = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_c = seen_connect.clone();
        let up_task = tokio::spawn(async move {
            while let Ok((sock, _)) = up_listener.accept().await {
                let seen_c = seen_c.clone();
                tokio::spawn(async move {
                    let mut r = BufReader::new(sock);
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) > 0 {
                        seen_c.lock().unwrap().push(line.trim().to_string());
                    }
                    let mut sock = r.into_inner();
                    let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
                });
            }
        });

        let mut policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "mixed-fake.test" } }"#).eval().unwrap();
            NetPolicy::from_lua(&t).unwrap()
        };
        policy.fake_ip = crate::netauth::FakeIpMode::Auto;
        let upstream = Upstream::fixed(&format!("http://127.0.0.1:{up_port}"), "");
        let proxy = ProxyHandle::start_with(policy, None, Some(upstream)).unwrap();

        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s.write_all(b"CONNECT mixed-fake.test:443 HTTP/1.1\r\nHost: mixed-fake.test:443\r\n\r\n").await.unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 403 Forbidden\r\n"), "expected 403 Forbidden, got: {resp:?}");
        assert!(seen_connect.lock().unwrap().is_empty(), "upstream proxy must not be contacted: {:?}", seen_connect);

        up_task.abort();
        drop(proxy);
        crate::netauth::set_resolver_override(None);
    }

    #[tokio::test]
    async fn shared_connect_via_http_proxy_pinned_handles_empty_fallback_auth_ipv6_and_pipelined_bytes() {
        let err = connect_via_http_proxy_pinned("127.0.0.1:1", None, &[]).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

        let up_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = up_listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let seen_c = seen.clone();
        let up_task = tokio::spawn(async move {
            while let Ok((sock, _)) = up_listener.accept().await {
                let seen_c = seen_c.clone();
                tokio::spawn(async move {
                    let mut r = BufReader::new(sock);
                    let mut line = String::new();
                    if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let mut auth_hdr = None;
                    loop {
                        let mut h = String::new();
                        if r.read_line(&mut h).await.unwrap_or(0) == 0 || h == "\r\n" {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':')
                            && k.eq_ignore_ascii_case("proxy-authorization")
                        {
                            auth_hdr = Some(v.trim().to_string());
                        }
                    }
                    let req_line = line.trim().to_string();
                    let target = req_line.split_whitespace().nth(1).unwrap_or("").to_string();
                    seen_c.lock().unwrap().push((req_line, auth_hdr));
                    let mut sock = r.into_inner();
                    if target == "[::1]:8443" {
                        let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\npipelined-bytes").await;
                    } else {
                        let _ = sock.write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n").await;
                    }
                });
            }
        });

        let addrs = vec![
            "203.0.113.10:8443".parse::<std::net::SocketAddr>().unwrap(),
            "[::1]:8443".parse::<std::net::SocketAddr>().unwrap(),
        ];
        let mut up = connect_via_http_proxy_pinned(&format!("127.0.0.1:{up_port}"), Some("Basic dXNlcjpwYXNz"), &addrs)
            .await
            .unwrap();
        let mut buf = [0u8; 15];
        up.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pipelined-bytes", "CONNECT head reader must not consume tunnel payload");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                ("CONNECT 203.0.113.10:8443 HTTP/1.1".to_string(), Some("Basic dXNlcjpwYXNz".to_string())),
                ("CONNECT [::1]:8443 HTTP/1.1".to_string(), Some("Basic dXNlcjpwYXNz".to_string())),
            ]
        );
        up_task.abort();
    }

    #[tokio::test]
    async fn socks5_handles_ipv4_ipv6_bad_atyp_non_connect_and_denied_targets() {
        async fn socks5_exchange(port: u16, req: &[u8]) -> [u8; 10] {
            let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            s.write_all(&[5, 1, 0]).await.unwrap();
            let mut greeting = [0u8; 2];
            s.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 0]);
            s.write_all(req).await.unwrap();
            let mut reply = [0u8; 10];
            s.read_exact(&mut reply).await.unwrap();
            reply
        }

        let policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "only.example" } }"#).eval().unwrap();
            NetPolicy::from_lua(&t).unwrap()
        };
        let proxy = ProxyHandle::start_with(policy, None, None).unwrap();

        // IPv4 target (93.184.216.34:80) -> denied by policy -> REP 0x02 (ruleset).
        let r4 = socks5_exchange(proxy.port(), &[5, 1, 0, 1, 93, 184, 216, 34, 0, 80]).await;
        assert_eq!(r4, [5, 2, 0, 1, 0, 0, 0, 0, 0, 0]);

        // IPv6 target (2001:db8::1:80) -> denied by policy -> REP 0x02 (ruleset).
        let r6 = socks5_exchange(
            proxy.port(),
            &[5, 1, 0, 4, 0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 80],
        )
        .await;
        assert_eq!(r6, [5, 2, 0, 1, 0, 0, 0, 0, 0, 0]);

        // Unsupported ATYP=9 -> REP 0x08 (address type not supported).
        let r_atyp = socks5_exchange(proxy.port(), &[5, 1, 0, 9]).await;
        assert_eq!(r_atyp, [5, 8, 0, 1, 0, 0, 0, 0, 0, 0]);

        // Non-CONNECT CMD=2 (BIND) with domain ATYP=3 -> REP 0x07 (command not supported).
        let mut bind_req = vec![5, 2, 0, 3, 12];
        bind_req.extend_from_slice(b"only.example");
        bind_req.extend_from_slice(&[0, 80]);
        let r_cmd = socks5_exchange(proxy.port(), &bind_req).await;
        assert_eq!(r_cmd, [5, 7, 0, 1, 0, 0, 0, 0, 0, 0]);

        let denied_hosts: Vec<String> = proxy.denied().into_iter().map(|(h, _)| h).collect();
        assert_eq!(denied_hosts, ["93.184.216.34", "2001:db8::1"]);
    }

    #[tokio::test]
    async fn proxy_start_range_env_and_socks5_tunnel_forward() {
        let policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "127.0.0.1" }, allow_local = true }"#).eval().unwrap();
            NetPolicy::from_lua(&t).unwrap()
        };

        // Busy port inside a 1-port range returns AddrInUse; wider range succeeds on the next free port.
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = occupied.local_addr().unwrap().port();
        let err = ProxyHandle::start_in(policy.clone(), Some((busy_port, busy_port)))
            .err()
            .expect("single occupied port must fail");
        assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

        let proxy = if busy_port < u16::MAX - 5 {
            ProxyHandle::start_in(policy, Some((busy_port, busy_port + 5))).unwrap()
        } else {
            ProxyHandle::start(policy).unwrap()
        };
        drop(occupied);

        let env = proxy.env();
        let expected_http = format!("http://127.0.0.1:{}", proxy.port());
        let expected_socks = format!("socks5h://127.0.0.1:{}", proxy.port());
        assert!(env.contains(&("HTTP_PROXY".to_string(), expected_http)));
        assert!(env.contains(&("ALL_PROXY".to_string(), expected_socks)));
        assert!(env.contains(&("NO_PROXY".to_string(), String::new())));

        // Immediate EOF before first byte returns cleanly.
        let eof_sock = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        drop(eof_sock);

        // Live loopback target succeeds over SOCKS5 CONNECT and tunnels bytes.
        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target_listener.local_addr().unwrap().port();
        let target_task = tokio::spawn(async move {
            if let Ok((mut s, _)) = target_listener.accept().await {
                let mut buf = [0u8; 4];
                if s.read_exact(&mut buf).await.is_ok() {
                    let _ = s.write_all(&buf).await;
                }
            }
        });

        let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s.write_all(&[5, 1, 0]).await.unwrap();
        let mut greeting = [0u8; 2];
        s.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);
        let [hi, lo] = target_port.to_be_bytes();
        s.write_all(&[5, 1, 0, 1, 127, 0, 0, 1, hi, lo]).await.unwrap();
        let mut reply = [0u8; 10];
        s.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        s.write_all(b"ping").await.unwrap();
        let mut echoed = [0u8; 4];
        s.read_exact(&mut echoed).await.unwrap();
        assert_eq!(&echoed, b"ping");
        target_task.abort();

        // Closed port triggers REP 0x05 (connection refused).
        let closed_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_port = closed_listener.local_addr().unwrap().port();
        drop(closed_listener);
        let [chi, clo] = closed_port.to_be_bytes();
        let mut s2 = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s2.write_all(&[5, 1, 0]).await.unwrap();
        s2.read_exact(&mut greeting).await.unwrap();
        s2.write_all(&[5, 1, 0, 1, 127, 0, 0, 1, chi, clo]).await.unwrap();
        s2.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 5, 0, 1, 0, 0, 0, 0, 0, 0]);
    }

    #[tokio::test]
    async fn connect_uses_tunnel_upstream_and_forward_uses_http_upstream_regardless_of_port() {
        let _guard = crate::netauth::RESOLVER_TEST_LOCK.lock().await;
        crate::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "cross-port.test".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        )])));

        async fn spawn_mock_upstream(
            label: &'static str,
        ) -> (u16, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let seen = Arc::new(Mutex::new(Vec::<String>::new()));
            let seen_c = seen.clone();
            let task = tokio::spawn(async move {
                while let Ok((sock, _)) = listener.accept().await {
                    let seen_c = seen_c.clone();
                    tokio::spawn(async move {
                        let mut r = BufReader::new(sock);
                        let mut line = String::new();
                        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        seen_c.lock().unwrap().push(line.trim().to_string());
                        loop {
                            let mut h = String::new();
                            if r.read_line(&mut h).await.unwrap_or(0) == 0 || h == "\r\n" {
                                break;
                            }
                        }
                        let mut sock = r.into_inner();
                        let _ = sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await;
                        let mut r = BufReader::new(sock);
                        let mut first = String::new();
                        let _ = r.read_line(&mut first).await;
                        if first.starts_with("GET ") || first.starts_with("POST ") {
                            loop {
                                let mut h = String::new();
                                if r.read_line(&mut h).await.unwrap_or(0) == 0 || h == "\r\n" {
                                    break;
                                }
                            }
                        }
                        let mut sock = r.into_inner();
                        let body = format!("via-{label}");
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = sock.write_all(resp.as_bytes()).await;
                    });
                }
            });
            (port, seen, task)
        }

        let (http_port, http_seen, http_task) = spawn_mock_upstream("http").await;
        let (https_port, https_seen, https_task) = spawn_mock_upstream("https").await;

        let upstream = Upstream(Arc::new(
            hyper_util::client::proxy::matcher::Matcher::builder()
                .http(format!("http://127.0.0.1:{http_port}"))
                .https(format!("http://127.0.0.1:{https_port}"))
                .build(),
        ));
        let policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "cross-port.test" } }"#).eval().unwrap();
            NetPolicy::from_lua(&t).unwrap()
        };
        let proxy = ProxyHandle::start_with(policy, None, Some(upstream)).unwrap();

        // 1. Plain HTTP absolute-URI request on port 443 must use HTTP_PROXY.
        let mut s_http = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s_http
            .write_all(b"GET http://cross-port.test:443/hello HTTP/1.1\r\nHost: cross-port.test:443\r\n\r\n")
            .await
            .unwrap();
        let mut resp_http = String::new();
        s_http.read_to_string(&mut resp_http).await.unwrap();
        assert!(resp_http.ends_with("via-http"), "expected response body via-http, got: {resp_http:?}");
        assert_eq!(http_seen.lock().unwrap().len(), 1, "http upstream should receive 1 request");
        assert!(
            https_seen.lock().unwrap().is_empty(),
            "https upstream must receive none: {:?}",
            https_seen.lock().unwrap()
        );

        // 2. HTTP CONNECT tunnel on port 80 must use the tunnel (HTTPS) upstream, not HTTP_PROXY.
        let mut s_connect = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s_connect.write_all(b"CONNECT cross-port.test:80 HTTP/1.1\r\nHost: cross-port.test:80\r\n\r\n").await.unwrap();
        let mut cr = BufReader::new(s_connect);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            if cr.read_line(&mut line).await.unwrap() == 0 || line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        assert!(head.starts_with("HTTP/1.1 200 Connection Established"), "CONNECT failed: {head:?}");
        let mut s_connect = cr.into_inner();
        s_connect.write_all(b"GET /tunnel HTTP/1.1\r\nHost: cross-port.test:80\r\n\r\n").await.unwrap();
        let mut resp_connect = String::new();
        s_connect.read_to_string(&mut resp_connect).await.unwrap();
        assert!(
            resp_connect.ends_with("via-https"),
            "expected tunneled response body via-https, got: {resp_connect:?}"
        );
        assert_eq!(http_seen.lock().unwrap().len(), 1, "http upstream must not receive CONNECT:80");
        assert_eq!(https_seen.lock().unwrap().len(), 1, "https upstream should receive CONNECT:80");

        // 3. SOCKS5 CONNECT tunnel on port 80 must also use the tunnel (HTTPS) upstream.
        let mut s_socks = TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s_socks.write_all(&[5, 1, 0]).await.unwrap();
        let mut greeting = [0u8; 2];
        s_socks.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);
        let target = b"cross-port.test";
        let mut req = vec![5, 1, 0, 3, target.len() as u8];
        req.extend_from_slice(target);
        req.extend_from_slice(&80u16.to_be_bytes());
        s_socks.write_all(&req).await.unwrap();
        let mut reply = [0u8; 10];
        s_socks.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        s_socks.write_all(b"GET /socks HTTP/1.1\r\nHost: cross-port.test:80\r\n\r\n").await.unwrap();
        let mut resp_socks = String::new();
        s_socks.read_to_string(&mut resp_socks).await.unwrap();
        assert!(resp_socks.ends_with("via-https"), "expected SOCKS5 response body via-https, got: {resp_socks:?}");
        assert_eq!(http_seen.lock().unwrap().len(), 1, "http upstream must not receive SOCKS5:80");
        assert_eq!(https_seen.lock().unwrap().len(), 2, "https upstream should receive SOCKS5:80");

        http_task.abort();
        https_task.abort();
        drop(proxy);
        crate::netauth::set_resolver_override(None);
    }
}
