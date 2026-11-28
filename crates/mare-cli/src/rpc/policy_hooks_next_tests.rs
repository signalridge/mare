use super::*;

/// Send `method` with `id` and read until its reply (other frames are dropped).
async fn request(
    cr: &mut Reader,
    cw: &mut Writer,
    id: &str,
    method: &str,
    params: serde_json::Value,
) -> serde_json::Value {
    rpc_send(cw, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
    loop {
        let msg = read_until_eof(cr).await.expect("EOF before the reply");
        if msg["id"] == id {
            return msg;
        }
    }
}

async fn next_hook_call(cr: &mut Reader) -> J {
    loop {
        let msg = rpc_read(cr).await;
        if msg["method"] == methods::CALLBACK_CALL {
            return msg;
        }
    }
}

/// `llm.register_provider` is `primary`-only (never grantable to a peer); the primary connection
/// registers and replaces a provider under its own fiber without retaining the table across disconnect.
async fn granted_peer_provider_is_owned_by_connection(replace_first: bool) {
    let _host_guard = HOST_LOCK.lock().await;
    let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
    let sid = host.kernel.event_log().session_identity().0.expect("session_id");
    let allowed = request(
        &mut cr,
        &mut cw,
        "allow-providers",
        methods::OPTIONS_SET,
        json!({"session_id": sid, "values": {"peer": {"allow": ["*", "llm.register_provider"]}}}),
    )
    .await;
    assert!(allowed["error"].is_null(), "{allowed}");
    let (peer, peer_server) = tokio::io::duplex(1 << 20);
    let (sr, sw) = tokio::io::split(peer_server);
    let peer_task = tokio::task::spawn_local(serve_peer(host.clone(), sr, sw, None));
    let (pr, mut pw) = tokio::io::split(peer);
    let mut pr = BufReader::new(pr);
    let init = request(
        &mut pr,
        &mut pw,
        "init",
        methods::INITIALIZE,
        json!({"protocol_version": proto::PROTOCOL_VERSION, "capabilities": {"callbacks": true}}),
    )
    .await;
    assert!(init["error"].is_null(), "{init}");
    let peer_denied = request(
        &mut pr,
        &mut pw,
        "peer-register",
        "llm.register_provider",
        json!({"provider":"hostprov", "default_model":"m1", "replace":replace_first, "callbacks":["stream"],
            "models":[{"model_id":"m1","name":"Host Model 1","context_window":128000,"max_output":4096,"input":["text","image"]}]}),
    )
    .await;
    assert_eq!(
        peer_denied["error"]["data"]["code"], "denied",
        "llm.register_provider must be primary-only even when listed in peer.allow: {peer_denied}"
    );
    drop((pr, pw));
    tokio::time::timeout(Duration::from_secs(10), peer_task).await.unwrap().unwrap();

    let p_init = request(
        &mut cr,
        &mut cw,
        "init-providers",
        methods::CONNECTION_SET_CAPABILITIES,
        json!({"capabilities": {"callbacks": true, "ui": false, "approvals": false, "elicitation": false}}),
    )
    .await;
    assert!(p_init["error"].is_null(), "{p_init}");

    // Old providers.register endpoint and legacy field names (name/id/vision) must be rejected cleanly.
    let old_ep = request(
        &mut cr,
        &mut cw,
        "old-prov-ep",
        "providers.register",
        json!({"name":"hostprov","models":[{"id":"m1"}]}),
    )
    .await;
    assert_eq!(old_ep["error"]["code"], codes::METHOD_NOT_FOUND, "{old_ep}");

    let legacy_fields = request(
        &mut cr,
        &mut cw,
        "legacy-prov-fields",
        "llm.register_provider",
        json!({"name":"hostprov","models":[{"id":"m1","vision":true}],"callbacks":["stream"]}),
    )
    .await;
    assert_eq!(legacy_fields["error"]["data"]["code"], "invalid_arguments", "{legacy_fields}");

    host.lua().load("WEAK_PROVIDERS = setmetatable({}, {__mode='v'})").exec().unwrap();
    for (index, model) in ["m1", "m2"].into_iter().enumerate() {
        let reg = request(
            &mut cr,
            &mut cw,
            if index == 0 { "register" } else { "replace" },
            "llm.register_provider",
            json!({"provider":"hostprov", "default_model":model, "replace":replace_first || index != 0, "callbacks":["stream"],
                "models":[{"model_id":model,"name":"Host Model 1","context_window":128000,"max_output":4096,"input":["text","image"]}]}),
        )
        .await;
        assert!(reg["error"].is_null(), "primary provider registration: {reg}");
        assert!(reg["result"]["registration_id"].as_str().is_some(), "{reg}");
        let models = request(&mut cr, &mut cw, "models", "llm.list_models", json!({})).await;
        assert!(
            models["result"]["data"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["ref"] == format!("hostprov/{model}") && m["model_id"] == model && m["images"] == true),
            "provider model missing or mismatched: {models}"
        );
        host.lua().load(format!("WEAK_PROVIDERS[{}] = rb_llm:provider('hostprov')", index + 1)).exec().unwrap();
    }
    drop((cr, cw));
    tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap();
    host.lua().gc_collect().unwrap();
    host.lua().gc_collect().unwrap();
    let retained: bool = host.lua().load("return next(WEAK_PROVIDERS) ~= nil").eval().unwrap();
    assert!(!retained, "a disconnected primary's provider table is still retained");
    host.shutdown().await;
}

#[tokio::test]
async fn rpc_granted_peer_registers_and_replaces_a_provider_without_retaining_it() {
    tokio::task::LocalSet::new().run_until(granted_peer_provider_is_owned_by_connection(false)).await;
}

#[tokio::test]
async fn rpc_granted_peer_can_register_a_provider_with_replace_true() {
    tokio::task::LocalSet::new().run_until(granted_peer_provider_is_owned_by_connection(true)).await;
}

#[tokio::test]
async fn rpc_llm_register_provider_streams_deltas_cancels_and_replaces() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            let sid = host.kernel.event_log().session_identity().0.unwrap();
            let caps = request(
                &mut cr,
                &mut cw,
                "caps",
                methods::CONNECTION_SET_CAPABILITIES,
                json!({"capabilities": {"callbacks": true, "ui": false, "approvals": false, "elicitation": false}}),
            )
            .await;
            assert!(caps["error"].is_null(), "{caps}");

            let reg = request(
                &mut cr,
                &mut cw,
                "reg-stream",
                "llm.register_provider",
                json!({
                    "provider": "streamprov",
                    "default_model": "s1",
                    "models": [{ "model_id": "s1", "name": "Stream 1", "context_window": 128000, "max_output": 2048, "input": ["text"] }],
                    "callbacks": ["stream"]
                }),
            )
            .await;
            assert!(reg["error"].is_null(), "{reg}");
            let reg_id = reg["result"]["registration_id"].as_str().unwrap().to_owned();

            // Select custom provider's model via contract `llm.set_model`.
            let sel = request(
                &mut cr,
                &mut cw,
                "set-model",
                "llm.set_model",
                json!({ "session_id": sid, "ref": "streamprov/s1" }),
            )
            .await;
            assert!(sel["error"].is_null(), "{sel}");

            // Invalid replace preserves original provider intact.
            let bad_replace = request(
                &mut cr,
                &mut cw,
                "bad-replace",
                "llm.register_provider",
                json!({
                    "provider": "streamprov",
                    "replace": true,
                    "models": [{ "model_id": "" }],
                    "callbacks": ["stream"]
                }),
            )
            .await;
            assert!(!bad_replace["error"].is_null(), "{bad_replace}");

            // Subscribe events & start prompt -> verify callback.call + ordered callback.update with tool_call_id + cancel.
            let sub_ev = request(&mut cr, &mut cw, "sub-ev", methods::EVENTS_SUBSCRIBE, json!({ "session_id": sid })).await;
            assert!(sub_ev["error"].is_null(), "{sub_ev}");

            let p1 = request(&mut cr, &mut cw, "p1", "agent.prompt", json!({ "session_id": sid, "input": "hello" })).await;
            assert!(p1["error"].is_null(), "{p1}");
            let turn_id = p1["result"]["turn_id"].as_str().unwrap().to_owned();

            let mut saw_text_delta = false;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" && msg["params"]["callback"] == "handle" {
                    let arg0 = msg["params"]["arguments"]["args"][0].clone();
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":arg0}})).await;
                } else if msg["method"] == "callback.call" && msg["params"]["callback"] == "stream" {
                    assert_eq!(msg["params"]["registration_id"], reg_id, "{msg}");
                    assert_eq!(msg["params"]["session_id"], sid, "{msg}");
                    assert_eq!(msg["params"]["turn_id"], turn_id, "{msg}");
                    assert!(msg["params"]["arguments"]["request"]["messages"].is_array(), "{msg}");
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","method":"callback.update","params":{"id":msg["id"],"partial":{"type":"text_delta","delta":"hello "}}}),
                    )
                    .await;
                    rpc_send(
                        &mut cw,
                        json!({"jsonrpc":"2.0","id":msg["id"],"result":{"role":"assistant","content":[{"type":"text","text":"hello world"}]}}),
                    )
                    .await;
                } else if msg["method"] == "event" && msg["params"]["type"] == "item/delta" {
                    if msg["params"]["data"]["delta"] == "hello " {
                        saw_text_delta = true;
                    }
                } else if msg["method"] == "event" && msg["params"]["type"] == "turn/failed" {
                    panic!("turn failed unexpectedly: {msg}");
                } else if msg["method"] == "stream.failed" {
                    panic!("stream failed unexpectedly: {msg}");
                } else if msg["method"] == "event" && msg["params"]["type"] == "turn/completed" {
                    break;
                }
            }
            assert!(saw_text_delta, "must receive live streamed text delta before turn/completed");

            // Cancelling a second in-flight turn cancels its stream callback via `$/cancel_request`.
            let p2 = request(&mut cr, &mut cw, "p2", "agent.prompt", json!({ "session_id": sid, "input": "hang" })).await;
            assert!(p2["error"].is_null(), "{p2}");
            let cb2_id = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" && msg["params"]["callback"] == "handle" {
                    let arg0 = msg["params"]["arguments"]["args"][0].clone();
                    rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":msg["id"],"result":{"value":arg0}})).await;
                } else if msg["method"] == "callback.call" && msg["params"]["callback"] == "stream" {
                    break msg["id"].clone();
                }
            };
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":"cancel-p2","method":"agent.cancel","params":{"session_id":sid}})).await;
            let (mut saw_cancel_req, mut saw_cancel_reply) = (false, false);
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "$/cancel_request" && msg["params"]["id"] == cb2_id {
                    saw_cancel_req = true;
                } else if msg["id"] == "cancel-p2" {
                    assert!(msg["error"].is_null(), "{msg}");
                    saw_cancel_reply = true;
                }
                if saw_cancel_req && saw_cancel_reply {
                    break;
                }
            }

            drop((cr, cw));
            tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_primary_provider_disconnect_does_not_retain_its_table() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            let init = request(
                &mut cr,
                &mut cw,
                "init-providers",
                methods::CONNECTION_SET_CAPABILITIES,
                json!({"capabilities": {"callbacks": true, "ui": false, "approvals": false, "elicitation": false}}),
            )
            .await;
            assert!(init["error"].is_null(), "{init}");
            let effect_count = || -> i64 {
                host.lua().load("local rpc = rb_ctx:get('rpc'); for _, c in pairs(rpc.contexts) do return #c.fiber._disposables:values() end").eval().unwrap()
            };
            let before = effect_count();
            let reg = request(&mut cr, &mut cw, "register", "llm.register_provider",
                json!({"provider":"hostprov", "default_model":"m1", "callbacks":["stream"],
                    "models":[{"model_id":"m1", "name":"Host Model 1",
                        "context_window":128000, "max_output":4096}]})).await;
            assert!(reg["error"].is_null(), "{reg}");
            host.lua().load("WEAK_PROVIDER = setmetatable({rb_llm:provider('hostprov')}, {__mode='v'}); assert(WEAK_PROVIDER[1] ~= nil, 'provider was not captured')").exec().unwrap();
            assert_eq!(effect_count(), before + 1, "provider effect does not belong to the connection");
            drop((cr, cw));
            tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap();
            host.lua().gc_collect().unwrap();
            host.lua().gc_collect().unwrap();
            let retained: bool = host.lua().load("return WEAK_PROVIDER[1] ~= nil").eval().unwrap();
            assert!(!retained, "a disconnected primary's provider table is still retained");
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_revoked_voter_reply_stays_denied_after_capability_is_restored() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            let sub = request(
                &mut cr,
                &mut cw,
                "sub-voter",
                "hooks.subscribe",
                json!({"scope":"all", "event":"tool/before", "callbacks":["handle"]}),
            )
            .await;
            assert!(sub["error"].is_null(), "{sub}");
            let lua = host.lua().clone();
            let vote = tokio::task::spawn_local(async move {
                lua.load("return rb_ctx:get('policy'):decide('tool/before', { {} })").eval_async::<Table>().await
            });
            let call = next_hook_call(&mut cr).await;
            assert_eq!(call["params"]["arguments"]["mode"], "policy", "{call}");
            for (id, callbacks) in [("revoke-voter", false), ("restore-voter", true)] {
                let response = request(
                    &mut cr,
                    &mut cw,
                    id,
                    methods::CONNECTION_SET_CAPABILITIES,
                    json!({"capabilities":{"callbacks":callbacks,"ui":false,"approvals":false,"elicitation":false}}),
                )
                .await;
                assert!(response["error"].is_null(), "{response}");
            }
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0", "id":call["id"],
                "result":{"value":{"decision":"allow"}}}),
            )
            .await;
            let decision = vote.await.unwrap().unwrap();
            assert_eq!(decision.get::<String>("decision").unwrap(), "deny", "revoked voter reply was accepted");
            finish(host, server).await;
        })
        .await;
}

#[tokio::test]
async fn rpc_cancelled_voter_does_not_hang_subsequent_calls() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            let sub = request(
                &mut cr,
                &mut cw,
                "sub-voter",
                "hooks.subscribe",
                json!({"scope":"all", "event":"tool/before", "callbacks":["handle"]}),
            )
            .await;
            assert!(sub["error"].is_null(), "{sub}");
            let lua = host.lua().clone();
            let vote = tokio::task::spawn_local(async move {
                lua.load("return rb_ctx:get('policy'):decide('tool/before', { {} })").eval_async::<Table>().await
            });
            let call = next_hook_call(&mut cr).await;
            assert_eq!(call["params"]["arguments"]["mode"], "policy", "{call}");
            vote.abort();
            let _ = vote.await;
            let inflight_empty: bool =
                host.lua().load("return next(rb_ctx:get('hooks').inflight) == nil").eval().unwrap();
            assert!(inflight_empty, "cancelled voter left an entry in hooks.inflight");

            let lua2 = host.lua().clone();
            let second_vote = tokio::task::spawn_local(async move {
                lua2.load("return rb_ctx:get('policy'):decide('tool/before', { {} })").eval_async::<Table>().await
            });
            let second_call = next_hook_call(&mut cr).await;
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0", "id":second_call["id"], "result":{"value":{"decision":"allow"}}}),
            )
            .await;
            let second_decision = second_vote.await.unwrap().unwrap();
            assert_eq!(second_decision.get::<String>("decision").unwrap(), "allow");
            finish(host, server).await;
        })
        .await;
}

async fn primary_waterfall_revoked_in_flight() {
    let _host_guard = HOST_LOCK.lock().await;
    let Rig { host, server, mut cr, mut cw, _dir } = rig_with_peer_hooks(None, true).await;
    let plugin_path = _dir.path().join("wfprobe.lua");
    std::fs::write(
        &plugin_path,
        r#"return {
    name = "wfprobe-plugin",
    contracts = {
        wfprobe = {
            methods = {},
            events = {
                pass = {
                    doc = "Public contract waterfall event.",
                    mode = "waterfall",
                    expose = "both",
                    delivery = "required",
                    ignorable = true,
                    data = { type = "object", properties = { model = { type = "string" } } },
                    example = { model = "base" },
                },
            },
        },
    },
}"#,
    )
    .unwrap();
    let def = host
        .kernel
        .load_def(&mare_kernel::PluginSource {
            name_hint: "wfprobe-plugin".into(),
            path: Some(plugin_path),
            code: None,
            origin: mare_kernel::Origin::User,
        })
        .unwrap();
    let (_, refused) = host.kernel.merge_defs(vec![def]);
    assert!(refused.is_empty(), "{refused:?}");
    let (peer, peer_server) = tokio::io::duplex(1 << 20);
    let (sr, sw) = tokio::io::split(peer_server);
    let peer_task = tokio::task::spawn_local(serve_peer(host.clone(), sr, sw, None));
    let (pr, mut pw) = tokio::io::split(peer);
    let mut pr = BufReader::new(pr);
    let p_init = request(
        &mut pr,
        &mut pw,
        "init",
        methods::INITIALIZE,
        json!({"protocol_version": proto::PROTOCOL_VERSION, "capabilities":{"callbacks":true}}),
    )
    .await;
    assert!(p_init["error"].is_null(), "{p_init}");
    let p_sub = request(
        &mut pr,
        &mut pw,
        "sub",
        "hooks.subscribe",
        json!({"event":"wfprobe/pass", "prepend":true, "callbacks":["handle"]}),
    )
    .await;
    assert_eq!(p_sub["error"]["data"]["code"], "denied", "hooks.subscribe must deny peers: {p_sub}");
    drop((pr, pw));
    tokio::time::timeout(Duration::from_secs(10), peer_task).await.unwrap().unwrap();

    // Primary's in-flight waterfall reply stays denied if callbacks capability is revoked and restored mid-flight.
    let sub = request(
        &mut cr,
        &mut cw,
        "sub",
        "hooks.subscribe",
        json!({"scope":"all", "event":"wfprobe/pass", "prepend":true, "callbacks":["handle"]}),
    )
    .await;
    assert!(sub["error"].is_null(), "{sub}");
    let lua = host.lua().clone();
    let waterfall = tokio::task::spawn_local(async move {
        lua.load("return rb_ctx:get('bus'):waterfall_checked('wfprobe/pass', { model = 'base' })")
            .eval_async::<mlua::Value>()
            .await
    });
    let call = next_hook_call(&mut cr).await;
    for (id, callbacks) in [("revoke", false), ("restore", true)] {
        let response = request(
            &mut cr,
            &mut cw,
            id,
            methods::CONNECTION_SET_CAPABILITIES,
            json!({"capabilities":{"callbacks":callbacks,"ui":false,"approvals":false,"elicitation":false}}),
        )
        .await;
        assert!(response["error"].is_null(), "{response}");
    }
    rpc_send(
        &mut cw,
        json!({"jsonrpc":"2.0", "id":call["id"],
        "result":{"value":{"model":"revoked-waterfall-rewrite"}}}),
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(10), waterfall).await.unwrap().unwrap();
    assert!(result.is_err(), "the revoked waterfall reply was accepted: {result:?}");
    finish(host, server).await;
}

#[tokio::test]
async fn rpc_peer_waterfall_reply_stays_revoked_after_the_grant_is_restored() {
    tokio::task::LocalSet::new().run_until(primary_waterfall_revoked_in_flight()).await;
}

#[tokio::test]
async fn rpc_peer_waterfall_reply_stays_revoked_after_public_events_withdrawn_in_flight() {
    tokio::task::LocalSet::new().run_until(primary_waterfall_revoked_in_flight()).await;
}
/// a model tool's sandboxed work
/// (inside a real `mare.exec.run`, which binds the thread's exec_ctx) cannot register a policy
/// voter or rewriter, neither with the policy service nor through the `hooks` service;
/// each is refused as a permission refusal (`denied`), not by some other error.
#[tokio::test]
async fn restricted_context_cannot_register_a_voter_directly_or_through_the_rpc_service() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, _dir, .. } = rig(None).await;
            let lua = host.lua();
            lua.globals().set("rs_policy", host.service_table("policy").unwrap()).unwrap();
            lua.globals().set("rs_hooks", host.service_table("hooks").unwrap()).unwrap();
            let refused: Vec<String> = lua
                .load(
                    r#"
                    local out = {}
                    local sandboxed = { sandbox = { level = "read-only", network = true } }
                    assert(mare.exec.run(sandboxed, function()
                        for _, try in ipairs({
                            function() return rs_policy:voter("tool/before", function() return "allow" end) end,
                            function() return rs_policy:rewriter("tool/rewrite", function() end) end,
                            function() return rs_hooks:subscribe("tool/before", { handle = function() end }) end,
                            function() return rs_hooks:subscribe("tool/rewrite", { handle = function() end }) end,
                        }) do
                            local ok, err = pcall(try)
                            out[#out + 1] = ok and "registered" or tostring(err)
                        end
                        return true
                    end))
                    return out
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert_eq!(refused.len(), 4, "{refused:?}");
            for (i, why) in refused.iter().enumerate() {
                assert!(
                    why.starts_with("denied:") && why.contains("sandboxed"),
                    "#{i}: not the permission refusal: {why}"
                );
            }
            finish(host, server).await;
        })
        .await;
}

#[tokio::test]
async fn rpc_callback_revocation_cannot_turn_policy_allow() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let Rig { host, server, mut cr, mut cw, _dir } = rig(None).await;
            let voters_before = host.kernel.voter_count("tool/before");
            let sub = request(
                &mut cr,
                &mut cw,
                "sub-policy",
                "hooks.subscribe",
                json!({"scope": "all", "event": "tool/before", "callbacks": ["handle"]}),
            )
            .await;
            assert!(sub["error"].is_null(), "{sub}");
            assert_eq!(host.kernel.voter_count("tool/before"), voters_before + 1);

            let revoke = request(
                &mut cr,
                &mut cw,
                "revoke-cb",
                methods::CONNECTION_SET_CAPABILITIES,
                json!({"capabilities": {"callbacks": false, "ui": false, "approvals": false, "elicitation": false}}),
            )
            .await;
            assert!(revoke["error"].is_null(), "{revoke}");
            // Voter remains active to enforce fail-closed deny rather than dropping to allow/abstain.
            assert_eq!(host.kernel.voter_count("tool/before"), voters_before + 1);

            let decision: Table = host
                .lua()
                .load("return rb_ctx:get('policy'):decide('tool/before', { {} })")
                .eval_async()
                .await
                .unwrap();
            assert_eq!(
                decision.get::<String>("decision").unwrap(),
                "deny",
                "revoked callback must fail closed to deny, never allow/abstain"
            );

            // Restoring callbacks re-enables the existing voter cleanly with no duplicate registration.
            let restore = request(
                &mut cr,
                &mut cw,
                "restore-cb",
                methods::CONNECTION_SET_CAPABILITIES,
                json!({"capabilities": {"callbacks": true, "ui": false, "approvals": false, "elicitation": false}}),
            )
            .await;
            assert!(restore["error"].is_null(), "{restore}");
            assert_eq!(host.kernel.voter_count("tool/before"), voters_before + 1);

            let lua = host.lua().clone();
            let allowed_vote = tokio::task::spawn_local(async move {
                lua.load("return rb_ctx:get('policy'):decide('tool/before', { {} })").eval_async::<Table>().await
            });
            let call = next_hook_call(&mut cr).await;
            rpc_send(&mut cw, json!({"jsonrpc": "2.0", "id": call["id"], "result": {"value": {"decision": "allow"}}}))
                .await;
            let allowed = allowed_vote.await.unwrap().unwrap();
            assert_eq!(allowed.get::<String>("decision").unwrap(), "allow");
            finish(host, server).await;
        })
        .await;
}
