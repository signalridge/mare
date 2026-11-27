//! `mare.store`: session JSONL append/read.

mod attachments;
mod index;
mod read;
mod session;

#[cfg(test)]
mod staged_attachment_tests;

use std::path::PathBuf;

use mlua::{Lua, Table};

use mare_shared::lua::rt_err;

#[cfg(debug_assertions)]
pub use attachments::before_attachment_directory_sync;
pub use session::LuaSessionFile;
#[cfg(debug_assertions)]
pub use session::before_close_marker;

fn closed() -> mlua::Error {
    mlua::Error::runtime("session file is closed")
}

/// Session management touches locks, indexes and related files, so it is a host operation.
fn host_only(lua: &Lua, operation: &str) -> mlua::Result<()> {
    if mare_shared::exec_ctx::current(lua).is_some() {
        return Err(rt_err(format!("store: {operation} is unavailable inside a tool call")));
    }
    if matches!(
        operation,
        "append"
            | "append_batch"
            | "append_batch_async"
            | "flush_async"
            | "open"
            | "delete"
            | "install_staged_attachments"
            | "reserve_staging_attachments"
            | "write staged attachment"
            | "checkpoint"
            | "index_append"
            | "index_reconcile"
            | "index_rebuild"
    ) {
        crate::exec_gate::check_unpoisoned(lua)?;
    }
    Ok(())
}

fn check_handle_read(lua: &Lua, file: &LuaSessionFile) -> mlua::Result<()> {
    let path = file.file.lock().map_err(rt_err)?.as_ref().ok_or_else(closed)?.path().to_string_lossy().into_owned();
    crate::exec_ctx::check(lua, "read", &path)
}

fn sessions_root() -> PathBuf {
    mare_shared::paths::sessions_dir()
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    session::install_session_ops(lua, &t)?;
    read::install_read_ops(lua, &t)?;
    read::install_snapshot_ops(lua, &t)?;
    index::install_index_ops(lua, &t)?;
    t.set("root", mare_shared::paths::sessions_dir().to_string_lossy().into_owned())?;
    // the session format version a header carries; its one owner is mare_store::CURRENT_VERSION
    t.set("format_version", mare_store::CURRENT_VERSION)?;
    crate::session_surface::install(lua, &t)?;
    crate::session_query::install(lua, &t)?;
    mare.set("store", t)?;
    Ok(())
}
