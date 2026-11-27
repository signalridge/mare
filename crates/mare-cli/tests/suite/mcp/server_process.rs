//! The server process: crash and reconnect, environment, stderr and cancellation.

use super::*;

#[tokio::test]
async fn disconnected_client_does_not_restart_inside_deny_read_tool() {
    local(async {
        let env = TestEnv::new().await;
        let secret = env.cwd.join("secret");
        std::fs::write(&secret, "secret").unwrap();
        let fake = Fake::new(&env, "deny-reconnect");
        let spec =
            with(fake.entry(json!({"tools": ["echo", "crash"], "rich": true})), json!({"name": "deny-reconnect"}));
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        global(&h, "SECRET", secret.to_string_lossy().to_string());
        exec(&h, "CL = mare.mcp.connect(SPEC)").await;
        let live: bool = eval(
            &h,
            r#"return mare.exec.run({sandbox = {level = "read-only", network = true,
            deny_read = {SECRET}, accept_partial = {"fs_deny_read"}}}, function()
            return pcall(function() CL:call("echo", {text = "live"}) end)
        end)"#,
        )
        .await;
        assert!(live, "an already connected external IPC client should remain usable");
        exec(&h, "pcall(function() CL:call('crash', {}) end)").await;
        wait_until(10, "MCP server disconnect", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return CL:status() == 'disconnected'").await }
        })
        .await;
        let failures: String = eval(
            &h,
            r#"return mare.exec.run({sandbox = {level = "read-only", network = true,
            deny_read = {SECRET}, accept_partial = {"fs_deny_read"}}}, function()
            local bad = {}
            local probes = {
                refresh = function() return CL:refresh() end,
                call = function() return CL:call("echo", {text = "x"}) end,
                resource = function() return CL:read_resource("test://one") end,
                prompt = function() return CL:get_prompt("greet", {}) end,
            }
            for name, f in pairs(probes) do
                local ok, e = pcall(f)
                if ok or not tostring(e):find("deny_read", 1, true) then bad[#bad + 1] = name end
            end
            return table.concat(bad, ",")
        end)"#,
        )
        .await;
        assert_eq!(failures, "");
        assert_eq!(fake.count("start"), 1, "tool methods must not reopen the client");
        let recovered: bool =
            eval(&h, "CL:reconnect(); return pcall(function() CL:call('echo', {text = 'back'}) end)").await;
        assert!(recovered);
        assert_eq!(fake.count("start"), 2);
        h.shutdown().await;
    })
    .await
}

/// A stdio server that exits is restarted on the next call.
#[tokio::test]
async fn mcp_reconnects_after_a_stdio_server_crash() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "crash");
        let cfg = servers(
            json!({ "fake": with(fake.entry(json!({ "tools": ["echo", "crash"] })), json!({ "tier": "direct" })) }),
        );
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        wait_until(15, "the tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__fake__crash') ~= nil").await }
        })
        .await;
        let version: i64 = eval(&h, "return T:get('mcp__fake__echo')._version").await;
        let (crashed, after): (bool, String) = eval(
            &h,
            r#"local r = T:execute({ id = "c1", name = "mcp__fake__crash", arguments = {} }, {})
               local e = T:execute({ id = "c2", name = "mcp__fake__echo", arguments = { text = "back" } }, {})
               assert(e.is_error ~= true, "unchanged tools after crash must succeed on first call: " .. tostring(e.content))
               return r.is_error == true, type(e.content) == "table" and e.content[1].text or tostring(e.content)"#,
        )
        .await;
        assert!(crashed, "crash tool call must report error");
        assert_eq!(after, "echo: back");
        assert_eq!(
            eval::<i64>(&h, "return T:get('mcp__fake__echo')._version").await,
            version,
            "unchanged tool definitions across reconnect must keep the same tool version"
        );
        assert_eq!(fake.count("start"), 2, "restarted once");
        h.shutdown().await;
    })
    .await
}

/// stdio servers get the scrubbed environment with env_include / env_exclude applied, `env`
/// numbers and booleans converted, and `${VAR}` / `${env:VAR}` / `${VAR:-default}` expanded;
/// a missing variable is listed. A table value is an error, not silently dropped.
#[tokio::test]
async fn mcp_environment_policy_expansion_and_values() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MCPT_KEEP", "k");
        env.set_var("MCPT_DROP", "d");
        env.set_var("MCPT_API_KEY", "secret");
        env.set_var("MCPT_VAL", "v1");
        let fake = Fake::new(&env, "env");
        let dump = env.home.path().join("env.json");
        let dotenv = env.home.path().join("vars.env");
        std::fs::write(&dotenv, "export FROM_FILE=\"file value\"\nOVERRIDDEN=file # comment\n").unwrap();
        let entry = with(
            fake.entry(json!({ "env_dump": dump })),
            json!({
                "tier": "direct",
                "env_exclude": ["MCPT_DROP"],
                "env_include": ["MCPT_API_KEY"],
                "env_file": dotenv,
                "env": { "NUM": 42, "FLAG": true, "A": "${MCPT_VAL}", "B": "${env:MCPT_VAL}", "C": "${MCPT_NOPE:-dflt}",
                         "D": "${MCPT_MISSING}", "OVERRIDDEN": "env" },
            }),
        );
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "fake": entry }))).no_session().build().await;
        expose(&h);
        wait_until(15, "the env dump", || {
            let d = dump.clone();
            async move { std::fs::read_to_string(&d).is_ok_and(|s| s.ends_with('}')) }
        })
        .await;
        let got: Value = serde_json::from_str(&std::fs::read_to_string(&dump).unwrap()).unwrap();
        assert_eq!(got["MCPT_KEEP"], "k");
        assert!(got.get("MCPT_DROP").is_none(), "env_exclude");
        assert_eq!(got["MCPT_API_KEY"], "secret", "env_include keeps a credential-shaped name");
        assert_eq!((got["NUM"].as_str(), got["FLAG"].as_str()), (Some("42"), Some("true")));
        assert_eq!((got["A"].as_str(), got["B"].as_str(), got["C"].as_str()), (Some("v1"), Some("v1"), Some("dflt")));
        assert_eq!((got["FROM_FILE"].as_str(), got["OVERRIDDEN"].as_str()), (Some("file value"), Some("env")));
        let out: String = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req) return table.concat(req.lines, "\n")"#,
        )
        .await;
        assert!(out.contains("missing variables: MCPT_MISSING"), "{out}");
        // a table is a configuration error
        global(
            &h,
            "SPEC",
            mare_shared::lua::json_to_lua(
                h.lua(),
                &with(fake.entry(json!({})), json!({ "env": { "BAD": { "x": 1 } } })),
            )
            .unwrap(),
        );
        let err: String = eval(&h, "local ok, e = pcall(mare.mcp.connect, SPEC); return tostring(e)").await;
        assert!(err.contains("env.BAD"), "{err}");
        h.shutdown().await;
    })
    .await
}

/// stderr lines land in the client's tail and in `<state>/mcp.log`; a timed-out call sends
/// `notifications/cancelled`, and so does a call abandoned by its task.
#[tokio::test]
async fn mcp_stderr_is_captured_and_cancellation_is_sent() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "cancel");
        let spec = with(
            fake.entry(json!({ "tools": ["echo", "slow"], "stderr": ["booting fake", "token=abc123secretvalue"] })),
            json!({ "name": "fake" }),
        );
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        exec(&h, "CL = mare.mcp.connect(SPEC)").await;
        wait_until(WAIT_SECS, "stderr", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #CL:stderr() >= 2").await }
        })
        .await;
        let tail: Vec<String> = eval(&h, "return CL:stderr()").await;
        assert_eq!(tail[0], "booting fake");
        assert!(!tail[1].contains("abc123secretvalue"), "redacted: {tail:?}");
        let log = std::fs::read_to_string(env.home.path().join("state/mcp.log")).unwrap();
        assert!(log.contains("[fake] stderr: booting fake") && !log.contains("abc123secretvalue"), "{log}");

        let err: String =
            eval(&h, "local ok, e = pcall(CL.call, CL, 'slow', {}, { timeout_ms = 300 }); return tostring(e)").await;
        assert!(err.contains("timed out after 300ms"), "{err}");
        wait_until(WAIT_SECS, "the cancellation after the timeout", || {
            let f = fake.lines();
            async move { f.iter().filter(|l| l.starts_with("notify notifications/cancelled")).count() == 1 }
        })
        .await;
        // abandoned: the task running the call is aborted
        exec(&h, r#"CALL_TASK = mare.task.spawn(function() CL:call('slow', {}, { timeout_ms = 600000 }) end)"#).await;
        // abort only once the server has the request: an abort before it is sent cancels nothing
        wait_until(WAIT_SECS, "the abandoned call to reach the server", || {
            let f = fake.lines();
            async move { f.iter().filter(|l| l.as_str() == "tools/call").count() == 2 }
        })
        .await;
        exec(&h, "CALL_TASK:abort()").await;
        wait_until(WAIT_SECS, "the cancellation of the abandoned call", || {
            let f = fake.lines();
            async move { f.iter().filter(|l| l.starts_with("notify notifications/cancelled")).count() == 2 }
        })
        .await;
        exec(&h, "CL:close()").await;
        h.shutdown().await;
    })
    .await
}

/// Streamable HTTP MCP client automatically re-initializes and retries when the server
/// rejects an expired `Mcp-Session-Id` with HTTP 404.
#[tokio::test]
async fn mcp_streamable_http_reinitializes_on_expired_session_404() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    local(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let init_count = Arc::new(AtomicUsize::new(0));
        let expired_404s = Arc::new(AtomicUsize::new(0));
        let call_count = Arc::new(AtomicUsize::new(0));
        let expire_first = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));

        let (ic, ec, cc, ef, sd) =
            (init_count.clone(), expired_404s.clone(), call_count.clone(), expire_first.clone(), shutdown.clone());
        let server_task = tokio::spawn(async move {
            while !sd.load(Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept().await else { break };
                let (ic, ec, cc, ef) = (ic.clone(), ec.clone(), cc.clone(), ef.clone());
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut req_line = String::new();
                        match reader.read_line(&mut req_line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let method = req_line.split_whitespace().next().unwrap_or("").to_string();
                        let mut content_len: usize = 0;
                        let mut session_id: Option<String> = None;
                        let mut close = false;
                        loop {
                            let mut hline = String::new();
                            match reader.read_line(&mut hline).await {
                                Ok(0) | Err(_) => return,
                                Ok(_) => {}
                            }
                            let trimmed = hline.trim();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = trimmed.split_once(':') {
                                let kl = k.trim().to_ascii_lowercase();
                                let vl = v.trim();
                                if kl == "content-length" {
                                    content_len = vl.parse().unwrap_or(0);
                                } else if kl == "mcp-session-id" {
                                    session_id = Some(vl.to_string());
                                } else if kl == "connection" && vl.eq_ignore_ascii_case("close") {
                                    close = true;
                                }
                            }
                        }
                        let mut body = vec![0u8; content_len];
                        if content_len > 0 && reader.read_exact(&mut body).await.is_err() {
                            break;
                        }

                        let (status, extra_hdr, resp_body) = if method == "GET" {
                            ("405 Method Not Allowed", String::new(), String::new())
                        } else if method == "DELETE" {
                            ("204 No Content", String::new(), String::new())
                        } else {
                            let msg: serde_json::Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                            let rpc_method = msg["method"].as_str().unwrap_or("");
                            let id = msg.get("id").cloned();
                            if rpc_method == "initialize" {
                                let n = ic.fetch_add(1, Ordering::SeqCst) + 1;
                                let sid = format!("sess-{n}");
                                let res = json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "protocolVersion": "2025-03-26",
                                        "capabilities": { "tools": {} },
                                        "serverInfo": { "name": "fake-http", "version": "1.0" }
                                    }
                                });
                                (
                                    "200 OK",
                                    format!("Content-Type: application/json\r\nMcp-Session-Id: {sid}\r\n"),
                                    res.to_string(),
                                )
                            } else if session_id.as_deref() == Some("sess-1") && ef.load(Ordering::SeqCst) {
                                ec.fetch_add(1, Ordering::SeqCst);
                                (
                                    "404 Not Found",
                                    "Content-Type: text/plain\r\n".to_string(),
                                    "session expired".to_string(),
                                )
                            } else if rpc_method == "notifications/initialized" || id.is_none() {
                                ("202 Accepted", String::new(), String::new())
                            } else if rpc_method == "tools/list" {
                                let res = json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "tools": [{
                                            "name": "echo",
                                            "description": "echo tool",
                                            "inputSchema": { "type": "object", "properties": {} }
                                        }]
                                    }
                                });
                                ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                            } else if rpc_method == "resources/list" {
                                let res = json!({ "jsonrpc": "2.0", "id": id, "result": { "resources": [] } });
                                ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                            } else if rpc_method == "prompts/list" {
                                let res = json!({ "jsonrpc": "2.0", "id": id, "result": { "prompts": [] } });
                                ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                            } else if rpc_method == "tools/call" {
                                let n = cc.fetch_add(1, Ordering::SeqCst) + 1;
                                let sid = session_id.clone().unwrap_or_default();
                                if n == 1 {
                                    ef.store(true, Ordering::SeqCst);
                                }
                                let res = json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {
                                        "content": [{ "type": "text", "text": format!("ok-{sid}-{n}") }]
                                    }
                                });
                                ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                            } else {
                                let res = json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "error": { "code": -32601, "message": format!("unknown method {rpc_method}") }
                                });
                                ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                            }
                        };

                        let resp = format!(
                            "HTTP/1.1 {status}\r\n{extra_hdr}Content-Length: {}\r\n\r\n{resp_body}",
                            resp_body.len()
                        );
                        if reader.get_mut().write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                        if close {
                            break;
                        }
                    }
                });
            }
        });

        let env = TestEnv::new().await;
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        let spec = json!({ "name": "http-reinit", "url": format!("http://{addr}/mcp") });
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        let (r1, r2): (String, String) = eval(
            &h,
            r#"
            local cl = mare.mcp.connect(SPEC)
            local first = cl:call("echo", {})
            local second = cl:call("echo", {})
            cl:close()
            return mare.json.encode(first), mare.json.encode(second)
            "#,
        )
        .await;
        assert!(r1.contains("ok-sess-1-1"), "first call must use sess-1: {r1}");
        assert!(r2.contains("ok-sess-2-2"), "second call after 404 must reinit to sess-2: {r2}");
        assert_eq!(init_count.load(Ordering::SeqCst), 2, "expected initial + reinit initialize");
        assert!(expired_404s.load(Ordering::SeqCst) >= 1, "expected at least one 404 on expired sess-1");
        shutdown.store(true, Ordering::Relaxed);
        server_task.abort();
        h.shutdown().await;
    })
    .await
}

/// When an MCP Streamable HTTP server returns HTTP 404 for a client reply POST (e.g. an
/// elicitation response) on an expired session, the client must close the dead transport and fail
/// the in-flight `tools/call` promptly without replaying it, then reconnect cleanly on the next
/// operation.
#[tokio::test]
async fn mcp_streamable_http_reply_post_404_fails_inflight_call_and_reconnects_next() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    local(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let init_count = Arc::new(AtomicUsize::new(0));
        let elicit_calls = Arc::new(AtomicUsize::new(0));
        let reply_404s = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (hold_tx, _) = tokio::sync::broadcast::channel::<()>(1);

        let (ic, ec, r404, sd, htx) =
            (init_count.clone(), elicit_calls.clone(), reply_404s.clone(), shutdown.clone(), hold_tx.clone());
        let server_task = tokio::spawn(async move {
            while !sd.load(Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept().await else { break };
                let (ic, ec, r404, htx) = (ic.clone(), ec.clone(), r404.clone(), htx.clone());
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut req_line = String::new();
                        match reader.read_line(&mut req_line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let method = req_line.split_whitespace().next().unwrap_or("").to_string();
                        let mut content_len: usize = 0;
                        let mut session_id: Option<String> = None;
                        let mut close = false;
                        loop {
                            let mut hline = String::new();
                            match reader.read_line(&mut hline).await {
                                Ok(0) | Err(_) => return,
                                Ok(_) => {}
                            }
                            let trimmed = hline.trim();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = trimmed.split_once(':') {
                                let kl = k.trim().to_ascii_lowercase();
                                let vl = v.trim();
                                if kl == "content-length" {
                                    content_len = vl.parse().unwrap_or(0);
                                } else if kl == "mcp-session-id" {
                                    session_id = Some(vl.to_string());
                                } else if kl == "connection" && vl.eq_ignore_ascii_case("close") {
                                    close = true;
                                }
                            }
                        }
                        let mut body = vec![0u8; content_len];
                        if content_len > 0 && reader.read_exact(&mut body).await.is_err() {
                            break;
                        }

                        if method == "GET" {
                            let resp = "HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n";
                            let _ = reader.get_mut().write_all(resp.as_bytes()).await;
                            continue;
                        }
                        if method == "DELETE" {
                            let resp = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
                            let _ = reader.get_mut().write_all(resp.as_bytes()).await;
                            continue;
                        }

                        let msg: serde_json::Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                        let rpc_method = msg.get("method").and_then(|v| v.as_str());
                        let id = msg.get("id").cloned();

                        // Client JSON-RPC Response / Error reply POST (no "method" field):
                        if rpc_method.is_none() && (msg.get("result").is_some() || msg.get("error").is_some()) {
                            if session_id.as_deref() == Some("sess-1") {
                                r404.fetch_add(1, Ordering::SeqCst);
                                let resp = "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: 15\r\n\r\nsession expired";
                                let _ = reader.get_mut().write_all(resp.as_bytes()).await;
                                continue;
                            }
                            let resp = "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n";
                            let _ = reader.get_mut().write_all(resp.as_bytes()).await;
                            continue;
                        }

                        let rpc_method = rpc_method.unwrap_or("");
                        if rpc_method == "tools/call" && msg["params"]["name"] == "elicit" {
                            ec.fetch_add(1, Ordering::SeqCst);
                            // Stream an elicitation request on the SSE response and keep the SSE
                            // stream open (simulating a server waiting for the client's reply on
                            // a session that expired before the reply POST arrived).
                            let elicit_req = json!({
                                "jsonrpc": "2.0",
                                "id": 99,
                                "method": "elicitation/create",
                                "params": {
                                    "message": "Your name?",
                                    "requestedSchema": {
                                        "type": "object",
                                        "properties": { "name": { "type": "string" } }
                                    }
                                }
                            });
                            let chunk = format!("event: message\ndata: {elicit_req}\n\n");
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n",
                                chunk.len()
                            );
                            if reader.get_mut().write_all(head.as_bytes()).await.is_err() {
                                break;
                            }
                            let _ = reader.get_mut().flush().await;
                            let mut hrx = htx.subscribe();
                            let _ = tokio::time::timeout(std::time::Duration::from_secs(30), hrx.recv()).await;
                            break;
                        }

                        let (status, extra_hdr, resp_body) = if rpc_method == "initialize" {
                            let n = ic.fetch_add(1, Ordering::SeqCst) + 1;
                            let sid = format!("sess-{n}");
                            let res = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": { "tools": {} },
                                    "serverInfo": { "name": "fake-http-elicit", "version": "1.0" }
                                }
                            });
                            (
                                "200 OK",
                                format!("Content-Type: application/json\r\nMcp-Session-Id: {sid}\r\n"),
                                res.to_string(),
                            )
                        } else if rpc_method == "notifications/initialized" || id.is_none() {
                            ("202 Accepted", String::new(), String::new())
                        } else if rpc_method == "tools/list" {
                            let res = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "tools": [
                                        { "name": "elicit", "description": "elicit", "inputSchema": { "type": "object", "properties": {} } },
                                        { "name": "echo", "description": "echo", "inputSchema": { "type": "object", "properties": {} } }
                                    ]
                                }
                            });
                            ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                        } else if rpc_method == "tools/call" && msg["params"]["name"] == "echo" {
                            let sid = session_id.clone().unwrap_or_default();
                            let res = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "content": [{ "type": "text", "text": format!("echo-{sid}") }] }
                            });
                            ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                        } else {
                            let res = json!({ "jsonrpc": "2.0", "id": id, "result": {} });
                            ("200 OK", "Content-Type: application/json\r\n".to_string(), res.to_string())
                        };

                        let resp = format!(
                            "HTTP/1.1 {status}\r\n{extra_hdr}Content-Length: {}\r\n\r\n{resp_body}",
                            resp_body.len()
                        );
                        if reader.get_mut().write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                        if close {
                            break;
                        }
                    }
                });
            }
        });

        let env = TestEnv::new().await;
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        let spec = json!({ "name": "http-elicit-404", "url": format!("http://{addr}/mcp") });
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        let started = std::time::Instant::now();
        let (first_ok, first_err, second_ok, second_out): (bool, String, bool, String) = eval(
            &h,
            r#"
            local cl = mare.mcp.connect(SPEC)
            local ev_task = mare.task.spawn(function()
                while true do
                    local e = cl:next_event()
                    if not e then return end
                    if e.type == "elicit" then
                        cl:respond(e.id, { action = "accept", content = { name = "Ada" } })
                    end
                end
            end)
            -- Long timeout: if reply 404 did not close the transport, this would stall for 10s.
            local ok1, r1 = pcall(cl.call, cl, "elicit", {}, { timeout_ms = 10000 })
            local ok2, r2 = pcall(cl.call, cl, "echo", {}, { timeout_ms = 5000 })
            cl:close()
            ev_task:wait()
            return ok1, tostring(r1), ok2, ok2 and mare.json.encode(r2) or tostring(r2)
            "#,
        )
        .await;
        let elapsed = started.elapsed();
        assert!(!first_ok, "in-flight call must fail when reply POST hits 404: {first_err}");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "in-flight call must fail promptly instead of waiting for the 10s timeout (took {elapsed:?}): {first_err}"
        );
        assert_eq!(elicit_calls.load(Ordering::SeqCst), 1, "in-flight call must not be replayed");
        assert!(reply_404s.load(Ordering::SeqCst) >= 1, "reply POST must have hit 404");
        assert!(second_ok, "next call must reconnect cleanly: {second_out}");
        assert!(second_out.contains("echo-sess-2"), "next call must run on new session sess-2: {second_out}");
        assert_eq!(init_count.load(Ordering::SeqCst), 2, "expected initial + reconnect initialize");
        let _ = hold_tx.send(());
        shutdown.store(true, Ordering::Relaxed);
        server_task.abort();
        h.shutdown().await;
    })
    .await
}

/// When a Streamable HTTP handshake receives a session ID from `initialize` and is then abandoned
/// (e.g. `notifications/initialized` fails or the connect attempt times out/is cancelled), mare
/// sends a best-effort `DELETE` for that orphaned session.
#[tokio::test]
async fn mcp_streamable_http_abandoned_handshake_sends_delete_session() {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    local(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let deleted_sessions: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let (ds, sd) = (deleted_sessions.clone(), shutdown.clone());
        let server_task = tokio::spawn(async move {
            while !sd.load(Ordering::Relaxed) {
                let Ok((stream, _)) = listener.accept().await else { break };
                let ds = ds.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    loop {
                        let mut req_line = String::new();
                        match reader.read_line(&mut req_line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        let method = req_line.split_whitespace().next().unwrap_or("").to_string();
                        let mut content_len: usize = 0;
                        let mut session_id: Option<String> = None;
                        let mut close = false;
                        loop {
                            let mut hline = String::new();
                            match reader.read_line(&mut hline).await {
                                Ok(0) | Err(_) => return,
                                Ok(_) => {}
                            }
                            let trimmed = hline.trim();
                            if trimmed.is_empty() {
                                break;
                            }
                            if let Some((k, v)) = trimmed.split_once(':') {
                                let kl = k.trim().to_ascii_lowercase();
                                let vl = v.trim();
                                if kl == "content-length" {
                                    content_len = vl.parse().unwrap_or(0);
                                } else if kl == "mcp-session-id" {
                                    session_id = Some(vl.to_string());
                                } else if kl == "connection" && vl.eq_ignore_ascii_case("close") {
                                    close = true;
                                }
                            }
                        }
                        let mut body = vec![0u8; content_len];
                        if content_len > 0 && reader.read_exact(&mut body).await.is_err() {
                            break;
                        }

                        if method == "DELETE" {
                            if let Some(sid) = session_id {
                                ds.lock().unwrap().push(sid);
                            }
                            let resp = "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n";
                            let _ = reader.get_mut().write_all(resp.as_bytes()).await;
                            continue;
                        }

                        let msg: serde_json::Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                        let rpc_method = msg["method"].as_str().unwrap_or("");
                        let id = msg.get("id").cloned();

                        let (status, extra_hdr, resp_body) = if rpc_method == "initialize" {
                            let res = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": { "tools": {} },
                                    "serverInfo": { "name": "fake-orphan", "version": "1.0" }
                                }
                            });
                            (
                                "200 OK",
                                "Content-Type: application/json\r\nMcp-Session-Id: sess-orphan\r\n".to_string(),
                                res.to_string(),
                            )
                        } else if rpc_method == "notifications/initialized" {
                            // Fail mid-handshake with a non-retryable 400 so initialize's session is abandoned.
                            ("400 Bad Request", "Content-Type: text/plain\r\n".to_string(), "rejected".to_string())
                        } else {
                            ("405 Method Not Allowed", String::new(), String::new())
                        };

                        let resp = format!(
                            "HTTP/1.1 {status}\r\n{extra_hdr}Content-Length: {}\r\n\r\n{resp_body}",
                            resp_body.len()
                        );
                        if reader.get_mut().write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                        if close {
                            break;
                        }
                    }
                });
            }
        });

        let env = TestEnv::new().await;
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        let spec = json!({ "name": "http-orphan", "url": format!("http://{addr}/mcp") });
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        let ok: bool = eval(&h, "local ok = pcall(mare.mcp.connect, SPEC); return ok").await;
        assert!(!ok, "connect must fail when notifications/initialized returns 400");
        wait_until(WAIT_SECS, "DELETE for abandoned handshake session", || {
            let ds = deleted_sessions.clone();
            async move { ds.lock().unwrap().iter().any(|s| s == "sess-orphan") }
        })
        .await;
        shutdown.store(true, Ordering::Relaxed);
        server_task.abort();
        h.shutdown().await;
    })
    .await
}

/// An unavailable MCP server notifies the model only
/// once even if subsequent connection/call attempts fail again.
#[tokio::test]
async fn mcp_server_unavailable_is_told_to_model_only_once() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = servers(json!({
            "broken": { "command": "/nonexistent/mare-mcp-unavailable", "tier": "direct" }
        }));
        let h = env
            .host()
            .builtins(&["mcp"])
            .faux(json!([{ "text": "ok1" }, { "text": "ok2" }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "M", h.service_table("mcp").unwrap());
        wait_until(15, "broken server settles into error state", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"for _, s in ipairs(M:list_servers().data) do
                         if s.name == "broken" and s.state == "error" then return true end
                       end
                       return false"#,
                )
                .await
            }
        })
        .await;

        // First prompt flushes the queued agent:notify message from the initial failure.
        h.prompt("first", json!({})).await.unwrap();
        let before_count: i64 = eval(
            &h,
            r#"local n = 0
               for _, m in ipairs(A:list_messages().data) do
                 for _, b in ipairs(m.content or {}) do
                   if type(b.text) == "string" and b.text:find("MCP server broken is unavailable:", 1, true) then
                     n = n + 1
                   end
                 end
               end
               return n"#,
        )
        .await;
        // Trigger additional failures and a second prompt — unavailable notice must not repeat.
        exec(
            &h,
            r#"T:execute({ id = "r1", name = "list_mcp_resources", arguments = { server = "broken" } }, {})
               T:execute({ id = "r2", name = "list_mcp_resources", arguments = { server = "broken" } }, {})"#,
        )
        .await;
        h.prompt("second", json!({})).await.unwrap();
        let after_count: i64 = eval(
            &h,
            r#"local n = 0
               for _, m in ipairs(A:list_messages().data) do
                 for _, b in ipairs(m.content or {}) do
                   if type(b.text) == "string" and b.text:find("MCP server broken is unavailable:", 1, true) then
                     n = n + 1
                   end
                 end
               end
               return n"#,
        )
        .await;
        assert_eq!(before_count, 1, "first failure notifies model once");
        assert_eq!(after_count, 1, "subsequent failures must not repeat the unavailable notice");
        h.shutdown().await;
    })
    .await
}
