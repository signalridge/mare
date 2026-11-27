//! Tool execution context: every model-initiated tool call runs in a Lua
//! thread tagged with a context; threads spawned from it (`mare.task.spawn`, `ctx:spawn`,
//! `coroutine.create/wrap`) inherit the tag. Core primitives (`mare.fs`, `mare.proc`,
//! `mare.http`, `mare.net`, …) look the tag up for the *calling* thread and enforce the sandbox
//! policy, so no tool implementation can bypass it. Code outside tool calls has no tag and is
//! unrestricted (plugins are trusted; the model is not).
//!
//! The tag is a Lua table `{ tool, tool_call_id, sandbox?, unconfined? }` stored in a weak-keyed
//! registry table (thread → ctx), so finished coroutines drop out with the GC.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_int, c_void};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use mlua::{Function, Lua, MultiValue, Table, Thread, UserData, UserDataMethods, Value};

const REGISTRY_KEY: &str = "mare.exec_ctx";

fn registry(lua: &Lua) -> mlua::Result<Table> {
    if let Ok(t) = lua.named_registry_value::<Table>(REGISTRY_KEY) {
        return Ok(t);
    }
    let t = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__mode", "k")?;
    t.set_metatable(Some(mt))?;
    lua.set_named_registry_value(REGISTRY_KEY, t.clone())?;
    Ok(t)
}

/// Per-VM state stored in `Lua` app-data so `current()` can short-circuit in O(1) before the
/// first tool call without mutably borrowing `app_data` on `bind()`, and `scoped_call` can verify
/// that the caller's root thread is the VM main thread before taking the ownerless fast path.
struct ExecCtxFlag {
    any_bound: std::sync::atomic::AtomicBool,
    main_thread_ptr: usize,
}

/// Fallback set if `bind(_, Some(_))` runs on a VM without an eager `ExecCtxFlag` while another
/// `app_data` borrow is active (guarantees `bind()` never panics on `set_app_data`).
static UNTRACKED_EXEC_CTX_BOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Eagerly install the per-VM exec_ctx state so `bind()` never needs to mutate `app_data` and
/// `scoped_call()` knows the VM's root main thread pointer. Must be called from the main thread.
pub fn init(lua: &Lua) {
    if lua.try_app_data_ref::<ExecCtxFlag>().ok().flatten().is_none() {
        lua.set_app_data(ExecCtxFlag {
            any_bound: std::sync::atomic::AtomicBool::new(false),
            main_thread_ptr: lua.current_thread().to_pointer() as usize,
        });
    }
}

fn is_main_thread(lua: &Lua) -> bool {
    match lua.try_app_data_ref::<ExecCtxFlag>() {
        Ok(Some(flag)) => lua.current_thread().to_pointer() as usize == flag.main_thread_ptr,
        _ => false,
    }
}

fn any_bound(lua: &Lua) -> bool {
    if UNTRACKED_EXEC_CTX_BOUND.load(std::sync::atomic::Ordering::Acquire) {
        return true;
    }
    match lua.try_app_data_ref::<ExecCtxFlag>() {
        Ok(Some(flag)) => flag.any_bound.load(std::sync::atomic::Ordering::Acquire),
        Ok(None) => false,
        // Conservatively fall back to registry lookup if app_data is mutably borrowed.
        Err(_) => true,
    }
}

/// The context of the thread calling into Rust right now (call this synchronously at the
/// start of a callback, before any `.await`).
pub fn current(lua: &Lua) -> Option<Table> {
    if !any_bound(lua) {
        return None;
    }
    let reg = registry(lua).ok()?;
    reg.get::<Option<Table>>(lua.current_thread()).ok().flatten()
}

/// Tag `thread` with `ctx` (`None` removes the tag).
pub fn bind(lua: &Lua, thread: &Thread, ctx: Option<Table>) -> mlua::Result<()> {
    if ctx.is_some() {
        // Never call `set_app_data` here: another caller may hold an `AppDataRef` on the VM.
        // Initialized VMs flip their per-VM atomic flag; uninitialized VMs fall back to the
        // process-wide atomic so `current()` never misses a binding and `bind()` never panics.
        if let Ok(Some(flag)) = lua.try_app_data_ref::<ExecCtxFlag>() {
            flag.any_bound.store(true, std::sync::atomic::Ordering::Release);
        } else {
            UNTRACKED_EXEC_CTX_BOUND.store(true, std::sync::atomic::Ordering::Release);
        }
    } else if !any_bound(lua) {
        return Ok(());
    }
    registry(lua)?.set(thread.clone(), ctx)
}

/// Drop the recyclable call before removing its execution context.
struct ScopedFuture {
    future: Option<mlua::function::AsyncCallFuture<MultiValue>>,
    owner: Option<Thread>,
}

impl Future for ScopedFuture {
    type Output = mlua::Result<MultiValue>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(self.future.as_mut().expect("scoped future present")).poll(cx)
    }
}

impl Drop for ScopedFuture {
    fn drop(&mut self) {
        drop(self.future.take());
        // WeakLua avoids a strong cycle when CoroutineDriver owns ScopedFuture inside Lua.
        if let Some(owner) = &self.owner
            && let Some(lua) = owner.weak_lua().try_upgrade()
        {
            let _ = bind(&lua, owner, None);
        }
    }
}

/// Run `f(args)` as mlua's recyclable `call_async`, inside the tool context `context`. An implicit
/// `call_async` thread reports its root owner from `Lua::current_thread`, so the call gets a
/// distinct, explicitly tagged owner thread and is constructed while that owner runs: the tag then
/// survives cancellation and every closing hook. Dropping the future (a cancellation) lets mlua
/// close the call's `<close>` scopes at once, before the owner's tag is removed; a thread owned
/// with `into_async` is never resumed with mlua's terminate signal, so its scopes stayed open.
/// A `None` context intentionally gives the owner no tool context, even inside another call.
pub fn scoped_call(
    lua: &Lua,
    context: Option<Table>,
    f: Function,
    args: MultiValue,
) -> mlua::Result<impl Future<Output = mlua::Result<MultiValue>> + use<>> {
    scoped_call_future(lua, context, f, args)
}

fn scoped_call_future(lua: &Lua, context: Option<Table>, f: Function, args: MultiValue) -> mlua::Result<ScopedFuture> {
    // Fast path: only safe when the calling thread's root is the VM main thread (which is
    // guaranteed to outlive detached tasks such as `Scope::spawn`), neither requested nor current
    // context carries a tool tag, and Lua coverage hooks are inactive. Non-main callers (transient
    // listener owner threads, managed coroutines) must fall back to the slow path so `ScopedFuture`
    // owns a dedicated live owner thread; `mlua::Lua::current_thread()` dereferences the recorded
    // root owner raw pointer.
    if context.is_none() && is_main_thread(lua) && current(lua).is_none() && !crate::lua::has_cov_hook(lua) {
        return Ok(ScopedFuture { future: Some(f.call_async::<MultiValue>(args)), owner: None });
    }
    type Pending = mlua::function::AsyncCallFuture<MultiValue>;
    let slot: Rc<RefCell<Option<Pending>>> = Rc::new(RefCell::new(None));
    let out = slot.clone();
    let mut call = Some((f, args));
    let starter = lua.create_function_mut(move |_, ()| {
        let (f, args) = call.take().expect("scoped_call starter runs once");
        *out.borrow_mut() = Some(f.call_async::<MultiValue>(args));
        Ok(())
    })?;
    let owner = lua.create_thread(starter)?;
    crate::lua::attach_cov_hook(lua, &owner);
    bind(lua, &owner, context)?;
    let mut scoped = ScopedFuture { future: None, owner: Some(owner.clone()) };
    owner.resume::<()>(())?;
    scoped.future = Some(slot.borrow_mut().take().expect("starter installed the call"));
    Ok(scoped)
}

/// Run `f(args…)` in a fresh thread tagged with `ctx`. A caller that already runs inside a
/// context keeps it: nested calls can never drop the tag (only tools-service code enters one).
pub async fn run_in(lua: &Lua, ctx: Table, f: Function, args: MultiValue) -> mlua::Result<MultiValue> {
    let ctx = current(lua).unwrap_or(ctx);
    scoped_call(lua, Some(ctx), f, args)?.await
}

struct ProxyWakerState {
    waker: Option<Waker>,
    woken: bool,
}

struct ProxyWaker(Mutex<ProxyWakerState>);

impl ProxyWaker {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(ProxyWakerState { waker: None, woken: false })))
    }

    fn clear_woken(&self) {
        self.0.lock().expect("proxy waker lock").woken = false;
    }

    fn arm_or_ready(&self, waker: &Waker) -> bool {
        let mut state = self.0.lock().expect("proxy waker lock");
        if state.woken {
            state.woken = false;
            return true;
        }
        state.waker = Some(waker.clone());
        false
    }
}

impl Wake for ProxyWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        let waker = {
            let mut state = self.0.lock().expect("proxy waker lock");
            state.woken = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

enum YieldSlot {
    Idle,
    Yielded(MultiValue),
    Resumed(MultiValue),
}

type YieldMap = Rc<RefCell<HashMap<*const c_void, Rc<RefCell<YieldSlot>>>>>;
type PendingMap = Rc<RefCell<HashMap<*const c_void, Arc<ProxyWaker>>>>;

fn error_to_value(lua: &Lua, error: &mlua::Error) -> mlua::Result<Value> {
    if let Some(table) = crate::lua_error::recall_mlua_error(lua, error) {
        return Ok(Value::Table(table));
    }
    Ok(Value::String(lua.create_string(error.to_string())?))
}

struct CoroutineDriver(RefCell<CoroutineDriverInner>);

struct CoroutineDriverInner {
    f: Function,
    wrapped_f: Function,
    future: Option<ScopedFuture>,
    yield_slot: Rc<RefCell<YieldSlot>>,
    proxy_waker: Arc<ProxyWaker>,
    close_err: Rc<RefCell<Option<Value>>>,
    co_ptr: *const c_void,
    exec_ptr: Rc<Cell<Option<*const c_void>>>,
    exec_threads: YieldMap,
    pending_wakers: PendingMap,
    was_yielded: bool,
    body_err: Option<Value>,
}

impl CoroutineDriverInner {
    fn cleanup(&mut self, lua: Option<&Lua>) {
        if let (Some(err_val), Some(ptr), Some(lua)) = (self.body_err.take(), self.exec_ptr.get(), lua) {
            let exec_l = ptr as *mut mlua::ffi::lua_State;
            // SAFETY: `exec_l` is owned by `self.future`, still alive until dropped below.
            // `mlua::Thread::resume_inner` popped the error object on `Err`, leaving a stale
            // value on top of the errored thread's stack; push the real error back so
            // `lua_closethread` forwards it to `<close>` metamethods as Lua 5.5 expects.
            let _ = unsafe {
                lua.exec_raw::<()>(err_val, |l| {
                    if mlua::ffi::lua_checkstack(exec_l, 1) != 0 {
                        mlua::ffi::lua_xmove(l, exec_l, 1);
                    }
                })
            };
        }
        self.body_err = None;
        drop(self.future.take());
        if let Some(ptr) = self.exec_ptr.take() {
            self.exec_threads.borrow_mut().remove(&ptr);
        }
        self.pending_wakers.borrow_mut().remove(&self.co_ptr);
        // Unblock any `await_pending` waiter when the coroutine is closed or dropped mid-wait.
        self.proxy_waker.wake_by_ref();
    }

    fn step(&mut self, lua: &Lua, args: MultiValue) -> mlua::Result<MultiValue> {
        if self.future.is_none() {
            let yield_slot = self.yield_slot.clone();
            let exec_ptr = self.exec_ptr.clone();
            let exec_threads = self.exec_threads.clone();
            let on_enter = lua.create_function(move |_, th: Thread| {
                let ptr = th.to_pointer();
                exec_ptr.set(Some(ptr));
                exec_threads.borrow_mut().insert(ptr, yield_slot.clone());
                Ok(())
            })?;
            let close_err = self.close_err.clone();
            let on_close = lua.create_function(move |_, err: Value| {
                *close_err.borrow_mut() = Some(err);
                Ok(())
            })?;
            let mut exec_args = MultiValue::new();
            exec_args.push_back(Value::Function(on_enter));
            exec_args.push_back(Value::Function(on_close));
            exec_args.push_back(Value::Function(self.f.clone()));
            for value in args {
                exec_args.push_back(value);
            }
            self.pending_wakers.borrow_mut().insert(self.co_ptr, self.proxy_waker.clone());
            let fut = scoped_call_future(lua, current(lua), self.wrapped_f.clone(), exec_args)?;
            self.future = Some(fut);
        } else if self.was_yielded {
            *self.yield_slot.borrow_mut() = YieldSlot::Resumed(args);
        }

        self.proxy_waker.clear_woken();
        let waker = Waker::from(self.proxy_waker.clone());
        let mut cx = Context::from_waker(&waker);
        let poll = Pin::new(self.future.as_mut().expect("driver future present")).poll(&mut cx);
        match poll {
            Poll::Pending => {
                let yielded = {
                    let mut slot = self.yield_slot.borrow_mut();
                    match std::mem::replace(&mut *slot, YieldSlot::Idle) {
                        YieldSlot::Yielded(vals) => Some(vals),
                        other => {
                            *slot = other;
                            None
                        }
                    }
                };
                if let Some(vals) = yielded {
                    self.was_yielded = true;
                    let mut out = vals;
                    out.push_front(Value::String(lua.create_string("yield")?));
                    Ok(out)
                } else {
                    self.was_yielded = false;
                    Ok(MultiValue::from_vec(vec![Value::String(lua.create_string("pending")?)]))
                }
            }
            Poll::Ready(Ok(mut vals)) => {
                self.cleanup(Some(lua));
                vals.push_front(Value::String(lua.create_string("ready")?));
                Ok(vals)
            }
            Poll::Ready(Err(err)) => {
                // Keep `future` until `close` so unclosed error scopes behave like native coroutines.
                let err_val = error_to_value(lua, &err)?;
                self.body_err = Some(err_val.clone());
                Ok(MultiValue::from_vec(vec![Value::String(lua.create_string("err")?), err_val]))
            }
        }
    }

    fn close(&mut self, lua: &Lua) -> MultiValue {
        self.cleanup(Some(lua));
        match self.close_err.borrow_mut().take() {
            Some(err) if !err.is_nil() => MultiValue::from_vec(vec![Value::Boolean(false), err]),
            _ => MultiValue::from_vec(vec![Value::Boolean(true)]),
        }
    }
}

impl Drop for CoroutineDriverInner {
    fn drop(&mut self) {
        let lua = self.f.weak_lua().try_upgrade();
        self.cleanup(lua.as_ref());
    }
}

impl UserData for CoroutineDriver {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("step", |lua, this, args: MultiValue| this.0.borrow_mut().step(lua, args));
        m.add_method("close", |lua, this, ()| Ok(this.0.borrow_mut().close(lua)));
    }
}

/// `coroutine.create` / `coroutine.wrap` that propagate the caller's context to the new
/// coroutine, and a `coroutine.resume` that lets async primitives work inside them (installed
/// over the stdlib versions).
///
/// An async primitive (`mare.ui.confirm`, `mare.task.sleep`, …) waits by yielding mlua's
/// "pending" marker to the poller of the async thread it runs in. Inside a coroutine the code
/// made itself (a key binding written as `coroutine.wrap(function() … end)()`), that yield
/// stopped at the nearest `resume` instead: the primitive "returned" the marker at once and its
/// coroutine was never resumed — a dialog that opened but whose answer went nowhere. `resume`
/// now awaits pending waits via an async helper (while the caller can yield) and resumes the
/// inner coroutine when ready, so the wait goes through every level. Each managed coroutine runs
/// its body inside a recyclable `scoped_call` owned by a `<close>` driver on the coroutine's
/// stack, so cancelling or closing the coroutine drops suspended Rust futures and closes nested
/// `<close>` scopes at once without waiting for GC. Explicit user yields return normally and leave
/// the coroutine resumable.
pub fn install_coroutine_inheritance(lua: &Lua) -> mlua::Result<()> {
    init(lua);
    let co: Table = lua.globals().get("coroutine")?;
    let raw_create: Function = co.get("create")?;
    let raw_resume: Function = co.get("resume")?;
    let raw_yield: Function = co.get("yield")?;
    let raw_running: Function = co.get("running")?;

    let exec_threads: YieldMap = Rc::new(RefCell::new(HashMap::new()));
    let pending_wakers: PendingMap = Rc::new(RefCell::new(HashMap::new()));

    let pending_wakers_wait = pending_wakers.clone();
    let await_pending = lua.create_async_function(move |_, thread: Thread| {
        let pw = pending_wakers_wait.borrow().get(&thread.to_pointer()).cloned();
        async move {
            let Some(pw) = pw else {
                return Ok(());
            };
            let mut first_poll = true;
            let mut yielded = false;
            std::future::poll_fn(|cx| {
                if yielded {
                    return Poll::Ready(());
                }
                let already_ready = pw.arm_or_ready(cx.waker());
                if already_ready {
                    if first_poll {
                        // A synchronous wake inside `step()` (e.g. cooperative yield) must yield
                        // one scheduler turn to avoid starving single-threaded `LocalSet` peers.
                        first_poll = false;
                        yielded = true;
                        cx.waker().wake_by_ref();
                        return Poll::Pending;
                    }
                    return Poll::Ready(());
                }
                first_poll = false;
                Poll::Pending
            })
            .await;
            Ok(())
        }
    })?;

    let exec_threads_yield = exec_threads.clone();
    let running_for_yield = raw_running.clone();
    let await_yield = lua.create_async_function(move |_lua, args: MultiValue| {
        let th: mlua::Result<Thread> = running_for_yield.call(());
        let slot = th.ok().and_then(|t| exec_threads_yield.borrow().get(&t.to_pointer()).cloned());
        async move {
            let Some(slot) = slot else {
                return Err(mlua::Error::runtime("coroutine.yield called outside managed coroutine"));
            };
            *slot.borrow_mut() = YieldSlot::Yielded(args);
            std::future::poll_fn(|_| {
                let mut guard = slot.borrow_mut();
                match std::mem::replace(&mut *guard, YieldSlot::Idle) {
                    YieldSlot::Resumed(resumed) => Poll::Ready(Ok(resumed)),
                    other => {
                        *guard = other;
                        Poll::Pending
                    }
                }
            })
            .await
        }
    })?;

    const YIELD_DISPATCH_KEY: &CStr = c"mare.exec_ctx.yield_dispatch";
    const AWAIT_YIELD_KEY: &CStr = c"mare.exec_ctx.await_yield";
    struct YieldDispatch {
        exec_threads: YieldMap,
    }
    struct DispatchDrop(*mut YieldDispatch);
    impl Drop for DispatchDrop {
        fn drop(&mut self) {
            // SAFETY: `self.0` was allocated by `Box::into_raw` in `install_coroutine_inheritance`
            // and lives until overwritten or the `Lua` VM is dropped.
            unsafe { drop(Box::from_raw(self.0)) };
        }
    }
    unsafe extern "C-unwind" fn yield_cont(
        l: *mut mlua::ffi::lua_State,
        _status: c_int,
        _ctx: mlua::ffi::lua_KContext,
    ) -> c_int {
        unsafe { mlua::ffi::lua_gettop(l) }
    }
    unsafe extern "C-unwind" fn coroutine_yield_c(l: *mut mlua::ffi::lua_State) -> c_int {
        unsafe {
            let nargs = mlua::ffi::lua_gettop(l);
            mlua::ffi::lua_getfield(l, mlua::ffi::LUA_REGISTRYINDEX, YIELD_DISPATCH_KEY.as_ptr());
            let ptr = mlua::ffi::lua_touserdata(l, -1) as *const YieldDispatch;
            mlua::ffi::lua_pop(l, 1);
            let is_managed = !ptr.is_null() && (*ptr).exec_threads.borrow().contains_key(&(l as *const c_void));
            if is_managed {
                let mut ar: mlua::ffi::lua_Debug = std::mem::zeroed();
                // Async wrappers created after `install_coroutine_inheritance` capture this C
                // function. Calling `lua_yield` directly from level 0 keeps `=__mlua_async_poll`
                // at level 1 so `mlua::RawLua::is_async_wrapper_yield` still sees it.
                let from_mlua_poll = mlua::ffi::lua_getstack(l, 1, &mut ar) != 0
                    && mlua::ffi::lua_getinfo(l, c"S".as_ptr(), &mut ar) != 0
                    && !ar.source.is_null()
                    && CStr::from_ptr(ar.source) == c"=__mlua_async_poll";
                if !from_mlua_poll && mlua::ffi::lua_isyieldable(l) != 0 {
                    mlua::ffi::lua_checkstack(l, 2);
                    mlua::ffi::lua_getfield(l, mlua::ffi::LUA_REGISTRYINDEX, AWAIT_YIELD_KEY.as_ptr());
                    mlua::ffi::lua_insert(l, 1);
                    mlua::ffi::lua_callk(l, nargs, mlua::ffi::LUA_MULTRET, 0, Some(yield_cont));
                    return mlua::ffi::lua_gettop(l);
                }
            }
            mlua::ffi::lua_yield(l, nargs)
        }
    }

    let dispatch = Box::into_raw(Box::new(YieldDispatch { exec_threads: exec_threads.clone() }));
    if let Err(err) = lua.set_named_registry_value(
        YIELD_DISPATCH_KEY.to_str().expect("utf-8"),
        Value::LightUserData(mlua::LightUserData(dispatch.cast())),
    ) {
        // SAFETY: `dispatch` has not been registered in `app_data` yet.
        unsafe { drop(Box::from_raw(dispatch)) };
        return Err(err);
    }
    lua.set_app_data(DispatchDrop(dispatch));
    lua.set_named_registry_value(AWAIT_YIELD_KEY.to_str().expect("utf-8"), await_yield)?;

    let wrapped_f: Function = lua
        .load(
            r#"local running = ...
            local probe_mt = { __close = function(self, err)
              if err ~= nil then self[1](err) end
            end }
            return function(on_enter, on_close, f, ...)
              on_enter(running())
              local probe <close> = setmetatable({on_close}, probe_mt)
              return f(...)
            end"#,
        )
        .set_name("=mare.coroutine_body")
        .call(raw_running.clone())?;

    let exec_threads_drv = exec_threads.clone();
    let pending_wakers_drv = pending_wakers.clone();
    let new_driver = lua.create_function(move |lua, (f, co_th): (Function, Thread)| {
        lua.create_userdata(CoroutineDriver(RefCell::new(CoroutineDriverInner {
            f,
            wrapped_f: wrapped_f.clone(),
            future: None,
            yield_slot: Rc::new(RefCell::new(YieldSlot::Idle)),
            proxy_waker: ProxyWaker::new(),
            close_err: Rc::new(RefCell::new(None)),
            co_ptr: co_th.to_pointer(),
            exec_ptr: Rc::new(Cell::new(None)),
            exec_threads: exec_threads_drv.clone(),
            pending_wakers: pending_wakers_drv.clone(),
            was_yielded: false,
            body_err: None,
        })))
    })?;

    let make_trampoline: Function = lua
        .load(
            r#"local raw_yield, new_driver, running, PENDING = ...
            local pack, unpack = table.pack, table.unpack
            return function(f)
              return function(...)
                local driver = new_driver(f, running())
                local close_guard <close> = setmetatable({}, {__close = function()
                  local ok, err = driver:close()
                  if not ok then error(err, 0) end
                end})
                local r = pack(driver:step(...))
                while true do
                  local tag = r[1]
                  if tag == "ready" then
                    return unpack(r, 2, r.n)
                  elseif tag == "err" then
                    error(r[2], 0)
                  elseif tag == "pending" then
                    raw_yield(PENDING)
                    r = pack(driver:step())
                  else
                    r = pack(driver:step(raw_yield(unpack(r, 2, r.n))))
                  end
                end
              end
            end"#,
        )
        .set_name("=mare.coroutine_trampoline")
        .call((raw_yield, new_driver, raw_running, Value::LightUserData(Lua::poll_pending())))?;

    // SAFETY: `coroutine_yield_c` follows the Lua 5.5 C yield/callk protocol and reads only
    // registry values owned by this `Lua` instance.
    let yield_fn = unsafe { lua.create_c_function(coroutine_yield_c)? };
    co.set("yield", yield_fn)?;

    let transparent: Function = lua
        .load(
            r#"local resume, isyieldable, status, close, PENDING, await_pending = ...
            local pack, unpack = table.pack, table.unpack
            return function(co, ...)
              local pending = false
              local guard <close> = setmetatable({}, {__close = function()
                if pending then pcall(close, co) end
              end})
              local r = pack(resume(co, ...))
              while r[1] and r.n == 2 and r[2] == PENDING and isyieldable() and status(co) == "suspended" do
                pending = true
                await_pending(co)
                pending = false
                r = pack(resume(co))
              end
              return unpack(r, 1, r.n)
            end"#,
        )
        .set_name("=coroutine.resume")
        .call((
            raw_resume,
            co.get::<Function>("isyieldable")?,
            co.get::<Function>("status")?,
            co.get::<Function>("close")?,
            Value::LightUserData(Lua::poll_pending()),
            await_pending,
        ))?;
    co.set("resume", transparent.clone())?;
    let wrap_resume = transparent;
    co.set(
        "create",
        lua.create_function(move |lua, f: Function| {
            let trampoline: Function = make_trampoline.call(f)?;
            let th: Thread = raw_create.call(trampoline)?;
            crate::lua::attach_cov_hook(lua, &th);
            if let Some(c) = current(lua) {
                bind(lua, &th, Some(c))?;
            }
            Ok(th)
        })?,
    )?;
    // wrap(f) = a function resuming a (context-inheriting) coroutine, re-raising its errors
    let wrap = lua
        .load(
            r#"local create, resume, close, status = ...
            return function(f)
              local co = create(f)
              return function(...)
                local r = table.pack(resume(co, ...))
                if not r[1] then
                  if status(co) == "dead" then
                    local ok, err = close(co)
                    if not ok then r[2] = err end
                  end
                  error(r[2], 2)
                end
                return table.unpack(r, 2, r.n)
              end
            end"#,
        )
        .set_name("=coroutine.wrap")
        .call::<Function>((
            co.get::<Function>("create")?,
            wrap_resume,
            co.get::<Function>("close")?,
            co.get::<Function>("status")?,
        ))?;
    co.set("wrap", wrap)?;
    Ok(())
}

/// Field accessor used by bindings: `ctx.<k>` as a Lua value.
pub fn field(ctx: &Table, k: &str) -> Value {
    ctx.get::<Value>(k).unwrap_or(Value::Nil)
}

/// Call `f` from Rust on behalf of Lua code that ran in `ctx` (captured synchronously when
/// that code called into Rust): the callback stays inside the same tool execution context.
/// Use for callbacks a primitive invokes later (`mare.task.timeout/select`, output handlers…).
pub async fn call_in<R: mlua::FromLuaMulti>(
    lua: &Lua,
    ctx: Option<Table>,
    f: Function,
    args: impl mlua::IntoLuaMulti,
) -> mlua::Result<R> {
    match ctx {
        None => f.call_async::<R>(args).await,
        Some(c) => {
            let values = scoped_call(lua, Some(c), f, args.into_lua_multi(lua)?)?.await?;
            R::from_lua_multi(values, lua)
        }
    }
}

/// A transient execution may inspect values but cannot mutate host-owned capabilities.
pub fn is_ephemeral(lua: &Lua) -> bool {
    current(lua).is_some_and(
        |ctx| matches!(field(&ctx, "lifetime"), Value::String(s) if s.as_bytes().as_ref() == b"ephemeral-read-only"),
    )
}

pub fn check_mutation(lua: &Lua, operation: &str) -> mlua::Result<()> {
    if is_ephemeral(lua) {
        return Err(mlua::Error::external(crate::errors::Coded {
            code: "denied",
            message: format!("ephemeral-read-only: {operation} cannot change host state"),
        }));
    }
    Ok(())
}

/// Policy-enforcement hook registered by higher-level bindings (`mare-binding::exec_ctx::install`).
pub trait Enforcer: 'static {
    fn is_active(&self, lua: &Lua) -> mlua::Result<bool>;
    fn check_process_path(&self, lua: &Lua, op: &str, target: &str) -> mlua::Result<()>;
    fn refuse_deny_read(&self, lua: &Lua, what: &str) -> mlua::Result<()>;
}

/// Register the VM-local execution-context policy enforcer.
pub fn install_enforcer(lua: &Lua, enforcer: std::rc::Rc<dyn Enforcer>) {
    lua.set_app_data(enforcer);
}

fn enforcer(lua: &Lua) -> mlua::Result<std::rc::Rc<dyn Enforcer>> {
    lua.app_data_ref::<std::rc::Rc<dyn Enforcer>>()
        .map(|e| e.clone())
        .ok_or_else(|| crate::lua::rt_err("exec_ctx enforcer not installed"))
}

/// Whether the calling thread has an active sandbox policy.
pub fn is_active(lua: &Lua) -> mlua::Result<bool> {
    if current(lua).is_none() {
        return Ok(false);
    }
    enforcer(lua)?.is_active(lua)
}

/// Check a path opened by a native Lua file API, which uses the process cwd.
pub fn check_process_path(lua: &Lua, op: &str, target: &str) -> mlua::Result<()> {
    if target.contains('\0') {
        return Err(crate::lua::rt_err("file path contains NUL"));
    }
    if current(lua).is_none() {
        return Ok(());
    }
    enforcer(lua)?.check_process_path(lua, op, target)
}

/// Refuse a tool operation whose implicit file accesses cannot be checked per path yet.
pub fn refuse_deny_read(lua: &Lua, what: &str) -> mlua::Result<()> {
    if current(lua).is_none() {
        return Ok(());
    }
    enforcer(lua)?.refuse_deny_read(lua, what)
}
