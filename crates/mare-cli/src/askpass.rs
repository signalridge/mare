//! sudo password prompts through the TUI.
//!
//! While the TUI runs, child processes see `SUDO_ASKPASS` pointing at a tiny shim that execs
//! `mare askpass <prompt>`, plus a `sudo` shim (first on `PATH`) that adds `-A` — tool commands
//! have no terminal, so plain `sudo` could never ask. The helper connects back to this process
//! over a private Unix socket (0700 directory) and sends a request carrying a per-session secret
//! and a timestamp; mare checks both, shows a masked input box, and returns the password. The
//! password only ever travels helper ⇄ socket ⇄ input box: it never enters a tool result, the
//! session or the model context. Requests time out.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mare_host::Host;

const ENV_SOCK: &str = "MARE_ASKPASS_SOCK";
/// how long the user has to answer
const ANSWER_TIMEOUT: Duration = Duration::from_secs(120);
/// requests older than this are rejected (replayed / stale)
const MAX_SKEW_SECS: u64 = 30;

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_hex(n: usize) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time comparison of the shared secret.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `mare askpass [--sock <path>] [prompt]` — run by sudo; prints the password on stdout.
pub fn client(sock_arg: Option<&str>, prompt: Option<String>) -> i32 {
    let Some(sock) = sock_arg.map(str::to_string).or_else(|| std::env::var(ENV_SOCK).ok()) else {
        eprintln!("mare askpass: not running under mare");
        return 1;
    };
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&sock) else {
        eprintln!("mare askpass: mare is not reachable");
        return 1;
    };
    let _ = stream.set_read_timeout(Some(ANSWER_TIMEOUT + Duration::from_secs(5)));
    let req = serde_json::json!({
        "ts": now_secs(),
        "prompt": prompt.unwrap_or_else(|| "Password:".into()),
    });
    if writeln!(stream, "{req}").is_err() {
        return 1;
    }
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return 1;
    }
    match serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v["password"].as_str().map(str::to_string))
    {
        Some(pw) => {
            println!("{pw}");
            0
        }
        None => 1,
    }
}

static ACTIVE_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
static ATEXIT_REGISTERED: std::sync::Once = std::sync::Once::new();

fn remove_dir_quiet(dir: &std::path::Path) {
    if dir.exists()
        && let Err(error) = std::fs::remove_dir_all(dir)
    {
        eprintln!("mare: remove askpass directory {}: {error}", dir.display());
    }
}

extern "C" fn cleanup_active_dirs_atexit() {
    force_cleanup();
}

fn register_active_dirs(dirs: &[PathBuf]) {
    ATEXIT_REGISTERED.call_once(|| {
        // SAFETY: `cleanup_active_dirs_atexit` is an `extern "C" fn()` safe to run at process exit.
        unsafe {
            libc::atexit(cleanup_active_dirs_atexit);
        }
    });
    let mut active = ACTIVE_DIRS.lock().unwrap_or_else(|e| e.into_inner());
    for dir in dirs {
        if !active.contains(dir) {
            active.push(dir.clone());
        }
    }
}

fn unregister_active_dirs(dirs: &[&PathBuf]) {
    let mut active = ACTIVE_DIRS.lock().unwrap_or_else(|e| e.into_inner());
    active.retain(|existing| !dirs.contains(&existing));
}

/// Remove every active askpass socket/shim directory before forced exit (`_exit`) or `atexit`,
/// matching `mare_binding::trees::force_cleanup()`.
pub(crate) fn force_cleanup() {
    let dirs = {
        let mut active = ACTIVE_DIRS.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *active)
    };
    for dir in dirs {
        remove_dir_quiet(&dir);
    }
}

/// Removes the askpass shim and socket directories when the TUI exits.
pub struct Guard {
    shim_dir: PathBuf,
    sock_dir: PathBuf,
    server: Option<tokio::task::AbortHandle>,
    #[cfg(test)]
    token: String,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.abort();
        }
        unregister_active_dirs(&[&self.sock_dir, &self.shim_dir]);
        for dir in [&self.sock_dir, &self.shim_dir] {
            remove_dir_quiet(dir);
        }
    }
}

fn is_private_owned_dir(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).is_ok_and(|m| {
        !m.file_type().is_symlink() && m.is_dir() && m.uid() == unsafe { libc::geteuid() } && (m.mode() & 0o077) == 0
    })
}

fn create_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(path)?;
    if !is_private_owned_dir(path) {
        let _ = std::fs::remove_dir_all(path);
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("askpass directory {} is not private", path.display()),
        ));
    }
    Ok(())
}

fn private_runtime_dir() -> std::io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(xdg);
        if p.is_absolute() && is_private_owned_dir(&p) {
            return Ok(p);
        }
    }
    let state_run = mare_shared::paths::state_dir().join("run");
    mare_shared::private::create_dir_all(&state_run)?;
    if !is_private_owned_dir(&state_run) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("runtime directory {} is not private", state_run.display()),
        ));
    }
    Ok(state_run)
}

fn socket_parent_dir(runtime_dir: &std::path::Path) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    const SUN_PATH_SAFE: usize = 103;
    let fits = |parent: &std::path::Path| {
        let sample = parent.join(format!("mare-ap-{}-{}", u32::MAX, "0".repeat(8))).join("s");
        sample.as_os_str().as_bytes().len() <= SUN_PATH_SAFE
    };
    if fits(runtime_dir) {
        return runtime_dir.to_path_buf();
    }
    let tmp = std::env::temp_dir();
    if tmp.is_absolute() && fits(&tmp) {
        return tmp;
    }
    PathBuf::from("/tmp")
}

fn write_exec(path: &std::path::Path, body: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o700).open(path)?;
    f.write_all(body.as_bytes())
}

/// Start the askpass server for the TUI and export the environment child processes inherit.
/// Must run before the host spawns tool processes (it sets this process's environment).
pub fn serve(host: Arc<Host>) -> anyhow::Result<Guard> {
    // Probe and cache sandbox backend/bwrap path before any PATH mutation.
    mare_binding::ossandbox::warm();
    let runtime_dir = private_runtime_dir()?;
    let pid = std::process::id();
    let shim_dir = runtime_dir.join(format!("mare-askpass-bin-{pid}-{}", random_hex(8)?));
    let sock_dir = socket_parent_dir(&runtime_dir).join(format!("mare-ap-{pid}-{}", random_hex(4)?));
    let token = random_hex(32)?;
    register_active_dirs(&[sock_dir.clone(), shim_dir.clone()]);
    let mut guard = Guard {
        shim_dir: shim_dir.clone(),
        sock_dir: sock_dir.clone(),
        server: None,
        #[cfg(test)]
        token: token.clone(),
    };
    create_private_dir(&shim_dir)?;
    create_private_dir(&sock_dir)?;

    let sock = sock_dir.join("s");
    let exe = std::env::current_exe()?;
    let q = |p: &std::path::Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));

    let helper = shim_dir.join("askpass");
    // Pass `--sock` directly in the private shim instead of relying on `MARE_*` env vars (which
    // `subenv` strips from child processes), and never place any secret token in the environment.
    write_exec(&helper, &format!("#!/bin/sh\nexec {} askpass --sock {} \"$@\"\n", q(&exe), q(&sock)))?;
    let bin = shim_dir.join("bin");
    create_private_dir(&bin)?;
    // `sudo` → the real sudo with -A (askpass), found on PATH after this directory
    write_exec(
        &bin.join("sudo"),
        "#!/bin/sh\nself=$(cd \"$(dirname \"$0\")\" && pwd)\nIFS=:\nfor d in $PATH; do\n  [ \"$d\" = \"$self\" ] && continue\n  [ -x \"$d/sudo\" ] && exec \"$d/sudo\" -A \"$@\"\ndone\necho 'sudo: command not found' >&2\nexit 127\n",
    )?;

    let listener = tokio::net::UnixListener::bind(&sock)?;
    // SAFETY: called on the main thread during TUI start-up, before tool processes are spawned.
    // `ENV_SOCK` stays in the parent process for sandbox root whitelisting (`ossandbox/roots.rs`)
    // while `subenv` continues scrubbing `MARE_*` and `*TOKEN*` from child processes.
    unsafe {
        std::env::set_var("SUDO_ASKPASS", &helper);
        std::env::set_var(ENV_SOCK, &sock);
        let path = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{path}", bin.display()));
    }

    let server = tokio::task::spawn_local(async move {
        let mut requests = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { break };
                    let host = host.clone();
                    let token = token.clone();
                    requests.spawn_local(async move {
                        let _ = handle(stream, &host, &token).await;
                    });
                }
                _ = requests.join_next(), if !requests.is_empty() => {}
            }
        }
        // JoinSet drop aborts pending password requests when the listener is cancelled.
    });
    guard.server = Some(server.abort_handle());
    Ok(guard)
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_stat_ppid(stat: &str) -> Option<u32> {
    let close = stat.rfind(')')?;
    let rest = stat.get(close + 1..)?.trim_start();
    // Fields after `(comm)`: state (0), ppid (1)
    let mut fields = rest.split_whitespace();
    let _state = fields.next()?;
    fields.next()?.parse::<u32>().ok()
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_status_euid(status: &str) -> Option<u32> {
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            let mut fields = rest.split_whitespace();
            let _real = fields.next()?;
            return fields.next()?.parse::<u32>().ok();
        }
    }
    None
}

/// Verify that the connecting helper's parent process is a root-owned `sudo` binary, so an
/// unprivileged sandboxed command cannot directly invoke `$SUDO_ASKPASS` to spoof a password
/// prompt and capture the user's password on stdout.
#[cfg(target_os = "linux")]
fn peer_parent_is_root_sudo(peer_pid: Option<i32>) -> bool {
    let Some(pid) = peer_pid.filter(|&p| p > 0) else {
        return false;
    };
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some(ppid) = parse_proc_stat_ppid(&stat).filter(|&p| p > 0) else {
        return false;
    };
    let Ok(comm) = std::fs::read_to_string(format!("/proc/{ppid}/comm")) else {
        return false;
    };
    if comm.trim() != "sudo" {
        return false;
    }
    let Ok(status) = std::fs::read_to_string(format!("/proc/{ppid}/status")) else {
        return false;
    };
    parse_proc_status_euid(&status) == Some(0)
}

#[cfg(target_os = "macos")]
fn macos_proc_ppid(pid: i32) -> Option<u32> {
    if pid <= 0 {
        return None;
    }
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, std::ptr::addr_of_mut!(info).cast(), size) };
    if n != size || info.pbi_ppid == 0 {
        return None;
    }
    Some(info.pbi_ppid)
}

#[cfg(target_os = "macos")]
fn macos_ps_uid_comm(pid: u32) -> Option<(u32, String)> {
    let out = std::process::Command::new("/bin/ps").args(["-o", "uid=,comm=", "-p", &pid.to_string()]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let trimmed = line.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let uid = parts.next()?.trim().parse::<u32>().ok()?;
    let comm = parts.next()?.trim().to_string();
    Some((uid, comm))
}

#[cfg(target_os = "macos")]
fn peer_parent_is_root_sudo(peer_pid: Option<i32>) -> bool {
    let Some(pid) = peer_pid.filter(|&p| p > 0) else {
        return false;
    };
    let Some(ppid) = macos_proc_ppid(pid) else {
        return false;
    };
    let Some((euid, comm)) = macos_ps_uid_comm(ppid) else {
        return false;
    };
    euid == 0 && (comm == "/usr/bin/sudo" || comm == "sudo")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn peer_parent_is_root_sudo(_peer_pid: Option<i32>) -> bool {
    false
}

async fn handle(stream: tokio::net::UnixStream, host: &Host, token: &str) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    let peer_cred = stream.peer_cred().ok();
    let same_uid = peer_cred.as_ref().is_some_and(|c| c.uid() == unsafe { libc::geteuid() });
    let peer_pid = peer_cred.and_then(|c| c.pid());
    let (r, mut w) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(r);
    let mut line = String::new();
    const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(5);
    const CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
    tokio::time::timeout(REQUEST_READ_TIMEOUT, (&mut reader).take(8192).read_line(&mut line)).await??;
    if line.len() >= 8192 && !line.ends_with('\n') {
        w.write_all(b"{\"error\":\"too_large\"}\n").await?;
        return Ok(());
    }
    let req: serde_json::Value = serde_json::from_str(&line)?;
    let fresh = req["ts"].as_u64().is_some_and(|ts| now_secs().abs_diff(ts) <= MAX_SKEW_SECS);
    let token_ok = req["token"].as_str().is_some_and(|t| same(t, token));
    let sudo_ok = peer_parent_is_root_sudo(peer_pid);
    if !fresh || !same_uid || (!token_ok && !sudo_ok) {
        w.write_all(b"{\"error\":\"denied\"}\n").await?;
        return Ok(());
    }
    let prompt = req["prompt"].as_str().unwrap_or("Password:").trim().to_string();
    let cancel = tokio_util::sync::CancellationToken::new();
    let modal_cancel = cancel.clone();
    // the regular input box with secret=true: masked, and covered by ui/wait-start (notify)
    let ask = async move {
        let lua = host.lua();
        let f: mlua::Function =
            lua.load("return function(t) return mare.ui.input(t) end").set_name("=askpass").eval()?;
        let t = lua.create_table()?;
        t.set("title", format!("sudo — {prompt}"))?;
        t.set("placeholder", "password (not shown to the model)")?;
        t.set("secret", true)?;
        // ui/wait-start {kind = "sudo"} (status: blocked, notify, terminal multiplexers)
        t.set("wait_kind", "sudo")?;
        t.set("cancel", mare_shared::lua::Token(modal_cancel))?;
        f.call_async::<Option<String>>(t).await
    };
    let peer_closed = async {
        let mut buf = [0u8; 1];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    };
    tokio::pin!(ask);
    let (answer, disconnected) = match tokio::time::timeout(ANSWER_TIMEOUT, async {
        tokio::select! {
            biased;
            res = &mut ask => (res.ok().flatten(), false),
            _ = peer_closed => {
                cancel.cancel();
                let _ = tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, &mut ask).await;
                (None, true)
            }
        }
    })
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            cancel.cancel();
            let _ = tokio::time::timeout(CANCEL_DRAIN_TIMEOUT, &mut ask).await;
            (None, false)
        }
    };
    if disconnected {
        return Ok(());
    }
    let reply = match answer {
        Some(pw) => serde_json::json!({ "password": pw }),
        None => serde_json::json!({ "error": "cancelled" }),
    };
    let _ = w.write_all(format!("{reply}\n").as_bytes()).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn guard_stops_listener_and_releases_its_owner() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = crate::HOST_TEST_LOCK.lock().await;
                let work = tempfile::tempdir().unwrap();
                let host = Host::new(mare_host::HostOptions {
                    cwd: work.path().to_path_buf(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let weak = Arc::downgrade(&host);
                let names = ["SUDO_ASKPASS", ENV_SOCK, "PATH"];
                let saved = names.map(|name| (name, std::env::var_os(name)));
                let guard = serve(host.clone()).unwrap();
                assert!(
                    std::env::var_os("MARE_ASKPASS_TOKEN").is_none(),
                    "askpass token must never be exported into environment"
                );
                let shim_dir = guard.shim_dir.clone();
                let sock_dir = guard.sock_dir.clone();
                assert_ne!(shim_dir, sock_dir);
                assert!(shim_dir.join("askpass").exists());
                assert!(shim_dir.join("bin/sudo").exists());
                assert!(!sock_dir.join("askpass").exists() && !sock_dir.join("bin").exists());
                let sock = sock_dir.join("s");
                let server = guard.server.clone().unwrap();
                let mut oversz = tokio::net::UnixStream::connect(&sock).await.unwrap();
                let payload = vec![b'a'; 16 * 1024];
                if let Err(err) = tokio::io::AsyncWriteExt::write_all(&mut oversz, &payload).await {
                    assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe, "unexpected askpass write error: {err}");
                }
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut oversz).await;
                let mut reply = String::new();
                tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(&mut oversz), &mut reply)
                    .await
                    .unwrap();
                assert!(reply.contains("too_large"), "{reply}");
                let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
                tokio::io::AsyncWriteExt::write_all(&mut client, b"{unfinished").await.unwrap();
                tokio::task::yield_now().await;
                assert!(Arc::strong_count(&host) >= 3, "server and pending request own the Host");
                drop(host);
                assert!(weak.upgrade().is_some());
                drop(guard);
                tokio::task::yield_now().await;
                tokio::task::yield_now().await;
                assert!(server.is_finished(), "serve listener must be cancelled");
                assert!(weak.upgrade().is_none(), "listener must release its Host owner");
                assert!(!shim_dir.exists() && !sock_dir.exists());
                assert!(tokio::net::UnixStream::connect(sock).await.is_err());
                for (name, value) in saved {
                    // SAFETY: nextest isolates this test in its own process.
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(name, value),
                            None => std::env::remove_var(name),
                        }
                    }
                }
            })
            .await;
    }

    #[tokio::test]
    async fn askpass_cancels_modal_immediately_when_client_disconnects() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = crate::HOST_TEST_LOCK.lock().await;
                let work = tempfile::tempdir().unwrap();
                let host = Host::new(mare_host::HostOptions {
                    cwd: work.path().to_path_buf(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel::<()>();
                let (cancelled_tx, cancelled_rx) = tokio::sync::oneshot::channel::<bool>();
                let entered_tx = std::rc::Rc::new(std::cell::RefCell::new(Some(entered_tx)));
                let cancelled_tx = std::rc::Rc::new(std::cell::RefCell::new(Some(cancelled_tx)));
                let mock_input = host
                    .lua()
                    .create_async_function(move |_, t: mlua::Table| {
                        let entered_tx = entered_tx.clone();
                        let cancelled_tx = cancelled_tx.clone();
                        async move {
                            if let Some(tx) = entered_tx.borrow_mut().take() {
                                let _ = tx.send(());
                            }
                            let cancel = t
                                .get::<Option<mlua::AnyUserData>>("cancel")?
                                .and_then(|ud| ud.borrow::<mare_binding::task::Token>().ok().map(|tok| tok.0.clone()));
                            let was_cancelled = match cancel {
                                Some(tok) => {
                                    tok.cancelled().await;
                                    true
                                }
                                None => {
                                    std::future::pending::<()>().await;
                                    false
                                }
                            };
                            if let Some(tx) = cancelled_tx.borrow_mut().take() {
                                let _ = tx.send(was_cancelled);
                            }
                            Ok(mlua::Value::Nil)
                        }
                    })
                    .unwrap();
                let ui: mlua::Table = host.lua().globals().get::<mlua::Table>("mare").unwrap().get("ui").unwrap();
                ui.set("input", mock_input).unwrap();

                let names = ["SUDO_ASKPASS", ENV_SOCK, "PATH"];
                let saved = names.map(|name| (name, std::env::var_os(name)));
                let guard = serve(host.clone()).unwrap();
                assert!(
                    std::env::var_os("MARE_ASKPASS_TOKEN").is_none(),
                    "askpass token must never be exported into environment"
                );
                let sock = guard.sock_dir.join("s");
                let token = guard.token.clone();

                // Direct connection without token and without root-owned sudo parent must be denied.
                let mut unauth = tokio::net::UnixStream::connect(&sock).await.unwrap();
                let bad_req = serde_json::json!({ "ts": now_secs(), "prompt": "Spoofed:" });
                tokio::io::AsyncWriteExt::write_all(&mut unauth, format!("{bad_req}\n").as_bytes()).await.unwrap();
                let mut unauth_reply = String::new();
                tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(&mut unauth), &mut unauth_reply)
                    .await
                    .unwrap();
                assert!(
                    unauth_reply.contains("denied"),
                    "non-sudo client without token must be denied: {unauth_reply}"
                );

                let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
                let req = serde_json::json!({ "token": token, "ts": now_secs(), "prompt": "Password:" });
                tokio::io::AsyncWriteExt::write_all(&mut client, format!("{req}\n").as_bytes()).await.unwrap();
                tokio::time::timeout(Duration::from_secs(2), entered_rx)
                    .await
                    .expect("askpass did not open input")
                    .unwrap();

                // Drop client socket (simulating killed sudo / mare askpass).
                drop(client);

                let cancelled = tokio::time::timeout(Duration::from_secs(2), cancelled_rx)
                    .await
                    .expect("askpass modal was not cancelled promptly on client disconnect")
                    .unwrap();
                assert!(cancelled, "askpass must cancel mare.ui.input token when client disconnects");

                drop(guard);
                host.shutdown().await;
                for (name, value) in saved {
                    // SAFETY: nextest isolates this test in its own process.
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(name, value),
                            None => std::env::remove_var(name),
                        }
                    }
                }
            })
            .await;
    }

    #[test]
    fn proc_stat_and_status_parsers_identify_sudo_parent_fields() {
        assert_eq!(parse_proc_stat_ppid("1234 (mare askpass) S 5678 1234 1234 0 -1"), Some(5678));
        assert_eq!(parse_proc_stat_ppid("99 (a)b)c) R 42 1 1"), Some(42));
        assert_eq!(parse_proc_status_euid("Name:\tsudo\nUid:\t1000\t0\t0\t0\nGid:\t1000\t1000\t1000\t1000\n"), Some(0));
        assert_eq!(parse_proc_status_euid("Name:\tbash\nUid:\t1000\t1000\t1000\t1000\n"), Some(1000));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_peer_parent_inspection_reads_self_ppid_and_root_pid1() {
        let self_pid = std::process::id() as i32;
        assert_eq!(macos_proc_ppid(self_pid), Some(std::os::unix::process::parent_id()));
        let (uid, comm) = macos_ps_uid_comm(1).expect("/bin/ps must read root PID 1");
        assert_eq!(uid, 0, "PID 1 must have euid 0");
        assert!(!comm.is_empty(), "PID 1 comm must be non-empty");
        assert!(!peer_parent_is_root_sudo(Some(self_pid)), "test runner is not spawned by root sudo");
    }

    #[tokio::test]
    async fn askpass_cancels_input_modal_when_client_disconnects() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = crate::HOST_TEST_LOCK.lock().await;
                let work = tempfile::tempdir().unwrap();
                let ui = mare_shared::ui::UiShared::new();
                let (ui_tx, mut ui_rx) = tokio::sync::mpsc::unbounded_channel();
                *ui.tx.write().unwrap() = Some(ui_tx);
                let host = Host::with_ui(
                    mare_host::HostOptions {
                        cwd: work.path().to_path_buf(),
                        mode: mare_plugin::Mode::Tui,
                        config: serde_json::json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                        load_init: false,
                        project_trusted: false,
                        args: serde_json::json!({"no_session":true}),
                        no_user_plugins: true,
                    },
                    ui,
                )
                .await
                .unwrap();

                let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
                let h = host.clone();
                let task = tokio::task::spawn_local(async move { handle(server, &h, "secret-token").await });

                let req = serde_json::json!({
                    "token": "secret-token",
                    "ts": now_secs(),
                    "prompt": "[sudo] password:",
                });
                tokio::io::AsyncWriteExt::write_all(&mut client, format!("{req}\n").as_bytes()).await.unwrap();

                // Wait until askpass opens the password input modal (keep `reply` alive so the modal stays open).
                let (modal_id, _reply_hold) = tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        match ui_rx.recv().await {
                            Some(mare_shared::ui::UiCmd::Input { modal_id, reply, .. }) => return (modal_id, reply),
                            Some(_) => continue,
                            None => panic!("ui_rx closed before Input modal opened"),
                        }
                    }
                })
                .await
                .expect("timed out waiting for askpass input modal");

                // Simulate sudo/askpass client being killed before the user answers.
                drop(client);

                // The askpass handler must abort promptly and send CancelModal for the orphaned prompt.
                tokio::time::timeout(Duration::from_millis(600), task)
                    .await
                    .expect("askpass handle must exit promptly when client disconnects")
                    .unwrap()
                    .unwrap();

                let cancelled = tokio::time::timeout(Duration::from_millis(300), async {
                    loop {
                        match ui_rx.recv().await {
                            Some(mare_shared::ui::UiCmd::CancelModal { modal_id: id }) if id == modal_id => {
                                return true;
                            }
                            Some(_) => continue,
                            None => return false,
                        }
                    }
                })
                .await
                .unwrap_or(false);
                assert!(cancelled, "askpass must cancel the open password modal when client disconnects");
                host.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn force_cleanup_removes_active_dirs_when_guard_is_not_dropped() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = crate::HOST_TEST_LOCK.lock().await;
                let work = tempfile::tempdir().unwrap();
                let host = Host::new(mare_host::HostOptions {
                    cwd: work.path().to_path_buf(),
                    mode: mare_plugin::Mode::Print,
                    config: serde_json::json!({"model":"faux/echo","plugins":{"permissions":{"enabled":false}}}),
                    load_init: false,
                    project_trusted: false,
                    args: serde_json::json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let names = ["SUDO_ASKPASS", ENV_SOCK, "PATH"];
                let saved = names.map(|name| (name, std::env::var_os(name)));
                let guard = serve(host.clone()).unwrap();
                let shim_dir = guard.shim_dir.clone();
                let sock_dir = guard.sock_dir.clone();
                let server = guard.server.clone().unwrap();
                assert!(shim_dir.exists() && sock_dir.exists());
                // Simulate forced exit (`_exit` / `std::process::exit`) skipping `Drop`.
                std::mem::forget(guard);
                assert!(shim_dir.exists() && sock_dir.exists());
                force_cleanup();
                assert!(!shim_dir.exists(), "force_cleanup must remove shim dir: {}", shim_dir.display());
                assert!(!sock_dir.exists(), "force_cleanup must remove sock dir: {}", sock_dir.display());
                server.abort();
                tokio::task::yield_now().await;
                host.shutdown().await;
                for (name, value) in saved {
                    // SAFETY: nextest isolates this test in its own process.
                    unsafe {
                        match value {
                            Some(value) => std::env::set_var(name, value),
                            None => std::env::remove_var(name),
                        }
                    }
                }
            })
            .await;
    }

    #[test]
    fn socket_parent_dir_keeps_short_name_inside_runtime_dir_under_sun_path_limit() {
        let seventy = PathBuf::from(format!("/{}", "a".repeat(69)));
        assert_eq!(socket_parent_dir(&seventy), seventy, "70-byte runtime_dir must fit without falling back to /tmp");
        let sample = seventy.join(format!("mare-ap-{}-{}", u32::MAX, "0".repeat(8))).join("s");
        assert!(sample.as_os_str().as_encoded_bytes().len() <= 103);

        let too_long = PathBuf::from(format!("/{}", "b".repeat(90)));
        assert_ne!(socket_parent_dir(&too_long), too_long, "oversized runtime_dir must fall back cleanly");
    }
}
