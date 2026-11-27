//! Kernel primitives for the boot fragment and host plugins: plugin owners (attribution only,
//! no lifecycle), event primitives, definitions and their module dirs, and the reload barrier.
//! They are fields of `prims`, which boot.lua gets as its argument and passes to host plugins as config;
//! never a global or a requirable module.

use mlua::{Function, Lua, MultiValue, Table, Value};

use crate::{CtxState, Kernel};

tokio::task_local! {
    /// The plugin owner the running work is attributed to (`prims.owner.enter`, a captured cordis
    /// scope). `None` inside [`in_owner`]`(None, ..)`: explicitly the host's work again, even
    /// inside a plugin's task.
    static OWNER: Option<u64>;
    /// The caller's Lua coroutine captured before `prims.owner.enter` spawns an inner
    /// `scoped_call` coroutine, so async host methods (e.g. `begin_session_switch`) track the
    /// real calling coroutine's lifetime rather than the ephemeral wrapper coroutine.
    static CALLER_THREAD: Option<mlua::Thread>;
}

thread_local! {
    /// Synchronous owner override for non-yielding host method calls (`prims.owner.enter_sync`).
    /// `Some(owner)` overrides `OWNER`; `None` defers to the task-local `OWNER`.
    static SYNC_OWNER: std::cell::Cell<Option<Option<u64>>> = const { std::cell::Cell::new(None) };
}

/// The owner the running task acts for (`prims.owner.enter`, `enter_sync`, or a captured cordis
/// scope), if any; `None` is the host itself (Rust, root).
pub(crate) fn current_owner() -> Option<u64> {
    if let Some(overridden) = SYNC_OWNER.with(|cell| cell.get()) {
        return overridden;
    }
    OWNER.try_with(|id| *id).ok().flatten()
}

/// Current active plugin owner id (`None` when running at root/host).
pub fn active_owner_id() -> Option<u64> {
    current_owner()
}

/// Non-main Lua coroutine that invoked the enclosing `prims.owner.enter`, if any.
pub fn active_caller_thread() -> Option<mlua::Thread> {
    CALLER_THREAD.try_with(Clone::clone).ok().flatten()
}

/// Plugin name and origin (`"builtin"|"package"|"user"|"project"`) of the current active owner,
/// or `None` when running at root/host/outside a plugin owner.
pub fn current_owner_info(lua: &Lua) -> Option<(String, String)> {
    let owner_id = current_owner()?;
    let weak = lua.app_data_ref::<std::sync::Weak<crate::KernelInner>>()?;
    let k = Kernel::from_weak(&weak)?;
    let st = k.st();
    let ctx = st.ctxs.get(&owner_id)?;
    if ctx.origin == "root" {
        return None;
    }
    Some((ctx.plugin.clone(), ctx.origin.clone()))
}

/// Attach a synchronous disposer callback to the active non-root owner scope and return a detach
/// handle that removes it immediately so manually removed callbacks become GC-eligible.
pub fn attach_owner_disposer(lua: &Lua, dispose: Function) -> mlua::Result<Option<Function>> {
    let Some(owner_id) = current_owner() else {
        return Ok(None);
    };
    let Some(weak) = lua.app_data_ref::<std::sync::Weak<crate::KernelInner>>().map(|w| w.clone()) else {
        return Ok(None);
    };
    let Some(k) = Kernel::from_weak(&weak) else {
        return Ok(None);
    };
    let disp_id = {
        let mut st = k.st();
        let Some(ctx) = st.ctxs.get_mut(&owner_id) else {
            return Ok(None);
        };
        if ctx.origin == "root" || ctx.disposed {
            return Ok(None);
        }
        ctx.next_disposer_id = ctx.next_disposer_id.saturating_add(1);
        let id = ctx.next_disposer_id;
        ctx.owner_disposers.insert(id, dispose);
        id
    };
    let detach = lua.create_function(move |_, ()| {
        if let Some(k) = Kernel::from_weak(&weak)
            && let Some(ctx) = k.st().ctxs.get_mut(&owner_id)
        {
            ctx.owner_disposers.remove(&disp_id);
        }
        Ok(())
    })?;
    Ok(Some(detach))
}

/// Run `fut` as exactly `owner`'s work: a plugin's (`Some`) or the host's (`None`). `None` sheds
/// a surrounding owner, so a callback the host registered that runs synchronously
/// (`scope.run`) inside a plugin's task is not attributed to that plugin. A cordis host scope
/// captures `current_owner()` with the grant and runs its callbacks through this, so the emitter
/// identity of an event is the scope's owner and cannot be passed in as an argument.
pub(crate) async fn in_owner<F: std::future::Future>(owner: Option<u64>, fut: F) -> F::Output {
    OWNER.scope(owner, fut).await
}

/// Run `fut` as host work (`owner = None`), shedding any surrounding plugin owner scope (for
/// example, a host callback invoked from inside a Lua service method).
pub async fn as_host<F: std::future::Future>(fut: F) -> F::Output {
    in_owner(None, fut).await
}

/// Default answer time for a policy / rewrite listener that didn't set `timeout_ms` (
/// a listener that never answers must still end in `deny`); `policy_timeout_ms` overrides it.
pub const DEFAULT_POLICY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

impl Kernel {
    /// A voter's timeout when it names none: `policy_timeout_ms`, else [`DEFAULT_POLICY_TIMEOUT`].
    pub(crate) fn policy_timeout(&self) -> std::time::Duration {
        self.config()
            .get("policy_timeout_ms")
            .and_then(|v| v.as_u64())
            .map(std::time::Duration::from_millis)
            .unwrap_or(DEFAULT_POLICY_TIMEOUT)
    }
}

fn check_origin(origin: &str) -> mlua::Result<()> {
    match origin {
        "builtin" | "package" | "user" | "project" | "root" => Ok(()),
        other => Err(mlua::Error::runtime(format!("unknown plugin origin `{other}`"))),
    }
}

pub(crate) const HOST_METHODS_KEY: &str = "mare.host_methods";

impl Kernel {
    fn host_methods_table(&self) -> mlua::Result<Table> {
        let lua = self.lua();
        match lua.named_registry_value::<Table>(HOST_METHODS_KEY) {
            Ok(t) => Ok(t),
            Err(_) => {
                let t = lua.create_table()?;
                lua.set_named_registry_value(HOST_METHODS_KEY, t.clone())?;
                Ok(t)
            }
        }
    }

    /// Register a synchronous host-provided contract method (`provider = "host"`), keyed by
    /// `<service>.<method>`. Invoked through `mare.Service`'s `guarded()` without yielding.
    pub fn register_host_method(&self, service: &str, method: &str, func: Function) -> mlua::Result<()> {
        let reg = self.host_methods_table()?;
        let entry = self.lua().create_table()?;
        entry.set("fn", func)?;
        entry.set("async", false)?;
        reg.set(format!("{service}.{method}"), entry)?;
        Ok(())
    }

    /// Register an asynchronous host-provided contract method (`provider = "host"`), keyed by
    /// `<service>.<method>`. Preserves the caller's active `exec_ctx` across awaits.
    pub fn register_host_method_async(&self, service: &str, method: &str, func: Function) -> mlua::Result<()> {
        let lua = self.lua();
        let wrapped = lua.create_async_function(move |lua, args: MultiValue| {
            let ctx = mare_shared::exec_ctx::current(&lua);
            let func = func.clone();
            async move { mare_shared::exec_ctx::call_in::<MultiValue>(&lua, ctx, func, args).await }
        })?;
        let reg = self.host_methods_table()?;
        let entry = lua.create_table()?;
        entry.set("fn", wrapped)?;
        entry.set("async", true)?;
        reg.set(format!("{service}.{method}"), entry)?;
        Ok(())
    }

    /// Look up a registered host-provided contract method `(fn, is_async)` by `<service>.<method>`.
    pub(crate) fn host_method_entry(&self, service: &str, method: &str) -> mlua::Result<Option<(Function, bool)>> {
        let lua = self.lua();
        let Ok(reg) = lua.named_registry_value::<Table>(HOST_METHODS_KEY) else {
            return Ok(None);
        };
        let key = format!("{service}.{method}");
        let Value::Table(entry) = reg.get::<Value>(key)? else {
            return Ok(None);
        };
        let func: Function = entry.get("fn")?;
        let is_async: bool = entry.get::<Option<bool>>("async")?.unwrap_or(false);
        Ok(Some((func, is_async)))
    }

    /// Return the lifetime cancellation token of owner `owner_id`, if active.
    pub fn owner_cancel(&self, owner_id: u64) -> Option<tokio_util::sync::CancellationToken> {
        let st = self.st();
        st.ctxs.get(&owner_id).filter(|c| !c.disposed).map(|c| c.cancel.clone())
    }

    /// An owner record (`prims.owner.open(name, origin)` and the Rust side): it attributes
    /// listeners, problems, timing and RPC registration ids to `name`: a plugin (the fiber that
    /// opened it closes it), or a Rust-side identity that is not one (an RPC connection's,
    /// `<root>` / `root`, released by Kernel::dispose_ctx; the successor of the old root Ctx).
    pub fn open_owner(&self, name: &str, origin: &str) -> mlua::Result<u64> {
        static NEXT_OWNER_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        check_origin(origin)?;
        let id = NEXT_OWNER_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut st = self.st();
        st.ctxs.insert(
            id,
            CtxState {
                plugin: name.to_owned(),
                origin: origin.to_owned(),
                disposed: false,
                cancel: tokio_util::sync::CancellationToken::new(),
                registrations: Default::default(),
                owner_disposers: Default::default(),
                next_disposer_id: 0,
            },
        );
        Ok(id)
    }

    /// `prims.owner.close(id)`: drop the record, cancel what it still runs, and invoke owner disposers. Idempotent.
    pub(crate) fn owner_close(&self, id: u64) {
        let disposers = {
            let mut st = self.st();
            let Some(mut c) = st.ctxs.remove(&id) else { return };
            c.cancel.cancel();
            // a provider its fiber did not withdraw goes with the owner (as in dispose_ctx): one slot
            // per name, so a leftover entry would keep the name taken
            st.services.retain(|_, entry| {
                let mine = entry.ctx == id;
                if mine {
                    entry.lifetime.cancel();
                }
                !mine
            });
            std::mem::take(&mut c.owner_disposers)
        };
        for (_, dispose) in disposers {
            if let Err(error) = dispose.call::<()>(()) {
                tracing::warn!("owner disposer error: {error}");
            }
        }
    }

    fn owner_live(&self, id: u64) -> mlua::Result<()> {
        if self.st().ctxs.contains_key(&id) {
            Ok(())
        } else {
            Err(mlua::Error::runtime(format!("plugin owner {id} is closed")))
        }
    }

    /// The running work's owner may still emit (a closed owner cannot); the host always may.
    fn emitter_live(&self) -> mlua::Result<()> {
        match current_owner() {
            Some(owner) => self.owner_live(owner),
            None => Ok(()),
        }
    }
}

type OwnerEnterFut = std::pin::Pin<Box<dyn std::future::Future<Output = mlua::Result<MultiValue>>>>;
type OwnerEnterSlot = std::rc::Rc<std::cell::RefCell<Option<OwnerEnterFut>>>;

struct OwnerEnterGuard {
    slot: OwnerEnterSlot,
}

impl mlua::UserData for OwnerEnterGuard {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::Close, |_, this, _: Value| {
            if let Ok(mut guard) = this.slot.try_borrow_mut() {
                drop(guard.take());
            }
            Ok(())
        });
    }
}

struct OwnerEnterPending {
    slot: OwnerEnterSlot,
}

impl mlua::UserData for OwnerEnterPending {}

thread_local! {
    /// Monotonic counter of `Poll::Pending` returns from `OwnerEnterFuture::poll` on this VM
    /// thread. Read before/after a plugin's `apply` to detect yields in O(1) without wrapping
    /// each plugin in an extra coroutine or scoped thread.
    static PENDING_POLLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

struct OwnerEnterFuture {
    slot: OwnerEnterSlot,
}

impl std::future::Future for OwnerEnterFuture {
    type Output = mlua::Result<MultiValue>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        let mut guard = self.slot.borrow_mut();
        match guard.as_mut() {
            Some(fut) => {
                let res = fut.as_mut().poll(cx);
                if res.is_pending() {
                    PENDING_POLLS.with(|c| c.set(c.get().wrapping_add(1)));
                }
                res
            }
            None => std::task::Poll::Ready(Err(mlua::Error::runtime("owner enter cancelled"))),
        }
    }
}

impl Drop for OwnerEnterFuture {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.slot.try_borrow_mut() {
            drop(guard.take());
        }
    }
}

fn owner_table(lua: &Lua, weak: &std::sync::Weak<crate::KernelInner>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let w = weak.clone();
    t.set(
        "open",
        lua.create_function(move |_, (name, origin): (String, String)| kernel(&w)?.open_owner(&name, &origin))?,
    )?;
    let w = weak.clone();
    t.set(
        "close",
        lua.create_function(move |_, id: u64| {
            kernel(&w)?.owner_close(id);
            Ok(())
        })?,
    )?;
    t.set("current", lua.create_function(|_, ()| Ok(current_owner()))?)?;
    let w_enter = weak.clone();
    let start = lua.create_function(
        move |lua, (id, caller_thread, f, args): (Option<u64>, Option<mlua::Thread>, Function, MultiValue)| {
            let scope_id = Kernel::from_weak(&w_enter).map_or(0, |k| k.resource_scope());
            let context = mare_shared::exec_ctx::current(lua);
            let scoped = mare_shared::exec_ctx::scoped_call(lua, context, f, args)?;
            let slot: OwnerEnterSlot =
                std::rc::Rc::new(std::cell::RefCell::new(Some(Box::pin(mare_shared::image_resource::in_scope(
                    scope_id,
                    in_owner(id, CALLER_THREAD.scope(caller_thread, scoped)),
                )))));
            Ok((OwnerEnterGuard { slot: slot.clone() }, OwnerEnterPending { slot }))
        },
    )?;
    let poll = lua.create_async_function(|_, pending: mlua::AnyUserData| {
        let slot = pending.borrow::<OwnerEnterPending>().map(|p| p.slot.clone());
        async move { OwnerEnterFuture { slot: slot? }.await }
    })?;
    let w_sync = weak.clone();
    let enter_sync = lua.create_function(move |_, (id, f, args): (Option<u64>, Function, MultiValue)| {
        let scope_id = Kernel::from_weak(&w_sync).map_or(0, |k| k.resource_scope());
        struct SyncOwnerGuard(Option<Option<u64>>);
        impl Drop for SyncOwnerGuard {
            fn drop(&mut self) {
                SYNC_OWNER.with(|cell| cell.set(self.0));
            }
        }
        let prev = SYNC_OWNER.with(|cell| cell.replace(Some(id)));
        let _guard = SyncOwnerGuard(prev);
        mare_shared::image_resource::in_scope_sync(scope_id, || f.call::<MultiValue>(args))
    })?;
    // A `<close>` guard on the caller's Lua frame drops the inner ScopedFuture immediately when
    // an enclosing user coroutine closes, before GC collects the suspended AsyncPoll frame.
    // Non-yieldable callers (e.g. main-thread `exec()` or `call_sync` boundaries) stay on the
    // synchronous path so `coroutine.isyieldable()` inside `f` stays accurate instead of seeing
    // an ephemeral inner `scoped_call` coroutine and failing when attempting to yield.
    let coroutine: Table = lua.globals().get("coroutine")?;
    let enter: Function = lua
        .load(
            r#"local start, poll, running, isyieldable, enter_sync = ...
            return function(id, f, ...)
                if not isyieldable() then
                    return enter_sync(id, f, ...)
                end
                local th, is_main = running()
                local guard <close>, pending = start(id, (not is_main) and th or nil, f, ...)
                return poll(pending)
            end"#,
        )
        .set_name("=prims.owner.enter")
        .call((
            start,
            poll,
            coroutine.get::<Function>("running")?,
            coroutine.get::<Function>("isyieldable")?,
            enter_sync.clone(),
        ))?;
    t.set("enter", enter)?;
    t.set("enter_sync", enter_sync)?;
    Ok(t)
}

fn events_table(lua: &Lua, weak: &std::sync::Weak<crate::KernelInner>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let w = weak.clone();
    t.set("policy_epoch", lua.create_function(move |_, ()| Ok(kernel(&w)?.policy_epoch()))?)?;
    // the bus's quiet check (bus.lua `bus.quiet`): whether a tap would take an event now, and
    // whether a tap's answer may change over time (then it is asked every time, never cached)
    let w = weak.clone();
    t.set("tapped", lua.create_function(move |_, event: String| Ok(kernel(&w)?.tapped(&event)))?)?;
    // the bus's quiet return keeps the Rust entry's emitter check: a closed owner cannot emit
    let w = weak.clone();
    t.set("emitter_live", lua.create_function(move |_, ()| kernel(&w)?.emitter_live())?)?;
    let w = weak.clone();
    t.set("taps_dynamic", lua.create_function(move |_, ()| Ok(kernel(&w)?.taps_dynamic()))?)?;
    // the bus (one bus): one listener call, its busy time recorded as the engine did for its own
    // (mare.profile callback timings, slow-callback reports)
    let w = weak.clone();
    t.set(
        "timed",
        lua.create_async_function(move |lua, (event, plugin, f): (String, String, Function)| {
            let w = w.clone();
            async move {
                let call = mare_shared::lua::call_async::<MultiValue>(&lua, &f, ());
                let (r, busy) = crate::dispatch::timing::timed(call).await;
                kernel(&w)?.record_slow_callback(&event, &plugin, busy);
                r
            }
        })?,
    )?;
    let w = weak.clone();
    t.set(
        "check_dispatch",
        lua.create_function(move |_, (mode, event, bus_owner, known): (String, String, Option<u64>, Option<bool>)| {
            let caller = (known == Some(true)).then_some(bus_owner);
            kernel(&w)?.validate_event_dispatch_caller(&mode, &event, caller)
        })?,
    )?;
    let w = weak.clone();
    // dispatch(mode, event, ...): the bus host plugin's modes (emit, emit_checked, bail, waterfall,
    // waterfall_checked; the waterfall forms take the value first). A decision is the policy
    // service's (policy:voter, Kernel::policy*), never a dispatch. The emitter is the running
    // scope's owner (`emitter()`), never an argument; a closed owner cannot emit.
    t.set(
        "dispatch",
        lua.create_async_function(move |_, (mode, event, rest): (String, String, MultiValue)| {
            let w = w.clone();
            async move {
                let k = kernel(&w)?;
                k.emitter_live()?;
                dispatch(&k, &mode, &event, rest).await
            }
        })?,
    )?;
    let w = weak.clone();
    t.set(
        "envelope",
        lua.create_function(move |lua, (event, arg): (String, Value)| {
            let mv = MultiValue::from_vec(vec![arg]);
            kernel(&w)?.current_envelope(&event, &mv, None).to_lua(lua)
        })?,
    )?;
    // emitter() -> the owner id the running work acts for, or "host" (Rust and the root)
    t.set(
        "emitter",
        lua.create_function(|lua, ()| match current_owner() {
            Some(id) => Ok(Value::Integer(id as i64)),
            None => Ok(Value::String(lua.create_string("host")?)),
        })?,
    )?;
    Ok(t)
}

async fn dispatch(k: &Kernel, mode: &str, event: &str, rest: MultiValue) -> mlua::Result<MultiValue> {
    let mut rest = rest;
    let none = || Ok(MultiValue::new());
    let one = |v: Value| Ok(MultiValue::from_vec(vec![v]));
    let mut first = || rest.pop_front().unwrap_or(Value::Nil);
    match mode {
        "emit" => {
            k.emit(event, rest).await;
            none()
        }
        "emit_checked" => k.emit_checked(event, rest).await.and_then(|()| none()),
        "bail" => one(k.bail(event, rest).await),
        "waterfall" => {
            let value = first();
            one(k.waterfall(event, value, rest).await)
        }
        "waterfall_checked" => {
            let value = first();
            one(k.waterfall_checked(event, value, rest).await?)
        }
        other => Err(mlua::Error::runtime(format!("unknown dispatch mode `{other}`"))),
    }
}

fn kernel(w: &std::sync::Weak<crate::KernelInner>) -> mlua::Result<Kernel> {
    Kernel::from_weak(w).ok_or_else(|| mlua::Error::runtime("kernel is gone"))
}

/// Plugin-tree primitives in `prims`: `owner`, `events`, `definition`,
/// `module_dir`, `reload`. Called by `lua_env/cordis_host.rs::prims`.
pub(crate) fn add_prims(lua: &Lua, t: &Table, weak: std::sync::Weak<crate::KernelInner>) -> mlua::Result<()> {
    t.set("owner", owner_table(lua, &weak)?)?;
    t.set("events", events_table(lua, &weak)?)?;
    // barrier(service, method): the kernel's poisoned-session admission check for a service method,
    // driven by the service contracts' `poisoned` allowlists
    // (the kernel is resolved per call, like the other prims: building prims needs no kernel)
    let w = weak.clone();
    t.set(
        "barrier",
        lua.create_function(move |lua, (service, method): (String, String)| {
            let k = kernel(&w)?;
            if let Err(err) = mare_shared::session_barrier::barrier(lua).check()
                && !k.allows_when_poisoned(&service, &method)
            {
                return Err(err);
            }
            Ok(())
        })?,
    )?;
    let w = weak.clone();
    // definition(name) -> the evaluated definition table | nil
    t.set("definition", lua.create_function(move |_, name: String| Ok(kernel(&w)?.def(&name).map(|d| d.table)))?)?;
    let w = weak.clone();
    // module_dir(name): register the plugin's `lua/` modules for require (idempotent)
    t.set(
        "module_dir",
        lua.create_function(move |_, name: String| {
            let k = kernel(&w)?;
            let d = k.def(&name).ok_or_else(|| mlua::Error::runtime(format!("module '{name}' not found:")))?;
            k.add_module_dir(&d);
            Ok(())
        })?,
    )?;
    let reload = lua.create_table()?;
    let w = weak.clone();
    reload.set(
        "barrier",
        lua.create_async_function(move |_, ()| {
            let w = w.clone();
            async move {
                kernel(&w)?.reload_barrier().await;
                Ok(())
            }
        })?,
    )?;
    let w = weak.clone();
    reload.set("generation", lua.create_function(move |_, ()| Ok(kernel(&w)?.reload_generation()))?)?;
    t.set("reload", reload)?;
    t.set("poll_count", lua.create_function(|_, ()| Ok(PENDING_POLLS.with(std::cell::Cell::get)))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_with_prims() -> Kernel {
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let t = k.lua().create_table().unwrap();
        add_prims(k.lua(), &t, k.weak()).unwrap();
        k.lua().globals().set("prims", t).unwrap();
        k
    }

    /// Every Lua source under `dirs` (relative to this crate) with a line holding `needle`.
    fn lua_lines_with(dirs: &[&str], needle: &str) -> Vec<String> {
        fn walk(dir: &std::path::Path, needle: &str, out: &mut Vec<String>) {
            let entries =
                std::fs::read_dir(dir).unwrap_or_else(|error| panic!("cannot scan {}: {error}", dir.display()));
            for entry in entries {
                let entry = entry.unwrap_or_else(|error| panic!("cannot scan {}: {error}", dir.display()));
                let path = entry.path();
                let metadata = std::fs::metadata(&path)
                    .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", path.display()));
                if metadata.is_dir() {
                    walk(&path, needle, out);
                } else if path.extension().is_some_and(|e| e == "lua") {
                    let text = std::fs::read_to_string(&path)
                        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
                    for (i, line) in text.lines().enumerate() {
                        if line.contains(needle) {
                            out.push(format!("{}:{}", path.display(), i + 1));
                        }
                    }
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = vec![];
        for dir in dirs {
            walk(&root.join(dir), needle, &mut out);
        }
        out
    }

    #[test]
    fn legacy_scan_reports_unreadable_directory() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("__missing_legacy_scan_fixture__");
        let error = std::panic::catch_unwind(|| lua_lines_with(&[path.to_str().unwrap()], "needle"))
            .expect_err("a missing scan directory must fail");
        let message = error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| error.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap();
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    #[test]
    fn legacy_scan_reports_unreadable_lua_file() {
        let dir = std::env::temp_dir().join(format!("mare-legacy-scan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("invalid.lua");
        std::fs::write(&path, [0xff]).unwrap();
        let error = std::panic::catch_unwind(|| lua_lines_with(&[dir.to_str().unwrap()], "needle"));
        std::fs::remove_dir_all(&dir).unwrap();
        let error = error.expect_err("an unreadable Lua source must fail");
        let message = error
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| error.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap();
        assert!(message.contains(&path.display().to_string()), "{message}");
    }

    /// The legacy registration prim is gone: a plugin listens with
    /// `ctx:on`, the Rust host with `Kernel::host_on` / `host_voter`. Nothing reaches for it.
    #[tokio::test]
    async fn no_legacy_registration_prim_is_left() {
        let k = kernel_with_prims();
        let on: mlua::Value = k.lua().load("return prims.events.on").eval_async().await.unwrap();
        assert!(on.is_nil(), "prims.events.on is back: {on:?}");
        let callers = lua_lines_with(&["lua", "lua-specs", "../mare-plugin/preset"], "prims.events.on");
        assert!(callers.is_empty(), "callers of the removed prims.events.on: {callers:?}");
    }

    /// The dispatch prim takes the bus host plugin's five modes only (step 6): a decision is the
    /// policy service's, so the old engine's policy / policy_with / rewrite dispatch is gone.
    #[tokio::test]
    async fn the_dispatch_prim_takes_the_bus_modes_only() {
        let k = kernel_with_prims();
        for mode in ["policy", "policy_with", "rewrite"] {
            let err: String = k
                .lua()
                .load(format!(r#"local ok, e = pcall(prims.events.dispatch, "{mode}", "probe/x") assert(not ok) return tostring(e)"#))
                .eval_async()
                .await
                .unwrap();
            assert!(err.contains("unknown dispatch mode"), "{mode}: {err}");
        }
    }

    /// The bus's legacy registration is gone (step 6): every listener is a cordis `ctx:on`, run in
    /// the context it registered in, in registration order; no numeric order, no emitter-context
    /// branch, no per-listener answer time, no policy entries made through the bus.
    #[test]
    fn no_legacy_bus_registration_is_left() {
        for needle in ["bus.on(", "bus.position", "attach_policy", "emitter_context", "hook.timeout_ms"] {
            let users = lua_lines_with(&["lua", "lua-specs", "../mare-plugin/preset"], needle);
            assert!(users.is_empty(), "`{needle}` of the removed legacy bus registration: {users:?}");
        }
    }

    /// An owner record names a known origin: an unknown one is refused.
    #[tokio::test]
    async fn an_unknown_origin_is_refused() {
        let k = kernel_with_prims();
        let err: String = k
            .lua()
            .load(
                r#"
                local ok, e = pcall(prims.owner.open, "probe", "nowhere")
                assert(not ok)
                return tostring(e)
                "#,
            )
            .eval_async()
            .await
            .unwrap();
        assert!(err.contains("unknown plugin origin"), "{err}");
    }

    /// The emitter is the running scope's owner: a closed owner cannot emit, the host is "host".
    #[tokio::test]
    async fn the_emitter_comes_from_the_running_scope() {
        let k = kernel_with_prims();
        let (host, inside, err): (String, bool, String) = k
            .lua()
            .load(
                r#"
                local id = prims.owner.open("probe", "user")
                local inside = prims.owner.enter(id, prims.events.emitter) == id
                prims.owner.close(id)
                local ok, e = pcall(prims.owner.enter, id, prims.events.dispatch, "emit", "probe/x")
                assert(not ok)
                return prims.events.emitter(), inside, tostring(e)
                "#,
            )
            .eval_async()
            .await
            .unwrap();
        assert_eq!(host, "host");
        assert!(inside);
        assert!(err.contains("is closed"), "{err}");
    }

    /// `in_owner(None, ..)` is the host's work even inside a plugin's: a host callback that a
    /// plugin's task runs synchronously is not attributed to the plugin (cordis-core's finding).
    #[tokio::test]
    async fn in_owner_none_sheds_the_surrounding_owner() {
        let (outer, inner, back) = in_owner(Some(7), async {
            let outer = current_owner();
            let inner = in_owner(None, async { current_owner() }).await;
            (outer, inner, current_owner())
        })
        .await;
        assert_eq!((outer, inner, back), (Some(7), None, Some(7)));
        assert_eq!(current_owner(), None);
    }

    /// The same through the Lua primitives: a host function entered from a plugin's work runs as
    /// the host, so it emits as "host".
    #[tokio::test]
    async fn a_host_callback_inside_a_plugins_work_emits_as_the_host() {
        let k = kernel_with_prims();
        let host_fn: Function = k
            .lua()
            .create_async_function(|lua, ()| async move {
                in_owner(None, async move {
                    let emitter: Function = lua.load("return prims.events.emitter").eval()?;
                    emitter.call_async::<Value>(()).await
                })
                .await
            })
            .unwrap();
        k.lua().globals().set("host_fn", host_fn).unwrap();
        let (inside, host): (bool, String) = k
            .lua()
            .load(
                r#"
                local id = prims.owner.open("probe", "user")
                return prims.owner.enter(id, function()
                    return prims.events.emitter() == id, host_fn()
                end)
                "#,
            )
            .eval_async()
            .await
            .unwrap();
        assert!(inside);
        assert_eq!(host, "host");
    }

    /// `enter` attributes the running work to an owner; outside it there is none.
    #[tokio::test]
    async fn enter_sets_the_current_owner_for_the_call_only() {
        let k = kernel_with_prims();
        let (inside, outside): (Option<u64>, Option<u64>) = k
            .lua()
            .load(
                r#"
                local id = prims.owner.open("probe", "user")
                local inside = prims.owner.enter(id, function() return prims.owner.current() end)
                return inside == id and id or nil, prims.owner.current()
                "#,
            )
            .eval_async()
            .await
            .unwrap();
        assert!(inside.is_some());
        assert_eq!(outside, None);
    }

    #[tokio::test]
    async fn owner_enter_closes_inner_scoped_future_synchronously_on_coroutine_close() {
        let k = kernel_with_prims();
        let notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let n_wait = notify.clone();
        let wait_forever = k
            .lua()
            .create_async_function(move |_, ()| {
                let n = n_wait.clone();
                async move {
                    n.notified().await;
                    Ok(())
                }
            })
            .unwrap();
        k.lua().globals().set("wait_forever", wait_forever).unwrap();

        let closed_before_gc: bool = k
            .lua()
            .load(
                r#"
                local id = prims.owner.open("probe", "user")
                local closed = false
                local co = coroutine.create(function()
                    return prims.owner.enter(id, function()
                        local guard <close> = setmetatable({}, {
                            __close = function() closed = true end,
                        })
                        return wait_forever()
                    end)
                end)
                local ok = coroutine.resume(co)
                assert(ok and coroutine.status(co) == "suspended")
                assert(not closed)
                coroutine.close(co)
                return closed
                "#,
            )
            .eval_async()
            .await
            .unwrap();
        assert!(closed_before_gc, "coroutine.close must drop prims.owner.enter future before GC");

        // Polling OwnerEnterFuture after slot cleared yields "owner enter cancelled".
        let slot: OwnerEnterSlot = std::rc::Rc::new(std::cell::RefCell::new(Some(Box::pin(async {
            std::future::pending::<mlua::Result<MultiValue>>().await
        }))));
        let _ = slot.borrow_mut().take();
        let err = OwnerEnterFuture { slot }.await.unwrap_err().to_string();
        assert!(err.contains("owner enter cancelled"), "{err}");
    }
}
