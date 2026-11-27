//! Diagnostics: the cordis tree reports what the old path did.
//!
//! A plugin left PENDING on a required service that nothing provides is reported with that
//! service, as the old path did ("plugin X not started: required service `Y` is not available"),
//! not as "setup failed: unknown error" (plugin e2e
//! skip probes match on the wording).

use crate::common::*;

#[tokio::test]
async fn a_plugin_waiting_for_a_missing_service_names_it() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("needs-missing-service.lua"),
            r#"return { name = "needs-missing-service", inject = { "missing_test_service" }, apply = function() end }"#,
        )
        .unwrap();
        let b = env.host().user_plugins().no_session();
        let h = mare_host::Host::new(b.opts).await.unwrap();
        expose(&h);
        let problems = h.kernel.problems();
        let failure: String = eval(
            &h,
            "for _, f in ipairs(mare.plugins.failures()) do
               if f.plugin == 'needs-missing-service' then return f.phase .. ' | ' .. f.summary end
             end
             return 'none'",
        )
        .await;
        h.shutdown().await;
        let msg = "plugin needs-missing-service not started: required service `missing_test_service` is not available";
        assert!(problems.iter().any(|p| p == msg), "{problems:?}");
        assert!(failure.starts_with("start | "), "{failure}");
        assert!(failure.contains("required service `missing_test_service` is not available"), "{failure}");
        assert!(!failure.contains("unknown error"), "{failure}");
    })
    .await
}

/// A plugin refused by the same-name rule (no `override = true`) is listed by `mare plugin tree` as a
/// refused row (the host's warning already names it; the dump had no row for it).
#[test]
fn mare_plugins_lists_a_same_name_refusal() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("config/plugins/usage");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("init.lua"), r#"return { name = "usage", apply = function() end }"#).unwrap();
    let out = mare_cmd()
        .args(["plugin", "tree", "--json"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stdin(std::process::Stdio::null())
        .run_output();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let dump: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows: Vec<&serde_json::Value> = dump.as_array().unwrap().iter().filter(|r| r["id"] == "usage").collect();
    assert!(rows.iter().any(|r| r["origin"] == "builtin" && r["refused"].is_null()), "the built-in runs: {rows:?}");
    let refused = rows.iter().find(|r| r["origin"] == "user").expect("a row for the refused user plugin");
    let why = refused["refused"].as_str().unwrap_or_default();
    assert!(why.contains("collides with the built-in `usage`") && why.contains("override = true"), "{why}");
}
