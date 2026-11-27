//! Outbound wire payload bounding at the event-tap boundary.
//!
//! Runs only after an event's first wire JSON encoding exceeds the log's byte limit.
//! In-process listeners always receive the original unprojected payload; external streams
//! receive bounded payloads without writing to disk:
//! 1. Project image blocks (`type == "image"` with inline `data`) into
//!    `{ type: "image", mime, path?, bytes, truncated: true }`.
//! 2. If still oversized, truncate the largest non-identity string fields on UTF-8 char
//!    boundaries in descending size order and mark `truncated: true`.
//! 3. If still oversized and the event is `item/completed`, fall back to
//!    `{ turn_id?, item_id, status, item_omitted: true }`.
//! 4. If still oversized (irreducible payload), return `Ok(None)` so the caller records `Dropped`.

use std::sync::Arc;

use mare_protocol::events::FIELD_STATUS;
use serde_json::{Map, Value, json};

use super::log::{EventIdentity, encode_wire_event, payload_turn_id};

const TRUNCATED_KEY_BYTES: usize = r#","truncated":true"#.len();

/// Keys that carry structural identity, type tags, or locator metadata and must not be truncated.
/// Covers every `NonEmptyString` protocol field (`*_id`, `*_ids`, `trigger`, `name`, `mime`) as well.
fn is_protected_key(key: &str) -> bool {
    key == "id"
        || key.ends_with("_id")
        || key.ends_with("_ids")
        || key == FIELD_STATUS
        || matches!(
            key,
            "trigger"
                | "type"
                | "role"
                | "result_status"
                | "execution_status"
                | "session_epoch"
                | "name"
                | "tool_name"
                | "mime"
                | "path"
                | "kind"
                | "level"
                | "reason"
        )
}

/// Compute exact decoded byte length of a standard base64 string without allocating.
fn decoded_base64_len(b64: &str) -> u64 {
    let mut valid = 0usize;
    let mut pad = 0usize;
    for b in b64.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad += 1;
        }
        valid += 1;
    }
    if valid == 0 {
        return 0;
    }
    ((valid / 4) * 3).saturating_sub(pad.min(2)) as u64
}

/// True when `map` is an inline image block: either `type == "image"` with string `data`,
/// or a type-less `{ mime, data }` image object (e.g. `turn/started.images`).
fn is_inline_image(map: &Map<String, Value>) -> bool {
    let Some(Value::String(_)) = map.get("data") else {
        return false;
    };
    match map.get("type").and_then(Value::as_str) {
        Some("image") => true,
        None => map.get("mime").and_then(Value::as_str).is_some_and(|m| !m.is_empty()),
        _ => false,
    }
}

/// Stage A: replace inline image objects with truncated reference blocks without cloning base64.
/// Any `path` field supplied by the producer on the image block is preserved as-is.
fn project_images(value: &mut Value) -> bool {
    match value {
        Value::Object(map) => {
            if is_inline_image(map) {
                let bytes = map.get("data").and_then(Value::as_str).map_or(0, decoded_base64_len);
                map.remove("data");
                map.insert("type".to_owned(), Value::String("image".to_owned()));
                let has_mime = map.get("mime").and_then(Value::as_str).is_some_and(|m| !m.is_empty());
                if !has_mime {
                    map.insert("mime".to_owned(), Value::String("application/octet-stream".to_owned()));
                }
                map.insert("bytes".to_owned(), json!(bytes));
                map.insert("truncated".to_owned(), Value::Bool(true));
                return true;
            }
            let mut changed = false;
            for v in map.values_mut() {
                if project_images(v) {
                    changed = true;
                }
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                if project_images(item) {
                    changed = true;
                }
            }
            changed
        }
        _ => false,
    }
}

#[derive(Clone)]
enum PathSeg {
    Key(String),
    Index(usize),
}

fn collect_truncatable_strings(value: &Value, path: &mut Vec<PathSeg>, out: &mut Vec<(Vec<PathSeg>, usize, bool)>) {
    match value {
        Value::Object(map) => {
            let is_text_block = map.get("type").and_then(Value::as_str) == Some("text")
                && map.get("truncated").and_then(Value::as_bool) != Some(true);
            for (k, v) in map {
                if is_protected_key(k) {
                    continue;
                }
                path.push(PathSeg::Key(k.clone()));
                if let Value::String(s) = v {
                    if !s.is_empty() {
                        out.push((path.clone(), s.len(), is_text_block && k == "text"));
                    }
                } else {
                    collect_truncatable_strings(v, path, out);
                }
                path.pop();
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                path.push(PathSeg::Index(i));
                if let Value::String(s) = item {
                    if !s.is_empty() {
                        out.push((path.clone(), s.len(), false));
                    }
                } else {
                    collect_truncatable_strings(item, path, out);
                }
                path.pop();
            }
        }
        _ => {}
    }
}

/// Truncate the string at `path` to at most `new_len` UTF-8 bytes and mark `truncated: true`
/// on a text block when applicable.
fn truncate_string_at(root: &mut Value, path: &[PathSeg], new_len: usize) -> bool {
    let mut cur = root;
    let Some((last, parents)) = path.split_last() else {
        return false;
    };
    for seg in parents {
        cur = match (cur, seg) {
            (Value::Object(m), PathSeg::Key(k)) => match m.get_mut(k) {
                Some(v) => v,
                None => return false,
            },
            (Value::Array(a), PathSeg::Index(i)) => match a.get_mut(*i) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }
    match (cur, last) {
        (Value::Object(parent_map), PathSeg::Key(k)) => {
            let Some(Value::String(s)) = parent_map.get_mut(k) else {
                return false;
            };
            if s.len() <= new_len {
                return false;
            }
            let cut = s.floor_char_boundary(new_len);
            s.truncate(cut);
            if k == "text" && parent_map.get("type").and_then(Value::as_str) == Some("text") {
                parent_map.insert("truncated".to_owned(), Value::Bool(true));
            }
            true
        }
        (Value::Array(arr), PathSeg::Index(i)) => {
            let Some(Value::String(s)) = arr.get_mut(*i) else {
                return false;
            };
            if s.len() <= new_len {
                return false;
            }
            let cut = s.floor_char_boundary(new_len);
            s.truncate(cut);
            true
        }
        _ => false,
    }
}

fn mark_enclosing_truncated(payload: &mut Value, touched_item: bool) {
    if touched_item && let Some(item_obj) = payload.get_mut("item").and_then(Value::as_object_mut) {
        item_obj.insert("truncated".to_owned(), Value::Bool(true));
        return;
    }
    if let Some(root_obj) = payload.as_object_mut() {
        root_obj.insert("truncated".to_owned(), Value::Bool(true));
    }
}

/// Stage B: single floor=0 descending pass shedding `excess` bytes plus exact `"truncated":true` overhead.
fn truncate_strings_once(payload: &mut Value, excess: usize) -> bool {
    let mut candidates = Vec::new();
    let mut path = Vec::new();
    collect_truncatable_strings(payload, &mut path, &mut candidates);
    if candidates.is_empty() {
        return false;
    }
    candidates.sort_by_key(|a| std::cmp::Reverse(a.1));
    let mut remaining = excess.saturating_add(TRUNCATED_KEY_BYTES);
    let mut any_changed = false;
    let mut touched_item = false;
    for (p, len, adds_block_flag) in candidates {
        if remaining == 0 {
            break;
        }
        if adds_block_flag && len <= TRUNCATED_KEY_BYTES {
            continue;
        }
        let extra = if adds_block_flag { TRUNCATED_KEY_BYTES } else { 0 };
        let need = remaining.saturating_add(extra);
        let shed = need.min(len);
        let new_len = len - shed;
        if truncate_string_at(payload, &p, new_len) {
            any_changed = true;
            if matches!(p.first(), Some(PathSeg::Key(k)) if k == "item") {
                touched_item = true;
            }
            remaining = need.saturating_sub(shed);
        }
    }
    if any_changed {
        mark_enclosing_truncated(payload, touched_item);
    }
    any_changed
}

fn item_omitted_fallback(payload: &Value) -> Option<Value> {
    let obj = payload.as_object()?;
    let item_id = obj.get("item_id")?.clone();
    let item_status = obj.get(FIELD_STATUS)?.clone();
    let mut out = Map::new();
    if let Some(turn_id) = obj.get("turn_id") {
        out.insert("turn_id".to_owned(), turn_id.clone());
    }
    out.insert("item_id".to_owned(), item_id);
    out.insert(FIELD_STATUS.to_owned(), item_status);
    out.insert("item_omitted".to_owned(), Value::Bool(true));
    Some(Value::Object(out))
}

/// Encode an event once; if it exceeds `max_bytes`, project images and truncate oversized strings
/// in place so outbound streams stay bounded without dropping recoverable payloads.
///
/// Returns `Ok(None)` only when the event remains irreducibly larger than `max_bytes`.
pub(super) fn encode_bounded_wire_event(
    identity: &EventIdentity,
    event: &str,
    seq: u64,
    ignorable: bool,
    payload: &mut Value,
    max_bytes: usize,
) -> Result<Option<Arc<str>>, String> {
    let turn_id = payload_turn_id(payload);
    let mut wire = encode_wire_event(identity, event, seq, turn_id.as_deref(), ignorable, payload)?;
    if wire.len() <= max_bytes {
        return Ok(Some(wire));
    }

    // Stage A: project inline image blocks into truncated reference blocks.
    let had_item = payload.get("item").is_some();
    if project_images(payload) {
        mark_enclosing_truncated(payload, had_item);
        let turn_id = payload_turn_id(payload);
        wire = encode_wire_event(identity, event, seq, turn_id.as_deref(), ignorable, payload)?;
        if wire.len() <= max_bytes {
            return Ok(Some(wire));
        }
    }

    // Stage B: single floor=0 descending pass across non-protected strings.
    let excess = wire.len().saturating_sub(max_bytes);
    if excess > 0 && truncate_strings_once(payload, excess) {
        let turn_id = payload_turn_id(payload);
        wire = encode_wire_event(identity, event, seq, turn_id.as_deref(), ignorable, payload)?;
        if wire.len() <= max_bytes {
            return Ok(Some(wire));
        }
    }

    // Stage C: for `item/completed`, fall back to `item_omitted: true` skeleton.
    if event == "item/completed"
        && let Some(fallback) = item_omitted_fallback(payload)
    {
        let turn_id = payload_turn_id(&fallback);
        let fallback_wire = encode_wire_event(identity, event, seq, turn_id.as_deref(), ignorable, &fallback)?;
        if fallback_wire.len() <= max_bytes {
            *payload = fallback;
            return Ok(Some(fallback_wire));
        }
    }

    // Stage D: irreducibly oversized payload -> caller emits `Dropped`.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::tap::{EventLog, EventLogLimits, HeadlessRecord, HeadlessSelection};

    #[tokio::test]
    async fn bounds_projects_images_truncates_utf8_and_falls_back_to_item_omitted() {
        let log = EventLog::with_limits(EventLogLimits {
            capacity: 16,
            max_bytes: 32 * 1024,
            max_event_bytes: 1024,
            ..EventLogLimits::default()
        });
        let (_tap, mut rx) =
            log.open_headless_cursor(Arc::new(|_| Some(HeadlessSelection { selected: true, required: true })));

        // 1. Oversized image block with producer-supplied `path` -> truncated image ref preserving
        // `path` and exact decoded bytes for any tool (not hardcoded to a specific tool name).
        let b64 = "A".repeat(2000); // 2000 / 4 * 3 = 1500 decoded bytes
        log.admit_json(
            "item/completed",
            json!({
                "turn_id": "t-1",
                "item_id": "c-1",
                "status": "completed",
                "item": {
                    "type": "tool_call",
                    "tool_call_id": "c-1",
                    "name": "custom_image_tool",
                    "status": "completed",
                    "arguments": { "path": "ignored.png" },
                    "message": {
                        "role": "tool_result",
                        "tool_call_id": "c-1",
                        "content": [{ "type": "image", "mime": "image/png", "path": "shot.png", "data": b64 }]
                    },
                    "result_status": "success",
                    "execution_status": "unknown"
                }
            }),
        )
        .await
        .unwrap();
        let HeadlessRecord::Event(img_ev) = rx.recv().await.unwrap().record else { panic!("expected event") };
        assert!(img_ev.wire_json.len() <= 1024);
        let blk = &img_ev.payload["item"]["message"]["content"][0];
        assert_eq!(blk["type"], "image");
        assert_eq!(blk["mime"], "image/png");
        assert_eq!(blk["path"], "shot.png");
        assert_eq!(blk["bytes"], 1500);
        assert_eq!(blk["truncated"], true);
        assert!(blk.get("data").is_none());
        assert_eq!(img_ev.payload["item"]["truncated"], true);

        // 1b. Image block without `path` does not synthesize `path` from tool_call `arguments.path`
        // (even when the tool happens to be named `read`).
        log.admit_json(
            "item/completed",
            json!({
                "turn_id": "t-1",
                "item_id": "c-1b",
                "status": "completed",
                "item": {
                    "type": "tool_call",
                    "tool_call_id": "c-1b",
                    "name": "read",
                    "status": "completed",
                    "arguments": { "path": "should-not-leak.png" },
                    "message": {
                        "role": "tool_result",
                        "tool_call_id": "c-1b",
                        "content": [{ "type": "image", "mime": "image/png", "data": b64 }]
                    },
                    "result_status": "success",
                    "execution_status": "unknown"
                }
            }),
        )
        .await
        .unwrap();
        let HeadlessRecord::Event(img_ev_no_path) = rx.recv().await.unwrap().record else { panic!("expected event") };
        let blk_no_path = &img_ev_no_path.payload["item"]["message"]["content"][0];
        assert_eq!(blk_no_path["type"], "image");
        assert_eq!(blk_no_path["truncated"], true);
        assert!(blk_no_path.get("path").is_none(), "core must not infer image path from tool arguments");

        // 2. Oversized multibyte UTF-8 text block -> truncated cleanly on UTF-8 boundary with truncated: true.
        let utf8_text = "あ".repeat(600); // 1800 bytes
        log.admit_json(
            "item/completed",
            json!({
                "turn_id": "t-1",
                "item_id": "c-2",
                "status": "completed",
                "item": {
                    "type": "tool_call",
                    "tool_call_id": "c-2",
                    "name": "read",
                    "status": "completed",
                    "message": {
                        "role": "tool_result",
                        "tool_call_id": "c-2",
                        "content": [{ "type": "text", "text": utf8_text }]
                    },
                    "result_status": "success",
                    "execution_status": "unknown"
                }
            }),
        )
        .await
        .unwrap();
        let HeadlessRecord::Event(txt_ev) = rx.recv().await.unwrap().record else { panic!("expected event") };
        assert!(txt_ev.wire_json.len() <= 1024);
        let tblk = &txt_ev.payload["item"]["message"]["content"][0];
        assert_eq!(tblk["truncated"], true);
        assert!(tblk["text"].as_str().unwrap().len() < 1800);
        assert_eq!(txt_ev.payload["item"]["truncated"], true);

        // 3. Irreducibly large `item/completed` (large numeric array) -> falls back to `item_omitted: true`.
        let nums = vec![1u8; 800];
        log.admit_json(
            "item/completed",
            json!({
                "turn_id": "t-1",
                "item_id": "c-3",
                "status": "completed",
                "item": {
                    "type": "custom",
                    "data": nums
                }
            }),
        )
        .await
        .unwrap();
        let HeadlessRecord::Event(omitted_ev) = rx.recv().await.unwrap().record else { panic!("expected event") };
        assert!(omitted_ev.wire_json.len() <= 1024);
        assert_eq!(omitted_ev.payload["item_omitted"], true);
        assert!(omitted_ev.payload.get("item").is_none());
        assert_eq!(omitted_ev.payload["item_id"], "c-3");

        // 4. Many short strings (< 256 bytes each) must still truncate in floor=0 pass without dropping.
        let mut fields = serde_json::Map::new();
        fields.insert("turn_id".to_owned(), json!("t-1"));
        fields.insert("id".to_owned(), json!("p".repeat(120)));
        fields.insert("custom_id".to_owned(), json!("q".repeat(120)));
        fields.insert("batch_ids".to_owned(), json!(["r".repeat(120)]));
        fields.insert("trigger".to_owned(), json!("prompt-trigger"));
        for i in 0..12 {
            fields.insert(format!("note_{i}"), json!("s".repeat(120)));
        }
        log.admit_json("host/notice", Value::Object(fields)).await.unwrap();
        let HeadlessRecord::Event(short_ev) = rx.recv().await.unwrap().record else {
            panic!("many short strings must be truncated, not dropped")
        };
        assert!(short_ev.wire_json.len() <= 1024);
        assert_eq!(short_ev.payload["truncated"], true);
        assert_eq!(short_ev.payload["id"], "p".repeat(120), "id must be protected");
        assert_eq!(short_ev.payload["custom_id"], "q".repeat(120), "*_id must be protected");
        assert_eq!(short_ev.payload["batch_ids"][0], "r".repeat(120), "*_ids must be protected");
        assert_eq!(short_ev.payload["trigger"], "prompt-trigger", "trigger must be protected");

        // 5. Type-less image `{ mime, data }` (e.g. turn/started.images) must project via Stage A, not truncate base64.
        let raw_b64 = "B".repeat(2000);
        log.admit_json(
            "turn/started",
            json!({
                "turn_id": "t-2",
                "trigger": "prompt",
                "images": [{ "mime": "image/png", "data": raw_b64 }]
            }),
        )
        .await
        .unwrap();
        let HeadlessRecord::Event(ts_ev) = rx.recv().await.unwrap().record else { panic!("expected turn/started") };
        assert!(ts_ev.wire_json.len() <= 1024);
        let ts_img = &ts_ev.payload["images"][0];
        assert_eq!(ts_img["type"], "image", "{ts_img}");
        assert_eq!(ts_img["mime"], "image/png", "{ts_img}");
        assert_eq!(ts_img["bytes"], 1500, "{ts_img}");
        assert_eq!(ts_img["truncated"], true, "{ts_img}");
        assert!(ts_img.get("data").is_none(), "type-less image must not keep partial base64: {ts_img}");
        assert_eq!(ts_ev.payload["truncated"], true);
    }
}
