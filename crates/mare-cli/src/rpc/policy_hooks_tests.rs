use super::*;
use crate::rpc::rpc_harness::{HOST_LOCK, TestRpc, rpc_read, rpc_send, test_host};

#[tokio::test]
async fn rpc_tool_access_path_arg_is_validated_and_bound_to_the_call() {
    tokio::task::LocalSet::new().run_until(async {
        let _guard = HOST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("input.txt"), "test data").unwrap();
        let host = test_host(dir.path()).await;
        let TestRpc { server, mut cr, mut cw, .. } = TestRpc::connect(host.clone());
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}})).await;
        while rpc_read(&mut cr).await["id"] != 1 {}
        for (id, spec) in [
            (2, json!({"kind":"file","op":"read","path_arg":"path","recursive":true})),
            (3, json!({"kind":"file","op":"read","path_arg":"missing","recursive":false})),
            (4, json!({"kind":"file","op":"read","pathArg":"path","recursive":false})),
        ] {
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":id,"method":"tools.register","params":{
                "name":"remote-path", "description":"remote read", "parameters":{"type":"object",
                    "properties":{"path":{"type":"string"}},"required":["path"]},
                "callbacks":["execute"], "access":[spec]
            }})).await;
            let reply = loop { let msg = rpc_read(&mut cr).await; if msg["id"] == id { break msg; } };
            assert_eq!(reply["error"]["code"], codes::INVALID_PARAMS, "{reply}");
        }
        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"tools.register","params":{
            "name":"remote-path", "description":"remote read", "parameters":{"type":"object",
                "properties":{"path":{"type":"string"}},"required":["path"]},
            "callbacks":["execute"],
            "access":[{"kind":"file","op":"read","path_arg":"path","recursive":false}]
        }})).await;
        let reply = loop { let msg = rpc_read(&mut cr).await; if msg["id"] == 5 { break msg; } };
        assert!(reply.get("error").is_none(), "{reply}");
        let lua = host.lua();
        lua.globals().set("RPC_TEST_CTX", host.kernel.host_ctx().unwrap()).unwrap();
        lua.globals().set("RPC_TEST_TOOLS", host.service_table("tools").unwrap()).unwrap();
        let (refused, state, path, unconfined): (bool, String, String, bool) = lua.load(r#"
            local seen
            RPC_TEST_CTX:get("policy"):voter("tool/before", function(call, info)
                if call.name == "remote-path" then seen = info; return { decision = "deny", reason = "test" } end
            end)
            local r = RPC_TEST_TOOLS:execute({ id = "rpc-access", name = "remote-path", arguments = { path = "input.txt" } })
            return r.refused == true, seen and seen.access_state, seen and seen.access[1].physical,
                seen and seen.unconfined == true
        "#).eval_async().await.unwrap();
        assert!(refused && unconfined);
        assert_eq!(state, "declared");
        assert_eq!(std::path::Path::new(&path), dunce::canonicalize(dir.path().join("input.txt")).unwrap());
        drop(cw);
        server.abort();
        let _ = server.await;
        host.shutdown().await;
    }).await;
}

/// Remote voters on decision points (a remote participant in a policy decision point the
/// policy service lists): without `capabilities.callbacks` nothing subscribes; a deny, `false`,
/// a malformed or a missing answer is a deny; a withdrawn capability turns an existing voter into a
/// deny; a point the service does not list refuses a remote voter.
#[tokio::test]
async fn rpc_dispatch_category_enforces_all_policy_events() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());
            let events = ["tool/before", "tool/approval", "net/approval", "permission/verdict", "request/ready"];
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocol_version":"3.0","capabilities":{"callbacks":false},"callback_timeout_ms":25}}),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 {
                    break;
                }
            }
            // No capability: hook subscription is rejected with `unsupported`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":2,"method":"hooks.subscribe","params":{"scope":"all","event":events[0],"callbacks":["handle"]}}),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 2 {
                    assert!(
                        msg["error"]["message"].as_str().is_some_and(|m| m.contains("callback.call")),
                        "expected callback.call unsupported error: {msg}"
                    );
                    break;
                }
            }
            // Enable capabilities.callbacks to subscribe, then test toggling enabled = [false, true, false].
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":20,"method":"connection.set_capabilities",
                    "params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 20 {}
            for (idx, event) in events.iter().enumerate() {
                let id = 21 + idx as i64;
                rpc_send(
                    &mut cw,
                    json!({"jsonrpc":"2.0","id":id,"method":"hooks.subscribe","params":{"scope":"all","event":event,"callbacks":["handle"]}}),
                )
                .await;
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["id"] == id {
                        assert!(msg.get("error").is_none(), "{msg}");
                        break;
                    }
                }
            }
            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            lua.globals().set("category_test_ctx", ctx).unwrap();
            lua.globals().set("category_test_path", dir.path().join("side-effect").to_str().unwrap()).unwrap();
            let run = lua
                .load(
                    r#"return function(event)
            local vote = category_test_ctx:get("policy"):decide(event, { {} })
            if vote.decision == "allow" then
                local f = assert(io.open(category_test_path, "w")); f:write("executed"); f:close()
            end
            return vote
        end"#,
                )
                .eval::<Function>()
                .unwrap();
            for enabled in [false, true, false] {
                rpc_send(
                    &mut cw,
                    json!({"jsonrpc":"2.0","id":3,"method":"connection.set_capabilities",
                "params":{"capabilities":{"callbacks":enabled,"ui":false,"approvals":false,"elicitation":false}}}),
                )
                .await;
                loop {
                    if rpc_read(&mut cr).await["id"] == 3 {
                        break;
                    }
                }
                for event in events {
                    if !enabled {
                        let vote = run.call_async::<Table>(event).await.unwrap();
                        assert_eq!(vote.get::<String>("decision").unwrap(), "deny", "{event}");
                        assert!(
                            vote.get::<String>("reason").unwrap().contains("callback.call"),
                            "{event}: {vote:?}"
                        );
                    } else {
                        for (reply, malformed) in [
                            (Some(json!({"value":{"decision":"deny"}})), false),
                            (Some(json!({"value":false})), false),
                            (Some(json!("malformed envelope")), true),
                            (Some(json!([])), true),
                            (Some(json!([1])), true),
                            (Some(json!(42)), true),
                            (Some(json!(true)), true),
                            (Some(json!(null)), true),
                            (None, false),
                        ] {
                            let f = run.clone();
                            let vote =
                                tokio::task::spawn_local(async move { f.call_async::<Table>(event).await.unwrap() });
                            loop {
                                let msg = rpc_read(&mut cr).await;
                                if msg["method"] == "callback.call" {
                                    assert_eq!(msg["params"]["arguments"]["event"], event);
                                    assert_eq!(msg["params"]["arguments"]["mode"], "policy");
                                    if let Some(reply) = reply {
                                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":reply})).await;
                                    }
                                    break;
                                }
                            }
                            let vote = vote.await.unwrap();
                            assert_eq!(vote.get::<String>("decision").unwrap(), "deny", "{event}: {vote:?}");
                            if malformed {
                                assert!(
                                    vote.get::<String>("reason").unwrap().contains("answered no { value } envelope"),
                                    "{event}: {vote:?}"
                                );
                            }
                        }
                    }
                    assert!(!dir.path().join("side-effect").exists(), "{event}: denied body executed");
                }
            }
            // Prove the guarded body is reachable when the host really authorizes it.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":4,"method":"connection.set_capabilities",
            "params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 4 {
                    break;
                }
            }
            // An object without a vote is an abstention, not a malformed envelope.
            let abstain = lua
                .load("return function() return category_test_ctx:get('policy'):decide('request/ready', { {} }) end")
                .eval::<Function>()
                .unwrap();
            for reply in [json!({"value": null}), json!({})] {
                let f = abstain.clone();
                let vote = tokio::task::spawn_local(async move { f.call_async::<Table>(()).await.unwrap() });
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["method"] == "callback.call" {
                        rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":reply})).await;
                        break;
                    }
                }
                assert_eq!(vote.await.unwrap().get::<String>("decision").unwrap(), "allow");
            }
            let f = run.clone();
            let approved =
                tokio::task::spawn_local(async move { f.call_async::<Table>("request/ready").await.unwrap() });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","id":msg["id"],
                    "result":{"value":{"decision":"allow"}}}),
                    )
                    .await;
                    break;
                }
            }
            assert_eq!(approved.await.unwrap().get::<String>("decision").unwrap(), "allow");
            assert_eq!(std::fs::read_to_string(dir.path().join("side-effect")).unwrap(), "executed");
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":5,"method":"connection.set_capabilities",
            "params":{"capabilities":{"callbacks":false,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 5 {
                    break;
                }
            }
            // a point the policy service does not list has no remote voters
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":6,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 6 {}
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":7,"method":"hooks.subscribe",
                    "params":{"scope":"all","event":"third-party/new-policy","callbacks":["handle"]}}),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 7 {
                    assert_eq!(msg["error"]["code"], json!(codes::INVALID_PARAMS), "{msg}");
                    break;
                }
            }
            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_tools_and_hooks_support_atomic_batches_and_revocation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true}}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}

            // Single-item tools.register rejects invalid parameter schema cleanly AND rejects legacy batch shape.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools.register",
                    "params": {
                        "name": "atomic_t2",
                        "description": "t2",
                        "parameters": {"type": "object", "properties": {"x": {"format": "email"}}},
                        "callbacks": ["execute"]
                    }
                }),
            )
            .await;
            let msg = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 2 {
                    break m;
                }
            };
            let has_tool = |name: &str| -> bool {
                let tools = host.service_table("tools").unwrap();
                let get: Function = tools.get("get").unwrap();
                get.call::<Value>((tools, name)).map(|v| !v.is_nil()).unwrap()
            };
            assert_eq!(msg["error"]["code"], codes::INVALID_PARAMS, "{msg}");
            assert!(!has_tool("atomic_t2"), "atomic_t2 must not be registered on invalid schema");

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 20,
                    "method": "tools.register",
                    "params": { "tools": [{"name": "batch_bad", "description": "b", "parameters": {"type": "object"}}] }
                }),
            )
            .await;
            let batch_err = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 20 {
                    break m;
                }
            };
            assert_eq!(batch_err["error"]["code"], codes::INVALID_PARAMS, "legacy batch must fail: {batch_err}");

            // Valid single-item contract registration returns registration_id, and registrations.release removes it.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools.register",
                    "params": {
                        "name": "rev_t1",
                        "description": "t1",
                        "parameters": {"type": "object"},
                        "callbacks": ["execute"]
                    }
                }),
            )
            .await;
            let reg = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 3 {
                    break m;
                }
            };
            assert!(reg.get("error").is_none(), "{reg}");
            let reg_id_1 = reg["result"]["registration_id"].as_str().unwrap().to_owned();

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 31,
                    "method": "tools.register",
                    "params": {
                        "name": "rev_t2",
                        "description": "t2",
                        "parameters": {"type": "object"},
                        "callbacks": ["execute"]
                    }
                }),
            )
            .await;
            let reg2 = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 31 {
                    break m;
                }
            };
            assert!(reg2.get("error").is_none(), "{reg2}");
            let reg_id_2 = reg2["result"]["registration_id"].as_str().unwrap().to_owned();

            rpc_send(
                &mut cw,
                json!({"jsonrpc": "2.0", "id": 4, "method": "registrations.release", "params": {"registration_id": reg_id_1}}),
            )
            .await;
            let unreg = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 4 {
                    break m;
                }
            };
            assert_eq!(unreg["result"], json!({}), "{unreg}");
            assert!(!has_tool("rev_t1"), "rev_t1 must be removed");
            assert!(has_tool("rev_t2"), "rev_t2 must remain registered");
            rpc_send(
                &mut cw,
                json!({"jsonrpc": "2.0", "id": 40, "method": "tools.unregister", "params": {"ids": [reg_id_2]}}),
            )
            .await;
            let removed_unreg = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 40 {
                    break m;
                }
            };
            assert_eq!(removed_unreg["error"]["code"], codes::METHOD_NOT_FOUND, "{removed_unreg}");

            // Process-only events cannot be subscribed remotely.
            let before_count = host.kernel.listener_count("queue/changed");
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "hooks.subscribe",
                    "params": {"scope": "all", "event": "provider/request", "callbacks": ["handle"]}
                }),
            )
            .await;
            let h_err = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 5 {
                    break m;
                }
            };
            assert_eq!(h_err["error"]["code"], codes::INVALID_PARAMS, "{h_err}");
            assert_eq!(host.kernel.listener_count("queue/changed"), before_count);

            // Valid hooks.subscribe returns registration_id, and registrations.release removes it.
            let mut hook_ids = Vec::new();
            for (idx, event) in ["queue/changed", "status/changed"].iter().enumerate() {
                let id = 6 + idx as i64;
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "method": "hooks.subscribe",
                        "params": {"scope": "all", "event": event, "callbacks": ["handle"]}
                    }),
                )
                .await;
                let h_sub = loop {
                    let m = rpc_read(&mut cr).await;
                    if m["id"] == id {
                        break m;
                    }
                };
                assert!(h_sub.get("error").is_none(), "{h_sub}");
                hook_ids.push(h_sub["result"]["registration_id"].as_str().unwrap().to_string());
            }
            let status_before = host.kernel.listener_count("status/changed") - 1;
            assert_eq!(host.kernel.listener_count("queue/changed"), before_count + 1);

            rpc_send(
                &mut cw,
                json!({"jsonrpc": "2.0", "id": 8, "method": "registrations.release", "params": {"registration_id": hook_ids[0].clone()}}),
            )
            .await;
            let h_unsub = loop {
                let m = rpc_read(&mut cr).await;
                if m["id"] == 8 {
                    break m;
                }
            };
            assert_eq!(h_unsub["result"], json!({}), "{h_unsub}");
            assert_eq!(host.kernel.listener_count("queue/changed"), before_count);
            assert_eq!(host.kernel.listener_count("status/changed"), status_before + 1);

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_hook_argument_projection_rejects_non_json_and_denies_policy_fail_closed() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true},"callback_timeout_ms":100}}),
            )
            .await;
            loop {
                if rpc_read(&mut cr).await["id"] == 1 {
                    break;
                }
            }
            for (idx, event) in ["tool/before", "tool/approval"].iter().enumerate() {
                let id = 2 + idx as i64;
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "method":"hooks.subscribe",
                        "params":{"scope":"all","event":event,"prepend":true,"callbacks":["handle"]}
                    }),
                )
                .await;
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["id"] == id {
                        assert!(msg.get("error").is_none(), "{msg}");
                        break;
                    }
                }
            }

            #[derive(Clone)]
            struct OpaqueUserData;
            impl mlua::UserData for OpaqueUserData {}

            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            lua.globals().set("proj_test_ctx", ctx).unwrap();
            let token = mare_binding::task::Token(tokio_util::sync::CancellationToken::new());
            lua.globals().set("proj_test_token", token).unwrap();
            lua.globals().set("proj_test_opaque_ud", OpaqueUserData).unwrap();

            // 1. Policy hook with a nested function must fail-closed (`deny`) without calling callback.call.
            let vote_fn: Table = lua
                .load(r#"return proj_test_ctx:get("policy"):decide("tool/before", { { name = "bash", cb = function() end }, {} })"#)
                .eval_async()
                .await
                .unwrap();
            assert_eq!(vote_fn.get::<String>("decision").unwrap(), "deny");
            assert!(
                vote_fn.get::<String>("reason").unwrap().contains("argument conversion failed"),
                "{vote_fn:?}"
            );

            // 2. Policy hook with a nested Token userdata or top-level non-Token userdata must fail-closed (`deny`).
            for expr in [
                r#"return proj_test_ctx:get("policy"):decide("tool/before", { { name = "bash", arguments = { tok = proj_test_token } }, {} })"#,
                r#"return proj_test_ctx:get("policy"):decide("tool/before", { { name = "bash", ud = proj_test_opaque_ud }, {} })"#,
            ] {
                let vote_ud: Table = lua.load(expr).eval_async().await.unwrap();
                assert_eq!(vote_ud.get::<String>("decision").unwrap(), "deny");
                assert!(
                    vote_ud.get::<String>("reason").unwrap().contains("argument conversion failed"),
                    "{expr}: {vote_ud:?}"
                );
            }

            // 3. `tool/approval` projects its 3 JSON args and omits the 4th `cancel` Token userdata.
            let approval_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(
                        r#"return proj_test_ctx:get("policy"):decide("tool/approval", { { id = "c1", name = "bash" }, {}, { decision = "ask" }, proj_test_token })"#,
                    )
                    .eval_async::<Table>()
                    .await
                    .unwrap()
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "tool/approval");
                    assert_eq!(msg["params"]["arguments"]["args"].as_array().unwrap().len(), 3, "{msg}");
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":{"decision":"allow"}}}),
                    )
                    .await;
                    break;
                }
            }
            assert_eq!(approval_fut.await.unwrap().get::<String>("decision").unwrap(), "allow");

            // 4. `tool/approval` still fails closed if one of the 3 JSON args has a function/userdata.
            let bad_approval: Table = lua
                .load(
                    r#"return proj_test_ctx:get("policy"):decide("tool/approval", { { id = "c1", bad = function() end }, {}, { decision = "ask" }, proj_test_token })"#,
                )
                .eval_async()
                .await
                .unwrap();
            assert_eq!(bad_approval.get::<String>("decision").unwrap(), "deny");

            // 5. `net/approval` abstaining with `null` does not hardcode-deny in RPC; kernel.policy
            // still denies if nobody approves, and allows if a later listener explicitly approves.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc":"2.0",
                    "id":4,
                    "method":"hooks.subscribe",
                    "params":{"scope":"all","event":"net/approval","prepend":true,"callbacks":["handle"]}
                }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 4 {
                    assert!(msg.get("error").is_none(), "{msg}");
                    break;
                }
            }
            let net_abstain = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"return proj_test_ctx:get("policy"):decide("net/approval", { { host = "example.com", port = 443 } }, { require_explicit_approval = true })"#)
                        .eval_async::<Table>()
                        .await
                        .unwrap()
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "net/approval");
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":null}})).await;
                    break;
                }
            }
            let net_vote = net_abstain.await.unwrap();
            assert_eq!(net_vote.get::<String>("decision").unwrap(), "deny");
            assert_eq!(net_vote.get::<Option<bool>>("unavailable").unwrap(), Some(true));

            lua.load(r#"proj_test_ctx:get("policy"):voter("net/approval", function() return "allow" end)"#)
                .exec_async()
                .await
                .unwrap();
            let net_allow = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"return proj_test_ctx:get("policy"):decide("net/approval", { { host = "example.com", port = 443 } }, { require_explicit_approval = true })"#)
                        .eval_async::<Table>()
                        .await
                        .unwrap()
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "net/approval");
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":null}})).await;
                    break;
                }
            }
            assert_eq!(net_allow.await.unwrap().get::<String>("decision").unwrap(), "allow");

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_hook_fixes_cover_checked_dispatch_null_votes_extra_args_and_request_prepare() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let host = test_host(dir.path()).await;
            let TestRpc { server: server_task, mut cr, mut cw, .. } = TestRpc::connect(host.clone());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"callbacks":true},"callback_timeout_ms":100}}),
            )
            .await;
            while rpc_read(&mut cr).await["id"] != 1 {}

            for (idx, event) in ["tool/before", "tool/approval", "tool/rewrite", "request/prepare"].iter().enumerate() {
                let id = 2 + idx as i64;
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc":"2.0",
                        "id":id,
                        "method":"hooks.subscribe",
                        "params":{"scope":"all","event":event,"prepend":true,"callbacks":["handle"]}
                    }),
                )
                .await;
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg["id"] == id {
                        assert!(msg.get("error").is_none(), "{msg}");
                        break;
                    }
                }
            }

            let lua = host.lua();
            let ctx = host.kernel.host_ctx().unwrap();
            lua.globals().set("fix_test_ctx", ctx).unwrap();
            let token = mare_binding::task::Token(tokio_util::sync::CancellationToken::new());
            lua.globals().set("fix_test_token", token).unwrap();

            // 1a. Non-policy conversion failure aborts emit_checked (`request/prepare`) without calling callback.call.
            let err = lua
                .load(r#"return fix_test_ctx:get("bus"):emit_checked("request/prepare", { bad = function() end })"#)
                .exec_async()
                .await
                .unwrap_err();
            assert!(err.to_string().contains("argument conversion failed"), "{err}");

            // 1b. Non-policy conversion failure aborts rewrite (`tool/rewrite`) instead of returning original args.
            let (rw_code, rw_err): (Option<String>, Option<String>) = lua
                .load(
                    r#"local ok, r = pcall(function() return fix_test_ctx:get("policy"):rewrite("tool/rewrite", { name = "bash", arguments = { bad = function() end } }) end); assert(not ok); return type(r) == "table" and r.code or nil, type(r) == "table" and r.message or tostring(r)"#,
                )
                .eval_async()
                .await
                .unwrap();
            assert_eq!(rw_code.as_deref(), Some("policy.rewrite_refused"), "{rw_err:?}");
            assert!(rw_err.as_deref().unwrap_or("").contains("argument conversion failed"), "{rw_err:?}");

            // 4. `tool/approval` rejects extra arguments beyond 4th cancel token.
            let extra_approval: Table = lua
                .load(
                    r#"return fix_test_ctx:get("policy"):decide("tool/approval", { { id = "c1" }, {}, { decision = "ask" }, fix_test_token, { extra = "hidden" } }, { require_explicit_approval = true })"#,
                )
                .eval_async()
                .await
                .unwrap();
            assert_eq!(extra_approval.get::<String>("decision").unwrap(), "deny");
            assert!(
                extra_approval.get::<String>("reason").unwrap().contains("at most 4 arguments"),
                "{extra_approval:?}"
            );

            // 5a. `request/prepare` with marked empty array `mare.json.array()` sends `[]`, not `{}`.
            let prep_arr_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"return fix_test_ctx:get("bus"):emit_checked("request/prepare", mare.json.array())"#)
                        .exec_async()
                        .await
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "request/prepare");
                    assert_eq!(msg["params"]["arguments"]["args"], json!([[]]), "{msg}");
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":null}})).await;
                    break;
                }
            }
            prep_arr_fut.await.unwrap().unwrap();

            // 5b. `request/prepare` with unsupported root metatable is rejected.
            let prep_mt_err = lua
                .load(r#"return fix_test_ctx:get("bus"):emit_checked("request/prepare", setmetatable({}, { __index = {} }))"#)
                .exec_async()
                .await
                .unwrap_err();
            assert!(prep_mt_err.to_string().contains("unsupported metatable"), "{prep_mt_err}");

            // 5c. `request/prepare` strips only `cancel` Task Token and forwards remaining fields strictly.
            let prep_tok_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(
                        r#"return fix_test_ctx:get("bus"):emit_checked("request/prepare", { model = "x", cancel = fix_test_token })"#,
                    )
                    .exec_async()
                    .await
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "request/prepare");
                    assert_eq!(msg["params"]["arguments"]["args"], json!([{"model": "x"}]), "{msg}");
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":null}})).await;
                    break;
                }
            }
            prep_tok_fut.await.unwrap().unwrap();

            // 6. Invalid RPC policy answer with `arguments: null` is NOT stripped and must be denied.
            let null_field_fut = tokio::task::spawn_local({
                let lua = lua.clone();
                async move {
                    lua.load(r#"return fix_test_ctx:get("policy"):decide("tool/before", { { name = "bash" }, {} })"#)
                        .eval_async::<Table>()
                        .await
                        .unwrap()
                }
            });
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" {
                    assert_eq!(msg["params"]["arguments"]["event"], "tool/before");
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":{"decision":"allow","arguments":null}}}),
                    )
                    .await;
                    break;
                }
            }
            let null_field_vote = null_field_fut.await.unwrap();
            assert_eq!(null_field_vote.get::<String>("decision").unwrap(), "deny");
            assert!(
                null_field_vote.get::<String>("reason").unwrap().contains("arguments"),
                "{null_field_vote:?}"
            );

            drop(cw);
            server_task.abort();
            let _ = server_task.await;
            host.shutdown().await;
        })
        .await;
}
