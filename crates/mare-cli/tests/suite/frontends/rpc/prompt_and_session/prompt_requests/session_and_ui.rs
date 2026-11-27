use super::*;

/// A blocked startup (`-c` with nothing to continue) refuses an RPC `prompt` with an error instead
/// of holding it or running it in a silent new session; `session.new` lifts
/// the block.
#[test]
fn rpc_prompt_fails_while_startup_is_blocked() {
    let home = tempfile::tempdir().unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "answered" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "-c"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let refused = c.call_raw(
        "agent.prompt",
        json!({ "session_id": init["result"]["session"]["session_id"].as_str().unwrap_or("blocked"), "input": "hi" }),
        |_| json!(null),
    );
    let msg = refused["error"]["message"].as_str().unwrap_or_default().to_string();
    assert!(msg.starts_with("startup blocked:") && msg.contains("session.create"), "{refused}");
    let created = c.call_raw("session.create", json!({}), |_| json!(null));
    let sid = created["result"]["session_id"].as_str().expect("session_id").to_string();
    let ok = c.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hi" }), |_| json!(null));
    assert!(ok["result"]["turn_id"].is_string(), "{ok}");
    let _ = c.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": ok["result"]["turn_id"] }), |_| json!(null));
    c.shutdown_and_wait();
}

/// The RPC session methods are a contract with the `session` *service*, not the built-in plugin
/// (reference implementation: consumers depend on Service Definitions, never concrete providers,
/// `packages/README.md:99`): a user plugin replacing `session` can implement the required contract
/// and omit optional methods such as `set_default_sandbox_mode` and `rewind_conversation`.
#[test]
fn rpc_session_methods_work_against_a_replaced_session_service() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("session.lua"),
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/session_replacement.lua")),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    assert!(init["result"]["protocol_version"].is_string(), "{init}");
    let new = c.call_raw("session.create", json!({}), |_| json!(null));
    assert_eq!(new["result"]["session_id"], "stub-2", "{new}");
    let list = c.call_raw("session.list", json!({}), |_| json!(null));
    let ids: Vec<&str> =
        list["result"]["data"].as_array().into_iter().flatten().filter_map(|s| s["session_id"].as_str()).collect();
    assert_eq!(ids, ["stub-1", "stub-2"], "{list}");
    let resumed = c.call_raw("session.load", json!({ "session_id": "stub-1" }), |_| json!(null));
    assert_eq!(resumed["result"]["session_id"], "stub-1", "{resumed}");
    let forked = c.call_raw("session.fork", json!({ "session_id": "stub-1" }), |_| json!(null));
    assert_eq!(forked["result"]["session_id"], "fork-3", "{forked}");
    let entries = c.call_raw("session.list_items", json!({ "session_id": "fork-3" }), |_| json!(null));
    assert_eq!(entries["result"]["leaf_entry_id"], "e1", "{entries}");
    assert_eq!(entries["result"]["data"][0]["session_id"], "fork-3", "{entries}");
    c.shutdown_and_wait();
}

#[test]
fn rpc_select_forwards_string_and_list_bodies() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("select_body.lua"),
        r#"return { name = "select-body", inject = { "commands" }, apply = function(ctx)
      ctx.commands:register("select-body", { run = function()
        mare.ui.select({title = "first", body = "line one\nline two", items = {"ok"}})
        mare.ui.select({title = "second", body = {"third", "fourth"}, items = {"ok"}})
      end })
    end }"#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
    let mut bodies = Vec::new();
    let r = c.call_raw("commands.run", json!({"line": "/select-body"}), |m| {
        if m["method"] == "ui.select" {
            bodies.push(m["params"]["body"].clone());
            json!({"index": 0})
        } else {
            json!(null)
        }
    });
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(bodies, vec![json!(["line one", "line two"]), json!(["third", "fourth"])]);
    c.shutdown_and_wait();
}

#[test]
fn rpc_compact_run_replaces_compact_and_returns_structured_status() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let methods = init["result"]["capabilities"]["methods"].as_array().unwrap();
    assert!(methods.iter().any(|v| v == "compact.run"), "missing compact.run in methods");
    assert!(!methods.iter().any(|v| v == "compact"), "legacy compact method must be gone");

    let legacy = c.call_raw("compact", json!({}), |_| json!(null));
    assert!(legacy.get("error").is_some(), "legacy compact must fail: {legacy}");

    let missing_sid = c.call_raw("compact.run", json!({}), |_| json!(null));
    assert_eq!(missing_sid["error"]["data"]["code"], "invalid_arguments", "{missing_sid}");

    let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    let res = c.call_raw("compact.run", json!({ "session_id": session_id }), |_| json!(null));
    assert!(res.get("error").is_none(), "{res}");
    assert_eq!(res["result"]["status"], json!("nothing_to_compact"), "{res}");
    assert!(res["result"]["tokens_before"].is_i64(), "{res}");
    assert_eq!(res["result"]["tokens_after"], res["result"]["tokens_before"], "{res}");

    let empty_sum =
        c.call_raw("compact.run", json!({ "session_id": session_id, "summarize_only": true, "messages": [] }), |_| {
            json!(null)
        });
    assert_eq!(empty_sum["error"]["data"]["code"], "compact.not_compacted", "{empty_sum}");

    let unknown_sid = c.call_raw("compact.run", json!({ "session_id": "nonexistent-session" }), |_| json!(null));
    assert_eq!(unknown_sid["error"]["data"]["code"], "unknown_session", "{unknown_sid}");

    for internal_reason in ["auto", "overflow", "model_switch"] {
        let rejected =
            c.call_raw("compact.run", json!({ "session_id": session_id, "reason": internal_reason }), |_| json!(null));
        assert_eq!(
            rejected["error"]["data"]["code"], "invalid_arguments",
            "internal reason `{internal_reason}` must be rejected on external RPC: {rejected}"
        );
    }

    let manual_res =
        c.call_raw("compact.run", json!({ "session_id": session_id, "reason": "manual" }), |_| json!(null));
    assert!(manual_res.get("error").is_none(), "explicit manual reason should succeed: {manual_res}");

    c.shutdown_and_wait();
}
