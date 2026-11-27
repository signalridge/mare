//! Concurrent runtime changes while a config or plugin reload is in flight.

use super::*;

#[tokio::test]
async fn failed_yielding_config_does_not_remove_concurrent_runtime_registrations() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { logo = false }
            mare.keymap.set('editor', '<C-k>', 'config-only')
            mare.ui.renderer('test/config-owned', { id = 'config-only' })
            mare.on('test/config-owner', function() mare._config_event = true end)
            mare._config_test_started = true
            for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end
            error('config failure sentinel')
            "#,
        )
        .unwrap();
        let (reload, ()) = tokio::join!(h.reload_config(), async {
            wait_flag(&h, "_config_test_started").await;
            assert!(eval::<bool>(&h, "return rawget(mare.config, 'logo') == nil").await);
            exec(
                &h,
                r#"mare.keymap.action('runtime-only', function() end)
                   mare.keymap.set('editor', '<C-j>', 'runtime-only')
                   mare.ui.renderer('test/runtime-owned', { id = 'runtime-only' })
                   mare.on('test/config-owner', function() mare._runtime_event = true end)"#,
            )
            .await;
            release_reload(&h).await;
        });
        assert!(reload.unwrap_err().to_string().contains("config failure sentinel"));
        assert!(eval::<bool>(&h, "return rawget(mare.config, 'logo') == nil").await);
        assert_eq!(
            eval::<(i64, String)>(
                &h,
                "local n, a = 0, ''; for _, e in ipairs(mare.keymap.list({ context = 'editor' })) do if e.key == 'ctrl+j' then n = n + 1; a = e.action end end; return n, a"
            )
            .await,
            (1, "runtime-only".to_string())
        );
        assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('test/runtime-owned').id").await, "runtime-only");
        assert!(eval::<bool>(&h, "return mare.ui.get_renderer('test/config-owned') == nil").await);
        h.kernel.emit_json("test/config-owner", json!({})).await;
        assert!(eval::<bool>(&h, "return mare._runtime_event == true and mare._config_event == nil").await);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn yielding_config_publishes_managed_ui_only_after_success() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, format!("{USER_INIT}\nmare.keymap.set('editor', '<C-k>', 'redraw')\nmare.ui.renderer('test/staged', {{id='old'}})\nmare._old_status=mare.ui.status('test_staged', {{render=function() mare._staged_status='old'; return 'old' end}})")).unwrap();
        h.reload_config().await.unwrap();
        std::fs::write(&init, format!("{USER_INIT}\nmare.keymap.set('editor', '<C-k>', 'transcript')\nmare.ui.renderer('test/staged', {{id='new'}})\nmare._new_status=mare.ui.status('test_staged', {{render=function() mare._staged_status='new'; return 'new' end}})\nmare._staged_started = true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end")).unwrap();
        let (result, ()) = tokio::join!(h.reload_config(), async {
            wait_flag(&h, "_staged_started").await;
            assert_eq!(eval::<String>(&h, "return mare.keymap.resolve('editor', '<C-k>').candidates[1].action").await, "redraw");
            assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('test/staged').id").await, "old");
            assert_eq!(eval::<String>(&h, "mare.ui.status_redraw('test_staged'); return mare._staged_status").await, "old");
            release_reload(&h).await;
        });
        result.unwrap();
        assert_eq!(eval::<String>(&h, "return mare.keymap.resolve('editor', '<C-k>').candidates[1].action").await, "transcript");
        assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('test/staged').id").await, "new");
        assert_eq!(eval::<String>(&h, "mare.ui.status_redraw('test_staged'); return mare._staged_status").await, "new");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn yielding_config_option_reads_stay_on_the_callers_version() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, format!("{USER_INIT}\nmare.setup {{ logo = 'compact' }}")).unwrap();
        h.reload_config().await.unwrap();
        exec(&h, "mare._old_logo_source = mare.config_sources().logo.where; mare._cached_options = mare.o; mare._cached_theme = mare.o.theme").await;
        std::fs::write(&init, format!("{USER_INIT}\n\nmare.setup {{ logo = false }}\nassert(mare.o.logo == false and mare.config.logo == false and rawget(mare, 'config').logo == false); assert(mare.config_sources().logo.where ~= mare._old_logo_source); mare._option_stage_started=true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end; assert(mare.o.logo == false)")).unwrap();
        let (result, ()) = tokio::join!(h.reload_config(), async {
            wait_flag(&h, "_option_stage_started").await;
            assert_eq!(eval::<String>(&h, "return mare.o.logo").await, "compact");
            assert_eq!(eval::<String>(&h, "return rawget(rawget(mare, 'config'), 'logo')").await, "compact");
            assert!(eval::<bool>(&h, "return mare.config_sources().logo.where == mare._old_logo_source").await);
            assert!(eval::<bool>(&h, r#"local ok, e = pcall(function() mare.o.logo = 'changed' end); return not ok and tostring(e):find('retry afterwards', 1, true) ~= nil"#).await);
            assert!(eval::<bool>(&h, r#"local ok, e = pcall(function() mare.o.plugins.timeout = 1 end); return not ok and tostring(e):find('retry afterwards', 1, true) ~= nil"#).await);
            assert!(eval::<bool>(&h, r#"local ok, e = pcall(function() mare._cached_options.logo = 'changed' end); return not ok and tostring(e):find('retry afterwards', 1, true) ~= nil"#).await);
            assert!(eval::<bool>(&h, r#"local ok, e = pcall(function() mare._cached_theme.marker = 1 end); return not ok and tostring(e):find('retry afterwards', 1, true) ~= nil"#).await);
            assert!(eval::<bool>(&h, r#"local ok, e = pcall(function() mare.setup {logo='changed'} end); return not ok and tostring(e):find('retry afterwards', 1, true) ~= nil"#).await);
            assert!(eval::<bool>(&h, r#"local ok, e = pcall(function() mare.provider('race', {}) end); return not ok and tostring(e):find('retry afterwards', 1, true) ~= nil"#).await);
            release_reload(&h).await;
        });
        result.unwrap();
        assert!(eval::<bool>(&h, "return mare.o.logo == false").await);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn concurrent_direct_config_mutation_aborts_reload_without_losing_update() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let init = env.config_dir().join("init.lua");
        std::fs::write(&init, format!("{USER_INIT}\nmare.setup {{ theme = {{ marker = 'old' }} }}")).unwrap();
        h.reload_config().await.unwrap();
        std::fs::write(&init, format!("{USER_INIT}\nmare.setup {{ theme = {{ marker = 'new' }} }}\nmare._conflict_started=true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end")).unwrap();
        let (result, ()) = tokio::join!(h.reload_config(), async {
            wait_flag(&h, "_conflict_started").await;
            exec(&h, "mare.config.theme.marker = 'runtime'").await;
            release_reload(&h).await;
        });
        assert!(result.unwrap_err().to_string().contains("config changed during reload"));
        assert_eq!(eval::<String>(&h, "return mare.config.theme.marker").await, "runtime");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn concurrent_cached_default_table_mutation_aborts_reload() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        exec(&h, "mare._cached_pkg_default = mare.options.plugins.default").await;
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("{USER_INIT}\nmare._default_conflict_started=true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end"),
        )
        .unwrap();
        let (result, ()) = tokio::join!(h.reload_config(), async {
            wait_flag(&h, "_default_conflict_started").await;
            exec(&h, "mare._cached_pkg_default.timeout = 17").await;
            release_reload(&h).await;
        });
        assert!(result.unwrap_err().to_string().contains("config changed during reload"));
        assert_eq!(eval::<i64>(&h, "return mare.o.plugins.timeout").await, 17);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn staged_unique_keymap_rechecks_concurrent_runtime_bindings() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let init = env.config_dir().join("init.lua");
        exec(&h, "mare.keymap.action('old', function() end); mare.keymap.action('new', function() end); mare.keymap.action('runtime', function() end)").await;
        std::fs::write(&init, format!("{USER_INIT}\nmare.keymap.set('editor', '<C-k>', 'old')")).unwrap();
        h.reload_config().await.unwrap();
        std::fs::write(&init, format!("{USER_INIT}\nmare.keymap.set('editor', '<C-k>', 'new', {{unique=true}})\nmare._unique_started=true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end")).unwrap();
        let (result, ()) = tokio::join!(h.reload_config(), async {
            wait_flag(&h, "_unique_started").await;
            exec(&h, "mare.keymap.set('editor', '<C-k>', 'runtime')").await;
            release_reload(&h).await;
        });
        assert!(result.unwrap_err().to_string().contains("unique = true"));
        let actions: (String, String) = eval(&h, "local c = mare.keymap.resolve('editor', '<C-k>').candidates; return c[2].action, c[1].action").await;
        // Config bindings (rank 2) stack above concurrent runtime bindings (rank 1).
        assert_eq!(actions, ("runtime".into(), "old".into()));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn new_config_events_are_active_while_plugin_reload_is_waiting() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let init = env.config_dir().join("init.lua");
        std::fs::write(
            &init,
            format!("{USER_INIT}\nmare.on('probe/stage', function() mare._old_stage_event=true end)"),
        )
        .unwrap();
        h.reload_config().await.unwrap();
        std::fs::write(
            env.config_dir().join("plugins/probe2/init.lua"),
            r#"return {name='probe2', apply=function() mare._plugin_stage_started=true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end end}"#,
        )
        .unwrap();
        std::fs::write(
            &init,
            format!(
                "{}\nmare.on('probe/stage', function() mare._new_stage_event=true end)",
                USER_INIT.replace("n = 1,", "n = 2,")
            ),
        )
        .unwrap();
        let (result, ()) = tokio::join!(h.reload_config(), async {
            wait_flag(&h, "_plugin_stage_started").await;
            h.kernel.emit_json("probe/stage", json!({})).await;
            assert!(eval::<bool>(&h, "return mare._new_stage_event == true and mare._old_stage_event == nil").await);
            release_reload(&h).await;
        });
        result.unwrap();
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_reload_is_rejected_and_cancellation_releases_guard() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { icons = 'always' }
            mare._reload_waiting = true
            mare.task.sleep(600000) -- only the cancellation ends it
        "#,
        )
        .unwrap();
        let host = h.clone();
        let task = tokio::task::spawn_local(async move { host.reload_config().await });
        wait_flag(&h, "_reload_waiting").await;
        let err = h.reload_all().await.unwrap_err().to_string();
        assert!(err.contains("reload already in progress"), "{err}");
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(eval::<String>(&h, "return mare.o.icons").await, "never");
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT).unwrap();
        h.reload_config().await.unwrap();
        assert_eq!(eval::<String>(&h, "return mare.o.icons").await, "never");
        h.shutdown().await;
    })
    .await
}
