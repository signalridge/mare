use super::*;

// Check the reason for a timeout during body reads and a quiet byte stream. Both clocks here are
// set in the past, with no sleeps.
#[tokio::test]
async fn http_watched_reports_the_first_expired_timeout_and_prioritizes_cancel() {
    async fn read_error(total_secs: u64, idle_secs: u64, cancelled: bool) -> String {
        let last = std::time::Instant::now() - Duration::from_secs(30);
        let token = CancellationToken::new();
        if cancelled {
            token.cancel();
        }
        let response = Response {
            status: 200,
            url: "http://example.test/".into(),
            headers: vec![],
            body: AsyncMutex::new(Some(Box::pin(futures_util::stream::pending()))),
            sse: AsyncMutex::new(None),
            close_token: token,
            bad_utf8: Default::default(),
            caps: SseCaps { line: sse::DEFAULT_CAP, event: sse::DEFAULT_CAP },
            idle: Some(Duration::from_secs(idle_secs)),
            deadline: Some(tokio::time::Instant::from_std(last + Duration::from_secs(total_secs))),
            last_activity: std::sync::Arc::new(std::sync::Mutex::new(last)),
        };
        let lua = Lua::new();
        lua.globals().set("response", response).unwrap();
        lua.load("return response:read()").eval_async::<Value>().await.unwrap_err().to_string()
    }

    let total_first = read_error(1, 2, false).await;
    assert!(total_first.contains("transport: request timed out"), "{total_first}");
    let idle_first = read_error(2, 1, false).await;
    assert!(idle_first.contains("transport: stream idle"), "{idle_first}");
    let cancelled = read_error(1, 2, true).await;
    assert!(cancelled.contains("aborted"), "{cancelled}");
}

#[test]
fn retry_after_reads_ms_seconds_and_http_dates() {
    let h = |k: &str, v: &str| vec![(k.to_string(), v.to_string())];
    assert_eq!(retry_after_ms(&h("retry-after-ms", "250")), Some(250));
    assert_eq!(retry_after_ms(&h("Retry-After", "2")), Some(2000));
    assert_eq!(retry_after_ms(&h("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")), Some(0), "past date");
    let future = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(120);
    let v = jiff::fmt::rfc2822::to_string(&future.to_zoned(jiff::tz::TimeZone::UTC)).unwrap();
    let ms = retry_after_ms(&h("retry-after", &v)).unwrap();
    assert!((110_000..=120_000).contains(&ms), "{ms}");
    assert_eq!(retry_after_ms(&h("retry-after", "soon")), None);
}

#[tokio::test]
async fn invalid_utf8_ends_the_stream_instead_of_stalling() {
    let bad = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // a split multi-byte character is fine; an invalid byte is not
    let chunks: Vec<reqwest::Result<Bytes>> = vec![
        Ok(Bytes::from_static(b"data: \xe4\xbd")),
        Ok(Bytes::from_static(b"\xa0\n\n")),
        Ok(Bytes::from_static(b"data: \xff\xfe\n\n")),
    ];
    let mut s = utf8_guarded(Box::pin(futures_util::stream::iter(chunks)), bad.clone());
    assert!(s.next().await.is_some());
    assert!(s.next().await.is_some());
    assert!(!bad.load(std::sync::atomic::Ordering::SeqCst), "split character accepted");
    assert!(s.next().await.is_none(), "stream ends");
    assert!(bad.load(std::sync::atomic::Ordering::SeqCst), "flagged malformed");
}

#[test]
fn strip_userinfo_and_apply_route_redact_credentials() {
    let mut url = url::Url::parse("https://user:s3cr3t@example.com/path?q=1").unwrap();
    strip_userinfo(&mut url);
    assert_eq!(url.as_str(), "https://example.com/path?q=1");

    let err = match apply_route(
        base_builder().unwrap(),
        &Route { proxy: Some("socks5://alice:s3cr3t@proxy.local:1080".into()), connect_timeout_ms: None },
        false,
    ) {
        Ok(_) => panic!("socks proxy must error"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("SOCKS proxies are not supported"), "{err}");
    assert!(!err.contains("s3cr3t"), "proxy password leaked: {err}");
}

#[test]
fn explicit_proxy_normalization_trims_and_fails_closed_on_invalid() {
    let norm = normalize_explicit_proxy(" http://127.0.0.1:8888 ").unwrap();
    assert_eq!(norm, "http://127.0.0.1:8888/");

    let url = url::Url::parse("http://example.com/hello").unwrap();
    let resolved = resolve_proxy_for_url(&url, Some(" http://127.0.0.1:8888 ")).unwrap().unwrap();
    assert_eq!(resolved.host, "127.0.0.1");
    assert_eq!(resolved.port, 8888);
    assert_eq!(resolved.scheme, "http");

    assert!(normalize_explicit_proxy("not a proxy").is_err());
    assert!(resolve_proxy_for_url(&url, Some("not a proxy")).is_err());
    assert!(resolve_proxy_for_url(&url, Some("ftp://127.0.0.1:21")).is_err());
}

#[tokio::test]
async fn cross_origin_redirect_never_forwards_custom_auth_headers() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let target_hits = Arc::new(AtomicUsize::new(0));
    let target_hits_srv = target_hits.clone();
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = target.accept().await {
            target_hits_srv.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
        }
    });

    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = origin.accept().await {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/steal\r\nContent-Length: 0\r\n\r\n"
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.globals().set("URL", format!("http://127.0.0.1:{origin_port}/v1/models")).unwrap();

    let (ok_same, err_same, manual_status): (bool, String, i64) = lua
        .load(
            r#"
            local headers = {
                ["authorization"] = "Bearer secret-token",
                ["chatgpt-account-id"] = "acct-secret",
                ["x-custom-auth"] = "custom-secret",
            }
            local ok, res = pcall(mare.http.request, {
                url = URL,
                headers = headers,
                redirect = "same-origin",
            })
            local m = mare.http.request({
                url = URL,
                headers = headers,
                redirect = "manual",
            })
            return ok, tostring(res), m.status
            "#,
        )
        .eval_async()
        .await
        .unwrap();

    assert!(!ok_same, "same-origin must reject cross-origin redirect");
    assert!(err_same.contains("redirect"), "{err_same}");
    assert_eq!(manual_status, 302);
    assert_eq!(target_hits.load(Ordering::SeqCst), 0, "cross-origin target must never be contacted");
}

#[tokio::test]
async fn cross_origin_307_308_post_does_not_forward_body_or_auth() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let target_hits = Arc::new(AtomicUsize::new(0));
    let target_hits_srv = target_hits.clone();
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = target.accept().await {
            target_hits_srv.fetch_add(1, Ordering::SeqCst);
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
        }
    });

    let origin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut n = 0usize;
        while let Ok((mut s, _)) = origin.accept().await {
            n += 1;
            let code = if n % 2 == 1 { "307 Temporary Redirect" } else { "308 Permanent Redirect" };
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 {code}\r\nLocation: http://127.0.0.1:{target_port}/steal\r\nContent-Length: 0\r\n\r\n"
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.globals().set("URL", format!("http://127.0.0.1:{origin_port}/oauth/token")).unwrap();

    let (ok_307, err_307, ok_308, err_308): (bool, String, bool, String) = lua
        .load(
            r#"
            local function post_same_origin()
                return pcall(mare.http.request, {
                    method = "POST",
                    url = URL,
                    headers = { ["authorization"] = "Bearer secret-token" },
                    form = { grant_type = "refresh_token", refresh_token = "rt-secret" },
                    redirect = "same-origin",
                })
            end
            local ok1, r1 = post_same_origin()
            local ok2, r2 = post_same_origin()
            return ok1, tostring(r1), ok2, tostring(r2)
            "#,
        )
        .eval_async()
        .await
        .unwrap();

    assert!(!ok_307, "307 cross-origin redirect must be rejected");
    assert!(err_307.contains("redirect"), "{err_307}");
    assert!(!ok_308, "308 cross-origin redirect must be rejected");
    assert!(err_308.contains("redirect"), "{err_308}");
    assert_eq!(target_hits.load(Ordering::SeqCst), 0, "cross-origin target must never receive body or auth");
}

#[tokio::test]
async fn same_origin_client_reuses_pooled_tcp_connection() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let conns = Arc::new(AtomicUsize::new(0));
    let conns_srv = conns.clone();
    let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = srv.accept().await {
            conns_srv.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                buf.drain(..pos + 4);
                                if s.write_all(
                                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\nok",
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.globals().set("URL", format!("http://127.0.0.1:{port}/ping")).unwrap();

    let b1: String = lua
        .load(
            r#"
            local r1 = mare.http.request({ url = URL, redirect = "same-origin" })
            return r1:text()
            "#,
        )
        .eval_async()
        .await
        .unwrap();
    // Drop the response userdata and let hyper's background connection task return the socket to the pool.
    lua.gc_collect().unwrap();
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    let b2: String = lua
        .load(
            r#"
            local r2 = mare.http.request({ url = URL, redirect = "same-origin" })
            return r2:text()
            "#,
        )
        .eval_async()
        .await
        .unwrap();

    assert_eq!((b1.as_str(), b2.as_str()), ("ok", "ok"));
    assert_eq!(conns.load(Ordering::SeqCst), 1, "same-origin requests must reuse the pooled TCP connection");
}

#[tokio::test]
async fn offline_guard_allows_loopback_and_blocks_public_hosts_and_ips() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for host in
        ["localhost", "sub.localhost", "localhost.", "127.0.0.1", "127.0.1.1", "::1", "[::1]", "::ffff:127.0.0.1"]
    {
        assert!(is_loopback_host(host), "expected loopback: {host}");
    }
    for host in ["example.com", "1.1.1.1", "93.184.216.34", "8.8.8.8", "192.168.1.1", "0.0.0.0", "::"] {
        assert!(!is_loopback_host(host), "expected non-loopback: {host}");
    }

    // Loopback server that redirects to a public host.
    let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = srv.accept().await {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf).await;
            let req = String::from_utf8_lossy(&buf);
            if req.starts_with("GET /redirect ") {
                let _ = s
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: https://example.com/blocked\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
            } else {
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
            }
        }
    });

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.globals().set("LOOP_URL", format!("http://127.0.0.1:{port}/ok")).unwrap();
    lua.globals().set("REDIR_URL", format!("http://127.0.0.1:{port}/redirect")).unwrap();

    let ok: String = lua.load(r#"return mare.http.request({ url = LOOP_URL }):text()"#).eval_async().await.unwrap();
    assert_eq!(ok, "ok");

    for bad_url in ["https://example.com/", "http://1.1.1.1/"] {
        lua.globals().set("BAD_URL", bad_url).unwrap();
        let err = lua
            .load(r#"return mare.http.request({ url = BAD_URL })"#)
            .eval_async::<Value>()
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("offline test guard"), "expected offline guard error for {bad_url}, got: {err}");
    }

    let redir_err = lua
        .load(r#"return mare.http.request({ url = REDIR_URL })"#)
        .eval_async::<Value>()
        .await
        .unwrap_err()
        .to_string();
    assert!(redir_err.contains("offline test guard"), "expected offline guard on redirect, got: {redir_err}");

    // TestClient + OfflineDnsResolver + redirect_policy block non-loopback domains, public literal IPs, and redirects.
    let client_host_err = test_client().get("https://example.com/").send().await.unwrap_err();
    assert!(client_host_err.contains("offline test guard"), "expected offline guard for host, got: {client_host_err}");
    let client_ip_err = test_client().get("http://1.1.1.1/").send().await.unwrap_err();
    assert!(client_ip_err.contains("offline test guard"), "expected offline guard for IP, got: {client_ip_err}");
    let get_ip_err = test_get("http://1.1.1.1/").await.unwrap_err();
    assert!(get_ip_err.contains("offline test guard"), "expected offline guard for test_get IP, got: {get_ip_err}");
    let client_redir_err = test_client().get(format!("http://127.0.0.1:{port}/redirect")).send().await.unwrap_err();
    assert!(
        client_redir_err.contains("offline test guard"),
        "expected offline guard for test_client redirect, got: {client_redir_err}"
    );

    // Env proxy vars pointing at non-loopback hosts or literal IPs are also blocked.
    for (k, v) in [
        ("HTTP_PROXY", "http://1.1.1.1:8080"),
        ("HTTP_PROXY", "1.1.1.1:8080"),
        ("https_proxy", "http://proxy.example.com:3128"),
    ] {
        let env = |name: &str| (name == k).then(|| v.to_string());
        let err = check_offline_proxy_env_with(&env).unwrap_err();
        assert!(err.contains("offline test guard"), "expected offline guard for {k}={v}, got: {err}");
    }
    let ok_env = |name: &str| (name == "HTTP_PROXY").then(|| "http://127.0.0.1:8080".to_string());
    check_offline_proxy_env_with(&ok_env).unwrap();

    // Unused non-loopback HTTP_PROXY with NO_PROXY=127.0.0.1 must not block loopback requests or client building.
    let noproxy_env = |name: &str| match name {
        "HTTP_PROXY" => Some("http://proxy.example:8080".to_string()),
        "NO_PROXY" => Some("127.0.0.1".to_string()),
        _ => None,
    };
    let _ =
        base_builder_with(&noproxy_env).expect("base_builder_with must succeed with HTTP_PROXY + NO_PROXY=127.0.0.1");
    check_offline_proxy_for_url_with("http://127.0.0.1:8080/ok", None, &noproxy_env).unwrap();
    let non_loopback_err = check_offline_proxy_for_url_with("http://example.com/ok", None, &noproxy_env).unwrap_err();
    assert!(
        non_loopback_err.contains("offline test guard"),
        "expected offline guard for non-loopback: {non_loopback_err}"
    );
    let no_noproxy_env = |name: &str| (name == "HTTP_PROXY").then(|| "http://proxy.example:8080".to_string());
    let missing_noproxy_err =
        check_offline_proxy_for_url_with("http://127.0.0.1:8080/ok", None, &no_noproxy_env).unwrap_err();
    assert!(
        missing_noproxy_err.contains("offline test guard"),
        "expected offline guard when NO_PROXY is unset: {missing_noproxy_err}"
    );
    check_offline_proxy_for_url_with("https://127.0.0.1:8080/ok", None, &no_noproxy_env)
        .expect("HTTP_PROXY alone must not block https:// loopback");
}

fn contains_word_token(line: &str, token: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = line[start..].find(token) {
        let abs = start + pos;
        let prev_ok =
            abs == 0 || !line[..abs].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if prev_ok {
            return true;
        }
        start = abs + token.len();
    }
    false
}

fn strip_rust_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        // Line comment: skip to '\n' (keep '\n' to preserve line numbers).
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'/') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        // Block comment: preserve newlines inside so line numbers match.
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    out.push(if bytes[i] == b'\n' { '\n' } else { ' ' });
                    i += 1;
                }
            }
            continue;
        }
        // Raw string literal: r"...", r#"..."#, br#"..."#.
        let raw_prefix = if bytes[i] == b'r' {
            Some(1)
        } else if bytes[i] == b'b' && bytes.get(i + 1) == Some(&b'r') {
            Some(2)
        } else {
            None
        };
        if let Some(p) = raw_prefix
            && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_'))
        {
            let mut j = i + p;
            let mut hashes = 0usize;
            while bytes.get(j) == Some(&b'#') {
                hashes += 1;
                j += 1;
            }
            if bytes.get(j) == Some(&b'"') {
                out.push_str(&src[i..=j]);
                i = j + 1;
                while i < bytes.len() {
                    let ch = src[i..].chars().next().unwrap();
                    out.push(ch);
                    i += ch.len_utf8();
                    if ch == '"' && bytes.get(i..i + hashes).is_some_and(|s| s.iter().all(|&b| b == b'#')) {
                        for _ in 0..hashes {
                            out.push('#');
                        }
                        i += hashes;
                        break;
                    }
                }
                continue;
            }
        }
        // Normal / byte string literal: "..." or b"...".
        if bytes[i] == b'"' {
            out.push('"');
            i += 1;
            while i < bytes.len() {
                let ch = src[i..].chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
                if ch == '\\' && i < bytes.len() {
                    let esc = src[i..].chars().next().unwrap();
                    out.push(esc);
                    i += esc.len_utf8();
                } else if ch == '"' {
                    break;
                }
            }
            continue;
        }
        let ch = src[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn check_use_stmt(path: &str, start_line: usize, stmt: &str, violations: &mut Vec<String>) {
    if (stmt.contains(&format!("use {}::", "reqwest")) || stmt.contains(&format!("use {} as ", "reqwest")))
        && ["Client", "ClientBuilder", "blocking", "get", " as "].iter().any(|k| stmt.contains(k))
    {
        violations.push(format!(
            "{path}:{start_line}: direct reqwest client import bypasses mare_binding::http: {}",
            stmt.trim()
        ));
    }
    if contains_word_token(stmt, &format!("use {}", "ureq")) {
        violations
            .push(format!("{path}:{start_line}: direct ureq import bypasses mare_binding::http: {}", stmt.trim()));
    }
}

fn scan_http_bypasses(path: &str, src: &str) -> Vec<String> {
    let direct: &[(&str, &str)] = &[
        ("reqwest::Client", "::new("),
        ("reqwest::Client", "::builder("),
        ("reqwest::Client", "::default("),
        ("reqwest::ClientBuilder", "::new("),
        ("reqwest::ClientBuilder", "::default("),
        ("reqwest::", "get("),
        ("reqwest::", "blocking::"),
        ("ureq", "::"),
    ];
    let bare_ctors: &[(&str, &str)] = &[
        ("Client", "::new("),
        ("Client", "::builder("),
        ("Client", "::default("),
        ("ClientBuilder", "::new("),
        ("ClientBuilder", "::default("),
    ];
    let stripped = strip_rust_comments(src);
    let mut violations = Vec::new();
    let mut in_use: Option<(usize, String)> = None;
    for (idx, code) in stripped.lines().enumerate() {
        for (a, b) in direct {
            let pat = format!("{a}{b}");
            if code.contains(&pat) {
                violations.push(format!(
                    "{path}:{}: direct `{pat}` bypasses mare_binding::http: {}",
                    idx + 1,
                    code.trim()
                ));
            }
        }
        for (a, b) in bare_ctors {
            let pat = format!("{a}{b}");
            if contains_word_token(code, &pat) {
                violations.push(format!(
                    "{path}:{}: bare `{pat}` bypasses mare_binding::http: {}",
                    idx + 1,
                    code.trim()
                ));
            }
        }
        let trimmed = code.trim();
        if let Some((start_line, ref mut buf)) = in_use {
            if !trimmed.is_empty() {
                buf.push(' ');
                buf.push_str(trimmed);
            }
            if trimmed.contains(';') {
                let (_, stmt) = in_use.take().unwrap();
                check_use_stmt(path, start_line, &stmt, &mut violations);
            }
        } else if contains_word_token(trimmed, "use ") {
            if trimmed.contains(';') {
                check_use_stmt(path, idx + 1, trimmed, &mut violations);
            } else {
                in_use = Some((idx + 1, trimmed.to_string()));
            }
        }
    }
    if let Some((start_line, stmt)) = in_use {
        check_use_stmt(path, start_line, &stmt, &mut violations);
    }
    violations
}

/// Static scan: no Rust file outside `http.rs`
/// may construct a reqwest/ureq client directly, bypassing the unified HTTP entry point and its
/// offline test guard.
#[test]
fn no_test_bypasses_http_entry() {
    // Counter-examples: imported/aliased constructors and ureq must be caught; AuthClient::new is allowed.
    let imported = format!("use {r}::{c};\nlet x = {c}::new();", r = "reqwest", c = "Client");
    assert!(!scan_http_bypasses("mem.rs", &imported).is_empty(), "imported Client::new must be caught");
    let builder = format!("use {r}::{c};\nlet x = {c}::builder();", r = "reqwest", c = "Client");
    assert!(!scan_http_bypasses("mem.rs", &builder).is_empty(), "imported Client::builder must be caught");
    let aliased = format!("use {r} as r;\nlet x = r::{c}::new();", r = "reqwest", c = "Client");
    assert!(!scan_http_bypasses("mem.rs", &aliased).is_empty(), "aliased reqwest must be caught");
    let blocking = format!("use {r}::blocking::{c};\nlet x = {c}::new();", r = "reqwest", c = "Client");
    assert!(!scan_http_bypasses("mem.rs", &blocking).is_empty(), "blocking Client must be caught");
    let ureq_use = format!("use {u};\nlet _ = {u}::get(\"http://example.com\");", u = "ureq");
    assert!(!scan_http_bypasses("mem.rs", &ureq_use).is_empty(), "ureq must be caught");
    let client_default = format!("let x = {r}::{c}::default();", r = "reqwest", c = "Client");
    assert!(!scan_http_bypasses("mem.rs", &client_default).is_empty(), "reqwest::Client::default must be caught");
    let bare_client_default = format!("let x = {c}::default();", c = "Client");
    assert!(!scan_http_bypasses("mem.rs", &bare_client_default).is_empty(), "bare Client::default must be caught");
    let builder_default = format!("let x = {r}::{cb}::default();", r = "reqwest", cb = "ClientBuilder");
    assert!(
        !scan_http_bypasses("mem.rs", &builder_default).is_empty(),
        "reqwest::ClientBuilder::default must be caught"
    );
    let bare_builder_default = format!("let x = {cb}::default();", cb = "ClientBuilder");
    assert!(
        !scan_http_bypasses("mem.rs", &bare_builder_default).is_empty(),
        "bare ClientBuilder::default must be caught"
    );
    let multiline_alias =
        format!("use {r}::{{\n    {c} as HttpClient,\n}};\nlet x = HttpClient::new();", r = "reqwest", c = "Client");
    assert!(!scan_http_bypasses("mem.rs", &multiline_alias).is_empty(), "multi-line use alias must be caught");
    let after_url_literal =
        format!("let _ = \"http://127.0.0.1\"; let c = {r}::{c}::new();", r = "reqwest", c = "Client");
    assert!(
        !scan_http_bypasses("mem.rs", &after_url_literal).is_empty(),
        "constructor after http:// string literal must be caught"
    );
    assert!(
        scan_http_bypasses("mem.rs", "let t = AuthClient::new(http, m);\nlet c = crate::http::test_client();")
            .is_empty(),
        "AuthClient::new and test_client must be allowed"
    );

    let crates_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let allowed = crates_dir.join("mare-binding").join("src").join("http.rs");

    let mut stack = vec![crates_dir.to_path_buf()];
    let mut violations = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                let name = entry.file_name();
                if name != "target" && !name.to_string_lossy().starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") || path == allowed {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            violations.extend(scan_http_bypasses(&path.display().to_string(), &src));
        }
    }
    assert!(violations.is_empty(), "HTTP entry bypasses found:\n{}", violations.join("\n"));
}

#[tokio::test]
async fn stream_cancel_token_and_overall_deadline_both_enforce_termination() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    tokio::task::LocalSet::new()
        .run_until(async {
            let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = srv.local_addr().unwrap().port();
            tokio::spawn(async move {
                while let Ok((mut s, _)) = srv.accept().await {
                    tokio::spawn(async move {
                        let mut buf = [0u8; 1024];
                        let _ = s.read(&mut buf).await;
                        if s.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n").await.is_err() {
                            return;
                        }
                        for _ in 0..50 {
                            if s.write_all(b"1\r\na\r\n").await.is_err() {
                                break;
                            }
                            // sleep-ok: paces a slow chunked body so the overall deadline, not idle, ends the read
                            tokio::time::sleep(Duration::from_millis(30)).await;
                        }
                        let _ = s.write_all(b"0\r\n\r\n").await;
                    });
                }
            });

            let lua = Lua::new();
            let mare = lua.create_table().unwrap();
            crate::task::install(&lua, &mare).unwrap();
            install(&lua, &mare).unwrap();
            lua.globals().set("mare", mare).unwrap();
            lua.globals().set("URL", format!("http://127.0.0.1:{port}/slow")).unwrap();

            // 1. Cancel token terminates an active stream immediately even with a long overall deadline.
            let t0 = std::time::Instant::now();
            let (first_chunk, cancel_ok, cancel_err): (String, bool, String) = lua
                .load(
                    r#"
                    local tok = mare.task.token()
                    local r = mare.http.request({ url = URL, cancel = tok, timeout_ms = 5000, idle_timeout_ms = 5000 })
                    local first = r:read()
                    mare.task.spawn(function()
                        mare.task.sleep(40)
                        tok:cancel()
                    end)
                    local ok, err = pcall(function() return r:text() end)
                    return first, ok, tostring(err)
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert_eq!(first_chunk, "a");
            assert!(!cancel_ok && cancel_err.contains("aborted"), "expected aborted error, got: {cancel_err}");
            assert!(t0.elapsed() < Duration::from_secs(2), "cancel must abort promptly, took {:?}", t0.elapsed());

            // 2. Overall deadline terminates a slowly-streaming response even when chunks keep arriving under idle_timeout_ms.
            let t1 = std::time::Instant::now();
            let (dl_ok, dl_err): (bool, String) = lua
                .load(
                    r#"
                    local tok = mare.task.token()
                    local r = mare.http.request({ url = URL, cancel = tok, timeout_ms = 120, idle_timeout_ms = 500 })
                    local ok, err = pcall(function() return r:text() end)
                    return ok, tostring(err)
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert!(!dl_ok && dl_err.contains("request timed out"), "expected request timed out, got: {dl_err}");
            assert!(t1.elapsed() < Duration::from_secs(2), "deadline must fire promptly, took {:?}", t1.elapsed());
        })
        .await;
}

#[tokio::test]
async fn redirect_preserves_bytes_body_on_307_and_drops_on_302() {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let recorded: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_srv = recorded.clone();
    let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        async fn read_http_request(s: &mut tokio::net::TcpStream) -> String {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            let mut header_end = None;
            let mut content_len = 0usize;
            loop {
                if let Some(end) = header_end
                    && buf.len() >= end + content_len
                {
                    break;
                }
                let n = s.read(&mut tmp).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if header_end.is_none()
                    && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                {
                    let end = pos + 4;
                    header_end = Some(end);
                    let headers = String::from_utf8_lossy(&buf[..end]);
                    for line in headers.lines() {
                        if let Some((name, val)) = line.split_once(':')
                            && name.trim().eq_ignore_ascii_case("content-length")
                        {
                            content_len = val.trim().parse::<usize>().unwrap_or(0);
                        }
                    }
                }
            }
            String::from_utf8_lossy(&buf).to_string()
        }

        while let Ok((mut s, _)) = srv.accept().await {
            let req = read_http_request(&mut s).await;
            recorded_srv.lock().unwrap().push(req.clone());
            let first_line = req.lines().next().unwrap_or("");
            let resp = if first_line.starts_with("POST /r307 ") {
                format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nLocation: http://127.0.0.1:{port}/final\r\nContent-Length: 0\r\n\r\n"
                )
            } else if first_line.starts_with("POST /r302 ") {
                format!("HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/final\r\nContent-Length: 0\r\n\r\n")
            } else {
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", first_line.len(), first_line)
            };
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.globals().set("BASE", format!("http://127.0.0.1:{port}")).unwrap();

    let (t307, t302): (String, String) = lua
        .load(
            r#"
            local r1 = mare.http.request({
                method = "POST",
                url = BASE .. "/r307",
                json = { hello = "world" },
            })
            local r2 = mare.http.request({
                method = "POST",
                url = BASE .. "/r302",
                body = "secret-post-body",
            })
            return r1:text(), r2:text()
            "#,
        )
        .eval_async()
        .await
        .unwrap();

    assert!(t307.starts_with("POST /final"), "expected POST after 307, got {t307}");
    assert!(t302.starts_with("GET /final"), "expected GET after 302, got {t302}");
    let log = recorded.lock().unwrap();
    let final_post = log.iter().find(|r| r.starts_with("POST /final ")).unwrap();
    assert!(final_post.contains(r#"{"hello":"world"}"#), "307 redirect must preserve JSON body: {final_post}");
    let final_get = log.iter().find(|r| r.starts_with("GET /final ")).unwrap();
    assert!(!final_get.contains("secret-post-body"), "302 redirect to GET must drop body: {final_get}");
}

#[tokio::test]
async fn same_host_cross_port_redirect_strips_auth_and_cookies_under_authorized_send() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let target_reqs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let target_reqs_c = target_reqs.clone();
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_port = target.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut s, _)) = target.accept().await {
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            target_reqs_c.lock().unwrap().push(req);
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
        }
    });

    let srv = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let src_port = srv.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut s, _)) = srv.accept().await {
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf).await;
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{target_port}/steal\r\nContent-Length: 0\r\n\r\n"
            );
            let _ = s.write_all(resp.as_bytes()).await;
        }
    });

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.globals().set("SRC_URL", format!("http://user:s3cr3t@127.0.0.1:{src_port}/start")).unwrap();

    let (body, final_url): (String, String) = lua
        .load(
            r#"
            return mare.exec.run({
                sandbox = { level = "workspace-write", network = { default = "allow", allow_local = true } }
            }, function()
                local r = mare.http.request({
                    url = SRC_URL,
                    headers = {
                        Authorization = "Bearer super-secret-token",
                        Cookie = "session=topsecret",
                    },
                    redirect = "follow",
                })
                return r:text(), r.url
            end)
            "#,
        )
        .eval_async()
        .await
        .unwrap();

    assert_eq!(body, "ok");
    assert!(!final_url.contains("s3cr3t"), "userinfo leaked in final_url: {final_url}");
    let captured = target_reqs.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let lower = captured[0].to_ascii_lowercase();
    assert!(!lower.contains("super-secret-token"), "Authorization leaked across ports: {lower}");
    assert!(!lower.contains("session=topsecret"), "Cookie leaked across ports: {lower}");
}

#[tokio::test]
async fn listen_once_enforces_loopback_host_header_port_zero_and_concurrent_accept() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async move {
            let lua = Lua::new();
            let mare = lua.create_table().unwrap();
            install(&lua, &mare).unwrap();
            lua.globals().set("mare", mare).unwrap();

            // Non-loopback bind host rejected immediately.
            let (res, err): (Option<mlua::Table>, Option<String>) = lua
                .load(r#"return mare.http.listen_once({ port = 0, host = "0.0.0.0", timeout_ms = 500 })"#)
                .eval_async()
                .await
                .unwrap();
            assert!(res.is_none());
            assert!(err.as_deref().unwrap_or("").contains("non-loopback"), "{err:?}");

            // Spawn a listener on port = 0, verify bound port > 0, stalled connection & bad Host: evil.test
            // don't block the valid callback.
            let (port_tx, port_rx) = tokio::sync::oneshot::channel::<u16>();
            let port_tx = std::sync::Mutex::new(Some(port_tx));
            let on_bound = lua
                .create_function(move |_, p: u16| {
                    if let Some(tx) = port_tx.lock().unwrap().take() {
                        let _ = tx.send(p);
                    }
                    Ok(())
                })
                .unwrap();
            lua.globals().set("on_bound_cb", on_bound).unwrap();

            let lua_srv = lua.clone();
            let server_fut = tokio::task::spawn_local(async move {
                lua_srv
                    .load(
                        r#"
                    return mare.http.listen_once({
                        port = 0,
                        path = "/cb",
                        timeout_ms = 5000,
                        on_bound = on_bound_cb,
                    })
                    "#,
                    )
                    .eval_async::<mlua::Value>()
                    .await
            });

            let bound_port = port_rx.await.unwrap();
            assert!(bound_port > 0, "ephemeral port 0 must return actual bound port");

            // 1. Stalled connection (writes partial headers and stays open).
            let mut stalled = tokio::net::TcpStream::connect(("127.0.0.1", bound_port)).await.unwrap();
            stalled.write_all(b"GET /cb HTTP/1.1\r\n").await.unwrap();

            // 2. DNS-rebound Host header must be rejected with 403.
            let mut bad_host = tokio::net::TcpStream::connect(("127.0.0.1", bound_port)).await.unwrap();
            bad_host.write_all(b"GET /cb?code=bad HTTP/1.1\r\nHost: evil.attacker.example\r\n\r\n").await.unwrap();
            let mut bad_resp = String::new();
            bad_host.read_to_string(&mut bad_resp).await.unwrap();
            assert!(bad_resp.starts_with("HTTP/1.1 403"), "{bad_resp}");

            // 3. Valid callback completes concurrently despite stalled connection!
            let mut good = tokio::net::TcpStream::connect(("127.0.0.1", bound_port)).await.unwrap();
            good.write_all(format!("GET /cb?code=ok123 HTTP/1.1\r\nHost: 127.0.0.1:{bound_port}\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut good_resp = String::new();
            good.read_to_string(&mut good_resp).await.unwrap();
            assert!(good_resp.starts_with("HTTP/1.1 200"), "{good_resp}");

            let out = server_fut.await.unwrap().unwrap();
            let tbl = out.as_table().unwrap();
            assert_eq!(tbl.get::<u16>("port").unwrap(), bound_port);
            assert_eq!(tbl.get::<String>("path").unwrap(), "/cb");
            assert_eq!(tbl.get::<mlua::Table>("query").unwrap().get::<String>("code").unwrap(), "ok123");
        })
        .await;
}
