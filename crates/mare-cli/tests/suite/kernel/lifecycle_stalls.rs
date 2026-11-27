//! A plugin stuck in a lifecycle state is reported, never forced: past `stall_ms` a plugin still
//! LOADING (its apply has not returned), UNLOADING (a cleanup has not finished) or PENDING (a
//! service it injects has no provider) is named with its state, how long it has been there, the
//! labels of the effects it still holds and the services it waits for.

use crate::common::*;

/// A cleanup that never returns, registered under the label "watcher".
const NEVER: &str = r#"return { name = "never", apply = function(ctx)
  ctx:effect(function() return function() local _, rx = mare.task.channel() rx:recv() end end, "watcher")
end }"#;

/// Applies at once, except after the test sets `mare._hang`: then its apply never returns.
const HANG: &str = r#"return { name = "hang", apply = function(ctx)
  if mare._hang then local _, rx = mare.task.channel() rx:recv() end
end }"#;

/// Injects a service nobody provides: it waits (PENDING), which is not a failure.
const WAITS: &str = r#"return { name = "waits", inject = { "no-such-service" }, apply = function() end }"#;

async fn host_with(files: &[(&str, &str)], stall_ms: u64) -> (TestEnv, std::sync::Arc<mare_host::Host>) {
    let env = TestEnv::new().await;
    let dir = env.home.path().join("config/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    for (name, src) in files {
        std::fs::write(dir.join(format!("{name}.lua")), src).unwrap();
    }
    let b = env.host().user_plugins().no_session().config(serde_json::json!({ "stall_ms": stall_ms }));
    let h = mare_host::Host::new(b.opts).await.unwrap();
    (env, h)
}

/// Dispose plugin `name`'s fibers directly (an unload that is not a shutdown).
async fn unload(h: &mare_host::Host, name: &str) {
    h.kernel.expose_test_boot();
    exec(
        h,
        &format!(
            r#"local root = require("mare_kernel.test").boot().test_ctx().cordis
               for _, rt in ipairs(root.registry:values()) do
                 if rt.name == "{name}" then
                   for _, fiber in ipairs(rt.fibers:values()) do fiber:dispose() end
                 end
               end"#
        ),
    )
    .await;
}

async fn stalled_until(h: &mare_host::Host, plugin: &str, state: &str) -> mare_kernel::StalledPlugin {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(WAIT_SECS);
    loop {
        if let Some(s) = h.kernel.stalled().into_iter().find(|s| s.plugin == plugin && s.state == state) {
            return s;
        }
        assert!(std::time::Instant::now() < deadline, "{plugin} never reported {state}: {:?}", h.kernel.stalled());
        // sleep-ok: poll interval while waiting for the report (bounded by WAIT_SECS)
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

// guarantee: unload-stall-reported-with-effect-labels
#[tokio::test]
async fn a_cleanup_that_never_returns_is_reported_with_its_effect_label() {
    local(async {
        let (_env, h) = host_with(&[("never", NEVER)], 100).await;
        unload(&h, "never").await;
        let s = stalled_until(&h, "never", "UNLOADING").await;
        assert!(s.elapsed_ms >= 100, "reported only past the threshold: {s:?}");
        assert!(s.effects.iter().any(|e| e == "watcher"), "names the cleanup it waits for: {s:?}");
        let line = s.message();
        assert!(line.contains("never") && line.contains("unloading") && line.contains("watcher"), "{line}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn nothing_is_reported_before_the_threshold() {
    local(async {
        let (_env, h) = host_with(&[("never", NEVER)], 60_000).await;
        unload(&h, "never").await;
        // sleep-ok: negative window: several watcher ticks pass and still nothing is reported
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(h.kernel.stalled().is_empty(), "{:?}", h.kernel.stalled());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_zero_threshold_turns_the_report_off() {
    local(async {
        let (_env, h) = host_with(&[("never", NEVER)], 0).await;
        unload(&h, "never").await;
        // sleep-ok: negative window: several watcher ticks pass and still nothing is reported
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(h.kernel.stalled().is_empty(), "stall_ms = 0 turns the report off: {:?}", h.kernel.stalled());
        h.shutdown().await;
    })
    .await
}

// guarantee: load-stall-reported-without-abort
#[tokio::test]
async fn an_apply_that_never_returns_is_reported_and_left_running() {
    local(async {
        let (_env, h) = host_with(&[("hang", HANG)], 100).await;
        exec(&h, "mare._hang = true").await;
        let k = h.kernel.clone();
        let reload = tokio::task::spawn_local(async move {
            let _ = k.reload(Some("hang")).await;
        });
        let s = stalled_until(&h, "hang", "LOADING").await;
        assert!(s.message().contains("loading"), "{}", s.message());
        // sleep-ok: negative window: the stall report does not end the reload
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!reload.is_finished(), "observed, never aborted: the reload is still waiting for the apply");
        h.shutdown().await;
    })
    .await
}

// guarantee: pending-stall-reported-as-waiting
#[tokio::test]
async fn a_plugin_left_waiting_names_the_missing_service_and_stays_pending() {
    local(async {
        let (_env, h) = host_with(&[("waits", WAITS)], 100).await;
        let before = h.kernel.problems();
        let s = stalled_until(&h, "waits", "PENDING").await;
        assert_eq!(s.missing, vec!["no-such-service".to_string()], "{s:?}");
        assert!(s.message().contains("no-such-service"), "{}", s.message());
        assert_eq!(h.kernel.problems(), before, "the stall report adds no problem line");
        assert_eq!(h.kernel.plugin_tree_row("waits").unwrap().unwrap().state.as_deref(), Some("PENDING"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_stall_is_logged_and_notified_once() {
    local(async {
        let (_env, h) = host_with(&[("never", NEVER)], 100).await;
        expose(&h);
        exec(
            &h,
            r#"mare._stalls = {}
               C.cordis:on("kernel/stalled", function(ev) table.insert(mare._stalls, ev) end)"#,
        )
        .await;
        unload(&h, "never").await;
        stalled_until(&h, "never", "UNLOADING").await;
        // sleep-ok: negative window: several watcher ticks past the first report add no second one
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let (n, plugin, state, effects): (i64, String, String, Vec<String>) = eval(
            &h,
            r#"local n, ev = 0, nil
               for _, e in ipairs(mare._stalls) do if e.plugin == "never" then n = n + 1; ev = e end end
               return n, ev and ev.plugin or "", ev and ev.state or "", ev and ev.effects or {}"#,
        )
        .await;
        assert_eq!((n, plugin.as_str(), state.as_str()), (1, "never", "UNLOADING"));
        assert_eq!(effects, vec!["watcher".to_string()]);
        h.shutdown().await;
    })
    .await
}
