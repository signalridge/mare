//! Hot reload: the reload transaction, its gate / generation / barrier, and syncing the
//! definition set.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use mlua::{MultiValue, Value};

use crate::plugins::overrides;
use crate::{Kernel, OverrideRefusal, PluginDef};

/// Outcome of a reload, including failures recovered by restoring an old definition.
pub struct ReloadReport {
    pub applied: Vec<String>,
    pub failures: Vec<String>,
    /// Plugins whose new definition failed and that were restored to run on their previous one.
    pub restored: Vec<String>,
    /// plugins left waiting for a service nobody provides (Cordis PENDING; not a failure), with
    /// the services they wait for
    pub pending: Vec<String>,
}

impl Kernel {
    /// Make the definition set match `defs` (a full `/reload` after packages changed
    /// "`/reload` 也处理新增和删除的包"): plugins no longer defined are disposed and dropped,
    /// new ones are added (the caller starts them), and a name now coming from another source
    /// (a package plugin removed, so the builtin is back) gets that definition. Returns
    /// (added, removed, replaced, refused); the refusals ([`OverrideRefusal`]) are also recorded
    /// as problems.
    pub async fn sync_defs(
        &self,
        defs: Vec<PluginDef>,
    ) -> (Vec<String>, Vec<String>, Vec<String>, Vec<OverrideRefusal>) {
        let mut wanted: Vec<PluginDef> = Vec::new();
        let (_, refused) = overrides::merge(&mut wanted, defs);
        self.st().problems.extend(refused.iter().map(OverrideRefusal::message));
        let current = self.st().defs.clone();
        let removed: Vec<String> =
            current.iter().filter(|c| !wanted.iter().any(|w| w.name == c.name)).map(|c| c.name.clone()).collect();
        // the plugin tree drops the rows no longer defined when it follows the new row list
        let mut added = vec![];
        let mut replaced = vec![];
        {
            let mut st = self.st();
            st.defs.retain(|d| !removed.contains(&d.name));
            for w in wanted {
                match st.defs.iter().position(|d| d.name == w.name) {
                    Some(pos) => {
                        let same =
                            st.defs[pos].source.path == w.source.path && st.defs[pos].source.origin == w.source.origin;
                        if !same {
                            replaced.push(w.name.clone());
                            st.defs[pos] = w;
                        }
                    }
                    None => {
                        added.push(w.name.clone());
                        st.defs.push(w);
                    }
                }
            }
        }
        self.sync_defs_state();
        (added, removed, replaced, refused)
    }

    /// Resolves once no reload is in progress: tool calls, and the frontends' requests that reach
    /// the plugin tree, arriving during a reload run after it (the old engine applied a reload
    /// without yielding, so nothing could run against a half-reloaded tree).
    pub async fn reload_barrier(&self) {
        let mut rx = self.0.reloading.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
    }

    /// Hold the reload barrier ([`Self::reload_barrier`]) until the returned guard drops: the
    /// kernel's own reloads, and the host's whole reload (init.lua, packages, definitions and the
    /// plugin reload, `Host::reload_all` / `reload_config`), so it is one step for everything else.
    pub fn reload_hold(&self) -> ReloadHold {
        self.0.reloading.send_modify(|n| *n += 1);
        ReloadHold(self.clone())
    }

    /// Whether a kernel or host reload barrier is currently active.
    pub fn is_reloading(&self) -> bool {
        *self.0.reloading.borrow() > 0
    }

    /// Hot reload, in two steps:
    /// 1. **prepare** — load and validate every target's new definition while the old ones keep
    ///    running. If a requested plugin's new definition fails (load error, missing provider,
    ///    dependency cycle), nothing is changed and the error is returned; in a full reload a
    ///    plugin whose new definition fails simply keeps running on its old one.
    /// 2. **switch** — dispose the targets (cancelling their tasks and tool calls) in reverse
    ///    activation order, apply the new definitions in dependency order; a plugin whose new
    ///    `apply` fails gets its old definition re-applied as a fresh instance, best effort (its
    ///    runtime state and external side effects are not restored). One failure doesn't stop
    ///    the rest. Tool calls arriving meanwhile wait ([`Self::reload_barrier`]).
    pub async fn reload(&self, name: Option<&str>) -> anyhow::Result<Vec<String>> {
        Ok(self.reload_report(name).await?.applied)
    }

    /// Reload while also returning failures which were recovered on a best-effort basis.
    pub async fn reload_report(&self, name: Option<&str>) -> anyhow::Result<ReloadReport> {
        if let Ok(purge) = self.lua().named_registry_value::<mlua::Function>("mare.purge_cancelled_leases") {
            let _ = purge.call::<()>(());
        }
        let busy_message = if self.is_reloading() {
            "reload already in progress; retry afterwards"
        } else {
            "a model request is active; retry afterwards"
        };
        let _gate = self.0.reload_gate.try_lock().map_err(|_| {
            anyhow::Error::new(mare_shared::errors::Coded { code: "busy", message: busy_message.to_owned() })
        })?;
        let report = self.reload_report_in_host_transaction(name).await?;
        if let Ok(after) = self.lua().named_registry_value::<mlua::Function>("mare.after_kernel_reload") {
            let _hold = self.reload_hold();
            mare_shared::lua::call_async::<()>(self.lua(), &after, ()).await?;
        }
        Ok(report)
    }

    /// The host owns `reload_gate` across its wider configuration transaction.
    pub fn reload_gate(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.0.reload_gate.clone()
    }

    pub fn reload_generation(&self) -> u64 {
        self.0.reload_generation.load(Ordering::Relaxed)
    }

    /// Targets of a reload that is running or was interrupted (its future dropped mid-way, so
    /// old and new state may be mixed); recovery requires another reload. A reload that ran to
    /// the end clears its target whatever failed: a plugin it couldn't start is simply not
    /// active, and the host decides whether that blocks requests.
    pub fn reload_incomplete(&self) -> Vec<String> {
        self.0.reload_incomplete.lock().unwrap().iter().cloned().collect()
    }

    /// Caller must hold `reload_gate`; used to include plugin reload in the host transaction.
    pub async fn reload_report_in_host_transaction(&self, name: Option<&str>) -> anyhow::Result<ReloadReport> {
        if let Some(n) = name
            && !self.st().defs.iter().any(|d| d.name == n)
        {
            anyhow::bail!("unknown plugin `{n}`");
        }
        let target = name.unwrap_or("<all>").to_string();
        self.0.reload_incomplete.lock().unwrap().insert(target.clone());
        self.0.reload_generation.fetch_add(1, Ordering::Relaxed);
        let _hold = self.reload_hold();
        let result = self.reload_inner(name).await;
        {
            let mut incomplete = self.0.reload_incomplete.lock().unwrap();
            if name.is_none() {
                incomplete.clear();
            } else {
                incomplete.remove(&target);
            }
        }
        result
    }

    async fn reload_inner(&self, name: Option<&str>) -> anyhow::Result<ReloadReport> {
        let mut failures = Vec::new();
        if let Some(n) = name
            && !self.st().defs.iter().any(|d| d.name == n)
        {
            anyhow::bail!("unknown plugin `{n}`");
        }
        // 1. prepare: new definitions are loaded and validated before anything changes; in a full
        //    reload a plugin whose new definition fails keeps running on its old one
        let defs = self.st().defs.clone();
        let old_defs: std::collections::HashMap<String, PluginDef> =
            defs.iter().map(|d| (d.name.clone(), d.clone())).collect();
        let mut new_defs: Vec<PluginDef> = Vec::with_capacity(defs.len());
        // the plugins whose new definition was prepared: boot.lua's fallback restart set (rows
        // without fingerprints)
        let mut targets: Vec<String> = vec![];
        for d in &defs {
            if name.is_some_and(|n| n != d.name) {
                new_defs.push(d.clone());
                continue;
            }
            match self.load_def(&d.source).and_then(|nd| {
                if nd.name != d.name {
                    anyhow::bail!("plugin {} renamed itself to `{}` on reload", d.name, nd.name);
                }
                if let Value::Function(_) | Value::Nil = nd.table.get::<Value>("apply")? {
                    Ok(nd)
                } else {
                    anyhow::bail!("plugin {}: `apply` must be a function", d.name)
                }
            }) {
                Ok(nd) => {
                    targets.push(nd.name.clone());
                    new_defs.push(nd)
                }
                Err(e) => {
                    if name == Some(d.name.as_str()) {
                        // the requested plugin can't be prepared: leave everything as it is
                        anyhow::bail!("reload of {} aborted, nothing changed: {e}", d.name);
                    }
                    let message = format!("{e} (keeping the running definition)");
                    self.st().problems.push(message.clone());
                    failures.push(message);
                    new_defs.push(d.clone());
                }
            }
        }
        // An inject cycle the new definitions close never resolves: a targeted reload of a member
        // aborts with nothing changed; a full reload keeps
        // the running definition of each member whose dependencies changed (its injects or the
        // services it provides: what closed the cycle; a full reload reloads every definition, so
        // an unchanged member is not named)
        let cycles = {
            let refs: Vec<&PluginDef> = new_defs.iter().collect();
            super::provide_check::cycles(&refs)
        };
        if let Some(n) = name
            && let Some(c) = cycles.iter().find(|c| c.path.iter().any(|p| p == n))
        {
            anyhow::bail!("reload of {n} aborted, nothing changed: {}", c.reason());
        }
        for c in &cycles {
            let n = &c.plugin;
            if let Some(i) = new_defs.iter().position(|d| &d.name == n)
                && let Some(old) = old_defs.get(n)
                && (old.inject != new_defs[i].inject || old.services() != new_defs[i].services())
            {
                let message =
                    format!("plugin {n}: new definition can't start ({}); keeping the running one", c.reason());
                self.st().problems.push(message.clone());
                failures.push(message);
                new_defs[i] = old.clone();
                targets.retain(|t| t != n);
            }
        }
        // 2. switch: the cordis tree does it (boot.lua `booted.reload`): one plugin through
        //    `loader:replace` (dependents reload as cordis sees the provider change, a failed
        //    apply puts the old definition back), all of them by following the new row list
        let km_guard = KeymapReloadGuard::begin(self.lua());
        self.st().defs = new_defs;
        self.sync_defs_state();
        let booted = self.cordis_boot().map_err(|e| anyhow::anyhow!("cordis boot failed: {e}"))?;
        let reload: mlua::Function = booted.get("reload")?;
        let report: mlua::Table = mare_shared::lua::call_async(self.lua(), &reload, (name, targets)).await?;
        let applied = report.get::<Option<Vec<String>>>("applied")?.unwrap_or_default();
        let pending = report.get::<Option<Vec<String>>>("pending")?.unwrap_or_default();
        // plugins whose new definition failed and that run on their previous one again
        let restored = report.get::<Option<Vec<String>>>("restored")?.unwrap_or_default();
        for n in &restored {
            if let Some(old) = old_defs.get(n)
                && let Some(slot) = self.st().defs.iter_mut().find(|x| x.name == *n)
            {
                *slot = old.clone();
            }
            self.st().problems.push(format!("plugin {n} re-enabled on its previous definition"));
        }
        // plugins whose new definition cannot start and that keep running on the previous one
        for n in report.get::<Option<Vec<String>>>("kept")?.unwrap_or_default() {
            if let Some(old) = old_defs.get(&n)
                && let Some(slot) = self.st().defs.iter_mut().find(|x| x.name == n)
            {
                *slot = old.clone();
            }
        }
        for msg in report.get::<Option<Vec<String>>>("failures")?.unwrap_or_default() {
            self.st().problems.push(msg.clone());
            failures.push(msg);
        }
        self.sync_defs_state();
        let km_diags = km_guard.finish(name.is_some());
        if !km_diags.is_empty() {
            for msg in &km_diags {
                self.st().problems.push(msg.clone());
            }
            if let Some(n) = name {
                if let Some(old) = old_defs.get(n)
                    && let Some(slot) = self.st().defs.iter_mut().find(|x| x.name == n)
                {
                    *slot = old.clone();
                }
                self.sync_defs_state();
                let rollback_km = KeymapReloadGuard::begin(self.lua());
                let _ =
                    mare_shared::lua::call_async::<mlua::Table>(self.lua(), &reload, (Some(n), vec![n.to_string()]))
                        .await;
                let _ = rollback_km.finish(false);
                anyhow::bail!("reload of {n} failed keymap validation: {}", km_diags.join("; "));
            }
            failures.extend(km_diags);
        }
        self.emit("kernel/reloaded", MultiValue::new()).await;
        Ok(ReloadReport { applied, failures, restored, pending })
    }
}

/// Transactional keymap guard across plugin/config reloads: begins a reload transaction on enter,
/// validates/settles on `finish`, and rolls back + restores `_settled = true` if dropped early.
pub struct KeymapReloadGuard {
    lua: mlua::Lua,
    active: bool,
}

impl KeymapReloadGuard {
    pub fn begin(lua: &mlua::Lua) -> Self {
        mare_shared::lua::core::keymap::begin_reload(lua);
        Self { lua: lua.clone(), active: true }
    }

    /// Close this guard layer. When the outermost reload guard finishes, runs `_settle(strict)`
    /// (returning any diagnostics).
    pub fn finish(mut self, strict: bool) -> Vec<String> {
        self.active = false;
        mare_shared::lua::core::keymap::finish_reload(&self.lua, strict)
    }
}

impl Drop for KeymapReloadGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        mare_shared::lua::core::keymap::rollback_reload(&self.lua);
    }
}

/// A held reload barrier ([`Kernel::reload_hold`]); dropping it releases the hold.
pub struct ReloadHold(Kernel);

impl Drop for ReloadHold {
    fn drop(&mut self) {
        self.0.0.reloading.send_modify(|n| *n = n.saturating_sub(1));
        // the last reload ended: derived exports of names left without a provider go (withdraw_now
        // keeps them while a reload runs); a busy state is left for the next withdraw
        if *self.0.0.reloading.borrow() == 0
            && let Ok(mut st) = self.0.0.state.try_lock()
        {
            let st = &mut *st;
            let services = &st.services;
            st.derived.retain(|name, _| services.get(name).is_some_and(|entry| !entry.lifetime.is_cancelled()));
        }
    }
}
