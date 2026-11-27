use std::future::Future;
use std::task::{Context, Waker};

struct DropFlag(std::rc::Rc<std::cell::Cell<bool>>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

fn install_pending(lua: &mlua::Lua, name: &str, dropped: std::rc::Rc<std::cell::Cell<bool>>) {
    let f = lua
        .create_async_function(move |_, ()| {
            let flag = DropFlag(dropped.clone());
            async move {
                let _flag = flag;
                std::future::pending::<()>().await;
                Ok(())
            }
        })
        .unwrap();
    lua.globals().set(name, f).unwrap();
}

fn install_yield_once<T: mlua::IntoLuaMulti + Clone + Send + 'static>(lua: &mlua::Lua, value: T) {
    let f = lua
        .create_async_function(move |_, ()| {
            let value = value.clone();
            async move {
                tokio::task::yield_now().await;
                Ok(value)
            }
        })
        .unwrap();
    lua.globals().set("yield_once", f).unwrap();
}

fn install_spawn_fn(lua: &mlua::Lua) {
    let f = lua
        .create_function(|lua, f: mlua::Function| super::TaskHandle::spawn(lua, f, mlua::MultiValue::new()))
        .unwrap();
    lua.globals().set("spawn_fn", f).unwrap();
}

#[test]
fn cancelled_nested_call_closes_scopes_with_and_without_coverage() {
    for coverage in [false, true] {
        let mut ctx = 0_u8;
        let lua = mlua::Lua::new();
        if coverage {
            fn noop(_: *mut std::ffi::c_void, _: &str, _: usize) {}
            // The context outlives the Lua VM and the callback does not dereference it.
            unsafe { super::install_cov_hook(&lua, noop, (&mut ctx as *mut u8).cast()).unwrap() };
        }
        let globals = lua.globals();
        globals.set("CLOSED", false).unwrap();
        globals.set("STARTED", false).unwrap();
        globals
            .set("pending", lua.create_async_function(|_, ()| std::future::pending::<mlua::Result<()>>()).unwrap())
            .unwrap();
        let g: mlua::Function = lua
            .load(
                r#"return function()
                    local c <close> = setmetatable({}, {__close = function() CLOSED = true end})
                    STARTED = true
                    pending()
                end"#,
            )
            .eval()
            .unwrap();
        globals
            .set(
                "nested",
                lua.create_async_function(move |lua, ()| {
                    let g = g.clone();
                    async move { super::call_async::<()>(&lua, &g, ()).await }
                })
                .unwrap(),
            )
            .unwrap();
        let f: mlua::Function = lua.load("return function() return nested() end").eval().unwrap();
        let mut call = Box::pin(super::call_async::<()>(&lua, &f, ()));
        assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(globals.get::<bool>("STARTED").unwrap());
        assert!(!globals.get::<bool>("CLOSED").unwrap());
        drop(call);
        assert!(globals.get::<bool>("CLOSED").unwrap(), "scope not closed with coverage={coverage}");
    }
}

fn nested_pending(lua: &mlua::Lua) -> mlua::Function {
    let globals = lua.globals();
    globals.set("STARTED", false).unwrap();
    globals.set("CLOSED", false).unwrap();
    globals
        .set("pending", lua.create_async_function(|_, ()| std::future::pending::<mlua::Result<()>>()).unwrap())
        .unwrap();
    globals
        .set(
            "closed",
            lua.create_function(|lua, ()| {
                let ctx = crate::exec_ctx::current(lua);
                lua.globals().set("CLOSED_TOOL", ctx.and_then(|ctx| ctx.get::<String>("tool").ok()))?;
                lua.globals().set("CLOSED", true)
            })
            .unwrap(),
        )
        .unwrap();
    let inner: mlua::Function = lua
        .load(
            r#"return function()
                local c <close> = setmetatable({}, {__close = function() closed() end})
                STARTED = true
                pending()
            end"#,
        )
        .eval()
        .unwrap();
    globals
        .set(
            "nested",
            lua.create_async_function(move |lua, ()| {
                let inner = inner.clone();
                async move { super::call_async::<()>(&lua, &inner, ()).await }
            })
            .unwrap(),
        )
        .unwrap();
    lua.load("return function() nested() end").eval().unwrap()
}

fn context(lua: &mlua::Lua) -> mlua::Table {
    let ctx = lua.create_table().unwrap();
    ctx.set("tool", "captured").unwrap();
    ctx
}

fn assert_closed(lua: &mlua::Lua) {
    let globals = lua.globals();
    assert!(globals.get::<bool>("CLOSED").unwrap(), "nested scope was not closed on cancellation");
    assert_eq!(globals.get::<String>("CLOSED_TOOL").unwrap(), "captured");
}

#[test]
fn cancelled_run_in_closes_nested_scope() {
    let lua = mlua::Lua::new();
    let f = nested_pending(&lua);
    let mut call = Box::pin(crate::exec_ctx::run_in(&lua, context(&lua), f, mlua::MultiValue::new()));
    assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    assert!(lua.globals().get::<bool>("STARTED").unwrap());
    assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
    drop(call);
    assert_closed(&lua);
}

#[test]
fn cancelled_call_in_closes_nested_scope() {
    let lua = mlua::Lua::new();
    let f = nested_pending(&lua);
    let mut call = Box::pin(crate::exec_ctx::call_in::<()>(&lua, Some(context(&lua)), f, ()));
    assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    assert!(lua.globals().get::<bool>("STARTED").unwrap());
    assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
    drop(call);
    assert_closed(&lua);
}

#[test]
fn cancelled_user_coroutine_wrap_closes_nested_scope() {
    let lua = mlua::Lua::new();
    crate::exec_ctx::install_coroutine_inheritance(&lua).unwrap();
    let inner = nested_pending(&lua);
    let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
    install_pending(&lua, "pending", dropped.clone());
    lua.globals().set("INNER", inner).unwrap();
    let f: mlua::Function =
        lua.load("return function() return coroutine.wrap(function() return INNER() end)() end").eval().unwrap();
    let mut call = Box::pin(crate::exec_ctx::run_in(&lua, context(&lua), f, mlua::MultiValue::new()));
    assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    assert!(lua.globals().get::<bool>("STARTED").unwrap());
    assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
    assert!(!dropped.get());
    drop(call);
    assert!(dropped.get(), "rust future inside coroutine.wrap was not dropped on cancellation");
    assert_closed(&lua);
}

#[test]
fn cancelled_user_coroutine_resume_closes_nested_scope() {
    let lua = mlua::Lua::new();
    crate::exec_ctx::install_coroutine_inheritance(&lua).unwrap();
    let inner = nested_pending(&lua);
    lua.globals().set("INNER", inner).unwrap();
    let f: mlua::Function = lua
        .load("return function() local co = coroutine.create(function() return INNER() end); return coroutine.resume(co) end")
        .eval()
        .unwrap();
    let mut call = Box::pin(crate::exec_ctx::run_in(&lua, context(&lua), f, mlua::MultiValue::new()));
    assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    assert!(lua.globals().get::<bool>("STARTED").unwrap());
    assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
    drop(call);
    assert_closed(&lua);
}

#[tokio::test]
async fn user_coroutine_yield_and_nested_wrap_cancel() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            crate::exec_ctx::install_coroutine_inheritance(&lua).unwrap();
            install_yield_once(&lua, ());
            let manual: bool = lua
                .load(
                    r#"
                    local co = coroutine.create(function()
                      local value = coroutine.yield('pause')
                      yield_once()
                      return value
                    end)
                    local a, first = coroutine.resume(co)
                    local status = coroutine.status(co)
                    local b, last = coroutine.resume(co, 'resumed')
                    return a and first == 'pause' and status == 'suspended' and b and last == 'resumed' and coroutine.status(co) == 'dead'
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert!(manual, "manual coroutine yield/resume failed");

            let yield_close: String = lua
                .load(
                    r#"
                    local closed = false
                    local co = coroutine.create(function()
                      local g <close> = setmetatable({}, {__close = function()
                        closed = true
                        error('yield-closer-err')
                      end})
                      coroutine.yield('paused')
                    end)
                    local a, v = coroutine.resume(co)
                    assert(a and v == 'paused' and coroutine.status(co) == 'suspended')
                    local ok, err = coroutine.close(co)
                    assert(not ok and closed and coroutine.status(co) == 'dead')
                    return tostring(err)
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert!(yield_close.contains("yield-closer-err"), "unexpected yield close error: {yield_close}");

            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
            let rx = std::rc::Rc::new(tokio::sync::Mutex::new(rx));
            lua.globals()
                .set("send_ready", lua.create_function(move |_, v: bool| Ok(tx.send(v).is_ok())).unwrap())
                .unwrap();
            lua.globals()
                .set(
                    "recv_ready",
                    lua.create_async_function(move |_, ()| {
                        let rx = rx.clone();
                        async move { Ok(rx.lock().await.recv().await) }
                    })
                    .unwrap(),
                )
                .unwrap();
            let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
            install_pending(&lua, "park_forever", dropped.clone());
            install_spawn_fn(&lua);
            lua.globals()
                .set(
                    "run_ctx",
                    lua.create_async_function(|lua, f: mlua::Function| async move {
                        let ctx = context(&lua);
                        crate::exec_ctx::run_in(&lua, ctx, f, mlua::MultiValue::new()).await
                    })
                    .unwrap(),
                )
                .unwrap();
            let got: (bool, bool) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                lua.load(
                    r#"
                    NESTED_CLOSED = false
                    local task = spawn_fn(function()
                      run_ctx(function()
                        local co = coroutine.create(function()
                          coroutine.wrap(function()
                            local g <close> = setmetatable({}, {__close = function() NESTED_CLOSED = true end})
                            send_ready(true)
                            park_forever()
                          end)()
                        end)
                        assert(coroutine.resume(co))
                      end)
                    end)
                    assert(recv_ready() == true)
                    task:abort()
                    local ok = pcall(function() task:wait() end)
                    return not ok, NESTED_CLOSED
                    "#,
                )
                .eval_async(),
            )
            .await
            .expect("timed out cancelling nested wrap")
            .unwrap();
            assert_eq!(got, (true, true));
            assert!(dropped.get(), "pending future inside nested run_ctx coroutine.wrap was not dropped");

            // Also verify TaskHandle::spawn without exec_ctx cancels coroutine.wrap cleanly.
            dropped.set(false);
            let no_ctx: (bool, bool) = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                lua.load(
                    r#"
                    NOCTX_CLOSED = false
                    local task = spawn_fn(function()
                      coroutine.wrap(function()
                        local g <close> = setmetatable({}, {__close = function() NOCTX_CLOSED = true end})
                        send_ready(true)
                        park_forever()
                      end)()
                    end)
                    assert(recv_ready() == true)
                    task:abort()
                    local ok = pcall(function() task:wait() end)
                    return not ok, NOCTX_CLOSED
                    "#,
                )
                .eval_async(),
            )
            .await
            .expect("timed out cancelling no-ctx wrap")
            .unwrap();
            assert_eq!(no_ctx, (true, true));
            assert!(dropped.get(), "pending future inside no-ctx coroutine.wrap was not dropped");
        })
        .await;
}

#[tokio::test]
async fn concurrent_coroutine_close_wakes_awaiting_resume() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            crate::exec_ctx::install_coroutine_inheritance(&lua).unwrap();
            let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
            install_pending(&lua, "park_forever", dropped.clone());
            install_spawn_fn(&lua);
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let tx = std::rc::Rc::new(std::cell::RefCell::new(Some(tx)));
            lua.globals()
                .set(
                    "signal_started",
                    lua.create_function(move |_, ()| {
                        if let Some(sender) = tx.borrow_mut().take() {
                            let _ = sender.send(());
                        }
                        Ok(())
                    })
                    .unwrap(),
                )
                .unwrap();
            let rx = std::rc::Rc::new(tokio::sync::Mutex::new(Some(rx)));
            lua.globals()
                .set(
                    "wait_started",
                    lua.create_async_function(move |_, ()| {
                        let rx = rx.clone();
                        async move {
                            if let Some(r) = rx.lock().await.take() {
                                let _ = r.await;
                            }
                            Ok(())
                        }
                    })
                    .unwrap(),
                )
                .unwrap();
            let res: (bool, bool, bool, String) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                lua.load(
                    r#"
                    CLOSED = false
                    local co = coroutine.create(function()
                      local g <close> = setmetatable({}, {__close = function() CLOSED = true end})
                      signal_started()
                      park_forever()
                    end)
                    local resumer = spawn_fn(function()
                      return coroutine.resume(co)
                    end)
                    wait_started()
                    local close_ok = coroutine.close(co)
                    local ok, err = resumer:wait()
                    return close_ok, CLOSED, ok, tostring(err)
                    "#,
                )
                .eval_async(),
            )
            .await
            .expect("concurrent coroutine.close left awaiting resume hung")
            .unwrap();
            assert!(res.0, "coroutine.close should succeed");
            assert!(res.1, "<close> guard should run on concurrent close");
            assert!(!res.2, "awaiting resume should return false after coroutine.close");
            assert!(dropped.get(), "pending future should be dropped by concurrent coroutine.close");
        })
        .await;
}

#[tokio::test]
async fn unyieldable_yield_inside_managed_coroutine_does_not_poison_next_async_call() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            crate::exec_ctx::install_coroutine_inheritance(&lua).unwrap();
            install_yield_once(&lua, "after_async");
            let out: (bool, String, String) = lua
                .load(
                    r#"
                    return coroutine.wrap(function()
                      local ok, err = pcall(table.sort, {2, 1}, function()
                        coroutine.yield('nope')
                      end)
                      local async_res = yield_once()
                      return ok, tostring(err), async_res
                    end)()
                    "#,
                )
                .eval_async()
                .await
                .unwrap();
            assert!(!out.0, "yield inside table.sort C comparator must error");
            assert!(out.1.contains("C-call boundary"), "unexpected error: {}", out.1);
            assert_eq!(out.2, "after_async");
        })
        .await;
}

#[test]
fn suspended_user_coroutine_does_not_leak_lua_vm_on_drop() {
    let vm_dropped = std::rc::Rc::new(std::cell::Cell::new(false));
    struct VmDrop(std::rc::Rc<std::cell::Cell<bool>>);
    impl Drop for VmDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    let weak = {
        let lua = mlua::Lua::new();
        lua.set_app_data(VmDrop(vm_dropped.clone()));
        crate::exec_ctx::install_coroutine_inheritance(&lua).unwrap();
        lua.load(
            r#"
            CO_YIELD = coroutine.create(function() coroutine.yield(1) end)
            assert(coroutine.resume(CO_YIELD))
            CO_ERR = coroutine.create(function() error("unclosed") end)
            assert(not coroutine.resume(CO_ERR))
            "#,
        )
        .exec()
        .unwrap();
        lua.weak()
    };
    assert!(weak.try_upgrade().is_none(), "Lua VM leaked via CoroutineDriver -> ScopedFuture");
    assert!(vm_dropped.get(), "Lua VM app_data was not dropped");
}

#[tokio::test]
async fn user_coroutine_sync_yield_loop_does_not_starve_localset() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            crate::exec_ctx::install_coroutine_inheritance(&lua).unwrap();
            install_yield_once(&lua, ());
            lua.globals()
                .set(
                    "sync_wake_yield",
                    lua.create_async_function(|_, ()| {
                        let mut polled = false;
                        std::future::poll_fn(move |cx| {
                            if polled {
                                std::task::Poll::Ready(Ok(()))
                            } else {
                                polled = true;
                                cx.waker().wake_by_ref();
                                std::task::Poll::Pending
                            }
                        })
                    })
                    .unwrap(),
                )
                .unwrap();
            install_spawn_fn(&lua);
            let spins: i64 = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                lua.load(
                    r#"
                    READY = false
                    local setter = spawn_fn(function()
                      yield_once()
                      READY = true
                    end)
                    local waiter = spawn_fn(function()
                      return coroutine.wrap(function()
                        local n = 0
                        while not READY do
                          sync_wake_yield()
                          yield_once()
                          n = n + 1
                          if n > 10000 then error("sync yield loop starved LocalSet") end
                        end
                        return n
                      end)()
                    end)
                    setter:wait()
                    return waiter:wait()
                    "#,
                )
                .eval_async(),
            )
            .await
            .expect("sync yield loop starved LocalSet")
            .unwrap();
            assert!(spins >= 1, "expected at least one yield iteration, got {spins}");
        })
        .await;
}

#[tokio::test]
async fn cancelled_inherited_spawn_closes_nested_scope() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            let f = nested_pending(&lua);
            let launch = lua
                .create_function(|lua, f: mlua::Function| super::TaskHandle::spawn(lua, f, mlua::MultiValue::new()))
                .unwrap();
            let owner = lua.create_thread(launch).unwrap();
            crate::exec_ctx::bind(&lua, &owner, Some(context(&lua))).unwrap();
            let task: mlua::AnyUserData = owner.resume(f).unwrap();
            tokio::task::yield_now().await;
            assert!(lua.globals().get::<bool>("STARTED").unwrap());
            assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
            let abort = task.borrow::<super::TaskHandle>().unwrap().abort.clone();
            abort.abort();
            while !abort.is_finished() {
                tokio::task::yield_now().await;
            }
            assert_closed(&lua);
        })
        .await;
}

#[tokio::test]
async fn scoped_call_fast_path_and_explicit_none_clears_active_exec_ctx() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            let check = lua
                .create_function(|lua, ()| Ok(crate::exec_ctx::current(lua).and_then(|t| t.get::<String>("tool").ok())))
                .unwrap();

            // 1. Before any `bind(_, Some(_))`: fast path runs cleanly and reports `None`.
            let before = crate::exec_ctx::scoped_call(&lua, None, check.clone(), mlua::MultiValue::new())
                .unwrap()
                .await
                .unwrap();
            assert_eq!(before.into_iter().next(), Some(mlua::Value::Nil));

            // 2. Inside a bound tool context, `scoped_call(lua, None, ...)` MUST take the slow
            //    path and explicitly shed the caller's tool context rather than inheriting it.
            let check_inner = check.clone();
            let call_none_from_bound = lua
                .create_async_function(move |lua, ()| {
                    let check_inner = check_inner.clone();
                    async move {
                        let outer = crate::exec_ctx::current(&lua).and_then(|t| t.get::<String>("tool").ok());
                        let inner_vals =
                            crate::exec_ctx::scoped_call(&lua, None, check_inner, mlua::MultiValue::new())?.await?;
                        let inner = match inner_vals.into_iter().next() {
                            Some(mlua::Value::String(s)) => Some(s.to_str()?.to_string()),
                            _ => None,
                        };
                        Ok((outer, inner))
                    }
                })
                .unwrap();
            let res = crate::exec_ctx::run_in(&lua, context(&lua), call_none_from_bound, mlua::MultiValue::new())
                .await
                .unwrap();
            let mut it = res.into_iter();
            let outer = match it.next() {
                Some(mlua::Value::String(s)) => Some(s.to_str().unwrap().to_string()),
                _ => None,
            };
            let inner = it.next();
            assert_eq!(outer.as_deref(), Some("captured"));
            assert_eq!(inner, Some(mlua::Value::Nil));

            // 3. After `ExecCtxFlag` has been set by the tool call above, an untagged caller
            //    taking the `scoped_call(None)` fast path still runs cleanly and sees `None`.
            let after =
                crate::exec_ctx::scoped_call(&lua, None, check, mlua::MultiValue::new()).unwrap().await.unwrap();
            assert_eq!(after.into_iter().next(), Some(mlua::Value::Nil));
        })
        .await;
}

#[test]
fn exec_ctx_bind_does_not_panic_while_other_app_data_is_borrowed() {
    struct OtherAppData(u32);

    // 1. Eagerly initialized VM (`exec_ctx::init`): `bind` flips the atomic flag without `set_app_data`.
    let lua_init = mlua::Lua::new();
    crate::exec_ctx::init(&lua_init);
    lua_init.set_app_data(OtherAppData(7));
    {
        let borrowed = lua_init.app_data_ref::<OtherAppData>().unwrap();
        assert_eq!(borrowed.0, 7);
        let th = lua_init.current_thread();
        crate::exec_ctx::bind(&lua_init, &th, Some(context(&lua_init))).unwrap();
        assert_eq!(
            crate::exec_ctx::current(&lua_init).and_then(|t| t.get::<String>("tool").ok()).as_deref(),
            Some("captured")
        );
        crate::exec_ctx::bind(&lua_init, &th, None).unwrap();
        assert!(crate::exec_ctx::current(&lua_init).is_none());
    }

    // 2. Uninitialized bare `Lua::new()`: holding another `AppDataRef` must still not panic.
    let lua_bare = mlua::Lua::new();
    lua_bare.set_app_data(OtherAppData(9));
    {
        let borrowed = lua_bare.app_data_ref::<OtherAppData>().unwrap();
        assert_eq!(borrowed.0, 9);
        let th = lua_bare.current_thread();
        crate::exec_ctx::bind(&lua_bare, &th, Some(context(&lua_bare))).unwrap();
        assert_eq!(
            crate::exec_ctx::current(&lua_bare).and_then(|t| t.get::<String>("tool").ok()).as_deref(),
            Some("captured")
        );
        crate::exec_ctx::bind(&lua_bare, &th, None).unwrap();
        assert!(crate::exec_ctx::current(&lua_bare).is_none());
    }
}

#[tokio::test]
async fn coverage_hook_forces_slow_path_in_untagged_scoped_call() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn on_line(ctx: *mut std::ffi::c_void, source: &str, _line: usize) {
        if source.contains("cov_slow_path_probe") {
            let counter = unsafe { &*(ctx as *const AtomicUsize) };
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            let hits = Box::new(AtomicUsize::new(0));
            let ptr = (&*hits as *const AtomicUsize).cast_mut().cast();
            // SAFETY: `hits` outlives `lua` (dropped after `lua` below).
            unsafe { crate::lua::install_cov_hook(&lua, on_line, ptr).unwrap() };
            assert!(crate::lua::has_cov_hook(&lua));

            let body: mlua::Function = lua
                .load("local x = 1\nlocal y = x + 2\nreturn y")
                .set_name("@cov_slow_path_probe.lua")
                .into_function()
                .unwrap();
            let res = crate::exec_ctx::scoped_call(&lua, None, body, mlua::MultiValue::new()).unwrap().await.unwrap();
            assert_eq!(res.into_iter().next(), Some(mlua::Value::Integer(3)));
            drop(lua);
            assert!(
                hits.load(Ordering::Relaxed) >= 2,
                "coverage hook must observe lines inside untagged scoped_call, got {}",
                hits.load(Ordering::Relaxed)
            );
        })
        .await;
}

#[tokio::test]
async fn untagged_scoped_call_spawned_from_listener_inside_tool_survives_listener_owner_gc() {
    use std::cell::RefCell;
    use std::rc::Rc;

    tokio::task::LocalSet::new()
        .run_until(async {
            let lua = mlua::Lua::new();
            crate::exec_ctx::init(&lua);

            let (tx_ready, rx_ready) = tokio::sync::oneshot::channel::<()>();
            let (tx_go, rx_go) = tokio::sync::oneshot::channel::<()>();
            let (tx_done, rx_done) = tokio::sync::oneshot::channel::<(usize, Option<String>)>();

            let rx_go_slot = Rc::new(RefCell::new(Some(rx_go)));
            let tx_ready_slot = Rc::new(RefCell::new(Some(tx_ready)));
            let tx_done_slot = Rc::new(RefCell::new(Some(tx_done)));
            let listener_owner_ptr = Rc::new(std::cell::Cell::new(0usize));

            // Weak-value table to observe that the listener's transient owner thread is collected.
            let weak_refs: mlua::Table =
                lua.load("local t = setmetatable({}, { __mode = 'v' }); return t").eval().unwrap();
            let weak_refs_for_listener = weak_refs.clone();
            let listener_owner_ptr_clone = listener_owner_ptr.clone();

            let delayed_fn = lua
                .create_async_function(move |lua, ()| {
                    let rx_go_slot = rx_go_slot.clone();
                    let tx_ready_slot = tx_ready_slot.clone();
                    let tx_done_slot = tx_done_slot.clone();
                    async move {
                        if let Some(tx) = tx_ready_slot.borrow_mut().take() {
                            let _ = tx.send(());
                        }
                        let rx = rx_go_slot.borrow_mut().take().expect("rx_go");
                        let _ = rx.await;
                        let ptr = lua.current_thread().to_pointer() as usize;
                        let tag = crate::exec_ctx::current(&lua).and_then(|t| t.get::<String>("tool").ok());
                        if let Some(tx) = tx_done_slot.borrow_mut().take() {
                            let _ = tx.send((ptr, tag));
                        }
                        Ok(())
                    }
                })
                .unwrap();

            // Simulate a plugin listener (captured with exec = None) invoked from inside a tool call:
            // it spawns a detached delayed task (like `Scope::spawn`) and returns immediately.
            let listener_fn = lua
                .create_function(move |lua, ()| {
                    let owner = lua.current_thread();
                    listener_owner_ptr_clone.set(owner.to_pointer() as usize);
                    weak_refs_for_listener.set(1, owner)?;
                    let detached =
                        crate::exec_ctx::scoped_call(lua, None, delayed_fn.clone(), mlua::MultiValue::new())?;
                    tokio::task::spawn_local(async move {
                        let _ = detached.await;
                    });
                    Ok(())
                })
                .unwrap();

            // Tool body runs in a tagged context, invokes the untagged listener, then completes.
            let tool_fn = lua
                .create_async_function(move |lua, ()| {
                    let listener_fn = listener_fn.clone();
                    async move {
                        assert!(crate::exec_ctx::current(&lua).is_some(), "tool body must have exec_ctx");
                        crate::exec_ctx::scoped_call(&lua, None, listener_fn, mlua::MultiValue::new())?.await?;
                        Ok(())
                    }
                })
                .unwrap();

            crate::exec_ctx::scoped_call(&lua, Some(context(&lua)), tool_fn, mlua::MultiValue::new())
                .unwrap()
                .await
                .unwrap();

            // Wait until the detached task is suspended, then force GC to collect the listener's
            // transient owner thread and confirm it was actually collected.
            rx_ready.await.unwrap();
            lua.gc_collect().unwrap();
            lua.gc_collect().unwrap();
            assert_eq!(
                weak_refs.get::<mlua::Value>(1).unwrap(),
                mlua::Value::Nil,
                "listener's transient owner thread must be garbage-collected before detached task resumes"
            );

            // Allocate fresh tagged threads to encourage allocator reuse of the freed lua_State slot.
            let mut decoy_threads = Vec::new();
            for _ in 0..32 {
                let f = lua.create_function(|_, ()| Ok(())).unwrap();
                let th = lua.create_thread(f).unwrap();
                crate::exec_ctx::bind(&lua, &th, Some(context(&lua))).unwrap();
                decoy_threads.push(th);
            }

            tx_go.send(()).unwrap();
            let (detached_owner_ptr, tag) = rx_done.await.unwrap();
            drop(decoy_threads);

            assert_ne!(
                detached_owner_ptr,
                listener_owner_ptr.get(),
                "detached task must hold its own live owner thread, not reference the collected listener owner"
            );
            assert_eq!(tag, None, "detached untagged task must not observe a dangling or reused tool exec_ctx");
        })
        .await;
}

#[cfg(debug_assertions)]
#[test]
fn spawn_aborts_its_task_when_the_handle_allocation_fails() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    tokio::task::LocalSet::new().block_on(&rt, async {
        let lua = mlua::Lua::new();
        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let flag = ran.clone();
        let f = lua
            .create_async_function(move |_, ()| {
                let flag = flag.clone();
                async move {
                    flag.set(true);
                    Ok(())
                }
            })
            .unwrap();
        super::fail_next_task_wrap(&lua);
        let err = super::TaskHandle::spawn(&lua, f, mlua::MultiValue::new()).unwrap_err();
        assert!(err.to_string().contains("injected task handle allocation failure"), "{err}");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!ran.get(), "the spawned task kept running without an owner");
    });
}

#[test]
fn lua_tables_encode_with_sorted_keys_except_ordered_properties() {
    let lua = mlua::Lua::new();
    let v: mlua::Value = lua
        .load(r#"return { zeta = 1, alpha = { y = 2, b = 3 }, schema = { ["x-mare-order"] = { "z", "a" }, properties = { a = 1, z = 2 } } }"#)
        .eval()
        .unwrap();
    if let mlua::Value::Table(t) = &v {
        super::mark_tool_schema(&lua, &t.get::<mlua::Value>("schema").unwrap()).unwrap();
    }
    let j = super::lua_to_json(&lua, v).unwrap();
    assert_eq!(
        serde_json::to_string(&j).unwrap(),
        r#"{"alpha":{"b":3,"y":2},"schema":{"properties":{"z":2,"a":1}},"zeta":1}"#
    );
}
#[test]
fn strict_json_rejects_loss_and_preserves_user_order_keys() {
    use mlua::Value;
    let lua = mlua::Lua::new();
    for source in [
        "return { value = function() end }",
        "return { [1] = 1, [3] = 3 }",
        "return { value = 0/0 }",
        "local t = {}; t.self = t; return t",
        "return setmetatable({}, {})",
        "return { [true] = 1 }",
    ] {
        let v: Value = lua.load(source).eval().unwrap();
        assert!(super::strict_lua_to_json(&lua, v).is_err(), "{source}");
    }
    let invalid: Value = lua.load("return { properties = { x = { const = 0/0 } } }").eval().unwrap();
    let error = super::strict_lua_to_json(&lua, invalid).unwrap_err().to_string();
    assert!(error.contains("$/properties/x/const"), "{error}");
    let source = serde_json::json!({"x-mare-order":["a"], "properties":{"x-mare-order": 1}, "null":null, "array":[], "object":{}});
    let value = super::json_to_lua_lossless(&lua, &source).unwrap();
    assert_eq!(super::strict_lua_to_json(&lua, value.clone()).unwrap(), source);
    assert_eq!(super::lua_to_json(&lua, value).unwrap(), source);
    let schema: Value = super::json_to_lua(
        &lua,
        &serde_json::json!({"type":"object", "x-mare-order":["z","a"],
        "properties":{"a":{"const":{"x-mare-order":["kept"],"properties":{}}},"z":{"type":"string"}}}),
    )
    .unwrap();
    super::mark_tool_schema(&lua, &schema).unwrap();
    let encoded = super::lua_to_json(&lua, schema).unwrap();
    assert_eq!(encoded["properties"]["a"]["const"]["x-mare-order"], serde_json::json!(["kept"]));
    assert!(encoded.get("x-mare-order").is_none());
    assert_eq!(encoded["properties"].as_object().unwrap().keys().next().unwrap(), "z");
    let nested = super::json_to_lua(
        &lua,
        &serde_json::json!({"type":"object", "properties":{
            "nested":{"type":"object", "x-mare-order":["z","a"], "properties":{"a":{},"z":{}}}
        }}),
    )
    .unwrap();
    super::mark_tool_schema(&lua, &nested).unwrap();
    let encoded = super::lua_to_json(&lua, nested).unwrap();
    assert_eq!(encoded["properties"]["nested"]["properties"].as_object().unwrap().keys().next().unwrap(), "z");
    assert!(encoded["properties"]["nested"].get("x-mare-order").is_none());
}

#[test]
fn lossless_json_rejects_integers_that_lua_cannot_represent() {
    let lua = mlua::Lua::new();
    let error = super::json_to_lua_lossless(&lua, &serde_json::json!({"properties":{"n":{"const":u64::MAX}}}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("$/properties/n/const"), "{error}");
}

#[test]
fn lossless_json_to_lua_rejects_over_budget_before_allocating() {
    let lua = mlua::Lua::new();
    let big = serde_json::Value::Array((0..200_000).map(serde_json::Value::from).collect());
    lua.gc_collect().unwrap();
    lua.gc_stop();
    let before = lua.used_memory();
    let error = super::json_to_lua_lossless_bounded(&lua, &big, 1_000).unwrap_err().to_string();
    let grown = lua.used_memory().saturating_sub(before);
    assert!(error.contains("exceeds resource limit"), "{error}");
    // A 200k-element Lua array would take megabytes; the budget check allocates nothing.
    assert!(grown < 64 * 1024, "allocated {grown} bytes before rejecting");
    let deep = (0..70).fold(serde_json::json!(1), |v, _| serde_json::json!([v]));
    assert!(super::json_to_lua_lossless_bounded(&lua, &deep, 1_000).is_err());
    let fits = serde_json::json!({"a": [1, 2, {"b": null}]});
    assert!(super::json_to_lua_lossless_bounded(&lua, &fits, 6).is_ok());
    assert!(super::json_to_lua_lossless_bounded(&lua, &fits, 5).is_err());
}

#[test]
fn lua_to_json_converts_non_utf8_strings_lossily() {
    let lua = mlua::Lua::new();
    let bad = lua.create_string(b"prefix\xe4\xb8").unwrap();
    let j = super::lua_to_json(&lua, mlua::Value::String(bad.clone())).unwrap();
    assert_eq!(j, serde_json::json!("prefix\u{FFFD}"));

    let tbl = lua.create_table().unwrap();
    tbl.set("text", bad.clone()).unwrap();
    let arr = lua.create_table().unwrap();
    arr.set(1, bad.clone()).unwrap();
    tbl.set("arr", arr).unwrap();
    let nested = super::lua_to_json(&lua, mlua::Value::Table(tbl)).unwrap();
    assert_eq!(nested["text"], serde_json::json!("prefix\u{FFFD}"));
    assert_eq!(nested["arr"], serde_json::json!(["prefix\u{FFFD}"]));

    // Strict conversion continues to reject non-UTF-8 strings.
    assert!(super::strict_lua_to_json(&lua, mlua::Value::String(bad)).is_err());
}

#[test]
fn lua_to_json_rejects_non_utf8_object_keys() {
    let lua = mlua::Lua::new();
    let key_a = lua.create_string(b"prefix\xe4\xb8").unwrap();
    let key_b = lua.create_string(b"prefix\xe4\xb9").unwrap();
    let tbl = lua.create_table().unwrap();
    tbl.set(key_a.clone(), "first").unwrap();
    tbl.set(key_b, "second").unwrap();
    assert!(
        super::lua_to_json(&lua, mlua::Value::Table(tbl)).is_err(),
        "distinct non-UTF-8 object keys must be rejected instead of colliding via U+FFFD"
    );

    let outer = lua.create_table().unwrap();
    let inner = lua.create_table().unwrap();
    inner.set(key_a, "val").unwrap();
    outer.set("nested", inner).unwrap();
    assert!(
        super::lua_to_json(&lua, mlua::Value::Table(outer)).is_err(),
        "nested non-UTF-8 object keys must also be rejected"
    );
}

#[test]
fn lua_to_json_lossy_reports_non_utf8_replacements() {
    let lua = mlua::Lua::new();
    let clean = lua.create_string("hello").unwrap();
    let (j, lossy) = super::lua_to_json_lossy(&lua, mlua::Value::String(clean.clone())).unwrap();
    assert_eq!(j, serde_json::json!("hello"));
    assert!(!lossy);

    let bad = lua.create_string(b"ab\xe4\xb8").unwrap();
    let tbl = lua.create_table().unwrap();
    tbl.set("raw", bad.clone()).unwrap();
    let (j, lossy) = super::lua_to_json_lossy(&lua, mlua::Value::Table(tbl)).unwrap();
    assert_eq!(j["raw"], serde_json::json!("ab\u{FFFD}"));
    assert!(lossy);

    // Counter must not leak across calls, including after an error.
    let bad_key_tbl = lua.create_table().unwrap();
    bad_key_tbl.set(bad, "val").unwrap();
    assert!(super::lua_to_json_lossy(&lua, mlua::Value::Table(bad_key_tbl)).is_err());

    let (j, lossy) = super::lua_to_json_lossy(&lua, mlua::Value::String(clean)).unwrap();
    assert_eq!(j, serde_json::json!("hello"));
    assert!(!lossy);
}

#[test]
fn lua_to_json_lossy_bounded_rejects_over_budget_before_allocating() {
    use mlua::LuaSerdeExt;
    let lua = mlua::Lua::new();
    let big = lua.create_table().unwrap();
    big.set_metatable(Some(lua.array_metatable())).unwrap();
    for i in 1..=200_000 {
        big.raw_set(i, i).unwrap();
    }
    lua.gc_collect().unwrap();
    lua.gc_stop();
    let before = lua.used_memory();
    let error = super::lua_to_json_lossy_bounded(&lua, mlua::Value::Table(big), 1_000).unwrap_err().to_string();
    let grown = lua.used_memory().saturating_sub(before);
    assert!(error.contains("exceeds resource limit"), "{error}");
    assert!(grown < 64 * 1024, "allocated {grown} bytes before rejecting");

    let fits = lua.create_table().unwrap();
    let arr = lua.create_table().unwrap();
    arr.set_metatable(Some(lua.array_metatable())).unwrap();
    arr.raw_set(1, 1).unwrap();
    arr.raw_set(2, lua.create_string(b"ab\xe4\xb8").unwrap()).unwrap();
    arr.raw_set(3, f64::NAN).unwrap();
    fits.set("a", arr).unwrap();
    let (j, lossy) = super::lua_to_json_lossy_bounded(&lua, mlua::Value::Table(fits.clone()), 5).unwrap();
    assert!(lossy);
    assert_eq!(j, serde_json::json!({ "a": [1, "ab\u{FFFD}", null] }));
    assert!(super::lua_to_json_lossy_bounded(&lua, mlua::Value::Table(fits), 4).is_err());

    let bad_key = lua.create_table().unwrap();
    bad_key.set(lua.create_string(b"k\xe4\xb8").unwrap(), "v").unwrap();
    let key_err = super::lua_to_json_lossy_bounded(&lua, mlua::Value::Table(bad_key), 10).unwrap_err().to_string();
    assert!(key_err.contains("UTF-8"), "{key_err}");
}

#[test]
fn shape_metadata_copy_rejects_cyclic_tables_without_overflow() {
    let lua = mlua::Lua::new();
    let (src, dst): (mlua::Table, mlua::Table) = lua
        .load(
            r#"
            local a = {}
            a.self = a
            local b = {}
            b.self = b
            return a, b
            "#,
        )
        .eval()
        .unwrap();
    let err = super::shape::copy_json_metadata(&lua, &mlua::Value::Table(src), &mlua::Value::Table(dst))
        .unwrap_err()
        .to_string();
    assert!(err.contains("cyclic"), "{err}");
}
