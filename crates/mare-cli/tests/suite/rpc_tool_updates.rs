//! End-to-end RPC tests for host tool incremental progress (`tool.update` -> `item/delta` `tool_output`).

use crate::common::*;
use serde_json::{Value, json};

type Rpc = RpcClient;

struct RpcProc {
    rpc: Rpc,
    _home: tempfile::TempDir,
}

fn spawn_rpc(script: Value) -> RpcProc {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, script.to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(&work)
        .stderr(std::process::Stdio::null());
    let rpc = RpcClient::spawn_cmd(&mut cmd);
    RpcProc { rpc, _home: home }
}

fn read_until_method(rpc: &mut Rpc, method: &str, events: &mut Vec<Value>) -> Value {
    loop {
        let msg = rpc.read();
        if msg["method"] == "event" {
            events.push(msg["params"].clone());
        }
        if msg["method"] == method {
            return msg;
        }
    }
}

fn read_until_event(rpc: &mut Rpc, event_type: &str, events: &mut Vec<Value>) -> Value {
    loop {
        let msg = rpc.read();
        if msg["method"] == "event" {
            let params = msg["params"].clone();
            events.push(params.clone());
            if params["type"] == event_type {
                return params;
            }
        }
    }
}

#[test]
fn host_tool_update_streams_live_and_rejects_late_unknown_or_invalid_updates() {
    let mut p = spawn_rpc(json!([
        { "tool_calls": [{ "name": "stream_tool", "arguments": { "q": "1" } }] },
        { "text": "done" }
    ]));
    let init = p.rpc.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 10_000 }),
        |_| json!(null),
    );
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();
    let reg = p.rpc.call_raw(
        "tools.register",
        json!({
            "name": "stream_tool",
            "description": "Streams incremental output",
            "parameters": { "type": "object", "properties": { "q": { "type": "string" } } },
            "callbacks": ["execute"]
        }),
        |_| json!(null),
    );
    assert!(reg.get("error").is_none(), "{reg}");

    let prompt_reply = p.rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "go" }), |_| json!(null));
    assert_eq!(prompt_reply["result"]["status"], "started", "{prompt_reply}");

    let mut events = Vec::new();
    let call_msg = read_until_method(&mut p.rpc, "callback.call", &mut events);
    assert_eq!(call_msg["params"]["callback"], "execute", "{call_msg}");
    let cb_req_id = call_msg["id"].clone();

    // 1. Live in-flight update arrives as `item/delta` (`kind = "tool_output"`) before the tool finishes.
    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "chunk 1", "step": 1 } }));
    let first_update = loop {
        let msg = p.rpc.read();
        if msg["method"] == "event" {
            let params = msg["params"].clone();
            events.push(params.clone());
            if params["type"] == "item/delta" && params["data"]["kind"] == "tool_output" {
                break params;
            }
        }
    };
    assert_eq!(first_update["data"]["partial"]["output"], "chunk 1", "{first_update}");
    assert_eq!(first_update["data"]["partial"]["step"], 1, "{first_update}");
    assert!(
        !events.iter().any(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call"),
        "{events:?}"
    );

    // 2. Unknown request id and malformed payloads are dropped without emitting events.
    p.rpc.notify("callback.update", json!({ "id": "unknown-call-id", "partial": { "output": "ghost" } }));
    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": "not-an-object" }));
    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": 123 } }));

    // 3. Second valid update followed immediately by `callback.call` completion and a pipelined late update.
    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "chunk 2", "step": 2 } }));
    p.rpc.send(json!({ "jsonrpc": "2.0", "id": cb_req_id, "result": { "content": "finished" } }));
    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "late-after-result" } }));

    let _end = read_until_event(&mut p.rpc, "turn/completed", &mut events);

    // 4. Late update after full completion is also dropped.
    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "late-after-end" } }));
    let _ = p.rpc.call_raw("shutdown", json!({}), |_| json!(null));
    // Drain remaining events until stdout closes.
    for msg in p.rpc.drain_and_wait() {
        if msg["method"] == "event" {
            events.push(msg["params"].clone());
        }
    }

    let tool_updates: Vec<_> =
        events.iter().filter(|e| e["type"] == "item/delta" && e["data"]["kind"] == "tool_output").collect();
    let outputs: Vec<_> = tool_updates.iter().map(|e| e["data"]["partial"]["output"].as_str().unwrap()).collect();
    assert_eq!(outputs, vec!["chunk 1", "chunk 2"], "{tool_updates:?}");

    let last_update_pos =
        events.iter().rposition(|e| e["type"] == "item/delta" && e["data"]["kind"] == "tool_output").unwrap();
    let tool_end_pos =
        events.iter().position(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call").unwrap();
    assert!(last_update_pos < tool_end_pos, "tool_output delta must precede tool_call item/completed: {events:?}");
}

#[test]
fn host_tool_update_rejects_updates_after_cancellation_and_capability_withdrawal() {
    let mut p = spawn_rpc(json!([
        { "tool_calls": [{ "name": "slow_tool", "arguments": {} }] },
        { "tool_calls": [{ "name": "slow_tool", "arguments": {} }] },
        { "text": "after revoke" }
    ]));
    let init = p.rpc.call_raw(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 10_000 }),
        |_| json!(null),
    );
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();
    p.rpc.call_raw(
        "tools.register",
        json!({
            "name": "slow_tool",
            "description": "Waits until cancelled or answered",
            "parameters": { "type": "object", "properties": {} },
            "callbacks": ["execute"]
        }),
        |_| json!(null),
    );

    // Part A: cancellation via `agent.cancel` drops all subsequent `callback.update` notifications.
    p.rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "first" }), |_| json!(null));
    let mut events = Vec::new();
    let call_msg = read_until_method(&mut p.rpc, "callback.call", &mut events);
    assert_eq!(call_msg["params"]["callback"], "execute", "{call_msg}");
    let cb_req_id = call_msg["id"].clone();

    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "before-abort" } }));
    let live = loop {
        let msg = p.rpc.read();
        if msg["method"] == "event" {
            let params = msg["params"].clone();
            events.push(params.clone());
            if params["type"] == "item/delta" && params["data"]["kind"] == "tool_output" {
                break params;
            }
        }
    };
    assert_eq!(live["data"]["partial"]["output"], "before-abort", "{live}");

    let abort_id = p.rpc.request("agent.cancel", json!({ "session_id": sid }));
    let mut saw_end = false;
    loop {
        let msg = p.rpc.read();
        if msg["method"] == "event" {
            if msg["params"]["type"] == "turn/completed" {
                saw_end = true;
            }
            events.push(msg["params"].clone());
        } else if msg["id"] == abort_id {
            assert!(msg.get("error").is_none(), "{msg}");
            break;
        }
    }
    p.rpc.notify("callback.update", json!({ "id": cb_req_id, "partial": { "output": "after-abort" } }));
    if !saw_end {
        read_until_event(&mut p.rpc, "turn/completed", &mut events);
    }

    // Part B: withdrawing `capabilities.callbacks` via connection.set_capabilities drops updates for in-flight calls.
    p.rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "second" }), |_| json!(null));
    let call2 = read_until_method(&mut p.rpc, "callback.call", &mut events);
    assert_eq!(call2["params"]["callback"], "execute", "{call2}");
    let cb2_req_id = call2["id"].clone();
    let reinit_id = p.rpc.request(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": false, "ui": false, "approvals": false, "elicitation": false } }),
    );
    loop {
        let msg = p.rpc.read();
        if msg["method"] == "event" {
            events.push(msg["params"].clone());
        } else if msg["id"] == reinit_id {
            assert!(msg.get("error").is_none(), "{msg}");
            break;
        }
    }
    p.rpc.notify("callback.update", json!({ "id": cb2_req_id, "partial": { "output": "after-revoke" } }));
    p.rpc.send(json!({ "jsonrpc": "2.0", "id": call2["id"], "result": { "content": "ok" } }));
    read_until_event(&mut p.rpc, "turn/completed", &mut events);

    let _ = p.rpc.call_raw("shutdown", json!({}), |_| json!(null));
    for msg in p.rpc.drain_and_wait() {
        if msg["method"] == "event" {
            events.push(msg["params"].clone());
        }
    }

    let outputs: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "item/delta" && e["data"]["kind"] == "tool_output")
        .map(|e| e["data"]["partial"]["output"].as_str().unwrap())
        .collect();
    assert_eq!(outputs, vec!["before-abort"], "{events:?}");
}
