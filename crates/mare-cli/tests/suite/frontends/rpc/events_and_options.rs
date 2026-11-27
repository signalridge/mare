use super::*;

/// an unencodable event never throws to the emitter or aborts the
/// turn; it takes no `seq` and emits `dropped { count }` to matching stream consumers.
#[test]
fn rpc_wildcard_reports_unencodable_internal_event_as_incomplete() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
    std::fs::write(
        home.path().join("config/plugins/unencodable.lua"),
        r#"
        return {
            name = "unencodable",
            inject = { "bus" },
            contracts = {
                internal = {
                    owner = "unencodable",
                    events = {
                        unencodable = {
                            doc = "test unencodable payload event",
                            mode = "emit",
                            expose = "both",
                            delivery = "required",
                            ignorable = false,
                            data = { type = "object", additionalProperties = true },
                            example = {},
                        },
                    },
                },
            },
            apply = function(ctx)
                ctx:on("turn/started", function()
                    ctx.bus:emit("internal/unencodable", { bad = function() end })
                end)
            end,
        }
        "#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "stream continues past drop" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.call_raw("events.subscribe", json!({"scope":"all","include":["*"]}), |_| json!(null));
    let prompt = c.request("agent.prompt", json!({"session_id": sid, "input":"go"}));
    let mut dropped_count = 0_u64;
    let prompt_reply = loop {
        let line = c.read();
        assert_ne!(line["method"], "stream.failed", "unencodable event must not fail stream: {line}");
        if line["method"] == "dropped" {
            assert_eq!(line["params"]["session_id"], init["result"]["session"]["session_id"], "{line}");
            assert_eq!(line["params"]["session_epoch"], init["result"]["session"]["session_epoch"], "{line}");
            dropped_count += line["params"]["count"].as_u64().unwrap_or(0);
        }
        if line.get("id").and_then(Value::as_u64) == Some(prompt) {
            assert!(line.get("result").is_some(), "prompt must succeed despite dropped unencodable event: {line}");
            break line;
        }
    };
    let wait = c.request("agent.wait", json!({"session_id": sid, "turn_id": prompt_reply["result"]["turn_id"]}));
    loop {
        let line = c.read();
        assert_ne!(line["method"], "stream.failed", "unencodable event must not fail stream: {line}");
        if line["method"] == "dropped" {
            assert_eq!(line["params"]["session_id"], init["result"]["session"]["session_id"], "{line}");
            assert_eq!(line["params"]["session_epoch"], init["result"]["session"]["session_epoch"], "{line}");
            dropped_count += line["params"]["count"].as_u64().unwrap_or(0);
        }
        if line.get("id").and_then(Value::as_u64) == Some(wait) {
            assert!(line.get("result").is_some(), "wait must succeed: {line}");
            break;
        }
    }
    assert!(dropped_count >= 1, "expected dropped notification for unencodable event");
    c.kill_and_wait();
}

#[test]
fn rpc_prompt_response_follows_its_selected_terminal_event() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "finished" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.call_raw("events.subscribe", json!({"scope":"all","include":["turn/completed"]}), |_| json!(null));
    let prompt = c.call_raw("agent.prompt", json!({"session_id": sid, "input":"go"}), |_| json!(null));
    let wait = c.request("agent.wait", json!({"session_id": sid, "turn_id": prompt["result"]["turn_id"]}));
    let mut ended = false;
    loop {
        let line = c.read();
        if line["method"] == "event" && line["params"]["type"] == "turn/completed" {
            ended = true;
        }
        if line.get("id").and_then(Value::as_u64) == Some(wait) {
            assert!(ended, "response preceded the selected turn/completed event: {line}");
            assert!(line.get("result").is_some(), "{line}");
            break;
        }
    }
    c.kill_and_wait();
}

#[test]
fn rpc_invalid_builtin_event_fails_the_stream() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("bad_event.lua"),
        r#"return {name="bad_event", inject = { "bus" }, apply=function(ctx)
        ctx:on("request/prepare", function() ctx.bus:emit("turn/started", {turn_id="bad-turn", text="missing trigger"}) end)
    end}"#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "finished" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init = c.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    c.call_raw("events.subscribe", json!({"scope":"all","include":["turn/started"]}), |_| json!(null));
    let started = c.call_raw("agent.prompt", json!({"session_id": sid, "input":"go"}), |_| json!(null));
    let wait_req = c.request("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}));
    loop {
        let line = c.read();
        assert_ne!(line.get("id").and_then(Value::as_u64), Some(wait_req), "invalid event did not fail: {line}");
        if line["method"] == "stream.failed" {
            break;
        }
    }
    c.kill_and_wait();
}

/// 条: `options.set` validates the whole candidate before anything is committed
/// — an unknown key, a wrong type or a value an operation refuses fails the call and leaves every
/// value as it was (no half-applied batch); a valid batch commits.
#[test]
fn rpc_options_set_validates_the_whole_batch_before_committing() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| panic!("unexpected callback"));
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let get = |rpc: &mut RpcClient| {
        let got = rpc.call_raw(
            "options.get",
            json!({"session_id": sid, "keys": ["reduce_motion", "modle", "effort", "plugins.approvals.policy"]}),
            |_| panic!("unexpected callback"),
        );
        got["result"].clone()
    };
    let before = get(&mut rpc);
    for values in [
        json!({"reduce_motion": "oops"}),
        json!({"reduce_motion": true, "modle": "x"}),
        json!({"reduce_motion": true, "effort": "not-an-effort"}),
        json!({"reduce_motion": true, "plugins.approvals.policy": 42}),
        // the schema accepts the form, the approvals plugin refuses it when applying (no categories):
        // the apply is part of the write, so the call fails and the committed reduce_motion rolls back
        json!({"reduce_motion": true, "plugins.approvals.policy": {"mode": "granular"}}),
    ] {
        let r = rpc
            .call_raw("options.set", json!({ "session_id": sid, "values": values }), |_| panic!("unexpected callback"));
        assert!(r["error"].is_object(), "{values}: {r}");
        assert_eq!(get(&mut rpc), before, "{values}: nothing of a refused batch is committed");
    }
    let unknown = rpc.call_raw("options.set", json!({"session_id": sid, "values": {"modle": "x"}}), |_| {
        panic!("unexpected callback")
    });
    assert!(unknown["error"]["message"].as_str().unwrap_or_default().contains("did you mean `model`"), "{unknown}");
    let ok = rpc.call_raw("options.set", json!({"session_id": sid, "values": {"reduce_motion": true}}), |_| {
        panic!("unexpected callback")
    });
    assert!(ok.get("error").is_none(), "{ok}");
    assert_eq!(get(&mut rpc)["reduce_motion"], true);
    for null_values in [
        json!({"reduce_motion": null}),
        json!({"plugins.approvals.policy": null}),
        json!({"plugins.approvals.policy": {"mode": null}}),
    ] {
        let r = rpc.call_raw("options.set", json!({ "session_id": sid, "values": null_values }), |_| {
            panic!("unexpected callback")
        });
        assert!(r["error"].is_object(), "{null_values}: null must be rejected, not silently ignored: {r}");
    }
    rpc.shutdown_and_wait();
}

#[test]
fn events_list_returns_the_registry() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| panic!("unexpected callback"));
    assert!(init.get("error").is_none(), "{init}");
    let listed = rpc.call_raw("events.list", json!({}), |_| panic!("unexpected callback"));
    assert!(listed.get("error").is_none(), "{listed}");
    let result: mare_protocol::EventsListResult =
        serde_json::from_value(listed["result"].clone()).expect("events.list decodes into EventsListResult");
    let expected: Vec<mare_protocol::EventInfo> = mare_protocol::event_registry::list_events();
    assert!(
        result.data.len() >= expected.len() && result.data[..expected.len()] == expected[..],
        "events.list must start with PROTOCOL_EVENTS: left={:?} right={:?}",
        result.data,
        expected
    );

    let expected_names: Vec<&str> = expected.iter().map(|entry| entry.name.as_str()).collect();
    let live_names: Vec<&str> = result.data[..expected.len()].iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(live_names, expected_names);

    for entry in &result.data {
        let encoded = serde_json::to_value(entry).unwrap();
        assert!(!encoded["data_schema"].to_string().contains("\"$ref\""), "{} data_schema has $ref", entry.name);
        if entry.public || !entry.example.is_null() {
            mare_protocol::validate_schema_at(&entry.data_schema, "#", &entry.example)
                .unwrap_or_else(|e| panic!("{} example failed data_schema: {e}", entry.name));
        }
    }

    rpc.shutdown_and_wait();
}

#[test]
fn rpc_initialize_exposes_session_and_status_changed_flows_only_through_event_envelope() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "hello-r5", "delay_ms": 20 }]).to_string()).unwrap();

    // 1. Sessionful initialize exposes pre-opened `session` object matching `session_id`.
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.path().display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, home.path());
    let init = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let sid = init["session"]["session_id"].as_str().expect("initialize must include session.session_id");
    let init_epoch = init["session"]["session_epoch"].as_str().expect("initialize must include session.session_epoch");
    assert!(init.get("session_id").is_none(), "top-level session_id must be absent: {init}");
    assert!(init.get("session_epoch").is_none(), "top-level session_epoch must be absent: {init}");
    assert!(!init_epoch.is_empty(), "{init}");
    let sess = client.call("session.get", json!({ "session_id": sid }));
    assert_eq!(sess["session_id"].as_str(), Some(sid), "{sess}");

    // 2. `events.list` includes `scope` (e.g. `status/changed` is `host`).
    let listed = client.call("events.list", json!({}));
    let decoded: mare_protocol::EventsListResult = serde_json::from_value(listed).unwrap();
    let status_info = decoded.data.iter().find(|e| e.name == "status/changed").unwrap();
    assert_eq!(status_info.scope, mare_protocol::EventScope::Host);

    // 3. Running a prompt emits `status/changed` exclusively through `method = "event"`, never standalone `status/changed`.
    let standalone_status = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let standalone_c = standalone_status.clone();
    client.on_method = Some(Box::new(move |_, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("status/changed") {
            standalone_c.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        false
    }));
    let sid = init["session"]["session_id"].as_str().expect("session_id").to_string();
    let started = client.call("agent.prompt", json!({ "session_id": sid, "input": "hi" }));
    assert!(started.get("turn_id").is_some(), "{started}");
    let prompt_res = client.call("agent.wait", json!({ "session_id": sid, "turn_id": started["turn_id"] }));
    assert_eq!(prompt_res["status"], "completed", "{prompt_res}");
    assert!(
        !standalone_status.load(std::sync::atomic::Ordering::SeqCst),
        "standalone status/changed notification must not be sent"
    );
    assert!(
        client.events.iter().any(|ev| ev["type"] == "status/changed"),
        "status/changed must still arrive inside event envelope: {:?}",
        client.events
    );
    client.shutdown_and_wait();

    // 4. `--no-session` opens an ephemeral session (`path == null`) matching `session_id`.
    let mut nosess = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--yolo", "--no-session", "--model", "faux/echo"],
        &env,
        home.path(),
    );
    let init_no = nosess.call("initialize", json!({"protocol_version": "3.0"}));
    let no_sid = init_no["session"]["session_id"].as_str().expect("initialize must include session.session_id");
    let no_sess = nosess.call("session.get", json!({ "session_id": no_sid }));
    assert_eq!(no_sess["session_id"].as_str(), Some(no_sid), "{no_sess}");
    assert!(no_sess["path"].is_null(), "--no-session must have null path: {no_sess}");
    nosess.shutdown_and_wait();
}

#[test]
fn rpc_agent_get_includes_projected_activity_without_separate_activity_method() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
    std::fs::write(
        home.path().join("config/plugins/act.lua"),
        r#"
        local KEEP_SEC
        return {
            name = "act-plug",
            inject = { "agent" },
            apply = function(ctx)
                KEEP_SEC = mare.projection.register("RPC_ACT_SECRET_99")
                ctx.agent:register_activity("demo.state", function()
                    return { phase = "ready", leaked = "before-RPC_ACT_SECRET_99-after" }
                end)
                ctx.agent:register_activity("demo.broken", function()
                    error("broken-activity")
                end)
            end,
        }
        "#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "ok" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| panic!("unexpected callback"));
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

    let got = rpc.call_raw("agent.get", json!({"session_id": sid}), |_| panic!("unexpected callback"));
    assert!(got.get("error").is_none(), "agent.get failed: {got}");
    let res = &got["result"];
    assert!(!res.to_string().contains("RPC_ACT_SECRET_99"), "secret leaked via RPC agent.get: {res}");
    assert_eq!(res["activity"]["extra"]["demo.state"]["phase"], "ready", "{res}");
    let err = res["activity"]["extra_errors"]["demo.broken"].as_str().unwrap_or("");
    assert!(err.contains("broken-activity"), "expected error in extra_errors: {res}");

    let sep = rpc.call_raw("agent.activity", json!({"session_id": sid}), |_| panic!("unexpected callback"));
    assert_eq!(sep["error"]["code"], -32601, "separate agent.activity RPC method must not exist: {sep}");

    rpc.shutdown_and_wait();
}

#[test]
fn rpc_focused_side_agent_get_preserves_projected_activity_and_unifies_side_turns() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("config/plugins")).unwrap();
    std::fs::write(
        home.path().join("config/plugins/side_act.lua"),
        r#"
        local KEEP_SEC
        return {
            name = "side-act-plug",
            inject = { "agent" },
            apply = function(ctx)
                KEEP_SEC = mare.projection.register("SIDE_ACT_SECRET_77")
                ctx.agent:register_activity("demo.state", function()
                    return { phase = "ready", leaked = "before-SIDE_ACT_SECRET_77-after" }
                end)
                ctx.agent:register_activity("demo.broken", function()
                    error("broken-side-activity")
                end)
            end,
        }
        "#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "parent ok" }, { "text": "side ok" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| panic!("unexpected callback"));
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

    let p = rpc.call("agent.prompt", json!({"session_id": sid, "input": "main hi"}));
    let w = rpc.call("agent.wait", json!({"session_id": sid, "turn_id": p["turn_id"]}));
    assert_eq!(w["text"], "parent ok", "{w}");

    rpc.call("commands.run", json!({"line": "/side"}));

    let got = rpc.call("agent.get", json!({"session_id": sid}));
    assert!(got.get("side_id").is_none(), "focused side agent.get must not expose side_id: {got}");
    assert!(got.get("parent_status").is_none(), "focused side agent.get must not expose parent_status: {got}");
    assert!(got.get("params").is_none(), "focused side agent.get must not expose params: {got}");
    assert!(!got.to_string().contains("SIDE_ACT_SECRET_77"), "secret leaked via side agent.get: {got}");
    assert_eq!(got["activity"]["extra"]["demo.state"]["phase"], "ready", "{got}");
    let err = got["activity"]["extra_errors"]["demo.broken"].as_str().unwrap_or("");
    assert!(err.contains("broken-side-activity"), "expected error in extra_errors: {got}");

    let sp = rpc.call("agent.prompt", json!({"session_id": sid, "input": "side hi"}));
    assert_eq!(sp["status"], "started", "{sp}");
    assert!(sp["turn_id"].is_string(), "{sp}");
    assert!(sp.get("side_id").is_none(), "{sp}");
    let sw = rpc.call("agent.wait", json!({"session_id": sid, "turn_id": sp["turn_id"]}));
    assert_eq!(sw["status"], "completed", "{sw}");
    assert_eq!(sw["text"], "side ok", "{sw}");

    let cancel_res = rpc.call("agent.cancel", json!({"session_id": sid}));
    assert!(cancel_res["removed"].is_array(), "{cancel_res}");
    assert!(cancel_res.get("side_only").is_none(), "agent.cancel must not expose side_only: {cancel_res}");

    let conv_send = rpc.call_raw("conversations.send", json!({"text": "nope"}), |_| panic!("unexpected callback"));
    assert_eq!(conv_send["error"]["code"], -32601, "conversations.send must not exist: {conv_send}");

    rpc.shutdown_and_wait();
}
