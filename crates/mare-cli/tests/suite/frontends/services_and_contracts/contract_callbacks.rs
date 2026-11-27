use super::*;

/// D11:
/// contract callbacks declare their wire parameter/result schemas, use one generic `callback.call`
/// reverse request carrying `{registration_id, callback, session_id?, arguments}`, validate both
/// ways, refuse missing `capabilities.callbacks`, and cancel/end cleanly when released, withdrawn,
/// or disconnected.
#[test]
fn contract_callbacks_use_callback_call_and_end_with_registration() {
    use std::sync::{Arc, Mutex};
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { peer = { allow = { 'cb-demo.watch' } } }").unwrap();
    std::fs::write(
        home.join("config/plugins/cb_demo.lua"),
        r#"return {
    name = "cb-demo-plugin",
    provide = { "cb-demo" },
    contracts = {
        ["cb-demo"] = {
            methods = {
                watch = {
                    doc = "register a remote callback",
                    expose = "both",
                    access = "peer",
                    registration = true,
                    params = {
                        type = "object",
                        properties = { prefix = { type = "string" } },
                        required = { "prefix" },
                        additionalProperties = false,
                    },
                    order = { "prefix", "..." },
                    callbacks = {
                        greet = {
                            type = "fun(name: string): string",
                            params = {
                                type = "object",
                                properties = { name = { type = "string" } },
                                required = { "name" },
                                additionalProperties = false,
                            },
                            order = { "name" },
                            result = { type = "string" },
                        },
                        done = {
                            type = "fun()",
                            optional = true,
                        },
                    },
                },
                fire = {
                    doc = "invoke the active callback",
                    expose = "both",
                    params = {
                        type = "object",
                        properties = { name = { type = "string" }, call_id = { type = "string" } },
                        required = { "name" },
                        additionalProperties = false,
                    },
                    order = { "name", "call_id" },
                    result = { type = "object" },
                },
            },
        },
    },
    apply = function(ctx)
        local active = nil
        local S = require("cordis").class("CbDemo", mare.Service)
        function S:constructor(c)
            S.super.constructor(self, c, "cb-demo")
        end
        function S:watch(prefix, opts)
            active = { prefix = prefix, greet = opts.greet, has_done = opts.done ~= nil }
            return function()
                active = nil
            end
        end
        function S:fire(name, call_id)
            if not active then
                return { ok = false, err = "none" }
            end
            local function run()
                return active.greet(name)
            end
            local ok, value
            if call_id then
                ok, value = pcall(mare.exec.run, { tool = "cb-probe", tool_call_id = call_id }, run)
            else
                ok, value = pcall(run)
            end
            if ok then
                return { ok = true, out = active.prefix .. ":" .. value, has_done = active.has_done }
            end
            return { ok = false, code = type(value) == "table" and value.code or tostring(value) }
        end
        ctx:plugin(S)
    end,
}
"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(&faux, "[]").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--peer", "--model", "faux/echo"], &env, root.path());
    let wait_until = |c: &mut crate::common::rpc::RpcClient,
                      req_id: u64,
                      on_msg: &mut dyn FnMut(&mut crate::common::rpc::RpcClient, &Value) -> bool|
     -> Value {
        loop {
            let msg = c.read();
            if msg.get("id").and_then(Value::as_u64) == Some(req_id) {
                return msg;
            }
            if !on_msg(c, &msg) {
                c.dispatch(msg);
            }
        }
    };
    let init0 = client.call("initialize", json!({"protocol_version": "3.0"}));
    assert_eq!(init0["capabilities"]["callbacks"]["callbacks"], json!(false), "{init0}");

    // Without capabilities.callbacks, registering a callback is refused with `unsupported`.
    let no_cap = client.call_err("cb-demo.watch", json!({ "prefix": "hi", "callbacks": ["greet"] }));
    assert_eq!(no_cap["data"]["code"], json!("unsupported"), "{no_cap}");

    let session = init0["session"]["session_id"].as_str().expect("session_id").to_string();
    let init = client.call(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": true, "ui": false, "approvals": false, "elicitation": false } }),
    );
    assert_eq!(init["capabilities"]["callbacks"], json!(true), "{init}");

    // `services.list` exposes `callbacks` parameter & schema catalog .
    let listed = client.call("services.list", json!({}));
    let watch_spec =
        listed["data"].as_array().unwrap().iter().find(|s| s["name"] == "cb-demo").unwrap()["methods"]["watch"].clone();
    assert_eq!(watch_spec["callbacks"]["greet"]["order"], json!(["name"]), "{watch_spec}");

    // Missing required callback is rejected with `invalid_arguments`.
    let missing_cb = client.call_err("cb-demo.watch", json!({ "prefix": "hi" }));
    assert_eq!(missing_cb["data"]["code"], json!("invalid_arguments"), "{missing_cb}");

    // Valid registration returns registration_id; reverse calls send `callback.call`.
    let reg_id = client.call("cb-demo.watch", json!({ "prefix": "hi", "callbacks": ["greet"] }))["registration_id"]
        .as_str()
        .unwrap()
        .to_string();

    let seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let seen_c = seen.clone();
    let fire_req = client.request("cb-demo.fire", json!({ "name": "world", "call_id": "call-cb-1" }));
    let cb_result = wait_until(&mut client, fire_req, &mut |c, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            seen_c.lock().unwrap().push(msg.clone());
            c.send(json!({ "id": msg["id"], "result": "world!" }));
            return true;
        }
        false
    });
    assert_eq!(cb_result["result"], json!({ "ok": true, "out": "hi:world!", "has_done": false }), "{cb_result}");
    {
        let recorded = seen.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0]["params"]["registration_id"], json!(reg_id));
        assert_eq!(recorded[0]["params"]["callback"], json!("greet"));
        assert_eq!(recorded[0]["params"]["arguments"], json!({ "name": "world" }));
        assert_eq!(recorded[0]["params"]["session_id"], json!(session));
        assert_eq!(recorded[0]["params"]["tool_call_id"], json!("call-cb-1"));
    }

    // Wrong callback result type surfaces `invalid_result` to the provider.
    let bad_req = client.request("cb-demo.fire", json!({ "name": "world" }));
    let bad_res = wait_until(&mut client, bad_req, &mut |c, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            c.send(json!({ "id": msg["id"], "result": 123 }));
            return true;
        }
        false
    });
    assert_eq!(bad_res["result"], json!({ "ok": false, "code": "invalid_result" }));

    // Releasing an active registration cancels an in-flight `callback.call` (`$/cancel_request`).
    let slow_req = client.request("cb-demo.fire", json!({ "name": "pending" }));
    let mut pending_cb = None;
    while pending_cb.is_none() {
        let msg = client.read();
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            pending_cb = Some(msg);
        } else {
            client.dispatch(msg);
        }
    }
    let pending_cb = pending_cb.unwrap();
    let rel_req = client.request("registrations.release", json!({ "registration_id": reg_id }));
    let mut cancelled_cb = false;
    let mut rel_res = None;
    let mut slow_res = None;
    while rel_res.is_none() || slow_res.is_none() {
        let msg = client.read();
        if msg.get("method").and_then(Value::as_str) == Some("$/cancel_request")
            && msg["params"]["id"] == pending_cb["id"]
        {
            cancelled_cb = true;
        } else if msg.get("id").and_then(Value::as_u64) == Some(rel_req) {
            rel_res = Some(msg);
        } else if msg.get("id").and_then(Value::as_u64) == Some(slow_req) {
            slow_res = Some(msg);
        } else {
            client.dispatch(msg);
        }
    }
    assert!(cancelled_cb, "registrations.release must cancel in-flight callback.call");
    assert_eq!(rel_res.unwrap()["result"], json!({}));
    assert_eq!(slow_res.unwrap()["result"], json!({ "ok": false, "code": "cancelled" }));
    assert_eq!(client.call("cb-demo.fire", json!({ "name": "stale" })), json!({ "ok": false, "err": "none" }));

    // Disconnecting a client while its `callback.call` is in flight unblocks immediately without hanging.
    let peer_fire_req;
    {
        let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
        let peer_init = peer.call(
            "initialize",
            json!({ "protocol_version": init0["protocol_version"], "capabilities": { "callbacks": true } }),
        );
        assert_eq!(peer_init["result"]["capabilities"]["callbacks"]["callbacks"], json!(true), "{peer_init}");
        let peer_sid = peer_init["result"]["session"]["session_id"].as_str().expect("peer session_id").to_string();
        let peer_reg =
            peer.call("cb-demo.watch", json!({ "session_id": peer_sid, "prefix": "peer", "callbacks": ["greet"] }));
        assert!(peer_reg["result"]["registration_id"].is_string(), "{peer_reg}");
        peer_fire_req = client.request("cb-demo.fire", json!({ "name": "drop" }));
        loop {
            let msg = peer.read();
            if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
                break;
            }
        }
    }
    let disconnect_res = wait_until(&mut client, peer_fire_req, &mut |_, _| false);
    assert!(
        matches!(disconnect_res["result"]["code"].as_str(), Some("disconnected" | "cancelled")),
        "disconnecting client must unblock pending callback.call: {disconnect_res}"
    );
    let after_drop = {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let res = client.call("cb-demo.fire", json!({ "name": "after-drop" }));
            if res == json!({ "ok": false, "err": "none" }) || std::time::Instant::now() >= deadline {
                break res;
            }
            // sleep-ok: poll interval waiting for peer connection disposal after socket close (bounded 2s)
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    assert_eq!(after_drop, json!({ "ok": false, "err": "none" }));

    // Withdrawing `capabilities.callbacks = false` cancels pending `callback.call` and refuses new callback registrations.
    let reg2 = client.call("cb-demo.watch", json!({ "prefix": "w2", "callbacks": ["greet"] }))["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    let w2_req = client.request("cb-demo.fire", json!({ "name": "withdraw" }));
    let _w2_cb = loop {
        let msg = client.read();
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            break msg;
        }
        client.dispatch(msg);
    };
    let withdraw_init = client.request(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": false, "ui": false, "approvals": false, "elicitation": false } }),
    );
    let mut withdraw_res = None;
    let w2_res = wait_until(&mut client, w2_req, &mut |_, msg| {
        if msg.get("id").and_then(Value::as_u64) == Some(withdraw_init) {
            withdraw_res = Some(msg.clone());
            return true;
        }
        false
    });
    let withdraw_res = withdraw_res.unwrap_or_else(|| wait_until(&mut client, withdraw_init, &mut |_, _| false));
    assert_eq!(withdraw_res["result"]["capabilities"]["callbacks"], json!(false));
    assert_eq!(w2_res["result"], json!({ "ok": false, "code": "unsupported" }));
    assert_eq!(client.call("registrations.release", json!({ "registration_id": reg2 })), json!({}));
    let refused_after = client.call_err("cb-demo.watch", json!({ "prefix": "hi", "callbacks": ["greet"] }));
    assert_eq!(refused_after["data"]["code"], json!("unsupported"), "{refused_after}");
    client.shutdown_and_wait();
}

/// `callback.call` projects registered secrets (and fails closed on
/// sensitive keys), validates callback results strictly without `shape_empty` (rejecting `{}` for
/// array and `[]` for object, plus >10k-node payloads), normalizes non-core errors to `internal`
/// while preserving core codes, handles timeouts (`$/cancel_request`), propagates `turn_id`, invokes
/// optional callbacks, rejects undeclared callback names, and isolates peers against hijacking.
#[test]
fn contract_callbacks_project_secrets_enforce_strict_results_and_isolate_peers() {
    use std::sync::{Arc, Mutex};
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { peer = { allow = { 'cb-sec.watch', 'cb-sec.fire' } } }")
        .unwrap();
    std::fs::write(
        home.join("config/plugins/cb_sec.lua"),
        r#"return {
    name = "cb-sec-plugin",
    inject = { "tools" },
    provide = { "cb-sec" },
    contracts = {
        ["cb-sec"] = {
            methods = {
                watch = {
                    doc = "register callbacks",
                    expose = "both",
                    access = "peer",
                    registration = true,
                    order = { "..." },
                    callbacks = {
                        greet = {
                            type = "fun(name: string): string",
                            params = {
                                type = "object",
                                properties = { name = { type = "string" } },
                                required = { "name" },
                                additionalProperties = false,
                            },
                            order = { "name" },
                            result = { type = "string" },
                        },
                        done = {
                            type = "fun(note: string)",
                            params = {
                                type = "object",
                                properties = { note = { type = "string" } },
                                required = { "note" },
                                additionalProperties = false,
                            },
                            order = { "note" },
                            optional = true,
                        },
                        list_cb = {
                            type = "fun(): any[]",
                            result = { type = "array" },
                            optional = true,
                        },
                        obj_cb = {
                            type = "fun(): table",
                            result = { type = "object" },
                            optional = true,
                        },
                        open_cb = {
                            type = "fun(payload: any): any",
                            params = {
                                type = "object",
                                properties = { payload = {} },
                                required = { "payload" },
                                additionalProperties = false,
                            },
                            order = { "payload" },
                            result = {},
                            optional = true,
                        },
                    },
                },
                fire = {
                    doc = "invoke active callback",
                    expose = "both",
                    access = "peer",
                    params = {
                        type = "object",
                        properties = {
                            which = { type = "string" },
                            arg = {},
                        },
                        required = { "which" },
                        additionalProperties = false,
                    },
                    order = { "which", "arg" },
                    result = { type = "object" },
                },
            },
        },
    },
    apply = function(ctx)
        local active = nil
        local lease = nil
        local S = require("cordis").class("CbSec", mare.Service)
        function S:constructor(c)
            S.super.constructor(self, c, "cb-sec")
        end
        function S:watch(opts)
            active = {
                greet = opts.greet,
                done = opts.done,
                list_cb = opts.list_cb,
                obj_cb = opts.obj_cb,
                open_cb = opts.open_cb,
            }
            return function()
                active = nil
            end
        end
        function S:fire(which, arg)
            if which == "reg_secret" then
                if lease then lease:close() end
                lease = mare.projection.register(tostring(arg or "top-secret-value-123"))
                return { ok = true }
            end
            if not active then
                return { ok = false, code = "none" }
            end
            local fn = active[which]
            if not fn then
                return { ok = false, code = "absent" }
            end
            local ok, value
            if arg == nil then
                ok, value = pcall(fn)
            else
                ok, value = pcall(fn, arg)
            end
            if ok then
                return { ok = true, value = value }
            end
            return {
                ok = false,
                code = type(value) == "table" and value.code or "unstructured",
                message = type(value) == "table" and value.message or tostring(value),
            }
        end
        ctx:effect(function()
            return function()
                if lease then lease:close() end
            end
        end)
        ctx:inject({ "tools" }, function(c)
            c.tools:register("cb_turn_probe", {
                description = "invoke greet callback inside turn",
                side_effect = "read",
                parameters = { type = "object", properties = {}, additionalProperties = false },
                execute = function()
                    if active and active.greet then
                        local v = active.greet("from-turn")
                        return { content = tostring(v) }
                    end
                    return { content = "no-cb" }
                end,
            })
        end)
        ctx:plugin(S)
    end,
}"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(
        &faux,
        r#"[
            {"tool_calls":[{"name":"cb_turn_probe","arguments":{}}]},
            {"text":"turn-complete"}
        ]"#,
    )
    .unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--peer", "--model", "faux/echo"], &env, root.path());
    client.call(
        "initialize",
        json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 5000 }),
    );

    // Undeclared callback name is rejected with invalid_arguments.
    let bad_sub = client.call_err("cb-sec.watch", json!({ "callbacks": ["greet", "no_such_cb"] }));
    assert_eq!(bad_sub["data"]["code"], json!("invalid_arguments"), "{bad_sub}");

    let reg_id = client.call("cb-sec.watch", json!({ "callbacks": ["greet", "done", "list_cb", "obj_cb", "open_cb"] }))
        ["registration_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Register a secret via mare.projection.register; callback.call must project it in arguments.
    let secret = "sk-live-super-secret-value-987654";
    assert_eq!(client.call("cb-sec.fire", json!({ "which": "reg_secret", "arg": secret }))["ok"], json!(true));

    let seen_calls: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let mode: Arc<Mutex<&'static str>> = Arc::new(Mutex::new("ok"));
    let cancelled_ids: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
    let seen_c = seen_calls.clone();
    let mode_c = mode.clone();
    let canc_c = cancelled_ids.clone();
    client.on_method = Some(Box::new(move |c, msg| {
        let m = msg.get("method").and_then(Value::as_str);
        if m == Some("callback.call") {
            seen_c.lock().unwrap().push(msg.clone());
            let current = *mode_c.lock().unwrap();
            match current {
                "ok" => c.send(json!({ "id": msg["id"], "result": "ok" })),
                "empty_obj" => c.send(json!({ "id": msg["id"], "result": {} })),
                "empty_arr" => c.send(json!({ "id": msg["id"], "result": [] })),
                "custom_err" => c.send(json!({
                    "id": msg["id"],
                    "error": { "code": -32000, "message": "boom", "data": { "code": "custom.boom" } }
                })),
                "core_err" => c.send(json!({
                    "id": msg["id"],
                    "error": { "code": -32000, "message": "nope", "data": { "code": "denied" } }
                })),
                "malformed" => {
                    c.send(json!({ "id": msg["id"], "result": "x", "error": { "code": -32000, "message": "bad" } }))
                }
                "huge" => {
                    let big: Vec<i64> = (0..10_500).collect();
                    c.send(json!({ "id": msg["id"], "result": big }));
                }
                "hang" => {}
                _ => c.send(json!({ "id": msg["id"], "result": null })),
            }
            return true;
        }
        if m == Some("$/cancel_request") {
            canc_c.lock().unwrap().push(msg["params"]["id"].clone());
            return true;
        }
        false
    }));

    // 1. Secret projection: plaintext secret never appears in callback.call arguments; sensitive key fails closed.
    let proj_res = client.call("cb-sec.fire", json!({ "which": "greet", "arg": format!("token={secret}") }));
    assert_eq!(proj_res["ok"], json!(true), "{proj_res}");
    {
        let calls = seen_calls.lock().unwrap();
        let wire = calls.last().unwrap().to_string();
        assert!(!wire.contains(secret), "secret leaked in callback.call: {wire}");
    }
    let key_leak_res =
        client.call("cb-sec.fire", json!({ "which": "open_cb", "arg": { format!("key-{secret}"): "v" } }));
    assert_eq!(key_leak_res["ok"], json!(false), "sensitive object key must fail closed: {key_leak_res}");

    // 2. Strict empty-container validation (no shape_empty on external JSON reply).
    *mode.lock().unwrap() = "empty_obj";
    let list_bad = client.call("cb-sec.fire", json!({ "which": "list_cb" }));
    assert_eq!(list_bad["code"], json!("invalid_result"), "empty {{}} for array must fail: {list_bad}");
    *mode.lock().unwrap() = "empty_arr";
    let obj_bad = client.call("cb-sec.fire", json!({ "which": "obj_cb" }));
    assert_eq!(obj_bad["code"], json!("invalid_result"), "empty [] for object must fail: {obj_bad}");

    // 3. Non-core error normalization, core error passthrough, malformed envelope, oversized result, optional cb.
    *mode.lock().unwrap() = "custom_err";
    assert_eq!(client.call("cb-sec.fire", json!({ "which": "greet", "arg": "x" }))["code"], json!("internal"));
    *mode.lock().unwrap() = "core_err";
    assert_eq!(client.call("cb-sec.fire", json!({ "which": "greet", "arg": "x" }))["code"], json!("denied"));
    *mode.lock().unwrap() = "malformed";
    assert_eq!(client.call("cb-sec.fire", json!({ "which": "greet", "arg": "x" }))["code"], json!("internal"));
    *mode.lock().unwrap() = "huge";
    assert_eq!(client.call("cb-sec.fire", json!({ "which": "open_cb", "arg": 1 }))["code"], json!("invalid_result"));
    *mode.lock().unwrap() = "null";
    assert_eq!(client.call("cb-sec.fire", json!({ "which": "done", "arg": "opt" }))["ok"], json!(true));

    // 4. `turn_id` propagation during an active turn.
    *mode.lock().unwrap() = "ok";
    seen_calls.lock().unwrap().clear();
    let sid = client.call("status.get", json!({}))["session"]["session_id"].as_str().unwrap().to_string();
    let prompt_res = client.call("agent.prompt", json!({ "session_id": sid, "input": "go" }));
    let turn_id = prompt_res["turn_id"].clone();
    assert!(turn_id.is_string(), "{prompt_res}");
    client.call("agent.wait", json!({ "session_id": sid, "turn_id": turn_id }));
    {
        let calls = seen_calls.lock().unwrap();
        let turn_cb = calls.iter().find(|c| c["params"]["arguments"]["name"] == "from-turn").unwrap();
        assert_eq!(turn_cb["params"]["turn_id"], turn_id, "{turn_cb}");
        assert!(turn_cb["params"]["tool_call_id"].is_string(), "{turn_cb}");
    }

    // 5. Timeout (`timed_out`) + `$/cancel_request` when client stays silent.
    {
        let mut to_client = crate::common::rpc::RpcClient::spawn(
            &["--rpc", "--yolo", "--peer", "--model", "faux/echo"],
            &env,
            root.path(),
        );
        to_client.call(
            "initialize",
            json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 50 }),
        );
        to_client.call("cb-sec.watch", json!({ "callbacks": ["greet", "done", "list_cb", "obj_cb", "open_cb"] }));
        let to_seen: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
        let to_cancelled: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![]));
        let to_seen_c = to_seen.clone();
        let to_canc_c = to_cancelled.clone();
        to_client.on_method = Some(Box::new(move |_c, msg| {
            let m = msg.get("method").and_then(Value::as_str);
            if m == Some("callback.call") {
                to_seen_c.lock().unwrap().push(msg.clone());
                return true;
            }
            if m == Some("$/cancel_request") {
                to_canc_c.lock().unwrap().push(msg["params"]["id"].clone());
                return true;
            }
            false
        }));
        let to_res = to_client.call("cb-sec.fire", json!({ "which": "greet", "arg": "silent" }));
        assert_eq!(to_res["code"], json!("timed_out"), "{to_res}");
        let timed_id = to_seen.lock().unwrap().last().unwrap()["id"].clone();
        assert!(to_cancelled.lock().unwrap().contains(&timed_id), "timeout must send $/cancel_request");
        to_client.shutdown_and_wait();
    }

    // 6. Cross-connection isolation: primary cannot see, spoof-reply, or release peer's registration.
    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    let peer_init =
        peer.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));
    assert!(peer_init["error"].is_null(), "{peer_init}");
    let peer_sid = peer_init["result"]["session"]["session_id"].as_str().expect("peer session_id").to_string();
    let peer_reg =
        peer.call("cb-sec.watch", json!({ "session_id": peer_sid, "callbacks": ["greet"] }))["result"]["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(client.call("registrations.release", json!({ "registration_id": peer_reg })), json!({}));

    *mode.lock().unwrap() = "ok";
    seen_calls.lock().unwrap().clear();
    let fire_req = client.request("cb-sec.fire", json!({ "which": "greet", "arg": "isolated" }));
    let peer_cb = loop {
        let msg = peer.read();
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            break msg;
        }
    };
    assert!(
        seen_calls.lock().unwrap().is_empty(),
        "primary must not receive peer callback.call: {:?}",
        seen_calls.lock().unwrap()
    );
    // Spoofed reply from primary with the exact same callback id must not resolve peer's call.
    client.send(json!({ "id": peer_cb["id"], "result": "spoofed" }));
    peer.send(json!({ "id": peer_cb["id"], "result": "from-peer" }));
    let fire_res = client.wait(fire_req).unwrap();
    assert_eq!(fire_res, json!({ "ok": true, "value": "from-peer" }));

    assert_eq!(
        client.call("registrations.release", json!({ "registration_id": reg_id })),
        json!({}),
        "client owns reg_id until released"
    );
    drop(peer);
    client.shutdown_and_wait();
}

/// Non-web verification of `MethodSpec::confined`: any contract declaring `confined = true` has its
/// direct RPC exports run inside `mare_binding::exec_ctx::run_host_query`
/// (with `mare.exec.current()` populated), while `confined = false` does not.
#[tokio::test]
async fn contract_confined_flag_wraps_non_web_service_exports_in_run_host_query() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        let dir = env.config_dir().join("plugins/confined-probe");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("init.lua"),
            r#"return {
    name = "confined-probe",
    provide = { "confined_probe" },
    contracts = {
        confined_probe = {
            doc = "Test contract for MethodSpec::confined.",
            methods = {
                confined_state = {
                    doc = "Runs inside host query execution context.",
                    expose = "both",
                    access = "peer",
                    confined = true,
                    params = { type = "object", properties = {}, additionalProperties = false },
                    order = {},
                    result = {
                        type = "object",
                        properties = { active = { type = "boolean" }, sandbox_level = { type = "string" } },
                        required = { "active", "sandbox_level" },
                        additionalProperties = false,
                    },
                },
                open_state = {
                    doc = "Runs without confined host query wrapper.",
                    expose = "both",
                    access = "peer",
                    params = { type = "object", properties = {}, additionalProperties = false },
                    order = {},
                    result = {
                        type = "object",
                        properties = { active = { type = "boolean" }, sandbox_level = { type = "string" } },
                        required = { "active", "sandbox_level" },
                        additionalProperties = false,
                    },
                },
            },
        },
    },
    apply = function(ctx)
        require("mare_lib").service(ctx, "confined_probe", {
            confined_state = function()
                local cur = mare.exec.current()
                return {
                    active = cur ~= nil,
                    sandbox_level = (cur and cur.sandbox and cur.sandbox.level) or "none",
                }
            end,
            open_state = function()
                local cur = mare.exec.current()
                return {
                    active = cur ~= nil,
                    sandbox_level = (cur and cur.sandbox and cur.sandbox.level) or "none",
                }
            end,
        })
    end,
}"#,
        )
        .unwrap();

        let mut b = env
            .host()
            .user_plugins()
            .no_session()
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } }));
        b.opts.mode = mare_shared::ui::Mode::Rpc;
        b.opts.project_trusted = true;
        let h = b.build().await;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (srv_r, srv_w) = tokio::io::split(server_io);
        let (cli_r, mut cli_w) = tokio::io::split(client_io);
        let stop = tokio_util::sync::CancellationToken::new();
        let stop_c = stop.clone();
        let srv_task = tokio::task::spawn_local(mare_cli::rpc::serve(h.clone(), srv_r, srv_w, true, async move {
            stop_c.cancelled().await;
        }));

        let mut lines = tokio::io::BufReader::new(cli_r).lines();
        let mut rpc_call = async |id: u64, method: &str, params: serde_json::Value| -> serde_json::Value {
            let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            cli_w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
            cli_w.flush().await.unwrap();
            loop {
                let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                    .await
                    .expect("RPC response timed out")
                    .unwrap()
                    .expect("RPC stream closed");
                let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
                if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    return msg;
                }
            }
        };

        let init = rpc_call(1, "initialize", json!({ "protocol_version": "3.0", "capabilities": {} })).await;
        assert!(init.get("result").is_some(), "{init}");

        let direct = rpc_call(2, "confined_probe.confined_state", json!({})).await;
        assert_eq!(direct["result"], json!({ "active": true, "sandbox_level": "read-only" }), "{direct}");

        let open = rpc_call(4, "confined_probe.open_state", json!({})).await;
        assert_eq!(open["result"], json!({ "active": false, "sandbox_level": "none" }), "{open}");

        let _ = rpc_call(5, "shutdown", json!({})).await;
        stop.cancel();
        drop(cli_w);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), srv_task).await;
        h.shutdown().await;
    })
    .await;
}

/// `hooks.subscribe {event, priority?, timeout_ms?}`
/// replaces the legacy batch `hooks.subscribe`, `hooks.unsubscribe`, `hooks.next`, `hook.call`, and
/// `hook.notify`: single registration via `hooks` contract, reverse calls via D11 `callback.call`,
/// disposal via `registrations.release`, and immediate unblock on peer disconnect without hanging.
#[test]
fn hooks_subscribe_uses_callback_call_and_ends_on_release_or_disconnect() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        "mare.setup { peer = { allow = { 'hooks.subscribe', 'agent.prompt' } } }",
    )
    .unwrap();
    std::fs::write(
        home.join("config/plugins/gate.lua"),
        r#"return {
    name = "gate",
    inject = { "policy" },
    contracts = {
        gate = {
            methods = {},
            events = {
                check = {
                    doc = "public policy check",
                    mode = "policy",
                    expose = "both",
                    delivery = "required",
                    ignorable = false,
                    data = { type = "object", properties = { tool = { type = "string" } } },
                    example = { tool = "read" },
                },
            },
        },
    },
    apply = function(ctx)
        ctx.policy:voter("tool/before", function(call)
            return ctx.policy:decide("gate/check", { { tool = call.name } })
        end)
    end,
}"#,
    )
    .unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(
        &faux,
        r#"[
            {"tool_calls":[{"name":"read","arguments":{"path":"a.txt"}}]},
            {"text":"done"},
            {"text":"after-release"},
            {"tool_calls":[{"name":"read","arguments":{"path":"a.txt"}}]},
            {"text":"after-disconnect"}
        ]"#,
    )
    .unwrap();
    std::fs::write(root.path().join("a.txt"), "hello\n").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--peer", "--model", "faux/echo"], &env, root.path());

    let init0 = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": false } }));
    let caps_methods = init0["capabilities"]["methods"].as_array().unwrap();
    assert!(caps_methods.iter().any(|m| m == "hooks.subscribe"), "{caps_methods:?}");
    assert!(!caps_methods.iter().any(|m| m == "hooks.unsubscribe" || m == "hooks.next"), "{caps_methods:?}");
    assert!(init0["capabilities"]["callbacks"].get("hooks").is_none(), "{init0}");

    // Without capabilities.callbacks, hooks.subscribe fails with unsupported.
    let no_cb = client
        .call_err("hooks.subscribe", json!({ "scope": "all", "event": "status/changed", "callbacks": ["handle"] }));
    assert_eq!(no_cb["data"]["code"], json!("unsupported"), "{no_cb}");

    // Enable capabilities.callbacks via connection.set_capabilities.
    let _cap_res = client.call(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": true, "ui": false, "approvals": false, "elicitation": false } }),
    );
    let sid = init0["session"]["session_id"].as_str().expect("session_id").to_string();

    // Old mode/batch methods and hook.call reverse method are gone from schema.
    let schema_out = mare_cmd().args(["rpc", "schema"]).run_output();
    assert!(schema_out.status.success());
    let schema: Value = serde_json::from_slice(&schema_out.stdout).unwrap();
    for removed in ["hooks.unsubscribe", "hooks.next"] {
        assert!(schema["methods"].get(removed).is_none(), "{removed} should be removed from schema");
    }
    for removed_rev in ["hook.call", "hook.notify"] {
        assert!(schema["callbacks"].get(removed_rev).is_none(), "{removed_rev} should be removed");
        assert!(schema["notifications"].get(removed_rev).is_none(), "{removed_rev} should be removed");
    }

    // Subscribe to emit (`status/changed`) and waterfall (`request/build`), check callback.call delivery.
    let status_reg = client.call(
        "hooks.subscribe",
        json!({ "scope": "all", "event": "status/changed", "prepend": true, "callbacks": ["handle"] }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    let build_reg = client.call(
        "hooks.subscribe",
        json!({ "scope": "all", "event": "request/build", "timeout_ms": 5000, "callbacks": ["handle"] }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();

    let seen_status = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen_build = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let s_status = seen_status.clone();
    let s_build = seen_build.clone();
    let s_reg = status_reg.clone();
    let b_reg = build_reg.clone();
    client.on_method = Some(Box::new(move |c, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            assert_eq!(msg["params"]["callback"], json!("handle"), "{msg}");
            let reg = msg["params"]["registration_id"].as_str().unwrap();
            let args = &msg["params"]["arguments"];
            if reg == s_reg {
                s_status.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(args["event"], json!("status/changed"), "{msg}");
                assert_eq!(args["mode"], json!("emit"), "{msg}");
                assert!(args["args"].is_array(), "{msg}");
                c.send(json!({ "id": msg["id"], "result": null }));
            } else if reg == b_reg {
                s_build.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert_eq!(args["event"], json!("request/build"), "{msg}");
                assert_eq!(args["mode"], json!("waterfall"), "{msg}");
                assert!(args["args"].is_array(), "{msg}");
                c.send(json!({ "id": msg["id"], "result": { "next": true } }));
            } else {
                panic!("unexpected callback.call reg: {msg}");
            }
            return true;
        }
        false
    }));
    let p1 = client.call("agent.prompt", json!({ "session_id": sid, "input": "step 1" }));
    let _ = client.call("agent.wait", json!({ "session_id": sid, "turn_id": p1["turn_id"] }));
    assert!(seen_status.load(std::sync::atomic::Ordering::SeqCst) >= 1, "emit hook did not receive callback.call");
    assert!(seen_build.load(std::sync::atomic::Ordering::SeqCst) >= 1, "waterfall hook did not receive callback.call");

    // Release both via registrations.release; subsequent prompt delivers zero callback.call frames.
    assert_eq!(client.call("registrations.release", json!({ "registration_id": status_reg })), json!({}));
    assert_eq!(client.call("registrations.release", json!({ "registration_id": build_reg })), json!({}));
    let before_rel =
        (seen_status.load(std::sync::atomic::Ordering::SeqCst), seen_build.load(std::sync::atomic::Ordering::SeqCst));
    let p2 = client.call("agent.prompt", json!({ "session_id": sid, "input": "step 2" }));
    let _ = client.call("agent.wait", json!({ "session_id": sid, "turn_id": p2["turn_id"] }));
    assert_eq!(
        (seen_status.load(std::sync::atomic::Ordering::SeqCst), seen_build.load(std::sync::atomic::Ordering::SeqCst),),
        before_rel,
        "released hooks must not receive callback.call"
    );

    // Peer cannot subscribe `hooks.subscribe` (`access = "primary"`), even with callbacks capability.
    {
        let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
        let p_init = peer.call(
            "initialize",
            json!({ "protocol_version": init0["protocol_version"], "capabilities": { "callbacks": true } }),
        );
        assert!(p_init["error"].is_null(), "{p_init}");
        let p_sub = peer.call("hooks.subscribe", json!({ "event": "gate/check", "callbacks": ["handle"] }));
        assert_eq!(p_sub["error"]["data"]["code"], "denied", "hooks.subscribe must deny peers: {p_sub}");
    }
    // Primary subscribing to `gate/check` and revoking `capabilities.callbacks` fails closed cleanly (no hang/leak).
    client.events.clear();
    client.call("events.subscribe", json!({ "scope": "all", "include": ["item/completed", "turn/completed"] }));
    let gate_reg =
        client.call("hooks.subscribe", json!({ "scope": "all", "event": "gate/check", "callbacks": ["handle"] }))["registration_id"]
            .as_str()
            .unwrap()
            .to_string();
    client.on_method = None;
    let disconnect_started = std::time::Instant::now();
    let p3 = client.call("agent.prompt", json!({ "session_id": sid, "input": "step 3" }));
    let denied_req = client.request("agent.wait", json!({ "session_id": sid, "turn_id": p3["turn_id"] }));
    loop {
        let msg = client.read();
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            assert_eq!(msg["params"]["registration_id"], json!(gate_reg), "{msg}");
            assert_eq!(msg["params"]["arguments"]["event"], json!("gate/check"), "{msg}");
            assert_eq!(msg["params"]["arguments"]["mode"], json!("policy"), "{msg}");
            // Close the callback channel without replying by revoking `capabilities.callbacks`.
            break;
        }
        client.dispatch(msg);
    }
    let _ = client.call(
        "connection.set_capabilities",
        json!({ "capabilities": { "callbacks": false, "ui": false, "approvals": false, "elicitation": false } }),
    );
    let res = client.wait(denied_req).expect("revoked voter should fail closed cleanly, not hang");
    assert!(
        disconnect_started.elapsed() < std::time::Duration::from_secs(5),
        "disconnect took {:?}",
        disconnect_started.elapsed()
    );
    assert!(res.is_object(), "{res}");
    let tool_err = res["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool_result" && m["tool_name"] == "read")
        .expect("expected read tool_result in prompt messages");
    assert_eq!(tool_err["is_error"], json!(true), "disconnected voter must deny read: {tool_err}");
    assert!(!res.to_string().contains("hello"), "a.txt leaked past tool/before: {res}");
    assert!(
        !client.events.iter().any(|ev| ev.to_string().contains("hello")),
        "a.txt leaked into step 3 events: {:?}",
        client.events
    );
    client.shutdown_and_wait();
}

/// `hooks.subscribe { timeout_ms }` enforces `minimum = 1`, clamps remote
/// callers to the host's `kernel.policy_timeout()` ceiling (cannot disable or exceed it), and
/// expires in-flight bus callbacks with `$/cancel_request` rather than waiting for `callbackTimeoutMs`.
#[test]
fn hooks_subscribe_enforces_timeout_bounds_and_expires_in_flight() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(home.join("config/init.lua"), "mare.setup { policy_timeout_ms = 120 }").unwrap();
    let faux = root.path().join("faux.json");
    std::fs::write(
        &faux,
        r#"[
            {"tool_calls":[{"name":"read","arguments":{"path":"secret.txt"}}]},
            {"text":"done"},
            {"tool_calls":[{"name":"read","arguments":{"path":"secret.txt"}}]},
            {"text":"done-rw"},
            {"text":"done-2"},
            {"text":"done-bus"}
        ]"#,
    )
    .unwrap();
    std::fs::write(root.path().join("secret.txt"), "top-secret\n").unwrap();
    let env = std::collections::BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), faux.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, root.path());
    let init = client
        .call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true }, "callback_timeout_ms": 999_999_999u64 }));
    let sid = init["session"]["session_id"].as_str().expect("session_id").to_string();

    // `timeout_ms <= 0` is rejected (`invalid_arguments`, schema minimum = 1).
    for bad in [0i64, -10] {
        let err = client.call_err(
            "hooks.subscribe",
            json!({ "scope": "all", "event": "tool/before", "timeout_ms": bad, "callbacks": ["handle"] }),
        );
        assert_eq!(err["data"]["code"], json!("invalid_arguments"), "timeout_ms={bad}: {err}");
    }

    // 1. Over-large `timeout_ms` is clamped to host `policy_timeout_ms` (120ms): silent voter fails closed fast.
    let huge_sub = client.call(
        "hooks.subscribe",
        json!({ "scope": "all", "event": "tool/before", "timeout_ms": 999_999, "callbacks": ["handle"] }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    let seen_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let c_flag = seen_cancel.clone();
    client.on_method = Some(Box::new(move |_c, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("$/cancel_request") {
            c_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            return true;
        }
        // Leave `callback.call` unanswered to trigger the clamped timeout.
        msg.get("method").and_then(Value::as_str) == Some("callback.call")
    }));
    client.call("events.subscribe", json!({ "scope": "all", "include": ["item/completed"] }));
    let t0 = std::time::Instant::now();
    let p0 = client.call("agent.prompt", json!({ "session_id": sid, "input": "read it" }));
    let res = client.call("agent.wait", json!({ "session_id": sid, "turn_id": p0["turn_id"] }));
    assert!(t0.elapsed() < std::time::Duration::from_secs(10), "clamped policy timeout took {:?}", t0.elapsed());
    let tool_err = res["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool_result" && m["tool_name"] == "read")
        .expect("expected read tool_result in prompt messages");
    assert_eq!(tool_err["is_error"], json!(true), "{tool_err}");
    assert!(!res.to_string().contains("top-secret"), "{res}");
    assert!(seen_cancel.load(std::sync::atomic::Ordering::SeqCst), "expected $/cancel_request on timeout");
    client.call("registrations.release", json!({ "registration_id": huge_sub }));

    // 2. Over-large `timeout_ms` on `tool/rewrite` is clamped to `policy_timeout_ms` (120ms) and fails closed.
    seen_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    let rw_sub = client.call(
        "hooks.subscribe",
        json!({ "scope": "all", "event": "tool/rewrite", "timeout_ms": 999_999, "callbacks": ["handle"] }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    let t_rw = std::time::Instant::now();
    let p_rw = client.call("agent.prompt", json!({ "session_id": sid, "input": "read rw" }));
    let res_rw = client.call("agent.wait", json!({ "session_id": sid, "turn_id": p_rw["turn_id"] }));
    assert!(
        t_rw.elapsed() < std::time::Duration::from_secs(10),
        "clamped tool/rewrite timeout took {:?}",
        t_rw.elapsed()
    );
    let rw_tool_err = res_rw["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool_result" && m["tool_name"] == "read")
        .expect("expected read tool_result in rewrite prompt messages");
    assert_eq!(rw_tool_err["is_error"], json!(true), "{rw_tool_err}");
    assert!(!res_rw.to_string().contains("top-secret"), "{res_rw}");
    assert!(seen_cancel.load(std::sync::atomic::Ordering::SeqCst), "expected $/cancel_request on rewrite timeout");
    client.call("registrations.release", json!({ "registration_id": rw_sub }));

    // 3. Short `timeout_ms = 50` on a bus hook expires faster than the large initialize `callbackTimeoutMs`.
    seen_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    let bus_sub = client.call(
        "hooks.subscribe",
        json!({ "scope": "all", "event": "status/changed", "timeout_ms": 50, "callbacks": ["handle"] }),
    )["registration_id"]
        .as_str()
        .unwrap()
        .to_string();
    let t1 = std::time::Instant::now();
    let p_bus = client.call("agent.prompt", json!({ "session_id": sid, "input": "trigger status" }));
    let _ = client.call("agent.wait", json!({ "session_id": sid, "turn_id": p_bus["turn_id"] }));
    assert!(t1.elapsed() < std::time::Duration::from_secs(10), "bus timeout_ms took {:?}", t1.elapsed());
    assert!(seen_cancel.load(std::sync::atomic::Ordering::SeqCst), "expected $/cancel_request for bus timeout_ms");
    client.call("registrations.release", json!({ "registration_id": bus_sub }));

    // 4. Omitting `timeout_ms` on a remote bus hook still clamps to `policy_timeout_ms` (120ms),
    //    not `initialize.callbackTimeoutMs` (`MAX_CALLBACK_TIMEOUT_MS`).
    seen_cancel.store(false, std::sync::atomic::Ordering::SeqCst);
    let _bus_default_sub =
        client.call("hooks.subscribe", json!({ "scope": "all", "event": "status/changed", "callbacks": ["handle"] }));
    let t2 = std::time::Instant::now();
    let p_def = client.call("agent.prompt", json!({ "session_id": sid, "input": "trigger default bus cap" }));
    let _ = client.call("agent.wait", json!({ "session_id": sid, "turn_id": p_def["turn_id"] }));
    assert!(
        t2.elapsed() < std::time::Duration::from_secs(10),
        "omitted bus timeout_ms bypassed policy_timeout_ms and took {:?}",
        t2.elapsed()
    );
    assert!(
        seen_cancel.load(std::sync::atomic::Ordering::SeqCst),
        "expected $/cancel_request for omitted bus timeout_ms"
    );
    client.shutdown_and_wait();
}

/// `callback.update { id: RequestId, partial }` streams partial results to an in-flight
/// contract callback in order before its final `callback.call` reply returns, ignores late/unknown/
/// invalid partials cleanly, rejects missing `update` schema when Lua passes an update handler, and
/// cleans up cleanly under registration release or withdrawn `capabilities.callbacks`.
#[test]
fn contract_callbacks_stream_partial_updates_via_callback_update() {
    use std::sync::{Arc, Mutex};
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config/plugins")).unwrap();
    std::fs::write(home.join("config/init.lua"), "").unwrap();
    std::fs::write(
        home.join("config/plugins/cb_stream.lua"),
        r#"return {
    name = "cb-stream-plugin",
    provide = { "cb-stream" },
    contracts = {
        ["cb-stream"] = {
            methods = {
                watch = {
                    doc = "register callbacks with and without update schema",
                    expose = "both",
                    registration = true,
                    order = { "..." },
                    callbacks = {
                        stream = {
                            type = "fun(topic: string, on_update?: fun(partial: string)): string",
                            params = {
                                type = "object",
                                properties = { topic = { type = "string" } },
                                required = { "topic" },
                                additionalProperties = false,
                            },
                            order = { "topic" },
                            update = { type = "string" },
                            result = { type = "string" },
                        },
                        plain = {
                            type = "fun(topic: string): string",
                            params = {
                                type = "object",
                                properties = { topic = { type = "string" } },
                                required = { "topic" },
                                additionalProperties = false,
                            },
                            order = { "topic" },
                            result = { type = "string" },
                        },
                    },
                },
                run = {
                    doc = "invoke stream callback and collect updates before return",
                    expose = "both",
                    params = {
                        type = "object",
                        properties = { topic = { type = "string" }, mode = { type = "string" } },
                        required = { "topic" },
                        additionalProperties = false,
                    },
                    order = { "topic", "mode" },
                    result = {
                        type = "object",
                        properties = {
                            ok = { type = "boolean" },
                            result = { type = "string" },
                            code = { type = "string" },
                            chunks = { type = "array", items = { type = "string" } },
                        },
                    },
                },
            },
        },
    },
    apply = function(ctx)
        local active = nil
        local S = require("cordis").class("CbStream", mare.Service)
        function S:constructor(c)
            S.super.constructor(self, c, "cb-stream")
        end
        function S:watch(opts)
            active = { stream = opts.stream, plain = opts.plain }
            return function()
                active = nil
            end
        end
        function S:run(topic, mode)
            if not active then
                return { ok = false, err = "none" }
            end
            local chunks = {}
            if mode == "plain_with_updater" then
                local ok, value = pcall(active.plain, topic, function(_) end)
                return { ok = ok, code = (not ok and type(value) == "table") and value.code or tostring(value) }
            end
            local ok, value
            if mode == "no_updater" then
                ok, value = pcall(active.stream, topic)
            else
                ok, value = pcall(active.stream, topic, function(partial)
                    chunks[#chunks + 1] = partial
                end)
            end
            if ok then
                return { ok = true, result = value, chunks = chunks }
            end
            return { ok = false, code = type(value) == "table" and value.code or tostring(value), chunks = chunks }
        end
        ctx:plugin(S)
    end,
}
"#,
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
    client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }));

    // `services.list` includes the callback's `update` schema.
    let listed = client.call("services.list", json!({}));
    let watch =
        listed["data"].as_array().unwrap().iter().find(|s| s["name"] == "cb-stream").unwrap()["methods"]["watch"]
            .clone();
    assert_eq!(watch["callbacks"]["stream"]["update"], json!({ "type": "string" }), "{watch}");

    let reg_id = client.call("cb-stream.watch", json!({ "callbacks": ["stream", "plain"] }))["registration_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 1. Happy path: ordered `callback.update` chunks + ignored invalid-schema partial precede final result.
    let seen_cb_id: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
    let seen_cb_id_c = seen_cb_id.clone();
    let req = client.request("cb-stream.run", json!({ "topic": "alpha" }));
    let res = loop {
        let msg = client.read();
        if msg.get("id").and_then(Value::as_u64) == Some(req) {
            break msg;
        }
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            let cb_id = msg["id"].clone();
            *seen_cb_id_c.lock().unwrap() = Some(cb_id.clone());
            // Valid chunks, an invalid-schema chunk (number), an unknown id chunk, then valid chunk, then final reply.
            client.send(
                json!({ "jsonrpc": "2.0", "method": "callback.update", "params": { "id": cb_id, "partial": "one" } }),
            );
            client.send(
                json!({ "jsonrpc": "2.0", "method": "callback.update", "params": { "id": cb_id, "partial": 999 } }),
            );
            client.send(json!({ "jsonrpc": "2.0", "method": "callback.update", "params": { "id": "mare-does-not-exist", "partial": "ghost" } }));
            client.send(
                json!({ "jsonrpc": "2.0", "method": "callback.update", "params": { "id": cb_id, "partial": "two" } }),
            );
            client.send(json!({ "id": cb_id, "result": "done" }));
        } else {
            client.dispatch(msg);
        }
    };
    assert_eq!(res["result"], json!({ "ok": true, "result": "done", "chunks": ["one", "two"] }), "{res}");

    // 2. Late `callback.update` after final response does not wedge or fail subsequent calls.
    let old_id = seen_cb_id.lock().unwrap().clone().unwrap();
    client
        .send(json!({ "jsonrpc": "2.0", "method": "callback.update", "params": { "id": old_id, "partial": "late" } }));

    // 3. `callback.update` when Lua caller did not pass an updater is ignored cleanly.
    let req_no_updater = client.request("cb-stream.run", json!({ "topic": "beta", "mode": "no_updater" }));
    let res_no_updater = loop {
        let msg = client.read();
        if msg.get("id").and_then(Value::as_u64) == Some(req_no_updater) {
            break msg;
        }
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            let cb_id = msg["id"].clone();
            client.send(json!({ "jsonrpc": "2.0", "method": "callback.update", "params": { "id": cb_id, "partial": "ignored" } }));
            client.send(json!({ "id": cb_id, "result": "ok-plain" }));
        } else {
            client.dispatch(msg);
        }
    };
    assert_eq!(res_no_updater["result"], json!({ "ok": true, "result": "ok-plain", "chunks": [] }), "{res_no_updater}");

    // 4. Passing an updater to a callback without `update` schema fails closed with `invalid_arguments`.
    let bad_updater = client.call("cb-stream.run", json!({ "topic": "gamma", "mode": "plain_with_updater" }));
    assert_eq!(bad_updater, json!({ "ok": false, "code": "invalid_arguments" }), "{bad_updater}");

    // 5. Releasing registration while `callback.update` was sent and `callback.call` is in flight ends cleanly.
    let rel_run_req = client.request("cb-stream.run", json!({ "topic": "delta" }));
    let inflight_cb = loop {
        let msg = client.read();
        if msg.get("method").and_then(Value::as_str) == Some("callback.call") {
            break msg;
        }
        client.dispatch(msg);
    };
    client.send(json!({ "jsonrpc": "2.0", "method": "callback.update", "params": { "id": inflight_cb["id"], "partial": "partial-before-release" } }));
    assert_eq!(client.call("registrations.release", json!({ "registration_id": reg_id })), json!({}));
    let rel_run_res = loop {
        let msg = client.read();
        if msg.get("id").and_then(Value::as_u64) == Some(rel_run_req) {
            break msg;
        }
        client.dispatch(msg);
    };
    assert_eq!(rel_run_res["result"]["ok"], json!(false), "{rel_run_res}");
    assert_eq!(rel_run_res["result"]["code"], json!("cancelled"), "{rel_run_res}");

    client.shutdown_and_wait();
}
