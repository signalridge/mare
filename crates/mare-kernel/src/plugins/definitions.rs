//! Evaluating plugin files into definitions (top-level guard) and keeping the definition set.

use std::collections::HashMap;

use mlua::{Lua, Table, Value};

use crate::plugins::overrides;
use crate::{Declared, Kernel, Origin, OverrideRefusal, PluginDef, PluginSource, ServiceContract};

/// What a plugin file's top level may read: the
/// pure standard library, enough for constant tables built from other constants.
const TOP_LEVEL_GLOBALS: &[&str] = &[
    "assert",
    "error",
    "getmetatable",
    "ipairs",
    "math",
    "next",
    "pairs",
    "pcall",
    "rawequal",
    "rawget",
    "rawlen",
    "select",
    "setmetatable",
    "string",
    "table",
    "tonumber",
    "tostring",
    "type",
    "utf8",
    "xpcall",
];

/// The metatable a plugin's env carries while its file's top level runs ([`Kernel::load_def`]):
/// reads outside [`TOP_LEVEL_GLOBALS`] and every global write raise. Built once per Lua state.
fn top_level_guard(lua: &Lua) -> mlua::Result<Table> {
    const KEY: &str = "mare.top_level_guard";
    if let Ok(t) = lua.named_registry_value::<Table>(KEY) {
        return Ok(t);
    }
    let allowed = lua.create_table()?;
    for n in TOP_LEVEL_GLOBALS {
        allowed.set(*n, true)?;
    }
    let meta: Table = lua
        .load(
            r#"local allowed, G = ...
            local msg = "the top level of a plugin file may only `return { ... }` (it %s `%s`); "
                .. "do that work in `apply`"
            return {
                __index = function(_, k)
                    if allowed[k] then
                        return G[k]
                    end
                    error(msg:format("reads", tostring(k)), 2)
                end,
                __newindex = function(_, k)
                    error(msg:format("sets", tostring(k)), 2)
                end,
            }"#,
        )
        .set_name("=mare top-level guard")
        .call((allowed, lua.globals()))?;
    lua.set_named_registry_value(KEY, &meta)?;
    Ok(meta)
}

fn parse_web_def(plugin_name: &str, value: Value) -> anyhow::Result<Option<crate::WebPluginDef>> {
    match value {
        Value::Nil => Ok(None),
        Value::Table(t) => {
            let dir = t
                .get::<Option<String>>("dir")?
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow::anyhow!("plugin `{plugin_name}`: `web.dir` must be a non-empty string"))?;
            let opt_str = |k: &str| -> anyhow::Result<Option<String>> {
                Ok(t.get::<Option<String>>(k)?.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
            };
            Ok(Some(crate::WebPluginDef {
                dir,
                prefix: opt_str("prefix")?,
                index: opt_str("index")?,
                fallback: opt_str("fallback")?,
            }))
        }
        other => anyhow::bail!("plugin `{plugin_name}`: `web` must be a table, got {}", other.type_name()),
    }
}

fn parse_flags(plugin_name: &str, value: Value) -> anyhow::Result<Vec<mare_shared::flags::FlagSpec>> {
    use mare_shared::flags::{FlagSpec, FlagType, FlagValue};
    match value {
        Value::Nil => Ok(vec![]),
        Value::Table(t) => {
            let mut out = vec![];
            for pair in t.pairs::<Value, Value>() {
                let (k, v) = pair?;
                let Value::String(ks) = k else {
                    anyhow::bail!("plugin `{plugin_name}`: `flags` keys must be strings");
                };
                let flag_name = ks.to_str()?.to_string();
                let Value::Table(opts) = v else {
                    anyhow::bail!("plugin `{plugin_name}`: flag `{flag_name}` entry must be a table");
                };
                let ty_str: String = opts
                    .get("type")
                    .map_err(|_| anyhow::anyhow!("plugin `{plugin_name}`: flag `{flag_name}` requires `type`"))?;
                let flag_type = FlagType::parse(&ty_str)
                    .map_err(|e| anyhow::anyhow!("plugin `{plugin_name}`: flag `{flag_name}`: {e}"))?;
                let description: Option<String> = opts.get("description")?;
                let default = match opts.get::<Value>("default")? {
                    Value::Nil => None,
                    dv => Some(
                        FlagValue::from_lua(dv, flag_type)
                            .map_err(|e| anyhow::anyhow!("plugin `{plugin_name}`: flag `{flag_name}` default: {e}"))?,
                    ),
                };
                let spec = FlagSpec {
                    name: flag_name.clone(),
                    flag_type,
                    description,
                    default,
                    source: Some(format!("plugin:{plugin_name}")),
                };
                spec.validate().map_err(|e| anyhow::anyhow!("plugin `{plugin_name}`: {e}"))?;
                out.push(spec);
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        }
        other => anyhow::bail!("plugin `{plugin_name}`: `flags` must be a table, got {}", other.type_name()),
    }
}

/// Host built-in services registered via `cordis.Service` before user plugins load
/// (`mare_kernel/boot.lua`): never admitted through `mare.Service`, so a plugin declaring them
/// would trigger contract-admission refusal during host boot.
const HOST_BUILTIN_SERVICES: &[&str] = &["task", "bus", "rpc", "contracts", "loader"];

/// The fields a definition has: what the loader, the rows and config_load read (`mare.PluginDef` in
/// preset/types/mare/services/ctx.lua).
const DEF_FIELDS: &[&str] = &[
    "name",
    "inject",
    "provide",
    "contracts",
    "tags",
    "wires",
    "providers",
    "flags",
    mare_shared::WEB_ASSETS_DEF_FIELD,
    "required",
    "override",
    "config_api",
    "config_schema",
    "apply",
];

/// Removed legacy fields and their replacements.
const REMOVED_FIELDS: &[(&str, &str)] = &[
    (
        "optional",
        "a service the plugin can run without stays out of `inject`: read it with `ctx:get(name)` \
         (`mare_lib.follow` wraps that), or register into it with `ctx:inject({ name }, fn)`",
    ),
    ("cordis", "every plugin is native now; delete the field"),
    ("startup", "every plugin starts before the host is handed out (there is no deferred start); delete the field"),
];

/// A removed or unknown field is ignored, and said so the way an unknown option is: on the config
/// warning list (`mare._warnings`: stderr in headless / RPC / ACP, an error under `--strict-config`,
/// listed by /config and doctor). Nothing is translated: the definition loads as written.
fn warn_unread_fields(lua: &Lua, table: &Table, name: &str, file: &str) -> anyhow::Result<()> {
    let mut warnings = vec![];
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair?;
        let Value::String(key) = key else { continue };
        let key = key.to_str()?.to_string();
        if DEF_FIELDS.contains(&key.as_str()) {
            continue;
        }
        warnings.push(match REMOVED_FIELDS.iter().find(|(field, _)| *field == key) {
            Some((field, instead)) => {
                format!("plugin `{name}` ({file}): field `{field}` was removed and is ignored; {instead}")
            }
            None => format!(
                "plugin `{name}` ({file}): unknown field `{key}` is ignored{}",
                mare_shared::layout::closest_match(&key, DEF_FIELDS)
                    .map(|near| format!(" — did you mean `{near}`?"))
                    .unwrap_or_default()
            ),
        });
    }
    if warnings.is_empty() {
        return Ok(());
    }
    warnings.sort();
    let Ok(Value::Table(_)) = lua.globals().raw_get::<Value>("mare") else { return Ok(()) };
    let list = mare_shared::lua::core::config::ensure_warnings_table(lua)?;
    // a definition is loaded more than once per evaluation (config discovery, rows, a reload's
    // prepare): each warning once
    let seen: Vec<String> = list.sequence_values::<String>().filter_map(Result::ok).collect();
    for warning in warnings {
        if !seen.contains(&warning) {
            tracing::warn!("{warning}");
            list.raw_push(warning)?;
        }
    }
    Ok(())
}

impl Kernel {
    /// Evaluate a plugin source into a definition.
    pub fn load_def(&self, src: &PluginSource) -> anyhow::Result<PluginDef> {
        let lua = self.lua();
        // every plugin runs in its own environment table (globals isolated, reads fall back to _G)
        let env = mare_shared::lua::isolated_env(lua)?;
        let chunk_name = src.chunk_name();
        let file = chunk_name.trim_start_matches('@');
        let chunk = src
            .load_chunk(lua, env.clone())
            .map_err(|e| anyhow::anyhow!("loading plugin {} ({file}): {e}", src.name_hint))?;
        // a plugin file's top level may only `return { ... }`; the work goes
        // in `apply`. While the chunk runs its env answers only pure standard-library names and
        // refuses every other read (`mare`, `require`, io, ...) and every global write; the
        // normal metatable comes back before anything in the returned table runs.
        let normal = env.metatable();
        env.set_metatable(Some(top_level_guard(lua)?))?;
        let value = chunk.call::<Value>(());
        env.set_metatable(normal)?;
        let table = match value.map_err(|e| anyhow::anyhow!("loading plugin {} ({file}): {e}", src.name_hint))? {
            Value::Table(t) => t,
            other => anyhow::bail!(
                "loading plugin {} ({file}): a plugin file must end in `return {{ ... }}` (it returned {})",
                src.name_hint,
                other.type_name()
            ),
        };
        let name: String = table.get::<Option<String>>("name")?.unwrap_or_else(|| src.name_hint.clone());
        // Bind `require` directly on the plugin's isolated environment table (caller identity
        // comes from the closure's owner, never from chunk names or call-stack inspection).
        env.raw_set("require", self.make_require(&name)?)?;
        self.parse_def_table(table, src, env, name, file)
    }

    fn parse_def_table(
        &self,
        table: Table,
        src: &PluginSource,
        env: Table,
        name: String,
        file: &str,
    ) -> anyhow::Result<PluginDef> {
        let lua = self.lua();
        // one declaration, no alias for the old pair
        let old: Vec<&str> = ["config", "config_types"]
            .into_iter()
            .filter(|k| !matches!(table.raw_get::<Value>(*k), Ok(Value::Nil)))
            .collect();
        if !old.is_empty() {
            anyhow::bail!(
                "loading plugin {name} ({file}): `{}` replaced by `config_schema` (one entry per option: \
                 `{{ type, default, desc, ... }}`); declare the options there",
                old.join("` and `")
            );
        }
        warn_unread_fields(lua, &table, &name, file)?;
        let list = |key: &str| -> anyhow::Result<Vec<String>> {
            Ok(match table.get::<Value>(key)? {
                Value::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>()?,
                Value::String(s) => vec![s.to_str()?.to_string()],
                _ => vec![],
            })
        };
        let inject = list("inject")?;
        let contracts = match table.get::<Value>("contracts")? {
            Value::Nil => Default::default(),
            value => crate::dispatch::services::parse_contracts(&self.0.lua, value)?,
        };
        let is_third_party = src.origin != Origin::Builtin;
        {
            let st = self.st();
            for (service, contract) in &contracts {
                if HOST_BUILTIN_SERVICES.contains(&service.as_str()) {
                    anyhow::bail!("plugin `{name}` cannot declare reserved host service `{service}` in `contracts`");
                }
                if st.contracts.contains_key(service) {
                    anyhow::bail!("plugin `{name}` cannot redeclare the built-in service contract `{service}`");
                }
                if contract.essential {
                    anyhow::bail!("plugin `{name}` cannot declare essential service `{service}`");
                }
                if contract.methods.values().any(crate::dispatch::contracts::MethodSpec::is_host) {
                    anyhow::bail!(
                        "plugin `{name}` cannot declare host-provided methods (`provider = \"host\"`) in service `{service}`"
                    );
                }
                if !contract.events.is_empty()
                    && crate::dispatch::contracts::is_protocol_reserved_event_root(
                        service,
                        contract.owner.as_deref().unwrap_or(service),
                        false,
                        is_third_party,
                    )
                {
                    anyhow::bail!("plugin `{name}` cannot declare events under protocol-reserved root `{service}`");
                }
                for event_name in contract.events.values().map(|e| &e.name) {
                    if st.contracts.values().flat_map(|c| c.events.values()).any(|e| &e.name == event_name) {
                        anyhow::bail!("plugin `{name}` event `{event_name}` conflicts with built-in contract event");
                    }
                    if let Some(other) = st.defs.iter().find(|d| {
                        d.name != name
                            && d.contracts.values().flat_map(|c| c.events.values()).any(|e| &e.name == event_name)
                    }) {
                        anyhow::bail!("plugin `{name}` event `{event_name}` conflicts with plugin `{}`", other.name);
                    }
                }
            }
        }
        let provide = list("provide")?;
        for service in &provide {
            if HOST_BUILTIN_SERVICES.contains(&service.as_str()) {
                anyhow::bail!("plugin `{name}` cannot provide reserved host service `{service}`");
            }
        }
        let flags = parse_flags(&name, table.get::<Value>("flags")?)?;
        let web = parse_web_def(&name, table.get::<Value>(mare_shared::WEB_ASSETS_DEF_FIELD)?)?;
        let tags = list("tags")?;
        let wires = list("wires")?;
        let providers = list("providers")?;
        let overrides = table.get::<Option<bool>>("override")?.unwrap_or(false);
        Ok(PluginDef {
            name,
            source: src.clone(),
            table,
            env,
            inject,
            contracts,
            provide,
            flags,
            web,
            tags,
            wires,
            providers,
            overrides,
            replaced: None,
            replaced_provide: vec![],
        })
    }

    /// Insert or replace a [`PluginDef`] inside an isolated test kernel and refresh its event catalog
    /// without mutating VM-global CLI flags.
    pub fn upsert_test_def(&self, def: PluginDef) {
        let mut st = self.st();
        let state = &mut *st;
        if let Some(slot) = state.defs.iter_mut().find(|d| d.name == def.name) {
            *slot = def;
        } else {
            state.defs.push(def);
        }
        Self::strip_conflicting_events(&state.contracts, &mut state.defs, &mut state.problems);
        Self::rebuild_event_catalog_locked(state);
    }

    /// Validate and register an inline Lua plugin definition table inside an isolated test kernel.
    pub fn register_test_def_table(&self, table: Table) -> mlua::Result<String> {
        let name: String = table
            .get::<Option<String>>("name")?
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| mlua::Error::runtime("plugin definition must have a non-empty string `name`"))?;
        let env = mare_shared::lua::isolated_env(self.lua())?;
        env.raw_set("require", self.make_require(&name)?)?;
        let src = PluginSource { name_hint: name.clone(), origin: Origin::User, path: None, code: None };
        let def = self
            .parse_def_table(table, &src, env, name.clone(), "<test>")
            .map_err(|e| mlua::Error::runtime(e.to_string()))?;
        self.upsert_test_def(def);
        Ok(name)
    }

    /// Synchronize declarative `PluginDef.flags` into the VM's shared [`mare_shared::flags::FlagStore`].
    pub fn sync_plugin_flags(&self) {
        let store = mare_shared::flags::FlagStore::of(self.lua());
        store.clear_source_prefix("plugin:");
        let specs: Vec<_> = self.st().defs.iter().flat_map(|d| d.flags.iter().cloned()).collect();
        for spec in specs {
            if let Ok(Some(warn)) = store.register(spec) {
                self.add_problem(warn);
            }
        }
    }

    /// Synchronize derived policy points, event log contracts, and plugin flags after `state.defs` changes.
    pub(crate) fn sync_defs_state(&self) {
        {
            let mut st = self.st();
            let state = &mut *st;
            Self::strip_conflicting_events(&state.contracts, &mut state.defs, &mut state.problems);
            Self::rebuild_event_catalog_locked(state);
        }
        crate::dispatch::policy_prims::sync_points(self);
        self.sync_event_log_contracts();
        self.sync_plugin_flags();
    }

    /// Register definitions. A later definition with the same name replaces an earlier one
    /// only with `override = true` ([`OverrideRefusal`]); the refused ones are recorded as
    /// problems and returned (a warning, not an essential-plugin failure).
    pub fn set_defs(&self, defs: Vec<PluginDef>) -> Vec<OverrideRefusal> {
        let mut out: Vec<PluginDef> = Vec::new();
        let (_, refused) = overrides::merge(&mut out, defs);
        {
            let mut st = self.st();
            st.defs = out;
            st.problems.extend(refused.iter().map(OverrideRefusal::message));
        }
        self.sync_defs_state();
        refused
    }

    /// Add definitions to the running set (same-name rule as [`Self::set_defs`]). Returns the
    /// names that replaced a definition (the caller reloads those, [`Self::reload`], and starts
    /// the new ones, [`Self::start`] skips active plugins) and the refusals. Used when a project
    /// becomes trusted mid-session.
    pub fn merge_defs(&self, defs: Vec<PluginDef>) -> (Vec<String>, Vec<OverrideRefusal>) {
        let (replaced, refused) = {
            let mut st = self.st();
            let state = &mut *st;
            let (replaced, refused) = overrides::merge(&mut state.defs, defs);
            state.problems.extend(refused.iter().map(OverrideRefusal::message));
            (replaced, refused)
        };
        self.sync_defs_state();
        (replaced, refused)
    }

    fn strip_conflicting_events(
        builtin: &std::collections::BTreeMap<String, std::sync::Arc<ServiceContract>>,
        defs: &mut [PluginDef],
        problems: &mut Vec<String>,
    ) {
        let mut owners: HashMap<String, String> = HashMap::new();
        for (svc, contract) in builtin {
            for spec in contract.events.values() {
                owners.insert(spec.name.clone(), format!("built-in contract `{svc}`"));
            }
        }
        for def in defs {
            let plugin_name = def.name.clone();
            for contract in def.contracts.values_mut() {
                contract.events.retain(|_, spec| {
                    if let Some(existing) = owners.get(&spec.name) {
                        problems
                            .push(format!("plugin `{plugin_name}` event `{}` conflicts with {existing}", spec.name));
                        false
                    } else {
                        owners.insert(spec.name.clone(), format!("plugin `{plugin_name}`"));
                        true
                    }
                });
            }
        }
    }

    pub fn def_names(&self) -> Vec<String> {
        self.st().defs.iter().map(|d| d.name.clone()).collect()
    }

    /// All loaded plugin definitions currently registered on this kernel.
    pub fn plugin_defs(&self) -> Vec<PluginDef> {
        self.st().defs.clone()
    }

    /// The registered definition named `name` (the one that runs or would run).
    pub fn def(&self, name: &str) -> Option<PluginDef> {
        self.st().defs.iter().find(|d| d.name == name).cloned()
    }

    /// Records what every definition read for the current config declares (the host calls it
    /// with all of them, before leaving out the turned-off ones).
    pub fn set_declared(&self, declared: Vec<Declared>) {
        self.st().declared = declared;
    }

    /// What plugin `name` declares: its registered definition, else the last definition read
    /// under that name (one turned off by the config).
    /// Every definition read under `name` for the current config, in precedence order (lowest
    /// source first), whatever the config or the same-name rule made of it.
    pub fn declared_sources(&self, name: &str) -> Vec<Declared> {
        self.st().declared.iter().filter(|d| d.name == name).cloned().collect()
    }

    /// All effective definitions picked by the same-name rule (turned-off slots included).
    pub fn effective_declared(&self) -> Vec<Declared> {
        self.st().declared.iter().filter(|d| d.effective).cloned().collect()
    }

    pub fn declared(&self, name: &str) -> Option<Declared> {
        let st = self.st();
        st.defs
            .iter()
            .find(|d| d.name == name)
            .map(PluginDef::declared)
            .or_else(|| st.declared.iter().rev().find(|d| d.name == name && d.effective).cloned())
    }

    /// The plugins whose definition declares `value` in `field` (`"tags"`, `"wires"`, or
    /// `"providers"`): registered definitions in definition order, then the turned-off ones.
    pub fn declaring(&self, field: &str, value: &str) -> Vec<String> {
        let has = |d: &Declared| match field {
            "tags" => d.tags.iter().any(|t| t == value),
            "wires" => d.wires.iter().any(|w| w == value),
            "providers" => d.providers.iter().any(|p| p == value),
            _ => false,
        };
        let st = self.st();
        let mut out: Vec<String> = st.defs.iter().map(PluginDef::declared).filter(&has).map(|d| d.name).collect();
        // a turned-off name counts by its effective definition (the one that would have run)
        let mut seen: Vec<&str> = st.defs.iter().map(|d| d.name.as_str()).collect();
        for d in st.declared.iter().rev().filter(|d| d.effective) {
            if !seen.contains(&d.name.as_str()) {
                seen.push(&d.name);
                if has(d) {
                    out.push(d.name.clone());
                }
            }
        }
        out
    }

    /// The plugins that replaced a same-name definition (`override = true`): (name, the origin
    /// they replaced), for `/plugins` and `mare doctor`.
    pub fn plugin_overrides(&self) -> Vec<(String, Origin)> {
        self.st().defs.iter().filter_map(|d| d.replaced.map(|o| (d.name.clone(), o))).collect()
    }

    /// Plugin list with status, for `/plugins`.
    pub fn plugin_status(&self) -> Vec<(String, String, bool)> {
        self.plugin_status_in(&self.plugin_tree().unwrap_or_default())
    }

    /// [`Self::plugin_status`] over a tree snapshot the caller already took (one dump for
    /// several readings).
    pub fn plugin_status_in(&self, tree: &[crate::PluginTreeRow]) -> Vec<(String, String, bool)> {
        let active = |name: &str| Self::active_in(tree, name);
        self.st().defs.iter().map(|d| (d.name.clone(), d.source.origin.as_str().to_string(), active(&d.name))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_origin_plugin_definition_cannot_declare_essential_contract() {
        let k = Kernel::new(mlua::Lua::new(), serde_json::json!({})).unwrap();
        let src = PluginSource {
            name_hint: "demo-builtin".into(),
            origin: Origin::Builtin,
            path: None,
            code: Some(
                br#"return {
                    name = "demo-builtin",
                    contracts = {
                        demo_svc = {
                            essential = true,
                            methods = {
                                ping = { doc = "ping", result = { type = "boolean" } },
                            },
                        },
                    },
                    apply = function() end,
                }"#,
            ),
        };
        let err = k.load_def(&src).err().expect("essential plugin contract must be rejected regardless of origin");
        assert!(err.to_string().contains("cannot declare essential service `demo_svc`"), "{err}");
    }
}
