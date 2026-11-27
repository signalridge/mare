//! Backend and capability probing: which mechanism confines processes here and what it enforces.

use std::sync::OnceLock;

#[cfg(target_os = "linux")]
use super::bwrap::{bwrap_base_args, bwrap_exec};
use super::caps::{Cap, caps_override, narrow};
#[cfg(target_os = "linux")]
use super::landlock::landlock_abi;
#[cfg(target_os = "linux")]
use super::prexec::{SocketBlock, child_restrict, seccomp_filter};
#[cfg(unix)]
use super::seatbelt::SEATBELT_EXEC;
use super::{Backend, Policy};
use crate::platform::Host;

impl Backend {
    /// What this backend actually enforces on this host:
    /// each dimension is declared only when its mechanism is implemented here *and* a runtime
    /// probe showed it works ([`probe_caps`], once per process) — never from the backend name
    /// alone.
    pub fn caps(self) -> Vec<Cap> {
        static CAPS: OnceLock<Vec<Cap>> = OnceLock::new();
        let probed = if self != backend() { probe_caps(self) } else { CAPS.get_or_init(|| probe_caps(self)).clone() };
        narrow(probed, caps_override().as_deref())
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Seatbelt => "seatbelt",
            Self::Bwrap => "bwrap",
            Self::Landlock { .. } => "landlock",
            Self::WinAcl => "windows-acl",
            Self::WinUser => "windows-user",
            Self::None => "none",
        }
    }
}

/// Probe the backend and its capabilities using the same once-per-process cache as callers.
/// The Host schedules this synchronous body in its owned blocking scope during startup.
pub fn warm() {
    #[cfg(target_os = "linux")]
    let _ = bwrap_exec();
    let _ = backend().caps();
}

/// The usable backend, probed once per process. `MARE_SANDBOX_BACKEND=none` forces "none"
/// (to check the refusal path); `MARE_SANDBOX_BACKEND=landlock` forces Landlock on Linux (tests).
pub fn backend() -> Backend {
    if let Ok(over) = std::env::var("MARE_SANDBOX_BACKEND") {
        match over.as_str() {
            "none" => return Backend::None,
            #[cfg(target_os = "linux")]
            "landlock" => {
                return match landlock_abi() {
                    Some(abi) => Backend::Landlock { partial: abi < 3 },
                    None => Backend::None,
                };
            }
            _ => {}
        }
    }
    static B: OnceLock<Backend> = OnceLock::new();
    *B.get_or_init(<Sandbox as SandboxBackend>::probe)
}

/// Probe each capability dimension of `b` on this host (capabilities come
/// from probing). A dimension whose probe fails is simply not declared, so a policy requiring it
/// is refused with `SANDBOX_INSUFFICIENT` instead of running weaker.
pub fn probe_caps(b: Backend) -> Vec<Cap> {
    use Cap::*;
    #[allow(unused_mut)]
    let mut caps = vec![];
    match b {
        Backend::None => {}
        #[cfg(unix)]
        Backend::Seatbelt => {
            let ok = |profile: &str| {
                std::process::Command::new(SEATBELT_EXEC)
                    .args(["-p", profile, "/usr/bin/true"])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success())
            };
            // the write allowlist profile is what backend() probed already
            caps.extend([FsWrite, FsProtect]);
            // the four probes are independent processes: run them side by side (startup cost)
            let (read, host, net, unix) = std::thread::scope(|sc| {
                let r = sc.spawn(|| ok("(version 1)(allow default)(deny file-read* (subpath \"/private/var/empty\"))"));
                let h = sc.spawn(seatbelt_host_protect_probe);
                let n = sc.spawn(|| {
                    ok("(version 1)(allow default)(deny network*)(allow network-outbound (remote ip \"localhost:9\"))")
                });
                let u = sc.spawn(|| {
                    ok("(version 1)(allow default)(deny network*)(allow network-outbound (remote unix-socket (path-literal \"/dev/null\")))")
                });
                (
                    r.join().unwrap_or(false),
                    h.join().unwrap_or(false),
                    n.join().unwrap_or(false),
                    u.join().unwrap_or(false),
                )
            });
            if read {
                caps.push(FsRead);
            }
            if host {
                caps.push(FsHostProtect);
            }
            if net {
                caps.extend([NetBlock, NetProxy]);
            }
            if unix {
                caps.push(UnixSocket);
            }
        }
        #[cfg(target_os = "linux")]
        Backend::Bwrap => {
            caps.push(FsWrite);
            let (protect_ok, host_protect_ok) = bwrap_protect_probe();
            if protect_ok {
                caps.push(FsProtect);
            }
            if host_protect_ok {
                caps.push(FsHostProtect);
            }
            let mut args: Vec<&str> = bwrap_base_args().to_vec();
            args.extend(["--unshare-net", "--", "true"]);
            if run_ok(bwrap_exec(), &args, SocketBlock::default()) {
                caps.push(NetBlock);
                // the loopback-only namespace bridged to the proxy (super::netns)
                if super::netns::probe().is_ok() {
                    caps.push(NetProxy);
                }
            }
            // reads: an empty root with only the allowed roots bound (crate::read_mask)
            let read = crate::read_mask::bwrap_probe_args(bwrap_base_args());
            if run_ok(bwrap_exec(), &read.iter().map(String::as_str).collect::<Vec<_>>(), SocketBlock::default()) {
                caps.push(FsRead);
            }
            // seccomp on bwrap itself (inherited by the command) blocks AF_UNIX
            if run_ok("true", &[], SocketBlock { unix: true, inet: false, netlink_route: false }) {
                caps.push(UnixSocket);
            }
        }
        #[cfg(target_os = "linux")]
        Backend::Landlock { .. } => {
            caps.push(FsWrite);
            // Landlock doesn't cover sockets here: seccomp on the socket families does
            if run_ok("true", &[], SocketBlock { unix: true, inet: true, netlink_route: false }) {
                caps.extend([NetBlock, UnixSocket]);
            }
            // reads: rights beneath the allowed roots only (crate::read_mask)
            if crate::read_mask::landlock_probe() {
                caps.push(FsRead);
            }
        }
        // restricted token + ACLs: writes only; no Unix-socket daemons to guard on Windows
        Backend::WinAcl => caps.extend([FsWrite, UnixSocket]),
        // WFP's proxy port range is shared by the sandbox account, so a restricted call can
        // use another call's proxy, so per-call network isolation is not guaranteed.
        // Sandbox accounts belong to Users and can still read Users-readable volumes, so
        // allowlist `FsRead` is not declared.
        Backend::WinUser => {
            caps.extend([FsWrite, UnixSocket]);
            if crate::winsandbox_user::host_protect_covered() {
                caps.push(FsHostProtect);
            }
        }
        #[allow(unreachable_patterns)]
        _ => {}
    }
    caps
}

#[cfg(all(test, target_os = "linux"))]
type ProtectionProbeEntered = Box<dyn FnOnce(&std::path::Path) + Send>;

#[cfg(all(test, target_os = "linux"))]
static PROTECTION_PROBE_ENTERED: std::sync::Mutex<Option<ProtectionProbeEntered>> = std::sync::Mutex::new(None);

#[cfg(unix)]
fn seatbelt_host_protect_probe() -> bool {
    let Ok(dir) = tempfile::tempdir() else { return false };
    let Ok(root) = crate::realpath::resolve(dir.path()) else { return false };
    let host_dir = root.join("host");
    if std::fs::create_dir(&host_dir).is_err() {
        return false;
    }
    let canary = host_dir.join("token");
    let control = root.join("control");
    if std::fs::write(&canary, "canary").is_err() || std::fs::write(&control, "ok").is_err() {
        return false;
    }
    let q = |p: &std::path::Path| p.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let qh = q(&host_dir);
    let profile =
        format!("(version 1)(allow default)(deny file-read* file-write* (literal \"{qh}\") (subpath \"{qh}\"))");
    std::process::Command::new(SEATBELT_EXEC)
        .args([
            "-p",
            &profile,
            "--",
            "/bin/sh",
            "-c",
            "test \"$(cat \"$1\")\" = ok && ! cat \"$2\"",
            "sh",
            &control.to_string_lossy(),
            &canary.to_string_lossy(),
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Verify bubblewrap can mask `.mare` (unwritable) and a host secret directory (unreadable
/// canary while a sibling control file remains readable) in one probe invocation.
#[cfg(target_os = "linux")]
fn bwrap_protect_probe() -> (bool, bool) {
    let Ok(dir) = tempfile::tempdir() else { return (false, false) };
    let root = dir.path().to_string_lossy().into_owned();
    let target = dir.path().join(".mare");
    let host_dir = dir.path().join("host");
    if std::fs::create_dir(&target).is_err() || std::fs::create_dir(&host_dir).is_err() {
        return (false, false);
    }
    let canary = host_dir.join("token");
    let control = dir.path().join("control");
    if std::fs::write(&canary, "canary").is_err() || std::fs::write(&control, "ok").is_err() {
        return (false, false);
    }
    #[cfg(test)]
    if let Some(entered) = PROTECTION_PROBE_ENTERED.lock().unwrap().take() {
        entered(dir.path());
    }
    let target_s = target.to_string_lossy().into_owned();
    let host_s = host_dir.to_string_lossy().into_owned();
    let control_s = control.to_string_lossy().into_owned();
    let canary_s = canary.to_string_lossy().into_owned();
    let mut args: Vec<&str> = bwrap_base_args().to_vec();
    args.extend([
        "--bind",
        &root,
        &root,
        "--perms",
        "555",
        "--tmpfs",
        &target_s,
        "--remount-ro",
        &target_s,
        "--perms",
        "000",
        "--tmpfs",
        &host_s,
        "--remount-ro",
        &host_s,
        "--",
        "sh",
        "-c",
        "test -d \"$1\" && ! touch \"$1/init.lua\" && test \"$(cat \"$2\")\" = ok && ! cat \"$3\"",
        "--",
        &target_s,
        &control_s,
        &canary_s,
    ]);
    let ok = run_ok(bwrap_exec(), &args, SocketBlock::default());
    (ok, ok)
}

/// Linux probe helper: run `prog args` with the seccomp socket filter (if any) applied in the
/// child; true when it exits 0.
#[cfg(target_os = "linux")]
fn run_ok(prog: &str, args: &[&str], block: SocketBlock) -> bool {
    use std::os::unix::process::CommandExt;
    let mut c = std::process::Command::new(prog);
    c.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if block.any() {
        let Ok(f) = seccomp_filter(block) else { return false };
        // SAFETY: only prctl syscalls in the child (filter built here)
        unsafe {
            c.pre_exec(move || child_restrict(None, Some(&f)));
        }
    }
    c.status().is_ok_and(|s| s.success())
}

/// Unix probe: Seatbelt on macOS; bubblewrap, then Landlock on Linux.
#[cfg(unix)]
pub(crate) fn probe_unix() -> Backend {
    use std::process::{Command, Stdio};
    let ok = |prog: &str, args: &[&str]| {
        Command::new(prog)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if cfg!(target_os = "macos") {
        if ok(SEATBELT_EXEC, &["-p", "(version 1)(allow default)(deny file-write*)", "--", "/usr/bin/true"]) {
            return Backend::Seatbelt;
        }
        return Backend::None;
    }
    #[cfg(target_os = "linux")]
    {
        let mut args: Vec<&str> = bwrap_base_args().to_vec();
        args.extend(["--", "true"]);
        if ok(bwrap_exec(), &args) {
            return Backend::Bwrap;
        }
        if let Some(abi) = landlock_abi() {
            // ABI 3 adds truncate; older kernels leave a file effect uncovered
            return Backend::Landlock { partial: abi < 3 };
        }
    }
    Backend::None
}

impl Policy {
    /// Whether bubblewrap cannot safely mount one of this policy's protected paths.
    pub(super) fn bwrap_metadata_gap(&self) -> bool {
        #[cfg(target_os = "linux")]
        if self.level == super::level::Level::WorkspaceWrite && !super::bwrap_guard::metadata_mounts_safe(self) {
            return true;
        }
        let grants = self.granted_in_protected();
        self.protected_subpaths().iter().any(|p| {
            if grants.iter().any(|g| p.starts_with(g)) {
                return false;
            }
            match std::fs::symlink_metadata(p) {
                Ok(meta) if meta.file_type().is_symlink() => true,
                Ok(_) => {
                    #[cfg(target_os = "linux")]
                    {
                        (super::bwrap_guard::is_marked(p) && !super::bwrap_guard::can_inspect_lifecycle())
                            || !super::bwrap_guard::live_or_unmarked(self, p)
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        false
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && p.file_name().is_some_and(|n| n == ".mare") => {
                    #[cfg(target_os = "linux")]
                    {
                        !super::bwrap_guard::registry_usable(self) || !super::bwrap_guard::can_inspect_lifecycle()
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        false
                    }
                }
                Err(_) => true,
            }
        })
    }

    /// `fs_host_protect` is satisfied ONLY if:
    /// (a) the selected backend declares `FsHostProtect`, uses a generated profile (`profile.is_none()`),
    ///     and the mask actually entered the args/profile for this invocation; OR
    /// (b) `FsRead` is actually declared and enforced (`restrict_read && profile.is_none()`), and the
    ///     full effective read set (`read_mask::visible` unioned with `writable` and `extra_binds`)
    ///     neither covers nor lies within any registered `Refuse` host-protected path.
    pub(super) fn host_protect_satisfied(
        &self,
        backend: Backend,
        extra_binds: &[std::path::PathBuf],
        mask_in_args: bool,
    ) -> bool {
        if self.profile.is_some() {
            return false;
        }
        let caps = backend.caps();
        if caps.contains(&Cap::FsHostProtect) && mask_in_args {
            return true;
        }
        if !matches!(backend, Backend::WinUser)
            && caps.contains(&Cap::FsRead)
            && self.restrict_read
            && let Some(read_set) = crate::read_mask::effective_read_set(self, extra_binds)
        {
            return !host_protected_touched_by_read_set(backend, &read_set);
        }
        false
    }

    /// Required dimensions `backend` can't enforce.
    pub fn missing(&self, backend: Backend) -> Vec<Cap> {
        let have = backend.caps();
        let protect_unavailable = self.profile.is_some() || (backend == Backend::Bwrap && self.bwrap_metadata_gap());
        let mask_in_args = self.profile.is_none()
            && match backend {
                Backend::Bwrap => !super::protected::host_protected_unmaskable(),
                Backend::Seatbelt => true,
                Backend::WinUser => crate::winsandbox_user::host_protect_covered(),
                _ => false,
            };
        let host_protect_satisfied = self.host_protect_satisfied(backend, &[], mask_in_args);
        let mut req = self.required();
        if backend == Backend::Bwrap && !mask_in_args && !req.contains(&Cap::FsHostProtect) {
            req.push(Cap::FsHostProtect);
        }
        let unix_socket_gap = matches!(backend, Backend::Bwrap | Backend::Landlock { .. })
            && matches!(self.network, super::Network::Proxy(_))
            && !self.unix_sockets.is_empty();
        req.into_iter()
            .filter(|c| {
                if *c == Cap::FsHostProtect {
                    return !host_protect_satisfied;
                }
                if *c == Cap::UnixSocket && unix_socket_gap {
                    return true;
                }
                !have.contains(c) || (*c == Cap::FsProtect && protect_unavailable)
            })
            .collect()
    }
}

fn host_protected_touched_by_read_set(backend: Backend, read_set: &[std::path::PathBuf]) -> bool {
    let Ok(resolved_reads) = read_set.iter().map(|r| crate::realpath::resolve(r)).collect::<std::io::Result<Vec<_>>>()
    else {
        return true;
    };
    let overlaps_read = |target: &std::path::Path| {
        resolved_reads
            .iter()
            .any(|r| crate::realpath::path_within(target, r) || crate::realpath::path_within(r, target))
    };

    super::protected::host_protected().into_iter().filter(|e| e.on_gap == super::protected::GapAction::Refuse).any(
        |entry| {
            let Ok(entry_resolved) = crate::realpath::resolve(&entry.path) else { return true };
            if overlaps_read(&entry_resolved) || overlaps_read(&entry.path) {
                return true;
            }
            #[cfg(target_os = "linux")]
            if matches!(backend, Backend::Bwrap | Backend::Landlock { .. }) {
                let Ok(aliases) = super::mount_inspection::all_aliases_or_fail(&entry_resolved) else {
                    // Mount identity could not be verified: never fall back to lexical prefix proof.
                    return true;
                };
                return aliases.iter().any(|alias| overlaps_read(alias));
            }
            let _ = backend;
            false
        },
    )
}

/// OS sandbox backends: which mechanism can confine processes here.
pub trait SandboxBackend {
    fn probe() -> crate::ossandbox::Backend;
}

/// Sandbox backend of this build.
pub type Sandbox = Host;

#[cfg(unix)]
impl SandboxBackend for Host {
    fn probe() -> crate::ossandbox::Backend {
        probe_unix()
    }
}

/// Windows: the enhanced tier ([`crate::winsandbox_user`]: sandbox accounts, deny-read ACLs,
/// WFP) when `mare sandbox setup` installed it *and* its self-test passes in this process;
/// else the restricted-token + ACL default tier ([`crate::winsandbox`]) when the current token
/// can be restricted (it needs a logon SID); otherwise confined levels fail with
/// `SANDBOX_UNAVAILABLE` instead of running unconfined.
#[cfg(windows)]
impl SandboxBackend for Host {
    fn probe() -> crate::ossandbox::Backend {
        if crate::winsandbox::win::probe() {
            if matches!(crate::winsandbox_user::gate(), crate::winsandbox_user::Gate::Full(_)) {
                return crate::ossandbox::Backend::WinUser;
            }
            crate::ossandbox::Backend::WinAcl
        } else {
            crate::ossandbox::Backend::None
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod probe_lifecycle_tests {
    use super::*;

    #[test]
    fn process_exit_does_not_leave_protection_probe_files() {
        const CHILD: &str = "MARE_TEST_PROTECTION_PROBE_EXIT";
        if std::env::var_os(CHILD).is_some() {
            let (send, entered) = std::sync::mpsc::channel();
            let (release, released) = std::sync::mpsc::channel::<()>();
            *PROTECTION_PROBE_ENTERED.lock().unwrap() = Some(Box::new(move |path| {
                send.send(path.to_path_buf()).unwrap();
                released.recv().unwrap();
            }));
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
                let lua = mlua::Lua::new();
                let worker = crate::blocking::spawn(&lua, warm).unwrap();
                let path = tokio::task::spawn_blocking(move || entered.recv().unwrap()).await.unwrap();
                println!("PROTECTION_PROBE_DIRECTORY={}", path.display());
                let shutdown = crate::blocking::close_and_wait(&lua);
                tokio::pin!(shutdown);
                assert!(
                    futures_util::poll!(shutdown.as_mut()).is_pending(),
                    "shutdown must wait for the real active probe"
                );
                release.send(()).unwrap();
                shutdown.await;
                worker.await.unwrap();
                assert!(!path.exists(), "probe TempDir must be removed before owned shutdown returns");
            });
            std::process::exit(0);
        }
        if backend() != Backend::Bwrap {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ossandbox::backend::probe_lifecycle_tests::process_exit_does_not_leave_protection_probe_files",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("TMPDIR", root.path())
            .env("TEMP", root.path())
            .env("TMP", root.path())
            .output()
            .unwrap();
        assert!(result.status.success(), "probe child failed: {result:?}");
        let entries = std::fs::read_dir(root.path()).unwrap().map(|entry| entry.unwrap().path()).collect::<Vec<_>>();
        assert!(
            entries.is_empty(),
            "normal process exit left a live warm worker's real TempDir: {entries:?}; {result:?}"
        );
        root.close().unwrap();
    }
}
