//! ACP rich session updates (#96): tool-call locations and diffs, stable message ids between live
//! streaming and `session/load` replay, non-ACP provider/config change notifications, and
//! capability-gated `compaction_update` notifications.

use super::*;
use agent_client_protocol::v1::schema::SessionNotification;

fn assert_valid_session_update(sid: &str, update: &Value) {
    let parsed: Result<SessionNotification, _> = serde_json::from_value(json!({ "sessionId": sid, "update": update }));
    assert!(parsed.is_ok(), "invalid ACP SessionUpdate {update}: {parsed:?}");
}

#[test]
fn acp_file_tools_emit_locations_and_diffs_live_and_on_replay_with_stable_message_ids() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();
    let existing = cwd.join("existing.txt");
    std::fs::write(&existing, "line1\nline2\nline3\n").unwrap();

    // Turn 1: reasoning + read (with offset=2) + write (new file) + edit (existing file) -> text reply.
    let script = json!([
        {
            "reasoning": "inspect and update files",
            "tool_calls": [
                { "name": "read", "arguments": { "path": "existing.txt", "offset": 2, "limit": 2 } },
                { "name": "write", "arguments": { "path": "created.txt", "content": "fresh line 1\nfresh line 2\n" } }
            ]
        },
        {
            "tool_calls": [
                { "name": "edit", "arguments": { "path": "existing.txt", "old_string": "line2", "new_string": "line2-edited" } }
            ]
        },
        { "text": "all file edits applied" }
    ]);

    let mut rpc = acp_in_home(home.path(), script, &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let p1 = rpc
        .request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "edit files" }] }));
    let (r1, live_updates) = read_until(&mut rpc, p1, Some(&sid));
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");
    for u in &live_updates {
        assert_valid_session_update(&sid, u);
    }

    let check_file_tool_updates = |label: &str, updates: &[Value]| {
        // 1. `read` tool_call carries absolute location + 1-based line from offset=2.
        let read_call = updates
            .iter()
            .find(|u| {
                u["sessionUpdate"] == "tool_call" && u["rawInput"]["path"] == "existing.txt" && u["kind"] == "read"
            })
            .unwrap_or_else(|| panic!("{label}: missing read tool_call: {updates:?}"));
        assert_eq!(
            read_call["locations"],
            json!([{ "path": existing.to_string_lossy(), "line": 2 }]),
            "{label}: read tool_call must include absolute path + line: {read_call}"
        );

        // 2. `write` (new file) tool_call carries location; tool_call_update carries location + diff with newText and no oldText.
        let created_path = cwd.join("created.txt");
        let write_call = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call" && u["rawInput"]["path"] == "created.txt")
            .unwrap_or_else(|| panic!("{label}: missing write tool_call: {updates:?}"));
        let write_id = write_call["toolCallId"].as_str().unwrap();
        assert_eq!(
            write_call["locations"],
            json!([{ "path": created_path.to_string_lossy() }]),
            "{label}: write tool_call must include location: {write_call}"
        );
        let write_done = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == write_id)
            .unwrap_or_else(|| panic!("{label}: missing write tool_call_update: {updates:?}"));
        assert_eq!(write_done["status"], "completed", "{label}: {write_done}");
        assert_eq!(
            write_done["locations"],
            json!([{ "path": created_path.to_string_lossy() }]),
            "{label}: write tool_call_update must include location: {write_done}"
        );
        let write_diff = write_done["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "diff")
            .unwrap_or_else(|| panic!("{label}: write tool_call_update must include diff block: {write_done}"));
        assert_eq!(write_diff["path"], created_path.to_string_lossy().as_ref(), "{label}: {write_diff}");
        assert!(write_diff.get("oldText").is_none(), "{label}: new file write diff must omit oldText: {write_diff}");
        assert_eq!(write_diff["newText"], "fresh line 1\nfresh line 2\n", "{label}: {write_diff}");

        // 3. `edit` tool_call carries location; tool_call_update carries location + diff with oldText & newText.
        let edit_call = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call" && u["rawInput"]["old_string"] == "line2")
            .unwrap_or_else(|| panic!("{label}: missing edit tool_call: {updates:?}"));
        let edit_id = edit_call["toolCallId"].as_str().unwrap();
        assert_eq!(edit_call["kind"], "edit", "{label}: {edit_call}");
        assert_eq!(
            edit_call["locations"],
            json!([{ "path": existing.to_string_lossy() }]),
            "{label}: edit tool_call must include location: {edit_call}"
        );
        let edit_done = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == edit_id)
            .unwrap_or_else(|| panic!("{label}: missing edit tool_call_update: {updates:?}"));
        assert_eq!(edit_done["status"], "completed", "{label}: {edit_done}");
        assert_eq!(
            edit_done["locations"],
            json!([{ "path": existing.to_string_lossy() }]),
            "{label}: edit tool_call_update must include location: {edit_done}"
        );
        let edit_diff = edit_done["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "diff")
            .unwrap_or_else(|| panic!("{label}: edit tool_call_update must include diff block: {edit_done}"));
        assert_eq!(edit_diff["path"], existing.to_string_lossy().as_ref(), "{label}: {edit_diff}");
        let old_text = edit_diff["oldText"].as_str().unwrap_or("");
        let new_text = edit_diff["newText"].as_str().unwrap_or("");
        assert!(old_text.contains("line2") && !old_text.contains("line2-edited"), "{label}: {edit_diff}");
        assert!(new_text.contains("line2-edited"), "{label}: {edit_diff}");
    };

    check_file_tool_updates("live", &live_updates);

    // Capture live messageIds for reasoning & final assistant text.
    let live_thought_ids: Vec<String> = live_updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_thought_chunk")
        .map(|u| {
            u["messageId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| panic!("live agent_thought_chunk missing messageId: {u}"))
                .to_string()
        })
        .collect();
    assert!(!live_thought_ids.is_empty(), "expected live agent_thought_chunk: {live_updates:?}");
    let live_thought_id = live_thought_ids[0].clone();
    assert!(
        live_thought_ids.iter().all(|id| id == &live_thought_id),
        "all chunks of one assistant thought must share one messageId: {live_thought_ids:?}"
    );

    let live_reply_ids: Vec<String> = live_updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .map(|u| {
            u["messageId"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| panic!("live agent_message_chunk missing messageId: {u}"))
                .to_string()
        })
        .collect();
    assert!(!live_reply_ids.is_empty(), "expected live agent_message_chunk: {live_updates:?}");
    let live_reply_id = live_reply_ids[0].clone();
    assert!(
        live_reply_ids.iter().all(|id| id == &live_reply_id),
        "all chunks of one assistant reply must share one messageId: {live_reply_ids:?}"
    );
    assert_ne!(live_thought_id, live_reply_id, "distinct assistant messages must have distinct messageIds");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");
    rpc.kill_and_wait();

    // Restart and replay via session/load: verify identical diffs, locations, and messageIds.
    let mut rpc2 = acp_in_home(home.path(), json!([]), &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let load_id = rpc2.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
    let (loaded, replay_updates) = read_until(&mut rpc2, load_id, Some(&sid));
    assert!(loaded.get("error").is_none(), "{loaded}");
    for u in &replay_updates {
        assert_valid_session_update(&sid, u);
    }

    check_file_tool_updates("replay", &replay_updates);

    let replay_user = replay_updates
        .iter()
        .find(|u| u["sessionUpdate"] == "user_message_chunk")
        .unwrap_or_else(|| panic!("missing replayed user_message_chunk: {replay_updates:?}"));
    assert!(
        replay_user["messageId"].as_str().is_some_and(|s| !s.is_empty()),
        "replayed user_message_chunk must carry messageId: {replay_user}"
    );
    let replay_thought = replay_updates
        .iter()
        .find(|u| u["sessionUpdate"] == "agent_thought_chunk")
        .unwrap_or_else(|| panic!("missing replayed agent_thought_chunk: {replay_updates:?}"));
    assert_eq!(
        replay_thought["messageId"], live_thought_id,
        "replayed agent_thought_chunk must reuse live messageId: live={live_thought_id} replay={replay_thought}"
    );
    let replay_reply = replay_updates
        .iter()
        .find(|u| u["sessionUpdate"] == "agent_message_chunk")
        .unwrap_or_else(|| panic!("missing replayed agent_message_chunk: {replay_updates:?}"));
    assert_eq!(
        replay_reply["messageId"], live_reply_id,
        "replayed agent_message_chunk must reuse live messageId: live={live_reply_id} replay={replay_reply}"
    );

    rpc2.kill_and_wait();
}

#[test]
fn acp_apply_patch_emits_locations_and_diffs_live_and_on_replay() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();
    let target = cwd.join("patchme.txt");
    let added = cwd.join("added.txt");
    std::fs::write(&target, "alpha\nbeta\ngamma\n").unwrap();

    let patch = concat!(
        "*** Begin Patch\n",
        "*** Update File: patchme.txt\n",
        "@@\n",
        " alpha\n",
        "-beta\n",
        "+BETA\n",
        " gamma\n",
        "*** Add File: added.txt\n",
        "+hello patch\n",
        "*** End Patch\n",
    );

    let script = json!([
        { "tool_calls": [{ "name": "read", "arguments": { "path": "patchme.txt" } }] },
        { "tool_calls": [{ "name": "apply_patch", "arguments": { "input": patch } }] },
        { "text": "patched" }
    ]);

    let config_dir = home.path().join("config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("init.lua"), "mare.plugin(\"tools\", { edit_mode = \"apply_patch\" })\n").unwrap();

    let mut rpc = acp_in_home(home.path(), script, &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let p1 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "patch" }] }));
    let (r1, live_updates) = read_until(&mut rpc, p1, Some(&sid));
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");

    let check_patch = |label: &str, updates: &[Value]| {
        let patch_call = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call" && u["rawInput"].get("input").is_some())
            .unwrap_or_else(|| panic!("{label}: missing apply_patch tool_call: {updates:?}"));
        let patch_id = patch_call["toolCallId"].as_str().unwrap();
        assert_eq!(
            patch_call["locations"],
            json!([
                { "path": target.to_string_lossy() },
                { "path": added.to_string_lossy() }
            ]),
            "{label}: apply_patch tool_call must include all touched paths: {patch_call}"
        );

        let patch_done = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update" && u["toolCallId"] == patch_id)
            .unwrap_or_else(|| panic!("{label}: missing apply_patch tool_call_update: {updates:?}"));
        assert_eq!(patch_done["status"], "completed", "{label}: {patch_done}");
        assert_eq!(
            patch_done["locations"],
            json!([
                { "path": target.to_string_lossy() },
                { "path": added.to_string_lossy() }
            ]),
            "{label}: apply_patch tool_call_update must include locations: {patch_done}"
        );
        let diffs: Vec<&Value> =
            patch_done["content"].as_array().unwrap().iter().filter(|b| b["type"] == "diff").collect();
        assert_eq!(diffs.len(), 2, "{label}: expected 2 diff blocks for update+add: {patch_done}");
        assert_eq!(diffs[0]["path"], target.to_string_lossy().as_ref(), "{label}: {diffs:?}");
        assert!(diffs[0]["oldText"].as_str().unwrap_or("").contains("beta"), "{label}: {diffs:?}");
        assert!(diffs[0]["newText"].as_str().unwrap_or("").contains("BETA"), "{label}: {diffs:?}");
        assert_eq!(diffs[1]["path"], added.to_string_lossy().as_ref(), "{label}: {diffs:?}");
        assert!(diffs[1].get("oldText").is_none(), "{label}: added file diff must omit oldText: {diffs:?}");
        assert!(diffs[1]["newText"].as_str().unwrap_or("").contains("hello patch"), "{label}: {diffs:?}");
    };

    check_patch("live", &live_updates);

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");
    rpc.kill_and_wait();

    let mut rpc2 = acp_in_home(home.path(), json!([]), &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let load_id = rpc2.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
    let (loaded, replay_updates) = read_until(&mut rpc2, load_id, Some(&sid));
    assert!(loaded.get("error").is_none(), "{loaded}");
    for u in &replay_updates {
        assert_valid_session_update(&sid, u);
    }
    check_patch("replay", &replay_updates);

    rpc2.kill_and_wait();
}

#[test]
fn acp_compaction_updates_are_gated_on_client_capability_live_and_on_replay() {
    let run_case = |advertise_cap: bool| {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join("config");
        let plugins_dir = config_dir.join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            config_dir.join("init.lua"),
            "mare.plugin(\"compact\", { keep_recent_tokens = 1, recent_user_tokens = 0 })\n",
        )
        .unwrap();
        std::fs::write(
            plugins_dir.join("trigger-compact.lua"),
            r#"return {
    name = "trigger-compact",
    inject = { "compact" },
    apply = function(ctx)
        local turns = 0
        ctx:on("agent/settled", function(ev)
            if ev and ev.busy then return end
            turns = turns + 1
            if turns ~= 2 then return end
            local res, err = ctx.compact:run(nil, { reason = "manual" })
            assert(res, "manual compaction failed: " .. tostring(err))
        end)
    end,
}"#,
        )
        .unwrap();

        // Turn 1 and Turn 2 build multi-turn history; on Turn 2's `agent/settled` the plugin runs
        // manual compaction (which calls the faux model for a summary) while the ACP forwarder is
        // live; Turn 3 flushes post-turn compaction updates and verifies follow-up turns work.
        let long_turn = "turn one historical detail ".repeat(80);
        let script = json!([
            { "text": long_turn },
            { "text": "second turn response kept as the recent tail" },
            { "text": "Compacted summary." },
            { "text": "post-compaction reply" }
        ]);
        let mut rpc = acp_in_home(home.path(), script, &["--permission", "auto", "--approval-policy", "never"], &[]);
        let client_caps = if advertise_cap { json!({ "session": { "compaction": {} } }) } else { json!({}) };
        let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": client_caps }), |_| {
            json!(null)
        });
        let cwd = dunce::canonicalize(home.path()).unwrap();
        let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
        let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

        let p1 = rpc.request(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hello turn one with enough content to compact" }] }),
        );
        let (r1, u1) = read_until(&mut rpc, p1, Some(&sid));
        assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");

        let p2 = rpc.request(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hello turn two kept recent" }] }),
        );
        let (r2, u2) = read_until(&mut rpc, p2, Some(&sid));
        assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");

        let p3 = rpc.request(
            "session/prompt",
            json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "after compact" }] }),
        );
        let (r3, u3) = read_until(&mut rpc, p3, Some(&sid));
        assert_eq!(r3["result"]["stopReason"], "end_turn", "{r3}");

        let mut live_updates = u1;
        live_updates.extend(u2);
        live_updates.extend(u3);
        for u in &live_updates {
            assert_valid_session_update(&sid, u);
        }

        let live_compactions: Vec<&Value> =
            live_updates.iter().filter(|u| u["sessionUpdate"] == "compaction_update").collect();
        if advertise_cap {
            assert_eq!(
                live_compactions.len(),
                2,
                "client advertising session.compaction must get in_progress + completed: {live_updates:?}"
            );
            assert_eq!(live_compactions[0]["status"], "in_progress", "{live_compactions:?}");
            assert_eq!(live_compactions[1]["status"], "completed", "{live_compactions:?}");
            let cid = live_compactions[0]["compactionId"].as_str().unwrap_or("");
            assert!(!cid.is_empty(), "{live_compactions:?}");
            assert_eq!(live_compactions[1]["compactionId"], cid, "{live_compactions:?}");
        } else {
            assert!(
                live_compactions.is_empty(),
                "client without session.compaction must NOT receive compaction_update: {live_compactions:?}"
            );
        }

        let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
        assert!(closed.get("result").is_some(), "{closed}");
        rpc.kill_and_wait();

        // Reload the compacted session with the same capability setting.
        let mut rpc2 =
            acp_in_home(home.path(), json!([]), &["--permission", "auto", "--approval-policy", "never"], &[]);
        let _ = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": client_caps }), |_| {
            json!(null)
        });
        let load_id = rpc2.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
        let (loaded, replay_updates) = read_until(&mut rpc2, load_id, Some(&sid));
        assert!(loaded.get("error").is_none(), "{loaded}");
        for u in &replay_updates {
            assert_valid_session_update(&sid, u);
        }

        let replay_compactions: Vec<&Value> =
            replay_updates.iter().filter(|u| u["sessionUpdate"] == "compaction_update").collect();
        if advertise_cap {
            assert_eq!(
                replay_compactions.len(),
                1,
                "replay for capable client must include completed compaction_update: {replay_updates:?}"
            );
            assert_eq!(replay_compactions[0]["status"], "completed", "{replay_compactions:?}");
            assert!(
                replay_compactions[0]["compactionId"].as_str().is_some_and(|s| !s.is_empty()),
                "{replay_compactions:?}"
            );
        } else {
            assert!(
                replay_compactions.is_empty(),
                "replay for non-capable client must NOT include compaction_update: {replay_compactions:?}"
            );
        }

        rpc2.kill_and_wait();
    };

    run_case(true);
    run_case(false);
}

#[test]
fn acp_pushes_config_option_update_when_non_acp_plugin_registers_provider() {
    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("dynamic-provider.lua"),
        r#"return {
    name = "dynamic-provider",
    inject = { "llm", "tools" },
    apply = function(ctx)
        ctx.tools:register("add_provider", {
            description = "register a new LLM provider outside ACP",
            side_effect = "external",
            parameters = { type = "object", additionalProperties = false },
            execute = function()
                ctx.llm:register_provider({
                    provider = "dynprov",
                    no_key = true,
                    models = {
                        { model_id = "fresh-model", name = "Fresh Model", reasoning = false },
                    },
                    stream = function()
                        return { role = "assistant", content = { { type = "text", text = "hi" } }, stop_reason = "stop" }
                    end,
                })
                return { content = "registered" }
            end,
        })
    end,
}"#,
    )
    .unwrap();

    let mut rpc = acp_in_home(
        home.path(),
        json!([
            { "tool_calls": [{ "name": "add_provider", "arguments": {} }] },
            { "text": "provider added" }
        ]),
        &["--permission", "auto", "--approval-policy", "never"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let p1 = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "add" }] }));
    let (r1, updates) = read_until(&mut rpc, p1, Some(&sid));
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");

    let cfg_update = updates
        .iter()
        .rev()
        .find(|u| u["sessionUpdate"] == "config_option_update")
        .unwrap_or_else(|| panic!("expected config_option_update after llm/providers-changed: {updates:?}"));
    assert_valid_session_update(&sid, cfg_update);
    let model_opt = cfg_update["configOptions"].as_array().unwrap().iter().find(|o| o["id"] == "model").unwrap();
    let serialized = model_opt.to_string();
    assert!(
        serialized.contains("dynprov/fresh-model"),
        "config_option_update must include newly registered provider model: {cfg_update}"
    );

    rpc.kill_and_wait();
}
