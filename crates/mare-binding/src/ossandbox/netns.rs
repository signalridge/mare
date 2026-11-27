//! Linux `net_proxy`: the command runs in bubblewrap's own network namespace, which
//! has only loopback; inside it a loopback TCP listener bridges over a Unix socket to mare's
//! filtering proxy ([`crate::netproxy`]) on the host (Apache-2.0):
//! the per-run 0700 socket directory and its name/length rules, the stale-directory
//! sweep by owner pid, and the host bridge UDS → TCP proxy; the in-namespace
//! half is [`super::netns_inner`].
//!
//! This file is the host half: the socket directory and the bridge task, both owned by the
//! spawned command's [`super::Prepared`] (dropped with it: task aborted, directory removed), and
//! the capability probe. The namespace's processes end with bubblewrap's PID namespace, which
//! the command's process tree ([`crate::trees::Tree`]) ends with its host.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

/// Hidden subcommand of the `mare` binary that runs inside the namespace (the inner half).
pub const NETNS_ARG: &str = "__netns-proxy";

const DIR_PREFIX: &str = "mare-netproxy-";
const SOCK_NAME: &str = "p.sock";
/// `sockaddr_un.sun_path` holds 108 bytes including the trailing NUL.
const SUN_PATH_MAX: usize = 107;

/// What a policy-filtered spawn needs to route its namespace through the proxy.
pub struct NetnsBridge {
    dir: PathBuf,
    sock: PathBuf,
    port: u16,
    helper: PathBuf,
    task: Option<tokio::task::AbortHandle>,
}

impl Drop for NetnsBridge {
    fn drop(&mut self) {
        if let Some(t) = &self.task {
            t.abort();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl NetnsBridge {
    /// Start the host half for the proxy listening on 127.0.0.1:`port`. Must run inside a tokio
    /// runtime (like [`crate::netproxy::ProxyHandle::start`]).
    pub fn start(port: u16) -> std::io::Result<Self> {
        Self::start_in(&socket_parent(), port)
    }

    /// [`Self::start`] with the socket directory under `parent`.
    pub fn start_in(parent: &Path, port: u16) -> std::io::Result<Self> {
        let helper = helper_exe()?;
        sweep_stale(parent);
        let dir = create_dir(parent)?;
        let sock = dir.join(SOCK_NAME);
        let mut me = Self { dir, sock, port, helper, task: None };
        let listener = tokio::net::UnixListener::bind(&me.sock)?;
        me.task = Some(tokio::spawn(serve(listener, port)).abort_handle());
        Ok(me)
    }

    /// Paths the namespace must see (read-only binds): the socket directory and the helper.
    pub fn mounts(&self) -> [&Path; 2] {
        [&self.dir, &self.helper]
    }

    /// The command line run inside the namespace in place of `cmd args…`.
    pub fn argv(&self, cmd: &str, args: &[String]) -> Vec<String> {
        let mut a = vec![
            self.helper.to_string_lossy().into_owned(),
            NETNS_ARG.into(),
            "--uds".into(),
            self.sock.to_string_lossy().into_owned(),
            "--port".into(),
            self.port.to_string(),
            "--".into(),
            cmd.into(),
        ];
        a.extend(args.iter().cloned());
        a
    }
}

/// The host bridge: every connection on the Unix socket goes to the proxy on 127.0.0.1:`port`.
async fn serve(listener: tokio::net::UnixListener, port: u16) {
    while let Ok((mut inner, _)) = listener.accept().await {
        tokio::spawn(async move {
            match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                Ok(mut proxy) => {
                    let _ = tokio::io::copy_bidirectional(&mut inner, &mut proxy).await;
                }
                Err(e) => {
                    let why = format!("the proxy on 127.0.0.1:{port} is not reachable ({e})");
                    let _ = unreachable_reply(&mut inner, &why).await;
                }
            }
        });
    }
}

/// Body of the reply when the proxy can't be reached: the sandbox has no other way out.
pub(super) fn unreachable_body(why: &str) -> String {
    format!("mare sandbox proxy: {why}; the sandbox has no other network path\n")
}

/// Fail closed, but say why: an HTTP client (anything but SOCKS5, whose first byte is 5) gets a
/// `502` naming the sandbox proxy; a SOCKS client just sees the connection close.
async fn unreachable_reply(s: &mut tokio::net::UnixStream, why: &str) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut first = [0u8; 1];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), s.read(&mut first)).await.unwrap_or(Ok(0))?;
    if n == 1 && first[0] != 0x05 {
        let body = unreachable_body(why);
        let resp = format!(
            "HTTP/1.1 502 Bad Gateway\r\nx-mare-sandbox: proxy-unreachable\r\ncontent-type: text/plain\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        s.write_all(resp.as_bytes()).await?;
    }
    s.shutdown().await
}

/// The executable that runs the inner half: the Windows runner's lookup
/// ([`crate::winsandbox::helper_exe`]: `MARE_SANDBOX_HELPER`, this `mare`, or a `mare` next to
/// the test harness), required to exist.
pub(super) fn helper_exe() -> std::io::Result<PathBuf> {
    let h = crate::winsandbox::helper_exe();
    let h = dunce::canonicalize(&h).map_err(|e| {
        std::io::Error::new(e.kind(), format!("the mare helper {} for the network bridge: {e}", h.display()))
    })?;
    if h.file_stem().is_none_or(|s| s != "mare") {
        return Err(std::io::Error::other(format!(
            "no mare executable for the network bridge (found {})",
            h.display()
        )));
    }
    Ok(h)
}

/// Where socket directories go: `$XDG_RUNTIME_DIR` (per user, 0700), else the temp directory,
/// else `/tmp` — the first whose socket path fits `sun_path`.
pub(super) fn socket_parent() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_absolute() && p.is_dir());
    runtime.into_iter().chain([std::env::temp_dir()]).find(|p| fits(p)).unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn fits(parent: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let longest = parent.join(format!("{DIR_PREFIX}{}-{}", u32::MAX, u32::MAX)).join(SOCK_NAME);
    longest.as_os_str().as_bytes().len() <= SUN_PATH_MAX
}

/// A fresh 0700 directory `mare-netproxy-<pid>-<seq>` under `parent`.
fn create_dir(parent: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    static SEQ: AtomicU32 = AtomicU32::new(0);
    if !fits(parent) {
        return Err(std::io::Error::other(format!("{} is too long for a Unix socket path", parent.display())));
    }
    for _ in 0..128 {
        let d = parent.join(format!("{DIR_PREFIX}{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed)));
        match std::fs::DirBuilder::new().mode(0o700).create(&d) {
            Ok(()) => return Ok(d),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(std::io::Error::new(e.kind(), format!("{}: {e}", d.display()))),
        }
    }
    Err(std::io::Error::other(format!("no free socket directory under {}", parent.display())))
}

/// The pid a socket directory belongs to (`mare-netproxy-<pid>-<seq>`).
fn owner_pid(name: &str) -> Option<i32> {
    let (pid, _) = name.strip_prefix(DIR_PREFIX)?.split_once('-')?;
    pid.parse().ok().filter(|p| *p > 0)
}

/// Remove socket directories left by mare processes that no longer run (killed before their
/// bridges were dropped), once per process and parent.
fn sweep_stale(parent: &Path) {
    static DONE: OnceLock<std::sync::Mutex<Vec<PathBuf>>> = OnceLock::new();
    let mut done = DONE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    if done.iter().any(|p| p == parent) {
        return;
    }
    done.push(parent.to_path_buf());
    let Ok(entries) = std::fs::read_dir(parent) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = owner_pid(&name.to_string_lossy()) else { continue };
        // SAFETY: signal 0 only checks that the process exists
        let alive =
            unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
        if !alive && e.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Probe (once per process): bubblewrap starts the helper in a fresh network namespace, where it
/// binds a loopback port and installs the command's seccomp filter. `Err` says why not — shown
/// by `mare doctor` (`sandbox net_proxy`).
pub fn probe() -> Result<(), String> {
    static R: OnceLock<Result<(), String>> = OnceLock::new();
    R.get_or_init(|| {
        let helper = helper_exe().map_err(|e| e.to_string())?;
        let h = helper.to_string_lossy().into_owned();
        let mut args: Vec<String> = super::bwrap::bwrap_base_args().iter().map(|s| (*s).to_string()).collect();
        args.extend(["--unshare-net", "--ro-bind", &h, &h, "--", &h, NETNS_ARG, "--check"].map(String::from));
        let out = std::process::Command::new(super::bwrap::bwrap_exec())
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("bwrap: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(format!("the network-namespace bridge did not start: {}", err.lines().last().unwrap_or("").trim()))
        }
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_dirs_are_private_short_and_swept_by_owner() {
        let base = socket_parent();
        let base = if fits(&base.join("m-XXXXXX")) { base } else { PathBuf::from("/tmp") };
        let d = tempfile::Builder::new().prefix("m-").tempdir_in(&base).unwrap();
        let parent = dunce::canonicalize(d.path()).unwrap();
        let a = create_dir(&parent).unwrap();
        let b = create_dir(&parent).unwrap();
        assert_ne!(a, b);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&a).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(owner_pid(&a.file_name().unwrap().to_string_lossy()), Some(std::process::id() as i32));
        assert_eq!(owner_pid("mare-netproxy-x-1"), None);
        assert_eq!(owner_pid("other-1-2"), None);
        // a directory whose owner is gone is removed; ours stays
        let dead = parent.join(format!("{DIR_PREFIX}{}-0", i32::MAX));
        std::fs::create_dir(&dead).unwrap();
        sweep_stale(&parent);
        assert!(!dead.exists() && a.exists());
        // a parent too long for sun_path is refused, not truncated
        let long = parent.join("x".repeat(100));
        std::fs::create_dir(&long).unwrap();
        assert!(!fits(&long) && create_dir(&long).is_err());
    }

    #[test]
    fn unreachable_reply_names_the_sandbox_proxy() {
        let b = unreachable_body("the proxy on 127.0.0.1:9 is not reachable (refused)");
        assert!(b.starts_with("mare sandbox proxy: ") && b.contains("no other network path"), "{b}");
    }
}
