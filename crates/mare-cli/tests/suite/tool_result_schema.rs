//! Shared positive and negative tests for `item/completed` (`item.type == "tool_call"`) across
//! Rust validation, `schema.json`, Lua kernel payloads, headless `--json`, and `--rpc`.

use std::process::Stdio;

use crate::common::*;
use serde_json::{Value, json};

pub(crate) fn assert_valid_tool_result_payload(catalog: &Value, item: &Value) {
    let payload = json!({
        "turn_id": "t-1",
        "item_id": item["tool_call_id"],
        "status": item["status"],
        "item": item,
    });
    mare_protocol::events::validate_protocol("item/completed", &payload)
        .unwrap_or_else(|e| panic!("validate_protocol rejected {payload}: {e}"));
    let parsed: mare_protocol::events::ToolResult =
        serde_json::from_value(item.clone()).unwrap_or_else(|e| panic!("ToolResult::deserialize rejected {item}: {e}"));
    assert_eq!(parsed.tool_call_id, parsed.message.tool_call_id, "{item}");
    assert!(!parsed.tool_call_id.is_empty(), "{item}");
    assert!(!parsed.name.is_empty(), "{item}");
    assert!(item.get("call").is_none(), "redundant item.call must be absent: {item}");

    let envelope = json!({ "type": "item/completed", "data": payload });
    for pointer in ["#/notifications/event", "#/headless/line"] {
        mare_protocol::validate_schema_at(catalog, pointer, &envelope)
            .unwrap_or_else(|e| panic!("schema {pointer} rejected {envelope}: {e}"));
    }
}

#[test]
fn headless_json_tool_result_satisfies_typed_schema_and_id_correspondence() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("a.txt"), "alpha").unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
            { "tool_calls": [{ "name": "read", "arguments": { "path": "missing.txt" } }] },
            { "text": "done" }
        ])
        .to_string(),
    )
    .unwrap();

    let out = mare_cmd()
        .args(["--json", "--model", "faux/echo", "--no-session", "run"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let catalog = mare_protocol::schema();
    let results: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call")
        .map(|e| e["data"]["item"].clone())
        .collect();
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["result_status"], "success");
    assert_eq!(results[1]["result_status"], "error");
    for item in &results {
        assert_valid_tool_result_payload(&catalog, item);
    }
}

#[test]
fn rpc_tool_result_satisfies_typed_schema_and_id_correspondence() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("a.txt"), "alpha").unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
            { "tool_calls": [{ "name": "read", "arguments": { "path": "missing.txt" } }] },
            { "text": "done" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    c.call_raw("events.subscribe", json!({"include": ["item/completed"]}), |_| json!(null));

    let started = c.call_raw("agent.prompt", json!({"session_id": sid, "input": "go"}), |_| json!(null));
    assert!(started["error"].is_null(), "{started}");
    let wait_id = c.request("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}));
    let catalog = mare_protocol::schema();
    let mut results = Vec::new();
    loop {
        let line = c.read();
        if line["method"] == "event"
            && line["params"]["type"] == "item/completed"
            && line["params"]["data"]["item"]["type"] == "tool_call"
        {
            mare_protocol::validate_schema_at(&catalog, "#/notifications/event", &line["params"])
                .unwrap_or_else(|e| panic!("RPC event failed schema: {e}: {line}"));
            assert_valid_tool_result_payload(&catalog, &line["params"]["data"]["item"]);
            results.push(line["params"]["data"]["item"].clone());
        } else if line.get("id").and_then(Value::as_u64) == Some(wait_id) {
            assert!(line.get("result").is_some(), "{line}");
            break;
        }
    }
    assert_eq!(results.len(), 2, "{results:?}");
    assert_eq!(results[0]["result_status"], "success");
    assert_eq!(results[1]["result_status"], "error");
    c.shutdown_and_wait();
}

#[test]
fn runtime_rejects_invalid_tool_result_payloads_in_headless_and_rpc() {
    let cases = [
        (
            "empty_message",
            r#"{item_id="c1", status="completed", item={type="tool_call", tool_call_id="c1", name="read", status="completed", message={}, result_status="success", execution_status="unknown"}}"#,
            json!({"item_id": "c1", "status": "completed", "item": {"type": "tool_call", "tool_call_id": "c1", "name": "read", "status": "completed", "message": {}, "result_status": "success", "execution_status": "unknown"}}),
            true,
        ),
        (
            "missing_tool_call_id",
            r#"{item_id="c1", status="completed", item={type="tool_call", name="read", status="completed", message={role="tool_result", tool_call_id="c1", content=mare.json.array()}, result_status="success", execution_status="unknown"}}"#,
            json!({"item_id": "c1", "status": "completed", "item": {"type": "tool_call", "name": "read", "status": "completed", "message": {"role": "tool_result", "tool_call_id": "c1", "content": []}, "result_status": "success", "execution_status": "unknown"}}),
            true,
        ),
        (
            "non_array_content",
            r#"{item_id="c1", status="completed", item={type="tool_call", tool_call_id="c1", name="read", status="completed", message={role="tool_result", tool_call_id="c1", content={}}, result_status="success", execution_status="unknown"}}"#,
            json!({"item_id": "c1", "status": "completed", "item": {"type": "tool_call", "tool_call_id": "c1", "name": "read", "status": "completed", "message": {"role": "tool_result", "tool_call_id": "c1", "content": {}}, "result_status": "success", "execution_status": "unknown"}}),
            true,
        ),
        (
            "mismatched_tool_call_id",
            r#"{item_id="c1", status="completed", item={type="tool_call", tool_call_id="c1", name="read", status="completed", message={role="tool_result", tool_call_id="c2", content=mare.json.array()}, result_status="success", execution_status="unknown"}}"#,
            json!({"item_id": "c1", "status": "completed", "item": {"type": "tool_call", "tool_call_id": "c1", "name": "read", "status": "completed", "message": {"role": "tool_result", "tool_call_id": "c2", "content": []}, "result_status": "success", "execution_status": "unknown"}}),
            false,
        ),
    ];

    let catalog = mare_protocol::schema();
    for (label, lua_payload, json_payload, schema_rejects) in cases {
        assert!(
            mare_protocol::events::validate_protocol("item/completed", &json_payload).is_err(),
            "{label} must fail validate_protocol"
        );
        if schema_rejects {
            assert!(
                mare_protocol::validate_schema_at(
                    &catalog,
                    "#/notifications/event",
                    &json!({"type": "item/completed", "data": json_payload}),
                )
                .is_err(),
                "{label} must fail JSON Schema validation"
            );
        }

        let home = tempfile::tempdir().unwrap();
        let plugins = home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("bad_tool_result.lua"),
            format!(
                r#"return {{name="bad_tool_result", inject={{"bus"}}, apply=function(ctx)
                ctx:on("request/prepare", function() ctx.bus:emit("item/completed", {lua_payload}) end)
            end}}"#
            ),
        )
        .unwrap();
        let script = home.path().join("script.json");
        std::fs::write(&script, json!([{ "text": "finished" }]).to_string()).unwrap();

        // Headless --json rejects the invalid payload with turn.failed and non-zero exit code.
        let out = mare_cmd()
            .args(["--json", "--model", "faux/echo", "--no-session", "go"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert!(!out.status.success(), "{label}: headless unexpectedly succeeded");
        let lines: Vec<Value> =
            String::from_utf8(out.stdout).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert!(lines.iter().any(|line| line["type"] == "turn.failed"), "{label}: {lines:?}");
        assert_ne!(lines.last().unwrap()["data"]["exit_code"], 0, "{label}: {lines:?}");

        // RPC --rpc fails the stream cleanly with stream.failed.
        let mut cmd = mare_cmd();
        cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(home.path())
            .stderr(Stdio::null());
        let mut c = RpcClient::spawn_cmd(&mut cmd);
        let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
        let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
        c.call_raw("events.subscribe", json!({"include": ["item/completed"]}), |_| json!(null));
        let prompt_id = c.request("agent.prompt", json!({"session_id": sid, "input": "go"}));
        let mut wait_id: Option<u64> = None;
        loop {
            let line = c.read();
            if line["method"] == "stream.failed" {
                break;
            }
            if line.get("id").and_then(Value::as_u64) == Some(prompt_id) {
                assert!(line["error"].is_null(), "{label}: agent.prompt error: {line}");
                let turn_id = line["result"]["turn_id"].clone();
                wait_id = Some(c.request("agent.wait", json!({"session_id": sid, "turn_id": turn_id})));
            } else if wait_id.is_some() && line.get("id").and_then(Value::as_u64) == wait_id {
                panic!("{label}: invalid item/completed tool_call did not fail RPC stream: {line}");
            }
        }
        c.kill_and_wait();
    }
}
