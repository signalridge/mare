use super::*;

#[test]
fn rpc_python_client_end_to_end() {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::Duration;

    use crate::common::rpc::RpcClient;

    let malformed_results = || -> Vec<Value> {
        vec![
            json!({}),
            Value::Null,
            json!({ "content": null }),
            json!({ "content": 42 }),
            json!({ "content": {} }),
            json!({ "content": [{}] }),
            json!({ "content": [{ "type": "text" }] }),
            json!({ "content": "ignored", "is_error": "not-a-boolean" }),
        ]
    };
    let malformed_count = malformed_results().len();
    let mut script_turns = vec![
        json!({ "tool_calls": [{ "name": "lookup", "arguments": { "q": "answer", "n": null, "a": [], "o": {} } }] }),
        json!({ "tool_calls": vec![json!({ "name": "malformed", "arguments": {} }); malformed_count] }),
        json!({ "tool_calls": [{ "name": "bash", "arguments": { "command": "echo should-not-run" } }] }),
        json!({ "text": "done" }),
        json!({ "tool_calls": [{ "name": "slow", "arguments": {} }] }),
        json!({ "text": "never reached" }),
    ];

    let tmp = tempfile::tempdir().unwrap();
    let script = tmp.path().join("faux.json");
    std::fs::write(&script, serde_json::to_string(&script_turns).unwrap()).unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let home = tmp.path().join("home");
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);

    // Uninitialized connection: shutdown works, business methods fail (-32600).
    let mut shut_only = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo", "--no-session"], &env, &work);
    assert_eq!(shut_only.call("shutdown", json!({})), json!({}));
    shut_only.wait_exit();

    let mut c = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo", "--no-session"], &env, &work);
    let pre_init_calls = [
        ("agent.prompt", json!({ "session_id": "s1", "input": "blocked" })),
        ("options.get", json!({})),
        ("options.set", json!({ "values": { "effort": "low" } })),
        (
            "tools.register",
            json!({ "name": "blocked", "description": "blocked", "parameters": { "type": "object" }, "callbacks": ["execute"] }),
        ),
        ("hooks.subscribe", json!({ "event": "status/changed", "callbacks": ["handle"] })),
        ("llm.list_models", json!({})),
        ("commands.run", json!({ "line": "/help" })),
        ("agent.get", json!({ "session_id": "s1" })),
        ("status.get", json!({})),
    ];
    for (method, params) in &pre_init_calls {
        let err = c.call_err(method, params.clone());
        assert_eq!(err["code"], -32600, "{method}: {err}");
    }
    for bad_version in ["1.7", "2", "2.invalid", "2.0.1", "", " 2.0"] {
        let err = c.call_err("initialize", json!({ "protocol_version": bad_version }));
        assert_eq!(err["data"]["code"], "unsupported_version", "{bad_version}: {err}");
        for (method, params) in &pre_init_calls[..3] {
            let after = c.call_err(method, params.clone());
            assert_eq!(after["code"], -32600, "{bad_version} {method}: {after}");
        }
    }

    let init = c.call(
        "initialize",
        json!({
            "protocol_version": "3.0",
            "client": { "name": "rpc_client.rs" },
            "capabilities": { "callbacks": true, "ui": true },
            "callback_timeout_ms": 10000,
        }),
    );
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    let methods = init["capabilities"]["methods"].as_array().unwrap();
    assert!(init["protocol_version"].as_str().unwrap().starts_with("3."), "{init}");
    assert!(methods.iter().any(|m| m == "agent.prompt"), "{init}");
    for m in ["realtime.start", "realtime.append", "realtime.stop"] {
        assert!(!methods.iter().any(|v| v == m), "unexpected {m}: {init}");
    }
    for field in ["video", "realtime", "media"] {
        assert!(init["capabilities"].get(field).is_none(), "unexpected {field}: {init}");
    }

    for (method, params) in [
        ("realtime.start", json!({ "outputModality": "audio", "voice": "alloy" })),
        ("realtime.append", json!({ "sessionId": "rt-1", "text": "hi" })),
        ("realtime.stop", json!({ "sessionId": "rt-1" })),
    ] {
        let err = c.call_err(method, params);
        assert!(err["code"] == -32601 && err["data"] == json!({ "code": "method_not_found" }), "{method}: {err}");
    }

    for (cap, payload) in [
        (
            "video",
            json!({ "session_id": sid, "input": [{ "type": "text", "text": "watch" }, { "type": "video", "mime": "video/mp4", "data": "AAAA" }] }),
        ),
        (
            "video",
            json!({ "session_id": sid, "input": [{ "type": "text", "text": "watch" }, { "type": "video", "path": "clip.mp4" }] }),
        ),
        (
            "video",
            json!({ "session_id": sid, "input": [{ "type": "text", "text": "watch" }, { "type": "file", "path": "clip.mp4" }] }),
        ),
        (
            "audio",
            json!({ "session_id": sid, "input": [{ "type": "text", "text": "listen" }, { "type": "audio", "path": "note.txt" }] }),
        ),
        (
            "audio",
            json!({ "session_id": sid, "input": [{ "type": "text", "text": "listen" }, { "type": "image", "data": "AAAA", "mime": "audio/ogg" }] }),
        ),
    ] {
        let err = c.call_err("agent.prompt", payload.clone());
        assert!(
            err["code"] == -32000 && err["data"] == json!({ "code": "unsupported", "data": { "capability": cap } }),
            "{payload}: {err}"
        );
        assert!(err["message"].as_str().unwrap().to_lowercase().contains("unsupported"), "{payload}: {err}");
    }

    for event in ["provider/request", "provider/transport", "provider/event"] {
        let err = c.call_err("hooks.subscribe", json!({ "scope": "all", "event": event, "callbacks": ["handle"] }));
        assert!(err["code"] == -32602, "{event}: {err}");
    }

    for bad in [
        json!({ "name": "bad", "description": "bad", "parameters": { "type": "object", "properties": { "x": { "format": "email" } } }, "callbacks": ["execute"] }),
        json!({ "name": "missing", "description": "missing" }),
    ] {
        let err = c.call_err("tools.register", bad.clone());
        assert!(!err.is_null(), "{err}");
    }

    let lookup_reg = c.call(
        "tools.register",
        json!({
            "name": "lookup",
            "description": "Look something up on the host",
            "parameters": {
                "type": "object",
                "properties": {
                    "q": { "type": "string" },
                    "n": { "const": null },
                    "a": { "type": "array" },
                    "o": { "type": "object" },
                },
                "required": ["q", "n", "a", "o"],
            },
            "side_effect": "read",
            "callbacks": ["execute"],
        }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    let slow_reg = c.call(
        "tools.register",
        json!({
            "name": "slow",
            "description": "Never answers",
            "parameters": { "type": "object", "properties": {} },
            "callbacks": ["execute"],
        }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    let malformed_reg = c.call(
        "tools.register",
        json!({
            "name": "malformed",
            "description": "Returns a malformed result from the host",
            "parameters": { "type": "object", "properties": {} },
            "callbacks": ["execute"],
        }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    // tool/before is a decision point: a remote participant is a voter of it
    c.call("hooks.subscribe", json!({ "scope": "all", "event": "tool/before", "callbacks": ["handle"] }));

    // 1. prompt: valid host tool, malformed host responses, hook-blocked bash, final answer.
    let mut malformed_iter = malformed_results().into_iter();
    let slow_req_id = std::sync::Arc::new(std::sync::Mutex::new(None::<Value>));
    let slow_req_id_cb = slow_req_id.clone();
    c.on_method = Some(Box::new(move |client, msg| {
        if msg.get("method").and_then(Value::as_str) != Some("callback.call")
            || msg["params"].get("callback").and_then(Value::as_str) != Some("execute")
        {
            return false;
        }
        let reg_id = msg["params"]["registration_id"].as_str().unwrap();
        let params = msg["params"]["arguments"].clone();
        client.tool_calls.push(json!({ "name": if reg_id == lookup_reg { "lookup" } else if reg_id == malformed_reg { "malformed" } else { "slow" }, "arguments": params }));
        if reg_id == lookup_reg {
            let cb_req_id = &msg["id"];
            client.tool_update(cb_req_id, json!({ "output": "searching..." }));
            client.tool_update(&json!("unknown-call-id"), json!({ "output": "should-be-dropped" }));
            client.send(json!({ "id": cb_req_id, "result": { "content": "42" } }));
            client.tool_update(cb_req_id, json!({ "output": "late-after-result" }));
            return true;
        }
        if reg_id == malformed_reg {
            let reply = malformed_iter.next().expect("extra malformed call");
            client.send(json!({ "id": msg["id"], "result": reply }));
            return true;
        }
        if reg_id == slow_reg {
            *slow_req_id_cb.lock().unwrap() = Some(msg["id"].clone());
        }
        true
    }));

    let r = c.call("agent.prompt", json!({ "session_id": sid, "input": "go" }));
    let turn = r["turn_id"].clone();
    let result = c.call("agent.wait", json!({ "session_id": sid, "turn_id": turn }));
    assert_eq!(result["text"], "done", "{result}");
    let tool_names: Vec<&str> = c.tool_calls.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let mut expected_tools = vec!["lookup"];
    expected_tools.extend(std::iter::repeat_n("malformed", malformed_count));
    assert_eq!(tool_names, expected_tools, "{:?}", c.tool_calls);
    assert_eq!(c.tool_calls[0]["arguments"]["args"], json!({ "q": "answer", "n": null, "a": [], "o": {} }));
    let hook_names: BTreeSet<&str> = c.hook_calls.iter().map(|h| h["args"][0]["name"].as_str().unwrap()).collect();
    assert_eq!(hook_names, BTreeSet::from(["lookup", "malformed", "bash"]), "{:?}", c.hook_calls);
    let dump = result["messages"].to_string();
    assert!(dump.contains("42") && dump.contains("invalid_result") && dump.contains("denied by host"), "{dump}");
    let invalid_results: Vec<&Value> = result["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool_result" && m["tool_name"] == "malformed")
        .collect();
    assert_eq!(invalid_results.len(), malformed_count);
    assert!(invalid_results.iter().all(|m| m["is_error"] == true));
    assert!(
        c.events.iter().any(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call"),
        "no tool_call item/completed event"
    );
    let tool_results: Vec<&Value> = c
        .events
        .iter()
        .filter(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call")
        .map(|e| &e["data"]["item"])
        .collect();
    assert!(!tool_results.is_empty(), "no tool_call item/completed events");
    for tr in tool_results {
        assert!(tr["tool_call_id"].as_str().is_some_and(|s| !s.is_empty()), "{tr}");
        assert!(tr["name"].as_str().is_some_and(|s| !s.is_empty()), "{tr}");
        assert_eq!(tr["message"]["role"], "tool_result", "{tr}");
        assert!(tr["message"]["content"].is_array(), "{tr}");
        assert_eq!(tr["message"]["tool_call_id"], tr["tool_call_id"], "{tr}");
    }
    let tool_updates: Vec<&Value> =
        c.events.iter().filter(|e| e["type"] == "item/delta" && e["data"]["kind"] == "tool_output").collect();
    assert!(tool_updates.iter().any(|e| e["data"]["partial"]["output"] == "searching..."), "{tool_updates:?}");
    assert!(
        !tool_updates.iter().any(|e| {
            let out = e["data"]["partial"]["output"].as_str();
            matches!(out, Some("should-be-dropped" | "late-after-result"))
        }),
        "{tool_updates:?}"
    );
    assert!(c.events.iter().any(|e| e["turn_id"] == turn && e["type"] == "turn/started"));
    let eseq: Vec<u64> = c.events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(eseq.windows(2).all(|w| w[0] < w[1]), "{eseq:?}");

    let agent_state = c.call("agent.get", json!({ "session_id": sid }));
    assert_eq!(agent_state["session_id"], sid, "{agent_state}");
    let msgs = c.call("agent.list_messages", json!({ "session_id": sid }))["data"].as_array().unwrap().clone();
    assert!(msgs.iter().any(|m| m["role"] == "assistant"));
    assert!(c.call("session.list_items", json!({ "session_id": sid })).get("data").is_some());
    let models = c.call("llm.list_models", json!({}))["data"].as_array().unwrap().clone();
    assert!(models.iter().any(|m| m["ref"] == "faux/echo"));
    assert_eq!(c.call("llm.get_model", json!({ "session_id": sid }))["ref"], "faux/echo");
    for removed in ["models.list", "state.get", "ui.select.highlight"] {
        let err = c.call_err(removed, json!({}));
        assert_eq!(err["code"], -32601, "{removed}: {err}");
    }

    c.wait_status("idle");
    let states: Vec<&str> = c.statuses.iter().map(|s| s["state"].as_str().unwrap()).collect();
    assert!(states.contains(&"working") && states.last() == Some(&"idle"), "{states:?}");
    let seqs: Vec<u64> = c.statuses.iter().map(|s| s["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");

    // 2. prompt without waiting; abort while the host tool is pending
    c.events.clear();
    let r = c.call("agent.prompt", json!({ "session_id": sid, "input": "again" }));
    assert_eq!(r["status"], "started", "{r}");
    while !c.tool_calls.iter().any(|t| t["name"] == "slow") {
        let msg = c.read();
        c.dispatch(msg);
    }
    let slow_cb_id = slow_req_id.lock().unwrap().clone().expect("slow callback request id");
    c.tool_update(&slow_cb_id, json!({ "output": "working..." }));
    let live_update = loop {
        let ev = c.wait_event("item/delta");
        if ev["data"]["kind"] == "tool_output" {
            break ev;
        }
    };
    assert_eq!(live_update["data"]["partial"]["output"], "working...", "{live_update}");
    let err = c.call_err("session.rewind", json!({ "session_id": sid }));
    assert!(err.to_string().to_lowercase().contains("turn"), "{err}");
    let cancelled = c.call("agent.cancel", json!({ "session_id": sid }));
    assert!(cancelled["turn_id"].is_string(), "{cancelled}");
    assert!(cancelled["removed"].is_array(), "{cancelled}");
    c.tool_update(&slow_cb_id, json!({ "output": "late-after-cancel" }));
    let end = c.wait_event("turn/completed");
    assert!(!end.to_string().contains("never reached"), "{end}");
    assert!(
        !c.events.iter().any(|e| {
            e["type"] == "item/delta"
                && e["data"]["kind"] == "tool_output"
                && e["data"]["partial"]["output"] == "late-after-cancel"
        }),
        "{:?}",
        c.events
    );
    assert!(c.call("agent.get", json!({ "session_id": sid })).is_object());
    c.shutdown_and_wait();

    let bash_path = |p: &std::path::Path| p.to_string_lossy().replace('\\', "/");
    // 3. disconnect while a policy hook (tool/before) is pending
    let marker = work.join("ran");
    script_turns = vec![
        json!({ "tool_calls": [{ "name": "bash", "arguments": { "command": format!("echo ok > '{}'", bash_path(&marker)) } }] }),
        json!({ "text": "after" }),
    ];
    std::fs::write(&script, serde_json::to_string(&script_turns).unwrap()).unwrap();
    let mut c = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo", "--no-session"], &env, &work);
    let init = c.call(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 60000 }),
    );
    c.call("hooks.subscribe", json!({ "scope": "all", "event": "tool/before", "callbacks": ["handle"] }));
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    c.request("agent.prompt", json!({ "session_id": sid, "input": "go" }));
    loop {
        let m = c.read();
        if m.get("method").and_then(Value::as_str) == Some("callback.call") {
            break;
        }
    }
    c.close_stdin();
    c.wait_exit();
    assert!(!marker.exists(), "tool ran although its policy hook disconnected");

    // 4. approvals over RPC
    let one = work.join("one");
    let two = work.join("two");
    let subdir = work.join("subdir");
    std::fs::create_dir(&subdir).unwrap();
    let turns = vec![
        json!({ "tool_calls": [{ "name": "bash", "arguments": { "command": format!("cd '{}'", bash_path(&subdir)) } }] }),
        json!({ "tool_calls": [{ "name": "bash", "arguments": { "command": format!("echo ok > '{}'", bash_path(&one)) } }] }),
        json!({ "tool_calls": [{ "name": "bash", "arguments": { "command": format!("echo ok > '{}'", bash_path(&two)) } }] }),
        json!({ "text": "never reached" }),
    ];
    std::fs::write(&script, serde_json::to_string(&turns).unwrap()).unwrap();
    let mut c = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo", "--no-session"], &env, &work);
    let init = c.call(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true, "approvals": true } }),
    );
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    assert_eq!(init["capabilities"]["callbacks"]["approvals"], true, "{init}");
    c.call("hooks.subscribe", json!({ "scope": "all", "event": "tool/before", "callbacks": ["handle"] }));
    c.on_hook = Some(Box::new(|params| {
        if params["args"][0]["arguments"]["command"].as_str().unwrap().starts_with("cd ") {
            Value::Null
        } else {
            json!({ "decision": "ask", "reason": "needs approval" })
        }
    }));
    c.on_approval = Some(Box::new(|p| {
        if p["command"].as_str().unwrap().trim_end_matches('\'').ends_with("one") {
            json!("allow_session")
        } else {
            json!("abort")
        }
    }));
    let started = c.call("agent.prompt", json!({ "session_id": sid, "input": "go" }));
    let r = c.call("agent.wait", json!({ "session_id": sid, "turn_id": started["turn_id"] }));
    assert!(one.exists() && !two.exists(), "{r}");
    assert_eq!(r["error"]["code"], "aborted", "{r}");
    let first = &c.approvals[0];
    assert!(
        first["kind"] == "tool" && first["tool"] == "bash" && first["cwd"] == subdir.display().to_string(),
        "{first}"
    );
    assert_eq!(first["reason"], "needs approval", "{first}");
    c.shutdown_and_wait();

    // Structured permission grants
    let grant_dir = work.join("grant");
    std::fs::create_dir(&grant_dir).unwrap();
    let grant_dir_str = grant_dir.display().to_string();
    let turns = vec![
        json!({
            "tool_calls": [{
                "name": "request_permissions",
                "arguments": {
                    "additional_permissions": { "write": [grant_dir_str], "network": true },
                    "justification": "write the report",
                    "scope": "session",
                }
            }]
        }),
        json!({
            "tool_calls": [{
                "name": "write",
                "arguments": { "path": grant_dir.join("report").display().to_string(), "content": "ok" }
            }]
        }),
        json!({ "text": "done" }),
    ];
    std::fs::write(&script, serde_json::to_string(&turns).unwrap()).unwrap();
    let mut c =
        RpcClient::spawn(&["--rpc", "--permission", "read-only", "--model", "faux/echo", "--no-session"], &env, &work);
    let init = c.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }));
    let sid = init["session"]["session_id"].as_str().unwrap().to_string();
    let gd = grant_dir_str.clone();
    c.on_approval =
        Some(Box::new(move |_| json!({ "decision": "allow", "permissions": { "write": [gd] }, "scope": "session" })));
    let started = c.call("agent.prompt", json!({ "session_id": sid, "input": "write report" }));
    c.call("agent.wait", json!({ "session_id": sid, "turn_id": started["turn_id"] }));
    assert_eq!(c.approvals.len(), 1, "{:?}", c.approvals);
    assert_eq!(c.approvals[0]["requested_permissions"]["network"], true, "{:?}", c.approvals);
    assert_eq!(c.approvals[0]["scope"], "session", "{:?}", c.approvals);
    assert_eq!(std::fs::read_to_string(grant_dir.join("report")).unwrap(), "ok");
    c.shutdown_and_wait();

    // 5. queued_image_prompt (normal + crash recovery)
    fn run_queued_image_prompt(env: &BTreeMap<String, String>, work: &std::path::Path, crash: bool) {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (req_tx, req_rx) = mpsc::channel::<(String, Value)>();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_srv = release.clone();

        let server_thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let req_tx = req_tx.clone();
                let release = release_srv.clone();
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut req_line = String::new();
                    if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
                        return;
                    }
                    let path = req_line.split_whitespace().nth(1).unwrap_or("").to_string();
                    if path == "/__stop" {
                        return;
                    }
                    let mut content_len = 0usize;
                    loop {
                        let mut h = String::new();
                        reader.read_line(&mut h).unwrap();
                        if h == "\r\n" || h == "\n" || h.is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':')
                            && k.eq_ignore_ascii_case("content-length")
                        {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body_bytes = vec![0u8; content_len];
                    reader.read_exact(&mut body_bytes).unwrap();
                    let body: Value = serde_json::from_slice(&body_bytes).unwrap();
                    let _ = req_tx.send((path, body));
                    let (lock, cvar) = &*release;
                    let mut released = lock.lock().unwrap();
                    let wait_res = cvar.wait_timeout_while(released, Duration::from_secs(15), |r| !*r).unwrap();
                    released = wait_res.0;
                    assert!(*released, "first provider request was never released");
                    let payload = concat!(
                        "event: response.output_text.delta\n",
                        "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg\",\"delta\":\"done\"}\n\n",
                        "event: response.output_item.done\n",
                        "data: {\"type\":\"response.output_item.done\",\"item\":{\"id\":\"msg\",\"type\":\"message\",",
                        "\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"done\"}]}}\n\n",
                        "event: response.completed\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"test\",",
                        "\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    );
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = stream.write_all(resp.as_bytes());
                    let _ = stream.flush();
                });
            }
        });

        let mut rpc_env = env.clone();
        rpc_env.insert("OPENAI_API_KEY".to_string(), "test-key".to_string());
        let base_url = format!("plugins.provider-openai.base_url=http://127.0.0.1:{port}");
        let mut base_args = vec![
            "--rpc",
            "--yolo",
            "--model",
            "openai/gpt-6.1-terra",
            "--set",
            &base_url,
            "--set",
            "plugins.provider-openai.transport=http",
        ];
        if !crash {
            base_args.push("--no-session");
        }
        let mut c = RpcClient::spawn(&base_args, &rpc_env, work);
        let init = c.call("initialize", json!({ "protocol_version": "3.0" }));
        let sid = init["session"]["session_id"].as_str().unwrap().to_string();
        let started = c.call("agent.prompt", json!({ "session_id": sid, "input": "first turn" }));
        assert_eq!(started["status"], "started", "{started}");
        let (_, first) = req_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert!(first.to_string().contains("first turn"), "{first}");

        let image = json!({
            "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC",
            "mime": "image/png",
        });
        let second_image = json!({
            "data": "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7",
            "mime": "image/gif",
        });
        let queued = c.call(
            "agent.prompt",
            json!({
                "session_id": sid,
                "input": [
                    { "type": "text", "text": "queued image" },
                    { "type": "image", "data": image["data"], "mime": "image/png" },
                    { "type": "image", "data": second_image["data"], "mime": "image/gif" },
                ],
            }),
        );
        assert_eq!(queued["status"], "queued", "{queued}");

        if crash {
            c.kill_and_wait();
            {
                let (lock, cvar) = &*release;
                *lock.lock().unwrap() = true;
                cvar.notify_all();
            }
            let mut cont_args = base_args.clone();
            cont_args.push("--continue");
            c = RpcClient::spawn(&cont_args, &rpc_env, work);
            c.call("initialize", json!({ "protocol_version": "3.0" }));
            assert!(c.call("commands.run", json!({ "line": "/continue" })).is_object());
            let (_, retry) = req_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            assert!(retry.to_string().contains("first turn"), "{retry}");
        } else {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }

        let (path, body) = req_rx.recv_timeout(Duration::from_secs(15)).unwrap();
        assert_eq!(path, "/responses", "{path}");
        let is_disclosure = |item: &&Value| {
            item["content"]
                .as_str()
                .or_else(|| item["content"].as_array().and_then(|a| a.first()).and_then(|b| b["text"].as_str()))
                .is_some_and(|t| t.starts_with("<system-reminder>"))
        };
        let users: Vec<&Value> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["role"] == "user" && !is_disclosure(item))
            .collect();
        assert_eq!(users.len(), 2, "{body}");
        assert_eq!(
            users.last().unwrap()["content"],
            json!([
                { "type": "input_text", "text": "queued image" },
                { "type": "input_image", "image_url": format!("data:image/png;base64,{}", image["data"].as_str().unwrap()), "detail": "auto" },
                { "type": "input_image", "image_url": format!("data:image/gif;base64,{}", second_image["data"].as_str().unwrap()), "detail": "auto" },
            ]),
            "{body}"
        );
        for att in [&image, &second_image] {
            let needle = att["data"].as_str().unwrap();
            let count: usize =
                body["input"].as_array().unwrap().iter().map(|i| i.to_string().matches(needle).count()).sum();
            assert_eq!(count, 1, "{body}");
        }
        c.wait_event("turn/completed");
        c.shutdown_and_wait();
        assert!(req_rx.try_recv().is_err(), "queued prompt caused duplicate provider requests");

        if crash {
            let mut cont_args = base_args.clone();
            cont_args.push("--continue");
            let mut c2 = RpcClient::spawn(&cont_args, &rpc_env, work);
            let init2 = c2.call("initialize", json!({ "protocol_version": "3.0" }));
            let sid2 = init2["session"]["session_id"].as_str().unwrap().to_string();
            let completed = c2.call("agent.prompt", json!({ "session_id": sid2, "input": "fresh turn" }));
            assert_eq!(completed["status"], "started", "{completed}");
            c2.call("agent.wait", json!({ "session_id": sid2, "turn_id": completed["turn_id"] }));
            let (_, replayed) = req_rx.recv_timeout(Duration::from_secs(15)).unwrap();
            let users: Vec<&Value> = replayed["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["role"] == "user" && !is_disclosure(item))
                .collect();
            assert_eq!(users.len(), 3, "{replayed}");
            for att in [&image, &second_image] {
                let needle = att["data"].as_str().unwrap();
                let count: usize =
                    replayed["input"].as_array().unwrap().iter().map(|i| i.to_string().matches(needle).count()).sum();
                assert_eq!(count, 1, "{replayed}");
            }
            c2.shutdown_and_wait();
            assert!(req_rx.try_recv().is_err(), "consumed queue entry replayed after a second restart");
        }
        drop(server_thread);
    }

    run_queued_image_prompt(&env, &work, false);
    let recovery_home = tempfile::tempdir().unwrap();
    let mut rec_env = env.clone();
    rec_env.insert("MARE_HOME".to_string(), recovery_home.path().display().to_string());
    run_queued_image_prompt(&rec_env, &work, true);

    // 6. empty_and_image_only_tool_results
    {
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("faux.json");
        std::fs::write(
            &s,
            serde_json::to_string(&json!([
                { "tool_calls": [{ "name": "empty_tool", "arguments": {} }, { "name": "image_only_tool", "arguments": {} }] },
                { "text": "done" }
            ]))
            .unwrap(),
        )
        .unwrap();
        let rpc_env = BTreeMap::from([
            ("MARE_HOME".to_string(), t.path().join("home").display().to_string()),
            ("MARE_FAUX_SCRIPT".to_string(), s.display().to_string()),
        ]);
        let mut c = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &rpc_env, &work);
        let init = c.call(
            "initialize",
            json!({
                "protocol_version": "3.0",
                "client": { "name": "rpc_client.rs" },
                "capabilities": { "callbacks": true },
                "callback_timeout_ms": 10000,
            }),
        );
        let sid = init["session"]["session_id"].as_str().unwrap().to_string();
        let empty_reg = c.call(
            "tools.register",
            json!({ "name": "empty_tool", "description": "Returns an empty content array", "parameters": { "type": "object", "properties": {} }, "side_effect": "read", "callbacks": ["execute"] }),
        )["registration_id"].as_str().unwrap().to_string();
        let _img_reg = c.call(
            "tools.register",
            json!({ "name": "image_only_tool", "description": "Returns an image-only content array", "parameters": { "type": "object", "properties": {} }, "side_effect": "read", "callbacks": ["execute"] }),
        )["registration_id"].as_str().unwrap().to_string();
        let empty_seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        c.on_method = Some(Box::new(move |client, m| {
            if m.get("method").and_then(Value::as_str) != Some("callback.call")
                || m["params"].get("callback").and_then(Value::as_str) != Some("execute")
            {
                return false;
            }
            let reply = if m["params"]["registration_id"] == empty_reg
                && !empty_seen.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                json!({ "content": [] })
            } else {
                json!({ "content": [{ "type": "image", "data": png_b64, "mime": "image/png" }] })
            };
            client.send(json!({ "id": m["id"], "result": reply }));
            true
        }));
        let started = c.call("agent.prompt", json!({ "session_id": sid, "input": "go" }));
        let result = c.call("agent.wait", json!({ "session_id": sid, "turn_id": started["turn_id"] }));
        c.wait_status("idle");
        assert_eq!(result["text"], "done", "{result}");
        // PROV-I1b: a tool's image stays in its tool_result message; the empty one stays empty
        let expected_content = |name: &str| {
            if name == "image_only_tool" {
                json!([{ "type": "image", "data": png_b64, "mime": "image/png" }])
            } else {
                json!([])
            }
        };

        let tool_result_events: Vec<&Value> = c
            .events
            .iter()
            .filter(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call")
            .collect();
        let names: Vec<&str> = tool_result_events.iter().map(|e| e["data"]["item"]["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["empty_tool", "image_only_tool"]);
        for ev in tool_result_events {
            assert_eq!(ev["data"]["item"]["result_status"], "success", "{ev}");
            let name = ev["data"]["item"]["name"].as_str().unwrap();
            assert_eq!(ev["data"]["item"]["message"]["content"], expected_content(name), "{ev}");
        }
        let empty_end = c
            .events
            .iter()
            .find(|e| {
                e["type"] == "item/completed"
                    && e["data"]["item"]["type"] == "tool_call"
                    && e["data"]["item"]["name"] == "empty_tool"
            })
            .unwrap();
        assert!(empty_end["data"]["item"]["result"].get("content").is_none(), "{empty_end}");

        let image_user_msgs: Vec<&Value> = c
            .events
            .iter()
            .filter(|e| {
                e["type"] == "item/completed"
                    && e["data"]["item"]["type"] == "user_message"
                    && e["data"]["item"]["message"]["content"]
                        .as_array()
                        .is_some_and(|arr| arr.iter().any(|b| b["type"] == "image" && b["data"] == png_b64))
            })
            .collect();
        assert_eq!(image_user_msgs.len(), 0, "no separate user message carries the tool image");

        for msg_source in [&result["messages"], &c.call("agent.list_messages", json!({ "session_id": sid }))["data"]] {
            let tr: Vec<&Value> =
                msg_source.as_array().unwrap().iter().filter(|m| m["role"] == "tool_result").collect();
            let names: Vec<&str> = tr.iter().map(|m| m["tool_name"].as_str().unwrap()).collect();
            assert_eq!(names, vec!["empty_tool", "image_only_tool"]);
            for m in tr {
                assert_eq!(m["content"], expected_content(m["tool_name"].as_str().unwrap()), "{m}");
            }
        }
        c.shutdown_and_wait();
    }

    // 7. options_redaction_and_describe
    {
        let home = tempfile::tempdir().unwrap();
        let cfg_dir = home.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("init.lua"),
            r#"
mare.setup({ border = "double", editor = "sk-PY_RAW_TOKEN_1234567890abcdef" })
mare.provider("example", {
  api_key = "PY_API_KEY_SENTINEL",
  headers = { ["X-Secret"] = "PY_HDR_SENTINEL" },
  query = { token = "PY_QUERY_SENTINEL" },
  query_params = { sig = "PY_QP_SENTINEL" },
  base_url = "https://example.invalid/?secret=PY_URL_SENTINEL",
  retries = 4,
})
mare.plugin("probe", {
  private_value = "PY_PLUGIN_SECRET",
  public_value = "py-ok",
  nested = { headers = { Authorization = "Bearer PY_NESTED_HDR" }, query = { auth = "PY_NESTED_Q" } },
})
mare.config_sensitive("plugins.probe.private_value")
"#,
        )
        .unwrap();
        let opt_env = BTreeMap::from([("MARE_HOME".to_string(), home.path().display().to_string())]);
        let mut c = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo", "--no-session"], &opt_env, &work);
        let init = c.call("initialize", json!({ "protocol_version": "3.0" }));
        let opt_sid = init["session"]["session_id"].as_str().expect("session_id").to_string();
        assert!(init["capabilities"]["methods"].as_array().unwrap().iter().any(|m| m == "config.describe"), "{init}");
        let sentinels = [
            "PY_RAW_TOKEN_1234567890abcdef",
            "PY_API_KEY_SENTINEL",
            "PY_HDR_SENTINEL",
            "PY_QUERY_SENTINEL",
            "PY_QP_SENTINEL",
            "PY_URL_SENTINEL",
            "PY_PLUGIN_SECRET",
            "PY_NESTED_HDR",
            "PY_NESTED_Q",
        ];
        for req in [
            json!({ "session_id": opt_sid }),
            json!({ "session_id": opt_sid, "keys": [] }),
            json!({ "session_id": opt_sid, "keys": ["providers"] }),
        ] {
            let res = c.call("options.get", req.clone());
            let dumped = res.to_string();
            for s in sentinels {
                assert!(!dumped.contains(s), "{s} leaked in {req}: {dumped}");
            }
            assert_eq!(res["providers"]["example"]["api_key"], "[REDACTED]", "{res}");
            assert_eq!(res["providers"]["example"]["headers"], "[REDACTED]", "{res}");
            assert_eq!(res["providers"]["example"]["query"], "[REDACTED]", "{res}");
            assert_eq!(res["providers"]["example"]["query_params"], "[REDACTED]", "{res}");
            assert_eq!(res["providers"]["example"]["retries"], 4, "{res}");
            if res.get("plugins").is_some() {
                assert_eq!(res["plugins"]["probe"]["private_value"], "[REDACTED]", "{res}");
                assert_eq!(res["plugins"]["probe"]["public_value"], "py-ok", "{res}");
                assert_eq!(res["plugins"]["probe"]["nested"]["headers"], "[REDACTED]", "{res}");
                assert_eq!(res["plugins"]["probe"]["nested"]["query"], "[REDACTED]", "{res}");
            }
        }
        let dotted = c.call(
            "options.get",
            json!({ "session_id": opt_sid, "keys": ["plugins.probe.private_value", "plugins.probe.public_value"] }),
        );
        assert_eq!(dotted["plugins.probe.private_value"], "[REDACTED]", "{dotted}");
        assert_eq!(dotted["plugins.probe.public_value"], "py-ok", "{dotted}");
        let desc = c.call(
            "options.get",
            json!({ "session_id": opt_sid, "keys": ["border", "providers", "plugins.probe.private_value"], "describe": true }),
        );
        let dumped_desc = desc.to_string();
        for s in sentinels {
            assert!(!dumped_desc.contains(s), "{s} leaked in describe: {dumped_desc}");
        }
        assert_eq!(desc["border"]["effective"], "double", "{desc}");
        assert_eq!(desc["border"]["default"], "rounded", "{desc}");
        assert_eq!(desc["border"]["source"]["layer"], "user", "{desc}");
        assert_eq!(desc["border"]["redacted"], false, "{desc}");
        assert_eq!(desc["providers"]["redacted"], true, "{desc}");
        assert_eq!(desc["plugins.probe.private_value"]["effective"], "[REDACTED]", "{desc}");
        assert_eq!(desc["plugins.probe.private_value"]["sensitive"], true, "{desc}");
        assert_eq!(desc["plugins.probe.private_value"]["redacted"], true, "{desc}");
        let cfg = c.call("config.describe", json!({ "keys": ["border", "providers"] }));
        assert_eq!(cfg["options"]["border"]["effective"], "double", "{cfg}");
        assert_eq!(cfg["options"]["providers"]["redacted"], true, "{cfg}");
        c.shutdown_and_wait();
    }

    // 5b. host_tool_terminate
    {
        let t = tempfile::tempdir().unwrap();
        let s = t.path().join("faux.json");
        std::fs::write(
            &s,
            serde_json::to_string(&json!([
                { "tool_calls": [{ "name": "finish_tool", "arguments": {} }] },
                { "text": "asked again" }
            ]))
            .unwrap(),
        )
        .unwrap();
        let rpc_env = BTreeMap::from([
            ("MARE_HOME".to_string(), t.path().join("home").display().to_string()),
            ("MARE_FAUX_SCRIPT".to_string(), s.display().to_string()),
        ]);
        let mut c = RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &rpc_env, &work);
        let init = c.call(
            "initialize",
            json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 10000 }),
        );
        let sid = init["session"]["session_id"].as_str().unwrap().to_string();
        c.call(
            "tools.register",
            json!({
                "name": "finish_tool",
                "description": "Ends the run",
                "parameters": { "type": "object", "properties": {} },
                "side_effect": "read",
                "callbacks": ["execute"],
            }),
        );
        c.on_tool = Some(Box::new(|_| Some(json!({ "content": "final answer", "terminate": true }))));
        let started = c.call("agent.prompt", json!({ "session_id": sid, "input": "go" }));
        c.call("agent.wait", json!({ "session_id": sid, "turn_id": started["turn_id"] }));
        c.wait_status("idle");
        let starts: Vec<&Value> = c.events.iter().filter(|e| e["type"] == "step/started").collect();
        assert_eq!(starts.len(), 1, "{starts:?}");
        let texts: Vec<&Value> = c
            .events
            .iter()
            .filter(|e| {
                e["type"] == "item/completed"
                    && e["data"]["item"]["type"] == "assistant_message"
                    && e["data"]["item"]["message"].to_string().contains("asked again")
            })
            .collect();
        assert!(texts.is_empty(), "{texts:?}");
        c.kill_and_wait();
    }
}
