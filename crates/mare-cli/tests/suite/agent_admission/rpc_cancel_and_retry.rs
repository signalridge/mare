use super::*;

/// Regression (`admit-r3` #1): while a `wait=true` prompt is blocked in `session/commit-ready` on a
/// busy agent, a concurrent retry with the same `requestId` (`wait=false`) must wait for actual
/// admission and must NOT receive a fake `started: true` receipt if policy denies admission.
#[test]
fn rpc_concurrent_retry_during_commit_ready_waits_for_admission_and_reflects_policy_denial() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Stdio;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());

    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("commit_gate.lua"),
        format!(
            r#"
            return {{
                name = "commit-gate-test",
                inject = {{ "policy" }},
                apply = function(ctx)
                    local gated = false
                    ctx.policy:voter("session/commit-ready", function(ev)
                        if not gated and ev and ev.inbox and ev.inbox.follow_up then
                            for _, item in ipairs(ev.inbox.follow_up) do
                                if item.text == "follow" then
                                    gated = true
                                    mare.http.request({{ url = "{url}", timeout_ms = 10000 }})
                                    return {{ decision = "deny", reason = "denied by policy" }}
                                end
                            end
                        end
                        return {{ decision = "allow" }}
                    end)
                end,
            }}
            "#
        ),
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "slow turn ".repeat(300), "delay_ms": 20 }]).to_string()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    rpc.call_raw("events.subscribe", json!({ "include": ["item/delta", "turn/completed"] }), |_| json!(null));
    let s = rpc.call_raw("session.create", json!({}), |_| json!(null));
    let sid = s["result"]["session_id"].as_str().unwrap().to_string();

    // Start an active turn so subsequent prompts enter the busy -> follow_up -> session/commit-ready path.
    let active = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "active" }), |_| json!(null));
    assert_eq!(active["result"]["status"], "started", "{active}");
    loop {
        let ev = rpc.read();
        if ev.get("method").and_then(|m| m.as_str()) == Some("event")
            && ev["params"]["type"].as_str() == Some("item/delta")
        {
            break;
        }
    }

    // Request A (request_id="req-policy") enters session/commit-ready and blocks on the HTTP gate.
    let req_a =
        rpc.request("agent.prompt", json!({ "session_id": sid, "input": "follow", "request_id": "req-policy" }));
    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });
    let (mut stream, _) = accept_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap().unwrap();

    // Concurrent retry B (same request_id and content) arrives while A is still blocked.
    let req_b =
        rpc.request("agent.prompt", json!({ "session_id": sid, "input": "follow", "request_id": "req-policy" }));

    // Ordering barrier instead of a sleep: the server dispatches requests in order on one thread
    // and B reaches A's pending entry without I/O, so once a later request has answered, a wrong
    // early reply for B (especially `started`) would already have been written.
    let ping = rpc.request("options.get", json!({ "session_id": sid }));
    loop {
        let m = rpc.read();
        assert_ne!(m.get("id").and_then(|v| v.as_u64()), Some(req_b), "B replied before A was decided: {m}");
        if m.get("id").and_then(|v| v.as_u64()) == Some(ping) {
            break;
        }
    }
    // Deny A at the policy gate.
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndeny");
    let _ = stream.flush();
    drop(stream);

    let (mut resp_a, mut resp_b) = (None, None);
    while resp_a.is_none() || resp_b.is_none() {
        let m = rpc.read();
        if m.get("id").and_then(|v| v.as_u64()) == Some(req_a) {
            resp_a = Some(m);
        } else if m.get("id").and_then(|v| v.as_u64()) == Some(req_b) {
            resp_b = Some(m);
        }
    }
    let (resp_a, resp_b) = (resp_a.unwrap(), resp_b.unwrap());
    assert!(
        resp_b.get("error").is_some(),
        "concurrent retry B must not receive fake started=true when A is denied: {resp_b}"
    );
    assert!(resp_a.get("error").is_some(), "request A must fail when policy denies: {resp_a}");

    rpc.call_raw("agent.cancel", json!({ "session_id": sid }), |_| json!(null));
    rpc.kill_and_wait();
}

/// Regression (`admit-r3` #2): `agent.prompt` cancelled via `$/cancel_request` during `queue/changed`
/// (after durable inbox commit) must still record the idempotent receipt so retrying with the same
/// `request_id` returns `status: "queued", duplicate: true` instead of enqueuing a second time.
#[test]
fn rpc_wait_false_cancel_during_agent_queue_preserves_idempotent_receipt() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Stdio;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());

    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("queue_gate.lua"),
        format!(
            r#"
            return {{
                name = "queue-gate-test",
                apply = function(ctx)
                    ctx:on("queue/changed", function()
                        mare.http.request({{ url = "{url}", timeout_ms = 10000 }})
                    end)
                end,
            }}
            "#
        ),
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            { "text": "slow turn ".repeat(300), "delay_ms": 20 },
            { "text": "queued reply" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    rpc.call_raw("events.subscribe", json!({ "include": ["item/delta", "turn/completed"] }), |_| json!(null));
    let s = rpc.call_raw("session.create", json!({}), |_| json!(null));
    let sid = s["result"]["session_id"].as_str().unwrap().to_string();

    let active = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "active" }), |_| json!(null));
    assert_eq!(active["result"]["status"], "started", "{active}");
    loop {
        let ev = rpc.read();
        if ev.get("method").and_then(|m| m.as_str()) == Some("event")
            && ev["params"]["type"].as_str() == Some("item/delta")
        {
            break;
        }
    }

    // Request A commits to the inbox and pauses in queue/changed.
    let req_a = rpc.request(
        "agent.prompt",
        json!({ "session_id": sid, "input": "follow once", "request_id": "req-queue-cancel" }),
    );
    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });
    let (mut stream, _) = accept_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap().unwrap();

    // Cancel A while it is in queue/changed (after inbox commit), then release the barrier.
    rpc.notify("$/cancel_request", json!({ "id": req_a }));
    let barrier = rpc.call_raw("peer.info", json!({}), |_| json!(null));
    assert!(barrier.get("error").is_none(), "{barrier}");
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let _ = stream.flush();
    drop(stream);
    let resp_a = rpc.wait_raw(req_a, |_| json!(null));
    assert_eq!(resp_a["result"]["status"], "queued", "{resp_a}");
    assert!(resp_a["result"]["input_id"].is_string(), "{resp_a}");

    // Retrying with the same request_id must return status="queued", duplicate=true (and not enqueue again).
    let retry = rpc.call_raw(
        "agent.prompt",
        json!({ "session_id": sid, "input": "follow once", "request_id": "req-queue-cancel" }),
        |_| json!(null),
    );
    assert_eq!(retry["result"]["status"], "queued", "retry must return status=queued: {retry}");
    assert_eq!(retry["result"]["duplicate"], true, "retry must be marked duplicate=true: {retry}");

    rpc.call_raw("agent.cancel", json!({ "session_id": sid }), |_| json!(null));
    rpc.kill_and_wait();
}

/// Regression (`admit-r4` #3): cancelling an `agent.wait` RPC request while paused in the first
/// `turn/started` listener must not strand `A.busy = true` permanently.
#[test]
fn rpc_cancel_wait_true_before_agent_start_does_not_leak_busy() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Stdio;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());

    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("hook_gate.lua"),
        format!(
            r#"
            return {{
                name = "hook-gate-start-test",
                apply = function(ctx)
                    local fired = false
                    -- the pause point is this plugin's own turn/started listener, ordered before every other one
                    ctx:on("turn/started", function()
                        if not fired then
                            fired = true
                            mare.http.request({{ url = "{url}", timeout_ms = 10000 }})
                        end
                    end, {{ prepend = true }})
                end,
            }}
            "#
        ),
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            { "text": "first reply" },
            { "text": "second reply" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    rpc.call_raw("events.subscribe", json!({ "include": ["turn/completed"] }), |_| json!(null));
    let s = rpc.call_raw("session.create", json!({}), |_| json!(null));
    let sid = s["result"]["session_id"].as_str().unwrap().to_string();

    // Send agent.prompt; claim_prompt sets A.busy = true, then the test plugin's first turn/started listener pauses.
    // Then wait on the turn via agent.wait and cancel the waiter via $/cancel_request.
    let started = rpc.call_raw(
        "agent.prompt",
        json!({ "session_id": sid, "input": "first", "request_id": "req-wait-true-cancel" }),
        |_| json!(null),
    );
    let turn_id = started["result"]["turn_id"].as_str().unwrap().to_string();
    let req_w = rpc.request("agent.wait", json!({ "session_id": sid, "turn_id": turn_id }));
    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });
    let (mut stream, _) = accept_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap().unwrap();

    // Cancel agent.wait while the turn is paused in the first turn/started listener (before the others run).
    rpc.notify("$/cancel_request", json!({ "id": req_w }));
    // The cancel is handled while the turn is still parked: wait's reply arrives before the release.
    let resp_w = rpc.wait_raw(req_w, |_| json!(null));
    assert_eq!(resp_w["error"]["message"], "service call was cancelled", "{resp_w}");
    assert_eq!(
        (resp_w["error"]["code"].as_i64(), resp_w["error"]["data"]["code"].as_str()),
        (Some(-32800), Some("cancelled")),
        "{resp_w}"
    );

    // Release the barrier.
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let _ = stream.flush();
    drop(stream);

    // The committed turn runs to its end; then a new prompt must start (not stay stuck busy).
    loop {
        let ev = rpc.read();
        if ev.get("method").and_then(|m| m.as_str()) == Some("event")
            && ev["params"]["type"].as_str() == Some("turn/completed")
        {
            break;
        }
    }

    let next = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "second" }), |_| json!(null));
    assert_eq!(next["result"]["status"], "started", "subsequent prompt must start cleanly: {next}");

    rpc.call_raw("agent.cancel", json!({ "session_id": sid }), |_| json!(null));
    rpc.kill_and_wait();
}

/// Regression (`admit-r4` #2): `agent.prompt` cancelled via `$/cancel_request` while paused in the first `queue/changed`
/// listener (after durable inbox commit, before other listeners run) must still record the idempotent receipt.
#[test]
fn rpc_wait_false_cancel_in_hook_start_before_agent_queue_preserves_receipt() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::Stdio;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());

    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("hook_gate_queue.lua"),
        format!(
            r#"
            return {{
                name = "hook-gate-queue-test",
                apply = function(ctx)
                    local fired = false
                    -- the pause point is this plugin's own queue/changed listener, ordered before every other one
                    ctx:on("queue/changed", function()
                        if not fired then
                            fired = true
                            mare.http.request({{ url = "{url}", timeout_ms = 10000 }})
                        end
                    end, {{ prepend = true }})
                end,
            }}
            "#
        ),
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            { "text": "slow turn ".repeat(300), "delay_ms": 20 },
            { "text": "queued reply" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    rpc.call_raw("events.subscribe", json!({ "include": ["item/delta", "turn/completed"] }), |_| json!(null));
    let s = rpc.call_raw("session.create", json!({}), |_| json!(null));
    let sid = s["result"]["session_id"].as_str().unwrap().to_string();

    let active = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "active" }), |_| json!(null));
    assert_eq!(active["result"]["status"], "started", "{active}");
    loop {
        let ev = rpc.read();
        if ev.get("method").and_then(|m| m.as_str()) == Some("event")
            && ev["params"]["type"].as_str() == Some("item/delta")
        {
            break;
        }
    }

    // Request A commits to the inbox and pauses in the first queue/changed listener.
    let req_a = rpc.request(
        "agent.prompt",
        json!({ "session_id": sid, "input": "follow once", "request_id": "req-hook-start-cancel" }),
    );
    let (accept_tx, accept_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = accept_tx.send(listener.accept());
    });
    let (mut stream, _) = accept_rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap().unwrap();

    // Cancel A while it is paused there (after inbox commit, before the other queue/changed listeners), then release the barrier.
    rpc.notify("$/cancel_request", json!({ "id": req_a }));
    let barrier = rpc.call_raw("peer.info", json!({}), |_| json!(null));
    assert!(barrier.get("error").is_none(), "{barrier}");
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    let _ = stream.flush();
    drop(stream);
    let resp_a = rpc.wait_raw(req_a, |_| json!(null));
    assert_eq!(resp_a["result"]["status"], "queued", "{resp_a}");
    assert!(resp_a["result"]["input_id"].is_string(), "{resp_a}");

    // Retrying with the same request_id must return status="queued", duplicate=true (and not enqueue again).
    let retry = rpc.call_raw(
        "agent.prompt",
        json!({ "session_id": sid, "input": "follow once", "request_id": "req-hook-start-cancel" }),
        |_| json!(null),
    );
    assert_eq!(retry["result"]["status"], "queued", "retry must return status=queued: {retry}");
    assert_eq!(retry["result"]["duplicate"], true, "retry must be marked duplicate=true: {retry}");

    rpc.call_raw("agent.cancel", json!({ "session_id": sid }), |_| json!(null));
    rpc.kill_and_wait();
}
