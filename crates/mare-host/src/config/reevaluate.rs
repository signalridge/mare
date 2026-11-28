//! Re-running the configuration for a reload, with the managed registrations rolled back when
//! it fails.

use std::sync::Arc;

use anyhow::Result;
use mlua::{Function, Table, Value};

use crate::Host;
use crate::config::eval::evaluate_config;

/// Restore the previous managed configuration when evaluation errors or its future is dropped.
/// This covers registrations managed here, not arbitrary effects performed by executable Lua.
struct ConfigEvalRollback {
    lua: mlua::Lua,
    mare: Table,
    previous_state: Table,
    saved_public: Vec<(&'static str, Value)>,
    previous_keymaps: Value,
    restore_keymaps: Function,
    keymap_snapshot: Value,
    old_keymaps_removed: bool,
    restore_actions: Function,
    actions_snapshot: Value,
    previous_contexts: Value,
    rollback_contexts: Function,
    previous_components: Value,
    undo: Table,
    functions_snapshot: Table,
    flags_snapshot: std::collections::BTreeMap<String, mare_shared::flags::FlagSpec>,
    previous_plugins_add: Value,
    committed: bool,
}

impl Drop for ConfigEvalRollback {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let report = |step: &str, result: mlua::Result<()>| {
            if let Err(error) = result {
                tracing::error!("config reload rollback ({step}): {error}");
            }
        };
        report(
            "keymaps",
            (|| {
                let current = mare_shared::lua::core::config::config_keymaps(&self.lua)?;
                if current != self.previous_keymaps {
                    let saved = if self.old_keymaps_removed { self.keymap_snapshot.clone() } else { Value::Nil };
                    self.restore_keymaps.call::<()>((saved, current))?;
                }
                Ok(())
            })(),
        );
        report("keymap actions", self.restore_actions.call::<()>(self.actions_snapshot.clone()));
        report(
            "contexts",
            (|| {
                let current = mare_shared::lua::core::config::config_contexts(&self.lua)?;
                if current != self.previous_contexts {
                    self.rollback_contexts.call::<()>(current)?;
                }
                Ok(())
            })(),
        );
        report(
            "components",
            (|| {
                if let Value::Table(components) = mare_shared::lua::core::config::config_components(&self.lua)?
                    && Value::Table(components.clone()) != self.previous_components
                {
                    for remove in components.sequence_values::<Function>() {
                        remove?.call::<()>(())?;
                    }
                }
                Ok(())
            })(),
        );
        for index in (1..=self.undo.raw_len()).rev() {
            report(
                "plugin config API",
                (|| {
                    let entry: Table = self.undo.raw_get(index)?;
                    let target: Table = entry.get("target")?;
                    target.set(entry.get::<Value>("key")?, entry.get::<Value>("old")?)
                })(),
            );
        }
        report("config state", mare_shared::lua::core::config::set_state(&self.lua, self.previous_state.clone()));
        for (key, err) in mare_shared::lua::core::config::restore_public_fields(&self.mare, &self.saved_public) {
            report(key, Err(err));
        }
        report(
            "plugins.add",
            (|| {
                if let Some(plugins) = mare_plugin::pkg_binding::plugins_table(&self.mare)? {
                    plugins.raw_set("add", self.previous_plugins_add.clone())?;
                }
                Ok(())
            })(),
        );
        report(
            "function registry",
            mare_shared::lua::restore_config_fn_registry(&self.lua, self.functions_snapshot.clone()),
        );
        mare_shared::flags::FlagStore::of(&self.lua).restore_specs(std::mem::take(&mut self.flags_snapshot));
    }
}

impl Host {
    /// Run init.lua (+ the trusted project's) again: returns (old config, new config, changed
    /// core option names). Plugins are not touched.
    pub(crate) async fn reevaluate(self: &Arc<Self>) -> Result<(serde_json::Value, serde_json::Value, Vec<String>)> {
        if !self.reload.load_init {
            return Err(anyhow::Error::new(mare_shared::errors::Coded {
                code: "unavailable",
                message: "config reload is unavailable (started without user config)".into(),
            }));
        }
        let lua = self.lua().clone();
        let mare: Table = lua.globals().get("mare")?;
        let old = self.kernel.config();
        // Keep table identities and raw entries, including cycles and function values. Lua code
        // can retain a direct reference to the committed config across a yielding evaluation;
        // abort the replacement if such a reference was mutated instead of losing that update.
        let unchanged: Function = lua
            .load(r#"local root, options = ...
              local records = {}
              local pending = { root, options }
              while #pending > 0 do
                local t = table.remove(pending)
                if type(t) == 'table' and not records[t] then
                  local entries = {}
                  records[t] = { entries = entries, mt = getmetatable(t) }
                  for k, v in next, t do
                    entries[k] = v
                    if type(k) == 'table' then pending[#pending+1] = k end
                    if type(v) == 'table' then pending[#pending+1] = v end
                  end
                end
              end
              return function()
                for t, snapshot in pairs(records) do
                  if not rawequal(getmetatable(t), snapshot.mt) then return false end
                  local seen = 0
                  for k, v in next, t do
                    local old = snapshot.entries[k]
                    if not rawequal(v, old) and not (type(v) == 'number' and type(old) == 'number' and v ~= v and old ~= old) then return false end
                    seen = seen + 1
                  end
                  local count = 0
                  for _ in next, snapshot.entries do count = count + 1 end
                  if seen ~= count then return false end
                end
                return true
              end"#)
            .set_name("@core/config_conflict_snapshot")
            .call((mare.get::<Value>("config")?, mare.get::<Value>(mare_shared::OPTIONS_KEY)?))?;
        // Preserve the live config state and public API until the replacement has evaluated.
        // Plugin config APIs may install functions in nested namespaces; their assignments are
        // recorded separately so a failed evaluation can restore those namespaces too.
        let saved_public = mare_shared::lua::core::config::snapshot_public_fields(&mare)?;
        let previous_plugins_add = mare_plugin::pkg_binding::plugins_table(&mare)?
            .and_then(|t| t.raw_get::<Value>("add").ok())
            .unwrap_or(Value::Nil);
        let previous_keymaps = mare_shared::lua::core::config::config_keymaps(&lua)?;
        let previous_contexts = mare_shared::lua::core::config::config_contexts(&lua)?;
        let keymap: Table = mare.get("keymap")?;
        let keymap_snapshot = mare_shared::lua::core::keymap::config_snapshot(&lua, &mare, previous_keymaps.clone())?;
        let actions_snapshot = mare_shared::lua::core::keymap::config_actions_snapshot(&lua, &mare)?;
        let functions_snapshot = mare_shared::lua::snapshot_config_fns(&lua)?;
        let flags_store = mare_shared::flags::FlagStore::of(&lua);
        let flags_snapshot = flags_store.snapshot_specs();
        let old_unsubs = mare_shared::lua::core::config::on_unsub(&lua)?;
        let old_components = mare_shared::lua::core::config::config_components(&lua)?;
        let undo = lua.create_table()?;
        let previous_state = mare_shared::lua::core::config::begin_reeval_state(&lua, undo.clone())?;
        let mut rollback = ConfigEvalRollback {
            lua: lua.clone(),
            mare: mare.clone(),
            previous_state,
            saved_public,
            previous_keymaps,
            restore_keymaps: mare_shared::lua::core::keymap::config_restore_fn(&lua, &mare)?,
            keymap_snapshot,
            old_keymaps_removed: false,
            restore_actions: mare_shared::lua::core::keymap::config_actions_restore_fn(&lua, &mare)?,
            actions_snapshot,
            previous_contexts: previous_contexts.clone(),
            rollback_contexts: mare_shared::lua::core::keymap::config_rollback_contexts_fn(&lua, &mare)?,
            previous_components: old_components.clone(),
            undo,
            functions_snapshot,
            flags_snapshot,
            previous_plugins_add,
            committed: false,
        };
        flags_store.clear_source_prefix("config:");
        let mut new = evaluate_config(
            &lua,
            &mare,
            &self.cwd,
            self.workspace.is_trusted(),
            &self.reload.cli,
            &self.reload.cli_flag_paths,
            None,
        )
        .await?;
        if !unchanged.call::<bool>(())? {
            anyhow::bail!("config changed during reload; retry after concurrent updates finish");
        }
        // #3: an invalid `on_invalid = "error"` option fails before taking effect and
        // preserves the plugin's running configuration.
        if let Some(errors) = mare_shared::lua::core::config::plugin_error(&lua)? {
            let old_lua_cfg =
                rollback.saved_public.iter().find(|(k, _)| *k == "config").and_then(|(_, v)| v.as_table().cloned());
            let new_lua_cfg: Option<Table> = mare.get("config").ok();
            for kv in errors.pairs::<String, Value>() {
                let (plugin, _) = kv?;
                match old.get(mare_shared::PLUGINS_KEY).and_then(|p| p.get(&plugin)) {
                    Some(prev) => {
                        if let Some(root) = new.as_object_mut() {
                            let plugins = root.entry(mare_shared::PLUGINS_KEY).or_insert_with(|| serde_json::json!({}));
                            if let Some(map) = plugins.as_object_mut() {
                                map.insert(plugin.clone(), prev.clone());
                            }
                        }
                    }
                    None => {
                        if let Some(map) = new.get_mut(mare_shared::PLUGINS_KEY).and_then(|p| p.as_object_mut()) {
                            map.remove(&plugin);
                        }
                    }
                }
                if let Some(new_cfg) = &new_lua_cfg {
                    let prev_lua = old_lua_cfg
                        .as_ref()
                        .and_then(|c| c.get::<Option<Table>>(mare_shared::PLUGINS_KEY).ok().flatten())
                        .and_then(|p| p.get::<Value>(plugin.as_str()).ok())
                        .unwrap_or(Value::Nil);
                    let plugins_tbl: Table = match new_cfg.get::<Option<Table>>(mare_shared::PLUGINS_KEY)? {
                        Some(t) => t,
                        None => {
                            let t = lua.create_table()?;
                            new_cfg.set(mare_shared::PLUGINS_KEY, t.clone())?;
                            t
                        }
                    };
                    plugins_tbl.set(plugin.as_str(), prev_lua)?;
                }
            }
        }
        mare_shared::lua::core::config::set_old_sources(&lua, Value::Nil)?;
        mare_shared::lua::core::config::set_old_cli_values(&lua, Value::Nil)?;
        mare_shared::lua::core::config::set_api_undo(&lua, Value::Nil)?;
        let stage = mare_shared::lua::core::config::stage(&lua)?;
        for entry in stage.sequence_values::<Table>() {
            let entry = entry?;
            if let Some(validate) = entry.get::<Option<Function>>("validate")? {
                validate.call::<()>(())?;
            }
        }
        mare_shared::lua::core::keymap::config_validate_stage(
            &lua,
            &mare,
            stage.clone(),
            rollback.previous_keymaps.clone(),
        )?;
        rollback.old_keymaps_removed = true;
        if let Value::Table(previous) = &rollback.previous_keymaps {
            let remove: Function = keymap.get("remove")?;
            for handle in previous.sequence_values::<Value>() {
                remove.call::<()>(handle?)?;
            }
        }
        mare_shared::lua::core::keymap::config_actions_commit(&lua, &mare, rollback.actions_snapshot.clone())?;
        let current_contexts = mare_shared::lua::core::config::config_contexts(&lua)?;
        mare_shared::lua::core::keymap::config_commit_contexts(
            &lua,
            &mare,
            rollback.previous_contexts.clone(),
            current_contexts,
        )?;
        mare_shared::lua::core::config::set_previous_contexts(&lua, Value::Nil)?;
        mare_shared::lua::core::config::set_stage(&lua, Value::Nil)?;
        for entry in stage.sequence_values::<Table>() {
            entry?.get::<Function>("commit")?.call::<()>(())?;
        }
        mare_shared::lua::core::config::set_previous_keymaps(&lua, Value::Nil)?;
        if let Some(view) = mare_shared::lua::core::config::current_view(&lua)? {
            mare_shared::lua::core::config::sync_config_views(&lua, &mare, view)?;
            mare_shared::lua::core::config::set_current_view(&lua, Value::Nil)?;
        }
        // There are no suspension points between committing and removing the old registrations.
        rollback.committed = true;
        if let Value::Table(components) = old_components {
            for remove in components.sequence_values::<Function>().flatten() {
                let _ = remove.call::<()>(());
            }
        }
        if let Value::Table(unsubs) = old_unsubs {
            for f in unsubs.sequence_values::<Function>().flatten() {
                let _ = f.call::<()>(());
            }
        }
        super::commit_kernel_config(&self.kernel, &self.workspace, new.clone());
        // Publish config subscriptions in the same non-yielding commit as the other managed
        // registrations; plugin reloads below may suspend for an arbitrary amount of time.
        self.install_config_runtime()?;
        let mut changed = vec![];
        let keys: std::collections::BTreeSet<&String> = old
            .as_object()
            .into_iter()
            .flat_map(|o| o.keys())
            .chain(new.as_object().into_iter().flat_map(|o| o.keys()))
            .collect();
        for k in keys {
            if k != mare_shared::PLUGINS_KEY && old.get(k) != new.get(k) {
                changed.push(k.clone());
            }
        }
        Ok((old, new, changed))
    }

    /// After [`Self::reevaluate`], report changed core options (theme next frame, prompt
    /// sections next request) and return the config warnings.
    pub(crate) fn finish_reevaluate(
        self: &Arc<Self>,
        new: &serde_json::Value,
        changed: &[String],
    ) -> Result<Vec<String>> {
        let lua = self.lua().clone();
        if let Some(f) = mare_shared::lua::core::config::option_changed(&lua)? {
            for k in changed {
                if !k.starts_with("plugins.") {
                    let v = mare_shared::lua::json_to_lua(&lua, new.get(k).unwrap_or(&serde_json::Value::Null))?;
                    let _ = f.call::<()>((k.clone(), v));
                }
            }
        }
        mare_shared::lua::retire_config_fns(&lua)?;
        Ok(mare_shared::lua::core::config::warnings(&lua)?)
    }
}
