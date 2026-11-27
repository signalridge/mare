use super::*;

/// 条: every layer records its explicit values leaf by leaf (lists as a whole) with
/// the layer and, for a file, file:line — init.lua (`mare.setup`, `mare.plugin`) and the command
/// line alike, nested tables included.
#[tokio::test]
async fn sources_are_recorded_leaf_by_leaf_in_every_layer() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/probe.lua"), PROBE).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', plugins = { probe = { nested = { flag = true } } } }\n\
             mare.plugin('probe', { nested = { label = 'y' }, names = { 'a', 'b' } })\n",
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .no_session()
            .config(json!({ "plugins": { "probe": { "limits": { "x": 3 }, "count": 2 } } }))
            .build()
            .await;
        let src = |key: &str| {
            let key = key.to_string();
            let h = h.clone();
            async move {
                let s: String = eval(
                    &h,
                    &format!("local s = mare.config_sources()['{key}'] return s and (s.layer or '?') .. '@' .. tostring(s.where) or 'none'"),
                )
                .await;
                s
            }
        };
        let flag = src("plugins.probe.nested.flag").await;
        assert!(flag.starts_with("user@") && flag.contains("init.lua:1"), "{flag}");
        let label = src("plugins.probe.nested.label").await;
        assert!(label.contains("init.lua:2"), "{label}");
        let names = src("plugins.probe.names").await;
        assert!(names.contains("init.lua:2"), "a list is one leaf: {names}");
        assert_eq!(src("plugins.probe.names.1").await, "none", "no leaf inside a list");
        assert!(src("plugins.probe.limits.x").await.starts_with("cli@"), "{}", src("plugins.probe.limits.x").await);
        assert!(src("plugins.probe.count").await.starts_with("cli@"));
        h.shutdown().await;
    })
    .await
}

const LIVE: &str = r#"
return { name = "live",
  config_schema = {
    rate = { type = "integer", default = 1, min = 1, apply = "next_op", desc = "read on every use" },
    boot = { type = "string", default = "a", desc = "read once at apply" },
  },
  provide = { "live" },
  contracts = { live = { methods = { read = { doc = "an option's running value", expose = "lua",
    params = { type = "object", properties = { key = { type = "string" } }, required = { "key" },
      additionalProperties = false }, order = { "key" }, result = {} } } } },
  apply = function(ctx, cfg)
    mare.g.live_applies = (mare.g.live_applies or 0) + 1
    -- the running plugin's own cfg table, read when asked
    local Live = require("cordis").class("Live", mare.Service)
    function Live:constructor(c) Live.super.constructor(self, c, "live") end
    function Live:read(key) return cfg[key] end
    ctx:plugin(Live)
  end,
}
"#;

/// 条: `mare.plugin(name, {...})` outside config execution validates the candidate,
/// commits it, emits `option/changed { name = "plugins.<p>.<path>" }` and applies it as declared:
/// `next_op`/`live` reach the running plugin's `cfg` in place, anything else reloads the plugin; an
/// invalid write fails and changes nothing.
#[tokio::test]
async fn runtime_plugin_writes_validate_emit_and_apply() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/live.lua"), LIVE).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        expose(&h);
        exec(&h, "CHANGED = {} C.cordis:on('option/changed', function(e) CHANGED[#CHANGED + 1] = e.name .. '=' .. tostring(e.value) end)").await;

        exec(&h, "mare.plugin('live', { rate = 5 })").await;
        exec(&h, "LIVE = C.cordis:get('live')").await;
        let (rate, applies): (i64, i64) = eval(&h, "return LIVE:read('rate'), mare.g.live_applies").await;
        assert_eq!((rate, applies), (5, 1), "next_op: the running cfg changes, no reload");

        exec(&h, "mare.plugin('live', { boot = 'b' })").await;
        let applied = |h: std::sync::Arc<mare_host::Host>| async move {
            for _ in 0..200 {
                let n: i64 = eval(&h, "return mare.g.live_applies").await;
                if n == 2 {
                    return true;
                }
                tokio::task::yield_now().await;
            }
            false
        };
        assert!(applied(h.clone()).await, "an option without live/next_op reloads the plugin");
        let (boot, rate): (String, i64) = eval(&h, "local L = C.cordis:get('live') return L:read('boot'), L:read('rate')").await;
        assert_eq!((boot.as_str(), rate), ("b", 5), "the reloaded plugin starts with every committed value");

        let err: String = eval(&h, "local ok, e = pcall(mare.plugin, 'live', { rate = 0 }) return tostring(e)").await;
        assert!(err.contains("below the minimum 1"), "{err}");
        assert_eq!(eval::<i64>(&h, "return C.cordis:get('live'):read('rate')").await, 5, "a refused write changes nothing");

        let changed: Vec<String> = eval(&h, "return CHANGED").await;
        assert_eq!(changed, vec!["plugins.live.rate=5", "plugins.live.boot=b"], "{changed:?}");
        h.shutdown().await;
    })
    .await
}

/// 条 / 第 9 条: `config_schema` replaced `config` + `config_types`, no alias. A
/// definition still declaring them is refused with the fix in the message, never run with an
/// empty `cfg`.
#[tokio::test]
async fn a_definition_with_the_old_config_fields_is_refused() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/old.lua"),
            "return { name = 'old', config = { n = 1 }, config_types = { m = 'string' }, apply = function() mare.g.old_ran = true end }",
        )
        .unwrap();
        let opts = env.host().user_plugins().no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        assert!(!eval::<bool>(&h, "return mare.g.old_ran == true").await);
        let w = h.plugin_warnings().join("\n");
        assert!(w.contains("`old`") && w.contains("config_schema") && w.contains("config_types"), "{w}");
        h.shutdown().await;
    })
    .await
}

/// A field the legacy removal took away is ignored, not translated, and said so on the config
/// warning list (the channel unknown options use) with the plugin, the field and what replaces it;
/// the plugin still starts. Once per field however often the definition is loaded (a reload too).
/// The declaration is built from `field` and `value` (the source then names no removed field itself,
/// which the legacy-api lint would refuse).
async fn removed_field_is_warned(field: &str, value: &str, instead: &str) {
    let env = TestEnv::new().await;
    std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
    std::fs::write(
        env.config_dir().join("plugins/legacy-def.lua"),
        format!(
            "return {{ name = 'legacy-def', {field} = {value}, apply = function() mare.g.legacy_def_ran = true end }}"
        ),
    )
    .unwrap();
    let opts = env.host().user_plugins().no_session().opts;
    let h = mare_host::Host::new(opts).await.unwrap();
    assert!(eval::<bool>(&h, "return mare.g.legacy_def_ran == true").await, "the plugin still starts");
    let about = |warnings: &[String]| -> Vec<String> {
        warnings.iter().filter(|w| w.contains("`legacy-def`") && w.contains(&format!("`{field}`"))).cloned().collect()
    };
    let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
    let found = about(&warnings);
    assert_eq!(found.len(), 1, "one warning for `{field}`: {warnings:?}");
    assert!(found[0].contains("was removed") && found[0].contains(instead), "{}", found[0]);
    h.reload_all().await.unwrap();
    let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
    assert_eq!(about(&warnings).len(), 1, "still one after a reload: {warnings:?}");
    h.shutdown().await;
}

#[tokio::test]
async fn a_definition_with_a_removed_field_is_warned() {
    local(async {
        for (field, value, instead) in [
            ("optional", "{ 'plan_mode' }", "ctx:inject"),
            ("cordis", "true", "delete the field"),
            ("startup", "'deferred'", "no deferred start"),
        ] {
            removed_field_is_warned(field, value, instead).await;
        }
    })
    .await
}

/// A field no definition has is ignored and warned about with the nearest field it may mean.
#[tokio::test]
async fn a_definition_with_an_unknown_field_is_warned_with_a_hint() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/typo-def.lua"),
            "return { name = 'typo-def', inejct = { 'llm' }, apply = function() mare.g.typo_def_ran = true end }",
        )
        .unwrap();
        let opts = env.host().user_plugins().no_session().opts;
        let h = mare_host::Host::new(opts).await.unwrap();
        assert!(eval::<bool>(&h, "return mare.g.typo_def_ran == true").await);
        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        assert!(
            warnings.iter().any(|w| w.contains("`typo-def`")
                && w.contains("unknown field `inejct`")
                && w.contains("did you mean `inject`")),
            "{warnings:?}"
        );
        h.shutdown().await;
    })
    .await
}

const GUARD: &str = r#"
return { name = "guard",
  config_schema = {
    policy = { type = "union", default = "ask", apply = "live", desc = "a word or a granular table",
      variants = {
        { type = "enum", choices = { "ask", "never" } },
        { type = "object", fields = {
          mode = { type = "enum", choices = { "granular" } },
          categories = { type = "map", values = { type = "boolean" } },
        } },
      } },
    note = { type = "string", apply = "live", desc = "optional, no default" },
  },
  provide = { "guard" },
  contracts = { guard = { methods = {
    read = { doc = "the running policy", expose = "lua",
      params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } },
    note = { doc = "the running note", expose = "lua",
      params = { type = "object", properties = {}, additionalProperties = false }, order = {}, result = { type = "string" } } } } },
  apply = function(ctx, cfg)
    local Guard = require("cordis").class("Guard", mare.Service)
    function Guard:constructor(c) Guard.super.constructor(self, c, "guard") end
    function Guard:read() return mare.json.encode(cfg.policy) end
    function Guard:note() return tostring(cfg.note) end
    ctx:plugin(Guard)
    -- an observer (a UI, an RPC event stream): its last view must end on the value in effect, and
    -- at each event the config it reads back must already say the same
    ctx:on("option/changed", function(ev)
      if ev.name == "plugins.guard.policy" then
        mare.g.guard_seen = mare.json.encode(ev.value)
        if mare.json.encode(mare.config.plugins.guard.policy) ~= mare.g.guard_seen then
          mare.g.guard_torn = (mare.g.guard_torn or 0) + 1
        end
      end
    end)
    -- the owner applies the change and can refuse it (a failed save): the write must fail too.
    -- `note = "slow"` waits for mare.g.release first, so another write can land meanwhile
    ctx:on("option/apply", function(ev)
      if ev.name == "plugins.guard.policy" and ev.value == "never" then error("policy could not be saved") end
      if ev.name == "plugins.guard.note" and ev.value == "slow" then
        while not mare.g.release do mare.task.sleep(1) end
        error("slow note refused")
      end
    end)
  end,
}
"#;

/// A `union` value must match one declared form exactly (an
/// unknown field or a wrong leaf refuses it), and a runtime write's apply is part of the write: when
/// the owner refuses it (its save failed) the caller gets the failure and nothing changes — not an
/// async `option/changed` the writer never hears about.
///
/// Reference tests: reference implementation (a plugin
/// rejecting activation after validation restores the patch), Kimi
/// `packages/agent-core-v2/test/app/config/config.test.ts:3244` (a failed write leaves memory and
/// file unchanged) and `:2993` (change events only after everything took effect), Codex
/// `codex-rs/app-server/src/config_manager_service_tests.rs:1717` (an invalid `approval_policy` is
/// refused, the file unchanged); reference implementation (one-of
/// forms) and Codex `codex-rs/config/src/strict_config_tests.rs:73` (unknown keys refused).
#[tokio::test]
async fn a_union_matches_one_form_and_a_refused_apply_fails_the_write() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/guard.lua"), GUARD).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }\n").unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        expose(&h);
        exec(&h, "G = C.cordis:get('guard')").await;
        // a refused first write of an option with a default leaves the running value at that
        // default, not nil (the rollback re-syncs an absent key as its declared default)
        let first: String =
            eval(&h, "local ok, e = pcall(mare.plugin, 'guard', { policy = 'never' }) return tostring(ok) .. ' ' .. G:read()").await;
        assert_eq!(first, r#"false "ask""#, "running value after a refused first write");
        let write = |lua: &'static str| {
            let h = h.clone();
            async move { eval::<String>(&h, &format!("local ok, e = pcall(mare.plugin, 'guard', {{ policy = {lua} }}) return ok and 'ok' or tostring(e)")).await }
        };
        assert_eq!(write("{ mode = 'granular', categories = { tool = true } }").await, "ok");
        assert_eq!(eval::<String>(&h, "return G:read()").await, r#"{"categories":{"tool":true},"mode":"granular"}"#);
        for bad in ["{ mode = 'granular', categories = { tool = 'yes' } }", "{ mode = 'granular', extra = 1 }", "'sometimes'", "42"] {
            let e = write(bad).await;
            assert!(e.contains("option `policy`"), "{bad}: {e}");
        }
        let refused = write("'never'").await;
        assert!(refused.contains("policy could not be saved"), "{refused}");
        assert_eq!(
            eval::<String>(&h, "return G:read()").await,
            r#"{"categories":{"tool":true},"mode":"granular"}"#,
            "a refused apply leaves the running value"
        );
        assert_eq!(eval::<String>(&h, "return mare.json.encode(mare.config.plugins.guard.policy)").await,
            r#"{"categories":{"tool":true},"mode":"granular"}"#, "and the committed value");
        // Kimi config.test.ts:2993 (change events only describe what took effect): an observer
        // that already saw the refused value is told the restored one
        assert_eq!(
            eval::<String>(&h, "return mare.g.guard_seen").await,
            r#"{"categories":{"tool":true},"mode":"granular"}"#,
            "an observer does not keep the refused value"
        );
        // A key the refused write introduced did not exist before, so the
        // rollback removes it (an old nil is no entry to restore) — config, running cfg, sources
        let refused: String = eval(&h, "local ok, e = pcall(mare.plugin, 'guard', { policy = 'never', note = 'x' }) return tostring(e)").await;
        assert!(refused.contains("policy could not be saved"), "{refused}");
        let (note_cfg, note_run, note_src): (String, String, bool) = eval(
            &h,
            "return tostring(mare.config.plugins.guard.note), G:note(), mare.config_sources()['plugins.guard.note'] ~= nil",
        )
        .await;
        assert_eq!((note_cfg.as_str(), note_run.as_str(), note_src), ("nil", "nil", false), "the new key is gone everywhere");
        assert_eq!(write("'ask'").await, "ok");
        assert_eq!(eval::<String>(&h, "return G:read()").await, r#""ask""#);
        assert_eq!(eval::<i64>(&h, "return mare.g.guard_torn or 0").await, 0, "an observer read a config the event did not describe");

        // Write A waits in its owner's apply; B (the same option) and C (another
        // option) are issued meanwhile; then A is refused. Writes are one at a time, so B and C
        // land after A, and A's rollback never takes them back (config, kernel copy, running
        // cfg, sources). Reference: reference implementation (two writes on one revision:
        // one wins, the stale one is refused, never overwriting).
        exec(
            &h,
            "mare.g.release = false; RESULTS = {}
             local function go(tag, t) mare.task.spawn(function()
               local ok, e = pcall(mare.plugin, 'guard', t) RESULTS[tag] = ok and 'ok' or tostring(e) end) end
             go('a', { note = 'slow' }); go('b', { note = 'b' })
             go('c', { policy = { mode = 'granular', categories = { network = true } } })",
        )
        .await;
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert_eq!(eval::<Option<String>>(&h, "return RESULTS.b").await, None, "B waits for A");
        exec(&h, "mare.g.release = true").await;
        let mut done = (String::new(), String::new(), String::new());
        for _ in 0..2000 {
            done = eval(&h, "return tostring(RESULTS.a), tostring(RESULTS.b), tostring(RESULTS.c)").await;
            if done.0 != "nil" && done.1 != "nil" && done.2 != "nil" {
                break;
            }
            // sleep-ok: polls Lua state for the queued writes to finish, bounded at 2000 tries
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert!(done.0.contains("slow note refused"), "A is refused: {done:?}");
        assert_eq!((done.1.as_str(), done.2.as_str()), ("ok", "ok"), "{done:?}");
        let b = r#"{"categories":{"network":true},"mode":"granular"}"#;
        assert_eq!(eval::<String>(&h, "return mare.json.encode(mare.config.plugins.guard.policy)").await, b);
        assert_eq!(eval::<String>(&h, "return G:read()").await, b, "running cfg keeps C");
        assert_eq!(
            h.kernel.config()["plugins"]["guard"]["policy"],
            json!({ "mode": "granular", "categories": { "network": true } }),
            "the kernel copy keeps C"
        );
        assert_eq!(eval::<String>(&h, "return mare.config_sources()['plugins.guard.policy'].layer").await, "runtime");
        assert_eq!(eval::<String>(&h, "return G:note()").await, "b", "B on the same option survives A's rollback");
        assert_eq!(h.kernel.config()["plugins"]["guard"]["note"], "b");
        h.shutdown().await;
    })
    .await
}

/// terra (CFG acceptance): core options and plugin options go through one validator, so an
/// invalid core value is reported and the default stays — it is not applied anyway (a string
/// `reduce_motion` was truthy, so motion was reduced although the value was refused in words).
/// Same entry points: init.lua, the command line layer.
#[tokio::test]
async fn an_invalid_core_value_is_reported_and_keeps_the_default() {
    local(async {
        let (_env, h) = probe_host("mare.setup { reduce_motion = 'oops', border = 42, editor = 'vi' }").await;
        let (motion, border, editor): (String, String, String) =
            eval(&h, "return tostring(mare.o.reduce_motion), mare.o.border, mare.o.editor").await;
        assert_eq!(motion, "false", "the default stays");
        assert_eq!(border, "rounded");
        assert_eq!(editor, "vi", "a valid sibling is taken");
        let w: String = eval(&h, "return table.concat(mare.config_warnings(), '\\n')").await;
        assert!(w.contains("option `reduce_motion` expects a boolean, got string"), "{w}");
        assert!(w.contains("option `border` expects a string, got number"), "{w}");
        let check = mare_shared::lua::core::config::check_config_fn(h.lua()).unwrap();
        let _: mlua::Value = check
            .call((mare_shared::lua::json_to_lua(h.lua(), &json!({ "reduce_motion": "again" })).unwrap(), "cli"))
            .unwrap();
        let checked_val: mlua::Value = check
            .call((
                mare_shared::lua::json_to_lua(h.lua(), &json!({ "reduce_motion": "again", "editor": "ed" })).unwrap(),
                "cli",
            ))
            .unwrap();
        let checked = mare_shared::lua::lua_to_json(h.lua(), checked_val).unwrap().to_string();
        assert_eq!(checked, r#"{"editor":"ed"}"#, "the command line layer drops it the same way");
        h.shutdown().await;
    })
    .await
}

/// 条: `mare.config_clear(path)` clears one layer's override (`nil` cannot say
/// "clear"). In a config file it removes what earlier files set, so the declared default applies;
/// at runtime it removes the runtime write, so the startup configuration shows through again (not
/// the default), with its source, the running value and an `option/changed`. Clearing a path
/// nothing set is a no-op.
///
/// Reference tests: Codex `codex-rs/app-server/tests/suite/v2/config_rpc.rs:2033` (removing a field
/// lets the lower layer's definition show through) and
/// `codex-rs/app-server/src/config_manager_service_tests.rs:756` (clearing a missing key is a
/// no-op); reference implementation (`unset` brings the
/// default back); Kimi `packages/agent-core-v2/test/app/config/config.test.ts:631` (`set(k,
/// undefined)` does not delete, an explicit replace does — the reason for a clear call).
#[tokio::test]
async fn config_clear_removes_one_layer_and_lets_the_one_below_show() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(env.config_dir().join("plugins/live.lua"), LIVE).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', reduce_motion = true, border = 'single' }\n\
             mare.plugin('live', { rate = 3, boot = 'z' })\n\
             mare.config_clear('reduce_motion')\n\
             mare.config_clear('plugins.live.boot')\n\
             mare.config_clear('never_set')\n",
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        expose(&h);
        let (motion, src, boot): (String, bool, String) = eval(
            &h,
            "return tostring(mare.o.reduce_motion), mare.config_sources()['reduce_motion'] ~= nil, C.cordis:get('live'):read('boot')",
        )
        .await;
        assert_eq!((motion.as_str(), src, boot.as_str()), ("false", false, "a"), "cleared in the file: defaults");

        exec(&h, "CHANGED = {} C.cordis:on('option/changed', function(e) CHANGED[#CHANGED + 1] = e.name .. '=' .. tostring(e.value) end)").await;
        exec(&h, "mare.plugin('live', { rate = 5 }); mare.o.border = 'double'").await;
        exec(&h, "mare.config_clear('plugins.live.rate'); mare.config_clear('border')").await;
        let (rate, rate_layer, border): (i64, String, String) = eval(
            &h,
            "return C.cordis:get('live'):read('rate'), mare.config_sources()['plugins.live.rate'].layer, mare.o.border",
        )
        .await;
        assert_eq!(rate, 3, "the startup value shows through, not the default 1");
        assert_ne!(rate_layer, "runtime", "its source is the startup layer again");
        assert_eq!(border, "single");
        assert_eq!(h.kernel.config()["plugins"]["live"]["rate"], 3);
        let changed: Vec<String> = eval(&h, "return CHANGED").await;
        assert!(changed.contains(&"plugins.live.rate=3".to_string()), "{changed:?}");
        exec(&h, "mare.config_clear('plugins.live.rate')").await;
        assert_eq!(eval::<i64>(&h, "return C.cordis:get('live'):read('rate')").await, 3, "a second clear is a no-op");
        h.shutdown().await;
    })
    .await
}

/// 条: `/config` and `config.describe` enumerate every loaded declaration, leaf by
/// leaf — a plugin option nobody set is listed with its declared default, type and description
/// (nested object fields too), and a `false` default is reported as false, not missing.
///
/// Reference tests: reference implementation (the schema dump
/// keeps descriptions and defaults) and `:131` (disabled and conditional rows are included);
/// Codex `codex-rs/app-server/tests/suite/v2/config_rpc.rs:885` (`config/read` returns effective
/// values with their layers). Kimi has no runtime listing (only a static manifest test).
#[tokio::test]
async fn config_lists_every_declared_option_with_its_default() {
    local(async {
        let (_env, h) = probe_host("").await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let (described, view): (String, String) = eval(
            &h,
            r#"
            local d = mare.config_describe()
            local pick = {}
            for _, k in ipairs({ 'plugins.probe.count', 'plugins.probe.mode', 'plugins.probe.nested.flag', 'reduce_motion' }) do
              local e = d[k]
              pick[k] = e and { default = e.default, description = e.description, type = e.type } or 'missing'
            end
            local overlay, connected = ui.overlay, ui.connected
            ui.connected = function() return false end
            ui.overlay = function(_, node) mare._captured_overlay = mare.json.encode(node) end
            CM:run('/config probe')
            ui.overlay, ui.connected = overlay, connected
            return mare.json.encode(pick), mare._captured_overlay
            "#,
        )
        .await;
        let d: Value = serde_json::from_str(&described).unwrap();
        assert_eq!(d["plugins.probe.count"], json!({ "default": 1, "description": "how many", "type": "integer" }), "{d}");
        assert_eq!(d["plugins.probe.mode"]["default"], "fast", "{d}");
        assert_eq!(d["plugins.probe.nested.flag"], json!({ "default": false, "description": "a flag", "type": "boolean" }), "{d}");
        assert_eq!(d["reduce_motion"]["default"], false, "{d}");
        for leaf in ["plugins.probe.count", "plugins.probe.nested.flag", "plugins.probe.nested.label"] {
            assert!(view.contains(leaf), "/config lists the unset {leaf}: {view}");
        }
        h.shutdown().await;
    })
    .await
}

/// 条 (no aliases): the deprecated `mare.o.notify` and `mare.o.scroll_lines` are
/// deleted — setting one warns as an unknown option (with the close-name hint), not as "unused and
/// will be removed". Reference: Codex `codex-rs/config/src/strict_config_tests.rs:296` (removed
/// settings are reported, with a migration hint for the replaced ones).
#[tokio::test]
async fn the_deprecated_core_options_are_gone() {
    local(async {
        let (_env, h) = probe_host("mare.setup { notify = 'always', scroll_lines = 5, ambiwidth = 'double' }").await;
        let w: String = eval(&h, "return table.concat(mare.config_warnings(), '\\n')").await;
        assert!(w.contains("unknown option `notify`"), "{w}");
        assert!(w.contains("unknown option `scroll_lines`"), "{w}");
        assert!(w.contains("unknown option `ambiwidth`"), "{w}");
        assert!(!w.contains("will be removed"), "{w}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn non_contiguous_or_keyed_list_is_rejected() {
    local(async {
        let (_env, h) = probe_host("mare.plugin('probe', { names = { 'a', extra = 'b' } })").await;
        let (cfg, w) = cfg_and_warnings(&h).await;
        assert!(w.contains("names"), "non-list key in list must warn: {w}");
        assert_eq!(cfg["names"], json!([]), "invalid list must fall back to default");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mistyped_permission_rules_are_rejected() {
    local(async {
        let (_env, h) = probe_host("").await;
        let validate = mare_shared::lua::core::config::validate_options_fn(h.lua()).unwrap();
        let candidate = mare_shared::lua::json_to_lua(
            h.lua(),
            &json!({ "plugins": { "permissions": { "rules": [{ "argv": 123, "decision": "bogus", "extra": true }] } } }),
        )
        .unwrap();
        let problems = validate.call::<Vec<String>>(candidate).unwrap().join("\n");
        assert!(problems.contains("rules[1].argv"), "mistyped rule argv must be reported: {problems}");
        assert!(problems.contains("rules[1].decision"), "mistyped rule decision must be reported: {problems}");
        assert!(problems.contains("rules[1].extra"), "unknown rule field must be reported: {problems}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn numeric_enum_choices_are_accepted_and_invalid_values_rejected() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/numenum.lua"),
            r#"return { name = "numenum",
                config_schema = {
                    scale = { type = "enum", choices = { 1, 2 }, default = 1, desc = "scale" },
                },
                apply = function(_, cfg) mare._numenum_scale = cfg.scale end }"#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo' }\nmare.plugin('numenum', { scale = 2 })",
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        let w: String = eval(&h, "return table.concat(mare.config_warnings(), '\\n')").await;
        assert!(!w.contains("scale"), "valid numeric enum value 2 must not warn: {w}");
        assert_eq!(eval::<i64>(&h, "return mare._numenum_scale").await, 2);
        let err: String =
            eval(&h, "local ok, e = pcall(mare.plugin, 'numenum', { scale = 3 }); return ok and 'ok' or tostring(e)")
                .await;
        assert!(err.contains("scale"), "invalid numeric enum value 3 must be rejected: {err}");
        assert_eq!(eval::<i64>(&h, "return mare._numenum_scale").await, 2);
        h.shutdown().await;
    })
    .await
}
