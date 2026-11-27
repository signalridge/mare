//! Closing and reloading the cordis tree: what close and shutdown wait for and report, and what
//! a full reload keeps (it restarts every plugin, never replays startup, keeps the session).

use crate::common::*;

const STUCK: &str = r#"return { name = "stuck", apply = function(ctx)
  ctx:effect(function() return function() mare.task.sleep(1500) end end)
end }"#;
const QUICK: &str = r#"return { name = "quick", apply = function(ctx)
  ctx:effect(function() return function() end end)
end }"#;

/// Bounded shutdown, boot side: `booted.close()` returns the tree's close Future and
/// `booted.unsettled()` names what has not finished unloading, so the kernel can stop waiting at
/// its deadline and report the rest as unconfirmed.
// guarantee: shutdown-is-bounded-and-names-unfinished-cleanups
#[tokio::test]
async fn close_lists_the_plugins_whose_cleanup_has_not_finished() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stuck.lua"), STUCK).unwrap();
        std::fs::write(dir.join("quick.lua"), QUICK).unwrap();
        let b = env.host().user_plugins().no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        h.kernel.expose_test_boot();
        let (before, during, same): (Vec<String>, Vec<String>, bool) = eval(
            &h,
            r#"local booted = require("mare_kernel.test").boot()
               local before = booted.unsettled()
               local f = booted.close()
               -- the quick cleanup finishes within a second even under load; the stuck one sleeps 1.5 s
               local function has(list, name) for _, n in ipairs(list) do if n == name then return true end end end
               local now = booted.unsettled()
               for _ = 1, 100 do
                 if not has(now, "quick") then break end
                 mare.task.sleep(10)
                 now = booted.unsettled()
               end
               return before, now, rawequal(f, booted.close())"#,
        )
        .await;
        assert!(before.contains(&"stuck".to_string()) && before.contains(&"quick".to_string()), "{before:?}");
        assert!(same, "close() returns the same Future when called again");
        assert!(during.contains(&"stuck".to_string()), "a cleanup still running is unsettled: {during:?}");
        assert!(!during.contains(&"quick".to_string()), "a finished one is not: {during:?}");
        h.shutdown().await;
    })
    .await
}

/// A plugin whose unload began before the close (a reload, a plugin turned off) has left its
/// runtime's list; the close report still names it while it unloads.
// guarantee: shutdown-is-bounded-and-names-unfinished-cleanups
#[tokio::test]
async fn an_unload_begun_before_the_close_still_counts_as_unsettled() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stuck.lua"), STUCK).unwrap();
        let b = env.host().user_plugins().no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        h.kernel.expose_test_boot();
        let unsettled: Vec<String> = eval(
            &h,
            r#"local booted = require("mare_kernel.test").boot()
               local root = booted.test_ctx().cordis
               for _, rt in ipairs(root.registry:values()) do
                 if rt.name == "stuck" then
                   for _, fiber in ipairs(rt.fibers:values()) do fiber:dispose() end
                 end
               end
               booted.close()
               -- the others' cleanups finish at once; the stuck one sleeps 1.5 s
               mare.task.sleep(300)
               return booted.unsettled()"#,
        )
        .await;
        h.shutdown().await;
        assert!(unsettled.contains(&"stuck".to_string()), "the unload in flight is not named: {unsettled:?}");
    })
    .await
}

const NEVER: &str = r#"return { name = "never", apply = function(ctx)
  ctx:effect(function() return function() local _, rx = mare.task.channel() rx:recv() end end)
end }"#;
const DONE: &str = r#"return { name = "done", apply = function(ctx)
  ctx:effect(function() return function() mare._done_cleanup = true end end)
end }"#;

/// Bounded shutdown, kernel side: `shutdown_until` closes the tree, stops waiting at its
/// deadline although one plugin's cleanup never returns, and reports that plugin (and only it)
/// as unconfirmed; the others have unloaded.
// guarantee: shutdown-is-bounded-and-names-unfinished-cleanups
#[tokio::test]
async fn shutdown_until_stops_at_the_deadline_and_names_the_cleanup_that_never_returns() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("never.lua"), NEVER).unwrap();
        std::fs::write(dir.join("done.lua"), DONE).unwrap();
        let b = env.host().user_plugins().no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        let started = std::time::Instant::now();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        let unconfirmed = h.kernel.shutdown_until(deadline).await;
        let took = started.elapsed();
        assert!(took < std::time::Duration::from_secs(5), "returned at the deadline, not after: {took:?}");
        assert!(unconfirmed.contains(&"never".to_string()), "{unconfirmed:?}");
        assert!(!unconfirmed.contains(&"done".to_string()), "{unconfirmed:?}");
        let done: bool = eval(&h, "return mare._done_cleanup == true").await;
        assert!(done, "the other plugins unloaded");
    })
    .await
}

/// `-r <id>`, then a new session, then a full `/reload`:
/// - a full reload restarts every plugin without firing app/ready again: the session plugin does
///   not run its startup again (no second `session/start`);
/// - the reload keeps the session the user moved to: neither the command line's `-r` session
///   again (the old path replayed startup) nor an empty one (the restarted session plugin kept
///   nothing).
// guarantee: full-reload-keeps-startup-and-session
#[tokio::test]
async fn a_full_reload_neither_replays_startup_nor_leaves_the_session() {
    local(async {
        let env = TestEnv::new().await;
        let script = serde_json::json!([{ "text": "one" }, { "text": "two" }, { "text": "three" }]);
        let first = env.host().faux(script.clone()).build().await;
        expose(&first);
        first.prompt("first", serde_json::json!({})).await.unwrap();
        let resumed_id: String = eval(&first, "return C.cordis:get('session'):info().session_id").await;
        first.shutdown().await;

        let h = env.host().faux(script).args(serde_json::json!({ "resume": resumed_id })).build().await;
        expose(&h);
        assert_eq!(
            eval::<String>(&h, "return C.cordis:get('session'):info().session_id").await,
            resumed_id,
            "-r opened the session"
        );
        exec(&h, "C.cordis:get('session'):create()").await;
        h.prompt("second", serde_json::json!({})).await.unwrap();
        let new_id: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;
        assert_ne!(new_id, resumed_id);
        exec(&h, r#"mare._starts = 0 C.cordis:on("session/started", function() mare._starts = mare._starts + 1 end)"#)
            .await;
        h.reload_all().await.unwrap();
        let (after, starts): (String, i64) =
            eval(&h, "return C.cordis:get('session'):info().session_id, mare._starts").await;
        h.shutdown().await;
        assert_eq!(starts, 0, "the reload fired session/start again");
        assert_eq!(after, new_id, "the reload left the session the user was in");
    })
    .await
}

/// `/reload config` restarts the session plugin while the user is in A (its
/// unload remembers A) but emits no `kernel/reloaded`; the user then resumes B. A later reload of
/// an unrelated plugin emits `kernel/reloaded`: the session the user moved to stays, A is not
/// reopened.
#[tokio::test]
async fn a_config_reloads_remembered_session_does_not_replace_the_one_resumed_after_it() {
    local(async {
        let env = TestEnv::new().await;
        let script = serde_json::json!([{ "text": "one" }, { "text": "two" }, { "text": "three" }]);
        let init = |poll: u32| format!(r#"mare.plugin("session", {{ transcript_poll_ms = {poll} }})"#);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), init(500)).unwrap();
        let h = env.host().user_config(false).faux(script).build().await;
        expose(&h);
        h.prompt("in A", serde_json::json!({})).await.unwrap();
        let (a_id, a_path): (String, String) =
            eval(&h, "local i = C.cordis:get('session'):info() return i.session_id, i.path").await;
        exec(&h, "C.cordis:get('session'):create()").await;
        h.prompt("in B", serde_json::json!({})).await.unwrap();
        let (b_id, b_path): (String, String) =
            eval(&h, "local i = C.cordis:get('session'):info() return i.session_id, i.path").await;
        exec(&h, &format!("C.cordis:get('session'):load({{ path = {a_path:?} }})")).await;
        assert_eq!(eval::<String>(&h, "return C.cordis:get('session'):info().session_id").await, a_id, "back in A");

        std::fs::write(env.config_dir().join("init.lua"), init(501)).unwrap();
        let r = h.reload_config().await.unwrap();
        assert!(r["reloaded"].as_array().unwrap().iter().any(|n| n == "session"), "{r}");
        exec(&h, &format!("C.cordis:get('session'):load({{ path = {b_path:?} }})")).await;
        assert_eq!(eval::<String>(&h, "return C.cordis:get('session'):info().session_id").await, b_id, "resumed B");
        h.kernel.reload(Some("notify")).await.unwrap();
        let after: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;
        h.shutdown().await;
        assert_eq!(after, b_id, "the reload of another plugin reopened the session remembered before B (A = {a_id})");
    })
    .await
}

/// the remembered session belongs to the reload that restarted the session
/// plugin. `/reload config` restarts it (while the user is in A) without a `kernel/reloaded`; the
/// next, unrelated reload does not take the record for its own and leaves the session as it is.
#[tokio::test]
async fn a_config_reloads_remembered_session_is_not_reopened_by_a_later_unrelated_reload() {
    local(async {
        let env = TestEnv::new().await;
        let script = serde_json::json!([{ "text": "one" }]);
        let init = |poll: u32| format!(r#"mare.plugin("session", {{ transcript_poll_ms = {poll} }})"#);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), init(500)).unwrap();
        let h = env.host().user_config(false).faux(script).build().await;
        expose(&h);
        h.prompt("in A", serde_json::json!({})).await.unwrap();
        let a_id: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;
        std::fs::write(env.config_dir().join("init.lua"), init(501)).unwrap();
        let r = h.reload_config().await.unwrap();
        assert!(r["reloaded"].as_array().unwrap().iter().any(|n| n == "session"), "{r}");
        let before: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;
        h.kernel.reload(Some("notify")).await.unwrap();
        let after: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;
        h.shutdown().await;
        assert_eq!(after, before, "an unrelated reload switched sessions (A = {a_id})");
    })
    .await
}

/// the record is stamped with the generation of the reload
/// that unloaded the session plugin, at unload. A reload after which the session plugin does not
/// run (here: disabled) leaves that reload's record behind; the next reload, in which it starts again, is
/// a different reload and must not reopen the old session from it.
#[tokio::test]
async fn a_record_left_by_a_reload_the_session_plugin_did_not_run_after_is_not_reopened_later() {
    local(async {
        let env = TestEnv::new().await;
        let script = serde_json::json!([{ "text": "one" }]);
        let init = |poll: &str| format!(r#"mare.plugin("session", {{ transcript_poll_ms = {poll} }})"#);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), init("500")).unwrap();
        let h = env.host().user_config(false).faux(script).build().await;
        expose(&h);
        h.prompt("in A", serde_json::json!({})).await.unwrap();
        let a_id: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;
        // round 1: the session plugin is unloaded (it saves A) and no new instance starts
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.plugin("session", { enabled = false })"#).unwrap();
        let _ = h.reload_all().await;
        assert!(h.service_table("session").is_none(), "the session plugin should not be running after round 1");
        // round 2, a later reload: the plugin starts again; A belongs to round 1
        std::fs::write(env.config_dir().join("init.lua"), init("501")).unwrap();
        h.reload_all().await.unwrap();
        expose(&h);
        let after: String = eval(&h, "return C.cordis:get('session'):info().session_id").await;
        h.shutdown().await;
        assert_ne!(after, a_id, "a later reload reopened the session saved by the reload the plugin did not run after");
    })
    .await
}

#[tokio::test]
async fn before_reload_listener_error_lands_in_the_reload_report() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/before_reload_err.lua"),
            r#"return {
                name = "before-reload-err",
                apply = function(ctx)
                    ctx:on("kernel/before-reload", function()
                        error("BEFORE_RELOAD_BOOM")
                    end)
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().builtins(&["copy"]).user_plugins().no_session().build().await;
        let report = h.kernel.reload(Some("copy")).await.unwrap();
        let joined = report.join("\n");
        assert!(
            joined.contains("BEFORE_RELOAD_BOOM") || h.kernel.problems().iter().any(|p| p.contains("BEFORE_RELOAD_BOOM")),
            "kernel/before-reload listener error must surface in reload report/problems: report={report:?} problems={:?}",
            h.kernel.problems()
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn before_reload_is_awaited_before_disposal() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/before_reload_order.lua"),
            r#"return {
                name = "before-reload-order",
                apply = function(ctx)
                    mare._reload_order = mare._reload_order or {}
                    ctx:on("kernel/before-reload", function(ev)
                        if ev and ev.plugins then
                            for _, name in ipairs(ev.plugins) do
                                if name == "before-reload-order" then
                                    mare.task.sleep(30)
                                    mare._reload_order[#mare._reload_order + 1] = "before-done"
                                end
                            end
                        end
                    end)
                    ctx:effect(function()
                        return function()
                            mare._reload_order[#mare._reload_order + 1] = "disposed"
                        end
                    end)
                end,
            }"#,
        )
        .unwrap();
        let h = env.host().user_plugins().no_session().build().await;
        h.kernel.reload(Some("before-reload-order")).await.unwrap();
        let order: String = eval(&h, "return table.concat(mare._reload_order or {}, ',')").await;
        assert_eq!(order, "before-done,disposed", "kernel/before-reload must finish before plugin disposal");
        h.shutdown().await;
    })
    .await
}
