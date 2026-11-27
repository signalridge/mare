use super::*;

#[tokio::test]
async fn fake_ip_local_proxy_uses_hostname_remote_proxy_rejected_and_config_off_disables() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.setup({ net = { fake_ip = "auto" } })"#).unwrap();
        let h = env.host().no_session().user_config(false).build().await;
        let (proxy_port, seen_http) = recording_http_proxy();
        let (up_port, seen_connect) = upstream_proxy();
        let (srv_port, _) = http_server(String::new());
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "fake.test".to_string(),
            vec!["198.18.0.5".parse().unwrap(), "fdfe:dcba:9876::1".parse().unwrap()],
        )])));
        global(&h, "LOCAL_PROXY", format!("http://127.0.0.1:{proxy_port}"));
        global(&h, "FAKE_URL", format!("http://fake.test:{srv_port}/hello"));
        let (ok_local, body, ok_remote, remote_err, ok_lit, lit_err): (bool, String, bool, String, bool, String) =
            eval(
                &h,
                r#"
                local ctx_local = { sandbox = { level = "workspace-write", network = { allow = { "fake.test", "198.18.0.5" }, allow_local = true } } }
                local ok1, r1 = mare.exec.run(ctx_local, function()
                    return pcall(function()
                        return mare.http.request({ url = FAKE_URL, proxy = LOCAL_PROXY }):text()
                    end)
                end)
                local ctx = { sandbox = { level = "workspace-write", network = { allow = { "fake.test", "198.18.0.5" } } } }
                local ok2, r2, ok3, r3 = mare.exec.run(ctx, function()
                    local ok2, r2 = pcall(function()
                        return mare.http.request({ url = FAKE_URL, proxy = "http://203.0.113.10:8080" }):text()
                    end)
                    local ok3, r3 = pcall(function()
                        return mare.http.request({ url = "http://198.18.0.5/hello" }):text()
                    end)
                    return ok2, tostring(r2), ok3, tostring(r3)
                end)
                return ok1, tostring(r1), ok2, r2, ok3, r3
            "#,
            )
            .await;
        assert!(ok_local && body == "ok", "local proxy fake-ip failed: {body}");
        assert!(
            seen_http.lock().unwrap().iter().any(|l| l.contains("http://fake.test:") && !l.contains("198.18.0.5")),
            "local proxy must receive hostname, got: {:?}",
            seen_http.lock().unwrap()
        );
        assert!(!ok_remote && remote_err.contains("non-local proxy"), "{remote_err}");
        assert!(!ok_lit && lit_err.contains("local/private"), "{lit_err}");

        // Sandbox proxy: local upstream with allow_local=true + fake_ip=Auto receives hostname;
        // remote upstream with allow_local=false + fake_ip=Auto is refused.
        let mut local_policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "fake.test" }, allow_local = true }"#).eval().unwrap();
            mare_binding::netauth::NetPolicy::from_lua(&t).unwrap()
        };
        local_policy.fake_ip = mare_binding::netauth::FakeIpMode::Auto;
        let mut remote_policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "fake.test" } }"#).eval().unwrap();
            mare_binding::netauth::NetPolicy::from_lua(&t).unwrap()
        };
        remote_policy.fake_ip = mare_binding::netauth::FakeIpMode::Auto;
        let local_up = mare_binding::netproxy::ProxyHandle::start_with(
            local_policy,
            None,
            Some(mare_binding::netproxy::Upstream::fixed(&format!("http://127.0.0.1:{up_port}"), "")),
        )
        .unwrap();
        let remote_up = mare_binding::netproxy::ProxyHandle::start_with(
            remote_policy,
            None,
            Some(mare_binding::netproxy::Upstream::fixed("http://203.0.113.10:8080", "")),
        )
        .unwrap();
        let (lp, rp) = (local_up.port(), remote_up.port());
        let (s_local, s_remote) = tokio::task::spawn_blocking(move || {
            let inner = "GET / HTTP/1.1\r\nHost: fake.test\r\nConnection: close\r\n\r\n";
            (
                connect_through(lp, &format!("fake.test:{srv_port}"), inner),
                connect_through(rp, &format!("fake.test:{srv_port}"), inner),
            )
        })
        .await
        .unwrap();
        assert!(s_local.0.contains(" 200 "), "local upstream CONNECT: {:?}", s_local);
        assert!(
            seen_connect.lock().unwrap().iter().any(|l| l.contains(&format!("fake.test:{srv_port}"))),
            "upstream must receive hostname: {:?}",
            seen_connect.lock().unwrap()
        );
        assert!(s_remote.0.contains(" 403 "), "remote upstream must be refused: {:?}", s_remote);
        h.shutdown().await;
        drop(local_up);
        drop(remote_up);
        drop(env);

        // Config `net.fake_ip = "off"` disables fake-ip recognition even on direct requests
        let env2 = TestEnv::new().await;
        std::fs::create_dir_all(env2.config_dir()).unwrap();
        std::fs::write(env2.config_dir().join("init.lua"), r#"mare.setup({ net = { fake_ip = "off" } })"#).unwrap();
        let h2 = env2.host().no_session().user_config(false).build().await;
        global(&h2, "FAKE_URL", format!("http://fake.test:{srv_port}/hello"));
        let (warns, ok_off, off_err): (Vec<String>, bool, String) = eval(
            &h2,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "fake.test" } } } }
            local ok, err = mare.exec.run(ctx, function()
                return pcall(function()
                    return mare.http.request({ url = FAKE_URL }):text()
                end)
            end)
            return mare.config_warnings(), ok, tostring(err)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(warns.is_empty(), "unexpected config warnings: {warns:?}");
        assert!(!ok_off && off_err.contains("local/private"), "{off_err}");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn default_config_rejects_198_18_0_0_15_and_http_proxy_rejects_unpinned_hostname() {
    local(async {
        let (srv_port, seen_srv) = http_server(String::new());
        let (proxy_port, seen_http) = recording_http_proxy();
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([
            ("fake.test".to_string(), vec!["198.18.0.5".parse().unwrap()]),
            ("rebind.test".to_string(), vec!["93.184.216.34".parse().unwrap()]),
            ("mixed-fake.test".to_string(), vec!["198.18.0.6".parse().unwrap(), "fd00::1".parse().unwrap()]),
        ])));

        // 1. Default config (net.fake_ip unset -> "off") rejects 198.18.0.0/15 direct request as non-public.
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        global(&h, "FAKE_URL", format!("http://fake.test:{srv_port}/hello"));
        let (ok_def, def_err): (bool, String) = eval(
            &h,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "fake.test" } } } }
            return mare.exec.run(ctx, function()
                local ok1, r1 = pcall(function()
                    return mare.http.request({ url = FAKE_URL }):text()
                end)
                return ok1, tostring(r1)
            end)
        "#,
        )
        .await;
        assert!(!ok_def && def_err.contains("local/private"), "default config must reject 198.18.0.5: {def_err}");
        assert!(
            seen_srv.lock().unwrap().is_empty(),
            "private server must not be reached: {:?}",
            seen_srv.lock().unwrap()
        );
        h.shutdown().await;
        drop(env);

        // 2. Even with net.fake_ip = "auto", explicit proxy with allow_local = false rejects mixed fake-IPv4 + ULA.
        let env2 = TestEnv::new().await;
        std::fs::create_dir_all(env2.config_dir()).unwrap();
        std::fs::write(env2.config_dir().join("init.lua"), r#"mare.setup({ net = { fake_ip = "auto" } })"#).unwrap();
        let h2 = env2.host().no_session().user_config(false).build().await;
        global(&h2, "LOCAL_PROXY", format!("http://127.0.0.1:{proxy_port}"));
        global(&h2, "MIXED_URL", format!("http://mixed-fake.test:{srv_port}/secret"));
        let (ok_mixed, mixed_err): (bool, String) = eval(
            &h2,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "mixed-fake.test" } } } }
            return mare.exec.run(ctx, function()
                local ok, r = pcall(function()
                    return mare.http.request({ url = MIXED_URL, proxy = LOCAL_PROXY }):text()
                end)
                return ok, tostring(r)
            end)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(
            !ok_mixed && mixed_err.contains("allow_local"),
            "explicit proxy must reject mixed fake-IPv4+ULA when allow_local=false: {mixed_err}"
        );
        assert!(seen_http.lock().unwrap().is_empty(), "proxy must not be contacted: {:?}", seen_http.lock().unwrap());
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicit_proxy_with_whitespace_pins_ip_and_allows_authorized_host() {
    local(async {
        let (srv_port, seen_hosts) = http_server(String::new());
        let (up_port, seen_connects) = upstream_proxy();
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "rebind.test".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        )])));

        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        let h = env.host().no_session().build().await;
        global(&h, "SPACED_PROXY", format!(" http://127.0.0.1:{up_port} "));
        global(&h, "TARGET_URL", format!("http://rebind.test:{srv_port}/hello"));
        let (ok, res): (bool, String) = eval(
            &h,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "rebind.test" } } } }
            return mare.exec.run(ctx, function()
                local ok, r = pcall(function()
                    return mare.http.request({ url = TARGET_URL, proxy = SPACED_PROXY }):text()
                end)
                return ok, tostring(r)
            end)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(
            ok && res == "ok",
            "authorized host via spaced proxy must succeed with pinned CONNECT: ok={ok}, res={res}"
        );
        let connects = seen_connects.lock().unwrap().clone();
        assert_eq!(
            connects,
            vec![format!("CONNECT 93.184.216.34:{srv_port} HTTP/1.1")],
            "upstream proxy must receive pinned IP CONNECT, not hostname: {connects:?}"
        );
        let hosts = seen_hosts.lock().unwrap().clone();
        assert_eq!(
            hosts,
            vec![format!("rebind.test:{srv_port}")],
            "origin server must receive original Host header: {hosts:?}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn pinned_proxy_shim_cleans_up_tasks_and_sockets_on_cancel_and_close() {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    local(async {
        // (a) Upstream stalls in CONNECT phase; timeout/cancel must close the upstream socket promptly.
        let stalled_eof = Arc::new(Mutex::new(None::<bool>));
        let stalled_eof_srv = stalled_eof.clone();
        let (tx_stalled, rx_stalled) = tokio::sync::oneshot::channel::<()>();
        let stall_srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let stall_port = stall_srv.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut c, _)) = stall_srv.accept() else { return };
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if c.read(&mut b).unwrap_or(0) == 0 {
                    break;
                }
                head.push(b[0]);
            }
            c.set_read_timeout(Some(std::time::Duration::from_millis(800))).unwrap();
            let eof = matches!(c.read(&mut b), Ok(0));
            *stalled_eof_srv.lock().unwrap() = Some(eof);
            let _ = tx_stalled.send(());
        });

        // (b) Upstream completes CONNECT and keeps connection open after sending partial chunked body;
        // resp:close() must close the upstream socket promptly while allowing streaming before close().
        let keepopen_res = Arc::new(Mutex::new(None::<String>));
        let keepopen_res_srv = keepopen_res.clone();
        let (tx_keepopen, rx_keepopen) = tokio::sync::oneshot::channel::<()>();
        let keepopen_srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let keepopen_port = keepopen_srv.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let Ok((mut c, _)) = keepopen_srv.accept() else { return };
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if c.read(&mut b).unwrap_or(0) == 0 {
                    return;
                }
                head.push(b[0]);
            }
            c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
            // Read the inner HTTP request head
            let mut inner = Vec::new();
            while !inner.ends_with(b"\r\n\r\n") {
                if c.read(&mut b).unwrap_or(0) == 0 {
                    return;
                }
                inner.push(b[0]);
            }
            // Send headers + first chunk, then hold the connection open without closing
            c.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n").unwrap();
            c.flush().unwrap();
            c.set_read_timeout(Some(std::time::Duration::from_millis(800))).unwrap();
            let res = format!("{:?}", c.read(&mut b));
            *keepopen_res_srv.lock().unwrap() = Some(res);
            let _ = tx_keepopen.send(());
        });

        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "rebind.test".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        )])));
        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        let h = env.host().no_session().build().await;
        global(&h, "STALL_PROXY", format!("http://127.0.0.1:{stall_port}"));
        global(&h, "KEEPOPEN_PROXY", format!("http://127.0.0.1:{keepopen_port}"));

        let (ok_stall, stall_err, chunk): (bool, String, String) = eval(
            &h,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "rebind.test" } } } }
            return mare.exec.run(ctx, function()
                local ok1, r1 = pcall(function()
                    return mare.http.request({
                        url = "http://rebind.test:8080/stall",
                        proxy = STALL_PROXY,
                        timeout_ms = 120,
                    }):text()
                end)
                local resp = mare.http.request({
                    url = "http://rebind.test:8080/stream",
                    proxy = KEEPOPEN_PROXY,
                })
                local chunk = resp:read()
                resp:close()
                return ok1, tostring(r1), tostring(chunk)
            end)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(!ok_stall, "stalled CONNECT must time out: {stall_err}");
        assert_eq!(chunk, "hello", "streaming chunk must be readable before close()");
        tokio::time::timeout(std::time::Duration::from_secs(2), rx_stalled).await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), rx_keepopen).await.unwrap().unwrap();
        assert_eq!(
            *stalled_eof.lock().unwrap(),
            Some(true),
            "upstream socket stalled in CONNECT must close when request times out/cancels"
        );
        assert_eq!(
            *keepopen_res.lock().unwrap(),
            Some("Ok(0)".to_string()),
            "upstream socket held open after response must close when resp:close() runs"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn allow_local_true_preserves_proxy_basic_auth_for_regular_and_fake_ip_targets() {
    use std::io::{Read, Write};
    local(async {
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let srv_port = srv.local_addr().unwrap().port();
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = proxy.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in proxy.incoming() {
                let Ok(mut c) = c else { break };
                std::thread::spawn(move || {
                    let mut head = Vec::new();
                    let mut b = [0u8; 1];
                    while !head.ends_with(b"\r\n\r\n") {
                        if c.read(&mut b).unwrap_or(0) == 0 {
                            return;
                        }
                        head.push(b[0]);
                    }
                    let text = String::from_utf8_lossy(&head);
                    // Base64("user:pass") == "dXNlcjpwYXNz"
                    let has_auth = text
                        .lines()
                        .any(|l| l.to_ascii_lowercase().starts_with("proxy-authorization: basic dxnlcjpwyxnz"));
                    if !has_auth {
                        let _ = c.write_all(
                            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    }
                    let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                });
            }
        });
        drop(srv);

        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "fake.test".to_string(),
            vec!["198.18.0.5".parse().unwrap()],
        )])));
        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.setup({ net = { fake_ip = "auto" } })"#).unwrap();
        let h = env.host().no_session().user_config(false).build().await;
        global(&h, "AUTH_PROXY", format!("http://user:pass@127.0.0.1:{proxy_port}"));
        global(&h, "REG_URL", format!("http://127.0.0.1:{srv_port}/regular"));
        global(&h, "FAKE_URL", format!("http://fake.test:{srv_port}/fake"));

        let (ok_reg, reg_status, reg_body, ok_fake, fake_status, fake_body): (
            bool,
            i64,
            String,
            bool,
            i64,
            String,
        ) = eval(
            &h,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "127.0.0.1", "fake.test" }, allow_local = true } } }
            return mare.exec.run(ctx, function()
                local r1 = mare.http.request({ url = REG_URL, proxy = AUTH_PROXY })
                local b1 = r1:text()
                local r2 = mare.http.request({ url = FAKE_URL, proxy = AUTH_PROXY })
                local b2 = r2:text()
                return r1.ok, r1.status, b1, r2.ok, r2.status, b2
            end)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);
        assert!(ok_reg && reg_status == 200 && reg_body == "ok", "regular target failed: status={reg_status}, body={reg_body}");
        assert!(ok_fake && fake_status == 200 && fake_body == "ok", "fake-ip target failed: status={fake_status}, body={fake_body}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn allow_local_true_rejects_fake_ip_via_remote_proxy_and_https_proxy_when_strict() {
    local(async {
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([
            ("fake.test".to_string(), vec!["198.18.0.5".parse().unwrap()]),
            ("rebind.test".to_string(), vec!["93.184.216.34".parse().unwrap()]),
        ])));
        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.setup({ net = { fake_ip = "auto" } })"#).unwrap();
        let h = env.host().no_session().user_config(false).build().await;

        let (ok_http, http_err, ok_https, https_err): (bool, String, bool, String) = eval(
            &h,
            r#"
            local ctx_local = { sandbox = { level = "workspace-write", network = { allow = { "fake.test" }, allow_local = true } } }
            local ok1, r1 = mare.exec.run(ctx_local, function()
                return pcall(function()
                    return mare.http.request({ url = "http://fake.test:8080/hello", proxy = "http://203.0.113.10:8080" }):text()
                end)
            end)
            local ctx_strict = { sandbox = { level = "workspace-write", network = { allow = { "rebind.test" } } } }
            local ok2, r2 = mare.exec.run(ctx_strict, function()
                return pcall(function()
                    return mare.http.request({ url = "http://rebind.test:8080/hello", proxy = "https://127.0.0.1:8443" }):text()
                end)
            end)
            return ok1, tostring(r1), ok2, tostring(r2)
        "#,
        )
        .await;
        assert!(!ok_http && http_err.contains("non-local proxy"), "allow_local=true must reject remote proxy for fake-ip: {http_err}");
        assert!(
            !ok_https && https_err.contains("cannot be pinned when allow_local = false"),
            "https:// proxy must fail closed when allow_local=false: {https_err}"
        );

        let mut remote_policy = {
            let lua = mlua::Lua::new();
            let t: mlua::Table = lua.load(r#"return { allow = { "fake.test" }, allow_local = true }"#).eval().unwrap();
            mare_binding::netauth::NetPolicy::from_lua(&t).unwrap()
        };
        remote_policy.fake_ip = mare_binding::netauth::FakeIpMode::Auto;
        let remote_up = mare_binding::netproxy::ProxyHandle::start_with(
            remote_policy,
            None,
            Some(mare_binding::netproxy::Upstream::fixed("http://203.0.113.10:8080", "")),
        )
        .unwrap();
        let rp = remote_up.port();
        let (status, _) = tokio::task::spawn_blocking(move || {
            connect_through(rp, "fake.test:8080", "GET / HTTP/1.1\r\nHost: fake.test\r\nConnection: close\r\n\r\n")
        })
        .await
        .unwrap();
        mare_binding::netauth::set_resolver_override(None);
        assert!(status.contains(" 403 "), "sandbox proxy with allow_local=true must refuse fake-ip via remote upstream: {status}");
        drop(remote_up);
        h.shutdown().await;
    })
    .await
}

// 参考测试：reference implementation/Codex/Kimi Code 三家均无同类测试
#[tokio::test]
async fn concurrent_close_unblocks_stalled_read_and_sse() {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    local(async {
        fn spawn_stalled_proxy(
            response_head: &'static [u8],
        ) -> (u16, Arc<Mutex<Option<String>>>, tokio::sync::oneshot::Receiver<()>) {
            let res = Arc::new(Mutex::new(None::<String>));
            let res_srv = res.clone();
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = srv.local_addr().unwrap().port();
            std::thread::spawn(move || {
                let Ok((mut c, _)) = srv.accept() else { return };
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if c.read(&mut b).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                c.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").unwrap();
                let mut inner = Vec::new();
                while !inner.ends_with(b"\r\n\r\n") {
                    if c.read(&mut b).unwrap_or(0) == 0 {
                        return;
                    }
                    inner.push(b[0]);
                }
                c.write_all(response_head).unwrap();
                c.flush().unwrap();
                c.set_read_timeout(Some(std::time::Duration::from_millis(800))).unwrap();
                let r = format!("{:?}", c.read(&mut b));
                *res_srv.lock().unwrap() = Some(r);
                let _ = tx.send(());
            });
            (port, res, rx)
        }

        let (read_port, read_eof, rx_read) =
            spawn_stalled_proxy(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        let (sse_port, sse_eof, rx_sse) =
            spawn_stalled_proxy(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n");

        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "rebind.test".to_string(),
            vec!["93.184.216.34".parse().unwrap()],
        )])));
        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        let h = env.host().no_session().build().await;
        global(&h, "READ_PROXY", format!("http://127.0.0.1:{read_port}"));
        global(&h, "SSE_PROXY", format!("http://127.0.0.1:{sse_port}"));

        let (read_close_idx, read_ok, read_err, sse_close_idx, sse_ok, sse_err): (
            i64,
            bool,
            String,
            i64,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "rebind.test" } } } }
            return mare.exec.run(ctx, function()
                local function check_close(proxy_url, method_name)
                    local resp = mare.http.request({
                        url = "http://rebind.test:8080/stall",
                        proxy = proxy_url,
                    })
                    local ready_tx, ready_rx = mare.task.channel()
                    local done_tx, done_rx = mare.task.channel()
                    mare.task.spawn(function()
                        ready_tx:send(true)
                        local ok, err = pcall(function() return resp[method_name](resp) end)
                        done_tx:send({ ok = ok, err = tostring(err) })
                    end)
                    assert(ready_rx:recv())
                    mare.task.sleep(30)
                    local close_idx = mare.task.select(
                        function() resp:close(); return "closed" end,
                        function() mare.task.sleep(500); return "timeout" end
                    )
                    local res = done_rx:recv()
                    return close_idx, res.ok, res.err
                end

                local c1, ok1, err1 = check_close(READ_PROXY, "read")
                local c2, ok2, err2 = check_close(SSE_PROXY, "next_event")
                return c1, ok1, err1, c2, ok2, err2
            end)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);

        assert_eq!(read_close_idx, 1, "resp:close() must unblock stalled read() without timing out (err={read_err})");
        assert!(
            !read_ok && read_err.contains("aborted"),
            "stalled read() must abort on close: ok={read_ok}, err={read_err}"
        );
        assert_eq!(
            sse_close_idx, 1,
            "resp:close() must unblock stalled next_event() without timing out (err={sse_err})"
        );
        assert!(
            !sse_ok && sse_err.contains("aborted"),
            "stalled next_event() must abort on close: ok={sse_ok}, err={sse_err}"
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), rx_read).await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), rx_sse).await.unwrap().unwrap();
        assert_eq!(
            *read_eof.lock().unwrap(),
            Some("Ok(0)".to_string()),
            "upstream read proxy socket must see EOF after close()"
        );
        assert_eq!(
            *sse_eof.lock().unwrap(),
            Some("Ok(0)".to_string()),
            "upstream SSE proxy socket must see EOF after close()"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn offline_guard_blocks_fake_ip_local_proxy_and_selected_non_loopback_proxy_unless_noproxy() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.setup({ net = { fake_ip = "auto" } })"#).unwrap();
        let (srv_port, _) = http_server(String::new());
        let (proxy_port, seen_http) = recording_http_proxy();
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "fake.test".to_string(),
            vec!["198.18.0.5".parse().unwrap()],
        )])));

        // Set HTTP_PROXY to a non-loopback proxy and NO_PROXY=127.0.0.1 so direct loopback
        // succeeds when NO_PROXY excludes it, while an explicit non-loopback proxy is blocked.
        env.set_var("HTTP_PROXY", "http://203.0.113.10:8080");
        env.set_var("NO_PROXY", "127.0.0.1");

        let h = env.host().no_session().user_config(false).build().await;
        global(&h, "LOCAL_PROXY", format!("http://127.0.0.1:{proxy_port}"));
        global(&h, "FAKE_URL", format!("http://fake.test:{srv_port}/hello"));
        global(&h, "LOOPBACK_URL", format!("http://127.0.0.1:{srv_port}/hello"));
        global(&h, "LOCALHOST_URL", format!("http://localhost:{srv_port}/hello"));

        let (ok_fake, fake_err, ok_remote_proxy, remote_proxy_err, ok_noproxy, noproxy_res): (
            bool,
            String,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local ctx = { sandbox = { level = "workspace-write", network = { allow = { "fake.test", "127.0.0.1", "localhost" }, allow_local = true } } }
            return mare.exec.run(ctx, function()
                local ok1, r1 = pcall(function()
                    return mare.http.request({ url = FAKE_URL, proxy = LOCAL_PROXY }):text()
                end)
                local ok2, r2 = pcall(function()
                    return mare.http.request({ url = LOCALHOST_URL, proxy = "http://203.0.113.10:8080" }):text()
                end)
                local ok3, r3 = pcall(function()
                    return mare.http.request({ url = LOOPBACK_URL }):text()
                end)
                return ok1, tostring(r1), ok2, tostring(r2), ok3, tostring(r3)
            end)
        "#,
        )
        .await;
        mare_binding::netauth::set_resolver_override(None);

        assert!(
            !ok_fake && fake_err.contains("offline test guard"),
            "ViaLocalProxyByName must reject non-loopback target under offline guard: ok={ok_fake}, err={fake_err}"
        );
        assert!(
            seen_http.lock().unwrap().is_empty(),
            "local proxy must not be contacted when offline guard rejects target: {:?}",
            seen_http.lock().unwrap()
        );
        assert!(
            !ok_remote_proxy && remote_proxy_err.contains("offline test guard"),
            "selected non-loopback proxy must be rejected under offline guard: ok={ok_remote_proxy}, err={remote_proxy_err}"
        );
        assert!(
            ok_noproxy && noproxy_res == "ok",
            "NO_PROXY-excluded non-loopback proxy must not block loopback request: ok={ok_noproxy}, res={noproxy_res}"
        );
        h.shutdown().await;
    })
    .await
}
