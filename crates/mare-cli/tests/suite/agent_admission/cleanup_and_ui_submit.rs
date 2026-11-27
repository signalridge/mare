use super::*;

/// Regression: when a new compaction starts while an old compaction is wrapping up (inside a
/// yielding `compact/end` or `compact/after` listener), the old compaction's `<close>` guard
/// must not clobber the new compaction's `busy()` or `token`.
#[tokio::test]
async fn overlapping_compaction_in_end_listener_is_not_clobbered_by_old_cleanup() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "keep_recent_tokens": 1, "stages": ["local"] } } });
        let h = env
            .host().builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "first summary" },
                { "text": "slow second summary ".repeat(150), "delay_ms": 20 }
            ]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"
            local function seed_msgs()
                local msgs = {}
                for i = 1, 4 do
                    msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = ("OLD-%d "):format(i) .. string.rep("words ", 200) } } }
                    msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "ack" } } }
                end
                A:set_messages(msgs)
            end
            seed_msgs()
            SECOND_STARTED = false
            SECOND_BUSY_IN_LISTENER = nil
            SECOND_DONE = false
            local seen = 0
            C.cordis:on("compact/completed", function()
                seen = seen + 1
                if seen == 1 then
                    seed_msgs()
                    mare.task.spawn(function()
                        local ok, res = pcall(K.run, K)
                        SECOND_OK = ok and res.status == "compacted"
                        SECOND_ERR = not ok and tostring(res) or nil
                        SECOND_KIND = (not ok and type(res) == "table" and res.data) and res.data.kind or nil
                        SECOND_DONE = true
                    end)
                    -- Yield so the second compaction starts and sets S.busy / S.token
                    -- before the first compaction's run() returns and runs its <close> guard.
                    while not SECOND_STARTED do
                        mare.task.sleep(5)
                    end
                    SECOND_BUSY_IN_LISTENER = K:busy()
                end
            end)
            C.cordis:on("compact/started", function()
                if seen == 1 then
                    SECOND_STARTED = true
                end
            end)
            local first_ok, first_res = pcall(K.run, K)
            FIRST_OK = first_ok and first_res.status == "compacted"
        "#,
        )
        .await;
        // First compaction has returned (and its <close> guard has executed);
        // the second compaction must still be busy and cancellable via A:abort().
        let (first_ok, busy_in_listener, still_busy, second_done): (bool, bool, bool, bool) = eval(
            &h,
            "return FIRST_OK == true, SECOND_BUSY_IN_LISTENER == true, K:busy() == true, SECOND_DONE == true",
        )
        .await;
        assert!(first_ok, "first compaction should succeed");
        assert!(busy_in_listener, "second compaction should be busy in listener");
        assert!(!second_done, "second compaction should still be running");
        assert!(still_busy, "old compaction cleanup must not clear new compaction's busy()");

        exec(&h, "A:cancel()").await;
        wait_until(10, "second compaction cancelled", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SECOND_DONE and not K:busy()").await }
        })
        .await;
        assert_eq!(eval::<Option<String>>(&h, "return SECOND_KIND").await.as_deref(), Some("cancelled"));
        h.shutdown().await;
    })
    .await
}

/// Regression (必修 ②): cancelling during `compact/before` must stop before any pruning/stage
/// runs and must leave tool results unpruned.
#[tokio::test]
async fn cancel_during_compact_before_hook_skips_pruning_and_stages() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "stages": ["prune", "local"],
                    "prune_protect_tokens": 0,
                    "prune_min_saving_tokens": 1,
                    "prune_tool_result_chars": 50,
                    "prune_head_chars": 10,
                    "prune_tail_chars": 10,
                    "keep_recent_tokens": 1
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "unused" }])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"
            local big = string.rep("PRUNABLE ", 300)
            A:set_messages({
                { role = "user", content = { { type = "text", text = "read" } } },
                { role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = { path = "x" } } } },
                { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = big } } },
                { role = "assistant", content = { { type = "text", text = "done" } } },
                { role = "user", content = { { type = "text", text = "next" } } },
                { role = "assistant", content = { { type = "text", text = "ok" } } },
            })
            STAGES_RAN = 0
            ENDS = {}
            C.cordis:on("compact/before", function()
                K:cancel()
            end)
            C.cordis:on("compact/stage", function()
                STAGES_RAN = STAGES_RAN + 1
            end)
            C.cordis:on("compact/completed", function(e)
                ENDS[#ENDS + 1] = e
            end)
            local ok, res = pcall(K.run, K)
            OK = ok and res.status == "compacted"
            ERR = not ok and tostring(res) or nil
            KIND = (not ok and type(res) == "table" and res.data) and res.data.kind or nil
        "#,
        )
        .await;
        let (ok, kind, stages_ran, cancelled, encoded): (bool, Option<String>, i64, bool, String) = eval(
            &h,
            "return OK == true, KIND, STAGES_RAN, #ENDS == 1 and ENDS[1].cancelled == true, mare.json.encode(A:list_messages().data)",
        )
        .await;
        assert!(!ok);
        assert_eq!(kind.as_deref(), Some("cancelled"));
        assert_eq!(stages_ran, 0, "no compaction stages must run when cancelled in compact/before");
        assert!(cancelled, "compact/completed must report cancelled=true");
        assert!(!encoded.contains("tool result middle pruned"), "tool results must not be pruned: {encoded}");
        h.shutdown().await;
    })
    .await
}

/// Regression (必修 ③): `ui/input.lua` `input/submit` during compaction must notify the user and
/// restore the draft instead of silently dropping the message.
#[tokio::test]
async fn ui_input_submit_during_compaction_notifies_and_restores_draft() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "keep_recent_tokens": 1, "stages": ["local"] } } });
        let h = env
            .host().builtins(&["compact", "usage"])
            .faux(json!([{ "text": "slow summary ".repeat(150), "delay_ms": 20 }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"
            local msgs = {}
            for i = 1, 4 do
                msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = ("OLD-%d "):format(i) .. string.rep("words ", 200) } } }
                msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "ack" } } }
            end
            A:set_messages(msgs)
            NOTIFIED = {}
            RESTORED = {}
            mare.ui.connected = function() return true end
            mare.ui.notify = function(text, level) NOTIFIED[#NOTIFIED + 1] = { text = text, level = level } end
            mare.ui.set_editor = function(text, opts) RESTORED[#RESTORED + 1] = { text = text, opts = opts } end
        "#,
        )
        .await;
        h.kernel.reload(Some("ui")).await.unwrap();
        exec(
            &h,
            r#"
            COMPACT_STARTED = false
            COMPACT_ENDS = {}
            C.cordis:on("compact/started", function() COMPACT_STARTED = true end)
            C.cordis:on("compact/completed", function(e) COMPACT_ENDS[#COMPACT_ENDS + 1] = e end)
            mare.task.spawn(function() K:run() end)
        "#,
        )
        .await;
        wait_until(10, "compaction started", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return COMPACT_STARTED and K:busy()").await }
        })
        .await;
        // only the notices the submit causes: a reloaded ui plugin may repeat its startup notices
        // (on the cordis tree the reload re-applies it, permissions-off warning included)
        exec(&h, r#"NOTIFIED = {}; C.cordis:get("bus"):emit("input/submit", { text = "do not drop me" })"#).await;
        wait_until(10, "input rejection notified and draft restored", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #NOTIFIED >= 1 and #RESTORED >= 1").await }
        })
        .await;
        let (notice, draft): (String, String) = eval(&h, "return NOTIFIED[1].text, RESTORED[1].text").await;
        assert!(notice.contains("compaction in progress"), "expected compaction rejection notice, got: {notice}");
        assert_eq!(draft, "do not drop me");

        exec(&h, "A:cancel()").await;
        wait_until(10, "compaction cancelled", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #COMPACT_ENDS == 1 and not K:busy()").await }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

/// Regression (`admit-r2` #2): an admitted turn that fails during execution must NOT restore the
/// user's editor draft (only admission rejections restore the draft).
#[tokio::test]
async fn ui_input_execution_failure_does_not_restore_draft() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "error": "upstream model failure" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            NOTIFIED = {}
            RESTORED = {}
            AGENT_ENDS = {}
            PROMPT_DONE = false
            local orig_prompt = A.prompt
            A.prompt = function(...)
                local r = orig_prompt(...)
                PROMPT_DONE = true
                return r
            end
            mare.ui.connected = function() return true end
            mare.ui.notify = function(text, level) NOTIFIED[#NOTIFIED + 1] = { text = text, level = level } end
            mare.ui.set_editor = function(text, opts) RESTORED[#RESTORED + 1] = { text = text, opts = opts } end
        "#,
        )
        .await;
        h.kernel.reload(Some("ui")).await.unwrap();
        exec(
            &h,
            r#"
            C.cordis:on("turn/completed", function(e) AGENT_ENDS[#AGENT_ENDS + 1] = e end)
            C.cordis:get("bus"):emit("input/submit", { text = "admitted prompt that fails mid-run" })
        "#,
        )
        .await;
        wait_until(10, "submit_prompt coroutine finished", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return PROMPT_DONE and #AGENT_ENDS == 1").await }
        })
        .await;
        let (restored_count, end_err): (i64, Option<String>) =
            eval(&h, "return #RESTORED, AGENT_ENDS[1].error and AGENT_ENDS[1].error.message or nil").await;
        assert!(end_err.is_some(), "expected execution error on turn/completed");
        assert_eq!(restored_count, 0, "admitted turn failure must not restore draft");
        h.shutdown().await;
    })
    .await
}
