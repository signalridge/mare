//! Linux bubblewrap: a policy's arguments.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use super::level::Level;
use super::{Network, Policy};

pub(super) struct ProtectionPlan {
    pub binds: Vec<PathBuf>,
    pub masks: Vec<PathBuf>,
    pub grants: Vec<PathBuf>,
}

// Do not pass `--new-session`: PTY children run inside mare's own PTY, and detaching the
// controlling terminal breaks ^C (SIGINT), job control, and `/dev/tty`.
const BWRAP_BASE_NO_USERNS: &[&str] = &[
    "--ro-bind",
    "/",
    "/",
    "--dev",
    "/dev",
    "--unshare-pid",
    "--proc",
    "/proc",
    "--die-with-parent",
    "--unshare-ipc",
    "--cap-drop",
    "ALL",
];

const BWRAP_BASE_WITH_USERNS: &[&str] = &[
    "--ro-bind",
    "/",
    "/",
    "--dev",
    "/dev",
    "--unshare-pid",
    "--proc",
    "/proc",
    "--die-with-parent",
    "--unshare-ipc",
    "--cap-drop",
    "ALL",
    "--unshare-user",
];

/// Trusted absolute path to `bwrap`, probed once before any `PATH` mutation (e.g. askpass shims).
fn is_safe_exec(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Ok(meta) = std::fs::metadata(p) else { return false };
        if !meta.is_file() || (meta.mode() & 0o111) == 0 || (meta.mode() & 0o002) != 0 {
            return false;
        }
        if let Some(parent) = p.parent()
            && let Ok(pm) = std::fs::metadata(parent)
            && (pm.mode() & 0o002) != 0
        {
            return false;
        }
        true
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

pub(crate) fn bwrap_exec() -> &'static str {
    static PATH: OnceLock<String> = OnceLock::new();
    PATH.get_or_init(|| {
        const TRUSTED: &[&str] =
            &["/usr/bin/bwrap", "/bin/bwrap", "/usr/local/bin/bwrap", "/run/current-system/sw/bin/bwrap"];
        for cand in TRUSTED {
            let p = Path::new(cand);
            if is_safe_exec(p) {
                return (*cand).to_string();
            }
        }
        if let Ok(found) = which::which("bwrap")
            && found.is_absolute()
            && !found.to_string_lossy().contains("mare-askpass")
            && is_safe_exec(&found)
        {
            return found.to_string_lossy().into_owned();
        }
        "bwrap".to_string()
    })
}

/// Base bubblewrap flags, enabling `--unshare-user` unless unprivileged user namespaces are
/// kernel-blocked in this environment (fallback only when the probe fails).
pub(super) fn bwrap_base_args() -> &'static [&'static str] {
    static USE_USERNS: OnceLock<bool> = OnceLock::new();
    let ok = *USE_USERNS.get_or_init(|| {
        let mut args = BWRAP_BASE_WITH_USERNS.to_vec();
        args.extend(["--", "true"]);
        let supported = std::process::Command::new(bwrap_exec())
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !supported {
            tracing::warn!("bwrap --unshare-user unavailable in this environment; falling back without --unshare-user");
        }
        supported
    });
    if ok { BWRAP_BASE_WITH_USERNS } else { BWRAP_BASE_NO_USERNS }
}

impl Policy {
    /// bubblewrap arguments: host root read-only, fresh /dev, private PID
    /// namespace with its own /proc; workspace-write adds an ephemeral /tmp and binds the roots.
    pub fn bwrap_args(&self, cwd: &Path) -> Vec<String> {
        self.bwrap_args_with(cwd, None)
    }

    /// [`Self::bwrap_args`]; `bridge` = the paths the network-namespace bridge needs visible
    /// (read-only) when a filtered network runs through it (`super::netns`): the namespace is
    /// then unshared too, so the bridge is the only way out.
    pub fn bwrap_args_with(&self, cwd: &Path, bridge: Option<&[&Path]>) -> Vec<String> {
        self.bwrap_args_with_plan(cwd, bridge, None)
    }

    pub(super) fn bwrap_args_with_plan(
        &self,
        cwd: &Path,
        bridge: Option<&[&Path]>,
        plan: Option<&ProtectionPlan>,
    ) -> Vec<String> {
        let mut a: Vec<String> = bwrap_base_args().iter().map(|s| (*s).to_string()).collect();
        // restricted reads: an empty root with only the readable roots on it (crate::read_mask)
        let masked = crate::read_mask::visible(self).is_some_and(|v| crate::read_mask::bwrap_mask(&mut a, &v));
        let bind = |a: &mut Vec<String>, p: &Path| {
            let s = p.to_string_lossy().into_owned();
            a.extend(["--bind".into(), s.clone(), s]);
        };
        if self.level == Level::WorkspaceWrite {
            if !self.exclude_slash_tmp {
                a.extend(["--tmpfs".into(), "/tmp".into()]);
            }
            for r in self.workspace_roots() {
                bind(&mut a, &r);
            }
            let dynamic;
            let plan = match plan {
                Some(plan) => plan,
                None => {
                    dynamic = ProtectionPlan {
                        binds: self.protected_subpaths().into_iter().filter(|p| p.exists()).collect(),
                        masks: vec![],
                        grants: self.granted_in_protected(),
                    };
                    &dynamic
                }
            };
            for p in &plan.masks {
                let s = p.to_string_lossy().into_owned();
                a.extend(["--perms".into(), "555".into(), "--tmpfs".into(), s.clone(), "--remount-ro".into(), s]);
            }
            for p in &plan.binds {
                let s = p.to_string_lossy().into_owned();
                a.extend(["--ro-bind".into(), s.clone(), s]);
            }
            for g in &plan.grants {
                bind(&mut a, g);
            }
        }
        // after the /tmp tmpfs and the empty root's binds, before the seal: visible either way
        for p in bridge.unwrap_or_default() {
            let s = p.to_string_lossy().into_owned();
            a.extend(["--ro-bind".into(), s.clone(), s]);
        }
        // Lock down parent directories of `host_protected` inside any writable bind BEFORE
        // `deny_read` and `host_protected` masks so a confined process cannot rename a parent
        // directory (e.g. `MARE_HOME/state`) and replace it with a symlink.
        let writable_binds = bwrap_writable_binds(self, plan.map(|p| p.grants.as_slice()));
        let mut parent_ro_binds: Vec<PathBuf> = Vec::new();
        let mut host_entries: Vec<(super::protected::HostPath, PathBuf)> = Vec::new();
        for entry in super::protected::host_protected() {
            if entry.shape == super::protected::HostShape::Dir {
                if let Err(e) = mare_shared::private::create_dir_all(&entry.path) {
                    tracing::warn!(
                        path = %entry.path.display(),
                        error = %e,
                        "bwrap: cannot create protected host dir; skipping tmpfs mask"
                    );
                    continue;
                }
                match std::fs::symlink_metadata(&entry.path) {
                    Ok(m) if !m.file_type().is_symlink() && m.file_type().is_dir() => {}
                    Ok(_) => {
                        tracing::warn!(
                            path = %entry.path.display(),
                            "bwrap: protected host path is not a real directory; skipping tmpfs mask"
                        );
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %entry.path.display(),
                            error = %e,
                            "bwrap: cannot stat protected host dir; skipping tmpfs mask"
                        );
                        continue;
                    }
                }
            }
            let Ok(resolved) = crate::realpath::resolve(&entry.path) else { continue };
            if resolved.starts_with("/proc") || resolved.starts_with("/dev") {
                continue;
            }
            let aliases = match host_protection_aliases(self, &resolved, &writable_binds) {
                Ok(aliases) => aliases,
                Err(e) => {
                    tracing::warn!(
                        path = %resolved.display(),
                        error = %e,
                        "bwrap: cannot prove mount aliases for protected host path"
                    );
                    vec![resolved.clone()]
                }
            };
            for alias in aliases {
                if alias.starts_with("/proc") || alias.starts_with("/dev") {
                    continue;
                }
                if entry.shape == super::protected::HostShape::Dir {
                    for w in &writable_binds {
                        if has_symlink_ancestor_under(&entry.path, w) || has_symlink_ancestor_under(&alias, w) {
                            continue;
                        }
                        if let Some(lock_target) = parent_lock_target(&alias, w)
                            && !parent_ro_binds.contains(&lock_target)
                        {
                            parent_ro_binds.push(lock_target);
                        }
                    }
                }
                host_entries.push((entry.clone(), alias));
            }
        }
        for p in &parent_ro_binds {
            let s = p.to_string_lossy().into_owned();
            a.extend(["--ro-bind".into(), s.clone(), s]);
        }
        // Apply denies after every host bind and bridge mount. Also mask registered host secret
        // paths from `protected::host_protected()` unconditionally so confined commands cannot
        // read tokens even if generated after the sandbox starts.
        let mut covered: Vec<PathBuf> = Vec::new();
        let mut entries: Vec<_> = self.deny_read.iter().collect();
        entries.sort_by_key(|d| d.target.components().count());
        for d in entries {
            let p = d.target.as_path();
            if p.starts_with("/proc") || p.starts_with("/dev") || covered.iter().any(|parent| p.starts_with(parent)) {
                continue;
            }
            let s = p.to_string_lossy().into_owned();
            if d.is_dir == Some(true) {
                a.extend(["--perms".into(), "000".into(), "--tmpfs".into(), s.clone(), "--remount-ro".into(), s]);
                covered.push(p.to_path_buf());
            } else if d.is_dir == Some(false) {
                a.extend(["--ro-bind".into(), "/dev/null".into(), s]);
            }
        }
        for (entry, resolved) in host_entries {
            if covered.iter().any(|parent| resolved.starts_with(parent)) {
                continue;
            }
            let s = resolved.to_string_lossy().into_owned();
            match entry.shape {
                super::protected::HostShape::Dir if std::fs::metadata(&resolved).is_ok_and(|m| m.is_dir()) => {
                    a.extend(["--perms".into(), "000".into(), "--tmpfs".into(), s.clone(), "--remount-ro".into(), s]);
                    covered.push(resolved);
                }
                super::protected::HostShape::Dir | super::protected::HostShape::File if resolved.exists() => {
                    a.extend(["--ro-bind".into(), "/dev/null".into(), s]);
                }
                super::protected::HostShape::Dir | super::protected::HostShape::File => {}
            }
        }
        // Mask WSL2/WSLg host-bridge and duplicate distro root if present so confined commands
        // cannot invoke unconfined Windows binaries or bypass masked paths through them.
        for wsl_path in ["/run/WSL", "/mnt/wslg/distro"] {
            if Path::new(wsl_path).exists() {
                a.extend([
                    "--perms".into(),
                    "000".into(),
                    "--tmpfs".into(),
                    wsl_path.into(),
                    "--remount-ro".into(),
                    wsl_path.into(),
                ]);
            }
        }
        if masked {
            a.extend(crate::read_mask::BWRAP_SEAL.iter().map(|s| (*s).to_string()));
        }
        if bridge.is_some() || matches!(&self.network, Network::Proxy(n) if n.is_deny_all()) {
            a.push("--unshare-net".into());
        }
        a.extend(["--chdir".into(), cwd.to_string_lossy().into_owned()]);
        a
    }
}

fn bwrap_writable_binds(policy: &Policy, grants: Option<&[PathBuf]>) -> Vec<PathBuf> {
    let mut binds = Vec::new();
    if policy.level == Level::WorkspaceWrite {
        binds.extend(policy.workspace_roots());
        match grants {
            Some(g) => binds.extend(g.iter().cloned()),
            None => binds.extend(policy.granted_in_protected()),
        }
    }
    binds
}

fn host_protection_aliases(
    policy: &Policy,
    resolved: &Path,
    writable_binds: &[PathBuf],
) -> std::io::Result<Vec<PathBuf>> {
    #[cfg(target_os = "linux")]
    {
        let visible_roots = match crate::read_mask::visible(policy) {
            Some(mut roots) => {
                roots.extend(writable_binds.iter().cloned());
                roots.push(resolved.to_path_buf());
                roots
            }
            None => vec![PathBuf::from("/")],
        };
        super::mount_inspection::protection_aliases(&[resolved.to_path_buf()], &visible_roots)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (policy, writable_binds);
        Ok(vec![resolved.to_path_buf()])
    }
}

fn has_symlink_ancestor_under(path: &Path, root: &Path) -> bool {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::path::Component;

    let abs =
        if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir().unwrap_or_default().join(path) };
    let Ok(root_res) = crate::realpath::resolve(root) else { return true };
    let root_abs = crate::realpath::lexical(&if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(root)
    });

    let mut pending: VecDeque<OsString> = VecDeque::new();
    let mut out = PathBuf::new();
    let mut prefix_len = 0usize;
    for c in abs.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => {
                out.push(c.as_os_str());
                prefix_len = out.components().count();
            }
            other => pending.push_back(other.as_os_str().to_os_string()),
        }
    }

    let mut hops = 0usize;
    while let Some(name) = pending.pop_front() {
        if name == "." {
            continue;
        }
        if name == ".." {
            if out.components().count() > prefix_len {
                out.pop();
            }
            continue;
        }
        let cand = out.join(&name);
        let under_root =
            (cand != root_res && cand.starts_with(&root_res)) || (cand != root_abs && cand.starts_with(&root_abs));
        match std::fs::symlink_metadata(&cand) {
            Ok(m) if m.file_type().is_symlink() => {
                if under_root {
                    return true;
                }
                hops += 1;
                if hops > 40 {
                    out = cand;
                    continue;
                }
                let Ok(target) = std::fs::read_link(&cand) else {
                    out = cand;
                    continue;
                };
                let mut front: Vec<OsString> = Vec::new();
                if target.is_absolute() {
                    out = PathBuf::new();
                    prefix_len = 0;
                    for c in target.components() {
                        match c {
                            Component::Prefix(_) | Component::RootDir => {
                                out.push(c.as_os_str());
                                prefix_len = out.components().count();
                            }
                            other => front.push(other.as_os_str().to_os_string()),
                        }
                    }
                } else {
                    front.extend(target.components().map(|c| c.as_os_str().to_os_string()));
                }
                for f in front.into_iter().rev() {
                    pending.push_front(f);
                }
            }
            Ok(_) => out = cand,
            Err(_) => break,
        }
    }
    false
}

fn parent_lock_target(resolved_protected: &Path, writable_root: &Path) -> Option<PathBuf> {
    let w_res = crate::realpath::resolve(writable_root).ok()?;
    if !resolved_protected.starts_with(&w_res) || resolved_protected == w_res {
        return None;
    }
    let rel = resolved_protected.strip_prefix(&w_res).ok()?;
    let first = rel.components().next()?;
    let target = w_res.join(first);
    (target != resolved_protected).then_some(target)
}

/// Whether `args` contains an unconditional `--perms 000 --tmpfs <dir> --remount-ro <dir>` mask
/// covering every `Refuse` `Dir` entry in [`super::protected::host_protected`] (including all
/// visible bind-mount aliases, either directly or via an ancestor `deny_read` mask) AND locks down
/// any parent directory inside writable binds against rename/symlink redirection.
pub(super) fn args_mask_host_protected(policy: &Policy, args: &[String], grants: Option<&[PathBuf]>) -> bool {
    let masked_dirs: Vec<&Path> = args
        .windows(6)
        .filter_map(|w| {
            if w[0] == "--perms" && w[1] == "000" && w[2] == "--tmpfs" && w[4] == "--remount-ro" && w[3] == w[5] {
                Some(Path::new(w[3].as_str()))
            } else {
                None
            }
        })
        .collect();
    let ro_binds: Vec<&Path> = args
        .windows(3)
        .filter_map(|w| if w[0] == "--ro-bind" && w[1] == w[2] { Some(Path::new(w[1].as_str())) } else { None })
        .collect();
    let ro_dev_null_binds: Vec<&Path> = args
        .windows(3)
        .filter_map(|w| if w[0] == "--ro-bind" && w[1] == "/dev/null" { Some(Path::new(w[2].as_str())) } else { None })
        .collect();
    let writable_binds = bwrap_writable_binds(policy, grants);

    super::protected::host_protected()
        .into_iter()
        .filter(|e| e.on_gap == super::protected::GapAction::Refuse && e.shape == super::protected::HostShape::Dir)
        .all(|entry| {
            let Ok(resolved) = crate::realpath::resolve(&entry.path) else { return false };
            if resolved.starts_with("/proc") || resolved.starts_with("/dev") {
                return false;
            }
            if !std::fs::symlink_metadata(&entry.path)
                .is_ok_and(|m| !m.file_type().is_symlink() && m.file_type().is_dir())
            {
                return false;
            }
            if writable_binds.iter().any(|w| has_symlink_ancestor_under(&entry.path, w)) {
                return false;
            }
            let Ok(aliases) = host_protection_aliases(policy, &resolved, &writable_binds) else {
                return false;
            };
            aliases.iter().all(|alias| {
                if alias.starts_with("/proc") || alias.starts_with("/dev") {
                    return false;
                }
                let is_masked = if std::fs::metadata(alias).is_ok_and(|m| m.is_dir()) {
                    masked_dirs.iter().any(|m| alias.starts_with(m))
                } else {
                    ro_dev_null_binds.contains(&alias.as_path()) || masked_dirs.iter().any(|m| alias.starts_with(m))
                };
                if !is_masked {
                    return false;
                }
                writable_binds.iter().all(|w| {
                    if has_symlink_ancestor_under(alias, w) {
                        return false;
                    }
                    match parent_lock_target(alias, w) {
                        Some(lock_target) => {
                            masked_dirs.iter().any(|m| lock_target.starts_with(m))
                                || ro_binds.contains(&lock_target.as_path())
                        }
                        None => true,
                    }
                })
            })
        })
}

#[cfg(all(test, target_os = "linux"))]
mod bwrap_helper_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn is_safe_exec_rejects_world_writable_and_non_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("bin");
        std::fs::write(&f, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_safe_exec(&f), "non-executable file must be rejected");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!is_safe_exec(&f), "world-writable file must be rejected");
    }

    #[test]
    fn has_symlink_ancestor_under_handles_relative_absolute_dotdot_and_loops() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let real = root.join("real");
        std::fs::create_dir_all(real.join("sub")).unwrap();
        // Relative symlink inside root
        let rel_link = root.join("rel_link");
        std::os::unix::fs::symlink("real", &rel_link).unwrap();
        assert!(has_symlink_ancestor_under(&rel_link.join("sub/file"), &root));
        // Absolute symlink inside root
        let abs_link = root.join("abs_link");
        std::os::unix::fs::symlink(&real, &abs_link).unwrap();
        assert!(has_symlink_ancestor_under(&abs_link.join("sub/file"), &root));
        // `.` and `..` without symlinks under root -> false
        assert!(!has_symlink_ancestor_under(&real.join("./sub/../sub/file"), &root));
        // Symlink loop does not hang
        let loop_a = root.join("loop_a");
        let loop_b = root.join("loop_b");
        std::os::unix::fs::symlink(&loop_b, &loop_a).unwrap();
        std::os::unix::fs::symlink(&loop_a, &loop_b).unwrap();
        assert!(has_symlink_ancestor_under(&loop_a.join("child"), &root));
    }
}
