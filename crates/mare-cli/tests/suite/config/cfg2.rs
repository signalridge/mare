//! Configuration conventions:
//! - Presets as data + single resolver (`mare.config_resolve_preset`, `ui.working`, `permissions`, `plugins.preset`).
//! - Schema `env`, `choices_from`, `nullable` + plugin option env layer (`MARE_APPROVAL_POLICY`).
//! - `ui.working` default back to `combined`/`swell`.
//! - `lua_memory_mb` is `invocation_only = true`; `slow_callback_ms` defaults to 1000 ms (1 s).
//! - Per-plugin generated LuaLS definitions (`types/mare/plugin_options.gen.lua`).

use crate::common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn presets_as_data_single_resolver_works_for_ui_working_permissions_and_plugins() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup {
              model = "faux/echo",
              ui = {
                working = {
                  presets = {
                    dusk = { like = "mare", glyph = "sail" },
                  },
                  preset = "dusk",
                },
              },
            }
            mare.plugin("permissions", {
              presets = {
                ci = { like = "read-only", label = "CI" },
              },
              preset = "ci",
            })
            "#,
        )
        .unwrap();
        let h = env.host().user_config(true).no_session().build().await;

        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        assert!(warnings.is_empty(), "valid custom presets must not warn: {warnings:?}");

        // Single public preset resolver: defaults < inherited preset < explicit overrides; detects cycles.
        let (resolved_glyph, resolved_label, cycle_err, unknown_err, default_working): (
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
            local res, err = mare.config_resolve_preset({
              defaults = { a = 1, b = 2, c = 3 },
              presets = {
                base = { a = 10, b = 20 },
                child = { like = "base", b = 200 },
              },
            }, { preset = "child", c = 300 })
            assert(err == nil and res.a == 10 and res.b == 200 and res.c == 300, tostring(err))

            local _, cyc = mare.config_resolve_preset({
              defaults = {},
              presets = { x = { like = "y" }, y = { like = "x" } },
            }, { preset = "x" })

            local _, unk = mare.config_resolve_preset({
              defaults = {},
              presets = { combined = { glyph = "swell" } },
            }, { preset = "combnd" })

            local W = require("ui.working")
            local custom = W.resolve()
            local def = W.resolve({ ui = {} })
            return custom.glyph, custom.label, tostring(cyc or ""), tostring(unk or ""), def.glyph .. "/" .. def.label
            "#,
        )
        .await;
        assert_eq!(resolved_glyph, "sail");
        assert_eq!(resolved_label, "foam");
        assert!(cycle_err.contains("cycle"), "expected cycle error, got: {cycle_err}");
        assert!(unknown_err.contains("did you mean") && unknown_err.contains("combined"), "{unknown_err}");
        assert_eq!(default_working, "swell/glint", "default ui.working must be combined/swell");

        // Unknown permissions preset is caught by schema `choices_from = "presets"` with did-you-mean.
        let validate = mare_shared::lua::core::config::validate_options_fn(h.lua()).unwrap();
        let bad_perm =
            mare_shared::lua::json_to_lua(h.lua(), &json!({ "plugins": { "permissions": { "preset": "read-onl" } } }))
                .unwrap();
        let problems = validate.call::<Vec<String>>(bad_perm).unwrap().join("\n");
        assert!(
            problems.contains("read-onl") && problems.contains("read-only"),
            "permissions.preset must validate against presets with did-you-mean: {problems}"
        );

        h.shutdown().await;
    })
    .await
}

const SCHEMA_PROBE_PLUGIN: &str = r#"
return {
  name = "cfg2_probe",
  contracts = {
    test = {
      owner = "cfg2_probe",
      events = {
        ["cfg2-probe"] = { doc = "probe cfg2 schema values", mode = "bail", expose = "process" },
      },
    },
  },
  config_schema = {
    endpoint = {
      type = "string",
      env = "MARE_CFG2_PROBE_ENDPOINT",
      default = "default-ep",
      desc = "Endpoint settable via env.",
    },
    optional_tag = {
      type = "string",
      nullable = true,
      desc = "Nullable string tag.",
    },
    strict_tag = {
      type = "string",
      desc = "Non-nullable string tag.",
    },
    mode = {
      type = "string",
      default = "fast",
      choices = { "fast", "safe" },
      choices_from = "modes",
      desc = "Selected mode from built-ins or modes map.",
    },
    modes = {
      type = "map",
      default = {},
      desc = "Custom modes.",
    },
  },
  apply = function(ctx, cfg)
    ctx:on("test/cfg2-probe", function()
      return { endpoint = cfg.endpoint, optional_tag = cfg.optional_tag, mode = cfg.mode }
    end)
  end,
}
"#;

#[tokio::test]
async fn schema_env_choices_from_nullable_and_mare_approval_policy() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/cfg2_probe.lua"), SCHEMA_PROBE_PLUGIN).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { model = "faux/echo" }
            mare.plugin("approvals", { policy = "ask" })
            mare.plugin("cfg2_probe", {
              endpoint = "from-init",
              optional_tag = mare.json.null,
              modes = { turbo = { speed = 10 } },
              mode = "turbo",
            })
            "#,
        )
        .unwrap();
        env.set_var("MARE_APPROVAL_POLICY", "never");
        env.set_var("MARE_CFG2_PROBE_ENDPOINT", "from-env");

        let h = env.host().user_config(true).no_session().build().await;
        expose(&h);

        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        assert!(warnings.is_empty(), "valid env/nullable/choices_from must not warn: {warnings:?}");

        // MARE_APPROVAL_POLICY overrides init.lua and records its env source.
        let (policy, policy_src, probe_json): (String, String, String) = eval(
            &h,
            r#"
            local ap = C.cordis:get("approvals")
            local src = mare.config_sources()["plugins.approvals.policy"]
            return tostring(ap:policy()), src and src.layer or "<none>", mare.json.encode(C.cordis:bail("test/cfg2-probe"))
            "#,
        )
        .await;
        assert_eq!(policy, "never", "MARE_APPROVAL_POLICY must override init.lua");
        assert_eq!(policy_src, "env MARE_APPROVAL_POLICY");
        let probe: Value = serde_json::from_str(&probe_json).unwrap();
        assert_eq!(probe["endpoint"], "from-env", "schema `env` must apply to plugin options: {probe}");
        assert_eq!(probe["mode"], "turbo", "choices_from must accept sibling map key: {probe}");

        // nullable vs non-nullable validation and choices_from rejection.
        let validate = mare_shared::lua::core::config::validate_options_fn(h.lua()).unwrap();
        let null_ok = mare_shared::lua::json_to_lua_with_null(
            h.lua(),
            &json!({ "plugins": { "cfg2_probe": { "optional_tag": null } } }),
        )
        .unwrap();
        assert!(validate.call::<Vec<String>>(null_ok).unwrap().is_empty(), "nullable = true must allow null");

        let null_bad = mare_shared::lua::json_to_lua_with_null(
            h.lua(),
            &json!({ "plugins": { "cfg2_probe": { "strict_tag": null } } }),
        )
        .unwrap();
        let strict_probs = validate.call::<Vec<String>>(null_bad).unwrap();
        assert!(!strict_probs.is_empty(), "non-nullable option must reject null");

        let bad_mode = mare_shared::lua::json_to_lua(
            h.lua(),
            &json!({ "plugins": { "cfg2_probe": { "mode": "turb" } } }),
        )
        .unwrap();
        let mode_probs = validate.call::<Vec<String>>(bad_mode).unwrap().join("\n");
        assert!(
            mode_probs.contains("turb") && mode_probs.contains("turbo"),
            "choices_from must reject unknown choice with did-you-mean: {mode_probs}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn lua_memory_mb_is_invocation_only_and_slow_callback_defaults_to_1s() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', lua_memory_mb = 32 }\n",
        )
        .unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        let (inv_only, slow_default, warnings): (bool, f64, Vec<String>) = eval(
            &h,
            "return mare.options.lua_memory_mb.invocation_only == true, mare.o.slow_callback_ms, mare.config_warnings()",
        )
        .await;
        assert!(inv_only, "lua_memory_mb must be marked invocation_only = true");
        assert_eq!(slow_default, 1000.0, "slow_callback_ms default warning threshold must be 1000 ms (1 s)");
        assert!(
            warnings.iter().any(|w| w.contains("lua_memory_mb only takes effect via --set or MARE_LUA_MEMORY_MB")),
            "setting lua_memory_mb in init.lua must warn as invocation-only: {warnings:?}"
        );

        let plugin_luals = mare_plugin::types_texts()
            .into_iter()
            .find(|(name, _)| *name == "mare/plugin_options.gen.lua")
            .map(|(_, body)| body)
            .expect("mare/plugin_options.gen.lua must be generated and embedded");
        assert!(
            plugin_luals.contains("mare.PluginOpts.Approvals")
                && plugin_luals.contains("MARE_APPROVAL_POLICY")
                && plugin_luals.contains("mare.PluginOpts.Permissions"),
            "plugin_options.gen.lua must declare per-plugin option classes"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn malformed_mare_approval_policy_json_fails_approvals_startup() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        // Truncated JSON starting with `{`: must trigger `on_invalid = "fail"` instead of silently falling back to "ask".
        env.set_var("MARE_APPROVAL_POLICY", r#"{"mode":"granular","categories":{"tool":false}"#);

        let h = mare_host::Host::new(env.host().user_config(true).no_session().opts).await.unwrap();
        expose(&h);
        let problems = h.kernel.problems().join("\n");
        let failed: Option<String> = mare_shared::lua::core::config::plugin_failed(h.lua())
            .ok()
            .flatten()
            .and_then(|t| t.get::<Option<String>>("approvals").ok().flatten());
        let approvals_started: bool = eval(&h, "return C.cordis:get('approvals') ~= nil").await;
        assert!(
            !approvals_started,
            "approvals must refuse to start on malformed MARE_APPROVAL_POLICY JSON; problems={problems}"
        );
        assert!(
            failed.as_deref().is_some_and(|s| s.contains("MARE_APPROVAL_POLICY")),
            "plugin_failed.approvals must record MARE_APPROVAL_POLICY parse failure: {failed:?}"
        );
        assert!(
            problems.contains("MARE_APPROVAL_POLICY")
                && problems.contains("essential service `approvals` has no provider"),
            "host startup problems must report malformed MARE_APPROVAL_POLICY and missing essential service: {problems}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn union_override_via_env_cli_and_config_load_does_not_leak_lower_layer_categories() {
    local(async {
        let init_src = r#"
            mare.setup { model = "faux/echo" }
            mare.plugin("approvals", {
              policy = { mode = "granular", categories = { tool = true, network = true } },
            })
            "#;

        // 1) `mare_plugin::config_load::load_files` with env granular policy must replace union whole.
        {
            let mut env = TestEnv::new().await;
            std::fs::create_dir_all(env.config_dir()).unwrap();
            let init_path = env.config_dir().join("init.lua");
            std::fs::write(&init_path, init_src).unwrap();
            env.set_var("MARE_APPROVAL_POLICY", r#"{"mode":"granular","categories":{"tool":false}}"#);
            let loaded =
                mare_plugin::config_load::load_files(&env.cwd, std::slice::from_ref(&init_path)).await.unwrap();
            let cats = &loaded.value["plugins"]["approvals"]["policy"]["categories"];
            assert_eq!(cats["tool"], json!(false), "env category must apply: {loaded:?}");
            assert!(
                cats.get("network").is_none(),
                "lower-layer category `network=true` must NOT leak through config_load merge: {cats}"
            );
        }

        // 2) Host CLI `--set` layer with granular policy must also replace union whole (not merge_json).
        {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(env.config_dir().join("init.lua"), init_src).unwrap();
            let h = env
                .host()
                .user_config(true)
                .no_session()
                .config(json!({
                    "plugins": {
                        "approvals": {
                            "policy": { "mode": "granular", "categories": { "tool": false } }
                        }
                    }
                }))
                .build()
                .await;
            expose(&h);
            let policy_json: String = eval(&h, "return mare.json.encode(C.cordis:get('approvals'):policy())").await;
            let policy: Value = serde_json::from_str(&policy_json).unwrap();
            assert_eq!(policy["categories"]["tool"], json!(false), "{policy}");
            assert!(
                policy["categories"].get("network").is_none(),
                "lower-layer category `network=true` must NOT leak through CLI merge: {policy}"
            );
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn config_describe_preserves_env_source_for_dotted_overrides_and_clears_stale_child_leaves() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { model = "faux/echo" }
            mare.plugin("approvals", {
              policy = { mode = "granular", categories = { tool = true, network = true } },
            })
            "#,
        )
        .unwrap();
        env.set_var("MARE_APPROVAL_POLICY", "never");

        let h = env.host().user_config(true).no_session().build().await;
        expose(&h);

        let (described_layer, stale_mode, stale_cat): (String, bool, bool) = eval(
            &h,
            r#"
            local desc = mare.config_describe(
              { "plugins.approvals.policy" },
              { overrides = { ["plugins.approvals.policy"] = "never" } }
            )
            local src = desc["plugins.approvals.policy"].source
            local all_sources = mare.config_sources()
            return src and src.layer or "<none>",
              all_sources["plugins.approvals.policy.mode"] ~= nil,
              all_sources["plugins.approvals.policy.categories.tool"] ~= nil
            "#,
        )
        .await;
        assert_eq!(
            described_layer, "env MARE_APPROVAL_POLICY",
            "config_describe must preserve env source when dotted override matches config"
        );
        assert!(!stale_mode, "stale child source `plugins.approvals.policy.mode` must be cleared");
        assert!(!stale_cat, "stale child source `plugins.approvals.policy.categories.tool` must be cleared");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn permissions_preset_like_chain_inherits_sandbox_constraints() {
    local(async {
        let env = TestEnv::new().await;
        let extra_dir = env.cwd.join("sub_writable");
        std::fs::create_dir_all(&extra_dir).unwrap();
        // Use an existing file so deny_read resolves on every platform (missing paths such as
        // `/etc/shadow` on macOS/Windows stay unresolved and fail-closed on spill writes).
        let denied_file = env.cwd.join("denied_secret.txt");
        std::fs::write(&denied_file, "secret").unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
                mare.setup {{ model = "faux/echo" }}
                mare.plugin("permissions", {{
                  presets = {{
                    auto = {{ network = false }},
                    restricted = {{ like = "auto", deny_read = {{ {:?} }} }},
                    sub_task = {{ like = "restricted", writable = {{ {:?} }} }},
                  }},
                  preset = "sub_task",
                }})
                "#,
                denied_file.to_string_lossy(),
                extra_dir.to_string_lossy()
            ),
        )
        .unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        let spec_json: String = eval(&h, "return mare.json.encode(mare.proc.sandbox.current() or {})").await;
        let spec: Value = serde_json::from_str(&spec_json).unwrap();
        assert_eq!(
            spec["network"],
            json!(false),
            "sub_task must inherit `network = false` from overridden built-in `auto` through `like` chain: {spec}"
        );
        let denied_str = denied_file.to_string_lossy().into_owned();
        let deny_read = spec["deny_read"].as_array().cloned().unwrap_or_default();
        assert!(
            deny_read.iter().any(|v| v.as_str() == Some(denied_str.as_str())),
            "sub_task must inherit `deny_read` from `restricted` through `like` chain: {spec}"
        );
        h.shutdown().await;
    })
    .await
}
