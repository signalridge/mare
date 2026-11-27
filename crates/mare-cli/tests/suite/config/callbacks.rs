//! Config callbacks across reloads: captured values, retirement and reapplication.

use super::*;

#[tokio::test]
async fn reload_same_line_callback_uses_new_behavior() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT.replace("return 42", "return 43")).unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.cb()").await, 43);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reload_callback_with_changed_captured_value_reapplies_plugin() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let old = USER_INIT
            .replace("mare.plugin(\"probe2\"", "local captured = 42; mare.plugin(\"probe2\"")
            .replace("return 42", "return captured");
        std::fs::write(env.config_dir().join("init.lua"), &old).unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.cb()").await, 42);
        let new = old.replace("captured = 42", "captured = 43");
        std::fs::write(env.config_dir().join("init.lua"), &new).unwrap();
        let r = h.reload_config().await.unwrap();
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.cb()").await, 43);
        assert!(r["reloaded"].as_array().unwrap().contains(&json!("probe2")), "{r}");
        let r = h.reload_config().await.unwrap();
        assert_eq!(r["reloaded"], json!([]), "unchanged captured value: {r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reload_tracks_captured_tables_and_conservatively_reloads_cycles() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let with_table = USER_INIT
            .replace("mare.plugin(\"probe2\"", "local captured = {nested = {value = 42}}; mare.plugin(\"probe2\"")
            .replace("return 42", "return captured.nested.value");
        std::fs::write(env.config_dir().join("init.lua"), &with_table).unwrap();
        h.reload_config().await.unwrap();
        std::fs::write(env.config_dir().join("init.lua"), with_table.replace("value = 42", "value = 43")).unwrap();
        let r = h.reload_config().await.unwrap();
        assert_eq!(r["reloaded"], json!(["probe2"]));
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.cb()").await, 43);
        assert_eq!(h.reload_config().await.unwrap()["reloaded"], json!([]));
        let cyclic = USER_INIT
            .replace("mare.plugin(\"probe2\"", "local captured = {}; captured.self = captured; mare.plugin(\"probe2\"")
            .replace("return 42", "return captured.self == captured and 42 or 0");
        std::fs::write(env.config_dir().join("init.lua"), cyclic).unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(h.reload_config().await.unwrap()["reloaded"], json!(["probe2"]), "cycles cannot be compared safely");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn retired_config_callbacks_survive_only_while_still_referenced() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let cyclic = USER_INIT
            .replace("mare.plugin(\"probe2\"", "local captured = {}; captured.self = captured; mare.plugin(\"probe2\"")
            .replace("return 42", "return captured.self == captured and 42 or 0");
        std::fs::write(env.config_dir().join("init.lua"), cyclic).unwrap();
        h.reload_config().await.unwrap();
        let old = h.kernel.config()["plugins"]["probe2"]["cb"].clone();
        exec(&h, "mare._old_config_callback = mare._cfg_probe2.cb").await;
        for _ in 0..5 {
            h.reload_config().await.unwrap();
        }
        h.lua().gc_collect().unwrap();
        let restored = mare_shared::lua::config_to_lua(h.lua(), &old).unwrap();
        let mlua::Value::Function(f) = restored else { panic!("held old callback was collected") };
        assert_eq!(f.call::<i64>(()).unwrap(), 42);
        drop(f);
        exec(&h, "mare._old_config_callback = nil").await;
        h.lua().gc_collect().unwrap();
        assert!(matches!(mare_shared::lua::config_to_lua(h.lua(), &old).unwrap(), mlua::Value::String(_)));
        let registry: mlua::Table = h.lua().named_registry_value("mare.config_fns").unwrap();
        let strong = registry
            .pairs::<mlua::Value, mlua::Value>()
            .filter(|p| matches!(p, Ok((_, mlua::Value::Function(_)))))
            .count();
        assert!(strong < 5, "old config functions accumulated: {strong}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reload_detects_changes_in_captured_table_identity() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let old = USER_INIT
            .replace("mare.plugin(\"probe2\"", "local a = {x = 1}; local b = {x = 1}; mare.plugin(\"probe2\"")
            .replace("return 42", "return a == b and 1 or 0");
        std::fs::write(env.config_dir().join("init.lua"), &old).unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.cb()").await, 0);
        std::fs::write(env.config_dir().join("init.lua"), old.replace("local b = {x = 1}", "local b = a")).unwrap();
        let changed = h.reload_config().await.unwrap();
        assert_eq!(changed["reloaded"], json!(["probe2"]));
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.cb()").await, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unchanged_callback_using_globals_without_config_modules_does_not_reload() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let init = USER_INIT.replace("return 42", "return mare.version");
        std::fs::write(env.config_dir().join("init.lua"), init).unwrap();
        h.reload_config().await.unwrap();
        let r = h.reload_config().await.unwrap();
        assert_eq!(r["reloaded"], json!([]), "{r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn config_callbacks_follow_committed_config_across_later_reloads() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let init = env.config_dir().join("init.lua");
        let callback = USER_INIT.replace("return 42", "return rawget(mare, 'config').logo");
        std::fs::write(&init, format!("{callback}\nmare.setup {{logo='compact'}}")).unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(eval::<String>(&h, "return mare._cfg_probe2.cb()").await, "compact");
        std::fs::write(&init, format!("{callback}\nmare.setup {{logo='other'}}")).unwrap();
        let result = h.reload_config().await.unwrap();
        assert_eq!(result["reloaded"], json!([]), "unchanged callback: {result}");
        assert_eq!(eval::<String>(&h, "return mare._cfg_probe2.cb()").await, "other");
        h.shutdown().await;
    })
    .await
}
