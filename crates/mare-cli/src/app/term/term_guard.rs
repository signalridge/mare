//! Terminal modes: entering, handing over, taking back; Ctrl+Z.

use crate::app::*;

/// Ctrl+Z: restore the terminal, stop the process like any job (SIGTSTP), and re-enter the
/// TUI with a full redraw once the shell continues us in the foreground (SIGCONT).
/// If the controlling terminal is lost while stopped, quit cleanly instead of re-entering.
pub(in crate::app) fn suspend(app: &mut App, guard: &TermGuard) {
    if !app.transcript.is_open() {
        app.renderer.erase();
    }
    app.ui.focus_known.store(false, std::sync::atomic::Ordering::Relaxed);
    if !suspend_terminal(guard) {
        app.quit = true;
        return;
    }
    app.reacquire_terminal(app_now());
    app.emit("ui/resume", serde_json::json!({}));
    super::probe::refresh_background_inband(app);
}

#[cfg(unix)]
fn tty_foreground_pgrp() -> Option<libc::pid_t> {
    for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO] {
        // SAFETY: tcgetpgrp only queries the controlling terminal's foreground process group.
        let pgrp = unsafe { libc::tcgetpgrp(fd) };
        if pgrp >= 0 {
            return Some(pgrp);
        }
    }
    None
}

#[cfg(unix)]
pub(in crate::app) fn suspend_until_foreground() -> bool {
    // Only enforce foreground ownership when we had a controlling terminal before stopping
    // (nested/headless PTY harnesses without TIOCSCTTY report no controlling tty).
    let had_ctty = tty_foreground_pgrp().is_some();
    // SAFETY: getpgrp has no preconditions.
    let our_pgrp = unsafe { libc::getpgrp() };
    loop {
        // SAFETY: SIGTSTP is a standard job-control stop signal.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        // Execution resumes here after SIGCONT.
        if !had_ctty {
            return true;
        }
        match tty_foreground_pgrp() {
            // Foreground regained (or degenerate pgrp 0): safe to re-enter TUI modes.
            Some(fg) if fg == our_pgrp || fg == 0 => return true,
            // Resumed in the background (`bg` + SIGCONT): do not touch termios or steal
            // foreground from the shell; stop again until brought back to foreground (`fg`).
            Some(_) => continue,
            // Controlling terminal was lost/revoked while stopped: cannot re-enter TUI safely.
            None => return false,
        }
    }
}

fn suspend_terminal(guard: &TermGuard) -> bool {
    guard.leave();
    #[cfg(unix)]
    if !suspend_until_foreground() {
        return false;
    }
    // execution resumes here after SIGCONT in the foreground
    guard.reenter();
    true
}

/// Terminal modes for the TUI (`mare_tui::modes`: kitty keyboard owned and popped exactly
/// once, handoff to editors and the shell, the exit reset list). Every byte goes through the
/// single write exit.
pub(in crate::app) struct TermGuard;

impl TermGuard {
    pub(in crate::app) fn enter() -> Result<Self> {
        tty::enter_raw()?;
        mare_tui::modes::enter();
        // no terminal teardown in a panic hook: panics unwind, and one caught inside a Lua
        // callback (a plugin error) must leave the TUI running; a fatal one unwinds through
        // `drop` below, which restores the terminal
        crate::TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(Self)
    }

    /// The probe found the kitty keyboard protocol: push the flags (once).
    pub(in crate::app) fn enable_kitty(&self) {
        mare_tui::modes::enable_kitty();
    }

    /// Hand the terminal back in its original state (exit, Ctrl+Z, external editor).
    fn leave(&self) {
        hand_off_terminal();
    }

    /// Back from a handoff: the same modes `enter` set up.
    fn reenter(&self) {
        take_back_terminal();
    }
}

/// Give the terminal to another program (or back to the shell): our modes off, raw mode off.
pub(crate) fn hand_off_terminal() {
    if mare_tui::modes::handed_off() {
        return;
    }
    mare_tui::modes::leave();
    tty::leave_raw();
    crate::TUI_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
}

pub(crate) fn take_back_terminal() {
    let _ = tty::enter_raw();
    crate::TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
    mare_tui::modes::resume();
}

/// Reclaim terminal teardown ownership during shutdown after an aborted external editor has exited.
/// Restores cooked mode immediately (in case a raw-mode editor was killed before restoring termios)
/// and clears `handed_off` so `TermGuard::drop` completes `leave()` + `tty::close()`.
pub(crate) fn reclaim_terminal_for_exit(leave_alt: bool) {
    mare_tui::modes::end_handoff_for_exit(leave_alt);
    tty::leave_raw();
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        // Dropping termina also restores termios; do not disturb a program that owns the tty.
        if !mare_tui::modes::handed_off() {
            self.leave();
            // the console modes and code pages as they were (Windows), the SIGWINCH handler off
            tty::close();
        }
        // a panic is unwinding out of the TUI: the hook stayed silent, report it now
        if std::thread::panicking()
            && let Some(p) = crate::write_crash_report()
        {
            use std::io::Write as _;
            // Never use `eprintln!` while already unwinding: a broken/closed stderr pipe
            // would trigger a second panic and abort the process.
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "mare crashed; report saved to {}", p.display());
            // Alacritty / WezTerm don't reset a leftover kitty keyboard mode on their own
            let _ = writeln!(err, "if the terminal behaves oddly (keys, cursor), run `reset`");
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::Ordering;

    // Run the real terminal mode calls in a separate process with its own pseudoterminal.
    fn in_pty(test: &str, expect_stop: bool) -> Option<Vec<u8>> {
        const CHILD: &str = "MARE_TERM_GUARD_PTY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return None;
        }
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty initializes both descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            },
            0
        );
        // SAFETY: each successful openpty descriptor is owned exactly once.
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            // SAFETY: these descriptors belong to this fixture; stdio is duplicated on exec.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
        }
        let original = attrs_for(master.as_raw_fd());
        let state = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .env("XDG_STATE_HOME", state.path())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::piped());
        if expect_stop {
            // SAFETY: setpgid is async-signal-safe and keeps the test runner outside the job.
            unsafe {
                command
                    .pre_exec(|| if libc::setpgid(0, 0) == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) });
            }
        }
        let mut child = command.spawn().unwrap();
        drop(command);
        drop(slave);
        // Drain the PTY master concurrently: on macOS Darwin, tcsetattr(TCSAFLUSH) inside
        // TermGuard::enter() waits for t_outq (including libtest's initial stdout) to drain.
        let mut master_reader = master.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut terminal = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                match master_reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => terminal.extend_from_slice(&buf[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    // A PTY master can return EIO, rather than EOF, after its final slave closes.
                    Err(_) => break,
                }
            }
            terminal
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let pid = child.id() as libc::pid_t;
        let watchdog = std::thread::spawn(move || {
            if matches!(done_rx.recv_timeout(Duration::from_secs(10)), Err(std::sync::mpsc::RecvTimeoutError::Timeout))
            {
                // SAFETY: bound a stuck fixture without sending a signal to the test runner.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        });
        let mut stopped_result = None;
        if expect_stop {
            let mut status = 0;
            let waited = loop {
                // SAFETY: this waits only for the isolated child, including its stop notification.
                let result = unsafe { libc::waitpid(child.id() as libc::pid_t, &mut status, libc::WUNTRACED) };
                if result >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    break result;
                }
            };
            let stopped = waited > 0 && libc::WIFSTOPPED(status);
            // Record the state before resuming; always resume or kill before asserting.
            let mut stopped_attrs = unsafe { std::mem::zeroed::<libc::termios>() };
            let attr_result =
                if stopped { unsafe { libc::tcgetattr(master.as_raw_fd(), &mut stopped_attrs) } } else { -1 };
            let mut continued = -1;
            if stopped {
                // SAFETY: SIGCONT targets only the stopped fixture child.
                continued = unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGCONT) };
            }
            if waited < 0 || (stopped && continued != 0) {
                let _ = child.kill();
            }
            stopped_result = Some((waited, status, stopped, continued, attr_result, stopped_attrs.c_lflag));
        }
        let output = child.wait_with_output();
        let _ = done_tx.send(());
        watchdog.join().unwrap();
        drop(master);
        let terminal = reader.join().unwrap();
        if let Some((waited, status, stopped, continued, attr_result, lflag)) = stopped_result {
            assert!(stopped, "fixture did not stop on SIGTSTP: wait={waited} status={status}");
            assert_eq!(continued, 0);
            assert_eq!(libc::WSTOPSIG(status), libc::SIGTSTP);
            assert_eq!(attr_result, 0);
            assert_eq!(
                persistent_lflag(lflag),
                persistent_lflag(original.c_lflag),
                "terminal must be cooked while stopped"
            );
        }
        let output = output.unwrap();
        assert!(output.status.success(), "PTY child failed: {}", String::from_utf8_lossy(&output.stderr));
        Some(terminal)
    }

    fn attrs_for(fd: libc::c_int) -> libc::termios {
        // SAFETY: tcgetattr fills a valid termios structure for the child PTY.
        let mut value = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(fd, &mut value) }, 0);
        value
    }

    fn attrs() -> libc::termios {
        attrs_for(libc::STDOUT_FILENO)
    }

    // PENDIN is a transient kernel input-retype bit (set by Darwin XNU when re-enabling ICANON),
    // not a persistent terminal mode restored by TermGuard.
    fn persistent_lflag(flag: libc::tcflag_t) -> libc::tcflag_t {
        flag & !libc::PENDIN
    }

    fn set_attrs(value: &libc::termios) {
        // SAFETY: value is an initialized termios obtained from the child PTY.
        assert_eq!(unsafe { libc::tcsetattr(libc::STDOUT_FILENO, libc::TCSANOW, value) }, 0);
    }

    fn signal_handler(signal: libc::c_int) -> libc::sighandler_t {
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        // SAFETY: querying the current disposition writes into an initialized structure.
        assert_eq!(unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) }, 0);
        action.sa_sigaction
    }

    #[test]
    fn handoff_resume_and_drop_restore_owned_terminal_modes() {
        if in_pty("app::term::term_guard::tests::handoff_resume_and_drop_restore_owned_terminal_modes", false).is_some()
        {
            return;
        }
        let _lock = mare_tui::out::test_lock();
        mare_tui::out::capture(true);
        let mut original = attrs();
        original.c_lflag |= libc::ECHO | libc::ICANON | libc::ISIG;
        set_attrs(&original);
        let guard = TermGuard::enter().unwrap();
        assert_eq!(attrs().c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG), 0);
        guard.enable_kitty();
        mare_tui::modes::set_mouse(true);
        let entered = String::from_utf8(mare_tui::out::capture(true)).unwrap();
        assert!(entered.contains("\x1b[?2004h\x1b[?1004h\x1b[>5u"), "{entered:?}");
        assert!(entered.contains("\x1b[?1002h\x1b[?1006h"), "{entered:?}");

        guard.leave();
        let left = String::from_utf8(mare_tui::out::capture(true)).unwrap();
        assert!(left.starts_with("\x1b[0m\x1b[?25h\x1b[<1u"), "{left:?}");
        assert!(left.contains("\x1b[?2004l\x1b[?1004l"), "{left:?}");
        assert_eq!(persistent_lflag(attrs().c_lflag), persistent_lflag(original.c_lflag));
        assert!(!crate::TUI_ACTIVE.load(Ordering::SeqCst));

        // An editor owns the PTY now; a second handoff must not reset its termios.
        let mut editor = attrs();
        editor.c_lflag &= !(libc::ECHO | libc::ICANON);
        set_attrs(&editor);
        guard.leave();
        assert_eq!(attrs().c_lflag, editor.c_lflag);
        assert!(mare_tui::out::capture(true).is_empty());

        guard.reenter();
        assert_eq!(attrs().c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG), 0);
        assert!(crate::TUI_ACTIVE.load(Ordering::SeqCst));
        let resumed = String::from_utf8(mare_tui::out::capture(true)).unwrap();
        assert!(resumed.contains("\x1b[?2004h\x1b[?1004h\x1b[>5u"), "{resumed:?}");
        assert!(resumed.contains("\x1b[?1002h\x1b[?1006h"), "{resumed:?}");
        drop(guard);
        assert!(!mare_tui::out::busy(), "exit must drain queued terminal resets before returning");
        let exit = String::from_utf8(mare_tui::out::capture(false)).unwrap();
        assert!(exit.starts_with("\x1b[0m\x1b[?25h\x1b[<1u"), "{exit:?}");
        assert!(exit.contains("\x1b[?2004l\x1b[?1004l"), "{exit:?}");
        assert_eq!(persistent_lflag(attrs().c_lflag), persistent_lflag(original.c_lflag));
        assert!(!crate::TUI_ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn dropping_handed_off_guard_does_not_restore_over_the_editor() {
        if in_pty("app::term::term_guard::tests::dropping_handed_off_guard_does_not_restore_over_the_editor", false)
            .is_some()
        {
            return;
        }
        let _lock = mare_tui::out::test_lock();
        mare_tui::out::capture(true);
        let guard = TermGuard::enter().unwrap();
        guard.leave();
        let mut editor = attrs();
        editor.c_lflag &= !(libc::ECHO | libc::ICANON);
        set_attrs(&editor);
        mare_tui::out::capture(true);
        drop(guard);
        assert_eq!(attrs().c_lflag, editor.c_lflag);
        assert!(mare_tui::out::capture(false).is_empty());
        tty::close(); // the simulated editor has now relinquished the terminal
    }

    #[test]
    fn sigtstp_handoff_restores_terminal_until_sigcont() {
        if in_pty("app::term::term_guard::tests::sigtstp_handoff_restores_terminal_until_sigcont", true).is_some() {
            return;
        }
        let _lock = mare_tui::out::test_lock();
        mare_tui::out::capture(true);
        // SAFETY: signal state is changed only in this isolated fixture process.
        unsafe {
            libc::signal(libc::SIGTSTP, libc::SIG_DFL);
            let mut set = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTSTP);
            assert_eq!(libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()), 0);
        }
        let handlers = [signal_handler(libc::SIGTSTP), signal_handler(libc::SIGCONT)];
        let guard = TermGuard::enter().unwrap();
        guard.enable_kitty();
        mare_tui::modes::set_mouse(true);
        mare_tui::out::capture(true);
        suspend_terminal(&guard);
        assert_eq!([signal_handler(libc::SIGTSTP), signal_handler(libc::SIGCONT)], handlers);
        assert_eq!(attrs().c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG), 0);
        assert!(crate::TUI_ACTIVE.load(Ordering::SeqCst));
        let output = String::from_utf8(mare_tui::out::capture(true)).unwrap();
        let stop = output.find("\x1b[0m\x1b[?25h\x1b[<1u").unwrap();
        let resume = output.find("\x1b[?2004h\x1b[?1004h\x1b[>5u").unwrap();
        assert!(stop < resume && output[resume..].contains("\x1b[?1002h\x1b[?1006h"), "{output:?}");
        drop(guard);
        assert!(!mare_tui::out::busy());
        mare_tui::out::capture(false);
    }

    #[test]
    fn guard_drop_flushes_terminal_resets_before_process_exit() {
        if let Some(output) =
            in_pty("app::term::term_guard::tests::guard_drop_flushes_terminal_resets_before_process_exit", false)
        {
            let output = String::from_utf8_lossy(&output);
            let pushed = output.find("\x1b[>5u").expect("kitty mode was enabled");
            let reset = output.find("\x1b[0m\x1b[?25h\x1b[<1u").expect("exit resets reached the PTY");
            assert!(pushed < reset && output[reset..].contains("\x1b[?2004l\x1b[?1004l"), "{output:?}");
            return;
        }
        let guard = TermGuard::enter().unwrap();
        guard.enable_kitty();
        drop(guard);
        // SAFETY: only the isolated fixture exits; this exposes queued output lost on immediate exit.
        unsafe { libc::_exit(0) }
    }

    #[test]
    fn guard_drop_during_panic_prints_reset_hint() {
        const CHILD: &str = "MARE_TERM_GUARD_PANIC_CHILD";
        if std::env::var_os(CHILD).is_some() {
            crate::diagnostics::install_crash_hook();
            let _guard = TermGuard::enter().unwrap();
            panic!("simulated TUI crash");
        }
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty initializes both descriptors on success.
        assert_eq!(
            unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            },
            0
        );
        // SAFETY: each successful openpty descriptor is owned exactly once.
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "app::term::term_guard::tests::guard_drop_during_panic_prints_reset_hint", "--nocapture"])
            .env(CHILD, "1")
            .env("MARE_HOME", home.path())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::piped());
        let child = command.spawn().unwrap();
        drop(command);
        drop(slave);
        let mut master_reader = master.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = master_reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
        let output = child.wait_with_output().unwrap();
        drop(master);
        let _ = reader.join();
        assert!(!output.status.success(), "panicking child must exit non-zero");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("mare crashed; report saved to"), "{stderr}");
        assert!(stderr.contains("run `reset`"), "crash hint must mention reset:\n{stderr}");
    }

    #[test]
    fn bg_resume_after_sigtstp_waits_for_foreground_before_reentering_tui() {
        const SHELL: &str = "MARE_TERM_GUARD_BG_SHELL";
        const JOB: &str = "MARE_TERM_GUARD_BG_JOB";
        if std::env::var_os(JOB).is_some() {
            let _lock = mare_tui::out::test_lock();
            mare_tui::out::capture(true);
            // SAFETY: signal state is changed only in this isolated job child process.
            unsafe {
                libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                let mut set = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTSTP);
                assert_eq!(libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()), 0);
            }
            let guard = TermGuard::enter().unwrap();
            guard.enable_kitty();
            mare_tui::out::capture(true);
            suspend_terminal(&guard);
            // After returning from suspend_terminal, we must be back in the foreground pgrp.
            let fg = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
            let me = unsafe { libc::getpgrp() };
            assert_eq!(fg, me, "re-entered TUI while not in foreground process group");
            assert_eq!(attrs().c_lflag & (libc::ECHO | libc::ICANON | libc::ISIG), 0);
            assert!(crate::TUI_ACTIVE.load(Ordering::SeqCst));
            drop(guard);
            mare_tui::out::capture(false);
            return;
        }
        if std::env::var_os(SHELL).is_some() {
            // Mini-shell session leader controlling stdin/stdout PTY.
            // Ignore SIGTTOU so this shell process can call `tcsetpgrp` freely like bash/zsh.
            unsafe {
                libc::signal(libc::SIGTTOU, libc::SIG_IGN);
            }
            let shell_pgrp = unsafe { libc::getpgrp() };
            let original = attrs();
            let mut job_cmd = Command::new(std::env::current_exe().unwrap());
            job_cmd
                .args([
                    "--exact",
                    "app::term::term_guard::tests::bg_resume_after_sigtstp_waits_for_foreground_before_reentering_tui",
                    "--nocapture",
                ])
                .env_remove(SHELL)
                .env(JOB, "1")
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::piped());
            unsafe {
                job_cmd.pre_exec(|| {
                    if libc::setpgid(0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    // Give ourselves foreground before exec so `TermGuard::enter()` owns the TTY.
                    libc::signal(libc::SIGTTOU, libc::SIG_IGN);
                    if libc::tcsetpgrp(libc::STDIN_FILENO, libc::getpgrp()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::signal(libc::SIGTTOU, libc::SIG_DFL);
                    Ok(())
                });
            }
            let job = job_cmd.spawn().unwrap();
            let job_pid = job.id() as libc::pid_t;
            let wait_stopped = |pid: libc::pid_t| -> libc::c_int {
                let mut status = 0;
                let waited = loop {
                    let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
                    if r >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                        break r;
                    }
                };
                assert!(
                    waited > 0 && libc::WIFSTOPPED(status),
                    "expected job to stop: wait={waited} status={status:#x}"
                );
                libc::WSTOPSIG(status)
            };

            // 1. Job stops on Ctrl+Z (SIGTSTP). Shell reclaims foreground and verifies cooked TTY.
            assert_eq!(wait_stopped(job_pid), libc::SIGTSTP);
            assert_eq!(unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, shell_pgrp) }, 0);
            assert_eq!(
                persistent_lflag(attrs().c_lflag),
                persistent_lflag(original.c_lflag),
                "terminal must be cooked while stopped"
            );

            // 2. Simulate `bg`: send SIGCONT while shell stays in foreground.
            //    Job must NOT re-enter raw mode; it must re-stop with SIGTSTP.
            assert_eq!(unsafe { libc::kill(-job_pid, libc::SIGCONT) }, 0);
            assert_eq!(wait_stopped(job_pid), libc::SIGTSTP, "bg-resumed job must re-stop with SIGTSTP");
            assert_eq!(
                persistent_lflag(attrs().c_lflag),
                persistent_lflag(original.c_lflag),
                "bg resume must not disturb shell's cooked terminal modes"
            );

            // 3. Simulate `fg`: hand foreground back to job and send SIGCONT.
            assert_eq!(unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, job_pid) }, 0);
            assert_eq!(unsafe { libc::kill(-job_pid, libc::SIGCONT) }, 0);
            let output = job.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "job failed after fg resume (status={:?}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            },
            0
        );
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }, 0);
        }
        let state = tempfile::tempdir().unwrap();
        let slave_fd = slave.as_raw_fd();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "app::term::term_guard::tests::bg_resume_after_sigtstp_waits_for_foreground_before_reentering_tui",
                "--nocapture",
            ])
            .env(SHELL, "1")
            .env("XDG_STATE_HOME", state.path())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(command);
        drop(slave);
        let child_pid = child.id() as libc::pid_t;
        let mut master_reader = master.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = master_reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            if matches!(done_rx.recv_timeout(Duration::from_secs(10)), Err(std::sync::mpsc::RecvTimeoutError::Timeout))
            {
                unsafe { libc::kill(-child_pid, libc::SIGKILL) };
            }
        });
        let output = child.wait_with_output().unwrap();
        let _ = done_tx.send(());
        watchdog.join().unwrap();
        drop(master);
        let _ = reader.join();
        assert!(
            output.status.success(),
            "bg/fg PTY controller failed (status={:?}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn guard_drop_during_panic_does_not_double_panic_on_closed_stderr() {
        const CHILD: &str = "MARE_TERM_GUARD_CLOSED_STDERR_CHILD";
        if std::env::var_os(CHILD).is_some() {
            crate::diagnostics::install_crash_hook();
            let _guard = TermGuard::enter().unwrap();
            panic!("simulated TUI crash with broken stderr pipe");
        }
        let (mut master, mut slave) = (-1, -1);
        assert_eq!(
            unsafe {
                libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
            },
            0
        );
        let master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        // Broken stderr pipe: close the read end before spawning so writes to fd 2 hit EPIPE.
        let mut pipe_fds = [-1; 2];
        assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
        assert_eq!(unsafe { libc::close(pipe_fds[0]) }, 0);
        let broken_stderr = unsafe { std::fs::File::from_raw_fd(pipe_fds[1]) };
        let home = tempfile::tempdir().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "app::term::term_guard::tests::guard_drop_during_panic_does_not_double_panic_on_closed_stderr",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MARE_HOME", home.path())
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(broken_stderr));
        let mut child = command.spawn().unwrap();
        drop(command);
        drop(slave);
        let mut master_reader = master.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = master_reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
        let status = child.wait().unwrap();
        drop(master);
        let _ = reader.join();
        assert_eq!(
            status.code(),
            Some(101),
            "panicking child with broken stderr pipe must unwind cleanly (exit 101), not abort on double-panic (status={status:?})"
        );
    }
}

#[cfg(test)]
mod drop_tests {
    use super::*;
    use std::io::Read;
    use std::process::{Command, Stdio};

    #[test]
    fn guard_drop_during_panic_handles_closed_stderr_on_all_platforms() {
        const CHILD: &str = "MARE_TERM_GUARD_DROP_BROKEN_STDERR";
        if std::env::var_os(CHILD).is_some() {
            crate::diagnostics::install_crash_hook();
            mare_tui::modes::leave();
            crate::TUI_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
            let _guard = TermGuard;
            let mut b = [0u8; 1];
            let _ = std::io::stdin().read(&mut b);
            panic!("simulated TUI crash with closed stderr pipe");
        }
        let home = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app::term::term_guard::drop_tests::guard_drop_during_panic_handles_closed_stderr_on_all_platforms",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MARE_HOME", home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // Close stderr read end before signalling stdin EOF so child stderr writes hit BrokenPipe.
        drop(child.stderr.take());
        drop(child.stdin.take());
        let status = child.wait().unwrap();
        assert_eq!(
            status.code(),
            Some(101),
            "TermGuard::drop must unwind cleanly (exit 101) when stderr pipe is closed (status={status:?})"
        );
        let crashes = home.path().join("state").join("crashes");
        let has_report = std::fs::read_dir(&crashes)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join("report.txt").is_file());
        assert!(has_report, "expected crash report under {}", crashes.display());
    }
}
