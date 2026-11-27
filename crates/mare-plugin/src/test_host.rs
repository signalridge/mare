//! Rust backing for `mare.test.host(opts?)`: creates an isolated [`mare_kernel::Kernel`]
//! sharing the Lua VM and built-in service contracts, without touching outer-host state.

use std::sync::Weak;

use mare_kernel::{Kernel, KernelInner};
use mlua::{Function, Lua, Table};

/// Build the async Lua function passed into `preset/test.lua` for `mare.test.host(opts?)`.
pub(crate) fn build_factory(lua: &Lua) -> mlua::Result<Function> {
    lua.create_async_function(|lua, opts: Option<Table>| async move {
        let outer = lua
            .app_data_ref::<Weak<KernelInner>>()
            .and_then(|weak| Kernel::from_weak(&weak))
            .ok_or_else(|| mlua::Error::runtime("mare.test.host(): kernel is not booted in this Lua VM"))?;

        let test_kernel = outer.new_isolated_from()?;
        let tk_weak = test_kernel.weak();
        let load_builtin = lua.create_function(move |_, name: String| {
            let k = Kernel::from_weak(&tk_weak).ok_or_else(|| mlua::Error::runtime("test host is disposed"))?;
            let src = crate::loader::builtin_sources()
                .into_iter()
                .find(|s| s.name_hint == name)
                .ok_or_else(|| mlua::Error::runtime(format!("unknown built-in plugin `{name}`")))?;
            let def = k.load_def(&src).map_err(|e| mlua::Error::runtime(e.to_string()))?;
            let table = def.table.clone();
            k.upsert_test_def(def);
            Ok(table)
        })?;

        outer.boot_isolated_test_host(&test_kernel, load_builtin, opts).await
    })
}
