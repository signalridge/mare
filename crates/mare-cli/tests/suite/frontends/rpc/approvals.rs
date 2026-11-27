use super::*;

#[test]
fn rpc_approval_waits_and_abort_cancels_by_request_id() {
    use std::io::{BufRead, Write};
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([
        {"tool_calls": [{"name":"bash", "arguments":{"command":"touch should-not-run", "sandbox_permissions":"all", "justification":"test"}}]},
        {"text":"done"}
    ]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--permission", "read-only", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if tx.send(serde_json::from_str::<Value>(&line).unwrap()).is_err() {
                break;
            }
        }
    });
    let send = |input: &mut std::process::ChildStdin, msg: Value| {
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
    };
    let next = |stage: &str, timeout: Duration| {
        rx.recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("RPC response timed out while waiting for {stage}: {error:?}"))
    };
    send(
        &mut input,
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocol_version": "3.0", "capabilities":{"approvals":true}, "callback_timeout_ms":30}}),
    );
    let sid = loop {
        let m = next("initialize response", Duration::from_secs(30));
        if m["id"] == 1 {
            break m["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
        }
    };
    send(
        &mut input,
        json!({"jsonrpc":"2.0", "id":2, "method":"agent.prompt", "params":{"session_id":sid, "input":"go"}}),
    );
    let ask = loop {
        let m = next("approval.request", Duration::from_secs(10));
        if m["method"] == "approval.request" {
            break m;
        }
    };
    // The property is that the request outlives callbackTimeoutMs (30 ms) and ends only by the
    // abort: watch the stream for 60 ms (sleep-ok: the window *is* the timeout under test), then
    // round-trip state.get to see the turn still busy before aborting. That the forwarded
    // approval ignores callbackTimeoutMs regardless of scheduling is proven with a paused clock
    // in src/rpc/approval_timeout_tests.rs.
    let past_callback_timeout = Instant::now() + Duration::from_millis(60);
    while let Some(remaining) = past_callback_timeout.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(remaining) {
            Ok(m) => {
                assert_ne!(m["method"], "$/cancel_request", "approval.request was cancelled at callbackTimeoutMs");
                assert!(
                    !(m["method"] == "event" && m["params"]["type"] == "turn/completed"),
                    "the turn ended before abort"
                );
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
            Err(err) => panic!("RPC stream closed while the approval was pending: {err:?}"),
        }
    }
    send(&mut input, json!({"jsonrpc":"2.0", "id":99, "method":"agent.get", "params":{"session_id":sid}}));
    loop {
        let m = next("agent.get response while the approval is pending", Duration::from_secs(10));
        assert_ne!(m["method"], "$/cancel_request", "approval.request was cancelled before abort");
        assert!(!(m["method"] == "event" && m["params"]["type"] == "turn/completed"), "the turn ended before abort");
        if m["id"] == 99 {
            assert!(m.get("error").is_none(), "{m}");
            assert_eq!(m["result"]["busy"], true, "the turn must still wait for the approval: {m}");
            assert_eq!(m["result"]["turn_cancelled"], false, "{m}");
            break;
        }
    }
    send(&mut input, json!({"jsonrpc":"2.0", "id":3, "method":"agent.cancel", "params":{"session_id":sid}}));
    let mut cancelled = false;
    let mut aborted = false;
    let mut ended = false;
    while !(cancelled && aborted) {
        let m = next("$/cancel_request and abort response", Duration::from_secs(10));
        ended |= m["method"] == "event" && m["params"]["type"] == "turn/completed";
        if m["method"] == "$/cancel_request" {
            assert_eq!(m["params"]["id"], ask["id"]);
            cancelled = true;
        }
        if m["id"] == 3 {
            aborted = true;
        }
    }
    send(&mut input, json!({"jsonrpc":"2.0", "id":ask["id"], "result":{"decision":"allow"}}));
    while !ended {
        let m = next("turn/completed after late approval response", Duration::from_secs(10));
        ended = m["method"] == "event" && m["params"]["type"] == "turn/completed";
    }
    send(&mut input, json!({"jsonrpc":"2.0", "id":4, "method":"shutdown", "params":{}}));
    let _ = child.wait_timeout(Duration::from_secs(10));
    assert!(!home.path().join("should-not-run").exists(), "late approval must not execute the command");
}

#[test]
fn rpc_explicit_approval_deadline_cancels_the_request() {
    use std::io::{BufRead, Write};
    use std::time::Duration;
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([
        {"tool_calls": [{"name":"bash", "arguments":{"command":"touch should-not-run", "sandbox_permissions":"all", "justification":"test"}}]},
        {"text":"done"}
    ]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--permission", "read-only", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if tx.send(serde_json::from_str::<Value>(&line).unwrap()).is_err() {
                break;
            }
        }
    });
    let send = |input: &mut std::process::ChildStdin, msg: Value| {
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
    };
    let next = |stage: &str, timeout: Duration| {
        rx.recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("RPC response timed out while waiting for {stage}: {error:?}"))
    };
    send(
        &mut input,
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocol_version": "3.0", "capabilities":{"approvals":true}, "approval_timeout_ms":40}}),
    );
    let sid = loop {
        let m = next("initialize response", Duration::from_secs(30));
        if m["id"] == 1 {
            break m["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
        }
    };
    send(
        &mut input,
        json!({"jsonrpc":"2.0", "id":2, "method":"agent.prompt", "params":{"session_id":sid, "input":"go"}}),
    );
    let ask = loop {
        let m = next("approval.request", Duration::from_secs(10));
        if m["method"] == "approval.request" {
            break m;
        }
    };
    let cancel = loop {
        let m = next("$/cancel_request", Duration::from_secs(10));
        if m["method"] == "$/cancel_request" {
            break m;
        }
    };
    assert_eq!(cancel["params"]["id"], ask["id"]);
    send(&mut input, json!({"jsonrpc":"2.0", "id":ask["id"], "result":{"decision":"allow"}}));
    loop {
        let m = next("turn/completed after late approval response", Duration::from_secs(10));
        if m["method"] == "event" && m["params"]["type"] == "turn/completed" {
            break;
        }
    }
    send(&mut input, json!({"jsonrpc":"2.0", "id":3, "method":"shutdown", "params":{}}));
    let _ = child.wait_timeout(Duration::from_secs(10));
    assert!(!home.path().join("should-not-run").exists());
}

#[test]

fn rpc_independent_approval_policy_precedes_host_and_can_switch() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("approval-policy.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls":[{"name":"write","arguments":{"path":"blocked.txt","content":"blocked"}}]},
            {"text":"first done"},
            {"tool_calls":[{"name":"write","arguments":{"path":"allowed.txt","content":"allowed"}}]},
            {"text":"second done"}
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args([
        "--rpc",
        "--permission",
        "read-only",
        "--approval-policy",
        "never",
        "--model",
        "faux/echo",
        "--no-session",
    ])
    .env("MARE_HOME", home.path())
    .env("MARE_FAUX_SCRIPT", &script)
    .current_dir(home.path())
    .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init =
        rpc.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities":{"approvals":true}}), |_| {
            panic!("unexpected callback")
        });
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let options = rpc.call_raw("options.get", json!({"session_id": sid, "keys":["plugins.approvals.policy"]}), |_| {
        panic!("unexpected callback")
    });
    assert_eq!(options["result"]["plugins.approvals.policy"], "never", "{options}");
    let first =
        rpc.call_raw("agent.prompt", json!({"session_id":sid,"input":"first"}), |_| panic!("never must not call host"));
    assert!(first.get("error").is_none(), "{first}");
    let first_wait = rpc.call_raw("agent.wait", json!({"session_id":sid,"turn_id":first["result"]["turn_id"]}), |_| {
        panic!("never must not call host")
    });
    assert!(first_wait.get("error").is_none(), "{first_wait}");
    assert!(!home.path().join("blocked.txt").exists());
    // Several edits in one batch (cf. Codex app-server/tests/suite/v2/config_rpc.rs:2489):
    // sibling leaves under one nested option all land, none overwrites the other
    let nested = rpc.call_raw(
        "options.set",
        json!({"session_id": sid, "values":{
            "plugins.approvals.policy.mode":"granular",
            "plugins.approvals.policy.categories.tool":true,
            "plugins.approvals.policy.categories.network":false}}),
        |_| panic!("unexpected callback"),
    );
    assert!(nested.get("error").is_none(), "{nested}");
    let got = rpc.call_raw("options.get", json!({"session_id": sid, "keys":["plugins.approvals.policy"]}), |_| {
        panic!("unexpected callback")
    });
    assert_eq!(
        got["result"]["plugins.approvals.policy"],
        json!({"mode":"granular","categories":{"tool":true,"network":false}}),
        "{got}"
    );
    let set =
        rpc.call_raw("options.set", json!({"session_id": sid, "values":{"plugins.approvals.policy":"ask"}}), |_| {
            panic!("unexpected callback")
        });
    assert!(set.get("error").is_none(), "{set}");
    let mut approvals = 0;
    let second_start = rpc.call_raw("agent.prompt", json!({"session_id":sid,"input":"second"}), |request| {
        assert_eq!(request["method"], "approval.request", "{request}");
        approvals += 1;
        json!({"decision":"allow"})
    });
    assert!(second_start.get("error").is_none(), "{second_start}");
    let second =
        rpc.call_raw("agent.wait", json!({"session_id":sid,"turn_id":second_start["result"]["turn_id"]}), |request| {
            assert_eq!(request["method"], "approval.request", "{request}");
            approvals += 1;
            json!({"decision":"allow"})
        });
    assert!(second.get("error").is_none(), "{second}");
    assert_eq!(approvals, 1);
    assert_eq!(std::fs::read_to_string(home.path().join("allowed.txt")).unwrap(), "allowed");
    rpc.kill_and_wait();
}

#[test]
fn rpc_host_tool_call_id_matches_approval_and_terminal_event() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("host-tool-callid.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls":[{"id":"model-call-xyz","name":"host_echo","arguments":{"msg":"hi"}}]},
            {"text":"done"}
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--approval-policy", "ask", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities": {"approvals": true, "callbacks": true}}),
        |_| panic!("unexpected callback"),
    );
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let reg = rpc.call_raw(
        "tools.register",
        json!({"name": "host_echo", "description": "echo", "parameters": {"type": "object"}, "callbacks": ["execute"]}),
        |_| panic!("unexpected callback"),
    );
    assert!(reg.get("error").is_none(), "{reg}");
    let sub = rpc.call_raw(
        "hooks.subscribe",
        json!({"scope": "all", "event": "tool/before", "callbacks": ["handle"]}),
        |_| panic!("unexpected callback"),
    );
    assert!(sub.get("error").is_none(), "{sub}");
    let started = rpc.call_raw("agent.prompt", json!({"session_id":sid,"input":"go"}), |_| json!(null));
    let rid = rpc.request("agent.wait", json!({"session_id":sid,"turn_id":started["result"]["turn_id"]}));
    let mut approval_call_id = None;
    let mut tool_call_id = None;
    let mut tool_idempotency_key = None;
    let mut tool_cwd = None;
    let mut tool_transport_id = None;
    let mut terminal_call_id = None;
    loop {
        let m = rpc.read();
        if m.get("method").and_then(|v| v.as_str()) == Some("event") {
            let p = &m["params"];
            if p.get("type").and_then(|v| v.as_str()) == Some("item/completed")
                && p["data"]["item"]["type"] == "tool_call"
            {
                terminal_call_id = p["data"]["item"]["tool_call_id"].as_str().map(str::to_string);
            }
            continue;
        }
        if let Some(method) = m.get("method").and_then(|v| v.as_str()) {
            let Some(cid) = m.get("id").cloned() else {
                continue;
            };
            let result = match method {
                "callback.call" => match m["params"]["callback"].as_str() {
                    Some("handle") => json!({"value": {"decision": "ask", "reason": "needs approval"}}),
                    Some("execute") => {
                        let args = &m["params"]["arguments"];
                        tool_call_id = m["params"]["tool_call_id"]
                            .as_str()
                            .or_else(|| args["ctx"]["tool_call_id"].as_str())
                            .map(str::to_string);
                        tool_idempotency_key = args["ctx"]["idempotency_key"].as_str().map(str::to_string);
                        tool_cwd = args["ctx"]["cwd"].as_str().map(str::to_string);
                        tool_transport_id = m.get("id").cloned();
                        json!({"content": "from-host"})
                    }
                    other => panic!("unexpected callback {other:?}: {m}"),
                },
                "approval.request" => {
                    approval_call_id = m["params"]["tool_call_id"].as_str().map(str::to_string);
                    json!({"decision": "allow"})
                }
                other => panic!("unexpected callback {other}: {m}"),
            };
            rpc.send(json!({"jsonrpc": "2.0", "id": cid, "result": result}));
            continue;
        }
        if m["id"] == rid {
            assert!(m.get("error").is_none(), "{m}");
            break;
        }
    }
    let approval_id = approval_call_id.expect("approval.request tool_call_id");
    let call_id = tool_call_id.expect("tool.call tool_call_id");
    let event_id = terminal_call_id.expect("item/completed tool_call tool_call_id");
    assert!(!call_id.is_empty());
    assert_eq!(approval_id, call_id, "approval.request and tool.call tool_call_id must match");
    assert_eq!(event_id, call_id, "item/completed tool_call tool_call_id and tool.call tool_call_id must match");
    assert_eq!(
        tool_idempotency_key.as_deref(),
        Some(call_id.as_str()),
        "tool.call idempotency_key must match tool_call_id"
    );
    let expected_cwd = dunce::canonicalize(home.path()).unwrap_or_else(|_| home.path().to_path_buf());
    let actual_cwd = std::path::PathBuf::from(tool_cwd.expect("tool.call cwd"));
    let actual_cwd = dunce::canonicalize(&actual_cwd).unwrap_or(actual_cwd);
    assert_eq!(actual_cwd, expected_cwd, "tool.call cwd must match session cwd");
    let tid = tool_transport_id.expect("JSON-RPC request id on tool.call");
    assert_ne!(tid, json!(call_id), "JSON-RPC transport id should be distinct from tool_call_id");
    assert_ne!(call_id, "call-3");
    let shutdown = rpc.call_raw("shutdown", json!({}), |_| panic!("unexpected callback"));
    assert!(shutdown.get("error").is_none(), "{shutdown}");
    rpc.wait_exit();
}

/// Reference: Codex app-server `serverRequest/resolved` correlation (`turn_start.rs:3191-3206`).
/// `approval/requested` is published after the record is written and before waiting for the RPC
/// `approval.request` callback; `approval/resolved` follows the decision with the same
/// `approval_id` and `tool_call_id`, and both durable session records hold that `approval_id`.
#[test]
fn approval_requested_precedes_the_wait_and_resolved_carries_the_same_id() {
    use std::io::{BufRead, Write};
    use std::time::Duration;
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls":[{"id":"call-req-1","name":"write","arguments":{"path":"ok.txt","content":"hi"}}]},
            {"text":"done"}
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--permission", "read-only", "--approval-policy", "ask", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut input = child.0.stdin.take().unwrap();
    let output = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if tx.send(serde_json::from_str::<Value>(&line).unwrap()).is_err() {
                break;
            }
        }
    });
    let send = |input: &mut std::process::ChildStdin, msg: Value| {
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
    };
    let next = |stage: &str, timeout: Duration| {
        rx.recv_timeout(timeout)
            .unwrap_or_else(|error| panic!("RPC response timed out while waiting for {stage}: {error:?}"))
    };
    send(
        &mut input,
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocol_version": "3.0", "capabilities":{"approvals":true}}}),
    );
    let sid = loop {
        let m = next("initialize response", Duration::from_secs(30));
        if m["id"] == 1 {
            break m["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
        }
    };
    send(
        &mut input,
        json!({"jsonrpc":"2.0", "id":2, "method":"agent.prompt", "params":{"session_id":sid, "input":"go"}}),
    );
    let mut order: Vec<&'static str> = Vec::new();
    let mut requested_data: Option<Value> = None;
    let mut resolved_data: Option<Value> = None;
    let mut ask_call_id: Option<String> = None;
    let mut ask_approval_id: Option<String> = None;
    loop {
        let m = next("approval lifecycle", Duration::from_secs(15));
        if m.get("method").and_then(|v| v.as_str()) == Some("event") {
            let p = &m["params"];
            match p.get("type").and_then(|v| v.as_str()) {
                Some("approval/requested") => {
                    order.push("requested");
                    requested_data = Some(p["data"].clone());
                }
                Some("approval/resolved") => {
                    order.push("resolved");
                    resolved_data = Some(p["data"].clone());
                }
                Some("turn/completed") => break,
                _ => {}
            }
        } else if m.get("method").and_then(|v| v.as_str()) == Some("approval.request") {
            order.push("callback");
            assert!(
                requested_data.is_some(),
                "approval/requested must precede the approval.request callback (order={order:?})"
            );
            ask_call_id = m["params"]["tool_call_id"].as_str().map(str::to_string);
            ask_approval_id = m["params"]["approval_id"].as_str().map(str::to_string);
            // Answer late (after observing requested -> callback).
            send(&mut input, json!({"jsonrpc":"2.0", "id":m["id"], "result":{"decision":"allow"}}));
        }
    }
    send(&mut input, json!({"jsonrpc":"2.0", "id":3, "method":"shutdown", "params":{}}));
    while next("shutdown response", Duration::from_secs(10))["id"] != 3 {}
    let _ = child.wait_timeout(Duration::from_secs(10));

    assert_eq!(order, vec!["requested", "callback", "resolved"]);
    let req = requested_data.expect("approval/requested event");
    let res = resolved_data.expect("approval/resolved event");
    let approval_id = req["approval_id"].as_str().expect("requested approval_id");
    assert!(!approval_id.is_empty());
    assert_eq!(ask_approval_id.as_deref(), Some(approval_id));
    assert_eq!(req["category"], "tool");
    assert_eq!(req["tool_call_id"], ask_call_id.as_deref().unwrap());
    assert_eq!(res["approval_id"], approval_id);
    assert_eq!(res["category"], "tool");
    assert_eq!(res["tool_call_id"], req["tool_call_id"]);
    assert_eq!(res["result"]["decision"], "allow");
    assert_eq!(res["result"]["call"]["tool_call_id"], req["tool_call_id"]);

    let sessions_dir = home.path().join("state/sessions");
    let mut raw = String::new();
    let mut stack = vec![sessions_dir.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                raw.push_str(&std::fs::read_to_string(&p).expect("session file"));
                raw.push('\n');
            }
        }
    }
    assert!(!raw.is_empty(), "expected persisted session file in {}", sessions_dir.display());
    let customs: Vec<Value> =
        raw.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|e| e["type"] == "custom").collect();
    let rec_req = customs
        .iter()
        .find(|e| e["kind"] == "approval/requested" && e["data"]["approval_id"] == approval_id)
        .expect("approval/requested session record");
    let rec_res = customs
        .iter()
        .find(|e| e["kind"] == "approval/resolved" && e["data"]["approval_id"] == approval_id)
        .expect("approval/resolved session record");
    assert_eq!(rec_req["data"]["category"], "tool");
    assert_eq!(rec_req["data"]["tool_call_id"], req["tool_call_id"]);
    assert_eq!(rec_res["data"]["category"], "tool");
    assert_eq!(rec_res["data"]["result"]["decision"], "allow");
}
