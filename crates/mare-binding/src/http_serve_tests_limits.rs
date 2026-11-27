//! Concurrency, connection, body, and limit-option tests.

use super::*;

#[test]
fn parse_limit_large_i64_clamps_safely() {
    let lua = setup_lua();
    let opts = lua.create_table().unwrap();
    opts.set("max_connections", 1i64 << 32).unwrap();
    let res = super::parse_limit(&opts, "max_connections", 256).unwrap().unwrap();
    let expected = usize::try_from(1u64 << 32).unwrap_or(usize::MAX).min(Semaphore::MAX_PERMITS);
    assert_eq!(res, expected);

    opts.set("max_connections", i64::MAX).unwrap();
    let res_max = super::parse_limit(&opts, "max_connections", 256).unwrap().unwrap();
    assert_eq!(res_max, Semaphore::MAX_PERMITS);
}

#[tokio::test]
async fn max_concurrency_and_connections_clamp_and_validate() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (bad_concur, err1): (Option<mlua::AnyUserData>, Option<String>) = lua
                .load("return mare.http.serve { token = false, max_concurrency = 0, handler = function() return nil end }")
                .eval_async()
                .await
                .unwrap();
            assert!(bad_concur.is_none());
            assert!(err1.unwrap().contains("max_concurrency"));

            let (neg_concur, err2): (Option<mlua::AnyUserData>, Option<String>) = lua
                .load("return mare.http.serve { token = false, max_concurrency = -5, handler = function() return nil end }")
                .eval_async()
                .await
                .unwrap();
            assert!(neg_concur.is_none());
            assert!(err2.unwrap().contains("max_concurrency"));

            let (bad_conn, err3): (Option<mlua::AnyUserData>, Option<String>) = lua
                .load("return mare.http.serve { token = false, max_connections = 0, handler = function() return nil end }")
                .eval_async()
                .await
                .unwrap();
            assert!(bad_conn.is_none());
            assert!(err3.unwrap().contains("max_connections"));

            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        max_concurrency = math.maxinteger,
                        max_connections = math.maxinteger,
                        handler = function(_req)
                            return { status = 200, body = "clamped" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let resp = crate::http::test_get(format!("http://{addr}/")).await.unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "clamped");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn bounded_concurrency_enforced() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let active = Arc::new(AtomicUsize::new(0));
            let max_seen = Arc::new(AtomicUsize::new(0));
            let (two_active_tx, two_active_rx) = oneshot::channel::<()>();
            let two_active_tx = Arc::new(std::sync::Mutex::new(Some(two_active_tx)));
            let release = Arc::new(tokio::sync::Notify::new());

            let active_enter = active.clone();
            let max_seen_enter = max_seen.clone();
            let release_enter = release.clone();
            lua.globals()
                .set(
                    "enter_and_wait",
                    lua.create_async_function(move |_, ()| {
                        let active = active_enter.clone();
                        let max_seen = max_seen_enter.clone();
                        let two_tx = two_active_tx.clone();
                        let rel = release_enter.clone();
                        async move {
                            let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                            max_seen.fetch_max(cur, Ordering::SeqCst);
                            if cur == 2
                                && let Some(tx) = two_tx.lock().unwrap().take()
                            {
                                let _ = tx.send(());
                            }
                            rel.notified().await;
                            active.fetch_sub(1, Ordering::SeqCst);
                            Ok(())
                        }
                    })
                    .unwrap(),
                )
                .unwrap();

            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        max_concurrency = 2,
                        handler = function(_req)
                            enter_and_wait()
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
            let mut handles = Vec::new();
            for _ in 0..4 {
                let c = client.clone();
                let url = format!("http://{addr}/");
                handles.push(tokio::task::spawn_local(async move { c.get(url).send().await.unwrap().status() }));
            }

            two_active_rx.await.unwrap();
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(max_seen.load(Ordering::SeqCst) <= 2, "concurrency exceeded cap");

            let finisher = tokio::task::spawn_local(async move {
                for _ in 0..40 {
                    release.notify_waiters();
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });

            for h in handles {
                assert_eq!(h.await.unwrap(), 200);
            }
            finisher.abort();
            assert_eq!(max_seen.load(Ordering::SeqCst), 2);

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_connection_cap_enforced() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        max_connections = 1,
                        handler = function(req)
                            if req.path == "/ws" then
                                local ws, err = req:upgrade()
                                assert(ws, err)
                                ws:send("ready")
                                req.cancel:wait()
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
            let (mut ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);
            let msg = ws.next().await.unwrap().unwrap();
            assert_eq!(msg.to_text().unwrap(), "ready");

            let client = crate::http::test_client();
            let second_req = client.get(format!("http://{addr}/ping")).send();
            let blocked = tokio::time::timeout(Duration::from_millis(150), second_req).await;
            assert!(blocked.is_err(), "second connection should wait while WS holds conn_sem permit");

            ws.close(None).await.unwrap();
            drop(ws);
            let ok = tokio::time::timeout(Duration::from_secs(3), client.get(format!("http://{addr}/ping")).send())
                .await
                .expect("second connection should proceed after WS closes")
                .unwrap();
            assert_eq!(ok.status(), 200);

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn post_expect_100_continue_unread_body_disconnect_cancels_and_releases_permits() {
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
                        max_connections = 1,
                        max_concurrency = 1,
                        close_grace_ms = 80,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            notify_started()
                            req.cancel:wait()
                            notify_cancelled()
                            return nil
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
            stream
                .write_all(
                    b"POST /hang HTTP/1.1\r\nHost: 127.0.0.1\r\nExpect: 100-continue\r\nContent-Length: 10\r\n\r\n",
                )
                .await
                .unwrap();

            started_rx.await.unwrap();
            // Abortive close (RST): a plain FIN is a half-close and does not cancel.
            stream.set_zero_linger().unwrap();
            drop(stream);

            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("req.cancel should be cancelled on Expect: 100-continue disconnect without body read")
                .unwrap();

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("permits should be released after Expect: 100-continue disconnect")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn queued_expect_continue_disconnect_releases_conn_permit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (hold_started_tx, hold_started_rx) = oneshot::channel::<()>();
            let hold_started_tx = Arc::new(std::sync::Mutex::new(Some(hold_started_tx)));
            let hold_release = Arc::new(tokio::sync::Notify::new());
            let hold_release_c = hold_release.clone();

            lua.globals()
                .set(
                    "notify_hold_started",
                    lua.create_function(move |_, ()| {
                        if let Some(tx) = hold_started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.globals()
                .set(
                    "wait_hold_release",
                    lua.create_async_function(move |_, ()| {
                        let n = hold_release_c.clone();
                        async move {
                            n.notified().await;
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
                        max_connections = 2,
                        max_concurrency = 1,
                        handler = function(req)
                            if req.path == "/hold" then
                                notify_hold_started()
                                wait_hold_release()
                                return { status = 200, body = "held" }
                            end
                            return { status = 200, body = "ok" }
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();

            // Connection 1 holds max_concurrency=1 and 1 of 2 connection permits (keep-alive).
            let mut conn1 = TcpStream::connect(&addr).await.unwrap();
            conn1.write_all(format!("GET /hold HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), hold_started_rx).await.unwrap().unwrap();

            // Connection 2 sends Expect: 100-continue and queues for req_sem, then disconnects.
            let mut permit_wait = std::pin::pin!(hooks.on_req_permit_wait.notified());
            permit_wait.as_mut().enable();
            let mut conn2 = TcpStream::connect(&addr).await.unwrap();
            conn2
                .write_all(
                    format!(
                        "POST /queued HTTP/1.1\r\nHost: {addr}\r\nExpect: 100-continue\r\nContent-Length: 100\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), permit_wait)
                .await
                .expect("queued request must enter req_sem permit wait");
            // Abortive close (RST): a plain FIN is a half-close and does not cancel.
            conn2.set_zero_linger().unwrap();
            drop(conn2);

            // While conn1 STILL holds req_sem (and 1 of 2 connection permits), conn3 must be
            // accepted and enter req_sem permit wait, proving conn2 released its connection permit.
            let mut permit_wait3 = std::pin::pin!(hooks.on_req_permit_wait.notified());
            permit_wait3.as_mut().enable();
            let mut conn3 = TcpStream::connect(&addr).await.unwrap();
            conn3.write_all(format!("GET /ping HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes()).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), permit_wait3)
                .await
                .expect("conn3 must be accepted and enter req_sem permit wait before conn1 releases");

            // Now release conn1's request while keeping conn1's keep-alive TCP socket open.
            hold_release.notify_one();
            let mut buf = [0u8; 256];
            let n = tokio::time::timeout(Duration::from_secs(3), tokio::io::AsyncReadExt::read(&mut conn1, &mut buf))
                .await
                .unwrap()
                .unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("held"));

            let mut buf3 = [0u8; 256];
            let n3 = tokio::time::timeout(Duration::from_secs(3), tokio::io::AsyncReadExt::read(&mut conn3, &mut buf3))
                .await
                .unwrap()
                .unwrap();
            let resp3 = String::from_utf8_lossy(&buf3[..n3]);
            assert!(resp3.contains("200 OK"), "expected 200 OK for conn3, got: {resp3}");
            assert!(resp3.contains("ok"), "expected ok body for conn3, got: {resp3}");

            drop(conn3);
            drop(conn1);
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn unread_body_disconnect_behind_full_window_cancels_via_body_stall() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));
            let (cancelled_tx, cancelled_rx) = oneshot::channel::<()>();
            let cancelled_tx = Arc::new(std::sync::Mutex::new(Some(cancelled_tx)));

            lua.globals()
                .set(
                    "notify_req_started",
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
                    "notify_req_cancelled",
                    lua.create_function(move |_, ()| {
                        if let Some(tx) = cancelled_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                r#"
                    {
                        max_connections = 1,
                        max_concurrency = 1,
                        -- 128 KiB can fill the server receive window, so the client FIN may never
                        -- arrive; the body stall timeout is the bounded fallback.
                        body_stall_ms = 100,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            notify_req_started()
                            req.cancel:wait()
                            notify_req_cancelled()
                            return { status = 499, body = "cancelled" }
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stalled_notified = std::pin::pin!(hooks.on_duplex_stalled.notified());
            stalled_notified.as_mut().enable();

            let mut stream = TcpStream::connect(&addr).await.unwrap();
            let body = vec![b'a'; 128 * 1024];
            let header = format!("POST /wait HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.flush().await.unwrap();

            tokio::time::timeout(Duration::from_secs(3), started_rx).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(3), stalled_notified)
                .await
                .expect("64 KiB duplex must saturate before disconnect");
            drop(stream);

            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("unread >64KiB body disconnect must cancel req.cancel")
                .unwrap();

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("permits must be released after unread >64KiB body disconnect")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn handler_ignoring_cancel_aborted_on_disconnect_releases_permits() {
    use tokio::io::AsyncWriteExt;

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
            lua.globals()
                .set(
                    "hang_forever",
                    lua.create_async_function(|_, ()| async move {
                        std::future::pending::<()>().await;
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        max_connections = 1,
                        max_concurrency = 1,
                        close_grace_ms = 80,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            notify_started()
                            hang_forever()
                            return { status = 200, body = "unreachable" }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
            stream.write_all(b"GET /stuck HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();

            started_rx.await.unwrap();
            // Abortive close (RST): a plain FIN is a half-close and does not cancel.
            stream.set_zero_linger().unwrap();
            drop(stream);

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("handler ignoring req.cancel should be aborted on disconnect, releasing permits")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn stalled_request_body_closes_connection_when_client_unread_large_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));

            lua.globals()
                .set(
                    "notify_stall_started",
                    lua.create_function(move |_, ()| {
                        if let Some(tx) = started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                r#"
                    {
                        max_connections = 1,
                        max_concurrency = 1,
                        body_stall_ms = 100,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            notify_stall_started()
                            req.cancel:wait()
                            return { status = 200, body = string.rep("x", 8 * 1024 * 1024) }
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stalled_notified = std::pin::pin!(hooks.on_duplex_stalled.notified());
            stalled_notified.as_mut().enable();

            // Client sends >64 KiB body, keeps TCP socket open, and never reads the response.
            let mut stream = TcpStream::connect(&addr).await.unwrap();
            let body = vec![b'a'; 128 * 1024];
            let header = format!("POST /stall HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.flush().await.unwrap();

            tokio::time::timeout(Duration::from_secs(3), started_rx).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(3), stalled_notified).await.expect("64 KiB duplex must saturate");

            // Connection task must close the HTTP connection on conn_cancel and release permits
            // even while the client keeps `stream` open and unread.
            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("permits must be released even when client never reads large response")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            drop(stream);
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_limits_trusted_hosts_accept_partial_and_bind_errors_return_nil_err() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            for (expr, needle) in [
                (
                    "return mare.http.serve { token = false, ws_max_message_bytes = 0, handler = function() return nil end }",
                    "ws_max_message_bytes",
                ),
                (
                    "return mare.http.serve { token = false, ws_max_frame_bytes = 0, handler = function() return nil end }",
                    "ws_max_frame_bytes",
                ),
                (
                    "return mare.http.serve { token = false, trusted_hosts = { 'bad host' }, handler = function() return nil end }",
                    "trusted_hosts",
                ),
                (
                    "return mare.http.serve { token = false, accept_partial = { 'bogus_cap' }, handler = function() return nil end }",
                    "bogus_cap",
                ),
            ] {
                let (srv, err): (Option<mlua::AnyUserData>, Option<String>) =
                    lua.load(expr).eval_async().await.unwrap();
                assert!(srv.is_none(), "expected nil server for {expr}");
                let err = err.expect("expected error string");
                assert!(err.contains(needle), "expected {needle:?} in {err:?}");
            }

            // Occupy an ephemeral loopback port so mare.http.serve fails at bind.
            let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = occupied.local_addr().unwrap().port();
            let (srv, err): (Option<mlua::AnyUserData>, Option<String>) = lua
                .load(format!(
                    "return mare.http.serve {{ host = '127.0.0.1', port = {port}, token = false, handler = function() return nil end }}"
                ))
                .eval_async()
                .await
                .unwrap();
            assert!(srv.is_none(), "expected bind failure when port is already bound");
            assert!(err.is_some());
            drop(occupied);
        })
        .await;
}

#[tokio::test]
async fn duplicate_and_non_utf8_host_headers_are_rejected() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load("return assert(mare.http.serve { token = false, handler = function() return { status = 200 } end })")
                .eval_async()
                .await
                .unwrap();
            let addr: String = srv.call_method("addr", ()).unwrap();

            let mut dup_stream = TcpStream::connect(&addr).await.unwrap();
            let dup_req = format!("GET / HTTP/1.1\r\nHost: {addr}\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
            dup_stream.write_all(dup_req.as_bytes()).await.unwrap();
            let mut dup_resp = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut dup_stream, &mut dup_resp).await.unwrap();
            assert!(dup_resp.starts_with("HTTP/1.1 400"), "expected 400 for duplicate Host: {dup_resp:?}");
            assert!(dup_resp.contains("duplicate Host header"), "unexpected body: {dup_resp:?}");

            let mut bad_stream = TcpStream::connect(&addr).await.unwrap();
            let mut bad_req = b"GET / HTTP/1.1\r\nHost: ".to_vec();
            bad_req.extend_from_slice(&[0xff, 0xfe]);
            bad_req.extend_from_slice(b"\r\nConnection: close\r\n\r\n");
            bad_stream.write_all(&bad_req).await.unwrap();
            let mut bad_resp = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut bad_stream, &mut bad_resp).await.unwrap();
            assert!(bad_resp.starts_with("HTTP/1.1 403"), "expected 403 for non-UTF-8 Host: {bad_resp:?}");
            assert!(bad_resp.contains("invalid Host header"), "unexpected body: {bad_resp:?}");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}
