use super::*;

/// RPC `prompt` with a `requestId` is idempotent per session: a repeat returns the
/// original turn, the same id with different content is refused.
#[test]
fn prompt_request_ids_are_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "one" }, { "text": "two" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let first =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "r1" }), |_| json!(null));
    let turn = first["result"]["turn_id"].clone();
    assert!(turn.is_string(), "{first}");
    let wait1 = c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": turn }), |_| json!(null));
    assert!(wait1["error"].is_null(), "{wait1}");
    let again =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "r1" }), |_| json!(null));
    assert_eq!(again["result"]["turn_id"], turn, "{again}");
    assert_eq!(again["result"]["duplicate"], true, "{again}");
    let conflict =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "something else", "request_id": "r1" }), |_| {
            json!(null)
        });
    assert_eq!(conflict["error"]["data"]["code"], "request_id_conflict", "{conflict}");
    // a new id starts a new turn
    let other =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "r2" }), |_| json!(null));
    assert_ne!(other["result"]["turn_id"], turn, "{other}");
    let _ =
        c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": other["result"]["turn_id"] }), |_| json!(null));
    c.shutdown_and_wait();
}

/// Switching sessions (`A -> B -> A`) must retain session A's `request_id` receipt so retrying the
/// same `request_id` after switching back is still idempotent and does not re-run the turn.
#[test]
fn prompt_request_id_survives_session_switch_a_b_a() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "turn-a" }, { "text": "turn-b" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    let info_a = c.call_raw("session.create", json!({}), |_| json!(null));
    let sid_a = info_a["result"]["session_id"].as_str().unwrap().to_string();

    let first_a =
        c.call_raw("agent.prompt", json!({ "session_id": sid_a, "input": "hello A", "request_id": "r-aba" }), |_| {
            json!(null)
        });
    let turn_a = first_a["result"]["turn_id"].clone();
    assert!(turn_a.is_string(), "{first_a}");
    assert_ne!(first_a["result"]["duplicate"], true, "{first_a}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": sid_a, "turn_id": turn_a }), |_| json!(null));

    // Switch to session B and use the same request_id (scoped per session, so B runs a new turn).
    let info_b = c.call_raw("session.create", json!({}), |_| json!(null));
    let sid_b = info_b["result"]["session_id"].as_str().unwrap().to_string();
    assert_ne!(sid_b, sid_a);

    let first_b =
        c.call_raw("agent.prompt", json!({ "session_id": sid_b, "input": "hello A", "request_id": "r-aba" }), |_| {
            json!(null)
        });
    let turn_b = first_b["result"]["turn_id"].clone();
    assert!(turn_b.is_string(), "{first_b}");
    assert_ne!(turn_b, turn_a, "{first_b}");
    assert_ne!(first_b["result"]["duplicate"], true, "{first_b}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": sid_b, "turn_id": turn_b }), |_| json!(null));

    // Switch back to session A: retrying r-aba must return turn_a as a duplicate without running a 3rd turn.
    let resumed_a = c.call_raw("session.load", json!({ "session_id": sid_a }), |_| json!(null));
    assert_eq!(resumed_a["result"]["session_id"], sid_a, "{resumed_a}");

    let replay_a =
        c.call_raw("agent.prompt", json!({ "session_id": sid_a, "input": "hello A", "request_id": "r-aba" }), |_| {
            json!(null)
        });
    assert_eq!(replay_a["result"]["turn_id"], turn_a, "{replay_a}");
    assert_eq!(replay_a["result"]["duplicate"], true, "{replay_a}");

    let conflict_a =
        c.call_raw("agent.prompt", json!({ "session_id": sid_a, "input": "changed A", "request_id": "r-aba" }), |_| {
            json!(null)
        });
    assert_eq!(conflict_a["error"]["data"]["code"], "request_id_conflict", "{conflict_a}");

    c.shutdown_and_wait();
}

/// A `request_id` too large to keep in the idempotency cache is refused before the prompt runs, so
/// a request is never executed without being retry-safe; a retry gets the same refusal and still
/// runs nothing.
#[test]
fn oversized_prompt_request_id_is_rejected_before_execution() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "first-scripted-answer" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let huge = "r".repeat(64 * 1024);
    for attempt in 0..2 {
        let refused =
            c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": huge }), |_| {
                json!(null)
            });
        assert_eq!(refused["error"]["code"], -32602, "attempt {attempt}: {refused}");
        assert!(refused["result"].is_null(), "attempt {attempt}: {refused}");
        let msg = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("request_id") || msg.contains("1024"), "attempt {attempt}: {refused}");
    }
    // nothing ran: the first scripted answer is still unconsumed and no turn was recorded
    let ok =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "r-ok" }), |_| json!(null));
    assert!(ok["result"]["turn_id"].is_string(), "{ok}");
    let wait_ok =
        c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": ok["result"]["turn_id"] }), |_| json!(null));
    assert!(wait_ok["result"].to_string().contains("first-scripted-answer"), "{wait_ok}");
    c.shutdown_and_wait();
}

/// An empty `request_id` is refused before anything runs (the schema says `minLength: 1`); a
/// repeat is refused the same way, so it can never run a prompt twice unkeyed.
#[test]
fn empty_prompt_request_id_is_rejected_before_execution() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "first-scripted-answer" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    for attempt in 0..2 {
        let refused =
            c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "" }), |_| json!(null));
        assert_eq!(refused["error"]["code"], -32602, "attempt {attempt}: {refused}");
        assert!(refused["result"].is_null(), "attempt {attempt}: {refused}");
        let msg = refused["error"]["message"].as_str().unwrap_or_default();
        assert!(msg.contains("request_id") || msg.contains("shorter than 1"), "attempt {attempt}: {refused}");
    }
    // nothing ran: the first scripted answer is still unconsumed
    let ok =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "r-ok" }), |_| json!(null));
    assert!(ok["result"]["turn_id"].is_string(), "{ok}");
    let wait_ok =
        c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": ok["result"]["turn_id"] }), |_| json!(null));
    assert!(wait_ok["result"].to_string().contains("first-scripted-answer"), "{wait_ok}");
    c.shutdown_and_wait();
}

/// A `session.delete` that fails leaves the session's `request_id` receipts in place: a retry of
/// an already-answered prompt still returns the cached turn instead of running again.
#[test]
fn failed_session_delete_keeps_prompt_request_receipts() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "one" }, { "text": "two" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let info = c.call_raw("session.create", json!({}), |_| json!(null));
    let sid = info["result"]["session_id"].as_str().unwrap_or_else(|| panic!("{info}")).to_string();
    let first =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "r-del" }), |_| json!(null));
    let turn = first["result"]["turn_id"].clone();
    assert!(turn.is_string(), "{first}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": turn }), |_| json!(null));

    // nothing is saved without a session file, so delete fails without deleting receipts
    let del_target = c.call_raw("session.delete", json!({ "session_id": sid }), |_| json!(null));
    assert!(del_target.get("error").is_some(), "{del_target}");

    let again =
        c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi", "request_id": "r-del" }), |_| json!(null));
    assert_eq!(again["result"]["turn_id"], turn, "{again}");
    assert_eq!(again["result"]["duplicate"], true, "{again}");
    c.shutdown_and_wait();
}

/// Deleting an unindexed session by path resolves header `id` and emits `session/deleted`.
#[test]
fn session_delete_by_path_resolves_header_id_when_unindexed() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("deleted-event.txt");
    let plugin_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("del_probe.lua"),
        format!(
            r#"return {{ name = "del-probe", inject = {{ "bus" }}, apply = function(ctx)
                ctx:on("session/deleted", function(ev)
                    mare.fs.write({:?}, tostring(ev.session_id or ""))
                end)
            end }}"#,
            marker.to_string_lossy()
        ),
    )
    .unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "one" }, { "text": "two" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let s1 = c.call_raw("session.create", json!({}), |_| json!(null));
    let s1_id = s1["result"]["session_id"].as_str().unwrap().to_string();
    let s1_start =
        c.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "hi", "request_id": "r-s1" }), |_| {
            json!(null)
        });
    assert!(s1_start.get("error").is_none(), "{s1_start}");
    assert!(
        c.call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": s1_start["result"]["turn_id"] }), |_| json!(
            null
        ))
        .get("error")
        .is_none()
    );
    let listed = c.call_raw("session.list", json!({}), |_| json!(null));
    assert!(!listed["result"]["data"].as_array().unwrap().is_empty(), "{listed}");

    let s2 = c.call_raw("session.create", json!({}), |_| json!(null));
    assert_ne!(s2["result"]["session_id"].as_str().unwrap(), s1_id);

    // Write a valid session file outside the sessions root after indexing so `resolve_any(path)`
    // misses `index_read` and must fall back to the unindexed file-header resolution path.
    let unindexed_path = home.path().join("unindexed-session.jsonl");
    std::fs::write(
        &unindexed_path,
        format!(
            "{}\n",
            json!({
                "type": "session",
                "version": mare_store::CURRENT_VERSION,
                "session_id": "unindexed-s1",
                "cwd": home.path(),
                "created_at_ms": 1
            })
        ),
    )
    .unwrap();

    let del = c.call_raw("session.delete", json!({ "session_id": unindexed_path.to_string_lossy() }), |_| json!(null));
    assert_eq!(del["result"], json!({}), "{del}");
    assert!(!unindexed_path.exists(), "unindexed session file must be removed");
    c.shutdown_and_wait();
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap_or_default(),
        "unindexed-s1",
        "session.delete by unindexed path must emit session/deleted with header id"
    );
}
