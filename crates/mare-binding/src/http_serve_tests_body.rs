//! Request-body backpressure tests: a slow handler keeps the connection, a disconnect cancels it.

use super::*;

fn oneshot_notifier(lua: &Lua, name: &str) -> oneshot::Receiver<()> {
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
    rx
}

#[tokio::test]
async fn slow_handler_reads_large_body_intact_after_duplex_stall() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let started_rx = oneshot_notifier(&lua, "notify_started");
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
                            -- Async work before touching the body, longer than the old 200 ms stall cutoff.
                            wait_release()
                            local body, err = req:body()
                            assert(body, err)
                            return { status = 200, body = body }
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stalled = std::pin::pin!(hooks.on_duplex_stalled.notified());
            stalled.as_mut().enable();

            // Larger than the 64 KiB duplex, so the pump must wait for the handler.
            let body: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
            let stream = TcpStream::connect(&addr).await.unwrap();
            let (mut rd, mut wr) = stream.into_split();
            let header = format!("POST /upload HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n", body.len());
            let body_c = body.clone();
            let writer = tokio::task::spawn_local(async move {
                wr.write_all(header.as_bytes()).await.unwrap();
                wr.write_all(&body_c).await.unwrap();
                wr
            });

            tokio::time::timeout(Duration::from_secs(3), started_rx).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(3), stalled).await.expect("64 KiB duplex must saturate");
            // Scenario duration (not synchronization): keep the body unread past the old 200 ms cutoff.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = release_tx.send(());

            let _wr =
                tokio::time::timeout(Duration::from_secs(5), writer).await.expect("client upload completes").unwrap();
            let mut resp = Vec::new();
            let head_end = loop {
                let mut chunk = [0u8; 16 * 1024];
                let n = tokio::time::timeout(Duration::from_secs(5), rd.read(&mut chunk)).await.unwrap().unwrap();
                assert!(n > 0, "connection closed before full response");
                resp.extend_from_slice(&chunk[..n]);
                if let Some(pos) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&resp[..pos]).to_string();
                    assert!(head.starts_with("HTTP/1.1 200"), "unexpected response head: {head}");
                    if resp.len() >= pos + 4 + body.len() {
                        break pos + 4;
                    }
                }
            };
            assert_eq!(&resp[head_end..head_end + body.len()], &body[..], "echoed body must be intact");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn disconnect_while_body_saturated_cancels_and_releases_permit() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let started_rx = oneshot_notifier(&lua, "notify_started");
            let cancelled_rx = oneshot_notifier(&lua, "notify_cancelled");

            // The stall timeout is far beyond the test's wait: only disconnect detection can cancel.
            let (srv, hooks) = start_serve_with_hooks(
                &lua,
                r#"
                    {
                        max_connections = 1,
                        max_concurrency = 1,
                        body_stall_ms = 600000,
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
            let mut stalled = std::pin::pin!(hooks.on_duplex_stalled.notified());
            stalled.as_mut().enable();

            let mut stream = TcpStream::connect(&addr).await.unwrap();
            // > 64 KiB duplex + 8 KiB pump chunk, but small enough that the client FIN still fits in the
            // server receive window (a FIN behind a full window is only covered by body_stall_ms).
            let body = vec![b'a'; 80 * 1024];
            let header = format!("POST /wait HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
            stream.flush().await.unwrap();

            tokio::time::timeout(Duration::from_secs(3), started_rx).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(3), stalled)
                .await
                .expect("64 KiB duplex must saturate before disconnect");
            // Abortive close (RST): a plain FIN is a half-close and does not cancel.
            stream.set_zero_linger().unwrap();
            drop(stream);

            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("disconnect while the body is backpressured must cancel req.cancel")
                .unwrap();
            let resp =
                tokio::time::timeout(Duration::from_secs(3), crate::http::test_get(format!("http://{addr}/ping")))
                    .await
                    .expect("permits must be released after the disconnect")
                    .unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.text().await.unwrap(), "ok");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn half_closed_client_during_body_backpressure_gets_full_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let started_rx = oneshot_notifier(&lua, "notify_started");
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
                        body_stall_ms = 600000,
                        handler = function(req)
                            notify_started()
                            wait_release()
                            local body, err = req:body()
                            assert(body, err)
                            return { status = 200, body = tostring(#body) .. ":" .. tostring(req.cancel:is_cancelled()) }
                        end
                    }
                "#,
            )
            .await;

            let addr: String = srv.call_method("addr", ()).unwrap();
            let mut stalled = std::pin::pin!(hooks.on_duplex_stalled.notified());
            stalled.as_mut().enable();
            let mut half_closed = std::pin::pin!(hooks.on_peer_half_close.notified());
            half_closed.as_mut().enable();

            let body = vec![b'h'; 80 * 1024];
            let stream = TcpStream::connect(&addr).await.unwrap();
            let (mut rd, mut wr) = stream.into_split();
            let header = format!("POST /upload HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n", body.len());
            wr.write_all(header.as_bytes()).await.unwrap();
            wr.write_all(&body).await.unwrap();
            // Complete body sent; half-close the write side and keep reading.
            wr.shutdown().await.unwrap();

            tokio::time::timeout(Duration::from_secs(3), started_rx).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(3), stalled).await.expect("64 KiB duplex must saturate");
            tokio::time::timeout(Duration::from_secs(3), half_closed)
                .await
                .expect("server must observe the FIN while the body is backpressured");
            let _ = release_tx.send(());

            let mut resp = Vec::new();
            tokio::time::timeout(Duration::from_secs(5), rd.read_to_end(&mut resp))
                .await
                .expect("full response after half-close")
                .unwrap();
            let resp = String::from_utf8_lossy(&resp);
            assert!(resp.starts_with("HTTP/1.1 200"), "unexpected response: {resp}");
            assert!(
                resp.ends_with(&format!("{}:false", body.len())),
                "full body must arrive and req.cancel must not fire on half-close: {resp}"
            );

            drop(wr);
            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}
