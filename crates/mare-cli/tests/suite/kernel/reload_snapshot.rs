//! Failures reported to plugins while the host is reloading.

use crate::common::*;

use super::{host, write_plugin};

const WAITING: &str = r#"return { name = "snapshot-target", inject = { "snapshot-missing" }, apply = function() end }"#;
const RUNNING: &str = r#"return { name = "snapshot-target", apply = function() end }"#;
const OBSERVER: &str = r#"
return { name = "snapshot-observer", apply = function(ctx)
  ctx:on("kernel/reloaded", function()
    local failures = mare.plugins.failures()
    mare._snapshot_during = { name = failures[1] and failures[1].plugin or "none",
      count = #failures, active = mare.plugins.status("snapshot-target").active }
  end)
  ctx:on("reload/completed", function()
    local failures = mare.plugins.failures()
    mare._snapshot_done = { name = failures[1] and failures[1].plugin or "none", count = #failures }
  end)
end }
"#;

#[tokio::test]
async fn reload_snapshot_mid_reload_uses_the_pre_reload_failures_even_when_the_tree_is_live() {
    local(async {
        let env = TestEnv::new().await;
        let target = write_plugin(&env, "snapshot-target", WAITING);
        write_plugin(&env, "snapshot-observer", OBSERVER);
        let h = host(&env).await;
        assert_eq!(eval::<String>(&h, "return mare.plugins.failures()[1].plugin").await, "snapshot-target");
        std::fs::write(target, RUNNING).unwrap();
        h.reload_all().await.unwrap();
        let during: (String, usize, bool) =
            eval(&h, "return mare._snapshot_during.name, mare._snapshot_during.count, mare._snapshot_during.active")
                .await;
        h.shutdown().await;
        assert_eq!(during, ("snapshot-target".to_string(), 1, true));
    })
    .await
}

#[tokio::test]
async fn reload_snapshot_done_listener_reads_live_failures_after_the_reload() {
    local(async {
        let env = TestEnv::new().await;
        let target = write_plugin(&env, "snapshot-target", WAITING);
        write_plugin(&env, "snapshot-observer", OBSERVER);
        let h = host(&env).await;
        std::fs::write(target, RUNNING).unwrap();
        h.reload_all().await.unwrap();
        let done: (String, usize) = eval(&h, "return mare._snapshot_done.name, mare._snapshot_done.count").await;
        let after: usize = eval(&h, "return #mare.plugins.failures()").await;
        h.shutdown().await;
        assert_eq!(done, ("none".to_string(), 0));
        assert_eq!(after, 0);
    })
    .await
}

#[tokio::test]
async fn reload_snapshot_is_cleared_after_a_failed_definition_is_rolled_back() {
    local(async {
        let env = TestEnv::new().await;
        let target = write_plugin(&env, "snapshot-target", WAITING);
        let rollback = write_plugin(
            &env,
            "snapshot-rollback",
            r#"return { name = "snapshot-rollback", apply = function() mare._rollback_old = (mare._rollback_old or 0) + 1 end }"#,
        );
        let h = host(&env).await;
        std::fs::write(target, RUNNING).unwrap();
        // a definition that loads but whose apply fails: the reload puts the old one back (it runs
        // again), not the prepare step keeping the running definition
        std::fs::write(&rollback, r#"return { name = "snapshot-rollback", apply = function() error("rollback v2 broken") end }"#)
            .unwrap();
        let report = h.reload_all().await.unwrap();
        let state: (usize, i64) = eval(&h, "return #mare.plugins.failures(), mare._rollback_old").await;
        h.shutdown().await;
        assert!(report["notices"].to_string().contains("snapshot-rollback"), "{report}");
        assert_eq!(state, (0, 2), "no snapshot left behind, and the old definition applied again");
    })
    .await
}

#[tokio::test]
async fn reload_snapshot_is_cleared_after_a_full_reload_returns_an_error() {
    local(async {
        let env = TestEnv::new().await;
        let target = write_plugin(&env, "snapshot-target", RUNNING);
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, "mare.setup { model = 'faux/echo' }").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        expose(&h);
        assert_eq!(eval::<usize>(&h, "return #mare.plugins.failures()").await, 0);
        std::fs::write(&init, "this is invalid Lua!").unwrap();
        let result = h.reload_all().await;
        assert!(result.is_err(), "{result:?}");
        // a failure after the reload ended is seen: no snapshot was left behind
        std::fs::write(target, WAITING).unwrap();
        h.kernel.reload(Some("snapshot-target")).await.unwrap();
        let failure: String = eval(&h, "local f = mare.plugins.failures()[1]; return f and f.plugin or 'none'").await;
        h.shutdown().await;
        assert_eq!(failure, "snapshot-target");
    })
    .await
}
