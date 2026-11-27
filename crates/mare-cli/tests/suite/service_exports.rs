//! The public JSON surface uses the existing provider stack and caller-owned lifetimes.
use crate::common::*;
use serde_json::json;

/// Native test plugins that provide services for the tests (the native form of the old
/// the legacy provide of a name, impl and options): one plugin `p-<name>` per service name, written before the host
/// starts. `PROVIDE(name, impl, opts, parent?)` stores the spec (methods `function(self, caller, ...)`
/// as before, `opts.contract`, an optional parent class) in `mare._specs` and re-applies that plugin
/// (admission needs a running owner, which a plugin's apply has); it raises when nothing was
/// published and returns the native service fiber's disposer.
const SVC_LUA: &str = r#"
    function PROVIDE(name, impl, opts, parent)
        mare._specs = mare._specs or {}
        local spec = { impl = impl, opts = opts, parent = parent }
        mare._specs[name] = spec
        local kernel = C.cordis:get("kernel")
        kernel:reload("p-" .. name)
        kernel:reload_barrier()
        if C.cordis:get(name) == nil then
            mare._specs[name] = nil
            error("provide refused: " .. name .. " :: " .. table.concat(kernel:problems(), " ; "), 0)
        end
        return mare._handles[spec].dispose
    end
"#;

fn svc_plugin_source(name: &str) -> String {
    format!(
        r#"return {{ name = "p-{name}", apply = function(ctx)
  local spec = (mare._specs or {{}})["{name}"]
  if not spec then return end
  local Class = require("cordis").class("P_{name}", spec.parent or mare.Service)
  function Class:constructor(c)
    Class.super.constructor(self, c, "{name}", spec.opts and {{ contract = spec.opts.contract }} or nil)
  end
  for k, v in pairs(spec.impl) do Class[k] = function(self, ...) return v(self, self.ctx, ...) end end
  mare._handles = mare._handles or {{}}
  mare._handles[spec] = ctx:plugin(Class)
end }}"#
    )
}

/// Write the provider plugins for `names`; the host must be built with `user_plugins()`.
fn svc_plugins(env: &TestEnv, names: &[&str]) {
    let dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    for name in names {
        std::fs::write(dir.join(format!("p-{name}.lua")), svc_plugin_source(name)).unwrap();
    }
}

/// Install `PROVIDE`.
async fn svc(host: &mare_host::Host) {
    exec(host, SVC_LUA).await;
}

/// A caller of the JSON export surface with its own owner record (`<root>` / `root`: what a root
/// Ctx was, without the userdata; `dispose_ctx(owner)` releases what it holds). Its
/// `call_service(name, method, args, opts)` goes through `Kernel::call_export_from_lua`, the one
/// Lua-side conversion, and raises the call's mare.Error value as the old Ctx method did.
fn json_caller_as_owner(host: &mare_host::Host) -> (mlua::Table, u64) {
    let owner = host.kernel.open_owner("<root>", "root").unwrap();
    let kernel = host.kernel.clone();
    let raw = host
        .lua()
        .create_async_function(
            move |_, (name, method, args, opts): (String, String, mlua::Value, Option<mlua::Table>)| {
                let kernel = kernel.clone();
                async move { kernel.call_export_from_lua(owner, &name, &method, args, opts).await }
            },
        )
        .unwrap();
    let caller: mlua::Table = host
        .lua()
        .load(
            r#"local raw = ...
            return { call_service = function(_, name, method, args, opts)
                local ok, value = raw(name, method, args, opts)
                if not ok then error(value, 2) end
                return value
            end }"#,
        )
        .call(raw)
        .unwrap();
    (caller, owner)
}

/// Install the JSON caller `RC` (its own owner record).
fn json_caller(host: &mare_host::Host) {
    let (caller, _) = json_caller_as_owner(host);
    global(host, "RC", caller);
}

mod exports_and_contracts;
mod lifecycle_and_inheritance;
