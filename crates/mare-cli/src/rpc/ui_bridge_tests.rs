use super::serve::serve_manager;
use super::tests::{HOST_LOCK, rpc_read, rpc_send};
use super::*;

async fn read_reply(cr: &mut (impl AsyncBufReadExt + Unpin), id: i64) -> J {
    loop {
        let msg = rpc_read(cr).await;
        if msg["id"] == id {
            return msg;
        }
    }
}

async fn setup_rpc(
    ui_cap: bool,
) -> (
    tempfile::TempDir,
    tokio::sync::MutexGuard<'static, ()>,
    Rc<RpcRuntime>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    tokio::task::JoinHandle<()>,
    String,
) {
    let host_guard = HOST_LOCK.lock().await;
    mare_shared::ui::install_hooks(mare_tui::hooks());
    let dir = tempfile::tempdir().unwrap();
    let opts = mare_host::HostOptions {
        cwd: dir.path().to_owned(),
        mode: mare_plugin::Mode::Rpc,
        config: json!({
            "model": "faux/echo",
            "plugins": { "permissions": { "enabled": false } }
        }),
        load_init: false,
        project_trusted: true,
        args: json!({ "no_session": true }),
        no_user_plugins: true,
    };
    let manager = mare_host::HostManager::new();
    let rt = manager.create_session(opts.clone()).await.unwrap();
    let sid = rt.session_id.clone();
    let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

    let (client, server) = tokio::io::duplex(131072);
    let (sr, sw) = tokio::io::split(server);
    let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
    let (cr, mut cw) = tokio::io::split(client);
    let mut cr = BufReader::new(cr);

    rpc_send(
        &mut cw,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocol_version": "3.0",
                "capabilities": { "ui": ui_cap, "hooks": true }
            }
        }),
    )
    .await;
    let init = read_reply(&mut cr, 1).await;
    assert!(init.get("error").is_none(), "{init}");
    (dir, host_guard, rpc, cr, cw, task, sid)
}

#[tokio::test]
async fn wire_node_trees_roundtrip_and_reject_paths_closures_and_oversized_images() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, cw, task, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();
            rt.host
                .lua()
                .load(
                    r##"
                    local ui = mare.ui
                    local tree = ui.Split(
                        ui.Block(ui.List({ ui.Line({ ui.Span("item-1"):bold() }) }, { selected = 1 }), { title = "Left" }),
                        ui.Frames({ ui.Line("f1"), ui.Line("f2") }, 120),
                        { ratio = 0.4 }
                    )
                    ui.set_widget("rich", "above-editor", tree, 5)
                    ui.set_status("st", "right", ui.Line({ ui.Span("ok"):fg("#00ff00") }), 1, true)
                    ui.overlay("Help", ui.Block({ ui.Line("body") }, { title = "Title" }))
                    ui.print(ui.Markdown("hello **world**"))
                    ui.set_widget("img", "above-editor", {
                        type = "image",
                        bytes = "\137PNG\r\n",
                        path = "/secret/local.png",
                        max_rows = 4,
                        max_cols = 8
                    }, 6)
                "##,
                )
                .exec()
                .unwrap();

            let mut saw_widget = false;
            let mut saw_status = false;
            let mut saw_overlay = false;
            let mut saw_img = false;
            let mut saw_print = false;
            for _ in 0..30 {
                let msg = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
                match msg["method"].as_str() {
                    Some("event") if msg["params"]["type"] == "ui/print" => {
                        let data = &msg["params"]["data"];
                        assert_eq!(data["node"]["type"], "markdown", "{msg}");
                        assert_eq!(data["node"]["text"], "hello **world**", "{msg}");
                        saw_print = true;
                    }
                    Some("ui.widget") if msg["params"]["widget_id"] == "rich" => {
                        let node = &msg["params"]["node"];
                        assert_eq!(node["type"], "split", "{msg}");
                        assert_eq!(node["left"]["type"], "block", "{msg}");
                        assert_eq!(node["left"]["child"]["type"], "list", "{msg}");
                        assert_eq!(node["right"]["type"], "frames", "{msg}");
                        assert_eq!(msg["params"]["session_id"], sid);
                        saw_widget = true;
                    }
                    Some("ui.status") if msg["params"]["status_id"] == "st" => {
                        assert_eq!(msg["params"]["node"]["type"], "line", "{msg}");
                        assert_eq!(msg["params"]["node"]["spans"][0]["text"], "ok", "{msg}");
                        saw_status = true;
                    }
                    Some("ui.overlay") if msg["params"]["title"] == "Help" => {
                        assert_eq!(msg["params"]["open"], true);
                        assert_eq!(msg["params"]["node"]["type"], "block", "{msg}");
                        saw_overlay = true;
                    }
                    Some("ui.widget") if msg["params"]["widget_id"] == "img" => {
                        let node = &msg["params"]["node"];
                        assert_eq!(node["type"], "image", "{msg}");
                        assert!(node["data"].as_str().is_some(), "expected base64 data: {msg}");
                        assert!(node.get("path").is_none() && node.get("bytes").is_none(), "{msg}");
                        saw_img = true;
                    }
                    _ => {}
                }
                if saw_widget && saw_status && saw_overlay && saw_img && saw_print {
                    break;
                }
            }
            assert!(saw_widget && saw_status && saw_overlay && saw_img && saw_print);

            // Deterministically reject local-only image paths, closures, and > 8 MiB images.
            let bad_path = rt
                .host
                .lua()
                .load(r#"mare.ui.set_widget("bad", "above-editor", { type = "image", path = "/etc/passwd" })"#)
                .exec();
            assert!(bad_path.is_err());
            let bad_fn = rt
                .host
                .lua()
                .load(r#"mare.ui.set_widget("bad2", "above-editor", { type = "line", fn = function() end })"#)
                .exec();
            assert!(bad_fn.is_err());
            let bad_big = rt
                .host
                .lua()
                .load(r#"mare.ui.set_widget("bad3", "above-editor", { type = "image", bytes = string.rep("a", 8 * 1024 * 1024 + 1) })"#)
                .exec();
            assert!(bad_big.is_err());

            let _ = &cw;
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn ui_attach_recovers_shell_takes_over_components_and_detaches_cleanly() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, cr1, cw1, task1, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();
            rt.host
                .lua()
                .load(
                    r#"
                    mare.ui.set_widget("w_persist", "above-editor", mare.ui.Line("saved widget"), 1)
                    mare.ui.editor_hint("draft hint")
                    mare.ui.editor_config({ prompt = ">>" })
                    mare.ui.working("thinking", "step 1", { background = true })
                "#,
                )
                .exec()
                .unwrap();

            // Connect a second frontend that attaches with takeover + new viewport, replacing responder.
            let (client2, server2) = tokio::io::duplex(131072);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 = tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"ui":false}}})).await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());

            // Reject invalid takeover name.
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":sid,"takeover":["widget:"]}})).await;
            assert_eq!(read_reply(&mut cr2, 2).await["error"]["code"], -32602);

            rpc_send(
                &mut cw2,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "ui.attach",
                    "params": {
                        "session_id": sid,
                        "responder": true,
                        "takeover": ["working", "widget:*", "editor"],
                        "viewport": { "width": 120, "height": 42, "supports_images": true }
                    }
                }),
            )
            .await;
            let attached = read_reply(&mut cr2, 3).await;
            assert_eq!(attached["result"]["session_id"], sid, "{attached}");
            assert!(attached["result"].get("ok").is_none(), "must not emit ok: bool: {attached}");
            assert_eq!(attached["result"]["responder"], true);
            let snap = &attached["result"]["snapshot"];
            assert_eq!(snap["widgets"][0]["widget_id"], "w_persist", "{snap}");
            assert_eq!(snap["widgets"][0]["node"]["spans"][0], "saved widget", "{snap}");
            assert_eq!(snap["editor_hint"], "draft hint", "{snap}");
            assert_eq!(snap["editor_config"]["prompt"], ">>", "{snap}");
            assert_eq!(snap["working"]["label"], "thinking", "{snap}");

            // Triggering working/widget now emits `ui/component-updated` to taking-over conn2, not conn1.
            rt.host.lua().load(r#"mare.ui.working("running", "tool", {})"#).exec().unwrap();
            let mut saw_takeover = false;
            for _ in 0..8 {
                let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr2)).await.unwrap();
                if m["method"] == "ui/component-updated" && m["params"]["component"] == "working" {
                    assert_eq!(m["params"]["payload"]["label"], "running", "{m}");
                    saw_takeover = true;
                    break;
                }
            }
            assert!(saw_takeover);

            // `ui.invalidate` triggers resize / redraw.
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":4,"method":"ui.invalidate","params":{"session_id":sid,"viewport":{"width":90,"height":30}}}),
            )
            .await;
            let inv_rep = read_reply(&mut cr2, 4).await;
            assert!(inv_rep.get("error").is_none(), "{inv_rep}");
            assert_eq!(rt.host.ui.width.load(std::sync::atomic::Ordering::Relaxed), 90);

            // `ui.detach` releases AnswererSlot cleanly.
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":5,"method":"ui.detach","params":{"session_id":sid}})).await;
            let det_rep = read_reply(&mut cr2, 5).await;
            assert_eq!(det_rep["result"]["session_id"], sid, "{det_rep}");
            assert!(rt.host.answerer.holder().is_none());

            // Peer connection cannot claim UI attach (`-32601` MethodNotFound on Primary-only method).
            let (p_cli, p_srv) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_srv);
            let ptask = tokio::task::spawn_local(serve_manager(rpc.clone(), psr, psw, false, Some(1234), std::future::pending()));
            let (pcr, mut pcw) = tokio::io::split(p_cli);
            let mut pcr = BufReader::new(pcr);
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{}}})).await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":sid,"responder":true}})).await;
            let perr = read_reply(&mut pcr, 2).await;
            assert_eq!(perr["error"]["code"], -32601, "{perr}");

            drop(pcw);
            drop(pcr);
            drop(cw2);
            drop(cr2);
            drop(cw1);
            drop(cr1);
            tokio::time::timeout(Duration::from_secs(5), ptask).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn keymap_and_editor_sync_lifecycle() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();
            rt.host
                .lua()
                .load(
                    r#"
                    DECLINED_COUNT = 0
                    INVOKED = 0
                    mare.keymap.action("test.decline", function()
                        DECLINED_COUNT = DECLINED_COUNT + 1
                        return false
                    end)
                    mare.keymap.action("test.accept", function(ev)
                        INVOKED = INVOKED + 1
                        LAST_KEY = ev.key
                        return true
                    end)
                "#,
                )
                .exec()
                .unwrap();

            // `ui.get_keymap` lists core + Lua actions and contexts.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":10,"method":"ui.get_keymap","params":{"session_id":sid}})).await;
            let snap = read_reply(&mut cr, 10).await;
            assert!(snap["result"]["contexts"].as_array().unwrap().iter().any(|c| c["name"] == "editor"), "{snap}");
            assert!(snap["result"]["actions"].as_array().unwrap().iter().any(|a| a["name"] == "test.accept"), "{snap}");

            // Reserved `ctrl+c` binding is rejected.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":11,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"ctrl+c","action":"interrupt"}}),
            )
            .await;
            assert_eq!(read_reply(&mut cr, 11).await["error"]["code"], -32602);

            // Set fallback (`global`) and declining top (`editor`) bindings on F6 and invoke by key.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":12,"method":"ui.set_keymap","params":{"session_id":sid,"context":"global","key":"<F6>","action":"test.accept"}}),
            )
            .await;
            let set1 = read_reply(&mut cr, 12).await;
            assert!(set1["result"]["registration_id"].as_str().is_some(), "{set1}");
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":13,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"<F6>","action":"test.decline"}}),
            )
            .await;
            let set2 = read_reply(&mut cr, 13).await;
            assert!(set2["result"]["registration_id"].as_str().is_some(), "{set2}");

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":14,"method":"ui.call_keymap","params":{"session_id":sid,"context":"editor","key":"<F6>"}}),
            )
            .await;
            let inv = read_reply(&mut cr, 14).await;
            assert_eq!(inv["result"]["handled"], true, "{inv}");
            assert_eq!(inv["result"]["context"], "global", "{inv}");
            assert_eq!(inv["result"]["action"], "test.accept", "{inv}");

            // `ui.delete_keymap` handles key sequences (`<C-x>g`) and errors when missing.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":15,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"<C-x>g","action":"test.accept"}}),
            )
            .await;
            assert!(read_reply(&mut cr, 15).await["result"]["registration_id"].is_string());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":16,"method":"ui.delete_keymap","params":{"session_id":sid,"context":"editor","key":"ctrl+x g"}}),
            )
            .await;
            let del_ok = read_reply(&mut cr, 16).await;
            assert_eq!(del_ok["result"]["key"], "ctrl+x g", "{del_ok}");
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":17,"method":"ui.delete_keymap","params":{"session_id":sid,"context":"editor","key":"ctrl+x g"}}),
            )
            .await;
            let del_miss = read_reply(&mut cr, 17).await;
            assert_eq!(del_miss["error"]["data"]["code"], "ui.unknown_keymap", "{del_miss}");

            // Editor sync: `ui.update_editor`, `set_editor { keep_draft = true, prepend = true }`, local path rejection, and `set_external_editor`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":20,"method":"ui.update_editor","params":{"session_id":sid,"text":"hello draft","offset":5,"shell_mode":true,"image_markers":["[Image #1]"]}}),
            )
            .await;
            let ed = read_reply(&mut cr, 20).await;
            assert_eq!(ed["result"]["text"], "hello draft", "{ed}");
            assert_eq!(ed["result"]["offset"], 5, "{ed}");

            // Reject local filesystem path in `image_markers`.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":21,"method":"ui.update_editor","params":{"session_id":sid,"text":"x","image_markers":["/home/secret.png"]}}),
            )
            .await;
            assert_eq!(read_reply(&mut cr, 21).await["error"]["code"], -32602);

            // `keep_draft = true` keeps non-empty buffer; `prepend = true` prepends cleanly.
            rt.host.lua().load(r#"mare.ui.set_editor("ignored", { keep_draft = true })"#).exec().unwrap();
            assert!(!rt.host.shell.editor_is_empty());
            rt.host.lua().load(r#"mare.ui.set_editor("prefix", { prepend = true })"#).exec().unwrap();

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":22,"method":"ui.set_external_editor","params":{"session_id":sid,"action":"start"}})).await;
            let st = read_reply(&mut cr, 22).await;
            assert_eq!(st["result"]["running"], true, "{st}");
            assert_eq!(st["result"]["text"], "prefix\n\nhello draft", "{st}");

            // While external editor is active, `ui.update_editor` is rejected as `busy`.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":220,"method":"ui.update_editor","params":{"session_id":sid,"text":"locked"}})).await;
            let locked = read_reply(&mut cr, 220).await;
            assert_eq!(locked["error"]["data"]["code"], "busy", "{locked}");

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":23,"method":"ui.set_external_editor","params":{"session_id":sid,"action":"commit","text":"final text"}})).await;
            let cm = read_reply(&mut cr, 23).await;
            assert_eq!(cm["result"]["running"], false, "{cm}");
            assert_eq!(cm["result"]["text"], "final text", "{cm}");

            // `ui.send_editor` submits current draft and clears editor buffer.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":24,"method":"ui.send_editor","params":{"session_id":sid}})).await;
            let sub = read_reply(&mut cr, 24).await;
            assert!(sub.get("error").is_none(), "{sub}");
            assert!(rt.host.shell.editor_is_empty());

            // Detaching removes connection-owned runtime keymap handles (`<F6>` goes away).
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":25,"method":"ui.detach","params":{"session_id":sid}})).await;
            let det = read_reply(&mut cr, 25).await;
            assert_eq!(det["result"]["session_id"], sid, "{det}");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":26,"method":"ui.call_keymap","params":{"session_id":sid,"context":"editor","key":"<F6>"}})).await;
            let after = read_reply(&mut cr, 26).await;
            assert_eq!(after["result"]["handled"], false, "{after}");

            let _ = &cw;
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn in_place_new_takeover_cancels_pending_input() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr1, mut cw1, task1, old_sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&old_sid).unwrap();
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":old_sid,"responder":true}})).await;
            assert!(read_reply(&mut cr1, 10).await.get("error").is_none());
            let host = rt.host.clone();
            let old_input = tokio::task::spawn_local(async move {
                host.lua().load(r#"return mare.ui.input({ title = "Old session" })"#).eval_async::<mlua::Value>().await
            });
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr1)).await.unwrap();
                if msg["method"] == "ui.input" {
                    assert_eq!(msg["params"]["session_id"], old_sid);
                    break;
                }
            }
            let service = rt.host.service_table("session").unwrap();
            rt.host.lua().globals().set("TRANSITION_SVC", service).unwrap();
            let result = rt.host.lua().load(r#"
                local info = TRANSITION_SVC:info()
                return TRANSITION_SVC:transition({ mode = "new" }, { session_id = info.session_id, session_epoch = info.session_epoch })
            "#).call_async::<mlua::Table>(()).await.unwrap();
            assert_eq!(result.get::<String>("status").unwrap(), "applied");
            let new_sid = rpc.manager.active_ids().into_iter().find(|id| id != &old_sid).unwrap();
            let host = rt.host.clone();
            let input = tokio::task::spawn_local(async move {
                host.lua().load(r#"return mare.ui.input({ title = "New session" })"#).eval_async::<mlua::Value>().await
            });
            loop {
                let msg = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr1)).await.unwrap();
                if msg["method"] == "ui.input" {
                    assert_eq!(msg["params"]["session_id"], new_sid);
                    break;
                }
            }
            let (client2, server2) = tokio::io::duplex(131072);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 = tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"ui":false}}})).await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":new_sid,"responder":true}})).await;
            assert!(read_reply(&mut cr2, 2).await.get("error").is_none());
            let _ = tokio::time::timeout(Duration::from_secs(2), input).await.expect("old input must be cancelled").unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(2), old_input).await.expect("pre-transition input must not be orphaned").unwrap();
            drop((cw1, cr1, cw2, cr2));
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        }).await;
}

#[tokio::test]
async fn ui_attach_responder_and_detach_does_not_leak_caps_ui_to_new_session() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_rpc(false).await;
            let rt1 = rpc.manager.session(&s1).unwrap();
            assert!(rt1.host.answerer.holder().is_none(), "capabilities.ui = false must not auto-claim answerer");

            // Attach as responder to s1 only, verify s1 UI works, then detach.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":s1,"responder":true}}),
            )
            .await;
            let att = read_reply(&mut cr, 10).await;
            assert_eq!(att["result"]["responder"], json!(true), "{att}");
            assert!(rt1.host.answerer.holder().is_some());

            let h1 = rt1.host.clone();
            let confirm_task = tokio::task::spawn_local(async move {
                h1.lua().load(r#"return mare.ui.confirm("ok?")"#).eval_async::<Option<bool>>().await.unwrap()
            });
            let req = loop {
                let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
                if m["method"] == "ui.confirm" {
                    break m;
                }
            };
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":req["id"],"result":{"confirmed":true}})).await;
            assert_eq!(confirm_task.await.unwrap(), Some(true));

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":11,"method":"ui.detach","params":{"session_id":s1}})).await;
            assert!(read_reply(&mut cr, 11).await.get("error").is_none());
            assert!(rt1.host.answerer.holder().is_none());

            // Creating a new session s2 on the same connection must NOT auto-bind it as UI answerer.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":12,"method":"session.create","params":{}})).await;
            let created = read_reply(&mut cr, 12).await;
            assert!(created.get("error").is_none(), "{created}");
            let s2 = created["result"]["session_id"].as_str().unwrap().to_string();
            let rt2 = rpc.manager.session(&s2).unwrap();
            assert!(
                rt2.host.answerer.holder().is_none(),
                "ui.attach on s1 must not mutate connection caps.ui and hijack s2 answerer"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn in_place_transition_editor_mirror_callbacks_use_live_session_id() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, old_sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&old_sid).unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":old_sid,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr, 10).await.get("error").is_none());

            let service = rt.host.service_table("session").unwrap();
            rt.host.lua().globals().set("TRANSITION_SVC_ED", service).unwrap();
            let result = rt
                .host
                .lua()
                .load(
                    r#"
                    local info = TRANSITION_SVC_ED:info()
                    return TRANSITION_SVC_ED:transition({ mode = "new" }, { session_id = info.session_id, session_epoch = info.session_epoch })
                    "#,
                )
                .call_async::<mlua::Table>(())
                .await
                .unwrap();
            assert_eq!(result.get::<String>("status").unwrap(), "applied");
            let new_sid = rpc.manager.active_ids().into_iter().find(|id| id != &old_sid).unwrap();

            // Invoke the host's installed `frontend.update_editor` mirror callback after rekey.
            rt.host
                .lua()
                .load(r#"assert(mare.ui.frontend.update_editor("after rekey", {}))"#)
                .exec()
                .unwrap();

            let mut got_sid = None;
            for _ in 0..12 {
                let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
                if m["method"] == "ui.editor_state" && m["params"]["text"] == "after rekey" {
                    got_sid = m["params"]["session_id"].as_str().map(str::to_string);
                    break;
                }
            }
            assert_eq!(
                got_sid.as_deref(),
                Some(new_sid.as_str()),
                "update_editor mirror must emit live new session_id after in-place transition"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn ui_attach_responder_survives_reattach_and_unrelated_set_capabilities_and_respects_ui_withdrawal() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, s1) = setup_rpc(false).await;
            let rt1 = rpc.manager.session(&s1).unwrap();

            // 1. Attach as per-session responder on s1 while connection caps.ui == false.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":s1,"responder":true}}),
            )
            .await;
            let att = read_reply(&mut cr, 10).await;
            assert_eq!(att["result"]["responder"], json!(true), "{att}");

            // 2. Re-attaching on s1 to update viewport/takeover (omitting `responder`) must keep responder active.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":11,"method":"ui.attach","params":{"session_id":s1,"viewport":{"width":100,"height":30}}}),
            )
            .await;
            let reatt = read_reply(&mut cr, 11).await;
            assert_eq!(
                reatt["result"]["responder"],
                json!(true),
                "omitting responder on re-attach must preserve existing per-session responder: {reatt}"
            );

            let h1 = rt1.host.clone();
            let confirm_reatt = tokio::task::spawn_local(async move {
                h1.lua().load(r#"return mare.ui.confirm("after reattach")"#).eval_async::<Option<bool>>().await.unwrap()
            });
            let req = loop {
                let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
                if m["method"] == "ui.confirm" {
                    break m;
                }
            };
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":req["id"],"result":{"confirmed":true}})).await;
            assert_eq!(confirm_reatt.await.unwrap(), Some(true));

            // 3. Updating unrelated connection capabilities (keeping caps.ui == false) must not evict s1's
            //    per-session responder lease, nor leak caps.ui to newly created s2.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":12,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 12).await.get("error").is_none());

            let h1 = rt1.host.clone();
            let confirm_caps = tokio::task::spawn_local(async move {
                h1.lua()
                    .load(r#"return mare.ui.confirm("after set_capabilities")"#)
                    .eval_async::<Option<bool>>()
                    .await
                    .unwrap()
            });
            let req = loop {
                let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
                if m["method"] == "ui.confirm" {
                    break m;
                }
            };
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":req["id"],"result":{"confirmed":true}})).await;
            assert_eq!(confirm_caps.await.unwrap(), Some(true));

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":13,"method":"session.create","params":{}})).await;
            let created = read_reply(&mut cr, 13).await;
            let s2 = created["result"]["session_id"].as_str().unwrap().to_string();
            let rt2 = rpc.manager.session(&s2).unwrap();
            assert!(rt2.host.answerer.holder().is_none(), "s2 must remain unaffected while caps.ui == false");

            // 4. Explicitly enabling then withdrawing connection-level `ui` MUST withdraw s1's responder too.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":14,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":true,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 14).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":15,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 15).await.get("error").is_none());
            assert!(rt1.host.answerer.holder().is_none(), "explicit ui withdrawal must release s1 answerer");
            assert!(rt2.host.answerer.holder().is_none(), "explicit ui withdrawal must release s2 answerer");

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn losing_can_answer_and_disconnect_clear_responder_and_release_answerer() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            mare_shared::ui::install_hooks(mare_tui::hooks());
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "webserver": { "remote_admin": true }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            let (rem_client, rem_server) = tokio::io::duplex(131072);
            let (rsr, rsw) = tokio::io::split(rem_server);
            let rem_task = tokio::task::spawn_local(super::serve::serve_inner(
                rpc.clone(),
                rsr,
                rsw,
                Principal::Owner { remote_admin: false },
                false,
                None,
                std::future::pending(),
            ));
            let (rcr, mut rcw) = tokio::io::split(rem_client);
            let mut rcr = BufReader::new(rcr);

            rpc_send(
                &mut rcw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": mare_protocol::PROTOCOL_VERSION,
                        "capabilities": { "callbacks": true, "ui": false }
                    }
                }),
            )
            .await;
            assert!(read_reply(&mut rcr, 1).await.get("error").is_none());

            let set_remote_admin = |enabled: bool| {
                let mut cfg = rt.host.kernel.config();
                cfg["webserver"] = json!({ "remote_admin": enabled });
                rt.host.kernel.set_config(cfg);
            };

            // 1. Acquire per-session UI responder, then revoke `remote_admin` and re-attach without `responder`.
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":sid,"responder":true}}),
            )
            .await;
            assert_eq!(read_reply(&mut rcr, 10).await["result"]["responder"], json!(true));
            let holder = rt.host.answerer.holder().expect("remote admin must hold answerer");
            assert!(rt.host.shell.is_responder(holder));

            set_remote_admin(false);
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":11,"method":"ui.attach","params":{"session_id":sid,"viewport":{"width":90,"height":30}}}),
            )
            .await;
            let reatt = read_reply(&mut rcr, 11).await;
            assert!(reatt.get("error").is_none(), "{reatt}");
            assert_eq!(reatt["result"]["responder"], json!(false), "{reatt}");
            assert!(!rt.host.shell.is_responder(holder), "re-attach without answerer authority must clear responder");
            assert!(
                rt.host.answerer.holder().is_none(),
                "re-attach dropping responder from true to false must release host.answerer"
            );

            // 2. Re-acquire responder, revoke `remote_admin`, and call `connection.set_capabilities`.
            set_remote_admin(true);
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":12,"method":"ui.attach","params":{"session_id":sid,"responder":true}}),
            )
            .await;
            assert_eq!(read_reply(&mut rcr, 12).await["result"]["responder"], json!(true));
            assert_eq!(rt.host.answerer.holder(), Some(holder));
            assert!(rt.host.shell.is_responder(holder));

            set_remote_admin(false);
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":13,"method":"connection.set_capabilities","params":{"capabilities":{"callbacks":true,"ui":false,"approvals":false,"elicitation":false}}}),
            )
            .await;
            assert!(read_reply(&mut rcr, 13).await.get("error").is_none());
            assert!(
                !rt.host.shell.is_responder(holder),
                "losing can_answer during set_capabilities must clear per-session responder"
            );
            assert!(
                rt.host.answerer.holder().is_none(),
                "losing can_answer during set_capabilities must release host.answerer"
            );

            // 3. Re-acquire responder and disconnect: responder and answerer must both be cleared.
            set_remote_admin(true);
            rpc_send(
                &mut rcw,
                json!({"jsonrpc":"2.0","id":14,"method":"ui.attach","params":{"session_id":sid,"responder":true}}),
            )
            .await;
            assert_eq!(read_reply(&mut rcr, 14).await["result"]["responder"], json!(true));
            assert_eq!(rt.host.answerer.holder(), Some(holder));
            assert!(rt.host.shell.is_responder(holder));

            drop((rcw, rcr));
            tokio::time::timeout(Duration::from_secs(5), rem_task).await.unwrap().unwrap();
            assert!(!rt.host.shell.is_responder(holder), "disconnect must clear responder");
            assert!(!rt.host.shell.is_attached(holder), "disconnect must detach shell");
            assert!(rt.host.answerer.holder().is_none(), "disconnect must release host.answerer");

            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn on_revoke_and_ui_detach_isolated_by_session_id() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr1, mut cw1, task1, s1) = setup_rpc(true).await;
            let mut opts2 = rpc.clone_base_opts(None).unwrap();
            opts2.args = json!({ "no_session": true });
            let rt2 = rpc.manager.create_session(opts2).await.unwrap();
            let s2 = rt2.session_id.clone();

            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":s1,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr1, 10).await.get("error").is_none());
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":11,"method":"ui.attach","params":{"session_id":s2,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr1, 11).await.get("error").is_none());

            // Start a pending `mare.ui.input` on S2.
            let rt2_c = rt2.clone();
            let input_task = tokio::task::spawn_local(async move {
                rt2_c
                    .host
                    .lua()
                    .load(r#"return mare.ui.input({ title = "Need S2 input", placeholder = "init" })"#)
                    .eval_async::<mlua::Value>()
                    .await
            });
            let cb_req = loop {
                let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr1)).await.unwrap();
                if m["method"] == "ui.input" {
                    break m;
                }
            };
            let cb_id = cb_req["id"].clone();
            assert_eq!(cb_req["params"]["session_id"], s2, "Must carry session_id: {cb_req}");

            // Detaching UI from S1 must NOT cancel S2 pending ui.input or wipe conn1 caps.
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":12,"method":"ui.detach","params":{"session_id":s1}})).await;
            assert!(read_reply(&mut cr1, 12).await.get("error").is_none());
            tokio::task::yield_now().await;
            assert!(!input_task.is_finished(), "ui.detach on S1 must not cancel S2 pending ui.input");

            // Reply to S2 ui.input and confirm it completes normally.
            rpc_send(&mut cw1, json!({"jsonrpc":"2.0","id":cb_id,"result":{"text":"s2 answer"}})).await;
            let val = tokio::time::timeout(Duration::from_secs(2), input_task).await.unwrap().unwrap().unwrap();
            assert_eq!(val.as_string().map(|s| s.to_string_lossy()).as_deref(), Some("s2 answer"));

            drop(cw1);
            drop(cr1);
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn peer_cannot_bypass_primary_via_ui_or_keymap() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } },
                    "peer": { "allow": ["ui.get_keymap", "ui.invalidate", "ui.*", "keymap.*"] }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            let (p_cli, p_srv) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_srv);
            let ptask = tokio::task::spawn_local(serve_manager(
                rpc.clone(),
                psr,
                psw,
                false,
                Some(999),
                std::future::pending(),
            ));
            let (pcr, mut pcw) = tokio::io::split(p_cli);
            let mut pcr = BufReader::new(pcr);

            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{}}}))
                .await;
            assert!(read_reply(&mut pcr, 1).await.get("error").is_none());

            for (id, method, params, expected_code, expected_err) in [
                (2, "ui.attach", json!({"session_id": sid, "responder": false, "takeover": ["editor"]}), -32601, None),
                (3, "ui.update_editor", json!({"session_id": sid, "text": "pwned"}), -32000, Some("denied")),
                (4, "ui.send_editor", json!({"session_id": sid, "text": "pwned prompt"}), -32000, Some("denied")),
                (5, "ui.call_keymap", json!({"session_id": sid, "action": "interrupt"}), -32000, Some("denied")),
                (6, "ui.set_external_editor", json!({"session_id": sid, "action": "start"}), -32000, Some("denied")),
                (
                    7,
                    "ui.set_keymap",
                    json!({"session_id": sid, "context": "editor", "key": "<F7>", "action": "redraw"}),
                    -32000,
                    Some("denied"),
                ),
                (
                    8,
                    "ui.delete_keymap",
                    json!({"session_id": sid, "context": "editor", "key": "<F7>"}),
                    -32000,
                    Some("denied"),
                ),
                (9, "ui.invalidate", json!({"session_id": sid, "component": "status"}), -32000, Some("denied")),
            ] {
                rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await;
                let reply = read_reply(&mut pcr, id).await;
                assert_eq!(reply["error"]["code"], expected_code, "peer must be rejected for `{method}`: {reply}");
                if let Some(code_str) = expected_err {
                    assert_eq!(reply["error"]["data"]["code"], code_str, "{reply}");
                }
            }
            // Read-only `ui.get_keymap` IS permitted for peer when granted by exact name (while wildcards and primary methods stay denied).
            rpc_send(&mut pcw, json!({"jsonrpc":"2.0","id":10,"method":"ui.get_keymap","params":{"session_id":sid}}))
                .await;
            let snap_rep = read_reply(&mut pcr, 10).await;
            assert!(snap_rep.get("error").is_none(), "{snap_rep}");

            drop(pcw);
            drop(pcr);
            tokio::time::timeout(Duration::from_secs(5), ptask).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn editor_submit_validates_before_clearing_draft_and_preserves_attachments() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();
            rt.host
                .shell
                .update_editor_input(
                    Some(&sid),
                    "keep this draft".into(),
                    Some(4),
                    None,
                    Some(vec!["[Image #1]".into()]),
                    None,
                )
                .unwrap();

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.send_editor","params":{"session_id":sid,"text":"   "}}),
            )
            .await;
            let err = read_reply(&mut cr, 10).await;
            assert_eq!(err["error"]["code"], -32602, "{err}");
            assert!(!rt.host.shell.editor_is_empty(), "draft must survive failed validation!");

            // Metadata-only shell.editor_attachments with empty text and no `attachments` payload MUST refuse
            // `invalid_arguments` and preserve the shell draft and attachment metadata.
            rt.host
                .shell
                .update_editor_input(
                    Some(&sid),
                    "".into(),
                    None,
                    None,
                    None,
                    Some(vec![mare_protocol::UiEditorAttachment {
                        attachment_id: "att-1".into(),
                        label: Some("[Image #1]".into()),
                        mime_type: Some("image/png".into()),
                        width: Some(10),
                        height: Some(10),
                    }]),
                )
                .unwrap();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":11,"method":"ui.send_editor","params":{"session_id":sid}}))
                .await;
            let meta_only_err = read_reply(&mut cr, 11).await;
            assert_eq!(
                meta_only_err["error"]["code"], -32602,
                "metadata-only empty submission must refuse invalid_arguments: {meta_only_err}"
            );
            assert_eq!(meta_only_err["error"]["data"]["code"], "invalid_arguments", "{meta_only_err}");
            assert_eq!(rt.host.shell.peek_editor().1.len(), 1, "metadata attachments must survive refused submit");

            // Submitting `ui.send_editor` with real `{ data, mime }` attachment payload routes via `build_input_opts`
            // into the agent message history / events and clears the shell attachment metadata on success.
            let png_b64 =
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 12,
                    "method": "ui.send_editor",
                    "params": {
                        "session_id": sid,
                        "attachments": [{ "kind": "image", "mime": "image/png", "data": png_b64 }]
                    }
                }),
            )
            .await;
            let mut saw_image_event = false;
            let ok_att = loop {
                let msg = rpc_read(&mut cr).await;
                if msg["id"] == 12 {
                    break msg;
                }
                if msg["method"] == "event" && msg.to_string().contains(png_b64) {
                    saw_image_event = true;
                }
            };
            assert!(ok_att.get("error").is_none(), "real attachment submission failed: {ok_att}");
            if let Some(turn_id) = ok_att["result"]["result"]["turn_id"].as_str() {
                let _ = call_service(&rt.host, "agent", "wait", vec![json!(turn_id)]).await.unwrap();
            }
            assert!(rt.host.shell.peek_editor().1.is_empty(), "submitted shell attachment metadata must be cleared");
            let msgs = call_service(&rt.host, "agent", "messages", vec![]).await.unwrap();
            let msgs_str = serde_json::to_string(&msgs).unwrap();
            assert!(
                msgs_str.contains(png_b64) && msgs_str.contains("image/png"),
                "agent messages must contain submitted image block via build_input_opts: {msgs}"
            );
            assert!(saw_image_event, "submitted image payload must reach emitted session events");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[test]
fn denied_uses_registered_code_not_internal() {
    let err = ui_bridge::denied_for_test("nope");
    let wire = serde_json::to_value(err.normalized()).unwrap();
    assert_eq!(wire["code"], -32000, "{wire}");
    assert_eq!(wire["data"]["code"], "denied", "{wire}");
}

#[tokio::test]
async fn responder_false_takeover_receives_updates_without_double_delivery() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, _cr1, _cw1, task1, sid) = setup_rpc(false).await;
            let rt = rpc.manager.session(&sid).unwrap();
            let (c2, s2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(s2);
            let task2 = tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(&mut cw2, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"ui":false}}})).await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());

            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":sid,"responder":false,"takeover":["working","widget:*"]}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 2).await.get("error").is_none());

            rt.host.lua().load(r#"mare.ui.set_widget("w2", "above-editor", mare.ui.Line("hello"), 1)"#).exec().unwrap();
            let mut updates = 0;
            let mut ui_widgets = 0;
            while let Ok(msg) = tokio::time::timeout(Duration::from_millis(300), rpc_read(&mut cr2)).await {
                if msg["method"] == "ui/component-updated" && msg["params"]["component"] == "widget:w2" {
                    updates += 1;
                } else if msg["method"] == "ui.widget" {
                    ui_widgets += 1;
                }
            }
            assert_eq!(updates, 1, "responder=false takeover must receive ui/component-updated once");
            assert_eq!(ui_widgets, 0, "responder=false takeover must not receive ui.widget");

            drop(cw2);
            drop(cr2);
            drop(_cw1);
            drop(_cr1);
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn ui_detach_preserves_approvals_and_session_close_cancels_ui_and_external_editor_blocks_callback() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            mare_shared::ui::install_hooks(mare_tui::hooks());
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt1 = manager.create_session(opts.clone()).await.unwrap();
            let s1 = rt1.session_id.clone();
            let mut opts2 = opts.clone();
            opts2.args = json!({ "no_session": true });
            let rt2 = manager.create_session(opts2).await.unwrap();
            let s2 = rt2.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt1.host.clone(), Some(s1.clone()), false);

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"ui":true,"approvals":true}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            // Attach UI on S1 with approvals enabled, start external editor, verify callback ui.editor is busy.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":s1,"responder":true}})).await;
            assert!(read_reply(&mut cr, 2).await.get("error").is_none());
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":3,"method":"ui.set_external_editor","params":{"session_id":s1,"action":"start"}})).await;
            assert_eq!(read_reply(&mut cr, 3).await["result"]["running"], true);
            let (_, ed_err): (mlua::Value, Option<String>) = rt1
                .host
                .lua()
                .load(r#"return mare.ui.editor({ title = "cb" })"#)
                .eval_async()
                .await
                .unwrap();
            assert!(ed_err.as_deref().is_some_and(|e| e.contains("active")), "external editor must block callback editor: {ed_err:?}");
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":4,"method":"ui.set_external_editor","params":{"session_id":s1,"action":"cancel"}})).await;
            assert_eq!(read_reply(&mut cr, 4).await["result"]["running"], false);

            // Detaching UI from S1 keeps AnswererSlot for approvals on S1.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":5,"method":"ui.detach","params":{"session_id":s1}})).await;
            assert!(read_reply(&mut cr, 5).await.get("error").is_none());
            assert!(rt1.host.answerer.holder().is_some(), "approvals capability must keep answerer slot on S1");
            assert!(!rt1.host.answerer.has_saved_ui(rt1.host.answerer.holder().unwrap()));

            // Closing S2 via session.close cancels pending ui.input on S2.
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":6,"method":"ui.attach","params":{"session_id":s2,"responder":true}})).await;
            assert!(read_reply(&mut cr, 6).await.get("error").is_none());
            let rt2_c = rt2.clone();
            let inp = tokio::task::spawn_local(async move {
                rt2_c
                    .host
                    .lua()
                    .load(r#"return mare.ui.input({ title = "S2 Close Test" })"#)
                    .eval_async::<(mlua::Value, Option<String>)>()
                    .await
            });
            loop {
                let m = tokio::time::timeout(Duration::from_secs(2), rpc_read(&mut cr)).await.unwrap();
                if m["method"] == "ui.input" {
                    assert_eq!(m["params"]["session_id"], s2);
                    break;
                }
            }
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":7,"method":"session.close","params":{"session_id":s2}})).await;
            assert!(read_reply(&mut cr, 7).await.get("error").is_none());
            let (_v, reason) = tokio::time::timeout(Duration::from_secs(2), inp).await.unwrap().unwrap().unwrap();
            assert_eq!(reason.as_deref(), Some("unavailable"), "session.close must cancel pending ui.input cleanly");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn legacy_unknown_fields_are_rejected_without_default_session_fallback() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let host_guard = HOST_LOCK.lock().await;
            mare_shared::ui::install_hooks(mare_tui::hooks());
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt1 = manager.create_session(opts.clone()).await.unwrap();
            let s1 = rt1.session_id.clone();
            let mut opts2 = opts.clone();
            opts2.args = json!({ "no_session": true });
            let rt2 = manager.create_session(opts2).await.unwrap();
            let s2 = rt2.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt1.host.clone(), Some(s1.clone()), false);

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"ui":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            // 1) Legacy `sessionId` or unknown camelCase fields must NEVER silently attach/subscribe/mutate S1 (default session).
            for (idx, (method, params)) in [
                ("ui.attach", json!({ "sessionId": s2, "responder": true })),
                ("ui.attach", json!({ "session_id": s2, "responder": true, "legacyTakeover": true })),
                ("ui.get_select_preview", json!({ "sessionId": s2, "selectId": "sel-1", "index": 0 })),
                ("events.subscribe", json!({ "sessionId": s2 })),
                ("agent.get", json!({ "sessionId": s2 })),
                ("ui.update_editor", json!({ "sessionId": s2, "text": "legacy-leak" })),
                ("ui.set_external_editor", json!({ "sessionId": s2, "action": "start" })),
            ]
            .into_iter()
            .enumerate()
            {
                let id = 10 + idx as i64;
                rpc_send(&mut cw, json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })).await;
                let reply = read_reply(&mut cr, id).await;
                assert_eq!(
                    reply["error"]["code"], -32602,
                    "{method} with legacy/unknown fields must fail with -32602 instead of falling back: {reply}"
                );
            }
            assert!(rt1.host.answerer.holder().is_none(), "rejected ui.attach must not attach default session S1");
            assert!(rt2.host.answerer.holder().is_none(), "rejected ui.attach must not attach S2");
            assert_eq!(rt1.host.shell.peek_editor().0, "", "rejected ui.update_editor must not mutate S1 editor");
            assert!(
                !rt1.host.shell.is_external_editor_running(),
                "rejected ui.set_external_editor must not start S1 external editor"
            );

            // 2) Attach both sessions using canonical `session_id`, then ensure `ui.detach` with legacy `sessionId`
            // rejects cleanly and does NOT silently detach default session S1 or S2.
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":30,"method":"ui.attach","params":{"session_id":s1,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr, 30).await.get("error").is_none());
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":31,"method":"ui.attach","params":{"session_id":s2,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr, 31).await.get("error").is_none());

            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":32,"method":"ui.detach","params":{"sessionId":s2}})).await;
            let detach_err = read_reply(&mut cr, 32).await;
            assert_eq!(detach_err["error"]["code"], -32602, "ui.detach with legacy sessionId must fail: {detach_err}");
            assert!(rt1.host.answerer.holder().is_some(), "rejected ui.detach must not detach default session S1");
            assert!(rt2.host.answerer.holder().is_some(), "rejected ui.detach must not detach S2");

            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
            drop(host_guard);
        })
        .await;
}

#[tokio::test]
async fn ui_set_keymap_contract_supports_priority_and_rejects_non_number_priority() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();
            rt.host
                .lua()
                .load(
                    r#"
                    mare.keymap.action("p.low", function() end, { contexts = { "editor" }, desc = "low" })
                    mare.keymap.action("p.high", function() end, { contexts = { "editor" }, desc = "high" })
                    "#,
                )
                .exec()
                .unwrap();

            // Connect a second client so distinct connections (distinct owners) register competing priorities.
            let (client2, server2) = tokio::io::duplex(131072);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 = tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"ui":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());

            // Reverse order check (high=20 registered BEFORE low=5 by a distinct owner): high still wins!
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"<F7>","action":"p.high","priority":20}}),
            )
            .await;
            assert!(read_reply(&mut cr, 10).await["result"]["registration_id"].is_string());

            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":11,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"<F7>","action":"p.low","priority":5}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 11).await["result"]["registration_id"].is_string());

            // Also verify forward order on `<F8>` (low=5 first, then high=20 from distinct owner).
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":12,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"<F8>","action":"p.low","priority":5}}),
            )
            .await;
            assert!(read_reply(&mut cr, 12).await["result"]["registration_id"].is_string());
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":13,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"<F8>","action":"p.high","priority":20}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 13).await["result"]["registration_id"].is_string());

            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":14,"method":"ui.set_keymap","params":{"session_id":sid,"context":"editor","key":"<F7>","action":"p.low","priority":false}}),
            )
            .await;
            assert_eq!(read_reply(&mut cr, 14).await["error"]["code"], -32602);

            let (f7_winner, f8_winner, stored_prios): (String, String, Vec<i64>) = rt
                .host
                .lua()
                .load(
                    r#"
                    local f7, f8 = "", ""
                    local prios = {}
                    for _, b in ipairs(mare.keymap.list()) do
                        if b.key == "f7" and b.context == "editor" and f7 == "" then f7 = b.action end
                        if b.key == "f8" and b.context == "editor" and f8 == "" then f8 = b.action end
                        if b.key == "f7" and b.context == "editor" then prios[#prios + 1] = b.priority end
                    end
                    return f7, f8, prios
                    "#,
                )
                .eval()
                .unwrap();
            assert_eq!(f7_winner, "p.high");
            assert_eq!(f8_winner, "p.high");
            assert_eq!(stored_prios, vec![20, 5]);
            drop(cw2);
            drop(cr2);
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            drop(cw);
            drop(cr);
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn in_place_transition_updates_editor_mirror_session_id() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr1, mut cw1, task1, old_sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&old_sid).unwrap();
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":old_sid,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr1, 10).await.get("error").is_none());

            let service = rt.host.service_table("session").unwrap();
            rt.host.lua().globals().set("TRANSITION_SVC", service).unwrap();
            let result = rt
                .host
                .lua()
                .load(
                    r#"
                    local info = TRANSITION_SVC:info()
                    return TRANSITION_SVC:transition({ mode = "new" }, { session_id = info.session_id, session_epoch = info.session_epoch })
                    "#,
                )
                .call_async::<mlua::Table>(())
                .await
                .unwrap();
            assert_eq!(result.get::<String>("status").unwrap(), "applied");
            let new_sid = rpc.manager.active_ids().into_iter().find(|id| id != &old_sid).unwrap();

            // Calling `mare.ui.frontend.update_editor` after in-place transition must tag
            // emitted `ui.editor_state` notifications with `new_sid`.
            rt.host
                .lua()
                .load(r#"assert(mare.ui.frontend.update_editor("after-transition", {}) ~= nil)"#)
                .exec()
                .unwrap();
            let editor_sid = tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let msg = rpc_read(&mut cr1).await;
                    if msg["method"] == "ui.editor_state"
                        && msg.pointer("/params/text").and_then(J::as_str) == Some("after-transition")
                    {
                        return msg["params"]["session_id"].as_str().unwrap_or_default().to_string();
                    }
                }
            })
            .await
            .expect("timed out waiting for ui.editor_state notification");
            assert_eq!(editor_sid, new_sid, "update_editor must emit new session_id after in-place transition");

            drop((cw1, cr1));
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn disconnect_releases_external_editor_lock_before_inflight_grace() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, _cr0, _cw0, task0, sid) = setup_rpc(false).await;
            let rt = rpc.manager.session(&sid).unwrap();
            // Register an async keymap action that deterministically holds an in-flight RPC task
            // open until `release_tx` fires, keeping `conn1` inside `wait_conn_inflight`.
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
            let entered_tx = std::rc::Rc::new(std::cell::RefCell::new(Some(entered_tx)));
            let release_rx = std::rc::Rc::new(std::cell::RefCell::new(Some(release_rx)));
            let hold_fn = rt
                .host
                .lua()
                .create_async_function(move |_, _ev: mlua::Value| {
                    let entered_tx = entered_tx.clone();
                    let release_rx = release_rx.clone();
                    async move {
                        if let Some(tx) = entered_tx.borrow_mut().take() {
                            let _ = tx.send(());
                        }
                        let rx = release_rx.borrow_mut().take();
                        if let Some(rx) = rx {
                            let _ = rx.await;
                        }
                        Ok(true)
                    }
                })
                .unwrap();
            rt.host
                .lua()
                .load("local f = ...; mare.on('ui/resize', f)")
                .call::<()>(hold_fn)
                .unwrap();

            // Connect conn1 as a non-lifecycle owner (like a WebSocket frontend).
            let (client1, server1) = tokio::io::duplex(131072);
            let (sr1, sw1) = tokio::io::split(server1);
            let task1 = tokio::task::spawn_local(super::serve::serve_inner(
                rpc.clone(),
                sr1,
                sw1,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr1, mut cw1) = tokio::io::split(client1);
            let mut cr1 = BufReader::new(cr1);
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"ui":true}}}),
            )
            .await;
            assert!(read_reply(&mut cr1, 1).await.get("error").is_none());

            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":sid,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr1, 10).await.get("error").is_none());

            // Acquire external editor lock on conn1.
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":11,"method":"ui.set_external_editor","params":{"session_id":sid,"action":"start"}}),
            )
            .await;
            assert!(read_reply(&mut cr1, 11).await.get("error").is_none());

            // Trigger an inflight `ui.attach` viewport resize that stays parked in `ui/resize`
            // until `release_tx` fires, keeping `conn1` inside `wait_conn_inflight` after EOF.
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":12,"method":"ui.attach","params":{"session_id":sid,"responder":true,"viewport":{"width":123,"height":45}}}),
            )
            .await;
            tokio::time::timeout(Duration::from_secs(2), entered_rx)
                .await
                .expect("ui/resize handler did not start")
                .unwrap();
            drop((cw1, cr1));
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
            assert!(!task1.is_finished(), "conn1 must still be waiting in wait_conn_inflight");

            // Connect conn2 immediately while conn1 is still parked in `wait_conn_inflight`.
            let (client2, server2) = tokio::io::duplex(131072);
            let (sr2, sw2) = tokio::io::split(server2);
            let task2 = tokio::task::spawn_local(super::serve::serve_inner(
                rpc.clone(),
                sr2,
                sw2,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr2, mut cw2) = tokio::io::split(client2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"ui":true}}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":sid,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 2).await.get("error").is_none());

            let started = std::time::Instant::now();
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":3,"method":"ui.update_editor","params":{"session_id":sid,"text":"reconnected"}}),
            )
            .await;
            let reply = tokio::time::timeout(Duration::from_millis(800), read_reply(&mut cr2, 3))
                .await
                .expect("update_editor reply timed out");
            let _ = release_tx.send(());
            assert!(
                reply.get("error").is_none(),
                "external editor lock must release immediately on conn1 disconnect (elapsed {:?}): {reply}",
                started.elapsed()
            );

            drop((cw2, cr2));
            drop((_cw0, _cr0));
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(5), task0).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn multi_client_viewport_restores_remaining_client_on_detach() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr1, mut cw1, task1, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();

            // Client 1 attaches with desktop viewport 120x40.
            rpc_send(
                &mut cw1,
                json!({
                    "jsonrpc": "2.0",
                    "id": 10,
                    "method": "ui.attach",
                    "params": {
                        "session_id": sid,
                        "responder": true,
                        "viewport": { "width": 120, "height": 40 }
                    }
                }),
            )
            .await;
            assert!(read_reply(&mut cr1, 10).await.get("error").is_none());
            assert_eq!(rt.host.ui.width.load(std::sync::atomic::Ordering::Relaxed), 120);
            assert_eq!(rt.host.ui.height.load(std::sync::atomic::Ordering::Relaxed), 40);

            // Client 2 attaches with narrow viewport 45x20.
            let (c2, s2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(s2);
            let task2 = tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"ui":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());
            rpc_send(
                &mut cw2,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "ui.attach",
                    "params": {
                        "session_id": sid,
                        "responder": false,
                        "viewport": { "width": 45, "height": 20 }
                    }
                }),
            )
            .await;
            assert!(read_reply(&mut cr2, 2).await.get("error").is_none());
            assert_eq!(rt.host.ui.width.load(std::sync::atomic::Ordering::Relaxed), 45);
            assert_eq!(rt.host.ui.height.load(std::sync::atomic::Ordering::Relaxed), 20);

            // Track ui/resize emissions on detach of Client 2.
            rt.host
                .lua()
                .load(
                    r#"
                    _G.__resize_count = 0
                    mare.on("ui/resize", function()
                        _G.__resize_count = _G.__resize_count + 1
                    end)
                    "#,
                )
                .exec()
                .unwrap();

            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":3,"method":"ui.detach","params":{"session_id":sid}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 3).await.get("error").is_none());

            assert_eq!(
                rt.host.ui.width.load(std::sync::atomic::Ordering::Relaxed),
                120,
                "detaching narrow client must restore remaining attached client's width"
            );
            assert_eq!(
                rt.host.ui.height.load(std::sync::atomic::Ordering::Relaxed),
                40,
                "detaching narrow client must restore remaining attached client's height"
            );
            let resizes: i64 = rt.host.lua().load("return _G.__resize_count").eval().unwrap();
            assert!(resizes >= 1, "restoring viewport on detach must emit ui/resize");

            drop((cw2, cr2));
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            drop((cw1, cr1));
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn multi_client_viewport_restores_remaining_client_on_disconnect() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr1, mut cw1, task1, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();

            // Client 1 attaches with desktop viewport 120x40.
            rpc_send(
                &mut cw1,
                json!({
                    "jsonrpc": "2.0",
                    "id": 10,
                    "method": "ui.attach",
                    "params": {
                        "session_id": sid,
                        "responder": true,
                        "viewport": { "width": 120, "height": 40 }
                    }
                }),
            )
            .await;
            assert!(read_reply(&mut cr1, 10).await.get("error").is_none());
            assert_eq!(rt.host.ui.width.load(std::sync::atomic::Ordering::Relaxed), 120);
            assert_eq!(rt.host.ui.height.load(std::sync::atomic::Ordering::Relaxed), 40);

            // Client 2 attaches with narrow viewport 45x20.
            let (c2, s2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(s2);
            let task2 = tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"ui":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());
            rpc_send(
                &mut cw2,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "ui.attach",
                    "params": {
                        "session_id": sid,
                        "responder": false,
                        "viewport": { "width": 45, "height": 20 }
                    }
                }),
            )
            .await;
            assert!(read_reply(&mut cr2, 2).await.get("error").is_none());
            assert_eq!(rt.host.ui.width.load(std::sync::atomic::Ordering::Relaxed), 45);
            assert_eq!(rt.host.ui.height.load(std::sync::atomic::Ordering::Relaxed), 20);

            // Track ui/resize emissions on abrupt disconnect of Client 2 (no ui.detach sent).
            rt.host
                .lua()
                .load(
                    r#"
                    _G.__resize_count = 0
                    mare.on("ui/resize", function()
                        _G.__resize_count = _G.__resize_count + 1
                    end)
                    "#,
                )
                .exec()
                .unwrap();

            drop((cw2, cr2));
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();

            assert_eq!(
                rt.host.ui.width.load(std::sync::atomic::Ordering::Relaxed),
                120,
                "disconnecting narrow client must restore remaining attached client's width"
            );
            assert_eq!(
                rt.host.ui.height.load(std::sync::atomic::Ordering::Relaxed),
                40,
                "disconnecting narrow client must restore remaining attached client's height"
            );
            let resizes: i64 = rt.host.lua().load("return _G.__resize_count").eval().unwrap();
            assert!(resizes >= 1, "restoring viewport on disconnect must emit ui/resize");

            drop((cw1, cr1));
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn in_place_session_switch_clears_stale_ui_shell_snapshot() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr1, mut cw1, task1, old_sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&old_sid).unwrap();

            // Attach Client 1 so the UI mirror is installed and populate shell visual state.
            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":10,"method":"ui.attach","params":{"session_id":old_sid,"responder":true}}),
            )
            .await;
            assert!(read_reply(&mut cr1, 10).await.get("error").is_none());

            rt.host.shell.set_widget(&mare_protocol::UiWidgetParams {
                session_id: Some(old_sid.clone()),
                widget_id: "stale-widget".into(),
                slot: "above-editor".into(),
                node: Some(json!({"type":"text","text":"old"})),
                order: 1,
            });
            rt.host.shell.set_status(&mare_protocol::UiStatusParams {
                session_id: Some(old_sid.clone()),
                status_id: "stale-status".into(),
                align: "right".into(),
                node: Some(json!({"type":"text","text":"old"})),
                order: 1,
                keep: false,
            });
            rt.host.shell.set_working(Some("thinking".into()), Some("old".into()), false);
            rt.host.shell.set_overlay(&mare_protocol::UiOverlayParams {
                session_id: Some(old_sid.clone()),
                open: true,
                title: "Stale".into(),
                node: Some(json!({"type":"text","text":"old"})),
                context: None,
            });
            rt.host.shell.set_flag("stale-flag", true);
            rt.host.shell.set_editor_hint("stale hint");

            // Switch session in-place via session service (triggers rekey_host_session).
            let _ = call_service(&rt.host, "session", "switch", vec![json!({"kind":"new"})]).await.unwrap();
            let new_sid = rt
                .host
                .kernel
                .event_log()
                .session_identity()
                .0
                .expect("new session id after in-place switch");
            assert_ne!(new_sid, old_sid);

            // A newly attaching client must not inherit the prior session's shell visuals.
            let (c2, s2) = tokio::io::duplex(65536);
            let (sr2, sw2) = tokio::io::split(s2);
            let task2 = tokio::task::spawn_local(serve_manager(rpc.clone(), sr2, sw2, true, None, std::future::pending()));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version":"3.0","capabilities":{"ui":false}}}),
            )
            .await;
            assert!(read_reply(&mut cr2, 1).await.get("error").is_none());
            rpc_send(
                &mut cw2,
                json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":new_sid,"responder":false}}),
            )
            .await;
            let attach = read_reply(&mut cr2, 2).await;
            assert!(attach.get("error").is_none(), "{attach}");
            let snap = &attach["result"]["snapshot"];
            assert!(
                snap.get("widgets").and_then(J::as_array).is_none_or(Vec::is_empty),
                "stale widgets leaked: {snap}"
            );
            assert!(
                !snap
                    .get("statuses")
                    .and_then(J::as_array)
                    .is_some_and(|arr| arr.iter().any(|s| s["status_id"] == "stale-status")),
                "stale status leaked across in-place switch: {snap}"
            );
            assert!(snap.get("working").is_none_or(J::is_null), "stale working leaked: {snap}");
            assert!(
                snap.get("overlays").and_then(J::as_array).is_none_or(Vec::is_empty),
                "stale overlays leaked: {snap}"
            );
            assert!(
                snap.get("flags").and_then(J::as_object).is_none_or(serde_json::Map::is_empty),
                "stale flags leaked: {snap}"
            );
            assert!(snap.get("editor_hint").is_none_or(J::is_null), "stale editor_hint leaked: {snap}");

            drop((cw2, cr2));
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            drop((cw1, cr1));
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn rekey_session_migrates_suppressed_subscriptions_and_global_registrations_without_session_ctx() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, cr, cw, task, old_sid) = setup_rpc(false).await;
            let rt = rpc.manager.session(&old_sid).unwrap();
            let conn = rpc.connections.borrow().values().find_map(|w| w.upgrade()).expect("active owner connection");

            // Simulate a connection that has NO session_ctxs and NO subscriptions for old_sid
            // (e.g. `session.load { subscribe = false }` or `ui.attach { responder = false }`).
            conn.session_ctxs.borrow_mut().remove(&old_sid);
            conn.subscriptions.borrow_mut().remove(&old_sid);
            conn.suppressed_subscriptions.borrow_mut().insert(old_sid.clone());
            {
                let mut regs = conn.global_registrations.borrow_mut();
                let mut instances = std::collections::BTreeSet::new();
                instances.insert(old_sid.clone());
                regs.by_id.insert(
                    "reg-ui-detach".into(),
                    GlobalRegistrationRecord {
                        scope: ResolvedRegistrationScope::Session(old_sid.clone()),
                        kind: GlobalRegistrationKind::Contract {
                            service: "ui".into(),
                            method: "set_keymap".into(),
                            validated_arguments: json!({}),
                            callback_names: Vec::new(),
                            release_on: mare_kernel::contracts::ReleaseOn::UiDetach,
                        },
                        active_session_instances: instances,
                    },
                );
            }

            let new_sid = "new-rekeyed-session";
            conn.rekey_session(&old_sid, new_sid, &rt.host);

            assert!(
                conn.suppressed_subscriptions.borrow().contains(new_sid)
                    && !conn.suppressed_subscriptions.borrow().contains(&old_sid),
                "suppressed_subscriptions must migrate from old_sid to new_sid even without session_ctxs/subscriptions"
            );
            {
                let regs = conn.global_registrations.borrow();
                let rec = regs.by_id.get("reg-ui-detach").expect("registration preserved");
                assert!(
                    matches!(&rec.scope, ResolvedRegistrationScope::Session(sid) if sid == new_sid),
                    "session-scoped global registration must migrate to new_sid: {:?}",
                    rec.scope
                );
                assert!(
                    rec.active_session_instances.contains(new_sid) && !rec.active_session_instances.contains(&old_sid),
                    "active_session_instances must migrate to new_sid"
                );
            }

            drop(conn);
            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// If the answering RPC client disconnects while a `mare.ui.editor` callback dialog is
/// awaiting a response, `callback_editor_active` must still be reset to `false` so other clients
/// can subsequently open the external editor.
#[tokio::test]
async fn callback_editor_disconnect_releases_callback_editor_active() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr1, mut cw1, task1, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();

            rpc_send(
                &mut cw1,
                json!({"jsonrpc":"2.0","id":2,"method":"ui.attach","params":{"session_id":sid,"responder":true}}),
            )
            .await;
            let att1 = read_reply(&mut cr1, 2).await;
            assert!(att1.get("error").is_none(), "{att1}");

            let host_for_cb = rt.host.clone();
            let cb_task = tokio::task::spawn_local(async move {
                let _ = host_for_cb
                    .lua()
                    .load(
                        r#"mare.task.select(
                             function() return mare.ui.editor({ title = "cb-abort" }) end,
                             function() mare.task.sleep(100); return "aborted" end
                           )"#,
                    )
                    .eval_async::<mlua::MultiValue>()
                    .await;
            });

            let mut saw_editor_req = false;
            tokio::time::timeout(Duration::from_secs(5), async {
                while !saw_editor_req {
                    let msg = rpc_read(&mut cr1).await;
                    if msg.get("method").and_then(J::as_str) == Some(methods::UI_EDITOR) {
                        saw_editor_req = true;
                    }
                }
            })
            .await
            .expect("client 1 must receive ui.editor callback request");

            // Wait for the select branch to forcibly drop/abort the mare.ui.editor future!
            tokio::time::timeout(Duration::from_secs(5), cb_task).await.unwrap().unwrap();

            let ext_res = rt.host.shell.start_external_editor(Some(&sid), mare_host::ConnId(999));
            assert!(
                ext_res.is_ok(),
                "callback_editor_active must be released after callback future is dropped/aborted: {ext_res:?}"
            );
            drop((cw1, cr1));
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// `ui.send_editor` must (1) refuse submitting a shared draft that has non-empty text AND
/// metadata-only attachments when no binary `attachments` payload is supplied, (2) leave the
/// shared draft untouched when explicit override `text` is passed, (3) preserve the shared
/// draft if submission fails, and (4) preserve text restored by an `input/submit` handler via
/// `set_editor(..., { keep_draft = true })` or `{ prepend = true }`.
#[tokio::test]
async fn ui_send_editor_preserves_draft_on_failure_override_and_refuses_nonempty_metadata_only_attachments() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let (_dir, _guard, rpc, mut cr, mut cw, task, sid) = setup_rpc(true).await;
            let rt = rpc.manager.session(&sid).unwrap();

            // 1. Non-empty draft text + metadata-only attachment + no binary `attachments`.
            rt.host
                .shell
                .update_editor_input(
                    Some(&sid),
                    "check this screenshot".into(),
                    None,
                    None,
                    None,
                    Some(vec![mare_protocol::UiEditorAttachment {
                        attachment_id: "att-nonempty".into(),
                        label: Some("[Image #1]".into()),
                        mime_type: Some("image/png".into()),
                        width: Some(10),
                        height: Some(10),
                    }]),
                )
                .unwrap();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":20,"method":"ui.send_editor","params":{"session_id":sid}}))
                .await;
            let att_err = read_reply(&mut cr, 20).await;
            assert_eq!(
                att_err["error"]["data"]["code"], "invalid_arguments",
                "non-empty draft with metadata-only attachment must not silently drop attachment: {att_err}"
            );
            let (kept_text, kept_atts) = rt.host.shell.peek_editor();
            assert_eq!(kept_text, "check this screenshot");
            assert_eq!(kept_atts.len(), 1);

            // 2. Explicit override `text` must NOT wipe the host's unsent shared draft.
            rt.host
                .shell
                .update_editor_input(Some(&sid), "unsent shared draft".into(), None, None, None, Some(vec![]))
                .unwrap();
            rpc_send(
                &mut cw,
                json!({"jsonrpc":"2.0","id":21,"method":"ui.send_editor","params":{"session_id":sid,"text":"one-off override"}}),
            )
            .await;
            let ov_rep = read_reply(&mut cr, 21).await;
            assert!(ov_rep.get("error").is_none(), "override submit failed: {ov_rep}");
            if let Some(tid) = ov_rep["result"]["result"]["turn_id"].as_str() {
                let _ = call_service(&rt.host, "agent", "wait", vec![json!(tid)]).await;
            }
            assert_eq!(
                rt.host.shell.peek_editor().0,
                "unsent shared draft",
                "explicit override text must not wipe the unsent shared draft"
            );

            // 3. When submit fails, the shared draft must remain intact so the user can retry.
            let agent_tbl = rt.host.service_table("agent").unwrap();
            let fail_fn = rt
                .host
                .lua()
                .load(r#"return function() require("mare_lib").fail("busy", "session switch in progress; retry") end"#)
                .eval::<mlua::Function>()
                .unwrap();
            agent_tbl.set("prompt", fail_fn).unwrap();

            rt.host
                .shell
                .update_editor_input(Some(&sid), "preserve me on fail".into(), None, None, None, Some(vec![]))
                .unwrap();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":22,"method":"ui.send_editor","params":{"session_id":sid}}))
                .await;
            let fail_rep = read_reply(&mut cr, 22).await;
            assert!(fail_rep.get("error").is_some(), "expected submit failure: {fail_rep}");
            assert_eq!(
                rt.host.shell.peek_editor().0,
                "preserve me on fail",
                "failed ui.send_editor must preserve the shared draft"
            );

            // 4. An `input/submit` handler calling `set_editor(..., { keep_draft = true })` or
            //    `{ prepend = true }` (such as `/fork` or side-conversation rejections) must see
            //    the submitted draft cleared and keep its restored text intact after `ui.send_editor`.
            let ui_tbl = rt.host.service_table("ui").unwrap();
            rt.host.lua().globals().set("TEST_UI_SUBMIT", ui_tbl).unwrap();
            rt.host
                .lua()
                .load(
                    r#"
                    TEST_UI_SUBMIT:handler("submit", "test-restore-draft", { order = 1 }, function(ev)
                        if ev.text == "/keep-test" then
                            mare.ui.set_editor("restored via keep_draft", { keep_draft = true })
                            return true
                        elseif ev.text == "/prepend-test" then
                            mare.ui.set_editor("restored via prepend", { prepend = true })
                            return true
                        end
                        return false
                    end)
                    "#,
                )
                .exec()
                .unwrap();

            rt.host
                .shell
                .update_editor_input(Some(&sid), "/keep-test".into(), None, None, None, Some(vec![]))
                .unwrap();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":23,"method":"ui.send_editor","params":{"session_id":sid}}))
                .await;
            let keep_rep = read_reply(&mut cr, 23).await;
            assert!(keep_rep.get("error").is_none(), "keep_draft handler submit failed: {keep_rep}");
            assert_eq!(keep_rep["result"]["handled"], true, "{keep_rep}");
            assert_eq!(
                rt.host.shell.peek_editor().0,
                "restored via keep_draft",
                "handler set_editor(keep_draft=true) must not be clobbered or blocked by submitted draft"
            );

            rt.host
                .shell
                .update_editor_input(Some(&sid), "/prepend-test".into(), None, None, None, Some(vec![]))
                .unwrap();
            rpc_send(&mut cw, json!({"jsonrpc":"2.0","id":24,"method":"ui.send_editor","params":{"session_id":sid}}))
                .await;
            let prepend_rep = read_reply(&mut cr, 24).await;
            assert!(prepend_rep.get("error").is_none(), "prepend handler submit failed: {prepend_rep}");
            assert_eq!(prepend_rep["result"]["handled"], true, "{prepend_rep}");
            assert_eq!(
                rt.host.shell.peek_editor().0,
                "restored via prepend",
                "handler set_editor(prepend=true) must not be clobbered or doubled with submitted draft"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

/// Multi-client (`owns_process_lifecycle = false`) responder disconnect path: when the answering
/// client A disconnects while `mare.ui.editor` is awaiting its response, `CallbackEditorGuard`
/// must release `callback_editor_active`, broadcast `external_editor_running = false` to surviving
/// client B, and allow client B to start the external editor.
#[tokio::test]
async fn callback_editor_multi_client_responder_disconnect_releases_lock_and_broadcasts() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            mare_shared::ui::install_hooks(mare_tui::hooks());
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            let (c1, s1) = tokio::io::duplex(131072);
            let (sr1, sw1) = tokio::io::split(s1);
            let task1 = tokio::task::spawn_local(super::serve::serve_inner(
                rpc.clone(),
                sr1,
                sw1,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr1, mut cw1) = tokio::io::split(c1);
            let mut cr1 = BufReader::new(cr1);

            let (c2, s2) = tokio::io::duplex(131072);
            let (sr2, sw2) = tokio::io::split(s2);
            let task2 = tokio::task::spawn_local(super::serve::serve_inner(
                rpc.clone(),
                sr2,
                sw2,
                Principal::Owner { remote_admin: true },
                false,
                None,
                std::future::pending(),
            ));
            let (cr2, mut cw2) = tokio::io::split(c2);
            let mut cr2 = BufReader::new(cr2);

            for (cw, cr, responder) in [(&mut cw1, &mut cr1, true), (&mut cw2, &mut cr2, false)] {
                rpc_send(
                    cw,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "initialize",
                        "params": { "protocol_version": "3.0", "capabilities": { "ui": responder } }
                    }),
                )
                .await;
                assert!(read_reply(cr, 1).await.get("error").is_none());
                rpc_send(
                    cw,
                    json!({
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "ui.attach",
                        "params": { "session_id": sid, "responder": responder }
                    }),
                )
                .await;
                assert!(read_reply(cr, 2).await.get("error").is_none());
            }

            let host_for_cb = rt.host.clone();
            let cb_task = tokio::task::spawn_local(async move {
                host_for_cb
                    .lua()
                    .load(r#"return mare.ui.editor({ title = "multi-client-disconnect" })"#)
                    .eval_async::<mlua::MultiValue>()
                    .await
            });

            // Wait until client A (responder) receives the `ui.editor` callback request.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let msg = rpc_read(&mut cr1).await;
                    if msg.get("method").and_then(J::as_str) == Some(methods::UI_EDITOR) {
                        break;
                    }
                }
            })
            .await
            .expect("client A must receive ui.editor callback request");

            // Drop client A while the callback editor dialog is open; host remains alive.
            drop((cw1, cr1));
            tokio::time::timeout(Duration::from_secs(5), task1).await.unwrap().unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), cb_task).await.unwrap().unwrap();

            // Client B must receive an editor-state broadcast with external_editor_running = false.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let msg = rpc_read(&mut cr2).await;
                    if msg.get("method").and_then(J::as_str) == Some(methods::UI_EDITOR_STATE)
                        && msg["params"]["external_editor_running"] == false
                    {
                        break;
                    }
                }
            })
            .await
            .expect("surviving client B must receive external_editor_running=false broadcast");

            // Client B can now open the external editor via RPC.
            rpc_send(
                &mut cw2,
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "ui.set_external_editor",
                    "params": { "session_id": sid, "action": "start" }
                }),
            )
            .await;
            let ext_rep = read_reply(&mut cr2, 3).await;
            assert_eq!(
                ext_rep["result"]["running"], true,
                "client B must be able to start external editor after client A disconnected: {ext_rep}"
            );

            drop((cw2, cr2));
            tokio::time::timeout(Duration::from_secs(5), task2).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}

#[tokio::test]
async fn commands_run_tree_completes_navigation_before_returning() {
    tokio::task::LocalSet::new()
        .run_until(async {
            struct HomeGuard(Option<std::ffi::OsString>);
            impl Drop for HomeGuard {
                fn drop(&mut self) {
                    // SAFETY: protected by `HOST_LOCK` for the duration of the test.
                    unsafe {
                        match &self.0 {
                            Some(v) => std::env::set_var("MARE_HOME", v),
                            None => std::env::remove_var("MARE_HOME"),
                        }
                    }
                }
            }
            let _guard = HOST_LOCK.lock().await;
            let home = tempfile::tempdir().unwrap();
            let prev_home = std::env::var_os("MARE_HOME");
            // SAFETY: protected by `HOST_LOCK` for the duration of the test.
            unsafe { std::env::set_var("MARE_HOME", home.path()) };
            let _home_guard = HomeGuard(prev_home);

            mare_shared::ui::install_hooks(mare_tui::hooks());
            let dir = tempfile::tempdir().unwrap();
            let opts = mare_host::HostOptions {
                cwd: dir.path().to_owned(),
                mode: mare_plugin::Mode::Rpc,
                config: json!({
                    "model": "faux/echo",
                    "plugins": { "permissions": { "enabled": false } }
                }),
                load_init: false,
                project_trusted: true,
                args: json!({}),
                no_user_plugins: true,
            };
            let manager = mare_host::HostManager::new();
            let rt = manager.create_session(opts.clone()).await.unwrap();
            let sid = rt.session_id.clone();
            let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(sid.clone()), false);

            let (client, server) = tokio::io::duplex(131072);
            let (sr, sw) = tokio::io::split(server);
            let task = tokio::task::spawn_local(serve_manager(rpc.clone(), sr, sw, true, None, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocol_version": "3.0",
                        "capabilities": { "ui": true, "hooks": true }
                    }
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 1).await.get("error").is_none());

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "ui.attach",
                    "params": { "session_id": sid, "responder": true }
                }),
            )
            .await;
            assert!(read_reply(&mut cr, 2).await.get("error").is_none());

            for (req_id, text) in [(3, "first turn"), (4, "second turn")] {
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "agent.prompt",
                        "params": { "session_id": sid, "input": text }
                    }),
                )
                .await;
                let rep = read_reply(&mut cr, req_id).await;
                let turn_id =
                    rep["result"]["turn_id"].as_str().unwrap_or_else(|| panic!("expected turn_id: {rep}")).to_string();
                let wait_id = req_id + 100;
                rpc_send(
                    &mut cw,
                    json!({
                        "jsonrpc": "2.0",
                        "id": wait_id,
                        "method": "agent.wait",
                        "params": { "session_id": sid, "turn_id": turn_id }
                    }),
                )
                .await;
                let wait_rep = read_reply(&mut cr, wait_id).await;
                assert!(wait_rep.get("error").is_none(), "{wait_rep}");
            }

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 10,
                    "method": "commands.run",
                    "params": { "session_id": sid, "line": "/tree" }
                }),
            )
            .await;

            let mut select_count = 0usize;
            let tree_rep = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let msg = rpc_read(&mut cr).await;
                    if msg.get("method").and_then(J::as_str) == Some(methods::UI_SELECT) {
                        select_count += 1;
                        rpc_send(
                            &mut cw,
                            json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "result": { "index": 0 }
                            }),
                        )
                        .await;
                    } else if msg["id"] == 10 {
                        return msg;
                    }
                }
            })
            .await
            .expect("commands.run(/tree) must complete after answering ui.select callbacks");
            assert!(tree_rep.get("error").is_none(), "commands.run(/tree) failed: {tree_rep}");
            assert_eq!(
                select_count, 2,
                "commands.run(/tree) must await both tree picker and branch-summary modals before replying"
            );

            // An immediate session call right after commands.run(/tree) must not race a detached
            // switch (`stale_session`) and must already observe the updated branch state.
            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 11,
                    "method": "session.list_records",
                    "params": { "session_id": sid, "kind": "test/tree-check" }
                }),
            )
            .await;
            let list_rep = read_reply(&mut cr, 11).await;
            assert!(list_rep.get("error").is_none(), "session.list_records after /tree failed: {list_rep}");

            rpc_send(
                &mut cw,
                json!({
                    "jsonrpc": "2.0",
                    "id": 12,
                    "method": "agent.list_messages",
                    "params": { "session_id": sid }
                }),
            )
            .await;
            let msgs_rep = read_reply(&mut cr, 12).await;
            assert!(msgs_rep.get("error").is_none(), "{msgs_rep}");
            assert_eq!(
                msgs_rep["result"]["data"].as_array().map(Vec::len),
                Some(1),
                "branching to the first tree entry must already trim the 4-message branch to 1 message after /tree returns: {msgs_rep}"
            );

            drop((cw, cr));
            tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
            let _ = rpc.manager.shutdown_all().await;
        })
        .await;
}
