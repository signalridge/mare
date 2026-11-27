//! The request gate's Lua side: the host's readiness vote, the
//! first in the readiness contract's order; `mare.reload_version()` names the host's reload
//! state (public in `types/mare/loader.lua`), while privileged host-internal operations
//! (`request_lease`, `mark_committed`, `set_session_identity`) live in the kernel's `prims`
//! host-internal registry and are authorized by service/plugin ownership (never exposed on global `mare`).

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, Mutex, Weak};

use anyhow::Result;
use mlua::thread::ThreadStatus;
use mlua::{Lua, MetaMethod, MultiValue, Table, UserData, UserDataMethods, Value};
use tokio_util::sync::CancellationToken;

use crate::Host;

pub(crate) type HostSlot = Rc<OnceCell<Weak<Host>>>;

type LeaseSlot = Arc<Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>>;
type WeakLeaseSlot = std::sync::Weak<Mutex<Option<tokio::sync::OwnedMutexGuard<()>>>>;

struct ActiveLease {
    slot: WeakLeaseSlot,
    tokens: Vec<CancellationToken>,
    origin_thread: Option<mlua::Thread>,
    released: Rc<Cell<bool>>,
}

#[derive(Default)]
struct TrackedLease(Rc<RefCell<Option<ActiveLease>>>);

/// Drop the active request lease if its owning `agent` registration, request cancellation token,
/// or origin coroutine ended, and prune already-released handles in O(1).
pub(crate) fn purge_cancelled_leases(lua: &Lua) {
    let Some(tracked) = lua.app_data_ref::<TrackedLease>() else {
        return;
    };
    let mut cell = tracked.0.borrow_mut();
    let Some(active) = cell.take() else {
        return;
    };
    if active.released.get() {
        return;
    }
    let Some(slot) = active.slot.upgrade() else {
        return;
    };
    let still_held = slot.lock().map(|g| g.is_some()).unwrap_or(false);
    if !still_held {
        return;
    }
    let thread_ended = active
        .origin_thread
        .as_ref()
        .is_some_and(|t| matches!(t.status(), ThreadStatus::Finished | ThreadStatus::Error));
    if thread_ended || active.tokens.iter().any(CancellationToken::is_cancelled) {
        if let Ok(mut guard) = slot.lock() {
            guard.take();
        }
        active.released.set(true);
        return;
    }
    *cell = Some(active);
}

fn track_lease(
    lua: &Lua,
    slot: &LeaseSlot,
    tokens: Vec<CancellationToken>,
    origin_thread: Option<mlua::Thread>,
    released: Rc<Cell<bool>>,
) {
    if lua.app_data_ref::<TrackedLease>().is_none() {
        lua.set_app_data(TrackedLease::default());
    }
    if let Some(tracked) = lua.app_data_ref::<TrackedLease>() {
        *tracked.0.borrow_mut() = Some(ActiveLease { slot: Arc::downgrade(slot), tokens, origin_thread, released });
    }
}

fn extract_cancel_token(arg: Option<Value>) -> mlua::Result<Option<CancellationToken>> {
    match arg {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::UserData(ud)) => Ok(Some(ud.borrow::<mare_shared::lua::Token>()?.0.clone())),
        Some(Value::Table(tbl)) => match tbl.get::<Value>("cancel")? {
            Value::Nil => Ok(None),
            Value::UserData(ud) => Ok(Some(ud.borrow::<mare_shared::lua::Token>()?.0.clone())),
            _ => Err(mlua::Error::runtime("request_lease: `cancel` must be a mare.Token")),
        },
        _ => Err(mlua::Error::runtime("request_lease: expected nil, mare.Token, or { cancel = mare.Token }")),
    }
}

/// A held request lease: released by `:release()`, by closing its `<close>` variable, when its
/// request cancellation token or owning `agent` service registration ends, or when collected.
struct RequestLease {
    slot: LeaseSlot,
    watcher: Option<tokio::task::AbortHandle>,
    released: Rc<Cell<bool>>,
}

impl RequestLease {
    fn clear(&mut self) {
        self.released.set(true);
        if let Some(handle) = self.watcher.take() {
            handle.abort();
        }
        if let Ok(mut guard) = self.slot.lock() {
            guard.take();
        }
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        self.clear();
    }
}

impl UserData for RequestLease {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("release", |_, this, ()| {
            this.clear();
            Ok(())
        });
        methods.add_meta_method_mut(MetaMethod::Close, |_, this, _: Value| {
            this.clear();
            Ok(())
        });
    }
}

/// Register `kernel.request_lease` and `kernel.mark_call_committed` before `kernel.start()` so
/// plugins can acquire leases during `apply()`.
pub(crate) fn register_lease_methods(
    kernel: &mare_kernel::Kernel,
    reload_gate: Arc<tokio::sync::Mutex<()>>,
    session_cancel: Rc<RefCell<CancellationToken>>,
    host_slot: HostSlot,
) -> Result<()> {
    let lua = kernel.lua().clone();
    lua.set_named_registry_value(
        "mare.purge_cancelled_leases",
        lua.create_function(move |lua, ()| {
            purge_cancelled_leases(lua);
            Ok(())
        })?,
    )?;
    let running_coro: mlua::Function = lua
        .load("local th, is_main = coroutine.running(); if not is_main then return th end")
        .set_name("=request_lease.running_coro")
        .into_function()?;
    let k_weak = kernel.weak();
    kernel.register_host_method(
        mare_shared::KERNEL_SERVICE_NAME,
        "request_lease",
        lua.create_function(move |lua, cancel_arg: Option<Value>| {
            let Some(kernel) = mare_kernel::Kernel::from_weak(&k_weak) else {
                return Ok((None, Some("host is shutting down".to_string())));
            };
            let host = host_slot.get().and_then(Weak::upgrade);
            if host_slot.get().is_some() && host.is_none() {
                return Ok((None, Some("host is shutting down".to_string())));
            }
            let owner_token = match mare_kernel::active_owner_id() {
                Some(id) => match kernel.owner_cancel(id) {
                    Some(tok) if !tok.is_cancelled() => Some(tok),
                    _ => return Ok((None, Some("caller context was disposed".to_string()))),
                },
                None => None,
            };
            let req_token = extract_cancel_token(cancel_arg)?;
            purge_cancelled_leases(lua);
            if req_token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Ok((None, Some("request was cancelled".to_string())));
            }
            let reload_reason = host.as_ref().and_then(|h| h.reload_reason());
            if let Some(reason) = mare_binding::exec_gate::poison_reason(lua).or(reload_reason) {
                return Ok((None, Some(reason)));
            }
            match reload_gate.clone().try_lock_owned() {
                Ok(guard) => {
                    let slot = Arc::new(Mutex::new(Some(guard)));
                    let released = Rc::new(Cell::new(false));
                    let origin_thread: Option<mlua::Thread> =
                        if req_token.is_none() { running_coro.call(())? } else { None };
                    let mut tokens = Vec::with_capacity(3);
                    if let Some(t) = owner_token {
                        tokens.push(t);
                    }
                    if let Some(t) = req_token {
                        tokens.push(t);
                    }
                    tokens.push(session_cancel.borrow().clone());
                    track_lease(lua, &slot, tokens.clone(), origin_thread, Rc::clone(&released));
                    let slot_watch = Arc::clone(&slot);
                    let released_watch = Rc::clone(&released);
                    let watcher = Some(
                        mare_shared::txn_scope::spawn_local(async move {
                            match tokens.as_slice() {
                                [a] => a.cancelled().await,
                                [a, b] => {
                                    tokio::select! {
                                        _ = a.cancelled() => {},
                                        _ = b.cancelled() => {},
                                    }
                                }
                                [a, b, c] => {
                                    tokio::select! {
                                        _ = a.cancelled() => {},
                                        _ = b.cancelled() => {},
                                        _ = c.cancelled() => {},
                                    }
                                }
                                _ => {}
                            }
                            released_watch.set(true);
                            if let Ok(mut held) = slot_watch.lock() {
                                held.take();
                            }
                        })
                        .abort_handle(),
                    );
                    Ok((Some(lua.create_userdata(RequestLease { slot, watcher, released })?), None))
                }
                Err(_) => Ok((None, Some("another request is in progress; retry afterwards".to_string()))),
            }
        })?,
    )?;
    let commit_kernel_id = kernel.id();
    kernel.register_host_method(
        mare_shared::KERNEL_SERVICE_NAME,
        "mark_call_committed",
        lua.create_function(move |_, ()| {
            mare_kernel::mark_service_call_committed(commit_kernel_id);
            Ok(())
        })?,
    )?;
    Ok(())
}

/// Register `mare.reload_version`, remaining host-internal bindings, and the host's readiness vote.
pub(crate) fn install(host: &Arc<Host>, mare: &Table) -> Result<()> {
    let lua = host.lua().clone();
    // Delegation may require safety services independently of the chosen provider plugin.
    // Capture once; a child's config can replace a provider but cannot remove the contract.
    let required_services =
        mare.get::<Table>("args")?.get::<Option<Vec<String>>>("required_services")?.unwrap_or_default();
    let weak: Weak<Host> = Arc::downgrade(host);
    let vote = lua.create_function(move |lua, _: MultiValue| {
        let Some(host) = weak.upgrade() else {
            return Err(mlua::Error::runtime("host is shutting down"));
        };
        match mare_binding::exec_gate::poison_reason(lua).or_else(|| host.reload_reason()).or_else(|| {
            required_services
                .iter()
                .find(|name| host.kernel.service(name).is_none())
                .map(|name| format!("delegated required service `{name}` is unavailable"))
        }) {
            Some(reason) => {
                let result = lua.create_table()?;
                result.set("decision", "deny")?;
                result.set("reason", reason)?;
                Ok(Value::Table(result))
            }
            None => Ok(Value::Nil),
        }
    })?;
    // the host's place in the readiness contract: a voter of the host's fiber
    // votes in the policy service's front segment, before llm's readiness voter and every other
    // plugin's
    let (ctx, owner) = (host.kernel.host_ctx()?, host.kernel.host_owner()?);
    for point in ["request/ready", "tool/before"] {
        host.kernel.host_voter(&ctx, owner, point, vote.clone(), mare_kernel::HostVoter::default())?;
    }
    let weak: Weak<Host> = Arc::downgrade(host);
    mare.set(
        "reload_version",
        lua.create_function(move |_, ()| {
            let Some(host) = weak.upgrade() else { return Ok("closed".to_string()) };
            Ok(format!(
                "{}:{}:{}:{}:{:?}:{:?}",
                host.reload_epoch.get(),
                host.kernel.reload_generation(),
                host.reload_active.get(),
                host.reload_full_incomplete.get(),
                host.reload_pending.borrow(),
                host.kernel.reload_incomplete()
            ))
        })?,
    )?;
    Ok(())
}
