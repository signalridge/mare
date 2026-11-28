//! The configuration's runtime surface, installed after startup: `mare.on` subscriptions,
//! `option/changed`, `mare.reload_config` and the config sources / warnings.

use std::sync::Arc;

use anyhow::Result;
use mlua::{Function, Table, Value};

use crate::Host;

impl Host {
    /// After startup: register `mare.on` subscriptions from init.lua on the root ctx, make
    /// `mare.o.x = v` emit `option/changed`, and expose `mare.reload_config` / `config_sources` / `config_warnings`.
    pub(crate) fn install_config_runtime(self: &Arc<Self>) -> Result<()> {
        let lua = self.lua().clone();
        let mare: Table = lua.globals().get("mare")?;
        mare_shared::lua::core::config::set_layer(&lua, "runtime")?;
        mare_shared::lua::core::config::set_layer_runtime(&lua, Value::String(lua.create_string("runtime")?))?;
        // init.lua's `mare.on` / `mare.policy.voter`: native registrations on the host's fiber
        // and a voter when it comes with its options
        let k = self.kernel.clone();
        let on_now = lua.create_function(move |_, (event, f, voter): (String, Function, Option<Table>)| {
            let (ctx, owner) = (k.host_ctx()?, k.host_owner()?);
            match voter {
                None => k.host_on(&ctx, owner, &event, f, mare_kernel::HostOn::default()),
                Some(o) => {
                    let opts = mare_kernel::HostVoter {
                        prepend: false,
                        timeout: o
                            .get::<Option<f64>>("timeout_ms")?
                            .map(|ms| std::time::Duration::from_millis(ms.max(0.0).ceil() as u64)),
                    };
                    k.host_voter(&ctx, owner, &event, f, opts)
                }
            }
        })?;
        mare_shared::lua::core::config::set_on_now(&lua, on_now.clone())?;
        // every release made is kept (the next reload withdraws them) even when a later entry fails
        let unsubs = lua.create_table()?;
        let mut failed = None;
        if let Some(pending) = mare_shared::lua::core::config::pending_on(&lua)? {
            for e in pending.sequence_values::<Table>() {
                let installed = e.and_then(|e| {
                    on_now.call::<Function>((e.get::<String>(1)?, e.get::<Function>(2)?, e.get::<Option<Table>>(3)?))
                });
                match installed {
                    Ok(off) => unsubs.push(off)?,
                    Err(error) => failed = failed.or(Some(error)),
                }
            }
            mare_shared::lua::core::config::set_pending_on(&lua, lua.create_table()?)?;
        }
        mare_shared::lua::core::config::set_on_unsub(&lua, Value::Table(unsubs))?;
        if let Some(error) = failed {
            return Err(error.into());
        }
        // the runtime config writer: one queue, a worker and a notifier (`txn.rs`
        // 6 条); `option/changed` is emitted only by the notifier
        let notifier = super::txn::install(self, &lua)?;
        mare_plugin::engines::config::load_config_txn(&lua, &mare)?;
        let k = self.kernel.clone();
        let weak_ws = Arc::downgrade(self);
        mare_shared::lua::core::config::set_option_changed(
            &lua,
            Value::Function(lua.create_function(move |lua, (name, value): (String, Value)| {
                // keep the kernel's config copy current so plugins (re)started later see the
                // runtime value, not the one init.lua produced
                let mut cfg = k.config();
                if let Some(o) = cfg.as_object_mut() {
                    o.insert(name.clone(), mare_shared::lua::lua_to_json(lua, value.clone())?);
                    if let Some(h) = weak_ws.upgrade() {
                        super::commit_kernel_config(&h.kernel, &h.workspace, cfg);
                    } else {
                        k.set_config(cfg);
                    }
                }
                // announced through the one redactor; the kernel copy above keeps the real value
                let redact: Function = lua.globals().get::<Table>("mare")?.get("config_redact")?;
                let (safe, redacted): (Value, bool) = redact.call((name.as_str(), value))?;
                let payload = lua.create_table()?;
                payload.set("name", name)?;
                payload.set("value", safe)?;
                if redacted {
                    payload.set("redacted", true)?;
                }
                notifier.announce(payload);
                Ok(())
            })?),
        )?;
        let weak = Arc::downgrade(self);
        let weak_all = Arc::downgrade(self);
        let reload_fn = lua.create_async_function(move |lua, ()| {
            let weak = weak_all.clone();
            async move {
                let Some(h) = weak.upgrade() else { return Ok((true, Value::Nil)) };
                let gate = match h.try_lock_reload_gate() {
                    Ok(gate) => gate,
                    Err(err) => return reload_error_outcome(&lua, err),
                };
                let _allow = mare_kernel::allow_provider_reload();
                // detached: the reload disposes the plugin that asked for it (and its task)
                let job = tokio::task::spawn_local(async move { h.reload_all_with_gate(gate).await });
                let r = match job.await {
                    Ok(res) => res,
                    Err(err) => return reload_error_outcome(&lua, anyhow::Error::new(err)),
                };
                drop(_allow);
                match r {
                    Ok(r) => Ok((true, mare_shared::lua::json_to_lua(&lua, &r)?)),
                    Err(err) => reload_error_outcome(&lua, err),
                }
            }
        })?;
        mare.set("reload", mare_shared::lua_error::wrap_coded(&lua, "@config/reload_wrap", reload_fn)?)?;
        let reload_config_fn = lua.create_async_function(move |lua, ()| {
            let weak = weak.clone();
            async move {
                let Some(h) = weak.upgrade() else { return Ok((true, Value::Nil)) };
                match h.reload_config().await {
                    Ok(r) => Ok((true, mare_shared::lua::json_to_lua(&lua, &r)?)),
                    Err(err) => reload_error_outcome(&lua, err),
                }
            }
        })?;
        mare.set("reload_config", mare_shared::lua_error::wrap_coded(&lua, "@config/reload_wrap", reload_config_fn)?)?;
        Ok(())
    }
}

fn strip_lua_traceback(text: &str) -> String {
    let head = text.split("\nstack traceback:").next().unwrap_or(text);
    let mut cleaned = Vec::new();
    for (i, line) in head.lines().enumerate() {
        let mut s = line;
        if i == 0 {
            while let Some(rest) = s.strip_prefix("runtime error: ") {
                s = rest;
            }
        }
        cleaned.push(s);
    }
    mare_shared::errors::bounded(cleaned.join("\n").trim())
}

fn reload_error_outcome(lua: &mlua::Lua, err: anyhow::Error) -> mlua::Result<(bool, Value)> {
    if let Some(coded) = err.chain().find_map(|c| c.downcast_ref::<mare_shared::errors::Coded>()) {
        let tbl = mare_shared::lua_error::error_value(
            lua,
            coded.code,
            &mare_shared::errors::bounded(&coded.message),
            Value::Nil,
        )?;
        return Ok((false, Value::Table(tbl)));
    }
    if let Some(known) = err.chain().find_map(|c| c.downcast_ref::<mare_kernel::ServiceCallError>()) {
        let tbl = mare_shared::lua_error::error_value(
            lua,
            known.code(),
            &mare_shared::errors::bounded(&known.message()),
            Value::Nil,
        )?;
        return Ok((false, Value::Table(tbl)));
    }
    let text = format!("{err:#}");
    if let Some(tbl) = mare_shared::lua_error::recall_error(lua, &text) {
        return Ok((false, Value::Table(tbl)));
    }
    let clean = strip_lua_traceback(&text);
    let tbl = mare_shared::lua_error::error_value(lua, "internal", &clean, Value::Nil)?;
    Ok((false, Value::Table(tbl)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HostOptions;

    #[tokio::test]
    async fn installed_voter_timeout_rounds_up_and_preserves_zero() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let host = Host::new(HostOptions {
                    cwd: std::env::temp_dir(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let lua = host.lua();
                lua.load(
                    r#"for _, ms in ipairs({0.125, 0, 1.25, 1}) do
                        mare.policy.voter("tool/before", function() end, {timeout_ms = ms})
                    end"#,
                )
                .exec()
                .unwrap();
                let lists: Function = lua.named_registry_value("mare.policy.lists").unwrap();
                let voters: Table = lists.call(("voters", "tool/before")).unwrap();
                let host_voters: Vec<Table> = voters
                    .sequence_values::<Table>()
                    .map(Result::unwrap)
                    .filter(|entry| entry.get::<Option<bool>>("host").unwrap() == Some(true))
                    .collect();
                let timeouts: Vec<f64> =
                    host_voters[host_voters.len() - 4..].iter().map(|entry| entry.get("timeout_ms").unwrap()).collect();
                assert_eq!(timeouts, [1.0, 0.0, 2.0, 1.0]);
                host.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn runtime_config_mutations_sync_workspace_config_snapshot() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let host = Host::new(HostOptions {
                    cwd: std::env::temp_dir(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let lua = host.lua();
                mare_shared::lua::core::config::option_changed(lua)
                    .unwrap()
                    .unwrap()
                    .call::<()>(("model", "faux/echo"))
                    .unwrap();
                mare_shared::lua::core::config::plugin_kernel_sync_fn(lua)
                    .unwrap()
                    .call::<()>(("demo", "flag", true))
                    .unwrap();
                assert_eq!(host.kernel.config()["model"], "faux/echo");
                assert_eq!(host.workspace.config_snapshot.borrow()["model"], "faux/echo");
                assert_eq!(host.kernel.config()["plugins"]["demo"]["flag"], true);
                assert_eq!(host.workspace.config_snapshot.borrow()["plugins"]["demo"]["flag"], true);
                host.shutdown().await;
            })
            .await;
    }
}
