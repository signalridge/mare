//! Running git: never waits for terminal input or a credential dialog
//! ([`NON_INTERACTIVE`]; lazy.nvim `manage/process.lua` `Process:env`: `GIT_TERMINAL_PROMPT=0`, `ssh -oBatchMode=yes`, no
//! inherited `GIT_DIR` / `GIT_WORK_TREE` / `GIT_INDEX_FILE`), carries the transport allowlist,
//! and is killed with its process group on timeout (lazy.nvim `git.timeout`) or cancellation.

use std::cell::RefCell;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

/// Typed cancellation/timeout error for git and package lock operations.
#[derive(Debug)]
pub(crate) enum AbortError {
    Cancelled { what: Option<String> },
    TimedOut { what: String, secs: f64 },
}

impl std::fmt::Display for AbortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled { what: Some(what) } => write!(f, "git {what} cancelled"),
            Self::Cancelled { what: None } => f.write_str("cancelled"),
            Self::TimedOut { what, secs } => {
                write!(f, "git {what} timed out after {secs}s (mare.o.plugins.timeout) and was stopped")
            }
        }
    }
}

impl std::error::Error for AbortError {}

/// Variables that point git at a particular repository (`git rev-parse --local-env-vars`, minus
/// the user's `GIT_CONFIG*`): a git hook exports them, and a mare started from one would
/// otherwise run every package command against that repository (lazy.nvim clears `GIT_DIR`,
/// `GIT_WORK_TREE`, `GIT_INDEX_FILE`).
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
];

/// The environment that keeps git from asking anyone anything.
/// - `GIT_TERMINAL_PROMPT=0`: no prompt on the terminal.
/// - `GIT_ASKPASS=` (empty): git's prompt asks `GIT_ASKPASS`, then `core.askPass`, then
///   `SSH_ASKPASS` before the terminal (git `prompt.c` `git_prompt`); an empty `GIT_ASKPASS` is
///   "set" and so skips the other two. Git Bash exports `SSH_ASKPASS=git-askpass.exe`
///   (`/etc/profile.d/env.sh`) and Linux desktops a GUI askpass: without this git opens a
///   dialog and waits for it until the timeout.
/// - `GCM_INTERACTIVE=never`: Git Credential Manager (the credential helper Git for Windows
///   installs) answers from what it has stored, or fails, instead of opening a sign-in window.
///   Other credential helpers still run, so stored credentials keep working.
const NON_INTERACTIVE: &[(&str, &str)] =
    &[("GIT_TERMINAL_PROMPT", "0"), ("GIT_ASKPASS", ""), ("GCM_INTERACTIVE", "never")];

/// lazy.nvim `git.timeout` (seconds).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);

thread_local! {
    static CANCEL: RefCell<Option<CancellationToken>> = const { RefCell::new(None) };
}

/// Run `f` (any `mare_plugin_manager` operation, on this thread) so that cancelling `token` kills the
/// running git and makes the operation stop with an error.
pub fn with_cancel<R>(token: &CancellationToken, f: impl FnOnce() -> R) -> R {
    let prev = CANCEL.with(|c| c.replace(Some(token.clone())));
    struct Restore(Option<CancellationToken>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let prev = self.0.take();
            CANCEL.with(|c| *c.borrow_mut() = prev);
        }
    }
    let _restore = Restore(prev);
    f()
}

/// The token of the operation running on this thread, so worker threads it starts can carry it.
pub(crate) fn current_token() -> Option<CancellationToken> {
    CANCEL.with(|c| c.borrow().clone())
}

/// The operation running on this thread was cancelled.
pub fn cancelled() -> bool {
    CANCEL.with(|c| c.borrow().as_ref().is_some_and(|t| t.is_cancelled()))
}

/// `GIT_SSH_COMMAND` with batch mode: the user's own command gets `-oBatchMode=yes` appended
/// (`-batch` when it runs plink, which git drives as its `plink` variant and which has no `-o`).
pub fn ssh_command(user: Option<&str>) -> String {
    match user.map(str::trim).filter(|s| !s.is_empty()) {
        Some(u) if u.contains("BatchMode") => u.to_string(),
        Some(u) if is_plink(u) => {
            if u.split_whitespace().any(|w| w == "-batch") {
                u.to_string()
            } else {
                format!("{u} -batch")
            }
        }
        Some(u) => format!("{u} -oBatchMode=yes"),
        None => "ssh -oBatchMode=yes".into(),
    }
}

/// Whether an ssh command runs plink (its first word, quotes allowed; see [`is_plink_prog`]).
fn is_plink(cmd: &str) -> bool {
    let cmd = cmd.trim();
    let prog = match cmd.strip_prefix(['"', '\'']) {
        Some(rest) => rest.split(['"', '\'']).next().unwrap_or(rest),
        None => cmd.split_whitespace().next().unwrap_or(cmd),
    };
    is_plink_prog(prog)
}

/// Whether a program path is plink (git's variant detection: the base name, `.exe` stripped,
/// case-insensitive). TortoisePlink is git's own `tortoiseplink` variant (git already passes it
/// `-batch`), so it isn't matched.
fn is_plink_prog(prog: &str) -> bool {
    let base = prog.trim().rsplit(['/', '\\']).next().unwrap_or(prog).to_ascii_lowercase();
    base.strip_suffix(".exe").unwrap_or(&base) == "plink"
}

/// Commands that may talk to a remote (and so may start ssh): only these look up the user's
/// `core.sshCommand`, the rest keep their single git process.
const REMOTE_COMMANDS: &[&str] = &["clone", "fetch", "ls-remote", "pull", "push", "remote", "submodule"];

/// A variable as the git child sees it: the explicit `env` (even empty) wins over mare's own.
fn git_var(env: &[(&str, &str)], key: &str) -> Option<String> {
    env.iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.to_string())
        .or_else(|| std::env::var(key).ok())
        .filter(|s| !s.trim().is_empty())
}

#[cfg(windows)]
const NULL_DEVICE: &str = "NUL";
#[cfg(not(windows))]
const NULL_DEVICE: &str = "/dev/null";

fn git_safety_flags() -> [&'static str; 6] {
    ["-c", "safe.bareRepository=explicit", "-c", const_hooks_path_flag(), "-c", "core.fsmonitor=false"]
}

const fn const_hooks_path_flag() -> &'static str {
    if cfg!(windows) { "core.hooksPath=NUL" } else { "core.hooksPath=/dev/null" }
}

/// The `core.sshCommand` configured in global or system git config (never local repo config).
fn configured_ssh_command(env: &[(&str, &str)]) -> Option<String> {
    let empty_dir = tempfile::tempdir().ok();
    let cwd = empty_dir.as_ref().map(|d| d.path().to_path_buf()).unwrap_or_else(std::env::temp_dir);
    for scope in ["--global", "--system"] {
        let mut c = Command::new("git");
        c.current_dir(&cwd)
            .args(["-c", "safe.bareRepository=explicit", "config", scope, "--get", "core.sshCommand"])
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        for v in REPO_ENV {
            c.env_remove(v);
        }
        if let Some(val) = c
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
        {
            return Some(val);
        }
    }
    let _ = NULL_DEVICE;
    None
}

/// The `GIT_SSH_COMMAND` a git command runs with, following git's own
/// precedence `GIT_SSH_COMMAND` > `core.sshCommand` > `GIT_SSH`. A plain `ssh -oBatchMode=yes`
/// would silently replace the user's configured command (lazy.nvim does that); instead the
/// user's own command gets batch mode appended: their `GIT_SSH_COMMAND` if set, else the
/// `core.sshCommand` git would use in `dir`. `None` — leave `GIT_SSH_COMMAND` unset — when only
/// `GIT_SSH` is set (plink on Windows, say): it takes no arguments, and setting
/// `GIT_SSH_COMMAND` would override it (`mare doctor` hints at `-batch`, [`ssh_doctor`]).
fn ssh_env(args: &[&str], env: &[(&str, &str)]) -> Option<String> {
    if let Some(user) = git_var(env, "GIT_SSH_COMMAND") {
        return Some(ssh_command(Some(&user)));
    }
    let remote = args.iter().find(|a| !a.starts_with('-')).is_some_and(|a| REMOTE_COMMANDS.contains(a));
    if remote && let Some(configured) = configured_ssh_command(env) {
        return Some(ssh_command(Some(&configured)));
    }
    if git_var(env, "GIT_SSH").is_some() {
        return None;
    }
    Some(ssh_command(None))
}

/// `mare doctor`: only `GIT_SSH` names the ssh program and it is plink,
/// which mare leaves alone — without `-batch` a host-key or password prompt waits for input
/// nobody gives until the git timeout.
pub fn ssh_doctor() -> Option<(String, bool, String)> {
    ssh_doctor_with(&[])
}

fn ssh_doctor_with(env: &[(&str, &str)]) -> Option<(String, bool, String)> {
    if git_var(env, "GIT_SSH_COMMAND").is_some() {
        return None;
    }
    let prog = git_var(env, "GIT_SSH")?;
    if !is_plink_prog(&prog) || configured_ssh_command(env).is_some() {
        return None;
    }
    let quoted = if prog.contains(' ') { format!("\"{prog}\"") } else { prog.clone() };
    Some((
        "git ssh".into(),
        false,
        format!(
            "GIT_SSH={prog} (plink) runs without -batch, so a host-key or password prompt stalls package \
             installs until the git timeout; set GIT_SSH_COMMAND='{quoted} -batch' (or core.sshCommand) instead"
        ),
    ))
}

/// Words in git's / ssh's stderr that mean the server wanted credentials we won't prompt for.
pub fn is_auth_failure(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    [
        "terminal prompts disabled",
        "could not read username",
        "could not read password",
        "authentication failed",
        "permission denied (publickey",
        "host key verification failed",
        "requested url returned error: 401",
        "requested url returned error: 403",
    ]
    .iter()
    .any(|k| m.contains(k))
}

/// Windows: git and the processes it starts (ssh, git-remote-https) share a Job Object, so a
/// timeout or cancellation ends the whole tree, like the process group on Unix.
/// git starts suspended and runs only once it is inside the job, so nothing
/// it starts can escape (same scheme as mare-binding
/// `platform/`, whose job support is tied to its tokio spawn path in a crate above this one).
/// A job that can't be set up fails the command — git never runs uncontained. No
/// KILL_ON_JOB_CLOSE: a normal exit leaves e.g. a detached `git gc --auto` alone, as on Unix.
#[cfg(windows)]
mod job {
    use std::io;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject};
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    pub struct Job(HANDLE);

    /// Creation flags for git: suspended until adopted, and no console window of its own.
    pub fn prepare(c: &mut std::process::Command) {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
    }

    /// Resume the threads of a process started with CREATE_SUSPENDED (std doesn't expose the
    /// main thread handle CreateProcess returned: a thread snapshot finds it).
    fn resume(pid: u32) -> io::Result<()> {
        // SAFETY: snapshot / thread handles are opened and closed here; the entry is sized
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap as isize == -1 {
                return Err(io::Error::last_os_error());
            }
            let mut e: THREADENTRY32 = std::mem::zeroed();
            e.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut ok = Thread32First(snap, &mut e) != 0;
            let mut resumed = 0;
            while ok {
                if e.th32OwnerProcessID == pid {
                    let t = OpenThread(THREAD_SUSPEND_RESUME, 0, e.th32ThreadID);
                    if !t.is_null() {
                        if ResumeThread(t) != u32::MAX {
                            resumed += 1;
                        }
                        CloseHandle(t);
                    }
                }
                ok = Thread32Next(snap, &mut e) != 0;
            }
            CloseHandle(snap);
            if resumed == 0 { Err(io::Error::other("could not resume git")) } else { Ok(()) }
        }
    }

    impl Job {
        /// Put a git spawned with [`prepare`] (still suspended) into a new job, then let it run.
        /// On an error the caller must end the child: it is still suspended, outside any job.
        pub fn adopt(child: &std::process::Child) -> io::Result<Self> {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: plain Win32 calls on a handle we own and the child's live process handle
            let job = unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let job = Self(job);
                if AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) == 0 {
                    return Err(io::Error::last_os_error());
                }
                job
            };
            resume(child.id())?;
            Ok(job)
        }

        pub fn terminate(&self) {
            // SAFETY: self.0 is a job handle we own
            unsafe {
                TerminateJobObject(self.0, 1);
            }
        }

        /// Processes still running in the job.
        #[cfg(test)]
        pub fn active(&self) -> u32 {
            use windows_sys::Win32::System::JobObjects::{
                JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JobObjectBasicAccountingInformation, QueryInformationJobObject,
            };
            // SAFETY: the out buffer is sized for the requested information class
            unsafe {
                let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = std::mem::zeroed();
                QueryInformationJobObject(
                    self.0,
                    JobObjectBasicAccountingInformation,
                    &mut info as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                );
                info.ActiveProcesses
            }
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: closed once, here
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

/// Spawn a prepared git command contained as a tree: its own process group on Unix, a Job
/// Object it is inside from its first instruction on Windows.
fn spawn_contained(c: &mut Command) -> Result<(std::process::Child, Contained)> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // its own group: killable as a tree, and never the terminal's foreground group
        c.process_group(0);
    }
    #[cfg(windows)]
    job::prepare(c);
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut child = c.spawn().context("running git")?;
    #[cfg(windows)]
    let contained = match job::Job::adopt(&child) {
        Ok(j) => j,
        Err(e) => {
            // still suspended: end it before it runs a single instruction
            let _ = child.kill();
            let _ = child.wait();
            bail!("running git: could not put it in a Job Object: {e}");
        }
    };
    #[cfg(not(windows))]
    let contained = ();
    Ok((child, contained))
}

#[cfg(windows)]
type Contained = job::Job;
#[cfg(not(windows))]
type Contained = ();

fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // the child leads its own process group (process_group(0)): ssh / remote helpers die too
        if let Ok(pid) = i32::try_from(child.id()) {
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
    let _ = child.kill();
}

/// Bounded wait for git process-group grandchildren to exit after SIGKILL.
#[cfg(unix)]
const GROUP_REAP_TIMEOUT: Duration = Duration::from_secs(1);
/// Poll interval while waiting for git process-group grandchildren to exit.
#[cfg(unix)]
const GROUP_REAP_POLL: Duration = Duration::from_millis(10);
/// Poll interval while waiting for a running git child process.
const CHILD_WAIT_POLL: Duration = Duration::from_millis(20);
/// Drain grace for stdout/stderr reader threads after git exits.
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

fn poll_pause(interval: Duration) {
    std::thread::sleep(interval);
}

/// Wait briefly for any grandchildren in `git`'s process group (e.g. `git-remote-https`,
/// `git-fetch-pack`) to finish exiting after SIGKILL so nothing still touches the checkout dir.
fn wait_group_gone(pid: u32) {
    #[cfg(unix)]
    {
        if let Ok(pgid) = i32::try_from(pid) {
            let deadline = Instant::now() + GROUP_REAP_TIMEOUT;
            while Instant::now() < deadline {
                // SAFETY: signal 0 only checks whether any process remains in the process group.
                let rc = unsafe { libc::kill(-pgid, 0) };
                if rc == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                    break;
                }
                poll_pause(GROUP_REAP_POLL);
            }
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

fn reader(mut r: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = vec![];
        let _ = r.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

/// Run git with the allowlist and the non-interactive environment; stdout (trimmed) on success.
pub fn run(dir: Option<&Path>, args: &[&str], timeout: Duration, allow_insecure: bool) -> Result<String> {
    run_with(dir, args, timeout, allow_insecure, &[])
}

/// [`run`] with extra environment variables for git (tests: a private git config).
fn run_with(
    dir: Option<&Path>,
    args: &[&str],
    timeout: Duration,
    allow_insecure: bool,
    env: &[(&str, &str)],
) -> Result<String> {
    if cancelled() {
        return Err(AbortError::Cancelled { what: None }.into());
    }
    let mut c = Command::new("git");
    let empty_dir = if dir.is_none() { tempfile::tempdir().ok() } else { None };
    if let Some(d) = dir {
        c.current_dir(d);
    } else if let Some(empty) = &empty_dir {
        c.current_dir(empty.path());
    }
    c.args(crate::source::protocol_flags(allow_insecure)).args(git_safety_flags());
    #[cfg(windows)]
    c.args(["-c", "core.longpaths=true"]);
    c.args(args)
        .envs(NON_INTERACTIVE.iter().copied())
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match ssh_env(args, env) {
        Some(ssh) => c.env("GIT_SSH_COMMAND", ssh),
        // only GIT_SSH: an empty GIT_SSH_COMMAND would still shadow it in git
        None => c.env_remove("GIT_SSH_COMMAND"),
    };
    for v in REPO_ENV {
        c.env_remove(v);
    }
    let (mut child, _job) = spawn_contained(&mut c)?;
    let out = reader(child.stdout.take().expect("piped"));
    let err = reader(child.stderr.take().expect("piped"));
    let start = Instant::now();
    let what = args.iter().find(|a| !a.starts_with('-')).copied().unwrap_or("git");
    let stopped = loop {
        if let Some(st) = child.try_wait().context("waiting for git")? {
            break Ok(st);
        }
        if cancelled() {
            break Err(AbortError::Cancelled { what: Some(what.to_string()) });
        }
        if start.elapsed() >= timeout {
            break Err(AbortError::TimedOut { what: what.to_string(), secs: timeout.as_secs_f64().round().max(1.0) });
        }
        poll_pause(CHILD_WAIT_POLL);
    };
    let status = match stopped {
        Ok(st) => st,
        Err(why) => {
            let pid = child.id();
            #[cfg(windows)]
            _job.terminate();
            kill_tree(&mut child);
            let _ = child.wait();
            wait_group_gone(pid);
            return Err(why.into());
        }
    };
    // a grandchild that outlived git may still hold the pipes: don't wait on it forever
    let stdout = out.recv_timeout(PIPE_DRAIN_GRACE).unwrap_or_default();
    let stderr = err.recv_timeout(PIPE_DRAIN_GRACE).unwrap_or_default();
    if !status.success() {
        let cmd = mare_shared::redact::redact_str(&args.join(" "));
        let err = mare_shared::redact::redact_str(String::from_utf8_lossy(&stderr).trim());
        bail!("git {cmd} failed: {err}");
    }
    Ok(String::from_utf8_lossy(&stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_batch_mode_is_appended_to_the_users_command() {
        assert_eq!(ssh_command(None), "ssh -oBatchMode=yes");
        assert_eq!(ssh_command(Some("ssh -i ~/.ssh/work")), "ssh -i ~/.ssh/work -oBatchMode=yes");
        assert_eq!(ssh_command(Some("ssh -oBatchMode=yes")), "ssh -oBatchMode=yes");
    }

    /// A fake ssh set as `core.sshCommand` in a private global git config: git runs it (mare's
    /// default didn't replace it) with `-oBatchMode=yes` appended; a `GIT_SSH_COMMAND` the user
    /// set wins over the config, as in git, and gets the same treatment.
    #[cfg(unix)]
    #[test]
    fn the_users_core_ssh_command_is_kept_with_batch_mode_appended() {
        use std::os::unix::fs::PermissionsExt;
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let fake = |name: &str| {
            let script = d.path().join(name);
            let log = d.path().join(format!("{name}.log"));
            std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 255\n", log.display())).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            (script.to_string_lossy().into_owned(), log)
        };
        let (cfg_ssh, cfg_log) = fake("cfg-ssh");
        let (env_ssh, env_log) = fake("env-ssh");
        let global = d.path().join("gitconfig");
        std::fs::write(&global, format!("[core]\n\tsshCommand = {cfg_ssh}\n")).unwrap();
        let home = d.path().to_string_lossy().into_owned();
        let global = global.to_string_lossy().into_owned();
        let base = [
            ("HOME", home.as_str()),
            ("GIT_CONFIG_GLOBAL", global.as_str()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_SSH", ""),
        ];
        let url = "ssh://git@mare-test.invalid/repo.git";
        let ls = |env: &[(&str, &str)]| run_with(None, &["ls-remote", url], DEFAULT_TIMEOUT, false, env);

        assert!(ls(&[&base[..], &[("GIT_SSH_COMMAND", "")]].concat()).is_err());
        let called = std::fs::read_to_string(&cfg_log).expect("core.sshCommand was not used");
        assert!(called.contains("-oBatchMode=yes") && called.contains("mare-test.invalid"), "{called}");
        let cfg_calls = called.lines().count();

        assert!(ls(&[&base[..], &[("GIT_SSH_COMMAND", env_ssh.as_str())]].concat()).is_err());
        let called = std::fs::read_to_string(&env_log).expect("GIT_SSH_COMMAND was not used");
        assert!(called.contains("-oBatchMode=yes"), "{called}");
        let after = std::fs::read_to_string(&cfg_log).unwrap().lines().count();
        assert_eq!(after, cfg_calls, "the env var wins over the config");
    }

    #[test]
    fn plink_gets_batch_not_an_openssh_option() {
        assert_eq!(ssh_command(Some("plink -i key.ppk")), "plink -i key.ppk -batch");
        assert_eq!(ssh_command(Some(r#""C:\Program Files\PuTTY\PLINK.EXE" -v"#)).rsplit(' ').next(), Some("-batch"));
        assert_eq!(ssh_command(Some("plink -batch")), "plink -batch");
        assert!(is_plink_prog(r"C:\Program Files\PuTTY\plink.exe"));
        assert!(!is_plink_prog("/usr/bin/ssh") && !is_plink_prog("TortoisePlink.exe"));
    }

    /// git's precedence GIT_SSH_COMMAND > core.sshCommand > GIT_SSH: with
    /// only GIT_SSH set (plink), mare doesn't set GIT_SSH_COMMAND, git runs the user's program as
    /// is, and `mare doctor` hints at `-batch`.
    #[cfg(unix)]
    #[test]
    fn only_git_ssh_is_left_alone_and_doctor_hints_batch() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let global = d.path().join("gitconfig");
        std::fs::write(&global, "").unwrap();
        // a fake plink: records its arguments, then fails like an unreachable host
        let bin = d.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let plink = bin.join("plink");
        let log = d.path().join("plink.log");
        std::fs::write(&plink, format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 255\n", log.display())).unwrap();
        std::fs::set_permissions(&plink, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (home, global, plink) = (
            d.path().to_string_lossy().into_owned(),
            global.to_string_lossy().into_owned(),
            plink.to_string_lossy().into_owned(),
        );
        let base = [
            ("HOME", home.as_str()),
            ("GIT_CONFIG_GLOBAL", global.as_str()),
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_SSH_COMMAND", ""),
        ];
        let with_plink = [&base[..], &[("GIT_SSH", plink.as_str())]].concat();
        let url = "ssh://git@mare-test.invalid/repo.git";
        assert_eq!(ssh_env(&["ls-remote", url], &with_plink), None, "GIT_SSH alone is not overridden");
        assert_eq!(ssh_env(&["status"], &with_plink), None);
        let without = [&base[..], &[("GIT_SSH", "")]].concat();
        assert_eq!(ssh_env(&["ls-remote", url], &without).as_deref(), Some("ssh -oBatchMode=yes"));
        let both = [&with_plink[..], &[("GIT_SSH_COMMAND", "ssh -i k")]].concat();
        let both: Vec<_> = both.iter().filter(|(k, v)| !(*k == "GIT_SSH_COMMAND" && v.is_empty())).copied().collect();
        assert_eq!(ssh_env(&["ls-remote", url], &both).as_deref(), Some("ssh -i k -oBatchMode=yes"));

        let (name, ok, detail) = ssh_doctor_with(&with_plink).expect("a doctor hint for plink");
        assert_eq!(name, "git ssh");
        assert!(!ok && detail.contains("-batch") && detail.contains(&plink), "{detail}");
        assert!(ssh_doctor_with(&[&base[..], &[("GIT_SSH", "/usr/bin/ssh")]].concat()).is_none());
        assert!(ssh_doctor_with(&both).is_none(), "GIT_SSH_COMMAND wins over GIT_SSH");

        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        // git itself ran the plink program, without mare's OpenSSH option
        assert!(run_with(None, &["ls-remote", url], DEFAULT_TIMEOUT, false, &with_plink).is_err());
        let called = std::fs::read_to_string(&log).expect("GIT_SSH (plink) was not used");
        assert!(called.contains("mare-test.invalid") && !called.contains("BatchMode"), "{called}");
    }

    #[test]
    fn the_protocol_allowlist_blocks_remote_helpers_in_git_itself() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("pwned");
        let url = format!("ext::sh -c touch% {}", marker.to_string_lossy());
        let e = run(None, &["ls-remote", &url], DEFAULT_TIMEOUT, true).unwrap_err().to_string();
        assert!(e.contains("not allowed") || e.contains("unsupported"), "{e}");
        assert!(!marker.exists());
    }

    /// The tree is in the job from its first instruction: a grandchild (`ping` under `cmd`) is
    /// a member, and terminating the job (what a timeout / cancellation does) ends it too.
    #[cfg(windows)]
    #[test]
    fn a_job_holds_the_whole_tree_and_ends_it() {
        let mut c = Command::new("cmd");
        c.args(["/c", "ping -n 30 127.0.0.1 >NUL"]).stdin(Stdio::null()).stdout(Stdio::null());
        let (mut child, job) = spawn_contained(&mut c).expect("spawn");
        let end = Instant::now() + Duration::from_secs(5);
        while job.active() < 2 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(job.active() >= 2, "cmd and its ping should both be in the job");
        job.terminate();
        let end = Instant::now() + Duration::from_secs(5);
        while job.active() > 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(job.active(), 0, "the grandchild outlived the job");
        child.wait().unwrap();
    }

    #[test]
    fn a_cancelled_token_stops_before_git_starts() {
        let t = CancellationToken::new();
        t.cancel();
        let e = with_cancel(&t, || run(None, &["--version"], DEFAULT_TIMEOUT, false)).unwrap_err();
        assert!(e.to_string().contains("cancelled"));
        assert!(!cancelled(), "the token is scoped to with_cancel");
    }
}
