//! Coordinated temporary mount targets for missing protected metadata.
//!
//! The registry is outside sandbox-writable roots. Each call holds a locked lease until its
//! child exits; a target is synthetic only while a live lease and its recorded inode agree.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::Policy;

fn registry() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = TEST_REGISTRY.with(|value| value.borrow().clone()) {
        return Ok(path);
    }
    let tmp = dunce::canonicalize("/tmp").map_err(|e| format!("sandbox: cannot resolve protection registry: {e}"))?;
    Ok(tmp.join(format!("mare-protected-{}", unsafe { libc::geteuid() })))
}

fn writable_roots(policy: &Policy) -> Vec<PathBuf> {
    policy.workspace_roots()
}

pub(super) fn metadata_mounts_safe(policy: &Policy) -> bool {
    super::mount_inspection::writable_mounts_safe(&writable_roots(policy), None).unwrap_or(false)
}

pub(super) fn registry_safe(policy: &Policy) -> bool {
    let Ok(dir) = registry() else { return false };
    let roots = writable_roots(policy);
    if roots.iter().any(|root| dir.starts_with(root)) {
        return false;
    }
    super::mount_inspection::writable_mounts_safe(&roots, Some(&dir)).unwrap_or(false)
}

pub(super) fn registry_usable(policy: &Policy) -> bool {
    if !registry_safe(policy) {
        return false;
    }
    let Ok(dir) = registry() else { return false };
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false,
        Ok(meta) if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 => {
            return false;
        }
        Ok(_) => (),
    }
    match std::fs::symlink_metadata(dir.join("lock")) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
        Ok(meta) => safe_file_metadata(&meta),
    }
}

#[cfg(test)]
pub(super) type PidAliveHook = std::sync::Arc<dyn Fn(u32) -> std::io::Result<bool> + Send + Sync>;

#[cfg(test)]
thread_local! {
    static AFTER_MARKER_CHECK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = std::cell::RefCell::new(None);
    static FORCE_SCAN_ENOENT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static TEST_REGISTRY: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    pub(super) static FAIL_REGISTRY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(super) static FAIL_PROC_INSPECTION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Scan this directory instead of /proc (a fixture tree), for deterministic retry checks.
    pub(super) static TEST_PROC_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    /// Override PID liveness checks in scan_process_group for deterministic ESRCH/live/EPERM tests.
    pub(super) static TEST_PID_ALIVE: std::cell::RefCell<Option<PidAliveHook>> =
        const { std::cell::RefCell::new(None) };
}

fn safe_file_metadata(meta: &std::fs::Metadata) -> bool {
    meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.nlink() == 1 && meta.mode() & 0o077 == 0
}

fn validate_file(file: &File) -> Result<(), String> {
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if safe_file_metadata(&meta) { Ok(()) } else { Err("sandbox: unsafe protection registry file".into()) }
}

fn read_identity(dir: &Path) -> Result<String, String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("identity"))
        .map_err(|e| e.to_string())?;
    validate_file(&file)?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(|e| e.to_string())?;
    Ok(text)
}

fn write_identity(dir: &Path, text: &str) -> Result<(), String> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("identity"))
        .map_err(|e| e.to_string())?;
    validate_file(&file)?;
    file.set_len(0).map_err(|e| e.to_string())?;
    file.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

struct RegistryLock(File);

impl RegistryLock {
    fn acquire() -> Result<Self, String> {
        #[cfg(test)]
        if FAIL_REGISTRY.with(|flag| flag.get()) {
            return Err("injected registry failure".into());
        }
        let dir = registry()?;
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(format!("sandbox: cannot create protection registry: {e}")),
        }
        let meta = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
            return Err("sandbox: unsafe protection registry".into());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(dir.join("lock"))
            .map_err(|e| e.to_string())?;
        validate_file(&file)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("sandbox: cannot lock protection registry: {}", std::io::Error::last_os_error()));
        }
        Ok(Self(file))
    }
}

impl Drop for RegistryLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn entry(path: &Path) -> Result<PathBuf, String> {
    let digest = Sha256::digest(path.as_os_str().as_bytes());
    Ok(registry()?.join(digest.iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

type TargetId = (u64, u64, String);

fn open_target(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)
}

fn identity(path: &Path) -> Option<TargetId> {
    let file = open_target(path).ok()?;
    let meta = file.metadata().ok()?;
    let mut value = [0u8; 64];
    let len = unsafe {
        libc::fgetxattr(file.as_raw_fd(), c"user.mare.synthetic".as_ptr(), value.as_mut_ptr().cast(), value.len())
    };
    if len <= 0 {
        return None;
    }
    let token = std::str::from_utf8(&value[..len as usize]).ok()?.to_string();
    Some((meta.dev(), meta.ino(), token))
}

fn mark_target(path: &Path) -> Result<TargetId, String> {
    let file = open_target(path).map_err(|e| e.to_string())?;
    let token = uuid::Uuid::new_v4().to_string();
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"user.mare.synthetic".as_ptr(),
            token.as_ptr().cast(),
            token.len(),
            libc::XATTR_CREATE,
        )
    };
    if result != 0 {
        return Err(format!("sandbox: cannot mark protected mount target: {}", std::io::Error::last_os_error()));
    }
    let meta = file.metadata().map_err(|e| e.to_string())?;
    Ok((meta.dev(), meta.ino(), token))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LeaseState {
    Active,
    Exited,
    Orphan,
    Unknown,
}

/// Watch a child with pidfd, or with non-reaping waitid on older kernels.
pub(super) enum ProcessWatch {
    Pidfd { fd: std::os::fd::OwnedFd, pid: u32 },
    Child { pid: u32 },
    Exited { pid: u32 },
}

/// `None` means still running; ECHILD means this child has already been reaped. PID reuse can
/// make us wait for a newer child, but cannot make a live original child appear reaped.
fn child_wait_status(pid: u32) -> std::io::Result<bool> {
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
    };
    if result == 0 {
        return Ok(unsafe { info.si_pid() } != 0);
    }
    let e = std::io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::ECHILD) { Ok(true) } else { Err(e) }
}

impl ProcessWatch {
    pub(super) fn open(pid: u32, force_waitid: bool) -> Result<Self, String> {
        use std::os::fd::FromRawFd;
        let raw = if force_waitid { -1 } else { unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 } };
        if raw >= 0 {
            return Ok(Self::Pidfd { fd: unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) }, pid });
        }
        match child_wait_status(pid) {
            Ok(true) => Ok(Self::Exited { pid }),
            Ok(false) => Ok(Self::Child { pid }),
            Err(e) => Err(format!("sandbox: cannot monitor child process: {e}")),
        }
    }

    pub(super) fn wait(self) {
        let pid = match self {
            Self::Exited { pid } => pid,
            Self::Pidfd { fd, pid } => {
                let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                loop {
                    let ret = unsafe { libc::poll(&mut p, 1, -1) };
                    if ret > 0 && p.revents & libc::POLLIN != 0 {
                        break;
                    }
                    if ret < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    wait_child(pid);
                    break;
                }
                pid
            }
            Self::Child { pid } => {
                wait_child(pid);
                pid
            }
        };
        // Bubblewrap's namespace reaper remains in this group until all namespace descendants,
        // including descendants that called setsid, have stopped. Ignore only zombie members.
        loop {
            let observed = group_has_live_processes(pid);
            // only a clean observation of an empty group releases; an unknown result keeps it
            if observed.as_ref().is_ok_and(|live| !live) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

fn wait_child(pid: u32) {
    loop {
        if child_wait_status(pid).is_ok_and(|done| done) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

const MAX_PROC_OBSERVATIONS: usize = 256;

pub(crate) fn group_has_live_processes(group: u32) -> std::io::Result<bool> {
    #[cfg(test)]
    if FAIL_PROC_INSPECTION.with(|flag| flag.get()) {
        return Err(std::io::Error::other("injected procfs failure"));
    }
    // A process can disappear between readdir and stat. Ignore entries whose PID is confirmed
    // dead (ESRCH), and retry the observation only if a still-live process returned ENOENT/ESRCH
    // (e.g., access policy); persistent failures remain an error ("unknown").
    #[cfg(test)]
    let proc_root = TEST_PROC_ROOT.with(|root| root.borrow().clone()).unwrap_or_else(|| PathBuf::from("/proc"));
    #[cfg(not(test))]
    let proc_root = PathBuf::from("/proc");
    for _ in 0..MAX_PROC_OBSERVATIONS {
        super::mount_inspection::visible_proc()?;
        match scan_process_group(&proc_root, group) {
            Ok(live) => {
                if !live {
                    super::mount_inspection::visible_proc()?;
                }
                return Ok(live);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
                std::thread::yield_now();
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("process visibility remained unstable"))
}

fn pid_is_alive(pid: u32) -> std::io::Result<bool> {
    #[cfg(test)]
    if let Some(check) = TEST_PID_ALIVE.with(|cell| cell.borrow().clone()) {
        return check(pid);
    }
    let Ok(pid_t) = libc::pid_t::try_from(pid) else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    if pid_t <= 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    if unsafe { libc::kill(pid_t, 0) } == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) { Ok(false) } else { Err(err) }
}

fn scan_process_group(proc_root: &Path, group: u32) -> std::io::Result<bool> {
    #[cfg(test)]
    let force_enoent = FORCE_SCAN_ENOENT.with(|count| {
        let n = count.get();
        if n > 0 {
            count.set(n - 1);
            true
        } else {
            false
        }
    });
    for entry in std::fs::read_dir(proc_root)? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        // ENOENT/ESRCH on stat can be produced by process exit or by access policy. Only ignore it
        // when pid_is_alive confirms the process is genuinely gone (ESRCH); if it is still live
        // or the check fails (e.g., EPERM), propagate the error so the observation retries or
        // reports unknown.
        // comm is an arbitrary byte sequence, not necessarily UTF-8. Only the numeric fields
        // after the final closing parenthesis are interpreted as ASCII.
        let stat = match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH) => {
                match pid_is_alive(pid) {
                    Ok(false) => continue,
                    Ok(true) | Err(_) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        let end = stat
            .windows(2)
            .rposition(|bytes| bytes == b") ")
            .ok_or_else(|| std::io::Error::other("invalid process stat"))?;
        let fields: Vec<_> = stat[end + 2..].split(|b| b.is_ascii_whitespace()).filter(|f| !f.is_empty()).collect();
        let state = fields.first().ok_or_else(|| std::io::Error::other("missing process state"))?;
        let pgrp = fields
            .get(2)
            .and_then(|s| std::str::from_utf8(s).ok()?.parse::<u32>().ok())
            .ok_or_else(|| std::io::Error::other("missing process group"))?;
        if pgrp == group {
            if *state != b"Z" && *state != b"X" {
                #[cfg(test)]
                if force_enoent {
                    continue;
                }
                return Ok(true);
            }
            // A thread-group leader can be a zombie while sibling threads still run. Only a
            // one-thread zombie group is conclusively inert; missing data is never assumed safe.
            let threads = fields
                .get(17)
                .and_then(|s| std::str::from_utf8(s).ok()?.parse::<u32>().ok())
                .ok_or_else(|| std::io::Error::other("missing process thread count"))?;
            if threads != 1 {
                #[cfg(test)]
                if force_enoent {
                    continue;
                }
                return Ok(true);
            }
        }
    }
    #[cfg(test)]
    if force_enoent {
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    }
    Ok(false)
}

/// A stale lease may name a process owned by another host. Never use waitid/ECHILD here.
fn state(entry: &Path, path: &Path) -> Result<LeaseState, String> {
    let Ok(record) = read_identity(entry) else { return Ok(LeaseState::Unknown) };
    let Some(id) = identity(path) else { return Ok(LeaseState::Unknown) };
    if record != format!("{} {} {}", id.0, id.1, id.2) {
        return Ok(LeaseState::Unknown);
    }
    let orphan_file = entry.join("orphan");
    let mut result = if orphan_file.exists() { LeaseState::Orphan } else { LeaseState::Exited };
    for e in std::fs::read_dir(entry).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        if !e.file_name().to_string_lossy().starts_with("lease-") {
            continue;
        }
        let mut f = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(e.path())
            .map_err(|e| e.to_string())?;
        validate_file(&f)?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
                return Ok(LeaseState::Active);
            }
            return Err("sandbox: cannot inspect protection lease".into());
        }
        let mut record = String::new();
        f.read_to_string(&mut record).map_err(|e| e.to_string())?;
        if let Some(pid) =
            record.strip_prefix("pid ").and_then(|s| s.trim().parse::<libc::pid_t>().ok()).filter(|p| *p > 0)
        {
            let alive = unsafe { libc::kill(pid, 0) } == 0
                || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH);
            if alive {
                return Ok(LeaseState::Active);
            }
        }
        // Persist abandonment even if another live lease is encountered later. Otherwise
        // that last holder could delete a target that has become an ordinary existing path.
        let marker = OpenOptions::new()
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&orphan_file)
            .map_err(|e| e.to_string())?;
        validate_file(&marker)?;
        std::fs::remove_file(e.path()).map_err(|e| e.to_string())?;
        result = LeaseState::Orphan;
    }
    Ok(result)
}

/// A target is absent: clear unlocked abandoned records before creating a new target.
/// Never disturb a live holder.
fn reset_absent_target(entry: &Path) -> Result<(), String> {
    for e in std::fs::read_dir(entry).map_err(|e| e.to_string())? {
        let e = e.map_err(|e| e.to_string())?;
        if !e.file_name().to_string_lossy().starts_with("lease-") {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(e.path())
            .map_err(|e| e.to_string())?;
        validate_file(&file)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err("sandbox: an active sandbox still owns the missing protected target".into());
        }
        std::fs::remove_file(e.path()).map_err(|e| e.to_string())?;
    }
    match std::fs::remove_file(entry.join("orphan")) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    match std::fs::remove_file(entry.join("identity")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

pub(super) fn can_inspect_lifecycle() -> bool {
    #[cfg(test)]
    if FAIL_PROC_INSPECTION.with(|flag| flag.get()) {
        return false;
    }
    if super::bwrap_lifecycle::supported() {
        super::mount_inspection::visible_proc().is_ok()
    } else {
        group_has_live_processes(std::process::id()).is_ok()
    }
}

pub(super) fn is_marked(path: &Path) -> bool {
    identity(path).is_some()
}

/// A marked target can be mounted read-only whether actively coordinated or abandoned.
pub(super) fn live_or_unmarked(policy: &Policy, path: &Path) -> bool {
    if identity(path).is_none() {
        return true;
    }
    #[cfg(test)]
    AFTER_MARKER_CHECK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    if !registry_safe(policy) {
        return false;
    }
    let Ok(_lock) = RegistryLock::acquire() else { return false };
    // The last holder can finish between the initial marker read and this lock. Once locked,
    // disappearance is the ordinary missing-target case, not an abandoned lease.
    if identity(path).is_none() {
        return match std::fs::symlink_metadata(path) {
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            Ok(meta) => !meta.file_type().is_symlink(),
        };
    }
    let Ok(dir) = entry(path) else { return false };
    matches!(state(&dir, path), Ok(LeaseState::Active | LeaseState::Exited | LeaseState::Orphan | LeaseState::Unknown))
}

/// An existing target belongs to a live call, or to a known-exited call that can be recovered.
#[cfg(test)]
pub(super) fn synthetic(policy: &Policy, path: &Path) -> bool {
    if !registry_safe(policy) {
        return false;
    }
    let Ok(_lock) = RegistryLock::acquire() else { return false };
    let Ok(dir) = entry(path) else { return false };
    match state(&dir, path) {
        Ok(LeaseState::Active) => true,
        Ok(LeaseState::Exited) if std::fs::remove_dir(path).is_ok() => {
            let _ = std::fs::remove_file(dir.join("identity"));
            true
        }
        _ => false,
    }
}

/// Roll back a newly created target if registration fails before a lease owns it.
struct NewTarget {
    path: PathBuf,
    entry: PathBuf,
    id: TargetId,
    committed: bool,
}

impl Drop for NewTarget {
    fn drop(&mut self) {
        if !self.committed && identity(&self.path).as_ref() == Some(&self.id) && std::fs::remove_dir(&self.path).is_ok()
        {
            let _ = std::fs::remove_file(self.entry.join("identity"));
        }
    }
}

pub(super) struct SyntheticMount {
    path: PathBuf,
    entry: PathBuf,
    lease_path: PathBuf,
    lease: File,
    id: TargetId,
}

impl SyntheticMount {
    pub fn synthetic_path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    pub fn valid(&self) -> bool {
        identity(&self.path).as_ref() == Some(&self.id)
    }

    pub fn mark_child(&self, pid: u32) -> Result<(), String> {
        validate_file(&self.lease)?;
        self.lease.set_len(0).map_err(|e| e.to_string())?;
        self.lease.write_all_at(format!("pid {pid}\n").as_bytes(), 0).map_err(|e| e.to_string())
    }
}

impl Drop for SyntheticMount {
    fn drop(&mut self) {
        let Ok(_lock) = RegistryLock::acquire() else { return };
        let _ = std::fs::remove_file(&self.lease_path);
        // Other holders have independent locked lease files. Never remove nonempty/replaced data.
        if state(&self.entry, &self.path).ok() == Some(LeaseState::Exited)
            && identity(&self.path).as_ref() == Some(&self.id)
            && std::fs::remove_dir(&self.path).is_ok()
        {
            let _ = std::fs::remove_file(self.entry.join("identity"));
        }
        let _ = self.lease.as_raw_fd();
    }
}

pub(super) fn prepare(policy: &Policy) -> Result<Vec<SyntheticMount>, String> {
    prepare_paths(policy, &policy.protected_subpaths())
}

pub(super) fn prepare_paths(policy: &Policy, paths: &[PathBuf]) -> Result<Vec<SyntheticMount>, String> {
    let grants: Vec<_> = policy.extra.iter().filter(|g| paths.iter().any(|p| g.starts_with(p))).collect();
    let candidates: Vec<_> = paths
        .iter()
        .filter(|p| {
            if p.file_name().is_none_or(|name| name != ".mare") || grants.iter().any(|g| p.starts_with(g)) {
                return false;
            }
            match std::fs::symlink_metadata(p) {
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
                Ok(m) => m.is_dir() && identity(p).is_some(),
            }
        })
        .cloned()
        .collect();
    if candidates.is_empty() {
        return Ok(vec![]);
    }
    // Without procfs we cannot verify that the namespace reaper and its descendants have stopped.
    if !can_inspect_lifecycle() {
        return Err("sandbox: cannot inspect process lifecycle".into());
    }
    if !registry_safe(policy) {
        return Err(
            "sandbox: cannot exclude writable mount aliases to protected paths or the protection registry".into()
        );
    }
    let mut mounts = Vec::new();
    let lock = RegistryLock::acquire()?;
    let result = (|| -> Result<(), String> {
        for path in candidates {
            let dir = entry(&path)?;
            let mut new_target = None;
            match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => (),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.to_string()),
            }
            let meta = std::fs::symlink_metadata(&dir).map_err(|e| e.to_string())?;
            if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } {
                return Err("sandbox: unsafe protection registry entry".into());
            }
            let create = match std::fs::symlink_metadata(&path) {
                Ok(_) => match state(&dir, &path)? {
                    LeaseState::Active => false,
                    LeaseState::Exited | LeaseState::Orphan | LeaseState::Unknown => {
                        // The abandoned target is an existing path from now on. Invalidate the
                        // identity so no subsequent holder can remove it as synthetic.
                        let _ = std::fs::remove_file(dir.join("identity"));
                        continue;
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    reset_absent_target(&dir)?;
                    true
                }
                Err(e) => return Err(e.to_string()),
            };
            if create {
                std::fs::create_dir(&path)
                    .map_err(|e| format!("sandbox: cannot create protected mount target: {e}"))?;
                let id = match mark_target(&path) {
                    Ok(id) => id,
                    Err(e) => {
                        let _ = std::fs::remove_dir(&path);
                        return Err(e);
                    }
                };
                new_target =
                    Some(NewTarget { path: path.clone(), entry: dir.clone(), id: id.clone(), committed: false });
                write_identity(&dir, &format!("{} {} {}", id.0, id.1, id.2))?;
            }
            let id = identity(&path).ok_or("sandbox: protected mount target changed")?;
            let mut tmp = tempfile::Builder::new().prefix("lease-").tempfile_in(&dir).map_err(|e| e.to_string())?;
            tmp.write_all(b"active").map_err(|e| e.to_string())?;
            if unsafe { libc::flock(tmp.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err("sandbox: cannot acquire protection lease".into());
            }
            let (lease, lease_path) = tmp.keep().map_err(|e| e.to_string())?;
            mounts.push(SyntheticMount { path, entry: dir, lease_path, lease, id });
            if let Some(mut target) = new_target {
                target.committed = true;
            }
        }
        Ok(())
    })();
    drop(lock);
    result?;
    Ok(mounts)
}

#[cfg(test)]
mod tests;
