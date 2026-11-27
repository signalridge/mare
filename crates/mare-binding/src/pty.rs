//! `mare.pty`: pseudo-terminal processes.
//! Reads happen on a blocking thread (or a task) and arrive through a channel, so Lua awaits them.
//!
//! Under a sandbox policy (a confined tool call, or an explicit `sandbox` option) the terminal's
//! child is confined like `mare.proc`:
//! - Seatbelt / bubblewrap: the wrapper itself runs inside the PTY and execs the command (under
//!   bubblewrap with a restricted network, plus the seccomp Unix-socket filter `mare.proc` adds);
//! - Landlock: the ruleset and seccomp filter go on in the terminal's own pre-exec step, as for
//!   `mare.proc`; the terminal device (`/dev/tty`) stays writable;
//! - Windows (restricted token / sandbox account): the console can't be shared with the confined
//!   process, so the sandbox runner creates the ConPTY itself and bridges it over its stdio
//!   ([`crate::ptybridge`]: raw output back, framed input + resizes in);

use std::io::{Read, Write};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use mlua::{Lua, Table, UserData, UserDataMethods, Value};

use super::ossandbox::{Backend, Prepared};
#[cfg(unix)]
use super::platform::PreExec;
use super::platform::{self, Terminal, TerminalCommand};
/// no pre-exec step on Windows (a confined terminal is bridged by the sandbox runner)
#[cfg(windows)]
type PreExec = std::convert::Infallible;
use tokio::sync::{Mutex as AsyncMutex, mpsc};

pub(crate) mod lease;
mod output;
mod read;
mod view;
const QUEUE_CHUNKS: usize = 32;
const MAX_WRITE: usize = 16 * 1024;

use mare_shared::lua::rt_err;

enum Inner {
    /// a terminal mare created and started itself ([`Terminal`]: contained from the first
    /// instruction — session + PDEATHSIG on Unix, a job on Windows)
    Native {
        term: Mutex<Terminal>,
        /// input for the writer thread: a program that isn't reading fills the terminal's
        /// input queue, and a blocking write must never stall the (single-threaded) runtime
        ///
        writer: std::sync::mpsc::SyncSender<Vec<u8>>,
    },
    /// sandbox runner owning the ConPTY: frames to its stdin, terminal output from its stdout
    Bridged { input: mpsc::Sender<Vec<u8>>, child: Box<Mutex<tokio::process::Child>>, pid: Option<u32> },
}

pub struct Pty {
    inner: Inner,
    rx: AsyncMutex<mpsc::Receiver<Vec<u8>>>,
    /// policy + network proxy: live as long as the terminal
    _sandbox: Option<Prepared>,
    /// the terminal's whole tree ends with this object, the host or mare: closing
    /// the terminal only hangs up its foreground group
    _tree: super::trees::Tree,
    lease: lease::Lease,
    filter: Mutex<mare_shared::ansi::StreamFilter>,
    view: Mutex<view::View>,
    view_enabled: bool,
    closed: Arc<AtomicBool>,
}

/// Validate pseudo-terminal dimensions before spawning or resizing.
pub(crate) fn validate_size(cols: u16, rows: u16) -> mlua::Result<()> {
    if cols == 0 || rows == 0 || u32::from(cols) * u32::from(rows) > 200_000 {
        return Err(rt_err("PTY_SIZE: dimensions must be positive"));
    }
    Ok(())
}

impl Pty {
    /// Write raw bytes to the terminal input.
    pub fn write(&self, data: &[u8]) -> mlua::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(rt_err("PTY_CLOSED: terminal was stopped"));
        }
        if data.len() > MAX_WRITE {
            return Err(rt_err("PTY_INPUT_LIMIT: write exceeds 16384 bytes"));
        }
        match &self.inner {
            Inner::Native { writer, .. } => writer
                .try_send(data.to_vec())
                .map_err(|_| rt_err("PTY_INPUT_UNAVAILABLE: terminal closed or input queue full")),
            Inner::Bridged { input, .. } => input
                .try_send(crate::ptybridge::encode_data(data))
                .map_err(|_| rt_err("PTY_INPUT_UNAVAILABLE: terminal closed or input queue full")),
        }
    }

    /// Resize the pseudo-terminal window.
    pub fn resize(&self, cols: u16, rows: u16) -> mlua::Result<()> {
        validate_size(cols, rows)?;
        match &self.inner {
            Inner::Native { term, .. } => term.lock().unwrap().resize(cols, rows).map_err(rt_err),
            Inner::Bridged { input, .. } => input
                .try_send(crate::ptybridge::encode_resize(cols, rows))
                .map_err(|_| rt_err("PTY_INPUT_UNAVAILABLE: terminal closed or input queue full")),
        }
    }

    /// Exit code once the process ended.
    pub fn try_wait(&self) -> mlua::Result<Option<u32>> {
        match &self.inner {
            Inner::Native { term, .. } => term.lock().unwrap().try_wait().map_err(rt_err),
            Inner::Bridged { child, .. } => {
                Ok(child.lock().unwrap().try_wait().map_err(rt_err)?.map(|s| s.code().unwrap_or(1) as u32))
            }
        }
    }

    /// Poll briefly after PTY output EOF until the child exit status becomes visible.
    pub(crate) async fn wait_exit_after_eof(&self, mut should_stop: impl FnMut() -> bool) -> Option<i32> {
        let mut exit_code = self.try_wait().ok().flatten().map(|c| c as i32);
        if exit_code.is_none() && !should_stop() {
            for _ in 0..100 {
                // sleep-ok: brief bounded poll for kernel exit status propagation after PTY EOF
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                if should_stop() {
                    break;
                }
                if let Some(code) = self.try_wait().ok().flatten() {
                    exit_code = Some(code as i32);
                    break;
                }
            }
        }
        exit_code
    }

    /// Kill the terminal process tree.
    pub fn kill(&self) -> mlua::Result<()> {
        self.closed.store(true, Ordering::Release);
        match &self.inner {
            Inner::Native { term, .. } => {
                // the pty child leads its own session/group: kill the session leader first so it
                // cannot fork another job-control child between session enumeration and death,
                // then signal the whole session/group.
                let mut t = term.lock().unwrap();
                let pid = t.pid();
                let res = t.kill().map_err(rt_err);
                platform::signal_tree(Some(pid), platform::Signal::Kill);
                res
            }
            Inner::Bridged { child, pid, .. } => {
                // the runner and the confined command share one tree (job / process group)
                platform::signal_tree(*pid, platform::Signal::Kill);
                let _ = child.lock().unwrap().start_kill();
                Ok(())
            }
        }
    }

    fn foreground(&self, lua: &Lua) -> mlua::Result<Option<Table>> {
        let value = match &self.inner {
            Inner::Native { term, .. } => term.lock().unwrap().foreground().map_err(rt_err)?,
            Inner::Bridged { .. } => None,
        };
        value
            .map(|(pgid, waiting)| {
                let t = lua.create_table()?;
                t.set("pgid", pgid)?;
                t.set("input_waiting", waiting)?;
                Ok(t)
            })
            .transpose()
    }

    /// Child process ID when known.
    pub fn pid(&self) -> Option<u32> {
        match &self.inner {
            Inner::Native { term, .. } => Some(term.lock().unwrap().pid()),
            Inner::Bridged { pid, .. } => *pid,
        }
    }

    /// Wait until the underlying process has an exit status recorded (Unix native PTY).
    async fn wait_exit(&self) {
        #[cfg(unix)]
        if let Inner::Native { term, .. } = &self.inner {
            let mut rx = term.lock().unwrap().exit_watcher();
            let _ = rx.wait_for(|v| v.is_some()).await;
        }
    }

    /// Read the next output chunk from the terminal, returning `None` at EOF/exit.
    pub async fn read_chunk(&self) -> Option<Vec<u8>> {
        let mut rx = self.rx.lock().await;
        let mut exit_grace: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
        loop {
            tokio::select! {
                biased;
                chunk = rx.recv() => {
                    if chunk.is_none() {
                        self.wait_exit().await;
                    }
                    return chunk;
                }
                _ = async {
                    match exit_grace.as_mut() {
                        Some(s) => s.as_mut().await,
                        None => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
                    }
                } => {
                    if exit_grace.is_some() {
                        return rx.try_recv().ok();
                    }
                    if self.try_wait().ok().flatten().is_some() {
                        // Give the PTY reader thread a bounded grace window while keeping `rx.recv()` active.
                        exit_grace = Some(Box::pin(tokio::time::sleep(std::time::Duration::from_millis(400))));
                    }
                }
            }
        }
    }

    /// Prepared sandbox handle for this terminal, if confined.
    pub(crate) fn sandbox_prepared(&self) -> Option<&Prepared> {
        self._sandbox.as_ref()
    }

    /// Spawn a PTY process directly from an explicit sandbox [`super::ossandbox::Policy`].
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_confined(
        prog: &str,
        args: &[String],
        cwd: &str,
        env: &[(String, String)],
        cols: u16,
        rows: u16,
        policy: Option<super::ossandbox::Policy>,
        owner: Option<super::trees::Owner>,
        approver: Option<super::netauth::NetApprover>,
    ) -> mlua::Result<Self> {
        validate_size(cols, rows)?;
        let sandbox =
            policy.map(|p| Prepared::from_policy_with_approver(p, std::path::Path::new(cwd), approver)).transpose()?;
        let s = Spec {
            cmd: prog.to_string(),
            args: args.to_vec(),
            cwd: cwd.to_string(),
            env: env.to_vec(),
            clear_env: false,
            env_policy: Default::default(),
            cols,
            rows,
            buffer_bytes: 256 * 1024,
            scrollback_lines: 2000,
            emulate: false,
        };
        Self::spawn_from_spec(&s, sandbox, owner, lease::Lease::unmanaged())
    }

    /// Spawn a PTY process for `mare.proc.start({ tty = true, ... })`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn_for_job(
        prog: &str,
        args: &[String],
        cwd: &str,
        env: &[(String, String)],
        env_policy: super::subenv::EnvPolicy,
        clear_env: bool,
        cols: u16,
        rows: u16,
        sandbox: Option<Prepared>,
        owner: Option<super::trees::Owner>,
        lease: lease::Lease,
    ) -> mlua::Result<Self> {
        validate_size(cols, rows)?;
        let s = Spec {
            cmd: prog.to_string(),
            args: args.to_vec(),
            cwd: cwd.to_string(),
            env: env.to_vec(),
            clear_env,
            env_policy,
            cols,
            rows,
            buffer_bytes: 256 * 1024,
            scrollback_lines: 2000,
            emulate: false,
        };
        Self::spawn_from_spec(&s, sandbox, owner, lease)
    }

    pub(crate) fn register_lease(&self, lua: &Lua) {
        self.lease.register(lua);
    }

    pub(crate) fn check_lease(&self, lua: &Lua) -> mlua::Result<()> {
        self.lease.check(lua)
    }

    fn spawn_from_spec(
        s: &Spec,
        sandbox: Option<Prepared>,
        owner: Option<super::trees::Owner>,
        mut lease: lease::Lease,
    ) -> mlua::Result<Self> {
        let mut view = view::View::new(s.rows, s.cols)?;
        view.limits(s.buffer_bytes, s.scrollback_lines);
        let (tx, rx, closed) = output::channel();
        let (inner, _tree) = match &sandbox {
            None => {
                let inner = spawn_native(s, &s.cmd, &s.args, &[], None, tx, None)?;
                let pid = match &inner {
                    Inner::Native { term, .. } => Some(term.lock().unwrap().pid()),
                    Inner::Bridged { pid, .. } => *pid,
                };
                (inner, super::trees::Tree::new(pid, owner))
            }
            Some(p) => match p.backend {
                Backend::WinAcl | Backend::WinUser => spawn_bridged(s, p, tx, owner)?,
                // Seatbelt / bubblewrap: the wrapper runs in the PTY and execs the command confined;
                // Landlock: the command itself, restricted in the terminal's pre-exec step
                _ => {
                    #[allow(unused_mut)]
                    let mut w = p.wrap_monitored(&s.cmd, &s.args);
                    // a terminal program reopens its controlling terminal by name (`/dev/tty`: ssh,
                    // sudo, editors); the device is its own, so writing it confines nothing
                    if let Some(g) = w.landlock.as_mut() {
                        g.rw.push("/dev/tty".into());
                    }
                    let restrict = pre_exec_restrict(&w)?;
                    let inner = spawn_native(s, &w.prog, &w.args, &w.env, restrict, tx, Some(p))?;
                    let pid = match &inner {
                        Inner::Native { term, .. } => Some(term.lock().unwrap().pid()),
                        Inner::Bridged { pid, .. } => *pid,
                    };
                    (inner, super::trees::Tree::new(pid, owner))
                }
            },
        };
        let pid = match &inner {
            Inner::Native { term, .. } => Some(term.lock().unwrap().pid()),
            Inner::Bridged { pid, .. } => *pid,
        };
        lease.publish(pid);
        Ok(Self {
            inner,
            rx: AsyncMutex::new(rx),
            _sandbox: sandbox,
            _tree,
            lease,
            filter: Mutex::new(mare_shared::ansi::StreamFilter::new(false)),
            view: Mutex::new(view),
            view_enabled: s.emulate,
            closed,
        })
    }
}

impl UserData for Pty {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("write", |lua, this, data: mlua::LuaString| {
            crate::exec_lifetime::check_mutation(lua, "pty write")?;
            this.lease.check(lua)?;
            this.write(&data.as_bytes())
        });
        read::methods(m);
        m.add_method("foreground", |lua, this, ()| {
            this.lease.check(lua)?;
            this.foreground(lua)
        });
        m.add_method("signal", |lua, this, (signal, opts): (String, Option<Table>)| {
            crate::exec_lifetime::check_mutation(lua, "pty signal")?;
            let protected_pgid = opts.as_ref().map(|t| t.get::<Option<u32>>("protected_pgid")).transpose()?.flatten();
            this.lease.check(lua)?;
            match &this.inner {
                Inner::Native { term, .. } => {
                    term.lock().unwrap().signal_foreground(&signal, protected_pgid).map_err(rt_err)
                }
                Inner::Bridged { .. } => Err(rt_err("PTY_SIGNAL_UNSUPPORTED: foreground signalling unavailable")),
            }
        });
        m.add_method("resize", |lua, this, (cols, rows): (u16, u16)| {
            crate::exec_lifetime::check_mutation(lua, "pty resize")?;
            this.lease.check(lua)?;
            validate_size(cols, rows)?;
            this.resize(cols, rows)?;
            this.view.lock().unwrap().resize(rows, cols);
            Ok(())
        });
        m.add_method("kill", |lua, this, ()| {
            crate::exec_lifetime::check_mutation(lua, "pty kill")?;
            this.kill()
        });
        // stop{grace_ms?} -> {stopped, stop_unconfirmed}: SIGTERM the tree, wait grace_ms
        // (default 3000), SIGKILL / end the job, then wait (bounded) for the exit and for the rest
        // of the group. Unconfirmed: don't reuse this terminal.
        m.add_async_method("stop", |lua, this, opts: Option<Table>| async move {
            crate::exec_lifetime::check_mutation(&lua, "pty stop")?;
            let grace_ms = opts.map(|o| o.get::<Option<u64>>("grace_ms")).transpose()?.flatten();
            if grace_ms.is_some_and(|ms| ms > 30_000) {
                return Err(rt_err("PTY_STOP_LIMIT: grace_ms exceeds 30000"));
            }
            let grace = grace_ms.map(std::time::Duration::from_millis).unwrap_or(crate::proc::STOP_GRACE);
            let pid = this.pid();
            let exited = |this: &Self| matches!(this.try_wait(), Ok(Some(_)));
            async fn until(d: std::time::Duration, mut f: impl FnMut() -> bool) -> bool {
                let end = tokio::time::Instant::now() + d;
                loop {
                    if f() {
                        return true;
                    }
                    if tokio::time::Instant::now() >= end {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            }
            // the interrupt character first, as a user's Ctrl+C would (on Windows the only
            // interrupt a ConPTY child understands), then the hard stop
            let _ = this.write(b"\x03");
            let mut done = until(std::time::Duration::from_millis(500).min(grace), || exited(&this)).await;
            if !done {
                platform::signal_tree(pid, platform::Signal::Term);
                done = until(grace, || exited(&this)).await;
            }
            let tree_alive = || pid.is_some_and(crate::trees::tree_has_live_member);
            if !done || tree_alive() {
                let _ = this.kill();
            }
            this.closed.store(true, Ordering::Release);
            if !done {
                done = until(crate::proc::REAP_TIMEOUT, || exited(&this)).await;
            }
            let gone = until(crate::proc::REAP_TIMEOUT, || {
                if !tree_alive() {
                    return true;
                }
                // Re-signal surviving session members in case a shell forked a new job-control
                // process group concurrently with the first SIGKILL.
                platform::signal_tree(pid, platform::Signal::Kill);
                false
            })
            .await;
            let confirmed = done && gone;
            let t = lua.create_table()?;
            t.set("stopped", confirmed)?;
            t.set("stop_unconfirmed", !confirmed)?;
            Ok(t)
        });
        m.add_method("exit_status", |lua, this, ()| {
            let value = match &this.inner {
                Inner::Native { term, .. } => term.lock().unwrap().exit_status().map_err(rt_err)?,
                Inner::Bridged { child, .. } => child.lock().unwrap().try_wait().map_err(rt_err)?.map(|s| {
                    #[cfg(unix)]
                    {
                        use std::os::unix::process::ExitStatusExt;
                        (s.code().map(|v| v as u32), s.signal())
                    }
                    #[cfg(windows)]
                    {
                        (s.code().map(|v| v as u32), None::<i32>)
                    }
                }),
            };
            value
                .map(|(code, signal)| {
                    let out = lua.create_table()?;
                    out.set("code", code)?;
                    out.set("signal", signal)?;
                    Ok(out)
                })
                .transpose()
        });
        // try_wait() -> exit code | nil while running
        m.add_method("try_wait", |_, this, ()| this.try_wait());
        m.add_method("pid", |_, this, ()| Ok(this.pid()));
        // sandbox() -> what the confinement actually enforces (nil when unconfined)
        m.add_method("sandbox", |lua, this, ()| this._sandbox.as_ref().map(|p| p.report(lua)).transpose());
    }
}

struct Spec {
    cmd: String,
    args: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    clear_env: bool,
    env_policy: super::subenv::EnvPolicy,
    cols: u16,
    rows: u16,
    buffer_bytes: usize,
    scrollback_lines: usize,
    emulate: bool,
}

fn spec(opts: &Table, default_cwd: &str) -> mlua::Result<Spec> {
    let mut env = vec![];
    if let Some(t) = opts.get::<Option<Table>>("env")? {
        for p in t.pairs::<String, String>() {
            env.push(p?);
        }
    }
    Ok(Spec {
        cmd: opts.get("cmd")?,
        args: opts.get::<Option<Vec<String>>>("args")?.unwrap_or_default(),
        cwd: opts.get::<Option<String>>("cwd")?.unwrap_or_else(|| default_cwd.to_string()),
        env,
        clear_env: opts.get::<Option<bool>>("clear_env")?.unwrap_or(false),
        env_policy: super::subenv::policy_from(opts)?,
        cols: opts.get::<Option<u16>>("cols")?.unwrap_or(80),
        rows: opts.get::<Option<u16>>("rows")?.unwrap_or(24),
        buffer_bytes: opts.get::<Option<usize>>("buffer_bytes")?.unwrap_or(4 * 1024 * 1024),
        scrollback_lines: opts.get::<Option<usize>>("scrollback_lines")?.unwrap_or(10000),
        emulate: opts.get::<Option<bool>>("emulate")?.unwrap_or(false),
    })
}

#[cfg(test)]
thread_local! {
    static FAIL_PTY_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn spawn_pty_worker<F>(name: &str, f: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(test)]
    if FAIL_PTY_WORKER.with(|flag| flag.replace(false)) {
        return Err(std::io::Error::other("injected PTY worker failure"));
    }
    std::thread::Builder::new().name(name.into()).spawn(f)
}

/// Never return a failed PTY initialization while its child is still running.
fn stop_failed_terminal(term: &mut Terminal) {
    let pid = Some(term.pid());
    platform::signal_tree(pid, platform::Signal::Kill);
    let _ = term.kill();
    loop {
        if matches!(term.try_wait(), Ok(Some(_))) && !super::ossandbox::failed_spawn_tree_alive(pid) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A PTY mare opens and starts `prog args…` on ([`Terminal`]); output goes to `tx`.
fn spawn_native(
    s: &Spec,
    prog: &str,
    args: &[String],
    extra_env: &[(String, String)],
    restrict: Option<PreExec>,
    tx: output::Output,
    sandbox: Option<&Prepared>,
) -> mlua::Result<Inner> {
    // scrubbed parent environment first, explicit variables after
    let mut base_env: Vec<(std::ffi::OsString, std::ffi::OsString)> =
        if s.clear_env { Vec::new() } else { super::subenv::scrubbed_parent_env(&s.env_policy) };
    if sandbox.is_some() {
        super::subenv::strip_askpass_env(&mut base_env);
    }
    let env = base_env.into_iter().chain(s.env.iter().chain(extra_env).map(|(k, v)| (k.into(), v.into()))).collect();
    let c = TerminalCommand { prog, args, cwd: &s.cwd, env, cols: s.cols, rows: s.rows };
    let monitor = sandbox.map(|sb| sb.start_monitor()).transpose()?.flatten();
    #[cfg(unix)]
    let started = match restrict {
        Some(r) => Terminal::spawn_restricted(c, r),
        None => Terminal::spawn(c),
    };
    #[cfg(windows)]
    let started = match restrict {
        Some(never) => match never {},
        None => Terminal::spawn(c),
    };
    let (mut term, mut reader, mut w) = match started {
        Ok(started) => started,
        Err(e) => {
            if let Some(monitor) = &monitor {
                monitor.after_kill();
            }
            return Err(rt_err(format!("spawn {prog}: {e}")));
        }
    };
    if let Some(monitor) = monitor
        && let Err(e) = monitor.attach(Some(term.pid()))
    {
        stop_failed_terminal(&mut term);
        monitor.after_kill();
        return Err(rt_err(e));
    }
    let (writer, input) = std::sync::mpsc::sync_channel::<Vec<u8>>(QUEUE_CHUNKS);
    let writer_thread = spawn_pty_worker("mare-pty-writer", move || {
        // ends when the terminal is dropped (sender gone) or the program closed it
        while let Ok(data) = input.recv() {
            if w.write_all(&data).and_then(|()| w.flush()).is_err() {
                break;
            }
        }
    });
    if let Err(e) = writer_thread {
        stop_failed_terminal(&mut term);
        return Err(rt_err(e));
    }
    let reader_thread = spawn_pty_worker("mare-pty", move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // EOF, or EIO once the program closed the slave (Linux)
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    if let Err(e) = reader_thread {
        stop_failed_terminal(&mut term);
        return Err(rt_err(e));
    }
    Ok(Inner::Native { term: Mutex::new(term), writer })
}

/// The Windows sandbox runner in PTY mode, on pipes: it owns the ConPTY.
fn spawn_bridged(
    s: &Spec,
    p: &Prepared,
    tx: output::Output,
    owner: Option<super::trees::Owner>,
) -> mlua::Result<(Inner, super::trees::Tree)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let w = p.wrap_with(&s.cmd, &s.args, Some((s.cols, s.rows)));
    let mut cmd = tokio::process::Command::new(&w.prog);
    cmd.args(&w.args).current_dir(&s.cwd).env_clear();
    if !s.clear_env {
        let mut base = super::subenv::scrubbed_parent_env(&s.env_policy);
        super::subenv::strip_askpass_env(&mut base);
        cmd.envs(base);
    }
    cmd.envs(s.env.iter().cloned())
        .envs(w.env.iter().cloned())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    <platform::Process as platform::ProcessBackend>::isolate(&mut cmd);
    let mut child = platform::spawn(&mut cmd).map_err(|e| rt_err(format!("spawn {}: {e}", w.prog)))?;
    let pid = child.id();
    let tree = super::trees::Tree::new(pid, owner);
    let (input, mut input_rx) = mpsc::channel::<Vec<u8>>(QUEUE_CHUNKS);
    let mut stdin = child.stdin.take().ok_or_else(|| rt_err("runner stdin"))?;
    tokio::spawn(async move {
        while let Some(frame) = input_rx.recv().await {
            if stdin.write_all(&frame).await.is_err() || stdin.flush().await.is_err() {
                break;
            }
        }
    });
    let mut stdout = child.stdout.take().ok_or_else(|| rt_err("runner stdout"))?;
    let out_tx = tx.clone();
    tokio::spawn(async move {
        let mut buf = vec![0u8; 8192];
        loop {
            match stdout.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
    // the runner's own diagnostics (sandbox failed to start) show up in the terminal output
    if let Some(mut stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 || tx.send(buf[..n].to_vec()).await.is_err() {
                    break;
                }
            }
        });
    }
    Ok((Inner::Bridged { input, child: Box::new(Mutex::new(child)), pid }, tree))
}

/// The sandbox restrictions a confined child gets right before exec, matching `proc/build.rs`:
/// baseline seccomp is applied unconditionally on Linux.
#[cfg(target_os = "linux")]
fn pre_exec_restrict(w: &super::ossandbox::Wrapped) -> mlua::Result<Option<PreExec>> {
    let rw = w.landlock.clone();
    let filter = Some(super::ossandbox::seccomp_filter(w.socket_block).map_err(rt_err)?);
    let lifecycle_fds = w.lifecycle_fds;
    Ok(Some(Box::new(move || {
        if let Some(fds) = lifecycle_fds {
            super::ossandbox::inherit_lifecycle(fds)?;
        }
        super::ossandbox::child_restrict(rw.as_ref(), filter.as_deref())
    })))
}

/// Only Linux backends restrict in the child (Seatbelt wraps; Windows bridges).
#[cfg(not(target_os = "linux"))]
fn pre_exec_restrict(w: &super::ossandbox::Wrapped) -> mlua::Result<Option<PreExec>> {
    let _ = (&w.landlock, w.socket_block);
    Ok(None)
}

/// spawn{cmd, args, cwd, env, cols=80, rows=24, sandbox?} -> Pty
fn spawn(lua: &Lua, opts: Table, default_cwd: &str) -> mlua::Result<Pty> {
    crate::exec_lifetime::check_mutation(lua, "pty spawn")?;
    let s = spec(&opts, default_cwd)?;
    if !(1024..=16 * 1024 * 1024).contains(&s.buffer_bytes) || !(1..=100000).contains(&s.scrollback_lines) {
        return Err(rt_err("PTY_VIEW_LIMIT: invalid history budget"));
    }
    validate_size(s.cols, s.rows)?;
    let lease = lease::Lease::prepare(lua)?;
    // the tool call's policy (or the caller's own `sandbox` option), exactly like mare.proc
    let requested = crate::exec_ctx::spawn_sandbox(&opts, opts.get::<Value>("sandbox")?)?;
    let own = crate::ossandbox::Policy::from_lua(lua, &requested, std::path::Path::new(&s.cwd))?;
    let session = crate::ossandbox::enforce_ceiling(lua, crate::ossandbox::current(lua));
    // Unlike one-shot host helpers, a persistent transport never escapes the session's policy,
    // including when created by a command callback outside an active tool execution context.
    let effective = match (session, own) {
        (Some(session), Some(own)) => Some(session.intersect(&own)),
        (Some(session), None) => Some(session),
        (None, own) => own,
    };
    let requested = effective
        .map(|p| lua.create_userdata(crate::ossandbox::PolicyUd(p)).map(Value::UserData))
        .transpose()?
        .unwrap_or(Value::Nil);
    let sandbox = Prepared::new(lua, &requested, std::path::Path::new(&s.cwd))?;
    let pty = Pty::spawn_from_spec(&s, sandbox, Some(super::trees::Owner::of(lua)), lease)?;
    pty.lease.register(lua);
    Ok(pty)
}

pub fn install(lua: &Lua, mare: &Table, cwd: String) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set(
        "capabilities",
        lua.create_function(|lua, ()| {
            let caps = lua.create_table()?;
            caps.set("streaming_text", true)?;
            caps.set("terminal_view", true)?;
            caps.set("max_view_cells", 200000)?;
            caps.set("policy_fence", true)?;
            caps.set("max_write_bytes", MAX_WRITE)?;
            caps.set("queue_chunks", QUEUE_CHUNKS)?;
            caps.set("output_chunk_bytes", 8192)?;
            caps.set("foreground", cfg!(unix))?;
            caps.set("foreground_signals", cfg!(unix))?;
            Ok(caps)
        })?,
    )?;
    t.set("spawn", lua.create_function(move |lua, opts: Table| spawn(lua, opts, &cwd))?)?;
    mare.set("pty", t)?;
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// A ConPTY child is in its own Job Object (only job members count for `tree_alive` on
    /// Windows), and killing the terminal ends the tree: `ping` under `cmd` goes too.
    #[test]
    fn conpty_children_are_in_a_job_and_die_with_it() {
        let lua = Lua::new();
        let s = Spec {
            cmd: "cmd".into(),
            args: vec!["/c".into(), "ping -n 30 127.0.0.1 >NUL".into()],
            cwd: std::env::temp_dir().to_string_lossy().into_owned(),
            env: vec![],
            clear_env: false,
            env_policy: Default::default(),
            cols: 80,
            rows: 24,
            buffer_bytes: 4 * 1024 * 1024,
            scrollback_lines: 10000,
            emulate: false,
        };
        let (tx, rx, closed) = output::channel();
        let inner = spawn_native(&s, &s.cmd, &s.args, &[], None, tx, None).expect("spawn");
        let pty = Pty {
            inner,
            rx: AsyncMutex::new(rx),
            _sandbox: None,
            _tree: crate::trees::Tree::new(None, None),
            lease: lease::Lease::prepare(&lua).unwrap(),
            filter: Mutex::new(mare_shared::ansi::StreamFilter::new(false)),
            view: Mutex::new(view::View::new(24, 80).unwrap()),
            view_enabled: false,
            closed,
        };
        let pid = pty.pid();
        assert!(platform::tree_alive(pid), "the ConPTY child must be inside a Job Object");
        pty.kill().expect("kill");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while platform::tree_alive(pid) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!platform::tree_alive(pid), "the job (cmd + ping) outlived the kill");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_failure_tests {
    use super::*;
    use crate::ossandbox::{Backend, Policy};

    #[test]
    fn worker_initialization_failure_stops_child_before_returning() {
        if crate::ossandbox::backend() != Backend::Bwrap {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let lua = mlua::Lua::new();
        let spec = lua.create_table().unwrap();
        spec.set("level", "workspace-write").unwrap();
        spec.set("network", true).unwrap();
        let policy = Policy::from_lua(&lua, &mlua::Value::Table(spec), dir.path()).unwrap().unwrap();
        let prepared = Prepared::from_policy(policy, dir.path()).unwrap();
        let wrapped = prepared.wrap_monitored("sh", &["-c".into(), "sleep 1; touch continued".into()]);
        let s = Spec {
            cmd: "sh".into(),
            args: vec![],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: vec![],
            clear_env: false,
            env_policy: Default::default(),
            cols: 80,
            rows: 24,
            buffer_bytes: 4 * 1024 * 1024,
            scrollback_lines: 10000,
            emulate: false,
        };
        let (tx, _, _) = output::channel();
        FAIL_PTY_WORKER.with(|flag| flag.set(true));
        let restrict = pre_exec_restrict(&wrapped).unwrap();
        assert!(spawn_native(&s, &wrapped.prog, &wrapped.args, &wrapped.env, restrict, tx, Some(&prepared)).is_err());
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(!dir.path().join("continued").exists(), "child continued after an error return");
        for _ in 0..100 {
            if !dir.path().join(".mare").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!dir.path().join(".mare").exists());
    }
}

#[cfg(all(test, unix))]
mod tests;
