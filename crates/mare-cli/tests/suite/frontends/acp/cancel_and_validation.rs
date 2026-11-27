use super::*;

#[test]
fn acp_close_session_propagates_stop_unconfirmed_and_releases_once_stop_is_confirmed() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("config");
    let plugin_dir = cfg_dir.join("plugins");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(cfg_dir.join("config.json"), r#"{"shutdown_grace_ms": 50}"#).unwrap();
    std::fs::write(
        plugin_dir.join("hang_shutdown.lua"),
        r#"return {
  name = "hang-shutdown",
  apply = function(C)
    C:on("app/shutdown", function()
      local _, never_rx = mare.task.channel()
      never_rx:recv()
    end, { timeout_ms = false })
  end,
}"#,
    )
    .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(dir.path()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "acp"]).env("MARE_HOME", home.path()).stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let sid =
        rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

    let p = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "persist" }] }),
        |_| json!(null),
    );
    assert_eq!(p["result"]["stopReason"], "end_turn", "{p}");

    let close_resp = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(close_resp.get("error").is_some(), "unconfirmed shutdown must not reply success: {close_resp}");
    let err_data = close_resp["error"]["data"].as_str().unwrap_or("");
    assert!(err_data.contains("stop_unconfirmed"), "expected stop_unconfirmed in error: {close_resp}");

    // the session stays "closing" (refusing resume) only until a later attempt
    // confirms the stop — here the hung app/shutdown listener was dropped with its plugin, so the
    // next attempt confirms — and is then resumable again instead of refused forever.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let resume_after =
            rpc.call_raw("session/resume", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
        if resume_after.get("result").is_some() {
            break;
        }
        let why = resume_after["error"]["data"].as_str().unwrap_or_default().to_string();
        assert!(why.contains("closing"), "only a still-closing session may refuse resume: {resume_after}");
        assert!(std::time::Instant::now() < deadline, "the closing session was never released");
        // sleep-ok: poll interval; no message tells the client a closing session was released
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    rpc.kill_and_wait();
}

/// A user plugin that parks `session/new` for a workspace whose path contains `acpblock` in
/// `app/ready` until the HTTP barrier at `url` answers — the session actor is blocked meanwhile.
fn write_new_session_barrier(home: &std::path::Path, url: &str) {
    let plugins = home.join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("block_new_session.lua"),
        format!(
            r#"
            return {{
                name = "block-new-session-test",
                apply = function(ctx)
                    ctx:on("app/ready", function()
                        if (mare.workspace.dirs()[1] or ""):find("acpblock", 1, true) then
                            mare.http.request({{ url = "{url}", timeout_ms = 30000 }})
                        end
                    end)
                end,
            }}
            "#
        ),
    )
    .unwrap();
}

fn release_barrier(mut barrier: std::net::TcpStream) {
    use std::io::{Read, Write};
    let mut buf = [0u8; 256];
    let _ = barrier.read(&mut buf);
    let _ = barrier.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
}

/// with the session actor provably blocked (a `session/new` parked in a
/// plugin's `app/ready` on an HTTP barrier) and the request queue provably full (later
/// `session/list` requests are refused with "request queue full"), `session/cancel` for a
/// session whose turn is waiting on a permission still ends that turn as `cancelled`.
/// Reference tests: Codex app-server/tests/suite/v2/connection_handling_websocket.rs:457 (a
/// disconnect cancels pending work while the thread is busy); Kimi
/// packages/remote-control/test/remote-control.test.ts:1660 (a cancel overtakes queued work).
/// reference implementation none for a full transport queue.
#[test]
fn acp_cancel_succeeds_while_the_session_actor_is_blocked_and_the_queue_is_full() {
    use std::net::TcpListener;
    use std::process::Stdio;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let home = tempfile::tempdir().unwrap();
    write_new_session_barrier(home.path(), &url);
    let faux_path = home.path().join("faux.json");
    std::fs::write(
        &faux_path,
        serde_json::to_string(&json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "never.txt", "content": "x" } }] },
            { "text": "done" }
        ]))
        .unwrap(),
    )
    .unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::Builder::new().prefix("acpblock").tempdir().unwrap();
    let cwd_b = dunce::canonicalize(dir_b.path()).unwrap();
    let cwd_c = dunce::canonicalize(dir_c.path()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--permission", "read-only", "--approval-policy", "ask", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let s_b =
        rpc.call_raw("session/new", json!({ "cwd": cwd_b, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

    // B's turn waits on a permission request the test never answers.
    let prompt_b =
        rpc.request("session/prompt", json!({ "sessionId": s_b, "prompt": [{ "type": "text", "text": "write" }] }));
    loop {
        let msg = rpc.read();
        if msg.get("method").and_then(|m| m.as_str()) == Some("session/request_permission")
            && msg["params"]["sessionId"] == s_b
        {
            break;
        }
    }

    // Block the session actor: C's start parks in app/ready until the barrier is answered.
    let new_c = rpc.request("session/new", json!({ "cwd": cwd_c, "mcpServers": [] }));
    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });
    let (barrier, _) = accept_rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap().unwrap();

    // Fill the request queue (256 slots) behind the blocked actor until a request is refused.
    let lists: Vec<u64> = (0..300).map(|_| rpc.request("session/list", json!({ "cwd": cwd_b }))).collect();
    let mut refused = false;
    let mut b_reply = None;
    while !refused {
        let m = rpc.read();
        if let Some(id) = m.get("id").and_then(|v| v.as_u64()) {
            if lists.contains(&id) && m["error"]["data"].as_str().is_some_and(|d| d.contains("request queue full")) {
                refused = true;
            }
            assert_ne!(id, new_c, "session/new for C finished while its barrier was held: {m}");
            if id == prompt_b {
                b_reply = Some(m);
            }
        }
    }
    assert!(b_reply.is_none(), "B's turn ended before the cancel: {b_reply:?}");

    // The actor is still blocked and the queue is full: cancel B.
    rpc.notify("session/cancel", json!({ "sessionId": s_b }));
    let resp_b = rpc.wait_raw(prompt_b, |_| json!(null));
    assert_eq!(resp_b["result"]["stopReason"], "cancelled", "cancel must not wait for the blocked actor: {resp_b}");

    release_barrier(barrier);
    let c = rpc.wait_raw(new_c, |_| json!(null));
    assert!(c["result"]["sessionId"].is_string(), "{c}");

    rpc.kill_and_wait();
}

/// a `session/prompt` followed by `session/cancel`, both received while the
/// session actor is blocked (so the prompt has not started), ends as `cancelled` and never
/// reaches the model; a prompt sent after the cancel runs normally.
/// Reference tests: reference implementation (work queued before the
/// claim is dropped and never reaches the model); Codex core/tests/suite/wake_reservation.rs:147.
#[test]
fn acp_cancel_right_after_prompt_covers_the_prompt_not_yet_started() {
    use std::net::TcpListener;
    use std::process::Stdio;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let home = tempfile::tempdir().unwrap();
    write_new_session_barrier(home.path(), &url);
    let faux_path = home.path().join("faux.json");
    std::fs::write(&faux_path, serde_json::to_string(&json!([{ "text": "FIRST MODEL CALL" }])).unwrap()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dir_c = tempfile::Builder::new().prefix("acpblock").tempdir().unwrap();
    let cwd = dunce::canonicalize(dir.path()).unwrap();
    let cwd_c = dunce::canonicalize(dir_c.path()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let sid =
        rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
    let new_c = rpc.request("session/new", json!({ "cwd": cwd_c, "mcpServers": [] }));
    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });
    let (barrier, _) = accept_rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap().unwrap();

    // Both arrive while the actor is parked on C: the prompt is queued, not started.
    let prompt =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hi" }] }));
    rpc.notify("session/cancel", json!({ "sessionId": sid }));
    // ordering barrier on the protocol thread (answered there, not by the blocked actor): both
    // earlier messages have been dispatched
    let _ = rpc.call_raw("authenticate", json!({ "methodId": "none" }), |_| json!(null));
    release_barrier(barrier);

    let mut text = String::new();
    let collect_text = |updates: Vec<Value>, text: &mut String| {
        for u in updates {
            text.push_str(u["content"]["text"].as_str().unwrap_or(""));
        }
    };
    // the actor handles its queue in order: C's start (it was blocked in) answers first
    let (_, u_c) = read_until(&mut rpc, new_c, None);
    collect_text(u_c, &mut text);
    let (resp, u_p) = read_until(&mut rpc, prompt, None);
    collect_text(u_p, &mut text);
    assert_eq!(resp["result"]["stopReason"], "cancelled", "{resp}");
    assert!(!text.contains("FIRST MODEL CALL"), "the cancelled prompt reached the model: {text}");

    // the cancel does not outlive the prompt it covered
    let next =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "again" }] }));
    let (resp, u_n) = read_until(&mut rpc, next, None);
    collect_text(u_n, &mut text);
    assert_eq!(resp["result"]["stopReason"], "end_turn", "{resp}");
    assert!(text.contains("FIRST MODEL CALL"), "the next prompt must reach the model: {text}");

    rpc.kill_and_wait();
}

/// When `session/prompt` is already registered in `in_flight_prompts` and is awaiting inside
/// `agent.prompt` before `claim_prompt` registers a `turn_id`, a `session/cancel` arriving in that
/// window must still cancel the accepted turn (returning `cancelled` without executing any tools)
/// instead of firing `agent.cancel` on an idle host and letting the turn run to `end_turn`.
/// Reference: Kimi `packages/acp-server/src/session.ts:408,512`.
#[test]
fn acp_cancel_while_prompt_is_in_flight_before_turn_claim_cancels_without_running_tools() {
    use std::net::TcpListener;
    use std::process::Stdio;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("block_prompt_before_claim.lua"),
        format!(
            r#"
            return {{
                name = "block-prompt-before-claim-test",
                apply = function(ctx)
                    local blocked_once = false
                    ctx:on("agent/frontend-route", function(ev)
                        if not blocked_once and type(ev) == "table" and ev.op == "prompt" then
                            blocked_once = true
                            mare.http.request({{ url = "{url}", timeout_ms = 30000 }})
                        end
                    end)
                end,
            }}
            "#
        ),
    )
    .unwrap();

    let faux_path = home.path().join("faux.json");
    std::fs::write(
        &faux_path,
        serde_json::to_string(&json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "leaked-by-cancelled-prompt.txt", "content": "MUST_NOT_EXIST" } }] },
            { "text": "TOOL RAN" }
        ]))
        .unwrap(),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(dir.path()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--yolo", "--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let sid =
        rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });

    // Prompt #1 enters `in_flight_prompts`, finishes `begin_turn`, and parks inside
    // `agent/frontend-route` before `claim_prompt` allocates a `turn_id`.
    let prompt_1 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "cancel me" }] }));
    let (barrier, _) = accept_rx.recv_timeout(std::time::Duration::from_secs(20)).unwrap().unwrap();

    // Cancel while prompt #1 is in-flight before turn claim, then flush protocol thread ordering.
    rpc.notify("session/cancel", json!({ "sessionId": sid }));
    let _ = rpc.call_raw("authenticate", json!({ "methodId": "none" }), |_| json!(null));
    release_barrier(barrier);

    let (resp_1, updates_1) = read_until(&mut rpc, prompt_1, None);
    let text_1: String = updates_1.iter().filter_map(|u| u.pointer("/content/text").and_then(|v| v.as_str())).collect();
    assert_eq!(resp_1["result"]["stopReason"], "cancelled", "pre-claim cancel must return cancelled: {resp_1}");
    assert!(
        !cwd.join("leaked-by-cancelled-prompt.txt").exists(),
        "cancelled prompt must not execute write tool; text={text_1:?}"
    );
    assert!(!text_1.contains("TOOL RAN"), "cancelled prompt must not complete model turn: {text_1:?}");

    // Follow-up prompt on the same session runs normally (the earlier cancel does not leak).
    let prompt_2 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run now" }] }));
    let (resp_2, updates_2) = read_until(&mut rpc, prompt_2, None);
    let text_2: String = updates_2.iter().filter_map(|u| u.pointer("/content/text").and_then(|v| v.as_str())).collect();
    assert_eq!(resp_2["result"]["stopReason"], "end_turn", "follow-up prompt must succeed: {resp_2}");
    assert!(cwd.join("leaked-by-cancelled-prompt.txt").exists(), "follow-up prompt must run the script normally");
    assert!(text_2.contains("TOOL RAN"), "follow-up prompt must receive model output: {text_2:?}");

    rpc.kill_and_wait();
}

/// Ported from reference implementation and
/// `packages/acp/acp/tests/turns.spec.ts:72`.
#[test]
fn acp_rejects_additional_dirs_sse_mcp_binary_blob_and_bad_list_cursor_and_reports_max_tokens() {
    let (mut rpc, home) = acp(json!([{ "text": "cut off", "stop": "length" }]));
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();

    // 1. Invalid `additionalDirectories` entry on `session/new` is refused (-32602).
    let bad_dirs = rpc.call_raw(
        "session/new",
        json!({ "cwd": cwd, "mcpServers": [], "additionalDirectories": [cwd.join("missing-additional-dir")] }),
        |_| json!(null),
    );
    assert_eq!(bad_dirs["error"]["code"], -32602, "{bad_dirs}");

    // 2. Legacy `sse` MCP server transport is refused (-32602).
    let bad_sse = rpc.call_raw(
        "session/new",
        json!({
            "cwd": cwd,
            "mcpServers": [{ "type": "sse", "name": "legacy", "url": "https://example.test/sse", "headers": [] }]
        }),
        |_| json!(null),
    );
    assert_eq!(bad_sse["error"]["code"], -32602, "{bad_sse}");

    // 3. Invalid `session/list` cursor is refused (-32602).
    let bad_cursor = rpc.call_raw("session/list", json!({ "cwd": cwd, "cursor": "not-a-cursor" }), |_| json!(null));
    assert_eq!(bad_cursor["error"]["code"], -32602, "{bad_cursor}");

    // 4. Embedded binary `Blob` resource prompt is refused (-32602) without starting a turn.
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();
    let bad_blob = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{ "type": "resource", "resource": { "uri": "file:///a.bin", "blob": "AAAA", "mimeType": "application/octet-stream" } }]
        }),
        |_| json!(null),
    );
    assert_eq!(bad_blob["error"]["code"], -32602, "{bad_blob}");

    // 5. Assistant stop_reason "length" returns `StopReason::MaxTokens` and preserves committed text.
    let pid = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "go" }] }));
    let mut text = String::new();
    let max_tok_resp = loop {
        let m = rpc.read();
        if m["method"] == "session/update" {
            text.push_str(m["params"]["update"]["content"]["text"].as_str().unwrap_or(""));
        }
        if m.get("method").is_none() && m["id"] == pid {
            break m;
        }
    };
    assert_eq!(max_tok_resp["result"]["stopReason"], "max_tokens", "{max_tok_resp}");
    assert!(text.contains("cut off"), "max_tokens turn must keep committed text: {text}");

    rpc.kill_and_wait();
}
