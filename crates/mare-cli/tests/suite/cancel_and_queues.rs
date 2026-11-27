//! Behavioral tests ("取消与队列" / shutdown grace & reliable `__close`).

use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn shutdown_grace_ms_does_not_truncate_process_tree_termination() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({ "shutdown_grace_ms": 50 })).build().await;
        expose(&h);
        global(&h, "FAKE", fake_mcp_bin());
        global(&h, "DIR", env.home.path().to_string_lossy().to_string());

        exec(
            &h,
            r#"
            C.cordis:on("app/shutdown", function()
                local _, never_rx = mare.task.channel()
                never_rx:recv()
            end)
            P = mare.proc.spawn({ cmd = FAKE, args = { "tree", DIR .. "/tree.pid" } })
            "#,
        )
        .await;

        let root_pid: u32 = eval(&h, "return P:pid()").await;
        let pid_file = env.home.path().join("tree.pid");
        let mut grandchild_pid = 0;
        wait_until(WAIT_SECS, "grandchild pid written", || {
            let text = std::fs::read_to_string(&pid_file).unwrap_or_default();
            grandchild_pid = if text.ends_with('\n') { text.trim().parse().unwrap_or(0) } else { 0 };
            async move { grandchild_pid != 0 }
        })
        .await;
        assert!(pid_alive(root_pid) && pid_alive(grandchild_pid));

        let report = h.shutdown_report().await;
        assert!(report.stop_unconfirmed(), "cooperative timeout must be reported as stop_unconfirmed: {report:?}");
        assert!(
            report.unconfirmed_tasks.contains(&"cooperative_shutdown"),
            "expected cooperative_shutdown in unconfirmed_tasks: {report:?}"
        );
        assert!(
            report.unconfirmed_trees.is_empty(),
            "process tree termination must complete even after shutdown_grace_ms expires: {report:?}"
        );
        wait_until(5, "process tree ended after untruncated shutdown", || async move {
            !pid_alive(root_pid) && !pid_alive(grandchild_pid)
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn close_unlock_and_cleanup_deliver_when_cancelled() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local lock_tx, lock_rx = mare.task.channel()
            local cleanup_tx, cleanup_rx = mare.task.channel()
            -- Pre-fill cleanup queue so delivery must not depend on an empty queue or yielding.
            for i = 1, 8 do
                cleanup_tx:send("pre-" .. i)
            end
            lock_tx:send(true)

            local timed_out = not mare.task.timeout(10, function()
                lock_rx:recv()
                local _guard <close> = setmetatable({}, {
                    __close = function()
                        assert(cleanup_tx:send("cleanup-from-close"))
                        assert(lock_tx:send(true))
                    end,
                })
                local _, never_rx = mare.task.channel()
                never_rx:recv()
            end)
            assert(timed_out, "expected inner task to be cancelled by timeout")

            -- Lock must be immediately acquirable after cancellation.
            local got_lock = lock_rx:try_recv()
            assert(got_lock == true, "lock was not released by __close")

            local saw_cleanup = false
            while true do
                local v = cleanup_rx:try_recv()
                if v == nil then break end
                if v == "cleanup-from-close" then
                    saw_cleanup = true
                end
            end
            return saw_cleanup
            "#,
        )
        .await;
        assert!(ok, "__close must reliably deliver unlock and cleanup signals");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn bounded_task_channel_enforces_capacity_and_rejects_invalid() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local ok0 = pcall(mare.task.channel, 0)
            local ok_neg = pcall(mare.task.channel, -1)
            assert(not ok0 and not ok_neg, "non-positive capacity must fail")

            local tx, rx = mare.task.channel(1)
            assert(tx:try_send("first") == true)
            assert(tx:try_send("second") == false, "bounded channel must reject try_send when full")

            local started_tx, started_rx = mare.task.channel()
            local sent_after_drain = false
            local waiter = mare.task.spawn(function()
                started_tx:send(true)
                assert(tx:send("third") == true)
                sent_after_drain = true
            end)
            assert(started_rx:recv() == true)
            mare.task.yield()
            assert(not sent_after_drain, "bounded send must wait while full")
            assert(rx:recv() == "first")
            assert(rx:recv() == "third")
            waiter:wait()
            return sent_after_drain
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn task_channel_close_wakes_concurrent_waiting_recv_for_unbounded_and_bounded() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            for _, cap in ipairs({ false, 2 }) do
                local tx, rx = mare.task.channel(cap or nil)
                local entered_tx, entered_rx = mare.task.channel()
                local recv_done = false
                local recv_val = "sentinel"
                local waiter = mare.task.spawn(function()
                    entered_tx:send(true)
                    recv_val = rx:recv()
                    recv_done = true
                end)

                assert(entered_rx:recv() == true)
                -- Yield so waiter advances into rx:recv() and holds the receiver lock.
                mare.task.yield()
                assert(not recv_done)

                rx:close()
                assert(tx:is_closed() == true, "is_closed must become true immediately on close()")
                assert(tx:send("after-close") == false, "send after close must return false")
                if cap then
                    assert(tx:try_send("after-close") == false, "try_send after close must return false")
                end

                local finished = mare.task.timeout(1000, function()
                    waiter:wait()
                end)
                assert(finished, "close() must wake waiting recv() even while recv holds lock")
                assert(recv_done and recv_val == nil)
            end

            -- Bounded full send() must also wake and return false when rx:close() is called.
            local btx, brx = mare.task.channel(1)
            assert(btx:try_send("one") == true)
            local send_entered_tx, send_entered_rx = mare.task.channel()
            local send_res = nil
            local sender_task = mare.task.spawn(function()
                send_entered_tx:send(true)
                send_res = btx:send("two")
            end)
            assert(send_entered_rx:recv() == true)
            mare.task.yield()
            brx:close()
            local send_finished = mare.task.timeout(1000, function()
                sender_task:wait()
            end)
            assert(send_finished, "close() must wake blocked bounded send()")
            assert(send_res == false)
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn inputs_lock_cancel_race_does_not_leak_lock_token() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);

        let ok: bool = eval(
            &h,
            r#"
            local block_commit = false
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local queued_texts = {}

            C.cordis:on("queue/changed", function(snap)
                queued_texts = {}
                for _, item in ipairs(snap.items or {}) do
                    if item.lane == "follow_up" then
                        queued_texts[item.text] = true
                    end
                end
            end)

            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get("policy"):voter("request/ready", function()
                if hold_tx then
                    local tx = hold_tx
                    hold_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = "deny", reason = "gate" }
                end
            end)
            local held_turn = A:prompt("hold")
            hold_rx:recv()

            C.cordis:get("policy"):voter("session/commit-ready", function()
                if block_commit then
                    entered_tx:send(true)
                    release_rx:recv()
                end
                return { decision = "allow" }
            end, { prepend = true })

            for _, order in ipairs({ "cancel_first", "release_first" }) do
                block_commit = true
                local holder = mare.task.spawn(function()
                    A:prompt("holder-" .. order, { when_busy = "queue" })
                end)
                assert(entered_rx:recv() == true)
                block_commit = false

                local tok = mare.task.token()
                local racer_started_tx, racer_started_rx = mare.task.channel()
                local racer_done = false
                local racer_ok, racer_err
                local racer = mare.task.spawn(function()
                    racer_started_tx:send(true)
                    racer_ok, racer_err = pcall(function()
                        A:prompt("racer-" .. order, { when_busy = "queue", cancel = tok })
                    end)
                    racer_done = true
                end)

                -- Confirm racer reached lock(cancel) -> mare.task.select(rx:recv(), cancel:wait()) and is blocked.
                assert(racer_started_rx:recv() == true)
                mare.task.yield()
                assert(not racer_done, "racer must be waiting on inputs lock before race: " .. order)

                if order == "cancel_first" then
                    tok:cancel()
                    release_tx:send(true)
                else
                    release_tx:send(true)
                    tok:cancel()
                end

                assert(mare.task.timeout(1000, function() holder:wait() end), "holder timed out for " .. order)
                assert(mare.task.timeout(1000, function() racer:wait() end), "racer timed out for " .. order)
                assert(not racer_ok, "racer must fail with cancellation for " .. order)
                assert(
                    tostring(racer_err):find("cancelled", 1, true) ~= nil,
                    "expected cancelled error for " .. order .. ", got: " .. tostring(racer_err)
                )

                -- Subsequent queue mutation must still acquire the lock without deadlocking.
                local after_ok = mare.task.timeout(1000, function()
                    A:prompt("after-" .. order, { when_busy = "queue" })
                end)
                assert(after_ok, "inputs lock token leaked after cancel race: " .. order)

                assert(queued_texts["holder-" .. order], "holder input missing from queue for " .. order)
                assert(not queued_texts["racer-" .. order], "cancelled racer input must not be enqueued for " .. order)
                assert(queued_texts["after-" .. order], "after input missing from queue for " .. order)
            end

            A:cancel({ turn_id = held_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(held_turn.turn_id)
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn shutdown_hook_timeout_does_not_skip_plugin_release() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("test_release.lua"),
            r#"
            return {
                name = "test_release",
                apply = function(ctx)
                    mare._plugin_released = false
                    ctx:effect(function()
                        return function() mare._plugin_released = true end
                    end)
                end,
            }
            "#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().config(json!({ "shutdown_grace_ms": 50 })).build().await;
        expose(&h);

        exec(
            &h,
            r#"
            C.cordis:on("app/shutdown", function()
                local _, never_rx = mare.task.channel()
                never_rx:recv()
            end)
            "#,
        )
        .await;

        let report = h.shutdown_report().await;
        assert!(report.stop_unconfirmed(), "{report:?}");

        let released: bool = eval(&h, "return mare._plugin_released == true").await;
        assert!(released, "plugin release (kernel.shutdown) was skipped when app/shutdown timed out");
    })
    .await;
}

/// Two user plugins: `a_released` records its release; `z_stuck` (activated after it, so
/// released first) never finishes its release.
fn stuck_and_released_plugins(env: &TestEnv) {
    let plugins_dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("a_released.lua"),
        r#"return { name = "a_released", apply = function(ctx)
            mare._a_released = false
            ctx:effect(function() return function() mare._a_released = true end end)
        end }"#,
    )
    .unwrap();
    std::fs::write(
        plugins_dir.join("z_stuck.lua"),
        r#"return { name = "z_stuck", apply = function(ctx)
            ctx:effect(function() return function() local _, never = mare.task.channel() never:recv() end end)
        end }"#,
    )
    .unwrap();
}

/// one plugin whose release never finishes must not keep the plugins after it
/// from being released; the stuck one is reported unconfirmed. Reference: Codex
/// core/tests/suite/shutdown (each MCP client shut down independently; one hung server does not
/// block the rest) — mare's plugins are released in reverse activation order.
#[tokio::test]
async fn a_stuck_plugin_release_does_not_skip_the_plugins_after_it() {
    local(async {
        let env = TestEnv::new().await;
        stuck_and_released_plugins(&env);
        let h = env.host().user_plugins().no_session().config(json!({ "shutdown_grace_ms": 200 })).build().await;
        expose(&h);
        let report = h.shutdown_report().await;
        assert!(report.stop_unconfirmed(), "{report:?}");
        let released: bool = eval(&h, "return mare._a_released == true").await;
        assert!(released, "the release after the stuck one was skipped: {report:?}");
    })
    .await;
}

/// `shutdown_grace_ms` is one budget for the whole cooperative shutdown,
/// not a fresh grace for every stage.
#[tokio::test]
async fn shutdown_grace_is_one_budget_for_all_cooperative_stages() {
    local(async {
        let env = TestEnv::new().await;
        stuck_and_released_plugins(&env);
        let h = env.host().user_plugins().no_session().config(json!({ "shutdown_grace_ms": 2500 })).build().await;
        expose(&h);
        // a second stage that also never finishes
        exec(&h, r#"C.cordis:on("app/shutdown", function() local _, never = mare.task.channel() never:recv() end)"#)
            .await;
        let t0 = std::time::Instant::now();
        let report = h.shutdown_report().await;
        let elapsed = t0.elapsed();
        assert!(report.stop_unconfirmed(), "{report:?}");
        // two stuck stages would take >= 5 s unshared; one shared 2.5 s budget ends well under 4.5 s
        assert!(elapsed < std::time::Duration::from_millis(4500), "cooperative shutdown took {elapsed:?}");
    })
    .await;
}
