//! restart durability: queued edits with attachments during failed switch prep survive process restart.

use super::*;
use std::process::Stdio;

const TINY_PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC";

#[test]
fn queue_edit_during_failed_switch_prep_survives_process_restart() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("switch_prep_queue_edit.lua"),
        format!(
            r#"
return {{
    name = "switch-prep-queue-edit",
    inject = {{ "agent", "tools", "commands", "session" }},
    apply = function(ctx)
        if mare.env("MARE_SWITCH_PREP_EDIT") ~= "1" then
            return
        end
        ctx.tools:register("hold_for_queue", {{
            side_effect = "read",
            description = "block until cancelled so a follow-up item can be queued while busy",
            parameters = {{ type = "object" }},
            execute = function(_, exec_ctx)
                if exec_ctx and exec_ctx.cancel then
                    exec_ctx.cancel:wait()
                end
                return {{ content = "cancelled" }}
            end,
        }})
        ctx.commands:register("switch-prep-test", {{
            description = "trigger in-host session switch prep for p3 restart test",
            run = function(path)
                ctx.session:switch({{ kind = "load", path = path }})
            end,
        }})
        local armed = true
        local original = mare.store.read_tail_snapshot
        mare.store.read_tail_snapshot = function(...)
            if armed then
                local q = ctx.agent:list_queue().data
                if q and q[1] then
                    armed = false
                    ctx.agent:update_queued(q[1].input_id, {{
                        input = "EDITED-DURING-SWITCH-PREP [Image #1]",
                        images = {{
                            {{
                                type = "image",
                                mime = "image/png",
                                data = "{TINY_PNG_BASE64}",
                                metadata = {{ source = "switch-prep-edit" }},
                            }},
                        }},
                        image_markers = {{ {{ text = "[Image #1]", offset = 26 }} }},
                    }})
                end
            end
            return original(...)
        end
    end,
}}
"#
        ),
    )
    .unwrap();

    let s1 = home.path().join("s1.json");
    std::fs::write(&s1, json!([{ "text": "TARGET-REPLY" }]).to_string()).unwrap();
    let s2 = home.path().join("s2.json");
    std::fs::write(&s2, json!([{ "text": "AFTER-RESTART-REPLY" }, { "text": "CONSUMED-QUEUE-REPLY" }]).to_string())
        .unwrap();

    // Process 1: create target session, switch to a new old session, enqueue a follow-up,
    // then edit that queued item (with text, image attachment, and marker) during switch prep so the switch fails.
    let mut cmd1 = mare_cmd();
    cmd1.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &s1)
        .env("MARE_SWITCH_PREP_EDIT", "1")
        .current_dir(&cwd)
        .stderr(Stdio::null());
    let mut rpc1 = RpcClient::spawn_cmd(&mut cmd1);
    let init1 = rpc1.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let init_sid = init1["result"]["session"]["session_id"].as_str().unwrap().to_string();

    let t_started =
        rpc1.call_raw("agent.prompt", json!({ "session_id": init_sid, "input": "TARGET-PROMPT" }), |_| json!(null));
    assert!(t_started.get("error").is_none(), "{t_started}");
    let target_prompt = rpc1.call_raw(
        "agent.wait",
        json!({ "session_id": init_sid, "turn_id": t_started["result"]["turn_id"] }),
        |_| json!(null),
    );
    assert!(target_prompt.get("error").is_none(), "{target_prompt}");

    let list = rpc1.call_raw("session.list", json!({}), |_| json!(null));
    let target_entry = list["result"]["data"].as_array().and_then(|a| a.first()).expect("target session in list");
    let target_id = target_entry["session_id"]
        .as_str()
        .or_else(|| target_entry["id"].as_str())
        .or_else(|| target_entry["header"]["id"].as_str())
        .unwrap_or_else(|| panic!("missing target id in {target_entry}"))
        .to_string();
    let target_path = target_entry["path"].as_str().unwrap().to_string();
    let target_attachments = format!("{}.attachments", target_path.trim_end_matches(".jsonl"));
    assert!(!std::path::Path::new(&target_attachments).exists(), "target should start without attachments dir");

    std::fs::write(
        &s1,
        json!([{ "tool_calls": [{ "id": "call_hold_p3", "name": "hold_for_queue", "arguments": {} }] }]).to_string(),
    )
    .unwrap();
    let new_res = rpc1.call_raw("session.create", json!({}), |_| json!(null));
    assert!(new_res.get("error").is_none(), "{new_res}");
    assert!(rpc1.call_raw("session.close", json!({ "session_id": target_id }), |_| json!(null))["error"].is_null());
    let old_id = new_res["result"]["session_id"].as_str().unwrap().to_string();
    let old_path = new_res["result"]["path"]
        .as_str()
        .or_else(|| new_res["result"]["planned"].as_str())
        .unwrap_or_else(|| panic!("missing old path in {new_res}"))
        .to_string();
    assert_ne!(old_id, target_id);
    assert_ne!(old_path, target_path);

    let old_started =
        rpc1.call_raw("agent.prompt", json!({ "session_id": old_id, "input": "OLD-PROMPT" }), |_| json!(null));
    assert!(old_started.get("error").is_none(), "{old_started}");
    let old_turn_id = old_started["result"]["turn_id"].as_str().unwrap().to_string();

    let follow_res = rpc1.call_raw(
        "agent.prompt",
        json!({ "session_id": old_id, "input": "QUEUED-INITIAL", "when_busy": "queue" }),
        |_| json!(null),
    );
    assert_eq!(follow_res["result"]["status"], "queued", "{follow_res}");

    let cancel_res = rpc1.call_raw(
        "agent.cancel",
        json!({ "session_id": old_id, "turn_id": old_turn_id, "clear_queue": false }),
        |_| json!(null),
    );
    assert!(cancel_res.get("error").is_none(), "{cancel_res}");
    let old_waited =
        rpc1.call_raw("agent.wait", json!({ "session_id": old_id, "turn_id": old_turn_id }), |_| json!(null));
    assert!(old_waited.get("error").is_none(), "{old_waited}");

    let target_before = std::fs::read(&target_path).unwrap();
    let switch_res =
        rpc1.call_raw("commands.run", json!({ "line": format!("/switch-prep-test {target_path}") }), |_| json!(null));
    let switch_err = switch_res["error"]["message"].as_str().unwrap_or("");
    assert!(
        switch_err.contains("the active session changed") || switch_err.contains("agent context changed"),
        "expected switch prep failure after queue edit, got: {switch_res}"
    );

    let state1 = rpc1.call_raw("session.list", json!({ "loaded": true }), |_| json!(null));
    assert_eq!(state1["result"]["data"][0]["session_id"], old_id, "{state1}");
    assert_eq!(std::fs::read(&target_path).unwrap(), target_before, "target session file must remain unmodified");
    assert!(!std::path::Path::new(&target_attachments).exists(), "target attachments dir must not be created");

    // End Process 1 via RPC EOF (closing stdin) so queued inputs are not discarded by `shutdown`'s `abort`.
    rpc1.wait_exit();

    // Process 2: restart `mare --rpc` against the same MARE_HOME and resume the old session.
    let mut cmd2 = mare_cmd();
    cmd2.args(["--rpc", "--yolo", "--model", "faux/echo", "--resume", &old_path])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &s2)
        .current_dir(&cwd)
        .stderr(Stdio::null());
    let mut rpc2 = RpcClient::spawn_cmd(&mut cmd2);
    rpc2.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));

    let state2 = rpc2.call_raw("agent.get", json!({ "session_id": old_id }), |_| json!(null));
    assert_eq!(state2["result"]["session_id"], old_id, "{state2}");

    let mut cursor = None;
    let mut latest_queue = None;
    loop {
        let mut args = json!({ "session_id": old_id, "kind": "agent/queue" });
        if let Some(token) = cursor.take() {
            args["cursor"] = json!(token);
        }
        let entries = rpc2.call_raw("session.list_records", args, |_| json!(null));
        if let Some(last) = entries["result"]["data"].as_array().unwrap().last() {
            latest_queue = Some(last.clone());
        }
        cursor = entries["result"].get("next_cursor").and_then(Value::as_str).map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    let latest_queue = latest_queue.expect("persisted agent/queue entry after restart");
    let queued_item = &latest_queue["follow_up"][0];
    assert_eq!(queued_item["text"], "EDITED-DURING-SWITCH-PREP [Image #1]", "{latest_queue}");
    assert_eq!(
        queued_item["images"],
        json!([{
            "type": "image",
            "mime": "image/png",
            "data": TINY_PNG_BASE64,
            "metadata": { "source": "switch-prep-edit" },
        }]),
        "{latest_queue}"
    );
    assert_eq!(queued_item["image_markers"], json!([{ "text": "[Image #1]", "offset": 26 }]), "{latest_queue}");

    let after_started =
        rpc2.call_raw("agent.prompt", json!({ "session_id": old_id, "input": "AFTER-RESTART" }), |_| json!(null));
    assert!(after_started.get("error").is_none(), "{after_started}");
    let after_restart = rpc2.call_raw(
        "agent.wait",
        json!({ "session_id": old_id, "turn_id": after_started["result"]["turn_id"] }),
        |_| json!(null),
    );
    assert!(after_restart.get("error").is_none(), "{after_restart}");

    let messages = rpc2.call_raw("agent.list_messages", json!({ "session_id": old_id }), |_| json!(null));
    let user_messages: Vec<&Value> =
        messages["result"]["data"].as_array().unwrap().iter().filter(|m| m["role"] == "user").collect();
    let user_texts: Vec<&str> = user_messages.iter().filter_map(|m| m["content"][0]["text"].as_str()).collect();
    assert!(user_texts.contains(&"OLD-PROMPT"), "{user_texts:?}");
    assert!(user_texts.contains(&"AFTER-RESTART"), "{user_texts:?}");
    assert!(user_texts.contains(&"EDITED-DURING-SWITCH-PREP [Image #1]"), "{user_texts:?}");
    assert!(!user_texts.contains(&"QUEUED-INITIAL"), "{user_texts:?}");
    assert!(!user_texts.contains(&"TARGET-PROMPT"), "{user_texts:?}");

    let edited_msg = user_messages
        .iter()
        .find(|m| m["content"][0]["text"] == "EDITED-DURING-SWITCH-PREP [Image #1]")
        .expect("delivered edited queued message");
    assert_eq!(
        edited_msg["content"],
        json!([
            { "type": "text", "text": "EDITED-DURING-SWITCH-PREP [Image #1]" },
            {
                "type": "image",
                "mime": "image/png",
                "data": TINY_PNG_BASE64,
                "metadata": { "source": "switch-prep-edit" },
            }
        ]),
        "{edited_msg}"
    );

    assert_eq!(std::fs::read(&target_path).unwrap(), target_before, "target session file must remain unmodified");
    assert!(!std::path::Path::new(&target_attachments).exists(), "target attachments dir must remain absent");
    let resume_target = rpc2.call_raw("session.load", json!({ "path": target_path }), |_| json!(null));
    assert!(resume_target.get("error").is_none(), "{resume_target}");
    let target_messages = rpc2.call_raw("agent.list_messages", json!({ "session_id": target_id }), |_| json!(null));
    let target_user_messages: Vec<&Value> = target_messages["result"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "user" && m.get("context_disclosure").is_none())
        .collect();
    assert_eq!(target_user_messages.len(), 1, "{target_user_messages:?}");
    assert_eq!(
        target_user_messages[0]["content"],
        json!([{ "type": "text", "text": "TARGET-PROMPT" }]),
        "{target_user_messages:?}"
    );

    rpc2.shutdown_and_wait();
}
