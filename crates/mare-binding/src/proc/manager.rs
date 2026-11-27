//! Caller-owned sandboxed piped/PTY process manager (`mare.proc.manager()`), backing the
//! built-in `proc` service (`proc.{spawn,write,resize,kill}`).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mare_shared::wire_projection::{WireByteStream, hook as projection_hook};
use mlua::{Function, Lua, MultiValue, Table, UserData, UserDataMethods};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex as AsyncMutex, oneshot};

use super::super::fs::resolve_path;
use super::super::{exec_ctx, netauth, ossandbox, platform, proc, pty, trees};

pub const MAX_PROCS_PER_OWNER: usize = 32;
const IO_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

static ACTIVE_PROCS: AtomicUsize = AtomicUsize::new(0);
static ACTIVE_LINGERING_TREES: AtomicUsize = AtomicUsize::new(0);
static LAST_SPAWNED_PID: AtomicU32 = AtomicU32::new(0);

#[doc(hidden)]
pub fn active_proc_count() -> usize {
    ACTIVE_PROCS.load(Ordering::SeqCst)
}

#[doc(hidden)]
pub fn active_lingering_tree_count() -> usize {
    ACTIVE_LINGERING_TREES.load(Ordering::SeqCst)
}

#[doc(hidden)]
pub fn last_spawned_pid() -> u32 {
    LAST_SPAWNED_PID.load(Ordering::SeqCst)
}

struct ProcDropGuard;

impl ProcDropGuard {
    fn new() -> Self {
        ACTIVE_PROCS.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for ProcDropGuard {
    fn drop(&mut self) {
        ACTIVE_PROCS.fetch_sub(1, Ordering::SeqCst);
    }
}

struct LingeringDropGuard;

impl LingeringDropGuard {
    fn new() -> Self {
        ACTIVE_LINGERING_TREES.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for LingeringDropGuard {
    fn drop(&mut self) {
        ACTIVE_LINGERING_TREES.fetch_sub(1, Ordering::SeqCst);
    }
}

struct LingeringTreeEntry {
    inner: proc::LingeringProcess,
    _guard: LingeringDropGuard,
}

impl LingeringTreeEntry {
    fn new(inner: proc::LingeringProcess) -> Self {
        Self { inner, _guard: LingeringDropGuard::new() }
    }

    fn is_alive(&self) -> bool {
        self.inner.is_alive()
    }
}

enum ProcEntry {
    Piped {
        pid: Option<u32>,
        stdin: Arc<AsyncMutex<Option<ChildStdin>>>,
        kill_tx: Option<oneshot::Sender<()>>,
        task: tokio::task::JoinHandle<()>,
    },
    Pty {
        pty: Arc<pty::Pty>,
        task: tokio::task::JoinHandle<()>,
    },
}

impl ProcEntry {
    fn kill(&mut self) {
        match self {
            Self::Piped { pid, kill_tx, .. } => {
                platform::signal_tree(*pid, platform::Signal::Kill);
                if let Some(tx) = kill_tx.take() {
                    let _ = tx.send(());
                }
            }
            Self::Pty { pty, .. } => {
                let _ = pty.kill();
            }
        }
    }
}

struct ManagerInner {
    default_cwd: PathBuf,
    procs: HashMap<String, ProcEntry>,
    cleaned_up: bool,
    lingering_trees: Vec<LingeringTreeEntry>,
    lingering_reaper_active: bool,
}

impl Drop for ManagerInner {
    fn drop(&mut self) {
        self.cleaned_up = true;
        for (_, mut entry) in self.procs.drain() {
            entry.kill();
            let task = match entry {
                ProcEntry::Piped { task, .. } | ProcEntry::Pty { task, .. } => task,
            };
            task.abort();
        }
        self.lingering_trees.clear();
    }
}

#[derive(Clone)]
pub(crate) struct ProcManager {
    inner: Rc<RefCell<ManagerInner>>,
}

impl ProcManager {
    fn new(default_cwd: PathBuf) -> Self {
        Self {
            inner: Rc::new(RefCell::new(ManagerInner {
                default_cwd,
                procs: HashMap::new(),
                cleaned_up: false,
                lingering_trees: Vec::new(),
                lingering_reaper_active: false,
            })),
        }
    }
}

fn err_tuple(lua: &Lua, code: &str, msg: impl AsRef<str>) -> mlua::Result<MultiValue> {
    Ok(MultiValue::from_vec(vec![
        mlua::Value::Nil,
        mlua::Value::String(lua.create_string(code)?),
        mlua::Value::String(lua.create_string(msg.as_ref())?),
    ]))
}

fn check_cwd_policy(
    policy: Option<&ossandbox::Policy>,
    path: &Path,
    base_cwd: &Path,
) -> Result<(), (&'static str, String)> {
    let Some(policy) = policy else { return Ok(()) };
    policy.check("read", &path.to_string_lossy(), base_cwd).map_err(|d| {
        let marker = policy.denial_marker(&d);
        let detail = d.path.as_deref().unwrap_or("");
        ("denied", if detail.is_empty() { marker } else { format!("{marker}: {detail}") })
    })
}

fn emit_output(lua: &Lua, cb: Option<&Function>, id: &str, stream: &str, bytes: &[u8]) {
    let Some(cb) = cb else { return };
    if bytes.is_empty() {
        return;
    }
    let Ok(ev) = lua.create_table() else { return };
    let _ = ev.set("process_id", id);
    let _ = ev.set("stream", stream);
    let _ = ev.set("data_base64", STANDARD.encode(bytes));
    let _ = cb.call::<()>(ev);
}

fn emit_exit(lua: &Lua, cb: Option<&Function>, id: &str, exit_code: Option<i32>, output_drain_timed_out: bool) {
    let Some(cb) = cb else { return };
    let Ok(ev) = lua.create_table() else { return };
    let _ = ev.set("process_id", id);
    if let Some(code) = exit_code {
        let _ = ev.set("exit_code", code);
    }
    if output_drain_timed_out {
        let _ = ev.set("output_drain_timed_out", true);
    }
    let _ = cb.call::<()>(ev);
}

async fn drain_pipe_tasks(
    out_task: Option<tokio::task::JoinHandle<()>>,
    err_task: Option<tokio::task::JoinHandle<()>>,
    timeout: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    async fn drain_one(task: Option<tokio::task::JoinHandle<()>>, deadline: tokio::time::Instant) -> bool {
        let Some(mut task) = task else {
            return false;
        };
        if tokio::time::timeout_at(deadline, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
            return true;
        }
        false
    }
    let (out_timed_out, err_timed_out) = tokio::join!(drain_one(out_task, deadline), drain_one(err_task, deadline));
    out_timed_out || err_timed_out
}

fn reap_lingering_trees(inner: &RefCell<ManagerInner>) {
    inner.borrow_mut().lingering_trees.retain(LingeringTreeEntry::is_alive);
}

fn spawn_lingering_reaper(inner: &Rc<RefCell<ManagerInner>>) {
    {
        let mut st = inner.borrow_mut();
        if st.lingering_reaper_active {
            return;
        }
        st.lingering_reaper_active = true;
    }
    let weak: Weak<RefCell<ManagerInner>> = Rc::downgrade(inner);
    mare_shared::txn_scope::spawn_local(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        interval.tick().await;
        loop {
            interval.tick().await;
            let Some(mgr) = weak.upgrade() else {
                break;
            };
            if mgr.borrow().cleaned_up {
                mgr.borrow_mut().lingering_reaper_active = false;
                break;
            }
            reap_lingering_trees(&mgr);
            if mgr.borrow().lingering_trees.is_empty() {
                mgr.borrow_mut().lingering_reaper_active = false;
                break;
            }
        }
    });
}

fn is_cleaned_up(weak: &Weak<RefCell<ManagerInner>>) -> bool {
    weak.upgrade().is_none_or(|m| m.borrow().cleaned_up)
}

async fn pump_pipe(
    lua: Lua,
    inner: Weak<RefCell<ManagerInner>>,
    cb: Option<Function>,
    id: String,
    stream: &'static str,
    mut reader: impl tokio::io::AsyncRead + Unpin,
) {
    let mut projector = WireByteStream::new(projection_hook(&lua));
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if is_cleaned_up(&inner) {
                    return;
                }
                let projected = projector.push(&buf[..n]);
                emit_output(&lua, cb.as_ref(), &id, stream, &projected);
            }
        }
    }
    if !is_cleaned_up(&inner) {
        let tail = projector.finish();
        emit_output(&lua, cb.as_ref(), &id, stream, &tail);
    }
}

fn spawn_process(lua: &Lua, mgr: &ProcManager, opts: Table) -> mlua::Result<MultiValue> {
    if mgr.inner.borrow().cleaned_up {
        return err_tuple(lua, "invalid_arguments", "process manager is closed");
    }
    reap_lingering_trees(&mgr.inner);

    let process_id: String = opts.get("process_id")?;
    if process_id.is_empty() {
        return err_tuple(lua, "invalid_arguments", "process_id must not be empty");
    }
    let command: Vec<String> = opts.get("command")?;
    if command.is_empty() {
        return err_tuple(lua, "invalid_arguments", "command must not be empty");
    }
    if mgr.inner.borrow().procs.contains_key(&process_id) {
        return err_tuple(lua, "invalid_arguments", format!("process_id `{process_id}` is already active"));
    }
    if mgr.inner.borrow().procs.len() >= MAX_PROCS_PER_OWNER {
        return err_tuple(
            lua,
            "process_limit_reached",
            format!("too many active processes ({MAX_PROCS_PER_OWNER} max per owner); kill or wait for one to exit"),
        );
    }

    let base_cwd = mgr.inner.borrow().default_cwd.clone();
    let cwd_opt: Option<String> = opts.get("cwd")?;
    let cwd = cwd_opt.as_deref().map(|c| resolve_path(&base_cwd, c)).unwrap_or_else(|| base_cwd.clone());
    if let Err(e) = exec_ctx::check(lua, "read", &cwd.to_string_lossy()) {
        return err_tuple(lua, "denied", e.to_string());
    }
    let policy = match exec_ctx::active(lua) {
        Ok(Some(active)) => Some(active.policy),
        Ok(None) => ossandbox::enforce_ceiling(lua, ossandbox::current(lua)),
        Err(e) => return err_tuple(lua, "denied", e.to_string()),
    };
    if let Err((code, msg)) = check_cwd_policy(policy.as_ref(), &cwd, &base_cwd) {
        return err_tuple(lua, code, msg);
    }
    if !cwd.is_dir() {
        return err_tuple(lua, "invalid_arguments", format!("cwd `{}` is not a directory", cwd.display()));
    }
    let cwd_str = cwd.to_string_lossy().into_owned();
    let prog = &command[0];
    let args = &command[1..];
    let env_map: BTreeMap<String, String> = opts.get::<Option<BTreeMap<String, String>>>("env")?.unwrap_or_default();
    let env: Vec<(String, String)> = env_map.into_iter().collect();
    let tty = opts.get::<Option<bool>>("tty")?.unwrap_or(false);
    let (cols, rows) = match opts.get::<Option<Table>>("size")? {
        Some(s) => (s.get::<u16>("cols")?, s.get::<u16>("rows")?),
        None => (80, 24),
    };
    let on_output: Option<Function> = opts.get("on_output")?;
    let on_exit: Option<Function> = opts.get("on_exit")?;

    let owner = Some(trees::Owner::of(lua));
    let approver = netauth::approver(lua);

    if tty {
        if let Err(e) = pty::validate_size(cols, rows) {
            return err_tuple(lua, "invalid_arguments", e.to_string());
        }
        let pty_proc = match pty::Pty::spawn_confined(prog, args, &cwd_str, &env, cols, rows, policy, owner, approver) {
            Ok(p) => Arc::new(p),
            Err(e) => return err_tuple(lua, "invalid_arguments", e.to_string()),
        };
        let pty_task = pty_proc.clone();
        let mgr_weak = Rc::downgrade(&mgr.inner);
        let lua_cb = lua.clone();
        let id_cb = process_id.clone();
        let guard = ProcDropGuard::new();
        let task = mare_shared::txn_scope::spawn_local(async move {
            let _guard = guard;
            let mut projector = WireByteStream::new(projection_hook(&lua_cb));
            while let Some(chunk) = pty_task.read_chunk().await {
                if is_cleaned_up(&mgr_weak) {
                    break;
                }
                let projected = projector.push(&chunk);
                emit_output(&lua_cb, on_output.as_ref(), &id_cb, "pty", &projected);
            }
            if !is_cleaned_up(&mgr_weak) {
                let tail = projector.finish();
                emit_output(&lua_cb, on_output.as_ref(), &id_cb, "pty", &tail);
            }
            let exit_code = pty_task.wait_exit_after_eof(|| is_cleaned_up(&mgr_weak)).await;
            if let Some(m) = mgr_weak.upgrade() {
                m.borrow_mut().procs.remove(&id_cb);
                if !m.borrow().cleaned_up {
                    emit_exit(&lua_cb, on_exit.as_ref(), &id_cb, exit_code, false);
                }
            }
        });
        mgr.inner.borrow_mut().procs.insert(process_id.clone(), ProcEntry::Pty { pty: pty_proc, task });
    } else {
        let mut raw = match proc::spawn_confined(prog, args, &cwd_str, &env, true, policy, owner, approver) {
            Ok(r) => r,
            Err(e) => return err_tuple(lua, "invalid_arguments", e.to_string()),
        };
        let pid = raw.pid;
        if let Some(p) = pid {
            LAST_SPAWNED_PID.store(p, Ordering::SeqCst);
        }
        let stdin = Arc::new(AsyncMutex::new(raw.stdin.take()));
        let stdout = raw.stdout.take();
        let stderr = raw.stderr.take();
        let (kill_tx, kill_rx) = oneshot::channel::<()>();
        let mgr_weak = Rc::downgrade(&mgr.inner);
        let lua_cb = lua.clone();
        let id_cb = process_id.clone();
        let guard = ProcDropGuard::new();
        let task = mare_shared::txn_scope::spawn_local(async move {
            let _guard = guard;
            let out_task = stdout.map(|out| {
                let l = lua_cb.clone();
                let m = mgr_weak.clone();
                let c = on_output.clone();
                let i = id_cb.clone();
                mare_shared::txn_scope::spawn_local(async move { pump_pipe(l, m, c, i, "stdout", out).await })
            });
            let err_task = stderr.map(|err| {
                let l = lua_cb.clone();
                let m = mgr_weak.clone();
                let c = on_output.clone();
                let i = id_cb.clone();
                mare_shared::txn_scope::spawn_local(async move { pump_pipe(l, m, c, i, "stderr", err).await })
            });
            let (exit_code, killed) = tokio::select! {
                status = raw.child.wait() => (status.ok().and_then(|s| s.code()), false),
                _ = kill_rx => {
                    let _ = raw.child.start_kill();
                    (raw.child.wait().await.ok().and_then(|s| s.code()), true)
                }
            };
            let drain_timed_out = drain_pipe_tasks(out_task, err_task, IO_DRAIN_TIMEOUT).await;
            let lingering = raw.finish_monitor_into_lingering().await;
            let Some(m) = mgr_weak.upgrade() else {
                drop(lingering);
                return;
            };
            let cleaned = m.borrow().cleaned_up;
            if !killed && !cleaned {
                reap_lingering_trees(&m);
                if lingering.is_alive() {
                    m.borrow_mut().lingering_trees.push(LingeringTreeEntry::new(lingering));
                    spawn_lingering_reaper(&m);
                } else {
                    drop(lingering);
                }
            } else {
                drop(lingering);
            }
            m.borrow_mut().procs.remove(&id_cb);
            if !cleaned {
                emit_exit(&lua_cb, on_exit.as_ref(), &id_cb, exit_code, drain_timed_out);
            }
        });
        mgr.inner
            .borrow_mut()
            .procs
            .insert(process_id.clone(), ProcEntry::Piped { pid, stdin, kill_tx: Some(kill_tx), task });
    }

    let res = lua.create_table()?;
    res.set("process_id", process_id)?;
    Ok(MultiValue::from_vec(vec![mlua::Value::Table(res)]))
}

async fn write_process(lua: Lua, mgr: ProcManager, opts: Table) -> mlua::Result<MultiValue> {
    reap_lingering_trees(&mgr.inner);
    let process_id: String = opts.get("process_id")?;
    let data_base64: String = opts.get::<Option<String>>("data_base64")?.unwrap_or_default();
    let close_stdin = opts.get::<Option<bool>>("close_stdin")?.unwrap_or(false);

    let bytes = if data_base64.is_empty() {
        Vec::new()
    } else {
        match STANDARD.decode(&data_base64) {
            Ok(b) => b,
            Err(e) => return err_tuple(&lua, "invalid_arguments", format!("invalid data_base64: {e}")),
        }
    };

    enum Target {
        Piped(Arc<AsyncMutex<Option<ChildStdin>>>),
        Pty(Arc<pty::Pty>),
    }
    let target = {
        let st = mgr.inner.borrow();
        let Some(entry) = st.procs.get(&process_id) else {
            return err_tuple(&lua, "unknown_process", format!("unknown process_id `{process_id}`"));
        };
        match entry {
            ProcEntry::Piped { stdin, .. } => Target::Piped(stdin.clone()),
            ProcEntry::Pty { pty, .. } => Target::Pty(pty.clone()),
        }
    };

    match target {
        Target::Piped(stdin) => {
            let mut guard = stdin.lock().await;
            if !bytes.is_empty() {
                let Some(w) = guard.as_mut() else {
                    return err_tuple(&lua, "invalid_arguments", "stdin is closed");
                };
                if let Err(e) = w.write_all(&bytes).await {
                    return err_tuple(&lua, "invalid_arguments", e.to_string());
                }
                if let Err(e) = w.flush().await {
                    return err_tuple(&lua, "invalid_arguments", e.to_string());
                }
            }
            if close_stdin {
                guard.take();
            }
        }
        Target::Pty(pty) => {
            if close_stdin {
                return err_tuple(&lua, "invalid_arguments", "cannot close stdin on a PTY process");
            }
            if !bytes.is_empty()
                && let Err(e) = pty.write(&bytes)
            {
                return err_tuple(&lua, "invalid_arguments", e.to_string());
            }
        }
    }
    Ok(MultiValue::from_vec(vec![mlua::Value::Boolean(true)]))
}

fn resize_process(lua: &Lua, mgr: &ProcManager, opts: Table) -> mlua::Result<MultiValue> {
    reap_lingering_trees(&mgr.inner);
    let process_id: String = opts.get("process_id")?;
    let size: Table = opts.get("size")?;
    let cols: u16 = size.get("cols")?;
    let rows: u16 = size.get("rows")?;
    let pty = {
        let st = mgr.inner.borrow();
        let Some(entry) = st.procs.get(&process_id) else {
            return err_tuple(lua, "unknown_process", format!("unknown process_id `{process_id}`"));
        };
        match entry {
            ProcEntry::Pty { pty, .. } => pty.clone(),
            ProcEntry::Piped { .. } => {
                return err_tuple(lua, "invalid_arguments", "process was not started with tty=true");
            }
        }
    };
    if let Err(e) = pty.resize(cols, rows) {
        return err_tuple(lua, "invalid_arguments", e.to_string());
    }
    Ok(MultiValue::from_vec(vec![mlua::Value::Boolean(true)]))
}

fn kill_process(lua: &Lua, mgr: &ProcManager, opts: Table) -> mlua::Result<MultiValue> {
    reap_lingering_trees(&mgr.inner);
    let process_id: String = opts.get("process_id")?;
    let mut st = mgr.inner.borrow_mut();
    let Some(entry) = st.procs.get_mut(&process_id) else {
        return err_tuple(lua, "unknown_process", format!("unknown process_id `{process_id}`"));
    };
    entry.kill();
    Ok(MultiValue::from_vec(vec![mlua::Value::Boolean(true)]))
}

async fn close_all_procs(mgr: ProcManager) -> mlua::Result<()> {
    let entries: Vec<_> = {
        let mut st = mgr.inner.borrow_mut();
        st.cleaned_up = true;
        st.procs.drain().collect()
    };
    let mut tasks = Vec::with_capacity(entries.len());
    for (_, mut entry) in entries {
        entry.kill();
        let task = match entry {
            ProcEntry::Piped { task, .. } | ProcEntry::Pty { task, .. } => task,
        };
        tasks.push(task);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let _ = tokio::time::timeout_at(deadline, async {
        for task in &mut tasks {
            let _ = task.await;
        }
    })
    .await;
    for task in tasks {
        if !task.is_finished() {
            task.abort();
            let _ = task.await;
        }
    }
    mgr.inner.borrow_mut().lingering_trees.clear();
    Ok(())
}

impl UserData for ProcManager {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("spawn", |lua, this, opts: Table| spawn_process(lua, this, opts));
        methods.add_async_method("write", |lua, this, opts: Table| {
            let mgr = this.clone();
            async move { write_process(lua, mgr, opts).await }
        });
        methods.add_method("resize", |lua, this, opts: Table| resize_process(lua, this, opts));
        methods.add_method("kill", |lua, this, opts: Table| kill_process(lua, this, opts));
        methods.add_async_method("close_all", |_lua, this, ()| {
            let mgr = this.clone();
            async move { close_all_procs(mgr).await }
        });
        methods.add_method("counts", |lua, this, ()| {
            reap_lingering_trees(&this.inner);
            let st = this.inner.borrow();
            let t = lua.create_table()?;
            t.set("active", st.procs.len())?;
            t.set("lingering", st.lingering_trees.len())?;
            Ok(t)
        });
    }
}

pub(super) fn install(lua: &Lua, proc_tbl: &Table, cwd: String) -> mlua::Result<()> {
    let default_cwd = PathBuf::from(cwd);
    proc_tbl.set("manager", lua.create_function(move |_, ()| Ok(ProcManager::new(default_cwd.clone())))?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use mlua::ObjectLike as _;

    use super::*;

    #[tokio::test]
    async fn drain_pipe_tasks_shares_single_deadline_when_one_pipe_closes_early() {
        let (mut r1, w1) = tokio::io::duplex(64);
        let (mut r2, w2) = tokio::io::duplex(64);
        tokio::spawn(async move {
            // sleep-ok: hold pipe 1 open past half of 250ms drain deadline
            tokio::time::sleep(Duration::from_millis(160)).await;
            drop(w1);
        });

        let t1 = tokio::spawn(async move {
            let mut buf = [0u8; 16];
            while let Ok(n) = r1.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });
        let t2 = tokio::spawn(async move {
            let mut buf = [0u8; 16];
            while let Ok(n) = r2.read(&mut buf).await {
                if n == 0 {
                    break;
                }
            }
        });

        let started = tokio::time::Instant::now();
        let timed_out = drain_pipe_tasks(Some(t1), Some(t2), IO_DRAIN_TIMEOUT).await;
        let elapsed = started.elapsed();
        drop(w2);

        assert!(timed_out, "open second pipe must trigger timeout");
        assert!(
            elapsed < Duration::from_millis(330),
            "drain_pipe_tasks must share a single 250ms deadline across stdout and stderr, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn dropping_proc_manager_without_close_all_kills_running_children() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ws = tempfile::tempdir().unwrap();
                let pid_file = ws.path().join("drop-leak.pid");
                let lua = Lua::new();
                let tbl = lua.create_table().unwrap();
                super::install(&lua, &tbl, ws.path().to_string_lossy().into_owned()).unwrap();
                let mgr_ud: mlua::AnyUserData = tbl.get::<Function>("manager").unwrap().call(()).unwrap();
                let mgr_weak = Rc::downgrade(&mgr_ud.borrow::<ProcManager>().unwrap().inner);
                let opts = lua.create_table().unwrap();
                opts.set("process_id", "drop-leak").unwrap();
                let cmd = format!("echo $$ > '{}'; exec cat", pid_file.display());
                opts.set("command", vec!["sh", "-c", &cmd]).unwrap();
                let (res, err_code): (Option<Table>, Option<String>) = mgr_ud.call_method("spawn", opts).unwrap();
                assert!(res.is_some(), "spawn failed: {err_code:?}");
                let counts: Table = mgr_ud.call_method("counts", ()).unwrap();
                assert_eq!(counts.get::<usize>("active").unwrap(), 1);

                let pid_ready = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        if let Ok(text) = std::fs::read_to_string(&pid_file)
                            && let Ok(pid) = text.trim().parse::<u32>()
                        {
                            return pid;
                        }
                        // sleep-ok: wait for spawned shell to record its pid before dropping manager
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await;
                let _pid = pid_ready.expect("child failed to record pid before drop");

                // Drop strong handle to ProcManager without calling close_all().
                drop(mgr_ud);
                lua.gc_collect().unwrap();
                assert!(
                    mgr_weak.upgrade().is_none(),
                    "dropping ProcManager must immediately drop ManagerInner (no Rc cycle)"
                );

                #[cfg(unix)]
                {
                    let dead = tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            let alive = std::process::Command::new("kill")
                                .args(["-0", &_pid.to_string()])
                                .status()
                                .unwrap()
                                .success();
                            if !alive {
                                return;
                            }
                            // sleep-ok: poll child exit after ManagerInner::drop killed it
                            tokio::time::sleep(Duration::from_millis(15)).await;
                        }
                    })
                    .await;
                    assert!(dead.is_ok(), "child pid {_pid} still alive after ProcManager drop");
                }
            })
            .await;
    }

    #[tokio::test]
    async fn close_all_kills_stuck_tasks_concurrently_within_single_deadline_and_aborts() {
        struct DropFlag(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        tokio::task::LocalSet::new()
            .run_until(async {
                let ws = tempfile::tempdir().unwrap();
                let mgr = ProcManager::new(ws.path().to_path_buf());
                let dropped1 = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let dropped2 = Arc::new(std::sync::atomic::AtomicBool::new(false));

                let mut keep_alive = Vec::new();
                for (id, dropped) in [("stuck-1", dropped1.clone()), ("stuck-2", dropped2.clone())] {
                    let (tx, rx) = oneshot::channel::<()>();
                    keep_alive.push(tx);
                    let (kill_tx, _kill_rx) = oneshot::channel::<()>();
                    let flag = DropFlag(dropped);
                    let guard = ProcDropGuard::new();
                    let task = mare_shared::txn_scope::spawn_local(async move {
                        let _flag = flag;
                        let _guard = guard;
                        let _ = rx.await;
                    });
                    mgr.inner.borrow_mut().procs.insert(
                        id.to_string(),
                        ProcEntry::Piped {
                            pid: None,
                            stdin: Arc::new(AsyncMutex::new(None)),
                            kill_tx: Some(kill_tx),
                            task,
                        },
                    );
                }

                let started = tokio::time::Instant::now();
                let mgr_check = mgr.clone();
                close_all_procs(mgr).await.unwrap();
                let elapsed = started.elapsed();

                assert!(
                    elapsed < Duration::from_millis(2800),
                    "close_all_procs must share a single 2s deadline across all processes, took {elapsed:?}"
                );
                assert!(dropped1.load(Ordering::SeqCst), "timed-out task 1 must be aborted");
                assert!(dropped2.load(Ordering::SeqCst), "timed-out task 2 must be aborted");
                assert!(mgr_check.inner.borrow().procs.is_empty());
                drop(keep_alive);
            })
            .await;
    }

    #[tokio::test]
    async fn pty_zero_and_oversized_dimensions_return_invalid_arguments_without_panic() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ws = tempfile::tempdir().unwrap();
                let lua = Lua::new();
                let tbl = lua.create_table().unwrap();
                super::install(&lua, &tbl, ws.path().to_string_lossy().into_owned()).unwrap();
                let mgr_ud: mlua::AnyUserData = tbl.get::<Function>("manager").unwrap().call(()).unwrap();

                for (cols, rows) in [(0u16, 24u16), (80, 0), (1000, 1000)] {
                    let opts = lua.create_table().unwrap();
                    opts.set("process_id", format!("bad-{cols}x{rows}")).unwrap();
                    opts.set("command", vec!["true"]).unwrap();
                    opts.set("tty", true).unwrap();
                    let size = lua.create_table().unwrap();
                    size.set("cols", cols).unwrap();
                    size.set("rows", rows).unwrap();
                    opts.set("size", size).unwrap();
                    let (res, code, msg): (Option<Table>, Option<String>, Option<String>) =
                        mgr_ud.call_method("spawn", opts).unwrap();
                    assert!(res.is_none(), "expected error for {cols}x{rows}");
                    assert_eq!(code.as_deref(), Some("invalid_arguments"), "msg={msg:?}");
                    assert!(
                        msg.as_deref().unwrap_or("").contains("PTY_SIZE"),
                        "expected PTY_SIZE error for {cols}x{rows}, got {msg:?}"
                    );
                }

                // Valid PTY spawn + invalid resize dimensions.
                let opts = lua.create_table().unwrap();
                opts.set("process_id", "ok-pty").unwrap();
                opts.set("command", vec!["cat"]).unwrap();
                opts.set("tty", true).unwrap();
                let (res, err): (Option<Table>, Option<String>) = mgr_ud.call_method("spawn", opts).unwrap();
                assert!(res.is_some(), "spawn failed: {err:?}");
                for (cols, rows) in [(0u16, 24u16), (80, 0), (500, 500)] {
                    let ropts = lua.create_table().unwrap();
                    ropts.set("process_id", "ok-pty").unwrap();
                    let size = lua.create_table().unwrap();
                    size.set("cols", cols).unwrap();
                    size.set("rows", rows).unwrap();
                    ropts.set("size", size).unwrap();
                    let (ok, code, msg): (Option<bool>, Option<String>, Option<String>) =
                        mgr_ud.call_method("resize", ropts).unwrap();
                    assert!(ok.is_none(), "expected resize error for {cols}x{rows}");
                    assert_eq!(code.as_deref(), Some("invalid_arguments"));
                    assert!(msg.as_deref().unwrap_or("").contains("PTY_SIZE"), "got {msg:?}");
                }
                let _: () = mgr_ud.call_async_method("close_all", ()).await.unwrap();
            })
            .await;
    }
}
