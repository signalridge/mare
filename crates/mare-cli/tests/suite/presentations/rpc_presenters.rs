use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use image::ImageEncoder as _;
use serde_json::{Value, json};

fn spawn_rpc_markdown_client(
    plugin_file: &str,
    plugin_lua: &str,
) -> (tempfile::TempDir, crate::common::rpc::RpcClient, String, Arc<Mutex<Vec<Value>>>) {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let plugins_dir = home.join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(plugins_dir.join(plugin_file), plugin_lua).unwrap();

    let env = BTreeMap::from([("MARE_HOME".to_string(), home.display().to_string())]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--no-session", "--yolo", "--model", "faux/echo"], &env, &work);
    let notifications: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let notes_clone = Arc::clone(&notifications);
    client.on_method = Some(Box::new(move |_, msg| {
        if msg.get("method").and_then(Value::as_str) == Some("ui.markdown_changed") {
            notes_clone.lock().unwrap().push(msg["params"].clone());
            return true;
        }
        false
    }));
    let init = client.call("initialize", json!({ "protocol_version": "3.0" }));
    let session_id = init["session"]["session_id"].as_str().expect("initialize must include session_id").to_string();
    (temp, client, session_id, notifications)
}

#[test]
fn real_rpc_frontend_consumes_native_positions_and_png_descriptions() {
    fn png_bytes(variant: bool) -> Vec<u8> {
        let (width, height) = if variant { (16, 32) } else { (32, 16) };
        let color = if variant { [200, 30, 50, 255] } else { [30, 180, 210, 255] };
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba(color));
        let mut buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        buf
    }

    fn html_escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    fn component_html(node: &Value, expected_png: &[u8]) -> String {
        let kind = node["type"].as_str().unwrap();
        match kind {
            "image" => {
                assert!(node.get("bytes").is_none() && node.get("path").is_none());
                let data = node["data"].as_str().unwrap();
                let raw = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
                assert_eq!(raw, expected_png);
                let w = u32::from_be_bytes(raw[16..20].try_into().unwrap());
                let h = u32::from_be_bytes(raw[20..24].try_into().unwrap());
                assert_eq!((w, h), (32, 16));
                format!("<img alt=\"diagram\" src=\"data:image/png;base64,{data}\">")
            }
            "lines" => node["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|child| component_html(child, expected_png))
                .collect::<String>(),
            "code" => format!("<pre>{}</pre>", html_escape(node["code"].as_str().unwrap())),
            "presented_markdown" => {
                let source = node["text"].as_str().unwrap().as_bytes();
                let mut cursor = 0usize;
                let mut pieces = Vec::new();
                for block in node["blocks"].as_array().unwrap() {
                    let start = block["start"].as_u64().unwrap() as usize;
                    let end = block["end"].as_u64().unwrap() as usize;
                    assert!(cursor <= start && start < end && end <= source.len());
                    pieces.push(format!(
                        "<pre>{}</pre>",
                        html_escape(std::str::from_utf8(&source[cursor..start]).unwrap())
                    ));
                    pieces.push(component_html(&block["node"], expected_png));
                    cursor = end;
                }
                pieces.push(format!("<pre>{}</pre>", html_escape(std::str::from_utf8(&source[cursor..]).unwrap())));
                pieces.join("")
            }
            other => panic!("unsupported fixture component: {other}"),
        }
    }

    let expected_png = png_bytes(false);
    let encoded = base64::engine::general_purpose::STANDARD.encode(&expected_png);
    let other = base64::engine::general_purpose::STANDARD.encode(png_bytes(true));
    let lua = format!(
        "return {{name='presentation-fixture',inject={{'ui','session'}},apply=function(ctx)\n\
        ctx:on('ui/caps',function(c, next) local p=mare.env('PRESENTATION_CAPS_REPORT'); if p then mare.fs.write(p,mare.json.encode({{mode=mare.mode,caps=c}})) end; return next(c) end)\n\
        ctx:on('turn/completed',function() local p=mare.env('PRESENTATION_SESSION_REPORT'); if p then mare.fs.write(p,mare.json.encode(ctx.session:info())) end end)\n\
        local slow_done_tx, slow_done_rx = mare.task.channel()\n\
        ctx.ui:markdown_presenter('drawing',{{render=function(r)\n\
        if r.source:find('rpc-cancel',1,true) then r.cancel:wait(); slow_done_tx:send(true) elseif r.source:find('slow-cancel',1,true) then r.cancel:wait() elseif r.source:find('fence',1,true) then slow_done_rx:recv() elseif r.source:find('slow',1,true) then mare.task.sleep(200) end\n\
        return mare.ui.Lines({{mare.ui.Code('viewport:'..r.width,'text'),\n\
        mare.ui.image{{bytes=mare.crypto.base64_decode((r.source:find('cancel',1,true)) and '{other}' or '{encoded}'),max_rows=8,max_cols=40}}}})\n\
        end}})\nend}}\n"
    );
    let (_temp, mut client, session_id, presentations) = spawn_rpc_markdown_client("presentation-fixture.lua", &lua);

    let settled = |c: &mut crate::common::rpc::RpcClient, identity: &str| -> Value {
        let is_settled = |v: &Value| v["pending"].as_u64() == Some(0);
        let result = c.call("ui.get_markdown", json!({ "session_id": session_id, "document_id": identity }));
        if !result.is_null() && is_settled(&result) {
            return result;
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let rem = deadline.saturating_duration_since(Instant::now());
            let Some(msg) = c.read_timeout(rem) else {
                break;
            };
            c.dispatch(msg);
            if presentations
                .lock()
                .unwrap()
                .iter()
                .rev()
                .any(|note| note["document_id"] == identity && is_settled(note))
            {
                return c.call("ui.get_markdown", json!({ "session_id": session_id, "document_id": identity }));
            }
        }
        panic!(
            "presentation did not settle: {identity}; last snapshot: {:?}; presentations: {:?}",
            c.call("ui.get_markdown", json!({ "session_id": session_id, "document_id": identity })),
            presentations.lock().unwrap()
        );
    };

    for legacy in [
        "ui.markdown.present",
        "ui.markdown.snapshot",
        "ui.markdown.cancel",
        "ui.markdown.release",
        "ui.markdown.presenters",
    ] {
        let err = client.call_err(legacy, json!({}));
        assert_eq!(err["code"], -32601, "legacy method {legacy} must be removed: {err}");
    }
    assert!(
        client.call("ui.list_markdown_presenters", json!({}))["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["language"] == "drawing")
    );
    let source = "前置\n\n> ~~~DRAWING info\n> same\n> ~~~\n\n```drawing\nsame\n```\n\nafter";
    let result = client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "document",
            "source": source,
            "viewport": { "supports_images": true, "width": 50 }
        }),
    );
    assert_eq!(result["source"], source);
    assert_eq!(result["session_id"], session_id);
    let result = settled(&mut client, "document");
    assert!(result["enhanced"] == true && result["node"]["blocks"].as_array().unwrap().len() == 2, "{result}");
    let document = component_html(&result["node"], &expected_png);
    assert!(
        document.matches("<img alt=\"diagram\"").count() == 2
            && document.contains("前置")
            && document.contains("after")
    );
    assert!(
        presentations
            .lock()
            .unwrap()
            .last()
            .is_some_and(|p| p["document_id"] == "document" && p["session_id"] == session_id),
        "ui.markdown_changed must carry session_id: {:?}",
        presentations.lock().unwrap().last()
    );
    let result_json = serde_json::to_string(&result).unwrap();
    assert!(!result_json.contains("cancel") && !result_json.contains("userdata"));

    client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "document",
            "source": source,
            "viewport": { "supports_images": true, "width": 30 }
        }),
    );
    let resized = settled(&mut client, "document");
    assert!(resized["revision"].as_u64().unwrap() > result["revision"].as_u64().unwrap());
    let before_ids: Vec<&Value> =
        result["node"]["blocks"].as_array().unwrap().iter().map(|b| &b["node"]["items"][1]["image_id"]).collect();
    let after_ids: Vec<&Value> =
        resized["node"]["blocks"].as_array().unwrap().iter().map(|b| &b["node"]["items"][1]["image_id"]).collect();
    assert_eq!(before_ids, after_ids, "occurrence image identity must survive profile updates");
    assert_eq!(resized["node"]["blocks"][0]["node"]["items"][0]["code"], "viewport:30");

    client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "plain",
            "source": source,
            "viewport": { "supports_images": false }
        }),
    );
    let plain = settled(&mut client, "plain");
    assert!(plain["source"] == source && plain["enhanced"] == false && plain["node"]["blocks"] == json!([]));

    client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "open",
            "source": "```drawing\nopen",
            "viewport": { "supports_images": true }
        }),
    );
    assert_eq!(settled(&mut client, "open")["enhanced"], false);

    client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "cancel",
            "source": "```drawing\nrpc-cancel\n```",
            "viewport": { "supports_images": true }
        }),
    );
    assert_eq!(
        client.call("ui.cancel_markdown", json!({ "session_id": session_id, "document_id": "cancel" })),
        json!({})
    );
    client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "fence-wait",
            "source": "```drawing\nfence\n```",
            "viewport": { "supports_images": true }
        }),
    );
    settled(&mut client, "fence-wait");
    assert_eq!(settled(&mut client, "cancel")["enhanced"], false);
    assert_eq!(
        client.call("ui.delete_markdown", json!({ "session_id": session_id, "document_id": "cancel" })),
        json!({})
    );
    let err = client.call_err("ui.get_markdown", json!({ "session_id": session_id, "document_id": "cancel" }));
    assert_eq!(err["data"]["code"], "ui.unknown_document", "{err}");

    let missing_sess = client.call_err("ui.get_markdown", json!({ "document_id": "document" }));
    assert_eq!(missing_sess["data"]["code"], "invalid_arguments", "{missing_sess}");
    let bad_vp = client.call_err(
        "ui.render_markdown",
        json!({ "session_id": session_id, "document_id": "bad", "source": "x", "viewport": { "width": 0 } }),
    );
    assert_eq!(bad_vp["data"]["code"], "invalid_arguments", "{bad_vp}");
    let bad_extra = client.call_err(
        "ui.render_markdown",
        json!({ "session_id": session_id, "document_id": "bad", "source": "x", "extra": true }),
    );
    assert_eq!(bad_extra["data"]["code"], "invalid_arguments", "{bad_extra}");
    let retired_id = client.call_err(
        "ui.render_markdown",
        json!({ "session_id": session_id, "document_id": "bad", "id": "legacy", "source": "x" }),
    );
    assert_eq!(retired_id["code"], -32602, "{retired_id}");
    assert_eq!(retired_id["data"]["code"], "invalid_arguments", "{retired_id}");
    let bad_id = client
        .call_err("ui.render_markdown", json!({ "session_id": session_id, "document_id": "bad\nid", "source": "x" }));
    assert_eq!(bad_id["data"]["code"], "invalid_arguments", "{bad_id}");
    client.shutdown_and_wait();
}

#[test]
fn rpc_markdown_snapshot_changed_and_transform_project_registered_secrets() {
    const SECRET: &str = "B12A_TOP_SECRET_VALUE_98765";
    let lua = format!(
        "return {{ name = 'secret-fixture', inject = {{ 'ui' }}, apply = function(ctx)\n\
            mare.projection.register('{SECRET}')\n\
            ctx.ui:markdown_presenter('secblock', {{\n\
                render = function(r)\n\
                    return mare.ui.Text('node:' .. r.source .. ':{SECRET}')\n\
                end\n\
            }})\n\
            ctx.ui:register_markdown_transform({{ transform = function(text)\n\
                return text .. '\\ntransformed:{SECRET}'\n\
            end }})\n\
        end }}\n"
    );
    let (_temp, mut client, session_id, notifications) = spawn_rpc_markdown_client("secret-fixture.lua", &lua);

    let initial = client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "sec-doc",
            "source": format!("```secblock\nhello {SECRET}\n```")
        }),
    );
    let initial_str = serde_json::to_string(&initial).unwrap();
    assert!(!initial_str.contains(SECRET), "secret leaked in ui.render_markdown response: {initial_str}");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut settled_snap =
        client.call("ui.get_markdown", json!({ "session_id": session_id, "document_id": "sec-doc" }));
    while settled_snap["pending"].as_u64() != Some(0) && Instant::now() < deadline {
        if let Some(msg) = client.read_timeout(Duration::from_millis(200)) {
            client.dispatch(msg);
        }
        settled_snap = client.call("ui.get_markdown", json!({ "session_id": session_id, "document_id": "sec-doc" }));
    }
    assert_eq!(settled_snap["pending"], 0, "{settled_snap}");
    let snap_str = serde_json::to_string(&settled_snap).unwrap();
    assert!(!snap_str.contains(SECRET), "secret leaked in ui.get_markdown: {snap_str}");

    let notes_str = serde_json::to_string(&*notifications.lock().unwrap()).unwrap();
    assert!(!notifications.lock().unwrap().is_empty(), "expected ui.markdown_changed notification");
    assert!(!notes_str.contains(SECRET), "secret leaked in ui.markdown_changed notification: {notes_str}");

    let transformed = client.call("ui.transform_markdown", json!({ "text": format!("in {SECRET}") }));
    let transformed_str = serde_json::to_string(&transformed).unwrap();
    assert!(!transformed_str.contains(SECRET), "secret leaked in ui.transform_markdown: {transformed_str}");

    client.shutdown_and_wait();
}

#[test]
fn rpc_omitted_viewport_skips_artifact_presenter_and_path_image_falls_back_gracefully() {
    let lua = "return { name = 'fallback-fixture', inject = { 'ui' }, apply = function(ctx)\n\
        ctx.ui:markdown_presenter('artonly', {\n\
            requires_artifact_images = true,\n\
            render = function() return mare.ui.Code('should-not-run', 'text') end,\n\
        })\n\
        ctx.ui:markdown_presenter('localpath', {\n\
            render = function() return mare.ui.image({ path = '/tmp/non-transferable.png' }) end,\n\
        })\n\
    end }\n";
    let (_temp, mut client, session_id, notifications) = spawn_rpc_markdown_client("fallback-fixture.lua", lua);

    // 1. Omitted viewport must NOT claim supports_artifact_images on RPC, and image-capable
    // viewports that omit supports_artifact_images skip `requires_artifact_images` presenters.
    let omitted_doc = client.call(
        "ui.render_markdown",
        json!({ "session_id": session_id, "document_id": "omitted-doc", "source": "plain" }),
    );
    assert_eq!(omitted_doc["supports_artifact_images"], false, "{omitted_doc}");

    let art_doc = client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "art-doc",
            "source": "```artonly\nx\n```",
            "viewport": { "supports_images": true }
        }),
    );
    assert_eq!(art_doc["supports_artifact_images"], false, "{art_doc}");
    assert_eq!(art_doc["pending"], 0, "{art_doc}");
    assert_eq!(art_doc["enhanced"], false, "{art_doc}");
    assert!(
        art_doc["diagnostic"].as_str().is_some_and(|d| d.contains("artifact images")),
        "expected artifact diagnostic: {art_doc}"
    );

    // 2. Non-transferable local path image must fall back cleanly to source + diagnostic and still notify.
    client.call(
        "ui.render_markdown",
        json!({
            "session_id": session_id,
            "document_id": "path-doc",
            "source": "```localpath\nkeep-source\n```",
            "viewport": { "supports_images": true }
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !notifications.lock().unwrap().iter().any(|n| n["document_id"] == "path-doc" && n["pending"] == 0)
        && Instant::now() < deadline
    {
        if let Some(msg) = client.read_timeout(Duration::from_millis(200)) {
            client.dispatch(msg);
        }
    }
    let path_note = notifications
        .lock()
        .unwrap()
        .iter()
        .find(|n| n["document_id"] == "path-doc" && n["pending"] == 0)
        .cloned()
        .expect("expected settled ui.markdown_changed notification even when node_to_wire falls back");
    assert_eq!(path_note["source"], "```localpath\nkeep-source\n```");
    assert_eq!(path_note["node"]["blocks"], json!([]));
    assert!(path_note["diagnostic"].is_string(), "{path_note}");

    client.shutdown_and_wait();
}
