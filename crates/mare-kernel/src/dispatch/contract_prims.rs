//! `prims.contracts`: what mare.Service (mare_kernel/service.lua)
//! needs from Rust. Admission and the registry half of a provide are separate so the service can run
//! them inside one cordis effect; `permitted` / `validate` back the public `mare.contracts.*`.
//!
//!   prims.contracts.admit(name, value, options?) -> handle contract selection + admission, for
//!                                                         the running owner; raises when refused
//!   prims.contracts.declared(name) -> bool a built-in contract or any definition
//!                                                         declares the name
//!   prims.contracts.export(handle, value) -> seq register what was admitted (JSON
//!                                                         export, catalog, permitted/validate)
//!   prims.contracts.registrations(handle) -> {method...} the contract's `registration = true` methods
//!   prims.contracts.methods(handle) -> {method...} the methods declared for Lua callers
//!   prims.contracts.guard_handle(handle) -> guard_handle lightweight handle retaining derived
//!                                                         exports for lazy per-method guards
//!   prims.contracts.method_guard(guard_handle, method) -> guard? {passthrough, enter(...) -> err?,
//!                                                         leave(packed) -> value, err, fail(err) -> err}
//!                                                         (mare.Service wraps with it; a
//!                                                         registration's disposer is returned as is)
//!   prims.contracts.withdraw(owner, seq) -> bool remove that registration (idempotent,
//!                                                         synchronous: it runs in effect cleanups)
//!   prims.contracts.permitted(service, method, caller?) -> bool
//!   prims.contracts.validate(service, method, value) -> true, or raises `invalid_result:...`

use std::cell::RefCell;

use mlua::{AnyUserData, Lua, Table, UserData, Value};

use super::services::Admitted;
use crate::{Kernel, KernelInner};

/// The registry key of mare.Service's class table (`prims.contracts.base`).
pub(crate) const SERVICE_BASE: &str = "mare.service_base";

/// What `admit` admitted, for one `export`.
struct Handle {
    owner: u64,
    name: String,
    admitted: RefCell<Option<Admitted>>,
}

impl UserData for Handle {}

/// Lightweight handle retaining only the admitted service's `Rc<ServiceExports>` so
/// `mare.Service` can construct per-method guards lazily on first invocation (even after
/// `export` has consumed `Handle`).
struct GuardHandle(Option<std::rc::Rc<super::service_rpc::ServiceExports>>);

impl UserData for GuardHandle {}

pub(crate) fn contracts_prim(lua: &Lua, kernel: std::sync::Weak<KernelInner>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let gone = || mlua::Error::runtime("kernel is gone");
    let k = kernel.clone();
    t.set(
        "declared",
        lua.create_function(move |_, name: String| {
            let kernel = Kernel::from_weak(&k).ok_or_else(gone)?;
            let st = kernel.st();
            Ok(st.contracts.contains_key(&name)
                || st.defs.iter().any(|def| def.provide.contains(&name) || def.contracts.contains_key(&name))
                || st.declared.iter().any(|def| def.provide.contains(&name) || def.contracts.contains(&name)))
        })?,
    )?;
    // base(class): mare.Service's class table, once (service.lua): admission's walk over a
    // provider's class chain stops at it, by identity
    t.set("base", lua.create_function(|lua, class: Table| lua.set_named_registry_value(SERVICE_BASE, class))?)?;
    let k = kernel.clone();
    t.set(
        "admit",
        lua.create_function(move |lua, (name, value, options): (String, Value, Option<Table>)| {
            let kernel = Kernel::from_weak(&k).ok_or_else(gone)?;
            let owner = crate::plugins::prims::current_owner()
                .ok_or_else(|| mlua::Error::runtime("prims.contracts.admit: no owner is running"))?;
            let admitted = kernel.admit_service(owner, &name, &value, options, false)?;
            lua.create_userdata(Handle { owner, name, admitted: RefCell::new(Some(admitted)) })
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "export",
        lua.create_function(move |_, (handle, value): (AnyUserData, Value)| {
            let kernel = Kernel::from_weak(&k).ok_or_else(gone)?;
            let handle = handle.borrow::<Handle>()?;
            let admitted =
                handle.admitted.borrow_mut().take().ok_or_else(|| {
                    mlua::Error::runtime("prims.contracts.export: this admission was already exported")
                })?;
            kernel.register_admitted(handle.owner, &handle.name, value, admitted)
        })?,
    )?;
    t.set(
        "registrations",
        lua.create_function(move |lua, handle: AnyUserData| {
            let handle = handle.borrow::<Handle>()?;
            let admitted = handle.admitted.borrow();
            let admitted = admitted
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("prims.contracts.registrations: already exported"))?;
            lua.create_sequence_from(admitted.registrations())
        })?,
    )?;
    t.set(
        "methods",
        lua.create_function(move |lua, handle: AnyUserData| {
            let handle = handle.borrow::<Handle>()?;
            let admitted = handle.admitted.borrow();
            let admitted =
                admitted.as_ref().ok_or_else(|| mlua::Error::runtime("prims.contracts.methods: already exported"))?;
            lua.create_sequence_from(admitted.lua_methods())
        })?,
    )?;
    t.set(
        "host_methods",
        lua.create_function(move |lua, handle: AnyUserData| {
            let handle = handle.borrow::<Handle>()?;
            let admitted = handle.admitted.borrow();
            let admitted = admitted
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("prims.contracts.host_methods: already exported"))?;
            lua.create_sequence_from(admitted.host_lua_methods())
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "host_method",
        lua.create_function(move |_, (service, method): (String, String)| {
            let kernel = Kernel::from_weak(&k).ok_or_else(gone)?;
            match kernel.host_method_entry(&service, &method)? {
                Some((func, is_async)) => Ok((func, is_async)),
                None => Err(mlua::Error::external(mare_shared::errors::Coded {
                    code: "unavailable",
                    message: format!("host method `{service}.{method}` is not registered"),
                })),
            }
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "register_host_method",
        lua.create_function(move |_, (service, method, func): (String, String, mlua::Function)| {
            let kernel = Kernel::from_weak(&k).ok_or_else(gone)?;
            kernel.register_host_method(&service, &method, func)
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "register_host_method_async",
        lua.create_function(move |_, (service, method, func): (String, String, mlua::Function)| {
            let kernel = Kernel::from_weak(&k).ok_or_else(gone)?;
            kernel.register_host_method_async(&service, &method, func)
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "host_only_services",
        lua.create_function(move |lua, ()| {
            let kernel = Kernel::from_weak(&k).ok_or_else(gone)?;
            lua.create_sequence_from(kernel.host_only_services())
        })?,
    )?;
    t.set(
        "guard_handle",
        lua.create_function(move |lua, handle: AnyUserData| {
            let handle = handle.borrow::<Handle>()?;
            let admitted = handle.admitted.borrow();
            let admitted = admitted
                .as_ref()
                .ok_or_else(|| mlua::Error::runtime("prims.contracts.guard_handle: already exported"))?;
            lua.create_userdata(GuardHandle(admitted.exports()))
        })?,
    )?;
    t.set(
        "method_guard",
        lua.create_function(move |lua, (guard_handle, method): (AnyUserData, String)| {
            let guard_handle = guard_handle.borrow::<GuardHandle>()?;
            match &guard_handle.0 {
                Some(exports) => exports.guard(lua, &method),
                None => Ok(None),
            }
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "withdraw",
        lua.create_function(move |_, (owner, seq): (u64, u64)| match Kernel::from_weak(&k) {
            Some(kernel) => Ok(kernel.withdraw_now(owner, seq)),
            None => Ok(false),
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "permitted",
        lua.create_function(move |_, (service, method, caller): (String, String, Option<u64>)| {
            Ok(Kernel::from_weak(&k).ok_or_else(gone)?.permitted_for_caller(caller, &service, &method))
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "is_peer",
        lua.create_function(move |_, caller: Option<u64>| {
            Ok(Kernel::from_weak(&k).ok_or_else(gone)?.is_peer_caller(caller))
        })?,
    )?;
    let k = kernel.clone();
    t.set(
        "allows_session_path",
        lua.create_function(move |_, caller: Option<u64>| {
            Ok(Kernel::from_weak(&k).ok_or_else(gone)?.allows_session_path(caller))
        })?,
    )?;
    t.set(
        "validate",
        lua.create_function(move |_, (service, method, value): (String, String, Value)| {
            Kernel::from_weak(&kernel)
                .ok_or_else(gone)?
                .validate_result(&service, &method, value)
                .map(|()| true)
                .map_err(mlua::Error::runtime)
        })?,
    )?;
    Ok(t)
}

impl Kernel {
    /// Remove one registration at once: its lifetime ends (in-flight JSON calls see `unavailable`)
    /// and it leaves the registry. In the cordis tree nothing follows a provider through the
    /// kernel's watchers, so nothing is awaited.
    pub(crate) fn withdraw_now(&self, owner: u64, seq: u64) -> bool {
        let reloading = *self.0.reloading.borrow() > 0;
        let mut st = self.st();
        let mut withdrawn = None;
        st.services.retain(|name, entry| {
            let hit = entry.ctx == owner && entry.seq == seq;
            if hit {
                entry.lifetime.cancel();
                withdrawn = Some(name.clone());
            }
            !hit
        });
        // a name nobody provides any more drops its derived exports (a connection's services come
        // and go by name); during a reload the restart provides it again, and what stays orphaned
        // is swept when the reload ends (ReloadHold)
        if let Some(name) = &withdrawn
            && !reloading
            && !st.services.get(name).is_some_and(|entry| !entry.lifetime.is_cancelled())
        {
            st.derived.remove(name);
        }
        withdrawn.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel_with_prims() -> Kernel {
        let k = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        let t = k.lua().create_table().unwrap();
        crate::plugins::prims::add_prims(k.lua(), &t, k.weak()).unwrap();
        t.set("contracts", contracts_prim(k.lua(), k.weak()).unwrap()).unwrap();
        k.lua().globals().set("prims", t).unwrap();
        k
    }

    const CONTRACT: &str = r#"{ contract = { methods = {
        echo = { doc = "echo", expose = "both", params = { type = "object",
            properties = { text = { type = "string" } }, required = { "text" }, additionalProperties = false },
            order = { "text" }, result = { type = "string" } } } } }"#;

    /// Admission refuses a value that does not implement its contract and registers nothing; an
    /// admitted value is registered only when exported, and is gone once withdrawn.
    #[tokio::test]
    async fn admission_is_separate_from_the_registry_and_permitted_validate_follow_it() {
        let k = kernel_with_prims();
        let seen: String = k
            .lua()
            .load(format!(
                r#"
                local id = prims.owner.open("probe", "user")
                local opts = {CONTRACT}
                local refused = select(2, pcall(prims.owner.enter, id, prims.contracts.admit, "probe-svc", {{}}, opts))
                return tostring(refused)
                "#
            ))
            .eval_async()
            .await
            .unwrap_or_else(|e| e.to_string());
        assert!(seen.contains("requires method `echo`"), "{seen}");
        let (before, after_export, ok, bad, withdrawn): (String, bool, bool, String, String) = k
            .lua()
            .load(format!(
                r#"
                local id = prims.owner.open("probe2", "user")
                local opts = {CONTRACT}
                local impl = {{ echo = function(_, _, text) return text end }}
                local handle = prims.owner.enter(id, prims.contracts.admit, "probe-svc", impl, opts)
                -- admitted only: nothing is registered yet
                local before = select(2, pcall(prims.contracts.validate, "probe-svc", "echo", "x"))
                local seq = prims.contracts.export(handle, impl)
                local after_export = prims.contracts.permitted("probe-svc", "echo")
                local ok = prims.contracts.validate("probe-svc", "echo", "x")
                local bad = select(2, pcall(prims.contracts.validate, "probe-svc", "echo", 42))
                assert(prims.contracts.withdraw(id, seq) and not prims.contracts.withdraw(id, seq))
                local withdrawn = select(2, pcall(prims.contracts.validate, "probe-svc", "echo", "x"))
                return tostring(before), after_export, ok, tostring(bad), tostring(withdrawn)
                "#
            ))
            .eval_async()
            .await
            .unwrap();
        assert!(before.contains("unavailable"), "{before}");
        assert!(after_export && ok, "permitted and validate see the exported contract");
        assert!(bad.contains("invalid_result"), "{bad}");
        assert!(withdrawn.contains("unavailable"), "{withdrawn}");
    }

    /// `methods` lists what the contract declares for Lua callers; `method_guard` checks one
    /// method's arguments and settles its result, for mare.Service's per-method wrappers.
    #[tokio::test]
    async fn the_per_method_guard_checks_arguments_and_results() {
        let k = kernel_with_prims();
        let seen: String = k
            .lua()
            .load(format!(
                r#"
                local id = prims.owner.open("probe3", "user")
                local impl = {{ echo = function(_, _, text) return text end }}
                local handle = prims.owner.enter(id, prims.contracts.admit, "probe-svc3", impl, {CONTRACT})
                local gh = prims.contracts.guard_handle(handle)
                local g = prims.contracts.method_guard(gh, "echo")
                local refused = g.enter(42)
                local value, err = g.leave(table.pack("ok"))
                local bad_value, bad = g.leave(table.pack(7))
                return table.concat({{ table.concat(prims.contracts.methods(handle), ","), tostring(g.passthrough),
                    tostring(g.enter("hi")), tostring(refused.code), tostring(value), tostring(err), tostring(bad.code),
                    tostring(prims.contracts.method_guard(gh, "nope")) }}, "|")
                "#
            ))
            .eval_async()
            .await
            .unwrap();
        assert_eq!(seen, "echo|false|nil|invalid_arguments|ok|nil|invalid_result|nil");
    }

    #[tokio::test]
    async fn contract_prims_reject_missing_owner_post_export_use_and_missing_host_method() {
        let k = kernel_with_prims();
        let ok: bool = k
            .lua()
            .load(format!(
                r#"
                local ok_no_owner, err_no_owner = pcall(prims.contracts.admit, "probe-svc4", {{}}, {CONTRACT})
                assert(not ok_no_owner and tostring(err_no_owner):find("no owner is running", 1, true))

                local ok_missing, err_missing = pcall(prims.contracts.host_method, "no_such_svc", "nope")
                assert(not ok_missing and tostring(err_missing):find("not registered", 1, true))

                local id = prims.owner.open("probe4", "user")
                local impl = {{ echo = function(_, _, text) return text end }}
                local handle = prims.owner.enter(id, prims.contracts.admit, "probe-svc4", impl, {CONTRACT})
                local gh = prims.contracts.guard_handle(handle)
                prims.contracts.export(handle, impl)

                for _, fn_name in ipairs({{ "registrations", "methods", "host_methods", "guard_handle" }}) do
                    local ok_after, err_after = pcall(prims.contracts[fn_name], handle)
                    assert(not ok_after and tostring(err_after):find("already exported", 1, true), fn_name)
                end
                local ok_exp2, err_exp2 = pcall(prims.contracts.export, handle, impl)
                assert(not ok_exp2 and tostring(err_exp2):find("already exported", 1, true))
                local ok_guard = pcall(prims.contracts.method_guard, handle, "echo")
                assert(not ok_guard)

                -- Extracted guard_handle stays usable after export and caches per-method guards.
                local g1 = prims.contracts.method_guard(gh, "echo")
                local g2 = prims.contracts.method_guard(gh, "echo")
                assert(g1 ~= nil and rawequal(g1, g2))
                assert(g1.enter("ok") == nil and g1.enter(42).code == "invalid_arguments")
                assert(prims.contracts.method_guard(gh, "nope") == nil)
                return true
                "#
            ))
            .eval_async()
            .await
            .unwrap();
        assert!(ok);

        let lua = k.lua().clone();
        drop(k);
        let withdrawn_gone: bool = lua.load("return prims.contracts.withdraw(1, 1)").eval().unwrap();
        assert!(!withdrawn_gone);
        let err_gone = lua.load("return prims.contracts.declared('x')").eval::<bool>().unwrap_err().to_string();
        assert!(err_gone.contains("kernel is gone"), "{err_gone}");
    }
}
