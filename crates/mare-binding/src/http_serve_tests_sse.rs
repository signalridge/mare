//! Streaming (SSE) response tests.

use super::*;

#[tokio::test]
async fn streaming_sse_response() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(_req)
                            local i = 0
                            return {
                                status = 200,
                                body = function()
                                    i = i + 1
                                    if i == 1 then
                                        return { id = "1", event = "tick", data = "alpha" }
                                    elseif i == 2 then
                                        return { event = "multi", data = "line1\nline2" }
                                    elseif i == 3 then
                                        return "data: raw\n\n"
                                    end
                                    return nil
                                end
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
            let resp = client.get(format!("http://{addr}/events")).send().await.unwrap();
            assert_eq!(resp.status(), 200);
            assert!(resp.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/event-stream"));
            let body = resp.text().await.unwrap();
            assert_eq!(
                body,
                "id: 1\nevent: tick\ndata: alpha\n\nevent: multi\ndata: line1\ndata: line2\n\ndata: raw\n\n"
            );

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn sse_client_disconnect_cancels_request_token() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let (cancelled_tx, cancelled_rx) = oneshot::channel::<()>();
            let cancelled_tx = Arc::new(std::sync::Mutex::new(Some(cancelled_tx)));
            lua.globals()
                .set(
                    "watch_cancel",
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
                            watch_cancel(req.cancel)
                            local sent = false
                            return {
                                status = 200,
                                body = function()
                                    if not sent then
                                        sent = true
                                        return { data = "first" }
                                    end
                                    -- Live stream: a client that closed (FIN) is noticed on the next write.
                                    return { data = "tick" }
                                end
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
            let mut resp = client.get(format!("http://{addr}/events")).send().await.unwrap();
            assert_eq!(resp.status(), 200);
            let first = resp.chunk().await.unwrap();
            assert!(first.is_some());
            drop(resp);

            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("SSE req.cancel should be cancelled on client disconnect")
                .unwrap();

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn sse_error_after_full_queue_is_not_swallowed() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let srv: mlua::AnyUserData = lua
                .load(
                    r#"
                    return mare.http.serve { token = false,
                        handler = function(req)
                            local i = 0
                            return {
                                status = 200,
                                body = function()
                                    i = i + 1
                                    if i <= 20 then
                                        return "data: chunk-" .. i .. "\n\n"
                                    end
                                    error("boom-after-full")
                                end
                            }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();

            let addr: String = srv.call_method("addr", ()).unwrap();
            let resp = crate::http::test_get(format!("http://{addr}/sse")).await.unwrap();
            assert_eq!(resp.status(), 200);
            let body_res = tokio::time::timeout(Duration::from_secs(3), resp.text()).await.unwrap();
            assert!(body_res.is_err(), "SSE stream error after full queue must propagate to client, not clean EOF");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn stream_normal_completion_does_not_cancel_request_token() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn read_until(stream: &mut TcpStream, buf: &mut Vec<u8>, needle: &[u8]) {
        let mut chunk = [0u8; 1024];
        while !buf.windows(needle.len()).any(|w| w == needle) {
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "connection closed before {:?}", String::from_utf8_lossy(needle));
            buf.extend_from_slice(&chunk[..n]);
        }
    }

    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let lua = setup_lua();
            let stashed: Arc<std::sync::Mutex<Option<CancellationToken>>> = Arc::new(std::sync::Mutex::new(None));
            let stashed_c = stashed.clone();
            lua.globals()
                .set(
                    "stash_cancel",
                    lua.create_function(move |_, tok: mlua::AnyUserData| {
                        *stashed_c.lock().unwrap() = Some(tok.borrow::<crate::task::Token>()?.0.clone());
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
                            if req.path ~= "/events" then
                                return { status = 200, body = "pong" }
                            end
                            stash_cancel(req.cancel)
                            local sent = false
                            return {
                                status = 200,
                                body = function()
                                    if sent then return nil end
                                    sent = true
                                    return { data = "only" }
                                end
                            }
                        end
                    }
                "#,
                )
                .eval_async()
                .await
                .unwrap();
            let addr: String = srv.call_method("addr", ()).unwrap();

            let mut stream = TcpStream::connect(&addr).await.unwrap();
            stream.write_all(b"GET /events HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
            let mut buf = Vec::new();
            // Chunked terminator: the stream ended normally.
            read_until(&mut stream, &mut buf, b"0\r\n\r\n").await;
            assert!(String::from_utf8_lossy(&buf).contains("data: only"));

            // A second request on the same keep-alive connection proves the server finished with the
            // first response body (HTTP/1.1 is sequential) while the connection is still open.
            stream.write_all(b"GET /ping HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
            buf.clear();
            read_until(&mut stream, &mut buf, b"pong").await;

            let tok = stashed.lock().unwrap().clone().expect("handler stashed req.cancel");
            assert!(!tok.is_cancelled(), "req.cancel must not fire on normal stream completion");

            // Disconnect still cancels (via the connection token).
            drop(stream);
            tokio::time::timeout(Duration::from_secs(3), tok.cancelled())
                .await
                .expect("req.cancel should be cancelled after client disconnect");

            srv.call_async_method::<()>("close", ()).await.unwrap();
        })
        .await;
}
