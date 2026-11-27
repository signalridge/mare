use super::*;

#[tokio::test]
async fn rpc_schema_events_and_custom_message_vs_entry_parity() {
    local(async {
        let env = TestEnv::new().await;
        let cfg_dir = env.config_dir();
        std::fs::create_dir_all(cfg_dir.join("plugins")).unwrap();
        std::fs::write(
            cfg_dir.join("plugins/c10_test.lua"),
            r#"
            return {
                name = "c10_test",
                inject = { "message", "ui" },
                apply = function(ctx)
                    ctx.message:register_type("rpc/card", {
                        validate = function(msg)
                            if type(msg.details) ~= "table" or msg.details.valid ~= true then
                                return false, "rpc validation rejected"
                            end
                            return true
                        end,
                    })
                    ctx.ui:register_markdown_transform({
                        transform = function(md)
                            return (md:gsub("RAW", "TRANSFORMED"))
                        end,
                        message_types = { "custom" },
                    })
                end,
            }
            "#,
        )
        .unwrap();

        let script_path = env.home.path().join("faux_script.json");
        std::fs::write(&script_path, json!([]).to_string()).unwrap();
        let mut cmd = mare_cmd();
        cmd.env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &script_path)
            .current_dir(&env.cwd)
            .args(["--rpc", "--model", "faux/echo"])
            .stderr(Stdio::piped());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);

        let init = rpc.call_raw(
            "initialize",
            json!({ "protocol_version": "3.0", "client": { "name": "c10-rpc-test" } }),
            |_| json!(null),
        );
        assert!(init["error"].is_null(), "{init}");
        let advertised = init["result"]["capabilities"]["methods"].as_array().unwrap();
        for name in ["message.append", "message.list_types", "session.add_record", "ui.transform_markdown"] {
            assert!(advertised.iter().any(|m| m == name), "missing capability {name}: {advertised:?}");
        }
        assert!(
            !advertised.iter().any(|m| m == "markdown.transform") && !advertised.iter().any(|m| m == "message.types"),
            "legacy markdown.transform and message.types must not be advertised: {advertised:?}"
        );

        let mut events = vec![];
        let sub = rpc_call_collecting(&mut rpc, "events.subscribe", json!({ "scope": "all" }), &mut events);
        assert!(sub["error"].is_null(), "{sub}");

        let sess = rpc_call_collecting(&mut rpc, "session.create", json!({}), &mut events);
        assert!(sess["error"].is_null(), "{sess}");
        let sid = sess["result"]["session_id"].as_str().unwrap().to_string();

        let old_types = rpc_call_collecting(&mut rpc, "message.types", json!({}), &mut events);
        assert_eq!(old_types["error"]["data"]["code"], "method_not_found", "{old_types}");

        let types = rpc_call_collecting(&mut rpc, "message.list_types", json!({}), &mut events);
        assert!(types["error"].is_null(), "{types}");
        assert_eq!(types["result"]["data"], json!([{ "type": "rpc/card" }]), "{types}");

        // Session-scoped message.append requires valid session_id.
        let missing_sid =
            rpc_call_collecting(&mut rpc, "message.append", json!({ "type": "rpc/card", "content": "x" }), &mut events);
        assert_eq!(missing_sid["error"]["data"]["code"], "invalid_arguments", "{missing_sid}");

        let bad = rpc_call_collecting(
            &mut rpc,
            "message.append",
            json!({ "session_id": sid, "type": "rpc/card", "content": "nope", "details": { "valid": false } }),
            &mut events,
        );
        assert!(bad["error"]["message"].as_str().unwrap_or("").contains("rpc validation rejected"), "{bad}");

        let rec = rpc_call_collecting(
            &mut rpc,
            "session.add_record",
            json!({
                "kind": "rpc/entry",
                "data": { "k": "display-only" },
                "session_id": sid,
                "sync": true,
            }),
            &mut events,
        );
        assert_eq!(rec["result"]["durable"], true, "{rec}");
        assert!(rec["result"]["record_id"].is_string() && rec["result"].get("item_id").is_none(), "{rec}");

        let app = rpc_call_collecting(
            &mut rpc,
            "message.append",
            json!({
                "session_id": sid,
                "type": "rpc/card",
                "content": "from-rpc-custom",
                "display": false,
                "details": { "valid": true },
                "request_id": "msg-append-1"
            }),
            &mut events,
        );
        assert!(app["result"].get("ok").is_none(), "{app}");
        assert_eq!(app["result"]["status"], "appended", "{app}");
        assert!(app["result"].get("duplicate").is_none(), "{app}");

        // Retrying the same request_id replays without appending a duplicate message.
        let replay = rpc_call_collecting(
            &mut rpc,
            "message.append",
            json!({
                "session_id": sid,
                "type": "rpc/card",
                "content": "from-rpc-custom",
                "display": false,
                "details": { "valid": true },
                "request_id": "msg-append-1"
            }),
            &mut events,
        );
        assert!(replay["result"].get("ok").is_none(), "{replay}");
        assert_eq!(replay["result"]["status"], "appended", "{replay}");
        assert_eq!(replay["result"]["duplicate"], true, "{replay}");

        // Reusing request_id with different content returns request_id_conflict.
        let conflict = rpc_call_collecting(
            &mut rpc,
            "message.append",
            json!({
                "session_id": sid,
                "type": "rpc/card",
                "content": "different-content",
                "display": false,
                "details": { "valid": true },
                "request_id": "msg-append-1"
            }),
            &mut events,
        );
        assert_eq!(conflict["error"]["data"]["code"], "request_id_conflict", "{conflict}");

        let legacy_xform = rpc_call_collecting(
            &mut rpc,
            "markdown.transform",
            json!({ "text": "hello RAW", "message_type": "custom" }),
            &mut events,
        );
        assert_eq!(legacy_xform["error"]["code"], -32601, "legacy markdown.transform must be removed: {legacy_xform}");

        let xform = rpc_call_collecting(
            &mut rpc,
            "ui.transform_markdown",
            json!({ "text": "hello RAW", "message_type": "custom" }),
            &mut events,
        );
        assert_eq!(xform["result"]["text"], "hello TRANSFORMED", "{xform}");

        let msgs = rpc_call_collecting(&mut rpc, "agent.list_messages", json!({ "session_id": sid }), &mut events);
        let msg_arr = msgs["result"]["data"].as_array().unwrap();
        assert_eq!(msg_arr.len(), 1, "only custom message enters agent.list_messages: {msgs}");
        assert_eq!(msg_arr[0]["role"], "custom");
        assert_eq!(msg_arr[0]["custom_type"], "rpc/card");

        // Switch away to a fresh session, then load the first session so the receipt reloads from disk.
        let other = rpc_call_collecting(&mut rpc, "session.create", json!({}), &mut events);
        assert!(other["error"].is_null(), "{other}");
        let resumed = rpc_call_collecting(&mut rpc, "session.load", json!({ "session_id": sid }), &mut events);
        assert!(resumed["error"].is_null(), "{resumed}");
        let resumed_sid = resumed["result"]["session_id"].as_str().unwrap().to_string();
        let replay_after_resume = rpc_call_collecting(
            &mut rpc,
            "message.append",
            json!({
                "session_id": resumed_sid,
                "type": "rpc/card",
                "content": "from-rpc-custom",
                "display": false,
                "details": { "valid": true },
                "request_id": "msg-append-1"
            }),
            &mut events,
        );
        assert_eq!(replay_after_resume["result"]["duplicate"], true, "{replay_after_resume}");

        let entries =
            rpc_call_collecting(&mut rpc, "session.list_items", json!({ "session_id": resumed_sid }), &mut events);
        let entry_arr = entries["result"]["data"].as_array().unwrap();
        assert!(!entry_arr.iter().any(|e| e["kind"] == "rpc/entry"), "{entries}");
        assert!(entry_arr.iter().any(|e| e["type"] == "custom" && e["message"]["role"] == "custom"), "{entries}");
        let records = rpc_call_collecting(
            &mut rpc,
            "session.list_records",
            json!({ "session_id": resumed_sid, "kind": "rpc/entry" }),
            &mut events,
        );
        assert!(
            records["result"]["data"].as_array().unwrap().iter().any(|data| data["k"] == "display-only"),
            "{records}"
        );

        let started = rpc_call_collecting(
            &mut rpc,
            "agent.prompt",
            json!({ "session_id": resumed_sid, "input": "after-rpc-custom" }),
            &mut events,
        );
        assert!(started["error"].is_null(), "{started}");
        let prompt = rpc_call_collecting(
            &mut rpc,
            "agent.wait",
            json!({ "session_id": resumed_sid, "turn_id": started["result"]["turn_id"] }),
            &mut events,
        );
        assert!(prompt["error"].is_null(), "{prompt}");
        assert_eq!(prompt["result"]["text"], "echo: after-rpc-custom", "{prompt}");

        assert!(
            !events.iter().any(|e| e["type"] == "item/completed"
                && e["data"]["item"]["type"] == "custom"
                && e["data"]["item"]["kind"] == "rpc/entry"),
            "session records must not be published as item notifications: {events:?}"
        );
        assert!(
            events.iter().any(|e| e["type"] == "item/completed"
                && e["data"]["item"]["message"]["role"] == "custom"
                && e["data"]["item"]["message"]["custom_type"] == "rpc/card"),
            "expected custom item/completed notification for rpc/card in RPC stream: {events:?}"
        );

        let shut = rpc_call_collecting(&mut rpc, "shutdown", json!({}), &mut events);
        assert!(shut["error"].is_null(), "{shut}");
        rpc.wait_exit();
    })
    .await;
}

#[tokio::test]
async fn rpc_client_registers_message_type_entry_renderer_and_markdown_transform_with_callbacks() {
    local(async {
        let env = TestEnv::new().await;
        let script_path = env.home.path().join("faux_script.json");
        std::fs::write(&script_path, json!([]).to_string()).unwrap();
        let mut cmd = mare_cmd();
        cmd.env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &script_path)
            .current_dir(&env.cwd)
            .args(["--rpc", "--model", "faux/echo"])
            .stderr(Stdio::piped());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);

        // Missing capabilities.callbacks refuses remote callback registration with `unsupported`.
        let init_no_cb = rpc.call_raw(
            "initialize",
            json!({
                "protocol_version": "3.0",
                "client": { "name": "c10-client-parity" },
                "capabilities": {}
            }),
            |_| json!(null),
        );
        assert!(init_no_cb["error"].is_null(), "{init_no_cb}");
        let unsupported =
            rpc.call_raw("message.register_type", json!({ "type": "client/card", "callbacks": ["validate"] }), |_| {
                json!(null)
            });
        assert_eq!(unsupported["error"]["data"]["code"], "unsupported", "{unsupported}");

        let advertised = init_no_cb["result"]["capabilities"]["methods"].as_array().unwrap();
        for name in ["message.register_type", "ui.register_entry_renderer", "ui.register_markdown_transform"] {
            assert!(advertised.iter().any(|m| m == name), "missing capability {name}: {advertised:?}");
        }

        let init = rpc.call_raw(
            "connection.set_capabilities",
            json!({
                "capabilities": { "callbacks": true, "ui": false, "approvals": false, "elicitation": false }
            }),
            |_| json!(null),
        );
        assert!(init["error"].is_null(), "{init}");
        assert_eq!(init["result"]["capabilities"]["callbacks"], true, "{init}");

        let sess = rpc.call_raw("session.create", json!({}), |_| json!(null));
        let sid = sess["result"]["session_id"].as_str().unwrap().to_string();

        let reg_type = rpc.call_raw(
            "message.register_type",
            json!({ "type": "client/card", "callbacks": ["validate", "render"] }),
            |_| json!(null),
        );
        let reg_id = reg_type["result"]["registration_id"].as_str().expect("registration_id").to_string();

        let types = rpc.call_raw("message.list_types", json!({}), |_| json!(null));
        assert_eq!(types["result"]["data"], json!([{ "type": "client/card" }]), "{types}");

        let mut validate_calls = vec![];
        let bad = rpc.call_raw(
            "message.append",
            json!({ "session_id": sid, "type": "client/card", "content": "nope", "details": { "ok": false } }),
            |cb| {
                if cb["method"] == "callback.call" && cb["params"]["callback"] == "validate" {
                    validate_calls.push(cb.clone());
                    let ok = cb["params"]["arguments"]["message"]["details"]["ok"].as_bool().unwrap_or(false);
                    return json!({ "valid": ok, "reason": "client rejected" });
                }
                json!(null)
            },
        );
        assert_eq!(bad["error"]["data"]["code"], "invalid_arguments", "{bad}");
        assert!(bad["error"]["message"].as_str().unwrap_or("").contains("client rejected"), "{bad}");
        assert_eq!(validate_calls.len(), 1, "{validate_calls:?}");
        assert_eq!(validate_calls[0]["params"]["registration_id"], reg_id, "{validate_calls:?}");

        let good = rpc.call_raw(
            "message.append",
            json!({ "session_id": sid, "type": "client/card", "content": "accepted", "details": { "ok": true } }),
            |cb| {
                if cb["method"] == "callback.call" && cb["params"]["callback"] == "validate" {
                    validate_calls.push(cb.clone());
                    let ok = cb["params"]["arguments"]["message"]["details"]["ok"].as_bool().unwrap_or(false);
                    return json!({ "valid": ok });
                }
                json!(null)
            },
        );
        assert_eq!(good["result"]["status"], "appended", "{good}");
        assert!(good["result"].get("ok").is_none(), "{good}");
        assert_eq!(validate_calls.len(), 2, "{validate_calls:?}");

        // Releasing the registration removes the custom message type cleanly.
        let rel = rpc.call_raw("registrations.release", json!({ "registration_id": reg_id }), |_| json!(null));
        assert_eq!(rel["result"], json!({}), "{rel}");
        let after_rel = rpc.call_raw("message.list_types", json!({}), |_| json!(null));
        assert_eq!(after_rel["result"]["data"], json!([]), "{after_rel}");

        let reg_entry = rpc.call_raw(
            "ui.register_entry_renderer",
            json!({ "kind": "client/entry", "callbacks": ["render"] }),
            |_| json!(null),
        );
        assert!(reg_entry["result"]["registration_id"].is_string(), "{reg_entry}");

        let reg_xform = rpc.call_raw(
            "ui.register_markdown_transform",
            json!({ "message_types": ["custom"], "callbacks": ["transform"] }),
            |_| json!(null),
        );
        let xform_id = reg_xform["result"]["registration_id"].as_str().unwrap().to_string();

        let mut xform_calls = vec![];
        let xformed =
            rpc.call_raw("ui.transform_markdown", json!({ "text": "hello RAW", "message_type": "custom" }), |cb| {
                if cb["method"] == "callback.call" && cb["params"]["callback"] == "transform" {
                    xform_calls.push(cb.clone());
                    let text = cb["params"]["arguments"]["text"].as_str().unwrap_or("");
                    return json!({ "text": text.replace("RAW", "CLIENT_TRANSFORMED") });
                }
                json!(null)
            });
        assert_eq!(xformed["result"]["text"], "hello CLIENT_TRANSFORMED", "{xformed}");
        assert_eq!(xform_calls.len(), 1, "{xform_calls:?}");
        assert_eq!(xform_calls[0]["params"]["registration_id"], xform_id);

        let untouched =
            rpc.call_raw("ui.transform_markdown", json!({ "text": "hello RAW", "message_type": "user" }), |cb| {
                if cb["method"] == "callback.call" {
                    xform_calls.push(cb.clone());
                }
                json!(null)
            });
        assert_eq!(untouched["result"]["text"], "hello RAW", "{untouched}");
        assert_eq!(xform_calls.len(), 1, "user surface should not invoke custom-only transform");

        let blank_kind =
            rpc.call_raw("ui.register_entry_renderer", json!({ "kind": "   ", "callbacks": ["render"] }), |_| {
                json!(null)
            });
        assert_eq!(blank_kind["error"]["code"], -32602, "{blank_kind}");

        let rel_xform = rpc.call_raw("registrations.release", json!({ "registration_id": xform_id }), |_| json!(null));
        assert_eq!(rel_xform["result"], json!({}), "{rel_xform}");
        let after_xform_rel =
            rpc.call_raw("ui.transform_markdown", json!({ "text": "hello RAW", "message_type": "custom" }), |cb| {
                if cb["method"] == "callback.call" {
                    xform_calls.push(cb.clone());
                }
                json!(null)
            });
        assert_eq!(after_xform_rel["result"]["text"], "hello RAW", "{after_xform_rel}");
        assert_eq!(xform_calls.len(), 1, "released transform must not fire");

        let shut = rpc.call_raw("shutdown", json!({}), |_| json!(null));
        assert!(shut["error"].is_null(), "{shut}");
        rpc.wait_exit();
    })
    .await;
}

/// Ported from reference implementation and
/// `packages/acp/acp/tests/edges.spec.ts:120` (missing capability / invalid payloads fail closed).
#[tokio::test]
async fn rpc_custom_message_capabilities_and_invalid_inputs_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        let script_path = env.home.path().join("faux_c10_neg.json");
        std::fs::write(&script_path, json!([]).to_string()).unwrap();
        let mut cmd = mare_cmd();
        cmd.env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &script_path)
            .current_dir(&env.cwd)
            .args(["--rpc", "--model", "faux/echo", "--no-session"])
            .stderr(Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);

        // Initialize WITHOUT capabilities.callbacks.
        let init =
            rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
        assert!(init["error"].is_null(), "{init}");
        let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

        let empty_type = rpc.call_raw("message.register_type", json!({ "type": "   " }), |_| json!(null));
        assert_eq!(empty_type["error"]["code"], -32602, "{empty_type}");
        let empty_kind =
            rpc.call_raw("ui.register_entry_renderer", json!({ "kind": "", "callbacks": ["render"] }), |_| json!(null));
        assert_eq!(empty_kind["error"]["code"], -32602, "{empty_kind}");
        let no_cb_entry = rpc.call_raw(
            "ui.register_entry_renderer",
            json!({ "kind": "nocap/entry", "callbacks": ["render"] }),
            |_| panic!("unexpected callback when capability absent"),
        );
        assert_eq!(no_cb_entry["error"]["data"]["code"], "unsupported", "{no_cb_entry}");

        let reg =
            rpc.call_raw("message.register_type", json!({ "type": "nocap/card", "callbacks": ["validate"] }), |_| {
                panic!("unexpected callback when capability absent")
            });
        assert_eq!(reg["error"]["data"]["code"], "unsupported", "{reg}");
        assert!(
            reg["error"]["message"].as_str().unwrap_or("").contains("callback.call"),
            "missing capabilities.callbacks must fail explicitly without callback or hang: {reg}"
        );

        let bad_delivery = rpc.call_raw(
            "message.append",
            json!({ "session_id": sid, "type": "free/card", "content": "x", "delivery": { "deliver_as": "bogus" } }),
            |_| json!(null),
        );
        assert!(bad_delivery["error"].is_object(), "invalid deliver_as must fail: {bad_delivery}");

        let msgs = rpc.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
        assert_eq!(msgs["result"]["data"], json!([]), "failed appends must not leak into messages: {msgs}");

        let shut = rpc.call_raw("shutdown", json!({}), |_| json!(null));
        assert!(shut["error"].is_null(), "{shut}");
        rpc.wait_exit();
    })
    .await;
}

/// Ported from reference implementation and
/// `packages/acp/acp/tests/edges.spec.ts:120` (explicit false capabilities & malformed
/// `message.render`/`entry.render` results must neither silently pass nor hang later requests).
#[tokio::test]
async fn rpc_malformed_render_callbacks_and_false_capabilities_fail_explicitly_without_hanging() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    local(async {
        async fn rpc_send(w: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, msg: Value) {
            let line = serde_json::to_string(&msg).unwrap();
            w.write_all(format!("{line}\n").as_bytes()).await.unwrap();
            w.flush().await.unwrap();
        }
        async fn rpc_read(r: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>) -> Value {
            let mut line = String::new();
            let n = r.read_line(&mut line).await.unwrap();
            assert!(n > 0, "unexpected EOF waiting for RPC line");
            serde_json::from_str(&line).unwrap()
        }
        async fn call_with_msg(
            cr: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
            cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
            id: u64,
            method: &str,
            params: Value,
            on_cb: &mut dyn FnMut(&Value) -> Value,
        ) -> Value {
            rpc_send(cw, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
            loop {
                let msg = rpc_read(cr).await;
                if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    return msg;
                }
                if msg.get("method").and_then(|v| v.as_str()).is_some()
                    && let Some(cb_id) = msg.get("id")
                {
                    let res = on_cb(&msg);
                    rpc_send(cw, json!({ "jsonrpc": "2.0", "id": cb_id, "result": res })).await;
                }
            }
        }
        async fn call_with(
            cr: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
            cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
            id: u64,
            method: &str,
            params: Value,
            on_cb: &mut dyn FnMut(&str) -> Value,
        ) -> Value {
            call_with_msg(cr, cw, id, method, params, &mut |msg| {
                on_cb(msg.get("method").and_then(|v| v.as_str()).unwrap_or(""))
            })
            .await
        }

        // 1) Explicit `capabilities.callbacks = false`: registering callback type rejects without callback or hang.
        {
            let env1 = TestEnv::new().await;
            let (h1, _rx1, _ui1) = tui_host(&env1, json!([])).await;
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let srv = tokio::task::spawn_local(mare_cli::rpc::serve(h1.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            let init = call_with(
                &mut cr,
                &mut cw,
                1,
                "initialize",
                json!({ "protocol_version": "3.0", "capabilities": { "callbacks": false } }),
                &mut |_| json!(null),
            )
            .await;
            assert!(init["error"].is_null(), "{init}");
            let denied_entry = call_with(
                &mut cr,
                &mut cw,
                2,
                "ui.register_entry_renderer",
                json!({ "kind": "falsecap/entry", "callbacks": ["render"] }),
                &mut |_| json!(null),
            )
            .await;
            assert_eq!(denied_entry["error"]["data"]["code"], "unsupported", "{denied_entry}");
            let denied_xform = call_with(
                &mut cr,
                &mut cw,
                3,
                "ui.register_markdown_transform",
                json!({ "message_types": ["custom"], "callbacks": ["transform"] }),
                &mut |_| json!(null),
            )
            .await;
            assert_eq!(denied_xform["error"]["data"]["code"], "unsupported", "{denied_xform}");
            let denied = call_with(
                &mut cr,
                &mut cw,
                4,
                "message.register_type",
                json!({ "type": "falsecap/card", "callbacks": ["validate", "render"] }),
                &mut |m| panic!("unexpected callback {m} when capability is false"),
            )
            .await;
            assert_eq!(denied["error"]["data"]["code"], "unsupported", "{denied}");
            assert!(
                denied["error"]["message"].as_str().unwrap_or("").contains("callback.call"),
                "capabilities.callbacks=false must fail explicitly: {denied}"
            );
            drop(cw);
            drop(cr);
            tokio::time::timeout(std::time::Duration::from_secs(5), srv).await.unwrap().unwrap();
            h1.shutdown().await;
        }

        // 2) Malformed `message.render` / `entry.render` results must surface fallback/error blocks
        //    and leave subsequent RPC calls responsive.
        let env2 = TestEnv::new().await;
        let (h, mut rx, _ui) = tui_host(&env2, json!([])).await;
        let (client, server) = tokio::io::duplex(65536);
        let (sr, sw) = tokio::io::split(server);
        let srv = tokio::task::spawn_local(mare_cli::rpc::serve(h.clone(), sr, sw, true, std::future::pending()));
        let (cr, mut cw) = tokio::io::split(client);
        let mut cr = BufReader::new(cr);

        let init = call_with(
            &mut cr,
            &mut cw,
            10,
            "initialize",
            json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }),
            &mut |_| json!(null),
        )
        .await;
        assert!(init["error"].is_null(), "{init}");

        let sess = call_with(&mut cr, &mut cw, 11, "session.create", json!({}), &mut |_| json!(null)).await;
        assert!(sess["error"].is_null(), "{sess}");
        let sid = sess["result"]["session_id"].clone();

        for (id, method, params) in [
            (12, "message.register_type", json!({ "type": "badrender/empty", "callbacks": ["render"] })),
            (13, "message.register_type", json!({ "type": "badrender/malformed", "callbacks": ["render"] })),
            (14, "ui.register_entry_renderer", json!({ "kind": "badentry/malformed", "callbacks": ["render"] })),
            (15, "ui.register_entry_renderer", json!({ "kind": "entry/hide", "callbacks": ["render"] })),
            (16, "ui.register_entry_renderer", json!({ "kind": "entry/null_opt", "callbacks": ["render"] })),
            (17, "ui.register_entry_renderer", json!({ "kind": "entry/bad_shape", "callbacks": ["render"] })),
            (18, "ui.register_entry_renderer", json!({ "kind": "entry/after_bad", "callbacks": ["render"] })),
        ] {
            let reg = call_with(&mut cr, &mut cw, id, method, params, &mut |_| json!(null)).await;
            assert!(reg["result"]["registration_id"].is_string(), "{reg}");
        }

        drain_cmds(&mut rx);
        let r_empty = call_with(
            &mut cr,
            &mut cw,
            19,
            "message.append",
            json!({ "session_id": sid, "type": "badrender/empty", "content": "FALLBACK_BODY" }),
            &mut |_| json!({}),
        )
        .await;
        assert_eq!(r_empty["result"]["status"], "appended", "{r_empty}");
        assert!(r_empty["result"].get("ok").is_none(), "{r_empty}");

        let r_bad_msg = call_with(
            &mut cr,
            &mut cw,
            20,
            "message.append",
            json!({ "session_id": sid, "type": "badrender/malformed", "content": "IGNORED" }),
            &mut |_| json!("not-an-object"),
        )
        .await;
        assert_eq!(r_bad_msg["result"]["status"], "appended", "{r_bad_msg}");
        assert!(r_bad_msg["result"].get("ok").is_none(), "{r_bad_msg}");

        let r_bad_entry = call_with(
            &mut cr,
            &mut cw,
            21,
            "session.add_record",
            json!({ "kind": "badentry/malformed", "data": { "x": 1 }, "session_id": sid, "sync": false }),
            &mut |_| json!("not-an-object"),
        )
        .await;
        assert_eq!(r_bad_entry["result"]["durable"], false, "{r_bad_entry}");
        let blocks = collect_pushed_blocks(&drain_cmds(&mut rx));
        assert!(
            blocks
                .iter()
                .any(|(_, k, t, _)| k == "custom" && t.contains("[badrender/empty]") && t.contains("FALLBACK_BODY")),
            "missing-node message.render must fall back to default custom block: {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|(_, k, t, _)| k == "error" && t.contains("message renderer error (badrender/malformed)")),
            "malformed message.render must emit an error block: {blocks:?}"
        );
        assert!(
            blocks.iter().any(|(_, k, t, _)| k == "error" && t.contains("entry renderer error (badentry/malformed)")),
            "malformed entry.render must emit an error block: {blocks:?}"
        );

        // {"node": null} must hide the entry cleanly without firing expanded = true or pushing an empty block.
        let mut hide_calls = 0usize;
        let r_hide = call_with_msg(
            &mut cr,
            &mut cw,
            22,
            "session.add_record",
            json!({ "kind": "entry/hide", "data": { "hidden": true }, "session_id": sid, "sync": true }),
            &mut |cb| {
                if cb["params"]["callback"] == "render" {
                    hide_calls += 1;
                    assert_eq!(
                        cb["params"]["arguments"]["expanded"], false,
                        "hidden entry must not call expanded: {cb}"
                    );
                }
                json!({ "node": null })
            },
        )
        .await;
        assert_eq!(r_hide["result"]["durable"], true, "{r_hide}");
        assert_eq!(hide_calls, 1, "entry returning {{\"node\": null}} must be called once, got {hide_calls}");
        let hide_cmds = collect_pushed_blocks(&drain_cmds(&mut rx));
        assert!(hide_cmds.is_empty(), "entry returning {{\"node\": null}} must push zero blocks: {hide_cmds:?}");

        // Optional fields serialized as JSON `null` must render without failing node_from.
        let r_null_opt = call_with(
            &mut cr,
            &mut cw,
            23,
            "session.add_record",
            json!({ "kind": "entry/null_opt", "data": { "n": 1 }, "session_id": sid, "sync": true }),
            &mut |_| json!({ "node": { "type": "code", "code": "NULL_OPT_NODE_OK", "lang": null } }),
        )
        .await;
        assert_eq!(r_null_opt["result"]["durable"], true, "{r_null_opt}");

        // Malformed res.node passes JSON Schema but must be caught inside pcall and NOT abort session/loaded replay.
        let r_bad_shape = call_with(
            &mut cr,
            &mut cw,
            24,
            "session.add_record",
            json!({ "kind": "entry/bad_shape", "data": { "bad": true }, "session_id": sid, "sync": true }),
            &mut |_| json!({ "node": { "type": "image", "image_id": 0 } }),
        )
        .await;
        assert_eq!(r_bad_shape["result"]["durable"], true, "{r_bad_shape}");

        let r_after_bad = call_with(
            &mut cr,
            &mut cw,
            25,
            "session.add_record",
            json!({ "kind": "entry/after_bad", "data": { "ok": 1 }, "session_id": sid, "sync": true }),
            &mut |_| json!({ "node": { "type": "markdown", "text": "AFTER_BAD_LIVE_OK" } }),
        )
        .await;
        assert_eq!(r_after_bad["result"]["durable"], true, "{r_after_bad}");

        let live_blocks = collect_pushed_blocks(&drain_cmds(&mut rx));
        assert!(
            live_blocks.iter().any(|(_, k, t, _)| k == "entry" && t.contains("NULL_OPT_NODE_OK")),
            "null optional field Node must render cleanly: {live_blocks:?}"
        );
        assert!(
            live_blocks.iter().any(|(_, k, t, _)| k == "error" && t.contains("entry renderer error (entry/bad_shape)")),
            "malformed res.node must surface an isolated error block: {live_blocks:?}"
        );
        assert!(
            live_blocks.iter().any(|(_, k, t, _)| k == "entry" && t.contains("AFTER_BAD_LIVE_OK")),
            "entry after malformed res.node must render: {live_blocks:?}"
        );

        let types_after = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            call_with(&mut cr, &mut cw, 26, "message.list_types", json!({}), &mut |_| json!(null)),
        )
        .await
        .expect("subsequent message.list_types must not hang");
        assert!(types_after["error"].is_null(), "{types_after}");

        // Replay persisted session via session.create + session.load to verify session/loaded survives malformed res.node.
        let s2 = call_with(&mut cr, &mut cw, 27, "session.create", json!({}), &mut |_| json!(null)).await;
        assert!(s2["error"].is_null(), "{s2}");
        drain_cmds(&mut rx);
        let mut replay_hide_calls = 0usize;
        let resumed = call_with_msg(&mut cr, &mut cw, 28, "session.load", json!({ "session_id": sid }), &mut |cb| {
            let kind = cb["params"]["arguments"]["kind"].as_str().unwrap_or("");
            match kind {
                "entry/hide" => {
                    replay_hide_calls += 1;
                    json!({ "node": null })
                }
                "entry/null_opt" => json!({ "node": { "type": "code", "code": "NULL_OPT_NODE_OK", "lang": null } }),
                "entry/bad_shape" => json!({ "node": { "type": "image", "image_id": 0 } }),
                "entry/after_bad" => json!({ "node": { "type": "markdown", "text": "AFTER_BAD_REPLAY_OK" } }),
                _ => json!({}),
            }
        })
        .await;
        assert!(resumed["error"].is_null(), "{resumed}");
        assert_eq!(
            replay_hide_calls, 1,
            "replayed {{\"node\": null}} entry must call render once, got {replay_hide_calls}"
        );

        let replay_blocks = collect_pushed_blocks(&drain_cmds(&mut rx));
        assert!(
            replay_blocks.iter().any(|(_, k, t, _)| k == "entry" && t.contains("NULL_OPT_NODE_OK")),
            "replay must keep null-optional-field Node: {replay_blocks:?}"
        );
        assert!(
            replay_blocks
                .iter()
                .any(|(_, k, t, _)| k == "error" && t.contains("entry renderer error (entry/bad_shape)")),
            "replay must isolate malformed res.node as error block: {replay_blocks:?}"
        );
        assert!(
            replay_blocks.iter().any(|(_, k, t, _)| k == "entry" && t.contains("AFTER_BAD_REPLAY_OK")),
            "session/loaded replay must continue rendering entries after malformed res.node: {replay_blocks:?}"
        );

        drop(cw);
        drop(cr);
        tokio::time::timeout(std::time::Duration::from_secs(5), srv).await.unwrap().unwrap();
        h.shutdown().await;
    })
    .await;
}
