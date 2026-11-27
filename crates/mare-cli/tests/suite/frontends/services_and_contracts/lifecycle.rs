use super::*;

/// A follower of the remote service `b4-svc` whose cleanup takes a while, and a probe that says
/// how often it started and finished cleaning up.
const B4_DEPENDENT: &str = r#"return {name="b4-dep",provide={"b4-probe"},contracts={["b4-probe"]={methods={state={doc="state",
    expose="both",params={type="object",properties={},additionalProperties=false},order={},result={type="object"}}}}},
apply=function(ctx)
    local state = {started=0, cleaned=0}
    ctx:inject({"b4-svc"}, function(sub)
        state.started = state.started + 1
        sub:effect(function() return function() mare.task.sleep(300); state.cleaned = state.cleaned + 1 end end, "slow")
    end)
    require("mare_lib").service(ctx, "b4-probe", {state=function() return {started=state.started, cleaned=state.cleaned} end})
end}
"#;

/// `mare --rpc` with [`B4_DEPENDENT`] installed, initialized with `capabilities.callbacks`, answering
/// every reverse `callback.call` with "pong".
fn client() -> (crate::common::rpc::RpcClient, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(home.join("config/plugins/b4-dep.lua"), B4_DEPENDENT).unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--yolo", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    client.on_method = Some(Box::new(|c, msg| {
        if msg["method"] == "callback.call" {
            c.send(json!({ "id": msg["id"], "result": "pong" }));
            return true;
        }
        false
    }));
    client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    (client, root)
}

fn service(extra: Value) -> Value {
    let mut echo = json!({
        "parameters": { "type": "object", "properties": {}, "additionalProperties": false },
        "result": { "type": "string" },
    });
    for (k, v) in extra.as_object().unwrap() {
        echo[k] = v.clone();
    }
    json!({ "name": "b4-svc", "methods": { "echo": echo } })
}

/// The probe's state once its follower has started `started` times.
fn probe(client: &mut crate::common::rpc::RpcClient, started: u64) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let state = client.call("b4-probe.state", json!({}));
        if state["started"] == started || std::time::Instant::now() > deadline {
            return state;
        }
        // sleep-ok: poll interval for the probe state over RPC (bounded by the 10 s deadline)
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// `registrations.release` answers `{}` once the withdrawal is done (the dependents'
/// asynchronous cleanup included), and is idempotent when called again.
#[test]
fn rpc_service_unregister_awaits_the_cleanup_and_answers_whether_it_existed() {
    let (mut client, _root) = client();
    let registered = client.call("services.register", service(json!({})));
    let state = probe(&mut client, 1);
    assert_eq!(state, json!({ "started": 1, "cleaned": 0 }), "the follower started on the remote service");
    let removed = client.call("registrations.release", json!({ "registration_id": registered["registration_id"] }));
    assert_eq!(removed, json!({}));
    let state = client.call("b4-probe.state", json!({}));
    assert_eq!(state["cleaned"], 1, "registrations.release answered before the cleanup finished: {state}");
    let again = client.call("registrations.release", json!({ "registration_id": registered["registration_id"] }));
    assert_eq!(again, json!({}));
    client.shutdown_and_wait();
}

/// An invalid duplicate registration leaves the existing service and its dependents running.
#[test]
fn rpc_service_invalid_replacement_keeps_the_old_service() {
    let (mut client, _root) = client();
    client.call("services.register", service(json!({})));
    assert_eq!(probe(&mut client, 1)["started"], 1);
    let bad = service(json!({ "expose": "nowhere" }));
    let err = client.call_err("services.register", bad);
    assert!(
        err["data"]["code"] == "invalid_arguments"
            && err["message"].as_str().is_some_and(|m| m.contains("nowhere") || m.contains("expose")),
        "{err}"
    );
    let answer = client.call("b4-svc.echo", json!({}));
    assert_eq!(answer, json!("pong"), "the old service no longer answers");
    let state = client.call("b4-probe.state", json!({}));
    assert_eq!(state, json!({ "started": 1, "cleaned": 0 }), "the old provision was withdrawn: {state}");
    client.shutdown_and_wait();
}

#[test]
fn rpc_service_can_publish_an_inject_method() {
    let (mut client, _root) = client();
    let mut service = service(json!({}));
    service["methods"]["inject"] = json!({
        "parameters": { "type": "object", "properties": { "message": { "type": "string" } },
            "required": ["message"], "additionalProperties": false },
        "order": ["message"],
        "result": { "type": "string" }
    });
    client.call("services.register", service);
    assert_eq!(probe(&mut client, 1)["started"], 1);
    let answer = client.call("b4-svc.inject", json!({ "message": "hello" }));
    assert_eq!(answer, json!("pong"));
    client.shutdown_and_wait();
}

fn reserved_method_replacement_keeps_the_old_service(method: &str) {
    let (mut client, _root) = client();
    client.call("services.register", service(json!({})));
    assert_eq!(probe(&mut client, 1)["started"], 1);
    let mut bad = service(json!({}));
    bad["methods"][method] = bad["methods"]["echo"].clone();
    let err = client.call_err("services.register", bad);
    assert!(err.to_string().contains(method), "{err}");
    let answer = client.call("b4-svc.echo", json!({}));
    assert_eq!(answer, json!("pong"), "the old service no longer answers");
    let state = client.call("b4-probe.state", json!({}));
    assert_eq!(state, json!({ "started": 1, "cleaned": 0 }), "the old provision was withdrawn: {state}");
    client.shutdown_and_wait();
}

#[test]
fn rpc_service_replacement_refuses_reserved_methods() {
    for method in ["name", "ctx"] {
        reserved_method_replacement_keeps_the_old_service(method);
    }
}

/// A replacement of a declared-contract service (the built-in `usage`
/// contract, its built-in provider turned off) that leaves out a required method is refused by the
/// kernel's admission, dry-run before anything is withdrawn: the old remote provider keeps answering
/// `usage.get`.
#[test]
fn rpc_declared_contract_replacement_missing_a_required_method_keeps_the_old_service() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(home.join("config/init.lua"), r#"mare.plugin("usage", { enabled = false })"#).unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, root.path());
    let totals = json!({ "input": 0, "output": 0, "cache_read": 0, "cache_write": 0, "cache_write_1h": 0,
        "cost": 0.0, "cost_unknown": false, "requests": 0, "subscription_requests": 0, "side_requests": 0,
        "last_context": 0, "last_cache_read": 0 });
    let answer = json!({ "totals": totals, "context_tokens": 7, "rate_limits": {} });
    client.on_method = Some(Box::new(move |c, msg| {
        if msg["method"] == "callback.call" {
            c.send(json!({ "id": msg["id"], "result": answer.clone() }));
            return true;
        }
        false
    }));
    client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let methods = ["get", "totals", "context_tokens", "set_context", "cost_text", "rate_limits", "quota_lines"];
    let full: serde_json::Map<String, Value> = methods.iter().map(|&m| (m.to_string(), json!({}))).collect();
    client.call("services.register", json!({ "name": "usage", "methods": full }));
    let session = client.call("session.create", json!({}))["session_id"].as_str().unwrap().to_string();
    let get = |client: &mut crate::common::rpc::RpcClient| client.call("usage.get", json!({ "session_id": session }));
    assert_eq!(get(&mut client)["context_tokens"], 7, "the remote usage provider answers");

    let mut short = full.clone();
    short.remove("totals");
    let err = client.call_err("services.register", json!({ "name": "usage", "methods": short }));
    assert!(err.to_string().contains("totals"), "refused for the missing required method: {err}");
    assert_eq!(get(&mut client)["context_tokens"], 7, "the old usage provider was withdrawn before the refusal");
    client.shutdown_and_wait();
}

#[test]
fn rpc_rate_limits_are_pushed_with_usage_changed() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    let sse_body = crate::common::sse_events(&[
        json!({ "type": "response.created", "response": { "id": "resp-rl", "status": "in_progress" } }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "id": "msg_1",
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "ok" }]
            }
        }),
        json!({
            "type": "response.completed",
            "response": { "id": "resp-rl", "status": "completed", "usage": { "input_tokens": 5, "output_tokens": 2 } }
        }),
    ]);
    let resp = crate::common::Resp::sse(sse_body)
        .header("x-codex-primary-used-percent", "42")
        .header("x-codex-primary-window-minutes", "300")
        .header("x-codex-primary-reset-at", "1700000000");

    let (base_tx, base_rx) = std::sync::mpsc::channel::<String>();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let server = crate::common::MockServer::start().await;
            server.route("/v1/responses", vec![resp]);
            let _ = base_tx.send(server.base.clone());
            let _ = stop_rx.await;
            drop(server);
        });
    });
    let base = base_rx.recv().unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        format!(
            "mare.setup {{\n  model = \"openai/gpt-6.1-terra\",\n  plugins = {{\n    [\"provider-openai\"] = {{ base_url = \"{base}/v1\", transport = \"http\" }},\n  }},\n}}\n"
        ),
    )
    .unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("OPENAI_API_KEY".to_string(), "test-openai".to_string()),
    ]);
    let mut client = crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo"], &env, root.path());
    let init = client.call("initialize", json!({"protocol_version": "3.0"}));
    let session = init["session"]["session_id"].as_str().expect("session_id").to_string();
    client.call("events.subscribe", json!({ "scope": "all", "include": ["usage/changed"] }));
    let p = client.call("agent.prompt", json!({ "session_id": session, "input": "rate limit headers" }));
    let pr = client.call("agent.wait", json!({ "session_id": session, "turn_id": p["turn_id"] }));
    assert_eq!(pr["status"], "completed", "prompt failed: {pr}");
    let ev = loop {
        let candidate = client.wait_event("usage/changed");
        if candidate["data"]["rate_limits"]["openai"].is_object() {
            break candidate;
        }
    };
    let pushed = &ev["data"]["rate_limits"]["openai"];
    assert_eq!(pushed["primary"]["used_percent"], 42, "{ev}");
    assert_eq!(pushed["primary"]["window_minutes"], 300, "{ev}");
    assert_eq!(pushed["primary"]["reset_at_ms"], 1_700_000_000_000_i64, "{ev}");
    assert!(pushed["received_at_ms"].is_number(), "{ev}");

    let usage = client.call("usage.get", json!({ "session_id": session }));
    assert_eq!(usage["rate_limits"]["openai"], *pushed, "{usage}");
    client.shutdown_and_wait();
    let _ = stop_tx.send(());
    let _ = server_thread.join();
}

#[test]
fn rpc_queue_list_remove_and_clear_follow_the_inbox() {
    use std::collections::BTreeMap;

    // Codex: app-server/tests/suite/v2/thread_queue.rs:120 (identity, order, notifications).
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { peer = { allow = { \"agent.list_queue\" } } }\n")
        .unwrap();
    let script = root.path().join("faux.json");
    std::fs::write(&script, r#"[{"text":"busy done"}]"#).unwrap();
    let env = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--yolo", "--model", "faux/echo"], &env, root.path());
    let held_hook: std::sync::Arc<std::sync::Mutex<Option<Value>>> = std::sync::Arc::new(std::sync::Mutex::new(None));
    let held_slot = std::sync::Arc::clone(&held_hook);
    client.on_method = Some(Box::new(move |_c, msg| {
        if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "request/ready" {
            let mut slot = held_slot.lock().unwrap();
            if slot.is_none() {
                *slot = Some(msg["id"].clone());
                return true;
            }
        }
        false
    }));
    let initialized =
        client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let session = initialized["session"]["session_id"].as_str().expect("session_id").to_string();
    client.call("hooks.subscribe", json!({ "scope": "all", "event": "request/ready", "callbacks": ["handle"] }));
    client.call("events.subscribe", json!({ "scope": "all", "include": ["queue/changed", "turn/completed"] }));

    let prompt = client.call("agent.prompt", json!({ "session_id": session, "input": "busy turn" }));
    assert_eq!(prompt["status"], "started", "{prompt}");
    while held_hook.lock().unwrap().is_none() {
        let msg = client.read();
        client.dispatch(msg);
    }

    let queued = client.call(
        "agent.prompt",
        json!({
            "session_id": session,
            "when_busy": "queue",
            "input": [
                { "type": "text", "text": "queued follow-up" },
                { "type": "image", "mime": "image/png", "data": "AQID" }
            ]
        }),
    );
    assert_eq!(queued["status"], "queued", "{queued}");
    client.wait_event("queue/changed");
    client.call("agent.steer", json!({ "session_id": session, "input": "queued steer" }));
    client.wait_event("queue/changed");

    let listed = client.call("agent.list_queue", json!({ "session_id": session }));
    assert!(!listed.to_string().contains("AQID"), "list_queue must not include image bytes: {listed}");
    let items = listed["data"].as_array().expect("data array");
    assert_eq!(items.len(), 2, "{listed}");
    assert_eq!(items[0]["lane"], "steer", "{listed}");
    assert_eq!(items[0]["text"], "queued steer", "{listed}");
    assert_eq!(items[1]["lane"], "follow_up", "{listed}");
    assert_eq!(items[1]["text"], "queued follow-up", "{listed}");
    assert_eq!(items[1]["images"][0]["mime"], "image/png", "{listed}");
    let steer_id = items[0]["input_id"].as_str().unwrap().to_string();
    let follow_id = items[1]["input_id"].as_str().unwrap().to_string();

    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    peer.call("initialize", json!({ "protocol_version": initialized["protocol_version"] }));
    let peer_list = peer.call("agent.list_queue", json!({ "session_id": session }));
    assert_eq!(peer_list["result"]["data"].as_array().map(Vec::len), Some(2), "{peer_list}");
    let denied_update = peer
        .call("agent.update_queued", json!({ "session_id": session, "input_id": follow_id, "input": "not granted" }));
    assert_eq!(denied_update["error"]["data"]["code"], "denied", "{denied_update}");
    let denied_remove = peer.call("agent.delete_queued", json!({ "session_id": session, "input_id": follow_id }));
    assert_eq!(denied_remove["error"]["data"]["code"], "denied", "{denied_remove}");
    let denied_clear = peer.call("agent.cancel", json!({ "session_id": session }));
    assert_eq!(denied_clear["error"]["data"]["code"], "denied", "{denied_clear}");

    // Grant each operation independently; a grant to one must not authorize the other.
    client
        .call(
            "options.set",
            json!({ "session_id": session, "values": { "peer": { "allow": ["agent.list_queue", "agent.update_queued"] } } }),
        );
    let edited = peer.call(
        "agent.update_queued",
        json!({ "session_id": session, "input_id": follow_id, "input": "edited by peer", "lane": "steer" }),
    );
    assert_eq!(edited["result"]["input_id"], follow_id, "{edited}");
    assert_eq!(edited["result"]["text"], "edited by peer", "{edited}");
    assert_eq!(edited["result"]["lane"], "steer", "{edited}");
    client.wait_event("queue/changed");
    let after_update = client.call("agent.list_queue", json!({ "session_id": session }));
    assert!(
        after_update["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["input_id"] == follow_id && item["text"] == "edited by peer" && item["lane"] == "steer"),
        "{after_update}"
    );
    let denied_remove = peer.call("agent.delete_queued", json!({ "session_id": session, "input_id": follow_id }));
    assert_eq!(denied_remove["error"]["data"]["code"], "denied", "{denied_remove}");

    client
        .call(
            "options.set",
            json!({ "session_id": session, "values": { "peer": { "allow": ["agent.list_queue", "agent.delete_queued"] } } }),
        );
    let denied_update = peer
        .call("agent.update_queued", json!({ "session_id": session, "input_id": follow_id, "input": "not granted" }));
    assert_eq!(denied_update["error"]["data"]["code"], "denied", "{denied_update}");
    let removed = peer.call("agent.delete_queued", json!({ "session_id": session, "input_id": follow_id }));
    assert_eq!(removed["result"], json!({}), "{removed}");
    let queue_ev = client.wait_event("queue/changed");
    assert!(queue_ev["data"].get("follow").is_none() && queue_ev["data"].get("steer").is_none(), "{queue_ev}");
    let follow_count = queue_ev["data"]["items"]
        .as_array()
        .map(|items| items.iter().filter(|item| item["lane"] == "follow_up").count());
    assert_eq!(follow_count, Some(0), "{queue_ev}");

    let after_remove = client.call("agent.list_queue", json!({ "session_id": session }));
    assert_eq!(after_remove["data"].as_array().map(Vec::len), Some(1), "{after_remove}");
    assert_eq!(after_remove["data"][0]["input_id"], steer_id, "{after_remove}");

    let unknown = client.call_err("agent.delete_queued", json!({ "session_id": session, "input_id": "missing-id" }));
    assert_eq!(unknown["data"]["code"], "agent.unknown_input", "{unknown}");

    client.call(
        "options.set",
        json!({ "session_id": session, "values": { "peer": { "allow": ["agent.list_queue", "agent.cancel"] } } }),
    );
    let cleared = peer.call("agent.cancel", json!({ "session_id": session, "clear_queue": true }));
    assert_eq!(cleared["result"]["removed"].as_array().map(Vec::len), Some(1), "{cleared}");
    assert_eq!(cleared["result"]["removed"][0]["input_id"], steer_id, "{cleared}");
    let clear_ev = client.wait_event("queue/changed");
    assert!(clear_ev["data"].get("steer").is_none() && clear_ev["data"].get("follow").is_none(), "{clear_ev}");
    assert_eq!(clear_ev["data"]["items"].as_array().map(Vec::len), Some(0), "{clear_ev}");
    assert_eq!(client.call("agent.list_queue", json!({ "session_id": session }))["data"], json!([]));

    let held_id = held_hook.lock().unwrap().take().unwrap();
    client.send(json!({ "id": held_id, "result": { "value": { "decision": "allow" } } }));
    client.wait_event("turn/completed");
    drop(peer);
    client.shutdown_and_wait();
}

#[test]
fn status_get_uses_contract_validation_and_peer_default() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--yolo", "--model", "faux/echo"], &env, root.path());
    let init = client.call("initialize", json!({"protocol_version": "3.0"}));
    assert!(
        init["capabilities"]["methods"].as_array().unwrap().iter().any(|m| m == "status.get"),
        "primary initialize missing status.get: {init}"
    );
    let st = client.call("status.get", json!({}));
    assert!(st["state"].is_string() && st["seq"].is_u64() && st["pid"].is_u64(), "{st}");

    let err = client.call_err("status.get", json!({ "unexpected": 1 }));
    assert_eq!(err["data"]["code"], json!("invalid_arguments"), "unexpected param must fail contract schema: {err}");

    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    let peer_init = peer.call("initialize", json!({ "protocol_version": init["protocol_version"] }));
    assert!(
        peer_init["result"]["capabilities"]["methods"].as_array().unwrap().iter().any(|m| m == "status.get"),
        "peer initialize must advertise default status.get: {peer_init}"
    );
    let peer_st = peer.call("status.get", json!({}));
    assert!(peer_st["error"].is_null(), "peer status.get failed: {peer_st}");
    assert_eq!(peer_st["result"]["pid"], st["pid"]);

    let peer_err = peer.call("status.get", json!({ "unexpected": 1 }));
    assert_eq!(peer_err["error"]["data"]["code"], json!("invalid_arguments"), "peer invalid args: {peer_err}");

    drop(peer);
    client.shutdown_and_wait();
}

/// Wire-level RPC assertion (`error.data.code == "inactive_ctx"`) for
/// both explicit `inactive_ctx` errors and Cordis `CordisError("INACTIVE_EFFECT")` arrive over JSON-RPC
/// with `error.data.code == "inactive_ctx"` instead of degrading to `internal`.
#[test]
fn rpc_wire_error_preserves_inactive_ctx_and_cordis_inactive_effect() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(
        home.join("config/plugins/wire_inactive_victim.lua"),
        r#"return { name = "wire-victim", apply = function(ctx) mare._wire_victim = ctx end }"#,
    )
    .unwrap();
    std::fs::write(
        home.join("config/plugins/wire_inactive_svc.lua"),
        r#"return {
    name = "wire-inactive-svc",
    provide = { "wire-inactive" },
    contracts = {
        ["wire-inactive"] = {
            methods = {
                stale = { doc = "raise inactive_ctx", expose = "both", result = { type = "object" } },
                cordis = { doc = "effect on inactive ctx", expose = "both", result = { type = "object" } },
            },
        },
    },
    apply = function(ctx)
        local S = require("cordis").class("WireInactive", mare.Service)
        function S:constructor(c)
            S.super.constructor(self, c, "wire-inactive")
        end
        function S:stale()
            error({ code = "inactive_ctx", message = "caller ctx disposed" })
        end
        function S:cordis()
            mare._wire_victim:effect(function() end)
            return {}
        end
        ctx:plugin(S)
    end,
}"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, root.path());
    client.call("initialize", json!({"protocol_version": "3.0"}));
    client.call("wire-inactive.cordis", json!({}));
    std::fs::remove_file(home.join("config/plugins/wire_inactive_victim.lua")).unwrap();
    client.call("commands.run", json!({ "line": "/reload" }));

    let send_err = |c: &mut crate::common::rpc::RpcClient, method: &str| -> Value {
        let req = c.request(method, json!({}));
        loop {
            let msg = c.read();
            if msg.get("id").and_then(Value::as_u64) == Some(req) {
                return msg;
            }
            c.dispatch(msg);
        }
    };
    let stale = send_err(&mut client, "wire-inactive.stale");
    assert_eq!(stale["error"]["data"]["code"], json!("inactive_ctx"), "{stale}");
    let cordis = send_err(&mut client, "wire-inactive.cordis");
    assert_eq!(cordis["error"]["data"]["code"], json!("inactive_ctx"), "{cordis}");
    client.shutdown_and_wait();
}
