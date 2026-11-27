//! Peer-session registry: every interactive session that serves RPC on its local
//! socket registers itself as `<state>/peers/<pid>.json`. This module reads the registry (dropping
//! entries whose process is gone) and holds this process's own entry; the frontend (mare-cli
//! `peer.rs`) writes it. Lua: `mare.peers.list()` / `mare.peers.self()` for the `peers` plugin.
//!
//! Core knows no plugin's data: a plugin puts its own into the entry with
//! `mare.peers.publish(namespace, fields)`, stored under `plugins.<namespace>` (one namespace per
//! owner; the owner validates what it reads back). Readers keep whatever they don't know.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mlua::{Lua, Table};
use serde_json::{Map, Value};
use tokio::sync::watch;

/// This session's registry entry, once registered.
static SELF: Mutex<Option<Value>> = Mutex::new(None);

/// The session this VM's host is on, stamped onto its peer clients' messages at send time
/// (`from.session_id`, self-reported). Per Lua VM, not per process: ACP runs one Host per
/// `session/new` in one process and a sub-agent's host has its own VM, so each host's clients
/// name that host's session. The host keeps it current (mare-host `startup/lifecycle.rs`).
#[derive(Clone, Default)]
pub struct SessionCell(Arc<Mutex<Option<String>>>);

impl SessionCell {
    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }

    pub fn set(&self, id: Option<String>) {
        *self.0.lock().unwrap() = id;
    }
}

/// This VM's session cell, created on first use.
pub fn session_cell(lua: &Lua) -> SessionCell {
    if let Some(cell) = lua.app_data_ref::<SessionCell>() {
        return cell.clone();
    }
    let cell = SessionCell::default();
    lua.set_app_data(cell.clone());
    cell
}

/// What this VM's plugins published, by namespace (`plugins` in the entry): per Lua VM, not per
/// process, because sub-agents run their own plugins in fresh VMs of the same process and must
/// not overwrite the session's. A watch, so the registry writer (mare-cli `peer.rs`) rewrites the
/// file when it changes.
struct Published(watch::Sender<Map<String, Value>>);

/// A namespace: one segment of the plugin record grammar (lowercase letters, digits, `.`, `_`,
/// `-`, starting with a letter or digit), by convention the publishing plugin's name.
fn valid_namespace(ns: &str) -> bool {
    let mut c = ns.chars();
    c.next().is_some_and(|f| f.is_ascii_lowercase() || f.is_ascii_digit())
        && c.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-'))
}

fn sender(lua: &Lua) -> watch::Sender<Map<String, Value>> {
    if let Some(p) = lua.app_data_ref::<Published>() {
        return p.0.clone();
    }
    let tx = watch::channel(Map::new()).0;
    lua.set_app_data(Published(tx.clone()));
    tx
}

/// Replace namespace `ns` with `fields` (an object), or remove it (`None`). Other namespaces and
/// the core fields are never touched.
fn publish(lua: &Lua, ns: &str, fields: Option<Map<String, Value>>) {
    sender(lua).send_if_modified(|m| match fields {
        Some(f) if m.get(ns).and_then(Value::as_object) == Some(&f) => false,
        Some(f) => {
            m.insert(ns.to_string(), Value::Object(f));
            true
        }
        None => m.remove(ns).is_some(),
    });
}

/// Follow what this VM's plugins publish (`borrow_and_update()` for the current set).
pub fn published(lua: &Lua) -> watch::Receiver<Map<String, Value>> {
    sender(lua).subscribe()
}

pub fn dir() -> PathBuf {
    mare_shared::paths::state_dir().join("peers")
}

pub fn json_path(pid: u32) -> PathBuf {
    dir().join(format!("{pid}.json"))
}

pub fn socket_path(pid: u32) -> PathBuf {
    dir().join(format!("{pid}.sock"))
}

/// Is a process with this pid still running?
pub fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // signal 0: existence check; EPERM still means "exists"
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        super::winsandbox::pid_alive(pid)
    }
}

/// Process start time token for PID-reuse detection (clock ticks since boot on Linux, µs since
/// epoch on macOS, 100ns FILETIME ticks on Windows). Returns `None` when unavailable or unsupported.
pub fn start_time(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rfind(')')?;
        // Field 2 is `(comm)`; fields after `)` start at field 3 (state), so field 22 (`starttime`)
        // is at 0-based index 19.
        stat[after_comm + 1..].split_whitespace().nth(19)?.parse::<u64>().ok()
    }
    #[cfg(target_os = "macos")]
    {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (n == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }
    #[cfg(windows)]
    {
        super::winsandbox::pid_start_time(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

/// Whether `pid` is still running and (when both `expected_start` and the OS start time are
/// available) matches `expected_start`. Prevents deleting files of a newly-spawned process that
/// reused an old PID.
pub fn alive_with_start_time(pid: u32, expected_start: Option<u64>) -> bool {
    if !alive(pid) {
        return false;
    }
    match (expected_start, start_time(pid)) {
        (Some(expected), Some(actual)) => actual == expected,
        _ => true,
    }
}

fn remove_entry_in(d: &Path, pid: u32) {
    let _ = std::fs::remove_file(d.join(format!("{pid}.json")));
    let _ = std::fs::remove_file(d.join(format!("{pid}.sock")));
    // Windows: the pipe's lock file next to the endpoint file
    #[cfg(windows)]
    let _ = std::fs::remove_file(crate::platform::pipe::lock_path(&d.join(format!("{pid}.sock"))));
}

/// All registered sessions (this one included), sorted by pid. Entries of dead processes are
/// removed on the way (with their socket).
pub fn list() -> Vec<Value> {
    list_in(&dir())
}

fn list_in(d: &Path) -> Vec<Value> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(d) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(pid) = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) else { continue };
        if !alive(pid) {
            remove_entry_in(d, pid);
            continue;
        }
        if let Some(v) = std::fs::read_to_string(&p).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()) {
            out.push(v);
        }
    }
    out.sort_by_key(|v| v.get("pid").and_then(Value::as_u64).unwrap_or(0));
    out
}

/// Write (or rewrite) this session's entry atomically and remember it.
pub fn write_self(entry: &Value) -> std::io::Result<()> {
    let pid = std::process::id();
    let p = json_path(pid);
    // never write a registry file into a directory that isn't ours and private
    ensure_dir()?;
    // 0600 from creation: never readable by others, not even between write and chmod
    mare_shared::paths::write_atomic(&p, &serde_json::to_vec_pretty(entry)?, mare_shared::paths::WriteMode::Private)?;
    *SELF.lock().unwrap() = Some(entry.clone());
    Ok(())
}

/// Remove this session's entry and socket (on shutdown).
pub fn unregister_self() {
    if SELF.lock().unwrap().take().is_some() {
        remove_entry_in(&dir(), std::process::id());
    }
}

pub fn self_entry() -> Option<Value> {
    SELF.lock().unwrap().clone()
}

/// Create (or check) the registry directory: mode 0700, a real directory (not a symlink) owned by
/// this user. Any failure — including a failed chmod — means the session must not listen.
/// Windows: the private DACL of `mare_shared::private` instead of the mode (setting it fails
/// unless this user owns the directory or may change its DACL); the registry files and pipe
/// endpoint / lock files inside inherit it.
pub fn ensure_dir() -> std::io::Result<PathBuf> {
    let d = dir();
    ensure_private_dir(&d)?;
    Ok(d)
}

#[cfg(unix)]
fn verify_private_dir_meta(
    d: &Path,
    is_symlink: bool,
    is_dir: bool,
    uid: u32,
    me: u32,
    mode: u32,
) -> std::io::Result<()> {
    if is_symlink || !is_dir || uid != me || (mode & 0o077) != 0 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} failed private directory ownership/mode check", d.display()),
        ));
    }
    Ok(())
}

fn ensure_private_dir(d: &Path) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    match std::fs::symlink_metadata(d) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(Error::new(ErrorKind::PermissionDenied, format!("{} is a symlink", d.display())));
        }
        Ok(m) if !m.is_dir() => {
            return Err(Error::new(ErrorKind::AlreadyExists, format!("{} is not a directory", d.display())));
        }
        Ok(_m) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                let me = unsafe { libc::geteuid() };
                if _m.uid() != me {
                    return Err(Error::new(
                        ErrorKind::PermissionDenied,
                        format!("{} belongs to uid {} (not {me})", d.display(), _m.uid()),
                    ));
                }
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => mare_shared::private::create_dir_all(d)?,
        Err(e) => return Err(e),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?;
        let m = std::fs::symlink_metadata(d)?;
        let me = unsafe { libc::geteuid() };
        verify_private_dir_meta(d, m.file_type().is_symlink(), m.is_dir(), m.uid(), me, m.mode())?;
    }
    // the chmod: only when needed (the registry is rewritten every few hundred ms)
    #[cfg(windows)]
    if mare_shared::private::check(d).is_err() {
        mare_shared::private::restrict(d)?;
    }
    Ok(())
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    // list() -> { {name, pid, cwd, socket, mode, status, started_at_ms, plugins?}, ... } (dead sessions
    // pruned; `plugins` = what plugins published, absent in older entries)
    t.set(
        "list",
        lua.create_function(|lua, ()| {
            crate::exec_ctx::refuse_deny_read(lua, "mare.peers.list")?;
            mare_shared::lua::json_to_lua(lua, &Value::Array(list()))
        })?,
    )?;
    // self() -> this session's entry, or nil when it isn't registered as a peer
    t.set(
        "self",
        lua.create_function(|lua, ()| match self_entry() {
            Some(v) => mare_shared::lua::json_to_lua(lua, &v),
            None => Ok(mlua::Value::Nil),
        })?,
    )?;
    t.set("dir", lua.create_function(|_, ()| Ok(dir().to_string_lossy().into_owned()))?)?;
    // publish(namespace, fields|nil): this process's `plugins.<namespace>` (replaced whole; nil
    // removes it). Written with the next registry rewrite (throttled); kept but unwritten while
    // the session isn't registered.
    t.set(
        "publish",
        lua.create_function(|lua, (ns, fields): (String, Option<Table>)| {
            if !valid_namespace(&ns) {
                return Err(mlua::Error::runtime(format!(
                    "mare.peers.publish: namespace {ns:?} must be lowercase letters, digits, `.`, `_`, `-`"
                )));
            }
            let fields = match fields {
                None => None,
                Some(t) => match mare_shared::lua::lua_to_json(lua, mlua::Value::Table(t))? {
                    Value::Object(m) => Some(m),
                    Value::Array(a) if a.is_empty() => Some(Map::new()),
                    _ => {
                        return Err(mlua::Error::runtime("mare.peers.publish: fields must be a table of named fields"));
                    }
                },
            };
            publish(lua, &ns, fields);
            Ok(())
        })?,
    )?;
    crate::peer_client::install(lua, &t)?;
    mare.set("peers", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Windows: a private DACL (`mare_shared::private`), below a directory open to everyone — for
    /// a directory created here and one that already existed; a directory symlink is refused too.
    #[test]
    fn registry_dir_must_be_a_private_real_directory() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        mare_shared::private::testing::open_to_everyone(d.path());
        let ok = d.path().join("peers");
        ensure_private_dir(&ok).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&ok).unwrap().permissions().mode() & 0o777, 0o700);
        }
        #[cfg(windows)]
        {
            assert_eq!(mare_shared::private::check(&ok), Ok(()));
            let existing = d.path().join("existing");
            std::fs::create_dir(&existing).unwrap();
            assert!(mare_shared::private::check(&existing).is_err());
            ensure_private_dir(&existing).unwrap();
            assert_eq!(mare_shared::private::check(&existing), Ok(()));
        }
        let link = d.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&ok, &link).unwrap();
        // creating a directory symlink needs Developer Mode or the privilege: skip that part
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&ok, &link).is_err() {
            return;
        }
        assert!(ensure_private_dir(&link).is_err(), "a symlinked registry dir is refused");
    }

    #[test]
    fn dead_entries_are_pruned() {
        let d = tempfile::tempdir().unwrap();
        let me = std::process::id();
        std::fs::write(d.path().join(format!("{me}.json")), format!(r#"{{"pid":{me}}}"#)).unwrap();
        // pid near the top of the range: not running
        std::fs::write(d.path().join("4194300.json"), r#"{"pid":4194300}"#).unwrap();
        let l = list_in(d.path());
        assert_eq!(l.len(), 1);
        assert_eq!(l[0]["pid"], me);
    }

    /// An entry written before `plugins` existed still lists; fields this version doesn't know
    /// (top-level or published) are kept as they are.
    #[test]
    fn older_and_newer_entries_both_list() {
        let d = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let old = format!(r#"{{"name":"w-1a2b","pid":{me},"cwd":"/w","socket":"/s","mode":"tui","started_at_ms":1}}"#);
        std::fs::write(d.path().join(format!("{me}.json")), old).unwrap();
        let l = list_in(d.path());
        assert_eq!((l.len(), l[0]["name"].as_str()), (1, Some("w-1a2b")));
        assert!(l[0].get("plugins").is_none(), "{}", l[0]);
        let newer = format!(r#"{{"name":"w","pid":{me},"future":true,"plugins":{{"x":{{"y":[1]}}}}}}"#);
        std::fs::write(d.path().join(format!("{me}.json")), newer).unwrap();
        let l = list_in(d.path());
        assert_eq!((l[0]["future"].as_bool(), &l[0]["plugins"]["x"]["y"]), (Some(true), &serde_json::json!([1])));
    }

    #[test]
    fn alive_with_start_time_detects_pid_reuse() {
        let me = std::process::id();
        assert!(alive_with_start_time(me, None));
        #[cfg(any(target_os = "linux", target_os = "macos", windows))]
        {
            let st = start_time(me).expect("current process has start time");
            assert!(alive_with_start_time(me, Some(st)));
            assert!(!alive_with_start_time(me, Some(st ^ 1)), "mismatched start time means reused pid");
        }
    }

    /// Peer registry directory refuses regular files, mismatched ownership, and loose modes.
    #[test]
    fn registry_dir_rejects_non_dir_wrong_owner_and_loose_mode() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        assert!(ensure_private_dir(&file).is_err(), "regular file must be rejected as peers dir");

        #[cfg(unix)]
        {
            let p = d.path().join("peers");
            assert!(verify_private_dir_meta(&p, false, true, 1000, 1000, 0o40700).is_ok());
            assert!(verify_private_dir_meta(&p, false, true, 1001, 1000, 0o40700).is_err(), "wrong uid must fail");
            assert!(
                verify_private_dir_meta(&p, false, true, 1000, 1000, 0o40755).is_err(),
                "group/other bits must fail"
            );
            assert!(verify_private_dir_meta(&p, true, true, 1000, 1000, 0o40700).is_err(), "symlink must fail");
        }
    }
}
