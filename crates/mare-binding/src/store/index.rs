use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use mlua::{Lua, Table, Value};

use mare_shared::lua::{json_to_lua, rt_err};
use mare_store::index::{Kinds, Query};

use super::{host_only, sessions_root};

static WARNED_UNSUPPORTED_SESSIONS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn take_unwarned_unsupported(paths: Vec<String>) -> Vec<String> {
    let mut warned = WARNED_UNSUPPORTED_SESSIONS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    paths.into_iter().filter(|path| warned.insert(path.clone())).collect()
}

/// The session index, built from session files on first use.
#[cfg(test)]
pub(super) static BEFORE_INDEX_WORK: Mutex<Option<Box<dyn FnOnce() + Send>>> = Mutex::new(None);

fn ready_index(root: &Path) -> anyhow::Result<mare_store::index::Shared> {
    #[cfg(test)]
    if let Some(hook) = BEFORE_INDEX_WORK.lock().unwrap().take() {
        hook();
    }
    mare_store::index::ready(root)
}

/// `index_read` options: scope = "all" | "cwd" | "project" (default "all"; `cwd` defaults to
/// `mare.cwd`), archived = false | true | "any" (default false), kind = "session" | "main" |
/// "fork" | "agent" | "any" (default "session": main sessions and forks), nonempty, session_id,
/// session_id_prefix, parent_session_id, limit, offset.
fn query_of(lua: &Lua, opts: Option<Table>) -> mlua::Result<Query> {
    let mut q = Query { archived: Some(false), ..Default::default() };
    let Some(o) = opts else { return Ok(q) };
    let s = |k: &str| o.get::<Option<String>>(k);
    let cwd = match s("cwd")? {
        Some(c) => c,
        None => lua.globals().get::<Table>("mare")?.get::<Option<String>>("cwd")?.unwrap_or_default(),
    };
    match s("scope")?.as_deref() {
        None | Some("all") => {}
        Some("cwd") => q.cwd = Some(cwd),
        Some("project") => q.project_root = Some(mare_store::index::project_root(&cwd)),
        Some(other) => return Err(mlua::Error::runtime(format!("index_read: unknown scope {other:?}"))),
    }
    q.archived = match o.get::<Value>("archived")? {
        Value::Nil | Value::Boolean(false) => Some(false),
        Value::Boolean(true) => Some(true),
        Value::String(x) if x.to_str()? == "any" => None,
        other => return Err(mlua::Error::runtime(format!("index_read: archived = {other:?}"))),
    };
    q.kinds = match s("kind")?.as_deref() {
        None | Some("session") => Kinds::Sessions,
        Some("main") => Kinds::Main,
        Some("fork") => Kinds::Fork,
        Some("agent") => Kinds::Agent,
        Some("any") => Kinds::Any,
        Some(other) => return Err(mlua::Error::runtime(format!("index_read: unknown kind {other:?}"))),
    };
    q.nonempty = o.get::<Option<bool>>("nonempty")?.unwrap_or(false);
    (q.session_id, q.session_id_prefix, q.parent_session_id) =
        (s("session_id")?, s("session_id_prefix")?, s("parent_session_id")?);
    q.limit = o.get::<Option<i64>>("limit")?;
    q.offset = o.get::<Option<i64>>("offset")?.unwrap_or(0);
    Ok(q)
}

fn rows_to_lua(lua: &Lua, rows: &[mare_store::index::Row]) -> mlua::Result<Value> {
    json_to_lua(lua, &serde_json::Value::Array(rows.iter().map(|r| r.to_json()).collect()))
}

pub(super) fn install_index_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // index_append(entry) -> row | nil: after appending to a session file, bring its index row
    // up to date with the file ({path, cwd?}: every field comes from the file; `cwd` is the
    // directory a live writer runs in). {session_id, path, deleted = true}: the session's files were
    // deleted, drop its row (and its sub-agents'). Errors are the caller's to log: the session
    // file stays the source of truth and a reconcile repairs the row.
    t.set(
        "index_append",
        lua.create_async_function(|lua, entry: Table| {
            let allowed = host_only(&lua, "index_append");
            async move {
                allowed?;
                let path: Option<String> = entry.get("path")?;
                let session_id: Option<String> = entry.get("session_id")?;
                let cwd: Option<String> = entry.get("cwd")?;
                let deleted = entry.get::<Option<bool>>("deleted")?.unwrap_or(false);
                let root = sessions_root();
                let row = crate::blocking::spawn(&lua, move || {
                    let ix = mare_store::index::shared(&root)?;
                    let mut ix = ix.lock().unwrap_or_else(|e| e.into_inner());
                    if deleted {
                        ix.remove(session_id.as_deref(), path.as_deref().map(Path::new))?;
                        return anyhow::Ok(None);
                    }
                    let path = path.ok_or_else(|| anyhow::anyhow!("index_append: no path"))?;
                    ix.update(Path::new(&path), cwd.as_deref())
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                match row {
                    Some(r) => json_to_lua(&lua, &r.to_json()),
                    None => Ok(Value::Nil),
                }
            }
        })?,
    )?;
    // index_read(opts?) -> rows, total, unsupported paths. Check changed files on this page so
    // an unsupported session cannot remain selectable through a stale index row.
    t.set(
        "index_read",
        lua.create_async_function(|lua, opts: Option<Table>| {
            let allowed = host_only(&lua, "index_read");
            async move {
                allowed?;
                let q = query_of(&lua, opts)?;
                // The normal index read may build or repair the SQLite database. Diagnostics
                // after poison must scan files without initiating a new write.
                let scan_only = crate::exec_gate::poison_reason(&lua).is_some();
                let root = sessions_root();
                let (rows, total, unsupported) = crate::blocking::spawn(&lua, move || {
                    if scan_only {
                        return mare_store::index::scan_list_with_unsupported(&root, &q);
                    }
                    match ready_index(&root) {
                        Ok(ix) => {
                            let mut index = ix.lock().unwrap_or_else(|e| e.into_inner());
                            let (rows, total) = index.list_checked(&q)?;
                            Ok((rows, total, index.unsupported()))
                        }
                        Err(e) => {
                            let detail = format!("{e:#}");
                            if mare_store::index::must_report_error(&e) {
                                anyhow::bail!("{detail}");
                            }
                            tracing::warn!("session index unavailable, scanning session files: {detail}");
                            mare_store::index::scan_list_with_unsupported(&root, &q)
                        }
                    }
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                Ok((rows_to_lua(&lua, &rows)?, total, unsupported))
            }
        })?,
    )?;
    // index_warn_unsupported(paths) -> paths not yet warned in this process; list and refresh
    // share this set so the same unsupported session isn't announced twice through two routes.
    t.set(
        "index_warn_unsupported",
        lua.create_function(|lua, paths: Vec<String>| {
            host_only(lua, "index_warn_unsupported")?;
            Ok(take_unwarned_unsupported(paths))
        })?,
    )?;
    // index_reconcile() -> {files, updated, removed, failed, pending}: make the index agree with the
    // session files (stat only for unchanged ones); the session plugin runs it in the
    // background at startup
    t.set(
        "index_reconcile",
        lua.create_async_function(|lua, ()| {
            let allowed = host_only(&lua, "index_reconcile");
            async move {
                allowed?;
                let root = sessions_root();
                let r = crate::blocking::spawn(&lua, move || mare_store::index::reconcile(&ready_index(&root)?))?
                    .await
                    .map_err(rt_err)?
                    .map_err(rt_err)?;
                let t = lua.create_table()?;
                t.set("files", r.files)?;
                t.set("updated", r.updated)?;
                t.set("removed", r.removed)?;
                t.set("failed", r.failed)?;
                t.set("pending", r.pending)?;
                t.set("unsupported", r.unsupported)?;
                Ok(t)
            }
        })?,
    )?;
    // index_notice() -> reason | nil: why the index had to be rebuilt in this process (corrupt,
    // schema version change, file missing), handed out once — the caller shows one notice.
    // Opens the database if needed; reads no session file and builds nothing.
    t.set(
        "index_notice",
        lua.create_function(|lua, ()| {
            host_only(lua, "index_notice")?;
            if crate::exec_gate::poison_reason(lua).is_some() {
                return Ok(None);
            }
            let ix = mare_store::index::shared(&sessions_root()).map_err(rt_err)?;
            let why = ix.lock().unwrap_or_else(|e| e.into_inner()).take_notice();
            Ok(why)
        })?,
    )?;
    // index_rebuild() -> row count: every row re-derived from the session files (order and
    // checkpoints kept)
    t.set(
        "index_rebuild",
        lua.create_async_function(|lua, ()| {
            let allowed = host_only(&lua, "index_rebuild");
            async move {
                allowed?;
                let root = sessions_root();
                crate::blocking::spawn(&lua, move || {
                    ready_index(&root)?.lock().unwrap_or_else(|e| e.into_inner()).rebuild()
                })?
                .await
                .map_err(rt_err)?
                .map_err(rt_err)
            }
        })?,
    )?;
    Ok(())
}
