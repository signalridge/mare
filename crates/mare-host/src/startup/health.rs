//! What failed and whether requests are blocked: plugin failures, warnings, startup notices and
//! the reload policy's reason.

use crate::Host;
use crate::plugins::plugin_failures;
use crate::plugins::plugin_failures::PluginFailure;

impl Host {
    /// Startup's plugin failures that don't block requests, one warning each, until
    /// a full /reload completes; headless and RPC print them to stderr once.
    pub fn plugin_warnings(&self) -> Vec<String> {
        self.failures().iter().filter(|f| !f.blocks).map(PluginFailure::notice).collect()
    }

    /// A config file (init.lua) that failed at startup, as one line: headless and RPC have no
    /// way to fix it from inside, so they report it and exit.
    pub fn config_error(&self) -> Option<String> {
        let errors: Vec<String> =
            self.plugin_failures.borrow().iter().filter(|f| f.phase == "config").map(PluginFailure::summary).collect();
        (!errors.is_empty())
            .then(|| format!("{} — fix it and run again (or start clean: mare --clean)", errors.join("; ")))
    }

    /// Every failure that counts now: the recorded config files and definitions, and the defined
    /// plugins that aren't active. While a full reload runs, the failures as they were when it
    /// started (a half-reloaded tree has no meaningful answer); live again before `reload/completed`.
    pub(crate) fn failures(&self) -> Vec<PluginFailure> {
        if let Some(failures) = self.reload_failures.borrow().as_ref() {
            return failures.clone();
        }
        self.failures_in(&self.kernel.plugin_tree().unwrap_or_default())
    }

    /// [`Self::failures`], live, over a plugin tree snapshot the caller already took.
    pub(crate) fn failures_in(&self, tree: &[mare_kernel::PluginTreeRow]) -> Vec<PluginFailure> {
        let mut out = self.plugin_failures.borrow().clone();
        out.extend(plugin_failures::start_failures(&self.kernel, tree));
        out
    }

    pub(crate) fn failure_for(&self, name: &str, tree: &[mare_kernel::PluginTreeRow]) -> Vec<PluginFailure> {
        let mut out: Vec<PluginFailure> =
            self.plugin_failures.borrow().iter().filter(|f| f.plugin == name).cloned().collect();
        if let Some(f) = plugin_failures::start_failure_for(&self.kernel, name, tree) {
            out.push(f);
        }
        out
    }

    /// Why requests are refused because something failed, if they are.
    pub(crate) fn blocked_reason(&self) -> Option<String> {
        plugin_failures::blocked_reason(&self.startup_blocked.borrow(), &self.failures(), self.reloaded.get())
    }

    /// What the frontend shows once at startup, each with its level: one short line per failed
    /// plugin or config file (the reason requests are blocked, if they are), then the other
    /// problems. Tracebacks stay in `/plugins`, `mare doctor` and the log.
    pub fn startup_notices(&self) -> Vec<(String, &'static str)> {
        let blocked = self.blocked_reason();
        plugin_failures::startup_notices(&self.failures(), blocked.as_deref(), self.kernel.problems())
    }

    /// The built-in agent and tools consult these policies before model or tool execution.
    /// A failed or incomplete config-driven plugin reload must not silently use mixed state.
    pub(crate) fn reload_reason(&self) -> Option<String> {
        if let Some(reason) = self.blocked_reason() {
            Some(reason)
        } else if self.reload_active.get() {
            Some("configuration reload is in progress; retry afterwards".to_string())
        } else if self.pending_session_trust_revoke.get() {
            Some("session-only trust revocation is pending or failed; retry or /reload".to_string())
        } else if self.reload_full_incomplete.get() {
            Some("full reload is incomplete; fix its errors and retry /reload".to_string())
        } else {
            let pending = self.reload_pending.borrow();
            if !pending.is_empty() {
                Some(format!(
                    "plugin reload is incomplete ({}); fix it and retry /reload config",
                    pending.iter().cloned().collect::<Vec<_>>().join(", ")
                ))
            } else {
                let incomplete = self.kernel.reload_incomplete();
                (!incomplete.is_empty()).then(|| {
                    format!("plugin reload is incomplete ({}); fix it and retry /reload", incomplete.join(", "))
                })
            }
        }
    }
}
