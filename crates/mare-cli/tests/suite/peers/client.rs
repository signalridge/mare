//! Public outbound peer clients across Lua, child and Print callers, plus peer RPC parity.
use super::*;

fn register_peer_entry(json_path: &std::path::Path) {
    let directory = mare_binding::peers::dir();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::copy(json_path, directory.join(json_path.file_name().unwrap())).unwrap();
}

const HOLD_TURN_PLUGIN: &str = r#"return { name = 'hold-turn', apply = function(ctx)
  local stop = false
  ctx:on('app/shutdown', function() stop = true end)
  ctx:on('request/build', function()
    for _ = 1, 2000 do
      if stop then break end
      mare.task.sleep(10)
    end
  end)
end }"#;

#[tokio::test]
async fn outbound_lua_clients_send_real_messages_track_receipts_and_status_changed_idle() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let mut receiver = session_ui(json!([{ "text": "received" }]), r#"name = "receiver""#, true);
        let _receiver_watch = watchdog(receiver.pid(), 60);
        register_peer_entry(&receiver.json_path);

        let host = env.host().build().await;
        expose(&host);
        global(&host, "RECV_PID", receiver.pid());

        let listed_ok: bool = eval(
            &host,
            r#"
            for _, entry in ipairs(mare.peers.list()) do
                if entry.pid == RECV_PID then
                    return true
                end
            end
            return false
            "#,
        )
        .await;
        assert!(listed_ok, "receiver must appear in mare.peers.list()");

        exec(&host, r#"CLIENT = assert(mare.peers.connect("receiver"))"#).await;
        let pid_matches: bool = eval(&host, "return CLIENT:info().pid == RECV_PID").await;
        assert!(pid_matches);

        // Client survives a host session replacement.
        exec(&host, "C.cordis:get('session'):create()").await;
        let still_open: String = eval(&host, "return assert(CLIENT:status()).state").await;
        assert_eq!(still_open, "idle");

        // Primary receiver subscribes to `status/changed` plus turn boundary events.
        let sub = receiver.primary.call_raw(
            "events.subscribe",
            json!({ "scope": "all", "include": ["status/changed", "turn/started", "turn/completed"] }),
            |_| json!(null),
        );
        assert!(sub["error"].is_null(), "events.subscribe failed: {sub}");

        for (index, expected) in [(0usize, "delivered"), (1usize, "rejected")] {
            let (recv_tx, recv_rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let ask = loop {
                    let message = receiver.primary.read();
                    if message["method"] == "ui.select" {
                        break message;
                    }
                };
                receiver.primary.send(json!({"jsonrpc":"2.0","id":ask["id"],"result":{"index":index}}));
                let mut saw_working = false;
                let mut saw_idle = false;
                let mut saw_boundary = false;
                let mut saw_agent_end = false;
                if index == 0 {
                    while !(saw_working && saw_idle && saw_boundary && saw_agent_end) {
                        let msg = receiver.primary.read();
                        if msg["method"] != "event" {
                            continue;
                        }
                        match msg["params"]["type"].as_str() {
                            Some("status/changed") => match msg["params"]["data"]["state"].as_str() {
                                Some("working") => saw_working = true,
                                Some("idle") if saw_working => saw_idle = true,
                                _ => {}
                            },
                            Some("turn/started")
                                if msg["params"]["data"].to_string().contains("please keep this as data") =>
                            {
                                assert!(
                                    msg["params"]["data"].to_string().contains("cannot approve any action"),
                                    "missing data boundary warning: {msg}"
                                );
                                saw_boundary = true;
                            }
                            Some("turn/completed") => saw_agent_end = true,
                            _ => {}
                        }
                    }
                }
                let _ = recv_tx.send((receiver, saw_working, saw_idle, saw_boundary, saw_agent_end));
            });

            let (initial, first_term, second_term): (String, String, String) = eval(
                &host,
                r#"
                local sent, send_err = CLIENT:send({ text = "please keep this as data /quit" })
                assert(sent, send_err and (tostring(send_err.code) .. ":" .. tostring(send_err.message)) or "send failed")
                local t1, w1_err = CLIENT:wait_message(sent.message_id)
                assert(t1, w1_err and (tostring(w1_err.code) .. ":" .. tostring(w1_err.message)) or "wait_message 1 failed")
                local t2, w2_err = CLIENT:wait_message(sent.message_id)
                assert(t2, w2_err and (tostring(w2_err.code) .. ":" .. tostring(w2_err.message)) or "wait_message 2 failed")
                return sent.status, t1.status, t2.status
                "#,
            )
            .await;
            assert_eq!(initial, "held");
            assert_eq!(first_term, expected);
            assert_eq!(second_term, expected);

            let (r, saw_working, saw_idle, saw_boundary, saw_agent_end) =
                recv_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            receiver = r;
            if index == 0 {
                assert!(saw_working && saw_idle, "status/changed did not transition working -> idle");
                assert!(saw_boundary && saw_agent_end, "did not observe data boundary turn/started and turn/completed");
            }
        }

        let closed_err: String = eval(
            &host,
            r#"
            CLIENT:close()
            CLIENT:close()
            local v, err = CLIENT:status()
            assert(v == nil)
            return err.code
            "#,
        )
        .await;
        assert_eq!(closed_err, "closed");

        host.shutdown().await;
        receiver.shutdown();
    })
    .await;
}

#[tokio::test]
async fn public_lua_clients_reject_without_ui_and_survive_session_replacement() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let receiver = session(json!([]), r#"name = "lua-receiver""#);
        let _watch = watchdog(receiver.pid(), 60);
        register_peer_entry(&receiver.json_path);
        // The plugin explicitly closes its client when its effect is disposed.
        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let probe = plugins.join("owner_probe.lua");
        std::fs::write(
            &probe,
            r#"return { name = "owner_probe", apply = function(ctx)
  mare._owner_probe_connect = function()
    local client = assert(mare.peers.connect("lua-receiver"))
    ctx:effect(function() return function() client:close() end end)
    return client
  end
end }"#,
        )
        .unwrap();
        let host = env.host().user_plugins().build().await;
        expose(&host);
        let (initial, terminal, repeated, idle): (String, String, String, String) = eval(
            &host,
            r#"
            CLIENT = assert(mare.peers.connect("lua-receiver"))
            local receipt = assert(CLIENT:send({text="data from public Lua client"}))
            local terminal = assert(CLIENT:wait_message(receipt.message_id))
            local repeated = assert(CLIENT:wait_message(receipt.message_id))
            local idle = assert(CLIENT:wait_idle({current_or_next=true}))
            return receipt.status, terminal.status, repeated.status, idle.state
        "#,
        )
        .await;
        assert_eq!(
            (initial.as_str(), terminal.as_str(), repeated.as_str(), idle.as_str()),
            ("held", "rejected", "rejected", "idle")
        );
        let denied: String = eval(
            &host,
            r#"return mare.exec.run({sandbox={level="workspace-write",network=false}}, function()
            local value, err = CLIENT:status(); assert(value == nil); return err.code
        end)"#,
        )
        .await;
        assert_eq!(denied, "permission_denied");
        exec(&host, "C.cordis:get('session'):create()").await;
        let state: String = eval(&host, "return assert(CLIENT:status()).state").await;
        assert_eq!(state, "idle");
        exec(&host, "CLIENT:close(); CLIENT:close()").await;
        let owned: mlua::AnyUserData = eval(&host, "return assert(mare._owner_probe_connect())").await;
        let owned = owned.borrow::<mare_binding::peer_client::PeerClient>().unwrap().clone();
        // the plugin goes (its fiber is disposed): its native clients are closed with it
        std::fs::remove_file(&probe).unwrap();
        host.reload_all().await.unwrap();
        assert!(owned.is_closed(), "disposing the owner must finish closing its native clients");
        host.shutdown().await;
        receiver.shutdown();
    })
    .await;
}

const TRANSPORT_PROBE: &str = r#"return {
        name='transport-check', inject={'tools'}, apply=function(ctx)
            ctx.tools:register('send_reference', {
                description='Send reference data to a confirmed local receiver',
                parameters={type='object',properties={text={type='string'}},required={'text'},additionalProperties=false},
                side_effect='external',idempotent=false,
                execute=function(args)
                    local client,err=mare.peers.connect(mare.env('TEST_TARGET'))
                    assert(client,err and err.message)
                    local initial=assert(client:send({text=args.text}))
                    local terminal,failed=client:wait_message(initial.message_id,{timeout_ms=20000})
                    client:close()
                    assert(terminal,failed and failed.message)
                    assert(mare.fs.write(mare.env('TEST_REPORT'),mare.json.encode(terminal)))
                    return {content=mare.json.encode(terminal)}
                end,
            })
        end,
    }"#;

#[test]
fn print_model_tool_uses_public_peer_client_and_actual_receiver_confirmation() {
    let mut receiver = session_ui(json!([{"text":"receiver finished"}]), r#"name = "print-receiver""#, true);
    let _receiver_watch = watchdog(receiver.pid(), 60);
    let home = tempfile::tempdir().unwrap();
    disable_peer_bytecode_cache(home.path());
    let config = home.path().join("config");
    let plugin = config.join("plugins/transport-check");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(config.join("init.lua"), "mare.setup { model='faux/echo' }\n").unwrap();
    std::fs::write(plugin.join("init.lua"), TRANSPORT_PROBE).unwrap();
    let registry = home.path().join("state/peers");
    std::fs::create_dir_all(&registry).unwrap();
    std::fs::copy(&receiver.json_path, registry.join(receiver.json_path.file_name().unwrap())).unwrap();
    let script = home.path().join("print.json");
    std::fs::write(&script,json!([{"tool_calls":[{"name":"send_reference","arguments":{"text":"print tool reference /quit"}}]},{"text":"print sender completed"}]).to_string()).unwrap();
    let report = home.path().join("result.json");
    let stdout_path = home.path().join("peers-print-stdout.log");
    let stderr_path = home.path().join("peers-print-stderr.log");
    let mut cmd = mare_cmd();
    cmd.args(["--print", "--yolo", "--model", "faux/echo", "send the reference"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", script)
        .env("TEST_TARGET", "print-receiver")
        .env("TEST_REPORT", &report)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&stderr_path).unwrap()));
    let child = ChildGroup::spawn(&mut cmd).unwrap();
    let _sender_watch = watchdog(child.id(), 60);
    let ask = loop {
        let message = receiver.primary.read();
        if message["method"] == "ui.select" {
            break message;
        }
    };
    receiver.primary.send(json!({"jsonrpc":"2.0","id":ask["id"],"result":{"index":0}}));
    let status = child.wait_timeout(Duration::from_secs(30));
    let stdout = std::fs::read_to_string(stdout_path).unwrap();
    let stderr = std::fs::read_to_string(stderr_path).unwrap();
    assert!(status.success(), "{stderr}");
    assert!(stdout.contains("print sender completed"), "{stdout}; {stderr}");
    let terminal: Value = serde_json::from_slice(&std::fs::read(report).expect("actual model tool result")).unwrap();
    assert_eq!(terminal["status"], "delivered");
    let start = wait_event(&mut receiver.primary, "turn/started", "print tool reference");
    assert_eq!(start["source"], "peer");
    receiver.shutdown();
}

#[tokio::test]
async fn real_child_model_tool_consumes_the_same_public_peer_client_contract() {
    local(async {
        let mut env=TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let mut receiver=session_ui(json!([{"text":"receiver finished"}]),r#"name = "child-receiver""#,true);
        let _receiver_watch=watchdog(receiver.pid(),60);
        let plugin=env.config_dir().join("plugins/transport-check");
        std::fs::create_dir_all(&plugin).unwrap();
        std::fs::write(plugin.join("init.lua"),TRANSPORT_PROBE).unwrap();
        register_peer_entry(&receiver.json_path);
        let report=env.home.path().join("child-result.json");
        env.set_var("TEST_TARGET","child-receiver");
        env.set_var("TEST_REPORT",&report);
        let confirmation=std::thread::spawn(move || {
            let ask=loop {let message=receiver.primary.read();if message["method"]=="ui.select" {break message;}};
            receiver.primary.send(json!({"jsonrpc":"2.0","id":ask["id"],"result":{"index":0}}));
            let start=wait_event(&mut receiver.primary,"turn/started","child tool reference");
            assert_eq!(start["source"],"peer");
            receiver
        });
        let host=env.host().no_session().faux(json!([{"tool_calls":[{"name":"send_reference","arguments":{"text":"child tool reference"}}]},{"text":"child sender completed"}])).build().await;
        let encoded:String=eval(&host,r#"local child=mare.subagent.start({prompt='Use send_reference then finish',tools={'send_reference'}})
            return mare.json.encode(child:wait())"#).await;
        let result:Value=serde_json::from_str(&encoded).unwrap();
        assert!(result["error"].is_null(),"{encoded}");
        assert!(encoded.contains("child sender completed"),"{encoded}");
        let terminal:Value=serde_json::from_slice(&std::fs::read(report).expect("actual child tool result")).unwrap();
        assert_eq!(terminal["status"],"delivered");
        host.shutdown().await;
        confirmation.join().unwrap().shutdown();
    }).await;
}

#[tokio::test]
async fn queue_only_enqueues_without_waking_idle_receiver() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());
        let mut receiver =
            session_ui(json!([{ "text": "first" }, { "text": "second" }]), r#"name = "queue-receiver""#, true);
        let _receiver_watch = watchdog(receiver.pid(), 60);
        register_peer_entry(&receiver.json_path);

        let (recv_tx, recv_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let ask = loop {
                let message = receiver.primary.read();
                if message["method"] == "ui.select" {
                    break message;
                }
            };
            receiver.primary.send(json!({"jsonrpc":"2.0","id":ask["id"],"result":{"index":0}}));
            let _ = recv_tx.send(receiver);
        });

        let host = env.host().build().await;
        let (has_mode, sent_status, wait_status): (bool, String, String) = eval(
            &host,
            r#"
            local client = assert(mare.peers.connect("queue-receiver"))
            local has_mode = false
            for _, m in ipairs(client:info().delivery_modes or {}) do
                if m == "queue_only" then has_mode = true end
            end
            local sent = assert(client:send({ text = "stay queued until next turn", queue_only = true }))
            local receipt = assert(client:wait_message(sent.message_id))
            client:close()
            return has_mode, sent.status, receipt.status
            "#,
        )
        .await;
        assert!(has_mode);
        assert_eq!(sent_status, "held");
        assert_eq!(wait_status, "delivered");

        let mut receiver = recv_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        let status = receiver.primary.call_raw("status.get", json!({}), |_| json!(null));
        assert_eq!(status["result"]["state"], "idle", "{status}");
        let r_sid = receiver.sid();
        let p_start =
            receiver.primary.call_raw("agent.prompt", json!({"session_id":r_sid,"input":"next turn"}), |_| json!(null));
        assert!(p_start.get("error").is_none(), "{p_start}");
        let prompted = receiver.primary.call_raw(
            "agent.wait",
            json!({"session_id":r_sid,"turn_id":p_start["result"]["turn_id"]}),
            |_| json!(null),
        );
        assert!(prompted.get("error").is_none(), "{prompted}");
        let messages =
            receiver.primary.call_raw("agent.list_messages", json!({ "session_id": r_sid }), |_| json!(null));
        assert!(messages["result"]["data"].to_string().contains("stay queued until next turn"), "{messages}");
        host.shutdown().await;
        receiver.shutdown();
    })
    .await;
}

/// all 8 `peer.client_*` RPC methods and `peer.idle_{subscribe,unsubscribe}`
/// leave core RPC and schema; default peers may subscribe explicitly to `status/changed` only.
#[test]
fn peer_client_rpc_methods_are_removed_and_idle_uses_status_changed() {
    let mut receiver = session(json!([{ "text": "done" }]), r#"name = "b20-receiver""#);
    let mut sender = session(json!([]), r#"name = "b20-sender""#);
    let _receiver_watch = watchdog(receiver.pid(), 60);
    let _sender_watch = watchdog(sender.pid(), 60);

    let removed_methods = [
        "peer.client_list",
        "peer.client_connect",
        "peer.client_info",
        "peer.client_status",
        "peer.client_send",
        "peer.client_wait_message",
        "peer.client_wait_idle",
        "peer.client_close",
        "peer.idle_subscribe",
        "peer.idle_unsubscribe",
        "peer.notify_when_idle",
    ];
    let schema = mare_protocol::schema();
    let schema_methods = schema["methods"].as_object().unwrap();
    let schema_notifications = schema["notifications"].as_object().unwrap();
    assert!(!schema_notifications.contains_key("peer.idle"), "peer.idle notification must be removed from schema");
    let msg_props = schema_methods["peer.message"]["params"]["properties"].as_object().unwrap();
    assert!(!msg_props.contains_key("notifyWhenIdle"), "notifyWhenIdle must be removed from peer.message schema");
    let mut peer = receiver.peer();
    let info = peer.call_raw("peer.info", json!({}), |_| json!(null));
    let allowed = info["result"]["allowed"].as_array().expect("peer.info allowed list");
    assert!(allowed.iter().any(|m| m == "events.subscribe"), "default peer must advertise events.subscribe: {info}");

    for m in removed_methods {
        assert!(!schema_methods.contains_key(m), "{m} must leave protocol schema");
        assert!(!allowed.iter().any(|entry| entry == m), "{m} must leave peer.info.allowed: {info}");
        let err = sender.primary.call_raw(m, json!({}), |_| json!(null));
        assert_eq!(err_code(&err), METHOD_NOT_FOUND, "primary {m}: {err}");
        let peer_err = peer.call_raw(m, json!({}), |_| json!(null));
        assert_eq!(err_code(&peer_err), METHOD_NOT_FOUND, "peer {m}: {peer_err}");
    }

    // Default peer may subscribe ONLY explicitly to `status/changed` (not unfiltered, wildcard, or other events).
    let r_sid = receiver.sid();
    for rejected in [
        json!({ "session_id": r_sid }),
        json!({ "session_id": r_sid, "include": ["*"] }),
        json!({ "session_id": r_sid, "include": ["status/*"] }),
        json!({ "session_id": r_sid, "include": ["turn/completed"] }),
        json!({ "session_id": r_sid, "include": ["status/changed", "turn/completed"] }),
        json!({ "session_id": r_sid, "include": ["status/changed"], "exclude": ["turn/completed"] }),
        json!({ "session_id": r_sid, "include": ["status/changed"], "exclude": ["status/changed"] }),
    ] {
        let denied = peer.call_raw("events.subscribe", rejected.clone(), |_| json!(null));
        assert_eq!(denied["error"]["data"]["code"], "denied", "default peer {rejected}: {denied}");
    }

    // A peer granted `session.create` without `events`/`events.subscribe` must not receive unsolicited `event` frames after switching sessions.
    let mut switch_only = session(json!([{ "text": "one" }, { "text": "two" }]), r#"allow = { "session.create" }"#);
    let _switch_watch = watchdog(switch_only.pid(), 60);
    let mut switch_peer = switch_only.peer();
    let new_res = switch_peer.call_raw("session.create", json!({}), |_| json!(null));
    assert!(new_res["error"].is_null(), "granted session.create failed: {new_res}");
    let sw_sid = new_res["result"]["session_id"].as_str().unwrap().to_string();
    let switch_start =
        switch_only.primary.call_raw("agent.prompt", json!({ "session_id": sw_sid, "input": "turn" }), |_| json!(null));
    assert!(switch_start["error"].is_null(), "switch prompt failed: {switch_start}");
    let switch_prompt = switch_only.primary.call_raw(
        "agent.wait",
        json!({ "session_id": sw_sid, "turn_id": switch_start["result"]["turn_id"] }),
        |_| json!(null),
    );
    assert!(switch_prompt["error"].is_null(), "switch wait failed: {switch_prompt}");
    let probe = switch_peer.request("peer.info", json!({}));
    loop {
        let msg = switch_peer.read();
        if msg["id"] == probe {
            break;
        }
        assert_ne!(
            msg["method"], "event",
            "peer without events grant must not receive unsolicited `event` after session.create: {msg}"
        );
    }
    drop(switch_peer);
    switch_only.shutdown();

    // Idle wait via `events.subscribe` on explicit `status/changed` from the default peer socket.
    let sub = peer
        .call_raw("events.subscribe", json!({ "session_id": r_sid, "include": ["status/changed"] }), |_| json!(null));
    assert!(sub["error"].is_null(), "default peer status/changed subscribe failed: {sub}");
    let prompt_start =
        receiver.primary.call_raw("agent.prompt", json!({ "session_id": r_sid, "input": "work" }), |_| json!(null));
    assert!(prompt_start["error"].is_null(), "prompt error: {prompt_start}");
    let prompt_ok = receiver.primary.call_raw(
        "agent.wait",
        json!({ "session_id": r_sid, "turn_id": prompt_start["result"]["turn_id"] }),
        |_| json!(null),
    );
    assert!(prompt_ok["error"].is_null(), "wait error: {prompt_ok}");

    let mut saw_working = false;
    let mut saw_idle = false;
    while !(saw_working && saw_idle) {
        let msg = peer.read();
        if msg["method"] == "event" {
            assert_eq!(msg["params"]["type"], "status/changed", "unexpected event for default peer: {msg}");
            match msg["params"]["data"]["state"].as_str() {
                Some("working") => saw_working = true,
                Some("idle") if saw_working => saw_idle = true,
                _ => {}
            }
        }
    }

    sender.shutdown();
    receiver.shutdown();
}

fn start_busy_turn(s: &mut Session) {
    let sub = s
        .primary
        .call_raw("events.subscribe", json!({ "scope": "all", "include": ["status/changed"] }), |_| json!(null));
    assert!(sub["error"].is_null(), "events.subscribe failed: {sub}");
    let sid = s.sid();
    let req_id = s.primary.request("agent.prompt", json!({ "session_id": sid, "input": "hold" }));
    let mut saw_prompt = false;
    let mut saw_working = false;
    while !(saw_prompt && saw_working) {
        let m = s.primary.read();
        if m["id"] == req_id {
            assert!(m["error"].is_null(), "prompt failed: {m}");
            saw_prompt = true;
        } else if m["method"] == "event"
            && m["params"]["type"] == "status/changed"
            && m["params"]["data"]["state"] == "working"
        {
            saw_working = true;
        }
    }
}

#[tokio::test]
async fn peer_client_wait_idle_terminates_on_cancel_disconnect_and_owner_reload() {
    local(async {
        let env = TestEnv::new().await;
        disable_peer_bytecode_cache(env.home.path());

        // 1 & 2: busy remote turn + local cancel, then busy remote shutdown/disconnect.
        let mut recv1 = session_env(
            json!([{ "text": "done" }]),
            r#"name = "hold-cancel""#,
            false,
            &[],
            &[("hold_turn.lua", HOLD_TURN_PLUGIN)],
        );
        let _w1 = watchdog(recv1.pid(), 60);
        register_peer_entry(&recv1.json_path);

        let host = env.host().user_plugins().build().await;
        expose(&host);
        exec(&host, r#"CLIENT = assert(mare.peers.connect("hold-cancel"))"#).await;

        start_busy_turn(&mut recv1);
        let state: String = eval(&host, "return assert(CLIENT:status()).state").await;
        assert_eq!(state, "working");

        let (cancel_ok, cancel_code, cancel_val): (bool, String, String) = eval(
            &host,
            r#"
            local tok = mare.task.token()
            local tx, rx = mare.task.channel()
            mare.task.spawn(function()
                local val, err = CLIENT:wait_idle({ cancel = tok, timeout_ms = 20000 })
                tx:send({ ok = val ~= nil, code = err and err.code or "none", val = mare.json.encode(val or mare.json.null) })
            end)
            mare.task.sleep(40)
            tok:cancel()
            local res = rx:recv()
            return res.ok, res.code, res.val
            "#,
        )
        .await;
        if cancel_ok {
            let msgs = recv1.primary.call_raw("session.messages", json!({}), |_| json!(null));
            panic!("wait_idle resolved instead of cancelling: val={cancel_val}, msgs={msgs}");
        }
        assert_eq!(cancel_code, "cancelled");

        exec(
            &host,
            r#"
            DISC_TX, DISC_RX = mare.task.channel()
            mare.task.spawn(function()
                local val, err = CLIENT:wait_idle({ timeout_ms = 20000 })
                DISC_TX:send({ ok = val ~= nil, code = err and err.code or "none", val = mare.json.encode(val or mare.json.null) })
            end)
            mare.task.sleep(40)
            "#,
        )
        .await;
        recv1.primary.kill_and_wait();
        let (disc_ok, disc_code, disc_val): (bool, String, String) = eval(
            &host,
            r#"
            local res = DISC_RX:recv()
            return res.ok, res.code, res.val
            "#,
        )
        .await;
        if disc_ok {
            panic!("wait_idle resolved instead of disconnecting: val={disc_val}");
        }
        assert_ne!(disc_code, "timeout");

        // 3: owner plugin reload while a plugin task is waiting on `wait_idle`.
        let mut recv2 = session_env(
            json!([{ "text": "done" }]),
            r#"name = "hold-reload""#,
            false,
            &[],
            &[("hold_turn.lua", HOLD_TURN_PLUGIN)],
        );
        let _w2 = watchdog(recv2.pid(), 60);
        register_peer_entry(&recv2.json_path);
        start_busy_turn(&mut recv2);

        let plugins = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let reload_plugin = plugins.join("wait_reload.lua");
        std::fs::write(
            &reload_plugin,
            r#"return { name = "wait_reload", apply = function(ctx)
  ctx:effect(function()
    local client = assert(mare.peers.connect("hold-reload"))
    mare._reload_client = client
    mare._reload_outcome = "pending"
    mare.task.spawn(function()
      local val, err = client:wait_idle({ timeout_ms = 20000 })
      mare._reload_outcome = val and "idle" or (err and err.code or "err")
    end)
    return function()
      client:close()
    end
  end)
end }"#,
        )
        .unwrap();
        host.reload_all().await.unwrap();
        let owned: mlua::AnyUserData = eval(&host, "return mare._reload_client").await;
        let owned = owned.borrow::<mare_binding::peer_client::PeerClient>().unwrap().clone();
        exec(&host, "mare.task.sleep(40)").await;
        let before: String = eval(&host, "return mare._reload_outcome").await;
        assert_eq!(before, "pending");

        std::fs::remove_file(&reload_plugin).unwrap();
        host.reload_all().await.unwrap();
        exec(&host, "mare.task.sleep(20)").await;
        assert!(owned.is_closed(), "owner reload must close the wait_idle peer client");
        let after: String = eval(&host, "return mare._reload_outcome").await;
        assert!(after != "pending" && after != "timeout", "wait_idle must terminate on owner reload: {after}");

        host.shutdown().await;
        recv2.shutdown();
    })
    .await;
}
