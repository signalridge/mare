//! Public recovery storage with owned local backends and best-effort age maintenance.
pub(crate) mod local;

use mare_shared::lua::rt_err;
mod bytes;
use mlua::{Lua, Table};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
fn owner_key(owner: &str) -> String {
    digest(owner.as_bytes())
}
fn current_session_owner(lua: &Lua) -> Option<String> {
    if let Some(ctx) = mare_shared::exec_ctx::current(lua)
        && let mlua::Value::String(s) = mare_shared::exec_ctx::field(&ctx, "session_id")
        && let Ok(sid) = s.to_str()
        && !sid.is_empty()
    {
        return Some(sid.to_owned());
    }
    crate::peers::session_cell(lua).get().filter(|s| !s.is_empty())
}
pub(super) fn resolve_read_or_delete_owner(lua: &Lua, opts: Option<&Table>) -> Result<String, (&'static str, String)> {
    let in_exec_ctx = mare_shared::exec_ctx::current(lua).is_some();
    let active_owner = current_session_owner(lua);
    if let Some(o) = opts {
        match o.get::<mlua::Value>("session_id") {
            Ok(mlua::Value::Nil) => {}
            Ok(mlua::Value::String(s)) => {
                let sid = s.to_str().map_err(|e| ("invalid_arguments", e.to_string()))?;
                if sid.is_empty() {
                    return Err(("invalid_arguments", "spill session_id must be non-empty".into()));
                }
                if in_exec_ctx && active_owner.as_deref() != Some(sid.as_ref()) {
                    return Err(("denied", "spill session_id override is not allowed inside a confined call".into()));
                }
                return Ok(sid.to_owned());
            }
            Ok(_) => return Err(("invalid_arguments", "spill session_id must be a string".into())),
            Err(e) => return Err(("invalid_arguments", e.to_string())),
        }
    }
    active_owner.ok_or_else(|| ("spill.unknown_spill", "spill requires an active session".into()))
}
pub(super) fn parse_spill_args(
    args: mlua::MultiValue,
    what: &str,
    allow_opts: bool,
) -> Result<(String, Option<Table>), (&'static str, String)> {
    let mut iter = args.into_iter();
    let Some(mlua::Value::String(first)) = iter.next() else {
        return Err(("invalid_arguments", format!("{what} requires a spill_id string")));
    };
    let spill_id = first.to_str().map_err(|e| ("invalid_arguments", e.to_string()))?.to_owned();
    let opts = match iter.next() {
        None | Some(mlua::Value::Nil) => None,
        Some(mlua::Value::Table(t)) if allow_opts => Some(t),
        Some(_) => {
            return Err((
                "invalid_arguments",
                if allow_opts { format!("{what} takes (spill_id, opts?)") } else { format!("{what} takes (spill_id)") },
            ));
        }
    };
    if iter.next().is_some() {
        return Err(("invalid_arguments", format!("{what}: too many positional arguments")));
    }
    Ok((spill_id, opts))
}
pub(super) fn opt_nonneg_u64(opts: Option<&Table>, key: &str) -> Result<Option<u64>, (&'static str, String)> {
    let Some(o) = opts else {
        return Ok(None);
    };
    match o.get::<mlua::Value>(key).map_err(|e| ("invalid_arguments", e.to_string()))? {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Integer(n) if n >= 0 => Ok(Some(n as u64)),
        _ => Err(("invalid_arguments", format!("spill option `{key}` must be a non-negative integer"))),
    }
}
pub(super) fn opt_nonneg_usize(opts: Option<&Table>, key: &str) -> Result<Option<usize>, (&'static str, String)> {
    match opt_nonneg_u64(opts, key)? {
        None => Ok(None),
        Some(n) => usize::try_from(n)
            .map(Some)
            .map_err(|_| ("invalid_arguments", format!("spill option `{key}` is out of range"))),
    }
}
pub(super) fn coded_result(lua: &Lua, code: &str, message: &str) -> mlua::Result<(bool, mlua::Value)> {
    let err = mare_shared::lua_error::error_value(lua, code, message, mlua::Value::Nil)?;
    Ok((false, mlua::Value::Table(err)))
}
pub(super) fn wrap_coded(lua: &Lua, inner: mlua::Function) -> mlua::Result<mlua::Function> {
    lua.load("local f = ... return function(...) local ok, v = f(...) if not ok then error(v, 0) end return v end")
        .set_name("@spill/wrap")
        .call(inner)
}
fn encode_segment(raw: &str) -> String {
    if raw.is_empty() {
        return "~".to_owned();
    }
    if raw == "." {
        return "~002E".to_owned();
    }
    if raw == ".." {
        return "~002E~002E".to_owned();
    }
    let mut out = String::new();
    for unit in raw.encode_utf16() {
        let token = match u8::try_from(unit) {
            Ok(b) if b != b'~' && (b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')) => {
                String::from(b as char)
            }
            _ => format!("~{unit:04X}"),
        };
        if out.len() + token.len() > 120 {
            break;
        }
        out.push_str(&token);
    }
    if out.is_empty() { "~".to_owned() } else { out }
}
pub(super) fn valid_stem(stem: &str) -> bool {
    if uuid::Uuid::parse_str(stem).is_ok_and(|value| value.to_string() == stem) {
        return true;
    }
    if stem.len() <= 37 || !stem.is_char_boundary(36) || stem.as_bytes()[36] != b'-' {
        return false;
    }
    let uuid_part = &stem[..36];
    let seg = &stem[37..];
    uuid::Uuid::parse_str(uuid_part).is_ok_and(|value| value.to_string() == uuid_part)
        && !seg.is_empty()
        && seg.len() <= 160
        && seg != "."
        && seg != ".."
        && seg.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~'))
}
fn stem_for(suggested_name: Option<&str>) -> String {
    let id = uuid::Uuid::new_v4();
    let Some(raw) = suggested_name else {
        return id.to_string();
    };
    let base = raw.strip_suffix(".txt").unwrap_or(raw);
    format!("{id}-{}", encode_segment(base))
}
fn path_for_spill_id(base: &Path, owner: &str, spill_id: &str) -> Result<PathBuf, (&'static str, String)> {
    let (key, id) = spill_id.split_once(':').ok_or_else(|| ("invalid_arguments", "invalid spill_id".into()))?;
    if !valid_stem(id) || key.len() != 64 || !key.bytes().all(|c| c.is_ascii_hexdigit()) {
        return Err(("invalid_arguments", "invalid spill_id".into()));
    }
    if key != owner_key(owner) {
        return Err(("spill.unknown_spill", "spill object does not belong to this session".into()));
    }
    Ok(base.join(key).join(format!("{id}.txt")))
}
fn root() -> PathBuf {
    mare_shared::paths::state_dir().join("spill-results")
}
fn plain_dir(path: &Path) -> std::io::Result<()> {
    mare_shared::private::create_dir_all(path)?;
    if fs::symlink_metadata(path)?.file_type().is_symlink() || !fs::metadata(path)?.is_dir() {
        return Err(std::io::Error::other("spill directory is not a plain directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = fs::metadata(path)?;
        // Refuse an existing public/unowned directory instead of exposing recovery text.
        // SAFETY: geteuid takes no pointers and has no preconditions.
        let uid = unsafe { libc::geteuid() };
        if metadata.permissions().mode() & 0o077 != 0 || metadata.uid() != uid {
            return Err(std::io::Error::other("spill directory must be private and owned by this user"));
        }
    }
    #[cfg(windows)]
    mare_shared::private::check(path).map_err(std::io::Error::other)?;
    Ok(())
}
fn validate(base: &Path, path: &Path) -> mlua::Result<()> {
    let parent = path.parent().ok_or_else(|| rt_err("invalid spill path"))?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let owner = parent.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if parent.parent() != Some(base)
        || owner.len() != 64
        || !owner.bytes().all(|c| c.is_ascii_hexdigit())
        || !name.ends_with(".txt")
        || !valid_stem(name.trim_end_matches(".txt"))
    {
        return Err(rt_err("not a managed spill path"));
    }
    local::trusted_root(base).map_err(rt_err)?;
    local::trusted_directory(parent, true).map_err(rt_err)?;
    for p in [base, parent, path] {
        if fs::symlink_metadata(p).map_err(rt_err)?.file_type().is_symlink() {
            return Err(rt_err("spill symlinks are refused"));
        }
    }
    let metadata = fs::metadata(path).map_err(rt_err)?;
    if !metadata.is_file() {
        return Err(rt_err("spill must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no preconditions.
        if metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(rt_err("spill files must be private, user owned and have one link"));
        }
    }
    Ok(())
}
fn check(lua: &Lua, path: &Path, read: bool) -> mlua::Result<()> {
    let active = crate::exec_ctx::active(lua)?;
    if read && let Some(a) = &active {
        return crate::exec_ctx::check_with(lua, a, "read", &path.to_string_lossy());
    }
    let policy =
        active.map(|a| a.policy).or_else(|| crate::ossandbox::enforce_ceiling(lua, crate::ossandbox::current(lua)));
    if let Some(p) = policy {
        let allowed = if read { p.can_read(path) } else { p.can_write_deny_read(path) };
        if !allowed {
            return Err(rt_err("sandbox: spill access denied"));
        }
    }
    Ok(())
}
#[cfg(test)]
fn save(base: &Path, owner: &str, text: &[u8]) -> mlua::Result<PathBuf> {
    save_with_name(base, owner, text, None)
}
fn save_with_name(base: &Path, owner: &str, text: &[u8], suggested_name: Option<&str>) -> mlua::Result<PathBuf> {
    if owner.is_empty() {
        return Err(rt_err("spill requires a session id"));
    }
    plain_dir(base).map_err(rt_err)?;
    local::trusted_root(base).map_err(rt_err)?;
    let dir = base.join(owner_key(owner));
    plain_dir(&dir).map_err(rt_err)?;
    let path = dir.join(format!("{}.txt", stem_for(suggested_name)));
    let mut file = mare_shared::private::create_new(&path).map_err(rt_err)?;
    let written = file.write_all(text).and_then(|()| file.sync_all());
    #[cfg(unix)]
    let written = written.and_then(|()| fs::File::open(&dir)?.sync_all());
    if let Err(e) = written {
        let _ = fs::remove_file(&path);
        return Err(rt_err(e));
    }
    Ok(path)
}
fn open_read(path: &Path) -> mlua::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = opts.open(path).map_err(rt_err)?;
    let metadata = file.metadata().map_err(rt_err)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(rt_err("spill must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(rt_err("spill hardlinks are refused"));
        }
    }
    Ok(file)
}
fn page(path: &Path, offset: usize, limit: usize, cursor: u64) -> Result<(String, bool, u64), (&'static str, String)> {
    if offset == 0 || limit == 0 || limit > 2000 {
        return Err(("invalid_arguments", "spill read requires offset >= 1 and limit 1..2000".into()));
    }
    let file = open_read(path).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(cursor)).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
    let mut position = cursor;
    let mut out = Vec::new();
    let mut line = 1;
    // Scan with bounded buffers even when the file contains a single very long line.
    loop {
        let chunk = reader.fill_buf().map_err(|e| ("spill.unknown_spill", e.to_string()))?;
        if chunk.is_empty() {
            let text = String::from_utf8(out).map_err(|e| ("invalid_arguments", e.to_string()))?;
            return Ok((text, false, position));
        }
        let mut used = 0;
        for &b in chunk {
            if line >= offset {
                if line >= offset.saturating_add(limit) || out.len() >= 50 * 1024 {
                    let valid = match std::str::from_utf8(&out) {
                        Ok(_) => out.len(),
                        Err(e) if e.error_len().is_none() => e.valid_up_to(),
                        Err(_) => return Err(("invalid_arguments", "spill cursor must be a UTF-8 boundary".into())),
                    };
                    let next = position + used as u64 - (out.len() - valid) as u64;
                    out.truncate(valid);
                    return Ok((String::from_utf8_lossy(&out).into_owned(), true, next));
                }
                out.push(b);
            }
            used += 1;
            if b == b'\n' {
                line += 1;
            }
        }
        reader.consume(used);
        position += used as u64;
    }
}
/// A canceled save must not leak an unadvertised file after its blocking worker finishes.
struct PendingFile(Option<PathBuf>);
impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_file(path);
        }
    }
}
fn current_scope(lua: &Lua, opts: &Option<Table>) -> mlua::Result<local::Scope> {
    let policy = crate::exec_ctx::active(lua)?
        .map(|a| a.policy)
        .or_else(|| crate::ossandbox::enforce_ceiling(lua, crate::ossandbox::current(lua)));
    let context = mare_shared::exec_ctx::current(lua);
    if let Some(opts) = opts
        && !opts.get::<mlua::Value>("cancel")?.is_nil()
        && crate::task::opt_token(&Some(opts.clone())).is_none()
    {
        return Err(rt_err("spill cancel must be a cancellation token"));
    }
    let cancel = crate::task::opt_token(opts).or_else(|| crate::task::opt_token(&context));
    Ok(local::Scope { policy, cancel })
}

#[derive(Clone, Debug, Default)]
pub(super) struct SaveOpts {
    pub suggested_name: Option<String>,
    pub source: Option<SaveSource>,
}

#[derive(Clone, Debug)]
pub(super) enum SaveSource {
    Tool { tool_name: String, tool_call_id: String, label: String },
    SessionReference { session_id: String, label: String },
}

fn non_empty_string(tbl: &Table, key: &str, what: &str) -> mlua::Result<String> {
    let val = tbl.get::<Option<String>>(key)?.ok_or_else(|| rt_err(format!("{what}: missing {key}")))?;
    if val.is_empty() {
        return Err(rt_err(format!("{what}: empty {key}")));
    }
    Ok(val)
}

fn parse_source(tbl: &Table) -> mlua::Result<SaveSource> {
    let kind = non_empty_string(tbl, "kind", "invalid spill source")?;
    match kind.as_str() {
        "tool" => {
            for item in tbl.clone().pairs::<String, mlua::Value>() {
                let (k, _) = item?;
                if !matches!(k.as_str(), "kind" | "tool_name" | "tool_call_id" | "label") {
                    return Err(rt_err(format!("unknown spill tool source field: {k}")));
                }
            }
            let tool_name = non_empty_string(tbl, "tool_name", "invalid spill tool source")?;
            let tool_call_id = non_empty_string(tbl, "tool_call_id", "invalid spill tool source")?;
            let label = match tbl.get::<Option<String>>("label")? {
                Some(s) if s.is_empty() => return Err(rt_err("invalid spill tool source: empty label")),
                Some(s) => s,
                None => "result".to_owned(),
            };
            Ok(SaveSource::Tool { tool_name, tool_call_id, label })
        }
        "session-reference" => {
            for item in tbl.clone().pairs::<String, mlua::Value>() {
                let (k, _) = item?;
                if !matches!(k.as_str(), "kind" | "session_id" | "label") {
                    return Err(rt_err(format!("unknown spill session-reference source field: {k}")));
                }
            }
            let session_id = non_empty_string(tbl, "session_id", "invalid spill session-reference source")?;
            let label = non_empty_string(tbl, "label", "invalid spill session-reference source")?;
            Ok(SaveSource::SessionReference { session_id, label })
        }
        _ => Err(rt_err(format!("invalid spill source kind: {kind}"))),
    }
}

pub(super) fn parse_save_opts(opts: Option<&Table>) -> mlua::Result<SaveOpts> {
    let Some(opts) = opts else {
        return Ok(SaveOpts::default());
    };
    for item in opts.clone().pairs::<String, mlua::Value>() {
        let (key, _) = item?;
        if !matches!(key.as_str(), "suggested_name" | "source" | "backend") {
            return Err(rt_err(format!("unknown spill save option: {key}")));
        }
    }
    let suggested_name = opts.get::<Option<String>>("suggested_name")?;
    let source = match opts.get::<mlua::Value>("source")? {
        mlua::Value::Nil => None,
        mlua::Value::Table(t) => Some(parse_source(&t)?),
        _ => return Err(rt_err("spill source must be a table")),
    };
    Ok(SaveOpts { suggested_name, source })
}

async fn save_value(
    lua: &Lua,
    store: &local::Store,
    owner: String,
    bytes: Vec<u8>,
    opts: SaveOpts,
) -> mlua::Result<(bool, mlua::Value)> {
    let _lease = store.lease().await?;
    let scope = current_scope(lua, &None)?;
    if scope.cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err(rt_err("spill owner was canceled"));
    }
    let base = store.config.root.clone();
    if let Err(e) = check(lua, &base, false) {
        return coded_result(lua, "denied", &e.to_string());
    }
    // A saved spill_id is only advertised when the current policy can retrieve it.
    let policy = crate::exec_ctx::active(lua)?
        .map(|a| a.policy)
        .or_else(|| crate::ossandbox::enforce_ceiling(lua, crate::ossandbox::current(lua)));
    if policy.is_some_and(|p| !p.can_read(&base)) {
        return coded_result(lua, "denied", "sandbox: recovery storage is not readable under the current policy");
    }
    store.start(scope.clone()).await;
    let size = bytes.len();
    let key = owner_key(&owner);
    let suggested_name = opts.suggested_name.clone();
    let mut pending = tokio::task::spawn_blocking(move || {
        save_with_name(&base, &owner, &bytes, suggested_name.as_deref())
            .map(|path| PendingFile(Some(path)))
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(rt_err)?
    .map_err(rt_err)?;
    if scope.cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
        return Err(rt_err("spill owner was canceled"));
    }
    let path = pending.0.as_ref().ok_or_else(|| rt_err("spill write did not produce a file"))?;
    let path_str = path.to_string_lossy();
    let result = lua.create_table()?;
    result.set("path", path_str.as_ref())?;
    result.set("size", size)?;
    result.set("spill_id", format!("{}:{}", key, path.file_stem().unwrap_or_default().to_string_lossy()))?;
    result.set(
        "retrieval_hint",
        format!("Use read_spill with offset/limit, or read (offset/limit) / grep at {path_str}."),
    )?;
    if let Some(src) = opts.source {
        let s = lua.create_table()?;
        match src {
            SaveSource::Tool { tool_name, tool_call_id, label } => {
                s.set("kind", "tool")?;
                s.set("tool_name", tool_name)?;
                s.set("tool_call_id", tool_call_id)?;
                s.set("label", label)?;
            }
            SaveSource::SessionReference { session_id, label } => {
                s.set("kind", "session-reference")?;
                s.set("session_id", session_id)?;
                s.set("label", label)?;
            }
        }
        result.set("source", s)?;
    }
    pending.0.take();
    Ok((true, mlua::Value::Table(result)))
}
fn store_api(lua: &Lua, store: std::sync::Arc<local::Store>) -> mlua::Result<Table> {
    let api = lua.create_table()?;
    api.set("identity", digest(store.config.root.as_os_str().as_encoded_bytes()))?;
    let save_store = store.clone();
    let save_raw =
        lua.create_async_function(move |lua, (owner, text, opts): (String, mlua::LuaString, Option<Table>)| {
            let store = save_store.clone();
            async move {
                let bytes = text.as_bytes().to_vec();
                if let Err(e) = std::str::from_utf8(&bytes) {
                    return coded_result(&lua, "invalid_arguments", &e.to_string());
                }
                let save_opts = match parse_save_opts(opts.as_ref()) {
                    Ok(v) => v,
                    Err(e) => return coded_result(&lua, "invalid_arguments", &e.to_string()),
                };
                save_value(&lua, &store, owner, bytes, save_opts).await
            }
        })?;
    api.set("save", wrap_coded(lua, save_raw)?)?;
    let read_store = store.clone();
    let read_raw = lua.create_async_function(move |lua, args: mlua::MultiValue| {
        let store = read_store.clone();
        async move {
            let (spill_id, opts) = match parse_spill_args(args, "mare.spill.read", true) {
                Ok(v) => v,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if let Some(o) = &opts {
                for item in o.clone().pairs::<String, mlua::Value>() {
                    let (key, _) = item?;
                    if !matches!(key.as_str(), "offset" | "limit" | "cursor" | "session_id") {
                        return coded_result(&lua, "invalid_arguments", &format!("unknown spill read option: {key}"));
                    }
                }
            }
            let offset = match opt_nonneg_usize(opts.as_ref(), "offset") {
                Ok(v) => v.unwrap_or(1),
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let limit = match opt_nonneg_usize(opts.as_ref(), "limit") {
                Ok(v) => v.unwrap_or(200),
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let cursor = match opt_nonneg_u64(opts.as_ref(), "cursor") {
                Ok(v) => v.unwrap_or(0),
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if offset == 0 || limit == 0 || limit > 2000 {
                return coded_result(&lua, "invalid_arguments", "spill read requires offset >= 1 and limit 1..2000");
            }
            let owner = match resolve_read_or_delete_owner(&lua, opts.as_ref()) {
                Ok(owner) => owner,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let _lease = store.lease().await?;
            let base = store.config.root.clone();
            let path = match path_for_spill_id(&base, &owner, &spill_id) {
                Ok(p) => p,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if let Err(e) = check(&lua, &path, true) {
                return coded_result(&lua, "denied", &e.to_string());
            }
            let paged = tokio::task::spawn_blocking(move || {
                validate(&base, &path).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
                page(&path, offset, limit, cursor)
            })
            .await
            .map_err(rt_err)?;
            let (text, more, next_byte) = match paged {
                Ok(v) => v,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let out = lua.create_table()?;
            out.set("content", text)?;
            out.set("more", more)?;
            out.set("next_byte", next_byte)?;
            Ok((true, mlua::Value::Table(out)))
        }
    })?;
    api.set("read", wrap_coded(lua, read_raw)?)?;
    let delete_store = store.clone();
    let delete_raw = lua.create_async_function(move |lua, args: mlua::MultiValue| {
        let store = delete_store.clone();
        async move {
            let (spill_id, opts) = match parse_spill_args(args, "mare.spill.delete", true) {
                Ok(v) => v,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if let Some(o) = &opts {
                for item in o.clone().pairs::<String, mlua::Value>() {
                    let (key, _) = item?;
                    if key != "session_id" {
                        return coded_result(&lua, "invalid_arguments", &format!("unknown spill delete option: {key}"));
                    }
                }
            }
            let owner = match resolve_read_or_delete_owner(&lua, opts.as_ref()) {
                Ok(owner) => owner,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            let _lease = store.lease().await?;
            let base = store.config.root.clone();
            let path = match path_for_spill_id(&base, &owner, &spill_id) {
                Ok(p) => p,
                Err((code, msg)) => return coded_result(&lua, code, &msg),
            };
            if let Err(e) = check(&lua, &path, false) {
                return coded_result(&lua, "denied", &e.to_string());
            }
            let removed = tokio::task::spawn_blocking(move || {
                validate(&base, &path).map_err(|e| ("spill.unknown_spill", e.to_string()))?;
                fs::remove_file(path).map_err(|e| ("spill.unknown_spill", e.to_string()))
            })
            .await
            .map_err(rt_err)?;
            if let Err((code, msg)) = removed {
                return coded_result(&lua, code, &msg);
            }
            Ok((true, mlua::Value::Nil))
        }
    })?;
    api.set("delete", wrap_coded(lua, delete_raw)?)?;
    bytes::install(lua, &api, store.clone())?;
    api.set(
        "dispose",
        lua.create_async_function(move |lua, ()| {
            let store = store.clone();
            async move {
                check(&lua, &store.config.root, false)?;
                let report = store.dispose().await?;
                let out = lua.create_table()?;
                out.set("removed", report.removed)?;
                out.set("warnings", report.warnings)?;
                Ok(out)
            }
        })?,
    )?;
    Ok(api)
}

pub fn install(lua: &Lua, mare: &Table, cwd: PathBuf) -> mlua::Result<()> {
    let default = local::Store::new(local::Config::resolve(None, &cwd, None).map_err(rt_err)?);
    let api = store_api(lua, default)?;
    // The default backend belongs to the VM; a plugin may only dispose objects it creates.
    api.raw_remove("dispose")?;
    api.set(
        "new_local",
        lua.create_async_function(move |lua, opts: Option<Table>| {
            let cwd = cwd.clone();
            async move {
                if let Some(opts) = &opts {
                    for item in opts.clone().pairs::<String, mlua::Value>() {
                        let (key, _) = item?;
                        if !matches!(key.as_str(), "root" | "cleanup_period_days" | "cancel") {
                            return Err(rt_err(format!("unknown local spill option: {key}")));
                        }
                    }
                }
                let root = opts.as_ref().map(|o| o.get::<Option<String>>("root")).transpose()?.flatten();
                let days = opts.as_ref().map(|o| o.get::<Option<u32>>("cleanup_period_days")).transpose()?.flatten();
                let config = local::Config::resolve(root.as_deref().map(Path::new), &cwd, days).map_err(rt_err)?;
                check(&lua, &config.root, false)?;
                let scope = current_scope(&lua, &opts)?;
                if scope.cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
                    return Err(rt_err("spill owner was canceled"));
                }
                let store = local::Store::new(config);
                store.start(scope).await;
                store_api(&lua, store)
            }
        })?,
    )?;
    mare.set("spill", api)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn session_source_accepts_only_canonical_kind() {
        let lua = mlua::Lua::new();
        for (kind, expected) in [("session-reference", true), ("session_reference", false)] {
            let source = lua.create_table().unwrap();
            source.set("kind", kind).unwrap();
            source.set("session_id", "s").unwrap();
            source.set("label", "history").unwrap();
            assert_eq!(parse_source(&source).is_ok(), expected, "{kind}");
        }
    }
    #[test]
    fn private_recoverable_text() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("spill");
        let p = save(&base, "../session", "α\nbeta\ngamma\n".as_bytes()).unwrap();
        validate(&base, &p).unwrap();
        let q = save(&base, "../session", b"other").unwrap();
        assert_ne!(p, q);
        assert_eq!(page(&p, 2, 1, 0).unwrap(), ("beta\n".into(), true, 8));
        assert_eq!(page(&p, 3, 10, 0).unwrap(), ("gamma\n".into(), false, 14));
        assert!(validate(&base, &tmp.path().join("outside.txt")).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
            fs::remove_file(&p).unwrap();
            symlink(&q, &p).unwrap();
            assert!(validate(&base, &p).is_err());
        }
    }
    #[test]
    fn canceled_save_is_cleaned_up_and_spill_ids_are_session_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let path = save(&tmp.path().join("spill"), "owner", b"text").unwrap();
        let pending = PendingFile(Some(path.clone()));
        assert!(path.exists());
        drop(pending);
        assert!(!path.exists());
        let id = uuid::Uuid::new_v4();
        let spill_id = format!("{}:{id}", owner_key("owner"));
        assert!(path_for_spill_id(&root(), "owner", &spill_id).is_ok());
        assert_eq!(path_for_spill_id(&root(), "other-owner", &spill_id).unwrap_err().0, "spill.unknown_spill");
        assert_eq!(path_for_spill_id(&root(), "owner", "/outside").unwrap_err().0, "invalid_arguments");
    }
    #[test]
    fn reserved_notice_stays_within_utf8_budget() {
        let text = "αβ\n".repeat(10000);
        let (s, changed) = crate::text::retain(&text, 128, 5, "[read recovery]").unwrap();
        assert!(changed && s.ends_with("[read recovery]") && s.len() <= 128 && s.lines().count() <= 5);
        assert!(crate::text::retain(&text, 2, 2, "notice").is_err());
        let (short, _) = crate::text::retain("line\n", 50, 2, "notice").unwrap();
        assert_eq!(short, "line\nnotice");
    }
    #[test]
    fn utf8_cursor_recovers_a_giant_line_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let text = "界".repeat(40000);
        let path = save(&tmp.path().join("spill"), "s", text.as_bytes()).unwrap();
        let mut recovered = String::new();
        let mut cursor = 0;
        loop {
            let (s, more, next) = page(&path, 1, 1, cursor).unwrap();
            recovered.push_str(&s);
            if !more {
                break;
            }
            assert!(next > cursor);
            cursor = next;
        }
        assert_eq!(text, recovered);
        assert!(page(&path, 1, 1, 1).is_err());
    }
    #[test]
    fn giant_lines_are_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let p = save(&tmp.path().join("spill"), "s", &vec![b'a'; 100_000]).unwrap();
        let (s, more, cursor) = page(&p, 1, 1, 0).unwrap();
        let (tail, more_tail, _) = page(&p, 1, 1, cursor).unwrap();
        assert_eq!(s.len() + tail.len(), 100_000);
        assert!(!more_tail);
        assert_eq!(s.len(), 50 * 1024);
        assert!(more);
    }
    #[test]
    fn suggested_name_is_single_segment_unique_and_validated() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("spill");
        assert_eq!(encode_segment(""), "~");
        assert_eq!(encode_segment("."), "~002E");
        assert_eq!(encode_segment(".."), "~002E~002E");
        assert_eq!(encode_segment("../etc/passwd"), "..~002Fetc~002Fpasswd");
        let a = save_with_name(&base, "sess", b"one", Some("../web_fetch.txt")).unwrap();
        let b = save_with_name(&base, "sess", b"two", Some("../web_fetch.txt")).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.parent(), Some(base.join(owner_key("sess")).as_path()));
        validate(&base, &a).unwrap();
        validate(&base, &b).unwrap();
        let stem = a.file_stem().unwrap().to_str().unwrap();
        assert!(valid_stem(stem));
        assert!(!valid_stem("not-a-uuid-seg"));
        assert!(!valid_stem(&format!("{}-..", uuid::Uuid::new_v4())));
        assert!(!valid_stem(&format!("{}-a/b", uuid::Uuid::new_v4())));
        let spill_id = format!("{}:{stem}", owner_key("sess"));
        assert_eq!(path_for_spill_id(&base, "sess", &spill_id).unwrap(), a);
        assert_eq!(path_for_spill_id(&base, "other", &spill_id).unwrap_err().0, "spill.unknown_spill");
    }
}
