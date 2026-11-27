//! Starting and closing the plugin tree, the kernel-held ctxs' disposal, each plugin's merged
//! config.

use mlua::{Function, Table, Value};

use crate::{Kernel, PluginDef};

/// Scheduler turns the tree's close still gets once the shutdown budget is spent (
/// [`Kernel::shutdown_until`]): enough for every cleanup's synchronous part, never a wait.
const CLOSE_TURNS_PAST_DEADLINE: usize = 64;

impl Kernel {
    /// Start the plugin tree: boot.lua's `booted.start`, one cordis fiber per
    /// enabled row. Failures are recorded, never fatal.
    pub async fn start(&self) -> anyhow::Result<()> {
        self.start_with_tree().await.map(|_| ())
    }

    /// Start the plugin tree and return the single post-start tree dump so callers do not need
    /// to invoke `loader:dump()` a second time immediately after startup.
    pub async fn start_with_tree(&self) -> anyhow::Result<Vec<crate::PluginTreeRow>> {
        self.start_cordis().await
    }

    /// Start the plugin tree through boot.lua (`booted.start`): one cordis fiber per enabled row.
    async fn start_cordis(&self) -> anyhow::Result<Vec<crate::PluginTreeRow>> {
        let booted = self.cordis_boot().map_err(|e| anyhow::anyhow!("cordis boot failed: {e}"))?;
        let start: Function = booted.get("start")?;
        let report: Table = mare_shared::lua::call_async(self.lua(), &start, ()).await?;
        let failures: Vec<String> = report.get::<Option<Vec<String>>>("failures")?.unwrap_or_default();
        for msg in failures {
            tracing::warn!("{msg}");
            self.st().problems.push(msg);
        }
        // a plugin left PENDING on a missing required service is named with its service
        let pending: Vec<String> = report.get::<Option<Vec<String>>>("pending")?.unwrap_or_default();
        if !pending.is_empty() {
            let mut st = self.st();
            for msg in pending {
                tracing::warn!("{msg}");
                if !st.problems.contains(&msg) {
                    st.problems.push(msg);
                }
            }
        }
        let dump: Vec<Table> = report.get::<Option<Vec<Table>>>("dump")?.unwrap_or_default();
        // from here on a plugin stuck loading, unloading or waiting is reported (plugins/stalls.rs)
        self.start_stall_watch();
        Ok(super::tree::rows_from_tables(&dump)?)
    }

    /// The plugin's defaults with the user's options on top, as JSON (`rows.rs` compares it to keep
    /// an unchanged config the same table and converts it directly on a cache miss).
    pub(crate) fn plugin_config_json_with(
        &self,
        d: &PluginDef,
        failed: Option<&Table>,
        plugins: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> mlua::Result<serde_json::Value> {
        let lua = self.lua();
        // an invalid option declared `on_invalid = "fail"` keeps the plugin from starting rather
        // than falling back to its default (a restrictive policy never silently becomes `ask`)
        if let Some(why) = failed.and_then(|f| f.get::<Option<String>>(d.name.as_str()).ok().flatten()) {
            return Err(mlua::Error::runtime(format!("invalid configuration: {why}")));
        }
        // the declaration's defaults
        let schema: Value = d.table.get("config_schema")?;
        let user = plugins.and_then(|p| p.get(&d.name));
        let has_schema = matches!(schema, Value::Table(_));
        if !has_schema && user.is_none() {
            return Ok(serde_json::Value::Object(Default::default()));
        }
        let mut merged = match schema {
            Value::Table(schema) => {
                let defaults = Value::Table(mare_shared::config_schema::defaults(lua, &schema)?);
                mare_shared::lua::config_to_json(lua, defaults)?
            }
            _ => serde_json::Value::Object(Default::default()),
        };
        // option by option, the same rule a runtime write uses ([`effective_option`])
        if let (Some(serde_json::Value::Object(user)), Some(out)) = (user, merged.as_object_mut()) {
            for (k, committed) in user {
                if let Some(v) = effective_option(out.remove(k), Some(committed.clone())) {
                    out.insert(k.clone(), v);
                }
            }
        }
        Ok(merged)
    }

    pub async fn dispose_ctx(&self, id: u64) {
        for token in self.dispose_ctx_defer_callbacks(id).await {
            token.cancel();
        }
    }

    /// Dispose a context and run its registration disposers, returning callback lifetime tokens so
    /// a disconnecting transport can cancel its own pending frames (`"disconnected"`) first.
    pub async fn dispose_ctx_defer_callbacks(&self, id: u64) -> Vec<tokio_util::sync::CancellationToken> {
        // A reload caller can be cancelled while a Lua disposal hook yields. Run cleanup in a
        // detached local task and let a retry join it instead of treating a half-disposed context
        // as active or ready. The reload failure marker remains set until the retry completes.
        let mut done = {
            let mut disposals = self.0.disposals.lock().unwrap();
            if let Some(done) = disposals.get(&id) {
                done.clone()
            } else {
                let (send, recv) = tokio::sync::watch::channel(None);
                disposals.insert(id, recv.clone());
                let kernel = self.clone();
                mare_shared::txn_scope::spawn_local(async move {
                    struct Finished {
                        kernel: Kernel,
                        id: u64,
                        send: tokio::sync::watch::Sender<Option<Vec<tokio_util::sync::CancellationToken>>>,
                    }
                    impl Drop for Finished {
                        fn drop(&mut self) {
                            let _ = self.send.send_if_modified(|value| {
                                if value.is_none() {
                                    *value = Some(Vec::new());
                                    true
                                } else {
                                    false
                                }
                            });
                            self.kernel.0.disposals.lock().unwrap().remove(&self.id);
                        }
                    }
                    let finished = Finished { kernel: kernel.clone(), id, send };
                    let tokens = kernel.dispose_ctx_inner(id).await;
                    finished.send.send_replace(Some(tokens));
                });
                recv
            }
        };
        let _ = done.wait_for(|finished| finished.is_some()).await;
        done.borrow().clone().unwrap_or_default()
    }

    async fn dispose_ctx_inner(&self, id: u64) -> Vec<tokio_util::sync::CancellationToken> {
        let (registrations, owner_disposers, lifetime_tokens, plugin) = {
            let mut st = self.st();
            let Some(c) = st.ctxs.get_mut(&id) else {
                return Vec::new();
            };
            if c.disposed {
                return Vec::new();
            }
            c.disposed = true;
            c.cancel.cancel();
            let held = std::mem::take(&mut c.registrations);
            let owner_disposers = std::mem::take(&mut c.owner_disposers);
            let plugin = c.plugin.clone();
            // Cancel callback lifetimes only after `release_all` finishes running registration
            // disposers, so waiters awakened by callback cancellation never race ahead of Lua cleanup.
            let lifetime_tokens: Vec<_> =
                held.keys().filter_map(|reg_id| st.callback_lifetimes.remove(reg_id)).collect();
            st.callback_channels.remove(&id);
            st.caller_access.remove(&id);
            let r: Vec<_> = held.into_values().collect();
            (r, owner_disposers, lifetime_tokens, plugin)
        };
        for (_, dispose) in owner_disposers {
            if let Err(error) = dispose.call::<()>(()) {
                tracing::warn!("owner disposer error: {error}");
            }
        }
        for entry in self.st().services.values().filter(|entry| entry.ctx == id) {
            entry.lifetime.cancel();
        }
        // Registrations held for JSON callers go with their owner; ones that failed earlier get
        // another attempt.
        self.release_all(&plugin, registrations).await;
        self.retry_releases().await;
        self.release_baselines_for_owner(id);
        let mut st = self.st();
        st.services.retain(|_, entry| entry.ctx != id);
        st.ctxs.remove(&id);
        lifetime_tokens
    }

    /// Close the plugin tree for good (`root:close()`: every fiber unloads, their
    /// asynchronous cleanup awaited, later registrations refused), release what the kernel still
    /// holds for JSON callers, and stop the Lua watchdog task.
    pub async fn shutdown(&self) {
        let _ = self.shutdown_until(tokio::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)).await;
    }

    /// [`Kernel::shutdown`] bounded by `deadline`: the tree's close, the release retries and the
    /// watchdog's stop are each waited for until the deadline; one that does not finish in time
    /// never keeps the next from starting. Returns what did not confirm: the plugins still
    /// unloading, then "release retries" / "watchdog".
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> Vec<String> {
        let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
        let mut unconfirmed = Vec::new();
        self.stop_stall_watch();
        self.release_all_baselines();
        // close the tree for good (`root:close()`), every fiber unloading — session
        // writers, jobs and the rest release here — and its asynchronous cleanup awaited until the
        // deadline; past it, the plugins whose fibers have not unloaded are reported
        // (booted.unsettled()) and their cleanup goes on without us waiting
        let mut close = std::pin::pin!(self.close_cordis_tree());
        if tokio::time::timeout(left(), close.as_mut()).await.is_err() {
            // past the deadline (an earlier stage may have used the whole budget) the close still
            // runs: it is given
            // CLOSE_TURNS_PAST_DEADLINE scheduler turns, no timers, so every cleanup runs its
            // synchronous part, as the old path's release did; a cleanup that waits on something
            // is not waited for and is reported unsettled
            let mut done = false;
            for _ in 0..CLOSE_TURNS_PAST_DEADLINE {
                if tokio::time::timeout(std::time::Duration::ZERO, close.as_mut()).await.is_ok() {
                    done = true;
                    break;
                }
                tokio::task::yield_now().await;
            }
            if !done {
                unconfirmed.extend(self.cordis_unsettled());
                // the same report a stall gets: which cleanups each unconfirmed plugin still runs
                for s in self.stalled_past(std::time::Duration::ZERO).iter().filter(|s| s.state == "UNLOADING") {
                    tracing::warn!(plugin = %s.plugin, effects = ?s.effects, "{}", s.message());
                }
            }
        }
        // no connection outlives the tree: what the thread keeps for this kernel's remotes goes
        self.forget_remote();
        if tokio::time::timeout(left(), self.retry_releases()).await.is_err() {
            unconfirmed.push("release retries".into());
        }
        if tokio::time::timeout(left(), crate::lua_env::watchdog::stop_watchdog(self.lua())).await.is_err() {
            unconfirmed.push("watchdog".into());
        }
        unconfirmed
    }

    /// `root:close()` on the booted tree, awaited; a tree that never booted has nothing to close.
    async fn close_cordis_tree(&self) {
        let Ok(booted) = self.cordis_boot() else { return };
        let close: mlua::Result<Function> = self
            .lua()
            .load("local booted = ... return function() booted.close():await() end")
            .set_name("=kernel shutdown")
            .call(booted);
        let result = match close {
            Ok(close) => mare_shared::lua::call_async::<()>(self.lua(), &close, ()).await,
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            tracing::warn!("closing the plugin tree failed: {e}");
        }
    }

    /// The plugins whose fibers have not finished unloading since the tree began closing
    /// (`booted.unsettled()`, plugin row names, host plugins included); empty off the tree.
    fn cordis_unsettled(&self) -> Vec<String> {
        let Ok(booted) = self.cordis_boot() else { return vec![] };
        match booted.get::<Function>("unsettled").and_then(|f| f.call::<Vec<String>>(())) {
            Ok(names) => names,
            Err(e) => {
                tracing::warn!("listing the plugins still unloading failed: {e}");
                vec!["plugin tree".into()]
            }
        }
    }

    /// The value plugin `name` runs with for option `key` when its committed value is `committed`
    /// (`None`: absent): its declared default with the committed value merged in, exactly as at
    /// startup (after a runtime write the running `cfg[key]` equals what a
    /// restart would compute). `None` when neither exists.
    pub fn plugin_option_effective(
        &self,
        name: &str,
        key: &str,
        committed: Option<serde_json::Value>,
    ) -> mlua::Result<Option<serde_json::Value>> {
        let lua = self.lua();
        let default = match self.def(name).map(|d| d.table.get::<Value>("config_schema")).transpose()? {
            Some(Value::Table(schema)) => {
                let defaults = mare_shared::config_schema::defaults(lua, &schema)?;
                match defaults.get::<Value>(key)? {
                    Value::Nil => None,
                    v => Some(mare_shared::lua::config_to_json(lua, v)?),
                }
            }
            _ => None,
        };
        Ok(effective_option(default, committed))
    }
}

/// One option's effective value: the committed value merged into the declared default (objects by
/// key, recursively; anything else, lists included, replaces); either alone when the other is
/// absent.
pub fn effective_option(
    default: Option<serde_json::Value>,
    committed: Option<serde_json::Value>,
) -> Option<serde_json::Value> {
    match (default, committed) {
        (Some(mut d), Some(c)) => {
            merge_json(&mut d, c);
            Some(d)
        }
        (d, c) => c.or(d),
    }
}

fn merge_json(base: &mut serde_json::Value, other: serde_json::Value) {
    match (base, other) {
        (serde_json::Value::Object(b), serde_json::Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) if bv.is_object() && v.is_object() => merge_json(bv, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use crate::Kernel;
    use mlua::Lua;

    #[tokio::test]
    async fn dispose_ctx_handles_missing_double_dispose_and_throwing_owner_disposer() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                assert!(k.dispose_ctx_inner(999_999).await.is_empty());

                let id = k.host_owner().unwrap();
                let bad_disposer = k
                    .lua()
                    .create_function(|_, ()| -> mlua::Result<()> { Err(mlua::Error::runtime("disposer boom")) })
                    .unwrap();
                k.st().ctxs.get_mut(&id).unwrap().owner_disposers.insert(1, bad_disposer);
                let _ = k.dispose_ctx_inner(id).await;
                assert!(k.dispose_ctx_inner(id).await.is_empty());
            })
            .await;
    }
}
