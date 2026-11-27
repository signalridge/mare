//! Lua bindings installation for `mare.fs`.

use std::path::{Path, PathBuf};

use mlua::{Lua, MultiValue, Table, Value};

use mare_shared::lua::rt_err;

use super::dir::{WalkOpts, build_glob, walk};
use super::read::{MAX_SNIFF_BYTES, open_regular_nonblocking, read_capped, read_window, sniff_kind};
use super::write::{Beneath, WriteOptions, anchored_mkdir_all, anchored_remove, anchored_rename, write_file};
use super::{mtime, resolve};

pub fn install(lua: &Lua, mare: &Table, cwd: PathBuf) -> mlua::Result<()> {
    let t = lua.create_table()?;
    install_path_ops(lua, &t, &cwd)?;
    super::path_glob::install(lua, &t, &cwd)?;
    install_read_ops(lua, &t, &cwd)?;
    install_write_ops(lua, &t, &cwd)?;
    install_dir_ops(lua, &t, &cwd)?;
    t.set("cwd", cwd.to_string_lossy().into_owned())?;
    t.set("home", dirs::home_dir().map(|p| p.to_string_lossy().into_owned()))?;
    t.set("sep", std::path::MAIN_SEPARATOR.to_string())?;
    mare.set("fs", t)?;
    Ok(())
}

fn opt<T: mlua::FromLua>(opts: Option<&Table>, key: &str) -> mlua::Result<Option<T>> {
    Ok(opts.map(|o| o.get::<Option<T>>(key)).transpose()?.flatten())
}

fn bool_opt(opts: Option<&Table>, key: &str) -> mlua::Result<bool> {
    Ok(opt::<bool>(opts, key)?.unwrap_or(false))
}

fn policy_cwd_opt(opts: Option<&Table>, fallback: &Path) -> mlua::Result<PathBuf> {
    Ok(opt::<String>(opts, "cwd")?.map(PathBuf::from).unwrap_or_else(|| fallback.to_path_buf()))
}

fn timestamp_ms(res: std::io::Result<std::time::SystemTime>) -> i64 {
    res.ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn fs_nil_err(lua: &Lua, e: &std::io::Error) -> mlua::Result<MultiValue> {
    use std::io::ErrorKind;
    let msg = e.to_string();
    let kind = if (e.kind() == ErrorKind::PermissionDenied
        && (msg.contains("sandbox:") || msg.contains("blocked by policy") || msg.contains("outside")))
        || msg.contains("sandbox:")
    {
        Some("denied")
    } else if msg.contains("(too_large)") {
        Some("too_large")
    } else if e.kind() == ErrorKind::NotFound {
        Some("not_found")
    } else if e.kind() == ErrorKind::AlreadyExists {
        Some("already_exists")
    } else {
        None
    };
    Ok(MultiValue::from_vec(match kind {
        Some(k) => vec![Value::Nil, Value::String(lua.create_string(&msg)?), Value::String(lua.create_string(k)?)],
        None => vec![Value::Nil, Value::String(lua.create_string(&msg)?)],
    }))
}

fn fs_policy_nil_err(lua: &Lua, e: mlua::Error) -> mlua::Result<MultiValue> {
    let msg = e.to_string();
    let denied = msg.contains("sandbox:");
    Ok(MultiValue::from_vec(if denied {
        vec![Value::Nil, Value::String(lua.create_string(&msg)?), Value::String(lua.create_string("denied")?)]
    } else {
        vec![Value::Nil, Value::String(lua.create_string(&msg)?)]
    }))
}

fn resolve_and_check_exec_read(lua: &Lua, path: &Path) -> mlua::Result<PathBuf> {
    let physical = crate::ossandbox::resolve(path).map_err(rt_err)?;
    super::read::check_with_resolved(path, &physical, |target| {
        crate::exec_ctx::check(lua, "read", &target.to_string_lossy())
    })?;
    Ok(physical)
}

/// Resolve under the current execution policy, including host-protected paths and deny-read.
pub fn resolve_policy_path(lua: &Lua, path: &Path) -> mlua::Result<mare_shared::paths::ResolvedPath> {
    let active = crate::exec_ctx::active(lua)?;
    let policy = active.as_ref().map(|a| a.policy.clone()).or_else(|| crate::ossandbox::current(lua));
    let protected = policy.as_ref().map(|_| crate::ossandbox::access::ProtectedHostMatcher::load());
    let checked = mare_shared::paths::resolve_path_checked(path, |part| {
        if let Some(policy) = &policy {
            let denied = policy.deny_read.iter().any(|d| {
                d.is_dir.is_none()
                    || crate::realpath::path_within(part, &d.input)
                    || crate::realpath::path_within(part, &d.target)
            });
            let host_secret = protected.as_ref().is_some_and(|m| m.matches_part(part));
            if denied || host_secret {
                return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "path metadata is denied"));
            }
        }
        Ok(())
    })
    .map_err(rt_err)?;
    if let Some(active) = &active {
        for path in [&checked.logical, &checked.physical] {
            crate::exec_ctx::check_with(lua, active, "read", &path.to_string_lossy())?;
        }
    }
    Ok(checked)
}

fn install_path_ops(lua: &Lua, t: &Table, cwd: &Path) -> mlua::Result<()> {
    let c = cwd.to_path_buf();
    t.set("resolve", lua.create_function(move |_, p: String| Ok(resolve(&c, &p).to_string_lossy().into_owned()))?)?;
    let c = cwd.to_path_buf();
    t.set(
        "project_root",
        lua.create_function(move |lua, ()| {
            let root = mare_shared::project::resource_root(&c).root.unwrap_or_else(|| c.clone());
            crate::exec_ctx::check(lua, "read", &root.to_string_lossy())?;
            Ok(root.to_string_lossy().into_owned())
        })?,
    )?;
    // relative(p) -> p without the cwd in front when it lies below it, else p as given (what
    // tool results and views show; Windows spellings of the cwd fold to one: relpath.rs)
    let base = cwd.to_string_lossy().into_owned();
    t.set(
        "relative",
        lua.create_function(move |_, p: String| Ok(crate::relpath::relative_to(&p, &base).unwrap_or(p)))?,
    )?;
    // realpath(path) -> physical path, appending only a verified missing suffix
    //
    let c = cwd.to_path_buf();
    t.set(
        "realpath",
        lua.create_function(move |lua, p: String| {
            let p = resolve(&c, &p);
            let resolved = resolve_policy_path(lua, &p)?;
            Ok(resolved.physical.to_string_lossy().into_owned())
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "resolve_path",
        lua.create_function(move |lua, (p, base): (String, Option<String>)| {
            if p.is_empty() || p.contains('\0') {
                return Err(rt_err("invalid path"));
            }
            let base = base.map(PathBuf::from).unwrap_or_else(|| c.clone());
            if !base.is_absolute() {
                return Err(rt_err("path base must be absolute"));
            }
            let p = resolve(&base, &p);
            let path = resolve_policy_path(lua, &p)?;
            let logical = path.logical.to_str().ok_or_else(|| rt_err("path is not valid UTF-8"))?;
            let physical = path.physical.to_str().ok_or_else(|| rt_err("path is not valid UTF-8"))?;
            let out = lua.create_table()?;
            out.set("logical", logical)?;
            out.set("physical", physical)?;
            let identity = mare_store::file_identity(&path.existing_ancestor)
                .ok_or_else(|| rt_err("cannot determine path identity"))?;
            out.set("identity", identity)?;
            Ok(out)
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "exists",
        lua.create_function(move |lua, p: String| {
            let path = resolve(&c, &p);
            crate::exec_ctx::check(lua, "read", &path.to_string_lossy())?;
            Ok(path.exists())
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "git_dir",
        lua.create_function(move |lua, p: String| {
            let path = resolve(&c, &p);
            let mut denied = None;
            let found = mare_shared::git::marker_checked(&path, |p| {
                let Some(p) = p.to_str() else {
                    denied = Some(rt_err("sandbox: Git metadata path is not valid UTF-8"));
                    return false;
                };
                match crate::exec_ctx::check(lua, "read", p) {
                    Ok(()) => true,
                    Err(e) => {
                        denied = Some(e);
                        false
                    }
                }
            });
            if let Some(e) = denied {
                return Err(e);
            }
            // nil, "invalid gitfile": a `.git` file where Git's discovery stops (no repository)
            Ok(match found {
                mare_shared::git::Marker::Found(dir) => (Some(dir.to_string_lossy().into_owned()), None),
                mare_shared::git::Marker::Stop => (None, Some("invalid gitfile")),
                mare_shared::git::Marker::Skip => (None, None),
            })
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "git_branch",
        lua.create_function(move |lua, p: Option<String>| {
            let path = match p {
                Some(p) => resolve(&c, &p),
                None => c.clone(),
            };
            let mut denied = None;
            let branch = mare_shared::git::git_branch_checked(&path, |p| {
                if denied.is_some() {
                    return false;
                }
                let Some(p) = p.to_str() else {
                    denied = Some(rt_err("sandbox: Git metadata path is not valid UTF-8"));
                    return false;
                };
                match crate::exec_ctx::check(lua, "read", p) {
                    Ok(()) => true,
                    Err(e) => {
                        denied = Some(e);
                        false
                    }
                }
            });
            if let Some(e) = denied {
                return Err(e);
            }
            Ok(branch)
        })?,
    )?;
    // stat(path, {policies?, cwd?, strict?}?) -> {kind, size, mtime, ctime, ...} | nil[, err]
    let c = cwd.to_path_buf();
    t.set(
        "stat",
        lua.create_function(move |lua, (p, opts): (String, Option<Table>)| {
            let path = resolve(&c, &p);
            let strict = bool_opt(opts.as_ref(), "strict")?;
            let physical = match resolve_and_check_exec_read(lua, &path) {
                Ok(p) => p,
                Err(e) if strict => return fs_policy_nil_err(lua, e),
                Err(e) => return Err(e),
            };
            let policy_cwd = policy_cwd_opt(opts.as_ref(), &c)?;
            let policies = super::read::resolve_policies(lua, opts.as_ref(), &policy_cwd)?;
            if !policies.is_empty()
                && let Err(e) = super::read::check_read_resolved(&policies, &path, &physical, &policy_cwd)
            {
                return if strict { fs_nil_err(lua, &e) } else { Ok(MultiValue::from_vec(vec![Value::Nil])) };
            }
            let target = physical;
            let m = match std::fs::metadata(&target) {
                Ok(m) => m,
                Err(e) if strict => return fs_nil_err(lua, &e),
                Err(_) => return Ok(MultiValue::from_vec(vec![Value::Nil])),
            };
            let t = lua.create_table()?;
            t.set(
                "kind",
                if m.is_dir() {
                    "dir"
                } else if m.is_file() {
                    "file"
                } else {
                    "other"
                },
            )?;
            t.set("size", m.len())?;
            t.set("mtime", mtime(&m))?;
            t.set("ctime", timestamp_ms(m.created()))?;
            // Unix permission bits (e.g. credential files must not be group/other-readable)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                t.set("mode", m.permissions().mode() & 0o7777)?;
                // hard links to this file (edits of a linked file write in place to keep them)
                t.set("nlink", std::os::unix::fs::MetadataExt::nlink(&m))?;
            }
            Ok(MultiValue::from_vec(vec![Value::Table(t)]))
        })?,
    )?;
    // identity(path) -> "dev:ino" (Windows "volume:index") | nil: one key per file however the path
    // is spelled
    let c = cwd.to_path_buf();
    t.set(
        "identity",
        lua.create_function(move |lua, p: String| {
            let path = resolve(&c, &p);
            crate::exec_ctx::check(lua, "read", &path.to_string_lossy())?;
            Ok(mare_store::file_identity(&path))
        })?,
    )?;
    Ok(())
}

fn install_read_ops(lua: &Lua, t: &Table, cwd: &Path) -> mlua::Result<()> {
    // read / write / list run on the blocking pool (async): large files never stall the
    // single-threaded runtime. Call them from coroutines (all plugin code runs in one).
    // read(path, {max_bytes}?) -> content | nil, err. With max_bytes the size is checked first
    // and the read stops one byte past the cap, so a file that grows after the stat is still
    // refused; the error ends in "(too_large)".
    let c = cwd.to_path_buf();
    t.set(
        "read",
        lua.create_async_function(move |lua, (p, opts): (String, Option<Table>)| {
            let sync_capture = crate::activity::in_sync_capture();
            let path = resolve(&c, &p);
            let physical = resolve_and_check_exec_read(&lua, &path);
            let max = opt::<u64>(opts.as_ref(), "max_bytes");
            let no_follow = bool_opt(opts.as_ref(), "no_follow");
            let policy_cwd = policy_cwd_opt(opts.as_ref(), &c);
            let policies = policy_cwd.as_ref().ok().map(|cwd| super::read::resolve_policies(&lua, opts.as_ref(), cwd));
            async move {
                if sync_capture {
                    return Err(mlua::Error::RuntimeError("attempt to yield across a C-call boundary".into()));
                }
                let physical = physical?;
                let max = max?;
                let no_follow = no_follow?;
                let policy_cwd = policy_cwd?;
                let policies = policies.expect("policy_cwd succeeded")?;
                let shown = p.clone();
                // the room for the NUL an external string appends is made here, off the Lua thread
                let r = tokio::task::spawn_blocking(move || {
                    let mut b = if no_follow {
                        let file = super::open_no_follow(&path, None)?;
                        let opened = super::opened_path(&file)?;
                        super::read::check_policies_read(&policies, &opened, &policy_cwd)?;
                        super::read::read_file_capped(file, max, &shown)?
                    } else {
                        super::read::check_read_resolved(&policies, &path, &physical, &policy_cwd)?;
                        read_capped(&physical, max, &shown)?
                    };
                    b.reserve_exact(1);
                    Ok::<_, std::io::Error>(b)
                })
                .await
                .map_err(rt_err)?;
                match r {
                    // handed to Lua as is (Lua 5.5 external string), never copied on the Lua
                    // thread: copying 128 MiB into fresh Lua memory stalled timers ~160 ms on
                    // Windows, where first-touch page faults are slow
                    Ok(b) => Ok(MultiValue::from_vec(vec![Value::String(lua.create_external_string(b)?)])),
                    Err(e) => fs_nil_err(&lua, &e),
                }
            }
        })?,
    )?;
    // read_window(path, {offset=1, limit, max_bytes?}) -> {lines, total, size, hash} | nil, err
    // — streams the file line by line: only the window
    // is kept, the whole content is hashed (sha256 hex, same as mare.crypto.sha256) for the
    // read-before-edit record; a line that isn't UTF-8 is a refusal ending in "(not_text)".
    let c = cwd.to_path_buf();
    t.set(
        "read_window",
        lua.create_async_function(move |lua, (p, opts): (String, Option<Table>)| {
            let path = resolve(&c, &p);
            let physical = resolve_and_check_exec_read(&lua, &path);
            let get = |k: &str| opts.as_ref().and_then(|o| o.get::<Option<usize>>(k).ok().flatten());
            let (offset, limit) = (get("offset").unwrap_or(1).max(1), get("limit").unwrap_or(usize::MAX));
            let max_bytes = opt::<u64>(opts.as_ref(), "max_bytes");
            let policy_cwd = policy_cwd_opt(opts.as_ref(), &c);
            let policies = policy_cwd.as_ref().ok().map(|cwd| super::read::resolve_policies(&lua, opts.as_ref(), cwd));
            async move {
                let physical = physical?;
                let max_bytes = max_bytes?;
                let policy_cwd = policy_cwd?;
                let policies = policies.expect("policy_cwd succeeded")?;
                let shown = p.clone();
                let r = tokio::task::spawn_blocking(move || {
                    super::read::check_read_resolved(&policies, &path, &physical, &policy_cwd)?;
                    read_window(&physical, offset, limit, max_bytes, &shown)
                })
                .await
                .map_err(rt_err)?;
                match r {
                    Ok((lines, total, size, hash)) => {
                        let t = lua.create_table()?;
                        t.set("lines", lines)?;
                        t.set("total", total)?;
                        t.set("size", size)?;
                        t.set("hash", hash)?;
                        Ok(MultiValue::from_vec(vec![Value::Table(t)]))
                    }
                    Err(e) => fs_nil_err(&lua, &e),
                }
            }
        })?,
    )?;
    // sniff(path) -> {binary, kind, size}: a look at the first 8 KiB only (rejects binary files
    // with their type and size; never loads a large binary to find out)
    let c = cwd.to_path_buf();
    t.set(
        "sniff",
        lua.create_async_function(move |lua, p: String| {
            let path = resolve(&c, &p);
            let physical = resolve_and_check_exec_read(&lua, &path);
            async move {
                let physical = physical?;
                let shown = p.clone();
                let (head, size) = tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<u8>, u64)> {
                    use std::io::Read;
                    let mut f = open_regular_nonblocking(&physical, &shown)?;
                    let size = f.metadata()?.len();
                    let mut buf = vec![0u8; MAX_SNIFF_BYTES];
                    let n = f.read(&mut buf)?;
                    buf.truncate(n);
                    Ok((buf, size))
                })
                .await
                .map_err(rt_err)?
                .map_err(rt_err)?;
                let kind = sniff_kind(&head);
                let binary = kind.is_some() || head.contains(&0);
                let t = lua.create_table()?;
                t.set("binary", binary)?;
                t.set("kind", kind.unwrap_or(if binary { "binary data" } else { "text" }))?;
                t.set("size", size)?;
                Ok(t)
            }
        })?,
    )?;
    Ok(())
}

#[cfg(all(test, unix))]
type ControlledWriteHook = Box<dyn Fn(&Path) -> std::io::Result<()>>;

#[cfg(all(test, unix))]
struct BeforeControlledWrite(ControlledWriteHook);

/// Test seam (unit tests only): runs after `exec_ctx::check` and before `spawn_blocking` writes.
#[cfg(all(test, unix))]
pub(super) fn before_controlled_write(lua: &Lua, callback: impl Fn(&Path) -> std::io::Result<()> + 'static) {
    lua.set_app_data(BeforeControlledWrite(Box::new(callback)));
}

fn install_write_ops(lua: &Lua, t: &Table, cwd: &Path) -> mlua::Result<()> {
    // write(path, data, {mode=0o600, append=false, atomic=false, create_new=false}) -> true | nil, err
    // (creates parent dirs; atomic = write a temp file, fsync, rename — for credentials etc.)
    let c = cwd.to_path_buf();
    t.set(
        "write",
        lua.create_async_function(move |lua, (p, data, opts): (String, mlua::LuaString, Option<Table>)| {
            let path = resolve(&c, &p);
            let denied = crate::exec_ctx::check(&lua, "write", &path.to_string_lossy());
            let data = data.as_bytes().to_vec();
            let append = bool_opt(opts.as_ref(), "append");
            let atomic = bool_opt(opts.as_ref(), "atomic");
            let create_new = bool_opt(opts.as_ref(), "create_new");
            let capture_snapshot = bool_opt(opts.as_ref(), "capture_snapshot");
            let mode = opt::<u32>(opts.as_ref(), "mode");
            let sync_boundary = opt::<String>(opts.as_ref(), "sync_boundary").map(|b| b.map(|p| resolve(&c, &p)));
            let boundary_denied = sync_boundary
                .as_ref()
                .ok()
                .and_then(|boundary| boundary.as_ref())
                .map(|boundary| crate::exec_ctx::check(&lua, "write", &boundary.to_string_lossy()))
                .transpose();
            // the policy the check was made against: preserved in full into the blocking write so
            // protected subpaths (`.git`, `.mare`, `deny_read`) are re-checked after opening the
            // parent directory handle
            let explicit_root = opt::<String>(opts.as_ref(), "root").map(|r| r.map(|p| resolve(&c, &p)));
            let policy_cwd = policy_cwd_opt(opts.as_ref(), &c);
            let extra_policies =
                policy_cwd.as_ref().ok().map(|cwd| super::read::resolve_policies(&lua, opts.as_ref(), cwd));
            let exec_policy = crate::exec_ctx::active(&lua).ok().flatten().map(|a| a.policy);
            // Capture the original for tool calls and explicitly requested host/RPC writes.
            // A real tool call id takes precedence over the RPC fallback.
            let fallback = capture_snapshot.as_ref().copied().unwrap_or(false).then_some("rpc.fs.write");
            let snap = crate::snapshot::capture_context(&lua, fallback);
            async move {
                denied?;
                let (append, atomic, create_new) = (append?, atomic?, create_new?);
                capture_snapshot?;
                let mode = mode?;
                let sync_boundary = sync_boundary?;
                boundary_denied?;
                let explicit_root = explicit_root?;
                let policy_cwd = policy_cwd?;
                let extra_policies = extra_policies.expect("policy_cwd succeeded")?;
                let extra_check = super::read::check_policies_op(&extra_policies, "write", &path, &policy_cwd);
                let active_policy = match exec_policy {
                    Some(pol) => Some(extra_policies.into_iter().fold(pol, |a, b| a.intersect(&b))),
                    None => extra_policies.into_iter().reduce(|a, b| a.intersect(&b)),
                };
                #[cfg(all(test, unix))]
                if let Some(hook) = lua.app_data_ref::<BeforeControlledWrite>() {
                    (hook.0)(&path).map_err(rt_err)?;
                }
                let (beneath, anchored_root): (Beneath, Option<PathBuf>) = match (&active_policy, explicit_root) {
                    (Some(pol), _) => (Some(pol.writable.clone()), None),
                    (None, Some(root)) => (None, Some(root)),
                    (None, None) => (None, None),
                };
                let r = tokio::task::spawn_blocking(move || {
                    extra_check?;
                    if let Some(s) = &snap {
                        s.capture(&path);
                    }
                    let r = write_file(
                        &path,
                        &data,
                        WriteOptions {
                            append,
                            atomic,
                            create_new,
                            mode,
                            sync_boundary: sync_boundary.as_deref(),
                            anchored_root: anchored_root.as_deref(),
                            policy: active_policy.as_ref(),
                        },
                        &beneath,
                    );
                    if r.is_ok()
                        && let Some(s) = &snap
                    {
                        s.after(&path);
                    }
                    r
                })
                .await
                .map_err(rt_err)?;
                match r {
                    Ok(()) => Ok(MultiValue::from_vec(vec![Value::Boolean(true)])),
                    Err(e) => fs_nil_err(&lua, &e),
                }
            }
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "mkdir",
        lua.create_function(move |lua, p: String| {
            let path = resolve(&c, &p);
            crate::exec_ctx::check(lua, "write", &path.to_string_lossy())?;
            let policy = crate::exec_ctx::active(lua)?.map(|a| a.policy);
            anchored_mkdir_all(&path, policy.as_ref()).map_err(rt_err)
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "remove",
        // async: removing a large tree must not block the Lua thread
        lua.create_async_function(move |lua, (p, recursive): (String, Option<bool>)| {
            let path = resolve(&c, &p);
            let checked = crate::exec_ctx::check(&lua, "write", &path.to_string_lossy());
            let policy = crate::exec_ctx::active(&lua).map(|a| a.map(|x| x.policy));
            let snap = crate::snapshot::capture_context(&lua, None);
            async move {
                checked?;
                let policy = policy?;
                tokio::task::spawn_blocking(move || {
                    if let Some(s) = &snap
                        && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file())
                    {
                        s.capture(&path);
                    }
                    anchored_remove(&path, recursive.unwrap_or(false), policy.as_ref())
                })
                .await
                .map_err(rt_err)?
                .map_err(rt_err)
            }
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "rename",
        lua.create_function(move |lua, (a, b): (String, String)| {
            let (a, b) = (resolve(&c, &a), resolve(&c, &b));
            // Moving an unreadable file to a readable location would expose its contents;
            // snapshot capture also reads both paths before the rename.
            crate::exec_ctx::check_all(lua, "read", [&*a.to_string_lossy(), &*b.to_string_lossy()])?;
            crate::exec_ctx::check_all(lua, "write", [&*a.to_string_lossy(), &*b.to_string_lossy()])?;
            let policy = crate::exec_ctx::active(lua)?.map(|x| x.policy);
            let snap = crate::snapshot::capture_context(lua, None);
            if let Some(s) = &snap {
                s.capture(&a);
                s.capture(&b);
            }
            anchored_rename(&a, &b, policy.as_ref()).map_err(rt_err)?;
            if let Some(s) = &snap {
                s.after(&a);
                s.after(&b);
            }
            Ok(())
        })?,
    )?;
    Ok(())
}

fn install_dir_ops(lua: &Lua, t: &Table, cwd: &Path) -> mlua::Result<()> {
    // list(dir, {strict=false}) -> { {name, kind}, ... } sorted | nil, err (when strict)
    let c = cwd.to_path_buf();
    t.set(
        "list",
        lua.create_async_function(move |lua, (p, opts): (String, Option<Table>)| {
            let dir = resolve(&c, &p);
            let physical = resolve_and_check_exec_read(&lua, &dir);
            let policy = crate::exec_ctx::active(&lua).map(|a| a.map(|a| a.policy));
            let strict = bool_opt(opts.as_ref(), "strict");
            let no_follow = bool_opt(opts.as_ref(), "no_follow");
            let filter_denied = bool_opt(opts.as_ref(), "filter_denied");
            let max_entries = opt::<usize>(opts.as_ref(), "max_entries");
            let policy_cwd = policy_cwd_opt(opts.as_ref(), &c);
            let extra_policies =
                policy_cwd.as_ref().ok().map(|cwd| super::read::resolve_policies(&lua, opts.as_ref(), cwd));
            async move {
                let physical = physical?;
                let policy_cwd = policy_cwd?;
                let mut policies = extra_policies.expect("policy_cwd succeeded")?;
                if let Some(p) = policy? {
                    policies.push(p);
                }
                let strict = strict?;
                let no_follow = no_follow?;
                let filter_denied = filter_denied?;
                let max_entries = max_entries?;
                let r = tokio::task::spawn_blocking(move || {
                    super::dir::list_dir_entries_checked(
                        &dir,
                        Some(&physical),
                        &policies,
                        &policy_cwd,
                        super::dir::ListDirOpts { strict, no_follow, filter_denied, max_entries },
                    )
                })
                .await
                .map_err(rt_err)?;
                match r {
                    Ok((mut v, truncated)) => {
                        v.sort();
                        let t = lua.create_table()?;
                        for (i, (n, kind)) in v.into_iter().enumerate() {
                            let e = lua.create_table()?;
                            e.set("name", n)?;
                            e.set("kind", kind.as_str())?;
                            t.set(i + 1, e)?;
                        }
                        if truncated {
                            t.set("truncated", true)?;
                        }
                        Ok(MultiValue::from_vec(vec![Value::Table(t)]))
                    }
                    Err(e) => fs_nil_err(&lua, &e),
                }
            }
        })?,
    )?;
    // walk(dir, {hidden, gitignore=true, max_depth, limit=20000, glob, dirs}) -> relative paths
    let c = cwd.to_path_buf();
    t.set(
        "walk",
        lua.create_async_function(move |lua, (p, opts): (Option<String>, Option<Table>)| {
            let root = resolve(&c, p.as_deref().unwrap_or("."));
            let denied = crate::exec_ctx::check(&lua, "read", &root.to_string_lossy());
            let recursive = crate::exec_ctx::refuse_deny_read(&lua, "mare.fs.walk");
            let policy = crate::exec_ctx::active(&lua);
            async move {
                denied?;
                recursive?;
                let policy = policy?.map(|a| a.policy);
                let get_b =
                    |k: &str, d: bool| opts.as_ref().and_then(|o| o.get::<Option<bool>>(k).ok().flatten()).unwrap_or(d);
                let globs: Option<Vec<String>> = match opts.as_ref().map(|o| o.get::<Value>("glob")).transpose()? {
                    Some(Value::String(s)) => Some(vec![s.to_str()?.to_string()]),
                    Some(Value::Table(t)) => Some(t.sequence_values::<String>().collect::<mlua::Result<_>>()?),
                    _ => None,
                };
                let o = WalkOpts {
                    hidden: get_b("hidden", false),
                    gitignore: get_b("gitignore", true),
                    dirs: get_b("dirs", false),
                    max_depth: opts.as_ref().and_then(|o| o.get::<Option<usize>>("max_depth").ok().flatten()),
                    limit: opts.as_ref().and_then(|o| o.get::<Option<usize>>("limit").ok().flatten()).unwrap_or(20000),
                    glob: globs.map(|g| build_glob(&g)).transpose()?,
                };
                tokio::task::spawn_blocking(move || walk(&root, &o, policy.as_ref())).await.map_err(rt_err)
            }
        })?,
    )?;
    // index(dir, {hidden, gitignore=true, dirs, limit=50000}) -> an incrementally refreshed file
    // list (`@` completion; crate::fileindex): idx:refresh{min_interval_ms} -> generation,
    // idx:files() -> relative paths
    let c = cwd.to_path_buf();
    t.set(
        "index",
        lua.create_function(move |lua, (p, opts): (Option<String>, Option<Table>)| {
            let root = resolve(&c, p.as_deref().unwrap_or("."));
            crate::exec_ctx::check(lua, "read", &root.to_string_lossy())?;
            crate::exec_ctx::refuse_deny_read(lua, "mare.fs.index")?;
            let get_b =
                |k: &str, d: bool| opts.as_ref().and_then(|o| o.get::<Option<bool>>(k).ok().flatten()).unwrap_or(d);
            let o = crate::fileindex::Opts {
                hidden: get_b("hidden", false),
                gitignore: get_b("gitignore", true),
                dirs: get_b("dirs", false),
                limit: opts.as_ref().and_then(|o| o.get::<Option<usize>>("limit").ok().flatten()).unwrap_or(50000),
            };
            Ok(crate::fileindex::create(lua, root, o))
        })?,
    )?;
    let c = cwd.to_path_buf();
    t.set(
        "watch",
        lua.create_async_function(move |lua, (p, on_change): (String, mlua::Function)| {
            let cwd = c.clone();
            async move { super::watch::start_watch(&lua, &cwd, &p, on_change).await }
        })?,
    )?;
    Ok(())
}
