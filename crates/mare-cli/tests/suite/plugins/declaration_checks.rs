//! plugin declaration checks as one table-driven suite against real host startup.
//! Covers:
//! 1. Unknown top-level field -> config warning naming file and field; plugin still loads.
//! 2. Same-source duplicate name -> refusal naming both files; neither loads.
//! 3. Hard inject cycle A<->B -> both refused with the cycle diagnostic; neither runs.
//! 4. Missing dependency -> PENDING with the missing service named in `mare.plugins.status` and
//!    the kernel problem list (not a load error).
//!
//! Reference: reference implementation.
use crate::common::*;

struct DeclarationCase {
    name: &'static str,
    files: &'static [(&'static str, &'static str)],
    active_plugins: &'static [(&'static str, bool)],
    global_flags: &'static [(&'static str, bool)],
    config_warning: &'static [&'static str],
    plugin_warning: &'static [&'static str],
    no_plugin_warning_for: &'static [&'static str],
    problems: &'static [&'static [&'static str]],
    status_failure: Option<(&'static str, &'static str, &'static str)>,
}

const CASES: &[DeclarationCase] = &[
    DeclarationCase {
        name: "unknown top-level field warns and still loads",
        files: &[(
            "unknown-field.lua",
            "return { name = 'unknown-field', bogus_flag = true, apply = function() mare.g.unknown_field_ran = true end }",
        )],
        active_plugins: &[("unknown-field", true)],
        global_flags: &[("unknown_field_ran", true)],
        config_warning: &["`unknown-field`", "plugins/unknown-field.lua", "unknown field `bogus_flag`"],
        plugin_warning: &[],
        no_plugin_warning_for: &["unknown-field"],
        problems: &[],
        status_failure: None,
    },
    DeclarationCase {
        name: "same-source duplicate name refuses both definitions and names both files",
        files: &[
            ("dup-plug.lua", "return { name = 'dup-plug', apply = function() mare.g.dup_a = true end }"),
            ("dup-plug/init.lua", "return { name = 'dup-plug', apply = function() mare.g.dup_b = true end }"),
        ],
        active_plugins: &[("dup-plug", false)],
        global_flags: &[("dup_a", false), ("dup_b", false)],
        config_warning: &[],
        plugin_warning: &["`dup-plug`", "twice", "plugins/dup-plug.lua", "plugins/dup-plug"],
        no_plugin_warning_for: &[],
        problems: &[],
        status_failure: None,
    },
    DeclarationCase {
        name: "hard inject cycle refuses both members with cycle diagnostic",
        files: &[
            (
                "cyc-a.lua",
                r#"return { name = "cyc-a", provide = { "svc_x" }, inject = { "svc_y" },
  apply = function(ctx) mare.g.cyc_a = true; require("mare_lib").service(ctx, "svc_x", {}) end }"#,
            ),
            (
                "cyc-b.lua",
                r#"return { name = "cyc-b", provide = { "svc_y" }, inject = { "svc_x" },
  apply = function(ctx) mare.g.cyc_b = true; require("mare_lib").service(ctx, "svc_y", {}) end }"#,
            ),
        ],
        active_plugins: &[("cyc-a", false), ("cyc-b", false)],
        global_flags: &[("cyc_a", false), ("cyc_b", false)],
        config_warning: &[],
        plugin_warning: &[],
        no_plugin_warning_for: &[],
        problems: &[&["cyc-a", "not loaded: dependency cycle"], &["cyc-b", "not loaded: dependency cycle"]],
        status_failure: None,
    },
    DeclarationCase {
        name: "missing dependency stays PENDING and names the missing service",
        files: &[(
            "missing-dep.lua",
            "return { name = 'missing-dep', inject = { 'never_provided' }, apply = function() mare.g.missing_dep_ran = true end }",
        )],
        active_plugins: &[("missing-dep", false)],
        global_flags: &[("missing_dep_ran", false)],
        config_warning: &[],
        plugin_warning: &["`missing-dep`", "not started", "never_provided"],
        no_plugin_warning_for: &[],
        problems: &[&["plugin missing-dep not started: required service `never_provided` is not available"]],
        status_failure: Some(("missing-dep", "start", "never_provided")),
    },
];

#[tokio::test]
async fn plugin_declaration_checks_table() {
    for case in CASES {
        local(async {
            let env = TestEnv::new().await;
            let plugins_dir = env.config_dir().join("plugins");
            for (rel_path, source) in case.files {
                let path = plugins_dir.join(rel_path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, source).unwrap();
            }

            let opts = env.host().user_plugins().no_session().opts;
            let h = mare_host::Host::new(opts).await.unwrap();

            for (plugin, expected_active) in case.active_plugins {
                let actual_active: bool =
                    eval(&h, &format!("return mare.plugins.status({plugin:?}).active == true")).await;
                assert_eq!(actual_active, *expected_active, "{}: {plugin} active", case.name);
                assert_eq!(h.kernel.plugin_active(plugin), *expected_active, "{}: {plugin} kernel active", case.name);
            }

            for (flag, expected) in case.global_flags {
                let ran: bool = eval(&h, &format!("return mare.g.{flag} == true")).await;
                assert_eq!(ran, *expected, "{}: mare.g.{flag}", case.name);
            }

            if !case.config_warning.is_empty() {
                let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
                assert!(
                    warnings.iter().any(|w| {
                        let path_normalized = w.replace('\\', "/");
                        case.config_warning.iter().all(|s| path_normalized.contains(s))
                    }),
                    "{}: expected config warning containing {:?}, got {:?}",
                    case.name,
                    case.config_warning,
                    warnings
                );
            }

            let plugin_warnings = h.plugin_warnings();
            if !case.plugin_warning.is_empty() {
                assert!(
                    plugin_warnings.iter().any(|w| {
                        let w = w.replace('\\', "/");
                        case.plugin_warning.iter().all(|s| w.contains(s))
                    }),
                    "{}: expected plugin warning containing {:?}, got {:?}",
                    case.name,
                    case.plugin_warning,
                    plugin_warnings
                );
            }
            for plugin in case.no_plugin_warning_for {
                assert!(
                    !plugin_warnings.iter().any(|w| w.contains(plugin)),
                    "{}: unexpected plugin warning for {plugin}: {:?}",
                    case.name,
                    plugin_warnings
                );
            }

            let problems = h.kernel.problems();
            for expected_terms in case.problems {
                assert!(
                    problems.iter().any(|p| expected_terms.iter().all(|s| p.contains(s))),
                    "{}: expected problem containing {:?}, got {:?}",
                    case.name,
                    expected_terms,
                    problems
                );
            }

            if let Some((plugin, expected_phase, expected_err)) = case.status_failure {
                let status_json: String =
                    eval(&h, &format!("return mare.json.encode(mare.plugins.status({plugin:?}))")).await;
                let status: serde_json::Value = serde_json::from_str(&status_json).unwrap();
                assert_eq!(status["defined"], true, "{}: {status}", case.name);
                assert_eq!(status["active"], false, "{}: {status}", case.name);
                assert_eq!(status["disabled"], false, "{}: {status}", case.name);
                assert_eq!(status["failure"]["phase"], expected_phase, "{}: {status}", case.name);
                let err = status["failure"]["error"].as_str().unwrap_or("");
                assert!(
                    err.contains(expected_err),
                    "{}: expected error containing {expected_err}, got {status}",
                    case.name
                );
            }

            h.shutdown().await;
        })
        .await;
    }
}

#[tokio::test]
async fn invalid_provider_declarations_are_refused_and_reported() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins/bad-providers");
        std::fs::create_dir_all(&pdir).unwrap();
        let plugin_src = r#"
return {
  name = "bad-providers",
  inject = { "llm" },
  providers = { "valid-sib" },
  apply = function(ctx)
    ctx.llm:register_provider({})
    ctx.llm:register_provider({ provider = "bad-type", default_model = 123 })
    ctx.llm:register_provider({ provider = "bad-model", models = { { model_id = "" } } })
    ctx.llm:register_provider({
      provider = "valid-sib",
      is_configured = function() return true end,
      models = { { model_id = "m1", name = "M1", context_window = 100000, max_output = 4096 } },
    })
    ctx.llm:register_provider({
      provider = "valid-sib",
      is_configured = function() return false end,
      models = { { model_id = "m2", name = "M2" } },
    })
    ctx.llm:register_wire({
      name = "bad-wire",
      encode = function() end,
      decoder = function() end,
      route = 42,
    })
  end,
}
"#;
        std::fs::write(pdir.join("init.lua"), plugin_src).unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (active, has_sib, has_m1, has_m2, still_orig_cfg, bad_absent, status_json): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local st = mare.plugins.status("bad-providers")
            local p = LLM:provider("valid-sib")
            return st.active == true,
              p ~= nil,
              LLM:resolve("valid-sib/m1") ~= nil,
              LLM:resolve("valid-sib/m2") == nil,
              p ~= nil and p.is_configured() == true,
              LLM:provider("bad-type") == nil and LLM:provider("bad-model") == nil and LLM:wire("bad-wire") == nil,
              mare.json.encode(st)
            "#,
        )
        .await;

        assert!(active, "plugin with invalid declarations should stay running");
        assert!(has_sib && has_m1, "valid sibling provider must still register");
        assert!(!has_m2 && still_orig_cfg, "duplicate provider must be refused keeping the first");
        assert!(bad_absent, "invalid provider/wire declarations must be refused");

        let status: serde_json::Value = serde_json::from_str(&status_json).unwrap();
        let probs = status["problems"].as_array().expect("status.problems array");
        assert!(probs.len() >= 5, "expected problems for each invalid declaration, got {probs:?}");
        for prob in probs {
            assert_eq!(prob["reporter"], "llm", "{prob}");
            assert_eq!(prob["kind"], "declaration", "{prob}");
            assert_eq!(prob["plugin"], "bad-providers", "{prob}");
        }
        let messages: Vec<&str> = probs.iter().filter_map(|p| p["message"].as_str()).collect();
        assert!(messages.iter().any(|m| m.contains("provider")), "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("default_model")), "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("models[1].model_id")), "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("duplicate") && m.contains("valid-sib")), "{messages:?}");
        assert!(messages.iter().any(|m| m.contains("bad-wire") && m.contains("route")), "{messages:?}");
        assert!(
            h.kernel.problems().is_empty(),
            "declaration problems must not populate handler errors: {:?}",
            h.kernel.problems()
        );

        // `mare doctor` surfaces declaration warnings without failing the run.
        let doctor_out = mare_cmd()
            .args(["doctor"])
            .env("MARE_HOME", env.home.path())
            .env("NO_COLOR", "1")
            .current_dir(env.cwd.clone())
            .stdin(std::process::Stdio::null())
            .run_output();
        let doctor_text = String::from_utf8_lossy(&doctor_out.stdout);
        assert!(
            doctor_text.contains("declaration") && doctor_text.contains("[llm] bad-providers"),
            "expected declaration warning in mare doctor output:\n{doctor_text}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn undeclared_provider_names_warn_and_declared_ones_do_not() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        std::fs::write(
            pdir.join("mixed-decl.lua"),
            r#"return {
  name = "mixed-decl",
  inject = { "llm" },
  providers = { "a" },
  apply = function(ctx)
    ctx.llm:register_provider({ provider = "a", is_configured = function() return true end })
    ctx.llm:register_provider({ provider = "b", is_configured = function() return true end })
  end,
}"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (a_reg, b_reg, declaring_a, st_json, builtin_probs): (bool, bool, Vec<String>, String, usize) = eval(
            &h,
            r#"return LLM:provider("a") ~= nil,
              LLM:provider("b") ~= nil,
              mare.plugins.declaring("providers", "a"),
              mare.json.encode(mare.plugins.status("mixed-decl")),
              #mare.plugins.status("provider-anthropic").problems"#,
        )
        .await;

        assert!(a_reg && b_reg, "undeclared provider name warns without blocking registration");
        assert!(declaring_a.contains(&"mixed-decl".to_string()), "{declaring_a:?}");
        assert_eq!(builtin_probs, 0, "built-in provider plugins must declare all registered providers");

        let status: serde_json::Value = serde_json::from_str(&st_json).unwrap();
        assert_eq!(status["providers"], serde_json::json!(["a"]));
        let probs = status["problems"].as_array().unwrap();
        assert_eq!(probs.len(), 1, "only undeclared `b` should warn: {probs:?}");
        assert_eq!(probs[0]["key"], "undeclared-provider:b");
        assert_eq!(probs[0]["reporter"], "llm");
        assert!(probs[0]["message"].as_str().unwrap().contains("\"b\""));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reported_problems_are_keyed_by_reporter_and_cleared_on_reload() {
    local(async {
        let env = TestEnv::new().await;
        let pdir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&pdir).unwrap();
        let target_path = pdir.join("target-plug.lua");
        let rep_path = pdir.join("other-rep.lua");
        std::fs::write(&target_path, r#"return { name = "target-plug", apply = function() end }"#).unwrap();
        std::fs::write(
            &rep_path,
            r#"return {
  name = "other-rep",
  apply = function()
    mare.plugins.report_problem("target-plug", "k1", "first")
    mare.plugins.report_problem("target-plug", "k2", "second")
    mare.plugins.report_problem("target-plug", "k1", "updated")
  end,
}"#,
        )
        .unwrap();

        let h = env.host().user_plugins().no_session().build().await;
        let init_json: String = eval(
            &h,
            r#"
            mare.plugins.report_problem("target-plug", "k1", "from host")
            return mare.json.encode(mare.plugins.status("target-plug").problems)
            "#,
        )
        .await;
        let init_probs: Vec<serde_json::Value> = serde_json::from_str(&init_json).unwrap();
        assert_eq!(init_probs.len(), 3, "two keys from other-rep + separate k1 from host: {init_probs:?}");
        assert!(
            init_probs.iter().any(|p| p["reporter"] == "other-rep" && p["key"] == "k1" && p["message"] == "updated")
        );
        assert!(
            init_probs.iter().any(|p| p["reporter"] == "other-rep" && p["key"] == "k2" && p["message"] == "second")
        );
        assert!(init_probs.iter().any(|p| p["reporter"] == "host" && p["key"] == "k1" && p["message"] == "from host"));

        // Reloading target clears all problems attached to target.
        let bumped = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        std::fs::write(&target_path, r#"return { name = "target-plug", apply = function() end }"#).unwrap();
        std::fs::File::options().write(true).open(&target_path).unwrap().set_modified(bumped).unwrap();
        h.kernel.reload(Some("target-plug")).await.unwrap();

        let after_target_reload: String =
            eval(&h, r#"return mare.json.encode(mare.plugins.status("target-plug").problems)"#).await;
        assert_eq!(after_target_reload, "[]");

        // Re-report from another plugin and host, then reload reporter -> clears only reporter's problem.
        std::fs::write(
            &rep_path,
            r#"return {
  name = "other-rep",
  apply = function()
    mare.plugins.report_problem("target-plug", "k3", "from rep reload test")
  end,
}"#,
        )
        .unwrap();
        let bumped2 = std::time::SystemTime::now() + std::time::Duration::from_secs(4);
        std::fs::File::options().write(true).open(&rep_path).unwrap().set_modified(bumped2).unwrap();
        h.kernel.reload(Some("other-rep")).await.unwrap();
        let p_after: String = eval(
            &h,
            r#"
            mare.plugins.report_problem("target-plug", "k1", "host keeps this")
            return mare.json.encode(mare.plugins.status("target-plug").problems)
            "#,
        )
        .await;
        assert_eq!(serde_json::from_str::<Vec<serde_json::Value>>(&p_after).unwrap().len(), 2);

        std::fs::write(&rep_path, r#"return { name = "other-rep", apply = function() end }"#).unwrap();
        let bumped3 = std::time::SystemTime::now() + std::time::Duration::from_secs(6);
        std::fs::File::options().write(true).open(&rep_path).unwrap().set_modified(bumped3).unwrap();
        h.kernel.reload(Some("other-rep")).await.unwrap();
        let remaining: String =
            eval(&h, r#"return mare.json.encode(mare.plugins.status("target-plug").problems)"#).await;
        let rem: Vec<serde_json::Value> = serde_json::from_str(&remaining).unwrap();
        assert_eq!(rem.len(), 1);
        assert_eq!(rem[0]["reporter"], "host");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn provider_from_config_converts_and_validates_user_config_entries() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (ok_reg, route, model_ok, bad_nil, bad_err, bad_models_nil, bad_models_err): (
            bool,
            String,
            bool,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local decl, err = LLM:provider_from_config("custom-r", {
              base_url = "https://example.test/v1",
              wire_api = "responses",
              default_model = "gpt-x",
              models = { { model_id = "gpt-x", name = "GPT X", context_window = 64000, max_output = 8192 } },
            })
            assert(decl ~= nil and err == nil, tostring(err))
            local rm = LLM:register_provider(decl)
            local bad, berr = LLM:provider_from_config("broken", { wire_api = "chat" })
            local bad_m, bad_m_err = LLM:provider_from_config("bad-models", {
              base_url = "https://example.test/v1",
              models = { a = { model_id = "m1" } },
            })
            return rm ~= nil, decl.route, LLM:resolve("custom-r/gpt-x") ~= nil, bad == nil, tostring(berr), bad_m == nil, tostring(bad_m_err)
            "#,
        )
        .await;

        assert!(ok_reg && model_ok, "converted declaration must register cleanly");
        assert_eq!(route, "responses");
        assert!(bad_nil && bad_err.contains("base_url"), "{bad_err}");
        assert!(bad_models_nil && bad_models_err.contains("array"), "{bad_models_err}");

        h.shutdown().await;
    })
    .await
}
