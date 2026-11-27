use super::*;

#[tokio::test]
async fn proxy_guarantees_cover_the_connect_target_not_names_inside_the_tunnel() {
    // guaranteed — the CONNECT target is in the rules, its resolved addresses
    // aren't local/private (unless allow_local), and the connection goes to the checked address.
    // Not guaranteed — the application-level name inside a pass-through tunnel (no SNI inspection;
    // Codex network-proxy and Cursor don't either). Shared IP: rules are by name, not address.
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (port, hosts) = http_server(String::new());
        fake_dns(&["allowed.test", "other.test"]); // both names share 127.0.0.1
        let policy_of = |src: &str| -> mare_binding::netauth::NetPolicy {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(src).eval().unwrap();
            mare_binding::netauth::NetPolicy::from_lua(&t).unwrap()
        };
        let local_ok = mare_binding::netproxy::ProxyHandle::start(policy_of(
            r#"return { allow = { "allowed.test" }, allow_local = true }"#,
        ))
        .unwrap();
        let strict =
            mare_binding::netproxy::ProxyHandle::start(policy_of(r#"return { allow = { "allowed.test" } }"#)).unwrap();
        let (p1, p2) = (local_ok.port(), strict.port());
        let res = tokio::task::spawn_blocking(move || {
            let inner = "GET / HTTP/1.1\r\nHost: other.test\r\nConnection: close\r\n\r\n";
            (
                // the allowed target passes; the name inside the tunnel isn't checked (documented limit)
                connect_through(p1, &format!("allowed.test:{port}"), inner),
                // same IP, different name: refused — the rule is about the target name
                connect_through(p1, &format!("other.test:{port}"), inner),
                // resolving to a private address without allow_local: refused
                connect_through(p2, &format!("allowed.test:{port}"), inner),
            )
        })
        .await
        .unwrap();
        mare_binding::netauth::set_resolver_override(None);
        let ((s1, b1), (s2, _), (s3, _)) = res;
        assert!(s1.contains(" 200 ") && b1.ends_with("ok"), "allowed target: {s1} {b1}");
        // allowed.test exists only in the proxy's checked resolution: reaching the server proves the
        // tunnel used the checked address; the tunnelled Host header is the unchecked inner name
        assert_eq!(hosts.lock().unwrap().as_slice(), ["other.test"]);
        assert!(s2.contains(" 403 "), "shared IP, other name: {s2}");
        assert!(s3.contains(" 403 "), "private address without allow_local: {s3}");
        let denied = strict.denied();
        assert!(denied.iter().any(|(h, r)| h == "allowed.test" && r.contains("local/private")), "{denied:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_sandbox_proxy_chains_to_an_upstream_proxy() {
    // Codex network-proxy UpstreamClient: behind a corporate proxy, allowed connections go
    // through it (CONNECT by name); refused targets never reach it
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (port, hosts) = http_server(String::new());
        let (up_port, connects) = upstream_proxy();
        fake_dns(&["allowed.test"]);
        let lua = mlua::Lua::new();
        let t: mlua::Table = lua.load(r#"return { allow = { "allowed.test" }, allow_local = true }"#).eval().unwrap();
        let policy = mare_binding::netauth::NetPolicy::from_lua(&t).unwrap();
        let upstream = mare_binding::netproxy::Upstream::fixed(&format!("http://127.0.0.1:{up_port}"), "");
        let proxy = mare_binding::netproxy::ProxyHandle::start_with(policy, None, Some(upstream)).unwrap();
        let p = proxy.port();
        let res = tokio::task::spawn_blocking(move || {
            let inner = "GET / HTTP/1.1\r\nHost: allowed.test\r\nConnection: close\r\n\r\n";
            (
                connect_through(p, &format!("allowed.test:{port}"), inner),
                connect_through(p, &format!("blocked.test:{port}"), inner),
            )
        })
        .await
        .unwrap();
        mare_binding::netauth::set_resolver_override(None);
        let ((s1, b1), (s2, _)) = res;
        assert!(s1.contains(" 200 ") && b1.ends_with("ok"), "through the upstream: {s1} {b1}");
        assert_eq!(connects.lock().unwrap().as_slice(), [format!("CONNECT allowed.test:{port} HTTP/1.1")]);
        assert_eq!(hosts.lock().unwrap().as_slice(), ["allowed.test"]);
        assert!(s2.contains(" 403 "), "refused before the upstream: {s2}");
        drop(proxy);
        h.shutdown().await;
    })
    .await
}

/// A keep-alive origin on 127.0.0.1: answers every request on a connection (closing only after
/// `Connection: close`) and records `<request line> | host=<Host> | connection=<Connection>`.
fn keepalive_server() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = srv.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(vec![]));
    let s2 = seen.clone();
    std::thread::spawn(move || {
        for c in srv.incoming().flatten() {
            let s2 = s2.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut c = c;
                loop {
                    let mut lines = vec![];
                    loop {
                        let mut l = String::new();
                        if r.read_line(&mut l).unwrap_or(0) == 0 {
                            return;
                        }
                        if l == "\r\n" {
                            break;
                        }
                        lines.push(l.trim_end().to_string());
                    }
                    let get = |k: &str| {
                        lines
                            .iter()
                            .find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(k)))
                            .map(|(_, v)| v.trim().to_string())
                            .unwrap_or_default()
                    };
                    let (host, conn) = (get("host"), get("connection"));
                    s2.lock().unwrap().push(format!("{} | host={host} | connection={conn}", lines[0]));
                    let _ = c.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
                    if conn.eq_ignore_ascii_case("close") {
                        return;
                    }
                }
            });
        }
    });
    (port, seen)
}

/// Raw bytes to the sandbox proxy; everything it answers until it closes (or 5 s pass).
fn raw_through(proxy: u16, req: &str) -> String {
    use std::io::Read;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", proxy)).unwrap();
    s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut out = vec![];
    let _ = s.read_to_end(&mut out);
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn plain_http_through_the_proxy_checks_the_host_header_and_every_request() {
    // Host-header fronting on a shared IP: the absolute-form URL names an allowed host, but the
    // Host header (what a virtual-hosting origin routes by) names another. The proxy refuses the
    // mismatch and duplicate Host headers, sends the checked authority as Host, and forwards one
    // request per connection (Connection: close), so a second keep-alive request is never sent.
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (port, seen) = keepalive_server();
        fake_dns(&["allowed.test", "other.test"]); // both names share 127.0.0.1
        let lua = mlua::Lua::new();
        let t: mlua::Table = lua.load(r#"return { allow = { "allowed.test" }, allow_local = true }"#).eval().unwrap();
        let policy = mare_binding::netauth::NetPolicy::from_lua(&t).unwrap();
        let proxy = mare_binding::netproxy::ProxyHandle::start_with(policy, None, None).unwrap();
        let p = proxy.port();
        let res = tokio::task::spawn_blocking(move || {
            let a = format!("allowed.test:{port}");
            (
                // allowed, with a keep-alive request: works, and is forwarded with Connection: close
                raw_through(p, &format!("GET http://{a}/ok HTTP/1.1\r\nHost: {a}\r\nConnection: keep-alive\r\n\r\n")),
                // fronting: allowed URL, other Host
                raw_through(p, &format!("GET http://{a}/front HTTP/1.1\r\nHost: other.test:{port}\r\n\r\n")),
                // duplicate Host headers
                raw_through(p, &format!("GET http://{a}/dup HTTP/1.1\r\nHost: {a}\r\nHost: other.test:{port}\r\n\r\n")),
                // pipelined keep-alive: the second request (to a disallowed host) never reaches the origin
                raw_through(
                    p,
                    &format!(
                        "GET http://{a}/first HTTP/1.1\r\nHost: {a}\r\nConnection: keep-alive\r\n\r\n\
                         GET http://other.test:{port}/second HTTP/1.1\r\nHost: other.test:{port}\r\n\r\n"
                    ),
                ),
                // origin-form with a disallowed Host
                raw_through(p, &format!("GET /origin HTTP/1.1\r\nHost: other.test:{port}\r\n\r\n")),
            )
        })
        .await
        .unwrap();
        mare_binding::netauth::set_resolver_override(None);
        let (ok, front, dup, pipelined, origin) = res;
        assert!(ok.starts_with("HTTP/1.1 200 ") && ok.ends_with("ok"), "allowed: {ok}");
        assert!(front.starts_with("HTTP/1.1 403 ") && front.contains("does not match"), "fronting: {front}");
        assert!(dup.starts_with("HTTP/1.1 400 ") && dup.contains("more than one Host"), "duplicate Host: {dup}");
        assert!(pipelined.starts_with("HTTP/1.1 200 ") && pipelined.matches("HTTP/1.1").count() == 1, "{pipelined}");
        assert!(origin.starts_with("HTTP/1.1 403 "), "origin-form, disallowed Host: {origin}");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [
                format!("GET /ok HTTP/1.1 | host=allowed.test:{port} | connection=close"),
                format!("GET /first HTTP/1.1 | host=allowed.test:{port} | connection=close"),
            ]
        );
        let denied = proxy.denied();
        assert!(denied.iter().any(|(h, r)| h.starts_with("other.test") && r.contains("does not match")), "{denied:?}");
        assert!(denied.iter().any(|(h, r)| h == "other.test" && r.contains("allow list")), "{denied:?}");
        drop(proxy);
        h.shutdown().await;
    })
    .await
}
