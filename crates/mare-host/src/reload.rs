//! `/reload config` and the full `/reload`: re-running the configuration, reloading the plugins
//! it changed, and packages / definitions for a full reload.

use std::sync::Arc;

use anyhow::Result;

use crate::Host;
use crate::plugins::defs::wanted_defs;
use crate::plugins::packages::{install_packages, package_hooks};
use crate::plugins::plugin_failures;
use crate::plugins::plugin_failures::PluginFailure;

/// A reload in progress: `reload_active` set, and for a full reload the failures as they were
/// when it started ([`Host::failures`]). Both clear when it drops, on every way out.
struct ReloadActivity<'a>(&'a Host);

impl Drop for ReloadActivity<'_> {
    fn drop(&mut self) {
        self.0.reload_active.set(false);
        self.0.reload_failures.take();
    }
}

impl Host {
    /// `/reload config`: re-run init.lua (+ trusted project config), diff against the current
    /// effective config, reload only plugins whose opts changed and notify changed core options.
    /// Returns `{changed = {...}, reloaded = {...}, warnings = {...}}`.
    pub async fn reload_config(self: &Arc<Self>) -> Result<serde_json::Value> {
        // owned and shared: a slot sync below outlives this call if its caller is cancelled
        let gate = std::rc::Rc::new(self.try_lock_reload_gate()?);
        // the whole reload is one step for the frontends' requests and tool calls (the kernel's
        // reload barrier), held as long as the gate: a slot sync below that outlives this call
        // keeps both
        let hold = std::rc::Rc::new(self.kernel.reload_hold());
        self.reload_active.set(true);
        let _activity = ReloadActivity(self);
        self.reload_epoch.set(self.reload_epoch.get().wrapping_add(1));
        let (old, new, mut changed) = self.reevaluate().await?;
        let km_guard = mare_kernel::KeymapReloadGuard::begin(self.lua());
        let mut reloaded = vec![];
        let mut failures = vec![];
        // `plugins.preset` or a slot's `enabled` changed (or an earlier sync did not finish): the
        // plugin set is resolved again as at startup. The sync runs in a task
        // of its own holding the reload gate, marked pending before its first suspension point:
        // closing the slot of the plugin that runs this reload (`commands`) disposes the caller,
        // not the sync, and a sync cut short is retried by the next `/reload config`.
        let mut started = vec![];
        if slot_state(&old) != slot_state(&new) || self.slot_sync_pending.get() {
            self.slot_sync_pending.set(true);
            let this = self.clone();
            let held = (gate.clone(), hold.clone());
            let config = new.clone();
            let sync = tokio::task::spawn_local(async move {
                let _held = held;
                let out = this.run_slot_sync(&config).await;
                this.slot_sync_pending.set(false);
                out
            });
            let (added, notices) = sync.await.map_err(|e| anyhow::anyhow!("slot sync: {e}"))?;
            changed.push("plugins (slots)".into());
            failures.extend(notices);
            reloaded.extend(added.iter().cloned());
            started = added;
        }
        // plugins whose opts changed → reload just those (and their dependents)
        let empty = serde_json::json!({});
        let po = old.get(mare_shared::PLUGINS_KEY).unwrap_or(&empty);
        let pn = new.get(mare_shared::PLUGINS_KEY).unwrap_or(&empty);
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for o in [po, pn] {
            if let Some(m) = o.as_object() {
                names.extend(m.keys().cloned());
            }
        }
        let retry = self.reload_retry.take();
        self.reload_pending.borrow_mut().extend(retry);
        names.extend(self.reload_pending.borrow().iter().cloned());
        let definitions = self.kernel.def_names();
        self.reload_pending.borrow_mut().retain(|name| definitions.contains(name));
        for n in &names {
            if po.get(n) != pn.get(n) && definitions.contains(n) {
                changed.push(format!("plugins.{n}"));
                // Mark every changed plugin before the first await so cancellation cannot lose
                // work later in the list after the new config has already been committed.
                self.reload_pending.borrow_mut().insert(n.clone());
            }
        }
        // just started on the new config: nothing to reload
        self.reload_pending.borrow_mut().retain(|n| !started.contains(n));
        // The whole tree follows the rows once (loader apply_rows): a changed row
        // updates its fiber with the new config, a row turned off is disposed, the rest are left
        // alone. Reloading each name would replace its definition and keep the old config.
        let pending: Vec<String> = self.reload_pending.borrow().iter().cloned().collect();
        if !pending.is_empty() {
            // the changed plugins' definitions are read again first (a broken one keeps its
            // plugin running as it is and is retried by the next config reload)
            for (n, message) in self.kernel.prepare_changed_defs(&pending).await {
                self.reload_retry.borrow_mut().insert(n);
                failures.push(message);
            }
            match self.kernel.apply_rows_report().await {
                Ok(report) => {
                    self.reload_pending.borrow_mut().clear();
                    if !report.failures.is_empty() {
                        self.reload_retry.borrow_mut().extend(pending.iter().cloned());
                    }
                    reloaded.extend(pending);
                    failures.extend(report.failures);
                }
                Err(e) => {
                    self.reload_pending.borrow_mut().clear();
                    self.reload_retry.borrow_mut().extend(pending);
                    tracing::warn!("reload config: {e}");
                    failures.push(format!("config reload failed: {e}"));
                }
            }
        }
        reloaded.sort();
        reloaded.dedup();
        for problem in km_guard.finish(false) {
            self.kernel.add_problem(problem.clone());
            failures.push(problem);
        }
        let warnings = self.finish_reevaluate(&new, &changed)?;
        if self.pending_session_trust_revoke.get() {
            self.revoke_session_only_trust_with_gate().await?;
        }
        // plugins left waiting for a service nobody provides (Cordis PENDING), read from the tree
        // once the update is done, whichever path changed it (rows, slots): a dependent whose
        // provider was turned off waits too, though it was not restarted
        let waiting = self.kernel.waiting().await;
        self.sync_web_assets();
        Ok(serde_json::json!({
            "changed": changed, "reloaded": reloaded, "warnings": warnings, "failures": failures, "pending": waiting,
        }))
    }

    /// Full `/reload`: init.lua
    /// runs again, newly declared packages are installed, plugins of added packages start and
    /// those of removed packages are disposed, everything else reloads (a plugin whose new
    /// definition fails keeps its old one), then packages' `config()` runs again. Emits
    /// `reload/completed {added, removed, reloaded, notices}`.
    pub(crate) fn try_lock_reload_gate(&self) -> Result<tokio::sync::OwnedMutexGuard<()>> {
        crate::startup::request_gate::purge_cancelled_leases(self.lua());
        self.reload_gate.clone().try_lock_owned().map_err(|_| {
            let message = if self.reload_active.get() || self.kernel.is_reloading() {
                "reload already in progress; retry afterwards"
            } else {
                "a model request is active; retry afterwards"
            };
            anyhow::Error::new(mare_shared::errors::Coded { code: "busy", message: message.to_owned() })
        })
    }

    pub async fn reload_all(self: &Arc<Self>) -> Result<serde_json::Value> {
        let gate = self.try_lock_reload_gate()?;
        self.reload_all_with_gate(gate).await
    }

    pub(crate) async fn reload_all_with_gate(
        self: &Arc<Self>,
        _reload_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<serde_json::Value> {
        // the whole reload is one step for the frontends' requests and tool calls (the kernel's
        // reload barrier): they run before it starts or after it ends, never against a tree
        // half reloaded (the old engine applied a reload without yielding)
        let _hold = self.kernel.reload_hold();
        // the restarts make plugins ask for the failures dozens of times (the statusline on every
        // event, llm per missing provider); a half-reloaded tree has no meaningful answer, so they
        // get the failures from before the reload, computed once
        self.reload_failures.replace(Some(self.failures()));
        self.reload_active.set(true);
        let activity = ReloadActivity(self);
        let mut out = self.reload_all_pass().await?;
        // A session switch triggered during this reload (e.g. kernel/reloaded reopening the
        // carried session while session-only trust was active) deferred revocation by setting
        // `pending_session_trust_revoke`; complete revocation inside the same reload barrier.
        if !self.reload_full_incomplete.get() && self.pending_session_trust_revoke.get() {
            out = self.reload_all_pass().await?;
        }
        // live again before the completion event: its listeners (the statusline) read the result
        drop(activity);
        self.kernel.emit_json("reload/completed", out.clone()).await;
        Ok(out)
    }

    async fn reload_all_pass(self: &Arc<Self>) -> Result<serde_json::Value> {
        self.reload_epoch.set(self.reload_epoch.get().wrapping_add(1));
        let revoking_pending = self.pending_session_trust_revoke.get();
        if revoking_pending {
            self.workspace.set_trusted(false);
            if let Ok(mare) = self.lua().globals().get::<mlua::Table>("mare") {
                let _ = mare.set("trusted", false);
                if let Ok(Some(args)) = mare.get::<Option<mlua::Table>>("args") {
                    let arg_value =
                        if mare_config::trust::decision(&self.cwd).is_none() { "undecided" } else { "untrusted" };
                    let _ = args.set("trust", arg_value);
                    let _ = args.set("trust_pending", false);
                }
            }
        }
        let mut notices: Vec<String> = vec![];
        let mut changed = vec![];
        let config = if self.reload.load_init {
            let (_, new, ch) = self.reevaluate().await?;
            changed = ch;
            new
        } else {
            self.kernel.config()
        };
        let km_guard = mare_kernel::KeymapReloadGuard::begin(self.lua());
        self.reload_full_incomplete.set(true);
        let mut incomplete = false;
        let mut package_notices = vec![];
        // Full reload has several suspension points. If it fails or is cancelled, keep the
        // affected definitions eligible for a later config reload even with unchanged options.
        self.reload_pending.borrow_mut().extend(self.kernel.def_names());
        if self.reload.load_init && !self.reload.no_user_plugins {
            // progress in the activity area while new packages install
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
            let ui = self.ui.clone();
            let shown = async move {
                let mut any = false;
                while let Some(line) = rx.recv().await {
                    any = true;
                    ui.send(mare_shared::ui::UiCmd::Working { label: Some(line), detail: None, background: false });
                }
                if any {
                    ui.send(mare_shared::ui::UiCmd::Working { label: None, detail: None, background: false });
                }
            };
            let (problems, ()) = tokio::join!(install_packages(self.lua(), &config, &self.cancel, Some(tx)), shown);
            package_notices = problems;
            notices.extend(package_notices.iter().cloned());
        }
        let SlotSync { added, removed, replaced, notices: sync_notices, failures: definition_failures } =
            self.sync_definitions(&config).await;
        let mut failures = definition_failures;
        if self.reload.load_init && !self.reload.no_user_plugins {
            failures.extend(plugin_failures::unavailable_packages(&self.kernel, &config, &package_notices));
        }
        notices.extend(sync_notices);
        let init_problems = if self.reload.load_init {
            let problems = package_hooks(&self.kernel, "init", &config).await;
            notices.extend(problems.iter().cloned());
            problems
        } else {
            vec![]
        };
        let report = self.kernel.reload_report_in_host_transaction(None).await?;
        failures.extend(plugin_failures::package_hook_failures(&self.kernel, &config, "init", &init_problems));
        let reloaded = report.applied;
        // finished: plugins that didn't start count like startup failures, the rest
        // kept or re-enabled their previous definition.
        self.reload_pending.borrow_mut().clear();
        if report.failures.is_empty() && report.restored.is_empty() {
            self.reload_retry.borrow_mut().clear();
        } else {
            self.reload_retry.borrow_mut().extend(self.kernel.def_names());
            self.reload_retry.borrow_mut().extend(report.restored.iter().cloned());
        }
        notices.extend(plugin_failures::reload_notices(&self.kernel, &report.failures));
        // waiting for a service nobody provides: shown, not a failure
        notices.extend(report.pending.iter().cloned());
        if self.reload.load_init {
            let problems = package_hooks(&self.kernel, "config", &config).await;
            failures.extend(plugin_failures::package_hook_failures(&self.kernel, &config, "config", &problems));
            notices.extend(problems);
            notices.extend(self.finish_reevaluate(&config, &changed)?);
        }
        for problem in km_guard.finish(false) {
            self.kernel.add_problem(problem.clone());
            notices.push(problem);
        }
        let blocked = package_notices
            .iter()
            .find(|n| n.starts_with("package installation task failed:"))
            .filter(|_| plugin_failures::package_operation_blocks(&self.kernel, &config))
            .cloned()
            .into_iter()
            .collect::<Vec<_>>();
        let inactive = plugin_failures::start_failures(&self.kernel, &self.kernel.plugin_tree().unwrap_or_default());
        incomplete |= !blocked.is_empty()
            || failures.iter().chain(&inactive).any(|failure| failure.blocks)
            || (revoking_pending && (!report.failures.is_empty() || !report.restored.is_empty()));
        self.reload_full_incomplete.set(incomplete);
        if !incomplete && (revoking_pending || !self.workspace.is_trusted()) {
            self.pending_session_trust_revoke.set(false);
            self.pending_session_trust_full_reload.set(false);
            self.workspace.set_session_only(false);
            self.workspace.update_config(self.kernel.config());
        }
        // a completed full reload replaces what startup couldn't do: its own failures (if any)
        // keep blocking as an incomplete reload
        self.startup_blocked.replace(blocked);
        let previous_failures = self.plugin_failures.replace(failures);
        self.kernel.retain_problems(|problem| !previous_failures.iter().any(|f| f.owns_package_problem(problem)));
        self.reloaded.set(true);
        self.sync_web_assets();
        if !incomplete {
            let startup = self.startup_problems.take();
            let retained: Vec<_> = startup
                .iter()
                .filter(|problem| {
                    inactive.iter().any(|f| {
                        problem.strip_prefix(&format!("plugin {} ", f.plugin)).is_some_and(|error| {
                            error == f.error || error.strip_prefix("not started: ") == Some(f.error.as_str())
                        })
                    })
                })
                .cloned()
                .collect();
            self.kernel.retain_problems(|p| !startup.iter().any(|s| s == p) || retained.iter().any(|s| s == p));
            self.startup_problems.replace(retained);
            self.kernel.clear_handler_errors();
        }
        for problem in self.plugin_failures.borrow().iter().filter_map(PluginFailure::package_problem) {
            self.kernel.add_problem(problem);
        }
        for notice in package_notices {
            if !self.plugin_failures.borrow().iter().any(|failure| failure.owns_package_problem(&notice)) {
                if !self.kernel.problems().contains(&notice) {
                    self.kernel.add_problem(notice.clone());
                }
                if !self.startup_problems.borrow().contains(&notice) {
                    self.startup_problems.borrow_mut().push(notice);
                }
            }
        }
        let out = serde_json::json!({
            "added": added,
            "removed": removed,
            "replaced": replaced,
            "reloaded": reloaded,
            "notices": notices,
        });
        Ok(out)
    }
}

/// What [`Host::sync_definitions`] changed.
pub(crate) struct SlotSync {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub replaced: Vec<String>,
    pub notices: Vec<String>,
    pub failures: Vec<PluginFailure>,
}

impl Host {
    /// Resolve the plugin definitions for `config` as startup does (discovery, the same-name
    /// rule, name slots, `plugins.preset`) and make the running set match: removed names are
    /// disposed, added ones are registered (the caller starts them), replaced ones swap their
    /// definition. A definition that no longer reads keeps a running plugin on its old one; a
    /// refusal (same-source twins, the reserved name) never does. Shared by a full `/reload`,
    /// `/reload config` when a slot changed, and a project trusted mid-session.
    pub(crate) async fn sync_definitions(self: &Arc<Self>, config: &serde_json::Value) -> SlotSync {
        let mut notices = vec![];
        // a definition that no longer loads: a running plugin keeps its definition (and keeps
        // running; WezTerm keeps the last good config), any other is recorded as failed
        let mut definition_failures = vec![];
        let mut kept = vec![];
        let essential = self.kernel.essential_services();
        let (mut defs, refusals) = wanted_defs(
            &self.kernel,
            config,
            &self.cwd,
            self.workspace.is_trusted(),
            self.reload.no_user_plugins,
            &mut |src, e, slot| {
                let running = plugin_failures::running_def(&self.kernel, src);
                // responsible for what the same-name definition it would replace provides: the
                // running one, or the one read now (a left-out built-in's slot included)
                let mut same_name = self.kernel.def(&src.name_hint).map(|d| d.services()).unwrap_or_default();
                same_name.extend(slot.iter().filter(|s| !same_name.contains(s)).cloned().collect::<Vec<_>>());
                let failure = PluginFailure::plugin(
                    &src.name_hint,
                    "definition",
                    e.clone(),
                    Some(src),
                    &same_name,
                    &essential,
                    plugin_failures::source_package_required(config, src),
                );
                match running {
                    // the kernel's reload reports it ("keeping the running definition")
                    Some(d) => kept.push(d),
                    None => {
                        self.kernel.add_problem(e);
                        notices.push(failure.notice());
                        definition_failures.push(failure);
                    }
                }
            },
        );
        // a kept definition is still subject to its slot (closed, or left out by the preset)
        defs.extend(kept.into_iter().filter(|d| {
            // a refusal stops only a kept definition from the refused source
            !refusals.iter().any(|r| r.name == d.name && r.source.origin == d.source.origin)
                && mare_plugin::loader::slot_enabled(
                    &d.name,
                    d.source.origin == mare_kernel::Origin::Builtin,
                    &d.tags,
                    config,
                )
        }));
        let (added, removed, replaced, refused) = self.kernel.sync_defs(defs).await;
        let _ = mare_plugin::config_load::sync_plugin_slots(self.kernel.lua(), &self.kernel.effective_declared());
        notices.extend(refused.iter().map(|r| PluginFailure::override_refused(r).notice()));
        definition_failures.extend(refused.iter().map(PluginFailure::override_refused));
        for r in &refusals {
            self.kernel.add_problem(r.message());
            notices.push(PluginFailure::override_refused(r).notice());
            definition_failures.push(PluginFailure::override_refused(r));
        }
        SlotSync { added, removed, replaced, notices, failures: definition_failures }
    }
}

/// What decides the plugin set in a config: `plugins.preset` and every slot's explicit `enabled`.
fn slot_state(config: &serde_json::Value) -> (Option<String>, std::collections::BTreeMap<String, bool>) {
    let plugins = config.get(mare_shared::PLUGINS_KEY).and_then(|p| p.as_object());
    let preset = plugins.and_then(|p| p.get("preset")).and_then(|v| v.as_str()).map(str::to_string);
    let enabled = plugins
        .into_iter()
        .flatten()
        .filter_map(|(name, v)| v.get("enabled").and_then(|e| e.as_bool()).map(|e| (name.clone(), e)))
        .collect();
    (preset, enabled)
}

impl Host {
    /// The slot sync of a `/reload config` (called with the reload gate held): resolve the set,
    /// reload replaced definitions inside the held lock, start the added ones. Returns the added
    /// names and the notices.
    async fn run_slot_sync(self: &Arc<Self>, config: &serde_json::Value) -> (Vec<String>, Vec<String>) {
        let sync = self.sync_definitions(config).await;
        let mut notices = sync.notices;
        for n in &sync.replaced {
            match self.kernel.reload_report_in_host_transaction(Some(n)).await {
                Ok(report) => {
                    if !report.failures.is_empty() {
                        self.reload_retry.borrow_mut().insert(n.clone());
                    }
                    notices.extend(report.failures);
                }
                Err(e) => {
                    self.reload_retry.borrow_mut().insert(n.clone());
                    notices.push(format!("plugin {n} reload failed: {e}"));
                }
            }
        }
        if let Err(e) = self.kernel.start().await {
            notices.push(format!("starting plugins: {e}"));
        }
        self.plugin_failures.borrow_mut().extend(sync.failures);
        (sync.added, notices)
    }
}
