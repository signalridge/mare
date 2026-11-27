use super::*;

#[test]
fn rpc_sigkill_at_the_production_inbox_commit_keeps_the_complete_input() {
    let home = tempfile::tempdir().unwrap();
    let spawn = |resume: Option<&str>| {
        let mut command = mare_cmd();
        command.args(["--rpc", "--model", "faux/echo"]);
        if let Some(path) = resume {
            command.args(["--resume", path]);
        }
        command.env("MARE_HOME", home.path()).current_dir(home.path()).stderr(Stdio::null());
        RpcClient::spawn_cmd(&mut command)
    };
    let allow = |_: &Value| json!({"value":{"decision":"allow"}});
    let mut rpc = spawn(None);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    rpc.call_raw("hooks.subscribe", json!({"scope":"all","event":"request/ready","callbacks":["handle"]}), |_| {
        json!(null)
    });
    rpc.call_raw(
        "hooks.subscribe",
        json!({"scope":"all","event":"session/commit-ready","callbacks":["handle"]}),
        |_| json!(null),
    );
    rpc.call_holding("agent.prompt", json!({"session_id":sid,"input":"hold before model"}));
    let held = loop {
        let frame = rpc.read();
        if frame["method"] == "callback.call" {
            break frame;
        }
    };
    let accepted = rpc.call_raw(
        "agent.prompt",
        json!({"session_id":sid,"input":[{"type":"text","text":"crash boundary image"},{"type":"image","mime":"image/png","data":"AQIDBA=="}]}),
        allow,
    );
    assert_eq!(accepted["result"]["status"], "queued", "{accepted}");
    let sessions = rpc.call_raw("session.list", json!({}), allow);
    let path = sessions["result"]["data"].as_array().unwrap()[0]["path"].as_str().unwrap().to_owned();
    rpc.send(json!({"jsonrpc":"2.0","id":held["id"],"result":allow(&held)}));
    let boundary = loop {
        let frame = rpc.read();
        if frame["method"] == "callback.call" {
            let args = &frame["params"]["arguments"];
            if args["event"] == "session/commit-ready"
                && args["args"][0]["messages"].as_array().is_some_and(|messages| !messages.is_empty())
            {
                break frame;
            }
            rpc.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":allow(&frame)}));
        }
    };
    assert_eq!(
        boundary["params"]["arguments"]["args"][0]["messages"][0]["content"][1]["data"], "AQIDBA==",
        "actual consumption must own the complete image"
    );
    rpc.kill_and_wait(); // Unix SIGKILL at the actual production consume/commit boundary
    for expected in [1, 0] {
        let mut rpc = spawn(Some(&path));
        let init_r = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
        let sid_r = init_r["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
        let state = rpc.call_raw("agent.get", json!({"session_id":sid_r}), |_| json!(null));
        assert_eq!(state["result"]["queued"], expected, "{state}");
        let sid = sid_r.clone();
        if expected == 1 {
            let result =
                rpc.call_raw("agent.prompt", json!({"session_id":sid_r,"input":"retry after crash"}), |_| json!(null));
            assert!(result["error"].is_null(), "{result}");
            let waited =
                rpc.call_raw("agent.wait", json!({"session_id":sid_r,"turn_id":result["result"]["turn_id"]}), |_| {
                    json!(null)
                });
            assert!(waited["error"].is_null(), "{waited}");
        }
        let messages = rpc.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
        let rows = messages["result"]["data"].as_array().unwrap();
        let delivered: Vec<_> = rows
            .iter()
            .filter(|message| {
                message["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["text"] == "crash boundary image"))
            })
            .collect();
        assert_eq!(
            delivered.len(),
            1,
            "accepted input must appear exactly once after delivery and restart: {messages}"
        );
        assert_eq!(delivered[0]["content"][1]["data"], "AQIDBA==");
        rpc.call_raw("shutdown", json!({}), |_| json!(null));
        rpc.wait_exit();
    }
}

/// A test plugin's latch: it marks that it got there (`<home>/latched`) and blocks, so the test
/// can SIGKILL the process at that point (in process: a remote voter on a test-only point is
/// refused).
const LATCH: &str = r#"
local function latch(info)
    local f = assert(io.open(mare.env("MARE_LATCH_MARK"), "w"))
    f:write(mare.json.encode(info))
    f:close()
    while true do mare.task.sleep(60000) end
end
"#;

fn wait_for_latch(mark: &std::path::Path, mut child_done: impl FnMut() -> Option<String>) {
    let t0 = std::time::Instant::now();
    while !mark.exists() {
        if let Some(why) = child_done() {
            panic!("finished before the latch: {why}");
        }
        assert!(t0.elapsed() < std::time::Duration::from_secs(60), "the latch was never reached");
        // sleep-ok: poll interval while waiting for the child to write the latch file (bounded by the 60 s assert)
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn hold_turn_at_request_ready(rpc: &mut RpcClient, sid: &str, text: &str, allow: impl Fn(&Value) -> Value) -> Value {
    let req = rpc.request("agent.prompt", json!({"session_id": sid, "input": text}));
    let mut prompt_done = false;
    let mut held = None;
    while !(prompt_done && held.is_some()) {
        let frame = rpc.read();
        if frame["id"] == req {
            assert!(frame["error"].is_null(), "{frame}");
            prompt_done = true;
        } else if frame["method"] == "callback.call" {
            if frame["params"]["arguments"]["event"] == "request/ready" && held.is_none() {
                held = Some(frame);
            } else {
                rpc.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":allow(&frame)}));
            }
        }
    }
    held.unwrap()
}

#[test]
fn rpc_abort_with_request_id_replays_complete_cancelled_inputs_after_sigkill_and_preserves_newer_queue() {
    let home = tempfile::tempdir().unwrap();
    // the cut point: an in-process agent/queue listener (emitted right after `session:commit`
    // returns, before the abort's reply) that holds the server once `<home>/arm` exists
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("latch.lua"),
        format!(
            r#"{LATCH}
return {{
    name = "latch",
    apply = function(ctx)
        ctx:on("queue/changed", function()
            local armed = io.open(mare.env("MARE_LATCH_ARM"), "r")
            if armed then
                armed:close()
                latch({{}})
            end
        end)
    end,
}}
"#
        ),
    )
    .unwrap();
    let spawn = |resume: Option<&str>| {
        let mut command = mare_cmd();
        command.args(["--rpc", "--model", "faux/echo"]);
        if let Some(path) = resume {
            command.args(["--resume", path]);
        }
        command
            .env("MARE_LATCH_ARM", home.path().join("arm"))
            .env("MARE_LATCH_MARK", home.path().join("latched"))
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stderr(Stdio::null());
        RpcClient::spawn_cmd(&mut command)
    };
    let allow = |_: &Value| json!({"value":{"decision":"allow"}});
    let mut rpc = spawn(None);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let sub1 =
        rpc.call_raw("hooks.subscribe", json!({"scope":"all","event":"request/ready","callbacks":["handle"]}), |_| {
            json!(null)
        });
    assert!(sub1["error"].is_null(), "{sub1}");
    let sub2 = rpc.call_raw(
        "hooks.subscribe",
        json!({"scope":"all","event":"session/commit-ready","callbacks":["handle"]}),
        |_| json!(null),
    );
    assert!(sub2["error"].is_null(), "{sub2}");
    let _held = hold_turn_at_request_ready(&mut rpc, &sid, "hold before model", allow);
    let accepted = rpc.call_raw(
        "agent.prompt",
        json!({"session_id":sid,"input":[{"type":"text","text":"abort receipt image"},{"type":"image","mime":"image/png","data":"AQIDBA=="}]}),
        allow,
    );
    assert_eq!(accepted["result"]["status"], "queued", "{accepted}");
    let sessions = rpc.call_raw("session.list", json!({}), allow);
    let path = sessions["result"]["data"].as_array().unwrap()[0]["path"].as_str().unwrap().to_owned();
    // armed after the prompt's own agent/queue: the next one is the abort's commit
    std::fs::write(home.path().join("arm"), "").unwrap();
    let abort_req = rpc.request("agent.cancel", json!({"session_id":sid,"request_id":"abort-req-1"}));
    loop {
        let frame = rpc.read();
        assert!(frame["id"] != abort_req, "abort returned unexpectedly early: {frame}");
        if frame["method"] == "callback.call" {
            let empty_table =
                |v: &Value| v.as_array().is_some_and(|a| a.is_empty()) || v.as_object().is_some_and(|o| o.is_empty());
            let args = &frame["params"]["arguments"];
            let is_abort_commit = args["event"] == "session/commit-ready"
                && empty_table(&args["args"][0]["inbox"]["follow_up"])
                && empty_table(&args["args"][0]["messages"]);
            rpc.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":allow(&frame)}));
            if is_abort_commit {
                break;
            }
        }
    }
    // The server is held inside agent/queue (the receipt is durable on disk) and the abort's
    // reply has not left: SIGKILL there.
    wait_for_latch(&home.path().join("latched"), || None);
    rpc.kill_and_wait();
    // disarmed: the restarted process must not latch on its own queue events
    std::fs::remove_file(home.path().join("arm")).unwrap();

    let mut rpc = spawn(Some(&path));
    let init2 = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid2 = init2["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    rpc.call_raw("hooks.subscribe", json!({"scope":"all","event":"request/ready","callbacks":["handle"]}), |_| {
        json!(null)
    });
    let held_new = hold_turn_at_request_ready(&mut rpc, &sid2, "newer turn", allow);
    let newer = rpc.call_raw("agent.prompt", json!({"session_id":sid2,"input":"newer queued item"}), allow);
    assert_eq!(newer["result"]["status"], "queued", "{newer}");

    let replay = rpc.call_raw("agent.cancel", json!({"session_id":sid2,"request_id":"abort-req-1"}), allow);
    assert!(replay["error"].is_null(), "{replay}");
    assert!(replay["result"]["removed"].is_array(), "{replay}");
    assert_eq!(replay["result"]["removed"][0]["text"], "abort receipt image", "{replay}");
    assert_eq!(replay["result"]["removed"][0]["images"][0]["data"], "AQIDBA==", "{replay}");

    let state = rpc.call_raw("agent.get", json!({"session_id":sid2}), allow);
    assert_eq!(state["result"]["queued"], 1, "replay must not clear newer queue items: {state}");
    assert_eq!(state["result"]["busy"], true, "replay must not cancel a newer turn: {state}");

    rpc.send(json!({"jsonrpc":"2.0","id":held_new["id"],"result":allow(&held_new)}));
    rpc.call_raw("shutdown", json!({}), allow);
    rpc.wait_exit();
}

#[test]
fn rpc_sigkill_during_attachment_preparation_leaves_journal_intact() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("latch.lua"),
        format!(
            r#"{LATCH}
return {{
    name = "latch",
    apply = function(ctx)
        if mare.env("MARE_LATCH_ATTACH") ~= "1" then
            return
        end
        local original = mare.fs.write
        mare.fs.write = function(path, bytes, opts)
            if path:find("%.attachments/") then
                latch({{ path = path }})
            end
            return original(path, bytes, opts)
        end
    end,
}}
"#
        ),
    )
    .unwrap();
    let spawn = |resume: Option<&str>, latch: bool| {
        let mut command = mare_cmd();
        command.args(["--rpc", "--model", "faux/echo"]);
        if let Some(path) = resume {
            command.args(["--resume", path]);
        }
        if latch {
            command.env("MARE_LATCH_ATTACH", "1").env("MARE_LATCH_MARK", home.path().join("latched"));
        }
        command.env("MARE_HOME", home.path()).current_dir(home.path()).stderr(Stdio::null());
        RpcClient::spawn_cmd(&mut command)
    };
    let allow = |_: &Value| json!({"value":{"decision":"allow"}});
    let mut rpc = spawn(None, true);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let seed = rpc.call_raw("agent.prompt", json!({"session_id":sid,"input":"seed session"}), allow);
    assert!(seed["error"].is_null(), "{seed}");
    let _ = rpc.call_raw("agent.wait", json!({"session_id":sid,"turn_id":seed["result"]["turn_id"]}), allow);
    let sessions = rpc.call_raw("session.list", json!({}), allow);
    let path = sessions["result"]["data"].as_array().unwrap()[0]["path"].as_str().unwrap().to_owned();

    let attach_req = rpc.request(
        "agent.prompt",
        json!({"session_id":sid,"input":[{"type":"text","text":"attachment crash"},{"type":"image","mime":"image/png","data":"AQIDBA=="}]}),
    );
    let _ = attach_req;
    wait_for_latch(&home.path().join("latched"), || None);
    rpc.kill_and_wait();

    let mut rpc = spawn(Some(&path), false);
    let init_r = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid_r = init_r["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let state = rpc.call_raw("agent.get", json!({"session_id":sid_r}), |_| json!(null));
    assert_eq!(state["result"]["queued"], 0, "{state}");
    let sid = sid_r.clone();
    let messages = rpc.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
    assert!(
        !messages.to_string().contains("attachment crash"),
        "SIGKILL during attachment preparation must not commit partial message: {messages}"
    );
    let after =
        rpc.call_raw("agent.prompt", json!({"session_id":sid_r,"input":"after attachment crash"}), |_| json!(null));
    assert!(after["error"].is_null(), "{after}");
    let _ =
        rpc.call_raw("agent.wait", json!({"session_id":sid_r,"turn_id":after["result"]["turn_id"]}), |_| json!(null));
    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_sigkill_during_final_publish_callback_preserves_committed_queue_state() {
    let home = tempfile::tempdir().unwrap();
    // the publish latch: an in-process agent/queue listener (emitted right after `session:commit`
    // durability + publish, before the prompt's reply), armed by `<home>/arm`
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("latch.lua"),
        format!(
            r#"{LATCH}
return {{
    name = "latch",
    apply = function(ctx)
        ctx:on("queue/changed", function()
            local armed = io.open(mare.env("MARE_LATCH_ARM"), "r")
            if armed then
                armed:close()
                latch({{}})
            end
        end)
    end,
}}
"#
        ),
    )
    .unwrap();
    let spawn = |resume: Option<&str>| {
        let mut command = mare_cmd();
        command.args(["--rpc", "--model", "faux/echo"]);
        if let Some(path) = resume {
            command.args(["--resume", path]);
        }
        command
            .env("MARE_LATCH_ARM", home.path().join("arm"))
            .env("MARE_LATCH_MARK", home.path().join("latched"))
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stderr(Stdio::null());
        RpcClient::spawn_cmd(&mut command)
    };
    let allow = |_: &Value| json!({"value":{"decision":"allow"}});
    let mut rpc = spawn(None);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let sub =
        rpc.call_raw("hooks.subscribe", json!({"scope":"all","event":"request/ready","callbacks":["handle"]}), |_| {
            json!(null)
        });
    assert!(sub["error"].is_null(), "{sub}");
    let seed = rpc.call_raw("agent.prompt", json!({"session_id":sid,"input":"seed session"}), allow);
    assert!(seed["error"].is_null(), "{seed}");
    let _ = rpc.call_raw("agent.wait", json!({"session_id":sid,"turn_id":seed["result"]["turn_id"]}), allow);
    let sessions = rpc.call_raw("session.list", json!({}), allow);
    let path = sessions["result"]["data"].as_array().unwrap()[0]["path"].as_str().unwrap().to_owned();
    let held = hold_turn_at_request_ready(&mut rpc, &sid, "hold turn", allow);
    std::fs::write(home.path().join("arm"), "").unwrap();
    let queue_req = rpc.request(
        "agent.prompt",
        json!({"session_id":sid,"input":[{"type":"text","text":"publish boundary image"},{"type":"image","mime":"image/png","data":"AQIDBA=="}]}),
    );
    let _ = queue_req;
    wait_for_latch(&home.path().join("latched"), || None);
    rpc.kill_and_wait();
    let _ = held;
    // disarmed: the restarted process must not latch on its own queue events
    std::fs::remove_file(home.path().join("arm")).unwrap();

    let mut rpc = spawn(Some(&path));
    let init_r = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid_r = init_r["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let state = rpc.call_raw("agent.get", json!({"session_id":sid_r}), |_| json!(null));
    assert_eq!(state["result"]["queued"], 1, "{state}");
    let delivered =
        rpc.call_raw("agent.prompt", json!({"session_id":sid_r,"input":"deliver after publish crash"}), |_| {
            json!(null)
        });
    assert!(delivered["error"].is_null(), "{delivered}");
    let _ = rpc
        .call_raw("agent.wait", json!({"session_id":sid_r,"turn_id":delivered["result"]["turn_id"]}), |_| json!(null));
    let messages = rpc.call_raw("agent.list_messages", json!({ "session_id": sid_r }), |_| json!(null));
    let body = messages.to_string();
    assert!(body.contains("publish boundary image") && body.contains("AQIDBA=="), "{messages}");
    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_sigkill_at_failure_rollback_point_preserves_queue_and_allows_request_id_retry() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("latch.lua"),
        format!(
            r#"{LATCH}
return {{
    name = "latch",
    inject = {{ "session" }}, apply = function(ctx)
        if mare.env("MARE_LATCH_ROLLBACK") ~= "1" then
            return
        end
        local original = ctx.session.release_operation
        ctx.session.release_operation = function(self, value, reason)
            latch({{ reason = reason }})
            return original(self, value, reason)
        end
    end,
}}
"#
        ),
    )
    .unwrap();
    let spawn = |resume: Option<&str>, latch: bool| {
        let mut command = mare_cmd();
        command.args(["--rpc", "--model", "faux/echo"]);
        if let Some(path) = resume {
            command.args(["--resume", path]);
        }
        if latch {
            command.env("MARE_LATCH_ROLLBACK", "1").env("MARE_LATCH_MARK", home.path().join("latched"));
        }
        command.env("MARE_HOME", home.path()).current_dir(home.path()).stderr(Stdio::null());
        RpcClient::spawn_cmd(&mut command)
    };
    let allow = |_: &Value| json!({"value":{"decision":"allow"}});
    let mut rpc = spawn(None, true);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"callbacks":true},"callback_timeout_ms":60000}),
        |_| json!(null),
    );
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let sub1 =
        rpc.call_raw("hooks.subscribe", json!({"scope":"all","event":"request/ready","callbacks":["handle"]}), |_| {
            json!(null)
        });
    assert!(sub1["error"].is_null(), "{sub1}");
    let sub2 = rpc.call_raw(
        "hooks.subscribe",
        json!({"scope":"all","event":"session/commit-ready","callbacks":["handle"]}),
        |_| json!(null),
    );
    assert!(sub2["error"].is_null(), "{sub2}");
    let _held = hold_turn_at_request_ready(&mut rpc, &sid, "hold before abort", allow);
    let accepted = rpc.call_raw(
        "agent.prompt",
        json!({"session_id":sid,"input":[{"type":"text","text":"rollback boundary image"},{"type":"image","mime":"image/png","data":"AQIDBA=="}]}),
        allow,
    );
    assert_eq!(accepted["result"]["status"], "queued", "{accepted}");
    let sessions = rpc.call_raw("session.list", json!({}), allow);
    let path = sessions["result"]["data"].as_array().unwrap()[0]["path"].as_str().unwrap().to_owned();

    let abort_req = rpc.request("agent.cancel", json!({"session_id":sid,"request_id":"abort-rollback-1"}));
    let empty_table =
        |v: &Value| v.as_array().is_some_and(|a| a.is_empty()) || v.as_object().is_some_and(|o| o.is_empty());
    loop {
        let frame = rpc.read();
        assert!(frame["id"] != abort_req, "abort returned before rollback SIGKILL latch: {frame}");
        if frame["method"] == "callback.call" {
            let args = &frame["params"]["arguments"];
            let is_abort_commit = args["event"] == "session/commit-ready"
                && empty_table(&args["args"][0]["inbox"]["follow_up"])
                && empty_table(&args["args"][0]["messages"]);
            if is_abort_commit {
                // the injected rollback runs into the plugin's latch (release_operation)
                rpc.send(json!({"jsonrpc":"2.0","id":frame["id"],
                    "result":{"value":{"decision":"deny","reason":"injected rollback"}}}));
                break;
            }
            rpc.send(json!({"jsonrpc":"2.0","id":frame["id"],"result":allow(&frame)}));
        }
    }
    wait_for_latch(&home.path().join("latched"), || None);
    rpc.kill_and_wait();

    let mut rpc = spawn(Some(&path), false);
    let init_r = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let sid_r = init_r["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let state = rpc.call_raw("agent.get", json!({"session_id":sid_r}), |_| json!(null));
    assert_eq!(state["result"]["queued"], 1, "rolled-back abort must keep queued input: {state}");
    let retry =
        rpc.call_raw("agent.cancel", json!({"session_id":sid_r,"request_id":"abort-rollback-1"}), |_| json!(null));
    assert!(retry["error"].is_null(), "{retry}");
    assert!(retry["result"]["removed"].is_array(), "{retry}");
    assert_eq!(retry["result"]["removed"][0]["text"], "rollback boundary image", "{retry}");
    assert_eq!(retry["result"]["removed"][0]["images"][0]["data"], "AQIDBA==", "{retry}");
    let state = rpc.call_raw("agent.get", json!({"session_id":sid_r}), |_| json!(null));
    assert_eq!(state["result"]["queued"], 0, "{state}");
    rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}
