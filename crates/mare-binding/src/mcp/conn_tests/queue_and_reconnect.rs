//! Submission queueing, reconnect backoff, elicitation, and debounce behavior.

use super::*;

#[tokio::test]
async fn reply_post_must_not_complete_colliding_tool_call() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let home = tempfile::tempdir().unwrap();
    unsafe {
        std::env::set_var("MARE_HOME", home.path());
    }
    for (oauth, reply_error, status, content_type) in [false, true].into_iter().flat_map(|oauth| {
        [false, true].into_iter().flat_map(move |reply_error| {
            [
                ("409 Conflict", "application/json"),
                ("200 OK", "application/json"),
                ("200 OK", "text/event-stream"),
                ("202 Accepted", "application/json"),
                ("204 No Content", "application/json"),
            ]
            .into_iter()
            .map(move |(status, ct)| (oauth, reply_error, status, ct))
        })
    }) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (reply_tx, mut reply_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let release_rx = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
        let server = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let reply_tx = reply_tx.clone();
                let release_rx = release_rx.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0; 4096];
                    let (header_end, len) = loop {
                        let n = stream.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let headers = String::from_utf8_lossy(&buf[..end]);
                            let len = headers
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse::<usize>().ok())
                                        .flatten()
                                })
                                .unwrap_or(0);
                            if buf.len() >= end + 4 + len {
                                break (end, len);
                            }
                        }
                    };
                    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let first = headers.lines().next().unwrap_or("").to_owned();
                    let req: serde_json::Value =
                        serde_json::from_slice(&buf[header_end + 4..header_end + 4 + len]).unwrap_or_default();
                    if first.starts_with("GET /.well-known/oauth-authorization-server/mcp ") {
                        let origin = format!("http://{addr}");
                        let meta = serde_json::json!({
                            "issuer": format!("{origin}/mcp"),
                            "authorization_endpoint": format!("{origin}/authorize"),
                            "token_endpoint": format!("{origin}/token"),
                            "code_challenge_methods_supported": ["S256"],
                        })
                        .to_string();
                        let _ = stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{meta}",
                                    meta.len()
                                )
                                .as_bytes(),
                            )
                            .await;
                        return;
                    }
                    if first.starts_with("GET ") {
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await;
                        return;
                    }
                    if req["method"] == "tools/call" {
                        let id = req["id"].clone();
                        let reverse = if reply_error {
                            serde_json::json!({"jsonrpc":"2.0","id":id,"method":"sampling/createMessage","params":{"messages":[],"maxTokens":1}})
                        } else {
                            serde_json::json!({"jsonrpc":"2.0","id":id,"method":"roots/list"})
                        };
                        let first_event = format!("data: {reverse}\n\n");
                        let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n").await;
                        let _ = stream
                            .write_all(format!("{:x}\r\n{}\r\n", first_event.len(), first_event).as_bytes())
                            .await;
                        if let Some(rx) = release_rx.lock().await.take() {
                            let _ = rx.await;
                        }
                        let correct = serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"content":[{"type":"text","text":"correct"}]}});
                        let event = format!("data: {correct}\n\n");
                        let _ =
                            stream.write_all(format!("{:x}\r\n{}\r\n0\r\n\r\n", event.len(), event).as_bytes()).await;
                        return;
                    }
                    let (st, ct, body) = if req["method"] == "initialize" {
                        ("200 OK", "application/json", serde_json::json!({"jsonrpc":"2.0","id":req["id"],"result":{"protocolVersion":"2025-03-26","capabilities":{},"serverInfo":{"name":"test","version":"1"}}}).to_string())
                    } else if req.get("method").is_some() {
                        ("202 Accepted", "application/json", String::new())
                    } else {
                        let poison = if status.starts_with("409") {
                            serde_json::json!({"jsonrpc":"2.0","id":req["id"],"error":{"code":-32000,"message":"poison"}})
                        } else {
                            serde_json::json!({"jsonrpc":"2.0","id":req["id"],"result":{"content":[{"type":"text","text":"poison"}]}})
                        };
                        let body = if status.starts_with("202") || status.starts_with("204") {
                            String::new()
                        } else if content_type == "text/event-stream" {
                            format!("data: {poison}\n\n")
                        } else {
                            poison.to_string()
                        };
                        (status, content_type, body)
                    };
                    let out = format!(
                        "HTTP/1.1 {st}\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(out.as_bytes()).await;
                    if req.get("method").is_none() && req.get("id").is_some() {
                        let _ = reply_tx.send((
                            headers.to_ascii_lowercase().contains("authorization: bearer test-token"),
                            req.get("error").is_some(),
                        ));
                    }
                });
            }
        });
        let url = format!("http://{addr}/mcp");
        if oauth {
            crate::mcp::oauth::FileCredentialStore(crate::mcp::oauth::oauth_file("mcp", &url))
                .save_bearer("test-token")
                .await
                .unwrap();
        }
        let raw = serde_json::json!({"url":url, "oauth":oauth});
        let inner = Inner::new(Spec::parse(raw).unwrap(), None);
        let svc = crate::mcp::conn_transport::connect_http(
            &inner.spec,
            inner.handler(),
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
        let peer = svc.peer().clone();
        let call_inner = inner.clone();
        let mut call = tokio::spawn(async move {
            call_tool(peer, "echo".into(), serde_json::Map::new(), Duration::from_secs(5), &call_inner, None).await
        });
        let (authenticated, was_error) =
            tokio::time::timeout(Duration::from_secs(3), reply_rx.recv()).await.unwrap().unwrap();
        assert_eq!(authenticated, oauth, "reply POST must use the selected authentication path");
        assert_eq!(was_error, reply_error, "fixture must exercise both reply types");
        let early = tokio::time::timeout(Duration::from_millis(300), &mut call).await;
        assert!(early.is_err(), "{status} {content_type}: response POST completed the unrelated tool call: {early:?}");
        let _ = release_tx.send(());
        let result = tokio::time::timeout(Duration::from_secs(3), call).await.unwrap().unwrap().unwrap();
        assert!(result.to_string().contains("correct"), "{result:?}");
        server.abort();
    }
}

#[tokio::test]
async fn notification_between_final_check_and_send_never_enqueues_stale_call() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut remote_write, client_read) = tokio::io::duplex(4096);
    let (client_write, remote_read) = tokio::io::duplex(4096);
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(
        inner.handler(),
        (client_read, client_write),
        None,
        CancellationToken::new(),
    );
    let peer = service.peer().clone();
    let expected = inner.tools_generation();
    // The outer MCP binding has checked `expected` and is paused immediately before
    // submitting the real rmcp request. A notification wins during that pause.
    remote_write.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while inner.tools_generation() == expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let sender = inner.clone();
    let task = tokio::spawn(async move {
        call_tool(peer, "echo".into(), serde_json::Map::new(), Duration::from_secs(3), &sender, Some(expected)).await
    });
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_millis(200), BufReader::new(remote_read).read_line(&mut line)).await;
    assert!(read.is_err(), "stale call was enqueued after notification: {line}");
    assert!(task.await.unwrap().unwrap_err().contains("definition changed"));
}

#[tokio::test]
async fn notification_overlapping_a_paused_submission_is_ordered_after_enqueue() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut remote_write, client_read) = tokio::io::duplex(4096);
    let (client_write, remote_read) = tokio::io::duplex(4096);
    let (seen_tx, mut seen_rx) = mpsc::unbounded_channel();
    let mut handler = inner.handler();
    handler.notification_seen = Some(seen_tx);
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(
        handler,
        (client_read, client_write),
        None,
        CancellationToken::new(),
    );
    let peer = service.peer().clone();
    let sender = inner.clone();
    let (checked_tx, checked_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        call_tool_with_before_submit(
            peer,
            "echo".into(),
            serde_json::Map::new(),
            Duration::from_secs(3),
            &sender,
            Some(0),
            None,
            async move {
                let _ = checked_tx.send(());
                let _ = release_rx.await;
            },
        )
        .await
    });
    checked_rx.await.unwrap();
    remote_write.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), seen_rx.recv()).await.unwrap().unwrap();
    assert_eq!(inner.tools_generation(), 0, "notification must wait for the checked submission");
    release_tx.send(()).unwrap();
    let mut reader = BufReader::new(remote_read);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], "tools/call");
    tokio::time::timeout(Duration::from_secs(2), async {
        while inner.tools_generation() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("notification did not proceed after enqueue");
    assert!(!task.is_finished(), "the remote tool has not replied yet");
    let response =
        serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"content":[{"type":"text","text":"ok"}]}});
    remote_write.write_all(format!("{response}\n").as_bytes()).await.unwrap();
    assert!(task.await.unwrap().is_ok());
}

#[tokio::test]
async fn tool_submission_releases_gate_before_waiting_for_remote_response() {
    use rmcp::service::serve_directly_with_ct;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
    let inner = Inner::new(spec, None);
    let (mut remote_write, client_read) = tokio::io::duplex(4096);
    let (client_write, remote_read) = tokio::io::duplex(4096);
    let service = serve_directly_with_ct::<RoleClient, _, _, _, _>(
        inner.handler(),
        (client_read, client_write),
        None,
        CancellationToken::new(),
    );
    let peer = service.peer().clone();
    let sender = inner.clone();
    let task = tokio::spawn(async move {
        call_tool(peer, "echo".into(), serde_json::Map::new(), Duration::from_secs(3), &sender, Some(0)).await
    });
    let mut line = String::new();
    let mut reader = BufReader::new(remote_read);
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await.unwrap().unwrap();
    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(request["method"], "tools/call");
    remote_write.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while inner.tools_generation() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("notification blocked while waiting for the tool response");
    assert!(!task.is_finished(), "the remote tool has not replied yet");
    let response =
        serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":{"content":[{"type":"text","text":"ok"}]}});
    remote_write.write_all(format!("{response}\n").as_bytes()).await.unwrap();
    assert!(task.await.unwrap().is_ok());
}

#[test]
fn mcp_env_values_convert_scalars_and_reject_tables() {
    assert_eq!(env_value("A", &serde_json::json!("x")).unwrap(), "x");
    assert_eq!(env_value("A", &serde_json::json!(3)).unwrap(), "3");
    assert_eq!(env_value("A", &serde_json::json!(true)).unwrap(), "true");
    assert!(env_value("A", &serde_json::json!({ "x": 1 })).unwrap_err().contains("env.A"));
}

#[test]
fn mcp_sse_type_is_a_clear_error_and_timeouts_are_capped() {
    let e = Spec::parse(serde_json::json!({ "url": "http://x/sse", "type": "sse" })).err().unwrap();
    assert!(e.contains("SSE") && e.contains("/mcp"), "{e}");
    let s = Spec::parse(serde_json::json!({ "command": "x", "timeout_ms": 10_000_000 })).ok().unwrap();
    assert_eq!(s.startup, MAX_STARTUP);
    let s = Spec::parse(serde_json::json!({ "command": "x" })).ok().unwrap();
    assert_eq!(s.startup, DEFAULT_STARTUP);
}

#[test]
fn mcp_retryable_http_errors() {
    for e in ["HTTP 503", "Transport send error: HTTP 429 Too Many", "HTTP 408: slow", "error sending request for url"]
    {
        assert!(is_retryable_http(e), "{e}");
    }
    for e in ["HTTP 401", "HTTP 404", "JSON-RPC error: bad"] {
        assert!(!is_retryable_http(e), "{e}");
    }
}

#[tokio::test]
async fn initialize_failure_and_cancel_reclaim_progress_tokens() {
    use rmcp::ServiceExt as _;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    // Case 1: initialize returns JSON-RPC error after sending progress notifications.
    {
        let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
        let inner = Inner::new(spec, None);
        let (mut writer, client_read) = tokio::io::duplex(65536);
        let (client_write, remote_read) = tokio::io::duplex(65536);
        let handler = inner.handler();
        let tracker = handler.progress_tracker.clone();
        let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::new(
            rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
            tracker.clone(),
        );

        let server = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(remote_read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();

            send_progress_notifications(&mut writer, "fail-tok", 3).await;

            let err = serde_json::json!({
                "jsonrpc": "2.0",
                "id": req["id"],
                "error": { "code": -32600, "message": "init failed" }
            });
            writer.write_all(format!("{err}\n").as_bytes()).await.unwrap();
            writer.flush().await.unwrap();
        });

        let res = handler.serve(tracked).await;
        assert!(res.is_err(), "initialize should fail on error response");
        server.await.unwrap();
        assert_eq!(tracker.token_count(), 0, "progress tokens sent before initialize failure must not leak");
    }

    // Case 2: initialize is cancelled after server sends progress notifications.
    {
        let spec = Spec::parse(serde_json::json!({ "command": "unused" })).unwrap();
        let inner = Inner::new(spec, None);
        let (mut writer, client_read) = tokio::io::duplex(65536);
        let (client_write, remote_read) = tokio::io::duplex(65536);
        let handler = inner.handler();
        let tracker = handler.progress_tracker.clone();
        let (recv_tx, mut recv_rx) = tokio::sync::mpsc::unbounded_channel();
        let tracked = crate::mcp::conn_handler::ProgressTrackingTransport::with_progress_received_tx(
            rmcp::transport::IntoTransport::into_transport((client_read, client_write)),
            tracker.clone(),
            recv_tx,
        );

        let _server = tokio::spawn(async move {
            let mut reader = tokio::io::BufReader::new(remote_read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            send_progress_notifications(&mut writer, "cancel-tok", 3).await;
            std::future::pending::<()>().await;
            drop(writer);
        });

        let ct = CancellationToken::new();
        let ct_cancel = ct.clone();
        let cancel_task = tokio::spawn(async move {
            let _cancel = ct_cancel.drop_guard();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                for i in 0..3 {
                    let tok = recv_rx.recv().await.expect("transport must receive progress notification");
                    assert_eq!(
                        tok,
                        ProgressToken(rmcp::model::NumberOrString::String(format!("cancel-tok-{i}").into()))
                    );
                }
            })
            .await
            .expect("timed out waiting for 3 progress notifications before initialize cancel");
        });

        let res = handler.serve_with_ct(tracked, ct).await;
        assert!(res.is_err(), "initialize should fail when cancelled");
        cancel_task.await.unwrap();
        assert_eq!(tracker.token_count(), 0, "progress tokens sent before initialize cancel must not leak");
    }
}

#[tokio::test]
async fn reconnect_resets_progress_tracker_state() {
    let spec = Spec::parse(serde_json::json!({ "command": "nonexistent-mcp-server-for-test" })).unwrap();
    let inner = Inner::new(spec, None);
    let tok = ProgressToken(rmcp::model::NumberOrString::String("stale-reconnect-tok".into()));
    let _seq = inner.progress_tracker.begin(&tok);
    assert_eq!(inner.progress_tracker.token_count(), 1);

    let _ = inner.reconnect().await;
    assert_eq!(inner.progress_tracker.token_count(), 0, "reconnect must reset/reclaim prior tracker state");
}

#[tokio::test]
async fn http_initialize_retry_backoff_sequence_is_pinned() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let times = Arc::new(std::sync::Mutex::new(Vec::<std::time::Instant>::new()));
    let times_srv = times.clone();
    let server = tokio::spawn(async move {
        for _ in 0..3 {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            while let Ok(n) = stream.read(&mut tmp).await {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let Some(hdr_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&buf[..hdr_end]);
                let content_len = headers
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
                    })
                    .unwrap_or(0);
                if buf.len() >= hdr_end + 4 + content_len {
                    break;
                }
            }
            times_srv.lock().unwrap().push(std::time::Instant::now());
            let _ = stream
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await;
            let _ = stream.flush().await;
            let _ = stream.shutdown().await;
        }
    });

    let raw = serde_json::json!({ "url": format!("http://{addr}/mcp") });
    let spec = Spec::parse(raw.clone()).unwrap();
    let inner = Inner::new(Spec::parse(raw).unwrap(), None);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let err = crate::mcp::conn_transport::connect_http(&spec, inner.handler(), deadline).await.err().unwrap();
    server.abort();

    let seen = times.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "1 initial + 2 retries ([250, 1000]ms), err={err}");
    let d1 = seen[1].duration_since(seen[0]);
    let d2 = seen[2].duration_since(seen[1]);
    assert!(d1 >= Duration::from_millis(250) && d1 < Duration::from_millis(900), "first retry delay ~250ms: {d1:?}");
    assert!(
        d2 >= Duration::from_millis(1000) && d2 < Duration::from_millis(2500),
        "second retry delay ~1000ms: {d2:?}"
    );
}

#[tokio::test]
async fn event_queue_full_reconnect_and_close_finish_without_deadlock() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    let n = match stream.read(&mut tmp).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    let Some(hdr_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&buf[..hdr_end]).to_string();
                    let content_len = headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim().eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if buf.len() < hdr_end + 4 + content_len {
                        continue;
                    }
                    let body = &buf[hdr_end + 4..hdr_end + 4 + content_len];
                    let first_line = headers.lines().next().unwrap_or("");
                    let (status, resp_body) = if first_line.starts_with("GET ") {
                        ("405 Method Not Allowed", String::new())
                    } else if first_line.starts_with("DELETE ") {
                        ("200 OK", String::new())
                    } else {
                        let req: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
                        let method = req["method"].as_str().unwrap_or("");
                        if req.get("id").is_none() {
                            ("202 Accepted", String::new())
                        } else {
                            let result = match method {
                                "initialize" => serde_json::json!({
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": {},
                                    "serverInfo": { "name": "test", "version": "1.0" }
                                }),
                                "tools/list" => serde_json::json!({ "tools": [] }),
                                "prompts/list" => serde_json::json!({ "prompts": [] }),
                                "resources/list" => serde_json::json!({ "resources": [] }),
                                "resources/templates/list" => serde_json::json!({ "resourceTemplates": [] }),
                                _ => serde_json::json!({}),
                            };
                            let resp = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": req["id"],
                                "result": result,
                            });
                            ("200 OK", resp.to_string())
                        }
                    };
                    let reply = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    break;
                }
            });
        }
    });

    let raw = serde_json::json!({ "url": format!("http://{addr}/mcp") });
    let inner = Inner::new(Spec::parse(raw).unwrap(), None);
    inner.start().await.unwrap();

    // Simulate transport drop so the next refresh() triggers reconnect with had == true.
    inner.conn.lock().await.svc.take();

    // Fill the bounded event queue completely before the consumer calls refresh().
    for _ in 0..EVENT_CAPACITY {
        inner.tx.try_send(Event::ToolsChanged).unwrap();
    }

    // Trigger many debounced notifications while the queue is full; waiting tasks must stay bounded.
    let handler = inner.handler();
    for _ in 0..50 {
        handler.debounced(0);
    }

    let refresh_res = tokio::time::timeout(Duration::from_secs(2), inner.refresh()).await;
    assert!(refresh_res.is_ok(), "refresh() deadlocked on full event queue during reconnect");
    refresh_res.unwrap().unwrap();

    let close_res = tokio::time::timeout(Duration::from_secs(2), inner.close()).await;
    assert!(close_res.is_ok(), "close() deadlocked after reconnect on full event queue");
    assert!(inner.next_event().await.is_none(), "closed client must return None from next_event()");

    server.abort();
}

#[tokio::test(start_paused = true)]
async fn event_queue_full_preserves_coalesced_list_changed_notifications() {
    use crate::mcp::conn_handler::DEBOUNCE;

    let raw = serde_json::json!({ "url": "http://127.0.0.1:1/mcp" });
    let inner = Inner::new(Spec::parse(raw).unwrap(), None);

    // Fill the bounded event queue completely with elicitations.
    for i in 0..EVENT_CAPACITY {
        inner
            .tx
            .try_send(Event::Elicit { id: i as u64, params: serde_json::json!({}), cancel: CancellationToken::new() })
            .unwrap();
    }

    // Fire tools/prompts/resources_changed (with duplicate tools_changed) while the queue is full.
    let handler = inner.handler();
    handler.debounced(0);
    handler.debounced(0);
    handler.debounced(1);
    handler.debounced(2);

    // Wait for debounce timers to fire while the queue is still full.
    // sleep-ok: virtual time (start_paused) — steps past the debounce deadline deterministically
    tokio::time::sleep(DEBOUNCE + Duration::from_millis(100)).await;

    // Drain all queued elicitations plus the 3 coalesced change notifications.
    let mut elicit_count = 0usize;
    let mut tools_changed = 0usize;
    let mut prompts_changed = 0usize;
    let mut resources_changed = 0usize;

    for _ in 0..(EVENT_CAPACITY + 3) {
        let ev = tokio::time::timeout(Duration::from_secs(1), inner.next_event())
            .await
            .expect("timed out waiting for event after full-queue list_changed")
            .expect("unexpected None from next_event()");
        match ev {
            Event::Elicit { .. } => elicit_count += 1,
            Event::ToolsChanged => tools_changed += 1,
            Event::PromptsChanged => prompts_changed += 1,
            Event::ResourcesChanged => resources_changed += 1,
            Event::Reconnected => panic!("unexpected Reconnected event"),
        }
    }

    assert_eq!(elicit_count, EVENT_CAPACITY);
    assert_eq!(tools_changed, 1, "coalesced ToolsChanged must be delivered once");
    assert_eq!(prompts_changed, 1, "coalesced PromptsChanged must be delivered once");
    assert_eq!(resources_changed, 1, "coalesced ResourcesChanged must be delivered once");

    // No extra duplicate events should remain.
    let extra = tokio::time::timeout(Duration::from_millis(50), inner.next_event()).await;
    assert!(extra.is_err(), "unexpected extra event after draining coalesced notifications: {extra:?}");

    // Also verify next_event() wakes up when a debounced notification arrives while waiting.
    let waiter = tokio::spawn({
        let inner = inner.clone();
        async move { tokio::time::timeout(Duration::from_secs(2), inner.next_event()).await }
    });
    tokio::task::yield_now().await;
    handler.debounced(0);
    let woken = waiter.await.unwrap().expect("next_event() did not wake on list_changed");
    assert!(matches!(woken, Some(Event::ToolsChanged)));

    inner.close().await;
}

#[tokio::test]
async fn continuous_list_changed_does_not_starve_queued_elicitations() {
    let raw = serde_json::json!({ "url": "http://127.0.0.1:1/mcp" });
    let inner = Inner::new(Spec::parse(raw).unwrap(), None);

    // Enqueue several elicitations in the bounded queue.
    for i in 0..4u64 {
        inner
            .tx
            .try_send(Event::Elicit { id: i, params: serde_json::json!({}), cancel: CancellationToken::new() })
            .unwrap();
    }

    // Re-arm tools_changed before every next_event() poll (simulating continuous remote churn).
    let mut elicit_seen = 0usize;
    for _ in 0..16 {
        inner.pending_changed[0].store(true, std::sync::atomic::Ordering::SeqCst);
        inner.changed_notify.notify_one();
        let ev = tokio::time::timeout(Duration::from_secs(1), inner.next_event())
            .await
            .expect("next_event timed out")
            .expect("next_event returned None");
        if matches!(ev, Event::Elicit { .. }) {
            elicit_seen += 1;
        }
    }

    assert_eq!(elicit_seen, 4, "continuous ToolsChanged starved queued elicitations (saw {elicit_seen}/4)");
    inner.close().await;
}

#[tokio::test(start_paused = true)]
async fn elicitation_timeout_covers_full_queue_wait() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let (inner, service, mut writer, mut reader, _seen) = progress_test_client();

    // Fill the bounded event queue so create_elicitation blocks trying to enqueue.
    for i in 0..EVENT_CAPACITY {
        inner
            .tx
            .try_send(Event::Elicit { id: i as u64, params: serde_json::json!({}), cancel: CancellationToken::new() })
            .unwrap();
    }

    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 901,
        "method": "elicitation/create",
        "params": { "mode": "form", "message": "blocked", "requestedSchema": { "type": "object", "properties": {} } }
    });
    writer.write_all(format!("{request}\n").as_bytes()).await.unwrap();
    tokio::task::yield_now().await;

    // Advance past ELICIT_TIMEOUT (600s); deadline must cover the full-queue send wait.
    tokio::time::advance(Duration::from_secs(601)).await;
    tokio::task::yield_now().await;

    let mut line = String::new();
    let read_res = tokio::time::timeout(Duration::from_millis(200), reader.read_line(&mut line)).await;
    assert!(read_res.is_ok(), "elicitation stuck in full queue did not time out within 600s");
    read_res.unwrap().unwrap();
    let answer: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(answer["result"]["action"], "cancel", "{answer}");

    drop(service);
    inner.close().await;
}

#[tokio::test(start_paused = true)]
async fn debounce_reuses_single_task_per_kind_under_burst() {
    use crate::mcp::conn_handler::DEBOUNCE;

    let raw = serde_json::json!({ "url": "http://127.0.0.1:1/mcp" });
    let inner = Inner::new(Spec::parse(raw).unwrap(), None);
    let handler = inner.handler();

    for _ in 0..500 {
        handler.debounced(0);
    }
    assert_eq!(
        inner.active_debounce_tasks.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "burst of 500 notifications must reuse a single debounce task"
    );

    for _ in 0..100 {
        handler.debounced(1);
        handler.debounced(2);
    }
    assert!(
        inner.active_debounce_tasks.load(std::sync::atomic::Ordering::SeqCst) <= 3,
        "at most 1 debounce task per notification kind"
    );

    // sleep-ok: virtual time (start_paused) — steps past the debounce deadline deterministically
    tokio::time::sleep(DEBOUNCE + Duration::from_millis(100)).await;
    assert_eq!(
        inner.active_debounce_tasks.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "debounce tasks must exit after firing"
    );

    inner.close().await;
}

/// `tls.ca_bundle` rejects missing or empty/invalid PEM files
/// with clear diagnostics and accepts a valid PEM certificate bundle.
#[tokio::test]
async fn mcp_tls_ca_bundle_validates_missing_empty_and_valid_pem() {
    use crate::mcp::conn_transport::http_client;

    const VALID_CA_PEM: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBiTCCAS+gAwIBAgIUdvGQexBOhwY//tyHIu6ZgTW2+t0wCgYIKoZIzj0EAwIw\n\
GTEXMBUGA1UEAwwObWFyZSB0ZXN0IGNhIGEwIBcNMjYxMTExMTYzOTQ5WhgPMjEy\n\
NjEwMTgxNjM5NDlaMBkxFzAVBgNVBAMMDm1hcmUgdGVzdCBjYSBhMFkwEwYHKoZI\n\
zj0CAQYIKoZIzj0DAQcDQgAE3cUULCSjlK3uuYxG/IbaPJydK59rD674FkghwzJ3\n\
r0lf87D8IEPFAAPAyBj5T+ePds0g5mi5ofvC7EYoUHpf6KNTMFEwHQYDVR0OBBYE\n\
FMZrSKCNYckZ1q6U2ISVIqamfBkRMB8GA1UdIwQYMBaAFMZrSKCNYckZ1q6U2ISV\n\
IqamfBkRMA8GA1UdEwEB/wQFMAMBAf8wCgYIKoZIzj0EAwIDSAAwRQIhAKO8t59H\n\
QWtI618MpB5Y91p/Rzc1sSQl+956ZQLLKf33AiBj3IeyoMJGFc90S0QvqZjD5WQW\n\
AGdSbQmSOCP+zV+Q6Q==\n\
-----END CERTIFICATE-----\n";

    let dir = tempfile::tempdir().unwrap();

    // 1. Missing bundle path.
    let missing = dir.path().join("nope.pem");
    let spec_missing =
        Spec::parse(serde_json::json!({ "url": "https://127.0.0.1:1/mcp", "tls": { "ca_bundle": missing } })).unwrap();
    let err_missing = http_client(&spec_missing).await.unwrap_err();
    assert!(err_missing.contains("tls.ca_bundle"), "{err_missing}");

    // 2. File without any PEM certificate.
    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, b"not a pem cert\n").unwrap();
    let spec_empty =
        Spec::parse(serde_json::json!({ "url": "https://127.0.0.1:1/mcp", "tls": { "ca_bundle": empty } })).unwrap();
    let err_empty = http_client(&spec_empty).await.unwrap_err();
    assert!(err_empty.contains("tls.ca_bundle") && err_empty.contains("no certificates"), "{err_empty}");

    // 3. Valid PEM bundle builds cleanly.
    let valid = dir.path().join("valid.pem");
    std::fs::write(&valid, VALID_CA_PEM).unwrap();
    let spec_valid =
        Spec::parse(serde_json::json!({ "url": "https://127.0.0.1:1/mcp", "tls": { "ca_bundle": valid } })).unwrap();
    assert!(http_client(&spec_valid).await.is_ok());
}

/// Transparent reconnect in `peer(true)` must NOT bump `tools_generation` when the restarted
/// server advertises identical tool definitions (allowing the triggering call and concurrent
/// calls in the same step to succeed), while genuinely changed tool definitions after restart
/// must still bump generation and reject stale calls with a clear restart message.
#[cfg(unix)]
#[tokio::test]
async fn reconnect_with_unchanged_tools_keeps_generation_and_changed_tools_rejects() {
    let dir = tempfile::tempdir().unwrap();
    let mode_file = dir.path().join("mode.txt");
    std::fs::write(&mode_file, "v1").unwrap();
    let script = dir.path().join("mcp-server.py");
    std::fs::write(
        &script,
        r#"import json, os, sys
mode = open(sys.argv[1]).read().strip()
desc = "echo tool v1" if mode == "v1" else "echo tool v2 CHANGED"
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    mid = msg.get("id")
    method = msg.get("method", "")
    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fake", "version": "1"}
        }}
    elif method == "tools/list":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {
            "tools": [
                {"name": "echo", "description": desc, "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
                {"name": "crash", "description": "exit now", "inputSchema": {"type": "object", "properties": {}}}
            ]
        }}
    elif method == "tools/call":
        name = msg["params"]["name"]
        if name == "crash":
            sys.exit(1)
        text = msg["params"].get("arguments", {}).get("text", "")
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": f"{mode}:{text}"}]}}
    elif mid is not None:
        resp = {"jsonrpc": "2.0", "id": mid, "result": {}}
    else:
        continue
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#,
    )
    .unwrap();

    let raw = serde_json::json!({
        "name": "restart-test",
        "command": "python3",
        "args": [script.to_string_lossy(), mode_file.to_string_lossy()],
    });
    let inner = Inner::new(Spec::parse(raw).unwrap(), None);
    let _ = inner.peer(true).await.unwrap();
    let gen1 = inner.tools_generation();

    // Crash the server process.
    let _ = call_tool(
        inner.peer(true).await.unwrap(),
        "crash".into(),
        serde_json::Map::new(),
        Duration::from_secs(5),
        &inner,
        Some(gen1),
    )
    .await;
    for _ in 0..50 {
        if inner.status().await == "disconnected" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(inner.status().await, "disconnected");

    // 1. Restart with IDENTICAL tool definitions: two concurrent calls carrying the old generation must succeed.
    let (r1, r2) = tokio::join!(
        async {
            let peer = inner.peer(true).await?;
            call_tool(
                peer,
                "echo".into(),
                serde_json::json!({"text": "one"}).as_object().unwrap().clone(),
                Duration::from_secs(5),
                &inner,
                Some(gen1),
            )
            .await
        },
        async {
            let peer = inner.peer(true).await?;
            call_tool(
                peer,
                "echo".into(),
                serde_json::json!({"text": "two"}).as_object().unwrap().clone(),
                Duration::from_secs(5),
                &inner,
                Some(gen1),
            )
            .await
        }
    );
    assert!(r1.is_ok(), "unchanged tools after restart must succeed: {r1:?}");
    assert!(r2.is_ok(), "parallel call with unchanged tools after restart must succeed: {r2:?}");
    assert_eq!(inner.tools_generation(), gen1, "generation must stay unchanged when tool defs match");

    // 2. Crash again and restart with CHANGED tool definitions: stale generation must be rejected.
    std::fs::write(&mode_file, "v2").unwrap();
    let _ = call_tool(
        inner.peer(true).await.unwrap(),
        "crash".into(),
        serde_json::Map::new(),
        Duration::from_secs(5),
        &inner,
        Some(gen1),
    )
    .await;
    for _ in 0..50 {
        if inner.status().await == "disconnected" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let peer = inner.peer(true).await.unwrap();
    let err = call_tool(
        peer,
        "echo".into(),
        serde_json::json!({"text": "stale"}).as_object().unwrap().clone(),
        Duration::from_secs(5),
        &inner,
        Some(gen1),
    )
    .await
    .unwrap_err();
    assert!(
        err.contains("definition changed") && err.contains("restarted"),
        "changed tool definitions after restart must clearly report restart + definition change: {err}"
    );
    assert!(inner.tools_generation() > gen1, "generation must advance when tool definitions changed");
    inner.close().await;
}
