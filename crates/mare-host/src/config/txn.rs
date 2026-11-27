//! The runtime config writer.
//!
//! Every runtime write (`mare.plugin`, `mare.config_clear`, RPC `options.set`) is a job submitted
//! to one host-owned queue and answered on a one-shot channel. A worker task runs the jobs one at a
//! time (`mare._config_run`, Lua: stage → owner apply → publish → settle | abort); a notifier task
//! announces `option/changed` — the only place it is emitted — and then answers. Nothing is held
//! by the caller, so a caller that is cancelled only drops its reply channel. The core never
//! branches on a plugin's name.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use mlua::{MultiValue, Table, Value};
use tokio::sync::{mpsc, oneshot};

use crate::Host;
use mare_shared::txn_scope;

/// What a job's caller gets back: `true`, or `nil, message, info`.
type Reply = oneshot::Sender<Vec<Value>>;

struct Job {
    job: Value,
    reply: Reply,
}

/// Announcements of one finished write (or of a direct core option write), then its answer.
struct Notice {
    notices: Vec<Table>,
    reload: Vec<String>,
    reply: Option<(Reply, Vec<Value>)>,
}

fn payload(lua: &mlua::Lua, path: &str, value: Value) -> mlua::Result<MultiValue> {
    let t = lua.create_table()?;
    t.set("name", path)?;
    t.set("value", value)?;
    Ok(MultiValue::from_vec(vec![Value::Table(t)]))
}

/// The notifier queue, shared with `mare.o.x = v`'s announcement (`runtime.rs`).
#[derive(Clone)]
pub(super) struct Notifier(mpsc::UnboundedSender<Notice>);

impl Notifier {
    /// Announce `{ name, value }` (already redacted by the caller) without an answer.
    pub(super) fn announce(&self, notice: Table) {
        let _ = self.0.send(Notice { notices: vec![notice], reload: vec![], reply: None });
    }
}

fn start_notifier(host: Weak<Host>) -> Notifier {
    let (tx, mut rx) = mpsc::unbounded_channel::<Notice>();
    tokio::task::spawn_local(async move {
        while let Some(n) = rx.recv().await {
            let Some(h) = host.upgrade() else { break };
            let lua = h.lua().clone();
            let k = h.kernel.clone();
            drop(h);
            // each batch in its own task, started in order: the notifier never waits on a
            // listener, so a listener that writes config and waits for it does not wait on the
            // notifier (its write's announcements start while the outer listener waits)
            tokio::task::spawn_local(announce(lua, k, n));
        }
    });
    Notifier(tx)
}

/// Emit `option/changed`, reload affected plugins, and answer the writer (truthfully returning any
/// reload error without rolling back the committed config).
async fn announce(lua: mlua::Lua, k: mare_kernel::Kernel, n: Notice) {
    for t in n.notices {
        k.emit("option/changed", MultiValue::from_vec(vec![Value::Table(t)])).await;
    }
    let definitions = k.def_names();
    let cfg = k.config();
    let mut reload_err = None;
    for name in &n.reload {
        let enabled = cfg
            .get(mare_shared::PLUGINS_KEY)
            .and_then(|p| p.get(name))
            .and_then(|p| p.get("enabled"))
            .and_then(|v| v.as_bool())
            != Some(false);
        if k.plugin_active(name) || (enabled && definitions.iter().any(|d| d == name)) {
            let err = match k.reload_report(Some(name)).await {
                Ok(report) if report.failures.is_empty() => None,
                Ok(report) => Some(report.failures.join("; ")),
                Err(e) => Some(e.to_string()),
            };
            if let Some(err) = err {
                if reload_err.is_none() {
                    reload_err = Some(format!("plugin {name} reload failed: {err}"));
                }
                if n.reply.is_none() {
                    k.add_problem(format!("plugin {name} reload after a runtime option write failed: {err}"));
                }
            }
        }
    }
    if let Some((reply, values)) = n.reply {
        if let Some(err) = reload_err {
            let _ = reply.send(failure(&lua, &err));
        } else {
            let _ = reply.send(values);
        }
    }
}

pub(super) fn install(host: &Arc<Host>, lua: &mlua::Lua) -> mlua::Result<Notifier> {
    let notifier = start_notifier(Arc::downgrade(host));
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel::<Job>();
    // the number of the transaction the worker is running (0: idle)
    let running = Rc::new(Cell::new(0u64));
    let worker_rx = Rc::new(std::cell::RefCell::new(Some(jobs_rx)));
    {
        let (running, worker_rx, notifier, host) = (running.clone(), worker_rx, notifier.clone(), Arc::downgrade(host));
        mare_shared::lua::core::config::set_config_submit(
            lua,
            lua.create_async_function(move |lua, job: Value| {
                let (running, worker_rx, notifier, host, jobs_tx) =
                    (running.clone(), worker_rx.clone(), notifier.clone(), host.clone(), jobs_tx.clone());
                async move {
                    // a write from inside an owner's `option/apply` of the running write would wait
                    // on itself: refused at once (an expired number — that write already ended —
                    // queues normally)
                    if txn_scope::current().is_some_and(|id| id == running.get()) {
                        return Ok(MultiValue::from_vec(vec![
                            Value::Nil,
                            Value::String(lua.create_string(
                                "config write inside option/apply of the same write; do it from option/changed or after the write returns",
                            )?),
                        ]));
                    }
                    // the worker starts with the first job (inside the local task set by then)
                    if let Some(rx) = worker_rx.borrow_mut().take() {
                        start_worker(host, rx, running.clone(), notifier);
                    }
                    let (tx, rx) = oneshot::channel();
                    let _ = jobs_tx.send(Job { job, reply: tx });
                    match rx.await {
                        Ok(values) => Ok(MultiValue::from_vec(values)),
                        Err(_) => Err(mlua::Error::runtime("the config writer stopped")),
                    }
                }
            })?,
        )?;
    }
    install_primitives(host, lua)?;
    Ok(notifier)
}

fn start_worker(host: Weak<Host>, mut rx: mpsc::UnboundedReceiver<Job>, running: Rc<Cell<u64>>, notifier: Notifier) {
    tokio::task::spawn_local(async move {
        let mut next = 0u64;
        while let Some(Job { job, reply }) = rx.recv().await {
            if reply.is_closed() {
                continue; // the caller was cancelled before its write started
            }
            let Some(h) = host.upgrade() else { break };
            let lua = h.lua().clone();
            drop(h);
            next += 1;
            running.set(next);
            // fetched every time: a config reload reinstalls the config layer
            let run = mare_shared::lua::core::config::config_run(&lua);
            let result: mlua::Result<Table> = match run {
                Ok(run) => txn_scope::scope(next, run.call_async::<Table>(job)).await,
                Err(e) => Err(e),
            };
            running.set(0);
            let (values, notices, reload) = match result {
                Ok(r) => match answer(&lua, &r) {
                    Ok(x) => x,
                    Err(e) => (failure(&lua, &e.to_string()), vec![], vec![]),
                },
                Err(e) => (failure(&lua, &e.to_string()), vec![], vec![]),
            };
            if notices.is_empty() && reload.is_empty() {
                let _ = reply.send(values);
            } else {
                let _ = notifier.0.send(Notice { notices, reload, reply: Some((reply, values)) });
            }
        }
    });
}

fn failure(lua: &mlua::Lua, msg: &str) -> Vec<Value> {
    vec![Value::Nil, lua.create_string(msg).map(Value::String).unwrap_or(Value::Nil)]
}

/// `_config_run`'s result `{ ok, msg?, info?, notices?, reload? }` → the caller's answer, the
/// announcements and the plugins to reload.
fn answer(lua: &mlua::Lua, r: &Table) -> mlua::Result<(Vec<Value>, Vec<Table>, Vec<String>)> {
    let notices: Vec<Table> = r.get::<Option<Vec<Table>>>("notices")?.unwrap_or_default();
    let reload: Vec<String> = r.get::<Option<Vec<String>>>("reload")?.unwrap_or_default();
    if r.get::<bool>("ok")? {
        return Ok((vec![Value::Boolean(true)], notices, reload));
    }
    let msg: Value = r.get("msg")?;
    let info: Value = r.get("info")?;
    let msg = if msg.is_nil() { Value::String(lua.create_string("config write failed")?) } else { msg };
    Ok((vec![Value::Nil, msg, info], notices, reload))
}

/// Primitives `config_txn.lua` uses inside a transaction.
fn install_primitives(host: &Arc<Host>, lua: &mlua::Lua) -> mlua::Result<()> {
    let kernel = host.kernel.clone();
    // the owner's live value of an option (`option/current`, bail): a service's state can differ
    // from the configured value (a policy set with `/approval`); nil when the owner keeps none.
    // `option/current` and `option/apply` carry real values, so they are declared `expose = "process"`:
    // no event stream, headless output or remote hook sees them
    let k = kernel.clone();
    mare_shared::lua::core::config::set_option_current(
        lua,
        lua.create_async_function(move |lua, path: String| {
            let k = k.clone();
            async move { Ok(k.bail("option/current", payload(&lua, &path, Value::Nil)?).await) }
        })?,
    )?;
    // the owner applies one leaf and may refuse it (an error: a failed save)
    let k = kernel.clone();
    mare_shared::lua::core::config::set_option_apply(
        lua,
        lua.create_async_function(move |lua, (path, value, restore): (String, Value, Option<bool>)| {
            let k = k.clone();
            async move {
                let p = payload(&lua, &path, value)?;
                if let (Some(true), Some(Value::Table(t))) = (restore, p.iter().next()) {
                    t.set("restore", true)?;
                }
                k.emit_checked("option/apply", p).await
            }
        })?,
    )?;
    // can `value` go into the kernel's config copy? (checked while staging, so publishing cannot fail)
    mare_shared::lua::core::config::set_config_convertible(
        lua,
        lua.create_function(|lua, value: Value| match mare_shared::lua::config_to_json(lua, value) {
            Ok(_) => Ok((true, Value::Nil)),
            Err(e) => Ok((false, Value::String(lua.create_string(e.to_string())?))),
        })?,
    )?;
    // the running plugin's own `cfg` table, nil if not running
    let k = kernel.clone();
    mare_shared::lua::core::config::set_plugin_running(
        lua,
        lua.create_function(move |_, name: String| Ok(k.plugin_cfg(&name)))?,
    )?;
    // the value `name` runs with for option `key` when its committed value is `value` (`present`
    // false: absent): the declared default with it merged in, as at startup
    let k = kernel.clone();
    mare_shared::lua::core::config::set_plugin_effective(
        lua,
        lua.create_function(move |lua, (name, key, present, value): (String, String, bool, Value)| {
            let committed = if present { Some(mare_shared::lua::config_to_json(lua, value)?) } else { None };
            match k.plugin_option_effective(&name, &key, committed)? {
                Some(v) => mare_shared::lua::config_to_lua(lua, &v),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    // the kernel's config copy of `plugins.<name>.<key>` (absent: removed); `json` is converted in
    // the staging step, so this never fails at the publish point
    let k = kernel.clone();
    let weak_host = Arc::downgrade(host);
    mare_shared::lua::core::config::set_plugin_kernel_sync(
        lua,
        lua.create_function(move |lua, (name, key, value): (String, String, Value)| {
            let json = match value {
                Value::Nil => None,
                v => Some(mare_shared::lua::config_to_json(lua, v)?),
            };
            let mut cfg = k.config();
            if let Some(root) = cfg.as_object_mut() {
                let plugins = root.entry(mare_shared::PLUGINS_KEY).or_insert_with(|| serde_json::json!({}));
                let own = plugins
                    .as_object_mut()
                    .map(|p| p.entry(name.clone()).or_insert_with(|| serde_json::json!({})))
                    .and_then(|own| own.as_object_mut());
                if let Some(own) = own {
                    match json {
                        None => own.remove(&key),
                        Some(v) => own.insert(key, v),
                    };
                }
                if let Some(h) = weak_host.upgrade() {
                    super::commit_kernel_config(&h.kernel, &h.workspace, cfg);
                } else {
                    k.set_config(cfg);
                }
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_and_answer_helpers_format_ok_and_error_outcomes() {
        let lua = mlua::Lua::new();
        let vals = failure(&lua, "boom");
        assert!(vals[0].is_nil());
        assert_eq!(vals[1].as_string().unwrap().to_str().unwrap(), "boom");

        let ok_tbl = lua.create_table().unwrap();
        ok_tbl.set("ok", true).unwrap();
        let (ok_vals, notices, reload) = answer(&lua, &ok_tbl).unwrap();
        assert_eq!(ok_vals, vec![Value::Boolean(true)]);
        assert!(notices.is_empty() && reload.is_empty());

        let err_tbl = lua.create_table().unwrap();
        err_tbl.set("ok", false).unwrap();
        let (err_vals, _, _) = answer(&lua, &err_tbl).unwrap();
        assert!(err_vals[0].is_nil());
        assert_eq!(err_vals[1].as_string().unwrap().to_str().unwrap(), "config write failed");

        let with_reload = lua.create_table().unwrap();
        with_reload.set("ok", true).unwrap();
        let notice = lua.create_table().unwrap();
        notice.set("message", "n1").unwrap();
        with_reload.set("notices", lua.create_sequence_from([notice]).unwrap()).unwrap();
        with_reload.set("reload", lua.create_sequence_from(["statusline"]).unwrap()).unwrap();
        let (_, notices, reload) = answer(&lua, &with_reload).unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(reload, vec!["statusline"]);
    }
}
