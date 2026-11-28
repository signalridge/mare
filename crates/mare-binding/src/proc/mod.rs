//! `mare.proc`: spawn processes with async stdio, signals, exit codes, open URLs.
//! Optional OS sandbox via `sandbox = { level, writable, readable, network, ... }` — see
//! [`crate::ossandbox`].

mod build;
mod manager;
mod run;

use std::time::Duration;

#[cfg(any(windows, test))]
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
};

use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Mutex as AsyncMutex;

use super::ossandbox::Prepared;
use super::platform::{self, ProcessBackend, Signal};
use mare_shared::lua::rt_err;

pub use build::{LingeringProcess, RawProcess, spawn_confined};
pub(crate) use build::{SpawnOpts, build, parse_opts};
pub use manager::{MAX_PROCS_PER_OWNER, active_lingering_tree_count, active_proc_count, last_spawned_pid};

pub(crate) fn signal_num(s: &str) -> Signal {
    Signal::parse(s)
}

/// Signal the whole process tree (bash children included).
pub(crate) fn kill_group(pid: Option<u32>, sig: Signal) {
    platform::signal_tree(pid, sig);
}

/// Grace between the polite stop and SIGKILL / ending the job.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(3);
/// After an interrupt's SIGINT, how long the group gets to stop before SIGTERM.
pub(crate) const INTERRUPT_WAIT: Duration = Duration::from_secs(1);
/// How long to wait for the reap and for leftovers of the group after SIGKILL.
pub(crate) const REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Hard stop of a child process tree: SIGTERM the group, wait `grace` for the exit, SIGKILL the
/// group, then wait (bounded) for the reap and for the rest of the group to disappear. Returns the
/// exit status when it was reaped, and whether the stop is confirmed — `false` means
/// `stop_unconfirmed` (not reaped, or group members left): callers report it and never reuse the
/// resource.
pub(crate) async fn stop_tree(
    child: &mut Child,
    pid: Option<u32>,
    grace: Duration,
) -> (Option<std::process::ExitStatus>, bool) {
    kill_group(pid, Signal::Term);
    let mut status = tokio::time::timeout(grace, child.wait()).await.ok().and_then(Result::ok);
    // the leader may exit on SIGTERM while others in its group ignore it
    if status.is_none() || platform::tree_alive(pid) {
        kill_group(pid, Signal::Kill);
    }
    if status.is_none() {
        status = tokio::time::timeout(REAP_TIMEOUT, child.wait()).await.ok().and_then(Result::ok);
    }
    let deadline = tokio::time::Instant::now() + REAP_TIMEOUT;
    while platform::tree_alive(pid) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let confirmed = status.is_some() && !platform::tree_alive(pid);
    (status, confirmed)
}

/// A child whose protection monitor could not attach must not outlive an error return.
pub(crate) fn attach_monitor(
    monitor: Option<super::ossandbox::ProtectionMonitor>,
    child: &mut Child,
) -> mlua::Result<Option<super::ossandbox::ProtectionMonitor>> {
    let Some(monitor) = monitor else { return Ok(None) };
    let pid = child.id();
    if let Err(e) = monitor.attach(pid) {
        kill_group(pid, Signal::Kill);
        let _ = child.start_kill();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                _ => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        while super::ossandbox::failed_spawn_tree_alive(pid) {
            std::thread::sleep(Duration::from_millis(10));
        }
        monitor.after_kill();
        return Err(rt_err(e));
    }
    Ok(Some(monitor))
}

pub struct Process {
    pid: Option<u32>,
    child: AsyncMutex<Child>,
    stdin: AsyncMutex<Option<ChildStdin>>,
    stdout: AsyncMutex<Option<BufReader<ChildStdout>>>,
    stderr: AsyncMutex<Option<BufReader<ChildStderr>>>,
    sandbox: Option<Prepared>,
    /// the whole tree ends with this process object, the host or mare; declared
    /// last: dropped after the child
    _tree: super::trees::Tree,
}

pub(crate) fn status_table(lua: &Lua, st: std::process::ExitStatus) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("code", st.code())?;
    t.set("signal", <platform::Process as ProcessBackend>::exit_signal(&st))?;
    t.set("success", st.success())?;
    Ok(t)
}

impl UserData for Process {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("pid", |_, this, ()| Ok(this.pid));
        // sandbox() -> what is enforced on this platform (nil when unsandboxed)
        m.add_method("sandbox", |lua, this, ()| this.sandbox.as_ref().map(|p| p.report(lua)).transpose());
        // sandbox_denied(output?) -> {sandbox_denied, path, host} | nil — only real sandbox refusals
        m.add_method("sandbox_denied", |lua, this, output: Option<String>| {
            let Some(p) = &this.sandbox else { return Ok(None) };
            p.classify(output.as_deref().unwrap_or("")).map(|d| d.to_lua(lua)).transpose()
        });
        m.add_async_method("write", |_, this, data: mlua::LuaString| async move {
            let mut g = this.stdin.lock().await;
            let w = g.as_mut().ok_or_else(|| rt_err("stdin is closed"))?;
            w.write_all(&data.as_bytes()).await.map_err(rt_err)?;
            w.flush().await.map_err(rt_err)?;
            Ok(())
        });
        m.add_async_method("close_stdin", |_, this, ()| async move {
            this.stdin.lock().await.take();
            Ok(())
        });
        // read_line() -> string (without newline) | nil at EOF
        m.add_async_method("read_line", |_, this, ()| async move {
            let mut g = this.stdout.lock().await;
            let Some(r) = g.as_mut() else { return Ok(None) };
            let mut buf = String::new();
            let n = r.read_line(&mut buf).await.map_err(rt_err)?;
            if n == 0 {
                return Ok(None);
            }
            if buf.ends_with('\n') {
                buf.pop();
                if buf.ends_with('\r') {
                    buf.pop();
                }
            }
            Ok(Some(buf))
        });
        // read() -> next chunk | nil at EOF
        m.add_async_method("read", |lua, this, ()| async move {
            let mut g = this.stdout.lock().await;
            let Some(r) = g.as_mut() else { return Ok(Value::Nil) };
            let mut buf = vec![0u8; 16384];
            let n = r.read(&mut buf).await.map_err(rt_err)?;
            if n == 0 {
                return Ok(Value::Nil);
            }
            Ok(Value::String(lua.create_string(&buf[..n])?))
        });
        m.add_async_method("read_stderr_line", |_, this, ()| async move {
            let mut g = this.stderr.lock().await;
            let Some(r) = g.as_mut() else { return Ok(None) };
            let mut buf = String::new();
            let n = r.read_line(&mut buf).await.map_err(rt_err)?;
            if n == 0 {
                return Ok(None);
            }
            Ok(Some(buf.trim_end_matches(['\n', '\r']).to_string()))
        });
        m.add_async_method("wait", |lua, this, ()| async move {
            let st = this.child.lock().await.wait().await.map_err(rt_err)?;
            status_table(&lua, st)
        });
        m.add_method("kill", |_, this, sig: Option<String>| {
            kill_group(this.pid, signal_num(sig.as_deref().unwrap_or("TERM")));
            Ok(())
        });
    }
}

/// `path` if it names a spill file of `prefix` directly inside `<state>/spill`, else an error.
/// The caller names its own files: `prefix` is `[a-z0-9-]+` and the file name is
/// `<prefix>-*.log`. The path is taken as given (not resolved): anything but an absolute path
/// whose parent is exactly the spill directory - `..` components, a relative path, a
/// subdirectory - is refused.
fn spill_path(prefix: &str, path: &str) -> mlua::Result<std::path::PathBuf> {
    if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        return Err(rt_err(format!("invalid spill prefix {prefix:?}: expected [a-z0-9-]+")));
    }
    let p = std::path::Path::new(path);
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let dir = mare_shared::paths::state_dir().join("spill");
    let named =
        name.strip_prefix(prefix).and_then(|rest| rest.strip_prefix('-')).is_some_and(|rest| rest.ends_with(".log"));
    if p.parent() == Some(dir.as_path()) && named {
        Ok(p.to_path_buf())
    } else {
        Err(rt_err(format!("not a spill file of `{prefix}` (<state>/spill/{prefix}-*.log): {path}")))
    }
}

/// Arbitrary spill paths obey the write policy. A housekeeping spill file - one whose caller
/// names its `prefix` (`spill_path`) - keeps the exemption from writable roots, but never from an
/// explicit deny-read subtree: neither the active tool context's nor the spawned command's own
/// per-call `sandbox` (`call`, prepared before the spill is opened, so a refused sandbox
/// truncates nothing).
pub(crate) fn checked_spill_path(
    lua: &Lua,
    path: &str,
    prefix: Option<&str>,
    call: Option<&crate::ossandbox::Policy>,
) -> mlua::Result<std::path::PathBuf> {
    if let Some(prefix) = prefix {
        spill_path(prefix, path)?;
    }
    let p = std::path::PathBuf::from(path);
    let p = if p.is_absolute() { p } else { std::env::current_dir().map_err(rt_err)?.join(p) };
    if call.is_some_and(|c| !c.can_write_deny_read(&p)) {
        return Err(rt_err("sandbox: spill path intersects a protected or deny_read path"));
    }
    let Some(a) = crate::exec_ctx::active(lua)? else { return Ok(p) };
    if prefix.is_some() {
        if !a.policy.can_write_deny_read(&p) {
            return Err(rt_err("sandbox: spill path intersects a protected or deny_read path"));
        }
    } else {
        crate::exec_ctx::check_with(lua, &a, "write", &p.to_string_lossy())?;
    }
    Ok(p)
}

fn register_spill_ops(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // spill_write(prefix, path, data, append?) / spill_remove(prefix, path): spill files that
    // mare or a plugin keeps (`<state>/spill/<prefix>-*.log`, the complete output of a command;
    // the caller names its prefix, e.g. the bash tool "bash", the hooks plugin "hooks"). Not
    // sandbox-checked like `mare.fs`: inside a tool call that check would refuse this
    // housekeeping, record it as a denial and turn it into a bogus "request write access to the
    // spill dir" hint. Any other path is refused.
    t.set(
        "spill_write",
        lua.create_function(|lua, (prefix, path, data, append): (String, String, mlua::LuaString, Option<bool>)| {
            let p = checked_spill_path(lua, &path, Some(&prefix), None)?;
            if let Some(d) = p.parent() {
                mare_shared::private::create_dir_all(d).map_err(rt_err)?;
            }
            let mut o = std::fs::OpenOptions::new();
            o.create(true).write(true);
            if append == Some(true) {
                o.append(true)
            } else {
                o.truncate(true)
            };
            std::io::Write::write_all(&mut mare_shared::private::open(&mut o, &p).map_err(rt_err)?, &data.as_bytes())
                .map_err(rt_err)
        })?,
    )?;
    t.set(
        "spill_remove",
        lua.create_function(|lua, (prefix, path): (String, String)| {
            let p = checked_spill_path(lua, &path, Some(&prefix), None)?;
            let _ = std::fs::remove_file(p);
            Ok(())
        })?,
    )?;
    Ok(())
}

fn register_env(lua: &Lua, t: &Table) -> mlua::Result<()> {
    // env{env_include?, env_exclude?} -> the scrubbed parent environment every child starts from
    // — for spawners outside mare.proc/job/pty (MCP servers, sub-agents)
    t.set(
        "env",
        lua.create_function(|lua, opts: Option<Table>| {
            let policy = match &opts {
                Some(o) => super::subenv::policy_from(o)?,
                None => Default::default(),
            };
            let out = lua.create_table()?;
            for (k, v) in super::subenv::scrubbed_parent_env(&policy) {
                out.set(k.to_string_lossy().to_string(), v.to_string_lossy().to_string())?;
            }
            Ok(out)
        })?,
    )
}

fn register_spawn(lua: &Lua, t: &Table, cwd: String) -> mlua::Result<()> {
    t.set(
        "spawn",
        lua.create_function(move |lua, opts: Table| {
            let o = parse_opts(&opts)?;
            let mut cmd = build(lua, &o, &cwd)?;
            let sandbox = o.prepared.take();
            let (mut child, monitor) = build::spawn_monitored(&mut cmd, sandbox.as_ref(), &format!("spawn {}", o.cmd))?;
            let tree = super::trees::Tree::new(child.id(), Some(super::trees::Owner::of(lua)));
            let _ = attach_monitor(monitor, &mut child)?;
            Ok(Process {
                sandbox,
                pid: child.id(),
                _tree: tree,
                stdin: AsyncMutex::new(child.stdin.take()),
                stdout: AsyncMutex::new(child.stdout.take().map(BufReader::new)),
                stderr: AsyncMutex::new(child.stderr.take().map(BufReader::new)),
                child: AsyncMutex::new(child),
            })
        })?,
    )
}

/// `mare.proc.which(name)`: the absolute path of an executable, or nil; no process is started
/// (Neovim `exepath()`). A bare name is looked up in `path_env` (PATH; on Windows with the
/// PATHEXT extensions); a name with a path separator is a path, relative to `cwd`, and is returned
/// only when it is an executable file.
pub fn which_path(name: &str, path_env: Option<std::ffi::OsString>, cwd: &std::path::Path) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    // a sub-agent's working directory may be relative (to the process's): make it absolute first,
    // so which_in's own cwd join is the only one and the result is absolute
    let cwd = std::path::absolute(cwd).ok()?;
    #[cfg(windows)]
    {
        let found = windows_which_path(name, path_env, &cwd, std::env::var_os("PATHEXT").as_deref())?;
        // Returning a lossy path would name a different file. Preserve PATH precedence by refusing
        // the first hit instead of silently selecting a later program.
        found.into_os_string().into_string().ok()
    }
    #[cfg(not(windows))]
    {
        which_unix_with(name, path_env, &cwd, &UnixWhichSys)
    }
}

#[cfg(not(windows))]
struct UnixWhichSys;

#[cfg(not(windows))]
impl which::sys::Sys for UnixWhichSys {
    type ReadDirEntry = std::fs::DirEntry;
    type Metadata = std::fs::Metadata;

    fn is_windows(&self) -> bool {
        false
    }
    fn current_dir(&self) -> std::io::Result<std::path::PathBuf> {
        which::sys::Sys::current_dir(&which::sys::RealSys)
    }
    fn home_dir(&self) -> Option<std::path::PathBuf> {
        which::sys::Sys::home_dir(&which::sys::RealSys)
    }
    fn env_split_paths(&self, paths: &std::ffi::OsStr) -> Vec<std::path::PathBuf> {
        which::sys::Sys::env_split_paths(&which::sys::RealSys, paths)
    }
    fn env_path(&self) -> Option<std::ffi::OsString> {
        // The caller supplies PATH explicitly; None must not fall back to the process PATH.
        None
    }
    fn env_path_ext(&self) -> Option<std::ffi::OsString> {
        None
    }
    fn metadata(&self, path: &std::path::Path) -> std::io::Result<Self::Metadata> {
        which::sys::Sys::metadata(&which::sys::RealSys, path)
    }
    fn symlink_metadata(&self, path: &std::path::Path) -> std::io::Result<Self::Metadata> {
        which::sys::Sys::symlink_metadata(&which::sys::RealSys, path)
    }
    fn read_dir(
        &self,
        path: &std::path::Path,
    ) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<Self::ReadDirEntry>>>> {
        which::sys::Sys::read_dir(&which::sys::RealSys, path)
    }
    fn is_valid_executable(&self, path: &std::path::Path) -> std::io::Result<bool> {
        which::sys::Sys::is_valid_executable(&which::sys::RealSys, path)
    }
}

#[cfg(not(windows))]
fn which_unix_with<S: which::sys::Sys>(
    name: &str,
    path_env: Option<std::ffi::OsString>,
    cwd: &std::path::Path,
    sys: &S,
) -> Option<String> {
    let mut cfg = which::WhichConfig::new_with_sys(sys).binary_name(name.into()).custom_cwd(cwd.to_path_buf());
    if let Some(paths) = path_env {
        cfg = cfg.custom_path_list(paths);
    }
    let found = cfg.first_result().ok()?;
    // Returning a lossy path would name a different file. Preserve PATH precedence by refusing
    // the first hit instead of silently selecting a later program.
    found.into_os_string().into_string().ok()
}

// `which`'s default Windows system caches PATHEXT and treats an empty value as no extensions.
// Supply the current value (or the Windows defaults) without changing the process environment,
// while retaining the crate's PATH, relative-path, and candidate ordering behavior.
#[cfg(any(windows, test))]
struct WindowsWhichSys(OsString);

#[cfg(any(windows, test))]
impl which::sys::Sys for WindowsWhichSys {
    type ReadDirEntry = std::fs::DirEntry;
    type Metadata = std::fs::Metadata;

    fn is_windows(&self) -> bool {
        true
    }
    fn current_dir(&self) -> std::io::Result<PathBuf> {
        which::sys::Sys::current_dir(&which::sys::RealSys)
    }
    fn home_dir(&self) -> Option<PathBuf> {
        which::sys::Sys::home_dir(&which::sys::RealSys)
    }
    fn env_split_paths(&self, paths: &OsStr) -> Vec<PathBuf> {
        which::sys::Sys::env_split_paths(&which::sys::RealSys, paths)
    }
    fn env_path(&self) -> Option<OsString> {
        // The caller supplies PATH explicitly; None must not fall back to the process PATH.
        None
    }
    fn env_path_ext(&self) -> Option<OsString> {
        Some(self.0.clone())
    }
    fn metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::Sys::metadata(&which::sys::RealSys, path)
    }
    fn symlink_metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
        which::sys::Sys::symlink_metadata(&which::sys::RealSys, path)
    }
    fn read_dir(&self, path: &Path) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<Self::ReadDirEntry>>>> {
        which::sys::Sys::read_dir(&which::sys::RealSys, path)
    }
    fn is_valid_executable(&self, path: &Path) -> std::io::Result<bool> {
        which::sys::Sys::is_valid_executable(&which::sys::RealSys, path)
    }
}

#[cfg(any(windows, test))]
fn windows_which_path(name: &str, path_env: Option<OsString>, cwd: &Path, pathext: Option<&OsStr>) -> Option<PathBuf> {
    const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
    let pathext = pathext.filter(|ext| !ext.is_empty()).unwrap_or(OsStr::new(DEFAULT_PATHEXT));
    let sys = WindowsWhichSys(pathext.to_owned());
    let extensions = which::sys::Sys::env_windows_path_ext(&sys);
    let search = which::WhichConfig::new_with_sys(sys).binary_name(name.into()).custom_cwd(cwd.to_path_buf());
    let search = match path_env {
        Some(path_env) => search.custom_path_list(path_env),
        None => search,
    };
    first_windows_which_match(search.all_results().ok()?, &extensions)
}

#[cfg(any(windows, test))]
fn first_windows_which_match(mut candidates: impl Iterator<Item = PathBuf>, extensions: &[String]) -> Option<PathBuf> {
    candidates.find(|path| {
        path.extension().and_then(OsStr::to_str).is_some_and(|ext| {
            !ext.is_empty()
                && extensions
                    .iter()
                    .filter_map(|allowed| allowed.strip_prefix('.'))
                    .any(|allowed| ext.eq_ignore_ascii_case(allowed))
        })
    })
}

fn split_browser_cmd(raw: &str) -> Option<Vec<String>> {
    #[cfg(unix)]
    {
        shlex::split(raw)
    }
    #[cfg(windows)]
    {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Some(Vec::new());
        }
        if std::path::Path::new(trimmed).exists() {
            return Some(vec![trimmed.to_owned()]);
        }
        // Split on whitespace while preserving Windows backslashes inside paths.
        let mut parts = Vec::new();
        let mut cur = String::new();
        let mut in_quotes = false;
        for ch in trimmed.chars() {
            match ch {
                '"' => in_quotes = !in_quotes,
                c if c.is_whitespace() && !in_quotes => {
                    if !cur.is_empty() {
                        parts.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if in_quotes {
            return None;
        }
        if !cur.is_empty() {
            parts.push(cur);
        }
        Some(parts)
    }
}

fn split_browser_candidates(raw: &str) -> Vec<&str> {
    #[cfg(unix)]
    {
        raw.split(':').map(str::trim).filter(|s| !s.is_empty()).collect()
    }
    #[cfg(windows)]
    {
        let trimmed = raw.trim();
        if trimmed.is_empty() { Vec::new() } else { vec![trimmed] }
    }
}

fn expand_browser_arg(arg: &str, url: &str) -> (String, bool) {
    let mut out = String::with_capacity(arg.len());
    let mut chars = arg.chars().peekable();
    let mut substituted = false;
    while let Some(ch) = chars.next() {
        if ch == '%' {
            match chars.peek().copied() {
                Some('s') => {
                    chars.next();
                    out.push_str(url);
                    substituted = true;
                }
                Some('%') => {
                    chars.next();
                    out.push('%');
                }
                _ => out.push('%'),
            }
        } else {
            out.push(ch);
        }
    }
    (out, substituted)
}

fn resolve_browser_program(prog: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(prog);
    if p.components().count() > 1 || p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    which::which(prog).ok()
}

fn spawn_detached_browser(program: &std::path::Path, args: &[String]) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Detach into a new session via double-fork and reap the intermediate child so the
        // browser neither becomes a zombie nor receives SIGHUP when the terminal closes.
        unsafe {
            cmd.pre_exec(|| {
                match libc::fork() {
                    -1 => return Err(std::io::Error::last_os_error()),
                    0 => (),
                    _ => libc::_exit(0),
                }
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn()?.wait().map(|_| ())
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        cmd.spawn().map(|_| ())
    }
}

fn open_url_with(
    url: &str,
    browser_env: Option<&str>,
    fallback: impl FnOnce(&str) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let Some(browser) = browser_env.map(str::trim).filter(|s| !s.is_empty()) else {
        return fallback(url);
    };
    let candidates = split_browser_candidates(browser);
    if candidates.is_empty() {
        return fallback(url);
    }
    for candidate in candidates {
        let parts = split_browser_cmd(candidate).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("invalid BROWSER command: {candidate:?}"))
        })?;
        let Some((prog, raw_args)) = parts.split_first() else {
            continue;
        };
        // Follow gh/git convention: `BROWSER=true` is a no-op on every platform (including Windows,
        // where `true` may not exist on PATH).
        if raw_args.is_empty() && prog == "true" {
            return Ok(());
        }
        let Some(resolved) = resolve_browser_program(prog) else {
            continue;
        };
        let mut substituted = false;
        let mut args = Vec::with_capacity(raw_args.len() + 1);
        for a in raw_args {
            let (expanded, did) = expand_browser_arg(a, url);
            substituted |= did;
            args.push(expanded);
        }
        if !substituted {
            args.push(url.to_owned());
        }
        match spawn_detached_browser(&resolved, &args) {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        }
    }
    fallback(url)
}

fn validate_open_url(raw: &str) -> std::io::Result<()> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.starts_with('-') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "mare.proc.open expects an http:// or https:// URL",
        ));
    }
    let parsed = url::Url::parse(trimmed).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "mare.proc.open expects a valid http:// or https:// URL")
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("mare.proc.open only permits http:// and https:// URLs (got {}://)", parsed.scheme()),
        ));
    }
    Ok(())
}

fn open_url(url: &str) -> std::io::Result<()> {
    validate_open_url(url)?;
    let browser = std::env::var("BROWSER").ok();
    open_url_with(url, browser.as_deref(), |u| open::that_detached(u))
}

pub fn install(lua: &Lua, mare: &Table, cwd: String) -> mlua::Result<()> {
    let t = lua.create_table()?;
    // alive(pid, expected_start_time?) -> whether the process (and optionally its start time) exists
    t.set(
        "alive",
        lua.create_function(|_, (pid, expected_start): (u32, Option<u64>)| {
            Ok(super::peers::alive_with_start_time(pid, expected_start))
        })?,
    )?;
    // start_time(pid?) -> process start time token (for PID-reuse-safe liveness checks)
    t.set(
        "start_time",
        lua.create_function(|_, pid: Option<u32>| Ok(super::peers::start_time(pid.unwrap_or_else(std::process::id))))?,
    )?;
    register_spill_ops(lua, &t)?;
    register_env(lua, &t)?;
    register_spawn(lua, &t, cwd.clone())?;
    run::register_run(lua, &t, cwd.clone())?;
    t.set(
        "open",
        lua.create_function(|lua, url: String| {
            crate::exec_ctx::refuse_unconfinable(lua, "mare.proc.open")?;
            open_url(&url).map_err(rt_err)?;
            Ok(())
        })?,
    )?;
    let which_cwd = cwd.clone();
    t.set(
        "which",
        lua.create_function(move |_, name: String| {
            Ok(which_path(&name, std::env::var_os("PATH"), std::path::Path::new(&which_cwd)))
        })?,
    )?;
    manager::install(lua, &t, cwd.clone())?;
    super::ossandbox::install(lua, &t, cwd)?;
    super::shell::install(lua, &t)?;
    mare.set("proc", t)?;
    Ok(())
}

#[cfg(test)]
mod which_tests {
    use super::{first_windows_which_match, open_url_with, which_path, windows_which_path};
    use std::ffi::{OsStr, OsString};
    use std::path::Path;

    #[test]
    fn open_url_respects_browser_true_and_falls_back_when_unset() {
        let mut fallback_calls = 0usize;
        open_url_with("http://127.0.0.1/authorize?client_id=test-client", Some("true"), |_| {
            fallback_calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(fallback_calls, 0, "BROWSER=true must not invoke the system opener");

        open_url_with("http://127.0.0.1/authorize", Some("  true  "), |_| {
            fallback_calls += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(fallback_calls, 0, "whitespace-padded BROWSER=true must remain a no-op");

        let mut seen_url = None;
        open_url_with("http://127.0.0.1/fallback", None, |u| {
            seen_url = Some(u.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(seen_url.as_deref(), Some("http://127.0.0.1/fallback"));

        let mut empty_fallback = false;
        open_url_with("http://127.0.0.1/empty", Some("   "), |_| {
            empty_fallback = true;
            Ok(())
        })
        .unwrap();
        assert!(empty_fallback, "empty BROWSER must fall back to the system opener");

        let err = open_url_with("http://127.0.0.1/bad", Some("\"unclosed"), |_| {
            panic!("invalid BROWSER must fail closed instead of calling system opener")
        })
        .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        let mut missing_fallback = None;
        open_url_with("http://127.0.0.1/missing", Some("definitely-missing-mare-browser-xyz"), |u| {
            missing_fallback = Some(u.to_owned());
            Ok(())
        })
        .unwrap();
        assert_eq!(
            missing_fallback.as_deref(),
            Some("http://127.0.0.1/missing"),
            "missing BROWSER executable must fall back to the system opener"
        );
    }

    #[test]
    fn validate_open_url_allows_http_and_rejects_other_schemes_and_flags() {
        use super::validate_open_url;
        assert!(validate_open_url("http://127.0.0.1:8080/callback").is_ok());
        assert!(validate_open_url("https://example.com/auth?state=1").is_ok());
        for bad in ["", "   ", "-flag", "--open", "file:///etc/passwd", "javascript:alert(1)", "ftp://example.com/x"] {
            let err = validate_open_url(bad).expect_err(&format!("expected rejection for {bad:?}"));
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput, "bad={bad:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_url_executes_custom_browser_command_instead_of_system_opener() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let marker = tmp.path().join("opened-url.txt");
        let script = tmp.path().join("fake-browser.sh");
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s' \"$*\" > '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let url = "http://127.0.0.1:43210/authorize?client_id=test-client";
        // Colon-separated fallback list + `%s` placeholder substitution (xdg convention).
        let browser = format!("missing-first-browser:{} --open=%s %%", script.display());
        open_url_with(url, Some(&browser), |_| {
            panic!("custom BROWSER must bypass the system opener");
        })
        .unwrap();

        let expected = format!("--open={url} %");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Ok(contents) = std::fs::read_to_string(&marker)
                && contents == expected
            {
                return;
            }
            // sleep-ok: bounded poll waiting for detached fake BROWSER script to write its marker file
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("custom BROWSER script did not record {expected:?}; marker={:?}", std::fs::read_to_string(&marker));
    }

    #[tokio::test]
    async fn protected_host_spill_is_refused_before_running() {
        const CHILD: &str = "MARE_TEST_PROC_HOST_SPILL";
        let Ok(workspace) = std::env::var(CHILD) else {
            let ws = tempfile::tempdir().unwrap();
            let home = ws.path().join("mare-home");
            std::fs::create_dir_all(&home).unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "proc::which_tests::protected_host_spill_is_refused_before_running", "--nocapture"])
                .env(CHILD, ws.path())
                .env("MARE_HOME", &home)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "child failed: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("HOST_SPILL_VERIFIED"));
            return;
        };

        let host = mare_shared::paths::host_token_file().parent().unwrap().to_path_buf();
        let spill = host.join("out.log");
        let marker = Path::new(&workspace).join("command-ran");
        assert!(!host.exists());
        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        crate::exec_ctx::install(&lua, &mare).unwrap();
        super::install(&lua, &mare, workspace.clone()).unwrap();
        let spec: mlua::Value = lua
            .load(
                r#"return {level = "workspace-write", network = true,
                accept_partial = {"fs_protect", "fs_host_protect"}}"#,
            )
            .eval()
            .unwrap();
        let policy = crate::ossandbox::Policy::from_lua(&lua, &spec, Path::new(&workspace)).unwrap().unwrap();
        let error = super::checked_spill_path(&lua, &spill.to_string_lossy(), None, Some(&policy)).unwrap_err();
        assert!(error.to_string().contains("sandbox: spill path intersects a protected or deny_read path"), "{error}");
        lua.load(
            r#"
            local cwd, spill, windows = ...
            -- This probe checks the in-process spill gate even on a backend that cannot
            -- protect workspace metadata itself; the command must never be started.
            local policy = {level = "workspace-write", network = true,
                accept_partial = {"fs_protect", "fs_host_protect"}}
            local no_backend = mare.proc.sandbox.capabilities(cwd).mechanism == "none"
            local function run(sandbox)
                return mare.proc.run({
                    cmd = windows and "cmd" or "sh",
                    args = { windows and "/c" or "-c",
                        windows and "type nul > command-ran" or "touch command-ran" },
                    cwd = cwd, spill = spill, sandbox = sandbox,
                })
            end
            local function refused(sandbox, allow_preflight)
                local ok, err = pcall(run, sandbox)
                local text = tostring(err)
                assert(not ok and (text:find("sandbox: spill path intersects a protected or deny_read path", 1, true)
                    or (allow_preflight and (text:find("SANDBOX_UNAVAILABLE", 1, true)
                        or text:find("SANDBOX_INSUFFICIENT", 1, true)))), text)
            end
            refused("workspace-write", true)
            refused(policy, no_backend)
            mare.exec.run({cwd = cwd, sandbox = policy}, function()
                refused("workspace-write", true)
                refused(policy, no_backend)
            end)
        "#,
        )
        .call_async::<()>((workspace, spill.to_string_lossy().to_string(), cfg!(windows)))
        .await
        .unwrap();
        assert!(!marker.exists(), "command ran");
        assert!(!spill.exists(), "spill file was created under the protected host directory");
        if host.exists() {
            assert_eq!(std::fs::read_dir(&host).unwrap().count(), 0, "protected host directory contains a file");
        }
        println!("HOST_SPILL_VERIFIED");
    }

    fn exe(dir: &Path, name: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    }

    #[test]
    fn finds_an_executable_on_path() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let file = "mare-which-probe.exe";
        #[cfg(not(windows))]
        let file = "mare-which-probe";
        let p = exe(dir.path(), file);
        let found = which_path("mare-which-probe", Some(OsString::from(dir.path())), dir.path()).unwrap();
        assert_eq!(dunce::canonicalize(std::path::PathBuf::from(found)).unwrap(), dunce::canonicalize(&p).unwrap());
    }

    #[test]
    fn missing_name_is_nil() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(which_path("mare-which-missing", Some(OsString::from(dir.path())), dir.path()), None);
        assert_eq!(which_path("", Some(OsString::from(dir.path())), dir.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_file_is_nil() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("plain-file");
        std::fs::write(&p, "data").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(which_path("plain-file", Some(OsString::from(dir.path())), dir.path()), None);
        // as a path too
        assert_eq!(which_path("./plain-file", None, dir.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn which_refuses_lossy_non_utf8_first_match_even_if_later_path_is_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let non_utf8 = dir.path().join(OsString::from_vec(b"bin-\xff".to_vec()));
        let valid = dir.path().join("valid");
        std::fs::create_dir_all(&valid).unwrap();
        let valid_file = exe(&valid, "probe");
        let paths = std::env::join_paths([&non_utf8, &valid]).unwrap();

        match std::fs::create_dir_all(&non_utf8) {
            Ok(()) => {
                exe(&non_utf8, "probe");
                assert_eq!(which_path("probe", Some(paths), dir.path()), None);
            }
            // macOS APFS rejects non-UTF-8 filenames with EILSEQ; exercise the exact same
            // `which_unix_with` non-UTF-8 hit refusal via a custom `which::sys::Sys` shim.
            Err(e) if e.raw_os_error() == Some(libc::EILSEQ) => {
                struct NonUtf8FirstSys {
                    fake_hit: std::path::PathBuf,
                    backing_exe: std::path::PathBuf,
                }
                impl which::sys::Sys for NonUtf8FirstSys {
                    type ReadDirEntry = std::fs::DirEntry;
                    type Metadata = std::fs::Metadata;
                    fn is_windows(&self) -> bool {
                        false
                    }
                    fn current_dir(&self) -> std::io::Result<std::path::PathBuf> {
                        which::sys::Sys::current_dir(&super::UnixWhichSys)
                    }
                    fn home_dir(&self) -> Option<std::path::PathBuf> {
                        which::sys::Sys::home_dir(&super::UnixWhichSys)
                    }
                    fn env_split_paths(&self, paths: &OsStr) -> Vec<std::path::PathBuf> {
                        which::sys::Sys::env_split_paths(&super::UnixWhichSys, paths)
                    }
                    fn env_path(&self) -> Option<OsString> {
                        None
                    }
                    fn env_path_ext(&self) -> Option<OsString> {
                        None
                    }
                    fn metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
                        let target = if path == self.fake_hit { &self.backing_exe } else { path };
                        which::sys::Sys::metadata(&super::UnixWhichSys, target)
                    }
                    fn symlink_metadata(&self, path: &Path) -> std::io::Result<Self::Metadata> {
                        let target = if path == self.fake_hit { &self.backing_exe } else { path };
                        which::sys::Sys::symlink_metadata(&super::UnixWhichSys, target)
                    }
                    fn read_dir(
                        &self,
                        path: &Path,
                    ) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<Self::ReadDirEntry>>>>
                    {
                        which::sys::Sys::read_dir(&super::UnixWhichSys, path)
                    }
                    fn is_valid_executable(&self, path: &Path) -> std::io::Result<bool> {
                        if path == self.fake_hit {
                            Ok(true)
                        } else {
                            which::sys::Sys::is_valid_executable(&super::UnixWhichSys, path)
                        }
                    }
                }
                let sys = NonUtf8FirstSys { fake_hit: non_utf8.join("probe"), backing_exe: valid_file.clone() };
                assert_eq!(super::which_unix_with("probe", Some(paths), dir.path(), &sys), None);
            }
            Err(e) => panic!("failed to create non-UTF-8 PATH dir: {e}"),
        }
        assert_eq!(
            which_path("probe", Some(valid.into_os_string()), dir.path()),
            valid_file.into_os_string().into_string().ok()
        );
    }

    #[test]
    fn windows_which_filters_explicit_extension_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let file = exe(dir.path(), "build.Py");
        let name = format!(".{}build.Py", std::path::MAIN_SEPARATOR);
        assert_eq!(windows_which_path(&name, None, dir.path(), Some(OsStr::new(".EXE;.CMD"))), None);
        assert_eq!(windows_which_path(&name, None, dir.path(), Some(OsStr::new(".EXE;.pY"))), Some(file.clone()));
        assert_eq!(windows_which_path(file.to_str().unwrap(), None, dir.path(), Some(OsStr::new(".EXE;.CMD"))), None);
        assert_eq!(
            windows_which_path(file.to_str().unwrap(), None, dir.path(), Some(OsStr::new(".EXE;.PY"))),
            Some(file)
        );
    }

    #[test]
    fn windows_which_defaults_when_pathext_is_absent_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        for ext in ["COM", "EXE", "BAT", "CMD"] {
            let name = format!("tool-{ext}");
            let file = exe(dir.path(), &format!("{name}.{ext}"));
            for pathext in [None, Some(OsStr::new(""))] {
                assert_eq!(windows_which_path(&name, Some(dir.path().into()), dir.path(), pathext), Some(file.clone()));
            }
        }
    }

    #[test]
    fn windows_which_skips_a_non_executable_candidate_before_a_later_match() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        exe(&first, "tool");
        let ordinary = exe(&first, "tool.py");
        let executable = exe(&second, "tool.CMD");
        assert_eq!(
            first_windows_which_match([ordinary, executable.clone()].into_iter(), &[".EXE".into(), ".CMD".into()]),
            Some(executable.clone())
        );
        let paths = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(
            windows_which_path("tool", Some(paths), dir.path(), Some(OsStr::new(".EXE;.CMD"))),
            Some(executable)
        );
    }

    #[test]
    fn windows_which_rejects_extensionless_explicit_paths() {
        let dir = tempfile::tempdir().unwrap();
        exe(dir.path(), "bare");
        let name = format!(".{}bare", std::path::MAIN_SEPARATOR);
        assert_eq!(windows_which_path(&name, None, dir.path(), Some(OsStr::new(".EXE;.CMD"))), None);
    }

    #[test]
    fn a_path_is_checked_directly_relative_to_cwd() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        let file = "tool.exe";
        #[cfg(not(windows))]
        let file = "tool";
        let p = exe(dir.path(), file);
        let rel = format!(".{}{}", std::path::MAIN_SEPARATOR, file);
        // no PATH at all: a path never goes through PATH
        let found = which_path(&rel, None, dir.path()).unwrap();
        assert!(Path::new(&found).is_absolute(), "{found}");
        assert_eq!(dunce::canonicalize(Path::new(&found)).unwrap(), dunce::canonicalize(&p).unwrap());
        let abs = which_path(p.to_str().unwrap(), None, Path::new("/")).unwrap();
        assert_eq!(dunce::canonicalize(Path::new(&abs)).unwrap(), dunce::canonicalize(&p).unwrap());
    }

    // A relative working directory (e.g. a sub-agent's) is joined once.
    #[test]
    fn a_relative_working_directory_is_joined_once() {
        let here = std::env::current_dir().unwrap();
        let dir = tempfile::Builder::new().prefix("which-rel-").tempdir_in(&here).unwrap();
        #[cfg(windows)]
        let file = "gradlew.exe";
        #[cfg(not(windows))]
        let file = "gradlew";
        let p = exe(dir.path(), file);
        let rel_cwd = Path::new(dir.path().file_name().unwrap());
        let found = which_path(&format!(".{}{}", std::path::MAIN_SEPARATOR, file), None, rel_cwd).unwrap();
        let found = Path::new(&found);
        assert!(found.is_absolute(), "{found:?}");
        assert!(found.exists(), "{found:?}");
        assert_eq!(dunce::canonicalize(found).unwrap(), dunce::canonicalize(&p).unwrap());
        let name = rel_cwd.to_str().unwrap();
        assert_eq!(found.to_str().unwrap().matches(name).count(), 1, "{found:?}");
    }

    // PATHEXT only applies on Windows (the which crate matches a bare name against PATH entries
    // with each PATHEXT extension there); on other systems an extension is part of the name.
    #[cfg(windows)]
    #[test]
    fn pathext_matches_a_bare_name_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        // The ordinary Windows PATHEXT includes .CMD; this test does not modify the environment.
        let p = exe(dir.path(), "batchtool.cmd");
        let found = which_path("batchtool", Some(OsString::from(dir.path())), dir.path()).unwrap();
        assert_eq!(dunce::canonicalize(Path::new(&found)).unwrap(), dunce::canonicalize(&p).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_which_respects_pathext_in_fresh_processes() {
        const CHILD: &str = "MARE_TEST_WHICH_PATHEXT_ROOT";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = Path::new(&root);
            let first = root.join("first");
            let second = root.join("second");
            let python = first.join("tool.py");
            let command = second.join("tool.cmd");
            let allows_python = std::env::var_os("MARE_TEST_WHICH_ALLOW_PY").is_some();
            assert_eq!(which_path(python.to_str().unwrap(), None, root).is_some(), allows_python);
            let paths = std::env::join_paths([&first, &second]).unwrap();
            let found = which_path("tool", Some(paths), root).expect("eligible executable should be found");
            let expected = if allows_python { python } else { command };
            assert_eq!(dunce::canonicalize(found).unwrap(), dunce::canonicalize(expected).unwrap());
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        exe(&first, "tool.py");
        exe(&second, "tool.cmd");
        for (label, pathext, allow_python) in [
            ("unset", None, false),
            ("empty", Some(""), false),
            ("standard", Some(".EXE;.CMD"), false),
            ("python", Some(".pY;.cMd"), true),
        ] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap());
            child.args([
                "--exact",
                "proc::which_tests::windows_which_respects_pathext_in_fresh_processes",
                "--nocapture",
            ]);
            child.env(CHILD, root.path());
            if let Some(pathext) = pathext {
                child.env("PATHEXT", pathext);
            } else {
                child.env_remove("PATHEXT");
            }
            if allow_python {
                child.env("MARE_TEST_WHICH_ALLOW_PY", "1");
            } else {
                child.env_remove("MARE_TEST_WHICH_ALLOW_PY");
            }
            let output = child.output().unwrap();
            assert!(output.status.success(), "{label}: {}", String::from_utf8_lossy(&output.stderr));
        }
    }
}
