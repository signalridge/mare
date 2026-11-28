use super::*;

#[test]
fn acp_concurrent_sessions_keep_snapshot_journals_isolated() {
    let w1 = tempfile::tempdir().unwrap();
    let w2 = tempfile::tempdir().unwrap();
    let w1_canon = dunce::canonicalize(w1.path()).unwrap();
    let w2_canon = dunce::canonicalize(w2.path()).unwrap();

    let (mut rpc, home) = acp_with_args(
        json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "first.txt", "content": "one" } }], "delay_ms": 20 },
            { "text": "done 1", "delay_ms": 20 },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "second.txt", "content": "two" } }], "delay_ms": 20 },
            { "text": "done 2", "delay_ms": 20 }
        ]),
        &["--permission", "auto"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let s1 = rpc.call_raw("session/new", json!({ "cwd": w1_canon, "mcpServers": [] }), |_| json!(null));
    let sid1 = s1["result"]["sessionId"].as_str().unwrap().to_string();
    let s2 = rpc.call_raw("session/new", json!({ "cwd": w2_canon, "mcpServers": [] }), |_| json!(null));
    let sid2 = s2["result"]["sessionId"].as_str().unwrap().to_string();
    assert_ne!(sid1, sid2);

    // Run both sessions' prompts concurrently in the same `mare acp` process.
    let id1 =
        rpc.request("session/prompt", json!({ "sessionId": sid1, "prompt": [{ "type": "text", "text": "edit 1" }] }));
    let id2 =
        rpc.request("session/prompt", json!({ "sessionId": sid2, "prompt": [{ "type": "text", "text": "edit 2" }] }));
    let (mut r1, mut r2) = (None, None);
    while r1.is_none() || r2.is_none() {
        let m = rpc.read();
        if m.get("method").is_some() {
            continue;
        }
        if m["id"] == id1 {
            r1 = Some(m);
        } else if m["id"] == id2 {
            r2 = Some(m);
        }
    }
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");

    // Verify each session's snapshot journal on disk only recorded its own file.
    let mut journals: Vec<String> = Vec::new();
    fn collect_indexes(dir: &std::path::Path, out: &mut Vec<String>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    collect_indexes(&p, out);
                } else if p.file_name().and_then(|n| n.to_str()) == Some("index.jsonl")
                    && let Ok(text) = std::fs::read_to_string(&p)
                {
                    out.push(text);
                }
            }
        }
    }
    collect_indexes(home.path(), &mut journals);
    assert_eq!(journals.len(), 2, "expected 2 independent snapshot journals, got {journals:?}");
    let p1 = w1_canon.join("first.txt");
    let p2 = w2_canon.join("first.txt");
    let contains_path = |journal: &str, path: &std::path::Path| {
        journal.lines().any(|line| {
            let entry: Value = serde_json::from_str(line).expect("snapshot journal must contain valid JSON lines");
            entry["path"].as_str().is_some_and(|p| std::path::Path::new(p) == path)
        })
    };
    let has_s1_only = journals.iter().filter(|j| contains_path(j, &p1) && !contains_path(j, &p2)).count();
    let has_s2_only = journals.iter().filter(|j| contains_path(j, &p2) && !contains_path(j, &p1)).count();
    assert_eq!(has_s1_only, 1, "journals: {journals:?}");
    assert_eq!(has_s2_only, 1, "journals: {journals:?}");

    rpc.kill_and_wait();
}

#[test]
fn acp_concurrent_sessions_real_ids_and_close_isolation() {
    let (mut rpc, home) = acp(json!([
        { "text": "s1-turn1" },
        { "text": "s2-turn1" },
        { "text": "s2-turn2" }
    ]));
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();

    let r1 = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let s1 = r1["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{r1}")).to_string();
    let r2 = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let s2 = r2["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{r2}")).to_string();
    assert_ne!(s1, s2, "concurrent sessions must have distinct ids");

    // Persist both sessions via prompt
    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s1, "prompt": [{ "type": "text", "text": "hi 1" }] }),
        |_| json!(null),
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");
    let p2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s2, "prompt": [{ "type": "text", "text": "hi 2" }] }),
        |_| json!(null),
    );
    assert_eq!(p2["result"]["stopReason"], "end_turn", "{p2}");

    // Close only s1
    let closed = rpc.call_raw("session/close", json!({ "sessionId": s1 }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    // s1 appears in session/list (closed), s2 remains active and excluded
    let listed = rpc.call_raw("session/list", json!({ "cwd": cwd }), |_| json!(null));
    let arr = listed["result"]["sessions"].as_array().unwrap_or_else(|| panic!("{listed}"));
    assert!(arr.iter().any(|e| e["sessionId"] == s1), "closed s1 should be listed with real durable id: {listed}");
    assert!(!arr.iter().any(|e| e["sessionId"] == s2), "active s2 must remain excluded: {listed}");

    // Prompting closed s1 fails; s2 continues working unaffected
    let p1_after = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s1, "prompt": [{ "type": "text", "text": "after" }] }),
        |_| json!(null),
    );
    assert_eq!(p1_after["error"]["code"], -32002, "a closed session is not active: {p1_after}");

    let p2_after = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s2, "prompt": [{ "type": "text", "text": "still up" }] }),
        |_| json!(null),
    );
    assert_eq!(p2_after["result"]["stopReason"], "end_turn", "{p2_after}");

    rpc.kill_and_wait();

    // Verify persisted session headers match the ACP sessionIds
    let mut header_ids = std::collections::HashSet::new();
    let mut dirs = vec![home.path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("index.jsonl")
                    && let Some(first) =
                        std::fs::read_to_string(&p).ok().and_then(|s| s.lines().next().map(str::to_string))
                    && let Ok(v) = serde_json::from_str::<Value>(&first)
                    && let Some(id) = v.get("session_id").and_then(|x| x.as_str())
                {
                    header_ids.insert(id.to_string());
                }
            }
        }
    }
    assert!(header_ids.contains(&s1), "missing durable header for {s1}: {header_ids:?}");
    assert!(header_ids.contains(&s2), "missing durable header for {s2}: {header_ids:?}");
}

#[test]
fn acp_concurrent_sessions_route_permissions_without_crosstalk() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let cwd1 = dunce::canonicalize(dir1.path()).unwrap();
    let cwd2 = dunce::canonicalize(dir2.path()).unwrap();

    let (mut rpc, _home) = acp_with_args(
        json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "out.txt", "content": "written" } }] },
            { "text": "done" }
        ]),
        &["--permission", "read-only", "--approval-policy", "ask"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let s1 =
        rpc.call_raw("session/new", json!({ "cwd": cwd1, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
    let s2 =
        rpc.call_raw("session/new", json!({ "cwd": cwd2, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

    let req1 =
        rpc.request("session/prompt", json!({ "sessionId": s1, "prompt": [{ "type": "text", "text": "write 1" }] }));
    let req2 =
        rpc.request("session/prompt", json!({ "sessionId": s2, "prompt": [{ "type": "text", "text": "write 2" }] }));

    let mut perm1_id: Option<Value> = None;
    let mut perm2_id: Option<Value> = None;
    let mut prompt_replies = std::collections::HashMap::new();

    // Collect both concurrent permission requests before replying to either.
    while perm1_id.is_none() || perm2_id.is_none() {
        let m = rpc.read();
        if m["method"] == "session/request_permission" {
            let sid = m["params"]["sessionId"].as_str().unwrap_or_default();
            if sid == s1 {
                assert!(perm1_id.is_none(), "duplicate permission request for {s1}");
                perm1_id = Some(m["id"].clone());
            } else if sid == s2 {
                assert!(perm2_id.is_none(), "duplicate permission request for {s2}");
                perm2_id = Some(m["id"].clone());
            } else {
                panic!("unexpected sessionId in permission request: {m}");
            }
        } else if m.get("method").is_none()
            && let Some(id) = m.get("id").and_then(|v| v.as_u64())
        {
            prompt_replies.insert(id, m);
        }
    }

    // Answer s2 first (allow-once), then s1 (reject-once).
    rpc.send(json!({
        "jsonrpc": "2.0",
        "id": perm2_id.unwrap(),
        "result": { "outcome": { "outcome": "selected", "optionId": "allow-once" } }
    }));
    rpc.send(json!({
        "jsonrpc": "2.0",
        "id": perm1_id.unwrap(),
        "result": { "outcome": { "outcome": "selected", "optionId": "reject-once" } }
    }));

    while prompt_replies.len() < 2 {
        let m = rpc.read();
        if m.get("method").is_none()
            && let Some(id) = m.get("id").and_then(|v| v.as_u64())
        {
            prompt_replies.insert(id, m);
        }
    }
    assert_eq!(prompt_replies[&req1]["result"]["stopReason"], "end_turn", "{prompt_replies:?}");
    assert_eq!(prompt_replies[&req2]["result"]["stopReason"], "end_turn", "{prompt_replies:?}");
    assert!(!cwd1.join("out.txt").exists(), "s1 was rejected and must not write out.txt");
    assert_eq!(std::fs::read_to_string(cwd2.join("out.txt")).unwrap(), "written");

    rpc.kill_and_wait();
}

#[test]
fn acp_client_mcp_declarations_are_isolated_and_cleaned_up_per_session() {
    let dir1 = tempfile::tempdir().unwrap();
    let dir2 = tempfile::tempdir().unwrap();
    let cwd1 = dunce::canonicalize(dir1.path()).unwrap();
    let cwd2 = dunce::canonicalize(dir2.path()).unwrap();

    let (mut rpc, home) = acp_with_args(
        json!([
            { "tool_calls": [{ "name": "mcp__srv_echo__echo", "arguments": { "text": "call-1" } }] },
            { "text": "ok-1" },
            { "tool_calls": [{ "name": "mcp__srv_echo__echo", "arguments": { "text": "call-2" } }] },
            { "text": "ok-2" }
        ]),
        &["--permission", "auto"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let log1 = home.path().join("mcp1.log");
    let log2 = home.path().join("mcp2.log");
    let extra_log = home.path().join("extra.log");
    let cfg1 = json!({ "log": log1, "tools": ["echo"] }).to_string();
    let cfg2 = json!({ "log": log2, "tools": ["echo"] }).to_string();

    // Also include a non-alphanumeric name ("srv.extra") to verify normalization without config error.
    // Both s1 and s2 declare "srv_echo" pointing to separate servers to prove per-session isolation.
    let s1 = rpc.call_raw(
        "session/new",
        json!({
            "cwd": cwd1,
            "mcpServers": [
                { "name": "srv_echo", "command": fake_mcp_bin(), "args": ["scripted", cfg1], "env": [] },
                { "name": "srv.extra", "command": fake_mcp_bin(), "args": ["scripted", json!({ "log": extra_log, "tools": ["echo"] }).to_string()], "env": [] }
            ]
        }),
        |_| json!(null),
    )["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    let s2 = rpc.call_raw(
        "session/new",
        json!({
            "cwd": cwd2,
            "mcpServers": [
                { "name": "srv_echo", "command": fake_mcp_bin(), "args": ["scripted", cfg2], "env": [] }
            ]
        }),
        |_| json!(null),
    )["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    let approve = |m: &Value| {
        if m["method"] == "session/request_permission" {
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } })
        } else {
            json!(null)
        }
    };

    // s1 calls its own MCP tool; s2 calls its own MCP tool
    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s1, "prompt": [{ "type": "text", "text": "call 1" }] }),
        approve,
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");

    let p2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s2, "prompt": [{ "type": "text", "text": "call 2" }] }),
        approve,
    );
    assert_eq!(p2["result"]["stopReason"], "end_turn", "{p2}");

    // Closing s1 cleans up s1 without disrupting s2's MCP server
    let closed = rpc.call_raw("session/close", json!({ "sessionId": s1 }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    let p2_after = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s2, "prompt": [{ "type": "text", "text": "call 2 again" }] }),
        approve,
    );
    assert_eq!(p2_after["result"]["stopReason"], "end_turn", "{p2_after}");

    let log1_text = std::fs::read_to_string(&log1).unwrap_or_default();
    let log2_text = std::fs::read_to_string(&log2).unwrap_or_default();
    let extra_text = std::fs::read_to_string(&extra_log).unwrap_or_default();
    assert_eq!(log1_text.matches("tools/call").count(), 1, "s1 MCP server must handle s1 call only: {log1_text}");
    assert_eq!(log2_text.matches("tools/call").count(), 2, "s2 MCP server must handle both s2 calls: {log2_text}");
    assert!(extra_text.contains("tools/list"), "normalized srv.extra server must initialize: {extra_text}");

    rpc.kill_and_wait();
}

#[test]
fn acp_slow_close_does_not_block_other_session_or_allow_premature_resume() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let home = tempfile::tempdir().unwrap();
    let faux_path = home.path().join("faux.json");
    std::fs::write(
        &faux_path,
        json!([
            { "text": "turn-0" },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "out.txt", "content": "ok-b" } }] },
            { "text": "b-done" }
        ])
        .to_string(),
    )
    .unwrap();

    let plugin_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("slow_close.lua"),
        format!(
            r#"return {{
  name = "slow-close",
  apply = function(C)
    C:on("app/shutdown", function()
      if mare.fs.exists(mare.cwd .. "/.slow-close") then
        mare.http.request({{
          url = "http://127.0.0.1:{port}/barrier",
          timeout_ms = 30000,
        }})
      end
    end, {{ timeout_ms = false }})
  end,
}}"#
        ),
    )
    .unwrap();

    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let cwd_a = dunce::canonicalize(dir_a.path()).unwrap();
    let cwd_b = dunce::canonicalize(dir_b.path()).unwrap();
    std::fs::write(cwd_a.join(".slow-close"), "1").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--permission", "read-only", "--approval-policy", "ask", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let s_a =
        rpc.call_raw("session/new", json!({ "cwd": cwd_a, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
    let s_b =
        rpc.call_raw("session/new", json!({ "cwd": cwd_b, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();

    // Persist session A so it is resumable once closed, and advance session B past turn 0.
    let p_a = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s_a, "prompt": [{ "type": "text", "text": "persist a" }] }),
        |_| json!(null),
    );
    assert_eq!(p_a["result"]["stopReason"], "end_turn", "{p_a}");
    let p_b_init = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s_b, "prompt": [{ "type": "text", "text": "init b" }] }),
        |_| json!(null),
    );
    assert_eq!(p_b_init["result"]["stopReason"], "end_turn", "{p_b_init}");

    // Start closing session A; its app/shutdown hook will block on the HTTP barrier.
    let close_id = rpc.request("session/close", json!({ "sessionId": s_a }));
    let (mut barrier_stream, _) = listener.accept().unwrap();
    let mut req_buf = [0u8; 512];
    let _ = barrier_stream.read(&mut req_buf);

    // While session A is mid-shutdown:
    // 1. Resuming session A must be rejected (still closing).
    let resume_during =
        rpc.call_raw("session/resume", json!({ "sessionId": s_a, "cwd": cwd_a, "mcpServers": [] }), |_| json!(null));
    assert_eq!(resume_during["error"]["code"], -32602, "{resume_during}");

    // 2. Listing sessions must exclude the still-closing session A.
    let list_during = rpc.call_raw("session/list", json!({ "cwd": cwd_a }), |_| json!(null));
    let during_arr = list_during["result"]["sessions"].as_array().unwrap();
    assert!(!during_arr.iter().any(|e| e["sessionId"] == s_a), "closing session must not be listed: {list_during}");

    // 3. Session B prompt + approval reply must complete without being blocked by session A's close.
    let mut saw_perm_b = false;
    let p_b = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": s_b, "prompt": [{ "type": "text", "text": "write b" }] }),
        |m| {
            if m["method"] == "session/request_permission" {
                assert_eq!(m["params"]["sessionId"], s_b, "{m}");
                saw_perm_b = true;
                json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } })
            } else {
                json!(null)
            }
        },
    );
    assert!(saw_perm_b, "session B must receive and answer permission request while session A is closing");
    assert_eq!(p_b["result"]["stopReason"], "end_turn", "{p_b}");
    assert_eq!(std::fs::read_to_string(cwd_b.join("out.txt")).unwrap(), "ok-b");

    // Release the barrier so session A finishes closing.
    barrier_stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
    drop(barrier_stream);

    let close_resp = rpc.wait_raw(close_id, |_| json!(null));
    assert!(close_resp.get("result").is_some(), "close should succeed after barrier release: {close_resp}");

    // Once closed, session A is listed and can be resumed.
    let list_after = rpc.call_raw("session/list", json!({ "cwd": cwd_a }), |_| json!(null));
    let after_arr = list_after["result"]["sessions"].as_array().unwrap();
    assert!(after_arr.iter().any(|e| e["sessionId"] == s_a), "closed session should be listed: {list_after}");

    let resume_after =
        rpc.call_raw("session/resume", json!({ "sessionId": s_a, "cwd": cwd_a, "mcpServers": [] }), |_| json!(null));
    assert!(resume_after.get("result").is_some(), "closed session should resume cleanly: {resume_after}");

    rpc.kill_and_wait();
}

#[test]
fn acp_lua_plugins_can_create_load_and_fork_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(dir.path()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let out_file = home.path().join("lua_ops_result.json");
    let trigger_file = home.path().join("run_ops.trigger");

    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    let trigger_lit = serde_json::to_string(trigger_file.to_str().unwrap()).unwrap();
    let out_lit = serde_json::to_string(out_file.to_str().unwrap()).unwrap();
    std::fs::write(
        plugins_dir.join("acp-session-ops.lua"),
        format!(
            r#"
return {{
    name = "acp-session-ops",
    inject = {{ "session", "agent", "task" }},
    apply = function(ctx)
        local ran = false
        local trigger = {trigger_lit}
        local out = {out_lit}
        ctx:on("turn/completed", function()
            if ran or not mare.fs.exists(trigger) then
                return
            end
            ran = true
            ctx.task:spawn(function()
                local ok_all, top_err = pcall(function()
                    local target_id = mare.fs.read(trigger):gsub("%s+$", "")
                    for _ = 1, 200 do
                        if not ctx.agent:activity().busy then
                            break
                        end
                        mare.task.sleep(20)
                    end
                    local cur = ctx.session:info().session_id
                    local result = {{ cur = cur }}
                    local ok_c, created = pcall(function()
                        return ctx.session:create({{ name = "lua-created" }})
                    end)
                    result.create_ok = ok_c
                    result.create = ok_c and created.session_id or tostring(created)

                    local ok_f, forked = pcall(function()
                        return ctx.session:fork({{ session_id = cur, name = "lua-forked" }})
                    end)
                    result.fork_ok = ok_f
                    result.fork = ok_f and forked.session_id or tostring(forked)

                    local ok_l, loaded = pcall(function()
                        return ctx.session:load({{ session_id = target_id }})
                    end)
                    result.load_ok = ok_l
                    result.load = ok_l and loaded.session_id or tostring(loaded)

                    local tmp = out .. ".tmp"
                    mare.fs.write(tmp, mare.json.encode(result))
                    mare.fs.rename(tmp, out)
                end)
                if not ok_all then
                    local tmp = out .. ".tmp"
                    mare.fs.write(tmp, mare.json.encode({{ fatal = tostring(top_err) }}))
                    mare.fs.rename(tmp, out)
                end
            end)
        end)
    end,
}}
"#
        ),
    )
    .unwrap();

    let mut rpc = acp_in_home(
        home.path(),
        json!([
            { "text": "saved-session-turn" },
            { "text": "driver-session-turn" },
            { "text": "created-session-turn" }
        ]),
        &["--permission", "auto"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    // Create and close a persisted session so Lua `ctx.session:load` loads a saved inactive session.
    let saved = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let saved_sid = saved["result"]["sessionId"].as_str().unwrap().to_string();
    let p_saved = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": saved_sid, "prompt": [{ "type": "text", "text": "save me" }] }),
        |_| json!(null),
    );
    assert_eq!(p_saved["result"]["stopReason"], "end_turn", "{p_saved}");
    let close_saved = rpc.call_raw("session/close", json!({ "sessionId": saved_sid }), |_| json!(null));
    assert!(close_saved.get("result").is_some(), "close saved session failed: {close_saved}");

    // Open the driver session, arm the trigger file with `saved_sid`, and run a turn so the plugin
    // invokes `ctx.session:create`, `ctx.session:fork`, and `ctx.session:load`.
    let driver = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let driver_sid = driver["result"]["sessionId"].as_str().unwrap().to_string();
    std::fs::write(&trigger_file, &saved_sid).unwrap();
    let p_driver = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": driver_sid, "prompt": [{ "type": "text", "text": "trigger lua ops" }] }),
        |_| json!(null),
    );
    assert_eq!(p_driver["result"]["stopReason"], "end_turn", "{p_driver}");

    // Wait for the spawned Lua task to finish writing its results. The background task runs three
    // sequential full-host ACP session operations (`create`, `fork`, `load`), each booting a fresh
    // Lua VM and all built-in plugins; scale the base 10s budget on Windows or under coverage
    // instrumentation / oversubscribed parallel gate runs via `scaled_test_timeout`.
    let wait_budget = mare_cli::testkit::scaled_test_timeout(std::time::Duration::from_secs(10));
    let deadline = std::time::Instant::now() + wait_budget;
    while !out_file.exists() && std::time::Instant::now() < deadline {
        // sleep-ok: polls cross-process output file written by background Lua task in child ACP process until deadline
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    assert!(
        out_file.exists(),
        "Lua session operations did not write output file within {}s timeout",
        wait_budget.as_secs()
    );

    let raw = std::fs::read_to_string(&out_file).unwrap();
    let res: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(res["create_ok"], true, "Lua session:create failed under ACP: {res}");
    assert_eq!(res["fork_ok"], true, "Lua session:fork failed under ACP: {res}");
    assert_eq!(res["load_ok"], true, "Lua session:load failed under ACP: {res}");

    let created_sid = res["create"].as_str().unwrap();
    let forked_sid = res["fork"].as_str().unwrap();
    let loaded_sid = res["load"].as_str().unwrap();
    assert_ne!(created_sid, driver_sid, "created session must have a new id");
    assert_ne!(forked_sid, driver_sid, "forked session must have a new id");
    assert_eq!(loaded_sid, saved_sid, "loaded session must match saved_sid");

    // The Lua-created session must be a live ACP session able to accept prompts.
    let p_created = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": created_sid, "prompt": [{ "type": "text", "text": "hello created" }] }),
        |_| json!(null),
    );
    assert_eq!(p_created["result"]["stopReason"], "end_turn", "prompt on Lua-created ACP session failed: {p_created}");

    rpc.kill_and_wait();
}

#[test]
fn acp_sessions_see_multi_orchestrator_during_app_ready() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(dir.path()).unwrap();
    let home = tempfile::tempdir().unwrap();
    let log_file = home.path().join("acp_ready_capacities.jsonl");
    let log_lit = serde_json::to_string(log_file.to_str().unwrap()).unwrap();

    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("acp-ready-cap.lua"),
        format!(
            r#"
return {{
    name = "acp-ready-cap",
    inject = {{ "session", "kernel" }},
    apply = function(ctx)
        ctx:on("app/ready", function()
            local line = mare.json.encode({{
                capacity = ctx.kernel:session_capacity(),
                session_id = ctx.session:info().session_id,
            }}) .. "\n"
            local prev = mare.fs.exists({log_lit}) and mare.fs.read({log_lit}) or ""
            mare.fs.write({log_lit}, prev .. line)
        end)
    end,
}}
"#
        ),
    )
    .unwrap();

    let mut rpc = acp_in_home(home.path(), json!([{ "text": "saved-acp-turn" }]), &["--permission", "auto"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    // 1. `session/new` must attach the Multi orchestrator before `app/ready`.
    let created = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = created["result"]["sessionId"].as_str().unwrap().to_string();
    let p = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "persist for acp load" }] }),
        |_| json!(null),
    );
    assert_eq!(p["result"]["stopReason"], "end_turn", "{p}");
    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    // 2. `session/load` (restore) must also attach the Multi orchestrator before `app/ready`.
    let loaded =
        rpc.call_raw("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(loaded.get("result").is_some(), "session/load failed: {loaded}");

    let raw = std::fs::read_to_string(&log_file).expect("acp_ready_capacities.jsonl must exist");
    let entries: Vec<Value> = raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
    assert_eq!(entries.len(), 2, "expected app/ready entries for both session/new and session/load: {entries:?}");
    for (idx, entry) in entries.iter().enumerate() {
        assert_eq!(
            entry["capacity"], "multi",
            "ACP host #{idx} must already see multi capacity during app/ready: {entry}"
        );
    }

    rpc.kill_and_wait();
}

#[test]
fn repro_issue_202_acp_session_resume_busy_lacks_machine_readable_data() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(dir.path()).unwrap();
    let home = tempfile::tempdir().unwrap();

    let mut rpc1 = acp_in_home(home.path(), json!([{ "text": "persisted" }]), &[], &[]);
    let _ = rpc1.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let sid =
        rpc1.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null))["result"]["sessionId"]
            .as_str()
            .unwrap()
            .to_string();
    let p = rpc1.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "save" }] }),
        |_| json!(null),
    );
    assert_eq!(p["result"]["stopReason"], "end_turn", "{p}");

    // Path 1: same-process already-loaded session (both session/resume and session/load).
    let same_proc =
        rpc1.call_raw("session/resume", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(same_proc.get("error").is_some(), "expected error resuming already-loaded session: {same_proc}");
    let same_proc_load =
        rpc1.call_raw("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(same_proc_load.get("error").is_some(), "expected error loading already-loaded session: {same_proc_load}");

    // Path 2: cross-process busy/locked session (both session/resume and session/load).
    let mut rpc2 = acp_in_home(home.path(), json!([]), &[], &[]);
    let _ = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cross_proc =
        rpc2.call_raw("session/resume", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(cross_proc.get("error").is_some(), "expected error resuming cross-process busy session: {cross_proc}");
    let cross_proc_load =
        rpc2.call_raw("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(
        cross_proc_load.get("error").is_some(),
        "expected error loading cross-process busy session: {cross_proc_load}"
    );

    rpc2.kill_and_wait();
    rpc1.kill_and_wait();

    for (label, resp, expected_reason, expected_substr) in [
        ("same_proc resume", &same_proc, "already_loaded", "already active"),
        ("same_proc load", &same_proc_load, "already_loaded", "already active"),
        ("cross_proc resume", &cross_proc, "locked", "open in another mare process"),
        ("cross_proc load", &cross_proc_load, "locked", "open in another mare process"),
    ] {
        assert_eq!(resp["error"]["code"], -32602, "{label}: {resp}");
        let data = &resp["error"]["data"];
        assert!(data.is_object(), "{label} must return structured data: {resp}");
        assert_eq!(data["code"], "session_busy", "{label}: {resp}");
        assert_eq!(data["reason"], expected_reason, "{label}: {resp}");
        assert_eq!(data["busy"], true, "{label}: {resp}");
        assert_eq!(data["sessionId"], sid, "{label}: {resp}");
        let msg = data["message"].as_str().unwrap_or("");
        assert!(
            msg.contains(expected_substr),
            "{label} must retain human-readable message containing {expected_substr:?}: {resp}"
        );
    }
}
