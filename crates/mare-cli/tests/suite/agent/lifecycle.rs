//! AL1 turn-exit wakeup, cancelling-window queuing, turn-stopping steer re-check,
//! session-switch/dispose isolation, and crash-recovery non-auto-start tests.

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn queue_only_keeps_idle_inbox_idle_after_wakeup_events() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "must-not-run" }])).build().await;
        expose(&h);
        let (starts, queue): (i64, String) = eval(
            &h,
            r#"
            local starts = 0
            C.cordis:on("turn/started", function() starts = starts + 1 end)
            local submitted = A:prompt("peer-queue-only", { queue_only = true })
            assert(submitted.status == "queued")
            mare.task.yield()
            mare.task.yield()
            C.cordis:get("bus"):emit("compact/completed", { reason = "manual", changed = false, tokens_before = 0, tokens_after = 0 })
            mare.task.yield()
            mare.task.yield()
            return starts, mare.json.encode(A:list_queue().data)
            "#,
        )
        .await;
        assert_eq!(starts, 0, "queue-only delivery must not wake an idle agent");
        assert!(queue.contains("peer-queue-only"), "{queue}");
        h.shutdown().await;
    })
    .await;
}

/// reference implementation: a non-waking delivery stays parked
/// even when it arrives while a cancelled turn is still unwinding.
#[tokio::test]
async fn queue_only_during_cancel_does_not_wake_a_new_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "must-not-run" }
            ]))
            .build()
            .await;
        expose(&h);
        let (starts, restarted, queued): (i64, bool, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local restarted_tx, restarted_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold the cancelled turn until explicitly released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })
            local starts = 0
            C.cordis:on("turn/started", function()
                starts = starts + 1
                if starts > 1 then restarted_tx:send(true) end
            end)

            local t1 = A:prompt("first")
            assert(entered_rx:recv() == true)
            assert(A:cancel({ turn_id = t1.turn_id, clear_queue = true }).turn_id == t1.turn_id)
            local q = A:prompt("queue-only-during-cancel", { when_busy = "queue", queue_only = true })
            assert(q.status == "queued" and type(q.input_id) == "string", mare.json.encode(q))
            release_tx:send(true)
            assert(A:wait(t1.turn_id).status == "cancelled")
            local _, restarted = mare.task.select(
                function() return restarted_rx:recv() end,
                function() mare.task.sleep(250); return false end
            )
            return starts, restarted, mare.json.encode(A:list_queue().data)
            "#,
        )
        .await;
        assert!(!restarted, "queue-only delivery during cancellation must not start another turn");
        assert_eq!(starts, 1);
        assert!(queued.contains("queue-only-during-cancel"), "{queued}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn prompt_when_busy_queue_during_cancel_persists_before_ack_and_wakes_next_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "after-cancel-reply" }
            ]))
            .build()
            .await;
        expose(&h);
        let (q_status, q_id, turn2_count, msgs_json): (String, String, i64, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            local turn2_done_tx, turn2_done_rx = mare.task.channel()
            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)
            C.cordis:on("turn/completed", function(ev)
                if turn_starts >= 2 then
                    turn2_done_tx:send(ev)
                end
            end)

            local t1 = A:prompt("first")
            assert(t1.status == "started")
            assert(entered_rx:recv() == true)

            -- Interrupt Turn 1 while Turn 1 is still active inside slow_hold (busy == true, token cancelled).
            local c_res = A:cancel({ turn_id = t1.turn_id, clear_queue = true })
            assert(c_res.turn_id == t1.turn_id, mare.json.encode(c_res))
            assert(#c_res.removed == 0)

            -- New input arrives AFTER interrupt was issued, BEFORE Turn 1 exits.
            -- Must persist into durable inbox and ack `queued` before Turn 1 exits.
            local q = A:prompt("arrived-during-cancel", { when_busy = "queue" })
            assert(#A:list_queue().data == 1, "must be durably queued before ack returns")

            -- Release Turn 1 tool so Turn 1 finishes unwinding; Turn 2 must wake automatically.
            release_tx:send(true)
            local r1 = A:wait(t1.turn_id)
            assert(r1.status == "cancelled", mare.json.encode(r1))

            local _, ev2 = mare.task.select(
                function() return turn2_done_rx:recv() end,
                function() mare.task.sleep(4000); return { error = "timeout waiting for Turn 2" } end
            )
            assert(not ev2.error, tostring(ev2.error))
            assert(#A:list_queue().data == 0, "woken Turn 2 must have consumed queued item")
            return q.status, tostring(q.input_id or ""), turn_starts, mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert_eq!(q_status, "queued");
        assert!(!q_id.is_empty());
        assert_eq!(turn2_count, 2, "Turn 2 must wake automatically once Turn 1 finishes unwinding");
        assert!(msgs_json.contains("arrived-during-cancel"), "{msgs_json}");
        assert!(msgs_json.contains("after-cancel-reply"), "{msgs_json}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn steer_during_cancel_routes_to_next_turn_and_wakes_after_unwind() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "steered-next-turn-reply" }
            ]))
            .build()
            .await;
        expose(&h);
        let (input_id, turn_starts, msgs_json): (String, i64, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            local turn2_done_tx, turn2_done_rx = mare.task.channel()
            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)
            C.cordis:on("turn/completed", function(ev)
                if turn_starts >= 2 then
                    turn2_done_tx:send(ev)
                end
            end)

            local t1 = A:prompt("first")
            assert(entered_rx:recv() == true)
            assert(A:cancel({ turn_id = t1.turn_id, clear_queue = true }).turn_id == t1.turn_id)

            local sres = A:steer("steer-while-cancelling")
            assert(type(sres.input_id) == "string", mare.json.encode(sres))

            release_tx:send(true)
            A:wait(t1.turn_id)
            local _, ev2 = mare.task.select(
                function() return turn2_done_rx:recv() end,
                function() mare.task.sleep(4000); return { error = "timeout waiting for Turn 2" } end
            )
            assert(not ev2.error, tostring(ev2.error))
            assert(#A:list_queue().data == 0, "woken Turn 2 must drain steer-during-cancel without leaving it in steer")
            return sres.input_id, turn_starts, mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert!(!input_id.is_empty());
        assert_eq!(turn_starts, 2);
        assert!(msgs_json.contains("steer-while-cancelling"), "{msgs_json}");
        assert!(msgs_json.contains("steered-next-turn-reply"), "{msgs_json}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn cancel_clear_queue_false_keeps_existing_input_idle() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "kept-inbox-ran" }
            ]))
            .build()
            .await;
        expose(&h);
        let (turn_starts, queued, msgs_json): (i64, String, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)

            local t1 = A:prompt("first")
            assert(entered_rx:recv() == true)
            assert(A:prompt("keep-after-cancel", { when_busy = "queue" }).status == "queued")
            local c_res = A:cancel({ turn_id = t1.turn_id, clear_queue = false })
            assert(c_res.turn_id == t1.turn_id and #c_res.removed == 0)

            release_tx:send(true)
            A:wait(t1.turn_id)
            mare.task.yield()
            mare.task.yield()
            return turn_starts, mare.json.encode(A:list_queue().data), mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert_eq!(turn_starts, 1, "cancelled turn must not restart previously queued input");
        assert!(queued.contains("keep-after-cancel"), "{queued}");
        assert!(!msgs_json.contains("keep-after-cancel"), "{msgs_json}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn turn_stopping_hook_inserting_steer_rechecks_and_runs_next_step() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "first-step" },
                { "text": "after-stop-hook-steer" }
            ]))
            .build()
            .await;
        expose(&h);
        let (steps, text, msgs_json): (i64, String, String) = eval(
            &h,
            r#"
            local steps = 0
            local injected = false
            C.cordis:on("step/started", function()
                steps = steps + 1
            end)
            C.cordis:on("agent/turn-stopping", function()
                if not injected then
                    injected = true
                    local s = A:steer("from-stop-hook")
                    assert(type(s.input_id) == "string", mare.json.encode(s))
                end
                return nil
            end)

            local submitted = A:prompt("hello")
            assert(submitted.status == "started", mare.json.encode(submitted))
            local res = A:wait(submitted.turn_id)
            assert(res.status == "completed", mare.json.encode(res))
            return steps, tostring(res.text or ""), mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert_eq!(steps, 2, "agent/turn-stopping inserting steer must trigger a 2nd step in the same turn");
        assert_eq!(text, "after-stop-hook-steer");
        assert!(msgs_json.contains("from-stop-hook"), "{msgs_json}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn turn_ending_from_session_switch_never_wakes_next_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "turn1-done" },
                { "text": "must-not-run-in-new-session" }
            ]))
            .build()
            .await;
        expose(&h);
        let (starts, new_msgs_len): (i64, usize) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            local switched = false
            local switched_tx, switched_rx = mare.task.channel()
            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)
            C.cordis:on("agent/settled", function()
                if turn_starts == 1 and not switched then
                    switched = true
                    A:set_session_id("session-two")
                    A:set_messages({})
                    switched_tx:send(true)
                end
            end)

            local t1 = A:prompt("session-one-turn")
            assert(entered_rx:recv() == true)
            assert(A:prompt("queued-in-session-one", { when_busy = "queue" }).status == "queued")

            -- Release Turn 1; its agent/settled listener switches to a brand-new session before any
            -- spawned wakeup can run. Session 2 must never execute Session 1's queued input.
            release_tx:send(true)
            local _, sw_ok = mare.task.select(
                function() return switched_rx:recv() end,
                function() mare.task.sleep(4000); return false end
            )
            assert(sw_ok == true, "session switch in agent/settled must complete")
            local ok_old = pcall(A.wait, A, t1.turn_id)
            assert(not ok_old, "old session turn_id must not resolve after session switch")
            mare.task.yield()
            mare.task.yield()
            return turn_starts, #A:list_messages().data
            "#,
        )
        .await;
        assert_eq!(starts, 1, "switched-away session turn end must not wake a turn on the new session");
        assert_eq!(new_msgs_len, 0);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn crash_recovery_preserves_unconsumed_inbox_without_auto_starting_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "first" },
                { "text": "must-not-auto-run-on-resume" }
            ]))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            -- Hold Turn 1 behind request/ready deny while persisting queued item to disk inbox,
            -- simulating crash/shutdown before consumption.
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get("policy"):voter("request/ready", function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                end
                return { decision = "deny", reason = "simulate crash before consume" }
            end)
            local t1 = A:prompt("seed")
            held_rx:recv()
            local qres = A:prompt("survive-crash", { when_busy = "queue" })
            assert(qres.status == "queued", mare.json.encode(qres))
            rel_tx:send(true)
            A:wait(t1.turn_id)
            assert(#A:list_queue().data == 1)
            "#,
        )
        .await;
        h.shutdown().await;

        let h2 = env.host().faux(json!([{ "text": "must-not-auto-run-on-resume" }])).resume().build().await;
        expose(&h2);
        let (starts, qlen, qtext, mlen): (i64, usize, String, usize) = eval(
            &h2,
            r#"
            local starts = 0
            C.cordis:on("step/started", function()
                starts = starts + 1
            end)
            mare.task.yield()
            mare.task.yield()
            local q = A:list_queue().data
            return starts, #q, q[1] and q[1].text or "", #A:list_messages().data
            "#,
        )
        .await;
        assert_eq!(starts, 0, "crash recovery must never auto-start unconsumed inbox items");
        assert_eq!(qlen, 1);
        assert_eq!(qtext, "survive-crash");
        assert_eq!(mlen, 0);
        h2.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn queue_persistence_failure_refuses_ack_and_starts_no_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "must-not-run" }
            ]))
            .build()
            .await;
        expose(&h);
        let (starts, ok_prompt, ok_steer, qlen): (i64, bool, bool, usize) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)

            local t1 = A:prompt("first")
            assert(entered_rx:recv() == true)
            assert(A:cancel({ turn_id = t1.turn_id, clear_queue = true }).turn_id == t1.turn_id)

            C.cordis:get("policy"):voter("session/commit-ready", function()
                return { decision = "deny", reason = "simulated disk full" }
            end)
            local ok_prompt = pcall(A.prompt, A, "lost-on-commit-fail", { when_busy = "queue" })
            local ok_steer = pcall(A.steer, A, "lost-steer-on-commit-fail")

            release_tx:send(true)
            A:wait(t1.turn_id)
            mare.task.yield()
            return turn_starts, ok_prompt, ok_steer, #A:list_queue().data
            "#,
        )
        .await;
        assert!(!ok_prompt, "commit failure must reject prompt queue ack");
        assert!(!ok_steer, "commit failure must reject steer queue ack");
        assert_eq!(starts, 1, "rejected queue commit must not trigger Turn 2");
        assert_eq!(qlen, 0);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn compacting_defers_wakeup_until_compact_after_same_session() {
    local(async {
        let env = TestEnv::new().await;
        let plugin = env.config_dir().join("plugins/replacement_compact/init.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(
            plugin,
            r#"return { name = "compact", override = true, provide = { "compact" },
                apply = function(ctx)
                    local Compact = require("cordis").class("Compact", mare.Service)
                    function Compact:constructor(c) Compact.super.constructor(self, c, "compact") end
                    function Compact:busy() return mare.g.fake_busy == true end
                    function Compact:run() return true end
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
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "after-compact-reply" }
            ]))
            .build()
            .await;
        expose(&h);
        let (starts_while_compacting, starts_after_compact, msgs_json): (i64, i64, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            local turn2_done_tx, turn2_done_rx = mare.task.channel()
            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)
            C.cordis:on("turn/completed", function(ev)
                if turn_starts >= 2 then
                    turn2_done_tx:send(ev)
                end
            end)

            local t1 = A:prompt("first")
            assert(entered_rx:recv() == true)
            assert(A:cancel({ turn_id = t1.turn_id, clear_queue = true }).turn_id == t1.turn_id)
            assert(A:prompt("wait-for-compact", { when_busy = "queue" }).status == "queued")

            mare.g.fake_busy = true
            release_tx:send(true)
            A:wait(t1.turn_id)
            mare.task.yield()
            mare.task.yield()
            local during_compact = turn_starts

            mare.g.fake_busy = false
            C.cordis:get("bus"):emit("compact/completed", { reason = "manual", changed = true, tokens_before = 10, tokens_after = 5 })
            local _, ev2 = mare.task.select(
                function() return turn2_done_rx:recv() end,
                function() mare.task.sleep(4000); return { error = "timeout waiting for Turn 2" } end
            )
            assert(not ev2.error, tostring(ev2.error))
            return during_compact, turn_starts, mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert_eq!(starts_while_compacting, 1, "Turn 2 must wait while compacting is active");
        assert_eq!(starts_after_compact, 2, "Turn 2 must wake once compact/completed fires");
        assert!(msgs_json.contains("wait-for-compact"), "{msgs_json}");
        assert!(msgs_json.contains("after-compact-reply"), "{msgs_json}");
        h.shutdown().await;
    })
    .await;
}

/// reference implementation: disposing the live agent
/// must not latch a pending wakeup or start another turn.
#[tokio::test]
async fn dispose_unload_while_unwinding_never_wakes_new_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "must-not-run-after-dispose" }
            ]))
            .build()
            .await;
        expose(&h);
        let (starts, restarted, queued): (i64, bool, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local reload_tx, reload_rx = mare.task.channel()
            local restarted_tx, restarted_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            _G._al1_turn_starts = 0
            C.cordis:on("turn/started", function()
                _G._al1_turn_starts = _G._al1_turn_starts + 1
                if _G._al1_turn_starts > 1 then restarted_tx:send(true) end
            end)

            local t1 = A:prompt("before-reload")
            assert(entered_rx:recv() == true)
            assert(A:cancel({ turn_id = t1.turn_id, clear_queue = true }).turn_id == t1.turn_id)
            assert(A:prompt("queued-before-reload", { when_busy = "queue" }).status == "queued")
            assert(#A:list_queue().data == 1, "input must be queued before reload")

            local old_agent = C.cordis:get("agent")
            assert(rawequal(C.cordis:get("agent"), old_agent), "agent identity must be stable before reload")
            local kernel = C.cordis:get("kernel")
            mare.task.spawn(function()
                local ok, result = pcall(function()
                    local applied = kernel:reload("agent")
                    kernel:reload_barrier()
                    return applied
                end)
                reload_tx:send({ ok = ok, result = result })
            end)
            local _, reload = mare.task.select(
                function() return reload_rx:recv() end,
                function() mare.task.sleep(4000); return { ok = false, result = "reload timed out" } end
            )
            assert(reload.ok, tostring(reload.result))
            local replaced = false
            for _, name in ipairs(reload.result) do
                if name == "agent" then replaced = true end
            end
            assert(replaced, "targeted reload must replace the agent")

            -- No release was sent before the old provider was disposed.
            -- Release slow_hold if cancellation did not already close the receiver.
            release_tx:send(true)
            local _, restarted = mare.task.select(
                function() return restarted_rx:recv() end,
                function() mare.task.sleep(250); return false end
            )
            local agent = C.cordis:get("agent")
            assert(agent ~= nil, "agent service must be available after reload")
            assert(not rawequal(agent, old_agent), "the previous agent provider must have been disposed")
            return _G._al1_turn_starts, restarted, mare.json.encode(agent:list_queue().data)
            "#,
        )
        .await;
        assert_eq!(starts, 1, "disposing the old agent must not start another turn");
        assert!(!restarted, "no new turn/started may occur after the reload");
        assert!(queued.contains("queued-before-reload"), "queue must survive the reload: {queued}");
        h.shutdown().await;
    })
    .await;
}

#[test]
fn rpc_queued_prompt_during_cancel_and_turn_stopping_steer() {
    use std::collections::BTreeMap;

    let env = tokio::runtime::Runtime::new().unwrap().block_on(TestEnv::new());
    let home = env.home.path().to_path_buf();
    let cfg_dir = env.config_dir();
    std::fs::create_dir_all(cfg_dir.join("plugins")).unwrap();
    std::fs::write(
        cfg_dir.join("plugins/al1_rpc_probe.lua"),
        r#"
        return {
            name = "al1_rpc_probe",
            inject = { "agent", "tools", "commands", "bus" },
            apply = function(ctx)
                local rel_tx, rel_rx = mare.task.channel()
                ctx:get("tools"):register("rpc_slow_hold", {
                    description = "hold ignoring turn cancel until /al1-release runs",
                    parameters = { type = "object", properties = {} },
                    execute = function()
                        ctx:get("bus"):emit("host/notice", { level = "info", text = "AL1_HOLD_ENTERED" })
                        rel_rx:recv()
                        return { content = { { type = "text", text = "released" } } }
                    end,
                })
                ctx:get("commands"):register("al1-release", {
                    description = "release rpc_slow_hold",
                    run = function()
                        rel_tx:send(true)
                    end,
                })
                local first_turn = nil
                local steered = false
                ctx:on("step/started", function(ev)
                    first_turn = first_turn or ev.turn_id
                end)
                ctx:on("agent/turn-stopping", function(ev)
                    if first_turn and ev.turn_id ~= first_turn and not steered then
                        steered = true
                        ctx:get("agent"):steer("rpc-stop-hook-steer")
                    end
                end)
            end,
        }
        "#,
    )
    .unwrap();
    let script = home.join("faux_al1.json");
    std::fs::write(
        &script,
        json!([
            { "tool_calls": [{ "id": "c1", "name": "rpc_slow_hold", "arguments": {} }] },
            { "text": "rpc-turn2-step1" },
            { "text": "rpc-turn2-after-stop-hook-steer" }
        ])
        .to_string(),
    )
    .unwrap();
    let env_map = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let mut client =
        crate::common::rpc::RpcClient::spawn(&["--rpc", "--yolo", "--model", "faux/echo"], &env_map, &env.cwd);
    let init = client.call("initialize", json!({"protocol_version": "3.0"}));
    let session = init["session"]["session_id"].as_str().expect("session_id").to_string();
    client.call("events.subscribe", json!({ "session_id": session, "include": ["turn/*", "host/notice"] }));

    let t1 = client.call("agent.prompt", json!({ "session_id": session, "input": "turn1" }));
    assert_eq!(t1["status"], "started", "{t1}");
    loop {
        let ev = client.wait_event("host/notice");
        if ev["data"]["text"].as_str().is_some_and(|t| t.contains("AL1_HOLD_ENTERED")) {
            break;
        }
    }

    let c1 =
        client.call("agent.cancel", json!({ "session_id": session, "turn_id": t1["turn_id"], "clear_queue": true }));
    assert_eq!(c1["turn_id"], t1["turn_id"], "{c1}");

    let q2 = client.call(
        "agent.prompt",
        json!({ "session_id": session, "input": "rpc-queued-during-cancel", "when_busy": "queue" }),
    );
    assert_eq!(q2["status"], "queued", "{q2}");

    client.call("commands.run", json!({ "line": "/al1-release" }));

    let w1 = client.call("agent.wait", json!({ "session_id": session, "turn_id": t1["turn_id"] }));
    assert_eq!(w1["status"], "cancelled", "{w1}");

    // Wait for woken Turn 2 to finish both step1 and the steer inserted during agent/turn-stopping.
    let mut ended_turn2 = None;
    for _ in 0..30 {
        let ev = client.wait_event("turn/completed");
        if ev["data"]["turn_id"] != t1["turn_id"] {
            ended_turn2 = Some(ev);
            break;
        }
    }
    let ended_turn2 = ended_turn2.expect("Turn 2 turn/completed after cancel");
    assert_eq!(ended_turn2["data"]["text"], "rpc-turn2-after-stop-hook-steer", "{ended_turn2}");
    let msgs = client.call("agent.list_messages", json!({ "session_id": session }));
    assert!(msgs.to_string().contains("rpc-queued-during-cancel"), "{msgs}");
    assert!(msgs.to_string().contains("rpc-stop-hook-steer"), "{msgs}");
    client.shutdown_and_wait();
}

#[tokio::test]
async fn remove_queued_during_cancel_suppresses_turn2_wakeup() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "must-not-run-after-remove" }
            ]))
            .build()
            .await;
        expose(&h);
        let (turn_starts, qlen): (i64, usize) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)

            local t1 = A:prompt("first")
            assert(entered_rx:recv() == true)
            assert(A:cancel({ turn_id = t1.turn_id, clear_queue = true }).turn_id == t1.turn_id)

            local q = A:prompt("transient-queued", { when_busy = "queue" })
            assert(q.status == "queued", mare.json.encode(q))
            A:delete_queued(q.input_id)
            assert(#A:list_queue().data == 0)

            release_tx:send(true)
            A:wait(t1.turn_id)
            mare.task.yield()
            mare.task.yield()
            return turn_starts, #A:list_queue().data
            "#,
        )
        .await;
        assert_eq!(turn_starts, 1, "removing queued item before Turn 1 exit must suppress Turn 2");
        assert_eq!(qlen, 0);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn queued_during_aborted_session_switch_wakes_on_switch_complete() {
    local(async {
        let env = TestEnv::new().await;
        let plugin = env.config_dir().join("plugins/replacement_compact/init.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(
            plugin,
            r#"return { name = "compact", override = true, provide = { "compact" },
                apply = function(ctx)
                    local Compact = require("cordis").class("Compact", mare.Service)
                    function Compact:constructor(c) Compact.super.constructor(self, c, "compact") end
                    function Compact:busy() return mare.g.fake_busy == true end
                    function Compact:run() return true end
                    function Compact:cancel()
                        local ch = mare.shared("al1_channels")
                        if ch.abort_entered_tx then
                            ch.abort_entered_tx:send(true)
                            ch.abort_release_rx:recv()
                            error("force session switch rollback")
                        end
                    end
                    function Compact:register_stage() end
                    function Compact:measure() return 0 end
                    ctx:plugin(Compact)
                end }"#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .faux(json!([
                { "tool_calls": [{ "id": "c1", "name": "slow_hold", "arguments": {} }] },
                { "text": "switch-complete-wake-reply" }
            ]))
            .build()
            .await;
        expose(&h);
        let (q_status, starts_while_switching, starts_after_switch, msgs_json): (String, i64, i64, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("tools"):register("slow_hold", {
                description = "hold until released",
                parameters = { type = "object", properties = {} },
                execute = function()
                    entered_tx:send(true)
                    release_rx:recv()
                    return { content = { { type = "text", text = "released" } } }
                end,
            })

            local turn_starts = 0
            local turn2_done_tx, turn2_done_rx = mare.task.channel()
            local abort_entered_tx, abort_entered_rx = mare.task.channel()
            local abort_release_tx, abort_release_rx = mare.task.channel()
            local ch = mare.shared("al1_channels")
            ch.abort_entered_tx = abort_entered_tx
            ch.abort_release_rx = abort_release_rx

            C.cordis:on("turn/started", function()
                turn_starts = turn_starts + 1
            end)
            C.cordis:on("agent/settled", function()
                if turn_starts == 1 and not mare.g.started_abort_switch then
                    mare.g.started_abort_switch = true
                    mare.g.fake_busy = true
                    mare.task.spawn(function()
                        pcall(S.create, S)
                    end)
                end
            end)
            C.cordis:on("turn/completed", function(ev)
                if turn_starts >= 2 then
                    turn2_done_tx:send(ev)
                end
            end)

            local t1 = A:prompt("first")
            assert(entered_rx:recv() == true)
            assert(A:cancel({ turn_id = t1.turn_id, clear_queue = true }).turn_id == t1.turn_id)
            local q = A:prompt("arrived-before-switch-end", { when_busy = "queue" })
            assert(q.status == "queued", mare.json.encode(q))

            release_tx:send(true)
            A:wait(t1.turn_id)
            local _, entered = mare.task.select(
                function() return abort_entered_rx:recv() end,
                function() mare.task.sleep(4000); return false end
            )
            assert(entered == true, "S:create() must enter begin_switch -> drain_side")
            assert(S:switching(), "real session:switching() must be true while drain_side blocks")
            mare.task.yield()
            mare.task.yield()
            local during = turn_starts

            mare.g.fake_busy = false
            abort_release_tx:send(true)
            local _, ev = mare.task.select(
                function() return turn2_done_rx:recv() end,
                function() mare.task.sleep(4000); return { error = "timeout waiting for Turn 2" } end
            )
            assert(not ev.error, tostring(ev.error))
            return q.status, during, turn_starts, mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert_eq!(q_status, "queued");
        assert_eq!(starts_while_switching, 1, "Turn 2 must wait while switching(s) is true");
        assert_eq!(starts_after_switch, 2, "Turn 2 must wake once session/switch-completed arrives");
        assert!(msgs_json.contains("arrived-before-switch-end"), "{msgs_json}");
        assert!(msgs_json.contains("switch-complete-wake-reply"), "{msgs_json}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn repro_263_parallel_tool_result_persisted_before_slow_sibling_settles() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {
                    "tool_calls": [
                        { "id": "c_slow", "name": "slow_read", "arguments": {} },
                        { "id": "c_fast", "name": "fast_read", "arguments": {} }
                    ]
                },
                { "text": "all done" }
            ]))
            .build()
            .await;
        expose(&h);

        let (session_path, fast_persisted_while_slow_blocked, crashed_jsonl): (String, bool, String) = eval(
            &h,
            r#"
            local slow_started_tx, slow_started_rx = mare.task.channel()
            local slow_release_tx, slow_release_rx = mare.task.channel()
            local fast_done_tx, fast_done_rx = mare.task.channel()

            C.cordis:get("tools"):register("slow_read", {
                description = "blocking read-only tool",
                side_effect = "read",
                concurrency_safe = true,
                parameters = { type = "object", properties = {} },
                execute = function()
                    slow_started_tx:send(true)
                    slow_release_rx:recv()
                    return { content = { { type = "text", text = "SLOW_PARALLEL_RESULT_263" } } }
                end,
            })
            C.cordis:get("tools"):register("fast_read", {
                description = "immediate read-only tool",
                side_effect = "read",
                concurrency_safe = true,
                parameters = { type = "object", properties = {} },
                execute = function()
                    fast_done_tx:send(true)
                    return { content = { { type = "text", text = "FAST_PARALLEL_RESULT_263" } } }
                end,
            })

            local turn = A:prompt("run parallel reads")
            assert(slow_started_rx:recv() == true, "slow_read did not start")
            assert(fast_done_rx:recv() == true, "fast_read did not finish while slow_read was running")
            -- Wait with a bounded deadline until fast_read's completed result reaches the session journal
            -- while slow_read remains blocked.
            local path = S:info().path
            local raw, persisted = "", false
            for _ = 1, 500 do
                raw = path and mare.fs.read(path) or ""
                if raw:find("FAST_PARALLEL_RESULT_263", 1, true) ~= nil then
                    persisted = true
                    break
                end
                mare.task.sleep(10)
            end

            slow_release_tx:send(true)
            A:wait(turn.turn_id)
            return tostring(path), persisted, raw
            "#,
        )
        .await;
        h.shutdown().await;
        assert!(
            fast_persisted_while_slow_blocked,
            "completed parallel tool result (c_fast) must be persisted to session file ({session_path}) before earlier slow sibling (c_slow) settles"
        );

        // Simulate crash recovery from the JSONL snapshot captured while `c_slow` was still pending:
        // `c_fast` must retain its completed result and provider replay must stay in call order [c_slow, c_fast].
        drop(env);
        let env2 = TestEnv::new().await;
        let recovered_path = env2
            .home
            .path()
            .join("state/sessions")
            .join(mare_store::cwd_slug(&env2.cwd))
            .join("2026-01-01T00-00-00_repro263.jsonl");
        std::fs::create_dir_all(recovered_path.parent().unwrap()).unwrap();
        std::fs::write(&recovered_path, &crashed_jsonl).unwrap();
        let h2 = env2.host().faux(json!([{ "text": "recovered ok" }])).resume().build().await;
        expose(&h2);
        let (replayed_order, fast_text, slow_interrupted): (String, String, bool) = eval(
            &h2,
            r#"
            local seen = {}
            C.cordis:on("request/build", function(req, next)
                for _, m in ipairs(req.messages or {}) do
                    if m.role == "tool_result" then
                        seen[#seen + 1] = tostring(m.tool_name)
                    end
                end
                return next()
            end)
            local turn = A:prompt("continue after crash")
            local res = A:wait(turn.turn_id)
            assert(res.text == "recovered ok", tostring(res.error))
            local fast_t, slow_int = "", false
            for _, m in ipairs(A:list_messages().data) do
                if m.role == "tool_result" and m.tool_name == "fast_read" then
                    fast_t = (m.content and m.content[1] and m.content[1].text) or ""
                elseif m.role == "tool_result" and m.tool_name == "slow_read" then
                    local t = (m.content and m.content[1] and m.content[1].text) or ""
                    slow_int = t:find("interrupted", 1, true) ~= nil
                end
            end
            return table.concat(seen, ","), fast_t, slow_int
            "#,
        )
        .await;
        h2.shutdown().await;
        assert_eq!(fast_text, "FAST_PARALLEL_RESULT_263", "completed sibling result must survive crash recovery");
        assert!(slow_interrupted, "unfinished earlier sibling c_slow must be settled as interrupted");
        assert_eq!(replayed_order, "slow_read,fast_read", "provider replay must retain model call order");
    })
    .await;
}

#[tokio::test]
async fn parallel_tool_result_persisted_while_earlier_item_completed_listener_blocks() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {
                    "tool_calls": [
                        { "id": "c_first", "name": "first_read", "arguments": {} },
                        { "id": "c_second", "name": "second_read", "arguments": {} }
                    ]
                },
                { "text": "both published" }
            ]))
            .build()
            .await;
        expose(&h);

        let (persisted_while_blocked, second_published_early, published_order): (bool, bool, String) = eval(
            &h,
            r#"
            local listener_entered_tx, listener_entered_rx = mare.task.channel()
            local listener_release_tx, listener_release_rx = mare.task.channel()
            local second_go_tx, second_go_rx = mare.task.channel()
            local second_done_tx, second_done_rx = mare.task.channel()
            local completed_names = {}
            local second_call_id = nil

            C.cordis:on("item/started", function(ev)
                if type(ev) == "table" and type(ev.item) == "table" and ev.item.type == "tool_call" then
                    if ev.item.name == "second_read" then
                        second_call_id = ev.item.tool_call_id
                    end
                end
            end)

            C.cordis:on("item/completed", function(ev)
                if type(ev) == "table" and type(ev.item) == "table" and ev.item.type == "tool_call" then
                    completed_names[#completed_names + 1] = tostring(ev.item.name)
                    if ev.item.name == "first_read" then
                        listener_entered_tx:send(true)
                        listener_release_rx:recv()
                    end
                end
            end)

            C.cordis:get("tools"):register("first_read", {
                description = "first parallel tool",
                side_effect = "read",
                concurrency_safe = true,
                parameters = { type = "object", properties = {} },
                execute = function()
                    return { content = { { type = "text", text = "FIRST_RESULT_263" } } }
                end,
            })
            C.cordis:get("tools"):register("second_read", {
                description = "second parallel tool released after first listener blocks",
                side_effect = "read",
                concurrency_safe = true,
                parameters = { type = "object", properties = {} },
                execute = function()
                    second_go_rx:recv()
                    second_done_tx:send(true)
                    return { content = { { type = "text", text = "SECOND_WHILE_LISTENER_BLOCKED_263" } } }
                end,
            })

            local turn = A:prompt("run parallel with blocking completion listener")
            assert(
                mare.task.timeout(5000, function() return listener_entered_rx:recv() end) == true,
                "first_read item/completed listener never ran"
            )
            -- Now first_read's item/completed listener is blocked; let second_read finish.
            second_go_tx:send(true)
            assert(
                mare.task.timeout(5000, function() return second_done_rx:recv() end) == true,
                "second_read did not finish"
            )

            -- Wait with a bounded deadline for second_read's result and completed state to land in
            -- the session journal while first_read's item/completed listener is STILL blocked.
            local path = S:info().path
            local persisted = false
            for _ = 1, 500 do
                local raw = path and mare.fs.read(path) or ""
                local has_result = raw:find("SECOND_WHILE_LISTENER_BLOCKED_263", 1, true) ~= nil
                local has_completed_state = false
                local needle = second_call_id and ('"tool_call_id":"' .. tostring(second_call_id) .. '"') or nil
                for line in raw:gmatch("[^\n]+") do
                    if
                        needle
                        and line:find(needle, 1, true) ~= nil
                        and line:find('"state":"completed"', 1, true) ~= nil
                    then
                        has_completed_state = true
                        break
                    end
                end
                if has_result and has_completed_state then
                    persisted = true
                    break
                end
                mare.task.sleep(10)
            end
            local second_early = #completed_names > 1

            listener_release_tx:send(true)
            local res = A:wait(turn.turn_id)
            assert(res.text == "both published", mare.json.encode(res))
            return persisted, second_early, table.concat(completed_names, ",")
            "#,
        )
        .await;
        h.shutdown().await;

        assert!(
            persisted_while_blocked,
            "completed sibling second_read must be persisted to the session journal even while first_read's item/completed listener is blocked"
        );
        assert!(!second_published_early, "ordered item/completed publication must still wait for first_read");
        assert_eq!(published_order, "first_read,second_read");
    })
    .await;
}
