//! Core Lua UI engine loader (`ui_prelude`, `mare_binding::ui`, `ui_components`, `ui_shell`).

use std::sync::Arc;

use mlua::{Lua, Table};

/// Install the UI prelude, Rust UI bindings, and default UI component/shell chunks.
pub fn install(lua: &Lua, mare: &Table, ui_shared: Arc<mare_shared::ui::UiShared>) -> mlua::Result<Table> {
    let core = mare_shared::lua::core::ensure(lua)?;
    let ui_core = mare_shared::lua::core::ui::handle(lua)?;
    ui_core.set(
        "component_changed",
        lua.create_function(|_, ()| {
            mare_shared::style::theme_changed();
            Ok(())
        })?,
    )?;
    let ui_table: Table = lua.load(crate::ui_prelude_chunk().as_ref()).set_name("@preset/ui_prelude.lua").eval()?;
    mare_binding::ui::install(lua, mare, ui_table.clone(), ui_shared)?;
    lua.load(crate::ui_components_chunk().as_ref())
        .set_name("@preset/ui_components.lua")
        .call::<()>((mare.clone(), core.clone()))?;
    lua.load(crate::ui_shell_chunk().as_ref()).set_name("@preset/ui_shell.lua").call::<()>((mare.clone(), core))?;
    Ok(ui_table)
}
