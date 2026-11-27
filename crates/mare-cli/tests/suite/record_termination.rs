//! durable `session:record(..., {sync=true})` with owner/write-barrier enforcement,
//! RPC `session.record`/`session.records`, and explicit `termination_reason` parity.
use crate::common::*;
use mlua::LuaSerdeExt;
use serde_json::{Value, json};
use std::process::Stdio;

fn msg(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "entry_id": id,
        "parent_entry_id": parent,
        "message": { "role": role, "content": [{ "type": "text", "text": text }] }
    })
}

#[tokio::test]
async fn custom_sync_record_is_durable_and_disabled_sessions_refuse_it() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        let path: String = eval(
            &h,
            "assert(S:add_record('sync-probe', { revision = 1 }, { sync = true })); assert(#S:list_records('sync-probe').data == 1); return S:info().path",
        )
        .await;
        assert!(std::fs::read_to_string(path).unwrap().contains("sync-probe"));
        h.shutdown().await;

        let h = env.host().faux(json!([])).no_session().build().await;
        expose(&h);
        let (ok, count): (bool, usize) = eval(
            &h,
            "local ok, err = pcall(S.add_record, S, 'sync-probe', {}, { sync = true }); assert(not ok and err); return ok == true, #S:list_records('sync-probe').data",
        )
        .await;
        assert!(!ok);
        assert_eq!(count, 0);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn failed_custom_sync_record_is_not_published() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.home.path().join("state")).unwrap();
        std::fs::write(env.home.path().join("state/sessions"), "").unwrap();
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        let (ok, count): (bool, usize) = eval(
            &h,
            "local ok, err = pcall(S.add_record, S, 'failed-sync', {}, { sync = true }); assert(not ok and err); return ok == true, #S:list_records('failed-sync').data",
        )
        .await;
        assert!(!ok);
        assert_eq!(count, 0);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn readonly_custom_sync_record_never_acknowledges_or_publishes() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("locked-custom.jsonl", &[msg("m1", None, "user", "hello")]);
        let before = std::fs::read(&path).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        global(&h, "PATH", path.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.notify = function() end
            mare.ui.confirm = function() return true end
            mare.ui.select = function() return 2 end
            assert(S:load({ path = PATH }))
            assert(S:info().readonly)
            "#,
        )
        .await;
        let (ok, count): (bool, usize) = eval(
            &h,
            "local ok, err = pcall(S.add_record, S, 'readonly-sync', {}, { sync = true }); assert(not ok and err); return ok == true, #S:list_records('readonly-sync').data",
        )
        .await;
        assert!(!ok);
        assert_eq!(count, 0);
        assert_eq!(std::fs::read(path).unwrap(), before);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn custom_record_owner_options_reject_stale_session_or_generation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        let (wrote_ok, read_len, stale_write, stale_read): (bool, usize, bool, bool) = eval(
            &h,
            r#"
            local info = S:info()
            local owner = { session_id = info.session_id, session_epoch = info.session_epoch }
            assert(S:add_record('owned', { step = 1 }, { persist = true, sync = true, session_id = owner.session_id, session_epoch = owner.session_epoch }))
            local rows = assert(S:list_records('owned', owner)).data
            local w_ok, w_err = pcall(S.add_record, S, 'owned', { step = 2 }, { sync = true, session_id = owner.session_id, session_epoch = 'stale' })
            local r_ok, r_err = pcall(S.list_records, S, 'owned', { session_id = owner.session_id, session_epoch = 'stale' })
            return true, #rows, (not w_ok and w_err ~= nil), (not r_ok and r_err ~= nil)
            "#,
        )
        .await;
        assert!(wrote_ok);
        assert_eq!(read_len, 1);
        assert!(stale_write);
        assert!(stale_read);
        h.shutdown().await;
    })
    .await;
}

#[test]
fn rpc_custom_records_bind_owner_and_report_durability() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("script.json");
    std::fs::write(&script, json!([{ "text": "rpc ok" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call("initialize", json!({ "protocol_version": "3.0" }));
    let methods = init["capabilities"]["methods"].as_array().unwrap();
    assert!(methods.contains(&json!("session.add_record")), "{init}");
    assert!(methods.contains(&json!("session.list_records")), "{init}");

    let session = rpc.call("session.create", json!({}));
    let s1_id = session["session_id"].as_str().unwrap().to_owned();
    let owner = json!({"kind": "probe", "session_id": s1_id});
    let mut write = owner.clone();
    write["data"] = json!({"revision": 1});
    write["sync"] = json!(true);
    let result = rpc.call("session.add_record", write.clone());
    assert_eq!(result["durable"], true, "{result}");
    let read = rpc.call("session.list_records", owner.clone());
    assert_eq!(read["data"][0]["revision"], 1, "{read}");
    for reserved in ["permission/preset", "sandbox/mode", "agent/queue", "session/transition-initial"] {
        let mut forged = owner.clone();
        forged["kind"] = json!(reserved);
        forged["data"] = json!({"preset": "full-access"});
        let resp = rpc.call_raw("session.add_record", forged, |_| json!(null));
        assert!(resp.get("error").is_some(), "RPC session.add_record must reject reserved kind {reserved}: {resp}");
    }
    let s2 = rpc.call("session.create", json!({}));
    assert!(rpc.call_raw("session.close", json!({"session_id": s1_id}), |_| json!(null))["error"].is_null());
    assert_eq!(rpc.call_raw("session.add_record", write, |_| json!(null))["error"]["data"]["code"], "unknown_session");
    assert_eq!(
        rpc.call_raw("session.list_records", owner, |_| json!(null))["error"]["data"]["code"],
        "unknown_session"
    );
    let s2_id = s2["session_id"].as_str().unwrap().to_owned();

    let started = rpc.call("agent.prompt", json!({ "session_id": s2_id, "input": "hello" }));
    let prompt_res = rpc.call("agent.wait", json!({ "session_id": s2_id, "turn_id": started["turn_id"] }));
    assert_eq!(prompt_res["termination_reason"], "completed", "{prompt_res}");
    let end = rpc.wait_event("turn/completed");
    assert_eq!(end["data"]["termination_reason"], "completed", "{end}");
    mare_protocol::events::validate_protocol("turn/completed", &end["data"]).unwrap();

    rpc.shutdown_and_wait();
}

#[tokio::test]
async fn final_termination_reason_uses_structured_outcome() {
    local(async {
        for (kind, message, reason) in [
            ("usage_limit", "account exhausted", "usage_limit"),
            ("quota", "account exhausted", "usage_limit"),
            ("rate_limit", "too many requests", "error"),
        ] {
            let env = TestEnv::new().await;
            let h = env
                .host()
                .faux(json!([{ "error": message, "kind": kind }]))
                .config(json!({ "plugins": { "llm": { "retries": 0 } } }))
                .no_session()
                .build()
                .await;
            expose(&h);
            exec(&h, r#"C.cordis:on("turn/completed", function(res) FINAL_END = res end)"#).await;
            let res = h.prompt("hi", json!({})).await.unwrap();
            assert_eq!(res["termination_reason"], reason, "{res}");
            assert_eq!(res["error_kind"], kind, "{res}");
            let end_json: String = eval(&h, "return mare.json.encode(FINAL_END)").await;
            let end: Value = serde_json::from_str(&end_json).unwrap();
            assert_eq!(end["termination_reason"], reason, "{end}");
            mare_protocol::events::validate_protocol("turn/completed", &end).unwrap();
            h.shutdown().await;
        }

        // Turn limit
        {
            let env = TestEnv::new().await;
            let h = env
                .host()
                .faux(json!([{ "text": "one" }, { "text": "two" }]))
                .config(json!({ "plugins": { "agent": { "max_turns": 1 } } }))
                .no_session()
                .build()
                .await;
            expose(&h);
            exec(&h, r#"C.cordis:on("agent/turn-stopping", function() return "again" end)"#).await;
            let res = h.prompt("hi", json!({})).await.unwrap();
            assert_eq!(res["termination_reason"], "turn_limit", "{res}");
            assert_eq!(res["error"]["message"], "max turns exceeded");
            assert_eq!(res["error_kind"], "max_turns");
            h.shutdown().await;
        }

        // Interrupted
        {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([{ "text": "slow reply", "delay_ms": 5000 }])).no_session().build().await;
            let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let h_prompt = h.clone();
            let prompt = tokio::task::spawn_local(async move { h_prompt.prompt("hi", json!({})).await.unwrap() });
            while let Some(rec) = cursor.recv().await {
                if let mare_kernel::CursorRecord::Event { event: ev, .. } = rec
                    && ev.name == "step/started"
                {
                    break;
                }
            }
            h.abort().await.unwrap();
            let res = prompt.await.unwrap();
            assert_eq!(res["termination_reason"], "interrupted", "{res}");
            assert_eq!(res["error"]["code"], "aborted");
            h.shutdown().await;
        }
    })
    .await;
}

#[test]
fn headless_settled_carries_structured_termination_reason() {
    for (script, extra_args, expected_reason, expected_code) in [
        (json!([{ "text": "ok" }]), vec![], "completed", 0),
        (json!([{ "error": "quota exhausted", "kind": "usage_limit" }]), vec![], "usage_limit", 1),
        (
            json!([
                { "tool_calls": [{ "id": "t1", "name": "bash", "arguments": { "cmd": "true" } }] },
                { "text": "never" }
            ]),
            vec!["--yolo", "--max-turns", "1"],
            "turn_limit",
            4,
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let script_path = home.path().join("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let out = mare_cmd()
            .args(["--json", "--model", "faux/echo", "--no-session"])
            .args(extra_args)
            .arg("go")
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &script_path)
            .current_dir(home.path())
            .stdin(Stdio::null())
            .run_output();
        assert_eq!(out.status.code(), Some(expected_code), "{}", String::from_utf8_lossy(&out.stderr));
        let lines: Vec<Value> =
            String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        let end = lines.iter().find(|l| l["type"] == "turn/completed").expect("missing turn/completed");
        assert_eq!(end["data"]["termination_reason"], expected_reason, "{end}");
        let settled = lines.last().expect("missing settled");
        assert_eq!(settled["type"], "settled");
        assert_eq!(settled["data"]["termination_reason"], expected_reason, "{settled}");
    }
}

#[test]
fn session_items_records_and_agent_list_messages() {
    use std::io::BufReader;
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(cfg.join("plugins")).unwrap();
    std::fs::write(cfg.join("init.lua"), r#"_G.KEEP_B18C_SECRET = mare.projection.register("top-secret-b18c")"#)
        .unwrap();
    let mut child = mare_cmd()
        .args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut rpc = RpcClient::new(BufReader::new(child.stdout.take().unwrap()), child.stdin.take().unwrap());
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let caps = init["result"]["capabilities"]["methods"].as_array().unwrap();
    for present in ["session.list_items", "session.add_record", "session.list_records", "agent.list_messages"] {
        assert!(caps.iter().any(|m| m == present), "missing {present} in capabilities: {caps:?}");
    }
    for removed in ["session.entries", "session.messages", "session.record", "session.records"] {
        assert!(!caps.iter().any(|m| m == removed), "removed {removed} still advertised");
        let err = rpc.call_raw(removed, json!({}), |_| json!(null));
        assert_eq!(err["error"]["code"], -32601, "removed {removed} must be -32601: {err}");
    }

    let s1 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    let s1_id = s1["session_id"].as_str().unwrap().to_string();
    let started =
        rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "echo top-secret-b18c" }), |_| json!(null));
    assert!(started.get("error").is_none(), "{started}");
    let waited =
        rpc.call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": started["result"]["turn_id"] }), |_| {
            json!(null)
        });
    assert!(waited.get("error").is_none(), "{waited}");

    let msgs = rpc.call_raw("agent.list_messages", json!({ "session_id": s1_id }), |_| json!(null));
    let msg_arr = msgs["result"]["data"].as_array().expect("agent.list_messages must return { data }");
    assert!(!msg_arr.is_empty(), "{msgs}");
    assert!(!msgs.to_string().contains("top-secret-b18c"), "secret leaked in agent.list_messages: {msgs}");

    let items_all = rpc.call_raw(
        "session.list_items",
        json!({ "session_id": s1_id, "branch": "current", "direction": "desc", "limit": 2 }),
        |_| json!(null),
    );
    let item_arr = items_all["result"]["data"].as_array().expect("session.list_items must return { data }");
    assert!(!item_arr.is_empty() && item_arr.len() <= 2, "{items_all}");
    assert!(items_all["result"]["leaf_entry_id"].is_string(), "{items_all}");
    assert!(!items_all.to_string().contains("top-secret-b18c"), "secret leaked in session.list_items: {items_all}");

    let items_filtered =
        rpc.call_raw("session.list_items", json!({ "session_id": s1_id, "types": ["user_message"] }), |_| json!(null));
    assert!(!items_filtered["result"]["data"].as_array().unwrap().is_empty(), "{items_filtered}");
    for it in items_filtered["result"]["data"].as_array().unwrap() {
        assert_eq!(it["type"], "user_message", "{it}");
    }

    let add_rec = rpc.call_raw(
        "session.add_record",
        json!({
            "session_id": s1_id,
            "kind": "probe/custom",
            "data": { "token": "top-secret-b18c", "rev": 1 },
            "sync": true
        }),
        |_| json!(null),
    );
    assert!(add_rec["result"]["record_id"].is_string() && add_rec["result"].get("item_id").is_none(), "{add_rec}");
    assert_eq!(add_rec["result"]["durable"], true, "{add_rec}");
    let record_as_item = rpc.call_raw(
        "session.get_item",
        json!({ "session_id": s1_id, "item_id": add_rec["result"]["record_id"] }),
        |_| json!(null),
    );
    assert_eq!(record_as_item["error"]["data"]["code"], "session.unknown_item", "{record_as_item}");

    let list_rec = rpc.call_raw(
        "session.list_records",
        json!({ "session_id": s1_id, "kind": "probe/custom", "branch": "current" }),
        |_| json!(null),
    );
    let rec_rows = list_rec["result"]["data"].as_array().expect("session.list_records must return { data }");
    assert_eq!(rec_rows.len(), 1, "{list_rec}");
    assert_eq!(rec_rows[0]["rev"], 1, "{list_rec}");
    assert!(!list_rec.to_string().contains("top-secret-b18c"), "secret leaked in session.list_records: {list_rec}");

    for reserved in [
        "job",
        "permission/preset",
        "sandbox/mode",
        "approval/resolved",
        "agent/queue",
        "session/transition-initial",
        "working-directory/changed",
    ] {
        let err =
            rpc.call_raw("session.add_record", json!({ "session_id": s1_id, "kind": reserved, "data": {} }), |_| {
                json!(null)
            });
        assert!(err.get("error").is_some(), "reserved kind {reserved} must be rejected: {err}");
    }

    // Closing `s1_id` invalidates old session_id for session-scoped record calls.
    let s2 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    assert!(rpc.call_raw("session.close", json!({ "session_id": s1_id }), |_| json!(null))["error"].is_null());
    let stale_err =
        rpc.call_raw("session.list_records", json!({ "session_id": s1_id, "kind": "probe/custom" }), |_| json!(null));
    assert_eq!(stale_err["error"]["data"]["code"], "unknown_session", "{stale_err}");
    let _ = s2;

    let _ = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    let _ = child.wait();
}

#[tokio::test]
async fn turn_completed_and_wait_report_cancellation_cause_for_all_origins() {
    local(async {
        for (reason, expected_cause) in [
            (None, "user"),
            (Some("user"), "user"),
            (Some("parent"), "parent"),
            (Some("hook"), "hook"),
            (Some("disposed"), "disposed"),
            (Some("freeform-note"), "user"),
        ] {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([{ "text": "slow reply", "delay_ms": 5000 }])).no_session().build().await;
            let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let h_prompt = h.clone();
            let prompt = tokio::task::spawn_local(async move { h_prompt.prompt("hi", json!({})).await.unwrap() });
            while let Some(rec) = cursor.recv().await {
                if let mare_kernel::CursorRecord::Event { event: ev, .. } = rec
                    && ev.name == "step/started"
                {
                    break;
                }
            }
            let cancel_arg = match reason {
                Some(r) => json!({ "reason": r }),
                None => json!({}),
            };
            let _ = h
                .call_service(
                    "agent",
                    "cancel",
                    mlua::MultiValue::from_vec(vec![h.lua().to_value(&cancel_arg).unwrap()]),
                )
                .await
                .unwrap();
            let res = prompt.await.unwrap();
            assert_eq!(res["termination_reason"], "interrupted", "reason={reason:?}: {res}");
            assert_eq!(res["cancellation_cause"], expected_cause, "reason={reason:?}: {res}");

            let mut ended = None;
            while let Ok(Some(rec)) = cursor.try_recv() {
                if let mare_kernel::CursorRecord::Event { event: ev, .. } = rec
                    && ev.name == "turn/completed"
                {
                    ended = Some(ev.payload);
                }
            }
            let ended = ended.expect("missing turn/completed");
            assert_eq!(ended["status"], "cancelled", "{ended}");
            assert_eq!(ended["termination_reason"], "interrupted", "{ended}");
            assert_eq!(ended["cancellation_cause"], expected_cause, "reason={reason:?}: {ended}");
            mare_protocol::events::validate_protocol("turn/completed", &ended).unwrap();

            let turn_id = res["turn_id"].as_str().expect("turn_id");
            let waited: Value = h
                .lua()
                .from_value(
                    h.call_service(
                        "agent",
                        "wait",
                        mlua::MultiValue::from_vec(vec![mlua::Value::String(h.lua().create_string(turn_id).unwrap())]),
                    )
                    .await
                    .unwrap()
                    .pop_front()
                    .unwrap_or(mlua::Value::Nil),
                )
                .unwrap();
            assert_eq!(waited["cancellation_cause"], expected_cause, "wait result for reason={reason:?}: {waited}");
            h.shutdown().await;
        }

        // Normal completed turn omits cancellation_cause.
        {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([{ "text": "ok" }])).no_session().build().await;
            let mut cursor = h.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
            let res = h.prompt("hi", json!({})).await.unwrap();
            assert_eq!(res["termination_reason"], "completed", "{res}");
            assert!(res.get("cancellation_cause").is_none(), "completed run must omit cancellation_cause: {res}");
            let mut ended = None;
            while let Ok(Some(rec)) = cursor.try_recv() {
                if let mare_kernel::CursorRecord::Event { event: ev, .. } = rec
                    && ev.name == "turn/completed"
                {
                    ended = Some(ev.payload);
                }
            }
            let ended = ended.expect("missing turn/completed");
            assert!(
                ended.get("cancellation_cause").is_none(),
                "completed turn/completed must omit cancellation_cause: {ended}"
            );
            h.shutdown().await;
        }

        // Real subagent cancelled by parent session switch (`source = "session_switch"`)
        // records cancellation_cause = "disposed" on turn/completed.
        {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([{ "text": "slow child", "delay_ms": 5000 }])).build().await;
            expose(&h);
            let child_ended_json: String = eval(
                &h,
                r#"
                local child = mare.subagent.start({ prompt = "child work" })
                local entered = false
                while true do
                    local ev = child:next_event()
                    if not ev then break end
                    if ev.name == "step/started" then entered = true break end
                end
                assert(entered, "child did not reach step/started")
                local ended_tx, ended_rx = mare.task.channel()
                mare.task.spawn(function()
                    local ended
                    while true do
                        local ev = child:next_event()
                        if not ev then break end
                        if ev.name == "turn/completed" then
                            ended = ev.payload
                            break
                        end
                    end
                    ended_tx:send(ended or false)
                end)
                S:create()
                local ok, ended = mare.task.timeout(5000, function() return ended_rx:recv() end)
                assert(ok and type(ended) == "table", "expected turn/completed from child")
                pcall(child.wait, child)
                return mare.json.encode(ended)
                "#,
            )
            .await;
            let ended: Value = serde_json::from_str(&child_ended_json).unwrap();
            assert_eq!(ended["status"], "cancelled", "{ended}");
            assert_eq!(ended["termination_reason"], "interrupted", "{ended}");
            assert_eq!(ended["cancellation_cause"], "disposed", "{ended}");
            mare_protocol::events::validate_protocol("turn/completed", &ended).unwrap();
            h.shutdown().await;
        }

        // Real subagent cancelled by parent records cancellation_cause = "parent" on turn/completed.
        {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([{ "text": "slow child", "delay_ms": 5000 }])).build().await;
            expose(&h);
            let child_ended_json: String = eval(
                &h,
                r#"
                local child = mare.subagent.start({ prompt = "child work" })
                local entered = false
                while true do
                    local ev = child:next_event()
                    if not ev then break end
                    if ev.name == "step/started" then entered = true break end
                end
                assert(entered, "child did not reach step/started")
                child:cancel()
                local ended
                while true do
                    local ev = child:next_event()
                    if not ev then break end
                    if ev.name == "turn/completed" then
                        ended = ev.payload
                        break
                    end
                end
                pcall(child.wait, child)
                return mare.json.encode(ended)
                "#,
            )
            .await;
            let ended: Value = serde_json::from_str(&child_ended_json).unwrap();
            assert_eq!(ended["status"], "cancelled", "{ended}");
            assert_eq!(ended["termination_reason"], "interrupted", "{ended}");
            assert_eq!(ended["cancellation_cause"], "parent", "{ended}");
            mare_protocol::events::validate_protocol("turn/completed", &ended).unwrap();
            h.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn turn_stopping_continuation_preserves_source_and_marks_notification() {
    local(async {
        // 1. Table return `{ message, source }` preserves `source` and marks `notification = true`.
        {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([{ "text": "first" }, { "text": "second" }])).no_session().build().await;
            expose(&h);
            exec(
                &h,
                r#"
                STOP_CALLS = 0
                REQ_MSGS = {}
                C.cordis:on("request/build", function(req, ev)
                    REQ_MSGS[#REQ_MSGS + 1] = mare.json.encode(req.messages or {})
                    return ev.next(req)
                end)
                C.cordis:on("agent/turn-stopping", function()
                    STOP_CALLS = STOP_CALLS + 1
                    if STOP_CALLS == 1 then
                        return { message = "STOP CONTINUE", source = "hooks" }
                    end
                end)
                "#,
            )
            .await;
            let res = h.prompt("start", json!({})).await.unwrap();
            assert_eq!(res["termination_reason"], "completed", "{res}");
            assert_eq!(res["text"], "second", "{res}");
            let req2_json: String = eval(&h, "return REQ_MSGS[2] or '[]'").await;
            let req2: Vec<Value> = serde_json::from_str(&req2_json).unwrap();
            let cont = req2
                .iter()
                .find(|m| m["role"] == "user" && m["content"].to_string().contains("STOP CONTINUE"))
                .unwrap_or_else(|| panic!("missing STOP CONTINUE in request 2 messages: {req2:?}"));
            assert_eq!(cont["source"], "hooks", "Stop continuation must preserve source: {cont}");
            assert_eq!(cont["notification"], true, "Stop continuation with source must set notification=true: {cont}");
            h.shutdown().await;
        }

        // 2. Plain string return stays an ordinary user continuation without source/notification.
        {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([{ "text": "first" }, { "text": "second" }])).no_session().build().await;
            expose(&h);
            exec(
                &h,
                r#"
                STOP_CALLS = 0
                REQ_MSGS = {}
                C.cordis:on("request/build", function(req, ev)
                    REQ_MSGS[#REQ_MSGS + 1] = mare.json.encode(req.messages or {})
                    return ev.next(req)
                end)
                C.cordis:on("agent/turn-stopping", function()
                    STOP_CALLS = STOP_CALLS + 1
                    if STOP_CALLS == 1 then
                        return "PLAIN CONTINUE"
                    end
                end)
                "#,
            )
            .await;
            let res = h.prompt("start", json!({})).await.unwrap();
            assert_eq!(res["termination_reason"], "completed", "{res}");
            let req2_json: String = eval(&h, "return REQ_MSGS[2] or '[]'").await;
            let req2: Vec<Value> = serde_json::from_str(&req2_json).unwrap();
            let cont = req2
                .iter()
                .find(|m| m["role"] == "user" && m["content"].to_string().contains("PLAIN CONTINUE"))
                .unwrap_or_else(|| panic!("missing PLAIN CONTINUE in request 2 messages: {req2:?}"));
            assert!(cont.get("source").is_none(), "plain string continuation must not set source: {cont}");
            assert!(cont.get("notification").is_none(), "plain string continuation must not set notification: {cont}");
            h.shutdown().await;
        }
    })
    .await;
}
