//! WebSocket inbound backpressure tests: a full queue throttles the peer instead of closing, and
//! a stuck backpressured connection is still reclaimed (disconnect or `body_stall_ms`).

use super::*;

fn watch_cancel_global(lua: &Lua) -> oneshot::Receiver<()> {
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
    cancelled_rx
}

/// Handler that never consumes WS frames; `/ping` proves both permits are free again.
const NON_CONSUMING_STALL_HANDLER: &str = r#"
    {
        max_connections = 1,
        max_concurrency = 1,
        body_stall_ms = 200,
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
"#;

/// Same handler with a stall limit long enough that the window-fill phase cannot hit it.
const NON_CONSUMING_SLOW_STALL_HANDLER: &str = r#"
    {
        max_connections = 1,
        max_concurrency = 1,
        body_stall_ms = 3000,
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
"#;

async fn assert_cancelled_and_permits_released(addr: &str, cancelled_rx: oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(5), cancelled_rx)
        .await
        .expect("stuck backpressured WS must cancel req.cancel")
        .unwrap();
    let resp = tokio::time::timeout(Duration::from_secs(5), crate::http::test_get(format!("http://{addr}/ping")))
        .await
        .expect("connection and request permits must be released")
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn ws_full_recv_queue_disconnect_cancels() {
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

            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                r#"
                    return {
                        handler = function(req)
                            watch_ws_cancel(req.cancel)
                            local ws, err = req:upgrade()
                            assert(ws, err)
                            req.cancel:wait()
                            return nil
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let (mut ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);

            let mut full_notified = std::pin::pin!(hooks.on_inbound_full.notified());
            full_notified.as_mut().enable();
            for i in 0..super::WS_QUEUE_CAPACITY {
                ws.send(ClientWsMsg::Text(format!("msg-{i}").into())).await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(3), full_notified)
                .await
                .expect("server should reach full inbound queue deterministically");
            ws.close(None).await.unwrap();

            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("WS req.cancel should be cancelled even when recv queue is full")
                .unwrap();

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_backpressured_inbound_stays_open_then_disconnect_releases_permits() {
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
                            req.cancel:wait()
                            return nil
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let (mut ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);

            // The handler never reads: past the queue capacity the read task applies backpressure
            // instead of closing the connection.
            let mut backpressured = std::pin::pin!(hooks.on_inbound_backpressure.notified());
            backpressured.as_mut().enable();
            for i in 0..super::WS_QUEUE_CAPACITY + 11 {
                ws.send(ClientWsMsg::Text(format!("msg-{i}").into())).await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(3), backpressured)
                .await
                .expect("server read task should apply backpressure once the inbound queue is full");
            assert!(cancelled_rx.try_recv().is_err(), "a full inbound queue must not close the connection");

            // A client disconnect while backpressured still cancels and releases the permit.
            drop(ws);
            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("disconnect while backpressured should cancel req.cancel")
                .unwrap();

            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("conn_sem permit should be released after disconnect while backpressured")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_close_behind_full_queue_reclaimed_by_stall_limit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let cancelled_rx = watch_cancel_global(&lua);
            let (srv, hooks) = start_serve_with_hooks(&lua, NON_CONSUMING_STALL_HANDLER).await;
            let addr: String = srv.call_method("addr", ()).unwrap();
            let (mut ws, resp) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.unwrap();
            assert_eq!(resp.status(), 101);

            let mut backpressured = std::pin::pin!(hooks.on_inbound_backpressure.notified());
            backpressured.as_mut().enable();
            for i in 0..=super::WS_QUEUE_CAPACITY {
                ws.send(ClientWsMsg::Text(format!("msg-{i}").into())).await.unwrap();
            }
            tokio::time::timeout(Duration::from_secs(3), backpressured)
                .await
                .expect("65th frame must hit inbound backpressure");

            // The Close sits behind the unread 65th frame, so the server cannot see it; the
            // client waits for a reply that only the stall limit can resolve (by teardown).
            ws.close(None).await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), async { while let Some(Ok(_)) = ws.next().await {} })
                .await
                .expect("server must tear down the stuck connection");

            assert_cancelled_and_permits_released(&addr, cancelled_rx).await;
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn ws_tcp_close_behind_full_receive_window_reclaimed_by_stall_limit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let mut cancelled_rx = watch_cancel_global(&lua);
            let hooks = Arc::new(super::super::ws::ServeTestHooks::default());
            let opts: mlua::Table = lua.load(NON_CONSUMING_SLOW_STALL_HANDLER).eval().unwrap();
            opts.set("token", false).unwrap();
            let mv = super::super::serve_with_binder_and_hooks(
                lua.clone(),
                opts,
                |host, port| async move {
                    let addr: std::net::SocketAddr = format!("{host}:{port}").parse().unwrap();
                    let sock = tokio::net::TcpSocket::new_v4().unwrap();
                    // Pin a small server receive buffer so the client's send buffer holds unsent
                    // data (and queues FIN behind it) once the receive window fills.
                    sock.set_recv_buffer_size(16 * 1024).unwrap();
                    sock.bind(addr).unwrap();
                    let listener = sock.listen(128).unwrap();
                    let local_addr = listener.local_addr().unwrap();
                    Ok((listener, local_addr))
                },
                mare_shared::paths::host_token_file(),
                Some(hooks.clone()),
            )
            .await
            .unwrap();
            let srv = match mv.into_iter().next().unwrap() {
                mlua::Value::UserData(ud) => ud,
                other => panic!("expected HttpServer userdata, got {other:?}"),
            };
            let addr: String = srv.call_method("addr", ()).unwrap();
            let client_sock = tokio::net::TcpSocket::new_v4().unwrap();
            client_sock.set_send_buffer_size(256 * 1024).unwrap();
            let tcp = client_sock.connect(addr.parse().unwrap()).await.unwrap();
            let (ws, resp) = tokio_tungstenite::client_async(format!("ws://{addr}/ws"), tcp).await.unwrap();
            assert_eq!(resp.status(), 101);

            // Raw masked binary frames (16 KiB payload, zero mask key) written below the client
            // codec so the test can tell when the socket stops accepting bytes.
            let mut frame = vec![0x82u8, 0xFE, 0x40, 0x00, 0, 0, 0, 0];
            frame.extend(std::iter::repeat_n(b'x', 16 * 1024));
            let sock = ws.get_ref();
            let mut inbound_full = std::pin::pin!(hooks.on_inbound_full.notified());
            inbound_full.as_mut().enable();
            let mut stalled = std::pin::pin!(hooks.on_duplex_stalled.notified());
            stalled.as_mut().enable();
            let mut inbound_full_seen = false;
            let mut stalled_seen = false;
            let mut off = 0;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match sock.try_write(&frame[off..]) {
                        Ok(n) => off = (off + n) % frame.len(),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if inbound_full_seen && stalled_seen {
                                // Confirm the kernel send buffer truly stays non-writable after
                                // `on_duplex_stalled` (avoiding a stale tokio `WRITABLE` flag from
                                // a `WouldBlock` observed before the server's final read drained a
                                // chunk and opened space in the receive window, notably on Darwin).
                                if let Ok(r) = tokio::time::timeout(Duration::from_millis(50), sock.writable()).await { r.unwrap(); continue; }
                                break;
                            }
                            tokio::select! {
                                _ = &mut inbound_full, if !inbound_full_seen => inbound_full_seen = true,
                                _ = &mut stalled, if !stalled_seen => stalled_seen = true,
                                r = sock.writable() => r.unwrap(),
                            }
                        }
                        Err(e) => panic!("unexpected write error: {e}"),
                    }
                }
            })
            .await
            .expect("client must fill the server receive window");

            assert!(cancelled_rx.try_recv().is_err(), "must not be cancelled before the client closes");

            // FIN is queued behind the full window: the server must neither read it nor see it via
            // the stall probe; only the backpressure stall timeout may reclaim the connection.
            let mut fin_read = std::pin::pin!(hooks.on_peer_eof_read.notified());
            fin_read.as_mut().enable();
            let mut fin_probed = std::pin::pin!(hooks.on_peer_half_close.notified());
            fin_probed.as_mut().enable();
            let mut stall_timeout = std::pin::pin!(hooks.on_backpressure_stall_timeout.notified());
            stall_timeout.as_mut().enable();
            drop(ws);
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::select! {
                    biased;
                    _ = &mut stall_timeout => {}
                    _ = &mut fin_read => panic!("server read the FIN instead of hitting backpressure stall timeout"),
                    _ = &mut fin_probed => panic!("server observed the FIN via stall probe instead of hitting backpressure stall timeout"),
                }
            })
            .await
            .expect("the WS backpressure stall-timeout branch must be taken");
            assert_cancelled_and_permits_released(&addr, cancelled_rx).await;
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}
