//! Rust accessors for the core Lua UI engine (`core.ui`).
//!
//! Host-installed UI wait hooks (`wait_hook`, `wait_close`) and core-UI helpers live strictly
//! on the registry-held `core.ui` table.

use mlua::{Function, Lua, Table};

/// Return the `core.ui` namespace table on the VM's core handle.
pub fn handle(lua: &Lua) -> mlua::Result<Table> {
    super::namespace(lua, "ui")
}

pub fn set_wait_hook(lua: &Lua, hook: Function) -> mlua::Result<()> {
    handle(lua)?.set("wait_hook", hook)
}

pub fn set_wait_close(lua: &Lua, close: Function) -> mlua::Result<()> {
    handle(lua)?.set("wait_close", close)
}

/// Defer status-segment renders until [`end_status_batch`] flushes dirty segments once.
pub fn begin_status_batch(lua: &Lua) {
    if let Ok(t) = handle(lua)
        && let Ok(f) = t.get::<Function>("status_batch_begin")
    {
        let _ = f.call::<()>(());
    }
}

/// End a status-render batch and flush dirty segments once when the outermost batch closes.
pub fn end_status_batch(lua: &Lua) {
    if let Ok(t) = handle(lua)
        && let Ok(f) = t.get::<Function>("status_batch_end")
    {
        let _ = f.call::<()>(());
    }
}
