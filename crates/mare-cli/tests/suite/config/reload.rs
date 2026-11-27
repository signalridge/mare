//! Reloading config and plugins: what reloads, failures, retries and the request lease.

use super::*;

#[tokio::test]
async fn reload_withdraws_voter_registered_by_model_changed_callback() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        std::fs::write(
            &init,
            r#"
            mare.setup { model = 'faux/echo' }
            mare.on('model/changed', function(model)
              if model.ref == 'faux/echo2' then
                mare.on('probe/from-model', function()
                  mare._nested_listener_calls = (mare._nested_listener_calls or 0) + 1
                end)
                mare.policy.voter('request/ready', function()
                  return { decision = 'deny', reason = 'callback voter' }
                end)
              end
            end)
        "#,
        )
        .unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        expose(&h);
        exec(&h, "C.cordis:get('llm'):set_model('faux/echo2', { save = false })").await;
        h.kernel.emit_json("probe/from-model", json!({})).await;
        assert_eq!(eval::<i64>(&h, "return mare._nested_listener_calls").await, 1);
        let refused = h.prompt("before reload", json!({})).await.unwrap();
        assert!(refused.to_string().contains("callback voter"), "{refused}");
        std::fs::write(init, "mare.setup { model = 'faux/echo2' }").unwrap();
        h.reload_config().await.unwrap();
        h.kernel.emit_json("probe/from-model", json!({})).await;
        assert_eq!(eval::<i64>(&h, "return mare._nested_listener_calls").await, 1);
        let accepted = h.prompt("after reload", json!({})).await.unwrap();
        assert!(accepted["error"].is_null(), "{accepted}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reload_config_reloads_only_plugins_whose_opts_changed() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let before: (i64, i64) = eval(&h, "return mare._applies.probe, mare._applies.probe2").await;
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT.replace("n = 1,", "n = 2,")).unwrap();
        let r = h.reload_config().await.unwrap();
        assert_eq!(r["reloaded"], json!(["probe2"]), "{r}");
        let after: (i64, i64, i64) =
            eval(&h, "return mare._applies.probe, mare._applies.probe2, mare._cfg_probe2.n").await;
        assert_eq!(after, (before.0, before.1 + 1, 2));
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.cb()").await, 42, "functions in opts survive layering");
        let r = h.reload_config().await.unwrap();
        assert_eq!(r["reloaded"], json!([]), "an unchanged reload reloads nothing: {r}");
        assert_eq!(
            eval::<i64>(&h, "local n = 0; for _, e in ipairs(mare.keymap.list({ context = 'editor' })) do if e.key == 'ctrl+y' then n = n + 1 end end; return n").await,
            1,
            "init.lua keymaps don't stack"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn config_reload_reports_plugin_definition_failure() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        std::fs::write(env.config_dir().join("plugins/probe2/init.lua"), "this is not valid Lua!").unwrap();
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT.replace("n = 1,", "n = 2,")).unwrap();
        let result = h.reload_config().await.unwrap();
        let failures = result["failures"].as_array().unwrap();
        assert_eq!(failures.len(), 1, "{result}");
        assert!(failures[0].as_str().unwrap().contains("probe2"), "{result}");
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.n").await, 1);
        std::fs::write(env.config_dir().join("plugins/probe2/init.lua"), PROBE.replace("NAME", "probe2")).unwrap();
        let retry = h.reload_config().await.unwrap();
        assert_eq!(retry["failures"], json!([]), "{retry}");
        assert_eq!(retry["reloaded"], json!(["probe2"]), "unchanged config must retry: {retry}");
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.n").await, 2);
        h.shutdown().await;
    })
    .await
}

/// a config reload that can't load a non-essential plugin's new definition
/// keeps it running on the old one and only warns — requests and tools go on; a later config
/// reload retries it.
#[tokio::test]
async fn failed_plugin_reload_of_an_optional_plugin_only_warns_and_is_retried() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        expose(&h);
        exec(&h, r#"mare._guard_runs=0; mare._guard_builds=0; C.cordis:on('request/build', function(req, next) mare._guard_builds=mare._guard_builds+1; return next() end); T:register('reload_guard_probe', {
          description='probe', parameters={type='object'}, execute=function() mare._guard_runs=mare._guard_runs+1; return 'ok' end })"#).await;
        let plugin = env.config_dir().join("plugins/probe2/init.lua");
        std::fs::write(&plugin, "invalid Lua source!").unwrap();
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT.replace("n = 1,", "n = 2,")).unwrap();
        let failed = h.reload_config().await.unwrap();
        assert!(!failed["failures"].as_array().unwrap().is_empty());
        assert!(crate::common::plugin_active(&h.kernel, "probe2"), "the running definition stays");
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        assert_eq!(eval::<i64>(&h, "return mare._guard_builds").await, 1);
        let (ok, runs): (bool, i64) = eval(&h, r#"local r=T:execute({id='x', name='reload_guard_probe', arguments={}}, {}); return not r.is_error, mare._guard_runs"#).await;
        assert!(ok);
        assert_eq!(runs, 1);
        std::fs::write(plugin, PROBE.replace("NAME", "probe2")).unwrap();
        let retry = h.reload_config().await.unwrap();
        assert_eq!(retry["failures"], json!([]), "{retry}");
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.n").await, 2, "retried with the new options");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn request_during_config_reload_stops_before_build() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        expose(&h);
        exec(&h, "mare._request_builds=0; C.cordis:on('request/build', function(req, next) mare._request_builds=mare._request_builds+1; return next() end)").await;
        // the reload stays inside init.lua until the test releases it (60 s cap if it never does)
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("{USER_INIT}\nmare._reload_waiting=true\nfor _ = 1, 6000 do if mare._reload_release then break end mare.task.sleep(10) end"),
        )
        .unwrap();
        let (reload, ()) = tokio::join!(h.reload_config(), async {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !eval::<bool>(&h, "return mare._reload_waiting == true").await {
                assert!(std::time::Instant::now() < end, "the reload never reached init.lua");
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            let result = h.prompt("hello", json!({})).await.unwrap();
            assert!(result.to_string().contains("configuration reload is in progress"), "{result}");
            assert_eq!(eval::<i64>(&h, "return mare._request_builds").await, 0);
            exec(&h, "mare._reload_release = true").await;
        });
        reload.unwrap();
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn request_holds_reload_lease_through_build_and_releases_afterwards() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        expose(&h);
        exec(&h, r#"C.cordis:on('request/build', function(req, next) mare._build_lease_started=true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end; return next() end)"#).await;
        let (prompt, ()) = tokio::join!(h.prompt("hello", json!({})), async {
            wait_flag(&h, "_build_lease_started").await;
            let error = h.reload_config().await.unwrap_err().to_string();
            assert!(error.contains("model request is active"), "{error}");
            let plugin_error = h.kernel.reload(Some("probe")).await.unwrap_err().to_string();
            assert!(plugin_error.contains("model request"), "{plugin_error}");
            release_reload(&h).await;
        });
        let prompt = prompt.unwrap();
        assert!(prompt["error"].is_null(), "{prompt}");
        h.reload_config().await.unwrap();
        h.shutdown().await;
    }).await
}

/// a single-plugin reload that can't prepare the new definition changes nothing —
/// the plugin keeps running, so (being optional) nothing is blocked.
#[tokio::test]
async fn failed_single_plugin_reload_keeps_the_plugin_and_readiness() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        expose(&h);
        let plugin = env.config_dir().join("plugins/probe2/init.lua");
        std::fs::write(&plugin, "invalid Lua source!").unwrap();
        assert!(h.kernel.reload(Some("probe2")).await.is_err());
        assert!(crate::common::plugin_active(&h.kernel, "probe2"));
        assert!(h.kernel.reload_incomplete().is_empty(), "a finished reload leaves no marker");
        assert_eq!(
            eval::<String>(&h, "return C.cordis:get('policy'):decide('request/ready', {}).decision").await,
            "allow"
        );
        std::fs::write(plugin, PROBE.replace("NAME", "probe2")).unwrap();
        h.kernel.reload(Some("probe2")).await.unwrap();
        assert_eq!(
            eval::<String>(&h, "return C.cordis:get('policy'):decide('request/ready', {}).decision").await,
            "allow"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reload_during_started_persistence_invalidates_tool_approval() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        expose(&h);
        exec(&h, r#"mare._guard_runs=0; T:register('reload_race_probe', {
          description='probe', parameters={type='object'}, execute=function() mare._guard_runs=mare._guard_runs+1; return 'ok' end })"#).await;
        let (result, ()) = tokio::join!(eval::<(bool, String)>(&h, r#"
          local r=T:execute({id='x', name='reload_race_probe', arguments={}}, {
            on_state=function(_, state) if state == 'started' then mare._guard_started=true; for _ = 1, 12000 do if mare._reload_release then break end mare.task.sleep(5) end end; return true end,
          }); return r.refused == true, tostring(r.content)
        "#), async {
            wait_flag(&h, "_guard_started").await;
            h.reload_config().await.unwrap();
            release_reload(&h).await;
        });
        assert!(result.0 && result.1.contains("approval changed"), "{result:?}");
        assert_eq!(eval::<i64>(&h, "return mare._guard_runs").await, 0);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn config_reload_reports_apply_failure_and_retries_without_another_config_change() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let plugin = env.config_dir().join("plugins/probe2/init.lua");
        std::fs::write(&plugin, r#"return { name = "probe2", apply = function() error("apply sentinel") end }"#)
            .unwrap();
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT.replace("n = 1,", "n = 2,")).unwrap();
        let failed = h.reload_config().await.unwrap();
        assert!(failed["failures"].to_string().contains("apply sentinel"), "{failed}");
        std::fs::write(plugin, PROBE.replace("NAME", "probe2")).unwrap();
        let retry = h.reload_config().await.unwrap();
        assert_eq!(retry["failures"], json!([]), "{retry}");
        assert_eq!(retry["reloaded"], json!(["probe2"]), "{retry}");
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.n").await, 2);
        assert_eq!(h.reload_config().await.unwrap()["reloaded"], json!([]));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_plugin_reload_retries_all_changed_plugins() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let plugin = env.config_dir().join("plugins/probe/init.lua");
        std::fs::write(
            &plugin,
            r#"return { name = "probe", apply = function() mare._reload_started = true; mare.task.sleep(600000) end }"#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            USER_INIT.replace("x = 1,", "x = 4,").replace("n = 1,", "n = 2,"),
        )
        .unwrap();
        // cancel the reload once it is parked inside the plugin's apply
        tokio::select! {
            r = h.reload_config() => panic!("reload unexpectedly completed: {r:?}"),
            () = wait_flag(&h, "_reload_started") => {}
        }
        std::fs::write(plugin, PROBE.replace("NAME", "probe")).unwrap();
        let retry = h.reload_config().await.unwrap();
        assert_eq!(retry["failures"], json!([]), "{retry}");
        assert_eq!(retry["reloaded"], json!(["probe", "probe2"]), "{retry}");
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe.a.x").await, 4);
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.n").await, 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn full_reload_with_an_optional_apply_failure_keeps_requests_running() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let plugin = env.config_dir().join("plugins/probe2/init.lua");
        std::fs::write(&plugin, r#"return { name = "probe2", apply = function() error("full reload sentinel") end }"#)
            .unwrap();
        let failed = h.reload_all().await.unwrap();
        assert!(failed["notices"].to_string().contains("full reload sentinel"), "{failed}");
        // probe2 is optional and back on its previous definition: nothing is blocked
        assert!(crate::common::plugin_active(&h.kernel, "probe2"));
        expose(&h);
        assert_eq!(
            eval::<String>(&h, "return C.cordis:get('policy'):decide('request/ready', {}).decision").await,
            "allow"
        );
        std::fs::write(plugin, PROBE.replace("NAME", "probe2")).unwrap();
        h.reload_all().await.unwrap();
        assert_eq!(eval::<i64>(&h, "return mare._cfg_probe2.n").await, 1);
        assert_eq!(
            eval::<String>(&h, "return C.cordis:get('policy'):decide('request/ready', {}).decision").await,
            "allow"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn full_reload_with_missing_package_blocks_until_full_retry_succeeds() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        expose(&h);
        let missing = env.home.path().join("missing-package");
        let cfg = format!(
            "{USER_INIT}\nmare.plugins.add {{ {{ dir = {:?}, name = 'local-missing', required = true }} }}",
            missing.to_string_lossy()
        );
        std::fs::write(env.config_dir().join("init.lua"), cfg).unwrap();
        let failed = h.reload_all().await.unwrap();
        assert!(failed["notices"].to_string().contains("local-missing"), "{failed}");
        let (decision, reason): (String, String) =
            eval(&h, "local v=C.cordis:get('policy'):decide('request/ready', {}); return v.decision, v.reason or ''")
                .await;
        assert_eq!(decision, "deny");
        assert!(reason.contains("local-missing"), "{reason}");
        std::fs::create_dir_all(missing.join("plugins")).unwrap();
        std::fs::write(missing.join("plugins/available.lua"), "return {name='available', apply=function() end}")
            .unwrap();
        h.reload_all().await.unwrap();
        assert_eq!(
            eval::<String>(&h, "return C.cordis:get('policy'):decide('request/ready', {}).decision").await,
            "allow"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn failed_reload_preserves_managed_config_events_and_keymaps() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        expose(&h);
        let good = format!(
            "{USER_INIT}\n{}",
            r#"
            mare.on('probe/reload', function() mare._old_reload_event = (mare._old_reload_event or 0) + 1 end)
            mare.policy.voter('tool/before', function(call)
              if call.name == 'blocked' then return { decision = 'deny', reason = 'old deny' } end
            end)
            mare.keymap.set('editor', '<C-k>', function() return 'old' end, { desc = 'old' })
            mare.ui.renderer('test/reload', { id = 'old' })
            mare.ui.status('test_reload', { align = 'right', order = 100, render = function() mare._reload_status_val = 'old'; return 'old' end })
        "#
        );
        std::fs::write(env.config_dir().join("init.lua"), &good).unwrap();
        h.reload_config().await.unwrap();
        for bad in [
            "this is not valid Lua!",
            r#"mare.setup { icons = 'always' }
               mare.on('probe/reload', function() mare._new_reload_event = true end)
               mare.keymap.set('editor', '<C-k>', function() return 'new' end, { desc = 'new' })
               mare.ui.renderer('test/reload', { id = 'new' })
               mare.ui.status('test_reload', { align = 'right', order = 100, render = function() mare._reload_status_val = 'new'; return 'new' end })
               error('reload sentinel')"#,
            // a real built-in name, but a rewrite point: refused as mare.policy.voter is called,
            // so the evaluation fails and nothing is committed (a-rnh4 #1)
            r#"mare.policy.voter('tool/before', function() end)
               mare.policy.voter('tool/rewrite', function() end)"#,
        ] {
            std::fs::write(env.config_dir().join("init.lua"), bad).unwrap();
            let err = h.reload_config().await.unwrap_err().to_string();
            assert!(err.contains("init.lua"), "missing config path: {err}");
            assert_eq!(eval::<String>(&h, "return mare.o.icons").await, "never");
            assert_eq!(eval::<String>(&h, "return mare.config_sources().icons.layer").await, "project");
            let key: String =
                eval(&h, "local c = mare.keymap.resolve('editor', '<C-k>').candidates; return mare.keymap.callback(c[1].action)()").await;
            assert_eq!(key, "old");
            assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('test/reload').id").await, "old");
            let (decision, reason): (String, String) = eval(
                &h,
                "local v = C.cordis:get('policy'):decide('tool/before', { { name = 'blocked' } }); return v.decision, v.reason or ''",
            )
            .await;
            assert_eq!((decision.as_str(), reason.as_str()), ("deny", "old deny"), "the old voter was withdrawn by a failed reload");
            assert_eq!(eval::<String>(&h, "mare.ui.status_redraw('test_reload'); return mare._reload_status_val").await, "old");
            h.kernel.emit_json("probe/reload", json!({})).await;
        }
        let check = mare_shared::lua::core::config::check_config_fn(h.lua()).unwrap();
        let _: mlua::Value = check
            .call((
                mare_shared::lua::json_to_lua(h.lua(), &json!({ "plugins": { "approvals": { "policy": 42 } } }))
                    .unwrap(),
                "cli",
            ))
            .unwrap();
        assert!(eval::<Vec<String>>(&h, "return mare.config_warnings()").await.iter().any(|w| w.contains("matches none of its forms")));
        assert_eq!(eval::<i64>(&h, "return mare._old_reload_event").await, 3);
        assert!(!eval::<bool>(&h, "return mare._new_reload_event == true").await);
        assert_eq!(eval::<i64>(&h, "local n = 0; for _, e in ipairs(mare.keymap.list({ context = 'editor' })) do if e.key == 'ctrl+k' then n = n + 1 end end; return n").await, 1);
        exec(&h, "mare.prompt.remove('after-failure'); mare.o.icons = 'always'").await;
        assert!(eval::<bool>(&h, "return mare.config.prompt_removed['after-failure']").await);
        assert_eq!(h.kernel.config()["icons"], "always");
        std::fs::write(env.config_dir().join("init.lua"), &good).unwrap();
        h.reload_config().await.unwrap();
        h.kernel.emit_json("probe/reload", json!({})).await;
        assert_eq!(eval::<i64>(&h, "return mare._old_reload_event").await, 4);
        assert_eq!(eval::<i64>(&h, "local n = 0; for _, e in ipairs(mare.keymap.list({ context = 'editor' })) do if e.key == 'ctrl+k' then n = n + 1 end end; return n").await, 1);
        let ui_core = mare_shared::lua::core::ui::handle(h.lua()).unwrap();
        let stack_len: i64 =
            ui_core.get::<mlua::Function>("renderer_stack_len").unwrap().call("test/reload").unwrap();
        assert_eq!(stack_len, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_reload_restores_previous_configuration() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let good = format!(
            "{USER_INIT}\n{}",
            r#"
            mare.on('probe/cancel', function() mare._cancel_old = (mare._cancel_old or 0) + 1 end)
            mare.keymap.set('editor', '<C-k>', function() return 'old' end, { desc = 'old' })
            mare.ui.renderer('test/cancel', { id = 'old' })
        "#
        );
        std::fs::write(env.config_dir().join("init.lua"), &good).unwrap();
        h.reload_config().await.unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { icons = 'always' }
            mare.on('probe/cancel', function() mare._cancel_new = true end)
            mare.keymap.set('editor', '<C-k>', function() return 'new' end, { desc = 'new' })
            mare.ui.renderer('test/cancel', { id = 'new' })
            mare._cancel_parked = true
            mare.task.sleep(600000)
        "#,
        )
        .unwrap();
        // cancel the reload once init.lua is parked
        tokio::select! {
            r = h.reload_config() => panic!("reload unexpectedly completed: {r:?}"),
            () = wait_flag(&h, "_cancel_parked") => {}
        }
        assert_eq!(eval::<String>(&h, "return mare.o.icons").await, "never");
        let key: String = eval(
            &h,
            "local c = mare.keymap.resolve('editor', '<C-k>').candidates; return mare.keymap.callback(c[1].action)()",
        )
        .await;
        assert_eq!(key, "old");
        assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('test/cancel').id").await, "old");
        h.kernel.emit_json("probe/cancel", json!({})).await;
        assert_eq!(eval::<i64>(&h, "return mare._cancel_old").await, 1);
        assert!(!eval::<bool>(&h, "return mare._cancel_new == true").await);
        exec(&h, "mare.o.icons = 'always'").await;
        assert_eq!(h.kernel.config()["icons"], "always");
        h.shutdown().await;
    })
    .await
}

async fn reject_then_fix_greeting(on_invalid: Option<&str>) -> (Option<String>, String, String) {
    let env = TestEnv::new().await;
    std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
    let on_invalid_field = on_invalid.map(|v| format!(", on_invalid = {v:?}")).unwrap_or_default();
    let plugin = format!(
        r#"return {{ name = "cfg", config_schema = {{ greeting = {{ type = "string", default = "default", desc = "a greeting"{on_invalid_field} }} }},
  apply = function(_ctx, cfg)
    mare._cfg_applied = (mare._cfg_applied or 0) + 1
    mare._greeting = cfg.greeting
  end }}"#
    );
    std::fs::write(env.config_dir().join("plugins/cfg.lua"), plugin).unwrap();
    std::fs::write(env.config_dir().join("init.lua"), r#"mare.plugin("cfg", { greeting = "one" })"#).unwrap();
    let h = env.host().user_config(false).no_session().build().await;
    std::fs::write(env.config_dir().join("init.lua"), r#"mare.plugin("cfg", { greeting = 42 })"#).unwrap();
    let report = h.reload_config().await;
    let kept: Option<String> = eval(&h, "return type(mare._greeting) == 'string' and mare._greeting or nil").await;
    let warnings = format!("{report:?} {:?}", h.plugin_warnings());
    std::fs::write(env.config_dir().join("init.lua"), r#"mare.plugin("cfg", { greeting = "fixed" })"#).unwrap();
    h.reload_config().await.unwrap();
    let fixed: String = eval(&h, "return mare._greeting").await;
    h.shutdown().await;
    (kept, fixed, warnings)
}

#[tokio::test]
async fn a_wrong_typed_plugin_config_falls_back_to_its_default_and_a_corrected_edit_applies() {
    local(async {
        let (kept, fixed, w) = reject_then_fix_greeting(None).await;
        assert_eq!(kept.as_deref(), Some("default"), "the wrong value reached the plugin, or no default: {w}");
        assert!(w.contains("greeting"), "the rejected option is not reported: {w}");
        assert_eq!(fixed, "fixed");
    })
    .await
}

// guarantee: rejected-config-keeps-the-previous-one
#[tokio::test]
async fn a_rejected_strict_plugin_config_keeps_the_previous_one_and_a_corrected_edit_applies() {
    local(async {
        let (kept, fixed, w) = reject_then_fix_greeting(Some("error")).await;
        assert_eq!(kept.as_deref(), Some("one"), "a rejected config replaced the running one: {w}");
        assert!(w.contains("greeting"), "the rejected option is not reported: {w}");
        assert_eq!(fixed, "fixed");
    })
    .await
}

/// code and config change together and the new definition's first apply fails
/// on a passing condition, so the plugin runs on its previous definition again (`restored`). Once
/// the condition is gone, a `/reload config` with nothing else changed must apply the new
/// definition — not treat its fingerprint as already applied and keep the old one running.
/// Reference: reference implementation (a failed reload is
/// retried and then applies).
#[tokio::test]
async fn a_restored_plugin_is_retried_on_its_new_definition_by_the_next_config_reload() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let plugin = env.config_dir().join("plugins/probe2/init.lua");
        let new_code = PROBE.replace("NAME", "probe2").replace(
            "apply = function(ctx, cfg)",
            "apply = function(ctx, cfg)\n    if not mare._probe2_ready then error('transient sentinel') end\n    mare._probe2_new = true",
        );
        std::fs::write(&plugin, new_code).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT.replace("n = 1,", "n = 2,")).unwrap();
        let failed = h.reload_config().await.unwrap();
        assert!(failed["failures"].to_string().contains("transient sentinel"), "{failed}");
        let new: Option<bool> = eval(&h, "return mare._probe2_new").await;
        assert_eq!(new, None, "precondition: the previous definition runs again (on the new config)");
        // the passing condition is gone; nothing else changes
        exec(&h, "mare._probe2_ready = true").await;
        let retry = h.reload_config().await.unwrap();
        let (n, new): (i64, Option<bool>) = eval(&h, "return mare._cfg_probe2.n, mare._probe2_new").await;
        h.shutdown().await;
        assert_eq!(retry["failures"], json!([]), "{retry}");
        assert_eq!(new, Some(true), "the retry kept the previous definition running: {retry}");
        assert_eq!(n, 2, "the retry did not apply the new config: {retry}");
    })
    .await
}

/// a config reload that leaves a plugin waiting for a service nobody provides
/// (Cordis PENDING) reports it in `pending` — it is not a failure, and not silently dropped.
#[tokio::test]
async fn a_config_reload_reports_a_plugin_left_pending() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let plugin = env.config_dir().join("plugins/probe2/init.lua");
        std::fs::write(
            &plugin,
            PROBE
                .replace("NAME", "probe2")
                .replace("config_schema = {", "inject = { \"no-such-service\" },\n  config_schema = {"),
        )
        .unwrap();
        std::fs::write(env.config_dir().join("init.lua"), USER_INIT.replace("n = 1,", "n = 2,")).unwrap();
        let r = h.reload_config().await.unwrap();
        h.shutdown().await;
        let pending = r["pending"].to_string();
        assert!(pending.contains("probe2") && pending.contains("no-such-service"), "{r}");
    })
    .await
}

/// Quick audit: a config reload that turns a provider off leaves its dependent
/// waiting, though the dependent is not restarted. `pending` names it with what it waits for
/// (collected from the tree after the update, not only from the restarted rows).
#[tokio::test]
async fn a_config_reload_reports_a_dependent_that_lost_its_provider() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("lostprov.lua"),
            r#"return { name = "lostprov", provide = { "lostsvc" },
  contracts = { lostsvc = { methods = {
    who = { doc = "who", expose = "lua", params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } },
  } } },
  apply = function(ctx)
    local Svc = require("cordis").class("LostSvc", mare.Service)
    function Svc:constructor(c) Svc.super.constructor(self, c, "lostsvc") end
    function Svc:who() return "p" end
    Svc.new(ctx)
  end }"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("lostdep.lua"),
            r#"return { name = "lostdep", inject = { "lostsvc" }, apply = function() end }"#,
        )
        .unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "").unwrap();
        let h = env.host().user_config(false).build().await;
        assert!(plugin_active(&h.kernel, "lostdep"), "the dependent runs at first");
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.plugin("lostprov", { enabled = false })"#)
            .unwrap();
        let r = h.reload_config().await.unwrap();
        h.shutdown().await;
        let pending = r["pending"].to_string();
        assert!(pending.contains("lostdep") && pending.contains("lostsvc"), "{r}");
    })
    .await
}

/// `mare.keymap.context` declared in `init.lua` is released/replaced cleanly across `/reload`
/// without failing with "already exists".
#[tokio::test]
async fn reload_twice_succeeds_when_init_declares_keymap_context() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = r#"
mare.keymap.context("editor.vi", { parent = "editor" })
mare.keymap.set("editor.vi", "x", "redraw")
"#;
        std::fs::write(env.config_dir().join("init.lua"), init).unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        let r1 = h.reload_config().await.unwrap();
        assert_eq!(r1["failures"], json!([]), "first reload: {r1}");
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("{init}\nmare.keymap.set(\"editor.vi\", \"y\", \"redraw\")\n"),
        )
        .unwrap();
        let r2 = h.reload_config().await.unwrap();
        assert_eq!(r2["failures"], json!([]), "second reload: {r2}");
        std::fs::write(env.config_dir().join("init.lua"), "").unwrap();
        let r3 = h.reload_config().await.unwrap();
        assert_eq!(r3["failures"], json!([]), "reload after removing context: {r3}");
        let gone: bool =
            eval(&h, r#"local ok = pcall(mare.keymap.set, "editor.vi", "x", "redraw"); return not ok"#).await;
        assert!(gone, "removed context from init.lua must no longer exist after reload");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn ui_public_surface_user_plugin_and_no_private_ui_fields() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins/user-ui-probe");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("init.lua"),
            r#"
            return {
                name = "user-ui-probe",
                apply = function()
                    local ui_priv = {}
                    for k in next, mare.ui do
                        if type(k) == "string" and k:sub(1, 1) == "_" then
                            ui_priv[#ui_priv + 1] = k
                        end
                    end
                    table.sort(ui_priv)
                    assert(#ui_priv == 0, "mare.ui must expose no underscore fields: " .. table.concat(ui_priv, ", "))
                    assert(rawget(mare, "_working_views") == nil, "mare._working_views must be removed")
                    assert(mare.ui.frontend == nil, "mare.ui.frontend must be nil in local host")

                    -- User-origin plugin sets viewport, ambiwidth, and non-persisting layout via public ui.* API.
                    assert(mare.ui.set_viewport(132, 44) == true, "set_viewport must report change")
                    assert(mare.ui.width() == 132 and mare.ui.height() == 44, "viewport dimensions updated")
                    mare.ui.set_ambiwidth("double")
                    mare.ui.set_ambiwidth("single")
                    mare.ui.set_layout({ "status", "live", "editor" }, { persist = false })
                    assert(
                        mare.o.ui == nil or mare.o.ui.layout == nil or mare.o.ui.layout.order == nil,
                        "set_layout({ persist = false }) must not mutate mare.o.ui.layout"
                    )

                    -- Read-only Working views remain intact without mare._working_views.
                    assert(type(mare.ui.Working.frames) == "table" and mare.ui.Working.frames.ember ~= nil)
                    assert(type(mare.ui.Working.sweeps) == "table")
                    local ok_ro = pcall(function()
                        mare.ui.Working.frames.ember = {}
                    end)
                    assert(not ok_ro, "mare.ui.Working.frames must remain read-only")
                    mare.g.USER_UI_PROBE_OK = true
                end,
            }
            "#,
        )
        .unwrap();

        let mut hb = env.host().user_config(true).no_session();
        hb.opts.load_init = true;
        hb.opts.no_user_plugins = false;
        let h = mare_host::Host::new(hb.opts).await.unwrap();
        let problems = h.kernel.problems();
        assert!(
            eval::<bool>(&h, "return mare.g.USER_UI_PROBE_OK == true").await,
            "user-ui-probe plugin must pass all UI public surface assertions; problems={problems:?}"
        );

        // Reloading the ui plugin preserves read-only Working views without leaking mare._working_views.
        h.kernel.reload(Some("ui")).await.unwrap();
        let after_reload: bool = eval(
            &h,
            r#"return rawget(mare, "_working_views") == nil
                and type(mare.ui.Working.frames) == "table"
                and mare.ui.Working.frames.ember ~= nil"#,
        )
        .await;
        assert!(after_reload, "Working views must survive ui plugin reload without mare._working_views");

        h.shutdown().await;
    })
    .await
}
