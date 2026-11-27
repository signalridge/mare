//! Plain HTTP request/response (incl. JSON bodies), loopback/Host validation, auth, and request cancel tests.

use super::*;

#[test]
fn is_loopback_host_checks() {
    assert!(is_loopback_host("127.0.0.1"));
    assert!(is_loopback_host("127.0.1.1"));
    assert!(is_loopback_host("::1"));
    assert!(is_loopback_host("[::1]"));
    assert!(is_loopback_host("localhost"));
    assert!(is_loopback_host("LOCALHOST"));

    assert!(!is_loopback_host("0.0.0.0"));
    assert!(!is_loopback_host("::"));
    assert!(!is_loopback_host("192.168.1.10"));
    assert!(!is_loopback_host("10.0.0.1"));
    assert!(!is_loopback_host("example.com"));
    assert!(!is_loopback_host(""));
}

#[test]
fn is_valid_loopback_host_header_checks() {
    assert!(is_valid_loopback_host_header("localhost"));
    assert!(is_valid_loopback_host_header("LOCALHOST:3000"));
    assert!(is_valid_loopback_host_header("127.0.0.1"));
    assert!(is_valid_loopback_host_header("127.0.0.1:8080"));
    assert!(is_valid_loopback_host_header("127.0.1.1:8080"));
    assert!(is_valid_loopback_host_header("[::1]"));
    assert!(is_valid_loopback_host_header("[::1]:8080"));

    assert!(!is_valid_loopback_host_header(""));
    assert!(!is_valid_loopback_host_header("evil.com"));
    assert!(!is_valid_loopback_host_header("evil.com:8080"));
    assert!(!is_valid_loopback_host_header("localhost.evil.com:8080"));
    assert!(!is_valid_loopback_host_header("192.168.1.1:8080"));
    assert!(!is_valid_loopback_host_header("0.0.0.0:8080"));
    assert!(!is_valid_loopback_host_header("[::]:8080"));
    assert!(!is_valid_loopback_host_header("localhost:"));
    assert!(!is_valid_loopback_host_header("localhost:abc"));
    assert!(!is_valid_loopback_host_header("[localhost]"));
}

#[test]
fn is_loopback_ip_checks() {
    assert!(is_loopback_ip("127.0.0.1".parse().unwrap()));
    assert!(is_loopback_ip("::1".parse().unwrap()));
    assert!(is_loopback_ip("::ffff:127.0.0.1".parse().unwrap()));
    assert!(!is_loopback_ip("0.0.0.0".parse().unwrap()));
    assert!(!is_loopback_ip("192.168.1.1".parse().unwrap()));
    assert!(!is_loopback_ip("::".parse().unwrap()));
}

#[tokio::test]
async fn loopback_http_request_and_server_close() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            local body = req:body()
                            return {
                                status = 201,
                                headers = { ["x-reply"] = "ok", ["content-type"] = "text/plain" },
                                body = req.method .. ":" .. req.path .. ":" .. (req.query.foo or "") .. ":" .. (req.headers["x-test"] or "") .. ":" .. body,
                            }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            assert!(addr.starts_with("127.0.0.1:"), "expected loopback addr, got {addr}");

            let client = crate::http::test_client();
            let resp = client
                .post(format!("http://{addr}/hello?foo=bar"))
                .header("x-test", "v1")
                .body("payload")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 201);
            assert_eq!(resp.headers().get("x-reply").unwrap(), "ok");
            let text = resp.text().await.unwrap();
            assert_eq!(text, "POST:/hello:bar:v1:payload");

            srv.call_async_method::<()>("close", ()).await.unwrap();
            // Listener is dropped before close() returns: connecting must fail immediately.
            assert!(tokio::net::TcpStream::connect(&addr).await.is_err());
        })
        .await;
}

#[tokio::test]
async fn non_loopback_rejected_unconditionally() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            for host in ["0.0.0.0", "192.168.1.10", "::"] {
                let err: String = lua
                    .load(format!(
                        r#"
                        local srv, err = mare.http.serve {{ token = false,
                            host = "{host}",
                            handler = function() return {{ status = 200 }} end
                        }}
                        assert(srv == nil)
                        return err
                    "#
                    ))
                    .eval_async()
                    .await
                    .unwrap();
                assert!(err.contains("loopback"), "expected loopback error for {host}, got: {err}");
            }
        })
        .await;
}

#[tokio::test]
async fn host_header_dns_rebinding_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(_req)
                            return { status = 200, body = "ok" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();

            async fn raw_request(addr: &str, req_bytes: &[u8]) -> String {
                let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                stream.write_all(req_bytes).await.unwrap();
                let mut buf = Vec::new();
                stream.read_to_end(&mut buf).await.unwrap();
                String::from_utf8_lossy(&buf).into_owned()
            }

            let r_evil =
                raw_request(&addr, b"GET / HTTP/1.1\r\nHost: evil.com:8080\r\nConnection: close\r\n\r\n").await;
            assert!(r_evil.starts_with("HTTP/1.1 403"), "expected 403, got: {r_evil}");

            let r_sub =
                raw_request(&addr, b"GET / HTTP/1.1\r\nHost: localhost.evil.com\r\nConnection: close\r\n\r\n").await;
            assert!(r_sub.starts_with("HTTP/1.1 403"), "expected 403, got: {r_sub}");

            let r_missing = raw_request(&addr, b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n").await;
            assert!(r_missing.starts_with("HTTP/1.1 403"), "expected 403, got: {r_missing}");

            let r_ok = raw_request(&addr, b"GET / HTTP/1.1\r\nHost: localhost:9999\r\nConnection: close\r\n\r\n").await;
            assert!(r_ok.starts_with("HTTP/1.1 200"), "expected 200, got: {r_ok}");
            assert!(r_ok.ends_with("ok"));

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn non_loopback_bound_addr_rejected() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let token_dir = tempfile::tempdir().unwrap();
            let token_path = token_dir.path().join("token");
            let opts: mlua::Table = lua
                .load(
                    r#"
                    return {
                        host = "localhost",
                        handler = function() return { status = 200, body = "ok" } end
                    }
                "#,
                )
                .eval()
                .unwrap();
            if !crate::ossandbox::protected::host_protect_available() {
                let accepted = lua.create_sequence_from(["fs_host_protect"]).unwrap();
                opts.set("accept_partial", accepted).unwrap();
            }
            let mv = super::super::serve_with_binder_and_hooks(
                lua.clone(),
                opts,
                |_host, _port| async move {
                    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                    let non_loopback: SocketAddr = "192.0.2.1:8080".parse().unwrap();
                    Ok((listener, non_loopback))
                },
                token_path.clone(),
                None,
            )
            .await
            .unwrap();
            assert!(token_path.is_file(), "the default token is still created in the isolated directory");
            let vals: Vec<mlua::Value> = mv.into_iter().collect();
            assert_eq!(vals.len(), 2);
            assert!(matches!(vals[0], mlua::Value::Nil));
            let err = match &vals[1] {
                mlua::Value::String(s) => s.to_str().unwrap().to_string(),
                other => panic!("expected error string, got {other:?}"),
            };
            assert!(err.contains("non-loopback address 192.0.2.1"), "unexpected err: {err}");
        })
        .await;
}

#[cfg(unix)]
#[test]
fn non_loopback_test_isolates_default_token_from_home() {
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "http_serve::tests::plain::non_loopback_bound_addr_rejected", "--nocapture"])
        .env("HOME", home.path())
        .env_remove("MARE_HOME")
        .env_remove("XDG_STATE_HOME")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child test failed: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"), "child test did not run");
    let state = home.path().join(".local/state/mare");
    assert!(!state.exists(), "test wrote to the default HOME state directory: {}", state.display());
}

#[tokio::test]
async fn localhost_bind_verifies_loopback_addr() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        host = "localhost",
                        handler = function() return { status = 200, body = "ok" } end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();
            let addr_str: String = srv.call_method("addr", ()).unwrap();
            let addr: SocketAddr = addr_str.parse().unwrap();
            assert!(is_loopback_ip(addr.ip()), "expected loopback addr, got {addr}");
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn token_auth_enforced_on_requests() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve {
                        token = "secret-123",
                        handler = function(_req)
                            return { status = 200, body = "authorized" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let client = crate::http::test_client();

            let r1 = client.get(format!("http://{addr}/")).send().await.unwrap();
            assert_eq!(r1.status(), 401);

            let r2 =
                client.get(format!("http://{addr}/")).header("authorization", "Bearer wrong").send().await.unwrap();
            assert_eq!(r2.status(), 401);

            let r3 = client
                .get(format!("http://{addr}/"))
                .header("authorization", "Bearer secret-123")
                .send()
                .await
                .unwrap();
            assert_eq!(r3.status(), 200);
            assert_eq!(r3.text().await.unwrap(), "authorized");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn close_cancels_active_connections_and_handlers() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
            let notify_fn = lua
                .create_function(move |_, ()| {
                    if let Some(tx) = started_tx.lock().unwrap().take() {
                        let _ = tx.send(());
                    }
                    Ok(())
                })
                .unwrap();
            lua.globals().set("notify_started", notify_fn).unwrap();

            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            notify_started()
                            -- Block indefinitely until cancelled or aborted by server:close()
                            req.cancel:wait()
                            return { status = 200, body = "done" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let client_task = tokio::task::spawn_local(async move {
                let client = crate::http::test_client();
                client.get(format!("http://{addr}/hang")).send().await
            });

            // Deterministic wait until the handler is actively running in-flight.
            started_rx.await.unwrap();

            // close() must cancel the in-flight handler and terminate the active connection.
            srv.call_async_method::<()>("close", ()).await.unwrap();

            let res = client_task.await.unwrap();
            if let Ok(resp) = res {
                assert_eq!(resp.status(), 503);
            }
        })
        .await;
}

#[tokio::test]
async fn client_disconnect_cancels_request_token() {
    use tokio::io::AsyncWriteExt;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let (cancelled_tx, cancelled_rx) = oneshot::channel::<()>();
            let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
            let cancelled_tx = Arc::new(std::sync::Mutex::new(Some(cancelled_tx)));

            lua.globals()
                .set(
                    "notify_started",
                    lua.create_function(move |_, ()| {
                        if let Some(tx) = started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.globals()
                .set(
                    "notify_cancelled",
                    lua.create_function(move |_, ()| {
                        if let Some(tx) = cancelled_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            notify_started()
                            req.cancel:wait()
                            notify_cancelled()
                            return { status = 200, body = "done" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
            stream.write_all(b"GET /hang HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await.unwrap();

            started_rx.await.unwrap();
            // Abortive close (RST): a plain FIN is a half-close and does not cancel.
            stream.set_zero_linger().unwrap();
            drop(stream);

            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("req.cancel should be cancelled on client disconnect")
                .unwrap();

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn plain_http_response_cancel_reflects_subsequent_connection_close() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (cancelled_tx, mut cancelled_rx) = oneshot::channel::<()>();
            let cancelled_tx = Arc::new(std::sync::Mutex::new(Some(cancelled_tx)));
            lua.globals()
                .set(
                    "watch_req_cancel",
                    lua.create_function(move |_, tok: mlua::AnyUserData| {
                        let t = tok.borrow::<crate::task::Token>()?.0.clone();
                        let tx_arc = cancelled_tx.clone();
                        tokio::task::spawn_local(async move {
                            t.cancelled().await;
                            if let Some(tx) = tx_arc.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                        });
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            watch_req_cancel(req.cancel)
                            return { status = 200, body = "ok" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
            stream.write_all(b"GET /bg HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();

            let mut buf = vec![0u8; 512];
            let mut acc = Vec::new();
            loop {
                let n = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await.unwrap().unwrap();
                assert!(n > 0);
                acc.extend_from_slice(&buf[..n]);
                if acc.ends_with(b"\r\n\r\nok") {
                    break;
                }
            }
            tokio::task::yield_now().await;
            assert!(cancelled_rx.try_recv().is_err(), "req.cancel must stay live while keep-alive conn is open");

            drop(stream);
            tokio::time::timeout(Duration::from_secs(3), &mut cancelled_rx)
                .await
                .expect("req.cancel should be cancelled when keep-alive connection closes")
                .unwrap();

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn keepalive_requests_do_not_accumulate_tasks() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            return { status = 200, body = "ok" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stream = TcpStream::connect(&addr).await.unwrap();
            let req = format!("GET /ok HTTP/1.1\r\nHost: {addr}\r\n\r\n");
            let mut buf = [0u8; 512];
            for _ in 0..25 {
                stream.write_all(req.as_bytes()).await.unwrap();
                let n = stream.read(&mut buf).await.unwrap();
                assert!(String::from_utf8_lossy(&buf[..n]).contains("200 OK"));
            }

            for _ in 0..5 {
                tokio::task::yield_now().await;
            }
            let unfinished = srv.borrow::<HttpServer>().unwrap().tasks.unfinished_count();
            assert!(unfinished <= 1, "keep-alive requests must not accumulate watcher tasks, got {unfinished}");

            drop(stream);
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

// On Windows, `tcpip.sys` keeps a half-closed loopback socket (`FIN_WAIT_2`) in `CLOSE_WAIT` on
// the server and emits no RST on `SO_LINGER(1, 0)` close until the server writes a response byte;
// `windows_peer_reset_detects_tcpstate_closed_after_half_close` verifies that post-write RST path.
#[cfg(unix)]
#[tokio::test]
async fn reset_after_half_close_cancels_and_releases_permits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let mut notifiers = Vec::new();
            for name in ["notify_started", "notify_cancelled"] {
                let (tx, rx) = oneshot::channel::<()>();
                let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
                lua.globals()
                    .set(
                        name,
                        lua.create_function(move |_, ()| {
                            if let Some(tx) = tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            Ok(())
                        })
                        .unwrap(),
                    )
                    .unwrap();
                notifiers.push(rx);
            }
            let mut cancelled_rx = notifiers.pop().unwrap();
            let started_rx = notifiers.pop().unwrap();

            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                r#"
                    {
                        max_connections = 1,
                        max_concurrency = 1,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            notify_started()
                            req.cancel:wait()
                            notify_cancelled()
                            return { status = 499, body = "cancelled" }
                        end
                    }
                "#,
            )
            .await;
            let addr: String = srv.call_method("addr", ()).unwrap();

            let mut stream = TcpStream::connect(&addr).await.unwrap();
            stream.write_all(b"GET /wait HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), started_rx).await.unwrap().unwrap();

            // Half-close; sync on the server pump actually reading the FIN.
            let mut eof_read = std::pin::pin!(hooks.on_peer_eof_read.notified());
            eof_read.as_mut().enable();
            stream.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), eof_read).await.expect("server must read the FIN");
            assert!(cancelled_rx.try_recv().is_err(), "a half-close alone must not cancel");

            // Then an abortive close (RST): no body backpressure, so no stall timer is involved.
            let mut reset_seen = std::pin::pin!(hooks.on_half_closed_reset.notified());
            reset_seen.as_mut().enable();
            stream.set_zero_linger().unwrap();
            drop(stream);
            tokio::time::timeout(Duration::from_secs(3), reset_seen)
                .await
                .expect("half-close watcher must observe the reset");
            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("RST after half-close must cancel req.cancel")
                .unwrap();
            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("connection and request permits must be released")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[cfg(windows)]
#[tokio::test]
async fn windows_peer_reset_detects_tcpstate_closed_after_half_close() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    client.write_all(b"GET /wait HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
    client.shutdown().await.unwrap();

    let mut req_buf = Vec::new();
    server.read_to_end(&mut req_buf).await.unwrap();
    assert!(req_buf.starts_with(b"GET /wait HTTP/1.1"));

    let (read_half, mut write_half) = server.into_split();
    assert!(!super::super::stream::windows_peer_reset(read_half.as_ref()));

    client.set_zero_linger().unwrap();
    drop(client);
    let _ = write_half.write_all(b"x").await;

    let res =
        tokio::time::timeout(Duration::from_secs(3), super::super::stream::peer_gone(&read_half, true, None)).await;
    assert!(matches!(res, Ok(super::super::stream::PeerGone::Reset)));
}

#[tokio::test]
async fn fin_only_half_close_keeps_request_alive_and_response_readable() {
    use tokio::io::AsyncReadExt;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
            lua.globals()
                .set(
                    "notify_started",
                    lua.create_function(move |_, ()| {
                        if let Some(tx) = started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
            lua.globals()
                .set(
                    "wait_release",
                    lua.create_async_function(move |_, ()| {
                        let rx = release_rx.clone();
                        async move {
                            if let Some(rx) = rx.lock().await.take() {
                                let _ = rx.await;
                            }
                            Ok(())
                        }
                    })
                    .unwrap(),
                )
                .unwrap();

            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                r#"
                    {
                        handler = function(req)
                            notify_started()
                            wait_release()
                            -- Reported from inside the handler, after the server observed the FIN.
                            return { status = 200, body = "done:" .. tostring(req.cancel:is_cancelled()) }
                        end
                    }
                "#,
            )
            .await;
            let addr: String = srv.call_method("addr", ()).unwrap();

            let mut stream = TcpStream::connect(&addr).await.unwrap();
            stream.write_all(b"GET /wait HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), started_rx).await.unwrap().unwrap();

            // Sync points: the pump read the FIN, then the half-close watcher probed the socket
            // and deliberately kept the request alive.
            let mut eof_read = std::pin::pin!(hooks.on_peer_eof_read.notified());
            eof_read.as_mut().enable();
            let mut fin_ignored = std::pin::pin!(hooks.on_fin_ignored.notified());
            fin_ignored.as_mut().enable();
            stream.shutdown().await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), eof_read).await.expect("server must read the FIN");
            tokio::time::timeout(Duration::from_secs(3), fin_ignored)
                .await
                .expect("half-close watcher must probe the FIN and keep the request");

            // Client stays open (read side) and receives the full response.
            let _ = release_tx.send(());
            let mut resp = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut resp))
                .await
                .expect("full response after FIN-only half-close")
                .unwrap();
            let resp = String::from_utf8_lossy(&resp);
            assert!(resp.starts_with("HTTP/1.1 200"), "unexpected response: {resp}");
            assert!(resp.ends_with("done:false"), "req.cancel must not fire on a FIN-only half-close: {resp}");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn http_request_json_preserves_null_in_objects_and_arrays() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            local data, err = req:json()
                            assert(data ~= nil, tostring(err))
                            local obj_has_x = rawget(data, "x") == mare.json.null
                            local arr_len = #data.arr
                            local arr_0_null = data.arr[1] == mare.json.null
                            local arr_1_two = data.arr[2] == 2
                            local arr_2_null = data.arr[3] == mare.json.null
                            return {
                                status = 200,
                                body = (obj_has_x and arr_len == 3 and arr_0_null and arr_1_two and arr_2_null)
                                    and "ok" or "mismatch",
                            }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let client = crate::http::test_client();
            let resp = client
                .post(format!("http://{addr}/"))
                .header("content-type", "application/json")
                .body(r#"{"x":null,"arr":[null,2,null]}"#)
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}
