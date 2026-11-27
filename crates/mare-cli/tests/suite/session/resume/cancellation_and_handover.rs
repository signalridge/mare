use super::*;

#[tokio::test]
async fn cancelled_resume_keeps_the_old_session_writable_and_its_context() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let target = env.write_session("cancel-u1.jsonl", &[msg("m1", None, "user", "TARGET-HISTORY")]);
        let target_body = std::fs::read_to_string(&target).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&target, &target_body).unwrap();
        let h = env.host().faux(json!([{ "text": "ORIGINAL-REPLY" }, { "text": "AFTER-CANCEL-REPLY" }])).build().await;
        expose(&h);
        h.prompt("ORIGINAL-PROMPT", json!({})).await.unwrap();
        let (old_id, old_path, old_messages): (String, String, String) =
            eval(&h, "return S:info().session_id, S:info().path, mare.json.encode(A:list_messages().data)").await;
        global(&h, "TARGET", target.to_string_lossy().to_string());
        let finished: bool = eval(
            &h,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.select = function() mare.task.sleep(500); return 1 end
            local ok = mare.task.timeout(50, function() S:load({ path = TARGET }) end)
            return ok
        "#,
        )
        .await;
        assert!(!finished, "the load should time out before the cwd selection");
        let (after_id, after_path, messages): (String, String, String) =
            eval(&h, "return S:info().session_id, S:info().path, mare.json.encode(A:list_messages().data)").await;
        assert_eq!((after_id, after_path, messages), (old_id, old_path.clone(), old_messages));
        let reply = h.prompt("AFTER-CANCEL-PROMPT", json!({})).await.unwrap();
        assert_eq!(reply["text"], "AFTER-CANCEL-REPLY", "{reply}");
        h.shutdown().await;
        let old_log = std::fs::read_to_string(old_path).unwrap();
        assert!(
            old_log.contains("ORIGINAL-PROMPT")
                && old_log.contains("AFTER-CANCEL-PROMPT")
                && old_log.contains("AFTER-CANCEL-REPLY"),
            "{old_log}"
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), target_body);
    })
    .await
}

#[tokio::test]
async fn cancellation_closes_lua_scope_before_timeout_returns() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let (finished, closed): (bool, bool) = eval(
            &h,
            r#"
            CLOSED = false
            local finished = mare.task.timeout(30, function()
                local scope <close> = setmetatable({}, { __close = function() CLOSED = true end })
                mare.task.sleep(1000)
            end)
            return finished, CLOSED
        "#,
        )
        .await;
        assert!(!finished && closed, "timeout must synchronously close a pending scope");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_select_and_task_close_lua_scopes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let (selected, after_select, after_abort): (i64, bool, bool) = eval(&h, r#"
            CLOSED_SELECT, CLOSED_TASK = false, false
            local selected = mare.task.select(
                function() local s <close> = setmetatable({}, {__close=function() CLOSED_SELECT=true end}); mare.task.sleep(1000) end,
                function() mare.task.sleep(30); return true end)
            mare.task.sleep(30)
            local after_select = CLOSED_SELECT
            local task = mare.task.spawn(function()
                local s <close> = setmetatable({}, {__close=function() CLOSED_TASK=true end}); mare.task.sleep(1000)
            end)
            mare.task.sleep(30)
            task:abort()
            pcall(task.wait, task)
            return selected, after_select, CLOSED_TASK
        "#).await;
        assert_eq!(selected, 2);
        assert!(after_select && after_abort, "cancelled tasks must close scopes");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn prompt_during_interactive_resume_is_rejected_and_can_be_retried() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("transaction-probe.jsonl", &[msg("m1", None, "user", "old-history")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, body).unwrap();
        let h = env.host().faux(json!([{ "text": "DURING-LOAD-REPLY" }])).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(
            &h,
            r#"SELECT_WAITING = false
            mare.ui.connected = function() return true end
            mare.ui.select = function() SELECT_WAITING = true; mare.task.sleep(350); return 1 end"#,
        )
        .await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(
                &loader,
                r#"local ok, err = pcall(S.load, S, { path = TARGET }); return ok, tostring(err)"#,
            )
            .await
        });
        wait_until(5, "interactive session selection", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SELECT_WAITING").await }
        })
        .await;
        let reply = h.prompt("DURING-LOAD-PROMPT", json!({})).await.unwrap_err().to_string();
        let (other_switch, switch_error): (bool, String) =
            eval(&h, "local ok, err = pcall(S.create, S); return ok, tostring(err)").await;
        assert!(!other_switch && switch_error.contains("switch") && switch_error.contains("retry"), "{switch_error}");
        let (cont_ok, cont_err): (bool, String) =
            eval(&h, "local ok, err = pcall(A.continue, A); return ok, tostring(err)").await;
        let (loaded, why) = loading.await.unwrap();
        let active: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        let retried = h.prompt("AFTER-LOAD-PROMPT", json!({})).await.unwrap();
        h.shutdown().await;
        let text = std::fs::read_to_string(path).unwrap();
        assert!(loaded, "load failed: {why}");
        assert!(reply.contains("switch") && reply.contains("retry"), "{reply}");
        assert!(!cont_ok && cont_err.contains("switch") && cont_err.contains("retry"), "{cont_err}");
        assert!(!active.contains("DURING-LOAD-PROMPT") && !text.contains("DURING-LOAD-PROMPT"), "{active} / {text}");
        assert!(
            retried.get("error").is_none() && text.contains("AFTER-LOAD-PROMPT") && text.contains("DURING-LOAD-REPLY"),
            "{retried} / {text}"
        );
    })
    .await
}

#[tokio::test]
async fn old_notification_does_not_cross_resume_and_new_notification_wakes() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("wake-transaction.jsonl", &[msg("m1", None, "user", "old-history")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, body).unwrap();
        let h = env.host().faux(json!([{ "text": "WAKE-REPLY" }])).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(
            &h,
            r#"SELECT_WAITING = false
            mare.ui.connected = function() return true end
            mare.ui.select = function() SELECT_WAITING = true; mare.task.sleep(350); return 1 end"#,
        )
        .await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<bool>(&loader, "local ok = pcall(S.load, S, { path = TARGET }); return ok").await
        });
        wait_until(5, "interactive session selection", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SELECT_WAITING").await }
        })
        .await;
        exec(&h, r#"A:notify("DEFERRED-NOTE", {wake=true})"#).await;
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let busy: bool = eval(&h, "return A:get().busy").await;
        assert!(!busy, "notification must not start a turn during a switch");
        assert!(loading.await.unwrap());
        let restored: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(!restored.contains("DEFERRED-NOTE"), "old-session notification leaked on resume: {restored}");
        exec(&h, r#"A:notify("NEW-SESSION-NOTE", {wake=true})"#).await;
        wait_until(5, "new-session notification", || {
            let h = h.clone();
            async move { eval::<String>(&h, "return mare.json.encode(A:list_messages().data)").await.contains("WAKE-REPLY") }
        })
        .await;
        h.shutdown().await;
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("NEW-SESSION-NOTE") && text.contains("WAKE-REPLY"), "{text}");
        assert!(!text.contains("DEFERRED-NOTE"), "old-session note was persisted in the resumed session: {text}");
    })
    .await
}

#[tokio::test]
async fn cancelling_during_target_read_releases_its_handle_and_keeps_old_writer() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("target-read.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "RETRY" }])).build().await;
        expose(&h);
        h.prompt("OLD", json!({})).await.unwrap();
        let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (finished, switching, opened, err): (bool, bool, bool, String) = eval(
            &h,
            r#"
            local original = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function(...) mare.task.sleep(500); return original(...) end
            local finished = mare.task.timeout(40, function() S:load({ path = TARGET }) end)
            mare.store.read_tail_snapshot = original
            local ok, f = pcall(mare.store.open, TARGET, {existing=true, defer_activation=true})
            if ok then f:close() end
            return finished, S:switching(), ok, tostring(f)
        "#,
        )
        .await;
        assert!(!finished && !switching && opened, "finished={finished} switching={switching} opened={opened} {err}");
        let (id, current): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        assert_eq!((id, current), (old_id, old_path.clone()));
        assert_eq!(h.prompt("RETRY-PROMPT", json!({})).await.unwrap()["text"], "RETRY");
        h.shutdown().await;
        assert!(std::fs::read_to_string(old_path).unwrap().contains("RETRY-PROMPT"));
    })
    .await
}

#[tokio::test]
async fn cancellation_after_publication_finishes_events_and_keeps_new_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("committed.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let h = env.host().faux(json!([{ "text": "AFTER-COMMIT" }])).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            CHANGED_COUNT, LOADED_COUNT, CHANGED_WAITING = 0, 0, false
            CHANGED_TX, CHANGED_RX = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            C.cordis:on("session/changed", function(e)
                if e.reason == "resume" then
                    CHANGED_COUNT = CHANGED_COUNT + 1
                    CHANGED_WAITING = true
                    CHANGED_TX:send(true)
                    RELEASE_RX:recv()
                end
            end)
            C.cordis:on("session/loaded", function() LOADED_COUNT = LOADED_COUNT + 1 end)
        "#,
        )
        .await;
        let (finished, waiting, switching, active): (bool, bool, bool, String) = eval(
            &h,
            r#"
            local load = mare.task.spawn(function() S:load({ path = TARGET }) end)
            local ready = mare.task.timeout(2000, function() CHANGED_RX:recv() end)
            if not ready then
                RELEASE_TX:send(true)
                load:abort()
                pcall(load.wait, load)
                return false, CHANGED_WAITING, S:switching(), S:info().path
            end
            local finished = load:done()
            load:abort()
            pcall(load.wait, load)
            local waiting, switching, active = CHANGED_WAITING, S:switching(), S:info().path
            RELEASE_TX:send(true)
            return finished, waiting, switching, active
        "#,
        )
        .await;
        assert!(!finished && waiting && switching, "finished={finished} waiting={waiting} switching={switching}");
        assert_eq!(active, path.to_string_lossy());
        let blocked = h.prompt("TOO-EARLY", json!({})).await.unwrap_err().to_string();
        assert!(blocked.contains("switch"), "{blocked}");
        wait_until(5, "post-commit notifications", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return not S:switching() and LOADED_COUNT == 1").await }
        })
        .await;
        let (changed, loaded): (i64, i64) = eval(&h, "return CHANGED_COUNT, LOADED_COUNT").await;
        assert_eq!((changed, loaded), (1, 1));
        assert_eq!(h.prompt("AFTER", json!({})).await.unwrap()["text"], "AFTER-COMMIT");
        h.shutdown().await;
        let log = std::fs::read_to_string(path).unwrap();
        assert!(log.contains("AFTER") && !log.contains("TOO-EARLY"), "{log}");
    })
    .await
}

#[tokio::test]
async fn locked_sources_offer_fork_and_damaged_sources_hide_it() {
    local(async {
        for (locked, damaged) in [(true, false), (false, true), (true, true)] {
            let env = TestEnv::new().await;
            let path = env.write_durable_session(
                "fork-source.jsonl",
                &[
                    msg("m1", None, "user", "INTACT-PREFIX"),
                    msg("m2", Some("m1"), "assistant", "LATER-RECORD"),
                    run_end("m2"),
                ],
            );
            let mut original = std::fs::read_to_string(&path).unwrap();
            if damaged {
                let offset = original.lines().take(2).map(|line| line.len() + 1).sum();
                original.insert_str(offset, "invalid json\n");
                std::fs::write(&path, &original).unwrap();
            }
            let lock = if locked {
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(mare_store::lock_path(&path))
                    .unwrap();
                f.try_lock().unwrap();
                Some(f)
            } else {
                None
            };
            let h = env.host().build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "DAMAGED", damaged);
            let (ok, err, fork_path, damaged_path, items, readonly, fork_ok, fork_err): (
                bool,
                String,
                String,
                String,
                String,
                bool,
                bool,
                String,
            ) = eval(
                &h,
                r#"
                DAMAGED_PATH = ''
                C.cordis:on('session/damaged', function(e) DAMAGED_PATH = e.path or '' end)
                mare.ui.select = function(opts)
                    MENU_ITEMS = {}
                    for _, item in ipairs(opts.items) do MENU_ITEMS[#MENU_ITEMS + 1] = item.label end
                    return 1
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                local fork_ok, fork_err = true, ''
                if DAMAGED then fork_ok, fork_err = pcall(S.fork, S, { session_id = S:info().session_id }) end
                local info = S:info()
                return ok, tostring(err), info.path or '', DAMAGED_PATH,
                    table.concat(MENU_ITEMS, ', '), info.readonly == true, fork_ok, tostring(fork_err)
            "#,
            )
            .await;
            if damaged {
                assert!(ok, "{err}");
                assert_eq!(items, "View read-only");
                assert!(readonly);
                assert_eq!(fork_path, path.to_string_lossy());
                assert!(!fork_ok && fork_err.contains("damaged"), "{fork_err}");
                assert_eq!(damaged_path, path.to_string_lossy());
            } else {
                assert!(ok, "{err}");
                assert_eq!(items, "Fork into a new session, View read-only");
                assert!(!readonly);
                assert_ne!(fork_path, path.to_string_lossy());
                assert!(fork_ok, "{fork_err}");
                assert!(damaged_path.is_empty());
            }
            h.shutdown().await;
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            drop(lock);
        }
    })
    .await
}

#[tokio::test]
async fn yielding_agent_publication_is_refused_before_session_handover() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("yield-agent.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let h = env.host().faux(json!([{ "text": "OLD" }, { "text": "AFTER" }])).build().await;
        expose(&h);
        h.prompt("OLD-PROMPT", json!({})).await.unwrap();
        let (id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, still_id, switching): (bool, String, String, bool) = eval(
            &h,
            r#"
            local original = A.adopt_session
            A.adopt_session = function() mare.task.sleep(1000) end
            local ok, err = pcall(S.load, S, { path = TARGET })
            A.adopt_session = original
            return ok, tostring(err), S:info().session_id, S:switching()
        "#,
        )
        .await;
        assert!(!ok && err.contains("yielded") && switching, "{err}");
        assert_eq!(still_id, id);
        assert!(h.prompt("NEXT", json!({})).await.is_err());
        h.shutdown().await;
        assert!(!std::fs::read_to_string(old_path).unwrap().contains("NEXT"));
    })
    .await
}

#[tokio::test]
async fn request_recovery_failure_still_delivers_commit_events() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "recover-failure.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"agent/request", "data":{"request_id":"r1", "attempt":0}}),
            ],
        );
        let h = env.host().build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, active, switching, changed, loaded): (bool, String, String, bool, i64, i64) = eval(
            &h,
            r#"
            CHANGED, LOADED = 0, 0
            C.cordis:on("session/changed", function(e) if e.reason == "resume" then CHANGED = CHANGED + 1 end end)
            C.cordis:on("session/loaded", function() LOADED = LOADED + 1 end)
            local original = A.set_pending_request
            A.set_pending_request = function() error("injected recovery failure") end
            local ok, err = pcall(S.load, S, { path = TARGET })
            A.set_pending_request = original
            return ok, tostring(err), S:info().path, S:switching(), CHANGED, LOADED
        "#,
        )
        .await;
        assert!(!ok && err.contains("session committed") && err.contains("injected recovery failure"), "{err}");
        assert_eq!(active, path.to_string_lossy());
        assert!(switching, "recovery failure keeps the host blocked");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        assert_eq!((changed, loaded), (1, 1));
        h.shutdown().await;
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn failed_request_recovery_journal_write_poison_barrier_blocks_turns() {
    local(async {
        for method in ["record_request_aborted", "record_request"] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "recover-write-failure.jsonl",
                &[
                    msg("m1", None, "user", "TARGET"),
                    json!({"type":"custom", "kind":"agent/request", "data":{"request_id":"r1", "attempt":0}}),
                ],
            );
            let h = env.host().build().await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "FAIL_METHOD", method);
            let fail = h
                .lua()
                .create_function(|lua, ()| {
                    mare_binding::exec_gate::fail_session_journal_ops(lua, 0, true, false);
                    Ok(())
                })
                .unwrap();
            global(&h, "FAIL_NEXT_APPEND", fail);
            let (ok, error, switching, changed, loaded): (bool, String, bool, i64, i64) = eval(
                &h,
                r#"
                local changed, loaded = 0, 0
                C.cordis:on('session/changed', function(e) if e.reason == 'resume' then changed = changed + 1 end end)
                C.cordis:on('session/loaded', function() loaded = loaded + 1 end)
                local orig = S[FAIL_METHOD]
                S[FAIL_METHOD] = function(self, ev)
                    S[FAIL_METHOD] = orig
                    FAIL_NEXT_APPEND()
                    return orig(self, ev)
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                return ok, tostring(err), S:switching(), changed, loaded
            "#,
            )
            .await;
            assert!(!ok && error.contains("injected journal append failure") && switching, "{method}: {error}");
            assert_eq!((changed, loaded), (1, 1));
            assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
            let after = std::fs::read(&path).unwrap();
            h.shutdown().await;
            assert_eq!(std::fs::read(&path).unwrap(), after);
        }
    })
    .await
}

#[tokio::test]
async fn cancelling_resume_picker_or_directory_choice_keeps_current_session() {
    local(async {
        for locked in [false, true] {
            let env = TestEnv::new().await;
            let elsewhere = tempfile::tempdir().unwrap();
            let other = dunce::canonicalize(elsewhere.path()).unwrap();
            let path = env.write_session("dismissed.jsonl", &[msg("m1", None, "user", "TARGET")]);
            let body = std::fs::read_to_string(&path).unwrap().replacen(
                &serde_json::to_string(&env.cwd).unwrap(),
                &serde_json::to_string(&other).unwrap(),
                1,
            );
            std::fs::write(&path, &body).unwrap();
            let lock = if locked {
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .write(true)
                    .open(mare_store::lock_path(&path))
                    .unwrap();
                f.try_lock().unwrap();
                Some(f)
            } else {
                None
            };
            let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "CONTINUED" }])).build().await;
            expose(&h);
            h.prompt("BEFORE", json!({})).await.unwrap();
            let (id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let (ok, err, after, switching): (bool, String, String, bool) = eval(
                &h,
                r#"
                mare.ui.connected = function() return true end
                mare.ui.select = function() return nil end
                local ok, err = pcall(S.load, S, { path = TARGET })
                return ok, tostring(err), S:info().session_id, S:switching()
            "#,
            )
            .await;
            assert!(!ok && err.contains("cancelled") && !switching, "locked={locked}: {err}");
            assert_eq!(after, id);
            assert_eq!(h.prompt("AFTER", json!({})).await.unwrap()["text"], "CONTINUED");
            h.shutdown().await;
            assert!(std::fs::read_to_string(old_path).unwrap().contains("AFTER"));
            assert_eq!(std::fs::read_to_string(path).unwrap(), body);
            drop(lock);
        }
    })
    .await
}

#[tokio::test]
async fn select_cancellation_releases_target_before_returning() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("select-target.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let h = env.host().build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (selected, switching, opened, err): (i64, bool, bool, String) = eval(
            &h,
            r#"
            local read = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function(...) mare.task.sleep(500); return read(...) end
            local selected = mare.task.select(
                function() S:load({ path = TARGET }) end,
                function() mare.task.sleep(40); return true end)
            mare.store.read_tail_snapshot = read
            local ok, file = pcall(mare.store.open, TARGET, {existing=true, defer_activation=true})
            if ok then file:close() end
            return selected, S:switching(), ok, tostring(file)
        "#,
        )
        .await;
        assert_eq!(selected, 2);
        assert!(!switching && opened, "switching={switching}, opened={opened}, {err}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelling_an_outer_select_cannot_leave_a_background_session_switch() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("nested-select.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, &body).unwrap();
        let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "AFTER" }])).build().await;
        expose(&h);
        h.prompt("BEFORE", json!({})).await.unwrap();
        let (id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (finished, after, switching): (bool, String, bool) = eval(
            &h,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.select = function() mare.task.sleep(180); return 1 end
            local finished = mare.task.timeout(40, function()
                return mare.task.select(function() return S:load({ path = TARGET }) end, function() mare.task.sleep(1000) end)
            end)
            mare.task.sleep(250)
            return finished, S:info().session_id, S:switching()
        "#,
        )
        .await;
        assert!(!finished && !switching);
        assert_eq!(after, id, "cancelled outer select must not commit later");
        assert_eq!(h.prompt("AFTER", json!({})).await.unwrap()["text"], "AFTER");
        h.shutdown().await;
        assert!(std::fs::read_to_string(old_path).unwrap().contains("AFTER"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), body);
    })
    .await
}

#[tokio::test]
async fn malformed_queued_inputs_cannot_partially_adopt_a_session() {
    for queue in [
        json!({"steer":42}),
        json!({"follow_up":["legacy text"]}),
        json!({"follow_up":[{"text":"missing identity"}]}),
        json!({"follow_up":[{"input_id":"same","text":"one"},{"input_id":"same","text":"two"}]}),
    ] {
        local(async {
            let env = TestEnv::new().await;
            let target = env.write_session(
                "bad-queue.jsonl",
                &[
                    msg("m1", None, "user", "TARGET"),
                    json!({"type":"custom", "kind":"agent/queue", "provenance":"session-host", "data":queue}),
                ],
            );
            let body = std::fs::read_to_string(&target).unwrap();
            let h = env.host().faux(json!([{ "text": "OLD" }, { "text": "AFTER" }])).build().await;
            expose(&h);
            h.prompt("ORIGINAL", json!({})).await.unwrap();
            let (id, before): (String, String) =
                eval(&h, "return S:info().session_id, mare.json.encode(A:list_messages().data)").await;
            global(&h, "TARGET", target.to_string_lossy().to_string());
            let (ok, err, after_id, after): (bool, String, String, String) = eval(
                &h,
                r#"
            local ok, e = pcall(S.load, S, { path = TARGET })
            return ok, tostring(e), S:info().session_id, mare.json.encode(A:list_messages().data)
        "#,
            )
            .await;
            assert!(!ok, "malformed queue unexpectedly accepted: {err}");
            assert_eq!((after_id, after), (id, before), "failed adoption changed live state: {err}");
            assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "AFTER");
            h.shutdown().await;
            assert_eq!(std::fs::read_to_string(target).unwrap(), body);
        })
        .await;
    }
}

#[tokio::test]
async fn cancelled_resume_does_not_print_preparation_warnings() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("warning-cancel.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            "\"/definitely/not/a/mare/directory\"",
            1,
        );
        std::fs::write(&path, body).unwrap();
        let h = env.host().build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (finished, printed): (bool, String) = eval(
            &h,
            r#"
            local original_tail, original_read, stderr = mare.store.read_tail_snapshot, mare.store.read_snapshot, io.stderr
            local printed = ''
            io.stderr = { write = function(_, text) printed = printed .. text end }
            mare.store.read_tail_snapshot = function(p)
                local tail = original_tail(p)
                tail.partial = true
                tail.entries = {{type='message', id='leaf', parent='missing', message={role='user',content={}}}}
                return tail
            end
            mare.store.read_snapshot = function(...) mare.task.sleep(500); return original_read(...) end
            local finished = mare.task.timeout(50, function() S:load({ path = TARGET }) end)
            io.stderr, mare.store.read_snapshot, mare.store.read_tail_snapshot = stderr, original_read, original_tail
            return finished, printed
        "#,
        )
        .await;
        assert!(!finished, "load should be cancelled while reading full history");
        assert!(printed.is_empty(), "preparation warning escaped from a cancelled switch: {printed}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn locked_fork_keeps_the_selected_working_directory() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let target = env.write_session(
            "locked-other-cwd.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"working-directory/changed", "data":{"to":other}}),
                msg("m2", Some("m1"), "assistant", "DONE"),
                run_end("m2"),
            ],
        );
        let body = std::fs::read_to_string(&target).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&target, &body).unwrap();
        drop(mare_store::SessionFile::open_existing(&target, None).unwrap());
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&target))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", target.to_string_lossy().to_string());
        let (ok, error, fork_path, loaded_cwd): (bool, String, String, String) = eval(
            &h,
            r#"
            LOADED_CWD = ''
            C.cordis:on('session/loaded', function(e) LOADED_CWD = e.shell_cwd or '' end)
            mare.ui.select = function() return 1 end
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:info().path or '', LOADED_CWD
        "#,
        )
        .await;
        assert!(ok, "{error}");
        assert_ne!(fork_path, target.to_string_lossy());
        assert_eq!(loaded_cwd, other.to_string_lossy());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), body);
        drop(lock);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_rejected_during_active_session_switch_emits_ui_notice() {
    local(async {
        let env = TestEnv::new().await;
        let target_a = env.write_session("switch-a.jsonl", &[msg("a1", None, "user", "SESSION-A")]);
        let target_b = env.write_session("switch-b.jsonl", &[msg("b1", None, "user", "SESSION-B")]);
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "TARGET_A", target_a.to_string_lossy().to_string());
        global(&h, "TARGET_B", target_b.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            CHANGED_TX, CHANGED_RX = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            LOADED_COUNT = 0
            C.cordis:on("session/changed", function(e)
                if e.reason == "resume" then
                    CHANGED_TX:send(true)
                    RELEASE_RX:recv()
                end
            end)
            C.cordis:on("session/loaded", function()
                LOADED_COUNT = LOADED_COUNT + 1
            end)
            LOAD_TASK = mare.task.spawn(function()
                S:load({ path = TARGET_A })
            end)
            assert(mare.task.timeout(2000, function() return CHANGED_RX:recv() end), "switch did not reach barrier")
            assert(S:switching(), "expected session switch to be active")
        "#,
        )
        .await;

        exec(
            &h,
            r#"
            N = {}
            CM:run("/resume " .. TARGET_B)
            mare.task.yield()
        "#,
        )
        .await;
        let arg_notices = notices(&h).await;
        assert!(
            arg_notices.to_lowercase().contains("switch"),
            "expected visible UI notice for `/resume <path>` during switch, got: {arg_notices:?}"
        );

        exec(
            &h,
            r#"
            N = {}
            CM:run("/resume")
            mare.task.yield()
        "#,
        )
        .await;
        let picker_notices = notices(&h).await;
        assert!(
            picker_notices.to_lowercase().contains("switch"),
            "expected visible UI notice for `/resume` during switch, got: {picker_notices:?}"
        );

        exec(&h, "RELEASE_TX:send(true); LOAD_TASK:wait()").await;
        wait_until(WAIT_SECS, "first switch completes", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return not S:switching() and LOADED_COUNT == 1").await }
        })
        .await;
        let active: String = eval(&h, "return S:info().path").await;
        assert_eq!(active, target_a.to_string_lossy());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn startup_picker_load_failure_aborts_startup_without_naming_wrong_session() {
    local(async {
        let env = TestEnv::new().await;
        let target = env.write_session("startup-target.jsonl", &[msg("m1", None, "user", "TARGET-HISTORY")]);
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let target_escaped = serde_json::to_string(&target.to_string_lossy().to_string()).unwrap();
        std::fs::write(
            plugins_dir.join("startup_picker_fail.lua"),
            format!(
                r#"
return {{
    name = "startup-picker-fail-probe",
    apply = function(ctx)
        mare.g.STARTUP_NOTICES = {{}}
        mare.g.SESSION_START_EVENTS = {{}}
        mare.ui.connected = function() return true end
        mare.ui.notify = function(msg)
            local t = mare.g.STARTUP_NOTICES or {{}}
            t[#t + 1] = tostring(msg)
            mare.g.STARTUP_NOTICES = t
        end
        mare.ui.select = function(opts)
            if opts and opts.title == "Resume session" then
                os.remove({target_escaped})
                return 2
            end
            return nil
        end
        ctx:on("session/started", function(e)
            mare.g.SESSION_START_EVENTS[#mare.g.SESSION_START_EVENTS + 1] = e
        end)
    end,
}}
"#
            ),
        )
        .unwrap();
        let h = env
            .host()
            .user_config(true)
            .faux(json!([]))
            .args(json!({ "resume": "", "fork_session": true, "name": "SHOULD-NOT-APPLY" }))
            .build()
            .await;
        expose(&h);
        let (name, resume_error, start_count, resumed_start_count, notices_text): (
            Option<String>,
            Option<String>,
            i64,
            i64,
            String,
        ) = eval(
            &h,
            r#"
                local info = S:info()
                local starts = #(mare.g.SESSION_START_EVENTS or {})
                local resumed_starts = 0
                for _, e in ipairs(mare.g.SESSION_START_EVENTS or {}) do
                    if e.resumed then
                        resumed_starts = resumed_starts + 1
                    end
                end
                return info.name, info.resume_error, starts, resumed_starts, table.concat(mare.g.STARTUP_NOTICES or {}, "\n")
            "#,
        )
        .await;
        assert_eq!(name, None, "failed startup picker load must not name the unresumed session");
        assert!(
            resume_error.as_deref().is_some_and(|e| !e.is_empty()),
            "expected startup resume_error when picker load fails, got: {resume_error:?}"
        );
        assert_eq!(resumed_start_count, 0, "must not emit resumed session/start on failure (starts={start_count})");
        assert!(
            !notices_text.is_empty(),
            "expected visible UI notice on startup picker failure (resume_error={resume_error:?})"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn startup_picker_post_publish_failure_aborts_without_resumed_start() {
    local(async {
        let env = TestEnv::new().await;
        let _target = env.write_session(
            "startup-post-publish.jsonl",
            &[
                json!({
                    "type": "custom",
                    "entry_id": "m2",
                    "parent_entry_id": null,
                    "kind": "sandbox/mode",
                    "scope": "session",
                    "provenance": "session-host",
                    "data": { "level": "read-only" }
                }),
                msg("m1", Some("m2"), "user", "TARGET-HISTORY"),
            ],
        );
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("startup_post_publish_fail.lua"),
            r#"
return {
    name = "startup-post-publish-fail-probe",
    apply = function(ctx)
        mare.g.STARTUP_NOTICES = {}
        mare.g.SESSION_START_EVENTS = {}
        mare.ui.connected = function() return true end
        mare.ui.notify = function(msg)
            local t = mare.g.STARTUP_NOTICES or {}
            t[#t + 1] = tostring(msg)
            mare.g.STARTUP_NOTICES = t
        end
        mare.ui.select = function(opts)
            if opts and opts.title == "Resume session" then
                return 2
            end
            return nil
        end
        local orig_set = mare.proc.sandbox.set_current
        mare.proc.sandbox.set_current = function(spec)
            orig_set(spec)
            error("injected post-publish failure", 0)
        end
        ctx:on("session/started", function(e)
            local t = mare.g.SESSION_START_EVENTS or {}
            t[#t + 1] = e
            mare.g.SESSION_START_EVENTS = t
        end)
    end,
}
"#,
        )
        .unwrap();
        let h = env
            .host()
            .user_config(true)
            .faux(json!([]))
            .args(json!({ "resume": "", "name": "SHOULD-NOT-APPLY" }))
            .build()
            .await;
        expose(&h);
        let (name, resume_error, start_count, resumed_start_count, notices_text): (
            Option<String>,
            Option<String>,
            i64,
            i64,
            String,
        ) = eval(
            &h,
            r#"
                local info = S:info()
                local starts = #(mare.g.SESSION_START_EVENTS or {})
                local resumed_starts = 0
                for _, e in ipairs(mare.g.SESSION_START_EVENTS or {}) do
                    if e.resumed then
                        resumed_starts = resumed_starts + 1
                    end
                end
                return info.name, info.resume_error, starts, resumed_starts, table.concat(mare.g.STARTUP_NOTICES or {}, "\n")
            "#,
        )
        .await;
        assert_eq!(name, None, "post-publish startup picker failure must not apply --name");
        assert!(
            resume_error.as_deref().is_some_and(|e| e.contains("injected post-publish failure")),
            "expected startup resume_error on post-publish failure, got: {resume_error:?}"
        );
        assert_eq!(resumed_start_count, 0, "must not emit resumed session/start on post-publish failure (starts={start_count})");
        assert_eq!(start_count, 0, "must not emit session/start when startup failed");
        assert!(!notices_text.is_empty(), "expected visible UI notice on post-publish failure");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_late_switch_race_after_entry_check_emits_ui_notice() {
    local(async {
        let env = TestEnv::new().await;
        let target_a = env.write_session("race-a.jsonl", &[msg("a1", None, "user", "RACE-A")]);
        let target_b = env.write_session("race-b.jsonl", &[msg("b1", None, "user", "RACE-B")]);
        std::fs::write(
            &target_a,
            std::fs::read_to_string(&target_a).unwrap().replacen("\"session_id\":\"s1\"", "\"session_id\":\"race-a\"", 1),
        )
        .unwrap();
        std::fs::write(
            &target_b,
            std::fs::read_to_string(&target_b).unwrap().replacen("\"session_id\":\"s1\"", "\"session_id\":\"race-b\"", 1),
        )
        .unwrap();
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "TARGET_A", target_a.to_string_lossy().to_string());
        global(&h, "TARGET_B", target_b.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            A_CHANGED_TX, A_CHANGED_RX = mare.task.channel()
            A_RELEASE_TX, A_RELEASE_RX = mare.task.channel()
            B_GATE_TX, B_GATE_RX = mare.task.channel()
            B_RELEASE_TX, B_RELEASE_RX = mare.task.channel()
            HOLD_B_GATE = false
            C.cordis:get("policy"):voter("session/before-switch", function(e)
                if HOLD_B_GATE and e and type(e.target_path) == "string" and e.target_path:find("race%-b%.jsonl$") then
                    HOLD_B_GATE = false
                    B_GATE_TX:send(true)
                    B_RELEASE_RX:recv()
                end
                return { decision = "allow" }
            end)
            C.cordis:on("session/changed", function(e)
                local p = S:info().path or ""
                if e.reason == "resume" and p:find("race%-a%.jsonl$") then
                    A_CHANGED_TX:send(true)
                    A_RELEASE_RX:recv()
                end
            end)
        "#,
        )
        .await;

        // 1. Direct `/resume <path>` race: passes `refuse_busy`, pauses before `begin_switch`,
        //    then `TARGET_A` enters `begin_switch` before `TARGET_B` resumes.
        exec(
            &h,
            r#"
            N = {}
            HOLD_B_GATE = true
            CM:run("/resume " .. TARGET_B)
            assert(mare.task.timeout(2000, function() return B_GATE_RX:recv() end), "direct /resume did not reach gate")
            LOAD_A = mare.task.spawn(function() S:load({ path = TARGET_A }) end)
            assert(mare.task.timeout(2000, function() return A_CHANGED_RX:recv() end), "TARGET_A did not enter switch")
            B_RELEASE_TX:send(true)
        "#,
        )
        .await;
        wait_until(WAIT_SECS, "direct /resume late-race notice", || {
            let h = h.clone();
            async move { notices(&h).await.to_lowercase().contains("switch") }
        })
        .await;
        exec(&h, "A_RELEASE_TX:send(true); LOAD_A:wait()").await;

        // 2. Picker `/resume` race: `after_pick` passes `refuse_busy`, pauses before `begin_switch`,
        //    then `TARGET_A` (via another session) or `S:load` enters `begin_switch`.
        exec(
            &h,
            r#"
            N = {}
            HOLD_B_GATE = true
            SELECT_DBG = "not-called"
            mare.ui.select = function(opts)
                SELECT_DBG = "title=" .. tostring(opts and opts.title)
                if opts and opts.title == "Resume session" then
                    local items = (opts.tabs and opts.tabs[1] and opts.tabs[1].items) or opts.items or {}
                    SELECT_DBG = SELECT_DBG .. " count=" .. tostring(#items)
                    for idx, item in ipairs(items) do
                        local enc = mare.json.encode(item)
                        if enc:find("RACE%-B") then
                            SELECT_DBG = SELECT_DBG .. " picked=" .. tostring(idx)
                            return idx
                        end
                    end
                    SELECT_DBG = SELECT_DBG .. " no-match"
                end
                return nil
            end
            CM:run("/resume")
            assert(mare.task.timeout(2000, function() return B_GATE_RX:recv() end), "picker /resume did not reach gate: " .. SELECT_DBG .. " N=" .. table.concat(N, ";") .. " sw=" .. tostring(S:switching()))
            LOAD_A2 = mare.task.spawn(function() S:load({ path = TARGET_A }) end)
            assert(mare.task.timeout(2000, function() return A_CHANGED_RX:recv() end), "TARGET_A did not enter switch (2)")
            B_RELEASE_TX:send(true)
        "#,
        )
        .await;
        wait_until(WAIT_SECS, "picker /resume late-race notice", || {
            let h = h.clone();
            async move { notices(&h).await.to_lowercase().contains("switch") }
        })
        .await;
        exec(&h, "A_RELEASE_TX:send(true); LOAD_A2:wait()").await;
        h.shutdown().await;
    })
    .await
}

#[test]
fn cli_continue_and_resume_with_fork_session_succeed_without_resume_error() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    let faux = home.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "orig answer" }, { "text": "after" }]).to_string()).unwrap();

    let rpc_call = |child_stdout: &mut BufReader<std::process::ChildStdout>,
                    child_stdin: &mut std::process::ChildStdin,
                    id: i64,
                    method: &str,
                    params: Value|
     -> Value {
        let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        writeln!(child_stdin, "{req}").unwrap();
        child_stdin.flush().unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            let n = child_stdout.read_line(&mut line).unwrap();
            assert!(n > 0, "unexpected EOF waiting for {method}");
            let v: Value = serde_json::from_str(&line).unwrap();
            if v.get("id").and_then(Value::as_i64) == Some(id) {
                return v;
            }
        }
    };

    // 1. Seed a persisted session via normal RPC startup.
    let mut seed = mare_cmd()
        .args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .current_dir(&work)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut s_out = BufReader::new(seed.stdout.take().unwrap());
    let mut s_in = seed.stdin.take().unwrap();
    let s_init =
        rpc_call(&mut s_out, &mut s_in, 1, "initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
    assert!(s_init.get("error").is_none());
    let orig_sid = s_init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let seed_start =
        rpc_call(&mut s_out, &mut s_in, 3, "agent.prompt", json!({ "session_id": orig_sid, "input": "hello seed" }));
    assert!(seed_start.get("error").is_none(), "{seed_start}");
    assert!(
        rpc_call(
            &mut s_out,
            &mut s_in,
            4,
            "agent.wait",
            json!({ "session_id": orig_sid, "turn_id": seed_start["result"]["turn_id"] })
        )
        .get("error")
        .is_none()
    );
    let _ = rpc_call(&mut s_out, &mut s_in, 5, "shutdown", json!({}));
    drop(s_in);
    assert!(seed.wait().unwrap().success());

    // 2. Launch CLI with `-c --fork-session` and `--resume <orig_sid> --fork-session`.
    for (label, extra_args) in [
        ("continue", vec!["--rpc", "-c", "--fork-session", "--yolo", "--model", "faux/echo"]),
        ("resume", vec!["--rpc", "--resume", orig_sid.as_str(), "--fork-session", "--yolo", "--model", "faux/echo"]),
    ] {
        let mut child = mare_cmd()
            .args(&extra_args)
            .env("MARE_HOME", home.path())
            .env("MARE_FAUX_SCRIPT", &faux)
            .current_dir(&work)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(child.stdout.take().unwrap());
        let mut stdin = child.stdin.take().unwrap();
        let init_reply =
            rpc_call(&mut out, &mut stdin, 1, "initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));
        assert!(init_reply.get("error").is_none());
        let forked_sid = init_reply["result"]["session"]["session_id"].as_str().unwrap().to_string();
        assert_ne!(forked_sid, orig_sid, "{label} --fork-session must create a new session: {init_reply}");
        let snap =
            rpc_call(&mut out, &mut stdin, 3, "session.get", json!({ "session_id": forked_sid }))["result"].clone();
        assert_eq!(snap["parent_session_id"], orig_sid, "{label} --fork-session parent_session_id: {snap}");
        let msgs =
            rpc_call(&mut out, &mut stdin, 4, "agent.list_messages", json!({ "session_id": forked_sid }))["result"]
                .clone();
        assert!(
            !msgs["data"].as_array().unwrap().is_empty(),
            "{label} --fork-session must copy parent history: {msgs}"
        );
        let _ = rpc_call(&mut out, &mut stdin, 5, "shutdown", json!({}));
        drop(stdin);
        assert!(child.wait().unwrap().success());
    }
}
