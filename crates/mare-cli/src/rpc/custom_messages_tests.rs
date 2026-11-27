use super::*;
use crate::rpc::rpc_harness::{HOST_LOCK, TestRpc, rpc_read, rpc_send};

#[tokio::test]
async fn rpc_render_callbacks_and_disconnect_cleanup() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let ui = mare_binding::ui::UiShared::new();
            let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
            *ui.tx.write().unwrap() = Some(ui_tx);
            let host = Host::with_ui(
                mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Tui,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": { "permissions": { "enabled": false } },
                        "peer": { "allow": [
                            "session.create",
                            "message.register_type",
                            "ui.register_entry_renderer",
                            "ui.register_markdown_transform",
                            "ui.transform_markdown",
                            "message.append"
                        ] }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({}),
                    no_user_plugins: true,
                },
                ui,
            )
            .await
            .unwrap();
            assert!(host.kernel.problems().is_empty(), "plugin problems: {:?}", host.kernel.problems());

            let mut rpc = TestRpc::connect_with_primary(host.clone(), false);

            async fn rpc_call_handling_callbacks(
                rpc: &mut TestRpc,
                id: u64,
                method: &str,
                params: J,
                seen_callbacks: &mut Vec<String>,
            ) -> J {
                rpc.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
                loop {
                    let msg = rpc.read().await;
                    if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        return msg;
                    }
                    if let Some(cb_method) = msg.get("method").and_then(|v| v.as_str())
                        && let Some(cb_id) = msg.get("id")
                    {
                        let cb_label = match cb_method {
                            "callback.call" => {
                                let cb_name = msg["params"]["callback"].as_str().unwrap_or("");
                                format!("callback.call:{cb_name}")
                            }
                            _ => cb_method.to_string(),
                        };
                        seen_callbacks.push(cb_label);
                        let result = match cb_method {
                            "callback.call" => match msg["params"]["callback"].as_str().unwrap_or("") {
                                "validate" => json!({ "valid": true }),
                                "render" => {
                                    if msg["params"]["arguments"].get("entry").is_some() {
                                        let expanded =
                                            msg["params"]["arguments"]["expanded"].as_bool().unwrap_or(false);
                                        json!({ "node": if expanded { "ENTRY_EXPANDED" } else { "ENTRY_COMPACT" } })
                                    } else {
                                        let expanded =
                                            msg["params"]["arguments"]["opts"]["expanded"].as_bool().unwrap_or(false);
                                        json!({ "node": if expanded { "MSG_EXPANDED" } else { "MSG_COMPACT" } })
                                    }
                                }
                                "transform" => {
                                    let text = msg["params"]["arguments"]["text"].as_str().unwrap_or("");
                                    json!({ "text": text.replace("RAW", "CLEAN") })
                                }
                                "entry.render" => {
                                    let expanded = msg["params"]["arguments"]["expanded"].as_bool().unwrap_or(false);
                                    json!({ "node": if expanded { "ENTRY_EXPANDED" } else { "ENTRY_COMPACT" } })
                                }
                                "markdown.transform" => {
                                    let text = msg["params"]["arguments"]["text"].as_str().unwrap_or("");
                                    json!({ "text": text.replace("RAW", "CLEAN") })
                                }
                                _ => json!(null),
                            },
                            _ => json!(null),
                        };
                        rpc.send(json!({ "jsonrpc": "2.0", "id": cb_id, "result": result })).await;
                    }
                }
            }

            let mut callbacks = vec![];
            let init = rpc_call_handling_callbacks(
                &mut rpc,
                1,
                "initialize",
                json!({
                    "protocol_version": "3.0",
                    "capabilities": { "callbacks": true }
                }),
                &mut callbacks,
            )
            .await;
            assert!(init["error"].is_null(), "{init}");

            let sess = rpc_call_handling_callbacks(&mut rpc, 100, "session.create", json!({}), &mut callbacks).await;
            assert!(sess["error"].is_null(), "{sess}");
            let sid = sess["result"]["session_id"].as_str().unwrap().to_string();

            let reg_type = rpc_call_handling_callbacks(
                &mut rpc,
                2,
                "message.register_type",
                json!({ "type": "disconnect/card", "session_id": sid, "callbacks": ["validate", "render"] }),
                &mut callbacks,
            )
            .await;
            assert!(reg_type["result"]["registration_id"].as_str().is_some(), "{reg_type}");

            let reg_entry = rpc_call_handling_callbacks(
                &mut rpc,
                3,
                "ui.register_entry_renderer",
                json!({ "kind": "disconnect/entry", "session_id": sid, "callbacks": ["render"] }),
                &mut callbacks,
            )
            .await;
            assert!(reg_entry["result"]["registration_id"].is_string(), "{reg_entry}");

            let reg_xform = rpc_call_handling_callbacks(
                &mut rpc,
                4,
                "ui.register_markdown_transform",
                json!({ "message_types": ["custom"], "session_id": sid, "callbacks": ["transform"] }),
                &mut callbacks,
            )
            .await;
            assert!(reg_xform["result"]["registration_id"].is_string(), "{reg_xform}");

            let app = rpc_call_handling_callbacks(
                &mut rpc,
                5,
                "message.append",
                json!({ "session_id": sid, "type": "disconnect/card", "content": "hi" }),
                &mut callbacks,
            )
            .await;
            assert_eq!(app["result"]["status"], "appended", "{app}");
            assert!(app["result"].get("ok").is_none(), "{app}");

            let peer_rec = rpc_call_handling_callbacks(
                &mut rpc,
                6,
                "session.add_record",
                json!({
                    "kind": "disconnect/entry",
                    "data": { "x": 1 },
                    "session_id": sess["result"]["session_id"],
                    "sync": true,
                }),
                &mut callbacks,
            )
            .await;
            assert_eq!(peer_rec["error"]["code"], -32000, "{peer_rec}");
            assert_eq!(peer_rec["error"]["data"]["code"], "denied", "{peer_rec}");
            assert!(
                peer_rec["error"]["message"].as_str().unwrap_or("").contains("not allowed on this connection"),
                "expected peer rejection on session.add_record: {peer_rec}"
            );

            let mut primary = TestRpc::connect(host.clone());
            let mut primary_callbacks = vec![];
            let p_init = rpc_call_handling_callbacks(
                &mut primary,
                1,
                "initialize",
                json!({ "protocol_version": "3.0", "capabilities": {} }),
                &mut primary_callbacks,
            )
            .await;
            assert!(p_init["error"].is_null(), "{p_init}");

            let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
            let peer_cb_fut = async {
                loop {
                    tokio::select! {
                        _ = &mut done_rx => break,
                        msg = rpc.read() => {
                            if let Some(cb_method) = msg.get("method").and_then(|v| v.as_str())
                                && let Some(cb_id) = msg.get("id")
                            {
                                let cb_label = match cb_method {
                                    "callback.call" => {
                                        let cb_name = msg["params"]["callback"].as_str().unwrap_or("");
                                        format!("callback.call:{cb_name}")
                                    }
                                    _ => cb_method.to_string(),
                                };
                                callbacks.push(cb_label);
                                let result = match cb_method {
                                    "callback.call" if msg["params"]["callback"] == "render" => {
                                        let expanded =
                                            msg["params"]["arguments"]["expanded"].as_bool().unwrap_or(false);
                                        json!({ "node": if expanded { "ENTRY_EXPANDED" } else { "ENTRY_COMPACT" } })
                                    }
                                    _ => json!(null),
                                };
                                rpc.send(json!({ "jsonrpc": "2.0", "id": cb_id, "result": result })).await;
                            }
                        }
                    }
                }
            };
            let primary_rec_fut = async {
                let rec = rpc_call_handling_callbacks(
                    &mut primary,
                    2,
                    "session.add_record",
                    json!({
                        "kind": "disconnect/entry",
                        "data": { "x": 1 },
                        "session_id": sess["result"]["session_id"],
                        "sync": true,
                    }),
                    &mut primary_callbacks,
                )
                .await;
                let _ = done_tx.send(());
                rec
            };
            let (rec, ()) = tokio::join!(primary_rec_fut, peer_cb_fut);
            assert_eq!(rec["result"]["durable"], true, "{rec}");
            assert!(rec["result"]["record_id"].is_string(), "{rec}");
            assert!(rec["result"].get("item_id").is_none(), "{rec}");

            let xform = rpc_call_handling_callbacks(
                &mut rpc,
                7,
                "ui.transform_markdown",
                json!({ "text": "RAW", "message_type": "custom" }),
                &mut callbacks,
            )
            .await;
            assert_eq!(xform["result"]["text"], "CLEAN", "{xform}");

            assert!(callbacks.iter().any(|c| c == "callback.call:validate"), "{callbacks:?}");
            assert!(callbacks.iter().any(|c| c == "callback.call:render"), "{callbacks:?}");
            assert!(callbacks.iter().any(|c| c == "callback.call:transform"), "{callbacks:?}");

            // Disconnect the RPC client and wait for the server loop to dispose conn.ctx.
            rpc.close().await;

            let msg_svc = host.service_table("message").unwrap();
            let types_fn: Function = msg_svc.get("list_types").unwrap();
            let types: Value = types_fn.call_async(msg_svc).await.unwrap();
            let types_json = mare_shared::lua::lua_to_json(host.lua(), types).unwrap();
            assert_eq!(
                types_json,
                json!({ "data": [] }),
                "custom message type must be cleaned up on disconnect: {types_json}"
            );

            let entry_renderer: Value =
                host.lua().load("return mare.ui.get_entry_renderer('disconnect/entry')").eval().unwrap();
            assert!(entry_renderer.is_nil(), "entry renderer must be cleaned up on disconnect");

            let ui_svc = host.service_table("ui").unwrap();
            let xform_fn: Function = ui_svc.get("transform_markdown").unwrap();
            let ctx_lua = json_to_lua(host.lua(), &json!({ "message_type": "custom" })).unwrap();
            let out: mlua::Table = xform_fn.call_async((ui_svc, "RAW", ctx_lua)).await.unwrap();
            assert_eq!(
                out.get::<String>("text").unwrap(),
                "RAW",
                "markdown transform must be cleaned up on disconnect"
            );

            primary.close().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_render_null_malformed_secret_projection_release_and_disconnect() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let ui = mare_binding::ui::UiShared::new();
            let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
            *ui.tx.write().unwrap() = Some(ui_tx);
            let host = Host::with_ui(
                mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Tui,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": { "permissions": { "enabled": false } },
                        "peer": { "allow": [
                            "session.create",
                            "message.register_type",
                            "ui.register_entry_renderer",
                            "ui.register_markdown_transform",
                            "ui.transform_markdown",
                            "message.append"
                        ] }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({}),
                    no_user_plugins: true,
                },
                ui,
            )
            .await
            .unwrap();
            assert!(host.kernel.problems().is_empty(), "plugin problems: {:?}", host.kernel.problems());

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            async fn call(
                cr: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
                cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
                id: u64,
                method: &str,
                params: J,
            ) -> J {
                rpc_send(cw, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
                loop {
                    let msg = rpc_read(cr).await;
                    if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        return msg;
                    }
                }
            }

            let init = call(
                &mut cr,
                &mut cw,
                1,
                "initialize",
                json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }),
            )
            .await;
            let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

            // Register a secret to verify projection in both outgoing callback args and returned render nodes.
            let secret = "MSG_CALLBACK_SECRET_99";
            let secret_ref: String = host
                .lua()
                .load(format!("return mare.projection.register({secret:?}).reference"))
                .eval()
                .unwrap();

            // 1. {"node": null} falls back to default "[null/card]" rendering rather than empty UI node.
            let r_null = call(
                &mut cr,
                &mut cw,
                2,
                "message.register_type",
                json!({ "type": "null/card", "callbacks": ["render"] }),
            )
            .await;
            let null_reg_id = r_null["result"]["registration_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "message.append",
                    "params": { "session_id": sid, "type": "null/card", "content": "null-fallback-body" }
                }),
            )
            .await;
            let mut null_render_calls = 0usize;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg.get("id").and_then(|v| v.as_u64()) == Some(3) {
                    assert!(msg["error"].is_null(), "{msg}");
                    break;
                }
                if msg["method"] == "callback.call" && msg["params"]["callback"] == "render" {
                    null_render_calls += 1;
                    rpc_send(
                        &mut cw,
                        json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "node": null } }),
                    )
                    .await;
                }
            }
            assert!(null_render_calls >= 1);
            let mut pushed = vec![];
            while let Ok(cmd) = ui_rx.try_recv() {
                if let mare_shared::ui::UiCmd::BlockPush { kind, patch, .. } = cmd {
                    pushed.push((kind, format!("{patch:?}")));
                }
            }
            assert!(
                pushed.iter().any(|(k, d)| k == "custom" && d.contains("[null/card]")),
                "returning {{\"node\": null}} must fall back to default [null/card] renderer: {pushed:?}"
            );

            // 2. Secret projection in validate + render callback args AND render return node.
            let r_sec = call(
                &mut cr,
                &mut cw,
                4,
                "message.register_type",
                json!({ "type": "sec/card", "callbacks": ["validate", "render"] }),
            )
            .await;
            assert!(r_sec["error"].is_null(), "{r_sec}");
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "message.append",
                    "params": {
                        "session_id": sid,
                        "type": "sec/card",
                        "content": format!("hello {secret}"),
                        "details": { "token": secret }
                    }
                }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg.get("id").and_then(|v| v.as_u64()) == Some(5) {
                    assert!(msg["error"].is_null(), "{msg}");
                    break;
                }
                if msg["method"] == "callback.call" {
                    let dumped = msg["params"].to_string();
                    assert!(!dumped.contains(secret), "secret leaked in callback.call args: {dumped}");
                    assert!(dumped.contains(&secret_ref), "expected projected ref in callback.call args: {dumped}");
                    let res = match msg["params"]["callback"].as_str() {
                        Some("validate") => json!({ "valid": true }),
                        Some("render") => json!({ "node": format!("rendered {secret}") }),
                        _ => json!(null),
                    };
                    rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": msg["id"], "result": res })).await;
                }
            }
            let mut sec_pushed = vec![];
            while let Ok(cmd) = ui_rx.try_recv() {
                if let mare_shared::ui::UiCmd::BlockPush { kind, patch, .. } = cmd {
                    sec_pushed.push((kind, format!("{patch:?}")));
                }
            }
            assert!(
                sec_pushed.iter().any(|(k, d)| k == "custom" && d.contains(&secret_ref) && !d.contains(secret)),
                "render return node must project secrets: {sec_pushed:?}"
            );

            // 3. Malformed render node must be caught cleanly (error notice), NOT abort session/loaded replay.
            let r_bad = call(
                &mut cr,
                &mut cw,
                6,
                "message.register_type",
                json!({ "type": "badnode/card", "callbacks": ["render"] }),
            )
            .await;
            assert!(r_bad["error"].is_null(), "{r_bad}");
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 7,
                    "method": "message.append",
                    "params": { "session_id": sid, "type": "badnode/card", "content": "first-bad" }
                }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg.get("id").and_then(|v| v.as_u64()) == Some(7) {
                    assert!(msg["error"].is_null(), "{msg}");
                    break;
                }
                if msg["method"] == "callback.call" && msg["params"]["callback"] == "render" {
                    rpc_send(
                        &mut cw,
                        json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "node": { "type": "lines", "items": "bad" } } }),
                    )
                    .await;
                }
            }
            // Also append an ordinary plain message after badnode/card, then switch away and resume sid.
            let after_bad = call(
                &mut cr,
                &mut cw,
                8,
                "message.append",
                json!({ "session_id": sid, "type": "plain/after", "content": "tail-survived-replay" }),
            )
            .await;
            assert!(after_bad["error"].is_null(), "{after_bad}");
            while ui_rx.try_recv().is_ok() {}
            let s2 = call(&mut cr, &mut cw, 9, "session.create", json!({})).await;
            assert!(s2["error"].is_null(), "{s2}");
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 10, "method": "session.load", "params": { "session_id": sid } }),
            )
            .await;
            loop {
                let msg = rpc_read(&mut cr).await;
                if msg.get("id").and_then(|v| v.as_u64()) == Some(10) {
                    assert!(msg["error"].is_null(), "{msg}");
                    break;
                }
                if msg["method"] == "callback.call" && msg["params"]["callback"] == "render" {
                    rpc_send(
                        &mut cw,
                        json!({ "jsonrpc": "2.0", "id": msg["id"], "result": { "node": { "type": "lines", "items": "bad" } } }),
                    )
                    .await;
                }
            }
            let mut replay_pushed = vec![];
            while let Ok(cmd) = ui_rx.try_recv() {
                if let mare_shared::ui::UiCmd::BlockPush { kind, patch, .. } = cmd {
                    replay_pushed.push((kind, format!("{patch:?}")));
                }
            }
            assert!(
                replay_pushed.iter().any(|(k, _)| k == "error"),
                "malformed render node should emit notice instead of breaking replay: {replay_pushed:?}"
            );
            assert!(
                replay_pushed.iter().any(|(k, d)| k == "custom" && d.contains("tail-survived-replay")),
                "malformed render node must not abort subsequent session/loaded replay: {replay_pushed:?}"
            );

            // 4. In-flight validate interrupted by registrations.release and by client disconnect.
            let r_hang = call(
                &mut cr,
                &mut cw,
                11,
                "message.register_type",
                json!({ "type": "hang/card", "callbacks": ["validate"] }),
            )
            .await;
            let hang_reg_id = r_hang["result"]["registration_id"].as_str().unwrap().to_string();
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 12,
                    "method": "message.append",
                    "params": { "session_id": sid, "type": "hang/card", "content": "waiting" }
                }),
            )
            .await;
            let val_cb = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["method"] == "callback.call" && msg["params"]["callback"] == "validate" {
                    break msg;
                }
            };
            rpc_send(
                &mut cw,
                json!({ "jsonrpc": "2.0", "id": 13, "method": "registrations.release", "params": { "registration_id": hang_reg_id } }),
            )
            .await;
            let (mut rel_done, mut app_done, mut saw_cancel) = (false, false, false);
            while !(rel_done && app_done && saw_cancel) {
                let msg = rpc_read(&mut cr).await;
                if msg.get("id").and_then(|v| v.as_u64()) == Some(13) {
                    assert_eq!(msg["result"], json!({}), "{msg}");
                    rel_done = true;
                } else if msg.get("id").and_then(|v| v.as_u64()) == Some(12) {
                    assert_eq!(msg["error"]["data"]["code"], "cancelled", "{msg}");
                    app_done = true;
                } else if msg["method"] == "$/cancel_request" && msg["params"]["id"] == val_cb["id"] {
                    saw_cancel = true;
                }
            }
            let _ = null_reg_id;

            // In-flight validate on a second call interrupted immediately by client disconnect.
            let r_dc = call(
                &mut cr,
                &mut cw,
                14,
                "message.register_type",
                json!({ "type": "dc/card", "callbacks": ["validate"] }),
            )
            .await;
            assert!(r_dc["error"].is_null(), "{r_dc}");
            let mut host_append = Box::pin(host.call_service(
                "message",
                "append",
                mlua::MultiValue::from_vec(vec![
                    mare_shared::lua::json_to_lua(
                        host.lua(),
                        &json!({ "type": "dc/card", "content": "dc-wait" }),
                    )
                    .unwrap(),
                ]),
            ));
            let _dc_cb = tokio::select! {
                _ = &mut host_append => panic!("message.append returned before validate callback"),
                msg = rpc_read(&mut cr) => {
                    assert_eq!(msg["method"], "callback.call");
                    msg
                }
            };
            drop(cw);
            drop(cr);
            let dc_err = tokio::time::timeout(Duration::from_secs(5), host_append)
                .await
                .expect("in-flight validate must unblock on disconnect")
                .unwrap_err();
            assert!(dc_err.to_string().contains("disconnected"), "{dc_err}");
            tokio::time::timeout(Duration::from_secs(5), server_task).await.unwrap().unwrap();
        })
        .await;
}
