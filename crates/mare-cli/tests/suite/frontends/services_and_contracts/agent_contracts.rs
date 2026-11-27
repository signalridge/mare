use super::*;

#[test]
fn agent_steer_and_cancel_contracts_enforce_turn_states_idempotency_and_peers() {
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

    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--yolo", "--model", "faux/echo"], &env, &work);
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
    let init = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let session = init["session"]["session_id"].as_str().expect("session_id").to_string();
    client.call("hooks.subscribe", json!({ "scope": "all", "event": "request/ready", "callbacks": ["handle"] }));
    client.call("events.subscribe", json!({ "scope": "all", "include": ["turn/*", "queue/changed"] }));

    // Legacy wire names are removed.
    for (legacy, body) in [
        ("prompt", json!({ "text": "legacy" })),
        ("prompt.try", json!({ "text": "legacy" })),
        ("steer", json!({ "text": "legacy" })),
        ("follow_up", json!({ "text": "legacy" })),
        ("abort", json!({})),
    ] {
        let err = client.call_err(legacy, body);
        assert_eq!(err["code"], -32601, "legacy `{legacy}` must be removed: {err}");
    }

    // Idle steer returns agent.no_active_turn error; idle cancel is idempotent with empty removed.
    let idle_steer = client.call_err("agent.steer", json!({ "session_id": session, "input": "idle steer" }));
    assert_eq!(idle_steer["data"]["code"], "agent.no_active_turn", "{idle_steer}");
    let idle_cancel = client.call("agent.cancel", json!({ "session_id": session }));
    assert!(idle_cancel.get("turn_id").is_none(), "{idle_cancel}");
    assert_eq!(idle_cancel["removed"], json!([]), "{idle_cancel}");
    assert_eq!(client.call("agent.list_queue", json!({ "session_id": session }))["data"], json!([]));

    // Hold turn 1 so we have an active turn_id.
    let started = client.call("agent.prompt", json!({ "session_id": session, "input": "hold turn 1" }));
    assert_eq!(started["status"], "started", "{started}");
    let turn_id = started["turn_id"].as_str().expect("turn_id").to_string();
    while held_hook.lock().unwrap().is_none() {
        let msg = client.read();
        client.dispatch(msg);
    }

    // Busy prompt with when_busy="reject" rejects with `busy` error; when_busy="queue" queues.
    let busy_rej =
        client.call_err("agent.prompt", json!({ "session_id": session, "input": "reject me", "when_busy": "reject" }));
    assert_eq!(busy_rej["data"]["code"], "busy", "{busy_rej}");
    let busy_q = client.call(
        "agent.prompt",
        json!({ "session_id": session, "input": "queued follow", "when_busy": "queue", "request_id": "prompt-q-1" }),
    );
    assert_eq!(busy_q["status"], "queued", "{busy_q}");
    let q_input_id = busy_q["input_id"].as_str().expect("input_id").to_string();

    // Update + delete queued item via contract methods.
    let updated = client.call(
        "agent.update_queued",
        json!({ "session_id": session, "input_id": q_input_id, "input": "edited follow", "lane": "steer" }),
    );
    assert_eq!(updated["input_id"], q_input_id, "{updated}");
    assert_eq!(updated["lane"], "steer", "{updated}");
    assert_eq!(updated["text"], "edited follow", "{updated}");
    client.call("agent.delete_queued", json!({ "session_id": session, "input_id": q_input_id }));
    assert_eq!(client.call("agent.list_queue", json!({ "session_id": session }))["data"], json!([]));

    // Wrong expected_turn_id -> agent.turn_mismatch with active turn_id; wrong cancel turn_id -> no-op.
    let stale_steer = client.call_err(
        "agent.steer",
        json!({ "session_id": session, "input": "wrong turn", "expected_turn_id": "turn-other" }),
    );
    assert_eq!(stale_steer["data"]["code"], "agent.turn_mismatch", "{stale_steer}");
    assert_eq!(stale_steer["data"]["data"]["turn_id"], turn_id, "{stale_steer}");
    let stale_cancel = client.call("agent.cancel", json!({ "session_id": session, "turn_id": "turn-other" }));
    assert!(stale_cancel.get("turn_id").is_none(), "{stale_cancel}");
    assert_eq!(stale_cancel["removed"], json!([]), "{stale_cancel}");
    assert_eq!(client.call("agent.get", json!({ "session_id": session }))["busy"], true);

    // Matching expected_turn_id with ContentPart[] + request_id queues & replays cleanly.
    let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";
    let steer_args = json!({
        "session_id": session,
        "input": [
            { "type": "text", "text": "steer ordered" },
            { "type": "image", "mime": "image/png", "data": png },
            { "type": "file", "path": "notes.md", "mime": "text/markdown", "name": "notes" }
        ],
        "expected_turn_id": turn_id,
        "request_id": "steer-req-1",
    });
    let steered = client.call("agent.steer", steer_args.clone());
    let input_id = steered["input_id"].as_str().expect("input_id").to_string();
    assert_ne!(steered.get("duplicate").and_then(Value::as_bool), Some(true), "{steered}");
    client.wait_event("queue/changed");

    let replay_steer = client.call("agent.steer", steer_args);
    assert_eq!(replay_steer["input_id"], input_id, "{replay_steer}");
    assert_eq!(replay_steer["duplicate"], true, "{replay_steer}");

    let steer_conflict = client.call_err(
        "agent.steer",
        json!({ "session_id": session, "input": "changed", "expected_turn_id": turn_id, "request_id": "steer-req-1" }),
    );
    assert_eq!(steer_conflict["data"]["code"], "request_id_conflict", "{steer_conflict}");

    // Cancel with clear_queue=false cancels turn 1 and retains the steered queue item.
    let cancel_keep = client.call(
        "agent.cancel",
        json!({ "session_id": session, "turn_id": turn_id, "clear_queue": false, "request_id": "cancel-keep-1" }),
    );
    assert_eq!(cancel_keep["turn_id"], turn_id, "{cancel_keep}");
    assert_eq!(cancel_keep["removed"], json!([]), "{cancel_keep}");
    let held_id = held_hook.lock().unwrap().take().unwrap();
    client.send(json!({ "id": held_id, "result": { "decision": "deny", "reason": "aborted" } }));
    client.wait_event("turn/completed");

    let listed = client.call("agent.list_queue", json!({ "session_id": session }));
    assert_eq!(listed["data"].as_array().map(Vec::len), Some(1), "{listed}");

    // First test peer denied when ungranted, then granted.
    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    peer.call("initialize", json!({ "protocol_version": init["protocol_version"] }));
    let denied = peer.call("agent.steer", json!({ "session_id": session, "input": "peer denied" }));
    assert_eq!(denied["error"]["data"]["code"], "denied", "{denied}");

    client.call(
        "options.set",
        json!({ "session_id": session, "values": { "peer": { "allow": ["agent.steer", "agent.cancel"] } } }),
    );
    let started2 = client.call("agent.prompt", json!({ "session_id": session, "input": "hold turn 2" }));
    assert_eq!(started2["status"], "started", "{started2}");
    let turn2_id = started2["turn_id"].as_str().expect("turn2").to_string();
    while held_hook.lock().unwrap().is_none() {
        let msg = client.read();
        client.dispatch(msg);
    }

    let peer_steer =
        peer.call("agent.steer", json!({ "session_id": session, "input": "peer steer", "expected_turn_id": turn2_id }));
    assert!(peer_steer["result"]["input_id"].is_string(), "{peer_steer}");
    let peer_bad_path = peer
        .call("agent.steer", json!({ "session_id": session, "input": [{ "type": "image", "path": "secret.png" }] }));
    assert_eq!(peer_bad_path["error"]["code"], -32602, "{peer_bad_path}");

    // Peer cancel drains queued items and returns full attachment bytes.
    let cancel_req = json!({ "session_id": session, "request_id": "cancel-drain-1" });
    let cancelled = peer.call("agent.cancel", cancel_req.clone());
    assert_eq!(cancelled["result"]["turn_id"], turn2_id, "{cancelled}");
    let removed = cancelled["result"]["removed"].as_array().expect("removed");
    assert_eq!(removed.len(), 2, "{cancelled}");
    assert_eq!(removed[0]["images"][0]["data"], png, "{cancelled}");
    let held_id2 = held_hook.lock().unwrap().take().unwrap();
    client.send(json!({ "id": held_id2, "result": { "decision": "deny", "reason": "aborted" } }));
    client.wait_event("turn/completed");

    let cancel_replay = client.call("agent.cancel", cancel_req);
    assert_eq!(cancel_replay["turn_id"], turn2_id, "{cancel_replay}");
    assert_eq!(cancel_replay["duplicate"], true, "{cancel_replay}");
    let cancel_conflict = client.call_err(
        "agent.cancel",
        json!({ "session_id": session, "clear_queue": false, "request_id": "cancel-drain-1" }),
    );
    assert_eq!(cancel_conflict["data"]["code"], "request_id_conflict", "{cancel_conflict}");

    drop(peer);
    client.shutdown_and_wait();
}

#[tokio::test]
async fn agent_cancel_interrupts_when_poisoned_or_reloading_via_direct_or_services_call() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    local(async {
        let env = TestEnv::new().await;
        let host = env.host().faux(json!([{ "text": "hold turn" }])).build().await;
        let lua = host.lua().clone();
        let ctx = host.kernel.host_ctx().unwrap();
        let owner = host.kernel.host_owner().unwrap();

        let started_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let gate_release = std::sync::Arc::new(tokio::sync::Notify::new());
        let s_n = started_notify.clone();
        let g_r = gate_release.clone();
        let hook_fn = lua
            .create_async_function(move |lua, _: mlua::MultiValue| {
                let s_n = s_n.clone();
                let g_r = g_r.clone();
                async move {
                    s_n.notify_one();
                    g_r.notified().await;
                    mare_shared::lua::json_to_lua(&lua, &json!({ "decision": "allow" }))
                }
            })
            .unwrap();
        host.kernel
            .host_voter(
                &ctx,
                owner,
                "request/ready",
                hook_fn,
                mare_kernel::HostVoter { timeout: Some(std::time::Duration::ZERO), ..Default::default() },
            )
            .unwrap();

        let (client, server) = tokio::io::duplex(65536);
        let (sr, sw) = tokio::io::split(server);
        let server_task = tokio::task::spawn_local(mare_cli::rpc::serve(
            host.clone(),
            sr,
            sw,
            true,
            std::future::pending(),
        ));
        let (cr, mut cw) = tokio::io::split(client);
        let mut cr = BufReader::new(cr).lines();

        async fn send(w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, v: Value) {
            let line = format!("{}\n", serde_json::to_string(&v).unwrap());
            w.write_all(line.as_bytes()).await.unwrap();
        }
        async fn recv_id(
            r: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
            id: i64,
        ) -> Value {
            loop {
                let line = r.next_line().await.unwrap().unwrap();
                let v: Value = serde_json::from_str(&line).unwrap();
                if v.get("id").and_then(Value::as_i64) == Some(id) {
                    return v;
                }
            }
        }

        send(&mut cw, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "3.0"} })).await;
        let init = recv_id(&mut cr, 1).await;
        let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

        send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "agent.prompt", "params": { "session_id": session_id, "input": "work" } })).await;
        let prompt_res = recv_id(&mut cr, 2).await;
        assert_eq!(prompt_res["result"]["status"], "started", "{prompt_res}");
        let turn_id = prompt_res["result"]["turn_id"].as_str().unwrap().to_string();
        started_notify.notified().await;

        // Poison the session barrier AND hold the reload barrier concurrently:
        // direct agent.cancel must STILL cancel the active turn without waiting or failing
        // (`services.call` is removed on the wire).
        mare_binding::exec_gate::poison(&lua, "disk transaction failed");
        let _reload_hold = host.kernel.reload_hold();
        send(
            &mut cw,
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "agent.cancel",
                "params": { "session_id": session_id, "turn_id": turn_id }
            }),
        )
        .await;
        let cancel_res = tokio::time::timeout(std::time::Duration::from_secs(2), recv_id(&mut cr, 3))
            .await
            .expect("agent.cancel must not wait on reload barrier");
        assert!(cancel_res.get("error").is_none(), "agent.cancel must succeed when poisoned: {cancel_res}");
        assert_eq!(cancel_res["result"]["turn_id"], turn_id, "{cancel_res}");
        assert_eq!(cancel_res["result"]["removed"], json!([]), "{cancel_res}");
        let st = host.state().await;
        assert!(
            st["turn_cancelled"] == true || st["busy"] == false,
            "turn must be cancelled or already settled: {st}"
        );

        gate_release.notify_one();
        drop(_reload_hold);
        send(
            &mut cw,
            json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "agent.wait",
                "params": { "session_id": session_id, "turn_id": turn_id }
            }),
        )
        .await;
        let wait_res = tokio::time::timeout(std::time::Duration::from_secs(5), recv_id(&mut cr, 4))
            .await
            .expect("agent.wait after cancel");
        assert!(wait_res.get("error").is_none(), "{wait_res}");
        assert_eq!(wait_res["result"]["status"], "cancelled", "{wait_res}");
        drop(cw);
        drop(cr);
        let _ = server_task.await;
        host.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn agent_public_methods_strip_internal_opts_while_spi_accepts_them() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "public-ok" }])).build().await;
        expose(&h);

        let caller = h.kernel.host_owner().unwrap();
        let img_json = json!([{ "type": "image", "mime": "image/png", "data": "AQID" }]);

        // Wire agent.prompt rejects Lua-only fields at the contract schema boundary.
        for (field, val) in
            [("turn_id", json!("forged-turn")), ("queue_only", json!(true)), ("images", img_json.clone())]
        {
            let mut args = json!({ "input": "wire msg" });
            args[field] = val;
            let err = h
                .kernel
                .call_service_export(caller, None, "agent", "prompt", args, Default::default())
                .await
                .expect_err(&format!("wire agent.prompt must reject `{field}`"));
            assert_eq!(err.code(), "invalid_arguments", "agent.prompt.{field}: {err}");
            assert!(err.to_string().contains(field), "{err}");
        }

        // In-process Lua agent:prompt accepts queue_only and images without waking an idle agent.
        let (queued_id, spi_status, q_after_rew): (String, String, String) = eval(
            &h,
            r#"
            local img = { { type = "image", mime = "image/png", data = "AQID" } }
            local queued = A:prompt("spi queued", { queue_only = true, images = img })
            local q = A:list_queue().data
            assert(#q == 1 and q[1].input_id == queued.input_id, mare.json.encode(q))
            return queued.input_id, queued.status, mare.json.encode(q)
            "#,
        )
        .await;
        assert_eq!(spi_status, "queued", "{q_after_rew}");

        // Wire agent.update_queued rejects Lua-only `images` while in-process Lua accepts it.
        let upd_err = h
            .kernel
            .call_service_export(
                caller,
                None,
                "agent",
                "update_queued",
                json!({ "input_id": queued_id, "input": "edited-pub", "images": img_json }),
                Default::default(),
            )
            .await
            .expect_err("wire agent.update_queued must reject `images`");
        assert_eq!(upd_err.code(), "invalid_arguments", "{upd_err}");
        assert!(upd_err.to_string().contains("images"), "{upd_err}");

        let q_after_lua_upd: String = eval(
            &h,
            r#"
            local img = { { type = "image", mime = "image/png", data = "AQID" } }
            local q = A:list_queue().data
            A:update_queued(q[1].input_id, { input = "edited-spi", images = img })
            return mare.json.encode(A:list_queue().data)
            "#,
        )
        .await;
        assert!(
            q_after_lua_upd.contains(r#""images":[{"mime":"image/png"}]"#) && q_after_lua_upd.contains("edited-spi"),
            "in-process update_queued must accept Lua-only images: {q_after_lua_upd}"
        );
        h.shutdown().await;
    })
    .await;
}

#[test]
fn agent_continue_executes_retained_steer_after_cancel_without_clearing_queue() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let script = root.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "steer-completed" }]).to_string()).unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);

    let mut client = crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, &work);
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

    let init = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    let session = init["session"]["session_id"].as_str().expect("session_id").to_string();
    client.call("hooks.subscribe", json!({ "scope": "all", "event": "request/ready", "callbacks": ["handle"] }));
    client.call("events.subscribe", json!({ "scope": "all", "include": ["turn/*", "queue/changed"] }));

    let started = client.call("agent.prompt", json!({ "session_id": session, "input": "hold turn 1" }));
    let turn1 = started["turn_id"].as_str().expect("turn_id").to_string();
    while held_hook.lock().unwrap().is_none() {
        let msg = client.read();
        client.dispatch(msg);
    }

    let steered = client
        .call("agent.steer", json!({ "session_id": session, "input": "retained steer", "expected_turn_id": turn1 }));
    assert!(steered["input_id"].is_string(), "{steered}");
    client.wait_event("queue/changed");

    let cancelled =
        client.call("agent.cancel", json!({ "session_id": session, "turn_id": turn1, "clear_queue": false }));
    assert_eq!(cancelled["turn_id"], turn1, "{cancelled}");
    let held_id = held_hook.lock().unwrap().take().unwrap();
    client.send(json!({ "id": held_id, "result": { "decision": "deny", "reason": "aborted" } }));
    client.wait_event("turn/completed");

    let queue = client.call("agent.list_queue", json!({ "session_id": session }));
    assert_eq!(queue["data"].as_array().map(Vec::len), Some(1), "{queue}");
    assert_eq!(queue["data"][0]["lane"], "steer", "{queue}");

    let cont = client.call("agent.continue", json!({ "session_id": session }));
    let cont_turn = cont["turn_id"].as_str().expect("continue turn_id").to_string();
    let held_id2 = loop {
        if let Some(id) = held_hook.lock().unwrap().take() {
            break id;
        }
        let msg = client.read();
        client.dispatch(msg);
    };
    client.send(json!({ "id": held_id2, "result": { "decision": "allow" } }));
    let done = client.call("agent.wait", json!({ "session_id": session, "turn_id": cont_turn }));
    assert_eq!(done["status"], "completed", "{done}");
    assert_eq!(done["text"], "steer-completed", "{done}");
    assert_eq!(client.call("agent.list_queue", json!({ "session_id": session }))["data"], json!([]));
    let msgs = client.call("agent.list_messages", json!({ "session_id": session }));
    assert!(msgs.to_string().contains("retained steer"), "retained steer must run on continue: {msgs}");

    client.shutdown_and_wait();
}

#[tokio::test]
async fn agent_continue_cancel_during_agent_start_hook_does_not_stick_busy() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    local(async {
        let env = TestEnv::new().await;
        let host = env
            .host()
            .faux(json!([{ "text": "seed-ok" }, { "text": "resumed-after-continue" }, { "text": "fresh-prompt-ok" }]))
            .build()
            .await;
        expose(&host);
        let lua = host.lua().clone();

        exec(
            &host,
            r#"
            local t1 = A:prompt("seed").turn_id
            A:wait(t1)
            -- Retain a follow-up item cleanly via SPI queue_only without waking an idle turn.
            A:prompt("queued for continue", { queue_only = true })
            "#,
        )
        .await;

        let start_entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let start_release = std::sync::Arc::new(tokio::sync::Notify::new());
        let se = start_entered.clone();
        let sr = start_release.clone();
        let hook = lua
            .create_async_function(move |_lua, _: mlua::MultiValue| {
                let se = se.clone();
                let sr = sr.clone();
                async move {
                    se.notify_one();
                    sr.notified().await;
                    Ok(())
                }
            })
            .unwrap();
        lua.globals().set("HOLD_AGENT_START", hook).unwrap();
        exec(&host, r#"C.cordis:on("turn/started", function() HOLD_AGENT_START() end)"#).await;

        let (client, server) = tokio::io::duplex(65536);
        let (sr_io, sw_io) = tokio::io::split(server);
        let server_task = tokio::task::spawn_local(mare_cli::rpc::serve(
            host.clone(),
            sr_io,
            sw_io,
            true,
            std::future::pending(),
        ));
        let (cr, mut cw) = tokio::io::split(client);
        let mut cr = BufReader::new(cr).lines();

        async fn send(w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, v: Value) {
            let line = format!("{}\n", serde_json::to_string(&v).unwrap());
            w.write_all(line.as_bytes()).await.unwrap();
        }
        async fn recv_id(
            r: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
            id: i64,
        ) -> Value {
            loop {
                let line = r.next_line().await.unwrap().unwrap();
                let v: Value = serde_json::from_str(&line).unwrap();
                if v.get("id").and_then(Value::as_i64) == Some(id) {
                    return v;
                }
            }
        }

        send(&mut cw, json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocol_version": "3.0"} })).await;
        let init = recv_id(&mut cr, 1).await;
        let session_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

        // Call `agent.continue`, wait until `turn/started` listener blocks, then send $/cancel_request.
        send(&mut cw, json!({ "jsonrpc": "2.0", "id": 2, "method": "agent.continue", "params": { "session_id": session_id } })).await;
        start_entered.notified().await;
        send(&mut cw, json!({ "jsonrpc": "2.0", "method": "$/cancel_request", "params": { "id": 2 } })).await;
        let cont_res = recv_id(&mut cr, 2).await;
        start_release.notify_one();

        assert!(
            cont_res.get("error").is_none(),
            "agent.continue must mark_committed before spawning turn/started so $/cancel_request never aborts mid-claim: {cont_res}"
        );
        let cont_turn = cont_res["result"]["turn_id"].as_str().expect("turn_id").to_string();
        send(
            &mut cw,
            json!({ "jsonrpc": "2.0", "id": 3, "method": "agent.wait", "params": { "session_id": session_id, "turn_id": cont_turn } }),
        )
        .await;
        let wait_res = recv_id(&mut cr, 3).await;
        assert_eq!(wait_res["result"]["status"], "completed", "{wait_res}");

        send(&mut cw, json!({ "jsonrpc": "2.0", "id": 4, "method": "agent.get", "params": { "session_id": session_id } })).await;
        let got = recv_id(&mut cr, 4).await;
        assert_eq!(got["result"]["busy"], false, "agent.get().busy must return false: {got}");

        send(
            &mut cw,
            json!({ "jsonrpc": "2.0", "id": 5, "method": "agent.prompt", "params": { "session_id": session_id, "input": "fresh", "when_busy": "reject" } }),
        )
        .await;
        let fresh = recv_id(&mut cr, 5).await;
        assert_eq!(fresh["result"]["status"], "started", "{fresh}");

        drop(cw);
        drop(cr);
        let _ = server_task.await;
        host.shutdown().await;
    })
    .await;
}

#[test]
fn rpc_and_peer_reject_spi_methods_and_internal_fields_on_steer_and_continue() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let env = std::collections::BTreeMap::from([("MARE_HOME".to_string(), home.display().to_string())]);

    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--peer", "--yolo", "--model", "faux/echo"], &env, &work);
    let init = client.call("initialize", json!({"protocol_version": "3.0"}));
    let session = init["session"]["session_id"].as_str().expect("session_id").to_string();

    let spi_names = ["submit", "steer_input", "resume", "rewrite_queued", "messages"];
    let rpc_methods = init["capabilities"]["methods"].as_array().unwrap();
    for name in spi_names {
        let full = format!("agent.{name}");
        assert!(!rpc_methods.iter().any(|m| m == &full), "{full} must not appear in initialize capabilities");
    }
    let listed = client.call("services.list", json!({}));
    let agent_entry = listed["data"].as_array().unwrap().iter().find(|s| s["name"] == "agent").expect("agent service");
    let exported = agent_entry["methods"].as_object().unwrap();
    for name in spi_names {
        assert!(!exported.contains_key(name), "agent.{name} SPI must not appear in services.list");
    }

    client.call(
        "options.set",
        json!({ "session_id": client.session_id(), "values": { "peer": { "allow": [
            "agent.submit",
            "agent.steer_input",
            "agent.resume",
            "agent.rewrite_queued",
            "agent.messages",
            "agent.steer",
            "agent.continue"
        ] } } }),
    );
    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    peer.call("initialize", json!({ "protocol_version": init["protocol_version"] }));

    for name in spi_names {
        let wire_method = format!("agent.{name}");
        let direct_primary = client.call_err(&wire_method, json!({ "session_id": session }));
        assert_eq!(direct_primary["code"], -32601, "primary direct {wire_method}: {direct_primary}");
        let direct_peer = peer.call(&wire_method, json!({ "session_id": session }));
        assert_eq!(direct_peer["error"]["code"], -32601, "peer direct {wire_method}: {direct_peer}");

        let svc_primary = client.call_err(
            "services.call",
            json!({ "service": "agent", "method": name, "arguments": { "session_id": session } }),
        );
        assert_eq!(svc_primary["code"], -32601, "primary services.call {wire_method}: {svc_primary}");
        let svc_peer = peer.call(
            "services.call",
            json!({ "service": "agent", "method": name, "arguments": { "session_id": session } }),
        );
        assert_eq!(svc_peer["error"]["code"], -32601, "peer services.call {wire_method}: {svc_peer}");
    }

    for (field, val) in [
        ("turn_id", json!("forged")),
        ("images", json!([{ "type": "image", "mime": "image/png", "data": "AQID" }])),
        ("custom_message", json!({ "type": "t", "content": "c" })),
    ] {
        let mut args = json!({ "session_id": session, "input": "hi" });
        args[field] = val;
        let err = client.call_err("agent.steer", args.clone());
        assert_eq!(err["code"], -32602, "primary agent.steer `{field}`: {err}");
        let peer_res = peer.call("agent.steer", args);
        assert_eq!(peer_res["error"]["code"], -32602, "peer agent.steer `{field}`: {peer_res}");
    }

    for (field, val) in [
        ("messages", json!([])),
        ("history_model", json!("faux/echo")),
        ("queued", json!(true)),
        ("transition_id", json!("t1")),
    ] {
        let mut args = json!({ "session_id": session });
        args[field] = val;
        let err = client.call_err("agent.continue", args.clone());
        assert_eq!(err["code"], -32602, "primary agent.continue `{field}`: {err}");
        let peer_res = peer.call("agent.continue", args);
        assert_eq!(peer_res["error"]["code"], -32602, "peer agent.continue `{field}`: {peer_res}");
    }

    drop(peer);
    client.shutdown_and_wait();
}

#[tokio::test]
async fn host_spi_cancel_propagates_to_follow_up_and_steer() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);

        exec(
            &h,
            r#"
            ENTERED_TX, ENTERED_RX = mare.task.channel()
            CAPTURED_CANCEL_TYPES = {}
            local orig_prompt = A.prompt
            A.prompt = function(self, input, opts, ...)
                CAPTURED_CANCEL_TYPES[#CAPTURED_CANCEL_TYPES + 1] = type(opts and opts.cancel)
                ENTERED_TX:send(true)
                opts.cancel:wait()
                return orig_prompt(self, input, opts, ...)
            end
            local orig_steer = A.steer
            A.steer = function(self, input, opts, ...)
                CAPTURED_CANCEL_TYPES[#CAPTURED_CANCEL_TYPES + 1] = type(opts and opts.cancel)
                ENTERED_TX:send(true)
                opts.cancel:wait()
                return orig_steer(self, input, opts, ...)
            end
            "#,
        )
        .await;

        let tok_follow = tokio_util::sync::CancellationToken::new();
        let h_c = h.clone();
        let tf_c = tok_follow.clone();
        let follow_task = tokio::task::spawn_local(async move {
            h_c.follow_up_with_cancel(
                "spi cancelled follow",
                json!({ "source": "peer", "peer_message": true, "queue_only": true }),
                tf_c,
            )
            .await
        });
        exec(&h, "assert(ENTERED_RX:recv())").await;
        tok_follow.cancel();
        let follow_err = tokio::time::timeout(std::time::Duration::from_secs(2), follow_task)
            .await
            .expect("follow_up_with_cancel should unblock when cancelled")
            .unwrap()
            .unwrap_err();
        assert!(follow_err.to_string().contains("cancelled"), "{follow_err}");

        let tok_steer = tokio_util::sync::CancellationToken::new();
        let h_s = h.clone();
        let ts_s = tok_steer.clone();
        let steer_task = tokio::task::spawn_local(async move {
            h_s.steer_with_cancel("spi cancelled steer", json!({ "source": "peer", "peer_message": true }), ts_s).await
        });
        exec(&h, "assert(ENTERED_RX:recv())").await;
        tok_steer.cancel();
        let steer_err = tokio::time::timeout(std::time::Duration::from_secs(2), steer_task)
            .await
            .expect("steer_with_cancel should unblock when cancelled")
            .unwrap()
            .unwrap_err();
        assert!(steer_err.to_string().contains("cancelled"), "{steer_err}");

        // Reload barrier must also abort immediately when cancel fires.
        let _reload = h.kernel.reload_hold();
        let pre_cancel = tokio_util::sync::CancellationToken::new();
        pre_cancel.cancel();
        let reload_err = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            h.follow_up_with_cancel("held", json!({ "peer_message": true, "queue_only": true }), pre_cancel),
        )
        .await
        .expect("reload barrier must cancel cleanly")
        .unwrap_err();
        assert!(reload_err.to_string().contains("cancelled"), "{reload_err}");
        drop(_reload);

        let captured: String = eval(&h, "return table.concat(CAPTURED_CANCEL_TYPES, ',')").await;
        assert_eq!(captured, "userdata,userdata");
        assert_eq!(eval::<i64>(&h, "return #A:list_queue().data").await, 0);
        h.shutdown().await;
    })
    .await;
}
