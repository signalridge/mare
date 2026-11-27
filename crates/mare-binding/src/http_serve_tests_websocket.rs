//! WebSocket framing, delivery, and close-handshake tests.

use super::*;

#[tokio::test]
async fn websocket_upgrade_echo() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            local ws, err = req:upgrade()
                            if not ws then
                                return { status = 400, body = err }
                            end
                            while true do
                                local msg = ws:recv()
                                if not msg then
                                    break
                                end
                                ws:send("echo:" .. msg)
                            end
                            ws:close()
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
            let reply1 = ws.next().await.unwrap().unwrap();
            assert_eq!(reply1, ClientWsMsg::Text("echo:hello".into()));

            ws.send(ClientWsMsg::Text("world".into())).await.unwrap();
            let reply2 = ws.next().await.unwrap().unwrap();
            assert_eq!(reply2, ClientWsMsg::Text("echo:world".into()));

            ws.close(None).await.unwrap();
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn websocket_binary_frames_preserve_arbitrary_bytes() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            local ws, err = req:upgrade()
                            if not ws then
                                return { status = 400, body = err }
                            end
                            while true do
                                local msg, _reason, is_bin = ws:recv()
                                if not msg then
                                    break
                                end
                                ws:send(msg, { binary = is_bin })
                            end
                            ws:close()
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

            let payload: Vec<u8> = vec![0x00, 0x01, 0x7f, 0x80, 0xfe, 0xff, 0x00, 0xff];
            ws.send(ClientWsMsg::Binary(payload.clone().into())).await.unwrap();
            let reply = ws.next().await.unwrap().unwrap();
            assert_eq!(reply, ClientWsMsg::Binary(payload.into()));

            ws.close(None).await.unwrap();
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_backpressure_beyond_queue_capacity_delivers_all_in_order() {
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

            let (resume_tx, resume_rx) = oneshot::channel::<()>();
            let resume_rx = Arc::new(tokio::sync::Mutex::new(Some(resume_rx)));
            lua.globals()
                .set(
                    "wait_resume",
                    lua.create_async_function(move |_, ()| {
                        let rx_arc = resume_rx.clone();
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

            let (got_tx, got_rx) = oneshot::channel::<Vec<String>>();
            let got_tx = Arc::new(std::sync::Mutex::new(Some(got_tx)));
            lua.globals()
                .set(
                    "report_msgs",
                    lua.create_function(move |_, msgs: Vec<String>| {
                        if let Some(tx) = got_tx.lock().unwrap().take() {
                            let _ = tx.send(msgs);
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let total = super::WS_QUEUE_CAPACITY + 11; // 75 frames: more than the inbound queue holds
            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                &format!(
                    r#"
                    return {{
                        max_connections = 1,
                        handler = function(req)
                            if req.path == "/ping" then
                                return {{ status = 200, body = "ok" }}
                            end
                            watch_ws_cancel(req.cancel)
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            wait_resume()
                            local msgs = {{}}
                            for i = 1, {total} do
                                local data, cerr = ws:recv()
                                assert(data, cerr)
                                msgs[i] = data
                            end
                            report_msgs(msgs)
                            ws:send("all-received")
                            req.cancel:wait()
                            return nil
                        end
                    }}
                "#
                ),
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let (mut ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);

            // Barrier: the read task holds a frame it cannot queue and stops reading the socket.
            let mut backpressured = std::pin::pin!(hooks.on_inbound_backpressure.notified());
            backpressured.as_mut().enable();
            for i in 0..total {
                ws.send(ClientWsMsg::Text(format!("msg-{i}").into())).await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(3), backpressured)
                .await
                .expect("server read task should apply backpressure once the inbound queue is full");
            assert!(cancelled_rx.try_recv().is_err(), "WS must stay open under inbound backpressure");

            // Resume consumption and verify all messages are delivered intact.
            let _ = resume_tx.send(());
            let msgs = tokio::time::timeout(Duration::from_secs(3), got_rx)
                .await
                .expect("consumer should receive all queued messages after resuming")
                .unwrap();
            assert_eq!(msgs.len(), total);
            for (i, m) in msgs.iter().enumerate() {
                assert_eq!(m, &format!("msg-{i}"));
            }
            // Connection is still open after the backlog drained: the server can still send.
            let ack = tokio::time::timeout(Duration::from_secs(3), ws.next())
                .await
                .expect("server ack after draining backlog")
                .unwrap()
                .unwrap();
            assert_eq!(ack, ClientWsMsg::Text("all-received".into()));
            assert!(cancelled_rx.try_recv().is_err(), "WS must stay open after the backlog drains");

            // Direct TCP drop (no Close frame) releases both req.cancel and conn_sem permit.
            drop(ws);

            tokio::time::timeout(Duration::from_secs(3), &mut cancelled_rx)
                .await
                .expect("WS req.cancel should be cancelled promptly after direct TCP drop")
                .unwrap();

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("conn_sem permit should be released promptly after direct TCP drop")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_slow_but_continuously_consuming_drains_all_frames() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (got_tx, got_rx) = oneshot::channel::<(Vec<String>, String)>();
            let got_tx = Arc::new(std::sync::Mutex::new(Some(got_tx)));

            lua.globals()
                .set(
                    "sleep_ms",
                    lua.create_async_function(|_, ms: u64| async move {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.globals()
                .set(
                    "report_slow_msgs",
                    lua.create_function(move |_, (msgs, why): (Vec<String>, String)| {
                        if let Some(tx) = got_tx.lock().unwrap().take() {
                            let _ = tx.send((msgs, why));
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
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            local msgs = {}
                            local why = ""
                            while true do
                                local data, cerr = ws:recv()
                                if not data then
                                    why = cerr or ""
                                    break
                                end
                                sleep_ms(60)
                                table.insert(msgs, data)
                            end
                            report_slow_msgs(msgs, why)
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

            for i in 0..15 {
                ws.send(ClientWsMsg::Text(format!("slow-{i}").into())).await.unwrap();
            }
            ws.send(ClientWsMsg::Close(None)).await.unwrap();
            drop(ws);

            let (msgs, why) = tokio::time::timeout(Duration::from_secs(5), got_rx)
                .await
                .expect("slow-but-progressing WS consumer must finish draining")
                .unwrap();
            let expected: Vec<String> = (0..15).map(|i| format!("slow-{i}")).collect();
            assert_eq!(msgs, expected);
            assert!(why.contains("closed"), "unexpected close reason: {why}");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_last_frame_processing_over_20ms_not_aborted_before_next_recv() {
    run_last_frame_processing_not_aborted(Some(500), 60).await;
}

/// Handling the last frame for longer than the old fixed 500 ms window must not be
/// interrupted under the default `close_grace_ms`; a genuinely stuck handler is still aborted by
/// the grace timeout (`close_grace_ms_validation_and_timeout_aborts_stuck_ws_handler`).
#[tokio::test]
async fn ws_last_frame_processing_over_500ms_not_aborted_with_default_grace() {
    run_last_frame_processing_not_aborted(None, 600).await;
}

async fn run_last_frame_processing_not_aborted(close_grace_ms: Option<u64>, process_ms: u64) {
    let grace_opt = close_grace_ms.map(|ms| format!("close_grace_ms = {ms},")).unwrap_or_default();
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (trigger_send_tx, trigger_send_rx) = oneshot::channel::<()>();
            let trigger_send_rx = Arc::new(tokio::sync::Mutex::new(Some(trigger_send_rx)));
            let (done_tx, done_rx) = oneshot::channel::<(String, String)>();
            let done_tx = Arc::new(std::sync::Mutex::new(Some(done_tx)));

            lua.globals()
                .set(
                    "wait_trigger_send",
                    lua.create_async_function(move |_, ()| {
                        let rx = trigger_send_rx.clone();
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
            lua.globals()
                .set(
                    "sleep_ms",
                    lua.create_async_function(|_, ms: u64| async move {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            lua.globals()
                .set(
                    "report_last_frame_done",
                    lua.create_function(move |_, (msg, why): (String, String)| {
                        if let Some(tx) = done_tx.lock().unwrap().take() {
                            let _ = tx.send((msg, why));
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                &format!(
                    r#"
                    {{
                        {grace_opt}
                        handler = function(req)
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            wait_trigger_send()
                            -- Trigger write-side error so reader exits without terminal while last-frame is queued.
                            ws:send("fail")
                            req.cancel:wait()
                            local data, cerr = ws:recv()
                            assert(data == "last-frame", "expected last-frame, got: " .. tostring(cerr))
                            -- Process last frame within close_grace_ms; must not be aborted before next recv().
                            sleep_ms({process_ms})
                            local next_data, next_err = ws:recv()
                            assert(next_data == nil, "expected closed on next recv")
                            report_last_frame_done(data, next_err or "")
                            return nil
                        end
                    }}
                "#
                ),
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let (mut ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);

            let mut pushed = std::pin::pin!(hooks.on_inbound_push.notified());
            pushed.as_mut().enable();
            ws.send(ClientWsMsg::Text("last-frame".into())).await.unwrap();
            tokio::time::timeout(Duration::from_secs(3), pushed)
                .await
                .expect("last-frame must be queued in inbound before write failure");

            hooks.fail_ws_writes.store(true, std::sync::atomic::Ordering::Release);
            let _ = trigger_send_tx.send(());

            let (msg, why) = tokio::time::timeout(Duration::from_secs(5), done_rx)
                .await
                .expect("last-frame processing must complete before next recv observes close")
                .unwrap();
            assert_eq!(msg, "last-frame");
            assert!(why.contains("closed"), "unexpected close reason: {why}");

            drop(ws);
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_close_delivers_queued_frames_with_code_reason() {
    use tokio_tungstenite::tungstenite::protocol::frame::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (resume_tx, resume_rx) = oneshot::channel::<()>();
            let resume_rx = Arc::new(tokio::sync::Mutex::new(Some(resume_rx)));
            lua.globals()
                .set(
                    "wait_resume",
                    lua.create_async_function(move |_, ()| {
                        let rx_arc = resume_rx.clone();
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

            let (got_tx, got_rx) = oneshot::channel::<(Vec<String>, String)>();
            let got_tx = Arc::new(std::sync::Mutex::new(Some(got_tx)));
            lua.globals()
                .set(
                    "report_result",
                    lua.create_function(move |_, (msgs, why): (Vec<String>, String)| {
                        if let Some(tx) = got_tx.lock().unwrap().take() {
                            let _ = tx.send((msgs, why));
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();

            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                r#"
                    return {
                        handler = function(req)
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            wait_resume()
                            local msgs = {}
                            local close_why = ""
                            while true do
                                local data, cerr = ws:recv()
                                if not data then
                                    close_why = cerr or ""
                                    break
                                end
                                table.insert(msgs, data)
                            end
                            report_result(msgs, close_why)
                            return nil
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let (mut ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);

            let mut terminal_notified = std::pin::pin!(hooks.on_inbound_terminal.notified());
            terminal_notified.as_mut().enable();
            for i in 0..super::WS_QUEUE_CAPACITY {
                ws.send(ClientWsMsg::Text(format!("msg-{i}").into())).await.unwrap();
            }
            ws.send(ClientWsMsg::Close(Some(CloseFrame { code: CloseCode::Library(4001), reason: "bye-now".into() })))
                .await
                .unwrap();

            tokio::time::timeout(Duration::from_secs(3), terminal_notified)
                .await
                .expect("server should record terminal Close frame even when inbound queue is full");

            let _ = resume_tx.send(());
            let (msgs, why) = tokio::time::timeout(Duration::from_secs(3), got_rx)
                .await
                .expect("consumer should drain all queued frames before Close")
                .unwrap();
            assert_eq!(msgs.len(), super::WS_QUEUE_CAPACITY);
            for (i, m) in msgs.iter().enumerate() {
                assert_eq!(m, &format!("msg-{i}"));
            }
            assert_eq!(why, "closed (4001: bye-now)");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_client_close_flushes_close_reply() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
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

            ws.send(ClientWsMsg::Close(None)).await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(3), ws.next())
                .await
                .expect("server should flush Close reply frame")
                .expect("stream should yield Close reply")
                .expect("Close reply should not be a protocol error");
            assert!(matches!(reply, ClientWsMsg::Close(_)), "expected Close reply, got {reply:?}");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}
