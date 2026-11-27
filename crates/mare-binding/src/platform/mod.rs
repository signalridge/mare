//! Platform backends: process management (spawn isolation + process-tree kill),
//! PTY and local IPC each sit behind a trait; the OS sandbox probe is
//! [`crate::ossandbox::SandboxBackend`]. Every `cfg(unix)` / `cfg(windows)` split of the
//! primitives lives here (plus the sandbox code in [`crate::ossandbox`] and
//! [`crate::winsandbox`]); the Lua APIs are identical on every platform.
//!
//! - Unix: process groups + signals, Unix domain sockets, Unix PTYs ([`Terminal`]: openpty +
//!   `std::process::Command` with a pre-exec hook).
//! - Windows: Job Objects (the whole tree dies with the job; children start suspended until they
//!   are inside it`), `CREATE_NO_WINDOW`, named
//!   pipes behind the same path-addressed IPC API, ConPTY processes created by mare itself
//!   ([`Terminal`]) so they too start suspended inside a job.

use std::future::Future;
use std::io;

use tokio::io::{AsyncRead, AsyncWrite};

pub(crate) mod cmdline;
mod ipc;
mod notify;
#[cfg(windows)]
pub mod pipe;
mod process;
#[cfg(unix)]
mod session_tree;
#[cfg(unix)]
#[path = "term_unix.rs"]
mod term;
#[cfg(windows)]
#[path = "term_win.rs"]
mod term;
#[cfg(windows)]
#[path = "job_win.rs"]
mod win;

pub use notify::notify_native;
pub use process::{adopt_std, exit, isolate_std, signal_tree, spawn, tree_alive};
#[cfg(target_os = "linux")]
pub(crate) use session_tree::alive as session_alive;

/// Error text for a capability a platform backend doesn't provide yet.
pub fn not_implemented(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, format!("{what} is not implemented on Windows yet"))
}

/// Portable signal names (mapped per platform).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
    Int,
    Hup,
    Quit,
}

impl Signal {
    pub fn parse(s: &str) -> Self {
        match s.trim_start_matches("SIG").to_ascii_uppercase().as_str() {
            "KILL" => Self::Kill,
            "INT" => Self::Int,
            "HUP" => Self::Hup,
            "QUIT" => Self::Quit,
            _ => Self::Term,
        }
    }
}

/// Spawning and stopping whole process trees.
pub trait ProcessBackend {
    /// Make the child the root of its own group (Unix process group / Windows job), so the whole
    /// tree it starts can be signalled at once.
    fn isolate(cmd: &mut tokio::process::Command);
    /// Called right after spawning a command prepared with [`ProcessBackend::isolate`] (Windows:
    /// put it into its job, then let it run).
    fn adopt(child: &tokio::process::Child) -> io::Result<()>;
    /// Signal the tree rooted at `pid` (bash children included).
    fn signal_tree(pid: u32, sig: Signal) -> io::Result<()>;
    /// Whether any process of the tree started as `pid` is still alive (process group / job).
    fn tree_alive(pid: u32) -> bool;
    /// Run `f` when the process exits normally (last-chance cleanup of live trees).
    fn at_exit(f: extern "C" fn());
    /// The signal that ended a process, if any.
    fn exit_signal(status: &std::process::ExitStatus) -> Option<i32>;
}

/// A connected local IPC stream (Unix domain socket; a named pipe on Windows).
pub trait IpcStream: AsyncRead + AsyncWrite + Send + Unpin + 'static {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin + 'static> IpcStream for T {}

/// Local IPC endpoints addressed by a filesystem path.
pub trait IpcBackend {
    type Stream: IpcStream;
    type Listener: 'static;
    fn connect(path: &str) -> impl Future<Output = io::Result<Self::Stream>>;
    /// Bind `path` with file mode `mode`; a stale endpoint is replaced, a live one refused.
    fn bind(path: &std::path::Path, mode: u32) -> io::Result<Self::Listener>;
    fn accept(l: &Self::Listener) -> impl Future<Output = io::Result<Self::Stream>>;
}

/// The platform this build runs on.
pub struct Host;

/// Process backend of this build.
pub type Process = Host;
/// IPC backend of this build.
pub type Ipc = Host;

/// The named pipe a mare from before random pipe names listens on for a path-addressed endpoint
/// (a hash of the path). Current listeners draw a random name and write it to the endpoint file
/// (`pipe::bind`); clients use this only to reach such an older session (`pipe::resolve`).
pub fn pipe_name(path: &std::path::Path) -> String {
    use sha2::Digest;
    let norm = path.to_string_lossy().replace('/', "\\").to_ascii_lowercase();
    let h = sha2::Sha256::digest(norm.as_bytes());
    let hex: String = h.iter().take(12).map(|b| format!("{b:02x}")).collect();
    format!(r"\\.\pipe\mare-{hex}")
}

/// A program running on a pseudo-terminal that mare created and started itself, so the child is
/// contained from its first instruction:
/// - Unix: openpty (both ends close-on-exec) and `std::process::Command` with a pre-exec hook —
///   new session with the slave as controlling terminal, signal dispositions and mask reset, on
///   Linux `PR_SET_PDEATHSIG(SIGKILL)` plus the parent-alive re-check;
/// - Windows: `CreatePseudoConsole` + `CreateProcessW` with `CREATE_SUSPENDED`, put in a
///   `KILL_ON_JOB_CLOSE` Job Object, then resumed through the main-thread handle. No grandchild can
///   start outside the job.
///
/// The child leads its own process group (Unix session / Windows job), so [`signal_tree`] and
/// [`tree_alive`] act on the whole tree.
pub type TerminalExitStatus = (Option<u32>, Option<i32>);

pub struct Terminal {
    imp: term::Imp,
}

/// What a [`Terminal`] runs: the full environment is given (nothing inherited).
pub struct TerminalCommand<'a> {
    pub prog: &'a str,
    pub args: &'a [String],
    pub cwd: &'a str,
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub cols: u16,
    pub rows: u16,
}

/// A step run in a [`Terminal`]'s child right before exec ([`Terminal::spawn_restricted`]).
#[cfg(unix)]
pub type PreExec = Box<dyn FnMut() -> io::Result<()> + Send + Sync>;

/// A [`Terminal`]'s output (read on a blocking thread).
pub type TerminalReader = Box<dyn io::Read + Send>;
/// A [`Terminal`]'s input (written on a blocking thread).
pub type TerminalWriter = Box<dyn io::Write + Send>;

impl Terminal {
    /// Start the program; returns the terminal plus its output reader and input writer.
    pub fn spawn(c: TerminalCommand<'_>) -> io::Result<(Self, TerminalReader, TerminalWriter)> {
        #[cfg(unix)]
        let (imp, r, w) = term::Imp::spawn(c, None)?;
        #[cfg(windows)]
        let (imp, r, w) = term::Imp::spawn(c)?;
        Ok((Self { imp }, r, w))
    }
    /// [`Terminal::spawn`], with `restrict` run in the child as the last step before exec (after
    /// the session, signal and descriptor setup): a sandbox's Landlock ruleset / seccomp filter,
    /// exactly where `mare.proc` installs them. It must be async-signal-safe.
    #[cfg(unix)]
    pub fn spawn_restricted(
        c: TerminalCommand<'_>,
        restrict: PreExec,
    ) -> io::Result<(Self, TerminalReader, TerminalWriter)> {
        let (imp, r, w) = term::Imp::spawn(c, Some(restrict))?;
        Ok((Self { imp }, r, w))
    }
    pub fn pid(&self) -> u32 {
        self.imp.pid()
    }
    /// Exit code once the program ended (a signal counts as 1, as portable-pty reported it).
    pub fn exit_status(&mut self) -> io::Result<Option<TerminalExitStatus>> {
        self.imp.exit_status()
    }
    #[cfg(unix)]
    pub(crate) fn exit_watcher(&self) -> term::ExitRx {
        self.imp.exit_watcher()
    }
    /// Drop this [`Terminal`]'s master PTY descriptor while keeping the exit waiter available for
    /// [`Terminal::try_wait`], [`Terminal::exit_status`] and [`Terminal::wait`].
    ///
    /// Note: [`Terminal::spawn`] also returns `TerminalReader` and `TerminalWriter`, each holding
    /// a duplicated master descriptor; on Unix the kernel sends `SIGHUP` to the child session's
    /// foreground process group only after those descriptors are dropped too.
    #[cfg(unix)]
    pub fn hang_up(&mut self) {
        self.imp.hang_up();
    }
    /// Wait for the child to exit and be reaped by the terminal's waiter thread.
    #[cfg(unix)]
    pub fn wait(&mut self) -> io::Result<TerminalExitStatus> {
        self.imp.wait()
    }
    /// Wait up to `timeout` for the child to exit; returns `Ok(None)` if it is still running.
    pub fn wait_timeout(&mut self, timeout: std::time::Duration) -> io::Result<Option<TerminalExitStatus>> {
        self.imp.wait_timeout(timeout)
    }
    pub fn try_wait(&mut self) -> io::Result<Option<u32>> {
        self.imp.try_wait()
    }
    /// End the root process (the caller ends the tree with [`signal_tree`]); fine if it already exited.
    pub fn kill(&mut self) -> io::Result<()> {
        self.imp.kill()
    }
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.imp.resize(cols, rows)
    }
    /// Verified foreground group; unknown input readiness stays `None`.
    pub fn foreground(&self) -> io::Result<Option<(u32, Option<bool>)>> {
        #[cfg(unix)]
        {
            self.imp.foreground()
        }
        #[cfg(windows)]
        {
            Ok(None)
        }
    }
    pub fn signal_foreground(&self, signal: &str, protected_pgid: Option<u32>) -> io::Result<u32> {
        #[cfg(unix)]
        {
            self.imp.signal_foreground(signal, protected_pgid)
        }
        #[cfg(windows)]
        {
            let _ = (signal, protected_pgid);
            Err(not_implemented("verified foreground signalling"))
        }
    }
}

/// The master side, for polling it (the TUI e2e tests wait on it with a timeout so they can
/// close every master fd and produce a real hangup).
#[cfg(unix)]
impl std::os::fd::AsRawFd for Terminal {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.imp.master.as_ref().map_or(-1, std::os::fd::AsRawFd::as_raw_fd)
    }
}

#[cfg(all(test, windows))]
mod windows_tests;

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
mod unix_tests;
