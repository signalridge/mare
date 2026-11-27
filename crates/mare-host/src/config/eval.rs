//! Evaluating the configuration from its six layers.

use std::path::PathBuf;

use anyhow::Result;
use mlua::{Function, Table, Value};

/// Evaluate the configuration in the host VM with the six layers of, low → high:
/// option defaults < state.json < user init.lua < project .mare/init.lua < env `MARE_<OPTION>`
/// < CLI (`--model`, `--set k=v`, …). Records each key's source in `core.config.state.sources`
/// for `/config`.
pub(crate) async fn evaluate_config(
    lua: &mlua::Lua,
    mare: &Table,
    cwd: &std::path::Path,
    project_trusted: bool,
    cli: &serde_json::Value,
    cli_flag_paths: &[String],
    errors: Option<&mut Vec<(PathBuf, String)>>,
) -> Result<serde_json::Value> {
    mare_shared::lua::begin_config_eval(lua)?;
    let old_options: Value = mare.get("o")?;
    let old_config: Value = mare.get("config")?;
    let staging = mare_shared::lua::core::config::is_staging(lua)?;
    let fresh_install_cfg =
        if !staging { mare_shared::lua::core::config::take_fresh_install_config(lua)? } else { None };
    let reuse_fresh = fresh_install_cfg.as_ref().is_some_and(|installed| match &old_config {
        Value::Table(oc) => oc.to_pointer() == installed.to_pointer(),
        _ => false,
    }) && mare_shared::lua::core::config::opt_sources(lua)
        .is_none_or(|s| s.pairs::<Value, Value>().next().is_none())
        && mare_shared::lua::core::config::warnings(lua).is_ok_and(|w| w.is_empty())
        && mare_shared::lua::core::config::pending_on(lua).ok().flatten().is_none_or(|p| p.raw_len() == 0);
    let c = if reuse_fresh {
        let Value::Table(c) = old_config.clone() else { unreachable!() };
        let keys: Vec<Value> = c.pairs::<Value, Value>().filter_map(|kv| kv.ok().map(|(k, _)| k)).collect();
        for k in keys {
            c.raw_set(k, Value::Nil)?;
        }
        mare_shared::lua::core::config::set_preset_cleared(lua, false);
        c
    } else {
        let c = lua.create_table()?;
        mare.set("config", c.clone())?;
        mare_shared::lua::core::config::set_sources(lua, lua.create_table()?)?;
        mare_shared::lua::core::config::set_preset_cleared(lua, false);
        mare_plugin::engines::config::load_config_api(lua, mare, &c)?;
        mare_plugin::engines::config::load_config_packages(lua, mare, &c)?;
        c
    };
    let new_options: Value = mare.get("o")?;
    let mut options_proxy = None;
    if staging && !old_options.is_nil() {
        // Config executes in a Lua coroutine and may yield. Until commit, runtime reads must
        // continue through the old option proxy while the config coroutine sees its own values.
        let proxy = mare_shared::lua::core::config::create_staged_options_proxy(lua, old_options, new_options.clone())?;
        mare.set("o", proxy.clone())?;
        options_proxy = Some(proxy);
    }
    let config_view: Option<Table> = if staging {
        Some(
            lua.load(
                r#"local mare, config = ...
          return setmetatable({ config = config, __mare_config_view = true }, {
            __index = mare,
            __newindex = function(_, k, v) mare[k] = v end,
          })"#,
            )
            .call((mare.clone(), c.clone()))?,
        )
    } else {
        None
    };
    let t_plugin_config = std::time::Instant::now();
    mare_plugin::config_load::install_plugin_config_apis(lua, mare, &c, cwd, project_trusted, config_view.as_ref())?;
    mare_shared::profile::record("config: plugin APIs", t_plugin_config.elapsed());
    if config_view.is_some() {
        // Leave the real table in place for concurrently running plugins, including rawget.
        mare.set("config", old_config)?;
    }
    let mut files = vec![(mare_config::user_init(), "user")];
    if project_trusted && let Some(mare_dir) = mare_shared::project::resource_root(cwd).mare_dir() {
        files.push((mare_dir.join("init.lua"), "project"));
    }
    // keymaps and subcontexts bound by the previous evaluation are dropped so /reload config doesn't stack them
    if !staging {
        if let Ok(Value::Table(prev)) = mare_shared::lua::core::config::config_keymaps(lua)
            && let Ok(Value::Table(km)) = mare.get::<Value>("keymap")
            && let Ok(remove) = km.get::<Function>("remove")
        {
            for h in prev.sequence_values::<Value>().flatten() {
                let _ = remove.call::<()>(h);
            }
        }
        if let Ok(Value::Table(prev_ctx)) = mare_shared::lua::core::config::config_contexts(lua) {
            for entry in prev_ctx.sequence_values::<Table>().flatten() {
                if let Ok(h) = entry.get::<Table>("handle")
                    && let Ok(remove) = h.get::<Function>("remove")
                {
                    let _ = remove.call::<()>(());
                }
            }
        }
    }
    mare_shared::lua::core::config::set_config_keymaps(lua, Value::Table(lua.create_table()?))?;
    mare_shared::lua::core::config::set_config_contexts(lua, Value::Table(lua.create_table()?))?;
    mare_shared::lua::core::config::set_config_components(lua, Value::Table(lua.create_table()?))?;
    mare_shared::lua::core::config::set_in_config(lua, true)?;
    let t_init_files = std::time::Instant::now();
    let r = mare_plugin::config_load::run_init_files(lua, cwd, &files, config_view.as_ref(), errors).await;
    mare_shared::profile::record("config: init files", t_init_files.elapsed());
    mare_shared::lua::core::config::set_in_config(lua, false)?;
    r?;
    if let Some(view) = &config_view {
        mare.set("config", view.get::<Value>("config")?)?;
        mare_shared::lua::core::config::set_current_view(lua, Value::Table(view.clone()))?;
    }
    if options_proxy.is_some_and(|proxy| mare.get::<Value>("o").ok() == Some(Value::Table(proxy))) {
        mare.set("o", new_options)?;
    }
    // the package declarations of every config file, merged
    mare_plugin::config_load::finalize_packages(lua)?;
    let mut cfg = mare_config::Config {
        value: mare_shared::lua::config_to_json(lua, mare.get::<Value>("config")?)?,
        warnings: vec![],
    };
    if !cfg.value.is_object() {
        cfg.value = serde_json::json!({});
    }
    let sources = mare_shared::lua::core::config::sources(lua)?;
    let check = mare_shared::lua::core::config::check_config_fn(lua)?;
    if let Some(state) = mare_config::read_state().map_err(anyhow::Error::msg)? {
        let checked = check_layer(lua, &check, &state, "state")?;
        for k in cfg.apply_state_defaults_from(checked) {
            record_source(lua, &sources, &k, "state")?;
        }
    }
    for (k, v) in mare_config::env_overrides() {
        let layer = format!("env MARE_{}", k.to_uppercase());
        let values = serde_json::json!({ k.clone(): v.clone() });
        let checked = check_layer(lua, &check, &values, layer.as_str())?;
        if let Some(val) = dotted_get(&checked, &k) {
            clear_sources_prefix(&sources, &k)?;
            cfg.set_dotted(&k, val.clone());
            record_layer_leaves(lua, &sources, &k, val, &layer)?;
        }
    }
    for binding in mare_shared::lua::core::config::plugin_env_bindings(lua)? {
        let Ok(raw) = std::env::var(&binding.env_var) else {
            continue;
        };
        if raw.is_empty() {
            continue;
        }
        let layer = format!("env {}", binding.env_var);
        let parsed = match mare_config::parse_value(&raw) {
            Ok(v) => v,
            Err(msg) => {
                mare_shared::lua::core::config::record_plugin_env_parse_error(lua, &binding, &msg)?;
                continue;
            }
        };
        let mut layer_cfg = mare_config::Config { value: serde_json::json!({}), warnings: vec![] };
        layer_cfg.set_dotted(&binding.path, parsed);
        let checked = check_layer(lua, &check, &layer_cfg.value, layer.as_str())?;
        if let Some(val) = dotted_get(&checked, &binding.path) {
            clear_sources_prefix(&sources, &binding.path)?;
            cfg.set_dotted(&binding.path, val.clone());
            record_layer_leaves(lua, &sources, &binding.path, val, &layer)?;
        }
    }
    // what the checks left of the command-line layer: an invalid declared plugin value is dropped
    let cli = check_layer(lua, &check, cli, "cli")?;
    // Snapshot only the validated values supplied via dedicated CLI flags before runtime writes
    // can mutate `mare.config` or `sources`, keeping `--set` and `HostOptions.config` separate.
    let cli_snapshot = snapshot_cli_flag_values(lua, &cli, cli_flag_paths)?;
    mare_shared::lua::core::config::set_cli_values(lua, cli_snapshot)?;
    // every leaf of the validated command-line layer (a list/union replaces whole and clears stale child leaves)
    record_cli_leaves(lua, &sources, &cli)?;
    mare_shared::lua::core::config::merge_json_respecting_plugin_schema(lua, &mut cfg.value, cli);
    mare_plugin_manager::set_project_scope(&mut cfg.value, cwd, project_trusted);
    // the packages' opts, located and checked with the configuration the runtime will use
    mare_plugin::config_load::merge_package_opts(lua, &mut cfg.value, config_view.as_ref())?;
    // If any post-init layer (env, cli, package) failed `plugins.preset` validation, clear any
    // inherited preset so `plugins.preset` behaves as unset ("no preset applies").
    if mare_shared::lua::core::config::preset_cleared(lua)
        && let Some(p) = cfg.value.get_mut(mare_shared::PLUGINS_KEY).and_then(|v| v.as_object_mut())
    {
        p.remove("preset");
    }
    // refill the table the config API closures write into (no second evaluation of plugin defs)
    let scope = cfg.value.as_object_mut().and_then(|root| root.remove(mare_plugin_manager::PROJECT_SCOPE_KEY));
    refill_config_table(lua, &c, &cfg.value)?;
    let keys_val = c.get::<Value>("keys")?;
    mare_shared::lua::core::keymap::apply_config_keys(lua, keys_val, Value::Table(sources.clone()), staging)?;
    if let Some(scope) = scope {
        cfg.value
            .as_object_mut()
            .expect("configuration is an object")
            .insert(mare_plugin_manager::PROJECT_SCOPE_KEY.into(), scope);
    }
    Ok(cfg.value)
}

fn record_source(lua: &mlua::Lua, sources: &Table, key: &str, layer: &str) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set("layer", layer)?;
    sources.set(key, t)
}

fn dotted_get<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = root;
    for seg in path.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

fn check_layer(
    lua: &mlua::Lua,
    check: &Function,
    layer_val: &serde_json::Value,
    layer_name: &str,
) -> mlua::Result<serde_json::Value> {
    let checked: Value = check.call((mare_shared::lua::config_to_lua(lua, layer_val)?, layer_name))?;
    match checked {
        Value::Nil => Ok(layer_val.clone()),
        v => Ok(mare_shared::lua::config_to_json(lua, v)?),
    }
}

fn refill_config_table(lua: &mlua::Lua, target: &Table, value: &serde_json::Value) -> mlua::Result<()> {
    let keys: Vec<Value> = target.pairs::<Value, Value>().filter_map(|kv| kv.ok().map(|(k, _)| k)).collect();
    for k in keys {
        target.raw_set(k, Value::Nil)?;
    }
    if let Value::Table(t) = mare_shared::lua::config_to_lua(lua, value)? {
        for kv in t.pairs::<Value, Value>() {
            let (k, v) = kv?;
            target.raw_set(k, v)?;
        }
    }
    Ok(())
}

fn clear_sources_prefix(sources: &Table, prefix: &str) -> mlua::Result<()> {
    let dot_prefix = format!("{prefix}.");
    let stale: Vec<String> = sources
        .pairs::<Value, Value>()
        .filter_map(|kv| {
            let (Value::String(k), _) = kv.ok()? else {
                return None;
            };
            let s = k.to_str().ok()?.to_string();
            (s == prefix || s.starts_with(&dot_prefix)).then_some(s)
        })
        .collect();
    for k in stale {
        sources.set(k, Value::Nil)?;
    }
    Ok(())
}

fn is_whole_option_path(lua: &mlua::Lua, path: &str) -> bool {
    let Some(rest) = path.strip_prefix("plugins.") else {
        return false;
    };
    let mut parts = rest.split('.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(plugin), Some(opt), None) if !plugin.is_empty() && !opt.is_empty() => {
            mare_shared::lua::core::config::is_plugin_option_whole(lua, plugin, opt)
        }
        _ => false,
    }
}

fn record_layer_leaves(
    lua: &mlua::Lua,
    sources: &Table,
    prefix: &str,
    v: &serde_json::Value,
    layer: &str,
) -> mlua::Result<()> {
    fn leaves(prefix: &str, v: &serde_json::Value, out: &mut Vec<String>) {
        out.push(prefix.to_string());
        if let Some(obj) = v.as_object() {
            for (k, x) in obj {
                leaves(&format!("{prefix}.{k}"), x, out);
            }
        }
    }
    let mut keys = vec![];
    leaves(prefix, v, &mut keys);
    for leaf in keys {
        record_source(lua, sources, &leaf, layer)?;
    }
    Ok(())
}

fn record_cli_leaves(lua: &mlua::Lua, sources: &Table, cli: &serde_json::Value) -> mlua::Result<()> {
    fn walk(lua: &mlua::Lua, sources: &Table, prefix: &str, v: &serde_json::Value) -> mlua::Result<()> {
        if !v.is_object() || is_whole_option_path(lua, prefix) {
            clear_sources_prefix(sources, prefix)?;
            return record_layer_leaves(lua, sources, prefix, v, "cli");
        }
        record_source(lua, sources, prefix, "cli")?;
        if let Some(obj) = v.as_object() {
            for (k, x) in obj {
                walk(lua, sources, &format!("{prefix}.{k}"), x)?;
            }
        }
        Ok(())
    }
    for (k, v) in cli.as_object().into_iter().flatten() {
        walk(lua, sources, k, v)?;
    }
    Ok(())
}

fn snapshot_cli_flag_values(
    lua: &mlua::Lua,
    cli: &serde_json::Value,
    cli_flag_paths: &[String],
) -> mlua::Result<Table> {
    let mut pinned = mare_config::Config { value: serde_json::json!({}), warnings: vec![] };
    for path in cli_flag_paths {
        if let Some(val) = dotted_get(cli, path) {
            pinned.set_dotted(path, val.clone());
        }
    }
    match mare_shared::lua::config_to_lua(lua, &pinned.value)? {
        Value::Table(t) => Ok(t),
        _ => lua.create_table(),
    }
}

/// Apply the invocation `--set` layer when `load_init` is false (`--clean` / `--clean`):
/// record its leaf sources, validate/prune via `_check_config`, and refill `mare.config` in place
/// so `mare.config`, `mare.o`, and `mare._cfg_internal.C` remain the same table instance.
pub(crate) fn apply_clean_cli(
    lua: &mlua::Lua,
    mare: &Table,
    cli: &serde_json::Value,
    cli_flag_paths: &[String],
) -> Result<serde_json::Value> {
    let sources = mare_shared::lua::core::config::ensure_sources(lua)?;
    mare_shared::lua::core::config::set_preset_cleared(lua, false);
    let check = mare_shared::lua::core::config::check_config_fn(lua)?;
    let out = check_layer(lua, &check, cli, "cli")?;
    // Snapshot only dedicated CLI flag paths (not arbitrary `HostOptions.config` in tests/children).
    let cli_snapshot = snapshot_cli_flag_values(lua, &out, cli_flag_paths)?;
    mare_shared::lua::core::config::set_cli_values(lua, cli_snapshot)?;
    // Keep `load_init = false` sources untouched outside `plugins.*` so clean/test
    // hosts do not tag `opts.config.model` or `opts.config.effort` as CLI-requested.
    if let Some(plugins) = out.get(mare_shared::PLUGINS_KEY) {
        let only_plugins = serde_json::Value::Object(serde_json::Map::from_iter([(
            mare_shared::PLUGINS_KEY.to_string(),
            plugins.clone(),
        )]));
        record_cli_leaves(lua, &sources, &only_plugins)?;
    }
    match mare.get::<Value>("config")? {
        Value::Table(cfg_tbl) => refill_config_table(lua, &cfg_tbl, &out)?,
        _ => mare.set("config", mare_shared::lua::config_to_lua(lua, &out)?)?,
    }
    let keys_val = match mare.get::<Value>("config")? {
        Value::Table(cfg_tbl) => cfg_tbl.get::<Value>("keys")?,
        _ => Value::Nil,
    };
    mare_shared::lua::core::keymap::apply_config_keys(lua, keys_val, Value::Table(sources), false)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_helpers_handle_missing_dotted_paths_and_nil_check_layer() {
        let root = serde_json::json!({ "a": 1 });
        assert!(dotted_get(&root, "a.b").is_none());
        assert_eq!(dotted_get(&root, "a"), Some(&serde_json::json!(1)));

        let lua = mlua::Lua::new();
        let nil_fn = lua.create_function(|_, (_val, _src): (Value, String)| Ok(Value::Nil)).unwrap();
        let layer = serde_json::json!({ "k": "v" });
        let out = check_layer(&lua, &nil_fn, &layer, "cli").unwrap();
        assert_eq!(out, layer);

        let snap =
            snapshot_cli_flag_values(&lua, &serde_json::json!({ "model": "m" }), &["model".into(), "missing".into()])
                .unwrap();
        assert_eq!(snap.get::<String>("model").unwrap(), "m");
    }

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("mare_eval_{tag}_{}_{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn run_eval(
        force_reeval: bool,
        init_src: &str,
        cli: &serde_json::Value,
    ) -> (serde_json::Value, Vec<String>, serde_json::Value) {
        let dir = TempDir::new(if force_reeval { "reeval" } else { "reuse" });
        let init_path = dir.path().join("config/init.lua");
        std::fs::create_dir_all(init_path.parent().unwrap()).unwrap();
        std::fs::write(&init_path, init_src).unwrap();
        // SAFETY: nextest isolates each test in its own process.
        unsafe { std::env::set_var("MARE_HOME", dir.path()) };

        let lua = mare_plugin::new_lua().unwrap();
        let env = mare_plugin::Env {
            cwd: dir.path().to_path_buf(),
            mode: mare_plugin::Mode::Child,
            ui: mare_shared::ui::UiShared::new(),
            config: cli.clone(),
            args: serde_json::Value::Null,
        };
        let mare = mare_plugin::install(&lua, &env).unwrap();
        if force_reeval {
            mare_shared::lua::core::config::set_fresh_install_config(&lua, None).unwrap();
        }
        let mut errors = Vec::new();
        let cfg = evaluate_config(&lua, &mare, dir.path(), false, cli, &["model".to_string()], Some(&mut errors))
            .await
            .unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        let warnings = mare_shared::lua::core::config::warnings(&lua).unwrap();
        let sources_val =
            mare_shared::lua::lua_to_json(&lua, Value::Table(mare_shared::lua::core::config::sources(&lua).unwrap()))
                .unwrap();
        (cfg, warnings, sources_val)
    }

    #[tokio::test]
    async fn fresh_install_reuse_matches_full_reevaluation_exactly() {
        let init_src = r#"
            mare.setup({ border = "single", reduce_motion = true })
            mare.plugin("compact", { enabled = true })
            mare.plugin("tools", { bash_timeout_ms = 12345 })
            mare.o.prompt = ">>"
        "#;
        let cli = serde_json::json!({ "model": "faux/echo", "plugins": { "statusline": { "enabled": false } } });

        let (cfg_reuse, warn_reuse, src_reuse) = run_eval(false, init_src, &cli).await;
        let (cfg_reeval, warn_reeval, src_reeval) = run_eval(true, init_src, &cli).await;

        // Normalize tempdir-prefixed `where` paths out of sources before comparing across the two runs.
        fn strip_where(v: &mut serde_json::Value) {
            if let Some(obj) = v.as_object_mut() {
                for entry in obj.values_mut() {
                    if let Some(e) = entry.as_object_mut()
                        && let Some(w) = e.get_mut("where")
                        && let Some(s) = w.as_str()
                        && let Some((_, tail)) = s.rsplit_once("init.lua:")
                    {
                        *w = serde_json::Value::String(format!("init.lua:{tail}"));
                    }
                }
            }
        }
        let mut src_reuse = src_reuse;
        let mut src_reeval = src_reeval;
        strip_where(&mut src_reuse);
        strip_where(&mut src_reeval);

        assert_eq!(cfg_reuse, cfg_reeval);
        assert_eq!(warn_reuse, warn_reeval);
        assert_eq!(src_reuse, src_reeval);
    }
}
