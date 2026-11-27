//! End-to-end headless `--json` tests for empty and image-only tool results.

use std::process::Stdio;

use crate::common::*;
use serde_json::{Value, json};

const PNG_1X1_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";

#[test]
fn headless_json_serializes_empty_and_image_only_tool_results_as_arrays() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("empty_tools.lua"),
        format!(
            r#"return {{
  name = "empty_tools",
  inject = {{ "tools" }},  apply = function(ctx)
    ctx.tools:register("empty_table_tool", {{
      description = "returns an untagged empty content table",
      parameters = {{ type = "object", properties = {{}} }},
      side_effect = "read",
      execute = function() return {{ content = {{}} }} end,
    }})
    ctx.tools:register("nil_content_tool", {{
      description = "returns a table without content",
      parameters = {{ type = "object", properties = {{}} }},
      side_effect = "read",
      execute = function() return {{}} end,
    }})
    ctx.tools:register("image_only_tool", {{
      description = "returns an image-only content list",
      parameters = {{ type = "object", properties = {{}} }},
      side_effect = "read",
      execute = function()
        return {{ content = {{ {{ type = "image", data = "{PNG_1X1_B64}", mime = "image/png" }} }} }}
      end,
    }})
  end,
}}"#
        ),
    )
    .unwrap();

    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            {
                "tool_calls": [
                    { "name": "empty_table_tool", "arguments": {} },
                    { "name": "nil_content_tool", "arguments": {} },
                    { "name": "image_only_tool", "arguments": {} }
                ]
            },
            { "text": "done" }
        ])
        .to_string(),
    )
    .unwrap();

    let out = mare_cmd()
        .args(["--json", "--yolo", "--model", "faux/echo", "run it"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let events: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid JSON line ({e}): {l}")))
        .collect();

    let session_rec = events.iter().find(|e| e["type"] == "session").expect("session event");
    let session_path = if let Some(p) = session_rec["data"]["path"].as_str() {
        std::path::PathBuf::from(p)
    } else {
        let id = session_rec["data"]["session_id"].as_str().expect("session event with session_id");
        let mut found = None;
        let mut stack = vec![home.path().to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "jsonl")
                    && std::fs::read_to_string(&p)
                        .ok()
                        .and_then(|t| t.lines().next().map(str::to_string))
                        .and_then(|l| serde_json::from_str::<Value>(&l).ok())
                        .is_some_and(|h| h["type"] == "session" && h["session_id"] == id)
                {
                    found = Some(p);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        found.expect("session jsonl file under MARE_HOME")
    };

    let expected_names = ["empty_table_tool", "nil_content_tool", "image_only_tool"];

    let expected_empty = json!([]);
    let expected_image_content = json!([{ "type": "image", "data": PNG_1X1_B64, "mime": "image/png" }]);
    let expected_content = |name: &str| {
        if name == "image_only_tool" { &expected_image_content } else { &expected_empty }
    };

    let tool_items: Vec<&Value> =
        events.iter().filter(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call").collect();
    let res_names: Vec<&str> = tool_items.iter().filter_map(|e| e["data"]["item"]["name"].as_str()).collect();
    assert_eq!(res_names, expected_names, "{tool_items:?}");
    for ev in &tool_items {
        let item = &ev["data"]["item"];
        let name = item["name"].as_str().unwrap();
        assert_eq!(item["result_status"], "success", "{ev}");
        let content = &item["message"]["content"];
        assert!(content.is_array(), "item/completed tool_result: {ev}");
        assert_eq!(content, expected_content(name), "item/completed tool_result: {ev}");
    }

    let empty_tool_end = events
        .iter()
        .find(|e| {
            e["type"] == "item/completed"
                && e["data"]["item"]["type"] == "tool_call"
                && e["data"]["item"]["name"] == "empty_table_tool"
        })
        .expect("item/completed for empty_table_tool");
    assert!(empty_tool_end["data"]["item"]["result"].get("content").is_none(), "{empty_tool_end}");

    assert!(events.iter().any(|e| e["type"] == "step/completed"), "{events:?}");
    let agent_end = events.iter().find(|e| e["type"] == "turn/completed").expect("turn/completed");
    assert_eq!(agent_end["data"]["status"], "completed", "{agent_end}");

    // Image-only result stays inside tool_result without a synthetic user message.
    let has_image_user_msg = events.iter().any(|e| {
        e["type"] == "item/completed"
            && e["data"]["item"]["type"] == "user_message"
            && e["data"]["item"]["message"]["content"]
                .as_array()
                .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "image" && b["data"] == PNG_1X1_B64))
    });
    assert!(!has_image_user_msg, "unexpected synthetic user message with extracted image");

    // Durable session JSONL entries must also preserve empty arrays and image blocks.
    let session_lines: Vec<Value> =
        std::fs::read_to_string(&session_path).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let persisted_tool_results: Vec<&Value> =
        session_lines.iter().filter(|e| e["type"] == "message" && e["message"]["role"] == "tool_result").collect();
    assert_eq!(persisted_tool_results.len(), 3, "{persisted_tool_results:?}");
    for entry in persisted_tool_results {
        let name = entry["message"]["tool_name"].as_str().unwrap();
        assert_eq!(&entry["message"]["content"], expected_content(name), "{entry}");
    }
}

fn push_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = 0xffff_ffffu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    out.extend_from_slice(&(!crc).to_be_bytes());
}

fn build_high_entropy_png(width: u32, height: u32, min_disk_bytes: usize) -> Vec<u8> {
    // High-entropy RGB PNG using uncompressed zlib stored blocks so `mare.image.load`
    // re-encodes a genuinely large PNG (unlike a 1x1 padded PNG).
    let stride = 1 + (width as usize) * 3;
    let mut raw = Vec::with_capacity(stride * (height as usize));
    let mut state = 0x1234_5678_9abc_def0u64;
    for _y in 0..height {
        raw.push(0);
        for _x in 0..width {
            for _ in 0..3 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                raw.push((state & 0xff) as u8);
            }
        }
    }
    let mut s1 = 1u32;
    let mut s2 = 0u32;
    for &b in &raw {
        s1 = (s1 + u32::from(b)) % 65521;
        s2 = (s2 + s1) % 65521;
    }
    let mut zlib = vec![0x78, 0x01];
    for (idx, chunk) in raw.chunks(65535).enumerate() {
        let is_last = (idx + 1) * 65535 >= raw.len();
        zlib.push(u8::from(is_last));
        let len = chunk.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(chunk);
    }
    zlib.extend_from_slice(&((s2 << 16) | s1).to_be_bytes());

    let mut png = vec![137, 80, 78, 71, 13, 10, 26, 10];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    push_png_chunk(&mut png, b"IHDR", &ihdr);
    push_png_chunk(&mut png, b"IDAT", &zlib);
    if png.len() + 24 < min_disk_bytes {
        let pad_len = min_disk_bytes - png.len() - 24;
        let mut text = Vec::with_capacity(pad_len.max(8));
        text.extend_from_slice(b"pad\0");
        text.resize(text.len() + pad_len.saturating_sub(4), b'A');
        push_png_chunk(&mut png, b"tEXt", &text);
    }
    push_png_chunk(&mut png, b"IEND", &[]);
    png
}

fn fetch_full_item_via_rpc(rpc: &mut crate::common::rpc::RpcClient, session_id: &str, item_id: &str) -> Value {
    let mut offset = 0u64;
    let mut raw = String::new();
    loop {
        let res = rpc.call(
            "session.get_item",
            json!({
                "session_id": session_id,
                "item_id": item_id,
                "offset": offset,
                "max_bytes": 65536,
            }),
        );
        assert!(res.get("error").is_none(), "session.get_item failed: {res}");
        let chunk = res["chunk"].as_str().expect("chunk string");
        raw.push_str(chunk);
        match res["next_offset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    serde_json::from_str(&raw).expect("valid full item JSON")
}

#[test]
fn headless_read_420kb_and_oversized_png_never_drop_and_get_item_returns_full_image() {
    // 1. 420 KB PNG: single carrier (`item.message`, no duplicate `item.result.content`) fits
    //    comfortably under EVENT_MAX_BYTES without dropping.
    {
        let home = tempfile::tempdir().unwrap();
        let png = build_high_entropy_png(380, 380, 420 * 1024);
        assert!(png.len() >= 420 * 1024);
        std::fs::write(home.path().join("medium.png"), &png).unwrap();
        let script = home.path().join("faux.json");
        std::fs::write(
            &script,
            json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "medium.png" } }] },
                { "text": "read 420kb ok" }
            ])
            .to_string(),
        )
        .unwrap();

        let out = mare_cmd()
            .args(["-p", "--json", "--yolo", "--model", "faux/echo", "read medium.png"])
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script)
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let events: Vec<Value> =
            String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert!(!events.iter().any(|e| e["type"] == "dropped"), "420KB PNG read must not drop: {events:?}");
        let completed = events
            .iter()
            .find(|e| e["type"] == "item/completed" && e["data"]["item"]["type"] == "tool_call")
            .expect("missing tool_call item/completed");
        assert!(
            completed["data"]["item"]["result"].get("content").is_none(),
            "item.result must not duplicate content: {completed}"
        );
        assert!(
            completed["data"]["item"]["result"].get("details").is_none(),
            "item.result must not duplicate details: {completed}"
        );
        mare_protocol::events::validate_protocol("item/completed", &completed["data"]).unwrap();
    }

    // 2. >1 MiB PNG over RPC/headless: wire projects image block to truncated ref, zero `dropped`,
    //    and `session.get_item(item_id = call_id)` returns the complete base64 image.
    {
        let home = tempfile::tempdir().unwrap();
        let png = build_high_entropy_png(650, 650, 1_520_000);
        assert!(png.len() >= 1_500_000);
        std::fs::write(home.path().join("big.png"), &png).unwrap();
        let script = home.path().join("faux.json");
        std::fs::write(
            &script,
            json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "big.png" } }] },
                { "text": "read big ok" }
            ])
            .to_string(),
        )
        .unwrap();

        let mut env = std::collections::BTreeMap::new();
        env.insert("MARE_HOME".into(), home.path().display().to_string());
        env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
        let mut rpc =
            crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, home.path());
        let init = rpc.call("initialize", json!({ "protocol_version": "3.0" }));
        let session_id = init["session"]["session_id"].as_str().expect("session_id").to_string();
        let prompt = rpc.call("agent.prompt", json!({ "session_id": session_id, "input": "read big.png" }));
        let turn_id = prompt["turn_id"].as_str().expect("turn_id").to_string();
        let mut saw_dropped = false;
        let mut tool_completed = None;
        let mut turn_done = false;
        for ev in rpc.events.drain(..) {
            if ev["type"] == "item/completed" && ev["data"]["item"]["type"] == "tool_call" {
                tool_completed = Some(ev["data"].clone());
            } else if ev["type"] == "turn/completed" && ev["data"]["turn_id"] == turn_id {
                turn_done = true;
            }
        }
        while !turn_done {
            let msg = rpc.read();
            match msg["method"].as_str() {
                Some("dropped") => saw_dropped = true,
                Some("event") => {
                    let p = &msg["params"];
                    if p["type"] == "item/completed" && p["data"]["item"]["type"] == "tool_call" {
                        tool_completed = Some(p["data"].clone());
                    } else if p["type"] == "turn/completed" && p["data"]["turn_id"] == turn_id {
                        turn_done = true;
                    }
                }
                _ => {}
            }
        }
        assert!(!saw_dropped, "oversized PNG read must be projected, never dropped");
        let tool_completed = tool_completed.expect("missing projected tool_call item/completed");
        mare_protocol::events::validate_protocol("item/completed", &tool_completed).unwrap();
        let call_id = tool_completed["item"]["tool_call_id"].as_str().expect("tool_call_id");
        let img = tool_completed["item"]["message"]["content"]
            .as_array()
            .and_then(|arr| arr.iter().find(|b| b["type"] == "image"))
            .expect("projected image block");
        assert_eq!(img["mime"], "image/png", "{img}");
        assert_eq!(img["path"], "big.png", "{img}");
        assert_eq!(img["truncated"], true, "{img}");
        assert!(img.get("data").is_none(), "projected image ref must omit base64 data: {img}");
        assert!(img["bytes"].as_u64().unwrap_or(0) >= 950 * 1024, "bytes must reflect decoded image size: {img}");

        let full = fetch_full_item_via_rpc(&mut rpc, &session_id, call_id);
        let full_img = full["result"]["content"]
            .as_array()
            .or_else(|| full["message"]["content"].as_array())
            .and_then(|arr| arr.iter().find(|b| b["type"] == "image"))
            .expect("full image in session.get_item");
        let full_b64 = full_img["data"].as_str().expect("full base64 data in session.get_item");
        assert!(full_b64.len() > mare_kernel::EVENT_MAX_BYTES, "full base64 must be preserved: {}", full_b64.len());

        let _ = rpc.call("shutdown", json!({}));
        rpc.wait_exit();
    }
}

#[test]
fn headless_oversized_write_over_1mib_never_drops_and_get_item_returns_full_content() {
    let home = tempfile::tempdir().unwrap();
    let big_body = "W".repeat(mare_kernel::EVENT_MAX_BYTES + 128 * 1024);
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "huge.txt", "content": big_body } }] },
            { "text": "wrote huge file" }
        ])
        .to_string(),
    )
    .unwrap();

    let mut env = std::collections::BTreeMap::new();
    env.insert("MARE_HOME".into(), home.path().display().to_string());
    env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
    let mut rpc = crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, home.path());
    let init = rpc.call("initialize", json!({ "protocol_version": "3.0" }));
    let session_id = init["session"]["session_id"].as_str().expect("session_id").to_string();
    let prompt = rpc.call("agent.prompt", json!({ "session_id": session_id, "input": "write huge.txt" }));
    let turn_id = prompt["turn_id"].as_str().expect("turn_id").to_string();

    let mut saw_dropped = false;
    let mut started = None;
    let mut completed = None;
    let mut turn_done = false;
    for ev in rpc.events.drain(..) {
        if ev["type"] == "item/started" && ev["data"]["item"]["type"] == "tool_call" {
            started = Some(ev["data"].clone());
        } else if ev["type"] == "item/completed" && ev["data"]["item"]["type"] == "tool_call" {
            completed = Some(ev["data"].clone());
        } else if ev["type"] == "turn/completed" && ev["data"]["turn_id"] == turn_id {
            turn_done = true;
        }
    }
    while !turn_done {
        let msg = rpc.read();
        match msg["method"].as_str() {
            Some("dropped") => saw_dropped = true,
            Some("event") => {
                let p = &msg["params"];
                if p["type"] == "item/started" && p["data"]["item"]["type"] == "tool_call" {
                    started = Some(p["data"].clone());
                } else if p["type"] == "item/completed" && p["data"]["item"]["type"] == "tool_call" {
                    completed = Some(p["data"].clone());
                } else if p["type"] == "turn/completed" && p["data"]["turn_id"] == turn_id {
                    turn_done = true;
                }
            }
            _ => {}
        }
    }
    assert!(!saw_dropped, ">1 MiB write must be projected on outbound tap, never dropped");
    let started = started.expect("missing projected tool_call item/started");
    let completed = completed.expect("missing projected tool_call item/completed");
    mare_protocol::events::validate_protocol("item/started", &started).unwrap();
    mare_protocol::events::validate_protocol("item/completed", &completed).unwrap();
    assert_eq!(started["item"]["arguments"]["path"], "huge.txt", "{started}");
    assert_eq!(started["item"]["truncated"], true, "oversized item/started must be marked truncated: {started}");
    assert_eq!(completed["item"]["truncated"], true, "oversized item/completed must be marked truncated: {completed}");
    assert_eq!(std::fs::read_to_string(home.path().join("huge.txt")).unwrap().len(), big_body.len());

    let call_id = completed["item"]["tool_call_id"].as_str().expect("tool_call_id");
    let full = fetch_full_item_via_rpc(&mut rpc, &session_id, call_id);
    assert_eq!(full["arguments"]["content"].as_str().map(str::len), Some(big_body.len()));

    let _ = rpc.call("shutdown", json!({}));
    rpc.wait_exit();
}

#[tokio::test]
async fn oversized_pasted_image_projects_on_wire_while_inprocess_listener_receives_raw() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "nice pic" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            INPROC_IMG_LEN = 0
            C.cordis:on("item/completed", function(ev)
                local item = ev and ev.item
                if item and item.type == "user_message" and item.message and item.message.content then
                    for _, b in ipairs(item.message.content) do
                        if b.type == "image" and type(b.data) == "string" then
                            INPROC_IMG_LEN = #b.data
                        end
                    end
                end
            end)
            "#,
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let png = build_high_entropy_png(650, 650, 1_520_000);
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let res = h
            .prompt("look at this", json!({ "images": [{ "type": "image", "mime": "image/png", "data": b64 }] }))
            .await
            .unwrap();
        assert_eq!(res["text"], "nice pic", "{res}");

        let inproc_len: usize = eval(&h, "return INPROC_IMG_LEN").await;
        assert_eq!(inproc_len, b64.len(), "in-process listener must receive full raw image data");

        let mut drops = 0u64;
        let mut user_completed = None;
        while let Ok(Some(rec)) = cursor.try_recv() {
            match rec {
                mare_kernel::CursorRecord::Dropped { count, .. } => drops += count,
                mare_kernel::CursorRecord::Event { event, .. } => {
                    if event.name == "item/completed" && event.payload["item"]["type"] == "user_message" {
                        user_completed = Some(event.payload);
                    }
                }
                mare_kernel::CursorRecord::Lagged { .. } => {}
            }
        }
        assert_eq!(drops, 0, "pasted oversized image must project on wire without dropping");
        let user_completed = user_completed.expect("missing user_message item/completed on cursor");
        let img = user_completed["item"]["message"]["content"]
            .as_array()
            .and_then(|arr| arr.iter().find(|b| b["type"] == "image"))
            .expect("projected image block on user_message");
        assert_eq!(img["mime"], "image/png", "{img}");
        assert_eq!(img["truncated"], true, "{img}");
        assert!(img.get("data").is_none(), "wire image block must omit base64 data: {img}");
        assert!(img["bytes"].as_u64().unwrap_or(0) >= 950 * 1024, "{img}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn tui_write_and_edit_rendering_reads_item_message_without_regression() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "name": "write", "arguments": { "path": "demo.txt", "content": "alpha\nbeta\n" } }] },
                { "tool_calls": [{ "name": "read", "arguments": { "path": "demo.txt" } }] },
                { "tool_calls": [{ "name": "edit", "arguments": { "path": "demo.txt", "old_string": "beta", "new_string": "gamma" } }] },
                { "text": "updated" }
            ]))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            LIVE_BY_ID = {}
            LIVE_ORDER = {}
            REPLAY_BY_ID = {}
            REPLAY_ORDER = {}
            NEXT_UID = 0
            local real_ui = mare.ui
            local stub_ui = setmetatable({
                connected = function() return true end,
                width = function() return 100 end,
                icon_set = function() return "ascii" end,
                clear = function() end,
                status = function() end,
                status_clear = function() end,
                set_editor = function() end,
                finish = function() end,
                push = function(kind, opts)
                    NEXT_UID = NEXT_UID + 1
                    local id = NEXT_UID
                    if kind == "tool" and type(opts) == "table" then
                        local rec = {
                            node = opts.node and real_ui.snapshot_node(opts.node) or nil,
                            expanded = opts.expanded and real_ui.snapshot_node(opts.expanded) or nil,
                        }
                        if IN_REPLAY then
                            REPLAY_ORDER[#REPLAY_ORDER + 1] = id
                            REPLAY_BY_ID[id] = rec
                        else
                            LIVE_ORDER[#LIVE_ORDER + 1] = id
                            LIVE_BY_ID[id] = rec
                        end
                    end
                    return id
                end,
                update = function(id, opts)
                    local bucket = IN_REPLAY and REPLAY_BY_ID or LIVE_BY_ID
                    if bucket[id] and type(opts) == "table" and opts.node ~= nil then
                        bucket[id] = {
                            node = real_ui.snapshot_node(opts.node),
                            expanded = opts.expanded and real_ui.snapshot_node(opts.expanded) or nil,
                        }
                    end
                end,
            }, { __index = real_ui })
            local cfg = {
                preview_lines = 2,
                diff_fold_lines = 8,
                group_lines = 4,
                tool_update_interval_ms = 100,
            }
            local D = {
                ctx = C.cordis,
                cfg = cfg,
                U = stub_ui,
                classify = require("ui.classify"),
                service = { service = { get_keymap = function() return { bindings = {} } end } },
                native_markdown_document = function() error("disabled in test") end,
            }
            D.tool_nodes = require("ui.tool_nodes")(D)
            require("ui.transcript")(D)
            TOOL_ITEMS = {}
            C.cordis:on("item/completed", function(ev)
                local item = ev and ev.item
                if item and item.type == "tool_call" then
                    TOOL_ITEMS[#TOOL_ITEMS + 1] = item
                end
            end)
            "#,
        )
        .await;
        let res = h.prompt("write and edit demo.txt", json!({})).await.unwrap();
        assert_eq!(res["text"], "updated", "{res}");
        let check: String = eval(
            &h,
            r#"
            assert(#TOOL_ITEMS == 3, "expected 3 tool items, got " .. tostring(#TOOL_ITEMS))
            for _, item in ipairs(TOOL_ITEMS) do
                assert(type(item.message) == "table", "missing item.message")
                if item.result ~= nil then
                    assert(item.result.content == nil, "item.result must not carry content")
                    assert(item.result.details == nil, "item.result must not carry details")
                end
            end
            -- Replay the exact persisted messages through ui/transcript.lua's session/loaded handler.
            IN_REPLAY = true
            C.cordis.bus:emit("session/loaded", { messages = A:list_messages().data })
            -- Both live and replay produce 3 tool blocks: write card, read exploration group, edit card.
            assert(#LIVE_ORDER == 3, "expected 3 live tool blocks, got " .. tostring(#LIVE_ORDER))
            assert(#REPLAY_ORDER == 3, "expected 3 replay tool blocks, got " .. tostring(#REPLAY_ORDER))
            for _, idx in ipairs({ 1, 3 }) do
                local live = LIVE_BY_ID[LIVE_ORDER[idx]]
                local replay = REPLAY_BY_ID[REPLAY_ORDER[idx]]
                assert(live and replay, "missing live/replay block at " .. tostring(idx))
                local live_n = mare.json.encode(live.node)
                local rep_n = mare.json.encode(replay.node)
                assert(live_n == rep_n, "collapsed mismatch at " .. idx .. ":\nlive=" .. live_n .. "\nreplay=" .. rep_n)
                local live_e = mare.json.encode(live.expanded)
                local rep_e = mare.json.encode(replay.expanded)
                assert(live_e == rep_e, "expanded mismatch at " .. idx .. ":\nlive=" .. live_e .. "\nreplay=" .. rep_e)
                assert(not live_n:find("render error", 1, true), "render error in " .. live_n)
            end
            return "ok"
            "#,
        )
        .await;
        assert_eq!(check, "ok");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn rpc_content_part_and_attachment_large_image_project_turn_started_without_drop() {
    use base64::Engine;
    let png = build_high_entropy_png(650, 650, 1_520_000);
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    assert!(b64.len() > mare_kernel::EVENT_MAX_BYTES);

    // 1. RPC ContentPart[] image prompt:
    {
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("faux.json");
        std::fs::write(&script, json!([{ "text": "part ok" }]).to_string()).unwrap();

        let mut env = std::collections::BTreeMap::new();
        env.insert("MARE_HOME".into(), home.path().display().to_string());
        env.insert("MARE_FAUX_SCRIPT".into(), script.display().to_string());
        let mut rpc =
            crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env, home.path());
        let init = rpc.call("initialize", json!({ "protocol_version": "3.0" }));
        let session_id = init["session"]["session_id"].as_str().expect("session_id").to_string();

        let prompt = rpc.call(
            "agent.prompt",
            json!({
                "session_id": session_id,
                "input": [
                    { "type": "text", "text": "inspect part" },
                    { "type": "image", "mime": "image/png", "data": b64, "path": "part.png" }
                ]
            }),
        );
        let turn_id = prompt["turn_id"].as_str().expect("turn_id").to_string();
        let mut saw_dropped = false;
        let mut turn_started = None;
        let mut user_messages = Vec::new();
        let mut turn_done = false;
        for ev in rpc.events.drain(..) {
            if ev["type"] == "turn/started" && ev["data"]["turn_id"] == turn_id {
                turn_started = Some(ev["data"].clone());
            } else if ev["type"] == "item/completed"
                && ev["data"]["turn_id"] == turn_id
                && ev["data"]["item"]["type"] == "user_message"
            {
                user_messages.push(ev["data"].clone());
            } else if ev["type"] == "turn/completed" && ev["data"]["turn_id"] == turn_id {
                turn_done = true;
            }
        }
        while !turn_done {
            let msg = rpc.read();
            match msg["method"].as_str() {
                Some("dropped") => saw_dropped = true,
                Some("event") => {
                    let p = &msg["params"];
                    if p["type"] == "turn/started" && p["data"]["turn_id"] == turn_id {
                        turn_started = Some(p["data"].clone());
                    } else if p["type"] == "item/completed"
                        && p["data"]["turn_id"] == turn_id
                        && p["data"]["item"]["type"] == "user_message"
                    {
                        user_messages.push(p["data"].clone());
                    } else if p["type"] == "turn/completed" && p["data"]["turn_id"] == turn_id {
                        turn_done = true;
                    }
                }
                _ => {}
            }
        }
        assert!(!saw_dropped, "content_part: oversized image prompt must never drop");
        let ts = turn_started.expect("content_part: missing turn/started");
        mare_protocol::events::validate_protocol("turn/started", &ts).unwrap();
        assert_eq!(ts["truncated"], true, "content_part: turn/started must be marked truncated: {ts}");
        let ts_img = &ts["images"][0];
        assert_eq!(ts_img["type"], "image", "content_part: turn/started image must carry type: {ts_img}");
        assert_eq!(ts_img["mime"], "image/png", "{ts_img}");
        assert_eq!(ts_img["path"], "part.png", "{ts_img}");
        assert_eq!(ts_img["truncated"], true, "{ts_img}");
        assert!(ts_img.get("data").is_none(), "content_part: turn/started image must omit inline data: {ts_img}");
        assert!(ts_img["bytes"].as_u64().unwrap_or(0) >= 1_500_000, "{ts_img}");

        let uc_img = user_messages
            .iter()
            .find_map(|uc| {
                mare_protocol::events::validate_protocol("item/completed", uc).unwrap();
                uc["item"]["message"]["content"].as_array().and_then(|arr| arr.iter().find(|b| b["type"] == "image"))
            })
            .unwrap_or_else(|| panic!("content_part: missing projected user_message image block: {user_messages:?}"));
        assert_eq!(uc_img["truncated"], true, "{uc_img}");
        assert!(uc_img.get("data").is_none(), "{uc_img}");

        let _ = rpc.call("shutdown", json!({}));
        rpc.wait_exit();
    }

    // 2. Type-less `{ mime, data }` image via Lua `opts.images` (verifies Stage A shape detection):
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "att ok" }])).no_session().build().await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let res = h
            .prompt(
                "inspect attachment",
                json!({
                    "images": [
                        { "mime": "image/png", "data": b64, "path": "att.png" }
                    ]
                }),
            )
            .await
            .unwrap();
        assert_eq!(res["text"], "att ok", "{res}");

        let mut drops = 0u64;
        let mut turn_started = None;
        let mut user_messages = Vec::new();
        while let Ok(Some(rec)) = cursor.try_recv() {
            match rec {
                mare_kernel::CursorRecord::Dropped { count, .. } => drops += count,
                mare_kernel::CursorRecord::Event { event, .. } => {
                    if event.name == "turn/started" {
                        turn_started = Some(event.payload);
                    } else if event.name == "item/completed" && event.payload["item"]["type"] == "user_message" {
                        user_messages.push(event.payload);
                    }
                }
                mare_kernel::CursorRecord::Lagged { .. } => {}
            }
        }
        assert_eq!(drops, 0, "attachment: oversized image prompt must never drop");
        let ts = turn_started.expect("attachment: missing turn/started");
        mare_protocol::events::validate_protocol("turn/started", &ts).unwrap();
        assert_eq!(ts["truncated"], true, "{ts}");
        let ts_img = &ts["images"][0];
        assert_eq!(ts_img["type"], "image", "{ts_img}");
        assert_eq!(ts_img["mime"], "image/png", "{ts_img}");
        assert_eq!(ts_img["path"], "att.png", "{ts_img}");
        assert_eq!(ts_img["truncated"], true, "{ts_img}");
        assert!(ts_img.get("data").is_none(), "{ts_img}");
        assert!(ts_img["bytes"].as_u64().unwrap_or(0) >= 1_500_000, "{ts_img}");

        let uc_img = user_messages
            .iter()
            .find_map(|uc| {
                mare_protocol::events::validate_protocol("item/completed", uc).unwrap();
                uc["item"]["message"]["content"].as_array().and_then(|arr| arr.iter().find(|b| b["type"] == "image"))
            })
            .unwrap_or_else(|| panic!("attachment: missing projected user_message image block: {user_messages:?}"));
        assert_eq!(uc_img["truncated"], true, "{uc_img}");
        assert!(uc_img.get("data").is_none(), "{uc_img}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn plugin_tool_image_blocks_normalize_and_output_truncated_renames_on_wire() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "name": "quirky_plugin_tool", "arguments": {} }] },
                { "text": "handled" }
            ]))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            &format!(
                r#"
                C.cordis:get("tools"):register("quirky_plugin_tool", {{
                    description = "returns unnormalized image blocks and >50KB text triggering output truncation",
                    parameters = {{ type = "object", properties = {{}} }},
                    side_effect = "read",
                    execute = function()
                        return {{
                            content = {{
                                {{ type = "image", media_type = "image/png", data = "{PNG_1X1_B64}" }},
                                {{ type = "image", mime = "image/png" }},
                                {{ type = "image", data = "{PNG_1X1_B64}" }},
                                {{ type = "text", text = string.rep(string.rep("z", 100) .. "\n", 1000) }},
                            }},
                        }}
                    end,
                }})
                "#
            ),
        )
        .await;
        let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let res = h.prompt("run quirky plugin tool", json!({})).await.unwrap();
        assert_eq!(res["text"], "handled", "{res}");

        let mut completed = None;
        while let Ok(Some(rec)) = cursor.try_recv() {
            if let mare_kernel::CursorRecord::Event { event, .. } = rec
                && event.name == "item/completed"
                && event.payload["item"]["type"] == "tool_call"
            {
                completed = Some(event.payload);
            }
        }
        let completed = completed.expect("missing tool_call item/completed");
        mare_protocol::events::validate_protocol("item/completed", &completed).unwrap();
        let blocks = completed["item"]["message"]["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 4, "{blocks:?}");
        assert_eq!(blocks[0]["type"], "image");
        assert_eq!(blocks[0]["mime"], "image/png");
        assert_eq!(blocks[0]["data"], PNG_1X1_B64);
        assert_eq!(blocks[1]["type"], "text", "data-less image block must degrade safely: {blocks:?}");
        assert_eq!(blocks[2]["type"], "text", "mime-less image block must degrade safely to text: {blocks:?}");
        assert_eq!(blocks[2]["text"], "[invalid image block]");
        assert_eq!(
            completed["item"]["result"]["output_truncated"], true,
            "tool output truncation must map to item.result.output_truncated: {completed}"
        );
        assert!(
            completed["item"]["result"].get("truncated").is_none(),
            "item.result must not use wire-reserved `truncated`: {completed}"
        );
        h.shutdown().await;
    })
    .await;
}
