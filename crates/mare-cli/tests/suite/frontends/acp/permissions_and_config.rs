use super::*;

#[test]
fn acp_prompt_preserves_origin_and_does_not_attach_files_without_permissions() {
    let home = tempfile::tempdir().unwrap();
    let faux_path = home.path().join("faux.json");
    std::fs::write(&faux_path, json!([{ "text": "ok" }]).to_string()).unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(cfg_dir.join("init.lua"), r#"mare.plugin("permissions", { enabled = false })"#).unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("secret.txt"), "acp-top-secret-body").unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["protocolVersion"], 1, "{init}");
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let p = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "inspect @secret.txt" }] }),
        |_| json!(null),
    );
    assert_eq!(p["result"]["stopReason"], "end_turn", "{p}");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    rpc.kill_and_wait();

    let mut session_contents = String::new();
    let mut stack = vec![home.path().to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && path.file_name().and_then(|n| n.to_str()) != Some("index.jsonl")
                {
                    session_contents.push_str(&std::fs::read_to_string(&path).unwrap());
                }
            }
        }
    }
    assert!(!session_contents.is_empty(), "expected persisted ACP session jsonl");
    assert!(
        !session_contents.contains("acp-top-secret-body"),
        "ACP prompt without permissions must not read or attach @secret.txt: {session_contents}"
    );
    let user_msg = session_contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["type"] == "message" && v["message"]["role"] == "user")
        .unwrap_or_else(|| panic!("expected persisted user message: {session_contents}"));
    assert_eq!(user_msg["message"]["source"], "acp", "ACP prompt must preserve source=acp: {user_msg}");
    assert_eq!(
        user_msg["message"]["content"][1],
        json!({ "type": "mention", "path": "secret.txt" }),
        "ACP prompt without permissions must emit only mention block: {user_msg}"
    );
}

/// ACP `session/prompt` (`source = "acp"`) is user input and must reset `jobs.max_wakeups`.
#[test]
fn acp_prompt_resets_jobs_max_wakeups() {
    let home = tempfile::tempdir().unwrap();
    let faux_path = home.path().join("faux.json");
    std::fs::write(
        &faux_path,
        json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg1", "command": "while [ ! -f .gate1 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-1" },
            { "text": "woke-1" },
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg2", "command": "while [ ! -f .gate2 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-2" },
            { "text": "woke-2" }
        ])
        .to_string(),
    )
    .unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        "mare.setup { plugins = { jobs = { max_wakeups = 1, batch_ms = 50 } } }\n",
    )
    .unwrap();
    let ws = tempfile::tempdir().unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--yolo", "--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["protocolVersion"], 1, "{init}");
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let read_sessions = || -> String {
        let mut out = String::new();
        let mut stack = vec![home.path().to_path_buf()];
        while let Some(dir) = stack.pop() {
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("jsonl")
                        && path.file_name().and_then(|n| n.to_str()) != Some("index.jsonl")
                        && let Ok(text) = std::fs::read_to_string(&path)
                    {
                        out.push_str(&text);
                    }
                }
            }
        }
        out
    };
    let wait_for_text = |needle: &str| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let contents = read_sessions();
            if contents.contains(needle) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for `{needle}` in ACP session jsonl: {contents}"
            );
            // sleep-ok: poll interval while the session file is written; bounded by `deadline`
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    };

    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "start 1" }] }),
        |_| json!(null),
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");
    std::fs::write(cwd.join(".gate1"), "go").unwrap();
    wait_for_text("woke-1");

    // Exhausted max_wakeups=1; a second ACP prompt must reset the counter so job 2 wakes too.
    let p2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "start 2" }] }),
        |_| json!(null),
    );
    assert_eq!(p2["result"]["stopReason"], "end_turn", "{p2}");
    std::fs::write(cwd.join(".gate2"), "go").unwrap();
    wait_for_text("woke-2");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    rpc.kill_and_wait();
}

/// An ACP `session/prompt` arriving while a background wakeup turn is still busy is queued as a
/// `follow_up` (`run_pending` starts its turn with `turn/started.text == nil`); consuming that
/// queued user message must still reset `jobs.max_wakeups`.
#[test]
fn acp_queued_prompt_during_wakeup_resets_jobs_max_wakeups() {
    let home = tempfile::tempdir().unwrap();
    let faux_path = home.path().join("faux-queued-reset.json");
    std::fs::write(
        &faux_path,
        json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg1", "command": "while [ ! -f .gate1 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-1" },
            { "text": "WOKE_ONE_SLOW_STREAM", "delay_ms": 400 },
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg2", "command": "while [ ! -f .gate2 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-2" },
            { "text": "WOKE_TWO_AFTER_QUEUED" }
        ])
        .to_string(),
    )
    .unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        "mare.setup { plugins = { jobs = { max_wakeups = 1, batch_ms = 30 } } }\n",
    )
    .unwrap();
    let ws = tempfile::tempdir().unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--yolo", "--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["protocolVersion"], 1, "{init}");
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "start 1" }] }),
        |_| json!(null),
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");
    // Release bg1 only after prompt 1 is idle so its completion reliably triggers a wakeup turn.
    std::fs::write(cwd.join(".gate1"), "go").unwrap();

    // Wait until wakeup turn 1 is actively streaming, then send prompt 2 while busy so it takes
    // the queued follow-up path.
    let mut saw_woke1 = String::new();
    let d1 = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !saw_woke1.contains("WOKE_ONE") {
        assert!(std::time::Instant::now() < d1, "never saw woke-1 streaming; got={saw_woke1:?}");
        if let Some(m) = rpc.read_timeout(std::time::Duration::from_millis(200))
            && m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
        {
            saw_woke1.push_str(t);
        }
    }

    let p2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "start 2 while busy" }] }),
        |_| json!(null),
    );
    assert_eq!(p2["result"]["stopReason"], "end_turn", "{p2}");
    std::fs::write(cwd.join(".gate2"), "go").unwrap();

    let mut saw_woke2 = String::new();
    let d2 = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !saw_woke2.contains("WOKE_TWO_AFTER_QUEUED") {
        assert!(
            std::time::Instant::now() < d2,
            "queued ACP prompt must reset jobs.max_wakeups so bg2 wakes; got={saw_woke2:?}"
        );
        if let Some(m) = rpc.read_timeout(std::time::Duration::from_millis(200))
            && m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
        {
            saw_woke2.push_str(t);
        }
    }

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");
    rpc.kill_and_wait();
}

#[test]
fn acp_permission_request_omits_allow_always_for_protected_arguments() {
    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("register-secret.lua"),
        r#"return { name = "register-secret", apply = function(ctx)
            KEEP_SECRET = mare.projection.register("ACP_PROTECTED_SECRET_42")
        end }"#,
    )
    .unwrap();
    let script_path = home.path().join("faux.json");
    let script = json!([
        { "tool_calls": [{ "name": "write", "arguments": { "path": "acp-secret.txt", "content": "ACP_PROTECTED_SECRET_42" } }] },
        { "text": "wrote secret" }
    ]);
    std::fs::write(&script_path, script.to_string()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--permission", "read-only", "--approval-policy", "ask", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let mut saw_permission_req = false;
    let id = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run" }] }));
    let res = rpc.wait_raw(id, |req| {
        if req["method"] == "session/request_permission" {
            saw_permission_req = true;
            let options = req["params"]["options"].as_array().expect("options array");
            let option_ids: Vec<&str> = options.iter().filter_map(|o| o["optionId"].as_str()).collect();
            assert!(
                !option_ids.contains(&"allow-always"),
                "protected arguments must not offer unachievable allow-always option: {option_ids:?}"
            );
            assert!(option_ids.contains(&"allow-once"), "expected allow-once option: {option_ids:?}");
            assert!(option_ids.contains(&"reject-once"), "expected reject-once option: {option_ids:?}");
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } })
        } else {
            json!(null)
        }
    });
    assert!(saw_permission_req, "expected session/request_permission call");
    assert_eq!(res["result"]["stopReason"], "end_turn", "{res}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("acp-secret.txt")).unwrap(),
        "ACP_PROTECTED_SECRET_42",
        "allow-once in ACP must restore protected argument and write file"
    );

    rpc.kill_and_wait();
}

/// Answers every `session/request_permission` with `answer(index, offered option ids)` and
/// records what was offered; returns the prompt's result.
fn prompt_answering(
    rpc: &mut RpcClient,
    sid: &str,
    mut answer: impl FnMut(usize, &[String]) -> &'static str,
) -> (Value, Vec<Vec<String>>) {
    let mut offered: Vec<Vec<String>> = vec![];
    let id = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run" }] }));
    let res = rpc.wait_raw(id, |req| {
        if req["method"] != "session/request_permission" {
            return json!(null);
        }
        let ids: Vec<String> = req["params"]["options"]
            .as_array()
            .expect("options array")
            .iter()
            .filter_map(|o| o["optionId"].as_str().map(str::to_owned))
            .collect();
        let pick = answer(offered.len(), &ids);
        offered.push(ids);
        json!({ "outcome": { "outcome": "selected", "optionId": pick } })
    });
    (res, offered)
}

// A client is outside the process: answering with an option it was not offered must not
// widen the grant. Reference: Codex core/tests/suite/request_permissions.rs:1243 (a granted
// request covers later calls), :1698 (a partial grant preapproves nothing else), :1853/:1969
// (turn vs session grants); reference implementation (an answer outside
// the approval seam's contract degrades to deny).
#[test]
fn acp_forged_allow_always_on_protected_arguments_is_refused_and_grants_nothing() {
    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("register-secret.lua"),
        r#"return { name = "register-secret", apply = function(ctx)
            KEEP_SECRET = mare.projection.register("ACP_FORGED_SECRET_7")
        end }"#,
    )
    .unwrap();
    let write = json!({ "name": "write", "arguments": { "path": "forged.txt", "content": "ACP_FORGED_SECRET_7" } });
    let script_path = home.path().join("faux.json");
    std::fs::write(
        &script_path,
        json!([{ "tool_calls": [write] }, { "tool_calls": [write] }, { "text": "done" }]).to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--permission", "read-only", "--approval-policy", "ask", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let (res, offered) = prompt_answering(&mut rpc, &sid, |i, _| if i == 0 { "allow-always" } else { "reject-once" });
    assert_eq!(res["result"]["stopReason"], "end_turn", "{res}");
    assert!(offered.iter().all(|ids| !ids.iter().any(|id| id == "allow-always")), "{offered:?}");
    assert_eq!(offered.len(), 2, "the forged answer granted nothing, so the same call is asked again: {offered:?}");
    assert!(!cwd.join("forged.txt").exists(), "a forged option must not run the tool");

    rpc.kill_and_wait();
}

#[test]
fn acp_allow_always_only_widens_a_request_that_offered_it() {
    let home = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let (turn_dir, session_dir) = (cwd.join("turn-dir"), cwd.join("session-dir"));
    std::fs::create_dir_all(&turn_dir).unwrap();
    std::fs::create_dir_all(&session_dir).unwrap();
    let request = |dir: &std::path::Path, scope: &str| {
        json!({ "tool_calls": [{ "name": "request_permissions", "arguments": {
            "additional_permissions": { "write": [dir] }, "scope": scope, "justification": "write reports"
        } }] })
    };
    let write = |path: std::path::PathBuf| json!({ "tool_calls": [{ "name": "write", "arguments": { "path": path, "content": "x" } }] });
    let script = json!([
        request(&turn_dir, "turn"),
        write(turn_dir.join("a.txt")),
        request(&session_dir, "session"),
        write(session_dir.join("b.txt")),
        { "text": "done" },
        // a later prompt in the same ACP session: the session grant still covers the directory
        write(session_dir.join("c.txt")),
        { "text": "done again" }
    ]);
    let script_path = home.path().join("faux.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--permission", "read-only", "--approval-policy", "ask", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    // 0: turn-scoped request, forged allow-always; 1: the write it would have covered;
    // 2: session-scoped request, legitimately offered allow-always.
    let (res, offered) = prompt_answering(&mut rpc, &sid, |i, _| match i {
        0 | 2 => "allow-always",
        _ => "reject-once",
    });
    assert_eq!(res["result"]["stopReason"], "end_turn", "{res}");
    assert_eq!(
        offered.len(),
        3,
        "turn request, its uncovered write, session request; the granted write asks nothing: {offered:?}"
    );
    assert!(
        !offered[0].iter().any(|id| id == "allow-always"),
        "a turn-scoped request does not offer allow-always: {offered:?}"
    );
    assert!(offered[2].iter().any(|id| id == "allow-always"), "a session-scoped request offers it: {offered:?}");
    assert!(!turn_dir.join("a.txt").exists(), "the forged answer granted nothing");
    assert_eq!(
        std::fs::read_to_string(session_dir.join("b.txt")).unwrap(),
        "x",
        "the offered allow-always grants the session request"
    );

    // Codex core/tests/suite/request_permissions.rs:1969: a session grant outlives its turn.
    let (res, offered) = prompt_answering(&mut rpc, &sid, |_, _| "reject-once");
    assert_eq!(res["result"]["stopReason"], "end_turn", "{res}");
    assert!(offered.is_empty(), "the next turn's covered write asks nothing: {offered:?}");
    assert_eq!(
        std::fs::read_to_string(session_dir.join("c.txt")).unwrap(),
        "x",
        "the session grant from the earlier turn still applies"
    );

    rpc.kill_and_wait();
}

#[test]
fn acp_set_config_option_is_session_scoped_and_resumable() {
    let home = tempfile::tempdir().unwrap();
    let script_path = home.path().join("faux.json");
    std::fs::write(&script_path, json!([{ "text": "a1" }, { "text": "a2" }]).to_string()).unwrap();

    // Start without `--model` so new sessions read the global default from state.json.
    let mut cmd = mare_cmd();
    cmd.arg("acp").env("MARE_HOME", home.path()).env("MARE_FAUX_SCRIPT", &script_path).stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["protocolVersion"], 1, "{init}");
    let cwd = dunce::canonicalize(home.path()).unwrap();

    let option_val = |opts: &Value, id: &str| -> String {
        opts.as_array()
            .and_then(|arr| arr.iter().find(|o| o["id"] == id))
            .and_then(|o| o["currentValue"].as_str())
            .unwrap_or_else(|| panic!("missing `{id}` in {opts}"))
            .to_string()
    };

    // Session A1: change reasoning_effort without overwriting global default.
    let s_a1 = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid_a1 = s_a1["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s_a1}")).to_string();
    let default_model = option_val(&s_a1["result"]["configOptions"], "model");
    let default_effort = option_val(&s_a1["result"]["configOptions"], "reasoning_effort");
    assert_eq!(default_model, "faux/echo");
    let alt_effort = if default_effort == "high" { "low" } else { "high" };

    let set_effort = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid_a1, "configId": "reasoning_effort", "value": alt_effort }),
        |_| json!(null),
    );
    assert_eq!(option_val(&set_effort["result"]["configOptions"], "reasoning_effort"), alt_effort, "{set_effort}");
    let p_a1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid_a1, "prompt": [{ "type": "text", "text": "hello a1" }] }),
        |_| json!(null),
    );
    assert_eq!(p_a1["result"]["stopReason"], "end_turn", "{p_a1}");
    let close_a1 = rpc.call_raw("session/close", json!({ "sessionId": sid_a1 }), |_| json!(null));
    assert!(close_a1.get("result").is_some(), "{close_a1}");

    // Session A2: change model without overwriting global default.
    let s_a2 = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid_a2 = s_a2["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s_a2}")).to_string();
    let set_model = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid_a2, "configId": "model", "value": "faux/echo2" }),
        |_| json!(null),
    );
    assert_eq!(option_val(&set_model["result"]["configOptions"], "model"), "faux/echo2", "{set_model}");
    let p_a2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid_a2, "prompt": [{ "type": "text", "text": "hello a2" }] }),
        |_| json!(null),
    );
    assert_eq!(p_a2["result"]["stopReason"], "end_turn", "{p_a2}");
    let close_a2 = rpc.call_raw("session/close", json!({ "sessionId": sid_a2 }), |_| json!(null));
    assert!(close_a2.get("result").is_some(), "{close_a2}");

    // Session B: newly created session without explicit startup model must still use original global defaults.
    let s_b = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid_b = s_b["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s_b}")).to_string();
    assert_eq!(option_val(&s_b["result"]["configOptions"], "model"), default_model, "{s_b}");
    assert_eq!(option_val(&s_b["result"]["configOptions"], "reasoning_effort"), default_effort, "{s_b}");
    let _ = rpc.call_raw("session/close", json!({ "sessionId": sid_b }), |_| json!(null));

    // Resuming A1 and A2 restores their respective session-scoped selections.
    let res_a1 =
        rpc.call_raw("session/resume", json!({ "sessionId": sid_a1, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert_eq!(option_val(&res_a1["result"]["configOptions"], "model"), default_model, "{res_a1}");
    assert_eq!(option_val(&res_a1["result"]["configOptions"], "reasoning_effort"), alt_effort, "{res_a1}");

    let res_a2 =
        rpc.call_raw("session/resume", json!({ "sessionId": sid_a2, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert_eq!(option_val(&res_a2["result"]["configOptions"], "model"), "faux/echo2", "{res_a2}");

    rpc.kill_and_wait();
}

#[test]
fn acp_slow_set_config_option_does_not_block_other_session_or_reorder_same_session() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    fn accept_and_read_with_deadline(listener: &TcpListener, timeout: Duration) -> TcpStream {
        let deadline = Instant::now() + timeout;
        listener.set_nonblocking(true).unwrap();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "timed out waiting for HTTP barrier connection");
                    // sleep-ok: non-blocking accept poll for the barrier connection, bounded by the deadline above
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("barrier accept failed: {e}"),
            }
        };
        listener.set_nonblocking(false).unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 512];
        let mut total = 0;
        loop {
            match stream.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => {
                    total += n;
                    if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") || total == buf.len() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "timed out reading HTTP barrier request");
                    // sleep-ok: non-blocking read poll of the barrier request, bounded by the deadline above
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("barrier read failed: {e}"),
            }
        }
        stream.set_nonblocking(false).unwrap();
        stream
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let home = tempfile::tempdir().unwrap();
    let faux_path = home.path().join("faux.json");
    std::fs::write(&faux_path, json!([{ "text": "x".repeat(4000), "delay_ms": 20 }]).to_string()).unwrap();

    let plugin_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("slow_switch.lua"),
        format!(
            r#"return {{
  name = "slow-switch",
  apply = function(C)
    C:on("model/changed", function()
      if mare.fs.exists(mare.cwd .. "/.slow-switch") then
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
    std::fs::write(cwd_a.join(".slow-switch"), "1").unwrap();

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

    // Start a slow streaming prompt on session B and wait until it is actively streaming.
    let prompt_b_id =
        rpc.request("session/prompt", json!({ "sessionId": s_b, "prompt": [{ "type": "text", "text": "slow b" }] }));
    let stream_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let m = rpc.read_deadline(stream_deadline).unwrap();
        if m["method"] == "session/update"
            && m["params"]["sessionId"] == s_b
            && m["params"]["update"]["content"]["text"].as_str().is_some_and(|t| t.starts_with('x'))
        {
            break;
        }
    }

    // Start a slow model switch on session A; its model/changed hook will block on the HTTP barrier.
    let set_a1_id = rpc
        .request("session/set_config_option", json!({ "sessionId": s_a, "configId": "model", "value": "faux/echo" }));
    let mut barrier1_stream = accept_and_read_with_deadline(&listener, Duration::from_secs(10));

    // Queue a second config mutation on session A while the first is still pending.
    let set_a2_id = rpc
        .request("session/set_config_option", json!({ "sessionId": s_a, "configId": "model", "value": "faux/echo" }));

    // While session A's switch is still pending:
    // 1. Session B's session/cancel must be dispatched promptly and cancel session B's prompt.
    rpc.send(json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": s_b } }));
    let cancel_res = rpc
        .wait_timeout(prompt_b_id, Duration::from_secs(5), |_| json!(null))
        .expect("session/cancel for session B must not be blocked by session A's slow set_config_option");
    assert_eq!(cancel_res["result"]["stopReason"], "cancelled", "{cancel_res}");

    // 2. Session A's second config mutation must remain serialized behind the first (must not hit barrier yet).
    listener.set_nonblocking(true).unwrap();
    let second_accept = listener.accept();
    assert!(
        matches!(second_accept, Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "second set_config_option on session A must wait for the first to finish"
    );
    listener.set_nonblocking(false).unwrap();

    // Release the first barrier; first switch completes, then the second hits the barrier.
    barrier1_stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
    drop(barrier1_stream);

    let set_a1_resp = rpc.wait_timeout(set_a1_id, Duration::from_secs(5), |_| json!(null)).unwrap();
    assert!(set_a1_resp["result"]["configOptions"].is_array(), "{set_a1_resp}");

    let mut barrier2_stream = accept_and_read_with_deadline(&listener, Duration::from_secs(10));
    barrier2_stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").unwrap();
    drop(barrier2_stream);

    let set_a2_resp = rpc.wait_timeout(set_a2_id, Duration::from_secs(5), |_| json!(null)).unwrap();
    assert!(set_a2_resp["result"]["configOptions"].is_array(), "{set_a2_resp}");

    rpc.kill_and_wait();
}

#[test]
fn acp_fallback_projects_permission_requests_and_error_responses() {
    const PERM_SEC: &str = "ACP_NON_EVT_PERM_SEC_33";
    const ERR_SEC: &str = "ACP_NON_EVT_ERR_SEC_44";
    const MODEL_SEC: &str = "ACP_CONFIG_SECRET_55";
    const EVENT_SEC: &str = "ACP_EVENT_SECRET_66";

    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("acp-non-event-probe.lua"),
        format!(
            r#"return {{
                name = "acp-non-event-probe",
                inject = {{ "tools", "policy", "llm", "bus" }},
                apply = function(ctx)
                    KEEP_ACP_LEASES = {{
                        mare.projection.register("{PERM_SEC}"),
                        mare.projection.register("{ERR_SEC}"),
                        mare.projection.register("{MODEL_SEC}"),
                        mare.projection.register("{EVENT_SEC}"),
                    }}
                    ctx.llm:register_provider({{
                        provider = "projection-probe", no_key = true,
                        models = {{ {{ model_id = "visible", name = "{MODEL_SEC}" }} }},
                        stream = function() error("not invoked") end,
                    }})
                    ctx:on("turn/started", function(ev)
                        ctx.bus:emit("item/delta", {{
                            turn_id = ev.turn_id,
                            item_id = "probe-item-1",
                            kind = "text",
                            delta = "{EVENT_SEC}",
                            offset = 0,
                        }})
                    end)
                    ctx.tools:register("custom_probe", {{
                        description = "direct tool/approval-host probe",
                        side_effect = "read",
                        parameters = {{ type = "object", additionalProperties = false }},
                        execute = function()
                            local vote = ctx.policy:decide("tool/approval-host", {{
                                {{
                                    id = "raw-call-1",
                                    name = "custom_probe",
                                    arguments = {{ note = "{PERM_SEC}" }},
                                }},
                            }}, {{ require_explicit_approval = true }})
                            assert(vote and vote.decision == "deny", "expected deny from test ACP client")
                            return {{ content = "ok" }}
                        end,
                    }})
                end,
            }}"#
        ),
    )
    .unwrap();
    let script_path = home.path().join("faux.json");
    std::fs::write(
        &script_path,
        json!([
            { "tool_calls": [{ "name": "custom_probe", "arguments": {} }] },
            { "error": format!("provider failed {ERR_SEC}") }
        ])
        .to_string(),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "--permission", "auto", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(!s.to_string().contains(MODEL_SEC), "ACP config response leaked plaintext: {s}");
    assert!(s.to_string().contains("[[mare-value:"), "expected projected ACP config response: {s}");
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    // 1. Non-event error response (`session/set_config_option`) projects error data.
    let bad_opt = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": ERR_SEC, "value": "x" }),
        |_| json!(null),
    );
    let bad_opt_str = bad_opt.to_string();
    assert!(bad_opt.get("error").is_some(), "{bad_opt}");
    assert!(!bad_opt_str.contains(ERR_SEC), "ACP config error leaked plaintext: {bad_opt}");
    assert!(bad_opt_str.contains("[[mare-value:"), "expected projected ACP error: {bad_opt}");

    // 2. Unprojected direct `tool/approval-host` request and failing turn error are both projected at ACP exit.
    let pid = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "go" }] }));
    let mut perm_requests = Vec::new();
    let prompt_resp = rpc.wait_raw(pid, |req| {
        if req["method"] == "session/request_permission" {
            perm_requests.push(req["params"].clone());
            json!({ "outcome": { "outcome": "selected", "optionId": "reject-once" } })
        } else {
            json!(null)
        }
    });
    assert_eq!(perm_requests.len(), 1, "expected permission request: {perm_requests:?}");
    let perm_str = perm_requests[0].to_string();
    assert!(!perm_str.contains(PERM_SEC), "ACP request_permission leaked plaintext: {perm_str}");
    assert!(perm_str.contains("[[mare-value:"), "expected projected ACP permission request: {perm_str}");

    let prompt_err = prompt_resp.to_string();
    assert!(prompt_resp.get("error").is_some(), "expected prompt error: {prompt_resp}");
    assert!(!prompt_err.contains(ERR_SEC), "ACP prompt error leaked plaintext: {prompt_err}");
    assert!(prompt_err.contains("[[mare-value:"), "expected projected ACP prompt error: {prompt_err}");

    let notifications: Vec<Value> = rpc.into_messages().collect();
    assert!(
        notifications.iter().any(|m| { m["method"] == "session/update" && m.to_string().contains("[[mare-value:") }),
        "missing projected ACP event notification: {notifications:?}"
    );
    assert!(notifications.iter().all(|m| !m.to_string().contains(EVENT_SEC)), "ACP event leaked plaintext");
}

#[test]
fn acp_command_tool_emits_execute_kind_in_tool_call_and_permission_request() {
    use std::io::BufReader;
    use std::process::Stdio;

    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        r#"mare.plugin("permissions", { rules = { { tool = "deploy_echo", decision = "prompt" } } })
        mare.tool.command({
            name = "deploy_echo",
            description = "custom command tool",
            command = { "echo", "{msg}" },
            params = { msg = { type = "string", required = true } },
        })
        "#,
    )
    .unwrap();
    let faux_path = home.path().join("faux.json");
    std::fs::write(
        &faux_path,
        serde_json::to_string(&json!([
            { "tool_calls": [{ "name": "deploy_echo", "arguments": { "msg": "hi" } }] },
            { "text": "done" }
        ]))
        .unwrap(),
    )
    .unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(workspace.path()).unwrap();

    let mut child = mare_cmd()
        .args(["--model", "faux/echo", "--permission", "read-only", "--approval-policy", "ask", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut rpc = RpcClient::new(BufReader::new(child.stdout.take().unwrap()), child.stdin.take().unwrap());
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let prompt_id =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run" }] }));
    let mut seen: Vec<Value> = Vec::new();
    let mut start_kind: Option<String> = None;
    let mut perm_tc: Option<Value> = None;
    let resp = loop {
        let m = rpc.read();
        seen.push(m.clone());
        match m.get("method").and_then(|v| v.as_str()) {
            Some("session/update") if m["params"]["update"]["sessionUpdate"] == "tool_call" => {
                start_kind = m["params"]["update"]["kind"].as_str().map(str::to_string);
            }
            Some("session/request_permission") => {
                perm_tc = Some(m["params"]["toolCall"].clone());
                rpc.send(json!({
                    "jsonrpc": "2.0",
                    "id": m["id"],
                    "result": { "outcome": { "outcome": "selected", "optionId": "allow-once" } }
                }));
            }
            _ if m.get("method").is_none() && m["id"] == prompt_id => break m,
            _ => {}
        }
    };
    assert_eq!(resp["result"]["stopReason"], "end_turn", "{resp}");
    assert_eq!(start_kind.as_deref(), Some("execute"), "session/update tool_call kind must be execute; seen={seen:?}");
    let perm_tc =
        perm_tc.unwrap_or_else(|| panic!("expected session/request_permission for command tool; seen={seen:?}"));
    let perm_kind =
        perm_tc.get("kind").or_else(|| perm_tc.get("fields").and_then(|f| f.get("kind"))).and_then(|v| v.as_str());
    assert_eq!(perm_kind, Some("execute"), "session/request_permission toolCall kind must be execute: {perm_tc}");

    drop(rpc);
    let _ = child.kill();
    let _ = child.wait();
}

/// ACP must (1) stream `session/update` notifications for background wakeup turns even when
/// no `session/prompt` is active, (2) NOT prematurely reply `end_turn` when a `session/prompt`
/// arrives while a background wakeup turn is busy (wait for the queued prompt's turn and stream
/// its reply), and (3) route `session/cancel` to abort background turns with no active prompt.
#[test]
fn acp_streams_background_wakeup_waits_for_queued_prompt_and_cancels_background_turn() {
    let home = tempfile::tempdir().unwrap();
    let faux_path = home.path().join("faux-acp-bg.json");
    std::fs::write(
        &faux_path,
        json!([
            // Turn 1: launch background job
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg1", "command": "while [ ! -f .gate1 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-bg1" },
            // Background wakeup turn 1: emits text over wire with no active session/prompt!
            { "text": "BG_WOKE_VISIBLE_1" },
            // Turn 2: launch a second bg job whose wakeup turn streams slowly so we can race prompt + cancel
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg2", "command": "while [ ! -f .gate2 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-bg2" },
            // Slow background wakeup turn 2
            { "text": "SLOW_BG_TURN_TWO", "delay_ms": 400 },
            // Answer to the queued prompt that arrived while slow bg turn 2 was busy
            { "text": "QUEUED_PROMPT_ANSWER" },
            // Turn 3: launch bg3 whose wakeup turn streams for >3s if uncancelled (8B/100ms)
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg3", "command": "while [ ! -f .gate3 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-bg3" },
            { "text": format!("{}NEVER_REACHED_TAIL", "C".repeat(240)), "delay_ms": 100 },
            // Follow-up prompt after cancel proves the cancelled background turn actually ended.
            { "text": "AFTER_CANCEL_OK" }
        ])
        .to_string(),
    )
    .unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        "mare.setup { plugins = { jobs = { max_wakeups = 3, batch_ms = 30 } } }\n",
    )
    .unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--yolo", "--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    // Part 1: Trigger bg1; after prompt 1 ends, background wakeup turn MUST stream `BG_WOKE_VISIBLE_1`
    // via `session/update` on the wire with NO active `session/prompt`.
    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run bg1" }] }),
        |_| json!(null),
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");
    std::fs::write(cwd.join(".gate1"), "go").unwrap();

    let mut woke1 = String::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while !woke1.contains("BG_WOKE_VISIBLE_1") {
        assert!(
            std::time::Instant::now() < deadline,
            "never received background wakeup session/update on wire; got={woke1:?}"
        );
        if let Some(m) = rpc.read_timeout(std::time::Duration::from_millis(250))
            && m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
        {
            woke1.push_str(t);
        }
    }

    // Part 2: Trigger bg2; wait until slow bg wakeup turn 2 starts streaming, then send a new
    // `session/prompt` WHILE busy. It MUST wait for `QUEUED_PROMPT_ANSWER` and stream it before replying.
    let p2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run bg2" }] }),
        |_| json!(null),
    );
    assert_eq!(p2["result"]["stopReason"], "end_turn", "{p2}");
    std::fs::write(cwd.join(".gate2"), "go").unwrap();
    let mut saw_slow_bg = String::new();
    let d2 = std::time::Instant::now() + std::time::Duration::from_secs(6);
    while !saw_slow_bg.contains("SLOW") {
        assert!(std::time::Instant::now() < d2, "never saw slow bg2 start streaming; got={saw_slow_bg:?}");
        if let Some(m) = rpc.read_timeout(std::time::Duration::from_millis(200))
            && m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
        {
            saw_slow_bg.push_str(t);
        }
    }
    let queued_id = rpc.request(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "queued question" }] }),
    );
    let mut queued_text = String::new();
    let queued_resp = loop {
        let m = rpc.read();
        if m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
        {
            queued_text.push_str(t);
        } else if m.get("method").is_none() && m["id"] == queued_id {
            break m;
        }
    };
    assert_eq!(queued_resp["result"]["stopReason"], "end_turn", "{queued_resp}");
    assert!(
        queued_text.contains("QUEUED_PROMPT_ANSWER"),
        "busy session/prompt must wait for and stream its queued answer instead of returning end_turn early; got={queued_text:?}"
    );

    // Part 3: Trigger bg3; once its slow (>3s) background wakeup turn starts streaming, send
    // `session/cancel` with NO active `session/prompt`, assert streaming actually settles quiet
    // before the tail can arrive, and verify a follow-up `session/prompt` runs cleanly.
    let p3 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run bg3" }] }),
        |_| json!(null),
    );
    assert_eq!(p3["result"]["stopReason"], "end_turn", "{p3}");
    std::fs::write(cwd.join(".gate3"), "go").unwrap();
    let d3 = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        assert!(std::time::Instant::now() < d3, "never saw bg3 streaming");
        if let Some(m) = rpc.read_timeout(std::time::Duration::from_millis(200))
            && m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && m["params"]["update"]["content"]["text"].as_str().is_some_and(|t| t.contains('C'))
        {
            break;
        }
    }
    rpc.send(json!({ "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": sid } }));
    // Without cancel, bg3 keeps emitting a chunk every 100ms for >3s and ends with NEVER_REACHED_TAIL.
    // With cancel, streaming must stop and reach 350ms of consecutive silence well within 1.5s.
    let mut after_cancel_text = String::new();
    let settle_deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    let mut settled_quiet = false;
    while std::time::Instant::now() < settle_deadline {
        match rpc.read_timeout(std::time::Duration::from_millis(350)) {
            Some(m) => {
                if m.get("method").and_then(|v| v.as_str()) == Some("session/update")
                    && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
                {
                    after_cancel_text.push_str(t);
                }
            }
            None => {
                settled_quiet = true;
                break;
            }
        }
    }
    assert!(
        settled_quiet,
        "session/cancel must stop background wakeup stream before deadline; kept receiving updates: {after_cancel_text:?}"
    );
    assert!(
        !after_cancel_text.contains("NEVER_REACHED_TAIL"),
        "cancelled background wakeup turn must NOT reach its tail: {after_cancel_text:?}"
    );

    // Follow-up prompt confirms the session is idle and responsive after cancelling the background turn.
    let follow_id = rpc
        .request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "after cancel" }] }));
    let mut follow_text = String::new();
    let follow_resp = loop {
        let m = rpc.read();
        if m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
        {
            follow_text.push_str(t);
        } else if m.get("method").is_none() && m["id"] == follow_id {
            break m;
        }
    };
    assert_eq!(follow_resp["result"]["stopReason"], "end_turn", "{follow_resp}");
    assert!(
        follow_text.contains("AFTER_CANCEL_OK") && !follow_text.contains("NEVER_REACHED_TAIL"),
        "expected clean follow-up prompt after cancel; got={follow_text:?}"
    );

    rpc.kill_and_wait();
}

/// If a prompt arrives while a background wakeup turn is running, and that background turn fails
/// (parking queued inputs so they will not auto-wake), `session/prompt` must fail promptly instead
/// of hanging forever.
#[test]
fn acp_queued_prompt_fails_promptly_when_preceding_background_turn_fails_and_parks_input() {
    let home = tempfile::tempdir().unwrap();
    let faux_path = home.path().join("faux-acp-bg-fail.json");
    std::fs::write(
        &faux_path,
        json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "description": "bg", "command": "while [ ! -f .gate1 ]; do sleep 0.02; done", "run_in_background": true } }] },
            { "text": "started-bg" },
            {
                "text": "BG_WILL_FAIL_SOON",
                "tool_calls": [{ "name": "bash", "arguments": { "description": "hold", "command": "while [ ! -f .hold_release ]; do sleep 0.02; done" } }]
            },
            { "error": "upstream boom" },
            { "text": "FRESH_AFTER_FAIL" }
        ])
        .to_string(),
    )
    .unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        "mare.setup { plugins = { jobs = { max_wakeups = 2, batch_ms = 30 } } }\n",
    )
    .unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--yolo", "--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "launch" }] }),
        |_| json!(null),
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");
    std::fs::write(cwd.join(".gate1"), "go").unwrap();

    // Wait for the background wakeup turn to start streaming before sending the queued prompt.
    let start_deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let mut saw_bg = String::new();
    while !saw_bg.contains("BG_WILL") {
        assert!(std::time::Instant::now() < start_deadline, "never saw failing bg wakeup stream; got={saw_bg:?}");
        if let Some(m) = rpc.read_timeout(std::time::Duration::from_millis(200))
            && m.get("method").and_then(|v| v.as_str()) == Some("session/update")
            && let Some(t) = m["params"]["update"]["content"]["text"].as_str()
        {
            saw_bg.push_str(t);
        }
    }

    let queued_id =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "queued" }] }));
    std::fs::write(cwd.join(".hold_release"), "go").unwrap();
    let resp_deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    let queued_resp = loop {
        let m = rpc.read_deadline(resp_deadline).expect("queued prompt hung after preceding background turn failed");
        if m.get("method").is_none() && m["id"] == queued_id {
            break m;
        }
    };
    assert!(
        queued_resp.get("error").is_some(),
        "queued prompt parked by failed background turn must return error, not end_turn: {queued_resp}"
    );

    // A subsequent prompt must succeed cleanly without re-executing the failed queued prompt.
    let next_id = rpc.request(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "fresh_after_fail" }] }),
    );
    let (next_resp, next_updates) = read_until(&mut rpc, next_id, Some(&sid));
    let next_text: String = next_updates.iter().filter_map(|u| u["content"]["text"].as_str()).collect();
    assert_eq!(next_resp["result"]["stopReason"], "end_turn", "{next_resp}");
    assert!(next_text.contains("FRESH_AFTER_FAIL"), "expected fresh turn output, got={next_text:?}");

    // Close and reload the session to confirm the failed queued prompt was deleted and never ran.
    // `session/load` boots a fresh host (same startup cost as `session/new`), so read via the
    // shared `read_until` helper instead of a hardcoded short deadline.
    let close_resp = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(close_resp.get("error").is_none(), "session/close failed: {close_resp}");
    let load_id = rpc.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
    let (load_resp, load_updates) = read_until(&mut rpc, load_id, Some(&sid));
    assert!(load_resp.get("error").is_none(), "session/load failed: {load_resp}");
    let replayed_users: Vec<String> = load_updates
        .iter()
        .filter(|u| u["sessionUpdate"].as_str() == Some("user_message_chunk"))
        .filter_map(|u| u["content"]["text"].as_str().map(str::to_string))
        .collect();
    assert!(
        replayed_users.iter().any(|u| u.contains("fresh_after_fail")),
        "expected fresh prompt in replayed history: {replayed_users:?}"
    );
    assert!(
        !replayed_users.iter().any(|u| u == "queued"),
        "parked queued prompt must have been deleted and never executed: {replayed_users:?}"
    );

    rpc.kill_and_wait();
}

#[test]
fn acp_preserves_trust_source_for_trust_default_and_trust_resolve_plugins() {
    // 1. `trust_default = "trust"` in init.lua loads project skills in both same-cwd and other-cwd sessions.
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("config");
    let plugins_dir = cfg_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(cfg_dir.join("init.lua"), "mare.setup { trust_default = \"trust\" }\n").unwrap();

    let req_log = home.path().join("acp-trust-req.json");
    std::fs::write(
        plugins_dir.join("record-prov.lua"),
        format!(
            r#"return {{
    name = "record-prov",
    inject = {{ "llm" }},
    apply = function(ctx)
        ctx.llm:register_provider({{
            provider = "rec",
            no_key = true,
            models = {{ {{ model_id = "m", name = "Rec", images = true }} }},
            stream = function(req)
                mare.fs.write({:?}, mare.json.encode({{ system = req.system }}))
                return {{ role = "assistant", content = {{ {{ type = "text", text = "ok" }} }}, stop_reason = "stop" }}
            end,
        }})
    end,
}}"#,
            req_log.to_string_lossy()
        ),
    )
    .unwrap();

    let ws = tempfile::tempdir().unwrap();
    let skill_dir = ws.path().join(".mare/skills/acp-proj-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: acp-proj-skill\ndescription: ACP project skill loaded via trust_default\n---\nbody\n",
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "rec/m", "acp"]).env("MARE_HOME", home.path()).current_dir(ws.path()).stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();
    let p_id = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hi" }] }));
    let (p_resp, _) = read_until(&mut rpc, p_id, Some(&sid));
    assert_eq!(p_resp["result"]["stopReason"], "end_turn", "{p_resp}");
    rpc.kill_and_wait();

    let recorded: Value = serde_json::from_str(&std::fs::read_to_string(&req_log).unwrap()).unwrap();
    let system = recorded["system"].as_str().unwrap_or("");
    assert!(
        system.contains("acp-proj-skill"),
        "ACP with trust_default = \"trust\" must load project skills into the request: {system}"
    );

    // 2. A `trust/resolve` plugin can trust a directory opened via ACP `session/new` even when launch cwd differs.
    let home2 = tempfile::tempdir().unwrap();
    let cfg_dir2 = home2.path().join("config");
    let plugins_dir2 = cfg_dir2.join("plugins");
    std::fs::create_dir_all(&plugins_dir2).unwrap();
    let launch_dir = tempfile::tempdir().unwrap();
    let ws2 = tempfile::tempdir().unwrap();
    let skill_dir2 = ws2.path().join(".mare/skills/acp-resolver-skill");
    std::fs::create_dir_all(&skill_dir2).unwrap();
    std::fs::write(
        skill_dir2.join("SKILL.md"),
        "---\nname: acp-resolver-skill\ndescription: ACP project skill loaded via trust/resolve\n---\nbody\n",
    )
    .unwrap();
    let req_log2 = home2.path().join("acp-resolver-req.json");
    std::fs::write(
        plugins_dir2.join("trust-resolver.lua"),
        format!(
            r#"return {{
    name = "trust-resolver",
    inject = {{ "bus", "llm" }},
    apply = function(ctx)
        ctx:on("trust/resolve", function(ev, next)
            ev.decision = true
            return next(ev)
        end)
        ctx.llm:register_provider({{
            provider = "rec",
            no_key = true,
            models = {{ {{ model_id = "m", name = "Rec", images = true }} }},
            stream = function(req)
                mare.fs.write({:?}, mare.json.encode({{ system = req.system }}))
                return {{ role = "assistant", content = {{ {{ type = "text", text = "ok" }} }}, stop_reason = "stop" }}
            end,
        }})
    end,
}}"#,
            req_log2.to_string_lossy()
        ),
    )
    .unwrap();

    let mut cmd2 = mare_cmd();
    cmd2.args(["--model", "rec/m", "acp"])
        .env("MARE_HOME", home2.path())
        .current_dir(launch_dir.path())
        .stderr(Stdio::piped());
    let mut rpc2 = RpcClient::spawn_cmd(&mut cmd2);
    let _ = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd2 = dunce::canonicalize(ws2.path()).unwrap();
    let s2 = rpc2.call_raw("session/new", json!({ "cwd": cwd2, "mcpServers": [] }), |_| json!(null));
    let sid2 = s2["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s2}")).to_string();
    let p_id2 =
        rpc2.request("session/prompt", json!({ "sessionId": sid2, "prompt": [{ "type": "text", "text": "hi" }] }));
    let (p_resp2, _) = read_until(&mut rpc2, p_id2, Some(&sid2));
    assert_eq!(p_resp2["result"]["stopReason"], "end_turn", "{p_resp2}");
    rpc2.kill_and_wait();

    let recorded2: Value = serde_json::from_str(&std::fs::read_to_string(&req_log2).unwrap()).unwrap();
    let system2 = recorded2["system"].as_str().unwrap_or("");
    assert!(
        system2.contains("acp-resolver-skill"),
        "ACP trust/resolve plugin must load project skills into the request: {system2}"
    );
}

#[test]
fn acp_forwards_config_option_update_on_in_session_model_and_effort_change() {
    let home = tempfile::tempdir().unwrap();
    let reqs_dir = home.path().join("reqs");
    std::fs::create_dir_all(&reqs_dir).unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("switch-model.lua"),
        format!(
            r#"local n = 0
return {{
    name = "switch-model",
    inject = {{ "llm", "tools" }},
    apply = function(ctx)
        ctx.llm:register_provider({{
            provider = "sw",
            no_key = true,
            models = {{
                {{ model_id = "alpha", name = "Alpha", reasoning = false }},
                {{ model_id = "beta", name = "Beta", reasoning = true, efforts = {{ "minimal", "medium", "max" }}, default_effort = "medium" }},
            }},
            stream = function(req)
                n = n + 1
                mare.fs.write({:?} .. "/req-" .. tostring(n) .. ".json", mare.json.encode({{
                    model = req.model and req.model.ref,
                    effort = req.effort,
                }}))
                if n == 1 then
                    return {{
                        role = "assistant",
                        content = {{ {{ type = "tool_call", id = "tc1", name = "switch_to_beta", arguments = {{}} }} }},
                        stop_reason = "tool_use",
                    }}
                elseif n == 3 then
                    return {{
                        role = "assistant",
                        content = {{ {{ type = "tool_call", id = "tc2", name = "bump_effort", arguments = {{}} }} }},
                        stop_reason = "tool_use",
                    }}
                end
                return {{ role = "assistant", content = {{ {{ type = "text", text = "ok-" .. tostring(n) }} }}, stop_reason = "stop" }}
            end,
        }})
        ctx.tools:register("switch_to_beta", {{
            description = "switch model to sw/beta",
            side_effect = "external",
            parameters = {{ type = "object", additionalProperties = false }},
            execute = function()
                ctx.llm:set_model("sw/beta", {{ target = "session" }})
                return {{ content = "switched" }}
            end,
        }})
        ctx.tools:register("bump_effort", {{
            description = "set effort to max",
            side_effect = "external",
            parameters = {{ type = "object", additionalProperties = false }},
            execute = function()
                ctx.llm:set_effort("max", {{ target = "session" }})
                return {{ content = "effort-updated" }}
            end,
        }})
    end,
}}"#,
            reqs_dir.to_string_lossy()
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "sw/alpha", "--permission", "auto", "--approval-policy", "never", "acp"])
        .env("MARE_HOME", home.path())
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    // Turn 1: plugin tool switches sw/alpha -> sw/beta.
    let p1 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "switch" }] }));
    let (r1, u1) = read_until(&mut rpc, p1, Some(&sid));
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");

    let cfg_update = u1
        .iter()
        .rev()
        .find(|u| u["sessionUpdate"] == "config_option_update")
        .unwrap_or_else(|| panic!("expected config_option_update after model switch: {u1:?}"));
    let opts1 = cfg_update["configOptions"].as_array().unwrap();
    let model_opt = opts1.iter().find(|o| o["id"] == "model").unwrap();
    assert_eq!(model_opt["currentValue"], "sw/beta", "{cfg_update}");
    let effort_opt = opts1.iter().find(|o| o["id"] == "reasoning_effort").unwrap();
    assert_eq!(effort_opt["currentValue"], "medium", "{cfg_update}");
    let effort_vals: Vec<&str> =
        effort_opt["options"].as_array().unwrap().iter().filter_map(|o| o["value"].as_str()).collect();
    assert!(effort_vals.contains(&"max"), "expected max effort option: {cfg_update}");

    // Turn 2: next request must go to sw/beta, and effort change emits config_option_update too.
    let p2 = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "bump" }] }));
    let (r2, u2) = read_until(&mut rpc, p2, Some(&sid));
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");

    let req3: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-3.json")).unwrap()).unwrap();
    assert_eq!(req3["model"], "sw/beta", "next request after model switch must target sw/beta: {req3}");

    let cfg_update2 = u2
        .iter()
        .rev()
        .find(|u| u["sessionUpdate"] == "config_option_update")
        .unwrap_or_else(|| panic!("expected config_option_update after effort change: {u2:?}"));
    let effort_opt2 =
        cfg_update2["configOptions"].as_array().unwrap().iter().find(|o| o["id"] == "reasoning_effort").unwrap();
    assert_eq!(effort_opt2["currentValue"], "max", "{cfg_update2}");

    rpc.kill_and_wait();
}

/// ACP permission requests forward reason, outside-sandbox marker, working directory, and full
/// command, label `allow-always` as `"Allow for this session"`, and scope that grant strictly to
/// the current ACP session (same call in same session skips prompt; new session prompts again).
#[test]
fn acp_permission_request_forwards_detail_and_session_grant_resets_on_new_session() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("existing.txt"), "old-existing-body").unwrap();
    std::fs::write(ws.path().join("multi.txt"), "alpha one\nalpha two\n").unwrap();
    let long_cmd = format!("echo head && {} && echo ACP_TAIL_MARKER", "a".repeat(160));
    let faux_path = home.path().join("faux-acp-detail.json");
    std::fs::write(
        &faux_path,
        json!([
            { "tool_calls": [{ "name": "unconfined_sh", "arguments": { "command": long_cmd } }] },
            { "text": "s1-turn1" },
            { "tool_calls": [{ "name": "unconfined_sh", "arguments": { "command": long_cmd } }] },
            { "text": "s1-turn2" },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "existing.txt", "content": "new-overwritten-body" } }] },
            { "text": "s1-write-done" },
            { "tool_calls": [{ "name": "edit", "arguments": { "path": "multi.txt", "old_string": "alpha", "new_string": "beta", "replace_all": true } }] },
            { "text": "s1-edit-done" }
        ])
        .to_string(),
    )
    .unwrap();
    let cfg_dir = home.path().join("config");
    let plugins_dir = cfg_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("unconfined-sh.lua"),
        r#"
        return {
            name = "unconfined-sh",
            inject = { "tools" },
            apply = function(ctx)
                ctx.tools:register("unconfined_sh", {
                    description = "unconfined command test tool",
                    side_effect = "external",
                    unconfined = true,
                    parameters = {
                        type = "object",
                        properties = { command = { type = "string" } },
                        required = { "command" },
                    },
                    execute = function() return { content = "ran" } end,
                })
            end,
        }
        "#,
    )
    .unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        "mare.setup { plugins = { permissions = { enabled = true, preset = 'read-only' } } }\n",
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["protocolVersion"], 1, "{init}");
    let cwd = dunce::canonicalize(ws.path()).unwrap();

    let s1 = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid1 = s1["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s1}")).to_string();

    let mut reqs_s1_t1 = Vec::new();
    let r1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid1, "prompt": [{ "type": "text", "text": "first" }] }),
        |req| {
            if req["method"] == "session/request_permission" {
                reqs_s1_t1.push(req["params"].clone());
                return json!({ "outcome": { "outcome": "selected", "optionId": "allow-always" } });
            }
            json!(null)
        },
    );
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");
    assert_eq!(reqs_s1_t1.len(), 1, "expected 1 permission prompt on first call: {reqs_s1_t1:?}");
    let p = &reqs_s1_t1[0];
    let always =
        p["options"].as_array().unwrap().iter().find(|o| o["optionId"] == "allow-always").expect("allow-always option");
    assert_eq!(always["name"], "Allow for this session", "allow-always label mismatch: {p}");
    assert_eq!(always["kind"], "allow_always", "allow-always kind must stay unchanged: {p}");
    let content_str = p["toolCall"]["content"].to_string();
    assert!(content_str.contains("ACP_TAIL_MARKER"), "ACP toolCall.content must include full command: {p}");
    assert!(content_str.contains("Working directory:"), "ACP toolCall.content must include cwd: {p}");
    assert!(content_str.contains("Reason:"), "ACP toolCall.content must include reason: {p}");
    assert!(
        content_str.contains("Runs outside the local sandbox") && p["toolCall"]["_meta"]["unconfined"] == json!(true),
        "ACP toolCall must include outside-sandbox marker: {p}"
    );

    // Same call in the SAME session must NOT ask again after "Allow for this session".
    let mut asks_s1_t2 = 0usize;
    let r2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid1, "prompt": [{ "type": "text", "text": "second" }] }),
        |req| {
            if req["method"] == "session/request_permission" {
                asks_s1_t2 += 1;
                return json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } });
            }
            json!(null)
        },
    );
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");
    assert_eq!(asks_s1_t2, 0, "same call in same session should reuse session grant");

    // Overwriting an existing file via `write` must asynchronously read the existing file and send
    // a real ACP `Diff` with `oldText` (not a new-file diff or unreadable fallback).
    let mut write_reqs = Vec::new();
    let r_write = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid1, "prompt": [{ "type": "text", "text": "overwrite" }] }),
        |req| {
            if req["method"] == "session/request_permission" {
                write_reqs.push(req["params"].clone());
                return json!({ "outcome": { "outcome": "selected", "optionId": "reject-once" } });
            }
            json!(null)
        },
    );
    assert_eq!(r_write["result"]["stopReason"], "end_turn", "{r_write}");
    assert_eq!(write_reqs.len(), 1, "expected 1 permission prompt for write overwrite: {write_reqs:?}");
    let write_content = write_reqs[0]["toolCall"]["content"].as_array().expect("write content array");
    assert!(
        write_content.iter().any(|c| {
            c["type"] == "diff" && c["oldText"] == "old-existing-body" && c["newText"] == "new-overwritten-body"
        }),
        "ACP write overwrite must include real oldText in Diff content: {}",
        write_reqs[0]
    );

    // Editing a file with `replace_all: true` must send full-file oldText and newText with all
    // matches replaced (matching TUI diff semantics).
    let mut edit_reqs = Vec::new();
    let r_edit = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid1, "prompt": [{ "type": "text", "text": "edit all" }] }),
        |req| {
            if req["method"] == "session/request_permission" {
                edit_reqs.push(req["params"].clone());
                return json!({ "outcome": { "outcome": "selected", "optionId": "reject-once" } });
            }
            json!(null)
        },
    );
    assert_eq!(r_edit["result"]["stopReason"], "end_turn", "{r_edit}");
    assert_eq!(edit_reqs.len(), 1, "expected 1 permission prompt for edit replace_all: {edit_reqs:?}");
    let edit_content = edit_reqs[0]["toolCall"]["content"].as_array().expect("edit content array");
    assert!(
        edit_content.iter().any(|c| {
            c["type"] == "diff" && c["oldText"] == "alpha one\nalpha two\n" && c["newText"] == "beta one\nbeta two\n"
        }),
        "ACP edit with replace_all must include full-file oldText and newText: {}",
        edit_reqs[0]
    );

    // Same call in a NEW session MUST ask again.
    let s2 = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid2 = s2["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s2}")).to_string();
    let mut asks_s2 = 0usize;
    let r3 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid2, "prompt": [{ "type": "text", "text": "third" }] }),
        |req| {
            if req["method"] == "session/request_permission" {
                asks_s2 += 1;
                return json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } });
            }
            json!(null)
        },
    );
    assert_eq!(r3["result"]["stopReason"], "end_turn", "{r3}");
    assert_eq!(asks_s2, 1, "new ACP session must prompt again for session-scoped grant");

    rpc.kill_and_wait();
}

/// Shared authorization-path previews, locations, and resolved commands reach ACP for:
/// 1. custom tools with a public `preview` callback returning `ui.Diff` + declared `access`,
/// 2. custom tools returning a text `preview` (`ui.Lines`),
/// 3. dynamically resolved command tools (`command = function(...)`),
/// 4. built-in `edit`/`write` on raw untrimmed paths (`"notes "` vs `"notes"`),
/// 5. a shadowed `"write"` tool with its own preview (never reading disk via hardcoded tool name).
#[test]
fn acp_permission_request_projects_custom_preview_dynamic_command_and_shadowed_write() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    std::fs::write(ws.path().join("notes"), "from-unspaced\n").unwrap();
    #[cfg(unix)]
    std::fs::write(ws.path().join("notes "), "from-trailing-space\n").unwrap();
    std::fs::write(ws.path().join("secret-on-disk.txt"), "DISK_SHOULD_NOT_LEAK\n").unwrap();

    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut turns = vec![
        json!({ "tool_calls": [{ "name": "custom_patcher", "arguments": { "target": "notes", "replacement": "patched\n" } }] }),
        json!({ "text": "custom-diff-done" }),
        json!({ "tool_calls": [{ "name": "custom_note", "arguments": { "env": "staging" } }] }),
        json!({ "text": "custom-text-done" }),
        json!({ "tool_calls": [{ "name": "dyn_runner", "arguments": { "dialect": "bash", "script": "echo DYN_CMD_PREVIEW" } }] }),
        json!({ "text": "dyn-cmd-done" }),
    ];
    #[cfg(unix)]
    {
        turns.push(json!({ "tool_calls": [{ "name": "edit", "arguments": { "path": "notes ", "old_string": "from-trailing-space", "new_string": "updated-trailing-space" } }] }));
        turns.push(json!({ "text": "space-edit-done" }));
    }
    let faux_path = home.path().join("faux-acp-custom-preview.json");
    std::fs::write(&faux_path, serde_json::to_string(&turns).unwrap()).unwrap();

    let cfg_dir = home.path().join("config");
    let plugins_dir = cfg_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("custom-preview-tools.lua"),
        r#"
        return {
            name = "custom-preview-tools",
            inject = { "tools" },
            apply = function(ctx)
                ctx.tools:register("custom_patcher", {
                    description = "custom file patcher with preview",
                    side_effect = "write",
                    access = function(a)
                        return { { kind = "file", op = "write", path = a.target, recursive = false } }
                    end,
                    parameters = {
                        type = "object",
                        properties = { target = { type = "string" }, replacement = { type = "string" } },
                        required = { "target", "replacement" },
                    },
                    preview = function(a, cwd)
                        local p = mare.fs.resolve_path(a.target, cwd).logical
                        local old = mare.fs.read(p) or ""
                        return ui.Diff({ old = old, new = a.replacement, path = a.target, full_path = p })
                    end,
                    execute = function() return { content = "patched" } end,
                })
                ctx.tools:register("custom_note", {
                    description = "custom tool with text preview",
                    side_effect = "external",
                    parameters = {
                        type = "object",
                        properties = { env = { type = "string" } },
                        required = { "env" },
                    },
                    preview = function(a)
                        return ui.Lines({ ui.Line({ ui.Span("Deploying release to " .. tostring(a.env)) }) })
                    end,
                    execute = function() return { content = "deployed" } end,
                })
                ctx.tools:register("dyn_runner", {
                    description = "dynamic command resolver tool",
                    side_effect = "external",
                    command_tool = true,
                    command = function(a)
                        return { shell = a.dialect or "bash", arg = "script" }
                    end,
                    parameters = {
                        type = "object",
                        properties = { dialect = { type = "string" }, script = { type = "string" } },
                        required = { "script" },
                    },
                    execute = function() return { content = "ran" } end,
                })
            end,
        }
        "#,
    )
    .unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        r#"mare.setup {
            plugins = {
                permissions = {
                    enabled = true,
                    preset = "read-only",
                    rules = {
                        { tool = "custom_note", decision = "prompt" },
                        { argv = { "echo" }, decision = "prompt" },
                    },
                },
            },
        }
"#,
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let mut ask_turn = |prompt_text: &str| -> Value {
        let mut reqs = Vec::new();
        let resp = rpc.call_raw(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": prompt_text }] }),
            |req| {
                if req["method"] == "session/request_permission" {
                    reqs.push(req["params"].clone());
                    return json!({ "outcome": { "outcome": "selected", "optionId": "reject-once" } });
                }
                json!(null)
            },
        );
        assert_eq!(resp["result"]["stopReason"], "end_turn", "{resp}");
        assert_eq!(reqs.len(), 1, "expected 1 permission prompt for {prompt_text}: {reqs:?}");
        reqs.remove(0)
    };

    // 1. Custom tool `ui.Diff` preview + declared `access` reaches ACP as `Diff` + `locations`.
    let p_diff = ask_turn("run custom_patcher");
    let expected_notes = cwd.join("notes").to_string_lossy().into_owned();
    assert_eq!(p_diff["toolCall"]["locations"][0]["path"], json!(expected_notes), "{p_diff}");
    assert!(
        p_diff["toolCall"]["content"].as_array().unwrap().iter().any(|c| {
            c["type"] == "diff"
                && c["path"] == expected_notes
                && c["oldText"] == "from-unspaced\n"
                && c["newText"] == "patched\n"
        }),
        "custom tool Diff preview must reach ACP: {p_diff}"
    );

    // 2. Custom tool text preview reaches ACP content.
    let p_text = ask_turn("run custom_note");
    assert!(
        p_text["toolCall"]["content"].to_string().contains("Deploying release to staging"),
        "custom tool text preview must reach ACP: {p_text}"
    );

    // 3. Dynamic command resolver tool forwards frozen command text to ACP.
    let p_dyn = ask_turn("run dyn_runner");
    assert!(
        p_dyn["toolCall"]["content"].to_string().contains("echo DYN_CMD_PREVIEW"),
        "dynamic command resolver must forward resolved command to ACP: {p_dyn}"
    );

    // 4. Raw untrimmed path `"notes "` is preserved in ACP `edit` diff and locations.
    #[cfg(unix)]
    {
        let p_space = ask_turn("edit notes space");
        let expected_spaced = cwd.join("notes ").to_string_lossy().into_owned();
        assert_eq!(p_space["toolCall"]["locations"][0]["path"], json!(expected_spaced), "{p_space}");
        assert!(
            p_space["toolCall"]["content"].as_array().unwrap().iter().any(|c| {
                c["type"] == "diff"
                    && c["path"] == expected_spaced
                    && c["oldText"] == "from-trailing-space\n"
                    && c["newText"] == "updated-trailing-space\n"
            }),
            "raw untrimmed 'notes ' path must be preserved in ACP diff & locations: {p_space}"
        );
    }

    rpc.kill_and_wait();

    // 5. Shadowing `"write"` with a custom tool uses that tool's own preview and never reads disk.
    let shadow_home = tempfile::tempdir().unwrap();
    let shadow_cfg = shadow_home.path().join("config");
    let shadow_plugins = shadow_cfg.join("plugins");
    std::fs::create_dir_all(&shadow_plugins).unwrap();
    std::fs::write(
        shadow_plugins.join("shadow-write.lua"),
        r#"
        return {
            name = "shadow-write",
            inject = { "tools" },
            apply = function(ctx)
                ctx.tools:register("write", {
                    description = "remote KV writer shadowing built-in write",
                    side_effect = "external",
                    parameters = {
                        type = "object",
                        properties = { path = { type = "string" }, content = { type = "string" } },
                        required = { "path", "content" },
                    },
                    preview = function(a)
                        return ui.Line({ ui.Span("Shadow remote KV put: " .. tostring(a.path)) })
                    end,
                    execute = function() return { content = "ok" } end,
                })
            end,
        }
        "#,
    )
    .unwrap();
    std::fs::write(
        shadow_cfg.join("init.lua"),
        r#"mare.setup {
            plugins = {
                permissions = {
                    enabled = true,
                    preset = "read-only",
                    rules = { { tool = "write", decision = "prompt" } },
                },
            },
        }
"#,
    )
    .unwrap();
    let shadow_faux = shadow_home.path().join("faux-shadow.json");
    std::fs::write(
        &shadow_faux,
        json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "secret-on-disk.txt", "content": "remote-val" } }] },
            { "text": "shadow-done" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut shadow_cmd = mare_cmd();
    shadow_cmd
        .args(["--model", "faux/echo", "acp"])
        .env("MARE_HOME", shadow_home.path())
        .env("MARE_FAUX_SCRIPT", &shadow_faux)
        .stderr(Stdio::piped());
    let mut srpc = RpcClient::spawn_cmd(&mut shadow_cmd);
    let _ = srpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let ss = srpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let ssid = ss["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{ss}")).to_string();
    let mut shadow_reqs = Vec::new();
    let sresp = srpc.call_raw(
        "session/prompt",
        json!({ "sessionId": ssid, "prompt": [{ "type": "text", "text": "shadow write" }] }),
        |req| {
            if req["method"] == "session/request_permission" {
                shadow_reqs.push(req["params"].clone());
                return json!({ "outcome": { "outcome": "selected", "optionId": "reject-once" } });
            }
            json!(null)
        },
    );
    assert_eq!(sresp["result"]["stopReason"], "end_turn", "{sresp}");
    assert_eq!(shadow_reqs.len(), 1, "{shadow_reqs:?}");
    let sp = &shadow_reqs[0];
    let sp_str = sp.to_string();
    assert!(
        sp_str.contains("Shadow remote KV put: secret-on-disk.txt"),
        "shadowed write must display its own preview: {sp}"
    );
    assert!(
        !sp_str.contains("DISK_SHOULD_NOT_LEAK"),
        "shadowed write must NEVER read disk via hardcoded tool name: {sp}"
    );
    assert!(
        sp["toolCall"]["content"].as_array().unwrap().iter().all(|c| c["type"] != "diff"),
        "shadowed write without diff preview must not fabricate Diff: {sp}"
    );
    srpc.kill_and_wait();
}
