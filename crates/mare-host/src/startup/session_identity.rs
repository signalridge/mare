//! Single session-identity write path, `RuntimeSet` coordination trait, and the
//! `kernel.begin_session_switch` / `SessionSwitchHandle` transaction.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak as RcWeak};
use std::sync::{Arc, Weak};

use anyhow::Result;
use mlua::thread::ThreadStatus;
use mlua::{MetaMethod, Table, UserData, UserDataMethods, Value};
use tokio_util::sync::CancellationToken;

use crate::startup::request_gate::HostSlot;
use crate::{Host, LoadedSessionEntry, SessionError};

/// Process-level runtime registry view attached to a managed [`Host`].
///
/// Standalone hosts (TUI, print, json, standalone ACP, unit tests) operate without a
/// `RuntimeSet`: reservations always succeed, rekey is a no-op, and `kernel.list_sessions`
/// reports only this host's open session.
pub trait RuntimeSet {
    /// Reserve `session_id` for `host` before switching. Fails with
    /// [`SessionError::AlreadyLoaded`] (or `Closing` / `Loading`) if another runtime owns,
    /// is transitioning, or has reserved `session_id`.
    fn reserve(&self, host: &Arc<Host>, session_id: &str) -> Result<(), SessionError>;

    /// Rekey `host` to `new_sid` and release any prior reservation held by `host`.
    fn rekey(&self, host: &Arc<Host>, new_sid: &str) -> Result<(), SessionError>;

    /// Release an active reservation held by `host` for `session_id` (no-op if absent).
    fn release(&self, host: &Arc<Host>, session_id: &str);

    /// Snapshot of currently loaded sessions across all runtimes in this set.
    fn list(&self) -> Vec<LoadedSessionEntry>;
}

/// RAII guard holding one bound session call on a [`Host`]; while any guard is live,
/// `kernel.begin_session_switch` refuses new switches.
pub struct SessionCallGuard {
    counter: Rc<Cell<usize>>,
}

impl Drop for SessionCallGuard {
    fn drop(&mut self) {
        self.counter.set(self.counter.get().saturating_sub(1));
    }
}

struct ActiveSwitch {
    inner: Rc<RefCell<SwitchInner>>,
    owner_token: Option<CancellationToken>,
    origin_thread: Option<mlua::Thread>,
}

/// Shared per-host session identity & switch coordination state, created before `kernel.start()`
/// so host methods registered early share the exact cells held by [`Host`].
pub(crate) struct IdentityState {
    pub(crate) web_session_id: Rc<RefCell<Option<String>>>,
    runtime_set: RefCell<RcWeak<dyn RuntimeSet>>,
    bound_session_calls: Rc<Cell<usize>>,
    active_switch: Rc<RefCell<Option<ActiveSwitch>>>,
}

impl IdentityState {
    pub(crate) fn new(web_session_id: Rc<RefCell<Option<String>>>) -> Self {
        Self {
            web_session_id,
            runtime_set: RefCell::new(RcWeak::<NoopRuntimeSet>::new()),
            bound_session_calls: Rc::new(Cell::new(0)),
            active_switch: Rc::new(RefCell::new(None)),
        }
    }

    pub(crate) fn attach_runtime_set(&self, set: RcWeak<dyn RuntimeSet>) {
        *self.runtime_set.borrow_mut() = set;
    }

    pub(crate) fn runtime_set(&self) -> Option<Rc<dyn RuntimeSet>> {
        self.runtime_set.borrow().upgrade()
    }

    pub(crate) fn bind_session_call(&self) -> Result<SessionCallGuard> {
        self.purge_cancelled_switch();
        if self.active_switch.borrow().is_some() {
            anyhow::bail!("cannot bind session call while a session switch is in progress; retry");
        }
        self.bound_session_calls.set(self.bound_session_calls.get().saturating_add(1));
        Ok(SessionCallGuard { counter: Rc::clone(&self.bound_session_calls) })
    }

    pub(crate) fn bound_session_calls(&self) -> usize {
        self.bound_session_calls.get()
    }

    pub(crate) fn has_active_switch(&self) -> bool {
        self.purge_cancelled_switch();
        self.active_switch.borrow().is_some()
    }

    pub(crate) fn purge_cancelled_switch(&self) {
        let should_abort = {
            let slot = self.active_switch.borrow();
            match slot.as_ref() {
                None => false,
                Some(active) => {
                    let thread_ended = active
                        .origin_thread
                        .as_ref()
                        .is_some_and(|t| matches!(t.status(), ThreadStatus::Finished | ThreadStatus::Error));
                    active.inner.borrow().closed
                        || thread_ended
                        || active.owner_token.as_ref().is_some_and(CancellationToken::is_cancelled)
                }
            }
        };
        let taken = if should_abort { self.active_switch.borrow_mut().take() } else { None };
        if let Some(active) = taken {
            abort_switch_inner(&active.inner);
        }
    }
}

struct NoopRuntimeSet;
impl RuntimeSet for NoopRuntimeSet {
    fn reserve(&self, _host: &Arc<Host>, _session_id: &str) -> Result<(), SessionError> {
        Ok(())
    }
    fn rekey(&self, _host: &Arc<Host>, _new_sid: &str) -> Result<(), SessionError> {
        Ok(())
    }
    fn release(&self, _host: &Arc<Host>, _session_id: &str) {}
    fn list(&self) -> Vec<LoadedSessionEntry> {
        Vec::new()
    }
}

/// The single Rust function that writes session identity across every host sink:
/// - `RuntimeSet` availability check & rekey (when attached),
/// - kernel `EventLog` session identity / epoch,
/// - `mare_binding::peers::session_cell`,
/// - `web_session_id` cell,
/// - `mare_binding::netauth::set_origin_source`.
///
/// When `bump_epoch` is true (`SessionSwitchHandle:commit`), committing the already-active
/// session ID is a no-op on the epoch while keeping all sinks synchronized.
pub(crate) fn write_session_identity(
    kernel: &mare_kernel::Kernel,
    state: &IdentityState,
    host: Option<&Arc<Host>>,
    new_id: Option<String>,
    bump_epoch: bool,
) -> Result<String, SessionError> {
    write_session_identity_with_force(kernel, state, host, new_id, bump_epoch, false)
}

fn write_session_identity_with_force(
    kernel: &mare_kernel::Kernel,
    state: &IdentityState,
    host: Option<&Arc<Host>>,
    new_id: Option<String>,
    bump_epoch: bool,
    force_epoch: bool,
) -> Result<String, SessionError> {
    if let (Some(host), Some(sid)) = (host, new_id.as_deref())
        && let Some(set) = state.runtime_set()
    {
        set.reserve(host, sid)?;
    }
    let current = kernel.event_log().session_identity();
    let epoch = if bump_epoch {
        if !force_epoch && current.0 == new_id && new_id.is_some() {
            current.1
        } else {
            kernel.switch_session_identity(new_id.clone())
        }
    } else {
        kernel.set_session_identity(new_id.clone())
    };
    mare_binding::peers::session_cell(kernel.lua()).set(new_id.clone());
    *state.web_session_id.borrow_mut() = new_id.clone();
    let (sid, ep) = kernel.event_log().session_identity();
    mare_binding::netauth::set_origin_source(kernel.lua(), sid, ep);
    if let (Some(host), Some(sid)) = (host, new_id.as_deref())
        && let Some(set) = state.runtime_set()
    {
        set.rekey(host, sid)?;
    }
    Ok(epoch)
}

fn coded_err(code: &'static str, message: impl Into<String>) -> mlua::Error {
    mlua::Error::external(mare_shared::errors::Coded { code, message: message.into() })
}

fn map_session_err(err: SessionError) -> mlua::Error {
    match err {
        SessionError::AlreadyLoaded(sid) | SessionError::Closing(sid) | SessionError::Loading(sid) => {
            coded_err("session.already_loaded", format!("session `{sid}` is already loaded"))
        }
        SessionError::ShuttingDown => coded_err("unavailable", "host manager is shutting down"),
        other => mlua::Error::runtime(other.to_string()),
    }
}

struct SwitchInner {
    closed: bool,
    reserved_id: Option<String>,
    identity: Rc<IdentityState>,
    host_slot: HostSlot,
    kernel_weak: Weak<mare_kernel::KernelInner>,
    running_coro: mlua::Function,
}

fn clear_active_if_same(identity: &IdentityState, target: &Rc<RefCell<SwitchInner>>) {
    let mut slot = identity.active_switch.borrow_mut();
    if slot.as_ref().is_some_and(|a| Rc::ptr_eq(&a.inner, target)) {
        slot.take();
    }
}

fn abort_switch_inner(inner_rc: &Rc<RefCell<SwitchInner>>) {
    let (reserved, identity, host_slot) = {
        let mut inner = inner_rc.borrow_mut();
        if inner.closed {
            return;
        }
        inner.closed = true;
        (inner.reserved_id.take(), Rc::clone(&inner.identity), Rc::clone(&inner.host_slot))
    };
    if let Some(sid) = reserved
        && let Some(host) = host_slot.get().and_then(Weak::upgrade)
        && let Some(set) = identity.runtime_set()
    {
        set.release(&host, &sid);
    }
    clear_active_if_same(&identity, inner_rc);
}

/// Lua userdata handle returned by `kernel.begin_session_switch`.
///
/// - `:commit(session_id)` synchronously commits identity through [`write_session_identity`]
///   and returns `{ session_epoch = string }`.
/// - `:abort()`, `<close>`, `Drop`, or disposal of the calling fiber releases the reservation
///   and lock without rolling back provider-side state.
struct SessionSwitchHandle {
    inner: Rc<RefCell<SwitchInner>>,
    watcher: Option<tokio::task::AbortHandle>,
}

impl SessionSwitchHandle {
    fn abort(&mut self) {
        if let Some(handle) = self.watcher.take() {
            handle.abort();
        }
        abort_switch_inner(&self.inner);
    }
}

impl Drop for SessionSwitchHandle {
    fn drop(&mut self) {
        self.abort();
    }
}

impl UserData for SessionSwitchHandle {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("commit", |lua, this, (session_id, opts): (String, Option<Table>)| {
            if session_id.is_empty() {
                return Err(coded_err("invalid_arguments", "session_id must be a non-empty string"));
            }
            let force_epoch = opts.and_then(|t| t.get::<Option<bool>>("force_epoch").ok().flatten()).unwrap_or(false);
            let (identity, host_slot, kernel_weak, prev_reserved) = {
                let inner = this.inner.borrow();
                if inner.closed {
                    return Err(coded_err("invalid_arguments", "session switch handle is already closed"));
                }
                (
                    Rc::clone(&inner.identity),
                    Rc::clone(&inner.host_slot),
                    inner.kernel_weak.clone(),
                    inner.reserved_id.clone(),
                )
            };
            identity.purge_cancelled_switch();
            if this.inner.borrow().closed {
                return Err(coded_err("cancelled", "session switch handle was cancelled"));
            }
            if identity.bound_session_calls() > 0 {
                return Err(coded_err(
                    "busy",
                    "cannot commit session switch while a call bound to this session is running; retry",
                ));
            }
            let Some(kernel) = mare_kernel::Kernel::from_weak(&kernel_weak) else {
                this.abort();
                return Err(coded_err("unavailable", "host is shutting down"));
            };
            let host = host_slot.get().and_then(Weak::upgrade);
            if host_slot.get().is_some() && host.is_none() {
                this.abort();
                return Err(coded_err("unavailable", "host is shutting down"));
            }
            let epoch = write_session_identity_with_force(
                &kernel,
                &identity,
                host.as_ref(),
                Some(session_id.clone()),
                true,
                force_epoch,
            )
            .map_err(map_session_err)?;
            // Committing host session identity crosses the durable commit boundary: automatically
            // mark any active session export call committed so custom providers do not need to
            // call `kernel:mark_call_committed()` manually.
            mare_kernel::mark_service_call_committed(kernel.id());
            if let Some(prev) = prev_reserved
                && prev != session_id
                && let Some(ref host) = host
                && let Some(set) = identity.runtime_set()
            {
                set.release(host, &prev);
            }
            {
                let mut inner = this.inner.borrow_mut();
                inner.closed = true;
                inner.reserved_id = None;
            }
            if let Some(handle) = this.watcher.take() {
                handle.abort();
            }
            clear_active_if_same(&identity, &this.inner);
            let out = lua.create_table()?;
            out.set("session_epoch", epoch)?;
            Ok(out)
        });
        methods.add_method_mut("adopt", |_, this, bind_current: Option<bool>| {
            let (identity, running_coro) = {
                let inner = this.inner.borrow();
                if inner.closed {
                    return Err(coded_err("invalid_arguments", "session switch handle is already closed"));
                }
                (Rc::clone(&inner.identity), inner.running_coro.clone())
            };
            identity.purge_cancelled_switch();
            if this.inner.borrow().closed {
                return Err(coded_err("cancelled", "session switch handle was cancelled"));
            }
            let new_thread = if bind_current == Some(false) {
                None
            } else {
                mare_kernel::active_caller_thread().or(running_coro.call(())?)
            };
            let mut slot = identity.active_switch.borrow_mut();
            if let Some(active) = slot.as_mut()
                && Rc::ptr_eq(&active.inner, &this.inner)
            {
                active.origin_thread = new_thread;
            }
            Ok(())
        });
        methods.add_method_mut("abort", |_, this, ()| {
            this.abort();
            Ok(())
        });
        methods.add_meta_method_mut(MetaMethod::Close, |_, this, _: Value| {
            this.abort();
            Ok(())
        });
    }
}

/// Register `kernel.begin_session_switch` before `kernel.start()`.
pub(crate) fn register_identity_methods(
    kernel: &mare_kernel::Kernel,
    identity: Rc<IdentityState>,
    host_slot: HostSlot,
) -> Result<()> {
    let lua = kernel.lua().clone();
    let on_builtins_kernel = kernel.weak();
    let on_builtins_identity = Rc::clone(&identity);
    lua.set_named_registry_value(
        "mare.after_builtins_started",
        lua.create_function(move |_, ()| {
            let Some(kernel) = mare_kernel::Kernel::from_weak(&on_builtins_kernel) else {
                return Ok(());
            };
            if kernel.event_log().session_identity().0.is_some() {
                return Ok(());
            }
            let Some(Value::Table(svc)) = kernel.service(mare_shared::SESSION_SERVICE_NAME) else {
                return Ok(());
            };
            let Ok(info_fn) = svc.get::<mlua::Function>("info") else {
                return Ok(());
            };
            let Ok(info) = info_fn.call::<Table>(svc) else {
                return Ok(());
            };
            if let Ok(Some(sid)) = info.get::<Option<String>>("session_id")
                && !sid.is_empty()
            {
                let _ = write_session_identity(&kernel, &on_builtins_identity, None, Some(sid), false);
            }
            Ok(())
        })?,
    )?;
    let after_reload_host = Rc::clone(&host_slot);
    lua.set_named_registry_value(
        "mare.after_kernel_reload",
        lua.create_async_function(move |_, ()| {
            let host_slot = Rc::clone(&after_reload_host);
            async move {
                if let Some(host) = host_slot.get().and_then(Weak::upgrade)
                    && host.pending_session_trust_revoke.get()
                {
                    host.revoke_session_only_trust_with_gate()
                        .await
                        .map_err(|e| mlua::Error::runtime(format!("revoking session-only trust failed: {e:#}")))?;
                }
                Ok(())
            }
        })?,
    )?;
    let running_coro: mlua::Function = lua
        .load("local th, is_main = coroutine.running(); if not is_main then return th end")
        .set_name("=begin_session_switch.running_coro")
        .into_function()?;
    let k_weak = kernel.weak();
    let begin_identity = Rc::clone(&identity);
    let begin_host_slot = Rc::clone(&host_slot);
    kernel.register_host_method_async(
        mare_shared::KERNEL_SERVICE_NAME,
        "begin_session_switch",
        lua.create_async_function(move |lua, opts: Option<Table>| {
            let identity = Rc::clone(&begin_identity);
            let host_slot = Rc::clone(&begin_host_slot);
            let k_weak = k_weak.clone();
            let running_coro = running_coro.clone();
            async move {
                let origin_thread: Option<mlua::Thread> =
                    mare_kernel::active_caller_thread().or(running_coro.call(())?);
                let target_id = match opts {
                    None => None,
                    Some(tbl) => match tbl.get::<Option<String>>("session_id")? {
                        Some(s) if s.is_empty() => {
                            return Err(coded_err("invalid_arguments", "session_id must be a non-empty string"));
                        }
                        other => other,
                    },
                };
                let Some(kernel) = mare_kernel::Kernel::from_weak(&k_weak) else {
                    return Err(coded_err("unavailable", "host is shutting down"));
                };
                let host = host_slot.get().and_then(Weak::upgrade);
                if host_slot.get().is_some() && host.is_none() {
                    return Err(coded_err("unavailable", "host is shutting down"));
                }
                identity.purge_cancelled_switch();
                if identity.bound_session_calls() > 0 {
                    return Err(coded_err(
                        "busy",
                        "cannot switch sessions while a call bound to this session is running; retry",
                    ));
                }
                if identity.active_switch.borrow().is_some() {
                    return Err(coded_err("busy", "cannot switch sessions while another switch is in progress; retry"));
                }
                let owner_token = match mare_kernel::active_owner_id() {
                    Some(id) => match kernel.owner_cancel(id) {
                        Some(tok) if !tok.is_cancelled() => Some(tok),
                        _ => return Err(coded_err("cancelled", "caller context was disposed")),
                    },
                    None => None,
                };
                let mut reserved_id = None;
                if let (Some(sid), Some(host)) = (&target_id, &host)
                    && let Some(set) = identity.runtime_set()
                {
                    set.reserve(host, sid).map_err(map_session_err)?;
                    reserved_id = Some(sid.clone());
                }
                let inner = Rc::new(RefCell::new(SwitchInner {
                    closed: false,
                    reserved_id,
                    identity: Rc::clone(&identity),
                    host_slot: Rc::clone(&host_slot),
                    kernel_weak: k_weak,
                    running_coro,
                }));
                *identity.active_switch.borrow_mut() =
                    Some(ActiveSwitch { inner: Rc::clone(&inner), owner_token: owner_token.clone(), origin_thread });

                // Guard releases reservation + lock if trust revocation fails or this future is dropped.
                struct PendingGuard(Option<Rc<RefCell<SwitchInner>>>);
                impl Drop for PendingGuard {
                    fn drop(&mut self) {
                        if let Some(inner) = self.0.take() {
                            abort_switch_inner(&inner);
                        }
                    }
                }
                let mut pending = PendingGuard(Some(Rc::clone(&inner)));

                if let Some(ref host) = host {
                    if host.reload_active.get() || kernel.is_reloading() {
                        if host.workspace.is_session_only() {
                            host.pending_session_trust_revoke.set(true);
                        }
                    } else if host.workspace.is_session_only() || host.pending_session_trust_revoke.get() {
                        host.revoke_session_only_trust()
                            .await
                            .map_err(|e| mlua::Error::runtime(format!("revoking session-only trust failed: {e:#}")))?;
                    }
                }

                // Re-check owner cancellation after await.
                if owner_token.as_ref().is_some_and(CancellationToken::is_cancelled) {
                    return Err(coded_err("cancelled", "caller context was disposed"));
                }

                pending.0 = None;
                let watcher = owner_token.map(|tok| {
                    let inner_watch = Rc::clone(&inner);
                    mare_shared::txn_scope::spawn_local(async move {
                        tok.cancelled().await;
                        abort_switch_inner(&inner_watch);
                    })
                    .abort_handle()
                });
                lua.create_userdata(SessionSwitchHandle { inner, watcher })
            }
        })?,
    )?;
    let list_identity = Rc::clone(&identity);
    let list_kernel = kernel.weak();
    kernel.register_host_method(
        mare_shared::KERNEL_SERVICE_NAME,
        "list_sessions",
        lua.create_function(move |lua, ()| {
            let mut entries = if let Some(set) = list_identity.runtime_set() {
                set.list()
            } else if let Some(kernel) = mare_kernel::Kernel::from_weak(&list_kernel) {
                match kernel.event_log().session_identity() {
                    (Some(sid), epoch) if !sid.is_empty() => {
                        vec![LoadedSessionEntry { session_id: sid, session_epoch: Some(epoch) }]
                    }
                    _ => Vec::new(),
                }
            } else {
                Vec::new()
            };
            entries.sort_by(|a, b| a.session_id.cmp(&b.session_id));
            let tbl = lua.create_table_with_capacity(entries.len(), 0)?;
            for (i, entry) in entries.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("session_id", entry.session_id)?;
                if let Some(epoch) = entry.session_epoch {
                    row.set("session_epoch", epoch)?;
                }
                tbl.set(i + 1, row)?;
            }
            Ok(tbl)
        })?,
    )?;
    kernel.register_host_method(
        mare_shared::KERNEL_SERVICE_NAME,
        "get_caller",
        lua.create_function(|_, ()| {
            Ok(mare_shared::txn_scope::current_caller_owner().unwrap_or_else(|| "host".to_string()))
        })?,
    )?;
    let cap_host_slot = Rc::clone(&host_slot);
    kernel.register_host_method(
        mare_shared::KERNEL_SERVICE_NAME,
        "session_capacity",
        lua.create_function(move |_, ()| {
            let is_multi = cap_host_slot
                .get()
                .and_then(Weak::upgrade)
                .and_then(|host| host.orchestrator())
                .is_some_and(|orch| orch.is_multi());
            Ok(if is_multi { "multi" } else { "single" })
        })?,
    )?;
    Ok(())
}
