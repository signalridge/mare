use std::io::Read;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use super::*;

fn path_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vec![("PATH".into(), std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()))]
}

fn sh(script: &str) -> (Terminal, std::sync::mpsc::Receiver<Vec<u8>>) {
    let args = ["-c".to_string(), script.to_string()];
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let (t, mut r, _w) =
        Terminal::spawn(TerminalCommand { prog: "sh", args: &args, cwd: &cwd, env: path_env(), cols: 80, rows: 24 })
            .expect("spawn");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    (t, rx)
}

fn output_until(rx: &std::sync::mpsc::Receiver<Vec<u8>>, needle: &str) -> String {
    let end = Instant::now() + Duration::from_secs(10);
    let mut out = String::new();
    while !out.contains(needle) && Instant::now() < end {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(100)) {
            out.push_str(&String::from_utf8_lossy(&b));
        }
    }
    out
}

/// The child leads its own session with the slave as its controlling terminal (`/dev/tty`
/// opens, the terminal's foreground group is the child's), and inherits nothing but the
/// terminal: neither the master nor a descriptor someone opened without close-on-exec.
#[test]
fn terminal_child_has_its_own_session_and_controlling_tty_and_inherits_only_it() {
    let stray = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
    assert!(stray > 2);
    let (mut t, rx) = sh("exec 3</dev/tty && echo CTTY-OK; sleep 30");
    let master = t.as_raw_fd();
    let out = output_until(&rx, "CTTY-OK");
    assert!(out.contains("CTTY-OK"), "no controlling terminal: {out:?}");
    let pid = t.pid() as libc::pid_t;
    assert_eq!(unsafe { libc::getsid(pid) }, pid, "the child must lead its own session");
    assert_eq!(unsafe { libc::tcgetpgrp(master) }, pid, "the child's group owns the terminal");
    assert_ne!(unsafe { libc::fcntl(master, libc::F_GETFD) } & libc::FD_CLOEXEC, 0, "master must be CLOEXEC");
    signal_tree(Some(t.pid()), Signal::Kill);
    t.kill().unwrap();

    let script =
        format!("for fd in {stray} {master}; do if [ -e /dev/fd/$fd ]; then echo LEAK:$fd; fi; done; echo CHECKED");
    let (mut t, rx) = sh(&script);
    let out = output_until(&rx, "CHECKED");
    assert!(out.contains("CHECKED") && !out.contains("LEAK"), "{out:?}");
    let _ = t.kill();
    unsafe { libc::close(stray) };
}

/// Exit codes, resize and kill keep working on the terminal mare starts itself.
#[test]
fn terminal_reports_exit_resizes_and_kills() {
    let (mut t, rx) = sh("stty size; exit 7");
    let out = output_until(&rx, "24 80");
    assert!(out.contains("24 80"), "{out:?}");
    let end = Instant::now() + Duration::from_secs(10);
    while t.try_wait().unwrap().is_none() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(t.try_wait().unwrap(), Some(7));

    let (mut t, rx) = sh("trap 'stty size; echo RESIZED' WINCH; echo READY; while :; do sleep 0.05; done");
    assert!(output_until(&rx, "READY").contains("READY"));
    t.resize(100, 30).unwrap();
    let out = output_until(&rx, "RESIZED");
    assert!(out.contains("30 100"), "{out:?}");
    t.kill().unwrap();
    signal_tree(Some(t.pid()), Signal::Kill);
    let end = Instant::now() + Duration::from_secs(10);
    while t.try_wait().unwrap().is_none() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(t.try_wait().unwrap().is_some(), "kill must end the child");
    t.kill().expect("killing an exited child is fine");
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct ProcessIdentity {
    pid: libc::pid_t,
    start_ticks: u64,
}

#[cfg(target_os = "linux")]
fn proc_stat(pid: libc::pid_t) -> std::io::Result<(char, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let rest = stat
        .rsplit_once(") ")
        .map(|(_, rest)| rest)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing /proc stat comm field"))?;
    let fields: Vec<_> = rest.split_whitespace().collect();
    let state = fields
        .first()
        .and_then(|s| s.chars().next())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing /proc stat state"))?;
    // starttime is field 22; `rest` begins at field 3 (state).
    let start_ticks = fields
        .get(19)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing /proc stat starttime"))?;
    Ok((state, start_ticks))
}

#[cfg(target_os = "linux")]
fn process_identity(pid: libc::pid_t) -> Option<ProcessIdentity> {
    let (_, start_ticks) = proc_stat(pid).ok()?;
    Some(ProcessIdentity { pid, start_ticks })
}

#[cfg(target_os = "linux")]
fn gone(process: ProcessIdentity) -> bool {
    let pid = process.pid;
    let missing =
        || (unsafe { libc::kill(pid, 0) }) != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    if missing() {
        return true;
    }
    match proc_stat(pid) {
        // An orphan nobody has reaped yet is dead too.
        Ok((state, start_ticks)) => state == 'Z' || start_ticks != process.start_ticks,
        // It can be reaped between kill(pid, 0) and reading /proc. Confirm it no longer exists;
        // a different start time means this PID now names a new process. Other read errors are
        // not evidence that the original process has ended.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => missing(),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
#[test]
fn a_reused_pid_is_not_the_original_process() {
    let current = process_identity(unsafe { libc::getpid() }).expect("current process identity");
    let replacement = ProcessIdentity { pid: current.pid, start_ticks: current.start_ticks.wrapping_add(1) };
    assert!(!gone(current), "the current process is still alive");
    assert!(gone(replacement), "a process with a different start time only reused the PID");
}

/// Linux: `kill -9` of the process owning the terminal ends the terminal's
/// tree without mare's help. The child ignores SIGHUP, so only PR_SET_PDEATHSIG can end it;
/// its background grandchild goes with the session's hangup when the leader dies. The owner
/// is this test binary re-run as a helper (it holds the terminal, then gets SIGKILL).
#[cfg(target_os = "linux")]
#[test]
fn terminal_tree_dies_with_its_killed_owner() {
    const NAME: &str = "platform::unix_tests::terminal_tree_dies_with_its_killed_owner";
    if let Some(dir) = std::env::var_os("MARE_PTY_OWNER_DIR") {
        let dir = std::path::PathBuf::from(dir);
        let gc = dir.join("grandchild");
        let script = format!("sleep 300 & echo $! > '{}'; trap '' HUP; exec sleep 300", gc.display());
        let (t, _rx) = sh(&script);
        std::fs::write(dir.join("child.tmp"), t.pid().to_string()).unwrap();
        std::fs::rename(dir.join("child.tmp"), dir.join("child")).unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let d = tempfile::tempdir().unwrap();
    let mut owner = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", NAME, "--nocapture", "--test-threads=1"])
        .env("MARE_PTY_OWNER_DIR", d.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let read_pid =
        |name: &str| -> Option<libc::pid_t> { std::fs::read_to_string(d.path().join(name)).ok()?.trim().parse().ok() };
    let end = Instant::now() + Duration::from_secs(20);
    while (read_pid("child").is_none() || read_pid("grandchild").is_none()) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    let (child, grandchild) = (
        process_identity(read_pid("child").expect("helper child")).expect("child identity"),
        process_identity(read_pid("grandchild").expect("grandchild")).expect("grandchild identity"),
    );
    assert!(!gone(child) && !gone(grandchild), "the tree must be running before the owner dies");
    owner.kill().unwrap(); // SIGKILL: no atexit, no Drop
    owner.wait().unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    while !(gone(child) && gone(grandchild)) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    let alive: Vec<_> = [child, grandchild].into_iter().filter(|p| !gone(*p)).collect();
    for p in &alive {
        if process_identity(p.pid).is_some_and(|current| current.start_ticks == p.start_ticks) {
            unsafe { libc::kill(p.pid, libc::SIGKILL) };
        }
    }
    assert!(alive.is_empty(), "outlived their killed owner: {alive:?}");
}

/// A polite signal to a bubblewrap sandbox reaches the command but not bwrap's monitor, which
/// leads the group and would die of it, taking the whole sandbox down at once
/// (`--die-with-parent`): on GitHub's Ubuntu runner, Esc on a persistent-shell command that traps
/// INT and TERM ended it at once instead of "stopping…". Any other leader gets the signal as
/// before. A shell script named `bwrap` stands in for the monitor, so no namespaces are needed.
#[cfg(target_os = "linux")]
#[test]
fn polite_signals_spare_a_bwrap_leader_but_reach_its_group() {
    use std::os::unix::process::CommandExt;
    let d = tempfile::tempdir().unwrap();
    for leader in ["bwrap", "not-bwrap"] {
        let exe = d.path().join(leader);
        let done = d.path().join(format!("{leader}.done"));
        let script = format!(
            "#!/bin/sh\nsleep 30 &\necho $! > '{}.member'\nwait\ntouch '{}'\nsleep 30\n",
            exe.display(),
            done.display()
        );
        std::fs::write(&exe, script).unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut c = std::process::Command::new(&exe).process_group(0).spawn().unwrap();
        let member_file = d.path().join(format!("{leader}.member"));
        let end = Instant::now() + Duration::from_secs(10);
        let member = loop {
            if let Some(p) =
                std::fs::read_to_string(&member_file).ok().and_then(|s| s.trim().parse::<libc::pid_t>().ok())
            {
                break p;
            }
            assert!(Instant::now() < end, "{leader}: the member never started");
            std::thread::sleep(Duration::from_millis(20));
        };
        signal_tree(Some(c.id()), Signal::Term);
        let end = Instant::now() + Duration::from_secs(10);
        while !done.exists() && c.try_wait().unwrap().is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        let leader_survived = c.try_wait().unwrap().is_none() && done.exists();
        signal_tree(Some(c.id()), Signal::Kill);
        let _ = c.wait();
        unsafe { libc::kill(member, libc::SIGKILL) };
        if leader == "bwrap" {
            assert!(leader_survived, "SIGTERM ended the bwrap leader instead of only its group's other members");
        } else {
            assert!(!leader_survived, "any other leader gets the signal with its group");
        }
    }
}

#[test]
fn terminal_hang_up_delivers_sighup_once_all_master_fds_drop_and_keeps_waiter() {
    let args = ["-c".to_string(), "trap 'exit 42' HUP; echo READY; while :; do sleep 0.05; done".to_string()];
    let cwd = std::env::temp_dir().to_string_lossy().into_owned();
    let (mut t, mut r, w) =
        Terminal::spawn(TerminalCommand { prog: "sh", args: &args, cwd: &cwd, env: path_env(), cols: 80, rows: 24 })
            .expect("spawn");
    let mut out = String::new();
    let mut buf = [0u8; 4096];
    while !out.contains("READY") {
        let n = r.read(&mut buf).expect("read READY");
        assert!(n > 0, "unexpected EOF before READY: {out:?}");
        out.push_str(&String::from_utf8_lossy(&buf[..n]));
    }

    // `hang_up` drops Terminal's own master fd, while `r` and `w` still hold their dup'd master fds.
    t.hang_up();
    assert_eq!(t.resize(90, 25).unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    assert!(t.try_wait().unwrap().is_none());

    // Dropping `w` and `r` closes the remaining master fds, delivering SIGHUP while `t.wait()`
    // remains available to collect the trapped exit code.
    drop(w);
    drop(r);
    let (code, sig) = t.wait().expect("wait after hang_up");
    assert_eq!(code, Some(42));
    assert_eq!(sig, None);
}

#[test]
fn setsid_eperm_falls_back_to_setpgid_and_other_errors_fail() {
    let mut pgid_calls = 0;
    process::isolate_session_with(
        || Ok(()),
        || {
            pgid_calls += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(pgid_calls, 0, "setpgid is skipped when setsid succeeds");

    process::isolate_session_with(
        || Err(std::io::Error::from_raw_os_error(libc::EPERM)),
        || {
            pgid_calls += 1;
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(pgid_calls, 1, "EPERM from setsid must fall back to setpgid");

    let err = process::isolate_session_with(
        || Err(std::io::Error::from_raw_os_error(libc::EPERM)),
        || Err(std::io::Error::from_raw_os_error(libc::EACCES)),
    )
    .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EACCES));

    let err = process::isolate_session_with(
        || Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
        || {
            pgid_calls += 1;
            Ok(())
        },
    )
    .unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
    assert_eq!(pgid_calls, 1, "non-EPERM error must not fall back to setpgid");
}

#[test]
fn signal_tree_delivers_polite_signal_once_to_leader_and_other_session_group() {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    struct Guard {
        child: Option<std::process::Child>,
        other_pgid: Option<libc::pid_t>,
        other_pgid_file: std::path::PathBuf,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let own_pgrp = unsafe { libc::getpgrp() };
            let other = self
                .other_pgid
                .or_else(|| std::fs::read_to_string(&self.other_pgid_file).ok()?.trim().parse::<libc::pid_t>().ok());
            if let Some(pgid) = other.filter(|&p| p > 1 && p != own_pgrp) {
                // SAFETY: `other_pgid` is the isolated child's own process group, never the test runner's.
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
            }
            if let Some(mut child) = self.child.take() {
                signal_tree(Some(child.id()), Signal::Kill);
                let _ = child.wait();
            }
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("sig.sock");
    let other_pgid_file = dir.path().join("other.pgid");
    let listener = UnixListener::bind(&sock_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let script = dir.path().join("session_members.py");
    std::fs::write(
        &script,
        r#"import os, signal, socket
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.connect('sig.sock')
other_pgid = 0
if os.getsid(0) == os.getpid():
    r, w = os.pipe()
    child = os.fork()
    if child == 0:
        os.close(r)
        os.setpgid(0, 0)
        signal.signal(signal.SIGINT, lambda *_: s.sendall(b'other\n'))
        os.write(w, b'R')
        os.close(w)
        while True:
            signal.pause()
    os.close(w)
    other_pgid = child
    with open('other.pgid', 'w') as f:
        f.write(str(other_pgid))
    assert os.read(r, 1) == b'R'
    os.close(r)
signal.signal(signal.SIGINT, lambda *_: s.sendall(b'leader\n'))
s.sendall(f'ready {other_pgid}\n'.encode())
while True:
    signal.pause()
"#,
    )
    .unwrap();

    let mut cmd = std::process::Command::new("python3");
    cmd.arg(&script)
        .current_dir(dir.path())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    isolate_std(&mut cmd);
    let child = cmd.spawn().unwrap();
    let pid = child.id();
    let mut guard = Guard { child: Some(child), other_pgid: None, other_pgid_file };

    let accept_deadline = Instant::now() + Duration::from_secs(10);
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if let Some(status) = guard.child.as_mut().unwrap().try_wait().unwrap() {
                    panic!("session_members.py exited before connecting: {status}");
                }
                assert!(Instant::now() < accept_deadline, "timed out waiting for session_members.py to connect");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("listener.accept failed: {e}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut reader = std::io::BufReader::new(stream);
    let mut ready = String::new();
    reader.read_line(&mut ready).unwrap();
    let other_pgid: libc::pid_t = ready
        .strip_prefix("ready ")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("unexpected ready record: {ready:?}"));
    if other_pgid > 0 {
        guard.other_pgid = Some(other_pgid);
    }

    let expected: Vec<String> =
        if other_pgid > 0 { vec!["leader\n".to_string(), "other\n".to_string()] } else { vec!["leader\n".to_string()] };
    const ROUNDS: usize = 10;
    for round in 0..ROUNDS {
        signal_tree(Some(pid), Signal::Int);
        let mut seen = Vec::with_capacity(expected.len());
        for _ in 0..expected.len() {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            seen.push(line);
        }
        seen.sort();
        assert_eq!(seen, expected, "round {round}: each session group must receive exactly one SIGINT");
    }

    signal_tree(Some(pid), Signal::Kill);
    let _ = guard.child.take().unwrap().wait();
    drop(guard);
    let mut tail = String::new();
    reader.read_to_string(&mut tail).unwrap();
    assert!(tail.is_empty(), "duplicate SIGINT leaked after polite delivery: {tail:?}");
}
