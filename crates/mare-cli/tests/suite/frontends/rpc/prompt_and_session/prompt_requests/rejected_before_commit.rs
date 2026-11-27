use super::*;

/// Codes `agent.prompt` / `agent.steer` only return before the input is committed: nothing is
/// queued, no turn starts, no message is recorded and the `request_id` is not remembered.
const REJECTED_BEFORE_COMMIT: &[&str] = &[
    "parse_error",
    "invalid_request",
    "method_not_found",
    "overloaded",
    "denied",
    "invalid_arguments",
    "unknown_session",
    "unsupported",
    "request_id_conflict",
    "busy",
    "agent.rejected",
    "agent.no_active_turn",
    "agent.turn_mismatch",
];

/// Codes that can also be returned after the input was committed.
const INDETERMINATE: &[&str] = &[
    "internal",
    "cancelled",
    "stale_session",
    "unavailable",
    "invalid_result",
    "persist_failed",
    "timed_out",
    "disconnected",
];

fn session_snapshot(c: &mut RpcClient, sid: &str) -> (Value, Value, Value) {
    let messages = c.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
    let queue = c.call_raw("agent.list_queue", json!({ "session_id": sid }), |_| json!(null));
    let state = c.call_raw("agent.get", json!({ "session_id": sid }), |_| json!(null));
    assert!(messages["error"].is_null() && queue["error"].is_null() && state["error"].is_null());
    (messages["result"]["data"].clone(), queue["result"]["data"].clone(), state["result"]["busy"].clone())
}

/// Idle-session refusals leave the session as it was, and resending with the same `request_id`
/// runs the prompt exactly once instead of replaying a refusal or a phantom turn.
#[test]
fn prompt_refused_before_commit_leaves_session_unchanged_and_resend_runs_once() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "first-scripted-answer" }, { "text": "second" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let before = session_snapshot(&mut c, &sid);
    assert_eq!(before.0, json!([]), "fresh session has no messages");

    let refusals = [
        (
            "unknown_session",
            "agent.prompt",
            json!({ "session_id": "no-such-session", "input": "hello", "request_id": "resend-1" }),
        ),
        (
            "invalid_arguments",
            "agent.prompt",
            json!({ "session_id": sid, "input": "hello", "request_id": "resend-1", "when_busy": "later" }),
        ),
        (
            "method_not_found",
            "agent.prompt_now",
            json!({ "session_id": sid, "input": "hello", "request_id": "resend-1" }),
        ),
    ];
    for (code, method, params) in refusals {
        let reply = c.call_raw(method, params, |_| json!(null));
        assert_eq!(reply["error"]["data"]["code"], code, "{reply}");
        assert!(reply["result"].is_null(), "{reply}");
        assert_eq!(session_snapshot(&mut c, &sid), before, "{code} must not change the session");
    }

    let sent =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hello", "request_id": "resend-1" }), |_| {
            json!(null)
        });
    assert_eq!(sent["result"]["status"], "started", "{sent}");
    assert_ne!(sent["result"]["duplicate"], true, "an earlier refusal must not be remembered: {sent}");
    let waited =
        c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": sent["result"]["turn_id"] }), |_| json!(null));
    assert!(waited["result"].to_string().contains("first-scripted-answer"), "nothing ran before: {waited}");

    // A conflicting reuse is refused and leaves the committed prompt and its receipt untouched.
    let after_turn = session_snapshot(&mut c, &sid);
    let conflict =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "changed", "request_id": "resend-1" }), |_| {
            json!(null)
        });
    assert_eq!(conflict["error"]["data"]["code"], "request_id_conflict", "{conflict}");
    assert_eq!(session_snapshot(&mut c, &sid), after_turn);
    let replay =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hello", "request_id": "resend-1" }), |_| {
            json!(null)
        });
    assert_eq!(replay["result"]["duplicate"], true, "{replay}");
    assert_eq!(replay["result"]["turn_id"], sent["result"]["turn_id"], "{replay}");
    assert_eq!(session_snapshot(&mut c, &sid), after_turn, "a replay runs nothing");
    c.shutdown_and_wait();
}

/// While a turn runs, `busy` (prompt) and `agent.turn_mismatch` (steer) leave the queue empty and
/// do not remember the `request_id`, so the corrected resend is accepted once.
#[test]
fn busy_and_turn_mismatch_refusals_leave_queue_unchanged_and_resend_queues_once() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script = root.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "turn-one" }, { "text": "turn-two" }]).to_string()).unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, &work);
    let held: std::sync::Arc<std::sync::Mutex<Option<Value>>> = std::sync::Arc::new(std::sync::Mutex::new(None));
    let slot = std::sync::Arc::clone(&held);
    client.on_method = Some(Box::new(move |_c, msg| {
        if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "request/ready" {
            let mut slot = slot.lock().unwrap();
            if slot.is_none() {
                *slot = Some(msg["id"].clone());
                return true;
            }
        }
        false
    }));
    let init = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let sid = init["session"]["session_id"].as_str().expect("session_id").to_string();
    client.call("hooks.subscribe", json!({ "scope": "all", "event": "request/ready", "callbacks": ["handle"] }));
    client.call("events.subscribe", json!({ "scope": "all", "include": ["turn/*", "queue/changed"] }));

    let started = client.call("agent.prompt", json!({ "session_id": sid, "input": "hold the turn" }));
    let turn_id = started["turn_id"].as_str().expect("turn_id").to_string();
    while held.lock().unwrap().is_none() {
        let msg = client.read();
        client.dispatch(msg);
    }
    let messages_before = client.call("agent.list_messages", json!({ "session_id": sid }))["data"].clone();

    let busy = client.call_err(
        "agent.prompt",
        json!({ "session_id": sid, "input": "next", "when_busy": "reject", "request_id": "busy-resend" }),
    );
    assert_eq!(busy["data"]["code"], "busy", "{busy}");
    let mismatch = client.call_err(
        "agent.steer",
        json!({ "session_id": sid, "input": "steer", "expected_turn_id": "turn-other", "request_id": "steer-resend" }),
    );
    assert_eq!(mismatch["data"]["code"], "agent.turn_mismatch", "{mismatch}");
    assert_eq!(client.call("agent.list_queue", json!({ "session_id": sid }))["data"], json!([]));
    assert_eq!(client.call("agent.list_messages", json!({ "session_id": sid }))["data"], messages_before);

    let queued = client.call(
        "agent.prompt",
        json!({ "session_id": sid, "input": "next", "when_busy": "queue", "request_id": "busy-resend" }),
    );
    assert_eq!(queued["status"], "queued", "{queued}");
    assert_ne!(queued.get("duplicate").and_then(Value::as_bool), Some(true), "{queued}");
    let steered = client.call(
        "agent.steer",
        json!({ "session_id": sid, "input": "steer", "expected_turn_id": turn_id, "request_id": "steer-resend" }),
    );
    assert_ne!(steered.get("duplicate").and_then(Value::as_bool), Some(true), "{steered}");
    let queue = client.call("agent.list_queue", json!({ "session_id": sid }));
    assert_eq!(queue["data"].as_array().map(Vec::len), Some(2), "each resend queued exactly once: {queue}");

    let held_id = held.lock().unwrap().take().unwrap();
    client.call("agent.cancel", json!({ "session_id": sid, "turn_id": turn_id, "clear_queue": true }));
    client.send(json!({ "id": held_id, "result": { "decision": "deny", "reason": "test done" } }));
    client.wait_event("turn/completed");
    client.shutdown_and_wait();
}

/// The RPC guides in every language sort the same codes into the same two groups as above.
#[test]
fn rpc_guides_document_which_prompt_errors_are_safe_to_resend() {
    let docs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    for lang in ["en", "ja", "zh"] {
        let path = docs.join(lang).join("integrate/rpc.md");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let codes_on = |needle: &str| -> std::collections::BTreeSet<String> {
            let line = text
                .lines()
                .find(|l| l.starts_with("- ") && l.contains(needle))
                .unwrap_or_else(|| panic!("{lang}: no list item containing {needle}"));
            // The codes follow the item's last colon; earlier prose may name fields such as `request_id`.
            let list = line.rsplit([':', '：']).next().unwrap_or_default();
            list.split('`')
                .skip(1)
                .step_by(2)
                .filter(|code| {
                    !code.is_empty() && code.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch == '.')
                })
                .map(str::to_string)
                .collect()
        };
        let before = codes_on("`parse_error`");
        let want: std::collections::BTreeSet<String> = REJECTED_BEFORE_COMMIT.iter().map(|s| s.to_string()).collect();
        assert_eq!(before, want, "{lang}: codes documented as rejected before commit");
        let indeterminate = codes_on("`invalid_result`");
        for code in INDETERMINATE {
            assert!(indeterminate.contains(*code), "{lang}: `{code}` must be documented as indeterminate");
        }
    }
}
