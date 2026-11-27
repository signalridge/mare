//! Rust accessors for the core Lua config engine (`core.config` / `core.config.state`).
//!
//! All config-engine state and per-evaluation closures live on the VM's registry-held
//! `core.config` handle (`core.config.state`), never as underscore fields on `mare` or plants in
//! `package.{preload,loaded}`. Rollback of `/reload config` swaps `core.config.state` as a single
//! table plus the public `mare` config API entries.

use mlua::{Function, Lua, Table, Value};

/// Public `mare` fields replaced during `/reload config` evaluation and restored on rollback.
const REEVAL_PUBLIC_FIELDS: &[&str] = &[
    "config",
    "setup",
    "plugin",
    "packages",
    "o",
    "on",
    "on_local",
    "policy",
    "config_clear",
    "config_sensitive",
    "config_sensitive_paths",
];

/// Return the `core.config` namespace table on the VM's core handle.
pub fn handle(lua: &Lua) -> mlua::Result<Table> {
    super::namespace(lua, "config")
}

/// Return (creating if absent) the active `core.config.state` table.
pub fn state(lua: &Lua) -> mlua::Result<Table> {
    let cfg = handle(lua)?;
    match cfg.raw_get::<Value>("state")? {
        Value::Table(t) => Ok(t),
        _ => {
            let t = lua.create_table()?;
            cfg.raw_set("state", t.clone())?;
            Ok(t)
        }
    }
}

/// Replace `core.config.state` atomically (used for `/reload config` setup and rollback).
pub fn set_state(lua: &Lua, new_state: Table) -> mlua::Result<()> {
    handle(lua)?.raw_set("state", new_state)
}

fn ensure_state_table(lua: &Lua, key: &str) -> mlua::Result<Table> {
    let st = state(lua)?;
    match st.raw_get::<Value>(key)? {
        Value::Table(t) => Ok(t),
        _ => {
            let t = lua.create_table()?;
            st.raw_set(key, t.clone())?;
            Ok(t)
        }
    }
}

fn shallow_clone_table(lua: &Lua, src: &Table) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    for kv in src.pairs::<Value, Value>() {
        let (k, v) = kv?;
        out.raw_set(k, v)?;
    }
    Ok(out)
}

/// Create a fresh candidate `core.config.state` for a staged `/reload config` evaluation,
/// preserving cross-reload metadata (`sensitive_config` cloned, `policy_points`, live `on_now` /
/// `on_unsub` for concurrent runtime `mare.on`, `old_sources`, `previous_keymaps`,
/// `previous_contexts`, `stage`, and `api_undo`).
///
/// The previous `state` table is returned untouched so rollback is a single [`set_state`] swap.
pub fn begin_reeval_state(lua: &Lua, undo: Table) -> mlua::Result<Table> {
    let prev = state(lua)?;
    let next = lua.create_table()?;
    let sensitive = match prev.raw_get::<Value>("sensitive_config")? {
        Value::Table(t) => shallow_clone_table(lua, &t)?,
        _ => lua.create_table()?,
    };
    next.raw_set("sensitive_config", sensitive)?;
    next.raw_set("policy_points", prev.raw_get::<Value>("policy_points")?)?;
    next.raw_set("on_now", prev.raw_get::<Value>("on_now")?)?;
    next.raw_set("on_unsub", prev.raw_get::<Value>("on_unsub")?)?;
    next.raw_set("layer_runtime", Value::Nil)?;
    next.raw_set("option_changed", Value::Nil)?;
    next.raw_set("warnings", lua.create_table()?)?;
    next.raw_set("plugin_failed", lua.create_table()?)?;
    next.raw_set("plugin_error", lua.create_table()?)?;
    next.raw_set("pending_on", lua.create_table()?)?;
    next.raw_set("plugin_defaults", lua.create_table()?)?;
    next.raw_set("plugin_schemas", lua.create_table()?)?;
    next.raw_set("plugin_slots", lua.create_table()?)?;
    next.raw_set("preset_cleared", false)?;
    next.raw_set("api_undo", undo)?;
    next.raw_set("old_sources", prev.raw_get::<Value>("sources")?)?;
    next.raw_set("old_cli_values", prev.raw_get::<Value>("cli_values")?)?;
    next.raw_set("stage", lua.create_table()?)?;
    next.raw_set("config_previous_keymaps", prev.raw_get::<Value>("config_keymaps")?)?;
    next.raw_set("config_previous_contexts", prev.raw_get::<Value>("config_contexts")?)?;
    set_state(lua, next)?;
    Ok(prev)
}

// ---------------------------------------------------------------------------
// Sources, warnings, layer, and config evaluation provenance
// ---------------------------------------------------------------------------

pub fn sources(lua: &Lua) -> mlua::Result<Table> {
    state(lua)?.raw_get("sources")
}

pub fn opt_sources(lua: &Lua) -> Option<Table> {
    state(lua).ok().and_then(|st| st.raw_get::<Table>("sources").ok())
}

pub fn set_sources(lua: &Lua, sources: Table) -> mlua::Result<()> {
    state(lua)?.raw_set("sources", sources)
}

pub fn set_cli_values(lua: &Lua, cli_values: Table) -> mlua::Result<()> {
    state(lua)?.raw_set("cli_values", cli_values)
}

pub fn set_old_cli_values(lua: &Lua, old_cli_values: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("old_cli_values", old_cli_values)
}

pub fn ensure_sources(lua: &Lua) -> mlua::Result<Table> {
    ensure_state_table(lua, "sources")
}

pub fn warnings(lua: &Lua) -> mlua::Result<Vec<String>> {
    let raw = state(lua)?.raw_get::<Option<Vec<String>>>("warnings")?.unwrap_or_default();
    Ok(raw.into_iter().map(|w| crate::redact::redact_str(&w)).collect())
}

pub fn warnings_from_lua(lua: &Lua) -> Vec<String> {
    warnings(lua).unwrap_or_default()
}

pub fn ensure_warnings_table(lua: &Lua) -> mlua::Result<Table> {
    ensure_state_table(lua, "warnings")
}

pub fn set_layer(lua: &Lua, layer: &str) -> mlua::Result<()> {
    state(lua)?.raw_set("layer", layer)
}

pub fn set_layer_runtime(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("layer_runtime", value)
}

pub fn set_in_config(lua: &Lua, in_config: bool) -> mlua::Result<()> {
    state(lua)?.raw_set("in_config", in_config)
}

/// Record the `mare.config` table created by `engines::config::install` so the initial host
/// `evaluate_config` can verify table identity and reuse it once without re-running the chunks.
pub fn set_fresh_install_config(lua: &Lua, config: Option<&Table>) -> mlua::Result<()> {
    state(lua)?.raw_set("fresh_install_config", config.cloned())
}

/// Consume the recorded fresh-install `mare.config` table, returning it at most once.
pub fn take_fresh_install_config(lua: &Lua) -> mlua::Result<Option<Table>> {
    let st = state(lua)?;
    let tbl = match st.raw_get::<Value>("fresh_install_config")? {
        Value::Table(t) => Some(t),
        _ => None,
    };
    if tbl.is_some() {
        st.raw_set("fresh_install_config", Value::Nil)?;
    }
    Ok(tbl)
}

pub fn config_coroutine(lua: &Lua) -> mlua::Result<Value> {
    state(lua)?.raw_get("config_coroutine")
}

pub fn set_config_coroutine(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("config_coroutine", value)
}

// ---------------------------------------------------------------------------
// Plugin defaults, schemas, failure maps, slots, and preset flags
// ---------------------------------------------------------------------------

pub fn ensure_plugin_defaults(lua: &Lua) -> mlua::Result<Table> {
    ensure_state_table(lua, "plugin_defaults")
}

pub fn ensure_plugin_schemas(lua: &Lua) -> mlua::Result<Table> {
    ensure_state_table(lua, "plugin_schemas")
}

pub fn plugin_schemas(lua: &Lua) -> mlua::Result<Option<Table>> {
    state(lua)?.raw_get("plugin_schemas")
}

pub use crate::config_env::PluginEnvBinding;

/// Collect plugin option env bindings declared via `env = "MARE_..."` in plugin `config_schema`
/// tables (including nested `object.fields`). Returned sorted by dotted path for deterministic
/// evaluation order.
pub fn plugin_env_bindings(lua: &Lua) -> mlua::Result<Vec<PluginEnvBinding>> {
    let mut out = Vec::new();
    if let Some(schemas) = plugin_schemas(lua)? {
        for kv in schemas.pairs::<Value, Value>() {
            let (Value::String(plugin), Value::Table(schema)) = kv? else {
                continue;
            };
            let Ok(plugin_name) = plugin.to_str() else {
                continue;
            };
            crate::config_env::collect_schema_env_bindings(
                &schema,
                &plugin_name,
                &format!("plugins.{plugin_name}"),
                None,
                &mut out,
            )?;
        }
    }
    out.sort();
    Ok(out)
}

/// Record a malformed JSON parse error for a plugin env binding, honouring `on_invalid = "fail"`
/// and `on_invalid = "error"` identically to `config_api_validate.lua`.
pub fn record_plugin_env_parse_error(lua: &Lua, binding: &PluginEnvBinding, parse_err: &str) -> mlua::Result<()> {
    let base = crate::redact::redact_str(&format!("{}: {parse_err}", binding.env_var));
    let warnings = ensure_warnings_table(lua)?;
    match binding.on_invalid.as_deref() {
        Some("fail") => {
            warnings.push(format!("error: {base}"))?;
            let target = if is_staging(lua)? { "plugin_error" } else { "plugin_failed" };
            let map = ensure_state_table(lua, target)?;
            if map.raw_get::<Value>(binding.plugin.as_str())?.is_nil() {
                map.raw_set(binding.plugin.as_str(), base)?;
            }
        }
        Some("error") => {
            warnings.push(format!("error: {base}"))?;
            let map = ensure_state_table(lua, "plugin_error")?;
            if map.raw_get::<Value>(binding.plugin.as_str())?.is_nil() {
                map.raw_set(binding.plugin.as_str(), base)?;
            }
        }
        _ => {
            warnings.push(format!("{base} (ignored)"))?;
        }
    }
    Ok(())
}

/// Return whether `plugins.<plugin>.<option>` is declared as a whole-replaced type (`union` or
/// `list`) in `core.config.state.plugin_schemas`.
pub fn is_plugin_option_whole(lua: &Lua, plugin: &str, option: &str) -> bool {
    let Ok(Some(schemas)) = plugin_schemas(lua) else {
        return false;
    };
    let Ok(Some(schema)) = schemas.get::<Option<Table>>(plugin) else {
        return false;
    };
    let Ok(Some(entry)) = schema.get::<Option<Table>>(option) else {
        return false;
    };
    let Ok(Some(ty)) = entry.get::<Option<String>>("type") else {
        return false;
    };
    matches!(ty.as_str(), "union" | "list")
}

fn merge_json_plain(base: &mut serde_json::Value, other: serde_json::Value) {
    match (base, other) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                merge_json_plain(a.entry(k).or_insert(serde_json::Value::Null), v);
            }
        }
        (a, b) => *a = b,
    }
}

/// Merge a single plugin's option table `src` into `dst`, replacing `union`/`list` options whole
/// instead of recursively merging keys from lower layers.
pub fn merge_plugin_opts_respecting_schema(
    lua: &Lua,
    plugin: &str,
    dst: &mut serde_json::Value,
    src: serde_json::Value,
) {
    match (dst, src) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (opt, val) in b {
                if is_plugin_option_whole(lua, plugin, &opt) {
                    a.insert(opt, val);
                } else {
                    merge_json_plain(a.entry(opt).or_insert(serde_json::Value::Null), val);
                }
            }
        }
        (a, b) => *a = b,
    }
}

/// Merge a higher-priority config layer (`src`) into `dst`, replacing any `plugins.<p>.<opt>`
/// whose schema declares `union` or `list` as one whole value rather than recursively merging.
pub fn merge_json_respecting_plugin_schema(lua: &Lua, dst: &mut serde_json::Value, src: serde_json::Value) {
    match (dst, src) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                if k == crate::PLUGINS_KEY {
                    let dst_plugins = a.entry(k).or_insert_with(|| serde_json::json!({}));
                    match (dst_plugins, v) {
                        (serde_json::Value::Object(dp), serde_json::Value::Object(sp)) => {
                            for (plugin, pval) in sp {
                                merge_plugin_opts_respecting_schema(
                                    lua,
                                    &plugin,
                                    dp.entry(plugin.clone()).or_insert(serde_json::Value::Null),
                                    pval,
                                );
                            }
                        }
                        (dp, sp) => *dp = sp,
                    }
                } else {
                    merge_json_plain(a.entry(k).or_insert(serde_json::Value::Null), v);
                }
            }
        }
        (a, b) => *a = b,
    }
}

pub fn plugin_failed(lua: &Lua) -> mlua::Result<Option<Table>> {
    state(lua)?.raw_get("plugin_failed")
}

pub fn plugin_error(lua: &Lua) -> mlua::Result<Option<Table>> {
    state(lua)?.raw_get("plugin_error")
}

pub fn ensure_plugin_slots(lua: &Lua) -> mlua::Result<Table> {
    ensure_state_table(lua, "plugin_slots")
}

pub fn set_plugin_slots(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("plugin_slots", value)
}

pub fn set_preset_cleared(lua: &Lua, val: bool) {
    if let Ok(st) = state(lua) {
        let _ = st.raw_set("preset_cleared", val);
    }
}

pub fn preset_cleared(lua: &Lua) -> bool {
    state(lua).ok().and_then(|st| st.raw_get::<bool>("preset_cleared").ok()).unwrap_or(false)
}

pub fn plugin_preset_entry(lua: &Lua) -> mlua::Result<Option<Table>> {
    handle(lua)?.raw_get("plugin_preset_entry")
}

// ---------------------------------------------------------------------------
// Config evaluation staging, rollback, views, and policy points
// ---------------------------------------------------------------------------

pub fn is_staging(lua: &Lua) -> mlua::Result<bool> {
    Ok(!state(lua)?.raw_get::<Value>("stage")?.is_nil())
}

pub fn stage(lua: &Lua) -> mlua::Result<Table> {
    state(lua)?.raw_get("stage")
}

pub fn set_stage(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("stage", value)
}

pub fn config_keymaps(lua: &Lua) -> mlua::Result<Value> {
    state(lua)?.raw_get("config_keymaps")
}

pub fn set_config_keymaps(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("config_keymaps", value)
}

pub fn config_contexts(lua: &Lua) -> mlua::Result<Value> {
    state(lua)?.raw_get("config_contexts")
}

pub fn set_config_contexts(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("config_contexts", value)
}

pub fn config_components(lua: &Lua) -> mlua::Result<Value> {
    state(lua)?.raw_get("config_components")
}

pub fn set_config_components(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("config_components", value)
}

pub fn set_previous_keymaps(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("config_previous_keymaps", value)
}

pub fn set_previous_contexts(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("config_previous_contexts", value)
}

pub fn set_api_undo(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("api_undo", value)
}

pub fn set_old_sources(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("old_sources", value)
}

pub fn current_view(lua: &Lua) -> mlua::Result<Option<Table>> {
    state(lua)?.raw_get("current_view")
}

pub fn set_current_view(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("current_view", value)
}

pub fn set_policy_points(lua: &Lua, points: Table) -> mlua::Result<()> {
    state(lua)?.raw_set("policy_points", points)
}

/// Snapshot only the public `mare` config API fields before re-evaluation.
pub fn snapshot_public_fields(mare: &Table) -> mlua::Result<Vec<(&'static str, Value)>> {
    REEVAL_PUBLIC_FIELDS.iter().map(|key| Ok((*key, mare.raw_get::<Value>(*key)?))).collect()
}

/// Restore the public `mare` fields captured by [`snapshot_public_fields`].
pub fn restore_public_fields(mare: &Table, saved: &[(&'static str, Value)]) -> Vec<(&'static str, mlua::Error)> {
    let mut errs = Vec::new();
    for (key, value) in saved {
        if let Err(err) = mare.raw_set(*key, value.clone()) {
            errs.push((*key, err));
        }
    }
    errs
}

/// Record a committed config reload view and sync `view.config = mare.config` across tracked views.
pub fn sync_config_views(lua: &Lua, mare: &Table, view: Table) -> mlua::Result<()> {
    let cfg = handle(lua)?;
    let update: Function = lua
        .load(
            r#"return function(mare, cfg, view)
              local views = rawget(cfg, 'views')
              if not views then views = setmetatable({}, { __mode = 'k' }); rawset(cfg, 'views', views) end
              views[view] = true
              for v in pairs(views) do rawset(v, 'config', mare.config) end
            end"#,
        )
        .eval()?;
    update.call::<()>((mare.clone(), cfg, view))
}

/// Build the staging `mare.o` proxy used while config reload runs in a coroutine: runtime reads
/// see a read-only view of `old_options` while the config coroutine sees `new_options`.
pub fn create_staged_options_proxy(lua: &Lua, old_options: Value, new_options: Value) -> mlua::Result<Table> {
    let cfg = handle(lua)?;
    lua.load(
        r#"local cfg, old, new = ...
        local function current()
          if cfg.in_config() then return new end
          return old
        end
        local wrapped = setmetatable({}, {__mode = 'k'})
        local function readonly(value)
          if type(value) ~= 'table' then return value end
          if wrapped[value] then return wrapped[value] end
          local proxy = {}
          wrapped[value] = proxy
          return setmetatable(proxy, {
            __index = function(_, k) return readonly(value[k]) end,
            __newindex = function() error('option update unavailable during config reload; retry afterwards', 2) end,
            __len = function() return #value end,
            __pairs = function()
              local iter, state, key = pairs(value)
              return function()
                local k, v = iter(state, key)
                key = k
                if k ~= nil then return k, readonly(v) end
              end
            end,
          })
        end
        return setmetatable({}, {
          __index = function(_, k)
            local options = current()
            local value = options[k]
            if options == old then return readonly(value) end
            return value
          end,
          __newindex = function(_, k, v)
            local options = current()
            if options == old then error('option update unavailable during config reload; retry afterwards', 2) end
            options[k] = v
          end,
          __pairs = function()
            local options = current()
            if options ~= old then return pairs(options) end
            return pairs(readonly(options))
          end,
        })"#,
    )
    .set_name("@core/config_options_stage")
    .call((cfg, old_options, new_options))
}

/// Install `mare.config_sources()`, `mare.config_cli()`, and `mare.config_warnings()` backed by `core.config.state`.
pub fn install_introspection_fns(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let cfg = handle(lua)?;
    lua.load(
        r#"local mare, cfg = ...
        local function copy_val(v)
          if type(v) ~= 'table' then return v end
          local out = {}
          for k, val in pairs(v) do out[k] = copy_val(val) end
          return out
        end
        function mare.config_sources()
          local st = cfg.state or {}
          if st.old_sources and not cfg.in_config() then
            return st.old_sources
          end
          return st.sources or {}
        end
        function mare.config_cli(path)
          if type(path) ~= 'string' or path == '' then return nil end
          local st = cfg.state or {}
          local vals = (st.old_cli_values and not cfg.in_config()) and st.old_cli_values or st.cli_values
          if type(vals) ~= 'table' then return nil end
          local cur = vals
          for seg in path:gmatch('[^.]+') do
            if type(cur) ~= 'table' then return nil end
            cur = cur[seg]
          end
          return copy_val(cur)
        end
        function mare.config_warnings()
          local st = cfg.state
          local raw = (st and st.warnings) or {}
          local redact = mare.text and mare.text.redact
          if not redact then return raw end
          local out = {}
          for i = 1, #raw do
            local w = raw[i]
            out[i] = type(w) == 'string' and redact(w) or w
          end
          return out
        end"#,
    )
    .set_name("=config_introspection")
    .call::<()>((mare.clone(), cfg))
}

// ---------------------------------------------------------------------------
// Subscriptions, runtime hooks, and config transaction primitives
// ---------------------------------------------------------------------------

pub fn set_on_now(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("on_now", f)
}

pub fn pending_on(lua: &Lua) -> mlua::Result<Option<Table>> {
    match state(lua)?.raw_get::<Value>("pending_on")? {
        Value::Table(t) => Ok(Some(t)),
        _ => Ok(None),
    }
}

pub fn set_pending_on(lua: &Lua, t: Table) -> mlua::Result<()> {
    state(lua)?.raw_set("pending_on", t)
}

pub fn on_unsub(lua: &Lua) -> mlua::Result<Value> {
    state(lua)?.raw_get("on_unsub")
}

pub fn set_on_unsub(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("on_unsub", value)
}

pub fn option_changed(lua: &Lua) -> mlua::Result<Option<Function>> {
    state(lua)?.raw_get::<Option<Function>>("option_changed")
}

pub fn set_option_changed(lua: &Lua, value: Value) -> mlua::Result<()> {
    state(lua)?.raw_set("option_changed", value)
}

pub fn set_config_submit(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("config_submit", f)
}

pub fn config_run(lua: &Lua) -> mlua::Result<Function> {
    state(lua)?.raw_get("config_run")
}

pub fn set_option_current(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("option_current", f)
}

pub fn set_option_apply(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("option_apply", f)
}

pub fn set_config_convertible(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("config_convertible", f)
}

pub fn set_plugin_running(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("plugin_running", f)
}

pub fn set_plugin_effective(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("plugin_effective", f)
}

pub fn plugin_effective_fn(lua: &Lua) -> mlua::Result<Function> {
    state(lua)?.raw_get("plugin_effective")
}

pub fn set_plugin_kernel_sync(lua: &Lua, f: Function) -> mlua::Result<()> {
    state(lua)?.raw_set("plugin_kernel_sync", f)
}

pub fn plugin_kernel_sync_fn(lua: &Lua) -> mlua::Result<Function> {
    state(lua)?.raw_get("plugin_kernel_sync")
}

// ---------------------------------------------------------------------------
// Config engine helper functions (eval, config_load, host options)
// ---------------------------------------------------------------------------

pub fn check_config_fn(lua: &Lua) -> mlua::Result<Function> {
    state(lua)?.raw_get("check_config")
}

pub fn opt_check_config_fn(lua: &Lua) -> Option<Function> {
    state(lua).ok().and_then(|st| st.raw_get::<Function>("check_config").ok())
}

pub fn opt_finalize_packages_fn(lua: &Lua) -> Option<Function> {
    state(lua).ok().and_then(|st| st.raw_get::<Function>("finalize_packages").ok())
}

pub fn opt_merge_package_opts_fn(lua: &Lua) -> Option<Function> {
    state(lua).ok().and_then(|st| st.raw_get::<Function>("merge_package_opts").ok())
}

/// Resolve a dotted config key against `mare.config` + `overrides` (used by `options.get`).
pub fn resolve_config_key_fn(lua: &Lua) -> mlua::Result<Function> {
    handle(lua)?.raw_get("resolve_config_key")
}

/// Validate a candidate options table and return diagnostic strings (used by `options.set`).
pub fn validate_options_fn(lua: &Lua) -> mlua::Result<Function> {
    state(lua)?.raw_get("validate_options")
}

/// Deep-copy a Lua config value (used by `options.set` rollback).
pub fn deepcopy_fn(lua: &Lua) -> mlua::Result<Function> {
    state(lua)?.raw_get("deepcopy")
}

/// Run the transactional `plugins.*` config write helper (used by `options.set`), if installed.
pub fn opt_config_write_fn(lua: &Lua) -> mlua::Result<Option<Function>> {
    state(lua)?.raw_get::<Option<Function>>("config_write")
}
