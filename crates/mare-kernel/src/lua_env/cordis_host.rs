//! Kernel-internal host primitives for the vendored cordis-lua: the
//! execution-context capture behind its `host.capture()` and the root-context
//! spawn for the library's own tasks. They are handed to `lua/mare_kernel/boot.lua` as an
//! argument only — never a global, never a `require`-able module — so plugins cannot reach the
//! host or start a second tree.
//!
//! A scope is the execution context of the code that was running when it was captured: the tool
//! execution context ([`exec_ctx::current`]) and the capturing plugin owner. `scope:run(fn, ...)`
//! calls `fn` under that context and waits for it; `scope:spawn(fn)` starts `fn` as a task under
//! it. Unlike `mare.task.spawn` (which inherits the *spawner's* context) a scope uses the captured
//! one, so a callback runs with the rights of whoever registered it, not of whoever triggers it.
//! The root scope has neither: the host's own context.
//!
//! The time each run or task keeps the loop thread busy is accounted like an event listener's
//! over the budget it is logged and shows up in
//! `/profile` under the plugin `cordis`.

use mare_shared::exec_ctx;
use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods, Value};

use crate::dispatch::timing::timed;
use crate::{Kernel, KernelInner};

/// Where the slow-callback accounting files cordis host work (no fiber owner yet).
const TIMING_PLUGIN: &str = "cordis";

#[derive(Clone)]
struct Scope {
    exec: Option<Table>,
    /// the plugin owner the captured code ran for (the emitter of an event)
    owner: Option<u64>,
    kernel: std::sync::Weak<KernelInner>,
}

impl Scope {
    fn capture(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> Self {
        Self { exec: exec_ctx::current(lua), owner: crate::plugins::prims::current_owner(), kernel }
    }

    fn root(kernel: std::sync::Weak<KernelInner>) -> Self {
        Self { exec: None, owner: None, kernel }
    }

    /// The host's own context: no tool context, no plugin owner.
    fn is_root(&self) -> bool {
        self.exec.is_none() && self.owner.is_none()
    }

    /// Is the code running now in this scope's context? Then `run` may call directly.
    fn is_current(&self, lua: &Lua) -> bool {
        let exec = match (&self.exec, exec_ctx::current(lua)) {
            (None, None) => true,
            (Some(a), Some(b)) => a.to_pointer() == b.to_pointer(),
            _ => false,
        };
        exec && self.owner == crate::plugins::prims::current_owner()
    }

    fn account(&self, event: &str, busy: std::time::Duration) {
        if let Some(k) = Kernel::from_weak(&self.kernel) {
            k.record_slow_callback(event, TIMING_PLUGIN, busy);
        }
    }

    async fn run(self, lua: Lua, f: Function, args: MultiValue) -> mlua::Result<MultiValue> {
        let scope_id = Kernel::from_weak(&self.kernel).map_or(0, |k| k.resource_scope());
        let (owner, exec) = (self.owner, self.exec.clone());
        let (r, busy) = timed(async move {
            let call = exec_ctx::scoped_call(&lua, exec, f, args)?;
            mare_shared::image_resource::in_scope(scope_id, crate::plugins::prims::in_owner(owner, call)).await
        })
        .await;
        self.account("cordis.run", busy);
        r
    }

    fn spawn(&self, lua: &Lua, f: Function, event: &'static str) -> mlua::Result<TaskHandle> {
        let scope_id = Kernel::from_weak(&self.kernel).map_or(0, |k| k.resource_scope());
        let call = exec_ctx::scoped_call(lua, self.exec.clone(), f, MultiValue::new())?;
        let scope = self.clone();
        // Inherit the spawning transaction; inside it restore the registrant's exec context and owner.
        let task = mare_shared::txn_scope::spawn_local(async move {
            let (r, busy) = timed(mare_shared::image_resource::in_scope(
                scope_id,
                crate::plugins::prims::in_owner(scope.owner, call),
            ))
            .await;
            scope.account(event, busy);
            if let Err(e) = r {
                // the library wraps user callbacks itself; an error here is the library's own
                tracing::warn!("cordis host task failed: {e}");
            }
        });
        Ok(TaskHandle(task.abort_handle()))
    }
}

/// The handle of a scope task. `cancel` aborts it where it is suspended
/// (a fiber disposed while its component is still applying, e.g. a cancelled reload).
pub(crate) struct TaskHandle(tokio::task::AbortHandle);

impl UserData for TaskHandle {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("cancel", |_, this, ()| {
            if tokio::task::try_id() != Some(this.0.id()) {
                this.0.abort();
            }
            Ok(())
        });
    }
}

impl UserData for Scope {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("is_current", |lua, this, ()| Ok(this.is_current(lua)));
        m.add_method("is_root", |_, this, ()| Ok(this.is_root()));
        m.add_async_method("run", |lua, this, (f, args): (Function, MultiValue)| {
            let scope = this.clone();
            async move { scope.run(lua, f, args).await }
        });
        m.add_method("spawn", |lua, this, f: Function| this.spawn(lua, f, "cordis.spawn"));
    }
}

/// `{ capture = fn() -> scope, spawn_root = fn(f) }` for boot.lua.
pub(crate) fn prims(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set(
        "coded_error",
        lua.create_function(|lua, (code, message): (String, String)| {
            mare_shared::lua_error::error_value(lua, &code, &message, mlua::Value::Nil)
        })?,
    )?;
    let k = kernel.clone();
    // startup opt: a capture in the host's own context (no grant, no tool context, no owner: the
    // fibers, effects and futures boot builds) returns one shared root scope instead of a new one
    // each time. A scope is immutable and every root capture is equal, so sharing it changes
    // nothing; any other capture stays its own (never shared across grants or owners).
    let root_scope: std::cell::RefCell<Option<mlua::AnyUserData>> = std::cell::RefCell::new(None);
    t.set(
        "capture",
        lua.create_function(move |lua, ()| {
            let scope = Scope::capture(lua, k.clone());
            if !scope.is_root() {
                return Ok(mlua::Value::UserData(lua.create_userdata(scope)?));
            }
            if let Some(ud) = root_scope.borrow().as_ref() {
                return Ok(mlua::Value::UserData(ud.clone()));
            }
            let ud = lua.create_userdata(scope)?;
            *root_scope.borrow_mut() = Some(ud.clone());
            Ok(mlua::Value::UserData(ud))
        })?,
    )?;
    // The row list and the owner / events / definition / reload primitives
    t.set("rows", crate::plugins::rows::rows_prim(lua, kernel.clone())?)?;
    t.set("refused", crate::plugins::rows::refused_prim(lua, kernel.clone())?)?;
    t.set("profile_record", crate::plugins::rows::profile_prim(lua)?)?;
    t.set(
        "after_builtins",
        lua.create_function(|lua, ()| {
            if let Ok(cb) = lua.named_registry_value::<mlua::Function>("mare.after_builtins_started") {
                let _ = cb.call::<()>(());
            }
            Ok(())
        })?,
    )?;
    if Kernel::from_weak(&kernel).is_some_and(|k| k.measure_apply_enabled()) {
        t.set("measure_apply", crate::plugins::rows::measure_apply_prim(lua, kernel.clone())?)?;
    }
    crate::plugins::prims::add_prims(lua, &t, kernel.clone())?;
    // Contract admission, export and checks for the contracts host plugin
    t.set("contracts", crate::dispatch::contract_prims::contracts_prim(lua, kernel.clone())?)?;
    // Reverse requests for the rpc host plugin
    t.set("rpc", crate::dispatch::remote::rpc_prim(lua, kernel.clone())?)?;
    // The policy fold over the policy host plugin's voters
    t.set("policy", crate::dispatch::policy_prims::policy_prim(lua, kernel.clone())?)?;
    // a plugin's fiber starting (owner) or unloading (nil): the kernel keeps the `cfg` it received
    let k = kernel.clone();
    t.set(
        "mark_active",
        // and the `cfg` the plugin's fiber received, which runtime config writes update in
        // place (Kernel::plugin_cfg, CFG's `mare._plugin_running`), as the old path's apply did
        lua.create_function(move |_, (name, owner, cfg): (String, Option<u64>, Option<mlua::Table>)| {
            if let Some(kernel) = Kernel::from_weak(&k) {
                let mut st = kernel.st();
                match (owner, cfg) {
                    (Some(_), Some(cfg)) => {
                        st.cfgs.insert(name.clone(), cfg);
                        drop(st);
                        kernel.record_row_dep_prints(&name);
                    }
                    (Some(_), None) => {
                        drop(st);
                        kernel.record_row_dep_prints(&name);
                    }
                    (None, _) => {
                        st.cfgs.remove(&name);
                        drop(st);
                        kernel.mark_module_inactive(&name);
                    }
                }
            }
            Ok(())
        })?,
    )?;
    t.set(mare_shared::KERNEL_SERVICE_NAME, kernel_prims(lua, kernel.clone())?)?;
    // a plugin's modules leave `package.loaded` when its fiber unloads (the old path's
    // dispose_plugin did this), so the next apply requires their current files
    let k = kernel.clone();
    t.set(
        "unload_modules",
        lua.create_function(move |_, name: String| {
            if let Some(kernel) = Kernel::from_weak(&k) {
                kernel.unload_modules(&name);
            }
            Ok(())
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "module_dependents",
        lua.create_function(move |_, name: String| {
            Ok(Kernel::from_weak(&k).map(|kernel| kernel.module_dependents(&name)).unwrap_or_default())
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "sync_core_modules",
        lua.create_function(move |_, ()| {
            if let Some(kernel) = Kernel::from_weak(&k) {
                kernel.sync_core_modules();
            }
            Ok(())
        })?,
    )?;
    // a plugin callback's error, reported like the old path did (problems, log, `kernel/error`)
    let k = kernel.clone();
    t.set(
        "handler_failed",
        lua.create_function(move |_, (plugin, what, err): (String, String, String)| {
            if let Some(kernel) = Kernel::from_weak(&k) {
                kernel.handler_failed(&plugin, &what, None, &err);
            }
            Ok(())
        })?,
    )?;
    let root = Scope::root(kernel);
    // the tree's own work (start, reload) runs in the host's context and waits for it, so
    // a reload triggered by a restricted peer or a tool does not fix its grant into the new fibers
    let in_root = root.clone();
    t.set(
        "run_root",
        lua.create_async_function(move |lua, (f, args): (Function, MultiValue)| in_root.clone().run(lua, f, args))?,
    )?;
    t.set("spawn_root", lua.create_function(move |lua, f: Function| root.spawn(lua, f, "cordis.task").map(|_| ()))?)?;
    Ok(t)
}

/// `prims.kernel` for the `kernel` host plugin (the old Ctx introspection and reload methods).
fn kernel_prims(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let k = kernel.clone();
    // reload(name|nil) -> applied names: the full reload (definitions prepared in Rust, then
    // boot.lua switches the tree), detached like the old `ctx:reload`, since it may dispose the
    // caller itself
    t.set(
        "reload",
        lua.create_async_function(move |_, name: Option<String>| {
            let k = k.clone();
            async move {
                let kernel = Kernel::from_weak(&k).ok_or_else(|| mlua::Error::runtime("kernel is gone"))?;
                let h = mare_shared::txn_scope::spawn_local(async move { kernel.reload(name.as_deref()).await });
                match h.await {
                    Ok(Ok(v)) => Ok(v),
                    Ok(Err(e)) => Err(mlua::Error::runtime(e.to_string())),
                    Err(e) => Err(mlua::Error::runtime(e.to_string())),
                }
            }
        })?,
    )?;
    let k = kernel.clone();
    let k_row = kernel.clone();
    // plugin_row(name) -> plugin `name`'s position in plugins() (definition order), 0 if none: the
    // definitions alone, no tree read (the tools registry asks once per registration)
    t.set(
        "plugin_row",
        lua.create_function(move |_, name: String| {
            Ok(Kernel::from_weak(&k_row)
                .and_then(|k| k.st().defs.iter().position(|d| d.name == name))
                .map_or(0, |i| i + 1))
        })?,
    )?;
    // plugins() -> { {name, origin, active, overrides?}, ... } in definition order
    t.set(
        mare_shared::PLUGINS_KEY,
        lua.create_function(move |lua, ()| {
            let t = lua.create_table()?;
            let Some(k) = Kernel::from_weak(&k) else { return Ok(t) };
            let overrides = k.plugin_overrides();
            for (i, (name, origin, active)) in k.plugin_status().into_iter().enumerate() {
                let e = lua.create_table()?;
                e.set("name", name.as_str())?;
                e.set("origin", origin)?;
                e.set("active", active)?;
                if let Some((_, o)) = overrides.iter().find(|(n, _)| *n == name) {
                    e.set("overrides", o.as_str())?;
                }
                t.set(i + 1, e)?;
            }
            Ok(t)
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "problems",
        lua.create_function(move |_, ()| Ok(Kernel::from_weak(&k).map(|k| k.problems()).unwrap_or_default()))?,
    )?;
    // the plugins stuck loading, unloading or waiting past `stall_ms` (plugins/stalls.rs)
    let k = kernel.clone();
    t.set(
        "stalled",
        lua.create_function(move |lua, ()| {
            let out = lua.create_table()?;
            for (i, s) in Kernel::from_weak(&k).map(|k| k.stalled()).unwrap_or_default().into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("summary", s.message())?;
                row.set("plugin", s.plugin)?;
                row.set("state", s.state)?;
                row.set("ms", s.elapsed_ms)?;
                row.set("effects", s.effects)?;
                row.set("missing", s.missing)?;
                out.set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "session_epoch",
        lua.create_function(move |_, ()| {
            Ok(Kernel::from_weak(&k).map(|k| k.event_log().session_identity().1).unwrap_or_default())
        })?,
    )?;
    let k = kernel;
    // origin(name) -> "builtin" | "user" | ... | nil
    t.set(
        "origin",
        lua.create_function(move |_, name: String| {
            Ok(Kernel::from_weak(&k).and_then(|k| k.def(&name)).map(|d| d.source.origin.as_str().to_string()))
        })?,
    )?;
    Ok(t)
}

impl Kernel {
    /// Boot the cordis host (`lua/mare_kernel/boot.lua`) once per VM and return what it built:
    /// `{ cordis, host, root }`. Runs at kernel start.
    pub fn cordis_boot(&self) -> mlua::Result<Table> {
        const KEY: &str = "mare.cordis_boot";
        let lua = self.lua();
        if let Ok(t) = lua.named_registry_value::<Table>(KEY) {
            return Ok(t);
        }
        let require: Function = lua.globals().get("require")?;
        let boot: Function = require.call("mare_kernel.boot")?;
        let booted: Table = boot.call(prims(lua, self.weak())?)?;
        lua.set_named_registry_value(KEY, booted.clone())?;
        Ok(booted)
    }

    /// Boot an isolated `mare.test.host()` tree backed by `test_kernel`, reusing this host's
    /// Cordis scheduler and `mare.Service` class while keeping all plugin owners, services,
    /// contracts, and loader rows isolated from the outer host.
    pub async fn boot_isolated_test_host(
        &self,
        test_kernel: &Self,
        load_builtin: Function,
        opts: Option<Table>,
    ) -> mlua::Result<Table> {
        struct KernelKeeper(#[allow(dead_code)] Kernel);
        impl UserData for KernelKeeper {}

        let lua = self.lua();
        let outer = self.cordis_boot()?;
        let test_prims = prims(lua, test_kernel.weak())?;
        let helpers = lua.create_table()?;
        helpers.set("load_builtin", load_builtin)?;
        let tk_reg = test_kernel.weak();
        helpers.set(
            "register_def",
            lua.create_function(move |_, table: Table| {
                let k = Self::from_weak(&tk_reg).ok_or_else(|| mlua::Error::runtime("test host is disposed"))?;
                k.register_test_def_table(table)
            })?,
        )?;
        let tk_lookup = test_kernel.weak();
        helpers.set(
            "def_table",
            lua.create_function(move |_, name: String| {
                let Some(k) = Self::from_weak(&tk_lookup) else { return Ok(Value::Nil) };
                Ok(k.def(&name).map(|d| Value::Table(d.table)).unwrap_or(Value::Nil))
            })?,
        )?;
        helpers.set("keeper", lua.create_userdata(KernelKeeper(test_kernel.clone()))?)?;

        let require: Function = lua.globals().get("require")?;
        let factory: Function = require.call("mare_kernel.test_host")?;
        factory.call_async((outer, test_prims, helpers, opts)).await
    }
}

/// The tests' `C`: `{ cordis = <the cordis root ctx> }` (boot.lua `booted.test_ctx`). Test support
/// only.
pub fn test_ctx(k: &Kernel) -> mlua::Result<Value> {
    let booted = k.cordis_boot()?;
    let f: Function = booted.get("test_ctx")?;
    f.call(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::rc::Rc;
    use std::task::{Context, Waker};

    fn nested_pending(lua: &Lua) -> Function {
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
                    let ctx = exec_ctx::current(lua);
                    lua.globals().set("CLOSED_TOOL", ctx.and_then(|ctx| ctx.get::<String>("tool").ok()))?;
                    lua.globals().set("CLOSED_OWNER", crate::plugins::prims::current_owner())?;
                    lua.globals().set("CLOSED", true)
                })
                .unwrap(),
            )
            .unwrap();
        let inner: Function = lua
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
                    async move { mare_shared::lua::call_async::<()>(&lua, &inner, ()).await }
                })
                .unwrap(),
            )
            .unwrap();
        lua.load("return function() nested() end").eval().unwrap()
    }

    fn captured_scope(lua: &Lua) -> Scope {
        let ctx = lua.create_table().unwrap();
        ctx.set("tool", "captured").unwrap();
        Scope { exec: Some(ctx), owner: Some(42), kernel: std::sync::Weak::new() }
    }

    fn assert_closed(lua: &Lua) {
        let globals = lua.globals();
        assert!(globals.get::<bool>("CLOSED").unwrap(), "nested scope was not closed on cancellation");
        assert_eq!(globals.get::<String>("CLOSED_TOOL").unwrap(), "captured");
        assert_eq!(globals.get::<u64>("CLOSED_OWNER").unwrap(), 42);
    }

    #[tokio::test]
    async fn cancelled_scope_run_closes_nested_scope() {
        let lua = Lua::new();
        let f = nested_pending(&lua);
        let mut call = Box::pin(captured_scope(&lua).run(lua.clone(), f, MultiValue::new()));
        assert!(call.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        assert!(lua.globals().get::<bool>("STARTED").unwrap());
        assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
        drop(call);
        assert_closed(&lua);
    }

    #[tokio::test]
    async fn cancelled_scope_spawn_closes_nested_scope() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = Lua::new();
                let f = nested_pending(&lua);
                let task = captured_scope(&lua).spawn(&lua, f, "cordis.test").unwrap();
                tokio::task::yield_now().await;
                assert!(lua.globals().get::<bool>("STARTED").unwrap());
                assert!(!lua.globals().get::<bool>("CLOSED").unwrap());
                task.0.abort();
                while !task.0.is_finished() {
                    tokio::task::yield_now().await;
                }
                assert_closed(&lua);
            })
            .await;
    }

    type ProbeLog = Rc<std::cell::RefCell<Vec<(Option<u64>, bool)>>>;

    fn lua_with_probe(seen: ProbeLog) -> Lua {
        let lua = Lua::new();
        let probe = lua
            .create_function(move |lua, ()| {
                // (active plugin owner, inside a tool execution context?)
                seen.borrow_mut().push((crate::plugins::prims::current_owner(), exec_ctx::current(lua).is_some()));
                Ok(())
            })
            .unwrap();
        lua.globals().set("probe", probe).unwrap();
        lua.globals().set("prims", prims(&lua, std::sync::Weak::new()).unwrap()).unwrap();
        lua
    }

    /// Startup opt: captures in the host's own context share one scope object; a capture in a
    /// tool context or non-root owner is its own (never the shared one) and keeps its context.
    #[tokio::test]
    async fn host_context_captures_share_one_scope_and_others_stay_their_own() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
                let lua = lua_with_probe(seen.clone());
                let same: bool = lua
                    .load("a = prims.capture() b = prims.capture() return rawequal(a, b) and a:is_root()")
                    .eval()
                    .unwrap();
                assert!(same, "two host-context captures are the same root scope");
                let capture: Function = lua
                    .load("return function() c = prims.capture() return rawequal(c, a), c:is_root() end")
                    .eval()
                    .unwrap();
                let tool_ctx = lua.create_table().unwrap();
                let out = exec_ctx::run_in(&lua, tool_ctx, capture.clone(), MultiValue::new()).await.unwrap();
                let (shared, root): (bool, bool) = mlua::FromLuaMulti::from_lua_multi(out, &lua).unwrap();
                assert_eq!((shared, root), (false, false), "a capture in a tool context is its own");
                let (shared_owner, root_owner): (bool, bool) =
                    crate::plugins::prims::in_owner(Some(7), async { capture.call_async(()).await.unwrap() }).await;
                assert_eq!((shared_owner, root_owner), (false, false), "a capture with an owner is its own");
                // the shared root scope still runs with neither owner nor tool context
                let call: Function = lua.load("return function() a:run(probe) end").eval().unwrap();
                let tool_ctx = lua.create_table().unwrap();
                crate::plugins::prims::in_owner(Some(7), exec_ctx::run_in(&lua, tool_ctx, call, MultiValue::new()))
                    .await
                    .unwrap();
                assert_eq!(*seen.borrow(), vec![(None, false)]);
            })
            .await;
    }

    /// T3: a scope carries the owner and tool context of the code that captured it, into `run`
    /// and `spawn`, whatever context later calls them; the root scope carries neither.
    #[tokio::test]
    async fn scopes_carry_the_capturing_context() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
                let lua = lua_with_probe(seen.clone());
                let capture: Function = lua
                    .load("return function() scope = prims.capture(); return scope:is_current() end")
                    .eval()
                    .unwrap();
                let tool_ctx = lua.create_table().unwrap();
                let current = crate::plugins::prims::in_owner(
                    Some(9),
                    exec_ctx::run_in(&lua, tool_ctx, capture, MultiValue::new()),
                )
                .await
                .unwrap();
                assert_eq!(current.into_iter().next().and_then(|v| v.as_boolean()), Some(true));

                let drive: Function = lua
                    .load(
                        r#"return function()
                            assert(not scope:is_current(), "host context is not the captured one")
                            scope:run(probe)
                            scope:spawn(probe)
                            prims.spawn_root(probe)
                        end"#,
                    )
                    .eval()
                    .unwrap();
                drive.call_async::<()>(()).await.unwrap();
                for _ in 0..10 {
                    tokio::task::yield_now().await;
                }
                assert_eq!(*seen.borrow(), vec![(Some(9), true), (Some(9), true), (None, false)]);
            })
            .await;
    }

    /// Work run through `prims.run_root` (boot.lua's start and reload) runs, and
    /// captures, the host's own context even when a restricted owner/tool triggered it.
    #[tokio::test]
    async fn run_root_sheds_the_triggers_grant_and_tool_context() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
                let lua = lua_with_probe(seen.clone());
                let trigger: Function = lua
                    .load(
                        r#"return function()
                            probe()
                            prims.run_root(function()
                                probe()
                                local scope = prims.capture()
                                scope:run(probe)
                            end)
                        end"#,
                    )
                    .eval()
                    .unwrap();
                let tool_ctx = lua.create_table().unwrap();
                crate::plugins::prims::in_owner(Some(11), exec_ctx::run_in(&lua, tool_ctx, trigger, MultiValue::new()))
                    .await
                    .unwrap();
                assert_eq!(*seen.borrow(), vec![(Some(11), true), (None, false), (None, false)]);
            })
            .await;
    }

    /// `Scope::spawn` inherits `txn_scope::current()` from the spawner while restoring the
    /// captured registrant `exec` context and `owner` when the spawned coroutine executes.
    #[tokio::test]
    async fn scope_spawn_inherits_txn_scope_while_restoring_exec_and_owner() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let lua = Lua::new();
                let seen_exec = Rc::new(std::cell::Cell::new(false));
                let seen_owner = Rc::new(std::cell::RefCell::new(None::<u64>));
                let seen_txn = Rc::new(std::cell::Cell::new(None::<u64>));
                let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
                let done_tx = Rc::new(std::cell::RefCell::new(Some(done_tx)));

                let reg_ctx = lua.create_table().unwrap();
                reg_ctx.set("tag", "registrant").unwrap();
                let seen_exec_probe = seen_exec.clone();
                let seen_owner_probe = seen_owner.clone();
                let seen_txn_probe = seen_txn.clone();
                let done_probe = done_tx.clone();
                let probe = lua
                    .create_function(move |lua, ()| {
                        let same_exec = exec_ctx::current(lua).and_then(|t| t.get::<String>("tag").ok()).as_deref()
                            == Some("registrant");
                        seen_exec_probe.set(same_exec);
                        *seen_owner_probe.borrow_mut() = crate::plugins::prims::current_owner();
                        seen_txn_probe.set(mare_shared::txn_scope::current());
                        if let Some(tx) = done_probe.borrow_mut().take() {
                            let _ = tx.send(());
                        }
                        Ok(())
                    })
                    .unwrap();
                lua.globals().set("probe", probe).unwrap();

                let scope_slot = Rc::new(std::cell::RefCell::new(None::<Scope>));
                let scope_capture = scope_slot.clone();
                let cap_fn = lua
                    .create_function(move |lua, ()| {
                        *scope_capture.borrow_mut() = Some(Scope::capture(lua, std::sync::Weak::new()));
                        Ok(())
                    })
                    .unwrap();
                crate::plugins::prims::in_owner(Some(101), exec_ctx::run_in(&lua, reg_ctx, cap_fn, MultiValue::new()))
                    .await
                    .unwrap();
                let captured = scope_slot.borrow_mut().take().unwrap();

                let spawn_fn: Function = lua.load("return function() probe() end").eval().unwrap();
                let caller_ctx = lua.create_table().unwrap();
                caller_ctx.set("tag", "caller").unwrap();
                let trigger = lua
                    .create_function(move |lua, ()| {
                        let _handle = captured.spawn(lua, spawn_fn.clone(), "cordis.test")?;
                        Ok(())
                    })
                    .unwrap();

                mare_shared::txn_scope::scope(42, async {
                    crate::plugins::prims::in_owner(
                        Some(202),
                        exec_ctx::run_in(&lua, caller_ctx, trigger, MultiValue::new()),
                    )
                    .await
                    .unwrap();
                    done_rx.await.unwrap();
                })
                .await;

                assert!(seen_exec.get(), "Scope::spawn must restore registrant exec_ctx");
                assert_eq!(*seen_owner.borrow(), Some(101));
                assert_eq!(seen_txn.get(), Some(42), "Scope::spawn must inherit caller txn_scope");
            })
            .await;
    }
}
