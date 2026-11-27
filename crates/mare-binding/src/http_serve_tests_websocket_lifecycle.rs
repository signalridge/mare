//! WebSocket disconnect/cancel/permit-release lifecycle tests.

use super::*;

#[tokio::test]
async fn ws_direct_tcp_drop_releases_cancel_and_permit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (cancelled_tx, cancelled_rx) = oneshot::channel::<()>();
            let cancelled_tx = Arc::new(std::sync::Mutex::new(Some(cancelled_tx)));
            lua.globals()
                .set(
                    "watch_ws_cancel",
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
                        max_connections = 1,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            watch_ws_cancel(req.cancel)
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            req.cancel:wait()
                            return nil
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
            ws.send(ClientWsMsg::Text("hello".into())).await.unwrap();

            // Direct TCP drop without Close frame.
            drop(ws);

            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("WS req.cancel should be cancelled promptly on direct TCP drop")
                .unwrap();

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("conn_sem permit should be released promptly on direct TCP drop")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_outbound_blocked_send_disconnect_releases_cancel_and_permit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (cancelled_tx, mut cancelled_rx) = oneshot::channel::<()>();
            let cancelled_tx = Arc::new(std::sync::Mutex::new(Some(cancelled_tx)));
            lua.globals()
                .set(
                    "watch_ws_cancel",
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

            let (start_tx, start_rx) = oneshot::channel::<()>();
            let start_rx = Arc::new(AsyncMutex::new(Some(start_rx)));
            lua.globals()
                .set(
                    "wait_ws_start",
                    lua.create_async_function(move |_, ()| {
                        let rx_arc = start_rx.clone();
                        async move {
                            if let Some(rx) = rx_arc.lock().await.take() {
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
                    return {
                        max_connections = 1,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            watch_ws_cancel(req.cancel)
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            wait_ws_start()
                            pcall(function() ws:send("hello") end)
                            req.cancel:wait()
                            return nil
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut blocked = std::pin::pin!(hooks.on_ws_write_pending.notified());
            blocked.as_mut().enable();
            let (ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);
            hooks.block_ws_writes.store(true, Ordering::Release);
            let _ = start_tx.send(());

            // Controllable IO returns Poll::Pending in poll_write, proving sink.send is actually blocked.
            tokio::time::timeout(Duration::from_secs(3), blocked)
                .await
                .expect("server sink.send should block in poll_write deterministically");
            assert!(cancelled_rx.try_recv().is_err(), "WS should still be open while outbound is blocked");

            // Direct TCP drop while sink.send is blocked.
            drop(ws);

            tokio::time::timeout(Duration::from_secs(3), &mut cancelled_rx)
                .await
                .expect("WS req.cancel should be cancelled promptly after outbound-blocked disconnect")
                .unwrap();

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("conn_sem permit should be released promptly after outbound-blocked disconnect")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_outbound_blocked_send_cancel_releases_permit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let captured_tok = Arc::new(std::sync::Mutex::new(None::<tokio_util::sync::CancellationToken>));
            let captured_tok_lua = captured_tok.clone();
            lua.globals()
                .set(
                    "capture_ws_cancel",
                    lua.create_function(move |_, tok: mlua::AnyUserData| {
                        let t = tok.borrow::<crate::task::Token>()?.0.clone();
                        *captured_tok_lua.lock().unwrap() = Some(t);
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let (start_tx, start_rx) = oneshot::channel::<()>();
            let start_rx = Arc::new(AsyncMutex::new(Some(start_rx)));
            lua.globals()
                .set(
                    "wait_ws_start",
                    lua.create_async_function(move |_, ()| {
                        let rx_arc = start_rx.clone();
                        async move {
                            if let Some(rx) = rx_arc.lock().await.take() {
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
                    return {
                        max_connections = 1,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            capture_ws_cancel(req.cancel)
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            wait_ws_start()
                            pcall(function() ws:send("hello") end)
                            return nil
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut blocked = std::pin::pin!(hooks.on_ws_write_pending.notified());
            blocked.as_mut().enable();
            let (_ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);
            hooks.block_ws_writes.store(true, Ordering::Release);
            let _ = start_tx.send(());

            // Wait until sink.send actually returns Poll::Pending in poll_write.
            tokio::time::timeout(Duration::from_secs(3), blocked)
                .await
                .expect("server sink.send should block in poll_write deterministically");

            // Cancel req.cancel while sink.send is blocked; w_handle must unblock and drop permit.
            let tok = captured_tok.lock().unwrap().take().expect("cancel token captured");
            tok.cancel();

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("conn_sem permit should be released promptly after cancelling outbound-blocked WS")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_handler_ignoring_cancel_aborted_on_disconnect_releases_permits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (started_tx, started_rx) = oneshot::channel::<()>();
            let started_tx = Arc::new(std::sync::Mutex::new(Some(started_tx)));

            lua.globals()
                .set(
                    "notify_ws_started",
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
                    "hang_ws_forever",
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
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            notify_ws_started()
                            hang_ws_forever()
                            return nil
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

            started_rx.await.unwrap();
            ws.send(ClientWsMsg::Close(None)).await.unwrap();
            drop(ws);

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("stuck WS handler ignoring cancel should be aborted on disconnect, releasing permits")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_close_with_idle_peer_read_half_releases_pump_and_socket() {
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
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            ws:recv()
                            ws:close()
                            return nil
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let stream = TcpStream::connect(&addr).await.unwrap();
            let (mut ws, resp) = tokio_tungstenite::client_async(format!("ws://{addr}/ws"), stream).await.unwrap();
            assert_eq!(resp.status(), 101);

            ws.send(ClientWsMsg::Close(None)).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if matches!(msg, ClientWsMsg::Close(_)) {
                    break;
                }
            }

            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            let unfinished = srv.borrow::<HttpServer>().unwrap().tasks.unfinished_count();
            assert_eq!(
                unfinished, 0,
                "pump_handle and WS tasks must all exit even while peer TCP write half stays idle"
            );

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("next connection must succeed")
                    .unwrap();
            assert_eq!(resp.status(), 200);

            drop(ws);
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn failed_upgrade_cancels_tokens_and_releases_permits() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let tokens = Arc::new(std::sync::Mutex::new(Vec::<tokio_util::sync::CancellationToken>::new()));
            let tokens_lua = tokens.clone();
            let (ws_entered_tx, ws_entered_rx) = oneshot::channel::<()>();
            let ws_entered_tx = Arc::new(std::sync::Mutex::new(Some(ws_entered_tx)));
            let (upgrade_proceed_tx, upgrade_proceed_rx) = oneshot::channel::<()>();
            let upgrade_proceed_rx = Arc::new(AsyncMutex::new(Some(upgrade_proceed_rx)));

            lua.globals()
                .set(
                    "record_cancel_tok",
                    lua.create_function(move |_, tok: mlua::AnyUserData| {
                        tokens_lua.lock().unwrap().push(tok.borrow::<crate::task::Token>()?.0.clone());
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.globals()
                .set(
                    "wait_upgrade_gate",
                    lua.create_async_function(move |_, ()| {
                        let entered = ws_entered_tx.clone();
                        let proceed = upgrade_proceed_rx.clone();
                        async move {
                            if let Some(tx) = entered.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            if let Some(rx) = proceed.lock().await.take() {
                                let _ = rx.await;
                            }
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
                        max_connections = 1,
                        max_concurrency = 1,
                        close_grace_ms = 80,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            record_cancel_tok(req.cancel)
                            if req.path == "/http" then
                                return { status = 200, body = "http-ok" }
                            end
                            wait_upgrade_gate()
                            local ws, err = req:upgrade()
                            if not ws then
                                req.cancel:wait()
                                return nil
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
            let mut stream = TcpStream::connect(&addr).await.unwrap();
            let http_req = format!("GET /http HTTP/1.1\r\nHost: {addr}\r\n\r\n");
            stream.write_all(http_req.as_bytes()).await.unwrap();
            let mut buf = [0u8; 512];
            let n = stream.read(&mut buf).await.unwrap();
            assert!(String::from_utf8_lossy(&buf[..n]).contains("http-ok"));

            let ws_req = format!(
                "GET /ws HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
            );
            stream.write_all(ws_req.as_bytes()).await.unwrap();
            ws_entered_rx.await.unwrap();
            drop(stream);
            let _ = upgrade_proceed_tx.send(());

            let resp = tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                .await
                .expect("failed upgrade must release req_sem and conn_sem permits")
                .unwrap();
            assert_eq!(resp.status(), 200);

            let recorded = tokens.lock().unwrap().clone();
            assert_eq!(recorded.len(), 2);
            for (i, tok) in recorded.iter().enumerate() {
                assert!(tok.is_cancelled(), "token {i} on failed-upgrade connection must be cancelled");
            }

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn close_grace_ms_validation_and_timeout_aborts_stuck_ws_handler() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (srv_bad, err): (Option<mlua::AnyUserData>, Option<String>) = lua
                .load(r#"return mare.http.serve { token = false, close_grace_ms = -1, handler = function() return nil end }"#)
                .eval_async()
                .await
                .unwrap();
            assert!(srv_bad.is_none());
            assert!(err.as_deref().unwrap_or("").contains("close_grace_ms"));

            let (frame_rx_tx, frame_rx_rx) = oneshot::channel::<()>();
            let frame_rx_tx = Arc::new(std::sync::Mutex::new(Some(frame_rx_tx)));
            lua.globals()
                .set(
                    "notify_ws_frame_received",
                    lua.create_function(move |_, ()| {
                        if let Some(tx) = frame_rx_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.globals()
                .set(
                    "hang_after_frame",
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
                        close_grace_ms = 60,
                        handler = function(req)
                            if req.path == "/ping" then
                                return { status = 200, body = "ok" }
                            end
                            local ws, uerr = req:upgrade()
                            assert(ws, uerr)
                            local msg = ws:recv()
                            assert(msg == "hello")
                            notify_ws_frame_received()
                            hang_after_frame()
                            return nil
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
            ws.send(ClientWsMsg::Text("hello".into())).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), frame_rx_rx).await.unwrap().unwrap();

            ws.send(ClientWsMsg::Close(None)).await.unwrap();
            drop(ws);

            let ping =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("stuck WS handler must be aborted when close_grace_ms expires")
                    .unwrap();
            assert_eq!(ping.status(), 200);
            assert_eq!(ping.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_dropping_all_handles_lets_read_and_write_tasks_finish() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (server_io, client_io) = tokio::io::duplex(64 * 1024);
            let server_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                server_io,
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            )
            .await;
            // Keep the client side open and idle so the read task stays parked on `stream.next()`.
            let _client = tokio_tungstenite::WebSocketStream::from_raw_socket(
                client_io,
                tokio_tungstenite::tungstenite::protocol::Role::Client,
                None,
            )
            .await;

            let sem = Arc::new(Semaphore::new(1));
            let conn_permit = Arc::new(sem.clone().acquire_owned().await.unwrap());
            let conn_cancel = CancellationToken::new();
            let req_cancel = CancellationToken::new();
            let tasks = ActiveTasks::default();
            let ws = ServerWs::from_stream(
                server_stream,
                req_cancel.clone(),
                super::ws::WsConn {
                    cancel_guard: Arc::new(CancelOnDrop::new(conn_cancel.clone())),
                    permit: conn_permit,
                    peer_eof: Arc::new((AtomicBool::new(false), tokio::sync::Notify::new())),
                    stall_limit: Duration::from_secs(60),
                },
                &tasks,
                Arc::new((AtomicBool::new(false), tokio::sync::Notify::new())),
                None,
            );
            let handles = tasks.drain();
            assert_eq!(handles.len(), 2, "expected read + write tasks");

            // Drop every user-side handle; nothing else cancels the tokens.
            drop(ws);

            tokio::time::timeout(Duration::from_secs(5), async {
                for h in handles {
                    h.await.unwrap();
                }
            })
            .await
            .expect("read and write tasks must finish once all ServerWs handles are dropped");
            assert!(req_cancel.is_cancelled());
            assert!(conn_cancel.is_cancelled());
            assert_eq!(sem.available_permits(), 1, "connection permit must be released");
        })
        .await;
}

#[tokio::test]
async fn ws_dropping_handles_interrupts_blocked_send_and_tasks_finish() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let hooks = Arc::new(super::ws::ServeTestHooks::default());
            let (server_io, client_io) = tokio::io::duplex(64 * 1024);
            let (server_r, server_w) = tokio::io::split(server_io);
            let io = PumpIo {
                reader: server_r,
                writer: super::ws::TestWriteHalf {
                    inner: server_w,
                    hooks: Some(hooks.clone()),
                    upgrade_phase: Arc::new(AtomicU8::new(UPGRADE_SUCCEEDED)),
                    upgraded_phase_val: UPGRADE_SUCCEEDED,
                },
            };
            let server_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
                io,
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            )
            .await;
            // Idle client: never reads, so the read task stays parked on `stream.next()`.
            let _client = tokio_tungstenite::WebSocketStream::from_raw_socket(
                client_io,
                tokio_tungstenite::tungstenite::protocol::Role::Client,
                None,
            )
            .await;

            let sem = Arc::new(Semaphore::new(1));
            let conn_permit = Arc::new(sem.clone().acquire_owned().await.unwrap());
            let conn_cancel = CancellationToken::new();
            let req_cancel = CancellationToken::new();
            let tasks = ActiveTasks::default();
            let ws = ServerWs::from_stream(
                server_stream,
                req_cancel.clone(),
                super::ws::WsConn {
                    cancel_guard: Arc::new(CancelOnDrop::new(conn_cancel.clone())),
                    permit: conn_permit,
                    peer_eof: Arc::new((AtomicBool::new(false), tokio::sync::Notify::new())),
                    stall_limit: Duration::from_secs(60),
                },
                &tasks,
                Arc::new((AtomicBool::new(false), tokio::sync::Notify::new())),
                None,
            );
            let handles = tasks.drain();
            assert_eq!(handles.len(), 2, "expected read + write tasks");
            let ud = lua.create_userdata(ws).unwrap();

            // Barrier: the write task is inside `sink.send` and the write returned Pending.
            hooks.block_ws_writes.store(true, std::sync::atomic::Ordering::Release);
            let mut pending = std::pin::pin!(hooks.on_ws_write_pending.notified());
            pending.as_mut().enable();
            ud.call_async_method::<()>("send", "blocked").await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), pending).await.expect("ws write must be Pending");

            // Drop the only user-side handle.
            ud.destroy().unwrap();

            tokio::time::timeout(Duration::from_secs(5), async {
                for h in handles {
                    h.await.unwrap();
                }
            })
            .await
            .expect("blocked send must be interrupted and both tasks finish after the handle drops");
            assert!(req_cancel.is_cancelled());
            assert!(conn_cancel.is_cancelled());
            assert_eq!(sem.available_permits(), 1, "connection permit must be released");
        })
        .await;
}
