use super::*;

pub(super) fn normalize_explicit_proxy(raw: &str) -> mlua::Result<String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("socks") {
        let safe = mare_shared::redact::redact_str(trimmed);
        return Err(rt_err(format!("proxy {safe}: SOCKS proxies are not supported — use an HTTP or HTTPS proxy")));
    }
    let parsed = url::Url::parse(trimmed).map_err(|e| {
        let safe_p = mare_shared::redact::redact_str(trimmed);
        let safe_e = mare_shared::redact::redact_str(&e.to_string());
        rt_err(format!("invalid proxy {safe_p}: {safe_e}"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        let safe_p = mare_shared::redact::redact_str(trimmed);
        return Err(rt_err(format!("invalid proxy {safe_p}: must be an http:// or https:// URL with a host")));
    }
    let canonical = parsed.as_str().to_string();
    if canonical.parse::<http::Uri>().is_err() {
        let safe_p = mare_shared::redact::redact_str(trimmed);
        return Err(rt_err(format!("invalid proxy {safe_p}: cannot parse as proxy URI")));
    }
    Ok(canonical)
}

#[derive(Clone)]
pub(super) struct ResolvedProxy {
    url: String,
    pub(super) scheme: String,
    pub(super) host: String,
    pub(super) port: u16,
    basic_auth: Option<String>,
}

impl ResolvedProxy {
    fn connect_addr(&self) -> String {
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        if host.contains(':') { format!("[{host}]:{}", self.port) } else { format!("{host}:{}", self.port) }
    }

    pub(super) fn from_intercept(intercept: hyper_util::client::proxy::matcher::Intercept) -> Result<Self, String> {
        let uri = intercept.uri();
        let scheme = uri.scheme_str().ok_or_else(|| "proxy URI missing scheme".to_string())?.to_ascii_lowercase();
        if !matches!(scheme.as_str(), "http" | "https") {
            return Err(format!("unsupported proxy scheme `{scheme}`"));
        }
        let host = uri.host().ok_or_else(|| "proxy URI missing host".to_string())?.to_string();
        let port = uri.port_u16().unwrap_or(if scheme == "https" { 443 } else { 80 });
        let basic_auth = intercept.basic_auth().and_then(|v| v.to_str().ok()).map(str::to_string);
        Ok(Self { url: uri.to_string(), scheme, host, port, basic_auth })
    }
}

fn build_reqwest_proxy(rp: &ResolvedProxy) -> mlua::Result<reqwest::Proxy> {
    let mut proxy = reqwest::Proxy::all(rp.url.as_str())
        .map_err(|e| rt_err(format!("invalid proxy: {}", mare_shared::redact::redact_str(&e.to_string()))))?
        .no_proxy(None);
    if let Some(auth) = &rp.basic_auth {
        let val =
            reqwest::header::HeaderValue::from_str(auth).map_err(|_| rt_err("invalid proxy authorization header"))?;
        proxy = proxy.custom_http_auth(val);
    }
    Ok(proxy)
}

pub(super) struct ShimGuard(CancellationToken);

impl Drop for ShimGuard {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn start_pinned_proxy_shim(
    upstream: ResolvedProxy,
    target_host: String,
    target_port: u16,
    addrs: Vec<std::net::SocketAddr>,
    cancel: Option<CancellationToken>,
) -> std::io::Result<(u16, ShimGuard)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let stop = CancellationToken::new();
    let stop_child = stop.clone();
    tokio::spawn(async move {
        loop {
            let sock = tokio::select! {
                res = listener.accept() => match res {
                    Ok((s, _)) => s,
                    Err(_) => break,
                },
                _ = stop_child.cancelled() => break,
                _ = async {
                    match &cancel {
                        Some(t) => t.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    stop_child.cancel();
                    break;
                }
            };
            let upstream = upstream.clone();
            let target_host = target_host.clone();
            let addrs = addrs.clone();
            let stop_conn = stop_child.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = stop_conn.cancelled() => {}
                    _ = handle_pinned_proxy_conn(sock, &upstream, &target_host, target_port, &addrs) => {}
                }
            });
        }
    });
    Ok((port, ShimGuard(stop)))
}

async fn read_http_head(stream: &mut tokio::net::TcpStream) -> std::io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "unexpected EOF reading HTTP head"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let end = pos + 4;
            let leftover = buf.split_off(end);
            return Ok((buf, leftover));
        }
        if buf.len() > 16 * 1024 {
            return Err(std::io::Error::other("HTTP head too large"));
        }
    }
}

async fn handle_pinned_proxy_conn(
    mut client: tokio::net::TcpStream,
    upstream: &ResolvedProxy,
    target_host: &str,
    target_port: u16,
    addrs: &[std::net::SocketAddr],
) -> std::io::Result<()> {
    let (head_bytes, leftover) = read_http_head(&mut client).await?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(&head_bytes) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => {
            let _ =
                client.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            return Ok(());
        }
    }
    let method = req.method.unwrap_or("");
    let path = req.path.unwrap_or("");
    let expected_host = crate::netauth::normalize_host(target_host);

    if method.eq_ignore_ascii_case("CONNECT") {
        let (req_host, req_port) = if let Some(rest) = path.strip_prefix('[') {
            match rest.split_once("]:") {
                Some((h, p)) => (h, p.parse::<u16>().ok()),
                None => (path, None),
            }
        } else if let Some((h, p)) = path.rsplit_once(':') {
            (h, p.parse::<u16>().ok())
        } else {
            (path, None)
        };
        if crate::netauth::normalize_host(req_host) != expected_host || req_port != Some(target_port) {
            let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            return Ok(());
        }
        let mut up = match crate::netproxy::connect_via_http_proxy_pinned(
            &upstream.connect_addr(),
            upstream.basic_auth.as_deref(),
            addrs,
        )
        .await
        {
            Ok(s) => s,
            Err(_) => {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                return Ok(());
            }
        };
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        if !leftover.is_empty() {
            up.write_all(&leftover).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
        return Ok(());
    }

    let Ok(u) = url::Url::parse(path) else {
        let _ = client.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return Ok(());
    };
    let req_host = u.host_str().unwrap_or_default();
    if crate::netauth::normalize_host(req_host) != expected_host || u.port_or_known_default() != Some(target_port) {
        let _ = client.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        return Ok(());
    }
    let origin_path = match u.query() {
        Some(q) => format!("{}?{}", u.path(), q),
        None => u.path().to_string(),
    };
    let mut up = match crate::netproxy::connect_via_http_proxy_pinned(
        &upstream.connect_addr(),
        upstream.basic_auth.as_deref(),
        addrs,
    )
    .await
    {
        Ok(s) => s,
        Err(_) => {
            let _ =
                client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            return Ok(());
        }
    };
    let mut out = format!("{method} {origin_path} HTTP/1.1\r\n");
    let mut has_conn = false;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("proxy-connection") || h.name.eq_ignore_ascii_case("proxy-authorization") {
            continue;
        }
        if h.name.eq_ignore_ascii_case("connection") {
            has_conn = true;
            out.push_str("Connection: close\r\n");
            continue;
        }
        let val = String::from_utf8_lossy(h.value);
        out.push_str(h.name);
        out.push_str(": ");
        out.push_str(&val);
        out.push_str("\r\n");
    }
    if !has_conn {
        out.push_str("Connection: close\r\n");
    }
    out.push_str("\r\n");
    up.write_all(out.as_bytes()).await?;
    if !leftover.is_empty() {
        up.write_all(&leftover).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
    Ok(())
}

/// A tool call with a restricted network: every hop — the first request and
/// each redirect — goes through the shared authorization layer ([`crate::netauth`]): host rules
/// and approval, DNS, private/loopback check on every resolved address; the connection is then
/// pinned to exactly the addresses that passed (`resolve_to_addrs` for direct connections, or
/// pinned-IP `CONNECT` through an HTTP proxy when `allow_local = false`).
pub(super) async fn send_authorized(
    lua: &Lua,
    a: &crate::exec_ctx::Active,
    np: &crate::netauth::NetPolicy,
    p: &Parts,
    url: &str,
    tok: &Option<CancellationToken>,
    deadline: Option<tokio::time::Instant>,
) -> mlua::Result<(reqwest::Response, Option<ShimGuard>)> {
    let mut url = url::Url::parse(url)
        .map_err(|e| rt_err(format!("invalid url {}: {e}", mare_shared::redact::redact_str(url))))?;
    let mut method = p.method.clone();
    let mut with_body = true;
    let first_origin = url.origin();
    let approver = crate::netauth::approver(lua);
    for hop in 0..=p.max_redirects {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(rt_err(format!(
                "sandbox: unsupported URL scheme `{}` under a restricted network",
                url.scheme()
            )));
        }
        let host = url.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_string();
        let port = url.port_or_known_default().unwrap_or(80);
        let resolved_proxy = resolve_proxy_for_url(&url, p.route.proxy.as_deref()).map_err(rt_err)?;
        if let Some(rp) = &resolved_proxy {
            check_offline_proxy(&rp.url).map_err(rt_err)?;
        }
        let route = match &resolved_proxy {
            None => crate::netauth::ProxyRoute::Direct,
            Some(rp) if crate::netauth::is_local_proxy_host(&rp.host) => crate::netauth::ProxyRoute::LocalProxy,
            Some(_) => crate::netauth::ProxyRoute::RemoteProxy,
        };
        let fake_ip_mode = match crate::netauth::fake_ip_mode_from_lua(lua) {
            crate::netauth::FakeIpMode::Off => crate::netauth::FakeIpMode::Off,
            crate::netauth::FakeIpMode::Auto => np.fake_ip,
        };
        let auth_fut = np.authorize_target(&host, port, approver.as_ref(), route, fake_ip_mode);
        let auth_res = match deadline {
            Some(dl) => {
                let remaining = dl
                    .checked_duration_since(tokio::time::Instant::now())
                    .ok_or_else(|| rt_err(format!("{TRANSPORT}request timed out")))?;
                match tokio::time::timeout(remaining, cancellable(tok, auth_fut)).await {
                    Ok(res) => res?,
                    Err(_) => return Err(rt_err(format!("{TRANSPORT}request timed out"))),
                }
            }
            None => cancellable(tok, auth_fut).await?,
        };
        let target = match auth_res {
            Ok(t) => t,
            Err(reason) => return Err(crate::exec_ctx::deny_network(lua, a, &host, &reason)),
        };
        if let crate::netauth::AuthTarget::Pinned(addrs) = &target
            && offline_guard_active()
            && addrs.iter().any(|sa| !sa.ip().is_loopback())
        {
            return Err(rt_err(format!(
                "offline test guard: {host} resolved to a non-loopback address; tests stay off the network"
            )));
        }
        let mut cb = base_builder().map_err(rt_err)?.no_proxy().redirect(reqwest::redirect::Policy::none());
        if let Some(ms) = p.route.connect_timeout_ms {
            cb = cb.connect_timeout(Duration::from_millis(ms));
        }
        if p.fresh {
            cb = cb.pool_max_idle_per_host(0);
        }
        let mut shim_guard = None;
        match (&resolved_proxy, &target) {
            (None, crate::netauth::AuthTarget::Pinned(addrs)) => {
                cb = cb.resolve_to_addrs(&host, addrs);
            }
            (None, crate::netauth::AuthTarget::ViaLocalProxyByName) => {
                return Err(crate::exec_ctx::deny_network(lua, a, &host, "invalid direct proxy route"));
            }
            (Some(rp), crate::netauth::AuthTarget::ViaLocalProxyByName) => {
                check_offline_url(url.as_str()).map_err(rt_err)?;
                cb = cb.proxy(build_reqwest_proxy(rp)?);
            }
            (Some(rp), crate::netauth::AuthTarget::Pinned(addrs)) if !np.allows_local() => {
                // Explicit fail-closed limitation: pinned-IP CONNECT shim only speaks plain HTTP to
                // the upstream proxy (`https://` upstream proxies cannot be pinned when
                // `allow_local = false`).
                if rp.scheme != "http" {
                    return Err(crate::exec_ctx::deny_network(
                        lua,
                        a,
                        &host,
                        &format!(
                            "proxy scheme `{}` cannot be pinned when allow_local = false (use an http:// proxy)",
                            rp.scheme
                        ),
                    ));
                }
                let (shim_port, guard) =
                    start_pinned_proxy_shim(rp.clone(), host.clone(), port, addrs.clone(), tok.clone())
                        .await
                        .map_err(rt_err)?;
                shim_guard = Some(guard);
                let proxy = reqwest::Proxy::all(format!("http://127.0.0.1:{shim_port}"))
                    .map_err(|e| rt_err(e.to_string()))?
                    .no_proxy(None);
                cb = cb.proxy(proxy).pool_max_idle_per_host(0);
            }
            (Some(rp), crate::netauth::AuthTarget::Pinned(_)) => {
                cb = cb.proxy(build_reqwest_proxy(rp)?);
            }
        }
        let client = build_client(cb).map_err(rt_err)?;
        // credentials never follow a cross-origin redirect
        let mut rb = builder(&client, p, url.as_str(), method.clone(), with_body, url.origin() != first_origin);
        if let Some(deadline) = deadline {
            let remaining = deadline
                .checked_duration_since(tokio::time::Instant::now())
                .ok_or_else(|| rt_err("transport: request timed out during redirects"))?;
            rb = rb.timeout(remaining);
        }
        crate::exec_gate::check_unpoisoned(lua)?;
        let resp = send(rb, p, tok).await?;
        let status = resp.status();
        if p.redirect == Redirect::Manual || !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok((resp, shim_guard));
        }
        let Some(loc) = resp.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()) else {
            return Ok((resp, shim_guard));
        };
        let next = url
            .join(loc)
            .map_err(|e| rt_err(format!("bad redirect location {}: {e}", mare_shared::redact::redact_str(loc))))?;
        url = next;
        if url.origin() != first_origin {
            strip_userinfo(&mut url);
        }
        if hop == p.max_redirects {
            return Err(rt_err(format!("too many redirects (> {})", p.max_redirects)));
        }
        if p.redirect == Redirect::SameOrigin && url.origin() != first_origin {
            return Err(rt_err("cross-origin redirect blocked"));
        }
        // 303 (and 301/302 for non-GET, as browsers do) switch to GET without a body
        if status == reqwest::StatusCode::SEE_OTHER
            || (matches!(status.as_u16(), 301 | 302)
                && method != reqwest::Method::GET
                && method != reqwest::Method::HEAD)
        {
            method = reqwest::Method::GET;
            with_body = false;
        }
    }
    unreachable!("redirect loop returns at its hop limit")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_explicit_proxy_rejects_socks_and_invalid_schemes_and_build_proxy_handles_bad_auth() {
        let socks_err = normalize_explicit_proxy("socks5://127.0.0.1:1080").unwrap_err().to_string();
        assert!(socks_err.contains("SOCKS proxies are not supported"), "{socks_err}");

        let bad_scheme = normalize_explicit_proxy("ftp://proxy.example:21").unwrap_err().to_string();
        assert!(bad_scheme.contains("must be an http:// or https://"), "{bad_scheme}");

        let ok = normalize_explicit_proxy("http://127.0.0.1:8080").unwrap();
        assert_eq!(ok, "http://127.0.0.1:8080/");

        let rp_v6 = ResolvedProxy {
            url: "http://[::1]:8080".into(),
            scheme: "http".into(),
            host: "[::1]".into(),
            port: 8080,
            basic_auth: Some("bad\nheader".into()),
        };
        assert_eq!(rp_v6.connect_addr(), "[::1]:8080");
        assert!(build_reqwest_proxy(&rp_v6).is_err());

        let rp_bad = ResolvedProxy {
            url: "://not-a-valid-proxy-url".into(),
            scheme: "http".into(),
            host: "127.0.0.1".into(),
            port: 8080,
            basic_auth: None,
        };
        assert!(build_reqwest_proxy(&rp_bad).is_err());
    }
}
