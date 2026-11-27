//! The process trees mare starts end with it. Every child spawn path —
//! `mare.proc.run` / `spawn` / `start`, PTYs, MCP stdio servers — makes its child the root of
//! its own tree (Unix process group / Windows job, [`crate::platform`]) and holds a [`Tree`]
//! for it while the tree may run:
//! - the tree belongs to its host's Lua state ([`Owner`]): [`end_all`] (`Host::shutdown`, after
//!   the plugins were disposed) ends every tree still running — whatever a plugin left behind,
//!   and whatever Lua keeps alive until garbage collection;
//! - a [`Tree`] dropped while its tree runs (the object holding it collected, the task or call
//!   holding it dropped with the runtime) ends the whole tree. The child's own `kill_on_drop`
//!   ends only the root: the rest of the tree keeps the output pipes open, and on Windows (where
//!   tokio reads child pipes with blocking reads on the blocking pool) the runtime's shutdown
//!   then waits for that read forever;
//! - process-wide, trees still running at a normal exit are ended (Unix `atexit`; Windows jobs
//!   are KILL_ON_JOB_CLOSE), and on macOS they are in the crash registry ([`crate::pgroups`]).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use mlua::Lua;

use super::platform::{self, ProcessBackend, Signal};

/// The trees one Lua state (one host) started and that may still run.
#[derive(Clone, Default)]
pub struct Owner(Arc<Mutex<HashSet<u32>>>);

impl Owner {
    /// The owner of everything `lua` starts.
    pub fn of(lua: &Lua) -> Self {
        if let Some(o) = lua.app_data_ref::<Self>() {
            return o.clone();
        }
        let o = Self::default();
        lua.set_app_data(o.clone());
        o
    }

    fn set(&self, pid: u32, live: bool) {
        let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if live {
            g.insert(pid);
        } else {
            g.remove(&pid);
        }
    }
}

/// End every tree `lua` started that is still running. Not waited for: a root nobody reaped yet
/// keeps its group in existence, and SIGKILL / TerminateJobObject need no grace.
pub fn end_all(lua: &Lua) {
    let Some(owner) = lua.app_data_ref::<Owner>().map(|o| o.clone()) else { return };
    let pids: Vec<u32> = owner.0.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect();
    for pid in pids {
        if platform::tree_alive(Some(pid)) {
            platform::signal_tree(Some(pid), Signal::Kill);
        }
    }
}

#[cfg(target_os = "linux")]
fn proc_stat_state_and_pgrp(pid: u32) -> Option<(char, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    let mut fields = rest.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<i32>().ok()?;
    Some((state, pgrp))
}

#[cfg(target_os = "linux")]
pub(crate) fn tree_has_live_member(pid: u32) -> bool {
    if !platform::tree_alive(Some(pid)) {
        return false;
    }
    // Check all process groups in the PTY session first; inspection failure returns true
    // so an unreadable /proc is never misreported as cleanly stopped.
    if platform::session_alive(pid) {
        return true;
    }
    if let Some((state, _)) = proc_stat_state_and_pgrp(pid)
        && state != 'Z'
    {
        return true;
    }
    let target_pgrp = pid as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return true;
    };
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
        let Ok(other_pid) = name.parse::<u32>() else { continue };
        if other_pid == pid {
            continue;
        }
        if let Some((state, pgrp)) = proc_stat_state_and_pgrp(other_pid)
            && pgrp == target_pgrp
            && state != 'Z'
        {
            return true;
        }
    }
    false
}

#[cfg(target_os = "macos")]
pub(crate) fn tree_has_live_member(pid: u32) -> bool {
    if !platform::tree_alive(Some(pid)) {
        return false;
    }
    let Ok(target_sid) = i32::try_from(pid) else {
        return true;
    };
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count < 0 {
        return true;
    }
    let mut pids = vec![0i32; count as usize + 1024];
    let got = unsafe {
        libc::proc_listallpids(pids.as_mut_ptr().cast(), std::mem::size_of_val(pids.as_slice()) as libc::c_int)
    };
    if got < 0 || got as usize >= pids.len() {
        return true;
    }
    let info_size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    for other_pid in pids.into_iter().take(got as usize) {
        if other_pid <= 0 {
            continue;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let n = unsafe {
            libc::proc_pidinfo(
                other_pid,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                info_size,
            )
        };
        if n != info_size {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            if unsafe { libc::getsid(other_pid) } == target_sid || unsafe { libc::getpgid(other_pid) } == target_sid {
                return true;
            }
            continue;
        }
        // Zombies (SZOMB) are already terminated and awaiting parent wait(); on macOS both
        // `kill(-pgid, 0)` and `getsid()` still succeed on zombies, which would otherwise
        // report a stopped child as live until its handle is dropped.
        if info.pbi_status == libc::SZOMB {
            continue;
        }
        if info.pbi_pgid == pid || unsafe { libc::getsid(other_pid) } == target_sid {
            return true;
        }
    }
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn tree_has_live_member(pid: u32) -> bool {
    platform::tree_alive(Some(pid))
}

async fn stop_registered_tree(pid: u32) -> Option<u32> {
    if !tree_has_live_member(pid) {
        return None;
    }
    platform::signal_tree(Some(pid), Signal::Term);
    let grace_deadline = tokio::time::Instant::now() + super::proc::STOP_GRACE;
    while tree_has_live_member(pid) && tokio::time::Instant::now() < grace_deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if tree_has_live_member(pid) {
        platform::signal_tree(Some(pid), Signal::Kill);
        let reap_deadline = tokio::time::Instant::now() + super::proc::REAP_TIMEOUT;
        while tree_has_live_member(pid) && tokio::time::Instant::now() < reap_deadline {
            platform::signal_tree(Some(pid), Signal::Kill);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    let confirm_deadline = tokio::time::Instant::now() + super::proc::REAP_TIMEOUT;
    while tree_has_live_member(pid) && tokio::time::Instant::now() < confirm_deadline {
        platform::signal_tree(Some(pid), Signal::Kill);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if tree_has_live_member(pid) { Some(pid) } else { None }
}

/// Gracefully stop every process tree `lua` started using the `proc/mod.rs` sequence
/// (`STOP_GRACE` SIGTERM -> `SIGKILL` -> `REAP_TIMEOUT` confirm), returning PIDs whose
/// exit could not be confirmed (`stop_unconfirmed`). Confirmed-dead trees are immediately
/// unregistered from the owner, global live set, and macOS crash-recovery registry so
/// `Host::shutdown` leaves no stale `pgroups/<pid>` file behind while lingering tasks wake.
pub async fn end_all_graceful(lua: &Lua) -> Vec<u32> {
    let Some(owner) = lua.app_data_ref::<Owner>().map(|o| o.clone()) else {
        return Vec::new();
    };
    let pids: Vec<u32> = owner.0.lock().unwrap_or_else(|e| e.into_inner()).iter().copied().collect();
    let results = futures_util::future::join_all(
        pids.into_iter().map(|pid| async move { (pid, stop_registered_tree(pid).await) }),
    )
    .await;
    let mut unconfirmed = Vec::new();
    for (pid, still_alive) in results {
        if let Some(pid) = still_alive {
            unconfirmed.push(pid);
        } else {
            track(pid, false);
            owner.set(pid, false);
            super::pgroups::set(pid, false);
        }
    }
    unconfirmed
}

// ---- trees still running, ended when mare exits -------------------------------------------
static LIVE: Mutex<Option<HashSet<u32>>> = Mutex::new(None);
static STOPPING: AtomicBool = AtomicBool::new(false);
static ATEXIT: Once = Once::new();

extern "C" fn end_live_trees() {
    if let Ok(g) = LIVE.lock()
        && let Some(set) = g.as_ref()
    {
        for &pid in set {
            let _ = <platform::Process as ProcessBackend>::signal_tree(pid, Signal::Kill);
        }
    }
    super::pgroups::clear();
}

/// Force-shutdown hook for the existing native shutdown supervisor. Call outside signal
/// handlers: it takes the same registry lock as spawn/registration. Closing admission first
/// prevents a tool, PTY or MCP server from escaping between the snapshot and process exit.
/// Signals whole process groups with SIGKILL; no plugin-specific process list.
pub fn force_cleanup() {
    STOPPING.store(true, Ordering::SeqCst);
    let registry = LIVE.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(trees) = registry.as_ref() {
        for &pid in trees {
            let _ = <platform::Process as ProcessBackend>::signal_tree(pid, Signal::Kill);
        }
    }
}

/// Unix spawn + adoption + global registration is atomic with force_cleanup. The child may
/// execute immediately, but the supervisor cannot finish its snapshot until it is registered.
#[cfg(unix)]
fn admit_into<T>(
    live: &Mutex<Option<HashSet<u32>>>,
    stopping: &AtomicBool,
    spawn: impl FnOnce() -> std::io::Result<(T, Option<u32>)>,
) -> std::io::Result<T> {
    let mut registry = live.lock().unwrap_or_else(|error| error.into_inner());
    if stopping.load(Ordering::SeqCst) {
        return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "process tree shutdown has begun"));
    }
    let (child, pid) = spawn()?;
    if let Some(pid) = pid {
        registry.get_or_insert_with(HashSet::new).insert(pid);
    }
    Ok(child)
}

#[cfg(unix)]
fn with_spawn_admission<T>(spawn: impl FnOnce() -> std::io::Result<(T, Option<u32>)>) -> std::io::Result<T> {
    ATEXIT.call_once(|| <platform::Process as ProcessBackend>::at_exit(end_live_trees));
    admit_into(&LIVE, &STOPPING, spawn)
}

#[cfg(unix)]
pub(crate) fn spawn_registered(command: &mut tokio::process::Command) -> std::io::Result<tokio::process::Child> {
    with_spawn_admission(|| {
        let child = command.spawn()?;
        if let Err(error) = <platform::Process as ProcessBackend>::adopt(&child) {
            if let Some(pid) = child.id() {
                let _ = <platform::Process as ProcessBackend>::signal_tree(pid, Signal::Kill);
            }
            return Err(error);
        }
        let pid = child.id();
        Ok((child, pid))
    })
}

/// Same admission gate for `std::process::Command` callers that already isolate themselves in
/// `pre_exec` (Unix `Terminal` via `setsid`).
#[cfg(unix)]
pub(crate) fn spawn_std_registered(command: &mut std::process::Command) -> std::io::Result<std::process::Child> {
    with_spawn_admission(|| {
        let child = command.spawn()?;
        let pid = Some(child.id());
        Ok((child, pid))
    })
}

fn track(pid: u32, live: bool) {
    ATEXIT.call_once(|| <platform::Process as ProcessBackend>::at_exit(end_live_trees));
    let mut g = LIVE.lock().unwrap_or_else(|e| e.into_inner());
    let set = g.get_or_insert_with(HashSet::new);
    if live {
        set.insert(pid);
        if STOPPING.load(Ordering::SeqCst) {
            let _ = <platform::Process as ProcessBackend>::signal_tree(pid, Signal::Kill);
        }
    } else {
        set.remove(&pid);
    }
}

/// A process tree mare started (rooted at `pid`), registered for as long as this value lives;
/// created right after the spawn. Dropped while the tree still runs, it ends the whole tree.
pub struct Tree {
    pid: Option<u32>,
    owner: Option<Owner>,
}

impl Tree {
    pub fn new(pid: Option<u32>, owner: Option<Owner>) -> Self {
        if let Some(p) = pid {
            track(p, true);
            if let Some(o) = &owner {
                o.set(p, true);
            }
            super::pgroups::set(p, true);
        }
        Self { pid, owner }
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn is_alive(&self) -> bool {
        platform::tree_alive(self.pid)
    }

    /// Keep the tree registered until it is empty, then let it go: what a command left running
    /// in the background (its root already exited) still ends with the host, the runtime or
    /// mare.
    pub fn linger(self) {
        if !platform::tree_alive(self.pid) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        rt.spawn(async move {
            while platform::tree_alive(self.pid) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            drop(self);
        });
    }
}

impl Tree {
    /// After a stop: a confirmed stop releases the tree; an unconfirmed one keeps it registered
    /// (with its owner and mare's exit registry) while any member lives, so the host's shutdown
    /// still sees it, stops it and reports `stop_unconfirmed` if it cannot.
    pub fn settle(self, confirmed: bool) {
        if confirmed {
            drop(self);
        } else {
            self.linger();
        }
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let Some(p) = self.pid else { return };
        // the tree check comes first: without a live tree (Windows: its job) a pid may already
        // name another process
        if platform::tree_alive(Some(p)) {
            platform::signal_tree(Some(p), Signal::Kill);
        }
        track(p, false);
        if let Some(o) = &self.owner {
            o.set(p, false);
        }
        // macOS: a group still dying stays in the crash registry; the next mare ends it if this
        // one is killed first
        if !platform::tree_alive(Some(p)) {
            super::pgroups::set(p, false);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// b7b M8-CG #7: an unconfirmed stop must not drop the tree's registration, or the host's
    /// shutdown would no longer see a process group that is still alive.
    #[tokio::test]
    async fn an_unconfirmed_stop_keeps_the_tree_registered_for_shutdown() {
        use std::os::unix::process::CommandExt;
        let lua = Lua::new();
        let owner = Owner::of(&lua);
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30").process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let tree = Tree::new(Some(pid), Some(owner.clone()));
        tree.settle(false);
        assert!(owner.0.lock().unwrap().contains(&pid), "a live tree whose stop was not confirmed stays registered");
        let unconfirmed = end_all_graceful(&lua).await;
        assert!(unconfirmed.is_empty(), "shutdown stopped it: {unconfirmed:?}");
        let _ = child.wait();
        assert!(!platform::tree_alive(Some(pid)), "the lingering tree was ended by shutdown");
        assert!(
            !owner.0.lock().unwrap().contains(&pid),
            "confirmed-stopped tree must be removed from owner registry by end_all_graceful"
        );
        assert!(
            LIVE.lock().unwrap().as_ref().is_none_or(|set| !set.contains(&pid)),
            "confirmed-stopped tree must be removed from global LIVE set by end_all_graceful"
        );
    }

    #[test]
    fn admission_registers_on_success_and_rejects_after_stopping() {
        let live: Mutex<Option<HashSet<u32>>> = Mutex::new(None);
        let stopping = AtomicBool::new(false);

        let admitted = admit_into(&live, &stopping, || Ok(("child", Some(4242)))).unwrap();
        assert_eq!(admitted, "child");
        assert!(live.lock().unwrap().as_ref().is_some_and(|set| set.contains(&4242)));

        let err = admit_into::<()>(&live, &stopping, || Err(std::io::Error::other("adopt failed"))).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);
        assert_eq!(live.lock().unwrap().as_ref().map(HashSet::len), Some(1));

        stopping.store(true, Ordering::SeqCst);
        let stopped = admit_into::<()>(&live, &stopping, || unreachable!("spawn must not run after stop")).unwrap_err();
        assert_eq!(stopped.kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn tree_drop_removes_early_registration_on_setup_error() {
        let pid = 999_999_u32;
        {
            let _tree = Tree::new(Some(pid), None);
            assert!(LIVE.lock().unwrap().as_ref().is_some_and(|set| set.contains(&pid)));
        }
        assert!(LIVE.lock().unwrap().as_ref().is_none_or(|set| !set.contains(&pid)));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn end_all_graceful_kills_other_pgrp_in_session_after_leader_exits() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg(
                r#"python3 -c 'import os, signal, sys, time; os.setpgid(0, 0); signal.signal(signal.SIGTERM, signal.SIG_IGN); f = open(sys.argv[1], "w"); f.write(f"{os.getpid()}\n"); f.close(); time.sleep(30)' "$1" &"#,
            )
            .arg("_")
            .arg(&pid_file);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut leader = cmd.spawn().unwrap();
        let leader_pid = leader.id();
        let _ = leader.wait().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let child_pid: u32 = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_file)
                && text.ends_with('\n')
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(std::time::Instant::now() < deadline, "timed out waiting for child pid");
            // sleep-ok: poll interval while the child writes its pid file; bounded by `deadline`
            std::thread::sleep(Duration::from_millis(10));
        };

        struct KillOnDrop(i32);
        impl Drop for KillOnDrop {
            fn drop(&mut self) {
                unsafe {
                    libc::kill(self.0, libc::SIGKILL);
                }
            }
        }
        let _guard = KillOnDrop(child_pid as i32);

        // Ensure child is alive in the same session (leader_pid) but a different process group.
        assert_eq!(unsafe { libc::kill(child_pid as i32, 0) }, 0);
        let child_pgid = unsafe { libc::getpgid(child_pid as i32) };
        let child_sid = unsafe { libc::getsid(child_pid as i32) };
        assert_eq!(child_sid, leader_pid as i32);
        assert_ne!(child_pgid, leader_pid as i32);

        let lua = Lua::new();
        Owner::of(&lua).set(leader_pid, true);
        let unconfirmed = end_all_graceful(&lua).await;
        assert!(unconfirmed.is_empty(), "expected clean shutdown, got unconfirmed: {unconfirmed:?}");
        assert_ne!(
            unsafe { libc::kill(child_pid as i32, 0) },
            0,
            "job in another process group of the PTY session survived shutdown"
        );
    }

    #[test]
    fn end_all_is_safe_without_owner_and_kills_owned_live_tree() {
        let empty = Lua::new();
        end_all(&empty);

        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30"]);
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();

        let lua = Lua::new();
        Owner::of(&lua).set(pid, true);
        end_all(&lua);
        let _ = child.wait();
        assert!(!platform::tree_alive(Some(pid)));
    }
}
