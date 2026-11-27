use super::*;

#[tokio::test]
async fn web_fetch_enforces_size_and_timeout_bounds_and_closes_connections() {
    // 参考测试：reference implementation/packages/web/web-fetch-http/tests/fetch-http.spec.ts:441,473,
    // codex-rs/network-proxy/src/body_budget_tests.rs:10,86,
    // reference implementation/packages/agent-core-v2/test/app/capability/host.test.ts:72
    // - Declared Content-Length > max_response_bytes -> WEB_FETCH_TOO_LARGE
    // - Chunked stream > max_response_bytes -> truncated=true, <= max_response_bytes, early server close
    // - Stalled headers -> WEB_FETCH_TIMEOUT + verified EOF/close
    // - Stalled body -> WEB_FETCH_TIMEOUT + verified EOF/close
    // - Cancelled in-flight body -> WEB_ABORTED + verified EOF/close
    // - Hanging DNS lookup -> WEB_FETCH_TIMEOUT promptly
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    local(async {
        // 1. Declared Content-Length > max_response_bytes (500)
        let srv_cl = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_cl = srv_cl.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut s, _)) = srv_cl.accept().await {
                let _ = read_http_request_head(&mut s).await;
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5000\r\n\r\nhello")
                    .await;
                let _ = s.shutdown().await;
            }
        });

        // 2. Chunked stream (no Content-Length) exceeding max_response_bytes (500): tracks bytes_sent & early close
        let srv_chunk = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_chunk = srv_chunk.local_addr().unwrap().port();
        let chunk_sent = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (chunk_closed_tx, chunk_closed_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        let chunk_sent_c = chunk_sent.clone();
        tokio::spawn(async move {
            let res = async {
                let (mut s, _) = srv_chunk.accept().await.map_err(|e| e.to_string())?;
                read_http_request_head(&mut s).await?;
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .await
                    .map_err(|e| e.to_string())?;
                let chunk = vec![b'a'; 200];
                let mut sink = [0u8; 64];
                for _ in 0..20 {
                    let hdr = format!("{:x}\r\n", chunk.len());
                    if s.write_all(hdr.as_bytes()).await.is_err()
                        || s.write_all(&chunk).await.is_err()
                        || s.write_all(b"\r\n").await.is_err()
                    {
                        return Ok(());
                    }
                    chunk_sent_c.fetch_add(chunk.len(), std::sync::atomic::Ordering::SeqCst);
                    tokio::select! {
                        r = s.read(&mut sink) => {
                            return match r {
                                Ok(0) => Ok(()),
                                Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof) => Ok(()),
                                Ok(n) => Err(format!("unexpected {n} bytes after request")),
                                Err(e) => Err(format!("read error: {e}")),
                            };
                        }
                        // sleep-ok: pace chunked stream to simulate slow upstream so client closes after max_response_bytes before all chunks buffer
                        _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
                    }
                }
                Err("client never closed chunked stream".into())
            }
            .await;
            let _ = chunk_closed_tx.send(res);
        });

        // 3. Stalled headers -> timeout & close
        let srv_hdr = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_hdr = srv_hdr.local_addr().unwrap().port();
        let (hdr_closed_tx, hdr_closed_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        tokio::spawn(async move {
            let res = async {
                let (mut s, _) = srv_hdr.accept().await.map_err(|e| e.to_string())?;
                read_http_request_head(&mut s).await?;
                assert_peer_closed(&mut s).await
            }
            .await;
            let _ = hdr_closed_tx.send(res);
        });

        // 4. Stalled body -> timeout & close
        let srv_body = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_body = srv_body.local_addr().unwrap().port();
        let (body_closed_tx, body_closed_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        tokio::spawn(async move {
            let res = async {
                let (mut s, _) = srv_body.accept().await.map_err(|e| e.to_string())?;
                read_http_request_head(&mut s).await?;
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\npart")
                    .await
                    .map_err(|e| e.to_string())?;
                assert_peer_closed(&mut s).await
            }
            .await;
            let _ = body_closed_tx.send(res);
        });

        // 5. Cancelled mid-body -> WEB_ABORTED & close (server waits mid-body until client aborts read)
        let srv_cancel = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port_cancel = srv_cancel.local_addr().unwrap().port();
        let (cancel_closed_tx, mut cancel_closed_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        tokio::spawn(async move {
            let res = async {
                let (mut s, _) = srv_cancel.accept().await.map_err(|e| e.to_string())?;
                read_http_request_head(&mut s).await?;
                s.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\npart")
                    .await
                    .map_err(|e| e.to_string())?;
                s.flush().await.map_err(|e| e.to_string())?;
                assert_peer_closed(&mut s).await
            }
            .await;
            let _ = cancel_closed_tx.send(res);
        });

        let env = TestEnv::new().await;
        let _guard = ResolverGuard;
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "slow-dns.test".to_string(),
            vec!["127.0.0.1".parse().unwrap()],
        )])));
        let mut b = env
            .host().builtins(&["web", "web-fetch-http"])
            .no_session()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "auto",
                        "network": { "default": "deny", "allow": ["127.0.0.1", "slow-dns.test"], "allow_local": true }
                    },
                    "approvals": { "policy": "never" },
                    "web-fetch-http": { "max_response_bytes": 500, "max_output_bytes": 500, "timeout_ms": 150 }
                }
            }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "U_CL", format!("http://127.0.0.1:{port_cl}/"));
        global(&h, "U_CHUNK", format!("http://127.0.0.1:{port_chunk}/"));
        global(&h, "U_HDR", format!("http://127.0.0.1:{port_hdr}/"));
        global(&h, "U_BODY", format!("http://127.0.0.1:{port_body}/"));

        let (ok_cl, err_cl, ch_len, ch_trunc, ok_hdr, err_hdr, ok_body, err_body): (
            bool,
            String,
            usize,
            bool,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local tok = mare.task.token()
            local opts = { cancel = function() return tok end }
            local ok_cl, err_cl = pcall(function() return C.cordis:get('web'):fetch(U_CL, opts) end)
            local ch = C.cordis:get('web'):fetch(U_CHUNK, opts)
            local ok_hdr, err_hdr = pcall(function() return C.cordis:get('web'):fetch(U_HDR, opts) end)
            local ok_body, err_body = pcall(function() return C.cordis:get('web'):fetch(U_BODY, opts) end)
            return ok_cl, tostring(err_cl), #ch.content, ch.truncated, ok_hdr, tostring(err_hdr), ok_body, tostring(err_body)
            "#,
        )
        .await;

        assert!(!ok_cl && err_cl.contains("WEB_FETCH_TOO_LARGE"), "{err_cl}");
        assert!(ch_trunc && ch_len == 500, "len={ch_len} trunc={ch_trunc}");
        chunk_closed_rx.await.unwrap().expect("chunked connection did not close cleanly");
        assert!(chunk_sent.load(std::sync::atomic::Ordering::SeqCst) < 4000, "server kept sending after cap");

        assert!(!ok_hdr && err_hdr.contains("WEB_FETCH_TIMEOUT"), "{err_hdr}");
        hdr_closed_rx.await.unwrap().expect("stalled-header connection did not close cleanly");

        assert!(!ok_body && err_body.contains("WEB_FETCH_TIMEOUT"), "{err_body}");
        body_closed_rx.await.unwrap().expect("stalled-body connection did not close cleanly");

        // 6. Hanging DNS lookup must be interrupted by timeout_ms = 150ms (well under 5s delay).
        mare_binding::netauth::set_resolver_delay(Some(std::time::Duration::from_secs(5)));
        let start = std::time::Instant::now();
        let lua = h.lua();
        let tok = std::rc::Rc::new(lua.create_userdata(mare_binding::task::Token(tokio_util::sync::CancellationToken::new())).unwrap());
        let opts = lua.create_table().unwrap();
        opts.set("cancel", lua.create_function(move |_, ()| Ok(tok.as_ref().clone())).unwrap()).unwrap();
        let req = lua.create_string("http://slow-dns.test/").unwrap();
        let err_dns = call_service_confined(
            &h,
            "web",
            "fetch",
            mlua::MultiValue::from_vec(vec![mlua::Value::String(req), mlua::Value::Table(opts)]),
        )
        .await
        .unwrap_err()
        .to_string();
        let dns_elapsed = start.elapsed();
        mare_binding::netauth::set_resolver_delay(None);
        assert!(err_dns.contains("WEB_FETCH_TIMEOUT"), "{err_dns}");
        assert!(dns_elapsed < std::time::Duration::from_secs(2), "DNS wait ignored timeout_ms: {dns_elapsed:?}");

        h.shutdown().await;

        // 5. Cancel mid-body -> verify server waits mid-body, cancellation aborts read(), and socket closes promptly
        // without waiting for server EOF or timeout_ms (5s).
        let mut b_cancel = env
            .host().builtins(&["web", "web-fetch-http"])
            .no_session()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "auto",
                        "network": { "default": "deny", "allow": ["127.0.0.1"], "allow_local": true }
                    },
                    "approvals": { "policy": "never" },
                    "web-fetch-http": { "max_response_bytes": 500, "max_output_bytes": 500, "timeout_ms": 5000 }
                }
            }));
        b_cancel.opts.project_trusted = true;
        let h_cancel = b_cancel.build().await;
        let lua = h_cancel.lua();

        let mid_body_ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let read_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let first_chunk = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let second_read_err = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));

        let mid_body_ready_c = mid_body_ready.clone();
        let read_calls_c = read_calls.clone();
        let first_chunk_c = first_chunk.clone();
        let second_read_err_c = second_read_err.clone();
        lua.globals()
            .set(
                "__on_mid_body_read",
                lua.create_function(move |_, ()| {
                    mid_body_ready_c.notify_one();
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();
        lua.globals()
            .set(
                "__record_first_chunk",
                lua.create_function(move |_, s: Option<String>| {
                    *first_chunk_c.lock().unwrap() = s;
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();
        lua.globals()
            .set(
                "__record_read_call",
                lua.create_function(move |_, ()| {
                    Ok(read_calls_c.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
                })
                .unwrap(),
            )
            .unwrap();
        lua.globals()
            .set(
                "__record_second_read_err",
                lua.create_function(move |_, s: String| {
                    *second_read_err_c.lock().unwrap() = Some(s);
                    Ok(())
                })
                .unwrap(),
            )
            .unwrap();
        lua.load(
            r#"
            local orig_req = mare.http.request
            mare.http.request = function(opts)
                local resp = orig_req(opts)
                local proxy = {
                    ok = resp.ok,
                    status = resp.status,
                    headers = resp.headers,
                    url = resp.url,
                }
                function proxy:read()
                    local n = __record_read_call()
                    if n == 1 then
                        local chunk = resp:read()
                        __record_first_chunk(chunk)
                        return chunk
                    end
                    if n == 2 then
                        __on_mid_body_read()
                        local ok, res = pcall(resp.read, resp)
                        if not ok then
                            __record_second_read_err(tostring(res))
                            error(res, 0)
                        end
                        return res
                    end
                    return resp:read()
                end
                function proxy:close()
                    return resp:close()
                end
                return proxy
            end
            "#,
        )
        .exec()
        .unwrap();

        let opts = lua.create_table().unwrap();
        let cancel_tok = tokio_util::sync::CancellationToken::new();
        let cancel_ud = lua.create_userdata(mare_binding::task::Token(cancel_tok.clone())).unwrap();
        opts.set("cancel", lua.create_function(move |_, ()| Ok(cancel_ud.clone())).unwrap()).unwrap();
        let req = lua.create_string(format!("http://127.0.0.1:{port_cancel}/")).unwrap();
        let call = call_service_confined(
            &h_cancel,
            "web",
            "fetch",
            mlua::MultiValue::from_vec(vec![mlua::Value::String(req), mlua::Value::Table(opts)]),
        );
        tokio::pin!(call);
        let (cancel_res, cancel_started) = tokio::select! {
            r = &mut call => panic!("cancelled fetch finished before mid-body read barrier: {r:?}"),
            ready_res = tokio::time::timeout(std::time::Duration::from_secs(2), mid_body_ready.notified()) => {
                ready_res.expect("timed out waiting for client to enter mid-body read()");
                assert!(
                    cancel_closed_rx.try_recv().is_err(),
                    "server connection closed before cancellation was triggered"
                );
                let started = std::time::Instant::now();
                cancel_tok.cancel();
                let res = tokio::time::timeout(std::time::Duration::from_secs(2), &mut call)
                    .await
                    .expect("cancelled fetch did not abort promptly");
                (res, started)
            }
        };
        let err_cancel = cancel_res.unwrap_err().to_string();
        assert!(err_cancel.contains("WEB_ABORTED"), "expected WEB_ABORTED, got: {err_cancel}");
        assert_eq!(first_chunk.lock().unwrap().as_deref(), Some("part"));
        assert_eq!(read_calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let read_err = second_read_err.lock().unwrap().clone().expect("second read() did not error on cancel");
        assert!(read_err.to_lowercase().contains("aborted"), "expected read() abort error, got: {read_err}");
        cancel_closed_rx.await.unwrap().expect("cancelled-body connection did not close cleanly");
        assert!(
            cancel_started.elapsed() < std::time::Duration::from_secs(1),
            "cancellation + socket close took too long (waited for timeout?): {:?}",
            cancel_started.elapsed()
        );

        h_cancel.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn web_fetch_rpc_maps_web_fetch_timeout_to_timed_out() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    local(async {
        let srv_hdr = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = srv_hdr.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
        tokio::spawn(async move {
            let res = async {
                let (mut s, _) = srv_hdr.accept().await.map_err(|e| e.to_string())?;
                read_http_request_head(&mut s).await?;
                assert_peer_closed(&mut s).await
            }
            .await;
            let _ = closed_tx.send(res);
        });

        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["web", "web-fetch-http"]).no_session().config(json!({
            "plugins": {
                "permissions": {
                    "enabled": true,
                    "preset": "auto",
                    "network": { "default": "deny", "allow": ["127.0.0.1"], "allow_local": true }
                },
                "approvals": { "policy": "never" },
                "web-fetch-http": { "timeout_ms": 150 }
            }
        }));
        b.opts.mode = mare_shared::ui::Mode::Rpc;
        b.opts.project_trusted = true;
        let h = b.build().await;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (srv_r, srv_w) = tokio::io::split(server_io);
        let (cli_r, mut cli_w) = tokio::io::split(client_io);
        let stop = tokio_util::sync::CancellationToken::new();
        let stop_c = stop.clone();
        let srv_task = tokio::task::spawn_local(mare_cli::rpc::serve(h.clone(), srv_r, srv_w, true, async move {
            stop_c.cancelled().await;
        }));

        let mut lines = tokio::io::BufReader::new(cli_r).lines();
        let mut rpc_call = async |id: u64, method: &str, params: serde_json::Value| -> serde_json::Value {
            let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            cli_w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
            cli_w.flush().await.unwrap();
            loop {
                let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                    .await
                    .expect("RPC response timed out")
                    .unwrap()
                    .expect("RPC stream closed");
                let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
                if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    return msg;
                }
            }
        };

        let init = rpc_call(1, "initialize", json!({ "protocol_version": "3.0", "capabilities": {} })).await;
        assert!(init.get("result").is_some(), "initialize failed: {init}");

        let resp = rpc_call(2, "web.fetch", json!({ "url": format!("http://127.0.0.1:{port}/") })).await;
        assert_eq!(resp["error"]["code"], json!(-32000), "unexpected RPC response: {resp}");
        assert_eq!(resp["error"]["data"]["code"], json!("timed_out"), "unexpected RPC response: {resp}");
        let err_msg = resp["error"]["message"].as_str().unwrap_or("");
        assert!(err_msg.contains("WEB_FETCH_TIMEOUT"), "unexpected RPC response: {resp}");
        closed_rx.await.unwrap().expect("stalled-header connection did not close cleanly");

        let _ = rpc_call(3, "shutdown", json!({})).await;
        stop.cancel();
        drop(cli_w);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), srv_task).await;
        h.shutdown().await;
    })
    .await
}

/// The same over RPC, the way a front end reloads: init.lua gains new `web-fetch-http` options,
/// `command.run /reload`, then `web.fetch` must use them (plugin repo e2e-web-rpc.py:537).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_fetch_http_follows_its_options_across_an_rpc_reload() {
    let server = MockServer::start().await;
    server.route("/plain", vec![resp_with(200, "text/plain", "landed"); 4]);
    let target = format!("{}/plain", server.base);
    server.route("/redirect", vec![redirect_resp(&target); 4]);
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
    let init = home.path().join("config/init.lua");
    std::fs::write(&init, "").unwrap();
    std::fs::write(
        home.path().join("config/plugins/reload-gate.lua"),
        r#"return { name = "reload-gate", apply = function(ctx)
  ctx:effect(function() return function()
    local gate = os.getenv("MARE_TEST_RELOAD_GATE")
    local f = assert(io.open(gate .. "/started", "w")); f:close()
    for _ = 1, 3000 do
      if mare.fs.exists(gate .. "/release") then return end
      mare.task.sleep(10)
    end
    error("reload gate timed out")
  end end)
end }"#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--yolo", "--rpc"])
        .env("MARE_HOME", home.path())
        .env("MARE_TEST_RELOAD_GATE", home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    // a saved session, as the e2e has: `/reload` re-establishes it and the client resumes it
    let owner = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    let s_start =
        rpc.call_raw("agent.prompt", json!({ "session_id": owner["session_id"], "input": "save the session" }), |_| {
            json!(null)
        });
    assert!(s_start["error"].is_null(), "{s_start}");
    let saved = rpc.call_raw(
        "agent.wait",
        json!({ "session_id": owner["session_id"], "turn_id": s_start["result"]["turn_id"] }),
        |_| json!(null),
    );
    assert!(saved["error"].is_null(), "{saved}");
    let url = format!("{}/redirect", server.base);
    let before = rpc.call_raw("web.fetch", json!({ "url": url }), |_| json!(null));
    assert_eq!(before["result"]["content"], "landed", "{before}");
    // the e2e's runtime writes to the web plugin's config before the reload
    for text in ["/web fetch http-text", "/web fetch auto"] {
        let r = rpc.call_raw("commands.run", json!({ "line": text }), |_| json!(null));
        assert!(r["error"].is_null(), "{text}: {r}");
        assert!(r["result"].is_object(), "{text}: {r}");
    }
    std::fs::write(&init, r#"mare.plugin("web-fetch-http", { redirect = "manual" })"#).unwrap();
    let reload = rpc.call_raw("commands.run", json!({ "line": "/reload" }), |_| json!(null));
    assert!(reload["error"].is_null(), "{reload}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !home.path().join("started").exists() {
        assert!(std::time::Instant::now() < deadline, "reload never entered plugin cleanup");
        // sleep-ok: bounded poll for the cleanup's explicit entry marker
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // status.get is exempt from the reload gate; its reply proves the single resume request was
    // read while cleanup was held, without discarding a premature resume response.
    let resume_id = rpc.request("session.load", json!({ "session_id": owner["session_id"] }));
    let status_id = rpc.request("status.get", json!({}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let msg = rpc.read_deadline(deadline).expect("status barrier during reload");
        assert_ne!(msg["id"], resume_id, "resume finished before reload was released: {msg}");
        if msg["id"] == status_id {
            assert!(msg["error"].is_null(), "{msg}");
            break;
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    loop {
        match rpc.read_deadline(deadline) {
            Ok(msg) => assert_ne!(msg["id"], resume_id, "resume did not wait for reload: {msg}"),
            Err(error) if error == "timed out waiting for RPC line" => break,
            Err(error) => panic!("waiting for resume during reload: {error}"),
        }
    }
    std::fs::write(home.path().join("release"), "").unwrap();
    let resumed = rpc.wait_timeout(resume_id, std::time::Duration::from_secs(20), |_| json!(null)).unwrap();
    assert!(resumed["error"].is_null(), "{resumed}");
    let after = rpc.call_raw("web.fetch", json!({ "url": url }), |_| json!(null));
    rpc.kill_and_wait();
    let msg = after["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("HTTP 302"), "the reloaded `redirect = manual` did not reach the backend: {after}");
}

/// A reload while a turn runs its tools never wedges the conversation: requests that reach the
/// plugin tree wait for the reload (they never see it half done), while cancelling the turn does
/// not wait — `abort` answers during the reload, the turn ends, the reload completes and the next
/// prompt runs. (A plugin whose cleanup takes 3 s keeps the reload in progress meanwhile.) The
/// requirement of the coordinator's ruling; nothing to port: reference implementation Codex have no reload of a
/// running host (see the commit message).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_during_a_turn_keeps_the_turn_cancellable_and_completes() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("slow-cleanup.lua"),
        r#"return { name = "slow-cleanup", apply = function(ctx)
  ctx:effect(function() return function() mare.task.sleep(3000) end end)
end }"#,
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "test -f reloaded || sleep 30" } }] },
            { "text": "after the reload" }
        ])
        .to_string(),
    )
    .unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.path().display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut rpc = crate::common::rpc::RpcClient::spawn(&["--model", "faux/echo", "--yolo", "--rpc"], &env, home.path());
    let init = rpc.call("initialize", json!({"protocol_version": "3.0"}));
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    rpc.call("events.subscribe", json!({ "scope": "all", "include": ["item/started"] }));
    let started = rpc.call("agent.prompt", json!({ "session_id": sid, "input": "run a long tool" }));
    assert_eq!(started["status"], "started", "{started}");
    // the turn is in its long bash tool (it no longer holds the request lease) before /reload; the
    // timings are measured from t0 after this
    loop {
        let ev = rpc.wait_event("item/started");
        if ev["data"]["item"]["type"] == "tool_call" {
            break;
        }
    }
    let t0 = std::time::Instant::now();
    rpc.call("commands.run", json!({ "line": "/reload" }));
    rpc.call("agent.cancel", json!({ "session_id": sid }));
    let abort_ms = t0.elapsed().as_millis();
    rpc.call("agent.get", json!({ "session_id": sid }));
    rpc.call("agent.list_messages", json!({ "session_id": sid }));
    let info_ms = t0.elapsed().as_millis();
    // the reloaded faux provider plays its script from the start: this time the tool returns
    std::fs::write(home.path().join("reloaded"), "").unwrap();
    let next_s = rpc.call("agent.prompt", json!({ "session_id": sid, "input": "again" }));
    let next = rpc.call("agent.wait", json!({ "session_id": sid, "turn_id": next_s["turn_id"] }));
    rpc.shutdown_and_wait();
    assert!(abort_ms < 2000, "abort waited for the reload: {abort_ms} ms");
    assert!(info_ms >= 2500, "agent.list_messages did not wait for the reload: {info_ms} ms");
    assert_eq!(next["text"], "after the reload", "the next prompt runs: {next}");
}
