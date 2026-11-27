use std::time::Duration;

use super::ensure_test_offline;

/// Own a child started in its own process group (Unix) or Job Object (Windows).
/// Descendants remain owned even when the direct child has already exited or been reaped.
pub struct ChildGroup(pub std::process::Child);

impl ChildGroup {
    pub fn spawn(cmd: &mut std::process::Command) -> std::io::Result<Self> {
        mare_binding::platform::isolate_std(cmd);
        let child = cmd.spawn()?;
        if let Err(error) = mare_binding::platform::adopt_std(&child) {
            mare_binding::platform::signal_tree(Some(child.id()), mare_binding::platform::Signal::Kill);
            return Err(error);
        }
        Ok(Self(child))
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }

    /// Wait up to `timeout` for the direct child to exit, killing and reaping the group on timeout.
    pub fn wait_timeout(mut self, timeout: Duration) -> std::process::ExitStatus {
        let deadline = std::time::Instant::now() + timeout;
        let status = loop {
            match self.0.try_wait().expect("try_wait child process") {
                Some(status) => break Some(status),
                None if std::time::Instant::now() >= deadline => break None,
                None => {
                    // sleep-ok: bounded poll waiting for child exit up to deadline
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        };
        drop(self);
        status.unwrap_or_else(|| panic!("child process timed out after {}s", timeout.as_secs()))
    }
}

pub const DEFAULT_CMD_TIMEOUT: Duration = Duration::from_secs(60);

/// Run `cmd` inside an isolated [`ChildGroup`], capturing stdout/stderr with [`DEFAULT_CMD_TIMEOUT`].
/// Any timeout or panic kills and reaps the entire process tree.
pub fn run_output(cmd: &mut std::process::Command) -> std::process::Output {
    run_output_timeout(cmd, DEFAULT_CMD_TIMEOUT)
}

/// Wait on an already-spawned [`ChildGroup`] whose stdout/stderr were piped (or taken/null), up to `timeout`.
pub fn wait_output_timeout(mut group: ChildGroup, timeout: Duration) -> std::process::Output {
    use std::io::Read as _;
    let out = group.0.stdout.take();
    let err = group.0.stderr.take();
    let out_thread = out.map(|mut out| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            buf
        })
    });
    let err_thread = err.map(|mut err| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf);
            buf
        })
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match group.0.try_wait().expect("try_wait child process") {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => break None,
            None => {
                // sleep-ok: bounded poll waiting for child exit up to deadline
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    drop(group);
    let stdout = out_thread.map_or_else(Vec::new, |t| t.join().unwrap_or_default());
    let stderr = err_thread.map_or_else(Vec::new, |t| t.join().unwrap_or_default());
    match status {
        Some(status) => std::process::Output { status, stdout, stderr },
        None => panic!(
            "command timed out after {}s; stderr:\n{}\nstdout:\n{}",
            timeout.as_secs(),
            String::from_utf8_lossy(&stderr),
            String::from_utf8_lossy(&stdout),
        ),
    }
}

/// Run `cmd` inside an isolated [`ChildGroup`] up to `timeout`, killing and reaping on timeout.
pub fn run_output_timeout(cmd: &mut std::process::Command, timeout: Duration) -> std::process::Output {
    use std::process::Stdio;
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let group = ChildGroup::spawn(cmd).expect("spawn child process");
    wait_output_timeout(group, timeout)
}

/// Async wrapper for [`run_output_timeout`] on blocking pool.
pub async fn run_output_async(mut cmd: std::process::Command, timeout: Duration) -> std::process::Output {
    tokio::task::spawn_blocking(move || run_output_timeout(&mut cmd, timeout)).await.unwrap()
}

/// Extension methods on [`std::process::Command`] to run blocking `.output` inside a bounded [`ChildGroup`].
pub trait ChildCommandExt {
    /// Run the command inside an isolated [`ChildGroup`] up to [`DEFAULT_CMD_TIMEOUT`].
    fn run_output(&mut self) -> std::process::Output;
    /// Run the command inside an isolated [`ChildGroup`] up to `timeout`.
    fn run_output_timeout(&mut self, timeout: Duration) -> std::process::Output;
}

impl ChildCommandExt for std::process::Command {
    fn run_output(&mut self) -> std::process::Output {
        run_output(self)
    }

    fn run_output_timeout(&mut self, timeout: Duration) -> std::process::Output {
        run_output_timeout(self, timeout)
    }
}

const CHILD_GROUP_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Poll `is_alive` until it reports the process tree is empty or `timeout` elapses.
pub fn confirm_tree_exited(mut is_alive: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_alive() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        // sleep-ok: polls another process tree for exit until the deadline; there is no event for it
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl Drop for ChildGroup {
    fn drop(&mut self) {
        use mare_binding::platform::{Process, ProcessBackend, Signal};

        let pid = self.0.id();
        let kill_tree = || {
            if let Err(error) = <Process as ProcessBackend>::signal_tree(pid, Signal::Kill) {
                #[cfg(unix)]
                let ignorable = error.raw_os_error() == Some(libc::ESRCH);
                #[cfg(not(unix))]
                let ignorable = false;
                if !ignorable {
                    eprintln!("test child group termination: {error}");
                }
            }
        };
        kill_tree();
        if let Err(error) = self.0.wait() {
            eprintln!("test child reap: {error}");
        }
        // Re-signal while polling: a shell killed mid-fork can leave a newly spawned descendant
        // in the same session/group after the first SIGKILL.
        let reap_timeout = mare_cli::testkit::scaled_test_timeout(CHILD_GROUP_REAP_TIMEOUT);
        if !confirm_tree_exited(
            || {
                if mare_binding::platform::tree_alive(Some(pid)) {
                    kill_tree();
                    true
                } else {
                    false
                }
            },
            reap_timeout,
        ) {
            let msg = format!("test child group (pid {pid}) still alive after bounded kill wait");
            if std::thread::panicking() {
                eprintln!("{msg}");
            } else {
                panic!("{msg}");
            }
        }
    }
}

/// Whether a process with this pid runs (Unix: a zombie or exiting process counts as gone).
pub fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return false;
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                && let Some((_, rest)) = stat.rsplit_once(')')
                && let Some(state) = rest.trim_start().chars().next()
            {
                return !matches!(state, 'Z' | 'X' | 'x');
            }
            rc == 0
        }
        #[cfg(not(target_os = "linux"))]
        {
            if let Ok(out) = std::process::Command::new("ps").args(["-o", "state=", "-p", &pid.to_string()]).output() {
                if !out.status.success() {
                    return unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
                        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
                }
                return !String::from_utf8_lossy(&out.stdout).trim_start().starts_with('Z');
            }
            rc == 0
        }
    }
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
    }
}

/// The `mare` binary under test. Private on purpose so every child
/// `mare` starts through one of the named entry points below, which carry the offline guard, so
/// a direct `Command::new(<binary>)` outside this module doesn't compile.
pub fn mare_bin() -> &'static str {
    ensure_test_offline();
    env!("CARGO_BIN_EXE_mare")
}

/// For a PTY child whose environment the test builds by hand (nothing inherited): sets the
/// offline guard in `env` (unless `MARE_TEST_ALLOW_LIVE=1`) and returns the program to start.
pub fn mare_pty_program(env: &mut Vec<(std::ffi::OsString, std::ffi::OsString)>) -> &'static str {
    if !env.iter().any(|(name, _)| name == "BROWSER") {
        env.push(("BROWSER".into(), "true".into()));
    }
    if std::env::var("MARE_TEST_ALLOW_LIVE").as_deref() != Ok("1") {
        env.retain(|(name, _)| name != "MARE_TEST_OFFLINE");
        env.push(("MARE_TEST_OFFLINE".into(), "1".into()));
    }
    mare_bin()
}

/// The `mare` binary as the sandbox helper (`MARE_SANDBOX_HELPER`): it re-executes itself only
/// as the sandbox wrapper, never as an agent, so it carries no network guard of its own.
pub fn sandbox_helper_bin() -> &'static str {
    mare_bin()
}

/// The scripted MCP servers (`tests/fake_mcp`): `scripted <config>`, `simple`, `tiered …`, `http …`.
pub fn fake_mcp_bin() -> &'static str {
    env!("CARGO_BIN_EXE_mare-test-mcp")
}

/// Owned `mare-test-mcp http` mock server in an isolated [`ChildGroup`]; kills and reaps the
/// entire process tree in [`Drop`] (even on panic).
pub struct FakeHttpMcp {
    child: ChildGroup,
    port: u16,
    headers_file: std::path::PathBuf,
}

impl FakeHttpMcp {
    /// Start `mare-test-mcp http` using unique state files under `dir` and wait until its port is ready.
    pub fn spawn(dir: &std::path::Path) -> Self {
        static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let port_file = dir.join(format!("fake-mcp-http-{id}.port"));
        let headers_file = dir.join(format!("fake-mcp-http-{id}.headers"));
        let _ = std::fs::remove_file(&port_file);
        let _ = std::fs::remove_file(&headers_file);

        let mut cmd = std::process::Command::new(fake_mcp_bin());
        cmd.arg("http").arg(&headers_file).arg(&port_file);
        let mut child = ChildGroup::spawn(&mut cmd).expect("spawn mare-test-mcp http");

        let t0 = std::time::Instant::now();
        let port = loop {
            if let Ok(raw) = std::fs::read_to_string(&port_file)
                && !raw.trim().is_empty()
            {
                break raw.trim().parse::<u16>().expect("fake-mcp http port must be u16");
            }
            if let Ok(Some(status)) = child.0.try_wait() {
                panic!("mare-test-mcp http exited before writing port: {status}");
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "mock MCP server didn't start");
            // sleep-ok: polls mock HTTP MCP server port file until bound or deadline
            std::thread::sleep(Duration::from_millis(50));
        };

        Self { child, port, headers_file }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/mcp", self.port)
    }

    pub fn headers_file(&self) -> &std::path::Path {
        &self.headers_file
    }

    /// Terminate and reap the mock server immediately (also performed automatically on drop).
    pub fn shutdown(self) {
        drop(self.child);
    }
}
