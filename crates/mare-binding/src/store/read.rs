use std::path::Path;
use std::sync::{Arc, atomic::Ordering};

use mlua::{Lua, Table};

use mare_shared::lua::{json_to_lua, rt_err};

use super::host_only;
use super::session::{LuaSessionFile, SnapshotRevision, read_session};

pub(super) fn install_read_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // read(path, {lenient?}) -> entries, skipped, intact, stable (async: parsing runs on the
    // blocking pool). `stable = false`: the file still changed on the stat-checked re-read (a
    // writer appending or rolling back) — mark the read-only view "unstable", never fork from it.
    // `intact`: entries before the first bad line (what a fork of a damaged file may copy).
    // Lock-free with a frozen end; an unterminated final entry never counts. Strict by default: a damaged file (a bad line that isn't a torn tail) is an error carrying
    // mare_store::CORRUPT. `lenient = true` (read-only views of a damaged file) skips bad lines
    // and also returns how many. Never migrates or rewrites the file.
    t.set(
        "read",
        lua.create_async_function(|lua, (p, opts): (String, Option<Table>)| {
            let path = crate::exec_ctx::checked_process_path(&lua, "read", &p);
            async move {
                let p = path?;
                let lenient = opts.and_then(|o| o.get::<Option<bool>>("lenient").ok().flatten()).unwrap_or(false);
                let (entries, skipped, intact, stable) = crate::blocking::spawn(&lua, move || {
                    if lenient {
                        mare_store::read_entries_lenient_stable(&p)
                    } else {
                        mare_store::read_entries_stable(&p).map(|(e, stable)| {
                            let n = e.len();
                            (e, 0, n, stable)
                        })
                    }
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                Ok((json_to_lua(&lua, &serde_json::Value::Array(entries))?, skipped, intact, stable))
            }
        })?,
    )?;
    t.set(
        "read_bound",
        lua.create_async_function(|lua, (p, lenient): (String, bool)| {
            let allowed = host_only(&lua, "read_snapshot");
            async move {
                allowed?;
                let (entries, skipped) =
                    crate::blocking::spawn(&lua, move || mare_store::read_entries_bound(Path::new(&p), lenient))?
                        .await
                        .map_err(rt_err)?
                        .map_err(rt_err)?;
                Ok((json_to_lua(&lua, &serde_json::Value::Array(entries))?, skipped))
            }
        })?,
    )?;
    // Fork observations are independent of the source writer and its lock.
    t.set(
        "read_fork_snapshot",
        lua.create_async_function(|lua, (path, id): (String, Option<String>)| {
            let allowed = host_only(&lua, "read_snapshot");
            async move {
                allowed?;
                let entries = crate::blocking::spawn(&lua, move || {
                    mare_store::read_fork_snapshot(Path::new(&path), id.as_deref())
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                json_to_lua(&lua, &serde_json::Value::Array(entries))
            }
        })?,
    )?;
    // read_from_compaction(path) -> {entries, partial}: reverse read back to the latest
    // compaction (and the message it keeps from); `partial` = older history not loaded.
    // Strict (CORRUPT on a bad line in the region read), lock-free, frozen end, read-only.
    t.set(
        "read_from_compaction",
        lua.create_async_function(|lua, p: String| {
            let path = crate::exec_ctx::checked_process_path(&lua, "read", &p);
            async move {
                let p = path?;
                let (tail, header) = crate::blocking::spawn(&lua, move || {
                    let tail = mare_store::reverse::read_from_compaction(&p)?;
                    let header = mare_store::read_header(&p);
                    anyhow::Ok((tail, header))
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                let t = lua.create_table()?;
                if let Some(h) = header {
                    t.set("header", json_to_lua(&lua, &h)?)?;
                }
                t.set("entries", json_to_lua(&lua, &serde_json::Value::Array(tail.entries))?)?;
                t.set("partial", tail.partial)?;
                t.set("stable", tail.stable)?;
                Ok(t)
            }
        })?,
    )?;
    // read_tail(path, n) -> last n entries
    t.set(
        "read_tail",
        lua.create_async_function(|lua, (p, n): (String, usize)| {
            let path = crate::exec_ctx::checked_process_path(&lua, "read", &p);
            async move {
                let p = path?;
                let v = crate::blocking::spawn(&lua, move || mare_store::reverse::read_tail(&p, n))?
                    .await
                    .map_err(rt_err)?
                    .map_err(rt_err)?;
                json_to_lua(&lua, &serde_json::Value::Array(v))
            }
        })?,
    )?;
    // file_contains(path, query) -> bool (streamed; for cancellable content search)
    t.set(
        "file_contains",
        lua.create_async_function(|lua, (p, q): (String, String)| {
            let path = crate::exec_ctx::checked_process_path(&lua, "read", &p);
            async move {
                let p = path?;
                crate::blocking::spawn(&lua, move || mare_store::index::file_contains(&p, &q))?.await.map_err(rt_err)
            }
        })?,
    )?;
    // search(path, query) -> snippet | nil: first match in message text (case-insensitive)
    t.set(
        "search",
        lua.create_async_function(|lua, (p, q): (String, String)| {
            let path = crate::exec_ctx::checked_process_path(&lua, "read", &p);
            async move {
                let p = path?;
                crate::blocking::spawn(&lua, move || mare_store::index::content_snippet(&p, &q))?.await.map_err(rt_err)
            }
        })?,
    )?;
    Ok(())
}

pub(super) fn install_snapshot_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // Read via an independently opened handle verified against the locked writer.
    t.set(
        "read_snapshot",
        lua.create_async_function(|lua, (file, revision): (mlua::AnyUserData, Option<mlua::AnyUserData>)| {
            let allowed = host_only(&lua, "read_snapshot");
            let file = file.borrow::<LuaSessionFile>().map(|f| f.file.clone());
            let revision = revision
                .map(|r| r.borrow::<SnapshotRevision>().map(|r| (r.file.clone(), r.stamp, r.generation)))
                .transpose();
            async move {
                allowed?;
                let file = file?;
                let revision = revision?;
                if revision.as_ref().is_some_and(|(owner, _, _)| !Arc::ptr_eq(owner, &file)) {
                    return Err(rt_err("session snapshot belongs to another file"));
                }
                let (entries, _) = crate::blocking::spawn(&lua, move || {
                    read_session(&file, revision.as_ref().map(|(_, _, generation)| *generation), |f| {
                        f.read_snapshot(revision.map(|(_, stamp, _)| stamp))
                    })
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                json_to_lua(&lua, &serde_json::Value::Array(entries))
            }
        })?,
    )?;
    t.set(
        "read_tail_snapshot",
        lua.create_async_function(|lua, file: mlua::AnyUserData| {
            let allowed = host_only(&lua, "read_snapshot");
            let file = file.borrow::<LuaSessionFile>().map(|f| f.file.clone());
            async move {
                allowed?;
                let file = file?;
                let work_file = file.clone();
                let ((tail, header, stamp), generation) =
                    crate::blocking::spawn(&lua, move || read_session(&work_file, None, |f| f.read_tail_snapshot()))?
                        .await
                        .map_err(rt_err)?
                        .map_err(rt_err)?;
                let out = lua.create_table()?;
                out.set("header", json_to_lua(&lua, &header)?)?;
                out.set("entries", json_to_lua(&lua, &serde_json::Value::Array(tail.entries))?)?;
                out.set("partial", tail.partial)?;
                out.set("stable", true)?;
                out.set("revision", lua.create_userdata(SnapshotRevision { file, stamp, generation })?)?;
                Ok(out)
            }
        })?,
    )?;
    t.set(
        "snapshot_matches",
        lua.create_function(|lua, (file, revision): (mlua::AnyUserData, mlua::AnyUserData)| {
            host_only(lua, "read_snapshot")?;
            let file = file.borrow::<LuaSessionFile>()?;
            let revision = revision.borrow::<SnapshotRevision>()?;
            if !Arc::ptr_eq(&file.file, &revision.file) {
                return Ok(false);
            }
            let guard = file.file.lock().map_err(rt_err)?;
            Ok(file.file.generation.load(Ordering::Acquire) == revision.generation
                && guard.as_ref().is_some_and(|f| f.matches_snapshot(revision.stamp)))
        })?,
    )?;
    Ok(())
}
