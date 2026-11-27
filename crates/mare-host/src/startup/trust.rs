//! The trust overlay's answer and loading the project's configuration and plugins once it is
//! trusted.

use std::sync::Arc;

use anyhow::Result;
use mlua::{MultiValue, Table, Value};

use crate::Host;
use crate::plugins::plugin_failures;

/// The answer to the trust overlay (
/// `onboarding/trust_directory.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustChoice {
    /// "Trust and load": saved, the project's protected resources load now
    Trust,
    /// "Trust this session only": not saved, loads for this session only
    TrustSession,
    /// "Don't trust": saved, nothing loads
    DontTrust,
    /// "Not now" (or Esc): nothing saved, nothing loads, asked again next time
    NotNow,
}

impl Host {
    /// Apply the answer to the trust overlay. `Trust` saves the decision and loads the project's
    /// protected resources into the running session: init.lua again with the project's
    /// `.mare/init.lua` (plugins whose options changed reload), `.mare/plugins` (added, or
    /// replacing a same-named plugin), and the context plugin again (project skills, prompt
    /// templates, SYSTEM.md / APPEND_SYSTEM.md). `TrustSession` loads the same resources for
    /// the current session only without persisting to disk or syncing sibling sessions. The
    /// permission preset keeps the untrusted start for this session. Emits
    /// `trust/changed {trusted, saved}`.
    pub async fn answer_trust(self: &Arc<Self>, choice: TrustChoice) -> Result<()> {
        match choice {
            TrustChoice::NotNow => {
                self.kernel.emit_json("trust/changed", serde_json::json!({ "trusted": false, "saved": false })).await;
            }
            TrustChoice::DontTrust => {
                self.apply_trust_change(false, true, None, false).await?;
            }
            TrustChoice::TrustSession => {
                self.apply_trust_change(true, false, Some(mare_shared::SESSION_TRUST_SOURCE), false).await?;
            }
            TrustChoice::Trust => {
                self.apply_trust_change(true, true, None, false).await?;
            }
        }
        Ok(())
    }

    /// Revoke session-only trust before a session switch (`/new`, `/resume`, `/fork`) completes
    /// or publishes incoming-session hooks on this host. Preserves `pending_session_trust_revoke`,
    /// `session_only`, and `reload_full_incomplete` if any step fails so admission stays denied and
    /// subsequent switch attempts re-run or refuse the barrier rather than skipping revocation.
    pub(crate) async fn revoke_session_only_trust(self: &Arc<Self>) -> Result<()> {
        if !self.workspace.is_session_only() && !self.pending_session_trust_revoke.get() {
            return Ok(());
        }
        if self.reload_active.get() {
            anyhow::bail!("cannot revoke session-only trust while configuration reload holds the reload gate");
        }
        self.pending_session_trust_revoke.set(true);
        self.reload_full_incomplete.set(true);

        let _gate = match self.reload_gate.clone().try_lock_owned() {
            Ok(g) => g,
            Err(_) => {
                let _ = tokio::time::timeout(self.shutdown_grace(), self.abort()).await;
                tokio::time::timeout(self.shutdown_grace(), self.reload_gate.clone().lock_owned()).await.map_err(
                    |_| anyhow::anyhow!("timed out waiting for reload gate while revoking session-only trust"),
                )?
            }
        };

        self.revoke_session_only_trust_with_gate().await
    }

    /// Revoke session-only trust when the caller already holds `reload_gate` (e.g. from
    /// [`Self::revoke_session_only_trust`], [`Self::reload_config`], or `Kernel::reload_report`
    /// post-reload settlement). Preserves `pending_session_trust_revoke`, `session_only`, and
    /// `reload_full_incomplete` if any step fails.
    pub(crate) async fn revoke_session_only_trust_with_gate(self: &Arc<Self>) -> Result<()> {
        if !self.workspace.is_session_only() && !self.pending_session_trust_revoke.get() {
            return Ok(());
        }
        self.pending_session_trust_revoke.set(true);
        self.reload_full_incomplete.set(true);

        let lua = self.lua().clone();
        let mare: Table = lua.globals().get("mare")?;

        self.workspace.set_trusted(false);
        mare.set("trusted", false)?;
        let _ = crate::agent::drain_session_as(&lua, "trust_revoked", self.shutdown_grace()).await;
        if let Some(a) = mare.get::<Option<Table>>("args")? {
            let arg_value = if mare_config::trust::decision(&self.cwd).is_none() { "undecided" } else { "untrusted" };
            a.set("trust", arg_value)?;
            a.set("trust_pending", false)?;
        }

        let _hold = self.kernel.reload_hold();
        let prev_active = self.reload_active.replace(true);
        struct ActiveGuard<'a>(&'a Host, bool);
        impl Drop for ActiveGuard<'_> {
            fn drop(&mut self) {
                self.0.reload_active.set(self.1);
            }
        }
        let _active = ActiveGuard(self, prev_active);

        let old = self.kernel.config();
        let mut new_cfg = old.clone();
        if self.reload.load_init {
            let (_prev, evaluated, changed) = self.reevaluate().await?;
            new_cfg = evaluated;
            let _ = self.finish_reevaluate(&new_cfg, &changed)?;
        }

        let empty = serde_json::json!({});
        let po = old.get(mare_shared::PLUGINS_KEY).unwrap_or(&empty);
        let pn = new_cfg.get(mare_shared::PLUGINS_KEY).unwrap_or(&empty);
        if self.pending_session_trust_full_reload.get()
            || po.get(mare_shared::SESSION_SERVICE_NAME) != pn.get(mare_shared::SESSION_SERVICE_NAME)
        {
            self.pending_session_trust_full_reload.set(true);
            anyhow::bail!("session-only project modified the session plugin; run /reload to switch sessions safely");
        }

        let sync = self.sync_definitions(&new_cfg).await;
        let blocking_sync: Vec<String> = sync.failures.iter().filter(|f| f.blocks).map(|f| f.summary()).collect();
        self.plugin_failures.borrow_mut().extend(sync.failures);
        if sync.replaced.iter().any(|n| n == mare_shared::SESSION_SERVICE_NAME)
            || sync.removed.iter().any(|n| n == mare_shared::SESSION_SERVICE_NAME)
        {
            self.pending_session_trust_full_reload.set(true);
            anyhow::bail!("session-only project modified the session plugin; run /reload to switch sessions safely");
        }

        let definitions = self.kernel.def_names();
        let mut prepare_targets = std::collections::BTreeSet::new();
        for o in [po, pn] {
            if let Some(m) = o.as_object() {
                for n in m.keys() {
                    if po.get(n) != pn.get(n) && definitions.contains(n) {
                        prepare_targets.insert(n.clone());
                    }
                }
            }
        }
        prepare_targets.extend(sync.replaced.iter().cloned());
        let retry = self.reload_retry.take();
        prepare_targets.extend(retry.into_iter().filter(|n| definitions.contains(n)));
        prepare_targets.extend(self.reload_pending.borrow().iter().filter(|n| definitions.contains(*n)).cloned());
        let prepare_list: Vec<String> = prepare_targets.into_iter().collect();
        let mut prep_errors = vec![];
        if !prepare_list.is_empty() {
            for (n, message) in self.kernel.prepare_changed_defs(&prepare_list).await {
                self.reload_retry.borrow_mut().insert(n);
                prep_errors.push(message);
            }
        }
        let report = self.kernel.apply_rows_report().await?;
        self.kernel.start().await?;

        for name in self.context_service_plugins() {
            self.kernel.reload_report_in_host_transaction(Some(&name)).await?;
        }

        let start_blocked: Vec<String> =
            plugin_failures::start_failures(&self.kernel, &self.kernel.plugin_tree().unwrap_or_default())
                .into_iter()
                .filter(|f| f.blocks)
                .map(|f| f.summary())
                .collect();
        if !blocking_sync.is_empty()
            || !prep_errors.is_empty()
            || !report.failures.is_empty()
            || !report.restored.is_empty()
            || !start_blocked.is_empty()
        {
            self.reload_retry.borrow_mut().extend(prepare_list);
            self.reload_retry.borrow_mut().extend(report.restored.iter().cloned());
            anyhow::bail!(
                "revoking session-only trust left incomplete state: {}",
                blocking_sync
                    .into_iter()
                    .chain(prep_errors)
                    .chain(report.failures)
                    .chain(report.restored.into_iter().map(|n| format!("plugin {n} restored previous definition")))
                    .chain(start_blocked)
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }

        // Notify subscribers (such as MCP) before clearing session_only / pending flags so project
        // clients disconnect and unregister tools synchronously inside the barrier.
        self.kernel
            .emit_json(
                "trust/changed",
                serde_json::json!({ "trusted": false, "saved": false, "source": mare_shared::SESSION_TRUST_SOURCE }),
            )
            .await;

        self.workspace.set_session_only(false);
        self.pending_session_trust_revoke.set(false);
        self.pending_session_trust_full_reload.set(false);
        self.reload_full_incomplete.set(false);
        self.workspace.update_config(self.kernel.config());
        self.sync_web_assets();
        Ok(())
    }

    /// Record or apply a workspace trust decision from RPC.
    pub async fn set_workspace_trust(self: &Arc<Self>, trusted: bool, save: bool) -> Result<()> {
        self.apply_trust_change(trusted, save, (!save).then_some("rpc"), false).await
    }

    async fn apply_trust_change(
        self: &Arc<Self>,
        trusted: bool,
        save: bool,
        source: Option<&str>,
        already_saved: bool,
    ) -> Result<()> {
        let lua = self.lua().clone();
        let mare: Table = lua.globals().get("mare")?;
        let args: Option<Table> = mare.get("args")?;
        if save {
            self.workspace.save_trust(trusted)?;
        }
        let saved = save || already_saved;
        let session_only = trusted && !saved && source == Some(mare_shared::SESSION_TRUST_SOURCE);
        let mut failure = None;
        let was = self.workspace.is_trusted();
        let effective = if trusted {
            if !self.reload.no_user_plugins {
                self.workspace.set_session_only(session_only);
                if !was {
                    let before = self.kernel.problems();
                    let result = self.load_trusted_project().await;
                    {
                        let mut tracked = self.startup_problems.borrow_mut();
                        for problem in self.kernel.problems() {
                            if !before.contains(&problem) && !tracked.contains(&problem) {
                                tracked.push(problem);
                            }
                        }
                    }
                    if let Err(error) = result {
                        // Trust is already active; keep requests closed until a full reload repairs it.
                        self.reload_full_incomplete.set(true);
                        failure = Some(error);
                    }
                } else {
                    self.workspace.set_trusted(true);
                    mare.set("trusted", true)?;
                }
                true
            } else {
                self.workspace.set_session_only(false);
                self.workspace.set_trusted(false);
                mare.set("trusted", false)?;
                false
            }
        } else {
            self.workspace.set_session_only(false);
            if was || mare.get::<Option<bool>>("trusted")?.unwrap_or(false) {
                self.workspace.set_trusted(false);
                mare.set("trusted", false)?;
                // Cancel and drain any running subagents started under the previously trusted
                // workspace before reloading into the untrusted policy.
                let _ = crate::agent::drain_session_as(&lua, "trust_revoked", self.shutdown_grace()).await;
                // Wait for any active turn/request lease to finish or abort before reloading so
                // trust revocation cannot fail halfway with an active config/reload gate.
                if self.reload_gate.try_lock().is_err() {
                    let _ = tokio::time::timeout(self.shutdown_grace(), self.abort()).await;
                    let gate = self.reload_gate.lock().await;
                    drop(gate);
                }
                self.reload_all().await?;
            } else {
                self.workspace.set_trusted(false);
                mare.set("trusted", false)?;
            }
            false
        };
        if let Some(a) = &args {
            let arg_value = if effective {
                if session_only { mare_shared::SESSION_TRUST_SOURCE } else { "trusted" }
            } else if source == Some(mare_shared::SESSION_TRUST_SOURCE)
                && mare_config::trust::decision(&self.cwd).is_none()
            {
                "undecided"
            } else {
                "untrusted"
            };
            a.set("trust", arg_value)?;
            a.set("trust_pending", false)?;
        }
        if (save || matches!(source, Some("plugin" | "default")))
            && let Err(error) = self.workspace.sync_siblings(effective, saved).await
            && failure.is_none()
        {
            failure = Some(error);
        }
        self.workspace.update_config(self.kernel.config());
        let mut payload = serde_json::json!({ "trusted": effective, "saved": saved });
        if let Some(src) = source {
            payload["source"] = serde_json::json!(src);
        }
        self.kernel.emit_json("trust/changed", payload).await;
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Load a newly trusted project's protected resources into the running session (see
    /// [`Host::answer_trust`]).
    async fn load_trusted_project(self: &Arc<Self>) -> Result<()> {
        let mare: Table = self.lua().globals().get("mare")?;
        self.workspace.set_trusted(true);
        mare.set("trusted", true)?;
        if let Some(a) = mare.get::<Option<Table>>("args")? {
            a.set("trust", "trusted")?;
        }
        if self.reload.load_init
            && let Err(e) = self.reload_config().await
        {
            self.kernel.add_problem(format!("project config: {e:#}"));
            return Err(e.context("loading trusted project configuration"));
        }
        // the project's plugins join through the same resolution as startup (the same-name
        // rule, name slots, `plugins.preset`, the reserved name), with the config that now
        // includes the project layer
        let config = self.kernel.config();
        let mut failed: Vec<String> = Vec::new();
        if self.reload.load_init && !self.reload.no_user_plugins {
            let package_notices =
                crate::plugins::packages::install_packages(self.lua(), &config, &self.cancel, None).await;
            for n in &package_notices {
                self.kernel.add_problem(n.clone());
            }
            let unavail = plugin_failures::unavailable_packages(&self.kernel, &config, &package_notices);
            failed.extend(unavail.iter().filter(|f| f.blocks).map(|f| f.summary()));
            self.plugin_failures.borrow_mut().extend(unavail);
        }
        let sync = self.sync_definitions(&config).await;
        let project_plugins =
            mare_shared::project::resource_root(&self.cwd).mare_dir().map(|dir| dir.join(mare_shared::PLUGINS_KEY));
        failed.extend(
            sync.failures
                .iter()
                .filter(|failure| {
                    failure.blocks
                        && failure
                            .path
                            .as_ref()
                            .is_some_and(|path| project_plugins.as_ref().is_some_and(|dir| path.starts_with(dir)))
                })
                .map(|failure| failure.summary()),
        );
        self.plugin_failures.borrow_mut().extend(sync.failures);
        if self.reload.load_init {
            let init_problems = crate::plugins::packages::package_hooks(&self.kernel, "init", &config).await;
            for n in &init_problems {
                self.kernel.add_problem(n.clone());
            }
            let init_failures = plugin_failures::package_hook_failures(&self.kernel, &config, "init", &init_problems);
            failed.extend(init_failures.iter().filter(|f| f.blocks).map(|f| f.summary()));
            self.plugin_failures.borrow_mut().extend(init_failures);
        }
        for name in sync.replaced {
            if let Err(e) = self.kernel.reload(Some(&name)).await {
                self.kernel.add_problem(format!("{e}"));
                return Err(e.context("reloading trusted project plugin"));
            }
        }
        self.kernel.start().await?;
        failed.extend(
            plugin_failures::start_failures(&self.kernel, &self.kernel.plugin_tree().unwrap_or_default())
                .into_iter()
                .filter(|failure| {
                    failure.blocks
                        && self
                            .kernel
                            .def(&failure.plugin)
                            .is_some_and(|def| def.source.origin == mare_kernel::Origin::Project)
                })
                .map(|failure| failure.summary()),
        );
        if self.reload.load_init {
            let config_problems = crate::plugins::packages::package_hooks(&self.kernel, "config", &config).await;
            for n in &config_problems {
                self.kernel.add_problem(n.clone());
            }
            let config_failures =
                plugin_failures::package_hook_failures(&self.kernel, &config, "config", &config_problems);
            failed.extend(config_failures.iter().filter(|f| f.blocks).map(|f| f.summary()));
            self.plugin_failures.borrow_mut().extend(config_failures);
        }
        for name in self.context_service_plugins() {
            if let Err(e) = self.kernel.reload(Some(&name)).await {
                self.kernel.add_problem(format!("{e}"));
            }
        }
        self.sync_web_assets();
        if !failed.is_empty() {
            anyhow::bail!("trusted project plugins failed to start: {}", failed.join("; "));
        }
        Ok(())
    }

    /// Collect plugin names responsible for the `context` service (by service, not plugin name, so
    /// replacements under another name are reloaded too).
    fn context_service_plugins(&self) -> Vec<String> {
        self.kernel
            .def_names()
            .into_iter()
            .filter(|n| {
                self.kernel.def(n).is_some_and(|d| d.services().iter().any(|s| s == mare_shared::CONTEXT_SERVICE_NAME))
            })
            .collect()
    }

    /// Finish the trust decision once the user-level plugins run (
    /// `coding-agent/docs/security.md` order: command line → plugins → nearest saved decision →
    /// global default). Unless `--trust` / `--no-trust` decided:
    ///
    /// 1. `trust/resolve` (waterfall `{cwd, resources, saved, default, decision = nil}`): the first
    ///    listener that sets `decision` to true / false owns the decision. It is applied for this
    ///    session and not saved: a project it trusts loads now, one it distrusts while a saved
    ///    decision or the default had loaded it is reloaded without its resources.
    /// 2. otherwise a folder still undecided follows `trust_default` as configured (init.lua
    ///    included): "trust" loads it now, "distrust" keeps it untrusted without asking.
    ///
    /// Returns true when nothing decided and the default is to ask: the TUI shows the trust
    /// overlay, headless says the resources were skipped. Emits `trust/changed {trusted, saved =
    /// false, source = "plugin"|"default"}` when it decides.
    pub async fn settle_trust(self: &Arc<Self>) -> Result<bool> {
        let mare: Table = self.lua().globals().get("mare")?;
        let args: Option<Table> = mare.get("args")?;
        let arg_str = |k: &str| args.as_ref().and_then(|a| a.get::<Option<String>>(k).ok().flatten());
        let pending = args.as_ref().and_then(|a| a.get::<Option<bool>>("trust_pending").ok().flatten()) == Some(true);
        // older callers pass only `trust_pending`
        let source = arg_str("trust_source").unwrap_or_else(|| (if pending { "pending" } else { "" }).to_string());
        let undecided = matches!(source.as_str(), "pending" | "undecided");
        if self.reload.no_user_plugins || !(undecided || matches!(source.as_str(), "saved" | "default")) {
            return Ok(pending);
        }
        let config = self.kernel.config();
        let default = config
            .get("trust_default")
            .and_then(|v| v.as_str())
            .and_then(mare_config::trust::DefaultTrust::parse)
            .unwrap_or_default();
        if self.kernel.listener_count("trust/resolve") > 0 {
            let resources: serde_json::Map<String, serde_json::Value> = self
                .workspace
                .summarize_resources()
                .into_iter()
                .map(|(k, n)| (k.to_string(), serde_json::json!(n)))
                .collect();
            let ev = serde_json::json!({
                "cwd": self.cwd.to_string_lossy(),
                "resources": resources,
                "saved": self.workspace.trust_decision(),
                "default": default.name(),
            });
            let lua = self.lua();
            let v = mare_shared::lua::json_to_lua(lua, &ev)?;
            let out = self.kernel.waterfall("trust/resolve", v, MultiValue::new()).await;
            let decided = match out {
                Value::Table(t) => t.get::<Option<bool>>("decision").ok().flatten(),
                _ => None,
            };
            if let Some(trusted) = decided {
                self.apply_trust(trusted, "plugin", false).await?;
                return Ok(false);
            }
        }
        match (undecided, default) {
            (true, mare_config::trust::DefaultTrust::Trust) => {
                self.apply_trust(true, "default", false).await?;
            }
            (true, mare_config::trust::DefaultTrust::Distrust) => {
                self.apply_trust(false, "default", false).await?;
            }
            (true, mare_config::trust::DefaultTrust::Ask) => return Ok(true),
            (false, _) => {}
        }
        Ok(false)
    }

    /// A decision by a plugin, the default, or workspace registry applied to this host.
    /// `saved` says the workspace caller already persisted this decision.
    pub(crate) async fn apply_trust(self: &Arc<Self>, trusted: bool, source: &str, saved: bool) -> Result<()> {
        self.apply_trust_change(trusted, false, Some(source), saved).await
    }
}
