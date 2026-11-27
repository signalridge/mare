//! Process trees: isolation, adoption into a group / job, signalling.

use std::io;

#[cfg(windows)]
use super::win;
use super::{Host, Process, ProcessBackend, Signal};

#[cfg(unix)]
pub(crate) fn isolate_session_with<F, G>(mut setsid: F, mut setpgid: G) -> io::Result<()>
where
    F: FnMut() -> io::Result<()>,
    G: FnMut() -> io::Result<()>,
{
    match setsid() {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(libc::EPERM) => setpgid(),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
/// Isolate a `std::process::Command` in its own session / process group.
pub fn isolate_std(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    // own session (falls back to a process group when already a session leader), and on Linux
    // the parent-death signal (Apache-2.0)
    #[cfg(target_os = "linux")]
    let parent = unsafe { libc::getpid() };
    unsafe {
        cmd.pre_exec(move || {
            isolate_session_with(
                || if libc::setsid() == -1 { Err(io::Error::last_os_error()) } else { Ok(()) },
                || if libc::setpgid(0, 0) == -1 { Err(io::Error::last_os_error()) } else { Ok(()) },
            )?;
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // the parent died between fork and here: the signal would never come
                if libc::getppid() != parent {
                    libc::raise(libc::SIGTERM);
                }
            }
            Ok(())
        });
    }
}

#[cfg(unix)]
/// Adopt a `std::process::Child` spawned with [`isolate_std`] into its process tree.
pub fn adopt_std(_child: &std::process::Child) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
impl ProcessBackend for Host {
    fn isolate(cmd: &mut tokio::process::Command) {
        isolate_std(cmd.as_std_mut());
    }
    fn adopt(_child: &tokio::process::Child) -> io::Result<()> {
        Ok(())
    }
    fn signal_tree(pid: u32, sig: Signal) -> io::Result<()> {
        let s = match sig {
            Signal::Term => libc::SIGTERM,
            Signal::Kill => libc::SIGKILL,
            Signal::Int => libc::SIGINT,
            Signal::Hup => libc::SIGHUP,
            Signal::Quit => libc::SIGQUIT,
        };
        #[cfg(target_os = "linux")]
        if s != libc::SIGKILL && bwrap_leads(pid) {
            return signal_all_but_leader(pid, s);
        }
        // A PTY leader owns its session; commands under job control have other groups.
        // For SIGKILL, terminate the leader's group first so a live shell cannot fork a new
        // job-control group between session enumeration and leader death, then kill every
        // remaining group in the session.
        if s == libc::SIGKILL {
            let leader_ok = unsafe { libc::kill(-(pid as i32), s) } == 0;
            let leader_err = if leader_ok { None } else { Some(io::Error::last_os_error()) };
            let session_result = super::session_tree::signal(pid, s);
            if leader_ok {
                return Ok(());
            }
            let error = leader_err.unwrap();
            return if error.raw_os_error() == Some(libc::ESRCH) { session_result } else { Err(error) };
        }
        // Non-SIGKILL polite signals: signal every process group in the session once,
        // never double-signalling the leader's own group `-(pid as i32)` (which under load
        // can deliver a duplicate SIGINT/SIGTERM after the process handles the first one).
        // Inspect other session groups first while the leader is still alive (and still
        // find them if the root already exited), then signal the leader's own group once
        // directly (also covering `setpgid` fallbacks and unreadable `/proc`).
        let session_result = super::session_tree::signal_other_groups(pid, s);
        if unsafe { libc::kill(-(pid as i32), s) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) { session_result } else { Err(error) }
    }
    fn tree_alive(pid: u32) -> bool {
        #[cfg(target_os = "linux")]
        if let Ok(alive) = super::session_tree::tree_alive(pid) {
            return alive;
        }
        super::session_tree::alive(pid)
            || unsafe { libc::kill(-(pid as i32), 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    fn at_exit(f: extern "C" fn()) {
        unsafe {
            libc::atexit(f);
        }
    }
    fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
        std::os::unix::process::ExitStatusExt::signal(status)
    }
}

/// Whether the group led by `pid` is a bubblewrap sandbox: its leader is bwrap's monitor
/// process.
#[cfg(target_os = "linux")]
fn bwrap_leads(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim_end() == "bwrap")
}

/// A polite signal (anything but SIGKILL) to a bubblewrap sandbox goes to every member of its
/// process group except the leader. The leader is bwrap's monitor: it has no handler, so the
/// signal would end it, and `--die-with-parent` would then SIGKILL the whole sandbox at once. The
/// command would never handle its SIGINT, and the grace period before SIGKILL would be lost.
/// The sandbox's own pid 1 ignores signals from outside its namespace that it has no handler for.
/// SIGKILL still goes to the whole group.
#[cfg(target_os = "linux")]
fn signal_all_but_leader(pid: u32, sig: libc::c_int) -> io::Result<()> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Ok(()) };
    for e in dir.flatten() {
        let Some(member) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        if member == pid {
            continue;
        }
        // /proc/<pid>/stat: "pid (comm) state ppid pgrp …"; comm may contain spaces and ')'
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{member}/stat")) else { continue };
        let pgrp = stat.rsplit_once(')').and_then(|(_, rest)| rest.split_whitespace().nth(2)?.parse::<u32>().ok());
        if pgrp == Some(pid) {
            unsafe { libc::kill(member as i32, sig) };
        }
    }
    Ok(())
}

#[cfg(windows)]
/// Isolate a `std::process::Command` in its own suspended Job-ready process group.
pub fn isolate_std(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED};
    // no console window per child; suspended until it is inside its job
    cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_SUSPENDED);
}

#[cfg(windows)]
/// Adopt a `std::process::Child` spawned with [`isolate_std`] into its Job Object and resume it.
pub fn adopt_std(child: &std::process::Child) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    win::adopt(child.id(), child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE, None)
}

#[cfg(windows)]
impl ProcessBackend for Host {
    fn isolate(cmd: &mut tokio::process::Command) {
        isolate_std(cmd.as_std_mut());
    }
    fn adopt(child: &tokio::process::Child) -> io::Result<()> {
        let (Some(pid), Some(h)) = (child.id(), child.raw_handle()) else { return Ok(()) };
        win::adopt(pid, h as windows_sys::Win32::Foundation::HANDLE, None)
    }
    fn signal_tree(pid: u32, _sig: Signal) -> io::Result<()> {
        // console control events can't reach CREATE_NO_WINDOW children: every stop request ends
        // the job (the tree), like SIGKILL on the process group
        win::terminate(pid)
    }
    fn tree_alive(pid: u32) -> bool {
        win::alive(pid)
    }
    fn at_exit(f: extern "C" fn()) {
        // Rust's Windows process::exit uses ExitProcess and skips the CRT exit list.
        // Register for normal return too; the explicit exit wrapper drains the same list.
        static REGISTER: std::sync::Once = std::sync::Once::new();
        EXIT_CALLBACKS.lock().unwrap_or_else(|e| e.into_inner()).push(f);
        REGISTER.call_once(|| unsafe {
            libc::atexit(run_exit_callbacks);
        });
    }
    fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
        None
    }
}

#[cfg(windows)]
static EXIT_CALLBACKS: std::sync::Mutex<Vec<extern "C" fn()>> = std::sync::Mutex::new(Vec::new());

#[cfg(windows)]
extern "C" fn run_exit_callbacks() {
    run_callbacks(&EXIT_CALLBACKS);
}

#[cfg(windows)]
fn run_callbacks(registry: &std::sync::Mutex<Vec<extern "C" fn()>>) {
    let callbacks = std::mem::take(&mut *registry.lock().unwrap_or_else(|e| e.into_inner()));
    for callback in callbacks.into_iter().rev() {
        callback();
    }
}

/// Exit after process-owned cleanup, including on Windows where Rust skips CRT atexit.
pub fn exit(code: i32) -> ! {
    #[cfg(windows)]
    run_exit_callbacks();
    std::process::exit(code)
}

/// Spawn a command prepared by [`crate::proc::build`] (which applied
/// [`ProcessBackend::isolate`]) and adopt it into its process group / job. Every isolated spawn
/// must go through here: on Windows the child starts suspended until adopted.
pub fn spawn(cmd: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    #[cfg(unix)]
    return crate::trees::spawn_registered(cmd);
    #[cfg(not(unix))]
    spawn_adopted(cmd)
}

#[cfg(not(unix))]
fn spawn_adopted(cmd: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    let child = cmd.spawn()?;
    if let Err(e) = <Process as ProcessBackend>::adopt(&child) {
        // a child we can't contain must not keep running unattended
        if let Some(pid) = child.id() {
            let _ = <Process as ProcessBackend>::signal_tree(pid, Signal::Kill);
        }
        return Err(e);
    }
    Ok(child)
}

/// Signal the tree rooted at `pid`; errors (already gone, unsupported) are ignored.
pub fn signal_tree(pid: Option<u32>, sig: Signal) {
    if let Some(pid) = pid {
        let _ = <Process as ProcessBackend>::signal_tree(pid, sig);
    }
}

/// Whether the tree started as `pid` still has live processes.
pub fn tree_alive(pid: Option<u32>) -> bool {
    pid.is_some_and(<Process as ProcessBackend>::tree_alive)
}

#[cfg(all(test, windows))]
mod exit_tests {
    use super::*;

    #[test]
    fn exit_callbacks_run_once_in_reverse_order() {
        static CALLED: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());
        extern "C" fn first() {
            CALLED.lock().unwrap().push(1);
        }
        extern "C" fn second() {
            CALLED.lock().unwrap().push(2);
        }
        let registry = std::sync::Mutex::new(vec![first as extern "C" fn(), second as extern "C" fn()]);
        run_callbacks(&registry);
        run_callbacks(&registry);
        assert_eq!(*CALLED.lock().unwrap(), [2, 1]);
    }
}
