//! `mare.plugins`: what any plugin can ask the host about the plugins — one
//! plugin's status, every defined plugin, what failed at startup or in the latest full
//! /reload, and which plugins declare a tag or a wire (turned-off ones included). The host
//! keeps those facts (failures, the blocking rule, explicit disables), so
//! Lua asks instead of guessing; built-ins use this public API like any third-party plugin.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use mlua::{Lua, LuaSerdeExt, Table};

use crate::Host;
use crate::plugins::plugin_disabled::disabled;
use crate::plugins::plugin_failures::{self, PluginFailure};

struct IntrospectHost(Rc<RefCell<Weak<Host>>>);

/// Install the public `mare.plugins` API on `mare` before `kernel.start()` so plugins can inspect
/// declarations and report/clear declaration problems inside `apply()`.
pub(crate) fn install_early(kernel: &mare_kernel::Kernel, mare: &Table) -> mlua::Result<()> {
    let lua = kernel.lua();
    if lua.app_data_ref::<IntrospectHost>().is_some() && mare.get::<mlua::Value>(mare_shared::PLUGINS_KEY)?.is_table() {
        return Ok(());
    }
    let host_slot: Rc<RefCell<Weak<Host>>> = Rc::new(RefCell::new(Weak::new()));
    let _ = lua.set_app_data(IntrospectHost(Rc::clone(&host_slot)));
    let plugins = match mare.get::<Option<Table>>(mare_shared::PLUGINS_KEY)? {
        Some(plugins) => plugins,
        None => lua.create_table()?,
    };

    let slot = Rc::clone(&host_slot);
    let failures = lua.create_function(move |lua, ()| match slot.borrow().upgrade() {
        Some(host) => plugin_failures::to_lua(lua, &host.failures()),
        None => lua.create_table(),
    })?;

    let k = kernel.clone();
    let slot = Rc::clone(&host_slot);
    let status = lua.create_function(move |lua, name: String| {
        let row = k.plugin_tree_row(&name).unwrap_or(None);
        let tree = row.map_or_else(Vec::new, |r| vec![r]);
        let host = slot.borrow().upgrade();
        let failed = host.as_ref().map(|h| h.failure_for(&name, &tree)).unwrap_or_default();
        status_row(lua, &k, &name, &failed, &tree)
    })?;

    let k = kernel.clone();
    let slot = Rc::clone(&host_slot);
    let list = lua.create_function(move |lua, ()| {
        let out = lua.create_table()?;
        let tree = k.plugin_tree().unwrap_or_default();
        let host = slot.borrow().upgrade();
        let failed = host.as_ref().map(|h| h.failures_in(&tree)).unwrap_or_default();
        for name in k.def_names() {
            out.push(status_row(lua, &k, &name, &failed, &tree)?)?;
        }
        Ok(out)
    })?;

    let k = kernel.clone();
    let declaring = lua.create_function(move |lua, (field, value): (String, String)| {
        let names = k.declaring(&field, &value);
        lua.create_sequence_from(names)
    })?;

    let k = kernel.clone();
    let report_problem = lua.create_function(move |_, (plugin, key, message): (String, String, String)| {
        if plugin.is_empty() {
            return Err(mlua::Error::runtime("mare.plugins.report_problem needs a plugin name"));
        }
        if key.is_empty() {
            return Err(mlua::Error::runtime("mare.plugins.report_problem needs a problem key"));
        }
        k.report_declaration_problem(&plugin, &key, &message);
        Ok(())
    })?;

    let k = kernel.clone();
    let clear_problem = lua.create_function(move |_, (plugin, key): (String, String)| {
        k.clear_declaration_problem(&plugin, &key);
        Ok(())
    })?;

    plugins.set(mare_shared::STATUS_KEY, status)?;
    plugins.set("list", list)?;
    plugins.set("failures", failures)?;
    plugins.set("declaring", declaring)?;
    plugins.set("report_problem", report_problem)?;
    plugins.set("clear_problem", clear_problem)?;
    mare.set(mare_shared::PLUGINS_KEY, plugins)?;
    Ok(())
}

/// Bind `host` to `mare.plugins` (installing first if `install_early` did not run).
pub(crate) fn install(host: &Arc<Host>, mare: &Table) -> mlua::Result<()> {
    install_early(&host.kernel, mare)?;
    if let Some(slot) = host.lua().app_data_ref::<IntrospectHost>() {
        *slot.0.borrow_mut() = Arc::downgrade(host);
    }
    Ok(())
}

/// Turned off explicitly by the current config (`enabled = false`) and not running.
fn is_disabled_now(k: &mare_kernel::Kernel, name: &str, active: bool) -> bool {
    !active && disabled(&k.config(), name)
}

/// `mare.PluginStatus` of plugin `name`: {name, defined, active, disabled, origin?, overrides?,
/// provide, tags, wires, providers, sources, problems, failure?}; `sources` lists every read definition of the name. An unknown name is `defined = false`; `tags`, `wires`, and `providers`
/// also come from a definition turned off by the config.
fn status_row(
    lua: &Lua,
    k: &mare_kernel::Kernel,
    name: &str,
    failed: &[PluginFailure],
    tree: &[mare_kernel::PluginTreeRow],
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("name", name)?;
    let def = k.def(name);
    t.set("defined", def.is_some())?;
    let active = mare_kernel::Kernel::active_in(tree, name);
    t.set("active", active)?;
    t.set("disabled", is_disabled_now(k, name, active))?;
    if let Some(d) = &def {
        t.set("origin", d.source.origin.as_str())?;
        if let Some(o) = d.replaced {
            t.set("overrides", o.as_str())?;
        }
    }
    t.set("provide", lua.create_sequence_from(def.map(|d| d.provide).unwrap_or_default())?)?;
    let declared = k.declared(name).unwrap_or_default();
    t.set("tags", lua.create_sequence_from(declared.tags)?)?;
    t.set("wires", lua.create_sequence_from(declared.wires)?)?;
    t.set("providers", lua.create_sequence_from(declared.providers)?)?;
    let sources = lua.create_table()?;
    for (i, d) in k.declared_sources(name).into_iter().enumerate() {
        let row = lua.create_table()?;
        row.set("origin", d.origin)?;
        if let Some(path) = d.path {
            row.set("path", path.to_string_lossy().into_owned())?;
        }
        row.set("tags", lua.create_sequence_from(d.tags)?)?;
        row.set("wires", lua.create_sequence_from(d.wires)?)?;
        row.set("providers", lua.create_sequence_from(d.providers)?)?;
        row.set("provide", lua.create_sequence_from(d.provide)?)?;
        row.set("override", d.overrides)?;
        sources.raw_set(i + 1, row)?;
    }
    t.set("sources", sources)?;
    let problems = lua.create_table()?;
    problems.set_metatable(Some(lua.array_metatable()))?;
    for (i, (plugin, reporter, key, kind, message)) in k.declaration_problems_for(name).into_iter().enumerate() {
        let row = lua.create_table()?;
        row.set("plugin", plugin)?;
        row.set("reporter", reporter)?;
        row.set("key", key)?;
        row.set("kind", kind)?;
        row.set(mare_shared::MESSAGE_KEY, message)?;
        problems.raw_set(i + 1, row)?;
    }
    t.set("problems", problems)?;
    if let Some(f) = failed.iter().rev().find(|f| f.plugin == name && f.phase != "config") {
        let row = plugin_failures::to_lua(lua, std::slice::from_ref(f))?;
        t.set("failure", row.get::<Table>(1)?)?;
    }
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mare_kernel::{Kernel, Origin, PluginSource};

    #[tokio::test]
    async fn status_and_single_plugin_queries_do_not_dump_whole_tree() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = mare_plugin::new_lua().unwrap();
                let cfg = serde_json::json!({ "plugins": { "delta": { "enabled": false } } });
                let env = mare_plugin::Env {
                    cwd: std::env::temp_dir(),
                    mode: mare_plugin::Mode::Print,
                    ui: mare_shared::ui::UiShared::new(),
                    config: cfg.clone(),
                    args: serde_json::json!({}),
                };
                let mare = mare_plugin::install(&lua, &env).unwrap();
                let k = Kernel::new(lua, cfg).unwrap();
                let alpha = k
                    .load_def(&PluginSource {
                        name_hint: "alpha".into(),
                        origin: Origin::Builtin,
                        path: None,
                        code: Some(b"return { name = 'alpha', apply = function() end }"),
                    })
                    .unwrap();
                let beta = k
                    .load_def(&PluginSource {
                        name_hint: "beta".into(),
                        origin: Origin::Builtin,
                        path: None,
                        code: Some(b"return { name = 'beta', inject = { 'missing_svc' }, apply = function() end }"),
                    })
                    .unwrap();
                let gamma = k
                    .load_def(&PluginSource {
                        name_hint: "gamma".into(),
                        origin: Origin::Builtin,
                        path: None,
                        code: Some(b"return { name = 'gamma', apply = function() error('boom-gamma') end }"),
                    })
                    .unwrap();
                let delta = k
                    .load_def(&PluginSource {
                        name_hint: "delta".into(),
                        origin: Origin::Builtin,
                        path: None,
                        code: Some(b"return { name = 'delta', apply = function() end }"),
                    })
                    .unwrap();
                let epsilon = k
                    .load_def(&PluginSource {
                        name_hint: "epsilon".into(),
                        origin: Origin::Builtin,
                        path: None,
                        code: Some(
                            b"return { name = 'epsilon', apply = function(ctx) ctx:plugin({ name = 'sub', apply = function() error('boom-sub') end }) end }",
                        ),
                    })
                    .unwrap();
                k.set_defs(vec![alpha, beta, gamma, delta, epsilon]);
                install_early(&k, &mare).unwrap();
                k.start().await.unwrap();

                let booted = k.cordis_boot().unwrap();
                let root: Table = booted.get("root").unwrap();
                let get: mlua::Function = root.get("get").unwrap();
                let loader: Table = get.call((root, "loader", false)).unwrap();
                k.lua().globals().set("_TEST_LOADER", loader).unwrap();
                k.lua()
                    .load(
                        r#"
                        _DUMP_CALLS = 0
                        local orig = _TEST_LOADER.dump
                        _TEST_LOADER.dump = function(self, ...)
                            _DUMP_CALLS = _DUMP_CALLS + 1
                            return orig(self, ...)
                        end
                        "#,
                    )
                    .exec()
                    .unwrap();

                assert!(k.plugin_active("alpha"));
                assert!(!k.plugin_active("beta"));
                assert!(!k.plugin_active("gamma"));
                assert!(!k.plugin_active("delta"));
                assert!(!k.plugin_active("epsilon"));
                assert!(!k.plugin_active("unknown"));
                assert_eq!(k.plugin_missing("beta"), vec!["missing_svc".to_string()]);
                assert!(k.plugin_missing("alpha").is_empty());

                let st_alpha: Table = k.lua().load("return mare.plugins.status('alpha')").eval().unwrap();
                assert!(st_alpha.get::<bool>("defined").unwrap());
                assert!(st_alpha.get::<bool>("active").unwrap());
                assert!(!st_alpha.get::<bool>("disabled").unwrap());

                let st_beta: Table = k.lua().load("return mare.plugins.status('beta')").eval().unwrap();
                assert!(st_beta.get::<bool>("defined").unwrap());
                assert!(!st_beta.get::<bool>("active").unwrap());

                let st_gamma: Table = k.lua().load("return mare.plugins.status('gamma')").eval().unwrap();
                assert!(st_gamma.get::<bool>("defined").unwrap());
                assert!(!st_gamma.get::<bool>("active").unwrap());

                let st_delta: Table = k.lua().load("return mare.plugins.status('delta')").eval().unwrap();
                assert!(st_delta.get::<bool>("defined").unwrap());
                assert!(st_delta.get::<bool>("disabled").unwrap());
                assert!(!st_delta.get::<bool>("active").unwrap());

                let st_epsilon: Table = k.lua().load("return mare.plugins.status('epsilon')").eval().unwrap();
                assert!(st_epsilon.get::<bool>("defined").unwrap());
                assert!(!st_epsilon.get::<bool>("active").unwrap());

                let st_unknown: Table = k.lua().load("return mare.plugins.status('unknown')").eval().unwrap();
                assert!(!st_unknown.get::<bool>("defined").unwrap());

                let single_rows: Vec<(&str, Option<mare_kernel::PluginTreeRow>)> =
                    ["alpha", "beta", "gamma", "delta", "epsilon"]
                        .into_iter()
                        .map(|name| (name, k.plugin_tree_row(name).unwrap()))
                        .collect();

                let calls: usize = k.lua().globals().get("_DUMP_CALLS").unwrap();
                assert_eq!(calls, 0, "single-plugin status/active/missing/row lookups must not call loader:dump()");

                // Verify single-row lookup and `start_failure_for` match full `plugin_tree()` (`loader:dump()`).
                let full_tree = k.plugin_tree().unwrap();
                let all_failures = plugin_failures::start_failures(&k, &full_tree);
                for (name, single) in single_rows {
                    let from_dump = full_tree.iter().find(|r| r.id == name).cloned();
                    assert_eq!(
                        single.as_ref().map(|r| (&r.state, &r.missing, r.disabled, &r.provides)),
                        from_dump.as_ref().map(|r| (&r.state, &r.missing, r.disabled, &r.provides)),
                        "plugin_tree_row({name}) must match plugin_tree() dump"
                    );
                    let single_slice = single.into_iter().collect::<Vec<_>>();
                    let single_failure = plugin_failures::start_failure_for(&k, name, &single_slice);
                    let dump_failure = all_failures.iter().find(|f| f.plugin == name).cloned();
                    assert_eq!(
                        single_failure.as_ref().map(|f| (&f.phase, &f.error, &f.blocks)),
                        dump_failure.as_ref().map(|f| (&f.phase, &f.error, &f.blocks)),
                        "start_failure_for({name}) must match start_failures()"
                    );
                }
                assert!(
                    all_failures.iter().any(|f| f.plugin == "gamma" && f.error.contains("boom-gamma")),
                    "gamma failure must be recorded"
                );
                assert!(
                    all_failures.iter().any(|f| f.plugin == "epsilon" && f.error.contains("boom-sub")),
                    "epsilon failed child ctx:plugin sub-fiber must be recorded"
                );
            })
            .await;
    }

    #[tokio::test]
    async fn kernel_start_dumps_plugin_tree_at_most_once() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = mare_plugin::new_lua().unwrap();
                let env = mare_plugin::Env {
                    cwd: std::env::temp_dir(),
                    mode: mare_plugin::Mode::Print,
                    ui: mare_shared::ui::UiShared::new(),
                    config: serde_json::json!({}),
                    args: serde_json::json!({}),
                };
                let _mare = mare_plugin::install(&lua, &env).unwrap();
                let k = Kernel::new(lua, serde_json::json!({})).unwrap();
                k.expose_test_boot();
                let alpha = k
                    .load_def(&PluginSource {
                        name_hint: "alpha".into(),
                        origin: Origin::Builtin,
                        path: None,
                        code: Some(b"return { name = 'alpha', apply = function() end }"),
                    })
                    .unwrap();
                k.set_defs(vec![alpha]);
                let wrap: mlua::Function = k
                    .lua()
                    .load(
                        r#"
                        return function()
                            local loader = require("mare_kernel.test").boot().loader()
                            _START_DUMP_CALLS = 0
                            local orig = loader.dump
                            loader.dump = function(self, ...)
                                _START_DUMP_CALLS = _START_DUMP_CALLS + 1
                                return orig(self, ...)
                            end
                        end
                        "#,
                    )
                    .eval()
                    .unwrap();
                mare_shared::lua::call_async::<()>(k.lua(), &wrap, ()).await.unwrap();

                k.start().await.unwrap();

                let calls: usize = k.lua().globals().get("_START_DUMP_CALLS").unwrap();
                assert_eq!(calls, 1, "Kernel::start() should dump the cordis loader tree once, not {calls} times");
            })
            .await;
    }
}
