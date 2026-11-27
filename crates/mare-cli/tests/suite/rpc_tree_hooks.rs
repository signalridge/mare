//! Remote hooks on the cordis tree:
//! a remote participant attaches via `hooks.subscribe` with D11 `callback.call` (`handle`) on
//! events whose contract marks them `expose = "both"` or policy decision/rewrite points; an
//! unlisted or `expose = "process"` event is `invalid_arguments`, and release goes via `registrations.release`.

use crate::common::*;
use serde_json::json;
use std::process::Stdio;

#[test]
fn remote_hooks_attach_to_asynchronous_events_only() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init =
        rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }), |_| {
            json!(null)
        });
    assert!(init.get("error").is_none(), "{init}");
    let subscribe = |rpc: &mut RpcClient, event: &str| {
        rpc.call_raw("hooks.subscribe", json!({ "scope": "all", "event": event, "callbacks": ["handle"] }), |_| {
            json!(null)
        })
    };
    let missing =
        rpc.call_raw("hooks.subscribe", json!({ "scope": "all", "event": "status/changed" }), |_| json!(null));
    assert_eq!(missing["error"]["code"], -32602, "{missing}");
    assert!(missing.to_string().contains("callback"), "{missing}");
    let status = subscribe(&mut rpc, "status/changed");
    assert!(status["result"]["registration_id"].is_string(), "{status}");
    // an event the protocol does not list for remote participants
    let unlisted = subscribe(&mut rpc, "custom/anything");
    assert_eq!(unlisted["error"]["code"], -32602, "{unlisted}");
    // a listed event in its mode: a registration on this connection's fiber, released via registrations.release
    let ok = subscribe(&mut rpc, "queue/changed");
    let id = ok["result"]["registration_id"].as_str().unwrap_or_else(|| panic!("{ok}")).to_owned();
    let gone = rpc.call_raw("registrations.release", json!({ "registration_id": id }), |_| json!(null));
    assert_eq!(gone["result"], json!({}), "{gone}");
    rpc.shutdown_and_wait();
}

// The same distinction between delegation and a result is tested by reference implementation remote-event protocol.
#[test]
fn request_build_requires_an_explicit_waterfall_reply() {
    for (answer, valid) in [(json!({"next": true}), true), (json!({}), false)] {
        let home = tempfile::tempdir().unwrap();
        let mut cmd = mare_cmd();
        cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stderr(Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);
        let init =
            rpc.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities":{"callbacks":true}}), |_| {
                json!(null)
            });
        assert!(init["error"].is_null(), "{init}");
        let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
        let sub = rpc.call_raw(
            "hooks.subscribe",
            json!({"session_id": sid, "event":"request/build","callbacks":["handle"]}),
            |_| json!(null),
        );
        assert!(sub["error"].is_null(), "{sub}");
        let mut called = false;
        let mut on_hook = |call: &serde_json::Value| {
            assert_eq!(call["method"], "callback.call", "{call}");
            assert_eq!(call["params"]["callback"], "handle", "{call}");
            assert_eq!(call["params"]["arguments"]["event"], "request/build", "{call}");
            called = true;
            answer.clone()
        };
        let started = rpc.call_raw("agent.prompt", json!({"session_id": sid, "input":"hello"}), &mut on_hook);
        let prompt = if started["error"].is_null() {
            rpc.call_raw("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}), on_hook)
        } else {
            started
        };
        assert!(called, "the request/build hook did not run: {prompt}");
        if valid {
            assert!(
                prompt["error"].is_null() && prompt["result"].is_object() && prompt["result"]["error"].is_null(),
                "{prompt}"
            );
        } else {
            assert!(prompt.to_string().contains("invalid_result"), "{prompt}");
        }
        rpc.shutdown_and_wait();
    }
}
