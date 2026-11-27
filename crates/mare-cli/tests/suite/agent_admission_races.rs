//! Admission races (b1-1520 ADMIT): the turn owner survives its caller, deferred wake-ups follow
//! every compaction release, and an RPC prompt is admitted only into the session its idempotency
//! receipt is keyed to. Interleavings are pinned with barriers, never with sleeps.
use crate::common::*;
use serde_json::json;

/// b1-1520 ADMIT #4: `agent.prompt` runs the turn (`finish_prompt`) in its own task. Aborting the
/// waiting caller while the turn is parked inside a `turn/started` listener must not strand
/// `busy`: the turn still ends and the agent accepts the next prompt. Without the separate task
/// the abort drops the turn mid-listener and `busy` stays true forever.
/// Reference tests: Kimi packages/agent-core-v2/test/agent/loop/loop.test.ts:1551 (the waiter of
/// a turn settles independently of the turn); reference implementation (a
/// cancelled prompt in admission leaves no late follow-up and the slot is released).
#[tokio::test]
async fn aborting_the_prompt_caller_before_agent_start_finishes_the_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "first" }, { "text": "second" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            GATE_TX, GATE_RX = mare.task.channel()
            ENTERED = false; ENDS = {}
            C.cordis:on("turn/started", function()
                if not ENTERED then
                    ENTERED = true
                    GATE_RX:recv()
                end
            end)
            C.cordis:on("turn/completed", function(e) ENDS[#ENDS + 1] = e end)
            CALLER = mare.task.spawn(function() local s = A:prompt("first"); RES = A:wait(s.turn_id) end)
        "#,
        )
        .await;
        wait_until(10, "turn parked in turn/started", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return ENTERED").await }
        })
        .await;
        // The caller is gone before the turn leaves turn/started.
        exec(&h, "CALLER:abort(); pcall(CALLER.wait, CALLER)").await;
        assert!(eval::<bool>(&h, "return CALLER:done() and RES == nil").await, "caller must have been aborted");
        assert!(eval::<bool>(&h, "return A:get().busy").await, "the turn is still owned while parked");
        exec(&h, "GATE_TX:send(true)").await;
        wait_until(10, "abandoned turn to end and release busy", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #ENDS == 1 and not A:get().busy").await }
        })
        .await;
        assert_eq!(eval::<String>(&h, "return ENDS[1].text").await, "first");
        let next: String = eval(
            &h,
            r#"local s = A:prompt("second"); local r = A:wait(s.turn_id); return tostring(r.error or r.text)"#,
        )
        .await;
        assert_eq!(next, "second");
        h.shutdown().await;
    })
    .await
}

/// b1-1520 ADMIT #3: a woken notification deferred because compaction was busy runs once a
/// `summarize_only` compaction (the `/tree` branch summary) releases — on success and on
/// failure. That path emits no `compact/after`, so the wake-up must follow the release itself.
/// Reference tests: reference implementation (a wake latched behind
/// maintenance is replayed at convergence); Kimi packages/agent-core-v2/test/agent/
/// fullCompaction/fullCompaction.test.ts:3883 and :3934 (input deferred mid-compaction replays
/// after completion and after failure); Codex core/tests/suite/pending_input.rs:2236 (gated
/// compaction, the steer is sent after it). The summarizer is a gated mock, not a timed one.
#[tokio::test]
async fn wake_notification_deferred_by_summarize_only_runs_after_it_releases() {
    for fail in [false, true] {
        local(async {
            let env = TestEnv::new().await;
            let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
            expose(&h);
            global(&h, "K", h.service_table("compact").unwrap());
            global(&h, "L", h.service_table("llm").unwrap());
            exec(
                &h,
                &format!(
                    r###"
                STARTS = {{}}; ENDS = {{}}; FAIL = {fail}; IN_SUMMARY = false
                GATE_TX, GATE_RX = mare.task.channel()
                C.cordis:on("turn/started", function(e) STARTS[#STARTS + 1] = e end)
                C.cordis:on("turn/completed", function(e) ENDS[#ENDS + 1] = e end)
                local calls = 0
                L:provider("faux").stream = function(req, sink)
                    calls = calls + 1
                    if calls == 1 then
                        IN_SUMMARY = true
                        GATE_RX:recv()
                        if FAIL then error({{ message = "summary provider failed", kind = "fatal", retryable = false }}) end
                        sink({{ type = "text_delta", id = "t", delta = "## Summary\nbranch" }})
                        return {{ role = "assistant", stop_reason = "stop", content = {{ {{ type = "text", text = "## Summary\nbranch" }} }} }}
                    end
                    sink({{ type = "text_delta", id = "t", delta = "noted" }})
                    return {{ role = "assistant", stop_reason = "stop", content = {{ {{ type = "text", text = "noted" }} }} }}
                end
                local msgs = {{
                    {{ role = "user", content = {{ {{ type = "text", text = "branch work " .. string.rep("w ", 50) }} }} }},
                    {{ role = "assistant", content = {{ {{ type = "text", text = "done" }} }} }},
                }}
                SUMMARY_DONE = false
                SUMMARIZER = mare.task.spawn(function()
                    local ok, res = pcall(K.run, K, nil, {{ summarize_only = true, messages = msgs }})
                    SUMMARY = ok and res.summary or nil
                    SUMMARY_DONE = true
                end)
            "###
                ),
            )
            .await;
            wait_until(10, "summarizer parked inside summarize_only", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return IN_SUMMARY").await }
            })
            .await;
            let (busy_at_notify, started_during): (bool, usize) = eval(
                &h,
                r#"
                local busy = K:busy()
                A:notify("background job finished", { wake = true })
                return busy, #STARTS
            "#,
            )
            .await;
            assert!(busy_at_notify, "the notification must arrive while summarize_only holds admission");
            assert_eq!(started_during, 0, "a busy compaction defers the woken notification");
            exec(&h, "GATE_TX:send(true)").await;
            wait_until(10, "summarize_only to finish", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return SUMMARY_DONE").await }
            })
            .await;
            let summarized: bool = eval(&h, "return type(SUMMARY) == 'string' and not K:busy()").await;
            assert_eq!(summarized, !fail, "fail={fail}");
            wait_until(10, "deferred notification turn to run", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return #ENDS == 1").await }
            })
            .await;
            assert!(
                eval::<bool>(&h, "return STARTS[1].trigger == 'notification' and ENDS[1].text == 'noted'").await,
                "fail={fail}"
            );
            h.shutdown().await;
        })
        .await
    }
}

/// b11 ADMIT #1: a replacement `compact` service (SPEC: compaction is replaceable) knows
/// nothing of the built-in's process-local `compact/idle`; it ends with the public
/// `compact/after`, and that must still run a notification deferred while it was busy — once.
#[tokio::test]
async fn wake_notification_deferred_by_a_replacement_compact_service_runs_on_compact_after() {
    local(async {
        let env = TestEnv::new().await;
        // a user plugin replaces compact (one provider per service); it reports busy while
        // mare.g.fake_busy is set (only `busy` matters here; the required methods are inert)
        let plugin = env.config_dir().join("plugins/replacement/init.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(
            plugin,
            r#"return { name = "compact", override = true, provide = { "compact" },
                 apply = function(ctx)
                   local Compact = require("cordis").class("Compact", mare.Service)
                   function Compact:constructor(c) Compact.super.constructor(self, c, "compact") end
                   function Compact:busy() return mare.g.fake_busy == true end
                   function Compact:run() return { status = "compacted", tokens_before = 0, tokens_after = 0 } end
                   function Compact:cancel() end
                   function Compact:register_stage() end
                   function Compact:measure() return 0 end
                   ctx:plugin(Compact)
                 end }"#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .faux(json!([{ "text": "noted" }, { "text": "unexpected second run" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            STARTS = {}; ENDS = {}
            C.cordis:on("turn/started", function(e) STARTS[#STARTS + 1] = e end)
            C.cordis:on("turn/completed", function(e) ENDS[#ENDS + 1] = e end)
            mare.g.fake_busy = true
            A:notify("background job finished", { wake = true })
            -- a run would start right away (asynchronously): give it the time to show
            mare.task.sleep(100)
            DEFERRED = #STARTS
        "#,
        )
        .await;
        assert_eq!(eval::<usize>(&h, "return DEFERRED").await, 0, "the busy replacement defers the notification");
        exec(
            &h,
            r#"
            mare.g.fake_busy = false
            C.cordis:get("bus"):emit("compact/completed", { status = "completed", changed = true, reason = "manual", tokens_before = 10, tokens_after = 5 })
        "#,
        )
        .await;
        wait_until(10, "deferred notification turn to run", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return #ENDS == 1").await }
        })
        .await;
        // a second, unrelated compaction signal must not start another run of the same notes
        exec(&h, r#"C.cordis:get("bus"):emit("compact/completed", { status = "completed", changed = true, reason = "manual", tokens_before = 5, tokens_after = 5 })"#)
            .await;
        let (starts, text): (usize, String) = eval(&h, "return #STARTS, ENDS[1].text").await;
        assert_eq!((starts, text.as_str()), (1, "noted"));
        h.shutdown().await;
    })
    .await
}
