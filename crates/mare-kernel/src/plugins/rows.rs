//! The row list the `loader` host plugin turns into the plugin tree. One row per merged definition
//! in load order, then one disabled row per definition the config turned off (known only by what it declares), so `--dump` lists them too.
//!
//! ```lua
//! { id, name, config, disabled = true|nil,
//!   origin, path, overrides } -- kept on the entry for --dump; cordis.loader ignores them
//! ```
//!
//! A refused definition is never a row: a row's id is its plugin name, and
//! the loader's group keys entries by id, so a refused row could stand in for the running one.
//! Refusals are a separate list ([`Kernel::refused_definitions`], `prims.refused()`).

use std::collections::{HashMap, HashSet};

use mlua::{Function, Lua, Table, Value};

use super::provide_check;
use crate::{Kernel, KernelInner};

impl Kernel {
    /// The rows for the current definitions and config. A definition refused by the static provide
    /// check ([`provide_check`]) is left out (it is in [`Self::refused_definitions`]); its conflict
    /// is recorded as a problem once.
    pub(crate) fn plugin_rows(&self) -> mlua::Result<Table> {
        crate::lua_env::modules::ModuleScan::scope(|| self.plugin_rows_scanned())
    }

    fn plugin_rows_scanned(&self) -> mlua::Result<Table> {
        let lua = self.lua();
        let (defs, declared) = {
            let st = self.st();
            (st.defs.clone(), st.declared.clone())
        };
        // refused statically: a second provider of a service, a member of an inject cycle
        let refused = provide_check::refusals(&defs);
        let existing_problems: HashSet<String> = self.problems().into_iter().collect();
        for (_, msg, _) in &refused {
            if !existing_problems.contains(msg) {
                self.add_problem(msg.clone());
            }
        }
        let refused_names: HashSet<&str> = refused.iter().map(|(name, _, _)| name.as_str()).collect();
        let cache = self.row_cache()?;
        let failed = mare_shared::lua::core::config::plugin_failed(lua)?;
        // `plugins.<name>.enabled = false` turns a definition off; at startup the host leaves it
        // out of the definitions, but a config reload keeps the definitions and only re-reads the
        // config, so the row is disabled here
        let config = self.config();
        let plugins_cfg = config.get(mare_shared::PLUGINS_KEY).and_then(|p| p.as_object());
        let off = |name: &str| {
            plugins_cfg.and_then(|p| p.get(name)).and_then(|p| p.get("enabled")).and_then(|e| e.as_bool())
                == Some(false)
        };
        let active_defs: Vec<&crate::PluginDef> =
            defs.iter().filter(|d| !refused_names.contains(d.name.as_str())).collect();
        let waves = topo_waves(&active_defs);
        let rows = lua.create_table()?;
        for (idx, d) in active_defs.iter().enumerate() {
            let row = lua.create_table()?;
            row.set("id", d.name.as_str())?;
            row.set("name", d.name.as_str())?;
            row.set("wave", waves[idx])?;
            // an invalid option fails this plugin only (the old path's apply did the same):
            // the row is not started, the failure is recorded once, the other rows go on
            let (config, print) = match self.row_state(d, &cache, failed.as_ref(), plugins_cfg) {
                Ok(state) => state,
                Err(e) => {
                    let msg = format!("plugin {} failed: {e}", d.name);
                    if !existing_problems.contains(&msg) {
                        tracing::warn!("{msg}");
                        self.add_problem(msg.clone());
                    }
                    row.set("origin", d.source.origin.as_str())?;
                    row.set("disabled", true)?;
                    row.set("config_error", e.to_string())?;
                    rows.push(row)?;
                    continue;
                }
            };
            row.set("config", config)?;
            row.set("origin", d.source.origin.as_str())?;
            if let Some(p) = &d.source.path {
                row.set("path", p.display().to_string())?;
            }
            if let Some(o) = d.replaced {
                row.set("overrides", o.as_str())?;
            }
            if off(&d.name) {
                row.set("disabled", true)?;
            }
            row.set("fingerprint", print)?;
            rows.push(row)?;
        }
        // turned off by the config (`plugins.<name>.enabled = false`, a builtins profile): the
        // last definition read under each name, once
        let mut seen: HashSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        for d in declared.iter().rev() {
            if !seen.insert(d.name.as_str()) {
                continue;
            }
            let row = lua.create_table()?;
            row.set("id", d.name.as_str())?;
            row.set("name", d.name.as_str())?;
            row.set("disabled", true)?;
            rows.push(row)?;
        }
        Ok(rows)
    }
}

impl Kernel {
    /// The definitions refused by the static provide check, for `--dump` / `/plugins`:
    /// `{ { id, origin, path, refused }, ... }`. Same-name refusals (`override`) are not
    /// definitions in the merged set; they stay in [`Kernel::problems`].
    pub(crate) fn refused_definitions(&self) -> mlua::Result<Table> {
        // read under the borrow, copying out only the strings (every loader dump asks: cloning the
        // definitions, contracts and Lua tables included, was most of a dump's cost)
        let refused: Vec<(String, String, Option<String>, String)> = {
            let st = self.st();
            let by_name: HashMap<&str, &crate::PluginDef> = st.defs.iter().map(|d| (d.name.as_str(), d)).collect();
            let mut refused = vec![];
            for (name, _, reason) in provide_check::refusals(&st.defs) {
                let Some(d) = by_name.get(name.as_str()).copied() else { continue };
                refused.push((d.name.clone(), d.source.origin.as_str().to_string(), path_of(&d.source.path), reason));
            }
            // a definition refused by the same-name rule (no `override = true`): declared but not
            // the effective one while a same-name definition runs. The kernel's problem carries the
            // full warning; `--dump` / `mare plugin tree` list it too
            for d in st.declared.iter().filter(|d| !d.effective) {
                let Some(kept) = by_name.get(d.name.as_str()).copied() else { continue };
                if kept.source.path.is_some() && kept.source.path == d.path {
                    continue;
                }
                let reason = format!(
                    "name collides with the {} `{}`; add `override = true` to replace it, or rename it",
                    if kept.source.origin.as_str() == "builtin" { "built-in" } else { kept.source.origin.as_str() },
                    d.name
                );
                refused.push((d.name.clone(), d.origin.as_str().to_string(), path_of(&d.path), reason));
            }
            refused
        };
        let lua = self.lua();
        let out = lua.create_table()?;
        for (id, origin, path, reason) in refused {
            let item = lua.create_table()?;
            item.set("id", id)?;
            item.set("origin", origin)?;
            if let Some(path) = path {
                item.set("path", path)?;
            }
            item.set("refused", reason)?;
            out.push(item)?;
        }
        Ok(out)
    }
}

fn path_of(path: &Option<std::path::PathBuf>) -> Option<String> {
    path.as_ref().map(|p| p.display().to_string())
}

impl Kernel {
    /// The cordis tree follows the current rows (boot.lua `booted.reload()` → `loader:apply_rows`):
    /// rows whose config changed update their fiber with the new config, rows turned off are
    /// disposed, rows turned on start, the others are left alone. `/reload config` uses it in
    /// tree mode; a per-name reload would replace the definition and
    /// keep the fiber's old config.
    pub async fn apply_rows_report(&self) -> anyhow::Result<crate::ReloadReport> {
        // the same transaction as a reload: a new generation, tool calls wait while it runs, and
        // it stays "incomplete" if it is cancelled midway
        const TARGET: &str = "<rows>";
        self.0.reload_incomplete.lock().unwrap().insert(TARGET.to_string());
        self.0.reload_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let _hold = self.reload_hold();
        let result = self.apply_rows_inner().await;
        self.0.reload_incomplete.lock().unwrap().remove(TARGET);
        result
    }

    async fn apply_rows_inner(&self) -> anyhow::Result<crate::ReloadReport> {
        let booted = self.cordis_boot().map_err(|e| anyhow::anyhow!("cordis boot failed: {e}"))?;
        let reload: Function = booted.get("reload")?;
        let report: Table = mare_shared::lua::call_async(self.lua(), &reload, mlua::Value::Nil).await?;
        let applied = report.get::<Option<Vec<String>>>("applied")?.unwrap_or_default();
        // waiting for a service nobody provides (Cordis PENDING): reported, not a failure
        let pending = report.get::<Option<Vec<String>>>("pending")?.unwrap_or_default();
        // a plugin whose new definition failed runs on its previous one again: its definition
        // and row go back too, so the next `/reload config` sees the change and applies it again
        // (otherwise the new definition's fingerprint would read as "unchanged")
        let prior = std::mem::take(&mut self.st().rows_prior);
        let cache = self.row_cache().ok();
        let restored = report.get::<Option<Vec<String>>>("restored")?.unwrap_or_default();
        for n in &restored {
            if let Some(old) = prior.get(n)
                && let Some(slot) = self.st().defs.iter_mut().find(|x| x.name == *n)
            {
                *slot = old.clone();
            }
            if let Some(cache) = &cache {
                let _ = cache.raw_remove(n.as_str());
            }
            self.add_problem(format!("plugin {n} re-enabled on its previous definition"));
        }
        self.sync_defs_state();
        let mut failures = vec![];
        for msg in report.get::<Option<Vec<String>>>("failures")?.unwrap_or_default() {
            self.add_problem(msg.clone());
            failures.push(msg);
        }
        Ok(crate::ReloadReport { applied, failures, restored, pending })
    }
}

/// `prims.refused()` for boot.lua: [`Kernel::refused_definitions`].
pub(crate) fn refused_prim(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> mlua::Result<Function> {
    lua.create_function(move |_, ()| {
        Kernel::from_weak(&kernel).ok_or_else(|| mlua::Error::runtime("kernel is gone"))?.refused_definitions()
    })
}

/// `prims.rows()` for boot.lua: the current rows of this kernel.
pub(crate) fn rows_prim(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> mlua::Result<Function> {
    lua.create_function(move |_, ()| {
        Kernel::from_weak(&kernel).ok_or_else(|| mlua::Error::runtime("kernel is gone"))?.plugin_rows()
    })
}

/// What a full reload compares to tell whether a plugin's definition changed: its origin and
/// source (the file's bytes; embedded code by address and size) and the modules its `require`
/// can load ([`Kernel::hash_modules`]). A row whose fingerprint changed is rebuilt by
/// `loader:apply_rows`; the others are left alone.
fn fingerprint(d: &crate::PluginDef) -> String {
    use std::hash::{Hash, Hasher};
    // Embedded built-in plugins have immutable code and embedded modules for the life of the
    // process; cache their fingerprint by (code ptr, code len, name) so repeated startups /
    // reloads skip rehashing.
    if d.source.origin == crate::Origin::Builtin
        && d.source.path.is_none()
        && let Some(code) = d.source.code
    {
        type EmbeddedFingerprintKey = (usize, usize, String);
        static EMBEDDED_FINGERPRINTS: std::sync::OnceLock<std::sync::Mutex<HashMap<EmbeddedFingerprintKey, String>>> =
            std::sync::OnceLock::new();
        let cache = EMBEDDED_FINGERPRINTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
        let key = (code.as_ptr() as usize, code.len(), d.name.clone());
        if let Some(hit) = cache.lock().unwrap().get(&key) {
            return hit.clone();
        }
        let mut h = std::collections::hash_map::DefaultHasher::new();
        d.source.origin.as_str().hash(&mut h);
        d.source.path.hash(&mut h);
        Some((code.as_ptr() as usize, code.len())).hash(&mut h);
        Kernel::hash_modules(d, &mut h);
        let fp = format!("{:016x}", h.finish());
        cache.lock().unwrap().insert(key, fp.clone());
        return fp;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.source.origin.as_str().hash(&mut h);
    d.source.path.hash(&mut h);
    match d.source.file() {
        Some(f) => std::fs::read(f).unwrap_or_default().hash(&mut h),
        None => d.source.code.map(|c| (c.as_ptr() as usize, c.len())).hash(&mut h),
    }
    Kernel::hash_modules(d, &mut h);
    format!("{:016x}", h.finish())
}

/// Compute static `provide -> inject` startup wave indices for `defs` (Kahn's level-by-level
/// topological layering) without reordering `defs`. Any leftover definitions (cycles, if not
/// already refused) receive the final wave index so Cordis PENDING semantics remain unchanged.
fn topo_waves(defs: &[&crate::PluginDef]) -> Vec<u32> {
    let n = defs.len();
    if n <= 1 {
        return vec![0; n];
    }
    let mut provider: HashMap<String, usize> = HashMap::with_capacity(n);
    for (idx, d) in defs.iter().enumerate() {
        for svc in d.services() {
            provider.entry(svc).or_insert(idx);
        }
    }
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut in_degree: Vec<usize> = vec![0; n];
    for (idx, d) in defs.iter().enumerate() {
        let mut seen_deps: HashSet<usize> = HashSet::new();
        for dep_svc in &d.inject {
            if let Some(&prov_idx) = provider.get(dep_svc.as_str())
                && prov_idx != idx
                && seen_deps.insert(prov_idx)
            {
                dependents[prov_idx].push(idx);
                in_degree[idx] += 1;
            }
        }
    }
    let mut current: Vec<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
    let mut waves = vec![0u32; n];
    let mut placed = vec![false; n];
    let mut wave: u32 = 0;
    while !current.is_empty() {
        let mut next = Vec::new();
        for idx in current {
            placed[idx] = true;
            waves[idx] = wave;
            for &dep in &dependents[idx] {
                in_degree[dep] -= 1;
                if in_degree[dep] == 0 {
                    next.push(dep);
                }
            }
        }
        next.sort_unstable();
        current = next;
        wave = wave.saturating_add(1);
    }
    for (idx, is_placed) in placed.into_iter().enumerate() {
        if !is_placed {
            waves[idx] = wave;
        }
    }
    waves
}

/// Per plugin, what the last rows gave the loader: `{ json, config, fingerprint, def, held }`
/// (Lua registry, `mare.rows`).
const ROW_CACHE: &str = "mare.rows";
/// Shared empty config table for schema-less rows without user options (`mare.empty_row_config`).
const EMPTY_ROW_CONFIG: &str = "mare.empty_row_config";

/// Serialize a JSON value with recursively sorted object keys so `serde_json`'s `preserve_order`
/// feature and Lua `pairs()` hash iteration order cannot cause spurious config mismatches.
fn canonical_json_string(val: &serde_json::Value) -> mlua::Result<String> {
    fn canonicalize(val: &serde_json::Value) -> serde_json::Value {
        match val {
            serde_json::Value::Object(map) => {
                let mut entries: Vec<(&String, &serde_json::Value)> = map.iter().collect();
                entries.sort_unstable_by_key(|(a, _)| *a);
                let mut out = serde_json::Map::with_capacity(entries.len());
                for (k, v) in entries {
                    out.insert(k.clone(), canonicalize(v));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(arr) => serde_json::Value::Array(arr.iter().map(canonicalize).collect()),
            other => other.clone(),
        }
    }
    if val.as_object().is_some_and(|m| m.is_empty()) {
        return Ok("{}".to_string());
    }
    serde_json::to_string(&canonicalize(val)).map_err(mlua::Error::external)
}

/// Resolve the JSON string of a prior `ROW_CACHE` entry, lazily computing it from
/// `default_schema` when the entry was created on the default-only fast path.
fn resolve_prev_json(lua: &Lua, prev: &Table) -> mlua::Result<Option<String>> {
    if let Some(s) = prev.get::<Option<String>>("json")? {
        return Ok(Some(s));
    }
    let Some(old_schema) = prev.get::<Option<Table>>("default_schema")? else {
        return Ok(None);
    };
    let old_defaults = Value::Table(mare_shared::config_schema::defaults(lua, &old_schema)?);
    let old_val = mare_shared::lua::config_to_json(lua, old_defaults)?;
    Ok(Some(canonical_json_string(&old_val)?))
}

/// Serialize a merged plugin config JSON and return `(json, lua_table)`, reusing `prev.config`
/// verbatim when the merged JSON matches the previous entry's JSON.
fn materialize_merged_row(lua: &Lua, prev: Option<&Table>, merged: serde_json::Value) -> mlua::Result<(String, Table)> {
    let empty = merged.as_object().is_some_and(|m| m.is_empty());
    let json = canonical_json_string(&merged)?;
    let prev_json = match prev {
        Some(p) => resolve_prev_json(lua, p)?,
        None => None,
    };
    let config: Table = match (prev, prev_json.as_deref()) {
        (Some(p), Some(pj)) if pj == json.as_str() => p.get("config")?,
        _ if empty => lua.create_table()?,
        _ => match mare_shared::lua::config_to_lua(lua, &merged)? {
            Value::Table(t) => t,
            _ => lua.create_table()?,
        },
    };
    Ok((json, config))
}

impl Kernel {
    fn row_cache(&self) -> mlua::Result<Table> {
        let lua = self.lua();
        if let Ok(t) = lua.named_registry_value::<Table>(ROW_CACHE) {
            return Ok(t);
        }
        let t = lua.create_table()?;
        lua.set_named_registry_value(ROW_CACHE, &t)?;
        Ok(t)
    }

    fn empty_row_config(&self) -> mlua::Result<Table> {
        let lua = self.lua();
        if let Ok(t) = lua.named_registry_value::<Table>(EMPTY_ROW_CONFIG) {
            return Ok(t);
        }
        let t = lua.create_table()?;
        lua.set_named_registry_value(EMPTY_ROW_CONFIG, &t)?;
        Ok(t)
    }

    /// A row's config and fingerprint. The config stays the same table while the plugin's merged
    /// options (JSON, what `/reload config` compares) are unchanged: cordis.loader compares
    /// configs with a strict deepEqual, under which a table with a metatable never equals a fresh
    /// copy, so a rebuilt table would restart the plugin. A plugin whose new definition failed to
    /// load in a config reload ([`Self::prepare_changed_defs`]) is held: its row stays what the
    /// loader runs (old config, old fingerprint) while it still has that definition.
    fn row_state(
        &self,
        d: &crate::PluginDef,
        cache: &Table,
        failed: Option<&Table>,
        plugins_cfg: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> mlua::Result<(Table, String)> {
        let prev: Option<Table> = cache.get(d.name.as_str())?;
        let def_ptr = d.table.to_pointer() as usize as i64;
        if let Some(prev) = &prev
            && prev.get::<Option<bool>>("held")?.unwrap_or(false)
        {
            if prev.get::<i64>("def")? == def_ptr {
                return Ok((prev.get("config")?, prev.get("fingerprint")?));
            }
            prev.set("held", false)?;
        }
        let lua = self.lua();
        let has_fail = failed.and_then(|f| f.get::<Option<String>>(d.name.as_str()).ok().flatten()).is_some();
        let user_override = plugins_cfg.and_then(|p| p.get(&d.name));
        let raw_schema = d.table.raw_get::<Value>("config_schema")?;
        // Fast path 1: no schema, no user config, and no failed option -> share one empty `{}`
        // table without allocating JSON or a per-row Lua table.
        // Fast path 2: schema defaults only (no user override, no failure) -> build the Lua
        // defaults table directly via `config_schema::defaults` without round-tripping through
        // `config_to_json` -> `serde_json::to_string` -> `config_to_lua`. The schema reference is
        // retained on the cache entry so a later reload can compare lazily if overrides appear or
        // the definition changes.
        let (json, config, default_schema): (Option<String>, Table, Option<Table>) =
            if !has_fail && user_override.is_none() {
                match raw_schema {
                    Value::Table(schema) => match &prev {
                        None => (None, mare_shared::config_schema::defaults(lua, &schema)?, Some(schema)),
                        Some(p) if p.get::<Option<String>>("json")?.is_none() && p.get::<i64>("def")? == def_ptr => {
                            (None, p.get("config")?, Some(schema))
                        }
                        Some(_) => {
                            let merged = self.plugin_config_json_with(d, failed, plugins_cfg)?;
                            let (j, c) = materialize_merged_row(lua, prev.as_ref(), merged)?;
                            (Some(j), c, None)
                        }
                    },
                    _ => (Some("{}".to_string()), self.empty_row_config()?, None),
                }
            } else {
                let merged = self.plugin_config_json_with(d, failed, plugins_cfg)?;
                let (j, c) = materialize_merged_row(lua, prev.as_ref(), merged)?;
                (Some(j), c, None)
            };
        let base_print = fingerprint(d);
        let prev_base: Option<String> =
            prev.as_ref().and_then(|p| p.get::<Option<String>>("base_fingerprint").ok().flatten());
        let prev_print: Option<String> =
            prev.as_ref().and_then(|p| p.get::<Option<String>>("fingerprint").ok().flatten());
        let dep_prints: Option<Table> = prev.as_ref().and_then(|p| p.get::<Option<Table>>("dep_prints").ok().flatten());
        let mut dep_changed = false;
        let mut changed_hashes = Vec::new();
        if let Some(deps) = &dep_prints {
            for pair in deps.clone().pairs::<String, String>() {
                let (provider, saved_hash) = pair?;
                let cur_hash = self.provider_modules_hash(&provider);
                if cur_hash != saved_hash {
                    dep_changed = true;
                    changed_hashes.push((provider, cur_hash));
                }
            }
        }
        let own_changed = prev_base.as_deref() != Some(base_print.as_str());
        let print = if !own_changed && !dep_changed {
            // Reuse the last emitted fingerprint verbatim so a dependent plugin whose provider
            // reloaded on an earlier pass does not restart a second time on the next no-op reload.
            prev_print.unwrap_or_else(|| base_print.clone())
        } else if dep_changed {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            base_print.hash(&mut h);
            changed_hashes.sort();
            changed_hashes.hash(&mut h);
            let mut mixed = format!("{:016x}", h.finish());
            if prev_print.as_deref() == Some(mixed.as_str()) {
                1u8.hash(&mut h);
                mixed = format!("{:016x}", h.finish());
            }
            mixed
        } else {
            base_print.clone()
        };
        let entry = lua.create_table()?;
        if let Some(json) = json {
            entry.set("json", json)?;
        }
        if let Some(schema) = default_schema {
            entry.set("default_schema", schema)?;
        }
        entry.set("config", &config)?;
        entry.set("base_fingerprint", base_print.as_str())?;
        entry.set("fingerprint", print.as_str())?;
        entry.set("def", def_ptr)?;
        if let Some(deps) = dep_prints {
            entry.set("dep_prints", deps)?;
        }
        cache.set(d.name.as_str(), entry)?;
        Ok((config, print))
    }

    pub(crate) fn provider_modules_hash(&self, provider: &str) -> String {
        use std::hash::Hasher;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        if provider == crate::lua_env::modules::CORE_OWNER {
            self.hash_core_modules(&mut h);
        } else if let Some(def) = self.def(provider) {
            Self::hash_modules(&def, &mut h);
        }
        format!("{:016x}", h.finish())
    }

    fn provider_loaded_modules_hash(&self, provider: &str) -> String {
        self.cached_provider_modules_hash(provider).unwrap_or_else(|| self.provider_modules_hash(provider))
    }

    /// Snapshot module hashes of every provider `name` required during its latest apply into
    /// `ROW_CACHE[name].dep_prints`, so future row-driven reloads restart `name` if a provider's
    /// modules change without falsely marking the first reload dirty.
    pub(crate) fn record_row_dep_prints(&self, name: &str) {
        self.mark_module_active(name);
        let deps = self.module_deps(name);
        let Ok(cache) = self.row_cache() else { return };
        let Ok(Some(entry)) = cache.get::<Option<Table>>(name) else { return };
        if deps.is_empty() {
            let _ = entry.set("dep_prints", Value::Nil);
            return;
        }
        let Ok(dep_table) = self.lua().create_table() else { return };
        for provider in deps {
            let hash = self.provider_loaded_modules_hash(&provider);
            let _ = dep_table.set(provider, hash);
        }
        let _ = entry.set("dep_prints", dep_table);
    }

    /// Incrementally record a single provider's loaded modules hash into
    /// `ROW_CACHE[caller].dep_prints` when an active plugin lazily requires a cross-owner module
    /// after `apply()` has completed. Preserves any existing snapshot for `provider`.
    pub(crate) fn record_row_dep_print(&self, caller: &str, provider: &str) {
        let Ok(cache) = self.row_cache() else { return };
        let Ok(Some(entry)) = cache.get::<Option<Table>>(caller) else { return };
        let dep_table = match entry.get::<Option<Table>>("dep_prints") {
            Ok(Some(t)) => t,
            _ => {
                let Ok(t) = self.lua().create_table() else { return };
                if entry.set("dep_prints", &t).is_err() {
                    return;
                }
                t
            }
        };
        if dep_table.get::<Option<String>>(provider).ok().flatten().is_some() {
            return;
        }
        let hash = self.provider_loaded_modules_hash(provider);
        let _ = dep_table.set(provider, hash);
    }

    /// The `cfg` table the running plugin `name` received (recorded by the loader's mark_active),
    /// which a runtime write of an `apply = "live"`/`"next_op"` option updates in
    /// place; `None` when it is not running.
    pub fn plugin_cfg(&self, name: &str) -> Option<Table> {
        if !self.plugin_active(name) {
            return None;
        }
        self.st().cfgs.get(name).cloned()
    }

    /// `/reload config` on the cordis tree, step one: the changed plugins' definitions are read
    /// again (as the per-plugin reload did). A definition that loads replaces the registered one
    /// (its fingerprint changes when its file did, so apply_rows rebuilds it); one that fails is
    /// reported, its plugin keeps running as it is (its row is held), and it is retried by the
    /// next config reload. Returns `(plugin, message)` per failure.
    pub async fn prepare_changed_defs(&self, names: &[String]) -> Vec<(String, String)> {
        let mut failures = vec![];
        let cache = self.row_cache().ok();
        for n in names {
            let Some(d) = self.def(n) else { continue };
            let loaded = self.load_def(&d.source).and_then(|nd| {
                if nd.name != d.name {
                    anyhow::bail!("plugin {} renamed itself to `{}` on reload", d.name, nd.name);
                }
                match nd.table.get::<mlua::Value>("apply")? {
                    mlua::Value::Function(_) | mlua::Value::Nil => Ok(nd),
                    _ => anyhow::bail!("plugin {}: `apply` must be a function", d.name),
                }
            });
            match loaded {
                Ok(mut nd) => {
                    nd.replaced = d.replaced;
                    nd.replaced_provide = d.replaced_provide.clone();
                    let mut st = self.st();
                    if let Some(slot) = st.defs.iter_mut().find(|x| x.name == *n) {
                        let old = std::mem::replace(slot, nd);
                        st.rows_prior.insert(n.clone(), old);
                    }
                }
                Err(e) => {
                    let message = format!("{e} (keeping the running definition)");
                    self.add_problem(message.clone());
                    failures.push((n.clone(), message));
                    if let Some(prev) = cache.as_ref().and_then(|c| c.get::<Option<Table>>(n.as_str()).ok().flatten()) {
                        let _ = prev.set("held", true);
                    }
                }
            }
        }
        self.sync_plugin_flags();
        failures
    }
}

/// `prims.profile_record(label, ms)` for the loader host plugin: one entry of the startup profile
/// (`/profile`, `--startuptime`), as the old path records `plugin <name> (<origin>)` per apply.
pub(crate) fn profile_prim(lua: &Lua) -> mlua::Result<Function> {
    lua.create_function(|_, (label, ms): (String, f64)| {
        mare_shared::profile::record(&label, std::time::Duration::from_secs_f64(ms.max(0.0) / 1000.0));
        Ok(())
    })
}

/// `prims.measure_apply(fn, ...)` for `component.lua`: runs a plugin's `apply` inside a timed
/// `ScopedFuture`, returning `(busy_ms, yielded, req_count, res...)` where `busy_ms` is the sum of
/// this `apply`'s own poll durations (excluding time spent yielded while other tasks ran),
/// `yielded` is whether any poll returned `Pending`, and `req_count` is the count of modules added
/// to `package.loaded` or any plugin's owner partition during its own poll slices.
pub(crate) fn measure_apply_prim(lua: &Lua, kernel: std::sync::Weak<crate::KernelInner>) -> mlua::Result<Function> {
    struct MeasuredApplyFuture {
        kernel: std::sync::Weak<crate::KernelInner>,
        fut: std::pin::Pin<Box<dyn std::future::Future<Output = mlua::Result<mlua::MultiValue>>>>,
        busy: std::time::Duration,
        yielded: bool,
        requires: u32,
    }
    impl std::future::Future for MeasuredApplyFuture {
        type Output = mlua::Result<(f64, bool, u32, mlua::MultiValue)>;
        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
            let before = Kernel::from_weak(&self.kernel).map_or(0, |k| k.loaded_module_count());
            let t0 = std::time::Instant::now();
            let res = self.fut.as_mut().poll(cx);
            self.busy += t0.elapsed();
            let after = Kernel::from_weak(&self.kernel).map_or(before, |k| k.loaded_module_count());
            self.requires = self.requires.saturating_add(after.saturating_sub(before));
            match res {
                std::task::Poll::Pending => {
                    self.yielded = true;
                    std::task::Poll::Pending
                }
                std::task::Poll::Ready(out) => std::task::Poll::Ready(
                    out.map(|vals| (self.busy.as_secs_f64() * 1000.0, self.yielded, self.requires, vals)),
                ),
            }
        }
    }
    lua.create_async_function(move |lua, (f, args): (Function, mlua::MultiValue)| {
        let kernel = kernel.clone();
        async move {
            let context = mare_shared::exec_ctx::current(&lua);
            let fut = mare_shared::exec_ctx::scoped_call(&lua, context, f, args)?;
            let (busy_ms, yielded, requires, vals) = MeasuredApplyFuture {
                kernel,
                fut: Box::pin(fut),
                busy: std::time::Duration::ZERO,
                yielded: false,
                requires: 0,
            }
            .await?;
            let mut out = mlua::MultiValue::with_capacity(vals.len() + 3);
            out.push_back(Value::Number(busy_ms));
            out.push_back(Value::Boolean(yielded));
            out.push_back(Value::Integer(i64::from(requires)));
            for v in vals {
                out.push_back(v);
            }
            Ok(out)
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::{Kernel, Origin, PluginSource};
    use mlua::{Lua, Table};

    fn load_plugin(k: &Kernel, name: &str, code: &'static str) {
        let src =
            PluginSource { name_hint: name.to_string(), origin: Origin::User, path: None, code: Some(code.as_bytes()) };
        let def = k.load_def(&src).unwrap();
        k.st().defs.push(def);
    }

    #[test]
    fn plugin_rows_reuses_unchanged_config_tables_and_merges_schema_defaults() {
        let cfg = serde_json::json!({
            "plugins": {
                "with-schema": { "mode": "custom" },
                "plain": { "flag": true }
            }
        });
        let k = Kernel::new(Lua::new(), cfg).unwrap();
        load_plugin(
            &k,
            "with-schema",
            r#"return {
                name = "with-schema",
                config_schema = {
                    mode = { type = "string", default = "auto" },
                    count = { type = "integer", default = 3 },
                },
            }"#,
        );
        load_plugin(&k, "plain", r#"return { name = "plain" }"#);
        load_plugin(&k, "empty", r#"return { name = "empty" }"#);

        let rows1 = k.plugin_rows().unwrap();
        let row1: Table = rows1.get(1).unwrap();
        let row2: Table = rows1.get(2).unwrap();
        let row3: Table = rows1.get(3).unwrap();
        let c1: Table = row1.get("config").unwrap();
        let c2: Table = row2.get("config").unwrap();
        let c3: Table = row3.get("config").unwrap();
        assert_eq!(c1.get::<String>("mode").unwrap(), "custom");
        assert_eq!(c1.get::<i64>("count").unwrap(), 3);
        assert!(c2.get::<bool>("flag").unwrap());
        assert_eq!(c3.raw_len(), 0);

        let rows2 = k.plugin_rows().unwrap();
        let r1_again: Table = rows2.get(1).unwrap();
        let c1_again: Table = r1_again.get("config").unwrap();
        assert_eq!(c1.to_pointer(), c1_again.to_pointer(), "unchanged config keeps table identity");

        k.set_config(serde_json::json!({
            "plugins": {
                "with-schema": { "mode": "updated" },
                "plain": { "flag": true }
            }
        }));
        let rows3 = k.plugin_rows().unwrap();
        let r1_upd: Table = rows3.get(1).unwrap();
        let c1_upd: Table = r1_upd.get("config").unwrap();
        assert_ne!(c1.to_pointer(), c1_upd.to_pointer(), "changed config creates updated table");
        assert_eq!(c1_upd.get::<String>("mode").unwrap(), "updated");
        assert_eq!(c1_upd.get::<i64>("count").unwrap(), 3);
    }

    #[test]
    fn plugin_rows_assigns_topological_waves_in_load_order_and_shares_empty_config() {
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        // Register consumers before providers: rows stay in load order while `wave` encodes
        // static `provide -> inject` layers.
        load_plugin(&k, "consumer", r#"return { name = "consumer", inject = { "svc_a", "svc_b" } }"#);
        load_plugin(&k, "mid", r#"return { name = "mid", inject = { "svc_a" }, provide = { "svc_b" } }"#);
        load_plugin(&k, "provider", r#"return { name = "provider", provide = { "svc_a" } }"#);
        load_plugin(&k, "empty_a", r#"return { name = "empty_a" }"#);
        load_plugin(&k, "empty_b", r#"return { name = "empty_b" }"#);

        let rows = k.plugin_rows().unwrap();
        let mut order = Vec::new();
        let mut waves = std::collections::HashMap::new();
        let mut configs = std::collections::HashMap::new();
        for row in rows.sequence_values::<Table>() {
            let row = row.unwrap();
            let id: String = row.get("id").unwrap();
            let wave: Option<u32> = row.get("wave").unwrap();
            let cfg: Table = row.get("config").unwrap();
            order.push(id.clone());
            waves.insert(id.clone(), wave);
            configs.insert(id, cfg.to_pointer());
        }
        assert_eq!(order, vec!["consumer", "mid", "provider", "empty_a", "empty_b"], "rows must keep load order");
        assert!(waves["provider"] < waves["mid"], "provider wave must precede mid wave: {waves:?}");
        assert!(waves["mid"] < waves["consumer"], "mid wave must precede consumer wave: {waves:?}");
        assert_eq!(
            configs["empty_a"], configs["empty_b"],
            "schema-less rows without user config must share the empty config table"
        );
    }

    #[test]
    fn measure_apply_prim_is_omitted_unless_startup_profile_enabled() {
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let prims = crate::lua_env::cordis_host::prims(k.lua(), std::sync::Arc::downgrade(&k.0)).unwrap();
        assert!(
            prims.get::<mlua::Value>("measure_apply").unwrap().is_nil(),
            "measure_apply must be omitted on normal startup"
        );
        assert!(
            prims.get::<mlua::Value>("measure_third_party_apply").unwrap().is_nil(),
            "third-party plugins must not have a separate measurement wrapper"
        );
        assert!(
            matches!(prims.get::<mlua::Value>("poll_count").unwrap(), mlua::Value::Function(_)),
            "poll_count must be exposed for O(1) yield detection"
        );
        k.set_measure_apply(true);
        let prims_on = crate::lua_env::cordis_host::prims(k.lua(), std::sync::Arc::downgrade(&k.0)).unwrap();
        assert!(
            matches!(prims_on.get::<mlua::Value>("measure_apply").unwrap(), mlua::Value::Function(_)),
            "measure_apply must be installed when startup profiling is enabled on the kernel"
        );
    }

    #[tokio::test]
    async fn kernel_start_dumps_plugin_tree_at_most_once() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let mare = k.lua().create_table().unwrap();
                let task = k.lua().create_table().unwrap();
                task.set(
                    "token",
                    k.lua()
                        .create_function(|_, ()| {
                            Ok(mare_shared::lua::Token(tokio_util::sync::CancellationToken::new()))
                        })
                        .unwrap(),
                )
                .unwrap();
                task.set("clock", k.lua().create_function(|_, ()| Ok(0.0_f64)).unwrap()).unwrap();
                mare.set("task", task).unwrap();
                mare.set("log", k.lua().create_function(|_, _: mlua::MultiValue| Ok(())).unwrap()).unwrap();
                k.lua().globals().set("mare", mare).unwrap();
                load_plugin(&k, "alpha", "return { name = 'alpha', apply = function() end }");
                load_plugin(&k, "beta", "return { name = 'beta', inject = { 'missing_svc' }, apply = function() end }");
                let booted = k.cordis_boot().unwrap();
                let loader_fn: mlua::Function = booted.get("loader").unwrap();
                let loader: Table = mare_shared::lua::call_async(k.lua(), &loader_fn, ()).await.unwrap();
                k.lua().globals().set("_START_LOADER", loader).unwrap();
                k.lua()
                    .load(
                        r#"
                        _START_DUMP_CALLS = 0
                        local orig = _START_LOADER.dump
                        _START_LOADER.dump = function(self, ...)
                            _START_DUMP_CALLS = _START_DUMP_CALLS + 1
                            return orig(self, ...)
                        end
                        "#,
                    )
                    .exec()
                    .unwrap();

                k.start().await.unwrap();

                let calls: usize = k.lua().globals().get("_START_DUMP_CALLS").unwrap();
                assert_eq!(calls, 1, "Kernel::start() must dump the plugin tree exactly once, got {calls}");
                assert!(
                    k.problems().iter().any(|p| p.contains("missing_svc")),
                    "pending plugin problem must still be reported: {:?}",
                    k.problems()
                );
            })
            .await;
    }

    #[tokio::test]
    async fn measure_apply_tracks_yields_and_requires_and_prepare_changed_defs_validates_reload() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let lua = k.lua();
                let measure = super::measure_apply_prim(lua, k.weak()).unwrap();
                lua.globals().set("measure_apply", measure).unwrap();
                lua.globals()
                    .set(
                        "yield_once",
                        lua.create_async_function(|_, ()| async move {
                            tokio::task::yield_now().await;
                            Ok(())
                        })
                        .unwrap(),
                    )
                    .unwrap();
                let (busy_ms, yielded, reqs, ret): (f64, bool, i64, String) = lua
                    .load(
                        r#"
                        return measure_apply(function(tag)
                            yield_once()
                            package.loaded["measured_test_mod"] = true
                            return "ok:" .. tag
                        end, "v1")
                        "#,
                    )
                    .eval_async()
                    .await
                    .unwrap();
                assert!(busy_ms >= 0.0);
                assert!(yielded);
                assert!(reqs >= 1);
                assert_eq!(ret, "ok:v1");

                // prepare_changed_defs rejects a plugin that renames itself or returns a non-function apply.
                let tmp = std::env::temp_dir().join(format!("mare-rows-reload-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&tmp);
                std::fs::create_dir_all(&tmp).unwrap();
                let p_rename = tmp.join("ren.lua");
                let p_bad = tmp.join("badapply.lua");
                std::fs::write(&p_rename, "return { name = 'ren', apply = function() end }\n").unwrap();
                std::fs::write(&p_bad, "return { name = 'badapply', apply = function() end }\n").unwrap();

                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                for (name, path) in [("ren", &p_rename), ("badapply", &p_bad)] {
                    let src = PluginSource {
                        name_hint: name.to_string(),
                        origin: Origin::User,
                        path: Some(path.clone()),
                        code: None,
                    };
                    let def = k.load_def(&src).unwrap();
                    k.st().defs.push(def);
                }
                std::fs::write(&p_rename, "return { name = 'other_name', apply = function() end }\n").unwrap();
                std::fs::write(&p_bad, "return { name = 'badapply', apply = 123 }\n").unwrap();
                let errs = k.prepare_changed_defs(&["ren".into(), "badapply".into()]).await;
                let _ = std::fs::remove_dir_all(&tmp);
                assert_eq!(errs.len(), 2, "{errs:?}");
                assert!(errs[0].1.contains("renamed itself"), "{errs:?}");
                assert!(errs[1].1.contains("must be a function"), "{errs:?}");
            })
            .await;
    }

    #[test]
    fn default_only_row_matches_json_roundtrip_and_reloads_cleanly() {
        use mlua::{Function, LuaSerdeExt, Value};
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let lua = k.lua();
        load_plugin(
            &k,
            "rich_defaults",
            r#"
            local function helper(x) return x + 1 end
            return {
                name = "rich_defaults",
                config_schema = {
                    flag = { type = "boolean", default = false },
                    count = { type = "integer", default = 7 },
                    empty_list = { type = "list", default = {} },
                    tags = { type = "list", default = { "a", "b" } },
                    nested = {
                        type = "object",
                        default = { b = 3 },
                        fields = {
                            a = { type = "boolean", default = false },
                            b = { type = "integer", default = 2 },
                            items = { type = "list", default = { "x" } },
                        },
                    },
                    cb = { type = "function", default = helper },
                },
                apply = function() end,
            }
            "#,
        );
        // 1. Default-only startup fast path vs explicit JSON round-trip equivalence
        let rows = k.plugin_rows().unwrap();
        let row: Table = rows.get(1).unwrap();
        let fast_cfg: Table = row.get("config").unwrap();
        let def = k.def("rich_defaults").unwrap();
        let merged = k.plugin_config_json_with(&def, None, None).unwrap();
        let roundtrip_cfg: Table = match mare_shared::lua::config_to_lua(lua, &merged).unwrap() {
            Value::Table(t) => t,
            other => panic!("expected table, got {other:?}"),
        };

        assert_eq!(fast_cfg.get::<bool>("flag").unwrap(), roundtrip_cfg.get::<bool>("flag").unwrap());
        assert!(!fast_cfg.get::<bool>("flag").unwrap());
        assert_eq!(fast_cfg.get::<i64>("count").unwrap(), roundtrip_cfg.get::<i64>("count").unwrap());
        let array_mt_ptr = lua.array_metatable().to_pointer();
        for key in ["empty_list", "tags"] {
            let f_list: Table = fast_cfg.get(key).unwrap();
            let r_list: Table = roundtrip_cfg.get(key).unwrap();
            assert_eq!(f_list.metatable().unwrap().to_pointer(), array_mt_ptr, "{key} array_metatable");
            assert_eq!(r_list.metatable().unwrap().to_pointer(), array_mt_ptr, "{key} roundtrip array_metatable");
            assert_eq!(
                f_list.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>().unwrap(),
                r_list.sequence_values::<String>().collect::<mlua::Result<Vec<_>>>().unwrap(),
            );
        }
        let f_nested: Table = fast_cfg.get("nested").unwrap();
        let r_nested: Table = roundtrip_cfg.get("nested").unwrap();
        assert_eq!(f_nested.get::<bool>("a").unwrap(), r_nested.get::<bool>("a").unwrap());
        assert_eq!(f_nested.get::<i64>("b").unwrap(), r_nested.get::<i64>("b").unwrap());
        let f_items: Table = f_nested.get("items").unwrap();
        assert_eq!(f_items.metatable().unwrap().to_pointer(), array_mt_ptr);
        assert_eq!(f_items.get::<String>(1).unwrap(), "x");
        // Function default identity & behavior match round-trip
        let f_cb: Function = fast_cfg.get("cb").unwrap();
        let r_cb: Function = roundtrip_cfg.get("cb").unwrap();
        assert_eq!(f_cb.to_pointer(), r_cb.to_pointer());
        assert_eq!(f_cb.call::<i64>(41).unwrap(), 42);
        assert_eq!(r_cb.call::<i64>(41).unwrap(), 42);

        // 2. Unchanged reload reuses the exact same config table identity
        let rows2 = k.plugin_rows().unwrap();
        let fast_cfg_2: Table = rows2.get::<Table>(1).unwrap().get("config").unwrap();
        assert_eq!(fast_cfg.to_pointer(), fast_cfg_2.to_pointer());

        // 3. Reload with a user override that matches the default reuses the same table via resolve_prev_json
        k.set_config(serde_json::json!({ "plugins": { "rich_defaults": { "count": 7 } } }));
        let rows3 = k.plugin_rows().unwrap();
        let cfg3: Table = rows3.get::<Table>(1).unwrap().get("config").unwrap();
        assert_eq!(fast_cfg.to_pointer(), cfg3.to_pointer());

        // 4. Reload with a changed user override produces a fresh config table
        k.set_config(serde_json::json!({ "plugins": { "rich_defaults": { "count": 99 } } }));
        let rows4 = k.plugin_rows().unwrap();
        let cfg4: Table = rows4.get::<Table>(1).unwrap().get("config").unwrap();
        assert_ne!(fast_cfg.to_pointer(), cfg4.to_pointer());
        assert_eq!(cfg4.get::<i64>("count").unwrap(), 99);
    }

    #[test]
    fn embedded_fingerprint_cache_matches_uncached_computation() {
        use std::hash::{Hash, Hasher};

        const EMBEDDED_CODE: &[u8] = b"return { name = 'cached_fp_builtin', apply = function() end }\n";
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let src = PluginSource {
            name_hint: "cached_fp_builtin".into(),
            origin: Origin::Builtin,
            path: None,
            code: Some(EMBEDDED_CODE),
        };
        let def = k.load_def(&src).unwrap();

        let mut expected_hasher = std::collections::hash_map::DefaultHasher::new();
        def.source.origin.as_str().hash(&mut expected_hasher);
        def.source.path.hash(&mut expected_hasher);
        Some((EMBEDDED_CODE.as_ptr() as usize, EMBEDDED_CODE.len())).hash(&mut expected_hasher);
        Kernel::hash_modules(&def, &mut expected_hasher);
        let expected = format!("{:016x}", expected_hasher.finish());

        let first = super::fingerprint(&def);
        let second = super::fingerprint(&def);
        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }
}
