//! ACP `session/load` history replay and pagination regressions.
//! References: agent-client-protocol-5.3.0/tests/session_restore.rs:109-174 (pre-response replay),
//! reference implementation/packages/acp-server/src/{server.ts:210-220,replay.ts:1-168,test/lifecycle.test.ts:140-375},
//! and reference implementation/packages/acp/acp/README.md:82 (`additionalDirectories`).
use super::acp::{acp_in_home, read_until};
use super::*;

#[test]
fn acp_load_session_advertises_capability_replays_history_and_continues() {
    const REPLAY_SECRET: &str = "ACP_REPLAY_SECRET_77";
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();
    std::fs::write(cwd.join("note.txt"), "hello from disk\n").unwrap();

    // Turn 1: thinking + tool_call (read) + tool_result + assistant reply.
    // Turn 2: interleaved text + PNG image + text -> assistant reply.
    let mut rpc = acp_in_home(
        home.path(),
        json!([
            {
                "reasoning": format!("checking note {REPLAY_SECRET}"),
                "tool_calls": [{ "name": "read", "arguments": { "path": "note.txt" } }]
            },
            { "text": "readme says hello" },
            { "text": "saw image and text" }
        ]),
        &[],
        &[],
    );
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["agentCapabilities"]["loadSession"], true, "{init}");

    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();
    let set_effort = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "reasoning_effort", "value": "high" }),
        |_| json!(null),
    );
    assert!(set_effort["result"]["configOptions"].is_array(), "{set_effort}");

    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "read note" }] }),
        |_| json!(null),
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");

    let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==";
    let p2 = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "before" },
                { "type": "image", "data": png_b64, "mimeType": "image/png" },
                { "type": "text", "text": "after" }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(p2["result"]["stopReason"], "end_turn", "{p2}");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");
    rpc.kill_and_wait();

    // Restart a fresh ACP process against the same MARE_HOME and load the persisted session.
    // The value was stored before registration; replay must project it at the ACP send boundary.
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("replay-secret.lua"),
        format!(r#"return {{ name = "replay-secret", apply = function() KEEP_REPLAY_SECRET = mare.projection.register("{REPLAY_SECRET}") end }}"#),
    )
    .unwrap();
    let mut rpc2 = acp_in_home(home.path(), json!([{ "text": "continued after load" }]), &[], &[]);
    let init2 = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init2["result"]["agentCapabilities"]["loadSession"], true, "{init2}");

    let load_id = rpc2.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
    let (loaded, updates) = read_until(&mut rpc2, load_id, Some(&sid));
    assert!(loaded.get("error").is_none(), "{loaded}");
    let opts = loaded["result"]["configOptions"].as_array().unwrap_or_else(|| panic!("{loaded}"));
    let effort = opts.iter().find(|o| o["id"] == "reasoning_effort").unwrap_or_else(|| panic!("{loaded}"));
    assert_eq!(effort["currentValue"], "high", "{loaded}");

    // Every history item must arrive as session/update strictly before the LoadSessionResponse:
    // user 1, thought, tool_call, tool_call_update, assistant 1, user 2 (text, image, text), assistant 2.
    assert!(updates.len() >= 9, "expected full history replay before response, got {updates:?}");
    let kinds: Vec<&str> = updates.iter().filter_map(|u| u["sessionUpdate"].as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "user_message_chunk",
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "agent_message_chunk",
            "user_message_chunk",
            "user_message_chunk",
            "user_message_chunk",
            "agent_message_chunk",
        ],
        "{updates:?}"
    );
    assert_eq!(updates[0]["content"], json!({ "type": "text", "text": "read note" }));
    let replayed_thought = updates[1]["content"]["text"].as_str().unwrap();
    assert!(replayed_thought.starts_with("checking note [[mare-value:"), "{replayed_thought}");
    assert!(!replayed_thought.contains(REPLAY_SECRET), "replay leaked registered text: {replayed_thought}");
    assert_eq!(updates[2]["kind"], "read");
    assert_eq!(updates[2]["status"], "in_progress");
    assert_eq!(updates[2]["rawInput"], json!({ "path": "note.txt" }));
    assert_eq!(updates[3]["toolCallId"], updates[2]["toolCallId"]);
    assert_eq!(updates[3]["status"], "completed");
    assert!(
        updates[3]["content"][0]["content"]["text"].as_str().unwrap_or("").contains("hello from disk"),
        "{:?}",
        updates[3]
    );
    assert_eq!(updates[4]["content"], json!({ "type": "text", "text": "readme says hello" }));
    assert_eq!(updates[5]["content"], json!({ "type": "text", "text": "before" }));
    assert_eq!(updates[6]["content"], json!({ "type": "image", "data": png_b64, "mimeType": "image/png" }));
    assert_eq!(updates[7]["content"], json!({ "type": "text", "text": "after" }));
    assert_eq!(updates[8]["content"], json!({ "type": "text", "text": "saw image and text" }));

    let p3_id = rpc2
        .request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "follow up" }] }));
    let (p3, p3_updates) = read_until(&mut rpc2, p3_id, Some(&sid));
    assert_eq!(p3["result"]["stopReason"], "end_turn", "{p3}");
    let reply_text: String = p3_updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert_eq!(reply_text, "continued after load");

    rpc2.kill_and_wait();
}

#[test]
fn acp_load_session_rejects_invalid_targets_without_replaying_updates() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let other_work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();
    let other_cwd = dunce::canonicalize(other_work.path()).unwrap();

    let mut rpc = acp_in_home(home.path(), json!([{ "text": "turn 1" }]), &[], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();
    let p1_id =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hi" }] }));
    let (p1, p1_updates) = read_until(&mut rpc, p1_id, Some(&sid));
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");
    assert!(!p1_updates.is_empty());

    // Loading an active session fails without replaying updates.
    let id_active = rpc.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
    let (active_err, active_updates) = read_until(&mut rpc, id_active, Some(&sid));
    assert_eq!(active_err["error"]["code"], -32602, "{active_err}");
    assert!(active_updates.is_empty(), "{active_updates:?}");

    // Loading an unknown session fails without replaying updates.
    let id_missing =
        rpc.request("session/load", json!({ "sessionId": "does-not-exist", "cwd": cwd, "mcpServers": [] }));
    let (missing_err, missing_updates) = read_until(&mut rpc, id_missing, Some("does-not-exist"));
    assert_eq!(missing_err["error"]["code"], -32002, "an unknown session is not found: {missing_err}");
    assert!(missing_updates.is_empty(), "{missing_updates:?}");

    // Close and try loading with mismatched cwd -> fails without updates.
    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");
    let id_cwd = rpc.request("session/load", json!({ "sessionId": sid, "cwd": other_cwd, "mcpServers": [] }));
    let (cwd_err, cwd_updates) = read_until(&mut rpc, id_cwd, Some(&sid));
    assert_eq!(cwd_err["error"]["code"], -32602, "{cwd_err}");
    assert!(cwd_updates.is_empty(), "{cwd_updates:?}");

    // `session/resume` and `session/load` reject unrecorded `additionalDirectories` without replaying updates.
    let bad_resume_dirs = rpc.call_raw(
        "session/resume",
        json!({ "sessionId": sid, "cwd": cwd, "additionalDirectories": [other_cwd], "mcpServers": [] }),
        |_| json!(null),
    );
    assert_eq!(bad_resume_dirs["error"]["code"], -32602, "{bad_resume_dirs}");
    let id_dirs = rpc.request(
        "session/load",
        json!({ "sessionId": sid, "cwd": cwd, "additionalDirectories": [other_cwd], "mcpServers": [] }),
    );
    let (dirs_err, dirs_updates) = read_until(&mut rpc, id_dirs, Some(&sid));
    assert_eq!(dirs_err["error"]["code"], -32602, "{dirs_err}");
    assert!(dirs_updates.is_empty(), "{dirs_updates:?}");

    // A persisted session with zero messages replays zero history updates on session/load.
    let sid_empty = "empty-persisted-acp-session";
    let sess_dir = home.path().join("state").join("sessions").join("empty");
    std::fs::create_dir_all(&sess_dir).unwrap();
    let header =
        json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": sid_empty, "cwd": cwd });
    std::fs::write(sess_dir.join(format!("{sid_empty}.jsonl")), format!("{header}\n")).unwrap();
    let id_empty = rpc.request("session/load", json!({ "sessionId": sid_empty, "cwd": cwd, "mcpServers": [] }));
    let (empty_resp, empty_updates) = read_until(&mut rpc, id_empty, Some(sid_empty));
    assert!(empty_resp.get("error").is_none(), "{empty_resp}");
    assert!(empty_resp["result"]["configOptions"].is_array(), "{empty_resp}");
    assert!(empty_updates.is_empty(), "{empty_updates:?}");

    rpc.kill_and_wait();
}

#[test]
fn acp_session_list_paginates_with_cursor() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();

    let sess_dir = home.path().join("state").join("sessions").join("paged");
    std::fs::create_dir_all(&sess_dir).unwrap();
    for idx in 0..105usize {
        let id = format!("sess-{idx:03}");
        let header = json!({
            "type": "session",
            "version": mare_store::CURRENT_VERSION,
            "session_id": id,
            "cwd": cwd,
        });
        std::fs::write(sess_dir.join(format!("{id}.jsonl")), format!("{header}\n")).unwrap();
    }

    let mut rpc = acp_in_home(home.path(), json!([]), &[], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    let page1 = rpc.call_raw("session/list", json!({ "cwd": cwd }), |_| json!(null));
    let items1 = page1["result"]["sessions"].as_array().unwrap_or_else(|| panic!("{page1}"));
    assert_eq!(items1.len(), 100, "{page1}");
    let cursor = page1["result"]["nextCursor"].as_str().unwrap_or_else(|| panic!("{page1}"));
    assert!(!cursor.is_empty(), "{page1}");

    // Open an item from page 1 before requesting page 2: the former item 101 must still appear
    // on page 2, and no item may repeat or be skipped.
    let opened_id = items1[0]["sessionId"].as_str().unwrap().to_string();
    let resumed =
        rpc.call_raw("session/resume", json!({ "sessionId": opened_id, "cwd": cwd, "mcpServers": [] }), |_| {
            json!(null)
        });
    assert!(resumed.get("error").is_none(), "{resumed}");

    let page2 = rpc.call_raw("session/list", json!({ "cwd": cwd, "cursor": cursor }), |_| json!(null));
    let items2 = page2["result"]["sessions"].as_array().unwrap_or_else(|| panic!("{page2}"));
    assert_eq!(items2.len(), 5, "{page2}");
    assert!(page2["result"].get("nextCursor").is_none_or(Value::is_null), "{page2}");

    let seen1: std::collections::HashSet<&str> = items1.iter().filter_map(|e| e["sessionId"].as_str()).collect();
    let seen2: std::collections::HashSet<&str> = items2.iter().filter_map(|e| e["sessionId"].as_str()).collect();
    assert_eq!(seen1.len(), 100);
    assert_eq!(seen2.len(), 5);
    assert!(seen1.is_disjoint(&seen2), "pages must not overlap");
    let mut total: std::collections::HashSet<&str> = seen1.union(&seen2).copied().collect();
    assert_eq!(total.len(), 105, "every session must appear across pages without skipping item 101");
    assert!(total.remove(opened_id.as_str()));
    assert_eq!(total.len(), 104);

    rpc.kill_and_wait();
}
