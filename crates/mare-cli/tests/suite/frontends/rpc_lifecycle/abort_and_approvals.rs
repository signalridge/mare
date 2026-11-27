use super::*;

#[tokio::test]
async fn conditional_turn_abort_ignores_an_old_id() {
    use std::time::Duration;
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "first turn" },
                { "text": "x".repeat(4000), "delay_ms": 20 }
            ]))
            .no_session()
            .build()
            .await;
        h.prompt("old", json!({"turn_id":"older"})).await.unwrap();
        let turn = {
            let h = h.clone();
            tokio::task::spawn_local(async move { h.prompt("go", json!({"turn_id":"current"})).await.unwrap() })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !h.is_busy().await {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(h.state().await["turn_id"], "current");
        h.abort_turn("older").await.unwrap();
        assert!(h.is_busy().await, "a stale ID must not cancel the current run");
        h.abort_turn("current").await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), turn).await.unwrap().unwrap();
        assert_eq!(result["error"]["code"], "aborted", "{result}");
        h.shutdown().await;
    })
    .await;
}

#[test]
fn rpc_refuses_turn_start_if_the_agent_cannot_cancel_by_id() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(cfg.join("init.lua"), "mare.setup { plugins = { agent = { enabled = false } } }\n").unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let prompt_denied = c.call_raw("agent.prompt", json!({"session_id": "none", "input": "go"}), |_| json!(null));
    assert_eq!(prompt_denied["error"]["data"]["code"], "method_not_found", "{prompt_denied}");
    let cont = c.call_raw("commands.run", json!({"line":"/continue"}), |_| json!(null));
    assert_eq!(
        cont["error"]["data"]["code"], "commands.unknown_command",
        "commands.run must refuse /continue without agent: {cont}"
    );
    c.call_raw("shutdown", json!({}), |_| json!(null));
    c.wait_exit();
}

#[test]
fn rpc_output_failure_cancels_a_waiting_approval() {
    use std::time::{Duration, Instant};
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([
        {"tool_calls":[{"name":"bash", "arguments":{"command":"touch should-not-run", "sandbox_permissions":"all", "justification":"test"}}]},
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
    let mut c = RpcClient::new(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let init = c
        .call_raw("initialize", json!({"protocol_version": "3.0", "capabilities":{"approvals":true}}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.request("agent.prompt", json!({"session_id":sid,"input":"go"}));
    loop {
        if c.read()["method"] == "approval.request" {
            break;
        }
    }
    c.close_reader(); // close stdout's reader, then force an observable write failure
    c.send(json!({"id":99, "method":"status.get", "params":{}}));
    let end = Instant::now() + Duration::from_secs(5);
    let completed = loop {
        if child.0.try_wait().unwrap().is_some() {
            break true;
        }
        if Instant::now() >= end {
            drop(child);
            break false;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    drop(c);
    assert!(completed, "the approval kept waiting after an observed output failure");
    assert!(!home.path().join("should-not-run").exists());
}

#[test]
fn rpc_shutdown_cancels_the_outstanding_approval() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([
        {"tool_calls":[{"name":"bash", "arguments":{"command":"touch should-not-run", "sandbox_permissions":"all", "justification":"test"}}]}
    ]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--permission", "read-only", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c
        .call_raw("initialize", json!({"protocol_version": "3.0", "capabilities":{"approvals":true}}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.request("agent.prompt", json!({"session_id":sid,"input":"go"}));
    let ask = loop {
        let m = c.read();
        if m["method"] == "approval.request" {
            break m;
        }
    };
    let shutdown = c.request("shutdown", json!({}));
    let mut cancelled = false;
    let mut answered = false;
    while !(cancelled && answered) {
        let m = c.read();
        if m["method"] == "$/cancel_request" {
            assert_eq!(m["params"]["id"], ask["id"]);
            cancelled = true;
        }
        if m["id"] == shutdown {
            answered = true;
        }
    }
    c.wait_exit();
    assert!(!home.path().join("should-not-run").exists());
}

fn rpc_disconnect_keeps_acknowledged_images_for_restart(stdin_eof: bool) {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildGroup::spawn(&mut cmd).unwrap();
    let mut rpc = RpcClient::new(BufReader::new(child.0.stdout.take().unwrap()), child.0.stdin.take().unwrap());
    let init2 = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid = init2["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    rpc.call_raw("hooks.subscribe", json!({"scope":"all","event":"request/ready","callbacks":["handle"]}), |_| {
        json!(null)
    });
    rpc.call_holding("agent.prompt", json!({"session_id":sid,"input":"held"}));
    loop {
        if rpc.read()["method"] == "callback.call" {
            break;
        }
    }
    let accepted = rpc.call_raw(
        "agent.prompt",
        json!({"session_id":sid,"input":[{"type":"text","text":"queued disconnect bytes"},{"type":"image","mime":"image/png","data":"AQIDBA=="}]}),
        |_| json!(null),
    );
    assert_eq!(accepted["result"]["status"], "queued", "{accepted}");
    let sessions = rpc.call_raw("session.list", json!({}), |_| json!(null));
    let path = sessions["result"]["data"].as_array().unwrap()[0]["path"].as_str().unwrap().to_owned();
    if stdin_eof {
        rpc.close_stdin();
        // Keep the output reader alive: only stdin EOF may trigger this disconnect.
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if child.0.try_wait().unwrap().is_some() {
                break;
            }
            if std::time::Instant::now() >= end {
                drop(child);
                panic!("stdin EOF did not cancel the primary connection");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(rpc);
    } else {
        rpc.close_reader(); // output is gone while the provider is still held at an explicit policy gate
        rpc.send(json!({"id":99,"method":"agent.get","params":{"session_id":sid}}));
        let _ = child.wait_timeout(std::time::Duration::from_secs(10));
        drop(rpc);
    }
    let mut cmd2 = mare_cmd();
    cmd2.args(["--rpc", "--model", "faux/echo", "--resume", &path])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd2);
    let init2 = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid2 = init2["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let state = rpc.call_raw("agent.get", json!({"session_id":sid2}), |_| json!(null));
    assert_eq!(state["result"]["session_id"], sid2, "{state}");
    let delivered_start =
        rpc.call_raw("agent.prompt", json!({"session_id":sid2,"input":"deliver pending"}), |_| json!(null));
    assert!(delivered_start["error"].is_null(), "{delivered_start}");
    let delivered =
        rpc.call_raw("agent.wait", json!({"session_id":sid2,"turn_id":delivered_start["result"]["turn_id"]}), |_| {
            json!(null)
        });
    assert!(delivered["error"].is_null(), "{delivered}");
    let sid = state["result"]["session_id"].as_str().unwrap().to_string();
    let messages = rpc.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
    let body = messages.to_string();
    assert!(body.contains("queued disconnect bytes") && body.contains("AQIDBA=="), "{messages}");
    let state = rpc.call_raw("agent.get", json!({"session_id":sid2}), |_| json!(null));
    assert_eq!(state["result"]["queued"], 0, "{state}");
    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_disconnect_cancels_work_but_keeps_acknowledged_images_for_restart() {
    rpc_disconnect_keeps_acknowledged_images_for_restart(false);
}

#[test]
fn rpc_stdin_eof_keeps_acknowledged_images_for_restart() {
    rpc_disconnect_keeps_acknowledged_images_for_restart(true);
}

/// A held policy callback gives the RPC turn a deterministic lifetime. A real journal
/// identity failure must never produce queued:true, and successful abort carries the bytes.
#[test]
fn rpc_queue_persistence_failure_and_abort_reply_preserve_complete_inputs() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    rpc.call_raw("hooks.subscribe", json!({"scope":"all","event":"request/ready","callbacks":["handle"]}), |_| {
        json!(null)
    });
    // holding: a callback.call that arrives before the prompt's reply is kept for the loop below, not
    // auto-answered (the same test as the old unregistered rpc_events.rs copy)
    let started = rpc.call_holding("agent.prompt", json!({"session_id":sid,"input":"held"}));
    assert!(started["error"].is_null(), "{started}");
    let held = loop {
        let frame = rpc.read();
        if frame["method"] == "callback.call" {
            break frame;
        }
    };
    let accepted = rpc.call_raw(
        "agent.prompt",
        json!({"session_id":sid,"input":[{"type":"text","text":"return these bytes"},{"type":"image","mime":"image/png","data":"AQIDBA=="}]}),
        |_| json!(null),
    );
    assert_eq!(accepted["result"]["status"], "queued", "{accepted}");
    let sessions = rpc.call_raw("session.list", json!({}), |_| json!(null));
    let path = std::path::PathBuf::from(sessions["result"]["data"].as_array().unwrap()[0]["path"].as_str().unwrap());
    let backup = path.with_extension("saved");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::write(&path, b"replaced journal identity").unwrap();
    let rejected = rpc.call_raw("agent.prompt", json!({"session_id":sid,"input":"must not confirm"}), |_| json!(null));
    assert!(rejected["error"].is_object(), "real writer failure must be an RPC error: {rejected}");
    assert!(rejected["result"].is_null(), "{rejected}");
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&backup, &path).unwrap();
    let cancelled = rpc.call_raw("agent.cancel", json!({"session_id":sid}), |_| json!(null));
    assert!(cancelled["result"]["turn_id"].is_string(), "{cancelled}");
    assert_eq!(cancelled["result"]["removed"][0]["text"], "return these bytes", "{cancelled}");
    assert_eq!(cancelled["result"]["removed"][0]["images"][0]["data"], "AQIDBA==", "{cancelled}");
    rpc.send(json!({"jsonrpc":"2.0","id":held["id"],"result":{"value":{"decision":"allow"}}}));
    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}
