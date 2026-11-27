//! Origin fences, rate limits, WebSocket/header limits, and listener cookies.

use super::*;

#[tokio::test]
async fn origin_and_sec_fetch_site_fence_blocks_cross_site_http_and_ws() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve {
                        token = "secret-tok",
                        allowed_origins = { "https://trusted.example" },
                        handler = function(req)
                            if req.path == "/ws" then
                                local ws = req:upgrade()
                                if ws then
                                    ws:send("ok")
                                    ws:close()
                                end
                                return nil
                            end
                            return { status = 200, body = "ok" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let client = crate::http::test_client();

            // 1. Sec-Fetch-Site: cross-site is rejected even with valid token & matching Origin.
            let resp = client
                .get(format!("http://{addr}/"))
                .header("authorization", "Bearer secret-tok")
                .header("origin", format!("http://{addr}"))
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403);

            // 2. Mismatched Origin is rejected.
            let resp = client
                .get(format!("http://{addr}/"))
                .header("authorization", "Bearer secret-tok")
                .header("origin", "http://evil.example")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403);

            // 3. Opaque Origin: null is rejected.
            let resp = client
                .get(format!("http://{addr}/"))
                .header("authorization", "Bearer secret-tok")
                .header("origin", "null")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 403);

            // 4. Same-authority Origin succeeds.
            let resp = client
                .get(format!("http://{addr}/"))
                .header("authorization", "Bearer secret-tok")
                .header("origin", format!("http://{addr}"))
                .header("sec-fetch-site", "same-origin")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);

            // 5. Explicitly allowed_origins succeeds.
            let resp = client
                .get(format!("http://{addr}/"))
                .header("authorization", "Bearer secret-tok")
                .header("origin", "https://trusted.example")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);

            // 6. Cross-site WebSocket upgrade is rejected with 403.
            let mut ws_req = format!("ws://{addr}/ws?token=secret-tok").into_client_request().unwrap();
            ws_req.headers_mut().insert("origin", "http://evil.example".parse().unwrap());
            let err = tokio_tungstenite::connect_async(ws_req).await.unwrap_err();
            match err {
                tokio_tungstenite::tungstenite::Error::Http(r) => assert_eq!(r.status(), 403),
                other => panic!("expected HTTP 403, got {other:?}"),
            }

            // 7. Sec-Fetch-Site: cross-site WebSocket upgrade is rejected with 403.
            let mut ws_req = format!("ws://{addr}/ws?token=secret-tok").into_client_request().unwrap();
            ws_req.headers_mut().insert("sec-fetch-site", "cross-site".parse().unwrap());
            let err = tokio_tungstenite::connect_async(ws_req).await.unwrap_err();
            match err {
                tokio_tungstenite::tungstenite::Error::Http(r) => assert_eq!(r.status(), 403),
                other => panic!("expected HTTP 403, got {other:?}"),
            }

            // 8. Same-authority WebSocket upgrade succeeds.
            let mut ws_req = format!("ws://{addr}/ws?token=secret-tok").into_client_request().unwrap();
            ws_req.headers_mut().insert("origin", format!("http://{addr}").parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(ws_req).await.unwrap();
            assert_eq!(ws.next().await.unwrap().unwrap(), ClientWsMsg::Text("ok".into()));

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn auth_rate_limiting_and_audit_log_never_leak_token() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber(buf.clone());
    let _guard = tracing::subscriber::set_default(subscriber);

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let secret = "super-secret-audit-token-xyz123";
            let srv: mlua::AnyUserData = lua
                .load(format!(
                    r#"
                    return mare.http.serve {{
                        token = "{secret}",
                        handler = function() return {{ status = 200, body = "ok" }} end
                    }}
                "#
                ))
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let client = crate::http::test_client_no_redirect();

            // Valid request succeeds first.
            let resp = client.get(format!("http://{addr}/hello?token={secret}")).send().await.unwrap();
            assert_eq!(resp.status(), 303);

            // 5 consecutive failures -> 401, 6th -> 429 Too Many Requests with Retry-After.
            for _ in 0..5 {
                let r = client
                    .get(format!("http://{addr}/hello?token=wrong-attempt"))
                    .header("authorization", "Bearer wrong-attempt")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), 401);
            }
            let limited = client
                .get(format!("http://{addr}/hello"))
                .header("authorization", format!("Bearer {secret}"))
                .send()
                .await
                .unwrap();
            assert_eq!(limited.status(), 429);
            assert!(limited.headers().contains_key("retry-after"));

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;

    let logs = buf.lock().unwrap().join("\n");
    assert!(logs.contains("http auth succeeded"), "expected audit success log: {logs}");
    assert!(logs.contains("http auth failed"), "expected audit failure log: {logs}");
    assert!(logs.contains("http auth rate limited"), "expected rate limit log: {logs}");
    assert!(!logs.contains("super-secret-audit-token-xyz123"), "token leaked in logs: {logs}");
    assert!(!logs.contains("wrong-attempt"), "query token leaked in logs: {logs}");
}

#[tokio::test]
async fn header_read_timeout_drops_slowloris_and_frees_connection_slot() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve {
                        token = false,
                        max_connections = 1,
                        header_read_timeout_ms = 50,
                        handler = function() return { status = 200, body = "ok" } end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();
            let addr: String = srv.call_method("addr", ()).unwrap();

            // Slowloris client sends partial header and never finishes.
            let mut slow = tokio::net::TcpStream::connect(&addr).await.unwrap();
            slow.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n").await.unwrap();
            slow.flush().await.unwrap();

            // Wait for server to close slowloris connection due to header_read_timeout.
            let mut tmp = [0u8; 64];
            let read_res = tokio::time::timeout(Duration::from_secs(2), slow.read(&mut tmp)).await.unwrap();
            assert!(matches!(read_res, Ok(0) | Err(_)), "expected slowloris socket closed, got {read_res:?}");
            drop(slow);

            // Connection slot is now free for a normal client.
            let resp = crate::http::test_get(format!("http://{addr}/")).await.unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn websocket_message_and_frame_size_limits_are_enforced() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve {
                        token = false,
                        ws_max_message_bytes = 256,
                        ws_max_frame_bytes = 128,
                        handler = function(req)
                            local ws = req:upgrade()
                            if not ws then return nil end
                            while true do
                                local msg, reason = ws:recv()
                                if not msg then break end
                                ws:send(msg)
                            end
                            return nil
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();
            let addr: String = srv.call_method("addr", ()).unwrap();

            // 1. Frame within limit (100 bytes <= 128) succeeds.
            let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            ws.send(ClientWsMsg::Text("a".repeat(100).into())).await.unwrap();
            assert_eq!(ws.next().await.unwrap().unwrap(), ClientWsMsg::Text("a".repeat(100).into()));
            let _ = ws.close(None).await;

            // 2. Single frame exceeding ws_max_frame_bytes (200 > 128) is rejected.
            let (mut ws2, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            ws2.send(ClientWsMsg::Text("b".repeat(200).into())).await.unwrap();
            let next = tokio::time::timeout(Duration::from_secs(2), ws2.next()).await.unwrap();
            assert!(
                matches!(next, None | Some(Err(_)) | Some(Ok(ClientWsMsg::Close(_)))),
                "expected oversized frame to close ws, got {next:?}"
            );

            // 3. Fragmented message: 3 frames of 100 bytes each (300 > ws_max_message_bytes 256) is rejected.
            use tokio_tungstenite::tungstenite::protocol::frame::{
                Frame,
                coding::{Data, OpCode},
            };
            let (mut ws3, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            ws3.send(ClientWsMsg::Frame(Frame::message(vec![b'c'; 100], OpCode::Data(Data::Text), false)))
                .await
                .unwrap();
            ws3.send(ClientWsMsg::Frame(Frame::message(vec![b'c'; 100], OpCode::Data(Data::Continue), false)))
                .await
                .unwrap();
            ws3.send(ClientWsMsg::Frame(Frame::message(vec![b'c'; 100], OpCode::Data(Data::Continue), true)))
                .await
                .unwrap();
            let next3 = tokio::time::timeout(Duration::from_secs(2), ws3.next()).await.unwrap();
            assert!(
                matches!(next3, None | Some(Err(_)) | Some(Ok(ClientWsMsg::Close(_)))),
                "expected oversized fragmented message to close ws, got {next3:?}"
            );

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn accept_error_backs_off_and_continues_serving() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let hooks = Arc::new(super::ws::ServeTestHooks::default());
            hooks.inject_accept_error.store(true, Ordering::Release);
            let err_notified = hooks.on_accept_error.notified();

            let opts: mlua::Table = lua
                .load(r#"{ token = false, handler = function() return { status = 200, body = "survived" } end }"#)
                .eval()
                .unwrap();
            let mv = super::serve_with_hooks(lua.clone(), opts, hooks.clone()).await.unwrap();
            let srv = match mv.into_iter().next().unwrap() {
                mlua::Value::UserData(ud) => ud,
                other => panic!("expected HttpServer, got {other:?}"),
            };
            let addr: String = srv.call_method("addr", ()).unwrap();

            tokio::time::timeout(Duration::from_secs(2), err_notified).await.unwrap();

            // Server must still accept and serve requests after the transient accept error.
            let resp = crate::http::test_get(format!("http://{addr}/")).await.unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "survived");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[test]
fn host_token_path_is_protected_by_sandbox_policies() {
    let lua = Lua::new();
    let spec: mlua::Table = lua.load(r#"{ level = "workspace-write" }"#).eval().unwrap();
    let policy = crate::ossandbox::Policy::from_lua(&lua, &mlua::Value::Table(spec), &std::env::current_dir().unwrap())
        .unwrap()
        .unwrap();
    let token_file = mare_shared::paths::host_token_file();

    assert!(!policy.can_read(&token_file), "sandbox policy must deny reading host token file");
    assert!(!policy.can_write(&token_file), "sandbox policy must deny writing host token file");

    let sbpl = policy.seatbelt(None);
    let token_dir = token_file.parent().unwrap();
    let escaped_dir =
        crate::realpath::resolve(token_dir).unwrap().to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let deny_rule = format!("(deny file-read* file-write* (subpath \"{escaped_dir}\"))");
    assert!(sbpl.contains(&deny_rule), "seatbelt profile must deny host token dir: {sbpl}");

    let registered = crate::ossandbox::protected::host_protected();
    assert!(
        registered.iter().any(|e| e.path == token_dir && e.on_gap == crate::ossandbox::protected::GapAction::Refuse),
        "host_protected registry must include host token dir with Refuse: {registered:?}"
    );
    let win_defaults = crate::winsandbox::enhanced::default_deny_read(Path::new("/home/u"), None, None);
    assert!(
        !win_defaults.iter().any(|p| p.ends_with("mare/host") || p.ends_with(r"mare\host")),
        "host token dir must be registered in host_protected, not user default_deny_read: {win_defaults:?}"
    );
}

#[tokio::test]
async fn bearer_and_header_auth_strip_query_token_on_get() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = Lua::new();
            let srv = start_server(
                &lua,
                r#"{
                    token = "sec-tok-strip",
                    handler = function(req)
                        return { status = 200, body = req.method .. ":" .. req.path }
                    end
                }"#,
            )
            .await;
            let addr: String = srv.call_method("addr", ()).unwrap();
            let no_redirect = crate::http::test_client_no_redirect();

            // Bearer + GET ?token=...&k=v -> 303 stripping token, keeping k=v
            let resp = no_redirect
                .get(format!("http://{addr}/app?token=sec-tok-strip&k=v"))
                .header("authorization", "Bearer sec-tok-strip")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 303);
            assert_eq!(resp.headers().get("location").unwrap().to_str().unwrap(), "/app?k=v");
            assert_eq!(resp.headers().get("cache-control").unwrap().to_str().unwrap(), "no-store");
            assert_eq!(resp.headers().get("referrer-policy").unwrap().to_str().unwrap(), "no-referrer");
            assert!(resp.headers().get("set-cookie").is_none());

            // x-mare-token + GET ?token=...&k=v -> 303 stripping token, keeping k=v
            let resp = no_redirect
                .get(format!("http://{addr}/app?k=v&token=sec-tok-strip"))
                .header("x-mare-token", "sec-tok-strip")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 303);
            assert_eq!(resp.headers().get("location").unwrap().to_str().unwrap(), "/app?k=v");
            assert_eq!(resp.headers().get("cache-control").unwrap().to_str().unwrap(), "no-store");
            assert_eq!(resp.headers().get("referrer-policy").unwrap().to_str().unwrap(), "no-referrer");
            assert!(resp.headers().get("set-cookie").is_none());

            // Non-GET (POST) with Bearer + ?token= is not redirected
            let resp = no_redirect
                .post(format!("http://{addr}/app?token=sec-tok-strip"))
                .header("authorization", "Bearer sec-tok-strip")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "POST:/app");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn per_listener_cookie_names_do_not_collide_across_ports() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = Lua::new();
            let s1 = start_server(
                &lua,
                r#"{ token = "tok-one", handler = function() return { status = 200, body = "one" } end }"#,
            )
            .await;
            let s2 = start_server(
                &lua,
                r#"{ token = "tok-two", handler = function() return { status = 200, body = "two" } end }"#,
            )
            .await;
            let a1: String = s1.call_method("addr", ()).unwrap();
            let a2: String = s2.call_method("addr", ()).unwrap();
            let no_redirect = crate::http::test_client_no_redirect();

            let r1 = no_redirect.get(format!("http://{a1}/?token=tok-one")).send().await.unwrap();
            assert_eq!(r1.status(), 303);
            let c1 = r1.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();

            let r2 = no_redirect.get(format!("http://{a2}/?token=tok-two")).send().await.unwrap();
            assert_eq!(r2.status(), 303);
            let c2 = r2.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();

            assert_ne!(c1.split('=').next().unwrap(), c2.split('=').next().unwrap());
            let jar = format!("{c1}; {c2}");

            let ok1 = no_redirect.get(format!("http://{a1}/")).header("cookie", &jar).send().await.unwrap();
            assert_eq!(ok1.status(), 200);
            assert_eq!(ok1.text().await.unwrap(), "one");

            let ok2 = no_redirect.get(format!("http://{a2}/")).header("cookie", &jar).send().await.unwrap();
            assert_eq!(ok2.status(), 200);
            assert_eq!(ok2.text().await.unwrap(), "two");

            s1.call_async_method::<()>("close", ()).await.unwrap();
            s2.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[test]
fn second_loopback_error_distinguishes_unavailable_from_startup_failure() {
    assert!(is_loopback_bind_unavailable_io_err(&std::io::Error::from(std::io::ErrorKind::AddrNotAvailable)));
    assert!(is_loopback_bind_unavailable_io_err(&std::io::Error::from(std::io::ErrorKind::PermissionDenied)));
    assert!(!is_loopback_bind_unavailable_io_err(&std::io::Error::from(std::io::ErrorKind::AddrInUse)));
    assert!(!is_loopback_bind_unavailable_io_err(&std::io::Error::from(std::io::ErrorKind::InvalidInput)));
    assert!(is_addr_in_use_serve_err("cannot listen on 127.0.0.1:8080: Address already in use (os error 98)"));
    assert!(!is_addr_in_use_serve_err("invalid token"));
    assert!(!is_addr_in_use_serve_err("cannot listen on 127.0.0.2:0: invalid argument"));
}

#[tokio::test]
async fn listener_cookie_names_are_unique_for_same_port() {
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let name = super::next_listener_cookie_name(8080);
        assert!(name.starts_with("mare_session_8080_"), "unexpected cookie name format: {name}");
        assert!(seen.insert(name), "listener cookie name collided on same port");
    }

    // Sequential same-port listeners on 127.0.0.1 must also issue distinct cookies and reject
    // cross-listener reuse without requiring a second loopback address.
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = Lua::new();
            let no_redirect = crate::http::test_client_no_redirect();
            let mut last_addr_in_use = None;
            for _attempt in 0..3 {
                let s1 = start_server(
                    &lua,
                    r#"{ host = "127.0.0.1", port = 0, token = "tok-one", handler = function() return { status = 200, body = "one" } end }"#,
                )
                .await;
                let a1: String = s1.call_method("addr", ()).unwrap();
                let port: u16 = a1.rsplit(':').next().unwrap().parse().unwrap();

                let r1 = no_redirect
                    .get(format!("http://{a1}/?token=tok-one"))
                    .header("connection", "close")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r1.status(), 303);
                let c1 = r1.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
                s1.call_async_method::<()>("close", ()).await.unwrap();

                let opts2: mlua::Table = lua
                    .load(format!(
                        r#"{{ host = "127.0.0.1", port = {port}, token = "tok-two", handler = function() return {{ status = 200, body = "two" }} end }}"#
                    ))
                    .eval()
                    .unwrap();
                let s2 = match extract_serve_result(super::serve(lua.clone(), opts2).await.unwrap()) {
                    Ok(ud) => ud,
                    Err(err) if is_addr_in_use_serve_err(&err) => {
                        last_addr_in_use = Some(err);
                        continue;
                    }
                    Err(err) => panic!("sequential same-port serve failed unexpectedly on port {port}: {err}"),
                };
                let a2: String = s2.call_method("addr", ()).unwrap();
                let r2 = no_redirect
                    .get(format!("http://{a2}/?token=tok-two"))
                    .header("connection", "close")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r2.status(), 303);
                let c2 = r2.headers().get("set-cookie").unwrap().to_str().unwrap().split(';').next().unwrap().to_string();
                assert_ne!(
                    c1.split('=').next().unwrap(),
                    c2.split('=').next().unwrap(),
                    "sequential same-port listeners must have distinct cookie names"
                );
                // the second listener accepts its own cookie (else rejecting c1 below proves nothing)
                let own = no_redirect
                    .get(format!("http://{a2}/"))
                    .header("cookie", &c2)
                    .header("connection", "close")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(own.status(), 200, "the second listener must accept its own cookie");
                assert_eq!(own.text().await.unwrap(), "two");
                let cross = no_redirect
                    .get(format!("http://{a2}/"))
                    .header("cookie", &c1)
                    .header("connection", "close")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(cross.status(), 401);
                s2.call_async_method::<()>("close", ()).await.unwrap();
                return;
            }
            panic!(
                "sequential same-port rebind exhausted retries due to AddrInUse: {:?}",
                last_addr_in_use
            );
        })
        .await;
}
