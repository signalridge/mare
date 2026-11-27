//! Config layers: precedence, option merging, warnings, name slots, redaction.

use super::*;

#[tokio::test]
async fn public_config_origin_tracks_user_project_and_runtime_calls() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        for (path, layer) in [(env.config_dir().join("init.lua"), "user"), (env.cwd.join(".mare/init.lua"), "project")]
        {
            std::fs::write(
                path,
                format!(
                    "local source = mare.config_origin()\n\
                     assert(source.layer == '{layer}', source.layer)\n\
                     assert(source.file and source.file:sub(-8) == 'init.lua')\n\
                     mare.params {{ match = 'faux/*' }}\n"
                ),
            )
            .unwrap();
        }
        h.reload_config().await.unwrap();
        let (layer, absent): (String, bool) =
            eval(&h, "local source = mare.config_origin(); return source.layer, source.file == nil").await;
        assert_eq!(layer, "runtime");
        assert!(absent);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn runtime_option_source_keeps_the_callers_file_and_line() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let source: String = eval(
            &h,
            r#"assert(load("mare.o.slow_callback_table_rows = 13", "@runtime_option_probe.lua"))()
               return tostring(mare.config_sources().slow_callback_table_rows.where)"#,
        )
        .await;
        assert!(source.ends_with("runtime_option_probe.lua:1"), "{source}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn layers_apply_in_precedence_order_and_record_sources() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let c = h.kernel.config();
        assert_eq!(c["model"], "faux/echo", "user init.lua beats state.json");
        assert_eq!(c["theme_name"], "x", "state.json fills unset keys");
        assert_eq!(c["icons"], "never", "project beats user");
        assert_eq!(c["slow_callback_table_rows"], 9, "env beats project");
        assert_eq!(c["editor"], "vim", "cli beats env");
        let (model, notify, scroll, editor, state): (String, String, String, String, String) = eval(
            &h,
            r#"local s = mare.config_sources()
               return s.model.layer .. " " .. (s.model.where or "?"), s.icons.layer, s.slow_callback_table_rows.layer,
                      s.editor.layer, s.theme_name.layer"#,
        )
        .await;
        assert!(model.starts_with("user ") && model.contains("init.lua:2"), "{model}");
        assert_eq!(
            (notify.as_str(), scroll.as_str(), editor.as_str(), state.as_str()),
            ("project", "env MARE_SLOW_CALLBACK_TABLE_ROWS", "cli", "state")
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn plugin_opts_merge_maps_and_replace_lists() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let c = h.kernel.config();
        assert_eq!(c["plugins"]["probe"]["a"], json!({ "x": 1, "y": 3 }));
        assert_eq!(c["plugins"]["probe"]["list"], json!([9]));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unknown_and_mistyped_options_warn_with_their_location() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        let joined = warnings.join("\n");
        assert!(joined.contains("unknown option `modle` — did you mean `model`?"), "{joined}");
        assert!(joined.contains("option `editor` expects a string, got number"), "{joined}");
        assert!(joined.contains("init.lua:"), "{joined}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn root_agent_limits_are_registered_public_options() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup({ agents = { max_active = 2, max_depth = 1 } })")
            .unwrap();
        let h = env.host().user_config(true).faux(json!([])).build().await;
        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        assert!(!warnings.iter().any(|warning| warning.contains("agents")), "{warnings:?}");
        assert_eq!(h.kernel.config()["agents"], json!({"max_active":2,"max_depth":1}));
        assert!(eval::<bool>(&h, "return mare.options.agents.type == 'table'").await);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn invalid_root_agent_limits_fail_host_startup() {
    local(async {
        let env = TestEnv::new().await;
        for invalid in [json!(false), json!({"max_active":-1}), json!({"max_depth":"2"}), json!({"max_depth":1.5})] {
            let opts = env.host().faux(json!([])).config(json!({"agents":invalid})).opts;
            match mare_host::Host::new(opts).await {
                Ok(host) => {
                    host.shutdown().await;
                    panic!("invalid limits must not silently become defaults");
                }
                Err(error) => assert!(error.to_string().contains("agents"), "{error:#}"),
            }
        }
    })
    .await;
}

#[tokio::test]
async fn builtin_slots_can_be_closed_and_replaced_without_breaking_the_rest() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        let names = h.kernel.def_names();
        assert!(!names.iter().any(|n| n == "usage" || n == "file-ref"), "{names:?}");
        assert!(
            eval::<bool>(&h, "return mare._my_notify == true").await,
            "the user plugin replaces the built-in with override = true"
        );
        let res = h.prompt("hello", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn prompt_remove_hides_global_and_agent_scoped_sections() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        exec(&h, "CX = C.cordis:get('context')").await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let leaked: bool = eval(
            &h,
            r#"CX:section({ name = "probe-sec", text = "GLOBAL-PROBE" })
               CX:section({ name = "probe-sec", agent = "worker", text = "AGENT-PROBE" })
               CX:invalidate()
               local a = CX:system_prompt(nil, nil) or ""
               local b = CX:system_prompt(nil, "worker") or ""
               return (a .. b):find("PROBE") ~= nil"#,
        )
        .await;
        assert!(!leaked, "removed section still in the system prompt");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn config_views_redact_sensitive_values_without_changing_config() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let views: (String, String, String) = eval(
            &h,
            r#"
            mare.config.providers = { example = {
              api_key = 'FAKE_KEY_SENTINEL', headers = { ['X-Custom'] = 'FAKE_HEADER_SENTINEL' },
              query_params = { routing = 'FAKE_QUERY_SENTINEL' },
              base_url = 'https://example.invalid/?routing=FAKE_URL_SENTINEL',
              note = 'proxy: https://FAKE_USERINFO_SENTINEL@example.invalid/path' } }
            mare.config.plugins.probe.private_value = 'FAKE_PLUGIN_SENTINEL'
            mare.config_sensitive('plugins.probe.private_value')
            mare.config_sources()['plugins.probe.private_value'] = { layer = 'user' }
            local select, overlay, connected = ui.select, ui.overlay, ui.connected
            ui.connected = function() return true end
            ui.select = function(opts) mare._captured_config = mare.json.encode(opts); return nil end
            ui.overlay = function(_, node) mare._captured_overlay = mare.json.encode(node) end
            CM:run('/config')
            local full = mare._captured_config
            CM:run('/config providers')
            local filtered = mare._captured_overlay
            CM:run('/config plugins.probe.private_value')
            local plugin = mare._captured_overlay
            ui.select, ui.overlay, ui.connected = select, overlay, connected
            return full, filtered, plugin
        "#,
        )
        .await;
        for view in [&views.0, &views.1, &views.2] {
            for sentinel in [
                "FAKE_KEY_SENTINEL",
                "FAKE_HEADER_SENTINEL",
                "FAKE_QUERY_SENTINEL",
                "FAKE_URL_SENTINEL",
                "FAKE_USERINFO_SENTINEL",
                "FAKE_PLUGIN_SENTINEL",
            ] {
                assert!(!view.contains(sentinel), "{sentinel} leaked: {view}");
            }
        }
        assert!(views.0.contains("[REDACTED]"));
        assert!(views.1.contains("[REDACTED]"));
        assert!(views.2.contains("[REDACTED]"));
        assert_eq!(eval::<String>(&h, "return mare.config.providers.example.api_key").await, "FAKE_KEY_SENTINEL");
        assert_eq!(
            eval::<String>(&h, "return mare.config.providers.example.note").await,
            "proxy: https://FAKE_USERINFO_SENTINEL@example.invalid/path"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn perf_from_init_lua_reaches_the_merged_config() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.o.perf = { markdown_cache = 7 }").unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        // the event loop reads this merged value (not the CLI layer)
        assert_eq!(h.kernel.config()["perf"]["markdown_cache"], 7);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn perf_from_env_reaches_the_merged_config() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_PERF", r#"{"wrap_cache":9}"#);
        let h = env.host().user_config(true).no_session().build().await;
        assert_eq!(h.kernel.config()["perf"]["wrap_cache"], 9);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn watchdog_ms_in_init_lua_warns_but_is_not_a_typo() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.o.watchdog_ms = 500").unwrap();
        env.set_var("MARE_WATCHDOG_MS", "20000");
        let h = env.host().user_config(true).no_session().build().await;
        let joined = eval::<Vec<String>>(&h, "return mare.config_warnings()").await.join("\n");
        assert!(joined.contains("watchdog_ms only takes effect via --set or MARE_WATCHDOG_MS"), "{joined}");
        assert!(!joined.contains("unknown option"), "{joined}");
        // the env layer carries it too (the watchdog itself reads MARE_WATCHDOG_MS at VM start)
        assert_eq!(h.kernel.config()["watchdog_ms"], 20000);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn lua_memory_mb_config_limits_host_vm_cleanly() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup({ lua_memory_mb = 32 })").unwrap();

        // lua_memory_mb is invocation-only: setting it in init.lua warns and does NOT cap the VM.
        let h_init = env.host().user_config(true).no_session().build().await;
        let init_warnings = eval::<Vec<String>>(&h_init, "return mare.config_warnings()").await;
        assert!(
            init_warnings.iter().any(|w| w.contains("lua_memory_mb") && w.contains("MARE_LUA_MEMORY_MB")),
            "init.lua lua_memory_mb must warn as invocation-only: {init_warnings:?}"
        );
        h_init
            .lua()
            .load(
                r#"
                local t = {}
                for i = 1, 80000 do
                    t[i] = string.rep("x", 512)
                end
                "#,
            )
            .exec()
            .expect("init.lua lua_memory_mb must not limit the host VM");
        h_init.shutdown().await;

        // CLI invocation override (`--set lua_memory_mb=32`) applies before init.lua with no warning.
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup({ model = 'faux/echo' })").unwrap();
        let h = env.host().user_config(true).config(json!({ "lua_memory_mb": 32 })).no_session().build().await;
        let warnings = eval::<Vec<String>>(&h, "return mare.config_warnings()").await;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(h.kernel.config()["lua_memory_mb"], 32);
        let err = h
            .lua()
            .load(
                r#"
                local t = {}
                for i = 1, 200000 do
                    t[i] = string.rep("x", 512)
                end
                "#,
            )
            .exec()
            .unwrap_err();
        assert!(
            matches!(err, mlua::Error::MemoryError(_)),
            "expected MemoryError under CLI lua_memory_mb = 32, got: {err:?}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn policy_timeout_ms_is_registered_without_warnings() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup({ policy_timeout_ms = 5000 })").unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        assert!(
            eval::<bool>(&h, "return mare.options.policy_timeout_ms ~= nil").await,
            "mare.options missing policy_timeout_ms"
        );
        let warnings = eval::<Vec<String>>(&h, "return mare.config_warnings()").await;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(h.kernel.config()["policy_timeout_ms"], 5000);
        assert_eq!(eval::<i64>(&h, "return mare.o.policy_timeout_ms").await, 5000);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn valid_invocation_layers_keep_precedence_and_share_runtime_checks() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_REDUCE_MOTION", "true");
        env.set_var("MARE_WATCHDOG_MS", "10000");
        std::fs::create_dir_all(env.config_dir().join("plugins/probe")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/probe/init.lua"),
            "return { name='probe', config_schema={count={type='integer', default=1, desc='a count'}}, apply=function() end }",
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.o.reduce_motion = false\nmare.plugin('probe', { count = 2 })",
        )
        .unwrap();
        let h = env
            .host()
            .user_config(true)
            .config(json!({
                "model": "faux/echo",
                "reduce_motion": false,
                "effort": "high",
                "plugins": { "probe": { "count": 5 } }
            }))
            .no_session()
            .build()
            .await;
        assert_eq!(h.kernel.config()["reduce_motion"], false);
        assert_eq!(h.kernel.config()["plugins"]["probe"]["count"], 5);
        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(eval::<String>(&h, "return mare.config_sources().reduce_motion.layer").await, "cli");
        assert_eq!(eval::<String>(&h, "return mare.config_sources()['plugins.probe.count'].layer").await, "cli");
        // a runtime write commits only a valid value: refused, the old one stays
        let refused: (String, String) = eval(
            &h,
            "local _, a = pcall(function() mare.o.reduce_motion = 'oops' end)
             local _, b = pcall(mare.plugin, 'approvals', { policy = 42 })
             return tostring(a), tostring(b)",
        )
        .await;
        assert!(refused.0.contains("expects a boolean"), "{refused:?}");
        assert!(refused.1.contains("matches none of its forms"), "{refused:?}");
        assert!(!eval::<bool>(&h, "return mare.o.reduce_motion").await, "the old value stays");
        // /reload config re-evaluates and keeps invocation precedence and clean warning state.
        h.reload_config().await.unwrap();
        assert_eq!(h.kernel.config()["reduce_motion"], false);
        assert_eq!(h.kernel.config()["plugins"]["probe"]["count"], 5);
        assert!(eval::<Vec<String>>(&h, "return mare.config_warnings()").await.is_empty());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn diagnostic_config_loader_checks_environment_values_too() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_REDUCE_MOTION", "oops");
        env.set_var("MARE_PERF", "{bad");
        let cfg = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        assert!(cfg.value.get("reduce_motion").is_none(), "invalid MARE_REDUCE_MOTION must be pruned: {:?}", cfg.value);
        assert!(
            cfg.warnings.iter().any(|w| w.contains("expects a boolean") && w.contains("MARE_REDUCE_MOTION")),
            "{:?}",
            cfg.warnings
        );
        assert!(
            cfg.warnings.iter().any(|w| w.contains("MARE_PERF") && w.contains("invalid JSON")),
            "{:?}",
            cfg.warnings
        );
    })
    .await
}

#[tokio::test]
async fn config_describe_and_redact_share_slash_config_rules() {
    local(async {
        let mut env = TestEnv::new().await;
        let h = setup(&mut env).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let (slash_view, described_json, still_raw): (String, String, bool) = eval(
            &h,
            r#"
            mare.config.providers = { example = {
              api_key = 'FAKE_KEY_SENTINEL',
              headers = { ['X-Custom'] = 'FAKE_HEADER_SENTINEL' },
              query_params = { routing = 'FAKE_QUERY_PARAMS_SENTINEL' },
              query = { sig = 'FAKE_QUERY_SENTINEL' },
              base_url = 'https://example.invalid/?routing=FAKE_URL_SENTINEL',
              note = 'proxy: https://FAKE_USERINFO_SENTINEL@example.invalid/path',
              retries = 3 } }
            mare.config.plugins.probe.private_value = 'FAKE_PLUGIN_SENTINEL'
            mare.config.plugins.probe.public_value = 'visible'
            mare.config.plugins.probe.nested = {
              headers = { Authorization = 'Bearer FAKE_NESTED_HDR_SENTINEL' },
              query = { token = 'FAKE_NESTED_QUERY_SENTINEL' },
              credentials = { secret = 'FAKE_NESTED_CRED_SENTINEL' },
              max_tokens = 256,
            }
            mare.config_sensitive('plugins.probe.private_value')
            mare.config_sources()['plugins.probe.private_value'] = { layer = 'user', where = 'init.lua:10' }
            local overlay, connected = ui.overlay, ui.connected
            ui.connected = function() return false end
            ui.overlay = function(_, node) mare._captured_overlay = mare.json.encode(node) end
            CM:run('/config providers')
            local slash_view = mare._captured_overlay
            ui.overlay, ui.connected = overlay, connected
            local desc = mare.config_describe({ 'providers', 'plugins.probe.private_value', 'plugins.probe.nested', 'border' })
            local raw_ok = mare.config.providers.example.api_key == 'FAKE_KEY_SENTINEL'
              and mare.config.plugins.probe.private_value == 'FAKE_PLUGIN_SENTINEL'
            return slash_view, mare.json.encode(desc), raw_ok
        "#,
        )
        .await;
        assert!(still_raw);
        for view in [&slash_view, &described_json] {
            for sentinel in [
                "FAKE_KEY_SENTINEL",
                "FAKE_HEADER_SENTINEL",
                "FAKE_QUERY_PARAMS_SENTINEL",
                "FAKE_QUERY_SENTINEL",
                "FAKE_URL_SENTINEL",
                "FAKE_USERINFO_SENTINEL",
                "FAKE_PLUGIN_SENTINEL",
                "FAKE_NESTED_HDR_SENTINEL",
                "FAKE_NESTED_QUERY_SENTINEL",
                "FAKE_NESTED_CRED_SENTINEL",
            ] {
                assert!(!view.contains(sentinel), "{sentinel} leaked: {view}");
            }
        }
        let desc: serde_json::Value = serde_json::from_str(&described_json).unwrap();
        assert_eq!(desc["providers"]["redacted"], true);
        assert_eq!(desc["providers"]["effective"]["example"]["api_key"], "[REDACTED]");
        assert_eq!(desc["providers"]["effective"]["example"]["headers"], "[REDACTED]");
        assert_eq!(desc["providers"]["effective"]["example"]["query_params"], "[REDACTED]");
        assert_eq!(desc["providers"]["effective"]["example"]["query"], "[REDACTED]");
        assert_eq!(desc["providers"]["effective"]["example"]["retries"], 3);
        assert_eq!(desc["plugins.probe.private_value"]["effective"], "[REDACTED]");
        assert_eq!(desc["plugins.probe.private_value"]["sensitive"], true);
        assert_eq!(desc["plugins.probe.private_value"]["redacted"], true);
        assert_eq!(desc["plugins.probe.private_value"]["source"]["layer"], "user");
        assert_eq!(desc["plugins.probe.private_value"]["source"]["where"], "init.lua:10");
        assert_eq!(desc["plugins.probe.nested"]["effective"]["headers"], "[REDACTED]");
        assert_eq!(desc["plugins.probe.nested"]["effective"]["query"], "[REDACTED]");
        assert_eq!(desc["plugins.probe.nested"]["effective"]["credentials"], "[REDACTED]");
        assert_eq!(desc["plugins.probe.nested"]["effective"]["max_tokens"], 256);
        assert_eq!(desc["plugins.probe.nested"]["redacted"], true);
        assert_eq!(desc["border"]["default"], "rounded");
        assert_eq!(desc["border"]["redacted"], false);
        assert!(desc["border"]["description"].is_string());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn state_json_merges_by_leaf_and_runs_config_validation() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"mare.plugin("tools", { bash_timeout_ms = 5000 })"#)
            .unwrap();
        std::fs::create_dir_all(env.home.path().join("state")).unwrap();
        std::fs::write(
            env.home.path().join("state/state.json"),
            serde_json::to_string(&json!({
                "version": 1,
                "editor": 42,
                "border": "single",
                "model_params": { "faux/echo": { "effort": "low" } },
                "plugins": {
                    "tools": {
                        "bash_timeout_ms": 9999,
                        "max_lines": 123,
                        "max_bytes": "not-an-integer"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let h = env.host().user_config(true).faux(json!([])).no_session().build().await;
        let c = h.kernel.config();
        assert_eq!(c["border"], "single", "valid state.json option applies");
        assert!(c.get("editor").is_none(), "invalid state.json core option must be pruned: {c}");
        assert_eq!(c["plugins"]["tools"]["bash_timeout_ms"], 5000, "init.lua leaf beats state.json leaf");
        assert_eq!(c["plugins"]["tools"]["max_lines"], 123, "state.json sibling leaf merges under plugins.tools");
        assert!(
            c["plugins"]["tools"].get("max_bytes").is_none(),
            "invalid state.json plugin option must be pruned: {c}"
        );
        let (warnings, src_lines, src_border): (Vec<String>, String, String) = eval(
            &h,
            r#"local s = mare.config_sources()
               return mare.config_warnings(),
                      s["plugins.tools.max_lines"] and s["plugins.tools.max_lines"].layer or "",
                      s.border and s.border.layer or """#,
        )
        .await;
        let joined = warnings.join("\n");
        assert!(
            joined.contains("option `editor` expects a string, got number") && joined.contains("(state)"),
            "{joined}"
        );
        assert!(joined.contains("max_bytes") && joined.contains("(state)"), "{joined}");
        assert!(!joined.contains("model_params"), "plugin runtime state key must not warn: {joined}");
        assert_eq!(src_lines, "state");
        assert_eq!(src_border, "state");
        h.shutdown().await;
    })
    .await
}
