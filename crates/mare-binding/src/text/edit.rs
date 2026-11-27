//! Heavy blocking edit and diff bindings (`replace`, `apply_patch`, `diff`, `diff_stats`).

use std::path::{Path, PathBuf};

use mlua::{Lua, MultiValue, Table, Value};

use mare_edit::{apply_patch, replace};
use mare_shared::lua::{nil_err, rt_err};

async fn check_patch_policy(
    lua: &Lua,
    active: Option<crate::exec_ctx::Active>,
    patch: &str,
    cwd: &Path,
    dry: bool,
) -> mlua::Result<()> {
    let Some(a) = active else { return Ok(()) };
    // the dry run reads every file the patch touches: inside a tool call those reads go
    // through the same policy check as mare.fs.read first, so a read-denied file's
    // contents (or existence, via the error text) never reach the model
    if let Ok(hunks) = apply_patch::parse_patch(patch) {
        for h in &hunks {
            let p = match h {
                apply_patch::Hunk::Add { path, .. }
                | apply_patch::Hunk::Delete { path }
                | apply_patch::Hunk::Update { path, .. } => path,
            };
            let abs = if p.is_absolute() { p.clone() } else { cwd.join(p) };
            crate::exec_ctx::check_with(lua, &a, "read", &abs.to_string_lossy())?;
        }
    }
    if !dry {
        let (p2, c2) = (patch.to_owned(), cwd.to_path_buf());
        if let Ok(changes) =
            tokio::task::spawn_blocking(move || apply_patch::compute_changes(&p2, &c2)).await.map_err(rt_err)?
        {
            for ch in &changes {
                crate::exec_ctx::check_with(lua, &a, "write", &ch.path.to_string_lossy())?;
                if let Some(m) = &ch.move_to {
                    crate::exec_ctx::check_with(lua, &a, "write", &m.to_string_lossy())?;
                }
            }
        }
    }
    Ok(())
}

fn build_patch_result(lua: &Lua, changes: Vec<apply_patch::FileChange>) -> mlua::Result<MultiValue> {
    let t = lua.create_table()?;
    let list = lua.create_table()?;
    for (i, ch) in changes.iter().enumerate() {
        let e = lua.create_table()?;
        e.set("path", ch.path.to_string_lossy().into_owned())?;
        e.set("move_to", ch.move_to.as_ref().map(|p| p.to_string_lossy().into_owned()))?;
        e.set("kind", format!("{:?}", ch.kind).to_lowercase())?;
        e.set("old", ch.old.clone())?;
        e.set("new", ch.new.clone())?;
        list.set(i + 1, e)?;
    }
    t.set("changes", list)?;
    t.set("summary", apply_patch::summarize(&changes))?;
    Ok(MultiValue::from_vec(vec![Value::Table(t)]))
}

async fn apply_patch_exec(
    lua: Lua,
    patch: String,
    opts: Option<Table>,
    cwd: PathBuf,
    active: mlua::Result<Option<crate::exec_ctx::Active>>,
) -> mlua::Result<MultiValue> {
    let dry = opts.and_then(|o| o.get::<Option<bool>>("dry_run").ok().flatten()).unwrap_or(false);
    check_patch_policy(&lua, active?, &patch, &cwd, dry).await?;
    let r = tokio::task::spawn_blocking(move || {
        if dry { apply_patch::compute_changes(&patch, &cwd) } else { apply_patch::apply_patch(&patch, &cwd) }
    })
    .await
    .map_err(rt_err)?;
    match r {
        Ok(changes) => build_patch_result(&lua, changes),
        Err(e) => nil_err(&lua, e),
    }
}

fn parse_patch_paths(lua: &Lua, patch: &str, resolve_cwd: Option<&Path>) -> mlua::Result<MultiValue> {
    let hunks = match apply_patch::parse_patch(patch) {
        Ok(hunks) if !hunks.is_empty() => hunks,
        Ok(_) => return nil_err(lua, "patch must contain at least one file"),
        Err(err) => return nil_err(lua, err),
    };
    let result = lua.create_table()?;
    for (index, hunk) in hunks.into_iter().enumerate() {
        let (kind, path, move_to) = match hunk {
            apply_patch::Hunk::Add { path, .. } => ("add", path, None),
            apply_patch::Hunk::Delete { path } => ("delete", path, None),
            apply_patch::Hunk::Update { path, move_to, .. } => ("update", path, move_to),
        };
        let format_path = |p: &Path| {
            let raw = p.to_str().filter(|s| !s.is_empty() && !s.contains('\0'))?;
            match resolve_cwd {
                Some(cwd) => {
                    let p = Path::new(raw);
                    let raw_abs = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
                    let abs: PathBuf = raw_abs.components().collect();
                    abs.to_str().map(str::to_owned)
                }
                None => Some(raw.to_owned()),
            }
        };
        let Some(path) = format_path(&path) else {
            return nil_err(lua, "patch contains an invalid path");
        };
        let move_to = match move_to {
            Some(path) => match format_path(&path) {
                Some(path) => Some(path),
                None => return nil_err(lua, "patch contains an invalid destination path"),
            },
            None => None,
        };
        let item = lua.create_table()?;
        item.set("kind", kind)?;
        item.set("path", path)?;
        item.set("move_to", move_to)?;
        result.set(index + 1, item)?;
    }
    Ok(MultiValue::from_vec(vec![Value::Table(result)]))
}

pub(super) fn register_edit_ops(lua: &Lua, t: &Table, cwd: PathBuf) -> mlua::Result<()> {
    // Heavy edit/diff work runs on the blocking pool so the single-threaded runtime (TUI,
    // streaming, agent loop) never stalls on large files. These are async: call them from
    // coroutines (tools, listeners, spawned tasks).
    let c = cwd.clone();
    t.set(
        "patch_paths",
        lua.create_function(move |lua, (patch, opts): (String, Option<Table>)| {
            let resolve = match opts {
                Some(o) => o.get::<Option<bool>>("resolve")?.unwrap_or(false),
                None => false,
            };
            parse_patch_paths(lua, &patch, resolve.then_some(c.as_path()))
        })?,
    )?;
    t.set(
        "replace",
        lua.create_async_function(|lua, (s, old, new, all): (String, String, String, Option<bool>)| async move {
            let r = tokio::task::spawn_blocking(move || replace::replace(&s, &old, &new, all.unwrap_or(false)))
                .await
                .map_err(rt_err)?;
            match r {
                Ok(v) => Ok(MultiValue::from_vec(vec![Value::String(lua.create_string(&v)?)])),
                Err(e) => nil_err(&lua, e),
            }
        })?,
    )?;
    // apply_patch(patch, {dry_run}) -> {changes={{path, kind, move_to, old, new}}, summary} | nil, err
    t.set(
        "apply_patch",
        lua.create_async_function(move |lua, (patch, o): (String, Option<Table>)| {
            let c = cwd.clone();
            // captured before any await: the tool execution context of the caller
            let active = crate::exec_ctx::active(&lua);
            async move { apply_patch_exec(lua, patch, o, c, active).await }
        })?,
    )?;
    // diff(old, new, {context=3, path}) -> unified diff text, added, removed, lines (async,
    // blocking pool). One diff serves all four: `added` / `removed` count exactly like
    // `diff_stats`, `lines` is the longer side's line count (`ui.Diff{unified=, lines=}` sizes
    // its gutter with it) — an edit result stores these instead of both texts.
    t.set(
        "diff",
        lua.create_async_function(|_, (old, new, o): (String, String, Option<Table>)| async move {
            let ctx = o.as_ref().and_then(|o| o.get::<Option<usize>>("context").ok().flatten()).unwrap_or(3);
            let path = o.as_ref().and_then(|o| o.get::<Option<String>>("path").ok().flatten()).unwrap_or_default();
            tokio::task::spawn_blocking(move || {
                let e = mare_edit::unified_edit(&old, &new, ctx, &path);
                (e.text, e.added, e.removed, e.lines)
            })
            .await
            .map_err(rt_err)
        })?,
    )?;
    t.set(
        "diff_stats",
        lua.create_async_function(|_, (old, new): (String, String)| async move {
            tokio::task::spawn_blocking(move || mare_edit::diff_stats(&old, &new)).await.map_err(rt_err)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn check_patch_policy_enforces_write_policy_for_updates_and_moves() {
        let dir = std::env::temp_dir().join(format!("mare-edit-policy-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\n").unwrap();

        let lua = Lua::new();
        let spec: Value = lua.load("return { level = 'workspace-write' }").eval().unwrap();
        let policy = crate::ossandbox::Policy::from_lua(&lua, &spec, &dir).unwrap().unwrap();
        let active = crate::exec_ctx::Active { ctx: lua.create_table().unwrap(), policy, cwd: dir.clone() };
        let patch = "*** Begin Patch\n*** Update File: a.txt\n*** Move to: b.txt\n@@\n-hello\n+world\n*** End Patch\n";
        check_patch_policy(&lua, Some(active), patch, &dir, false).await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
