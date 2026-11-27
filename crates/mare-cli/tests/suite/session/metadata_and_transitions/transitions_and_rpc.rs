use super::*;

#[tokio::test]
async fn before_switch_before_fork_and_before_tree_policy_hooks_can_cancel() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"one"},{"text":"two"}])).build().await;
        expose(&h);
        let r1 = h.prompt("first", json!({})).await.unwrap();
        assert_eq!(r1["text"], "one");
        let r2 = h.prompt("second", json!({})).await.unwrap();
        assert_eq!(r2["text"], "two");

        exec(
            &h,
            r#"
            DENY_SWITCH, DENY_FORK, DENY_TREE = true, true, true
            C.cordis:get("policy"):voter('session/before-switch', function()
                return { decision = DENY_SWITCH and 'deny' or 'allow', reason = 'switch blocked' }
            end)
            C.cordis:get("policy"):voter('session/before-fork', function()
                return { decision = DENY_FORK and 'deny' or 'allow', reason = 'fork blocked' }
            end)
            C.cordis:get("policy"):voter('session/before-tree', function()
                return { decision = DENY_TREE and 'deny' or 'allow', reason = 'tree blocked' }
            end)

            local info = C.cordis:get("session"):info()
            local first_entry = C.cordis:get("session"):entries()[1].entry_id
            local leaf_before = C.cordis:get("session"):leaf()

            local ok_tree, err_tree = C.cordis:get("session"):set_leaf(first_entry)
            assert(not ok_tree and tostring(err_tree):find('tree blocked', 1, true), tostring(err_tree))
            assert(C.cordis:get("session"):leaf() == leaf_before)

            local ok_fork, err_fork = pcall(function() C.cordis:get("session"):fork({ session_id = info.session_id }) end)
            assert(not ok_fork and tostring(err_fork):find('fork blocked', 1, true), tostring(err_fork))
            assert(C.cordis:get("session"):info().session_id == info.session_id)

            local ok_new, err_new = pcall(function() C.cordis:get("session"):create() end)
            assert(not ok_new and tostring(err_new):find('switch blocked', 1, true), tostring(err_new))
            assert(C.cordis:get("session"):info().session_id == info.session_id)

            local ok_load, err_load = pcall(function() C.cordis:get("session"):load({ path = info.path }) end)
            assert(not ok_load and tostring(err_load):find('switch blocked', 1, true), tostring(err_load))
            assert(C.cordis:get("session"):info().session_id == info.session_id)

            DENY_TREE, DENY_FORK, DENY_SWITCH = false, false, false
            assert(C.cordis:get("session"):set_leaf(first_entry))
            assert(C.cordis:get("session"):leaf() == first_entry)
            assert(C.cordis:get("session"):set_leaf(leaf_before))
            C.cordis:get("session"):fork({ session_id = info.session_id })
            assert(C.cordis:get("session"):info().session_id ~= info.session_id)
            local forked_id = C.cordis:get("session"):info().session_id
            C.cordis:get("session"):create()
            assert(C.cordis:get("session"):info().session_id ~= forked_id)
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_metadata_public_apis_return_version_leaf_and_errors() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"hello"},{"text":"world"}])).build().await;
        expose(&h);
        fake_ui(&h).await;
        assert_eq!(h.prompt("first", json!({})).await.unwrap()["text"], "hello");
        assert_eq!(h.prompt("second", json!({})).await.unwrap()["text"], "world");

        exec(
            &h,
            r#"
            local info = C.cordis:get("session"):info()
            local msg_ids = {}
            for _, e in ipairs(C.cordis:get("session"):entries()) do
                if e.type == "message" and e.entry_id then
                    msg_ids[#msg_ids + 1] = e.entry_id
                end
            end
            local first_user_id, first_assistant_id, second_user_id = msg_ids[1], msg_ids[2], msg_ids[3]
            assert(first_user_id and first_assistant_id and second_user_id, "expected two turns of messages")

            local ren = C.cordis:get("session"):update('renamed-session')
            assert(ren and ren.session_id == info.session_id and ren.name == 'renamed-session' and ren.leaf_entry_id == info.leaf_entry_id)
            assert(C.cordis:get("session"):info().name == 'renamed-session')

            local lbl = C.cordis:get("session"):label_item('checkpoint-1')
            assert(lbl and lbl.item_id == info.leaf_entry_id and lbl.label == 'checkpoint-1')

            local ok_bad, bad_lbl = pcall(function()
                return C.cordis:get("session"):label_item('nope', { item_id = 'missing-entry-id' })
            end)
            assert(not ok_bad and type(bad_lbl) == 'table' and bad_lbl.code == 'session.unknown_item', tostring(bad_lbl))

            local rew_mid = C.cordis:get("session"):rewind({ session_id = info.session_id, before_item_id = second_user_id })
            assert(rew_mid and rew_mid.session_id == info.session_id and rew_mid.leaf_entry_id == first_assistant_id, mare.json.encode(rew_mid))

            local rew = C.cordis:get("session"):rewind({ session_id = info.session_id, before_item_id = first_user_id })
            assert(rew and rew.session_id == info.session_id and rew.leaf_entry_id == nil, mare.json.encode(rew))

            C.cordis:get("session"):create()
            local arch = C.cordis:get("session"):archive(info.session_id)
            assert(type(arch) == 'table' and next(arch) == nil)
            local archived_id = info.session_id
            local archived_path = info.path

            local unarch = C.cordis:get("session"):unarchive(archived_id)
            assert(type(unarch) == 'table' and next(unarch) == nil)

            local del = C.cordis:get("session"):delete(archived_id)
            assert(type(del) == 'table' and next(del) == nil and not mare.fs.exists(archived_path))

            local ok_missing, missing_err = pcall(function()
                return C.cordis:get("session"):delete('nonexistent-session-id')
            end)
            assert(not ok_missing and tostring(missing_err):find('no session matches', 1, true), tostring(missing_err))
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn clean_context_new_transition_commits_records_and_runs_initial_input() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {"tool_calls":[{"name":"handoff-tool","arguments":{}}]},
                {"text":"first-turn-done"},
                {"text":"clean-turn-done"}
            ]))
            .build()
            .await;
        expose(&h);
        let settled = std::sync::Arc::new(tokio::sync::Notify::new());
        let settled_signal = settled.clone();
        global(
            &h,
            "SIGNAL_SETTLED",
            h.lua()
                .create_function(move |_, ()| {
                    settled_signal.notify_one();
                    Ok(())
                })
                .unwrap(),
        );
        exec(
            &h,
            r#"
            TRANSITION_EVENT = nil
            FAILED_TRANSITION_EVENT = nil
            CLEAN_SETTLED = false
            C.cordis:on('session/transition-completed', function(ev)
                TRANSITION_EVENT = ev
                CLEAN_SETTLED = true
                SIGNAL_SETTLED()
            end)
            C.cordis:on('session/transition-failed', function(ev)
                FAILED_TRANSITION_EVENT = ev
                CLEAN_SETTLED = true
                SIGNAL_SETTLED()
            end)
            C.cordis:get("tools"):register('handoff-tool', {
                description = 'request clean-context transition',
                side_effect = 'read',
                parameters = { type = 'object' },
                execute = function(_, tctx)
                    local info = C.cordis:get("session"):info()
                    SOURCE_ID = info.session_id
                    local res = C.cordis:get("session"):transition({
                        mode = 'new',
                        records = { { kind = 'handoff/context', data = { plan = 'step 1' }, scope = 'session' } },
                        initial = { kind = 'input', content = { { type = 'text', text = 'execute clean plan' } }, source = 'handoff' }
                    }, { session_id = info.session_id, session_epoch = info.session_epoch, cancel = tctx.run_cancel, defer = true })
                    assert(res.status == 'deferred', tostring(res.error))
                    return { content = 'handoff scheduled' }
                end
            })
        "#,
        )
        .await;
        let first = h.prompt("start", json!({})).await.unwrap();
        assert_eq!(first["text"], "first-turn-done");
        if !eval::<bool>(&h, "return CLEAN_SETTLED").await {
            tokio::time::timeout(std::time::Duration::from_secs(5), settled.notified())
                .await
                .expect("timed out waiting for clean-context initial run to settle");
        }
        let err: Option<String> = eval(
            &h,
            "return FAILED_TRANSITION_EVENT and mare.json.encode(FAILED_TRANSITION_EVENT) or nil",
        )
        .await;
        assert!(err.is_none(), "transition failed: {err:?}");
        let detail: String = eval(
            &h,
            r#"
            local info = C.cordis:get("session"):info()
            local recs = C.cordis:get("session"):list_records('handoff/context').data
            local msgs = C.cordis:get("agent"):list_messages().data
            if TRANSITION_EVENT ~= nil
                and TRANSITION_EVENT.mode == 'new'
                and TRANSITION_EVENT.initial_status == 'settled'
                and TRANSITION_EVENT.source.session_id == SOURCE_ID
                and TRANSITION_EVENT.target.session_id == info.session_id
                and info.session_id ~= SOURCE_ID
                and #recs == 1 and recs[1].plan == 'step 1'
                and #msgs == 2
                and msgs[1].content[1].text == 'execute clean plan'
                and msgs[2].content[1].text == 'clean-turn-done' then
                return "ok"
            end
            return mare.json.encode({
                ev = TRANSITION_EVENT,
                info = info,
                source_id = SOURCE_ID,
                recs = recs,
                msgs = msgs,
            })
        "#,
        )
        .await;
        assert_eq!(detail, "ok");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn deferred_transition_is_cancelled_when_turn_aborts_even_without_explicit_cancel() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {"tool_calls":[{"name":"handoff-no-cancel","arguments":{}}]},
                {"text":"should-not-complete"}
            ]))
            .build()
            .await;
        expose(&h);
        let failed = std::sync::Arc::new(tokio::sync::Notify::new());
        let failed_signal = failed.clone();
        global(
            &h,
            "SIGNAL_FAILED",
            h.lua()
                .create_function(move |_, ()| {
                    failed_signal.notify_one();
                    Ok(())
                })
                .unwrap(),
        );
        exec(
            &h,
            r#"
            FAILED_EV = nil
            FINISHED_EV = nil
            C.cordis:on('session/transition-completed', function(ev) FINISHED_EV = ev end)
            C.cordis:on('session/transition-failed', function(ev)
                FAILED_EV = ev
                SIGNAL_FAILED()
            end)
            C.cordis:get("tools"):register('handoff-no-cancel', {
                description = 'schedule deferred handoff without explicit cancel, then abort',
                side_effect = 'read',
                parameters = { type = 'object' },
                execute = function()
                    local info = C.cordis:get("session"):info()
                    SOURCE_ID = info.session_id
                    local res = C.cordis:get("session"):transition({
                        mode = 'new',
                        records = { { kind = 'handoff/context', data = { plan = 'aborted' }, scope = 'session' } },
                    }, { session_id = info.session_id, session_epoch = info.session_epoch, defer = true })
                    assert(res.status == 'deferred', tostring(res.error))
                    C.cordis:get("agent"):cancel()
                    return { content = 'aborted turn' }
                end
            })
        "#,
        )
        .await;
        let _ = h.prompt("start", json!({})).await;
        if eval::<bool>(&h, "return FAILED_EV == nil").await {
            tokio::time::timeout(std::time::Duration::from_secs(5), failed.notified())
                .await
                .expect("timed out waiting for cancelled deferred transition failure");
        }
        let err: Option<String> =
            eval(&h, "return FAILED_TRANSITION_EVENT and mare.json.encode(FAILED_TRANSITION_EVENT) or nil").await;
        assert!(err.is_none(), "transition failed: {err:?}");
        let ok: bool = eval(
            &h,
            r#"
            return FINISHED_EV == nil
                and FAILED_EV ~= nil
                and FAILED_EV.status == 'rejected'
                and tostring(FAILED_EV.error):find('cancelled', 1, true) ~= nil
                and C.cordis:get("session"):info().session_id == SOURCE_ID
        "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn initial_handoff_failure_surfaces_as_rejected_and_does_not_emit_finished() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            FINISHED_EV = nil
            FAILED_EV = nil
            COMMITTED_EV = nil
            C.cordis:on('session/transition-committed', function(ev) COMMITTED_EV = ev end)
            C.cordis:on('session/transition-completed', function(ev) FINISHED_EV = ev end)
            C.cordis:on('session/transition-failed', function(ev) FAILED_EV = ev end)

            -- the seam: a method replaced on the published agent service is what every caller,
            -- the provider's own calls included, runs (mare_lib.service)
            local orig_prompt = A.prompt
            A.prompt = function()
                return { error = 'provider unavailable', not_sent = true }
            end

            local info = S:info()
            local res = S:transition({
                mode = 'new',
                initial = { kind = 'input', content = 'implement plan', source = 'handoff' },
            }, { session_id = info.session_id, session_epoch = info.session_epoch })

            A.prompt = orig_prompt
            assert(res.status == 'rejected', tostring(res.status))
            assert(tostring(res.error):find('provider unavailable', 1, true), tostring(res.error))
            assert(COMMITTED_EV ~= nil and COMMITTED_EV.has_initial == true)
            assert(FINISHED_EV == nil, 'transition-finished must not fire on initial failure')
            assert(FAILED_EV ~= nil and FAILED_EV.status == 'rejected')
            local recs = S:list_records('session/transition-initial').data
            assert(#recs == 2 and recs[1].status == 'pending' and recs[2].status == 'failed', 'expected durable pending -> failed records')
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[test]
fn rpc_session_metadata_and_transition_parity() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let methods = init["result"]["capabilities"]["methods"].as_array().unwrap();
    for m in [
        "session.list",
        "session.update",
        "session.label_item",
        "session.archive",
        "session.unarchive",
        "session.delete",
        "session.rewind",
    ] {
        assert!(methods.iter().any(|v| v == m), "missing {m} in capabilities");
    }
    assert!(!methods.iter().any(|v| v == "session.transition"), "session.transition must not be exposed on RPC");

    let s1 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    let s1_id = s1["session_id"].as_str().unwrap().to_string();
    let p1s = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "first" }), |_| json!(null));
    assert!(p1s.get("error").is_none(), "{p1s}");
    let p1 = rpc
        .call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p1s["result"]["turn_id"] }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");

    let ren = rpc.call_raw("session.update", json!({ "session_id": s1_id, "name": "rpc-renamed" }), |_| json!(null));
    assert_eq!(ren["result"]["name"], "rpc-renamed", "{ren}");
    assert_eq!(ren["result"]["session_id"], s1_id, "{ren}");

    let lbl = rpc.call_raw("session.label_item", json!({ "session_id": s1_id, "label": "tag-1" }), |_| json!(null));
    assert_eq!(lbl["result"]["label"], "tag-1", "{lbl}");
    assert!(lbl["result"]["item_id"].is_string(), "{lbl}");

    let rew = rpc.call_raw("session.rewind", json!({ "session_id": s1_id }), |_| json!(null));
    assert!(rew.get("error").is_none(), "{rew}");
    assert!(rew["result"].get("leaf_entry_id").is_none(), "{rew}");

    let _s2 = rpc.call_raw("session.create", json!({}), |_| json!(null));
    assert_eq!(rpc.call_raw("session.close", json!({ "session_id": s1_id }), |_| json!(null))["result"], json!({}));
    let arch = rpc.call_raw("session.archive", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(arch["result"], json!({}), "{arch}");

    let unarch = rpc.call_raw("session.unarchive", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(unarch["result"], json!({}), "{unarch}");

    let del = rpc.call_raw("session.delete", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(del["result"], json!({}), "{del}");

    let tr = rpc.call_raw("session.transition", json!({ "spec": { "mode": "new" } }), |_| json!(null));
    assert_eq!(tr["error"]["code"], -32601, "session.transition must be Lua-only (-32601 on RPC): {tr}");

    let _ = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_session_rewind_summarize_failure_reports_error_with_leaf() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let script_path = home.path().join("script.json");
    std::fs::write(
        &script_path,
        json!([
            { "text": "first reply" },
            { "text": "second reply" },
            { "error": "provider down", "kind": "api_error", "retryable": false }
        ])
        .to_string(),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let s1 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    let s1_id = s1["session_id"].as_str().unwrap().to_string();

    let p1s = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "first" }), |_| json!(null));
    assert!(p1s.get("error").is_none(), "{p1s}");
    let p1 = rpc
        .call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p1s["result"]["turn_id"] }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    let entries1 = rpc.call_raw("session.list_items", json!({ "session_id": s1_id }), |_| json!(null));
    let target_leaf = entries1["result"]["leaf_entry_id"]
        .as_str()
        .unwrap_or_else(|| panic!("expected leaf_id in session.list_items: {entries1}"))
        .to_owned();

    let p2s = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "second" }), |_| json!(null));
    assert!(p2s.get("error").is_none(), "{p2s}");
    let p2 = rpc
        .call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p2s["result"]["turn_id"] }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");

    let entries2 = rpc.call_raw("session.list_items", json!({ "session_id": s1_id }), |_| json!(null));
    let second_user_id = entries2["result"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["parent_entry_id"] == target_leaf)
        .and_then(|it| it["item_id"].as_str())
        .expect("turn 2 root item must point to target_leaf")
        .to_owned();

    let rew = rpc.call_raw(
        "session.rewind",
        json!({ "session_id": s1_id, "before_item_id": second_user_id, "summarize": true }),
        |_| json!(null),
    );
    assert!(rew.get("error").is_some(), "expected RPC error when summarize failed, got: {rew}");
    let msg = rew["error"]["message"].as_str().unwrap_or("");
    assert!(msg.contains("provider down"), "expected summarize failure reason in message: {rew}");
    assert_eq!(rew["error"]["data"]["data"]["rewound"], true, "{rew}");
    assert_eq!(rew["error"]["data"]["data"]["leaf_entry_id"], target_leaf, "{rew}");

    let _ = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[test]
fn rpc_session_archive_does_not_self_cancel_on_session_changed() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    // Register a yielding session/changed listener in init.lua so session.create yields
    // after rotate_session_cancel() fires on the old session token.
    std::fs::write(
        home.path().join("init.lua"),
        r#"
        mare.on("session/changed", function()
            mare.task.sleep(20)
        end)
        "#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));

    let s1 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    let s1_id = s1["session_id"].as_str().unwrap().to_string();
    let p1s = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "first" }), |_| json!(null));
    assert!(p1s.get("error").is_none(), "{p1s}");
    let p1 = rpc
        .call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p1s["result"]["turn_id"] }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");

    // Switch explicitly and close s1 before archiving an inactive session cleanly without self-cancelling.
    let s2 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    assert_eq!(
        rpc.call_raw("session.close", json!({ "session_id": s1["session_id"] }), |_| json!(null))["result"],
        json!({})
    );
    let arch = rpc.call_raw("session.archive", json!({ "session_id": s1["session_id"] }), |_| json!(null));
    assert_eq!(arch["result"], json!({}), "session.archive failed: {arch}");
    assert!(s2["session_id"].is_string(), "{s2}");

    let _ = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[tokio::test]
async fn lua_session_transition_does_not_self_cancel_on_session_changed() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "follow-up ok" }])).build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"mare.on("session/changed", function() mare.task.sleep(20) end)
               local cur = S:info()
               local tr, err = S:transition({
                   mode = "new",
                   records = { { kind = "handoff/lua", data = { n = 7 }, scope = "session" } },
                   initial = { kind = "input", content = "follow-up", source = "handoff" },
               }, { session_id = cur.session_id, session_epoch = cur.session_epoch })
               assert(err == nil, tostring(err))
               return tr.status == "applied" and tr.target.session_id ~= cur.session_id"#,
        )
        .await;
        assert!(ok, "Lua session:transition with records and initial input must not self-cancel on session/changed");
        h.shutdown().await;
    })
    .await;
}

#[test]
fn rpc_session_management_contract_and_no_implicit_switch() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let work = dunce::canonicalize(&work).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"]).env("MARE_HOME", home.path()).current_dir(&work).stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let caps = init["result"]["capabilities"]["methods"].as_array().unwrap();
    for removed in ["session.rename", "session.label"] {
        assert!(!caps.iter().any(|m| m == removed), "legacy {removed} still advertised");
        let err = rpc.call_raw(removed, json!({}), |_| json!(null));
        assert_eq!(err["error"]["code"], -32601, "legacy {removed} must be -32601: {err}");
    }
    for spi in [
        "session.persist_message",
        "session.record_tool_state",
        "session.record_request",
        "session.record_request_end",
        "session.record_request_aborted",
        "session.prepare_children",
    ] {
        assert!(!caps.iter().any(|m| m == spi), "SPI {spi} must not be advertised over RPC");
        let err = rpc.call_raw(spi, json!({}), |_| json!(null));
        assert_eq!(err["error"]["code"], -32601, "SPI {spi} must not be exposed over RPC: {err}");
    }
    for present in [
        "session.list",
        "session.update",
        "session.label_item",
        "session.archive",
        "session.unarchive",
        "session.delete",
    ] {
        assert!(caps.iter().any(|m| m == present), "missing {present} in capabilities");
    }

    let s1 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    let s1_id = s1["session_id"].as_str().unwrap().to_string();
    let p1 = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "one" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    assert!(
        rpc.call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p1["result"]["turn_id"] }), |_| json!(null))
            .get("error")
            .is_none()
    );

    // session.update validates session_id, expected_revision, and returns Session.
    let updated = rpc.call_raw(
        "session.update",
        json!({ "session_id": s1_id, "name": "renamed-v1", "expected_revision": 0 }),
        |_| json!(null),
    );
    assert!(updated.get("error").is_none(), "{updated}");
    assert_eq!(updated["result"]["session_id"], s1_id, "{updated}");
    assert_eq!(updated["result"]["name"], "renamed-v1", "{updated}");
    assert_eq!(updated["result"]["loaded"], true, "{updated}");
    let stale_rev =
        rpc.call_raw("session.update", json!({ "session_id": s1_id, "name": "stale", "expected_revision": 0 }), |_| {
            json!(null)
        });
    assert_eq!(stale_rev["error"]["data"]["code"], "session.revision_conflict", "{stale_rev}");
    let bad_sess =
        rpc.call_raw("session.update", json!({ "session_id": "nonexistent-session", "name": "nope" }), |_| json!(null));
    assert_eq!(bad_sess["error"]["data"]["code"], "unknown_session", "{bad_sess}");

    // session.label_item sets and clears labels on current leaf or item_id.
    let lbl = rpc.call_raw("session.label_item", json!({ "session_id": s1_id, "label": "pin-1" }), |_| json!(null));
    assert!(lbl.get("error").is_none(), "{lbl}");
    let item_id = lbl["result"]["item_id"].as_str().unwrap().to_string();
    assert_eq!(lbl["result"]["label"], "pin-1", "{lbl}");
    let cleared =
        rpc.call_raw("session.label_item", json!({ "session_id": s1_id, "item_id": item_id, "label": "" }), |_| {
            json!(null)
        });
    assert!(cleared.get("error").is_none(), "{cleared}");
    assert_eq!(cleared["result"]["item_id"], item_id, "{cleared}");
    assert!(cleared["result"].get("label").is_none(), "cleared label should be omitted: {cleared}");

    // session.archive/delete refuse the currently open session rather than implicitly switching.
    let arch_cur = rpc.call_raw("session.archive", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(arch_cur["error"]["data"]["code"], "session.active_session", "{arch_cur}");
    let del_cur = rpc.call_raw("session.delete", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(del_cur["error"]["data"]["code"], "session.active_session", "{del_cur}");

    // Switch to a second session explicitly, then list/archive/unarchive/delete s1 via {session_id} -> {}.
    let s2 = rpc.call_raw("session.create", json!({}), |_| json!(null))["result"].clone();
    let s2_id = s2["session_id"].as_str().unwrap().to_string();
    let p2 = rpc.call_raw("agent.prompt", json!({ "session_id": s2_id, "input": "two" }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");
    assert!(
        rpc.call_raw("agent.wait", json!({ "session_id": s2_id, "turn_id": p2["result"]["turn_id"] }), |_| json!(null))
            .get("error")
            .is_none()
    );

    let listed = rpc.call_raw("session.list", json!({}), |_| json!(null));
    let data = listed["result"]["data"].as_array().expect("session.list must return { data }");
    assert_eq!(data.len(), 2, "{listed}");
    let s1_row = data.iter().find(|r| r["session_id"] == s1_id).unwrap();
    assert_eq!(s1_row["name"], "renamed-v1", "session.list must reflect session.update name: {listed}");
    let s1_path = s1_row["path"].as_str().unwrap().to_string();

    let cwd_filtered = rpc.call_raw("session.list", json!({ "cwd": work.to_string_lossy() }), |_| json!(null));
    assert_eq!(cwd_filtered["result"]["data"].as_array().unwrap().len(), 2, "{cwd_filtered}");
    let other_cwd_filtered =
        rpc.call_raw("session.list", json!({ "cwd": home.path().join("absent-dir").to_string_lossy() }), |_| {
            json!(null)
        });
    assert_eq!(other_cwd_filtered["result"]["data"].as_array().unwrap().len(), 0, "{other_cwd_filtered}");
    let parent_filtered = rpc.call_raw("session.list", json!({ "parent_session_id": s1_id }), |_| json!(null));
    assert_eq!(parent_filtered["result"]["data"].as_array().unwrap().len(), 0, "{parent_filtered}");

    // Another process holding the lock on s1 causes session.archive/delete to return `"busy"`.
    let mut lock_cmd = mare_cmd();
    lock_cmd
        .args(["--rpc", "--model", "faux/echo", "--resume", &s1_path])
        .env("MARE_HOME", home.path())
        .current_dir(&work)
        .stderr(Stdio::null());
    let mut lock_rpc = RpcClient::spawn_cmd(&mut lock_cmd);
    assert!(
        lock_rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null)).get("error").is_none()
    );
    let locked_arch = rpc.call_raw("session.archive", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(locked_arch["error"]["data"]["code"], "busy", "{locked_arch}");
    let locked_del = rpc.call_raw("session.delete", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(locked_del["error"]["data"]["code"], "busy", "{locked_del}");
    let _ = lock_rpc.call_raw("shutdown", json!({}), |_| json!(null));
    lock_rpc.wait_exit();

    let loaded_both = rpc.call_raw("session.list", json!({ "loaded": true }), |_| json!(null));
    assert_eq!(loaded_both["result"]["data"].as_array().unwrap().len(), 2, "{loaded_both}");
    assert_eq!(rpc.call_raw("session.close", json!({ "session_id": s1_id }), |_| json!(null))["result"], json!({}));
    let loaded_only = rpc.call_raw("session.list", json!({ "loaded": true }), |_| json!(null));
    let loaded_rows = loaded_only["result"]["data"].as_array().unwrap();
    assert_eq!(loaded_rows.len(), 1, "{loaded_only}");
    assert_eq!(loaded_rows[0]["session_id"], s2_id, "{loaded_only}");

    let bad_limit = rpc.call_raw("session.list", json!({ "limit": 0 }), |_| json!(null));
    assert_eq!(bad_limit["error"]["data"]["code"], "invalid_arguments", "{bad_limit}");

    let arch = rpc.call_raw("session.archive", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(arch["result"], json!({}), "{arch}");
    // Active session stayed s2 (no implicit switch).
    let still_loaded = rpc.call_raw("session.list", json!({ "loaded": true }), |_| json!(null));
    assert_eq!(still_loaded["result"]["data"][0]["session_id"], s2_id, "{still_loaded}");

    let unarch = rpc.call_raw("session.unarchive", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(unarch["result"], json!({}), "{unarch}");

    let del = rpc.call_raw("session.delete", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(del["result"], json!({}), "{del}");
    let after_del = rpc.call_raw("session.list", json!({}), |_| json!(null));
    let after_rows = after_del["result"]["data"].as_array().unwrap();
    assert!(after_rows.iter().all(|r| r["session_id"] != s1_id), "{after_del}");

    let _ = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    rpc.wait_exit();
}

#[tokio::test]
async fn tui_slash_archive_and_delete_explicitly_switch_without_implicit_service_switch() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "one" }, { "text": "two" }])).build().await;
        expose(&h);
        fake_ui(&h).await;

        assert_eq!(h.prompt("first", json!({})).await.unwrap()["text"], "one");
        let (first_id, first_path): (String, String) =
            eval(&h, "local i = C.cordis:get('session'):info(); return i.session_id, i.path").await;

        // Direct session:archive(active_id) refuses instead of switching implicitly.
        let (arch_ok, arch_code, stayed_same): (bool, String, bool) = eval(
            &h,
            r#"
            local S = C.cordis:get("session")
            local before = S:info()
            local ok, err = pcall(S.archive, S, before.session_id)
            local after = S:info()
            return ok, type(err) == "table" and tostring(err.code) or tostring(err), after.session_id == before.session_id
            "#,
        )
        .await;
        assert!(!arch_ok && arch_code == "session.active_session" && stayed_same, "{arch_code}");

        // TUI `/archive` explicitly switches to a new session and archives the old one.
        exec(&h, "C.cordis:get('commands'):run('/archive')").await;
        let (second_id, first_archived_rows): (String, usize) = eval(
            &h,
            &format!(
                r#"
                local i = C.cordis:get("session"):info()
                local rows = mare.store.index_read({{ session_id = {first_id:?}, archived = true }})
                return i.session_id, #rows
                "#
            ),
        )
        .await;
        assert_ne!(second_id, first_id, "TUI /archive must explicitly switch to a new session");
        assert_eq!(first_archived_rows, 1, "TUI /archive must archive the prior session");
        assert!(std::fs::read_to_string(&first_path).unwrap().contains("\"type\":\"archive\""));

        // TUI `/delete` explicitly switches to a new session and deletes the prior one.
        assert_eq!(h.prompt("second", json!({})).await.unwrap()["text"], "two");
        let (before_del_id, before_del_path): (String, String) =
            eval(&h, "local i = C.cordis:get('session'):info(); return i.session_id, i.path").await;
        assert_eq!(before_del_id, second_id);

        let (third_id, cleared): (String, bool) = eval(
            &h,
            r#"
            local tx, rx = mare.task.channel()
            local cleared = false
            mare.ui.confirm = function() return true end
            mare.ui.clear = function() cleared = true end
            mare.ui.notify = function(msg)
                if tostring(msg):find("Deleted session", 1, true) then tx:send(true) end
            end
            C.cordis:get("commands"):run("/delete")
            rx:recv()
            return C.cordis:get("session"):info().session_id, cleared
            "#,
        )
        .await;
        assert_ne!(third_id, before_del_id, "TUI /delete must explicitly switch to a new session");
        assert!(cleared, "TUI /delete must clear the UI");
        assert!(!std::path::Path::new(&before_del_path).exists(), "deleted session file must be removed");

        h.shutdown().await;
    })
    .await
}

#[test]
fn session_lifecycle_create_get_load_fork_close_rewind_contract() {
    use std::{io::BufReader, process::Stdio};
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let mut child = mare_cmd()
        .args(["--rpc", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(&work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut rpc = RpcClient::new(BufReader::new(child.stdout.take().unwrap()), child.stdin.take().unwrap());
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0"}), |_| json!(null));
    let caps = init["result"]["capabilities"]["methods"].as_array().unwrap();
    for present in ["session.create", "session.get", "session.load", "session.fork", "session.close", "session.rewind"]
    {
        assert!(caps.iter().any(|m| m == present), "missing {present} in capabilities: {caps:?}");
    }
    for removed in ["session.new", "session.resume", "session.transition"] {
        assert!(!caps.iter().any(|m| m == removed), "removed {removed} still advertised");
        let err = rpc.call_raw(removed, json!({}), |_| json!(null));
        assert_eq!(err["error"]["code"], -32601, "removed {removed} must be -32601: {err}");
    }

    // session.create: returns mare.Session without generation, supports name & idempotent request_id.
    let s1_resp =
        rpc.call_raw("session.create", json!({ "name": "alpha-b18d", "request_id": "create-1" }), |_| json!(null));
    let s1 = s1_resp["result"].clone();
    let s1_id = s1["session_id"].as_str().expect("session.create must return session_id").to_string();
    assert!(s1.get("generation").is_none(), "wire Session must omit generation: {s1}");
    assert_eq!(s1["name"], "alpha-b18d", "{s1}");
    assert_eq!(s1["name_revision"], 1, "{s1}");
    assert_eq!(s1["loaded"], true, "{s1}");

    let ren_cas = rpc.call_raw(
        "session.update",
        json!({ "session_id": s1_id, "name": "alpha-cas", "expected_revision": s1["name_revision"] }),
        |_| json!(null),
    );
    assert_eq!(ren_cas["result"]["name_revision"], 2, "{ren_cas}");
    assert_eq!(
        rpc.call_raw("session.get", json!({ "session_id": s1_id }), |_| json!(null))["result"]["name_revision"],
        2
    );
    let _ = rpc.call_raw(
        "session.update",
        json!({ "session_id": s1_id, "name": "alpha-b18d", "expected_revision": 2 }),
        |_| json!(null),
    );

    let s1_dup =
        rpc.call_raw("session.create", json!({ "name": "alpha-b18d", "request_id": "create-1" }), |_| json!(null));
    assert_eq!(s1_dup["result"]["session_id"], s1_id, "{s1_dup}");
    let s1_conflict =
        rpc.call_raw("session.create", json!({ "name": "other", "request_id": "create-1" }), |_| json!(null));
    assert_eq!(s1_conflict["error"]["data"]["code"], "request_id_conflict", "{s1_conflict}");

    let p1 = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "one" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    assert!(
        rpc.call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p1["result"]["turn_id"] }), |_| json!(null))
            .get("error")
            .is_none()
    );
    let first_leaf =
        rpc.call_raw("session.get", json!({ "session_id": s1_id }), |_| json!(null))["result"]["leaf_entry_id"]
            .as_str()
            .unwrap()
            .to_string();
    let p2 = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "two" }), |_| json!(null));
    assert!(p2.get("error").is_none(), "{p2}");
    assert!(
        rpc.call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p2["result"]["turn_id"] }), |_| json!(null))
            .get("error")
            .is_none()
    );

    // session.rewind: { session_id, before_item_id } -> mare.Session; `text` parameter dropped.
    let items_before_rew = rpc.call_raw("session.list_items", json!({ "session_id": s1_id }), |_| json!(null));
    let rew_second_user_id = items_before_rew["result"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|it| it["parent_entry_id"] == first_leaf)
        .and_then(|it| it["item_id"].as_str())
        .expect("turn 2 root item must point to first_leaf")
        .to_string();
    let bad_rew = rpc.call_raw(
        "session.rewind",
        json!({ "session_id": s1_id, "before_item_id": rew_second_user_id, "text": "dropped" }),
        |_| json!(null),
    );
    assert_eq!(bad_rew["error"]["data"]["code"], "invalid_arguments", "dropped `text` must be rejected: {bad_rew}");
    let rew =
        rpc.call_raw("session.rewind", json!({ "session_id": s1_id, "before_item_id": rew_second_user_id }), |_| {
            json!(null)
        });
    assert!(rew.get("error").is_none(), "{rew}");
    assert_eq!(rew["result"]["session_id"], s1_id, "{rew}");
    assert_eq!(rew["result"]["leaf_entry_id"], first_leaf, "{rew}");
    assert!(rew["result"].get("generation").is_none(), "{rew}");

    // session.fork: supports inactive session_id, before_item_id boundary, and rejects unknown_item.
    let bad_fork =
        rpc.call_raw("session.fork", json!({ "session_id": s1_id, "before_item_id": "nonexistent-entry" }), |_| {
            json!(null)
        });
    assert_eq!(
        bad_fork["error"]["data"]["code"], "session.unknown_item",
        "invalid before_item_id must fail with session.unknown_item: {bad_fork}"
    );
    let p3 = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "two-again" }), |_| json!(null));
    assert!(p3.get("error").is_none(), "{p3}");
    assert!(
        rpc.call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p3["result"]["turn_id"] }), |_| json!(null))
            .get("error")
            .is_none()
    );
    let items_after = rpc.call_raw("session.list_items", json!({ "session_id": s1_id }), |_| json!(null));
    let items_arr = items_after["result"]["data"].as_array().unwrap();
    let second_user_id = items_arr
        .iter()
        .find(|it| it["parent_entry_id"] == first_leaf)
        .and_then(|it| it["item_id"].as_str())
        .expect("turn 2 root item must point to first_leaf")
        .to_string();
    let partial_fork = rpc.call_raw(
        "session.fork",
        json!({ "session_id": s1_id, "before_item_id": second_user_id, "name": "forked-before-turn2" }),
        |_| json!(null),
    );
    assert!(partial_fork.get("error").is_none(), "{partial_fork}");
    assert_eq!(partial_fork["result"]["parent_session_id"], s1_id, "{partial_fork}");

    // Cross-cwd session.create sets live_snapshot cwd to target cwd, not mare.cwd.
    let other_proj = tempfile::tempdir().unwrap();
    let created_other =
        rpc.call_raw("session.create", json!({ "cwd": other_proj.path().to_string_lossy() }), |_| json!(null));
    assert!(created_other.get("error").is_none(), "{created_other}");
    let other_live_cwd = created_other["result"]["cwd"].as_str().unwrap().to_string();
    assert_eq!(
        dunce::canonicalize(&other_live_cwd).unwrap(),
        dunce::canonicalize(other_proj.path()).unwrap(),
        "{created_other}"
    );

    let s2 = rpc.call_raw("session.create", json!({ "name": "second" }), |_| json!(null))["result"].clone();
    let s2_id = s2["session_id"].as_str().unwrap().to_string();
    let got_s1 = rpc.call_raw("session.get", json!({ "session_id": s1_id }), |_| json!(null))["result"].clone();
    assert_eq!(got_s1["session_id"], s1_id);
    assert_eq!(got_s1["loaded"], true, "multi-session HostManager keeps sibling s1 loaded: {got_s1}");

    let forked = rpc
        .call_raw("session.fork", json!({ "session_id": s1_id, "name": "forked-from-s1" }), |_| json!(null))["result"]
        .clone();
    assert_eq!(forked["parent_session_id"], s1_id, "{forked}");
    assert_eq!(forked["name"], "forked-from-s1", "{forked}");
    assert_eq!(forked["loaded"], true, "{forked}");

    // session.load: loads existing session and returns mare.Session.
    let loaded = rpc.call_raw("session.load", json!({ "session_id": s1_id }), |_| json!(null))["result"].clone();
    assert_eq!(loaded["session_id"], s1_id, "{loaded}");
    assert_eq!(loaded["loaded"], true, "{loaded}");

    // session.close: unloads active session so `loaded` becomes false and `session.delete` succeeds.
    let closed = rpc.call_raw("session.close", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(closed["result"], json!({}), "{closed}");
    let after_close = rpc.call_raw("session.get", json!({ "session_id": s1_id }), |_| json!(null))["result"].clone();
    assert_eq!(after_close["loaded"], false, "session.close must unload runtime: {after_close}");
    assert_eq!(rpc.call_raw("session.delete", json!({ "session_id": s1_id }), |_| json!(null))["result"], json!({}));
    assert_eq!(
        rpc.call_raw("session.close", json!({ "session_id": "nonexistent-b18d" }), |_| json!(null))["error"]["data"]["code"],
        "unknown_session"
    );
    let _ = s2_id;

    let _ = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    drop(rpc);
    assert!(child.wait().unwrap().success());
}

#[test]
fn live_session_epoch_roundtrips_and_stale_blocks_mutation() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let work = dunce::canonicalize(&work).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo"]).env("MARE_HOME", home.path()).current_dir(&work).stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0" }), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");
    let init_epoch = init["result"]["session"]["session_epoch"].as_str().unwrap_or("");
    assert!(!init_epoch.is_empty(), "initialize must include session_epoch: {init}");

    let created = rpc.call_raw("session.create", json!({ "name": "orig" }), |_| json!(null));
    assert!(created.get("error").is_none(), "{created}");
    let s1_id = created["result"]["session_id"].as_str().unwrap().to_string();
    let e1 = created["result"]["session_epoch"].as_str().unwrap_or("").to_string();
    assert!(!e1.is_empty(), "session.create must return session_epoch: {created}");

    // Stale expected_epoch blocks mutations cleanly with zero side effects.
    let bad_upd = rpc.call_raw(
        "session.update",
        json!({ "session_id": s1_id, "name": "mutated", "expected_epoch": "stale-epoch" }),
        |_| json!(null),
    );
    assert_eq!(bad_upd["error"]["data"]["code"], "stale_session", "{bad_upd}");
    let got_after_upd = rpc.call_raw("session.get", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(got_after_upd["result"]["name"], "orig", "{got_after_upd}");
    assert_eq!(got_after_upd["result"]["session_epoch"], e1, "{got_after_upd}");

    let bad_close =
        rpc.call_raw("session.close", json!({ "session_id": s1_id, "expected_epoch": "stale-epoch" }), |_| json!(null));
    assert_eq!(bad_close["error"]["data"]["code"], "stale_session", "{bad_close}");
    assert_eq!(rpc.call_raw("session.get", json!({ "session_id": s1_id }), |_| json!(null))["result"]["loaded"], true);

    let bad_fork = rpc.call_raw(
        "session.fork",
        json!({ "session_id": s1_id, "expected_epoch": "stale-epoch", "name": "should-not-exist" }),
        |_| json!(null),
    );
    assert_eq!(bad_fork["error"]["data"]["code"], "stale_session", "{bad_fork}");

    // session.fork { switch = false } is removed in 3.0.
    let removed_switch = rpc.call_raw("session.fork", json!({ "session_id": s1_id, "switch": false }), |_| json!(null));
    assert_eq!(removed_switch["error"]["code"], -32602, "{removed_switch}");

    // Wire Scope::Session methods must reject legacy `expected_session` (-32602 invalid_arguments).
    let legacy_prompt = rpc.call_raw(
        "agent.prompt",
        json!({
            "session_id": s1_id,
            "input": "blocked",
            "expected_session": { "session_id": s1_id, "session_epoch": e1 }
        }),
        |_| json!(null),
    );
    assert_eq!(legacy_prompt["error"]["code"], -32602, "{legacy_prompt}");
    assert_eq!(legacy_prompt["error"]["data"]["code"], "invalid_arguments", "{legacy_prompt}");

    let legacy_update = rpc.call_raw(
        "session.update",
        json!({
            "session_id": s1_id,
            "name": "legacy-reject",
            "expected_session": { "session_id": s1_id, "session_epoch": e1 }
        }),
        |_| json!(null),
    );
    assert_eq!(legacy_update["error"]["code"], -32602, "{legacy_update}");
    assert_eq!(legacy_update["error"]["data"]["code"], "invalid_arguments", "{legacy_update}");

    // Run one turn so we can test rewind + agent.prompt expected_epoch checks.
    let p1 = rpc.call_raw("agent.prompt", json!({ "session_id": s1_id, "input": "hello" }), |_| json!(null));
    assert!(p1.get("error").is_none(), "{p1}");
    assert!(
        rpc.call_raw("agent.wait", json!({ "session_id": s1_id, "turn_id": p1["result"]["turn_id"] }), |_| json!(null))
            .get("error")
            .is_none()
    );
    let items = rpc.call_raw("session.list_items", json!({ "session_id": s1_id }), |_| json!(null));
    let first_id = items["result"]["data"]
        .as_array()
        .and_then(|arr| arr.iter().find_map(|entry| entry["item_id"].as_str()))
        .unwrap_or_else(|| panic!("missing item id in session.list_items: {items}"))
        .to_string();

    let bad_rewind = rpc.call_raw(
        "session.rewind",
        json!({ "session_id": s1_id, "before_item_id": first_id, "expected_epoch": "stale-epoch" }),
        |_| json!(null),
    );
    assert_eq!(bad_rewind["error"]["data"]["code"], "stale_session", "{bad_rewind}");

    let bad_record = rpc.call_raw(
        "session.add_record",
        json!({ "session_id": s1_id, "kind": "probe", "data": { "k": 1 }, "expected_epoch": "stale-epoch" }),
        |_| json!(null),
    );
    assert_eq!(bad_record["error"]["data"]["code"], "stale_session", "{bad_record}");

    let s2_created = rpc.call_raw("session.create", json!({ "name": "other" }), |_| json!(null));
    assert!(s2_created.get("error").is_none(), "{s2_created}");
    let s2_epoch = s2_created["result"]["session_epoch"].as_str().unwrap_or("").to_string();
    assert!(!s2_epoch.is_empty(), "{s2_created}");
    let cross_session = rpc.call_raw(
        "session.update",
        json!({ "session_id": s1_id, "name": "cross", "expected_epoch": s2_epoch }),
        |_| json!(null),
    );
    assert_eq!(cross_session["error"]["data"]["code"], "stale_session", "{cross_session}");

    let bad_prompt = rpc.call_raw(
        "agent.prompt",
        json!({ "session_id": s1_id, "input": "blocked", "expected_epoch": "stale-epoch" }),
        |_| json!(null),
    );
    assert_eq!(bad_prompt["error"]["data"]["code"], "stale_session", "{bad_prompt}");

    // Closing unloads runtime and clears session_epoch; reloading rotates session_epoch.
    let ok_close = rpc.call_raw("session.close", json!({ "session_id": s1_id, "expected_epoch": e1 }), |_| json!(null));
    assert!(ok_close.get("error").is_none(), "{ok_close}");
    let unloaded = rpc.call_raw("session.get", json!({ "session_id": s1_id }), |_| json!(null));
    assert_eq!(unloaded["result"]["loaded"], false, "{unloaded}");
    assert!(unloaded["result"].get("session_epoch").is_none(), "{unloaded}");

    let reloaded = rpc.call_raw("session.load", json!({ "session_id": s1_id }), |_| json!(null));
    assert!(reloaded.get("error").is_none(), "{reloaded}");
    let e2 = reloaded["result"]["session_epoch"].as_str().unwrap_or("").to_string();
    assert!(!e2.is_empty() && e2 != e1, "reload must rotate session_epoch (e1={e1}, e2={e2})");

    let old_epoch_upd =
        rpc.call_raw("session.update", json!({ "session_id": s1_id, "name": "stale", "expected_epoch": e1 }), |_| {
            json!(null)
        });
    assert_eq!(old_epoch_upd["error"]["data"]["code"], "stale_session", "{old_epoch_upd}");

    let new_epoch_upd =
        rpc.call_raw("session.update", json!({ "session_id": s1_id, "name": "fresh", "expected_epoch": e2 }), |_| {
            json!(null)
        });
    assert!(new_epoch_upd.get("error").is_none(), "{new_epoch_upd}");
    assert_eq!(new_epoch_upd["result"]["name"], "fresh", "{new_epoch_upd}");

    let _ = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    drop(rpc);
}

#[tokio::test]
async fn session_rewind_restore_files_picks_current_branch_latest_turn_and_aborts_on_conflict() {
    local(async {
        let env = TestEnv::new().await;
        let file_a = env.cwd.join("a.txt");
        let file_b = env.cwd.join("b.txt");
        std::fs::write(&file_a, "a-v0\n").unwrap();
        std::fs::write(&file_b, "b-v0\n").unwrap();

        let h = env
            .host()
            .builtins(&["changes"])
            .faux(json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "a-v1\n" } }] },
                { "text": "wrote a-v1" },
                { "tool_calls": [{ "name": "read", "arguments": { "path": "b.txt" } }] },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "b.txt", "content": "b-v1\n" } }] },
                { "text": "wrote b-v1" },
                { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "a-v2\n" } }] },
                { "text": "wrote a-v2" }
            ]))
            .build()
            .await;
        expose(&h);

        // Turn 1 (P1): writes a.txt -> "a-v1\n".
        let r1 = h.prompt("turn 1", json!({})).await.unwrap();
        assert_eq!(r1["text"], "wrote a-v1", "{r1}");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a-v1\n");

        // Rewind conversation ONLY (restore_files = false) back to before P1; a.txt stays "a-v1\n".
        let (sid, p1_id): (String, String) = eval(
            &h,
            r#"
            local sid = S:info().session_id
            local items = S:list_items().data
            local p1
            for _, it in ipairs(items) do
                if it.type == "user_message" then p1 = it.item_id break end
            end
            assert(p1, "missing P1 item")
            S:rewind({ session_id = sid, before_item_id = p1, restore_files = false })
            return sid, p1
            "#,
        )
        .await;
        assert!(!p1_id.is_empty());
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a-v1\n");

        // Turn 2 (P2) at the exact same `before` point: writes b.txt -> "b-v1\n", leaves a.txt at "a-v1\n".
        let r2 = h.prompt("turn 2", json!({})).await.unwrap();
        assert_eq!(r2["text"], "wrote b-v1", "{r2}");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b-v1\n");

        // Rewind before P2 with restore_files = true: MUST pick Turn 2 (not Turn 1!),
        // restoring b.txt to "b-v0\n" while preserving a.txt at "a-v1\n".
        let () = eval(
            &h,
            &format!(
                r#"
                local items = S:list_items().data
                local p2
                for _, it in ipairs(items) do
                    if it.type == "user_message" then p2 = it.item_id end
                end
                assert(p2, "missing P2 item")
                S:rewind({{ session_id = {:?}, before_item_id = p2, restore_files = true }})
                "#,
                sid
            ),
        )
        .await;
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b-v0\n", "Turn 2 file must be restored");
        assert_eq!(
            std::fs::read_to_string(&file_a).unwrap(),
            "a-v1\n",
            "Turn 1 file must NOT be overwritten when rewinding Turn 2"
        );

        // Turn 3 (P3): writes a.txt -> "a-v2\n", then an external edit creates a conflict.
        let r3 = h.prompt("turn 3", json!({})).await.unwrap();
        assert_eq!(r3["text"], "wrote a-v2", "{r3}");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a-v2\n");
        std::fs::write(&file_a, "external-edit\n").unwrap();

        let (err_code, has_conflict, item_count_before, item_count_after): (String, bool, usize, usize) = eval(
            &h,
            &format!(
                r#"
                local items = S:list_items().data
                local before_n = #items
                local p3
                for _, it in ipairs(items) do
                    if it.type == "user_message" then p3 = it.item_id end
                end
                assert(p3, "missing P3 item")
                local ok, err = pcall(S.rewind, S, {{ session_id = {:?}, before_item_id = p3, restore_files = true }})
                assert(not ok, "rewind with external file conflict must fail")
                local code = type(err) == "table" and err.code or tostring(err)
                local conflicts = type(err) == "table" and type(err.data) == "table" and err.data.conflicts or {{}}
                local after_n = #(S:list_items().data)
                return code, #conflicts > 0, before_n, after_n
                "#,
                sid
            ),
        )
        .await;
        assert_eq!(err_code, "session.file_conflict", "conflict must raise session.file_conflict");
        assert!(has_conflict, "error data must include conflicting file paths");
        assert_eq!(
            item_count_after, item_count_before,
            "conversation must NOT be rewound when file restoration hits a conflict"
        );
        assert_eq!(
            std::fs::read_to_string(&file_a).unwrap(),
            "external-edit\n",
            "conflicting file must not be overwritten"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn session_rewind_restore_files_fails_when_backup_blob_is_missing_and_preserves_leaf() {
    local(async {
        let env = TestEnv::new().await;
        let file_a = env.cwd.join("a.txt");
        let file_b = env.cwd.join("b.txt");
        std::fs::write(&file_a, "a-orig-unique-content\n").unwrap();
        std::fs::write(&file_b, "b-orig-unique-content\n").unwrap();

        let h = env
            .host()
            .builtins(&["changes"])
            .faux(json!([
                { "tool_calls": [
                    { "name": "read", "arguments": { "path": "a.txt" } },
                    { "name": "read", "arguments": { "path": "b.txt" } }
                ] },
                { "tool_calls": [
                    { "name": "write", "arguments": { "path": "a.txt", "content": "a-updated\n" } },
                    { "name": "write", "arguments": { "path": "b.txt", "content": "b-updated\n" } }
                ] },
                { "text": "updated both" }
            ]))
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("modify both files", json!({})).await.unwrap();
        assert_eq!(r1["text"], "updated both", "{r1}");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a-updated\n");
        assert_eq!(std::fs::read_to_string(&file_b).unwrap(), "b-updated\n");

        // Prune only a.txt's backup blob so rewind hits a partial restore (b.txt restored, a.txt missing).
        let session_path: String = eval(&h, "return assert(S:info().path)").await;
        let blobs_dir = std::path::PathBuf::from(format!("{session_path}.snapshots")).join("blobs");
        let mut pruned = 0usize;
        for blob in std::fs::read_dir(&blobs_dir).unwrap() {
            let p = blob.unwrap().path();
            if std::fs::read(&p).map(|b| b == b"a-orig-unique-content\n").unwrap_or(false) {
                std::fs::remove_file(&p).unwrap();
                pruned += 1;
            }
        }
        assert_eq!(pruned, 1, "expected to prune exactly a.txt's snapshot blob in {}", blobs_dir.display());

        let (err_code, missing_json, restored_json, leaf_before, leaf_after, count_before, count_after): (
            String,
            String,
            String,
            String,
            String,
            usize,
            usize,
        ) = eval(
            &h,
            r#"
            local sid = S:info().session_id
            local leaf_before = tostring(S:leaf())
            local items = S:list_items().data
            local p1
            for _, it in ipairs(items) do
                if it.type == "user_message" then p1 = it.item_id break end
            end
            assert(p1, "missing user item")
            local ok, err = pcall(S.rewind, S, { session_id = sid, before_item_id = p1, restore_files = true })
            assert(not ok, "rewind with missing backup blob must fail")
            local code = type(err) == "table" and err.code or tostring(err)
            local data = type(err) == "table" and type(err.data) == "table" and err.data or {}
            return code,
                mare.json.encode(data.missing or {}),
                mare.json.encode(data.restored or {}),
                leaf_before,
                tostring(S:leaf()),
                #items,
                #(S:list_items().data)
            "#,
        )
        .await;
        assert_eq!(err_code, "persist_failed", "missing backup blob must fail with persist_failed");
        assert!(missing_json.contains("a.txt"), "error data.missing must include a.txt: {missing_json}");
        assert!(restored_json.contains("b.txt"), "error data.restored must include b.txt: {restored_json}");
        assert_eq!(leaf_after, leaf_before, "conversation leaf must remain unchanged when blobs are missing");
        assert_eq!(count_after, count_before, "conversation items must remain unchanged when blobs are missing");
        assert_eq!(std::fs::read_to_string(&file_a).unwrap(), "a-updated\n", "missing file stays untouched");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn concurrent_session_update_with_same_expected_revision_rejects_second_writer() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "seeded" }])).build().await;
        expose(&h);
        let seed = h.prompt("seed persisted file", json!({})).await.unwrap();
        assert_eq!(seed["text"], "seeded");

        let (ok1, code1, ok2, code2, final_name, final_rev, path): (bool, String, bool, String, String, i64, String) =
            eval(
                &h,
                r#"
            local tx1, rx1 = mare.task.channel()
            local tx2, rx2 = mare.task.channel()
            local function run_rename(name, tx)
                local ok, res = pcall(S.update, S, name, { expected_revision = 0 })
                local code = (not ok and type(res) == "table" and res.code) or tostring(res)
                local rev = ok and type(res) == "table" and tonumber(res.name_revision or 0) or -1
                tx:send({ ok = ok, code = code, rev = rev })
            end
            C.cordis:spawn(function() run_rename("winner-one", tx1) end)
            C.cordis:spawn(function() run_rename("winner-two", tx2) end)
            local r1 = assert(rx1:recv())
            local r2 = assert(rx2:recv())
            local info = S:info()
            local snap = S:get(info.session_id)
            return r1.ok, tostring(r1.code), r2.ok, tostring(r2.code),
                tostring(info.name), tonumber(snap.name_revision or 0), tostring(info.path)
            "#,
            )
            .await;
        assert_ne!(
            ok1, ok2,
            "exactly one concurrent rename at expected_revision=0 must succeed: r1=({ok1},{code1}) r2=({ok2},{code2})"
        );
        let loser_code = if ok1 { &code2 } else { &code1 };
        let winner_name = if ok1 { "winner-one" } else { "winner-two" };
        let loser_name = if ok1 { "winner-two" } else { "winner-one" };
        assert_eq!(
            loser_code, "session.revision_conflict",
            "losing concurrent rename must fail with session.revision_conflict"
        );
        assert_eq!(final_name, winner_name);
        assert_eq!(final_rev, 1, "name_revision must advance exactly once");
        h.shutdown().await;
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(disk.contains(winner_name), "winning name must be persisted: {disk}");
        assert!(!disk.contains(loser_name), "rejected concurrent rename must not be written to disk: {disk}");
    })
    .await;
}

#[tokio::test]
async fn refresh_reconcile_abort_releases_waiting_sentinel_and_wakes_waiters() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let (waiter_done, waiter_val_nil, third_ok): (bool, bool, bool) = eval(
            &h,
            r#"
            local refresh_factory = require("session.refresh")
            local refresh = refresh_factory(C.cordis, function() end)
            local orig_reconcile = mare.store.index_reconcile
            local entered_tx, entered_rx = mare.task.channel()
            local block_tx, block_rx = mare.task.channel()
            local call_n = 0
            mare.store.index_reconcile = function()
                call_n = call_n + 1
                if call_n == 1 then
                    entered_tx:send(true)
                    block_rx:recv()
                    return { files = 0, updated = 0, removed = 0, pending = 0 }
                end
                return { files = 1, updated = 1, removed = 0, pending = 0 }
            end

            local cancel_tx, cancel_rx = mare.task.channel()
            local waiter_started_tx, waiter_started_rx = mare.task.channel()
            local waiter_tx, waiter_rx = mare.task.channel()
            local done1_tx, done1_rx = mare.task.channel()

            C.cordis:spawn(function()
                mare.task.select(
                    function() return refresh.reconcile() end,
                    function() return cancel_rx:recv() end
                )
                done1_tx:send(true)
            end)
            assert(mare.task.timeout(5000, function() return entered_rx:recv() end), "reconcile did not start")

            -- Second caller joins `waiting` while first reconcile is in-flight.
            C.cordis:spawn(function()
                waiter_started_tx:send(true)
                local v = refresh.reconcile()
                waiter_tx:send(v == nil)
            end)
            assert(mare.task.timeout(5000, function() return waiter_started_rx:recv() end), "waiter did not start")
            -- Yield so the waiter enters `waiting` and parks on `rx:recv()`, then abort caller 1 via select.
            mare.task.yield()
            mare.task.yield()
            cancel_tx:send(true)
            assert(mare.task.timeout(5000, function() return done1_rx:recv() end), "caller 1 did not exit")
            local w_val, w_err = mare.task.timeout(5000, function() return waiter_rx:recv() end)
            local r3 = refresh.reconcile()
            mare.store.index_reconcile = orig_reconcile
            return w_err ~= "timeout", w_val == true, type(r3) == "table" and r3.files == 1
            "#,
        )
        .await;
        assert!(waiter_done, "waiter must be woken when in-flight reconcile coroutine is aborted");
        assert!(waiter_val_nil, "woken waiter must receive nil on aborted reconcile");
        assert!(third_ok, "subsequent reconcile must succeed instead of hanging on stale waiting sentinel");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn children_and_pending_approvals_do_not_leak_effects_or_denied_entries() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["permissions"])
            .config(json!({
                "model": "faux/echo",
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only"
                    }
                }
            }))
            .build()
            .await;
        expose(&h);
        // 1. Repeated session:child + close must dispose both ctx and caller effects without poisoning.
        let (baseline_disposers, during_disposers, after_disposers): (usize, usize, usize) = eval(
            &h,
            r#"
            S:create()
            local function count_disposers()
                local n = 0
                local seen = {}
                local function add_fiber(f)
                    if type(f) ~= "table" or seen[f] then return end
                    seen[f] = true
                    if f._disposables and type(f._disposables.length) == "function" then
                        n = n + f._disposables:length()
                    end
                end
                add_fiber(C.cordis.fiber)
                local reg = C.cordis.registry
                if reg and type(reg.values) == "function" then
                    for _, rt in ipairs(reg:values()) do
                        if rt.fibers and type(rt.fibers.values) == "function" then
                            for _, f in ipairs(rt.fibers:values()) do
                                add_fiber(f)
                            end
                        end
                    end
                end
                return n
            end
            local h0 = S:child({ run_id = "run_warm", seq = 0, label = "warm" })
            h0.close()
            local base = count_disposers()
            local h1 = S:child({ run_id = "run_probe", seq = 1, label = "probe" })
            local during = count_disposers()
            h1.close()
            for i = 1, 8 do
                local ch = S:child({ run_id = "run_loop", seq = i, label = "child-" .. i })
                ch.close()
            end
            local after = count_disposers()
            return base, during, after
            "#,
        )
        .await;
        assert!(baseline_disposers > 0, "disposer count must observe real active cordis disposables");
        assert_eq!(
            during_disposers,
            baseline_disposers + 2,
            "open child handle must register exactly 2 effects (session ctx + caller)"
        );
        assert_eq!(
            after_disposers, baseline_disposers,
            "create_child + close must dispose both registered effects instead of leaking per child"
        );

        // 2. Denied/unresolved tool calls with a pending key must be evicted on turn/completed
        //    even when no turn-level grants were issued (so stale pending state cannot be reused).
        let (first_decision, first_reason, after_turn_decision): (String, String, String) = eval(
            &h,
            r#"
            local call = {
                id = "call-pending-leak-1",
                name = "demo_external_tool",
                arguments = { target = "x" },
                snapshot = 1,
                execution_id = "exec-pending-leak-1",
                attempt = 1,
            }
            local info = { side_effect = "external", unconfined = true }
            local v1 = C.cordis.policy:decide("tool/before", { call, info })
            -- Turn completes with NO turn-level grants; ordinary pending entry must be evicted.
            C.cordis:emit("turn/completed", { turn_id = "t-leak-1" })
            -- A late allow_session for that call must NOT find a stale pending entry and cache v.key.
            C.cordis:emit("pipeline/approval-decided", {
                call = call,
                decision = "allow_session",
                source = "user",
            })
            local v2 = C.cordis.policy:decide("tool/before", { call, info })
            return tostring(v1 and v1.decision), tostring(v1 and v1.reason or ""), tostring(v2 and v2.decision)
            "#,
        )
        .await;
        assert_eq!(first_decision, "ask", "external tool under default preset must ask (reason={first_reason})");
        assert_eq!(
            after_turn_decision, "ask",
            "pending entry must be evicted on turn/completed even without turn grants"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn rewind_confirm_aborts_if_additional_file_changes_while_dialog_is_open() {
    local(async {
        let env = TestEnv::new().await;
        let file_a = env.cwd.join("a.txt");
        let file_b = env.cwd.join("b.txt");
        std::fs::write(&file_a, "a-v0\n").unwrap();
        std::fs::write(&file_b, "b-v0\n").unwrap();

        let h = env
            .host()
            .builtins(&["changes", "commands"])
            .faux(json!([
                { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "a-v1\n" } }] },
                { "tool_calls": [{ "name": "read", "arguments": { "path": "b.txt" } }] },
                { "tool_calls": [{ "name": "write", "arguments": { "path": "b.txt", "content": "b-v1\n" } }] },
                { "text": "wrote both" }
            ]))
            .build()
            .await;
        expose(&h);
        fake_ui(&h).await;

        assert_eq!(h.prompt("edit both", json!({})).await.unwrap()["text"], "wrote both");
        // Modify a.txt externally so /rewind asks for conflict confirmation on a.txt ONLY.
        std::fs::write(&file_a, "a-ext\n").unwrap();

        let noticed_extra: bool = eval(
            &h,
            &format!(
                r#"
                local noticed = false
                local done_tx, done_rx = mare.task.channel()
                mare.ui.select = function()
                    return 1
                end
                mare.ui.confirm = function(_msg)
                    -- While the user is looking at the confirm dialog for a.txt, b.txt is modified externally.
                    mare.fs.write({:?}, "b-ext-during-confirm\n")
                    return true
                end
                mare.ui.notify = function(msg)
                    if tostring(msg):find("additional files changed outside mare while confirming", 1, true) then
                        noticed = true
                        done_tx:send(true)
                    end
                end
                C.cordis:get("commands"):run("/rewind")
                mare.task.timeout(5000, function() return done_rx:recv() end)
                return noticed
                "#,
                file_b.to_string_lossy()
            ),
        )
        .await;
        assert!(noticed_extra, "/rewind must abort when a new unconfirmed file changes during confirmation");
        assert_eq!(
            std::fs::read_to_string(&file_b).unwrap(),
            "b-ext-during-confirm\n",
            "newly modified file must not be overwritten by force=true"
        );
        h.shutdown().await;
    })
    .await;
}
