// exec-ctx: not a tool entry (HTTP proxy protocol helpers split from netproxy.rs, host sandbox proxy)
use super::*;

/// How the request body after the head is delimited (RFC 9112 §6).
#[derive(Debug, PartialEq)]
pub(super) enum Body {
    None,
    Length(u64),
    Chunked,
}

/// A parsed, checked HTTP/1.x proxy request (one per client connection).
#[derive(Debug, PartialEq)]
pub(super) enum Request {
    /// `CONNECT host:port`: a tunnel to the checked target
    Connect { host: String, port: u16 },
    /// a plain-HTTP request, rewritten for the origin server: the head to send and its body framing
    Forward { host: String, port: u16, head: String, body: Body },
}

/// Why a request head is refused: `Bad` (400, malformed or ambiguous) or `Mismatch` (403: the
/// `Host` header names another host than the request target — Host-header fronting).
#[derive(Debug, PartialEq)]
pub(super) enum Refusal {
    Bad(&'static str),
    Mismatch { target: String, host_header: String },
}

/// Headers never forwarded: hop-by-hop ones (RFC 9110 §7.6.1) and the proxy's own. `Host` is
/// replaced by the checked authority; `Connection: close` is always sent.
const DROPPED: &[&str] = &[
    "host",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "proxy-authenticate",
    "te",
    "trailer",
    "upgrade",
];

/// Parse and check one request head with `httparse`. Every form names its
/// target exactly once: `CONNECT host:port`, the absolute-form URL's authority, or (origin-form)
/// the single `Host` header. A `Host` header that disagrees with the URL authority is refused,
/// as are duplicate `Host` headers and ambiguous body framing (request smuggling). Forwarded
/// requests carry `Host: <checked authority>` and `Connection: close`: one request per
/// connection, so a keep-alive connection can never carry a second, unchecked request.
pub(super) fn parse_request(head: &[u8]) -> Result<Request, Refusal> {
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(Refusal::Bad("incomplete request head")),
        Err(_) => return Err(Refusal::Bad("malformed request head")),
    }
    let (Some(method), Some(target), Some(version)) = (req.method, req.path, req.version) else {
        return Err(Refusal::Bad("malformed request line"));
    };
    let header = |name: &'static str| req.headers.iter().filter(move |h| h.name.eq_ignore_ascii_case(name));
    let mut hosts = header("host");
    let host_header = hosts.next().map(|h| String::from_utf8_lossy(h.value).trim().to_string());
    if hosts.next().is_some() {
        return Err(Refusal::Bad("more than one Host header"));
    }
    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(target, 443)?;
        if host.is_empty() {
            return Err(Refusal::Bad("CONNECT needs host:port"));
        }
        return Ok(Request::Connect { host, port });
    }
    let (host, port, path) = if target.starts_with('/') {
        // origin-form: the Host header is the target
        let Some(h) = host_header.as_deref().filter(|h| !h.is_empty()) else {
            return Err(Refusal::Bad("missing Host header"));
        };
        let (host, port) = split_host_port(h, 80)?;
        (host, port, target.to_string())
    } else {
        let Ok(u) = url::Url::parse(target) else { return Err(Refusal::Bad("bad request target")) };
        if u.scheme() != "http" {
            return Err(Refusal::Bad("only http:// requests can be proxied without CONNECT"));
        }
        let host = match u.host() {
            Some(url::Host::Domain(d)) => d.to_string(),
            Some(url::Host::Ipv4(a)) => a.to_string(),
            Some(url::Host::Ipv6(a)) => a.to_string(),
            None => return Err(Refusal::Bad("request target has no host")),
        };
        let port = u.port_or_known_default().unwrap_or(80);
        if port == 0 {
            return Err(Refusal::Bad("port 0 is invalid"));
        }
        if let Some(h) = host_header.as_deref().filter(|h| !h.is_empty()) {
            let (hh, hp) = split_host_port(h, 80)?;
            if normalize_host(&hh) != normalize_host(&host) || hp != port {
                return Err(Refusal::Mismatch { target: host, host_header: h.to_string() });
            }
        }
        let path = match u.query() {
            Some(q) => format!("{}?{q}", u.path()),
            None => u.path().to_string(),
        };
        (host, port, path)
    };
    if host.is_empty() {
        return Err(Refusal::Bad("empty host"));
    }
    // body framing: Transfer-Encoding (only plain `chunked`) or one Content-Length, never both
    let te: Vec<String> =
        header("transfer-encoding").map(|h| String::from_utf8_lossy(h.value).trim().to_ascii_lowercase()).collect();
    let mut cls = header("content-length").map(|h| String::from_utf8_lossy(h.value).trim().to_string());
    let cl = cls.next();
    if cls.any(|c| Some(&c) != cl.as_ref()) {
        return Err(Refusal::Bad("conflicting Content-Length headers"));
    }
    let body = match (te.is_empty(), cl) {
        (false, Some(_)) => return Err(Refusal::Bad("both Transfer-Encoding and Content-Length")),
        (false, None) if te.len() == 1 && te[0] == "chunked" => Body::Chunked,
        (false, None) => return Err(Refusal::Bad("unsupported Transfer-Encoding")),
        (true, Some(c)) => {
            if c.is_empty() || !c.bytes().all(|b| b.is_ascii_digit()) {
                return Err(Refusal::Bad("bad Content-Length"));
            }
            match c.parse::<u64>() {
                Ok(0) => Body::None,
                Ok(n) => Body::Length(n),
                Err(_) => return Err(Refusal::Bad("bad Content-Length")),
            }
        }
        (true, None) => Body::None,
    };
    // headers named in Connection are hop-by-hop too
    let listed: Vec<String> = header("connection")
        .flat_map(|h| {
            String::from_utf8_lossy(h.value).split(',').map(|t| t.trim().to_ascii_lowercase()).collect::<Vec<_>>()
        })
        .collect();
    let authority = {
        let h = if host.contains(':') { format!("[{host}]") } else { host.clone() };
        if port == 80 { h } else { format!("{h}:{port}") }
    };
    let mut out = format!("{method} {path} HTTP/1.{version}\r\nHost: {authority}\r\n");
    for h in req.headers.iter() {
        let k = h.name.to_ascii_lowercase();
        if DROPPED.contains(&k.as_str()) || listed.contains(&k) {
            continue;
        }
        out.push_str(h.name);
        out.push_str(": ");
        out.push_str(&String::from_utf8_lossy(h.value));
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    Ok(Request::Forward { host, port, head: out, body })
}

async fn read_line_bounded(
    r: &mut (impl AsyncBufReadExt + Unpin),
    out: &mut Vec<u8>,
    max_bytes: usize,
) -> std::io::Result<usize> {
    let mut total = 0usize;
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return Ok(total);
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if total + take > max_bytes {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line exceeds limit"));
            }
            out.extend_from_slice(&buf[..take]);
            r.consume(take);
            return Ok(total + take);
        }
        let len = buf.len();
        if total + len > max_bytes {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "line exceeds limit"));
        }
        out.extend_from_slice(buf);
        r.consume(len);
        total += len;
    }
}

/// Copy exactly one request body from `r` to `w` (nothing after it: a pipelined second request
/// is never forwarded).
pub(super) async fn forward_body(
    r: &mut (impl AsyncBufReadExt + Unpin),
    w: &mut (impl AsyncWriteExt + Unpin),
    body: &Body,
) -> std::io::Result<()> {
    match body {
        Body::None => {}
        Body::Length(n) => {
            let copied = tokio::io::copy(&mut r.take(*n), w).await?;
            if copied != *n {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "request body cut short"));
            }
        }
        Body::Chunked => loop {
            // chunk-size [; ext] CRLF, data CRLF … 0 CRLF, trailers, CRLF
            let mut line = Vec::new();
            let n_read = read_line_bounded(r, &mut line, 4096).await?;
            if n_read == 0 || !line.ends_with(b"\r\n") {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk header"));
            }
            w.write_all(&line).await?;
            let text = String::from_utf8_lossy(&line);
            let size = text.split(';').next().unwrap_or("").trim();
            if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"));
            }
            let n = u64::from_str_radix(size, 16)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk size"))?;
            let _want = n
                .checked_add(2)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk size overflow"))?;
            if n == 0 {
                loop {
                    let mut t = Vec::new();
                    let tn = read_line_bounded(r, &mut t, 8192).await?;
                    if tn == 0 || !t.ends_with(b"\r\n") {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk trailer"));
                    }
                    w.write_all(&t).await?;
                    if t == b"\r\n" {
                        break;
                    }
                }
                break;
            }
            // the data and its CRLF
            if tokio::io::copy(&mut r.take(n), w).await? != n {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "chunk cut short"));
            }
            let mut crlf = [0u8; 2];
            r.read_exact(&mut crlf).await.map_err(|e| {
                if e.kind() == std::io::ErrorKind::UnexpectedEof {
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "chunk cut short")
                } else {
                    e
                }
            })?;
            if &crlf != b"\r\n" {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "missing chunk CRLF"));
            }
            w.write_all(&crlf).await?;
        },
    }
    w.flush().await
}

pub(super) async fn http(
    sock: TcpStream,
    policy: &NetPolicy,
    denied: &Denied,
    upstream: Option<&Upstream>,
    approver: Option<&NetApprover>,
) -> std::io::Result<()> {
    const MAX_HEAD_BYTES: usize = 64 * 1024;
    let mut r = BufReader::new(sock);
    let mut head = Vec::new();
    loop {
        let remaining = MAX_HEAD_BYTES.saturating_sub(head.len());
        if remaining == 0 {
            return respond(r.get_mut(), "431 Request Header Fields Too Large", "", "request head too large\n").await;
        }
        let n = match read_line_bounded(&mut r, &mut head, remaining).await {
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                return respond(r.get_mut(), "431 Request Header Fields Too Large", "", "request head too large\n")
                    .await;
            }
            Err(e) => return Err(e),
        };
        if n == 0 || head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
    }
    let req = match parse_request(&head) {
        Ok(req) => req,
        Err(Refusal::Bad(why)) => return respond(r.get_mut(), "400 Bad Request", "", &format!("{why}\n")).await,
        Err(Refusal::Mismatch { target, host_header }) => {
            let reason = format!("Host header `{host_header}` does not match the request target `{target}`");
            return forbid(r.get_mut(), denied, &host_header, &reason).await;
        }
    };
    let (host, port, via) = match &req {
        Request::Connect { host, port } => (host.clone(), *port, upstream.and_then(|u| u.for_tunnel(host, *port))),
        Request::Forward { host, port, .. } => {
            (host.clone(), *port, upstream.and_then(|u| u.for_target("http", host, *port)))
        }
    };
    let route = Upstream::route_for_via(via.as_ref());
    let target = match policy.authorize_target(&host, port, approver, route, policy.fake_ip).await {
        Ok(t) => t,
        Err(reason) => return forbid(r.get_mut(), denied, &host, &reason).await,
    };
    let addrs = match &target {
        AuthTarget::Pinned(a) => a.as_slice(),
        AuthTarget::ViaLocalProxyByName => &[],
    };
    let mut up = match connect_target(via, &host, port, addrs, !policy.allows_local()).await {
        Ok(s) => s,
        Err(e) => return respond(r.get_mut(), "502 Bad Gateway", "", &format!("mare sandbox proxy: {e}\n")).await,
    };
    match req {
        Request::Connect { .. } => {
            // a tunnel to the checked address: what flows inside is opaque
            let buffered = r.buffer().to_vec();
            let mut client = r.into_inner();
            client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
            up.write_all(&buffered).await?;
            tokio::io::copy_bidirectional(&mut client, &mut up).await?;
        }
        Request::Forward { head, body, .. } => {
            up.write_all(head.as_bytes()).await?;
            let buffered = r.buffer().to_vec();
            let (cr, mut cw) = r.into_inner().into_split();
            let mut cr = BufReader::new(std::io::Cursor::new(buffered).chain(cr));
            let (mut ur, mut uw) = up.into_split();
            // the response streams back while the body goes up (Expect: 100-continue); once the
            // origin closes (it must: Connection: close) the client connection closes too
            let recv = async {
                tokio::io::copy(&mut ur, &mut cw).await?;
                cw.shutdown().await
            };
            tokio::pin!(recv);
            tokio::select! {
                res = &mut recv => return res,
                s = forward_body(&mut cr, &mut uw, &body) => s?,
            }
            recv.await?;
        }
    }
    Ok(())
}

pub(super) fn split_host_port(s: &str, default: u16) -> Result<(String, u16), Refusal> {
    let parse_port = |p: &str| -> Result<u16, Refusal> {
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Refusal::Bad("invalid port"));
        }
        let port = p.parse::<u16>().map_err(|_| Refusal::Bad("invalid port"))?;
        if port == 0 {
            return Err(Refusal::Bad("port 0 is invalid"));
        }
        Ok(port)
    };
    if let Some(rest) = s.strip_prefix('[') {
        let Some((h, after)) = rest.split_once(']') else {
            return Err(Refusal::Bad("unclosed IPv6 literal"));
        };
        if h.is_empty() {
            return Err(Refusal::Bad("empty host"));
        }
        let port = if after.is_empty() {
            default
        } else if let Some(p) = after.strip_prefix(':') {
            parse_port(p)?
        } else {
            return Err(Refusal::Bad("invalid host:port"));
        };
        return Ok((h.to_string(), port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => {
            if h.is_empty() {
                return Err(Refusal::Bad("empty host"));
            }
            Ok((h.to_string(), parse_port(p)?))
        }
        Some(_) => Err(Refusal::Bad("unbracketed IPv6 host")),
        None => {
            if s.is_empty() {
                return Err(Refusal::Bad("empty host"));
            }
            Ok((s.to_string(), default))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_rejects_malformed_and_edge_cases() {
        assert!(matches!(parse_request(b"GET / HTTP/1.1\r\n"), Err(Refusal::Bad("incomplete request head"))));
        assert!(matches!(
            parse_request(b"GET http://127.0.0.1:0/path HTTP/1.1\r\n\r\n"),
            Err(Refusal::Bad("port 0 is invalid"))
        ));
        let ok_zero = parse_request(b"POST http://127.0.0.1:8080/x HTTP/1.1\r\nContent-Length: 0\r\n\r\n").unwrap();
        assert!(matches!(ok_zero, Request::Forward { body: Body::None, port: 8080, .. }));
        assert!(matches!(
            parse_request(b"POST /x HTTP/1.1\r\nHost: example.com\r\nContent-Length: 999999999999999999999999\r\n\r\n"),
            Err(Refusal::Bad("bad Content-Length"))
        ));
    }

    #[test]
    fn split_host_port_validates_ipv6_empty_and_ports() {
        assert_eq!(split_host_port("[::1]", 443).unwrap(), ("::1".into(), 443));
        assert!(matches!(split_host_port("[]:443", 443), Err(Refusal::Bad("empty host"))));
        assert!(matches!(split_host_port("[::1]junk", 443), Err(Refusal::Bad("invalid host:port"))));
        assert!(matches!(split_host_port(":443", 443), Err(Refusal::Bad("empty host"))));
        assert!(matches!(split_host_port("", 80), Err(Refusal::Bad("empty host"))));
        assert!(matches!(split_host_port("example.com:0", 80), Err(Refusal::Bad("port 0 is invalid"))));
        assert!(matches!(split_host_port("example.com:abc", 80), Err(Refusal::Bad("invalid port"))));
    }

    #[tokio::test]
    async fn read_line_bounded_enforces_limit_and_handles_eof() {
        let mut out = Vec::new();
        let mut empty = BufReader::new(&b""[..]);
        assert_eq!(read_line_bounded(&mut empty, &mut out, 8).await.unwrap(), 0);

        let mut too_long = BufReader::new(&b"0123456789\n"[..]);
        let err = read_line_bounded(&mut too_long, &mut out, 4).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);

        let mut no_nl = BufReader::new(&b"0123456789"[..]);
        let err = read_line_bounded(&mut no_nl, &mut out, 4).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
