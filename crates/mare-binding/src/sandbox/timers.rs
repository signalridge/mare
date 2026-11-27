//! Timers belong to one execution scope, never keep a completed script alive.
use super::limits::Limits;
use mlua::{Function, Lua, Table};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};
#[derive(Clone)]
pub(super) struct Timers(Rc<RefCell<HashMap<u64, tokio::task::AbortHandle>>>);
impl Timers {
    pub fn clear(&self) {
        for (_, handle) in self.0.borrow_mut().drain() {
            handle.abort();
        }
    }
}
fn delay(value: f64, deadline: Option<std::time::Duration>) -> std::time::Duration {
    let millis = if !value.is_finite() || value <= 0.0 { 0 } else { value.trunc().min(u64::MAX as f64) as u64 };
    let millis = deadline.map(|limit| millis.min(limit.as_millis() as u64 + 1)).unwrap_or(millis);
    std::time::Duration::from_millis(millis)
}

pub(super) fn install(
    lua: &Lua,
    env: &Table,
    limits: Limits,
    epoch: std::sync::Arc<std::sync::atomic::AtomicU64>,
    birth: u64,
) -> mlua::Result<Timers> {
    let timers = Timers(Rc::new(RefCell::new(HashMap::new())));
    let ids = Rc::new(Cell::new(0_u64));
    let create = timers.clone();
    env.set(
        "setTimeout",
        lua.create_function(move |_, (callback, ms): (Function, Option<f64>)| {
            if epoch.load(std::sync::atomic::Ordering::SeqCst) != birth {
                return Err(mlua::Error::runtime("timer belongs to an expired execution"));
            }
            limits.check()?;
            if create.0.borrow().len() >= limits.pending_calls {
                return Err(mlua::Error::runtime("sandbox timer limit exceeded"));
            }
            let duration = delay(ms.unwrap_or(0.0), limits.deadline_enabled.then_some(limits.timeout));
            let id = ids.get() + 1;
            ids.set(id);
            let scope = create.clone();
            let limits = limits.clone();
            let task = mare_shared::txn_scope::spawn_local(async move {
                if duration.is_zero() {
                    tokio::task::yield_now().await;
                } else {
                    tokio::time::sleep(duration).await;
                }
                if limits.check().is_ok() && callback.call_async::<()>(()).await.is_err() {
                    limits.stop("timer callback failed");
                }
                scope.0.borrow_mut().remove(&id);
            });
            create.0.borrow_mut().insert(id, task.abort_handle());
            Ok(id)
        })?,
    )?;
    let clear = timers.clone();
    env.set(
        "clearTimeout",
        lua.create_function(move |_, id: Option<u64>| {
            if let Some(id) = id
                && let Some(task) = clear.0.borrow_mut().remove(&id)
            {
                task.abort();
            }
            Ok(())
        })?,
    )?;
    env.set(
        "sleep",
        lua.create_async_function(|_, ms: f64| async move {
            tokio::time::sleep(delay(ms, None)).await;
            Ok(())
        })?,
    )?;
    Ok(timers)
}
