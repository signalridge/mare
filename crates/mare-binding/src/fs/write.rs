//! Atomic, sandbox-anchored, and directory-synced file writes for `mare.fs`.

use std::path::{Path, PathBuf};

#[path = "write_anchored.rs"]
mod anchored;

#[cfg(target_os = "linux")]
pub(crate) use anchored::open_beneath_fd;
#[cfg(unix)]
pub(crate) use anchored::open_parent_dir_from_root;
use anchored::write_anchored;
#[cfg(windows)]
use anchored::write_anchored_windows;
#[cfg(unix)]
use anchored::write_dir_fd;
pub(crate) use anchored::{anchored_mkdir_all, anchored_remove, anchored_rename};

/// Writable roots of the calling tool's sandbox: a policy-checked write is
/// re-anchored to the root it was checked against when it actually happens.
pub type Beneath = Option<Vec<PathBuf>>;

#[cfg(all(test, unix))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum WriteSeamPoint {
    BeforeAnchor,
    AfterOpenDirBeforeResolve,
    AfterPostOpenCheckBeforeWrite,
}

#[cfg(all(test, unix))]
type WriteSeamHook = Box<dyn FnMut() -> std::io::Result<()>>;

#[cfg(all(test, unix))]
thread_local! {
    static WRITE_SEAM: std::cell::RefCell<Option<(WriteSeamPoint, WriteSeamHook)>> =
        std::cell::RefCell::new(None);
}

/// Test seam (unit tests only): register a one-shot deterministic barrier at `point`.
#[cfg(all(test, unix))]
pub(super) fn set_write_seam(point: WriteSeamPoint, f: impl FnMut() -> std::io::Result<()> + 'static) {
    WRITE_SEAM.with(|cell| {
        *cell.borrow_mut() = Some((point, Box::new(f)));
    });
}

#[cfg(all(test, unix))]
fn fire_write_seam(point: WriteSeamPoint) -> std::io::Result<()> {
    let hook = WRITE_SEAM.with(|cell| {
        let mut guard = cell.borrow_mut();
        if guard.as_ref().is_some_and(|(p, _)| *p == point) { guard.take().map(|(_, f)| f) } else { None }
    });
    if let Some(mut f) = hook { f() } else { Ok(()) }
}

pub(crate) fn normalize_lexical(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(comp),
            },
            _ => out.push(comp),
        }
    }
    out
}

/// The canonical writable root containing the already-resolved `physical` path and the path
/// relative to it. Never re-resolves symlinks after the policy check.
pub(super) fn anchor(roots: &[PathBuf], physical: &Path) -> Option<(PathBuf, PathBuf)> {
    roots
        .iter()
        .filter(|r| physical.starts_with(r))
        .max_by_key(|r| r.components().count())
        .map(|r| (r.clone(), physical.strip_prefix(r).unwrap_or(Path::new("")).to_path_buf()))
}

/// Write `data` to `path`. `atomic`: temp file + fsync + rename (readers never see a partial
/// file; an existing file keeps its permissions). `create_new`: fail with `AlreadyExists` if the
/// path exists — with `atomic`, the temp file is hard-linked into place so a file someone else
/// created meanwhile is never clobbered.
#[cfg(unix)]
pub(crate) fn open_sync_directory(path: &Path, root: Option<&Path>) -> std::io::Result<std::fs::File> {
    use std::os::fd::AsRawFd;
    #[cfg(not(target_os = "linux"))]
    use std::os::fd::FromRawFd;
    #[cfg(not(target_os = "linux"))]
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let Some(root) = root else {
        return std::fs::File::open(path);
    };
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let relative = path.strip_prefix(root).map_err(std::io::Error::other)?;
    #[cfg(target_os = "linux")]
    {
        let relative = if relative.as_os_str().is_empty() { Path::new(".") } else { relative };
        open_beneath_fd(directory.as_raw_fd(), relative, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)
            .map(std::fs::File::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut directory = directory;
        for component in relative.components() {
            let name = std::ffi::CString::new(component.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            directory = unsafe { std::fs::File::from_raw_fd(fd) };
        }
        Ok(directory)
    }
}

/// Directory handles captured once and checked against the named paths before publication.
/// Unix also syncs the held directories; Windows verifies their identities around file sync.
/// Callers may use the held leaf/parent handles for their own relative publication.
pub(crate) struct DirectorySyncPlan {
    #[cfg(any(unix, windows))]
    pub(crate) directories: Vec<(PathBuf, std::fs::File)>,
}

#[cfg(windows)]
fn open_windows_sync_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(std::io::Error::other("sync parent is not a plain directory"));
    }
    Ok(directory)
}

#[cfg(windows)]
fn windows_directory_identity(directory: &std::fs::File) -> std::io::Result<(u32, u32, u32)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};

    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: directory owns a live handle and info is a valid output buffer.
    if unsafe { GetFileInformationByHandle(directory.as_raw_handle() as _, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow))
}

impl DirectorySyncPlan {
    pub(crate) fn verify(&self) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            for (path, handle) in &self.directories {
                let held = handle.metadata()?;
                let named = std::fs::symlink_metadata(path)?;
                if !named.is_dir() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
                    return Err(std::io::Error::other("directory identity changed before durable publication"));
                }
            }
        }
        #[cfg(windows)]
        for (path, handle) in &self.directories {
            let named = open_windows_sync_directory(path)?;
            if windows_directory_identity(handle)? != windows_directory_identity(&named)? {
                return Err(std::io::Error::other("directory identity changed before durable publication"));
            }
        }
        Ok(())
    }

    pub(crate) fn sync(&self) -> std::io::Result<()> {
        self.verify()?;
        #[cfg(unix)]
        for (_, handle) in &self.directories {
            handle.sync_all()?;
        }
        self.verify()
    }
}

/// Capture physical checked parents leaf-first through the caller's durable boundary.
pub(crate) fn capture_directory_sync_plan(
    path: &Path,
    boundary: &Path,
    beneath: &Beneath,
) -> std::io::Result<DirectorySyncPlan> {
    if !path.parent().is_some_and(|parent| parent.starts_with(boundary)) {
        return Err(std::io::Error::other("sync boundary must be a parent directory"));
    }
    #[cfg(unix)]
    {
        let mut directories = Vec::new();
        let root = match beneath {
            Some(roots) => Some(
                roots
                    .iter()
                    .filter(|root| path.starts_with(root) && boundary.starts_with(root))
                    .max_by_key(|root| root.components().count())
                    .ok_or_else(|| {
                        std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "sync boundary is outside controlled roots",
                        )
                    })?
                    .as_path(),
            ),
            None => None,
        };
        for directory in path.ancestors().skip(1) {
            if !directory.as_os_str().is_empty() {
                let handle = open_sync_directory(directory, root)?;
                if !handle.metadata()?.is_dir() {
                    return Err(std::io::Error::other("sync parent is not a directory"));
                }
                directories.push((directory.to_path_buf(), handle));
            }
            if directory == boundary {
                break;
            }
        }

        let plan = DirectorySyncPlan { directories };
        plan.verify()?;
        Ok(plan)
    }
    #[cfg(windows)]
    {
        if let Some(roots) = beneath
            && !roots.iter().any(|root| path.starts_with(root) && boundary.starts_with(root))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "sync boundary is outside controlled roots",
            ));
        }
        let mut directories = Vec::new();
        for directory in path.ancestors().skip(1) {
            if !directory.as_os_str().is_empty() {
                directories.push((directory.to_path_buf(), open_windows_sync_directory(directory)?));
            }
            if directory == boundary {
                break;
            }
        }
        let plan = DirectorySyncPlan { directories };
        plan.verify()?;
        Ok(plan)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = beneath;
        Ok(DirectorySyncPlan {})
    }
}

pub(super) fn sync_parent_dirs(path: &Path, boundary: &Path, beneath: &Beneath) -> std::io::Result<()> {
    capture_directory_sync_plan(path, boundary, beneath)?.sync()
}

#[derive(Default)]
pub struct WriteOptions<'a> {
    pub append: bool,
    pub atomic: bool,
    pub create_new: bool,
    pub mode: Option<u32>,
    pub sync_boundary: Option<&'a Path>,
    /// Trusted explicit root directory (e.g. quarantine) when not inside a tool sandbox.
    /// Writes are anchored to an open directory handle for this root, refusing any
    /// symlink in the relative path beneath it, with no `/dev` exception.
    pub anchored_root: Option<&'a Path>,
    /// Full effective sandbox write policy for controlled writes (`beneath`).
    /// Preserved across the async/blocking boundary so the write re-checks the full policy
    /// (including `.git`/`.mare` protected subpaths and `deny_read`) against the opened
    /// parent directory's physical path before writing through that handle.
    pub policy: Option<&'a crate::ossandbox::Policy>,
}

pub(super) fn deny_write(path: &Path, policy: &crate::ossandbox::Policy) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("sandbox: write {} denied {}", path.display(), policy.denied_marker()),
    )
}

pub fn write_file(path: &Path, data: &[u8], options: WriteOptions<'_>, beneath: &Beneath) -> std::io::Result<()> {
    let WriteOptions { append, atomic, create_new, mode, sync_boundary, anchored_root, policy } = options;
    if let Some(root) = anchored_root {
        let sync_plan = if atomic && !append {
            let norm_root = normalize_lexical(root);
            let norm_path = normalize_lexical(path);
            let boundary = match sync_boundary {
                Some(b) => {
                    let b = normalize_lexical(b);
                    if !norm_path.parent().is_some_and(|parent| parent.starts_with(&b)) {
                        return Err(std::io::Error::other("sync boundary must be a parent directory"));
                    }
                    if !b.starts_with(&norm_root) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "sync boundary is outside the write target's anchored root",
                        ));
                    }
                    b
                }
                None => norm_root.clone(),
            };
            Some((norm_root, norm_path, boundary))
        } else {
            None
        };
        write_anchored(root, path, data, append, atomic, create_new, mode)?;
        if let Some((norm_root, norm_path, boundary)) = sync_plan {
            let physical = crate::ossandbox::resolve(&norm_path)?;
            let physical_root = crate::ossandbox::resolve(&norm_root)?;
            let physical_boundary = crate::ossandbox::resolve(&boundary)?;
            let anchored_beneath = Some(vec![physical_root]);
            sync_parent_dirs(&physical, &physical_boundary, &anchored_beneath)?;
        }
        return Ok(());
    }
    if let Some(pol) = policy
        && !pol.can_write(path)
    {
        return Err(deny_write(path, pol));
    }
    let physical = crate::ossandbox::resolve(path)?;
    if let Some(pol) = policy
        && !pol.can_write(&physical)
    {
        return Err(deny_write(&physical, pol));
    }
    let effective_beneath: std::borrow::Cow<'_, Beneath> = match (policy, beneath) {
        (Some(pol), None) => std::borrow::Cow::Owned(Some(pol.writable.clone())),
        _ => std::borrow::Cow::Borrowed(beneath),
    };
    let boundary = match sync_boundary {
        Some(boundary) => {
            let boundary = crate::ossandbox::resolve(boundary)?;
            if !physical.parent().is_some_and(|parent| parent.starts_with(&boundary)) {
                return Err(std::io::Error::other("sync boundary must be a parent directory"));
            }
            if let Some(roots) = effective_beneath.as_ref()
                && !roots.iter().any(|root| physical.starts_with(root) && boundary.starts_with(root))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "sync boundary is outside the write target's controlled root",
                ));
            }
            boundary
        }
        None => physical
            .ancestors()
            .skip(1)
            .find(|directory| directory.is_dir())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
    };
    let final_target =
        write_file_inner(path, data, append, atomic, create_new, mode, effective_beneath.as_ref(), policy)?;
    if atomic && !append {
        sync_parent_dirs(&final_target, &boundary, effective_beneath.as_ref())?;
    }
    Ok(())
}

fn write_atomic_unanchored(
    path: &Path,
    data: &[u8],
    create_new: bool,
    mode: Option<u32>,
    private: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    let _ = private;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp = path.with_file_name(format!(
        ".{}.tmp-{}-{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("mare"),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // The temp file is created exclusively and private (0600) so nobody can read or swap it while
    // it is being written; the final mode (requested, or the replaced file's) is applied just
    // before it is published by rename.
    let target_meta = match std::fs::symlink_metadata(path) {
        Ok(m) => {
            #[cfg(windows)]
            let is_reparse = {
                use std::os::windows::fs::MetadataExt;
                (m.file_attributes() & 0x0400) != 0
            };
            #[cfg(not(windows))]
            let is_reparse = false;
            if m.file_type().is_symlink() || is_reparse {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("refusing to overwrite symlink or reparse point: {}", path.display()),
                ));
            }
            Some(m)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let final_perms = match mode {
        #[cfg(unix)]
        Some(m) => {
            use std::os::unix::fs::PermissionsExt;
            Some(std::fs::Permissions::from_mode(m))
        }
        _ => target_meta.map(|m| m.permissions()),
    };
    let mut oo = std::fs::OpenOptions::new();
    oo.write(true).create_new(true);
    #[cfg(unix)]
    if final_perms.is_some() {
        use std::os::unix::fs::OpenOptionsExt;
        oo.mode(0o600);
    }
    #[cfg(windows)]
    let mut f = if private { mare_shared::private::create_new(&tmp)? } else { oo.open(&tmp)? };
    #[cfg(not(windows))]
    let mut f = oo.open(&tmp)?;
    let written = f.write_all(data).and_then(|()| f.sync_all());
    let written = written.and_then(|()| match &final_perms {
        Some(p) => f.set_permissions(p.clone()),
        None => Ok(()),
    });
    drop(f);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if create_new {
        // link fails if the target exists: no window where another creator's file is lost
        let r = std::fs::hard_link(&tmp, path);
        let _ = std::fs::remove_file(&tmp);
        return r;
    }
    mare_shared::paths::rename_retry(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[allow(clippy::too_many_arguments)]
fn write_file_inner(
    path: &Path,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    mode: Option<u32>,
    beneath: &Beneath,
    policy: Option<&crate::ossandbox::Policy>,
) -> std::io::Result<PathBuf> {
    use std::io::Write;
    // test-only fault injection (debug builds only; release ignores it):
    // MARE_TEST_FS_FAULT=write-then-eio:<file name> writes half of the data to the target, then
    // fails with EIO — a write that errored after changing the file (apply_patch's `unknown`)
    #[cfg(debug_assertions)]
    if let Ok(f) = std::env::var("MARE_TEST_FS_FAULT")
        && let Some(name) = f.strip_prefix("write-then-eio:")
        && path.file_name().is_some_and(|n| n == name)
    {
        std::fs::write(path, &data[..data.len() / 2])?;
        return Err(std::io::Error::other("injected I/O error (MARE_TEST_FS_FAULT)"));
    }
    // act on the physical path the policy checked (symlinks resolved per component, `..` after
    // them, as the kernel does:148-175): `ws/link/..` can't be checked
    // as `ws/` and then opened as link's parent, and a write through a link edits its target
    let physical = crate::ossandbox::resolve(path)?;
    if let Some(pol) = policy
        && (!pol.can_write(path) || !pol.can_write(&physical))
    {
        return Err(deny_write(&physical, pol));
    }
    let roots = policy.map(|pol| &pol.writable).or(beneath.as_ref());
    if let Some(roots) = roots
        && !crate::ossandbox::is_write_device(&physical)
    {
        #[cfg(all(test, unix))]
        fire_write_seam(WriteSeamPoint::BeforeAnchor)?;
        let Some((root, rel)) = anchor(roots, &physical) else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "sandbox: {} changed after the policy check (now outside the writable roots)",
                    physical.display()
                ),
            ));
        };
        #[cfg(unix)]
        {
            use std::os::fd::AsFd;
            let (dir_fd, final_parent, name) = open_parent_dir_from_root(&root, &rel, mode, policy)?;
            let final_target = final_parent.join(&name);
            if let Some(pol) = policy
                && !pol.can_write(&final_target)
            {
                return Err(deny_write(&final_target, pol));
            }
            #[cfg(test)]
            fire_write_seam(WriteSeamPoint::AfterPostOpenCheckBeforeWrite)?;
            write_dir_fd(dir_fd.as_fd(), &name, data, append, atomic, create_new, mode)?;
            return Ok(final_target);
        }
        #[cfg(windows)]
        {
            let private = mode.is_some_and(|m| m & 0o077 == 0);
            write_anchored_windows(&root, &rel, data, append, atomic, create_new, mode, private)?;
            return Ok(physical);
        }
    }
    let path: &Path = &physical;
    // a private file (mode without group/other bits: auth.json, state, locks); Windows has no mode
    // bits: such a file gets the private DACL instead (`mare_shared::private`), anything else
    // inherits the directory's ACL
    let private = mode.is_some_and(|m| m & 0o077 == 0);
    if let Some(d) = path.parent()
        && !d.exists()
    {
        // ... and private parent directories too (0700), whatever the umask; anything else uses
        // the defaults
        if private {
            mare_shared::private::create_dir_all(d)?;
        } else {
            std::fs::create_dir_all(d)?;
        }
    }
    if atomic && !append {
        write_atomic_unanchored(path, data, create_new, mode, private)?;
        return Ok(physical);
    }
    if create_new {
        #[cfg(windows)]
        if private {
            mare_shared::private::create_new(path)?.write_all(data)?;
            return Ok(physical);
        }
        let mut oo = std::fs::OpenOptions::new();
        oo.write(true).create_new(true);
        oo.open(path)?.write_all(data)?;
        return Ok(physical);
    }
    #[cfg(unix)]
    if let Some(m) = mode
        && path.exists()
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(m))?;
    }
    // the chmod of an existing file
    #[cfg(windows)]
    if private && path.exists() {
        mare_shared::private::restrict(path)?;
    }
    let mut oo = std::fs::OpenOptions::new();
    oo.write(true).create(true);
    if append {
        oo.append(true);
    } else {
        oo.truncate(true);
    }
    #[cfg(unix)]
    if let Some(m) = mode {
        use std::os::unix::fs::OpenOptionsExt;
        oo.mode(m);
    }
    #[cfg(windows)]
    if private {
        mare_shared::private::open(&mut oo, path)?.write_all(data)?;
        return Ok(physical);
    }
    oo.open(path)?.write_all(data)?;
    Ok(physical)
}
