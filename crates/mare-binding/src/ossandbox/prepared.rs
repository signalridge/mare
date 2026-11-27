//! A command wrapped for its backend: capability matching, proxy, grants, the per-backend wrapper.

use std::path::{Path, PathBuf};

use mare_shared::lua::rt_err;
use mlua::{Lua, Table, Value};

use super::backend::backend;
use super::caps::Cap;
use super::classify::classify;
use super::level::Level;
use super::prexec::SocketBlock;
use super::seatbelt::SEATBELT_EXEC;
use super::{Backend, Denial, Network, Policy, SANDBOX_INSUFFICIENT, SANDBOX_UNAVAILABLE};
use crate::netproxy::ProxyHandle;

/// How a command is wrapped for its backend.
pub struct Wrapped {
    pub prog: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Landlock: the grants, applied in the child right before exec
    pub landlock: Option<crate::read_mask::LandlockGrants>,
    /// Linux: socket families blocked in the child (seccomp)
    pub socket_block: SocketBlock,
    #[cfg(target_os = "linux")]
    pub(crate) lifecycle_fds: Option<[std::os::fd::RawFd; 2]>,
}

#[cfg(all(test, target_os = "linux"))]
thread_local! {
    static BEFORE_PROTECTION_SNAPSHOT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static MONITOR_FAULT: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// A parsed policy plus the proxy serving it, owned by the spawned process.
pub struct Prepared {
    pub policy: Policy,
    pub proxy: Option<ProxyHandle>,
    pub cwd: PathBuf,
    pub backend: Backend,
    /// Windows: capability SIDs granted on the writable roots (the runner restricts to them)
    pub win_sids: Vec<String>,
    /// Windows enhanced tier: RAII guard revoking runtime `grant_group` ACEs on drop.
    #[cfg(windows)]
    _win_group_grants: Option<crate::winsandbox_user::win::GroupGrantGuard>,
    /// Missing `.mare` mount points owned by this prepared sandbox.
    #[cfg(target_os = "linux")]
    protected_mounts: std::sync::Mutex<Vec<std::sync::Arc<super::bwrap_guard::SyntheticMount>>>,
    #[cfg(target_os = "linux")]
    lifecycle_gate: std::sync::Mutex<Option<super::bwrap_lifecycle::Gate>>,
    #[cfg(target_os = "linux")]
    lifecycle_armed: std::sync::atomic::AtomicBool,
    /// A per-call protection failure, reported as partial.
    protection_failure: Option<String>,
    required_caps: Vec<Cap>,
    missing_caps: Vec<Cap>,
    bwrap_protection: super::bwrap::ProtectionPlan,
    /// Linux bubblewrap with a filtered network: the bridge from the command's loopback-only
    /// network namespace to `proxy`
    #[cfg(target_os = "linux")]
    pub netns: Option<super::netns::NetnsBridge>,
}

/// A prestarted lease owner. An unconfirmed handoff retains protection conservatively.
pub(crate) struct ProtectionMonitor {
    #[cfg(target_os = "linux")]
    tx: std::sync::mpsc::Sender<Option<u32>>,
    #[cfg(target_os = "linux")]
    ack: std::sync::mpsc::Receiver<Result<(), String>>,
    #[cfg(target_os = "linux")]
    done: tokio::sync::oneshot::Receiver<()>,
    #[cfg(all(test, target_os = "linux"))]
    fault: u8,
}

#[cfg(target_os = "linux")]
struct LeaseOwner {
    mounts: Option<Vec<std::sync::Arc<super::bwrap_guard::SyntheticMount>>>,
    gate: Option<super::bwrap_lifecycle::Gate>,
    safe_to_release: bool,
}

#[cfg(target_os = "linux")]
impl Drop for LeaseOwner {
    fn drop(&mut self) {
        if !self.safe_to_release {
            // Closing an unconfirmed gate can launch the command. Retain both it and the
            // locked leases on panic or a lost sender; process exit will close the descriptors.
            if let Some(gate) = self.gate.take() {
                std::mem::forget(gate);
            }
            if let Some(mounts) = self.mounts.take() {
                std::mem::forget(mounts);
            }
        }
    }
}

impl ProtectionMonitor {
    #[cfg(target_os = "linux")]
    fn start(
        mounts: Vec<std::sync::Arc<super::bwrap_guard::SyntheticMount>>,
        gate: Option<super::bwrap_lifecycle::Gate>,
    ) -> Result<Self, String> {
        #[cfg(test)]
        let fault = MONITOR_FAULT.with(|f| f.replace(0));
        #[cfg(not(test))]
        let fault = 0;
        if fault == 1 {
            return Err("injected monitor startup failure".into());
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (ack_tx, ack) = std::sync::mpsc::channel();
        let (done_tx, done) = tokio::sync::oneshot::channel();
        std::thread::Builder::new()
            .name("mare-protection-wait".into())
            .spawn(move || {
                let mut owner = LeaseOwner { mounts: Some(mounts), gate, safe_to_release: false };
                let pid = match rx.recv() {
                    Ok(Some(pid)) => pid,
                    Ok(None) => {
                        owner.safe_to_release = true;
                        return;
                    }
                    Err(_) => return,
                };
                let status = (|| {
                    if fault == 2 {
                        return Err("injected child registration failure".into());
                    }
                    for mount in owner.mounts.as_ref().unwrap() {
                        mount.mark_child(pid)?;
                    }
                    if let Some(gate) = &mut owner.gate {
                        gate.spawned();
                        let init = gate.bind(pid, std::time::Duration::from_secs(5)).map_err(|e| e.to_string())?;
                        gate.release().map_err(|e| e.to_string())?;
                        #[cfg(test)]
                        if fault == 5 {
                            panic!("injected protection monitor panic after release");
                        }
                        Ok(Some(init))
                    } else {
                        Ok(None)
                    }
                })();
                match status {
                    Ok(Some(init)) => {
                        let _ = ack_tx.send(Ok(()));
                        if fault == 3
                            || super::bwrap_lifecycle::wait(&init).is_err()
                            || super::mount_inspection::visible_proc().is_err()
                        {
                            // A failed pidfd observation is not proof of exit.
                            if let Ok(watch) = super::bwrap_guard::ProcessWatch::open(pid, fault == 3) {
                                watch.wait();
                            } else {
                                return;
                            }
                        }
                    }
                    Ok(None) => match super::bwrap_guard::ProcessWatch::open(pid, fault == 3) {
                        Ok(watch) => {
                            let _ = ack_tx.send(Ok(()));
                            watch.wait();
                        }
                        Err(e) => {
                            let _ = ack_tx.send(Err(e));
                            if !matches!(rx.recv(), Ok(None)) {
                                return;
                            }
                        }
                    },
                    Err(e) => {
                        let _ = ack_tx.send(Err(e));
                        // The caller owns the child and sends this only after kill + reap.
                        if !matches!(rx.recv(), Ok(None)) {
                            return;
                        }
                    }
                }
                owner.safe_to_release = true;
                drop(owner);
                let _ = done_tx.send(());
            })
            .map_err(|e| format!("sandbox: cannot start protection monitor: {e}"))?;
        Ok(Self {
            tx,
            ack,
            done,
            #[cfg(test)]
            fault,
        })
    }

    pub(crate) fn attach(&self, pid: Option<u32>) -> Result<(), String> {
        #[cfg(target_os = "linux")]
        {
            #[cfg(test)]
            if self.fault == 4 {
                return Err("injected missing child PID".into());
            }
            if pid.is_none() {
                return Err("sandbox: child PID unavailable".into());
            }
            self.tx.send(pid).map_err(|_| "sandbox: protection monitor ended".to_string())?;
            self.ack.recv().map_err(|_| "sandbox: protection monitor ended".to_string())?
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            Ok(())
        }
    }

    pub(crate) async fn finished(self) {
        #[cfg(target_os = "linux")]
        {
            let _ = self.done.await;
        }
    }

    pub(crate) fn after_kill(&self) {
        #[cfg(target_os = "linux")]
        {
            let _ = self.tx.send(None);
        }
    }
}

impl Prepared {
    /// Parse the `sandbox` option; `None` = run unconfined. Errors with `SANDBOX_UNAVAILABLE`
    /// when no backend can enforce the level, `SANDBOX_INVALID_ROOT` for a bad extra root.
    pub fn new(lua: &Lua, v: &Value, cwd: &Path) -> mlua::Result<Option<Self>> {
        let Some(mut policy) = Policy::from_lua(lua, v, cwd)? else { return Ok(None) };
        if let Network::Proxy(np) = &mut policy.network {
            np.tighten_fake_ip(crate::netauth::fake_ip_mode_from_lua(lua));
        }
        let approver = crate::netauth::approver(lua);
        Self::from_policy_with_approver(policy, cwd, approver).map(Some)
    }

    pub fn from_policy(policy: Policy, cwd: &Path) -> mlua::Result<Self> {
        Self::from_policy_with_approver(policy, cwd, None)
    }

    pub fn from_policy_with_approver(
        policy: Policy,
        cwd: &Path,
        approver: Option<crate::netauth::NetApprover>,
    ) -> mlua::Result<Self> {
        let backend = ensure_supported(&policy)?;
        let protected_paths = policy.protected_subpaths();
        policy.revalidate().map_err(rt_err)?;
        if backend == Backend::Bwrap {
            crate::read_mask::bwrap_check(&policy).map_err(rt_err)?;
        }
        let proxy = match &policy.network {
            Network::Proxy(np) => {
                let range = (backend == Backend::WinUser).then_some(crate::winsandbox_user::PROXY_PORT_RANGE);
                Some(
                    ProxyHandle::start_in_with_approver(np.clone(), range, approver)
                        .map_err(|e| rt_err(format!("sandbox proxy: {e}")))?,
                )
            }
            Network::Open => None,
        };
        // fail closed: when the bridge can't start the call is refused, never run unconfined
        #[cfg(target_os = "linux")]
        let netns = match (&proxy, &policy.network) {
            (Some(p), Network::Proxy(np))
                if backend == Backend::Bwrap && !np.is_deny_all() && backend.caps().contains(&Cap::NetProxy) =>
            {
                Some(super::netns::NetnsBridge::start(p.port()).map_err(|e| {
                    rt_err(format!(
                        "{SANDBOX_UNAVAILABLE}: the sandbox network bridge could not start ({e}); refusing to run \
                         the command with its network unconfined"
                    ))
                })?)
            }
            _ => None,
        };
        let win = matches!(backend, Backend::WinAcl | Backend::WinUser);
        let win_sids = if backend == Backend::WinUser && policy.level == Level::ReadOnly {
            vec![crate::winsandbox::readonly_capability_sid()]
        } else if win && policy.level == Level::WorkspaceWrite {
            let grants = if policy.exclude_tmpdir_env_var {
                crate::winsandbox::prepare_root_grants(&policy.workspace_roots())
            } else {
                let temp = crate::winsandbox::session_temp_dir()
                    .map_err(|e| rt_err(format!("{}{e}", crate::winsandbox::RUNNER_SIGNATURE)))?;
                crate::winsandbox::prepare_grants(&policy.workspace_roots(), &temp)
            };
            grants.map_err(|e| {
                rt_err(format!("{}granting the writable roots: {e}", crate::winsandbox::RUNNER_SIGNATURE))
            })?
        } else {
            vec![]
        };
        // enhanced tier: the sandbox accounts must also pass the normal access check.
        // Enforce `restrict_read` and `reads_pinned` instead of unconditionally granting `cwd`
        // or read access on newly granted writable roots, and revoke runtime ACEs on drop.
        #[cfg(windows)]
        let win_group_grants = if backend == Backend::WinUser
            && let crate::winsandbox_user::Gate::Full(s) = crate::winsandbox_user::gate()
        {
            let mut writable = if policy.level == Level::WorkspaceWrite { policy.workspace_roots() } else { vec![] };
            // a root containing %TEMP% already covers the private temp directory
            if policy.level == Level::WorkspaceWrite
                && !policy.exclude_tmpdir_env_var
                && let Ok(t) = crate::winsandbox::session_temp_dir()
                && crate::winsandbox::session_temp_grant(&writable) == Ok(crate::winsandbox::TempGrant::Isolated)
            {
                writable.push(t);
            }
            let (readable, writable_rw, writable_wo) = if policy.restrict_read {
                let readable = policy.read_roots().unwrap_or_default();
                if policy.reads_pinned {
                    let (rw, wo): (Vec<PathBuf>, Vec<PathBuf>) =
                        writable.into_iter().partition(|w| readable.iter().any(|r| crate::realpath::path_within(w, r)));
                    (readable, rw, wo)
                } else {
                    (readable, writable, vec![])
                }
            } else {
                let mut readable = policy.readable.clone();
                readable.push(cwd.to_path_buf());
                (readable, writable, vec![])
            };
            Some(
                crate::winsandbox_user::win::grant_group_scoped(&s.group_sid, &writable_rw, &writable_wo, &readable)
                    .map_err(|e| {
                        rt_err(format!("{}granting the sandbox accounts: {e}", crate::winsandbox::RUNNER_SIGNATURE))
                    })?,
            )
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let mut protection_failure = None;
        #[cfg(not(target_os = "linux"))]
        let protection_failure = None;
        #[cfg(target_os = "linux")]
        let protected_mounts = if backend == Backend::Bwrap
            && policy.level == Level::WorkspaceWrite
            && backend.caps().contains(&Cap::FsProtect)
        {
            match super::bwrap_guard::prepare_paths(&policy, &protected_paths) {
                Ok(mounts) => mounts.into_iter().map(std::sync::Arc::new).collect(),
                Err(e) => {
                    protection_failure = Some(e);
                    vec![]
                }
            }
        } else {
            vec![]
        };
        #[cfg(all(test, target_os = "linux"))]
        BEFORE_PROTECTION_SNAPSHOT.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        let grants: Vec<PathBuf> =
            policy.extra.iter().filter(|g| protected_paths.iter().any(|p| g.starts_with(p))).cloned().collect();
        #[cfg(target_os = "linux")]
        let masks: Vec<PathBuf> = protected_mounts
            .iter()
            .filter_map(|m: &std::sync::Arc<super::bwrap_guard::SyntheticMount>| {
                m.valid().then(|| m.synthetic_path().map(Path::to_path_buf)).flatten()
            })
            .collect();
        #[cfg(not(target_os = "linux"))]
        let masks: Vec<PathBuf> = vec![];
        let mut binds = Vec::new();
        let mut protection_gap = protection_failure.is_some();
        #[cfg(target_os = "linux")]
        {
            if backend == Backend::Bwrap && policy.level == Level::WorkspaceWrite {
                protection_gap |= !super::bwrap_guard::metadata_mounts_safe(&policy);
                if !protected_mounts.is_empty() {
                    protection_gap |= !super::bwrap_guard::registry_safe(&policy);
                }
            }
            protection_gap |= protected_mounts.iter().any(|m| !m.valid());
        }
        if backend == Backend::Bwrap && policy.level == Level::WorkspaceWrite {
            for path in &protected_paths {
                if grants.iter().any(|g| path.starts_with(g)) {
                    continue;
                }
                if masks.contains(path) {
                    continue;
                }
                match std::fs::symlink_metadata(path) {
                    Ok(m) if m.file_type().is_symlink() => {
                        protection_gap = true;
                    }
                    Ok(_) => binds.push(path.clone()),
                    Err(_) => {
                        protection_gap = true;
                    }
                }
            }
        }
        let mut required_caps = policy.required_for_paths(&protected_paths);
        let host_dir_ok = if backend == Backend::Bwrap {
            !super::protected::host_protected_unmaskable() && super::protected::ensure_host_protected_dirs().is_ok()
        } else {
            true
        };
        let available = backend.caps();
        let bwrap_protection = super::bwrap::ProtectionPlan { binds, masks, grants };
        #[cfg(target_os = "linux")]
        let bwrap_protection = if backend == Backend::Bwrap && policy.level == Level::WorkspaceWrite {
            let mut plan = bwrap_protection;
            let mut roots = policy.workspace_roots();
            roots.extend(super::roots::passthrough_dirs());
            // Share the original synthetic-target lease; an alias is another mount
            // destination, not another host resource to create or remove.
            for paths in [&mut plan.binds, &mut plan.masks, &mut plan.grants] {
                *paths = super::mount_inspection::protection_aliases(paths, &roots)
                    .map_err(|e| rt_err(format!("sandbox: cannot project protected mount aliases: {e}")))?;
            }
            plan
        } else {
            bwrap_protection
        };
        #[cfg(target_os = "linux")]
        let extra_binds: Vec<PathBuf> =
            netns.as_ref().map(|b| b.mounts().map(Path::to_path_buf).to_vec()).unwrap_or_default();
        #[cfg(not(target_os = "linux"))]
        let extra_binds: Vec<PathBuf> = Vec::new();
        let mask_in_args = policy.profile.is_none()
            && match backend {
                Backend::Bwrap => {
                    let mount_refs: Option<Vec<&Path>> =
                        (!extra_binds.is_empty()).then(|| extra_binds.iter().map(PathBuf::as_path).collect());
                    let args = policy.bwrap_args_with_plan(cwd, mount_refs.as_deref(), Some(&bwrap_protection));
                    host_dir_ok
                        && super::bwrap::args_mask_host_protected(&policy, &args, Some(&bwrap_protection.grants))
                }
                Backend::Seatbelt => super::seatbelt::profile_masks_host_protected(&policy.seatbelt(None)),
                Backend::WinUser => crate::winsandbox_user::host_protect_covered(),
                _ => false,
            };
        if backend == Backend::Bwrap && (!host_dir_ok || !mask_in_args) && !required_caps.contains(&Cap::FsHostProtect)
        {
            required_caps.push(Cap::FsHostProtect);
        }
        let host_protect_satisfied = policy.host_protect_satisfied(backend, &extra_binds, mask_in_args);
        let unix_socket_gap = matches!(backend, Backend::Bwrap | Backend::Landlock { .. })
            && matches!(policy.network, Network::Proxy(_))
            && !policy.unix_sockets.is_empty();
        let missing_caps: Vec<Cap> = required_caps
            .iter()
            .copied()
            .filter(|c| {
                if *c == Cap::FsHostProtect {
                    return !host_protect_satisfied;
                }
                if *c == Cap::UnixSocket && unix_socket_gap {
                    return true;
                }
                !available.contains(c) || (*c == Cap::FsProtect && (policy.profile.is_some() || protection_gap))
            })
            .collect();
        check_supported(&policy, backend, &missing_caps)?;
        #[cfg(target_os = "linux")]
        let lifecycle_gate = if !protected_mounts.is_empty() && super::bwrap_lifecycle::supported() {
            Some(super::bwrap_lifecycle::Gate::new().map_err(rt_err)?)
        } else {
            None
        };
        Ok(Self {
            policy,
            proxy,
            cwd: cwd.to_path_buf(),
            backend,
            win_sids,
            #[cfg(windows)]
            _win_group_grants: win_group_grants,
            protection_failure,
            required_caps,
            missing_caps,
            bwrap_protection,
            #[cfg(target_os = "linux")]
            protected_mounts: std::sync::Mutex::new(protected_mounts),
            #[cfg(target_os = "linux")]
            lifecycle_gate: std::sync::Mutex::new(lifecycle_gate),
            #[cfg(target_os = "linux")]
            lifecycle_armed: std::sync::atomic::AtomicBool::new(false),
            #[cfg(target_os = "linux")]
            netns,
        })
    }

    /// Start the lease owner before launching a child, so thread-creation failure is harmless.
    pub(crate) fn start_monitor(&self) -> mlua::Result<Option<ProtectionMonitor>> {
        #[cfg(target_os = "linux")]
        {
            let mounts = std::mem::take(&mut *self.protected_mounts.lock().unwrap());
            if mounts.is_empty() {
                return Ok(None);
            }
            let gate = self.lifecycle_gate.lock().unwrap().take();
            let gate = if self.lifecycle_armed.load(std::sync::atomic::Ordering::Acquire) { gate } else { None };
            ProtectionMonitor::start(mounts, gate).map(Some).map_err(rt_err)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Ok(None)
        }
    }

    /// Linux: the bridge's read-only mounts and the command line run through it, when the
    /// network goes through the namespace bridge.
    fn bridged(&self, cmd: &str, args: &[String]) -> Option<(Vec<PathBuf>, Vec<String>)> {
        #[cfg(target_os = "linux")]
        if let Some(b) = &self.netns {
            return Some((b.mounts().map(Path::to_path_buf).to_vec(), b.argv(cmd, args)));
        }
        let _ = (cmd, args);
        None
    }

    /// The command to run in place of `cmd args…`, plus extra environment.
    pub fn wrap(&self, cmd: &str, args: &[String]) -> Wrapped {
        self.wrap_with(cmd, args, None)
    }

    /// Like [`Prepared::wrap`]; `pty = Some((cols, rows))` asks the Windows runner to attach the
    /// command to a ConPTY and bridge it (Unix wrappers run inside the caller's PTY as they are).
    pub fn wrap_with(&self, cmd: &str, args: &[String], pty: Option<(u16, u16)>) -> Wrapped {
        self.wrap_inner(cmd, args, pty, false)
    }

    /// The host's spawn paths pass the inherited descriptors and attach a protection monitor.
    pub(crate) fn wrap_monitored(&self, cmd: &str, args: &[String]) -> Wrapped {
        self.wrap_inner(cmd, args, None, true)
    }

    fn wrap_inner(&self, cmd: &str, args: &[String], pty: Option<(u16, u16)>, monitored: bool) -> Wrapped {
        #[cfg(target_os = "linux")]
        if monitored {
            self.lifecycle_armed.store(true, std::sync::atomic::Ordering::Release);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = monitored;
        let env = self.proxy.as_ref().map(ProxyHandle::env).unwrap_or_default();
        let restricted_net = matches!(self.policy.network, Network::Proxy(_));
        let blocked_net = matches!(&self.policy.network, Network::Proxy(n) if n.is_deny_all());
        match self.backend {
            Backend::Bwrap => {
                let bridged = self.bridged(cmd, args);
                let via_bridge = bridged.is_some();
                let mounts: Option<Vec<&Path>> =
                    bridged.as_ref().map(|(m, _)| m.iter().map(PathBuf::as_path).collect());
                let mut a =
                    self.policy.bwrap_args_with_plan(&self.cwd, mounts.as_deref(), Some(&self.bwrap_protection));
                #[cfg(target_os = "linux")]
                let lifecycle_fds = if monitored {
                    self.lifecycle_gate.lock().unwrap().as_ref().map(|gate| gate.inherited())
                } else {
                    None
                };
                #[cfg(target_os = "linux")]
                if let Some([info, block]) = lifecycle_fds {
                    a.extend(["--info-fd".into(), info.to_string(), "--block-fd".into(), block.to_string()]);
                }
                a.push("--".into());
                match bridged {
                    Some((_, argv)) => a.extend(argv),
                    None => {
                        a.push(cmd.into());
                        a.extend(args.iter().cloned());
                    }
                }
                Wrapped {
                    prog: super::bwrap::bwrap_exec().into(),
                    args: a,
                    env,
                    landlock: None,
                    // the network namespace handles inet; Unix sockets on the (read-only) host
                    // filesystem are still reachable without seccomp. Through the bridge, its
                    // inner half installs the AF_UNIX filter itself right before exec'ing the
                    // command (the bridge must still reach the host's socket)
                    socket_block: SocketBlock {
                        unix: restricted_net && !via_bridge,
                        inet: false,
                        netlink_route: restricted_net && !via_bridge && blocked_net,
                    },
                    #[cfg(target_os = "linux")]
                    lifecycle_fds,
                }
            }
            Backend::Landlock { .. } => Wrapped {
                prog: cmd.into(),
                args: args.to_vec(),
                env,
                landlock: Some(self.policy.landlock_grants()),
                socket_block: SocketBlock { unix: restricted_net, inet: blocked_net, netlink_route: false },
                #[cfg(target_os = "linux")]
                lifecycle_fds: None,
            },
            Backend::WinAcl | Backend::WinUser => {
                let mut env = env;
                if let Ok(t) = crate::winsandbox::session_temp_dir() {
                    let t = t.to_string_lossy().into_owned();
                    env.push(("TEMP".into(), t.clone()));
                    env.push(("TMP".into(), t));
                }
                let mut argv = vec![cmd.to_string()];
                argv.extend(args.iter().cloned());
                let ra = crate::winsandbox::RunnerArgs {
                    mode: if self.policy.level == Level::ReadOnly { "read-only" } else { "workspace-write" }.into(),
                    sids: self.win_sids.clone(),
                    cwd: self.cwd.clone(),
                    argv,
                    as_user: (self.backend == Backend::WinUser).then(|| {
                        if matches!(self.policy.network, Network::Open) { "online" } else { "offline" }.into()
                    }),
                    // the outer runner hands the account SID to the inner one (winsandbox_user)
                    account: None,
                    pty,
                };
                let mut a = vec![crate::winsandbox::RUNNER_ARG.to_string()];
                a.extend(ra.to_args());
                Wrapped {
                    prog: crate::winsandbox::helper_exe().to_string_lossy().into_owned(),
                    args: a,
                    env,
                    landlock: None,
                    socket_block: SocketBlock::default(),
                    #[cfg(target_os = "linux")]
                    lifecycle_fds: None,
                }
            }
            // ensure_supported refused Backend::None; seatbelt is the remaining case
            Backend::Seatbelt | Backend::None => {
                let profile = self.policy.seatbelt(self.proxy.as_ref().map(ProxyHandle::port));
                let mut a = vec!["-p".to_string(), profile, "--".to_string(), cmd.to_string()];
                a.extend(args.iter().cloned());
                Wrapped {
                    prog: SEATBELT_EXEC.into(),
                    args: a,
                    env,
                    landlock: None,
                    socket_block: SocketBlock::default(),
                    #[cfg(target_os = "linux")]
                    lifecycle_fds: None,
                }
            }
        }
    }

    pub fn report(&self, lua: &Lua) -> mlua::Result<Table> {
        let report = self.policy.report_with_caps(
            lua,
            self.proxy.as_ref(),
            self.backend,
            &self.required_caps,
            &self.missing_caps,
        )?;
        if let Some(reason) = &self.protection_failure {
            let enforced = report.get::<Table>("enforced")?;
            let filtered = lua.create_table()?;
            for cap in enforced.sequence_values::<String>() {
                let cap = cap?;
                if cap != Cap::FsProtect.name() {
                    filtered.push(cap)?;
                }
            }
            let unavailable = report.get::<Table>("unavailable")?;
            if !unavailable.clone().sequence_values::<String>().any(|v| v.is_ok_and(|v| v == Cap::FsProtect.name())) {
                unavailable.push(Cap::FsProtect.name())?;
            }
            report.set("enforced", filtered)?;
            report.set("accepted_partial", unavailable.clone())?;
            report.set("enforcement", "partial")?;
            report.set("protected_paths", reason.clone())?;
        }
        Ok(report)
    }

    /// The sandbox itself failed to start (as opposed to denying the command): the runner's own
    /// diagnostic on stderr with a non-zero exit.
    pub fn start_failure(&self, code: Option<i32>, stderr: &str) -> Option<String> {
        if matches!(code, None | Some(0)) {
            return None;
        }
        let runner = code == Some(crate::winsandbox::RUNNER_FAILURE_EXIT);
        let sigs: &[&str] = match self.backend {
            Backend::Seatbelt => &["sandbox-exec: "],
            // bubblewrap itself, or the network bridge's inner half (super::netns_inner)
            Backend::Bwrap if runner => &["bwrap: ", crate::winsandbox::RUNNER_SIGNATURE],
            Backend::Bwrap => &["bwrap: "],
            Backend::WinAcl | Backend::WinUser if runner => &[crate::winsandbox::RUNNER_SIGNATURE],
            _ => return None,
        };
        stderr
            .lines()
            .find(|l| {
                let l = l.to_ascii_lowercase();
                sigs.iter().any(|s| l.starts_with(s))
            })
            .map(str::to_string)
    }

    pub fn classify(&self, output: &str) -> Option<Denial> {
        let host = self.proxy.as_ref().and_then(|p| p.denied().into_iter().next()).map(|(h, _)| h);
        classify(&self.policy, &self.cwd, output, host, self.backend)
    }
}

/// Match `policy` against the usable backend: no backend →
/// `SANDBOX_UNAVAILABLE`; a required dimension the backend can't enforce and the user didn't
/// accept → `SANDBOX_INSUFFICIENT` naming it. Never silently runs with weaker constraints.
pub fn ensure_supported(policy: &Policy) -> mlua::Result<Backend> {
    let b = backend();
    if b == Backend::None {
        return Err(rt_err(format!(
            "{SANDBOX_UNAVAILABLE}: a confined sandbox level is requested but no sandbox backend is usable on this \
             host; refusing to run the command unconfined. Install bubblewrap or use a Landlock-enabled kernel \
             (Linux), make sure sandbox-exec works (macOS), or switch to danger-full-access{}",
            if cfg!(windows) { " (Windows: the restricted-token backend could not be used on this host)" } else { "" }
        )));
    }
    let missing = policy.missing(b);
    check_supported(policy, b, &missing)?;
    Ok(b)
}

fn check_supported(policy: &Policy, b: Backend, missing: &[Cap]) -> mlua::Result<()> {
    let refused: Vec<Cap> = missing.iter().copied().filter(|c| !policy.accept_partial.contains(c)).collect();
    if policy.reads_pinned && refused.contains(&Cap::FsRead) {
        return Err(rt_err(crate::deny_read::refusal(b)));
    }
    if !refused.is_empty() {
        let dims = refused.iter().map(|c| format!("{} ({})", c.name(), c.why())).collect::<Vec<_>>().join(", ");
        let detail = if refused.contains(&Cap::FsDenyRead) {
            format!(
                "; {}{}",
                super::deny_path::gap_reason(b, policy.profile.is_some()),
                if policy.deny_read.iter().any(|d| d.is_dir.is_none()) {
                    "; unresolved entries are not masked"
                } else {
                    ""
                }
            )
        } else if refused.contains(&Cap::FsHostProtect) {
            "; delete the host token dir (<state>/host/) if unused, or accept fs_host_protect \
             (note: full-access, disabled permissions, approved unconfined escalations, custom static tokens, \
             raw Seatbelt profiles, and pre-existing processes on partial backends are not protected)"
                .to_string()
        } else if refused.contains(&Cap::UnixSocket) && !policy.unix_sockets.is_empty() {
            "; Linux seccomp blocks all AF_UNIX sockets under a restricted network and cannot allowlist unix_sockets"
                .to_string()
        } else {
            String::new()
        };
        return Err(rt_err(format!(
            "{SANDBOX_INSUFFICIENT}: the {} sandbox can't enforce what this call requires: {dims}{detail}; refusing to run \
             with weaker constraints. To run anyway with these left unenforced, accept them explicitly: \
             mare.plugin(\"permissions\", {{ accept_partial = {{ {} }} }})",
            b.name(),
            refused.iter().map(|c| format!("\"{}\"", c.name())).collect::<Vec<_>>().join(", ")
        )));
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod snapshot_tests {
    use super::*;
    use mlua::Value;

    #[test]
    fn registry_failure_is_checked_only_when_synthetic_protection_is_needed() {
        if backend() != Backend::Bwrap {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        super::super::bwrap_guard::FAIL_REGISTRY.with(|f| f.set(true));
        let readonly = Policy::build(Level::ReadOnly, dir.path(), None, &Value::Boolean(true)).unwrap();
        assert!(Prepared::from_policy(readonly, dir.path()).is_ok(), "read-only does not need the registry");
        let workspace = Policy::build(Level::WorkspaceWrite, dir.path(), None, &Value::Boolean(true)).unwrap();
        assert!(
            Prepared::from_policy(workspace.clone(), dir.path()).err().unwrap().to_string().contains("fs_protect"),
            "a direct policy without an accepted capability remains strict"
        );
        let mut workspace = workspace;
        workspace.accept_partial.push(Cap::FsProtect);
        let prepared = Prepared::from_policy(workspace, dir.path()).unwrap();
        let lua = Lua::new();
        assert_eq!(prepared.report(&lua).unwrap().get::<String>("enforcement").unwrap(), "partial");
        assert!(prepared.bwrap_protection.masks.is_empty());
        std::fs::create_dir(dir.path().join(".mare")).unwrap();
        let existing = Policy::build(Level::WorkspaceWrite, dir.path(), None, &Value::Boolean(true)).unwrap();
        let existing = Prepared::from_policy(existing, dir.path()).expect("existing paths need no synthetic registry");
        assert_eq!(existing.report(&lua).unwrap().get::<String>("enforcement").unwrap(), "full");
        super::super::bwrap_guard::FAIL_REGISTRY.with(|f| f.set(false));
    }

    #[test]
    fn unavailable_lifecycle_inspection_reports_partial() {
        if backend() != Backend::Bwrap {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy::build(Level::WorkspaceWrite, dir.path(), None, &Value::Boolean(true)).unwrap();
        super::super::bwrap_guard::FAIL_PROC_INSPECTION.with(|f| f.set(true));
        assert!(policy.missing(Backend::Bwrap).contains(&Cap::FsProtect), "status must report the lifecycle gap");
        assert!(
            Prepared::from_policy(policy.clone(), dir.path()).err().unwrap().to_string().contains("fs_protect"),
            "a direct policy without an accepted capability remains strict"
        );
        let mut accepted = policy;
        accepted.accept_partial.push(Cap::FsProtect);
        let prepared = Prepared::from_policy(accepted, dir.path()).unwrap();
        let lua = Lua::new();
        assert_eq!(prepared.report(&lua).unwrap().get::<String>("enforcement").unwrap(), "partial");
        super::super::bwrap_guard::FAIL_PROC_INSPECTION.with(|f| f.set(false));
    }

    /// Ceiling for an "eventually" wait: it only catches a hang. Release latency is not what
    /// these tests check, and on a loaded host one legacy /proc group observation alone can take
    /// seconds (bwrap_guard/mod.rs `group_has_live_processes`).
    const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(60);

    #[tokio::test]
    async fn monitor_faults_do_not_leak_or_leave_a_running_child() {
        use crate::platform::{self, ProcessBackend};
        if backend() != Backend::Bwrap {
            return;
        }
        let mut workspaces = Vec::new();
        for fault in [1, 2, 3, 4] {
            let dir = tempfile::tempdir().unwrap();
            let policy = Policy::build(Level::WorkspaceWrite, dir.path(), None, &Value::Boolean(true)).unwrap();
            let prepared = Prepared::from_policy(policy, dir.path()).unwrap();
            let wrapped = prepared.wrap_monitored("sh", &["-c".into(), "sleep 1; touch continued".into()]);
            MONITOR_FAULT.with(|f| f.set(fault));
            let started = prepared.start_monitor();
            if fault == 1 {
                assert!(started.is_err());
            } else {
                let monitor = started.unwrap().unwrap();
                let mut cmd = tokio::process::Command::new(wrapped.prog);
                cmd.args(wrapped.args).kill_on_drop(true);
                if let Some(fds) = wrapped.lifecycle_fds {
                    unsafe {
                        cmd.pre_exec(move || super::super::bwrap_lifecycle::inherit(fds));
                    }
                }
                <platform::Process as ProcessBackend>::isolate(&mut cmd);
                let mut child = platform::spawn(&mut cmd).unwrap();
                let result = crate::proc::attach_monitor(Some(monitor), &mut child);
                if fault == 3 {
                    let monitor = result.expect("old-kernel /proc fallback must work").expect("monitored call");
                    crate::proc::kill_group(child.id(), crate::platform::Signal::Kill);
                    child.start_kill().unwrap();
                    child.wait().await.unwrap();
                    // `finished` fires only after the lease owner dropped the leases: an event, not
                    // a clock (the release itself waits for a clean /proc group observation).
                    tokio::time::timeout(HANG_GUARD, monitor.finished())
                        .await
                        .expect("fault 3: monitor never finished");
                } else {
                    assert!(result.is_err(), "monitor attach fault must be reported");
                    assert!(child.try_wait().unwrap().is_some(), "failed child must be reaped before return");
                }
            }
            // Faults 2 and 4: the owner releases on its own thread after `after_kill`.
            let deadline = tokio::time::Instant::now() + HANG_GUARD;
            while dir.path().join(".mare").exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(!dir.path().join(".mare").exists(), "fault {fault} leaked a target");
            workspaces.push((fault, dir));
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        for (fault, dir) in workspaces {
            assert!(!dir.path().join("continued").exists(), "fault {fault} let the child continue after return");
        }
    }

    #[tokio::test]
    async fn monitor_panic_retains_the_target_after_a_child_is_started() {
        use crate::platform::{self, ProcessBackend};
        if backend() != Backend::Bwrap || !super::super::bwrap_lifecycle::supported() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy::build(Level::WorkspaceWrite, dir.path(), None, &Value::Boolean(true)).unwrap();
        let prepared = Prepared::from_policy(policy, dir.path()).unwrap();
        let wrapped = prepared.wrap_monitored("sh", &["-c".into(), "sleep 30".into()]);
        MONITOR_FAULT.with(|f| f.set(5));
        let monitor = prepared.start_monitor().unwrap().unwrap();
        let mut cmd = tokio::process::Command::new(wrapped.prog);
        cmd.args(wrapped.args).kill_on_drop(true);
        if let Some(fds) = wrapped.lifecycle_fds {
            unsafe {
                cmd.pre_exec(move || super::super::bwrap_lifecycle::inherit(fds));
            }
        }
        <platform::Process as ProcessBackend>::isolate(&mut cmd);
        let mut child = platform::spawn(&mut cmd).unwrap();
        assert!(crate::proc::attach_monitor(Some(monitor), &mut child).is_err());
        assert!(child.try_wait().unwrap().is_some(), "failed child must be reaped");
        drop(prepared);
        assert!(dir.path().join(".mare").is_dir(), "unknown monitor state must retain the target");
    }

    #[tokio::test]
    async fn monitor_releases_the_target_when_spawn_never_started() {
        if backend() != Backend::Bwrap {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let policy = Policy::build(Level::WorkspaceWrite, dir.path(), None, &Value::Boolean(true)).unwrap();
        let prepared = Prepared::from_policy(policy, dir.path()).unwrap();
        let _wrapped = prepared.wrap_monitored("true", &[]);
        let monitor = prepared.start_monitor().unwrap().unwrap();
        monitor.after_kill();
        tokio::time::timeout(std::time::Duration::from_secs(2), monitor.finished()).await.unwrap();
        assert!(!dir.path().join(".mare").exists(), "a failed spawn must release its target");
    }

    #[test]
    fn changed_mount_target_is_rechecked_and_reported_consistently() {
        if backend() != Backend::Bwrap {
            return;
        }
        for accepted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(".mare");
            let old = dir.path().join("old-target");
            let destination = dir.path().join("destination");
            std::fs::create_dir(&destination).unwrap();
            let mut policy = Policy::build(Level::WorkspaceWrite, dir.path(), None, &Value::Boolean(true)).unwrap();
            if accepted {
                policy.accept_partial.push(Cap::FsProtect);
            }
            BEFORE_PROTECTION_SNAPSHOT.with(|hook| {
                *hook.borrow_mut() = Some(Box::new(move || {
                    std::fs::rename(&path, &old).unwrap();
                    std::os::unix::fs::symlink(&destination, &path).unwrap();
                }));
            });
            let result = Prepared::from_policy(policy, dir.path());
            if !accepted {
                assert!(result.err().unwrap().to_string().contains("fs_protect"));
                continue;
            }
            {
                let prepared = result.unwrap();
                let lua = Lua::new();
                let report = prepared.report(&lua).unwrap();
                assert_eq!(report.get::<String>("enforcement").unwrap(), "partial");
                let missing = report.get::<Table>("unavailable").unwrap();
                assert!(missing.sequence_values::<String>().any(|v| v.unwrap() == "fs_protect"));
                assert!(prepared.bwrap_protection.masks.is_empty());
                let wrapped = prepared.wrap_monitored("sh", &["-c".into(), "printf ok > ordinary".into()]);
                let monitor = prepared.start_monitor().unwrap().unwrap();
                let mut command = std::process::Command::new(wrapped.prog);
                command.args(wrapped.args);
                if let Some(fds) = wrapped.lifecycle_fds {
                    use std::os::unix::process::CommandExt;
                    unsafe {
                        command.pre_exec(move || super::super::bwrap_lifecycle::inherit(fds));
                    }
                }
                let mut child = command.spawn().unwrap();
                monitor.attach(Some(child.id())).unwrap();
                assert!(child.wait().unwrap().success());
                assert_eq!(std::fs::read_to_string(dir.path().join("ordinary")).unwrap(), "ok");
                assert_eq!(prepared.report(&lua).unwrap().get::<String>("enforcement").unwrap(), "partial");
            }
        }
    }
}
