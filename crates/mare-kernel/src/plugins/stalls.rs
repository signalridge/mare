//! Plugins stuck in a lifecycle state, observed and reported, never forced (rutis reports a
//! plugin "still waiting" the same way and, like it, nothing here aborts or isolates a fiber: an
//! old generation may still hold what its cleanup releases).
//!
//! boot.lua `booted.stalled(ms)` stamps when every fiber entered its state and lists what has
//! been LOADING (its apply has not returned), UNLOADING (a cleanup has not finished) or PENDING (a
//! service it injects has no provider) for `stall_ms` or longer, with the labels of the effects
//! it still holds and the services it waits for. One report feeds `/plugins tree`,
//! `mare plugin tree --json`, the shutdown log and the watcher below (log + `kernel/stalled`).

use std::collections::HashSet;
use std::time::Duration;

use mlua::{Function, Table};
use tokio_util::sync::CancellationToken;

use crate::Kernel;

/// `stall_ms` when the config does not set it.
const DEFAULT_STALL: Duration = Duration::from_secs(10);
/// Minimum interval between stall-watch polls.
const STALL_TICK_MIN: Duration = Duration::from_millis(25);
/// Maximum interval between stall-watch polls (and fallback when stalls are off).
const STALL_TICK_MAX: Duration = Duration::from_secs(1);

/// A plugin left LOADING, UNLOADING or PENDING past `stall_ms`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StalledPlugin {
    pub plugin: String,
    /// `LOADING`, `UNLOADING` or `PENDING`
    pub state: String,
    /// how long it has been in that state
    pub elapsed_ms: u64,
    /// the labels of the effects its fibers still hold (`ctx:effect(fn, label)`)
    pub effects: Vec<String>,
    /// for PENDING: the injected services with no provider
    pub missing: Vec<String>,
}

impl StalledPlugin {
    /// One line for the log, the notice and `/plugins tree`.
    pub fn message(&self) -> String {
        let after = format!("{:.0}s", self.elapsed_ms as f64 / 1000.0);
        let quoted = |list: &[String]| list.iter().map(|e| format!("'{e}'")).collect::<Vec<_>>().join(", ");
        match self.state.as_str() {
            "LOADING" if self.effects.is_empty() => {
                format!("plugin {} still loading after {after}: its apply has not returned", self.plugin)
            }
            "LOADING" => format!(
                "plugin {} still loading after {after}: its apply has not returned (effects so far: {})",
                self.plugin,
                quoted(&self.effects)
            ),
            "UNLOADING" if self.effects.is_empty() => {
                format!("plugin {} still unloading after {after}: its cleanup has not finished", self.plugin)
            }
            "UNLOADING" => format!(
                "plugin {} still unloading after {after}: waiting for the cleanup of {}",
                self.plugin,
                quoted(&self.effects)
            ),
            _ => format!(
                "plugin {} still waiting after {after} for {}: it starts once that is provided",
                self.plugin,
                if self.missing.is_empty() { "a service".to_string() } else { self.missing.join(", ") }
            ),
        }
    }

    fn from_table(t: &Table) -> mlua::Result<Self> {
        Ok(Self {
            plugin: t.get("plugin")?,
            state: t.get("state")?,
            elapsed_ms: t.get::<f64>("ms")?.max(0.0) as u64,
            effects: t.get::<Option<Vec<String>>>("effects")?.unwrap_or_default(),
            missing: t.get::<Option<Vec<String>>>("missing")?.unwrap_or_default(),
        })
    }
}

/// The watcher's stop token, one per VM (cancelled when the tree closes).
struct StallWatch(CancellationToken);

impl Kernel {
    /// `stall_ms` from the config; `None` when it is 0 (the report is off).
    fn stall_threshold(&self) -> Option<Duration> {
        let cfg = self.0.config.lock().unwrap_or_else(|e| e.into_inner());
        match cfg.get("stall_ms").and_then(|v| v.as_f64()) {
            Some(ms) if ms.is_finite() && ms <= 0.0 => None,
            Some(ms) if ms.is_finite() => Duration::try_from_secs_f64(ms / 1000.0).ok(),
            _ => Some(DEFAULT_STALL),
        }
    }

    /// The plugins stuck in a lifecycle state for `stall_ms` or longer (empty when `stall_ms` is
    /// 0 or the tree has not booted).
    pub fn stalled(&self) -> Vec<StalledPlugin> {
        match self.stall_threshold() {
            Some(threshold) => self.stalled_past(threshold),
            None => vec![],
        }
    }

    /// [`Self::stalled`] with an explicit threshold (shutdown reports every unload in flight).
    pub(crate) fn stalled_past(&self, threshold: Duration) -> Vec<StalledPlugin> {
        let Ok(booted) = self.cordis_boot() else { return vec![] };
        let list = booted
            .get::<Function>("stalled")
            .and_then(|f| f.call::<Vec<Table>>(threshold.as_secs_f64() * 1000.0))
            .and_then(|items| items.iter().map(StalledPlugin::from_table).collect::<mlua::Result<Vec<_>>>());
        match list {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!("listing the stuck plugins failed: {e}");
                vec![]
            }
        }
    }

    /// Start the stall watcher once per VM: every tick (a quarter of `stall_ms`, between 25 ms
    /// and 1 s) it reads [`Self::stalled`]; a plugin newly stuck in a state is logged and
    /// announced once (`kernel/stalled {summary, plugin, state, ms, effects, missing}`, shown as a
    /// notice), and again only after it left that state and got stuck anew.
    pub(crate) fn start_stall_watch(&self) {
        if self.lua().app_data_ref::<StallWatch>().is_some() {
            return;
        }
        let cancel = CancellationToken::new();
        let _ = self.lua().set_app_data(StallWatch(cancel.clone()));
        let weak = self.weak();
        mare_shared::txn_scope::spawn_local(async move {
            let mut reported: HashSet<(String, String)> = HashSet::new();
            loop {
                let tick = {
                    let Some(k) = Self::from_weak(&weak) else { return };
                    k.stall_threshold().map(|t| (t / 4).clamp(STALL_TICK_MIN, STALL_TICK_MAX)).unwrap_or(STALL_TICK_MAX)
                };
                let mut wait = tokio::time::interval_at(tokio::time::Instant::now() + tick, tick);
                wait.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = wait.tick() => {}
                }
                let Some(k) = Self::from_weak(&weak) else { return };
                let now = k.stalled();
                let current: HashSet<(String, String)> =
                    now.iter().map(|s| (s.plugin.clone(), s.state.clone())).collect();
                for s in now.iter().filter(|s| !reported.contains(&(s.plugin.clone(), s.state.clone()))) {
                    k.announce_stall(s).await;
                }
                reported = current;
            }
        });
    }

    /// Stop the stall watcher (the tree is closing: what unloads now is the shutdown's to report).
    pub(crate) fn stop_stall_watch(&self) {
        if let Some(watch) = self.lua().app_data_ref::<StallWatch>() {
            watch.0.cancel();
        }
    }

    async fn announce_stall(&self, s: &StalledPlugin) {
        let message = s.message();
        tracing::warn!(plugin = %s.plugin, state = %s.state, effects = ?s.effects, missing = ?s.missing, "{message}");
        let lua = self.lua();
        let Ok(t) = lua.create_table() else { return };
        let _ = t.set("summary", message);
        let _ = t.set("plugin", s.plugin.as_str());
        let _ = t.set("state", s.state.as_str());
        let _ = t.set("ms", s.elapsed_ms);
        let _ = t.set("effects", lua.create_sequence_from(s.effects.iter().map(String::as_str)).ok());
        let _ = t.set("missing", lua.create_sequence_from(s.missing.iter().map(String::as_str)).ok());
        self.emit("kernel/stalled", mlua::MultiValue::from_vec(vec![mlua::Value::Table(t)])).await;
    }
}

#[cfg(test)]
mod tests {
    use super::StalledPlugin;

    fn s(state: &str, effects: &[&str], missing: &[&str]) -> StalledPlugin {
        StalledPlugin {
            plugin: "foo".into(),
            state: state.into(),
            elapsed_ms: 12_400,
            effects: effects.iter().map(|e| e.to_string()).collect(),
            missing: missing.iter().map(|e| e.to_string()).collect(),
        }
    }

    #[test]
    fn messages_name_the_state_the_time_and_what_holds_it() {
        assert_eq!(
            s("UNLOADING", &["watcher"], &[]).message(),
            "plugin foo still unloading after 12s: waiting for the cleanup of 'watcher'"
        );
        assert_eq!(
            s("UNLOADING", &[], &[]).message(),
            "plugin foo still unloading after 12s: its cleanup has not finished"
        );
        assert_eq!(s("LOADING", &[], &[]).message(), "plugin foo still loading after 12s: its apply has not returned");
        assert_eq!(
            s("PENDING", &[], &["llm", "tools"]).message(),
            "plugin foo still waiting after 12s for llm, tools: it starts once that is provided"
        );
    }
}
