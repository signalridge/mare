//! Public config surface invariants (CFG):
//! 1. Walking `mare` and `package.{preload,loaded}` finds zero config-engine `_` fields or plants,
//!    both after startup and after `/reload config`.
//! 2. A user-origin plugin using `ctx.options:get` / `ctx.options:set` validates, commits, and
//!    rolls back identically to RPC `options.get` / `options.set`.

use crate::common::*;
use serde_json::{Value, json};

const CHECK_SURFACE_LUA: &str = r#"
local non_cfg_allowed = {}
local bad_mare = {}
for k in next, mare do
  if type(k) == "string" and k:sub(1, 1) == "_" and not non_cfg_allowed[k] then
    bad_mare[#bad_mare + 1] = k
  end
end
table.sort(bad_mare)

local bad_preload = {}
if type(package) == "table" and type(package.preload) == "table" then
  for _, name in ipairs({ "config_api_ui_layout", "config_api_validate" }) do
    if rawget(package.preload, name) ~= nil then
      bad_preload[#bad_preload + 1] = name
    end
  end
end

local bad_loaded = {}
if type(package) == "table" and type(package.loaded) == "table" then
  for _, name in ipairs({ "mare.plugin_slots", "mare.preset_cleared", "mare.plugin_preset_entry" }) do
    if rawget(package.loaded, name) ~= nil then
      bad_loaded[#bad_loaded + 1] = name
    end
  end
end

return mare.json.encode({
  mare = bad_mare,
  preload = bad_preload,
  loaded = bad_loaded,
})
"#;

#[tokio::test]
async fn config_engine_leaves_no_private_fields_on_mare_or_package() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo', border = 'single' }\n")
            .unwrap();
        let h = env.host().user_config(true).no_session().build().await;

        let check = |stage: &str, raw: String| {
            let v: Value = serde_json::from_str(&raw).unwrap();
            let empty_list = |field: &str| {
                let items = v[field].as_array().cloned().unwrap_or_default();
                assert!(items.is_empty(), "{stage}: unexpected config internals in `{field}`: {items:?}");
            };
            empty_list("mare");
            empty_list("preload");
            empty_list("loaded");
        };

        check("startup", eval::<String>(&h, CHECK_SURFACE_LUA).await);
        h.reload_config().await.unwrap();
        check("after reload", eval::<String>(&h, CHECK_SURFACE_LUA).await);

        h.shutdown().await;
    })
    .await
}

const OWN_PLUGIN: &str = r#"
return {
  name = "own",
  contracts = {
    own = {
      owner = "own",
      events = {
        refuse = { doc = "toggle own plugin refusal", mode = "emit", expose = "process" },
        cfg = { doc = "read own plugin config", mode = "bail", expose = "process" },
      },
    },
  },
  config_schema = {
    num = { type = "integer", default = 1, apply = "live", desc = "a number" },
    obj = {
      type = "object",
      apply = "live",
      desc = "an object",
      fields = {
        a = { type = "integer", default = 1, desc = "a" },
        b = { type = "integer", default = 2, desc = "b" },
      },
    },
    secret_pin = { type = "string", sensitive = true, apply = "live", desc = "a secret" },
  },
  apply = function(ctx, cfg)
    local refuse_num = false
    ctx:on("own/refuse", function(flag)
      refuse_num = flag == true
    end)
    ctx:on("own/cfg", function()
      return { num = cfg.num, a = cfg.obj and cfg.obj.a, b = cfg.obj and cfg.obj.b, secret_pin = cfg.secret_pin }
    end)
    ctx:on("option/apply", function(ev)
      if ev.name == "plugins.own.num" and refuse_num and not ev.restore then
        error("own refuses plugins.own.num")
      end
    end)
  end,
}
"#;

const OPT_PROBE_PLUGIN: &str = r#"
return {
  name = "opt_probe",
  inject = { "options" },
  contracts = {
    opt_probe = {
      owner = "opt_probe",
      events = {
        get = { doc = "probe ctx.options:get", mode = "bail", expose = "process" },
        set = { doc = "probe ctx.options:set", mode = "bail", expose = "process" },
      },
    },
  },
  apply = function(ctx)
    ctx:on("opt_probe/get", function(req)
      return ctx.options:get(req and req.keys or nil, req and req.opts or nil)
    end)
    ctx:on("opt_probe/set", function(req)
      local ok, err = pcall(function()
        ctx.options:set(req.values, req.opts)
      end)
      return { ok = ok, err = ok and nil or tostring(err) }
    end)
  end,
}
"#;

#[tokio::test]
async fn user_plugin_options_service_validates_and_commits_like_rpc() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/own.lua"), OWN_PLUGIN).unwrap();
        std::fs::write(env.config_dir().join("plugins/opt_probe.lua"), OPT_PROBE_PLUGIN).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', border = 'rounded' }\nmare.plugin('own', { secret_pin = 'TOPSECRET' })\n",
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        expose(&h);

        let opt_get = |req: Value| {
            let h = h.clone();
            async move {
                let s: String = eval(
                    &h,
                    &format!("return mare.json.encode(C.cordis:bail('opt_probe/get', mare.json.decode({:?})))", req.to_string()),
                )
                .await;
                serde_json::from_str::<Value>(&s).unwrap()
            }
        };
        let opt_set = |req: Value| {
            let h = h.clone();
            async move {
                let s: String = eval(
                    &h,
                    &format!("return mare.json.encode(C.cordis:bail('opt_probe/set', mare.json.decode({:?})))", req.to_string()),
                )
                .await;
                serde_json::from_str::<Value>(&s).unwrap()
            }
        };

        // 1. Reading options includes live overrides and redacts sensitive plugin leaves.
        let got = opt_get(json!({ "keys": ["model", "border", "plugins.own.secret_pin"] })).await;
        assert_eq!(got["model"], "faux/echo", "{got}");
        assert_eq!(got["border"], "rounded", "{got}");
        assert_eq!(got["plugins.own.secret_pin"], "[REDACTED]", "{got}");

        let described = opt_get(json!({ "keys": ["border"], "opts": { "describe": true } })).await;
        assert_eq!(described["border"]["effective"], "rounded", "{described}");

        // 2. validate_only checks without mutating committed or running values.
        let dry_ok = opt_set(json!({
            "values": { "border": "thick", "plugins.own.num": 99 },
            "opts": { "validate_only": true }
        }))
        .await;
        assert_eq!(dry_ok["ok"], true, "{dry_ok}");
        assert_eq!(eval::<String>(&h, "return mare.o.border").await, "rounded");
        let own_after_dry: Value = serde_json::from_str(
            &eval::<String>(&h, "return mare.json.encode(C.cordis:bail('own/cfg'))").await,
        )
        .unwrap();
        assert_eq!(own_after_dry["num"], 1, "{own_after_dry}");

        // 3. Invalid batches are rejected in both validate_only and commit modes.
        for bad in [
            json!({ "border": 123 }),
            json!({ "plugins.own.num": "not-an-int" }),
            json!({ "approval_constraints": {} }),
            json!({ "plugins": {} }),
        ] {
            for validate_only in [true, false] {
                let res = opt_set(json!({
                    "values": bad,
                    "opts": { "validate_only": validate_only }
                }))
                .await;
                assert_eq!(res["ok"], false, "expected failure for {bad} (validate_only={validate_only}): {res}");
            }
        }

        // 4. Valid batch commits core options + dotted plugin options + model coordination.
        let ok = opt_set(json!({
            "values": {
                "model": "faux/echo2",
                "border": "double",
                "plugins.own.num": 42,
                "plugins.own.obj.a": 7
            }
        }))
        .await;
        assert_eq!(ok["ok"], true, "{ok}");
        let got2 = opt_get(json!({ "keys": ["model", "border", "plugins.own.num"] })).await;
        assert_eq!(got2["model"], "faux/echo2", "{got2}");
        assert_eq!(got2["border"], "double", "{got2}");
        assert_eq!(got2["plugins.own.num"], 42, "{got2}");
        let own_after_set: Value = serde_json::from_str(
            &eval::<String>(&h, "return mare.json.encode(C.cordis:bail('own/cfg'))").await,
        )
        .unwrap();
        assert_eq!(own_after_set["num"], 42, "{own_after_set}");
        assert_eq!(own_after_set["a"], 7, "{own_after_set}");
        assert_eq!(own_after_set["b"], 2, "{own_after_set}");

        // 5. Refusal by a plugin owner rolls back earlier core options and model in the same batch.
        exec(&h, "C.cordis:emit('own/refuse', true)").await;
        let refused = opt_set(json!({
            "values": {
                "model": "faux/echo",
                "border": "single",
                "plugins.own.num": 77
            }
        }))
        .await;
        assert_eq!(refused["ok"], false, "{refused}");
        assert!(refused["err"].as_str().unwrap_or("").contains("own refuses"), "{refused}");
        let got3 = opt_get(json!({ "keys": ["model", "border", "plugins.own.num"] })).await;
        assert_eq!(got3["model"], "faux/echo2", "model must roll back on batch refusal: {got3}");
        assert_eq!(got3["border"], "double", "border must roll back on batch refusal: {got3}");
        assert_eq!(got3["plugins.own.num"], 42, "plugin num must remain unchanged: {got3}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn yielding_reload_concurrent_options_set_does_not_crash_on_missing_config_write() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/opt_probe.lua"), OPT_PROBE_PLUGIN).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo', border = 'rounded' }\n")
            .unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        expose(&h);

        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { model = 'faux/echo', border = 'single' }
            mare._cfg_yield_started = true
            for _ = 1, 12000 do
              if mare._cfg_yield_release then break end
              mare.task.sleep(5)
            end
            "#,
        )
        .unwrap();

        let (reload, ()) = tokio::join!(h.reload_config(), async {
            wait_until(15, "yielding reload did not start", || async {
                eval::<bool>(&h, "return mare._cfg_yield_started == true").await
            })
            .await;

            // Updating `model` while init.lua yields must succeed (not crash on nil config_write).
            let model_res: Value = serde_json::from_str(
                &eval::<String>(
                    &h,
                    r#"return mare.json.encode(C.cordis:bail("opt_probe/set", { values = { model = "faux/echo2" } }))"#,
                )
                .await,
            )
            .unwrap();
            assert_eq!(model_res["ok"], true, "concurrent model update during yielding reload failed: {model_res}");

            // Updating a core option during yielding reload must return the clean reload refusal, not nil->Function.
            let border_res: Value = serde_json::from_str(
                &eval::<String>(
                    &h,
                    r#"return mare.json.encode(C.cordis:bail("opt_probe/set", { values = { border = "thick" } }))"#,
                )
                .await,
            )
            .unwrap();
            assert_eq!(border_res["ok"], false, "{border_res}");
            assert!(
                border_res["err"].as_str().unwrap_or("").contains("retry afterwards"),
                "expected clean reload-in-progress refusal, got: {border_res}"
            );

            exec(&h, "mare._cfg_yield_release = true").await;
        });
        reload.unwrap();
        assert_eq!(eval::<String>(&h, "return mare.o.border").await, "single");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn options_service_uses_contract_poison_barrier_unified_save_and_effort_rollback() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/own.lua"), OWN_PLUGIN).unwrap();
        std::fs::write(env.config_dir().join("plugins/opt_probe.lua"), OPT_PROBE_PLUGIN).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo', border = 'rounded' }\n")
            .unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        expose(&h);

        // 1. `options` is contracted and has generated LuaLS definitions.
        assert!(
            mare_plugin::types_texts().iter().any(|(name, _)| *name == "mare/services/options.gen.lua"),
            "types/mare/services/options.gen.lua must be generated from contracts/options.lua"
        );

        let opt_get = |req: Value| {
            let h = h.clone();
            async move {
                let s: String = eval(
                    &h,
                    &format!(
                        "return mare.json.encode(C.cordis:bail('opt_probe/get', mare.json.decode({:?})))",
                        req.to_string()
                    ),
                )
                .await;
                serde_json::from_str::<Value>(&s).unwrap()
            }
        };
        let opt_set = |req: Value| {
            let h = h.clone();
            async move {
                let s: String = eval(
                    &h,
                    &format!(
                        "return mare.json.encode(C.cordis:bail('opt_probe/set', mare.json.decode({:?})))",
                        req.to_string()
                    ),
                )
                .await;
                serde_json::from_str::<Value>(&s).unwrap()
            }
        };

        // 2. validate_only rejects unknown model, invalid effort, and null effort.
        for bad in [json!({ "model": "nonexistent/model" }), json!({ "effort": "bogus-effort" })] {
            let res = opt_set(json!({ "values": bad, "opts": { "validate_only": true } })).await;
            assert_eq!(res["ok"], false, "validate_only must reject {bad}: {res}");
        }
        let null_effort: Value = serde_json::from_str(
            &eval::<String>(
                &h,
                r#"return mare.json.encode(C.cordis:bail("opt_probe/set", { values = { effort = mare.json.null }, opts = { validate_only = true } }))"#,
            )
            .await,
        )
        .unwrap();
        assert_eq!(null_effort["ok"], false, "validate_only must reject effort = mare.json.null: {null_effort}");

        // 3. Initial unset effort (e.g. starting on faux/echo2 where raw_effort() == nil) rolls back
        //    completely when a batch `{ model = "faux/echo", effort = "high", ["plugins.own.num"] = 77 }` fails.
        let switch_echo2 = opt_set(json!({ "values": { "model": "faux/echo2" } })).await;
        assert_eq!(switch_echo2["ok"], true, "{switch_echo2}");
        assert!(eval::<bool>(&h, "return C.cordis:get('llm'):raw_effort() == nil").await);
        exec(&h, "C.cordis:emit('own/refuse', true)").await;
        let refused_effort = opt_set(json!({
            "values": { "model": "faux/echo", "effort": "high", "plugins.own.num": 77 }
        }))
        .await;
        assert_eq!(refused_effort["ok"], false, "{refused_effort}");
        assert!(
            eval::<bool>(&h, "return C.cordis:get('llm'):raw_effort() == nil").await,
            "unset effort must roll back to nil when batch fails"
        );
        let got_after_refusal = opt_get(json!({ "keys": ["model", "effort"] })).await;
        assert_eq!(got_after_refusal["model"], "faux/echo2", "model must roll back: {got_after_refusal}");
        assert!(got_after_refusal.get("effort").is_none(), "effort must remain unset: {got_after_refusal}");

        // Switching to faux/echo afterwards must see its default effort ("medium"), NOT leaked "high".
        exec(&h, "C.cordis:emit('own/refuse', false)").await;
        let switch_echo = opt_set(json!({ "values": { "model": "faux/echo" } })).await;
        assert_eq!(switch_echo["ok"], true, "{switch_echo}");
        assert_eq!(
            eval::<String>(&h, "return tostring(C.cordis:get('llm'):raw_effort())").await,
            "medium",
            "rolled-back effort must not leak onto faux/echo"
        );

        // 4. Unified save semantics with RPC: committed model write persists to state.json,
        //    and a refused batch does NOT leave the failed model in state.json.
        let saved = opt_set(json!({ "values": { "model": "faux/echo2" } })).await;
        assert_eq!(saved["ok"], true, "{saved}");
        let state_after_ok = mare_config::read_state().unwrap().unwrap_or_default();
        assert_eq!(
            state_after_ok["model"], "faux/echo2",
            "ctx.options:set must persist model like RPC options.set: {state_after_ok}"
        );

        exec(&h, "C.cordis:emit('own/refuse', true)").await;
        let refused_model = opt_set(json!({
            "values": { "model": "faux/echo", "plugins.own.num": 88 }
        }))
        .await;
        assert_eq!(refused_model["ok"], false, "{refused_model}");
        let state_after_fail = mare_config::read_state().unwrap().unwrap_or_default();
        assert_eq!(
            state_after_fail["model"], "faux/echo2",
            "rolled-back batch must not leave failed model in state.json: {state_after_fail}"
        );

        // 5. Poisoned session barrier refuses ctx.options:set while allowing read-only ctx.options:get.
        mare_shared::session_barrier::barrier(h.lua()).poison("disk full");
        let get_poisoned = opt_get(json!({ "keys": ["border"] })).await;
        assert_eq!(get_poisoned["border"], "rounded", "read-only options:get must still work when poisoned");
        let set_poisoned = opt_set(json!({ "values": { "border": "single" } })).await;
        assert_eq!(set_poisoned["ok"], false, "poisoned session barrier must refuse ctx.options:set: {set_poisoned}");
        assert!(
            set_poisoned["err"].as_str().unwrap_or("").contains("disk full")
                || set_poisoned["err"].as_str().unwrap_or("").contains("poison"),
            "expected session barrier error, got: {set_poisoned}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn config_warnings_are_redacted_across_all_exits() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let env_secret = "sk-cfg-env-secret-1234567890abcdef";
        let lua_secret = "sk-cfg-lua-secret-1234567890abcdef";
        env.set_var("MARE_PROVIDERS", format!(r#"{{"openai":{{"api_key":"{env_secret}""#));
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("mare.setup {{ model = 'faux/echo', ui = {{ working = {{ glyph = '{lua_secret}' }} }} }}\n"),
        )
        .unwrap();

        // 1. env_warnings redacts malformed JSON env values before CLI/ACP/strict-config report them.
        let env_warns = mare_config::env_warnings().join("\n");
        assert!(!env_warns.contains(env_secret), "env_warnings leaked secret: {env_warns}");
        assert!(env_warns.contains("[REDACTED]"), "env_warnings must redact secret: {env_warns}");

        // 2. mare.config_warnings(), Rust core::config::warnings_from_lua(), and reload_config() warnings all redact.
        let h = env.host().user_config(true).no_session().build().await;
        let lua_warns = eval::<Vec<String>>(&h, "return mare.config_warnings()").await.join("\n");
        assert!(!lua_warns.contains(lua_secret), "mare.config_warnings leaked secret: {lua_warns}");
        assert!(lua_warns.contains("[REDACTED]"), "mare.config_warnings must redact secret: {lua_warns}");

        let rust_warns = mare_shared::lua::core::config::warnings_from_lua(h.lua()).join("\n");
        assert!(!rust_warns.contains(lua_secret), "warnings_from_lua leaked secret: {rust_warns}");

        let reload = h.reload_config().await.unwrap();
        let reload_warns = reload["warnings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!reload_warns.contains(lua_secret), "reload_config warnings leaked secret: {reload_warns}");
        assert!(reload_warns.contains("[REDACTED]"), "reload_config warnings must redact secret: {reload_warns}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn config_eval_exposes_no_config_staging_fields_and_preserves_keys_components_reload() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");

        let inspect_snippet = r#"
local real_mare = (getmetatable(mare) and getmetatable(mare).__index) or mare
local seen = {}
for k in next, real_mare do
  if type(k) == "string" and (k == "_in_config" or k:sub(1, 7) == "_config") then
    seen[#seen + 1] = k
  end
end
table.sort(seen)
mare.test_cfg_bridge_seen = mare.test_cfg_bridge_seen or {}
mare.test_cfg_bridge_seen[#mare.test_cfg_bridge_seen + 1] = table.concat(seen, ",")
"#;

        // 1. Startup evaluation with keymap context, unique keymap, setup.keys, renderer, and status.
        std::fs::write(
            &init,
            format!(
                r#"
{inspect_snippet}
mare.setup {{
  model = "faux/echo",
  keys = {{ ["<A-1>"] = "transcript" }},
}}
mare.keymap.context("editor.cfgbridge", {{ parent = "editor", active = true }})
mare.keymap.set("editor.cfgbridge", "<C-g>", function() end, {{ unique = true, desc = "v1-unique" }})
mare.ui.renderer("tool:cfgbridge", {{ compact = function() return "v1" end }})
mare.test_cfg_bridge_status_v = nil
mare.ui.status("cfgbridge_status", {{
  render = function()
    mare.test_cfg_bridge_status_v = "v1"
    return mare.ui.Span("v1")
  end,
}})
"#
            ),
        )
        .unwrap();

        let h = env.host().user_config(true).no_session().build().await;

        let check_no_staging_fields = |phase: &str, runs: Vec<String>| {
            assert!(!runs.is_empty(), "{phase}: init.lua did not record mare keys");
            for (idx, seen) in runs.iter().enumerate() {
                assert!(
                    seen.is_empty(),
                    "{phase} run #{idx}: mare must expose zero _in_config/_config* fields during evaluation, saw: [{seen}]"
                );
            }
        };

        check_no_staging_fields("startup", eval::<Vec<String>>(&h, "return mare.test_cfg_bridge_seen or {}").await);
        assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('tool:cfgbridge').compact()").await, "v1");
        assert_eq!(eval::<String>(&h, "return mare.test_cfg_bridge_status_v").await, "v1");
        assert_eq!(
            eval::<String>(
                &h,
                r#"
                for _, e in ipairs(mare.keymap.list({ context = "editor.cfgbridge" })) do
                  if e.key == "ctrl+g" then return e.desc or "" end
                end
                return ""
                "#
            )
            .await,
            "v1-unique"
        );
        assert!(
            eval::<bool>(
                &h,
                r#"
                for _, e in ipairs(mare.keymap.list({ context = "editor" })) do
                  if e.key == "alt+1" then return true end
                end
                return false
                "#
            )
            .await,
            "v1 setup.keys <A-1> must be bound"
        );

        // 2. Failing staged reload: stages v2 keys/context/setup.keys/renderer/status, yields so
        //    runtime observer also checks `mare`, then errors to trigger full rollback.
        exec(&h, "mare.test_cfg_bridge_seen = {}").await;
        std::fs::write(
            &init,
            format!(
                r#"
{inspect_snippet}
mare.setup {{
  model = "faux/echo",
  keys = {{ ["<A-2>"] = "transcript" }},
}}
mare.keymap.context("editor.cfgbridge", {{ parent = "editor", active = true }})
mare.keymap.context("editor.cfgbridge_bad", {{ parent = "editor", active = true }})
mare.keymap.set("editor.cfgbridge", "<C-g>", function() end, {{ unique = true, desc = "v2-rolled-back" }})
mare.keymap.set("editor", "<F9>", function() end, {{ desc = "v2-extra" }})
mare.ui.renderer("tool:cfgbridge", {{ compact = function() return "v2-rolled-back" end }})
mare.ui.renderer("tool:cfgbridge_bad", {{ compact = function() return "bad" end }})
mare.ui.status("cfgbridge_status", {{
  render = function()
    mare.test_cfg_bridge_status_v = "v2-rolled-back"
    return mare.ui.Span("v2-rolled-back")
  end,
}})
mare.test_cfg_bridge_waiting = true
for _ = 1, 6000 do
  if mare.test_cfg_bridge_release then break end
  mare.task.sleep(5)
end
error("cfg-bridge rollback boom")
"#
            ),
        )
        .unwrap();

        let (failed_reload, runtime_seen) = tokio::join!(h.reload_config(), async {
            wait_until(15, "yielding reload did not start", || async {
                eval::<bool>(&h, "return mare.test_cfg_bridge_waiting == true").await
            })
            .await;
            let seen_raw = eval::<String>(
                &h,
                r#"
                local bad = {}
                for k in next, mare do
                  if type(k) == "string" and (k == "_in_config" or k:sub(1, 7) == "_config") then
                    bad[#bad + 1] = k
                  end
                end
                table.sort(bad)
                return table.concat(bad, ",")
                "#,
            )
            .await;
            exec(&h, "mare.test_cfg_bridge_release = true").await;
            seen_raw
        });
        assert!(failed_reload.is_err(), "failing init.lua must fail reload");
        assert!(
            runtime_seen.is_empty(),
            "runtime observer saw staging fields on mare while init.lua yielded: [{runtime_seen}]"
        );
        check_no_staging_fields(
            "failed reload",
            eval::<Vec<String>>(&h, "return mare.test_cfg_bridge_seen or {}").await,
        );

        // Verify v1 keymaps, contexts, setup.keys, and UI components rolled back intact.
        assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('tool:cfgbridge').compact()").await, "v1");
        assert!(eval::<bool>(&h, "return mare.ui.get_renderer('tool:cfgbridge_bad') == nil").await);
        exec(&h, "mare.ui.status_redraw('cfgbridge_status')").await;
        assert_eq!(eval::<String>(&h, "return mare.test_cfg_bridge_status_v").await, "v1");
        assert_eq!(
            eval::<String>(
                &h,
                r#"
                for _, e in ipairs(mare.keymap.list({ context = "editor.cfgbridge" })) do
                  if e.key == "ctrl+g" then return e.desc or "" end
                end
                return ""
                "#
            )
            .await,
            "v1-unique"
        );
        assert!(
            !eval::<bool>(
                &h,
                r#"
                for _, e in ipairs(mare.keymap.list({ context = "editor" })) do
                  if e.key == "f9" or e.key == "alt+2" then return true end
                end
                return false
                "#
            )
            .await,
            "rolled-back bindings must not leak"
        );
        assert!(
            eval::<bool>(
                &h,
                r#"
                for _, e in ipairs(mare.keymap.list({ context = "editor" })) do
                  if e.key == "alt+1" then return true end
                end
                return false
                "#
            )
            .await,
            "v1 setup.keys <A-1> must survive rollback"
        );

        // 3. Succeeding hot reload: replaces unique keymap (exercising stage validate against
        //    previous keymaps), replaces setup.keys, reuses context, and swaps UI components.
        exec(&h, "mare.test_cfg_bridge_seen = {}").await;
        std::fs::write(
            &init,
            format!(
                r#"
{inspect_snippet}
mare.setup {{
  model = "faux/echo",
  keys = {{ ["<A-2>"] = "transcript" }},
}}
mare.keymap.context("editor.cfgbridge", {{ parent = "editor", active = true }})
mare.keymap.set("editor.cfgbridge", "<C-g>", function() end, {{ unique = true, desc = "v2-unique" }})
mare.ui.renderer("tool:cfgbridge", {{ compact = function() return "v2" end }})
mare.ui.status("cfgbridge_status", {{
  render = function()
    mare.test_cfg_bridge_status_v = "v2"
    return mare.ui.Span("v2")
  end,
}})
"#
            ),
        )
        .unwrap();

        h.reload_config().await.unwrap();
        check_no_staging_fields(
            "hot reload",
            eval::<Vec<String>>(&h, "return mare.test_cfg_bridge_seen or {}").await,
        );
        assert_eq!(eval::<String>(&h, "return mare.ui.get_renderer('tool:cfgbridge').compact()").await, "v2");
        exec(&h, "mare.ui.status_redraw('cfgbridge_status')").await;
        assert_eq!(eval::<String>(&h, "return mare.test_cfg_bridge_status_v").await, "v2");
        assert_eq!(
            eval::<(i64, String)>(
                &h,
                r#"
                local n, desc = 0, ""
                for _, e in ipairs(mare.keymap.list({ context = "editor.cfgbridge" })) do
                  if e.key == "ctrl+g" then
                    n = n + 1
                    desc = e.desc or ""
                  end
                end
                return n, desc
                "#
            )
            .await,
            (1, "v2-unique".into())
        );
        assert_eq!(
            eval::<(bool, bool)>(
                &h,
                r#"
                local has1, has2 = false, false
                for _, e in ipairs(mare.keymap.list({ context = "editor" })) do
                  if e.key == "alt+1" then has1 = true end
                  if e.key == "alt+2" then has2 = true end
                end
                return has1, has2
                "#
            )
            .await,
            (false, true),
            "setup.keys must hot-reload from <A-1> to <A-2>"
        );

        h.shutdown().await;
    })
    .await
}
