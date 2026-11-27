//! Unix pseudo-terminals: openpty and the pre-exec session setup.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};

pub(crate) type ExitRx =
    tokio::sync::watch::Receiver<Option<Result<super::TerminalExitStatus, (io::ErrorKind, String)>>>;

pub struct Imp {
    pub(super) master: Option<File>,
    pid: u32,
    exit_rx: ExitRx,
    exited: Arc<(Mutex<bool>, Condvar)>,
    wait_thread: Option<std::thread::JoinHandle<()>>,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

fn set_winsize(fd: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = winsize(cols, rows);
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws as *const libc::winsize) } == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Linux: master and slave opened close-on-exec atomically, so no process spawned
/// concurrently by another thread can inherit them (a held master keeps the terminal open:
/// the child never sees the hangup).
#[cfg(target_os = "linux")]
fn open_pty(cols: u16, rows: u16) -> io::Result<(File, File)> {
    let last = io::Error::last_os_error;
    unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if m < 0 {
            return Err(last());
        }
        let master = File::from_raw_fd(m);
        if libc::grantpt(m) != 0 || libc::unlockpt(m) != 0 {
            return Err(last());
        }
        let mut name = [0 as libc::c_char; 256];
        let r = libc::ptsname_r(m, name.as_mut_ptr(), name.len());
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r));
        }
        let s = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
        if s < 0 {
            return Err(last());
        }
        let slave = File::from_raw_fd(s);
        set_winsize(m, cols, rows)?;
        Ok((master, slave))
    }
}

/// Elsewhere openpty, then close-on-exec on both ends right away.
#[cfg(not(target_os = "linux"))]
fn open_pty(cols: u16, rows: u16) -> io::Result<(File, File)> {
    let (mut m, mut s): (libc::c_int, libc::c_int) = (-1, -1);
    let mut ws = winsize(cols, rows);
    let r = unsafe {
        libc::openpty(&mut m, &mut s, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::addr_of_mut!(ws))
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let (master, slave) = unsafe { (File::from_raw_fd(m), File::from_raw_fd(s)) };
    for fd in [m, s] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((master, slave))
}

/// In the child before exec: descriptors above stderr that someone opened without
/// close-on-exec get it now, so the program inherits only its terminal. Marking (rather than
/// closing, as portable-pty did) keeps std's exec-error pipe working. Linux: one
/// async-signal-safe `close_range(CLOSE_RANGE_CLOEXEC)` (5.11+); else (and as the fallback)
/// the `/dev/fd` walk.
fn cloexec_inherited() {
    #[cfg(target_os = "linux")]
    {
        const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;
        if unsafe { libc::syscall(libc::SYS_close_range, 3 as libc::c_uint, libc::c_uint::MAX, CLOSE_RANGE_CLOEXEC) }
            == 0
        {
            return;
        }
    }
    if let Ok(dir) = std::fs::read_dir("/dev/fd") {
        let fds: Vec<RawFd> =
            dir.filter_map(|e| e.ok()?.file_name().into_string().ok()?.parse().ok()).filter(|fd| *fd > 2).collect();
        for fd in fds {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags != -1 && flags & libc::FD_CLOEXEC == 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_MASTER_CLONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn clone_master(master: &File) -> io::Result<File> {
    #[cfg(test)]
    if FAIL_MASTER_CLONE.with(|flag| flag.replace(false)) {
        return Err(io::Error::other("injected PTY descriptor failure"));
    }
    master.try_clone()
}

impl Imp {
    pub fn spawn(
        c: super::TerminalCommand<'_>,
        mut restrict: Option<super::PreExec>,
    ) -> io::Result<(Self, super::TerminalReader, super::TerminalWriter)> {
        let (master, slave) = open_pty(c.cols, c.rows)?;
        let mut cmd = Command::new(c.prog);
        cmd.args(c.args)
            .current_dir(c.cwd)
            .env_clear()
            .envs(c.env)
            // the same terminal on all three streams (dup'd close-on-exec; dup2 in the child)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        #[cfg(target_os = "linux")]
        let parent = unsafe { libc::getpid() };
        unsafe {
            cmd.pre_exec(move || {
                // Linux: the terminal's program dies with mare however mare dies (SIGKILL
                // included: atexit never runs, and only the master's hangup would remain —
                // which a program ignoring SIGHUP survives)
                #[cfg(target_os = "linux")]
                {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    // mare died between fork and here: the signal would never come
                    if libc::getppid() != parent {
                        libc::_exit(1);
                    }
                }
                // dispositions mare ignores (SIGPIPE, job-control signals in the TUI) would
                // survive exec; handlers don't, but reset everything catchable anyway
                for sig in 1..32 {
                    if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                        libc::signal(sig, libc::SIG_DFL);
                    }
                }
                let empty: libc::sigset_t = std::mem::zeroed();
                libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut());
                // own session, and the slave (stdin by now) as its controlling terminal:
                // job control, Ctrl+C from the input, SIGWINCH on resize
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                cloexec_inherited();
                // the sandbox's own restrictions come last: nothing above needs what they forbid
                if let Some(r) = restrict.as_mut() {
                    r()?;
                }
                Ok(())
            });
        }
        // Prepare every fallible terminal descriptor before starting a child. If a clone fails,
        // no process has been launched and the caller can safely release its sandbox resources.
        let reader = clone_master(&master)?;
        let writer = clone_master(&master)?;
        let mut child = crate::trees::spawn_std_registered(&mut cmd)?;
        // the slave copies were moved into `cmd`: drop it so only the child holds the slave
        drop(cmd);
        let pid = child.id();
        let (exit_tx, exit_rx) = tokio::sync::watch::channel(None);
        let exited = Arc::new((Mutex::new(false), Condvar::new()));
        let exited_sig = exited.clone();
        let wait_thread = match std::thread::Builder::new().name("mare-pty-wait".into()).spawn(move || {
            use std::os::unix::process::ExitStatusExt;
            let res =
                child.wait().map(|s| (s.code().map(|c| c as u32), s.signal())).map_err(|e| (e.kind(), e.to_string()));
            let _ = exit_tx.send(Some(res));
            let (lock, cvar) = &*exited_sig;
            *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
            cvar.notify_all();
        }) {
            Ok(handle) => handle,
            Err(e) => {
                if let Ok(raw_pid) = i32::try_from(pid) {
                    unsafe {
                        libc::kill(raw_pid, libc::SIGKILL);
                        libc::waitpid(raw_pid, std::ptr::null_mut(), 0);
                    }
                }
                return Err(e);
            }
        };
        Ok((
            Self { master: Some(master), pid, exit_rx, exited, wait_thread: Some(wait_thread) },
            Box::new(reader),
            Box::new(writer),
        ))
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub(crate) fn exit_watcher(&self) -> ExitRx {
        self.exit_rx.clone()
    }
    pub fn exit_status(&mut self) -> io::Result<Option<super::TerminalExitStatus>> {
        match self.exit_rx.borrow().as_ref() {
            None => Ok(None),
            Some(Ok(st)) => Ok(Some(*st)),
            Some(Err((kind, msg))) => Err(io::Error::new(*kind, msg.as_str())),
        }
    }
    pub fn hang_up(&mut self) {
        self.master.take();
    }
    pub fn wait(&mut self) -> io::Result<super::TerminalExitStatus> {
        if let Some(handle) = self.wait_thread.take() {
            let _ = handle.join();
        }
        self.exit_status()?.ok_or_else(|| io::Error::other("terminal waiter exited without status"))
    }
    pub fn wait_timeout(&mut self, timeout: std::time::Duration) -> io::Result<Option<super::TerminalExitStatus>> {
        let (lock, cvar) = &*self.exited;
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (guard, _) = cvar.wait_timeout_while(guard, timeout, |done| !*done).unwrap_or_else(|e| e.into_inner());
        if !*guard {
            return Ok(None);
        }
        drop(guard);
        if let Some(handle) = self.wait_thread.take() {
            let _ = handle.join();
        }
        self.exit_status()
    }
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        Ok(self.exit_status()?.map(|(code, _)| code.unwrap_or(1)))
    }
    pub fn kill(&mut self) -> io::Result<()> {
        if self.exit_rx.borrow().is_some() {
            return Ok(());
        }
        let Ok(pid) = i32::try_from(self.pid) else {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "pid overflows pid_t"));
        };
        if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ESRCH) || self.exit_rx.borrow().is_some() {
                return Ok(());
            }
            return Err(err);
        }
        Ok(())
    }
    /// Kernel terminal ownership, verified against the session we created.
    pub fn foreground(&self) -> io::Result<Option<(u32, Option<bool>)>> {
        let Some(master) = self.master.as_ref() else {
            return Ok(None);
        };
        if self.pid > i32::MAX as u32 {
            return Ok(None);
        }
        let pgid = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
        if pgid <= 0 {
            return Ok(None);
        }
        let sid = unsafe { libc::getsid(pgid) };
        if sid != self.pid as i32 {
            return Ok(None);
        }
        Ok(Some((pgid as u32, input_waiting(pgid))))
    }
    pub fn signal_foreground(&self, signal: &str, protected_pgid: Option<u32>) -> io::Result<u32> {
        let sig = match signal {
            "SIGINT" => libc::SIGINT,
            "SIGTERM" => libc::SIGTERM,
            "SIGKILL" => libc::SIGKILL,
            "SIGTSTP" => libc::SIGTSTP,
            "SIGHUP" => libc::SIGHUP,
            _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported foreground signal")),
        };
        let (pgid, _) = self.foreground()?.ok_or_else(|| io::Error::other("PTY_FOREGROUND_UNKNOWN"))?;
        if sig == libc::SIGKILL && (pgid == self.pid || Some(pgid) == protected_pgid) {
            return Err(io::Error::other("refusing SIGKILL on terminal shell; use stop"));
        }
        let Some(master) = self.master.as_ref() else {
            return Err(io::Error::other("PTY_FOREGROUND_CHANGED"));
        };
        // Recheck immediately before delivery. No arbitrary process id enters this API.
        if unsafe { libc::tcgetpgrp(master.as_raw_fd()) } != pgid as i32
            || unsafe { libc::getsid(pgid as i32) } != self.pid as i32
        {
            return Err(io::Error::other("PTY_FOREGROUND_CHANGED"));
        }
        if unsafe { libc::kill(-(pgid as i32), sig) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(pgid)
    }
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let master =
            self.master.as_ref().ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "terminal master closed"))?;
        set_winsize(master.as_raw_fd(), cols, rows)
    }
}

/// Positive evidence only: a verified terminal foreground leader blocked in read(0).
/// Other syscalls/platforms remain unknown, never "ready" inferred from silence.
#[cfg(target_os = "linux")]
fn input_waiting(pgid: i32) -> Option<bool> {
    let fd = std::fs::read_link(format!("/proc/{pgid}/fd/0")).ok()?;
    if !fd.to_string_lossy().starts_with("/dev/pts/") {
        return None;
    }
    let call = std::fs::read_to_string(format!("/proc/{pgid}/syscall")).ok()?;
    let mut fields = call.split_whitespace();
    let number = fields.next()?.parse::<libc::c_long>().ok()?;
    let fd = u64::from_str_radix(fields.next()?.trim_start_matches("0x"), 16).ok()?;
    (number == libc::SYS_read && fd == 0).then_some(true)
}
#[cfg(not(target_os = "linux"))]
fn input_waiting(_pgid: i32) -> Option<bool> {
    None
}

#[cfg(test)]
mod failure_tests {
    use super::*;

    #[test]
    fn dropping_writer_does_not_inject_input_or_echo() {
        use std::io::{BufRead, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let args = vec!["-c".to_string(), "echo READY; IFS= read -r line; echo GOT".to_string()];
        let command =
            super::super::TerminalCommand { prog: "sh", args: &args, cwd: &cwd, env: vec![], cols: 80, rows: 24 };
        let (mut term, reader, writer) = Imp::spawn(command, None).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            tx.send(line).unwrap();
            let mut next = String::new();
            let _ = reader.read_line(&mut next);
            let _ = tx.send(next);
        });
        assert!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap().contains("READY"));
        drop(writer);
        let extra = rx.recv_timeout(std::time::Duration::from_millis(200));
        let _ = term.kill();
        let _ = term.wait();
        thread.join().unwrap();
        assert!(
            matches!(extra, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
            "writer drop produced unexpected input or echo: {extra:?}"
        );
    }

    #[test]
    fn descriptor_failure_happens_before_starting_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let args = vec!["-c".to_string(), "touch continued".to_string()];
        let command =
            super::super::TerminalCommand { prog: "sh", args: &args, cwd: &cwd, env: vec![], cols: 80, rows: 24 };
        FAIL_MASTER_CLONE.with(|flag| flag.set(true));
        assert!(Imp::spawn(command, None).is_err());
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!dir.path().join("continued").exists());
    }
}
