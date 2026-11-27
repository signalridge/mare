//! Configuration and best-effort startup maintenance for the local recovery backend.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime},
};
use tokio::sync::{Mutex, OwnedRwLockReadGuard, RwLock};

#[derive(Clone, Default)]
pub(super) struct Scope {
    pub policy: Option<crate::ossandbox::Policy>,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
}
impl Scope {
    fn check(&self, path: &Path) -> io::Result<()> {
        if self.cancel.as_ref().is_some_and(|token| token.is_cancelled()) {
            return Err(io::Error::other("spill cleanup owner was canceled"));
        }
        if self.policy.as_ref().is_some_and(|policy| !policy.can_read(path) || !policy.can_write_deny_read(path)) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "spill cleanup denied by its originating policy",
            ));
        }
        Ok(())
    }
}

pub(super) struct Store {
    pub config: Config,
    closed: AtomicBool,
    started: AtomicBool,
    gate: Arc<RwLock<()>>,
    cleanup: Mutex<Option<tokio::task::JoinHandle<Sweep>>>,
}

impl Store {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config,
            closed: AtomicBool::new(false),
            started: AtomicBool::new(false),
            gate: Arc::new(RwLock::new(())),
            cleanup: Mutex::new(None),
        })
    }

    pub async fn start(&self, scope: Scope) {
        // Hold the task slot while publishing started, so dispose can never miss a
        // worker which has started but has not yet published its join handle.
        let mut task = self.cleanup.lock().await;
        if !self.started.swap(true, Ordering::AcqRel) && self.config.cleanup_period_days > 0 {
            let config = self.config.clone();
            *task = Some(tokio::task::spawn_blocking(move || {
                let report = sweep_scoped(&config, SystemTime::now(), &scope);
                for warning in &report.warnings {
                    tracing::warn!("{warning}");
                }
                report
            }));
        }
    }

    pub async fn lease(&self) -> mlua::Result<OwnedRwLockReadGuard<()>> {
        let lease = self.gate.clone().read_owned().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(super::rt_err("spill backend has been disposed"));
        }
        Ok(lease)
    }

    pub async fn dispose(&self) -> mlua::Result<Sweep> {
        self.closed.store(true, Ordering::Release);
        // New calls are refused; existing writes finish before disposal returns.
        let _exclusive = self.gate.write().await;
        let mut cleanup = self.cleanup.lock().await;
        if let Some(task) = cleanup.as_mut() {
            // Borrow the handle across await. If the disposer itself is canceled,
            // a retry still owns and joins the original worker.
            let report = task.await.map_err(super::rt_err)?;
            cleanup.take();
            Ok(report)
        } else {
            Ok(Sweep::default())
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct Config {
    pub root: PathBuf,
    pub cleanup_period_days: u32,
}

pub(crate) const DEFAULT_CLEANUP_DAYS: u32 = 30;

impl Config {
    pub fn resolve(root: Option<&Path>, cwd: &Path, days: Option<u32>) -> io::Result<Self> {
        if !cwd.is_absolute() {
            return Err(io::Error::other("spill cwd must be absolute"));
        }
        let raw = root.map_or_else(super::root, |p| if p.is_absolute() { p.to_owned() } else { cwd.join(p) });
        let mut resolved = PathBuf::new();
        for part in raw.components() {
            match part {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                other => resolved.push(other.as_os_str()),
            }
        }
        if resolved.parent().is_none() {
            return Err(io::Error::other("filesystem root cannot be spill storage"));
        }
        Ok(Self { root: resolved, cleanup_period_days: days.unwrap_or(DEFAULT_CLEANUP_DAYS) })
    }
}

/// Existing roots must be private; ancestors must not permit another user to replace them.
pub(super) fn trusted_directory(path: &Path, private: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::other("spill directory is not a plain directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        if private {
            if metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::other("spill directory must be private and user owned"));
            }
        } else if !safe_ancestor(&metadata, uid, Identity::lookup) {
            return Err(io::Error::other("spill directory has an unsafe ancestor"));
        }
    }
    #[cfg(windows)]
    if private {
        mare_shared::private::check(path).map_err(io::Error::other)?;
    }
    Ok(())
}

/// An ancestor is owned by the user or root, and nobody else can replace its entries: not
/// writable by others unless sticky, and group-writable only by the user's private group.
#[cfg(unix)]
fn safe_ancestor(
    metadata: &fs::Metadata,
    uid: libc::uid_t,
    identity: impl FnOnce(libc::uid_t, libc::gid_t) -> Option<Identity>,
) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let mode = metadata.permissions().mode();
    (metadata.uid() == uid || metadata.uid() == 0)
        && (mode & 0o1000 != 0
            || (mode & 0o002 == 0
                && (mode & 0o020 == 0 || identity(uid, metadata.gid()).is_some_and(|id| id.private(metadata.gid())))))
}

/// The user and a group, as the account database reports them.
#[cfg(unix)]
struct Identity {
    user_gid: libc::gid_t,
    user_name: std::ffi::CString,
    group_name: std::ffi::CString,
    members: Vec<std::ffi::CString>,
}

#[cfg(unix)]
impl Identity {
    /// Debian/Ubuntu default to user private groups with umask 002, so the user's own
    /// directories (`~/.local`, `$XDG_STATE_HOME`, ...) are group-writable by a group nobody
    /// else is in. Accepted: the user's primary group, named after the user, listing no other
    /// member (Debian OpenSSH `secure_permissions`, debian/patches/user-group-modes.patch,
    /// without its account enumeration, which NSS directories such as sssd do not answer
    /// completely). Another account given this group as its primary group is an administrator's
    /// misconfiguration, outside the trust boundary.
    fn private(&self, gid: libc::gid_t) -> bool {
        gid == self.user_gid && self.group_name == self.user_name && self.members.iter().all(|m| *m == self.user_name)
    }

    /// `None` when either lookup fails or does not fit the buffer: treated as shared.
    fn lookup(uid: libc::uid_t, gid: libc::gid_t) -> Option<Self> {
        use std::ffi::{CStr, CString};
        let mut buf = vec![0u8; 64 * 1024];
        // SAFETY: zeroed passwd/group are valid out-parameters; the reentrant lookups write
        // only into `buf`, and every string is copied out before `buf` is reused or dropped.
        unsafe {
            let mut pw: libc::passwd = std::mem::zeroed();
            let mut found = std::ptr::null_mut();
            if libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr().cast(), buf.len(), &mut found) != 0 || found.is_null() {
                return None;
            }
            let (user_gid, user_name) = (pw.pw_gid, CString::from(CStr::from_ptr(pw.pw_name)));
            let mut gr: libc::group = std::mem::zeroed();
            let mut found = std::ptr::null_mut();
            if libc::getgrgid_r(gid, &mut gr, buf.as_mut_ptr().cast(), buf.len(), &mut found) != 0 || found.is_null() {
                return None;
            }
            let mut members = Vec::new();
            let mut member = gr.gr_mem;
            while !member.is_null() && !(*member).is_null() {
                members.push(CString::from(CStr::from_ptr(*member)));
                member = member.add(1);
            }
            Some(Self { user_gid, user_name, group_name: CString::from(CStr::from_ptr(gr.gr_name)), members })
        }
    }
}

const MAX_SYMLINK_DEPTH: usize = 40;

#[derive(Clone)]
enum PathStep {
    Prefix(OsString),
    RootDir,
    ParentDir,
    Normal(OsString),
}

fn push_front_path(queue: &mut VecDeque<PathStep>, path: &Path) {
    let mut steps = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => steps.push(PathStep::Prefix(prefix.as_os_str().to_os_string())),
            Component::RootDir => steps.push(PathStep::RootDir),
            Component::CurDir => {}
            Component::ParentDir => steps.push(PathStep::ParentDir),
            Component::Normal(name) => steps.push(PathStep::Normal(name.to_os_string())),
        }
    }
    for step in steps.into_iter().rev() {
        queue.push_front(step);
    }
}

/// An ancestor symlink (strictly above the spill root itself) is safe on Unix only when owned by
/// the current user or root (e.g. macOS `/var -> private/var`, `/tmp -> private/tmp`) inside an
/// already-verified ancestor directory; its target path is then validated component-by-component.
fn trusted_ancestor_symlink(metadata: &fs::Metadata) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        if metadata.uid() != uid && metadata.uid() != 0 {
            return Err(io::Error::other("spill directory has an unsafe ancestor"));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(io::Error::other("spill directory is not a plain directory"))
    }
}

pub(super) fn trusted_root(root: &Path) -> io::Result<()> {
    trusted_directory(root, true)?;
    let Some(parent) = root.parent() else {
        return Err(io::Error::other("filesystem root cannot be spill storage"));
    };
    let Some(leaf) = root.file_name() else {
        return Err(io::Error::other("filesystem root cannot be spill storage"));
    };
    let mut queue = VecDeque::new();
    push_front_path(&mut queue, parent);
    let mut cur = PathBuf::new();
    let mut links = 0usize;
    while let Some(step) = queue.pop_front() {
        match step {
            PathStep::Prefix(prefix) => cur.push(prefix),
            PathStep::RootDir => {
                cur.push(std::path::MAIN_SEPARATOR_STR);
                trusted_directory(&cur, false)?;
            }
            PathStep::ParentDir => {
                cur.pop();
            }
            PathStep::Normal(name) => {
                cur.push(name);
                let metadata = fs::symlink_metadata(&cur)?;
                if metadata.file_type().is_symlink() {
                    trusted_ancestor_symlink(&metadata)?;
                    links += 1;
                    if links > MAX_SYMLINK_DEPTH {
                        return Err(io::Error::other("spill directory has too many symlink levels"));
                    }
                    let target = fs::read_link(&cur)?;
                    if target.is_absolute() {
                        cur.clear();
                    } else {
                        cur.pop();
                    }
                    push_front_path(&mut queue, &target);
                } else {
                    trusted_directory(&cur, false)?;
                }
            }
        }
    }
    trusted_directory(&cur.join(leaf), true)?;
    Ok(())
}

#[derive(Debug, Default)]
pub(super) struct Sweep {
    pub removed: usize,
    pub warnings: Vec<String>,
}

fn managed_owner(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn managed_file(path: &Path) -> bool {
    matches!(path.extension().and_then(|p| p.to_str()), Some("txt" | "bin"))
        && path.file_stem().and_then(|p| p.to_str()).is_some_and(super::valid_stem)
}

/// No write lock is held: a failed or slow scan never blocks a save. The live root and
/// owner directories are retained, including when temporarily empty during a save.
#[cfg(test)]
pub(super) fn sweep(config: &Config, now: SystemTime) -> Sweep {
    sweep_scoped(config, now, &Scope::default())
}

fn sweep_scoped(config: &Config, now: SystemTime, scope: &Scope) -> Sweep {
    let mut report = Sweep::default();
    if config.cleanup_period_days == 0 {
        return report;
    }
    let Some(cutoff) = now.checked_sub(Duration::from_secs(u64::from(config.cleanup_period_days) * 86400)) else {
        return report;
    };
    let mut run = || -> io::Result<()> {
        scope.check(&config.root)?;
        match trusted_root(&config.root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            other => other?,
        }
        for entry in fs::read_dir(&config.root)? {
            let entry = entry?;
            if !entry.file_name().to_str().is_some_and(managed_owner) {
                continue;
            }
            let owner = entry.path();
            // A hostile entry does not prevent checking other owners.
            if let Err(error) = trusted_directory(&owner, true) {
                report.warnings.push(format!("spill cleanup skipped {}: {error}", owner.display()));
                continue;
            }
            for file in fs::read_dir(&owner)? {
                let path = file?.path();
                if !managed_file(&path) {
                    continue;
                }
                let remove = || -> io::Result<bool> {
                    scope.check(&path)?;
                    trusted_root(&config.root)?;
                    trusted_directory(&owner, true)?;
                    let metadata = fs::symlink_metadata(&path)?;
                    if !metadata.is_file() || metadata.file_type().is_symlink() {
                        return Ok(false);
                    }
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::{MetadataExt, PermissionsExt};
                        // SAFETY: geteuid has no preconditions.
                        if metadata.uid() != unsafe { libc::geteuid() }
                            || metadata.permissions().mode() & 0o077 != 0
                            || metadata.nlink() != 1
                        {
                            return Ok(false);
                        }
                    }
                    if metadata.modified()? >= cutoff {
                        return Ok(false);
                    }
                    fs::remove_file(&path)?;
                    Ok(true)
                };
                match remove() {
                    Ok(true) => report.removed += 1,
                    Ok(false) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => report.warnings.push(format!("spill cleanup skipped {}: {error}", path.display())),
                }
            }
        }
        Ok(())
    };
    if let Err(error) = run() {
        report.warnings.push(format!("spill cleanup failed: {error}"));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canceled_origin_does_not_delete_expired_files() {
        let (_tmp, config, owner) = fixture();
        let expired = file(&owner, Duration::from_secs(40 * 86400));
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let scope = Scope { policy: None, cancel: Some(cancel) };
        let report = sweep_scoped(&config, SystemTime::now(), &scope);
        assert_eq!(report.removed, 0);
        assert!(!report.warnings.is_empty() && expired.exists());
    }

    #[test]
    fn cleanup_uses_a_frozen_origin_policy_instead_of_a_later_wider_policy() {
        let (_tmp, config, owner) = fixture();
        let expired = file(&owner, Duration::from_secs(40 * 86400));
        let lua = mlua::Lua::new();
        let spec = lua.create_table().unwrap();
        spec.set("level", "read-only").unwrap();
        spec.set("deny_read", vec![config.root.to_string_lossy().into_owned()]).unwrap();
        let mut policy =
            crate::ossandbox::Policy::from_lua(&lua, &mlua::Value::Table(spec), config.root.parent().unwrap())
                .unwrap()
                .unwrap();
        let scope = Scope { policy: Some(policy.clone()), cancel: None };
        policy.deny_read.clear();
        let report = sweep_scoped(&config, SystemTime::now(), &scope);
        assert_eq!(report.removed, 0);
        assert!(!report.warnings.is_empty() && expired.exists());
    }
    #[tokio::test]
    async fn canceled_dispose_can_be_retried_without_losing_the_cleanup_worker() {
        let (_tmp, config, _) = fixture();
        let store = Store::new(config);
        store.started.store(true, Ordering::Release);
        let (release, wait) = tokio::sync::oneshot::channel();
        *store.cleanup.lock().await = Some(tokio::spawn(async {
            wait.await.unwrap();
            Sweep { removed: 7, warnings: vec![] }
        }));
        let closing = store.clone();
        let dispose = tokio::spawn(async move { closing.dispose().await.map_err(|e| e.to_string()) });
        // Observe the disposer holding the cleanup slot across its pending join.
        while store.cleanup.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        dispose.abort();
        assert!(dispose.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        assert_eq!(store.dispose().await.unwrap().removed, 7);
    }
    #[tokio::test]
    async fn dispose_waits_for_operations_and_cleanup_then_refuses_cached_calls() {
        let (_tmp, config, owner) = fixture();
        let expired = file(&owner, Duration::from_secs(40 * 86400));
        let store = Store::new(config);
        store.start(Scope::default()).await;
        let lease = store.lease().await.unwrap();
        let closing = store.clone();
        let dispose = tokio::spawn(async move { closing.dispose().await.map_err(|e| e.to_string()) });
        tokio::task::yield_now().await;
        assert!(!dispose.is_finished());
        drop(lease);
        assert_eq!(dispose.await.unwrap().unwrap().removed, 1);
        assert!(!expired.exists());
        assert!(store.lease().await.is_err());
        assert_eq!(store.dispose().await.unwrap().removed, 0);
    }
    fn fixture() -> (tempfile::TempDir, Config, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let config = Config::resolve(Some(Path::new("results")), tmp.path(), None).unwrap();
        let owner = config.root.join("a".repeat(64));
        mare_shared::private::create_dir_all(&owner).unwrap();
        (tmp, config, owner)
    }
    fn file(owner: &Path, age: Duration) -> PathBuf {
        let path = owner.join(format!("{}.txt", uuid::Uuid::new_v4()));
        let f = mare_shared::private::create_new(&path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(SystemTime::now() - age)).unwrap();
        path
    }
    #[test]
    fn configured_root_and_disabled_cleanup() {
        let (_tmp, mut config, owner) = fixture();
        assert_eq!(config.cleanup_period_days, 30);
        assert!(Config::resolve(Some(Path::new("/")), Path::new("/home"), None).is_err());
        let expired = file(&owner, Duration::from_secs(40 * 86400));
        config.cleanup_period_days = 0;
        assert_eq!(sweep(&config, SystemTime::now()).removed, 0);
        assert!(expired.exists());
    }
    #[test]
    fn only_expired_managed_private_files_are_removed() {
        let (_tmp, config, owner) = fixture();
        let expired = file(&owner, Duration::from_secs(40 * 86400));
        let recent = file(&owner, Duration::ZERO);
        fs::write(owner.join("user-data.txt"), "untouched").unwrap();
        let report = sweep(&config, SystemTime::now());
        assert_eq!(report.removed, 1, "{:?}", report.warnings);
        assert!(!expired.exists());
        assert!(recent.exists() && owner.join("user-data.txt").exists() && owner.exists());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_symlink_and_unsafe_ancestor_without_touching_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (_tmp, config, owner) = fixture();
        let target = file(&owner, Duration::from_secs(40 * 86400));
        let link = owner.join(format!("{}.txt", uuid::Uuid::new_v4()));
        symlink(&target, &link).unwrap();
        fs::set_permissions(config.root.parent().unwrap(), fs::Permissions::from_mode(0o777)).unwrap();
        let report = sweep(&config, SystemTime::now());
        assert_eq!(report.removed, 0);
        assert!(!report.warnings.is_empty());
        assert!(target.exists());
    }

    /// Ref: Debian openssh `debian/patches/user-group-modes.patch` (`secure_permissions`): a
    /// group-writable directory is safe only when its group is private to its owner. The real
    /// ancestor check on a real directory, with the account database's answer controlled.
    #[cfg(unix)]
    #[test]
    fn group_writable_ancestor_needs_the_users_private_group() {
        use std::ffi::CString;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let check = |mode: u32, identity: Option<Identity>| {
            fs::set_permissions(tmp.path(), fs::Permissions::from_mode(mode)).unwrap();
            let metadata = fs::metadata(tmp.path()).unwrap();
            safe_ancestor(&metadata, metadata.uid(), |_, _| identity)
        };
        let gid = fs::metadata(tmp.path()).unwrap().gid();
        let name = |n: &str| CString::new(n).unwrap();
        let upg = || Identity { user_gid: gid, user_name: name("me"), group_name: name("me"), members: vec![] };
        // umask 002 under a user private group (an explicit self-membership is fine too)
        assert!(check(0o775, Some(upg())));
        assert!(check(0o775, Some(Identity { members: vec![name("me")], ..upg() })));
        // my primary group under another name: possibly a shared (directory) primary group
        assert!(!check(0o775, Some(Identity { group_name: name("staff"), ..upg() })));
        // another member listed
        assert!(!check(0o775, Some(Identity { members: vec![name("me"), name("bob")], ..upg() })));
        // not my primary group
        assert!(!check(0o775, Some(Identity { user_gid: gid.wrapping_add(1), ..upg() })));
        // the lookup failed
        assert!(!check(0o775, None));
        // world-writable stays refused whatever the group, unless sticky
        assert!(!check(0o757, Some(upg())));
        assert!(check(0o1777, None));
        assert!(check(0o755, None));
    }

    /// The same through the real account database: on a host whose primary group is a user
    /// private group (Debian/Ubuntu defaults), a 0775 ancestor of that group is trusted; without
    /// the relaxation it was refused. Hosts without one have nothing to check here.
    #[cfg(unix)]
    #[test]
    fn umask_002_ancestor_of_a_real_user_private_group_is_trusted() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let metadata = fs::metadata(tmp.path()).unwrap();
        let (uid, gid) = (metadata.uid(), metadata.gid());
        if !Identity::lookup(uid, gid).is_some_and(|id| id.private(gid)) {
            eprintln!("skipped: the test directory's group is not this user's private group");
            return;
        }
        fs::set_permissions(tmp.path(), fs::Permissions::from_mode(0o775)).unwrap();
        trusted_directory(tmp.path(), false).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn skips_symlinks_and_hardlinks_and_continues_after_bad_owner() {
        use std::os::unix::fs::symlink;
        let (_tmp, config, owner) = fixture();
        let shared = file(&owner, Duration::from_secs(40 * 86400));
        let hardlink = owner.join(format!("{}.txt", uuid::Uuid::new_v4()));
        fs::hard_link(&shared, &hardlink).unwrap();
        let link = owner.join(format!("{}.bin", uuid::Uuid::new_v4()));
        symlink(&shared, &link).unwrap();
        symlink(&owner, config.root.join("b".repeat(64))).unwrap();
        let expired = file(&owner, Duration::from_secs(40 * 86400));
        let report = sweep(&config, SystemTime::now());
        assert_eq!(report.removed, 1);
        assert_eq!(report.warnings.len(), 1);
        assert!(shared.exists() && hardlink.exists() && link.symlink_metadata().is_ok());
        assert!(!expired.exists());
    }

    #[test]
    fn concurrent_save_does_not_lose_its_directory_or_new_file() {
        let (_tmp, config, owner) = fixture();
        for _ in 0..20 {
            file(&owner, Duration::from_secs(40 * 86400));
        }
        let base = config.root.clone();
        let writer = std::thread::spawn(move || {
            (0..20).map(|_| super::super::save(&base, "a producer", b"current output").unwrap()).collect::<Vec<_>>()
        });
        let report = sweep(&config, SystemTime::now());
        let saved = writer.join().unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(saved.iter().all(|path| fs::read(path).unwrap() == b"current output"));
        assert!(owner.exists());
    }

    #[cfg(unix)]
    #[test]
    fn trusted_root_allows_safe_symlink_ancestor_and_rejects_unsafe_targets_or_symlink_root() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let real_base = tmp.path().join("real");
        let real_spill = real_base.join("state/spill-results");
        mare_shared::private::create_dir_all(&real_spill).unwrap();
        let link_ancestor = tmp.path().join("var-link");
        symlink(&real_base, &link_ancestor).unwrap();
        let via_link = link_ancestor.join("state/spill-results");

        // Owned absolute and relative symlink ancestors resolving through trusted directories
        // (matching macOS `/var -> private/var` and `/tmp -> private/tmp`).
        trusted_root(&via_link).unwrap();
        let saved = super::super::save(&via_link, "session-via-symlink", b"hello").unwrap();
        super::super::validate(&via_link, &saved).unwrap();

        let rel_link = tmp.path().join("rel-var");
        symlink("real", &rel_link).unwrap();
        let via_rel = rel_link.join("state/spill-results");
        trusted_root(&via_rel).unwrap();
        let saved_rel = super::super::save(&via_rel, "session-via-rel", b"hello-rel").unwrap();
        super::super::validate(&via_rel, &saved_rel).unwrap();

        // A symlink at the spill root itself remains refused.
        let link_root = tmp.path().join("root-link");
        symlink(&real_spill, &link_root).unwrap();
        assert!(trusted_root(&link_root).is_err());

        // A symlink ancestor whose target (or chained target) passes through an unsafe directory is refused.
        let unsafe_dir = tmp.path().join("world-writable");
        let unsafe_spill = unsafe_dir.join("spill-results");
        mare_shared::private::create_dir_all(&unsafe_spill).unwrap();
        let chained_safe_link = unsafe_dir.join("hop");
        symlink(&real_base, &chained_safe_link).unwrap();
        fs::set_permissions(&unsafe_dir, fs::Permissions::from_mode(0o777)).unwrap();
        let link_to_unsafe = tmp.path().join("to-unsafe");
        symlink(&unsafe_dir, &link_to_unsafe).unwrap();
        assert!(trusted_root(&link_to_unsafe.join("spill-results")).is_err());
        let chained_via_unsafe = tmp.path().join("via-unsafe-hop");
        symlink(&chained_safe_link, &chained_via_unsafe).unwrap();
        assert!(trusted_root(&chained_via_unsafe.join("state/spill-results")).is_err());
        fs::set_permissions(&unsafe_dir, fs::Permissions::from_mode(0o700)).unwrap();

        // A symlink loop in an ancestor fails cleanly instead of looping forever.
        let loop_a = tmp.path().join("loop-a");
        let loop_b = tmp.path().join("loop-b");
        symlink(&loop_b, &loop_a).unwrap();
        symlink(&loop_a, &loop_b).unwrap();
        assert!(trusted_root(&loop_a.join("spill-results")).is_err());
    }
}
