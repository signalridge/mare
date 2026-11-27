//! Startup handshake for observing a bubblewrap PID namespace's init process.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Clear CLOEXEC only in the child, immediately before exec.
pub(crate) fn inherit(fds: [RawFd; 2]) -> io::Result<()> {
    for fd in fds {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// The gate writer must remain open until the child is killed or the handshake succeeds:
/// bubblewrap treats EOF on --block-fd as permission to proceed.
pub(super) struct Gate {
    info: Option<OwnedFd>,
    info_writer: Option<OwnedFd>,
    block_reader: Option<OwnedFd>,
    block: Option<OwnedFd>,
}

impl Gate {
    pub(super) fn new() -> io::Result<Self> {
        let (info, info_writer) = pipe()?;
        let (block_reader, block) = pipe()?;
        Ok(Self {
            info: Some(info),
            info_writer: Some(info_writer),
            block_reader: Some(block_reader),
            block: Some(block),
        })
    }

    pub(super) fn inherited(&self) -> [RawFd; 2] {
        [self.info_writer.as_ref().unwrap().as_raw_fd(), self.block_reader.as_ref().unwrap().as_raw_fd()]
    }

    /// Close the copies inherited by the child only after it has actually spawned.
    pub(super) fn spawned(&mut self) {
        self.info_writer.take();
        self.block_reader.take();
    }

    /// Bind a stable pidfd before allowing the sandbox command to execute.
    pub(super) fn bind(&mut self, wrapper: u32, timeout: Duration) -> io::Result<OwnedFd> {
        let info = self.info.take().ok_or_else(|| io::Error::other("missing sandbox info pipe"))?;
        let mut poll = libc::pollfd { fd: info.as_raw_fd(), events: libc::POLLIN | libc::POLLHUP, revents: 0 };
        let deadline = Instant::now() + timeout;
        let mut data = Vec::new();
        let mut reader = std::fs::File::from(info);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "sandbox info timed out"));
            }
            let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().min(i32::MAX as u128) as i32) };
            if result < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if result == 0 {
                continue;
            }
            let mut buf = [0u8; 1024];
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            data.extend_from_slice(&buf[..n]);
            if data.len() > 16384 {
                return Err(io::Error::other("sandbox info too large"));
            }
        }
        let info: serde_json::Value = serde_json::from_slice(&data).map_err(io::Error::other)?;
        let pid = info
            .get("child-pid")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n != 0)
            .ok_or_else(|| io::Error::other("sandbox init PID missing"))?;
        let namespace = info
            .get("pid-namespace")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| io::Error::other("sandbox PID namespace missing"))?;
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw) };
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name)).map(str::trim);
        let parent = field("PPid:").and_then(|v| v.parse::<u32>().ok());
        let ns_pid = field("NSpid:").and_then(|v| v.split_whitespace().last()).and_then(|v| v.parse::<u32>().ok());
        let inode = std::fs::metadata(format!("/proc/{pid}/ns/pid"))?.ino();
        if parent != Some(wrapper) || ns_pid != Some(1) || inode != namespace || readable(&pidfd, 0)? {
            return Err(io::Error::other("sandbox init identity could not be verified"));
        }
        Ok(pidfd)
    }

    pub(super) fn release(&mut self) -> io::Result<()> {
        let fd = self.block.as_ref().ok_or_else(|| io::Error::other("missing sandbox gate"))?.as_raw_fd();
        loop {
            let result = unsafe { libc::write(fd, [1u8].as_ptr().cast(), 1) };
            if result == 1 {
                self.block.take();
                return Ok(());
            }
            let error =
                if result < 0 { io::Error::last_os_error() } else { io::Error::other("sandbox gate short write") };
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

fn readable(fd: &OwnedFd, timeout_ms: i32) -> io::Result<bool> {
    let mut poll = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    loop {
        let n = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
        if n >= 0 {
            return Ok(n > 0 && poll.revents & libc::POLLIN != 0);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

pub(super) fn wait(pidfd: &OwnedFd) -> io::Result<()> {
    if readable(pidfd, -1)? { Ok(()) } else { Err(io::Error::other("sandbox init pidfd failed")) }
}

/// Probe the actual launcher, including the namespace identity and the pidfd handshake. The
/// handshake is repeated for every command, so later launcher changes still fail closed.
pub(super) fn supported() -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(probe)
}

fn probe() -> bool {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let Ok(mut gate) = Gate::new() else { return false };
    let fds = gate.inherited();
    let mut command = Command::new("bwrap");
    command
        .args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-pid", "--proc", "/proc", "--die-with-parent"])
        .args(["--info-fd", &fds[0].to_string(), "--block-fd", &fds[1].to_string(), "--", "/bin/echo", "ready"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    unsafe {
        command.pre_exec(move || inherit(fds));
    }
    let Ok(mut child) = command.spawn() else { return false };
    gate.spawned();
    let init = gate.bind(child.id(), Duration::from_secs(5));
    let mut output_poll = libc::pollfd {
        fd: child.stdout.as_ref().unwrap().as_raw_fd(),
        events: libc::POLLIN | libc::POLLHUP,
        revents: 0,
    };
    // Verify the launcher actually holds the command at the gate before using it. Any output,
    // EOF, or polling error is a failure; the actual launch repeats the identity handshake.
    let blocked = unsafe { libc::poll(&mut output_poll, 1, 50) } == 0;
    let ok = init.is_ok() && blocked && gate.release().is_ok();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline && ok => std::thread::sleep(Duration::from_millis(10)),
            _ => break None,
        }
    };
    if status.is_none() {
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
    }
    let mut output = Vec::new();
    let output_ok =
        child.stdout.take().is_some_and(|mut out| out.read_to_end(&mut output).is_ok()) && output == b"ready\n";
    ok && output_ok && status.is_some_and(|s| s.success()) && init.is_ok_and(|fd| readable(&fd, 5000).unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn gate_holds_command_until_verified_init_is_bound() {
        if super::super::backend::backend() != super::super::Backend::Bwrap {
            return;
        }
        let mut gate = Gate::new().unwrap();
        let fds = gate.inherited();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let mut command = Command::new("bwrap");
        command
            .args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-pid", "--proc", "/proc", "--die-with-parent"])
            .args(["--bind", dir.path().to_str().unwrap(), dir.path().to_str().unwrap()])
            .args([
                "--info-fd",
                &fds[0].to_string(),
                "--block-fd",
                &fds[1].to_string(),
                "--",
                "touch",
                marker.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        unsafe {
            command.pre_exec(move || inherit(fds));
        }
        let mut child = command.spawn().unwrap();
        gate.spawned();
        let init = gate.bind(child.id(), Duration::from_secs(5)).expect("verified namespace init");
        assert!(!marker.exists(), "the command must not run while the gate is held");
        gate.release().unwrap();
        assert!(child.wait().unwrap().success());
        wait(&init).unwrap();
        assert!(marker.exists());
    }

    #[test]
    fn failed_identity_check_keeps_command_blocked_until_killed() {
        if super::super::backend::backend() != super::super::Backend::Bwrap {
            return;
        }
        let mut gate = Gate::new().unwrap();
        let fds = gate.inherited();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        let mut command = Command::new("bwrap");
        command
            .args(["--ro-bind", "/", "/", "--dev", "/dev", "--unshare-pid", "--proc", "/proc", "--die-with-parent"])
            .args(["--bind", dir.path().to_str().unwrap(), dir.path().to_str().unwrap()])
            .args([
                "--info-fd",
                &fds[0].to_string(),
                "--block-fd",
                &fds[1].to_string(),
                "--",
                "touch",
                marker.to_str().unwrap(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.process_group(0);
        unsafe {
            command.pre_exec(move || inherit(fds));
        }
        let mut child = command.spawn().unwrap();
        gate.spawned();
        assert!(gate.bind(u32::MAX, Duration::from_secs(5)).is_err());
        assert!(!marker.exists());
        let group = child.id();
        assert_eq!(unsafe { libc::kill(-(group as i32), libc::SIGKILL) }, 0);
        child.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !super::super::bwrap_guard::group_has_live_processes(group).is_ok_and(|live| !live) {
            assert!(Instant::now() < deadline, "failed sandbox did not stop");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(gate);
        assert!(!marker.exists(), "failed identity verification must not run the command");
    }
}
