// exec-ctx: not a tool entry (anchored-open helpers split from fs/write.rs; gated by callers in fs/lua_api.rs)
use super::*;

#[cfg(unix)]
fn process_umask() -> libc::mode_t {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("Umask:")
                    && let Ok(mask) = libc::mode_t::from_str_radix(rest.trim(), 8)
                {
                    return mask & 0o777;
                }
            }
        }
        0o022
    }
    #[cfg(not(target_os = "linux"))]
    {
        static UMASK: std::sync::OnceLock<libc::mode_t> = std::sync::OnceLock::new();
        *UMASK.get_or_init(|| unsafe {
            let m = libc::umask(0o022);
            libc::umask(m);
            m & 0o777
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn open_beneath_fd(dir: i32, p: &Path, flags: i32, mode: u32) -> std::io::Result<std::os::fd::OwnedFd> {
    let mut how: libc::open_how = unsafe { std::mem::zeroed() };
    how.flags = flags as u64;
    how.mode = mode as u64;
    how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // On Linux, `openat2` with `RESOLVE_BENEATH` returns `EAGAIN` if a concurrent
    // filesystem rename occurs while traversing `..` components; retry transiently.
    for attempt in 0..8 {
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                dir,
                c.as_ptr(),
                &how as *const libc::open_how,
                size_of::<libc::open_how>(),
            )
        };
        if fd >= 0 {
            return Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        }
        let err = std::io::Error::last_os_error();
        if matches!(err.raw_os_error(), Some(libc::EINTR | libc::EAGAIN)) && attempt + 1 < 8 {
            continue;
        }
        return Err(err);
    }
    unreachable!()
}

#[cfg(target_os = "linux")]
fn write_dir_fd_beneath(
    dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    mode: Option<u32>,
) -> Option<std::io::Result<()>> {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let cs = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);

    let perm = mode.unwrap_or(0o666);
    let run = || -> std::io::Result<()> {
        let name_p = Path::new(name);
        if atomic && !append {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let tmp = format!(
                ".{}.tmp-{}-{}",
                name.to_string_lossy(),
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let tmp_p = Path::new(&tmp);
            // private (0600) while being written; the final mode is applied before publishing
            //
            let fd = match open_beneath_fd(
                dir.as_raw_fd(),
                tmp_p,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
                0o600,
            ) {
                Ok(fd) => fd,
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                    return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
                }
                Err(e) => return Err(e),
            };
            let mut f = std::fs::File::from(fd);
            let res = (|| {
                f.write_all(data)?;
                if let Some(m) = mode {
                    unsafe { libc::fchmod(f.as_raw_fd(), m as libc::mode_t) };
                } else if open_beneath_fd(dir.as_raw_fd(), name_p, libc::O_PATH | libc::O_CLOEXEC, 0).is_err() {
                    // a new file: the usual default mode, not 0600
                    let umask = process_umask();
                    unsafe { libc::fchmod(f.as_raw_fd(), 0o666 & !umask) };
                }
                // replacing an existing file keeps its permissions (unless a mode was asked for)
                if mode.is_none()
                    && let Ok(existing) = open_beneath_fd(dir.as_raw_fd(), name_p, libc::O_PATH | libc::O_CLOEXEC, 0)
                {
                    let mut st: libc::stat = unsafe { std::mem::zeroed() };
                    if unsafe { libc::fstat(existing.as_raw_fd(), &mut st) } == 0 {
                        unsafe { libc::fchmod(f.as_raw_fd(), st.st_mode & 0o7777) };
                    }
                }
                f.sync_all()
            })();
            drop(f);
            let (tc, nc) = (cs(tmp_p)?, cs(name_p)?);
            let res = res.and_then(|()| {
                let rc = if create_new {
                    unsafe { libc::linkat(dir.as_raw_fd(), tc.as_ptr(), dir.as_raw_fd(), nc.as_ptr(), 0) }
                } else {
                    unsafe { libc::renameat(dir.as_raw_fd(), tc.as_ptr(), dir.as_raw_fd(), nc.as_ptr()) }
                };
                if rc != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
            });
            if create_new || res.is_err() {
                unsafe { libc::unlinkat(dir.as_raw_fd(), tc.as_ptr(), 0) };
            }
            return res;
        }
        let mut flags = libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC;
        flags |= if create_new {
            libc::O_EXCL
        } else if append {
            libc::O_APPEND
        } else {
            libc::O_TRUNC
        };
        let fd = open_beneath_fd(dir.as_raw_fd(), name_p, flags, perm)?;
        let mut f = std::fs::File::from(fd);
        if let Some(m) = mode {
            unsafe { libc::fchmod(f.as_raw_fd(), m as libc::mode_t) };
        }
        f.write_all(data)
    };
    match run() {
        Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => None,
        other => Some(other),
    }
}

#[cfg(target_os = "linux")]
fn write_beneath_fd(
    root_fd: std::os::fd::BorrowedFd<'_>,
    rel: &Path,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    mode: Option<u32>,
) -> Option<std::io::Result<()>> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let cs = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);

    let name = rel.file_name()?.to_owned();
    let parent = rel.parent().unwrap_or(Path::new(""));
    let dir_mode: libc::mode_t = if mode.is_some_and(|m| m & 0o077 == 0) { 0o700 } else { 0o777 };
    // the parent directory, beneath the root, creating missing components one by one
    let dir = {
        let mut held: Option<OwnedFd> = None;
        for comp in parent.components() {
            let cur_fd = held.as_ref().map_or(root_fd.as_raw_fd(), |f| f.as_raw_fd());
            let c = Path::new(comp.as_os_str());
            let next = match open_beneath_fd(cur_fd, c, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC, 0) {
                Ok(fd) => fd,
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => return None,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let cc = match cs(c) {
                        Ok(cc) => cc,
                        Err(e) => return Some(Err(e)),
                    };
                    if unsafe { libc::mkdirat(cur_fd, cc.as_ptr(), dir_mode) } != 0 {
                        let e = std::io::Error::last_os_error();
                        if e.kind() != std::io::ErrorKind::AlreadyExists {
                            return Some(Err(e));
                        }
                    }
                    match open_beneath_fd(cur_fd, c, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC, 0) {
                        Ok(fd) => fd,
                        Err(e) => return Some(Err(e)),
                    }
                }
                Err(e) => return Some(Err(e)),
            };
            held = Some(next);
        }
        let cur_fd = held.as_ref().map_or(root_fd.as_raw_fd(), |f| f.as_raw_fd());
        // O_PATH can't be used as an *at() base for creation everywhere: reopen as a directory
        match open_beneath_fd(cur_fd, Path::new("."), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0) {
            Ok(fd) => fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => return None,
            Err(e) => return Some(Err(e)),
        }
    };
    write_dir_fd_beneath(dir.as_fd(), &name, data, append, atomic, create_new, mode)
}

#[cfg(unix)]
fn verify_dir_identity(dir_fd: &std::os::fd::OwnedFd, expected: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(dir_fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let meta = std::fs::symlink_metadata(expected)?;
    if !meta.is_dir() || (st.st_dev as u64, st.st_ino as u64) != (meta.dev(), meta.ino()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("directory identity changed while opening {}", expected.display()),
        ));
    }
    Ok(())
}

/// Safely open the target parent directory beneath `root` without following symlinks in `rel`,
/// returning the open directory handle, its physical path, and the target file name.
#[cfg(unix)]
pub(crate) fn open_parent_dir_from_root(
    root: &Path,
    rel: &Path,
    mode: Option<u32>,
    policy: Option<&crate::ossandbox::Policy>,
) -> std::io::Result<(std::os::fd::OwnedFd, PathBuf, std::ffi::OsString)> {
    open_parent_dir_from_root_opts(root, rel, mode, policy, true)
}

#[cfg(unix)]
fn open_parent_dir_from_root_opts(
    root: &Path,
    rel: &Path,
    mode: Option<u32>,
    policy: Option<&crate::ossandbox::Policy>,
    create_parents: bool,
) -> std::io::Result<(std::os::fd::OwnedFd, PathBuf, std::ffi::OsString)> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    let cs = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let openat_dir_nofollow = |dir_fd: i32, name: &Path| -> std::io::Result<OwnedFd> {
        let c = cs(name)?;
        let fd = unsafe {
            libc::openat(dir_fd, c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0)
        };
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    };

    let name = rel
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name"))?
        .to_owned();
    let parent = rel.parent().unwrap_or(Path::new(""));
    let dir_mode: libc::mode_t = if mode.is_some_and(|m| m & 0o077 == 0) { 0o700 } else { 0o777 };
    let root_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let mut cur: OwnedFd = root_file.into();
    let mut cur_path = root.to_path_buf();
    #[cfg(target_os = "linux")]
    let mut use_openat2 = true;

    for comp in parent.components() {
        let c = Path::new(comp.as_os_str());
        cur_path.push(c);
        #[cfg(target_os = "linux")]
        if use_openat2 {
            match open_beneath_fd(cur.as_raw_fd(), c, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC, 0) {
                Ok(fd) => {
                    cur = fd;
                    continue;
                }
                Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                    use_openat2 = false;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create_parents => {
                    if let Some(pol) = policy
                        && !pol.can_write(&cur_path)
                    {
                        return Err(deny_write(&cur_path, pol));
                    }
                    let cc = cs(c)?;
                    if unsafe { libc::mkdirat(cur.as_raw_fd(), cc.as_ptr(), dir_mode) } != 0 {
                        let err = std::io::Error::last_os_error();
                        if err.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(err);
                        }
                    }
                    cur = open_beneath_fd(cur.as_raw_fd(), c, libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)?;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        cur = match openat_dir_nofollow(cur.as_raw_fd(), c) {
            Ok(fd) => fd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create_parents => {
                if let Some(pol) = policy
                    && !pol.can_write(&cur_path)
                {
                    return Err(deny_write(&cur_path, pol));
                }
                let cc = cs(c)?;
                if unsafe { libc::mkdirat(cur.as_raw_fd(), cc.as_ptr(), dir_mode) } != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(err);
                    }
                }
                openat_dir_nofollow(cur.as_raw_fd(), c)?
            }
            Err(e) => return Err(e),
        };
    }

    #[cfg(target_os = "linux")]
    if use_openat2 && !parent.as_os_str().is_empty() {
        match open_beneath_fd(cur.as_raw_fd(), Path::new("."), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0)
        {
            Ok(fd) => cur = fd,
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {
                cur = openat_dir_nofollow(cur.as_raw_fd(), Path::new("."))?;
            }
            Err(e) => return Err(e),
        }
    }

    #[cfg(test)]
    fire_write_seam(WriteSeamPoint::AfterOpenDirBeforeResolve)?;

    let expected_parent = if parent.as_os_str().is_empty() { root.to_path_buf() } else { root.join(parent) };
    #[cfg(target_os = "linux")]
    let final_parent = match std::fs::read_link(format!("/proc/self/fd/{}", cur.as_raw_fd())) {
        Ok(p) if !p.to_string_lossy().ends_with(" (deleted)") => p,
        _ => {
            verify_dir_identity(&cur, &expected_parent)?;
            dunce::canonicalize(&expected_parent).unwrap_or(expected_parent)
        }
    };
    #[cfg(not(target_os = "linux"))]
    let final_parent = {
        verify_dir_identity(&cur, &expected_parent)?;
        dunce::canonicalize(&expected_parent).unwrap_or(expected_parent)
    };
    Ok((cur, final_parent, name))
}

#[cfg(unix)]
fn write_dir_fd_nofollow(
    dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    mode: Option<u32>,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let cs = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let openat_nofollow = |dir_fd: i32, name: &Path, flags: i32, mode: u32| -> std::io::Result<OwnedFd> {
        let c = cs(name)?;
        let fd = unsafe { libc::openat(dir_fd, c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode) };
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    };

    let dir_fd = dir.as_raw_fd();
    let perm = mode.unwrap_or(0o666);
    let name_p = Path::new(name);
    if atomic && !append {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = format!(
            ".{}.tmp-{}-{}",
            name.to_string_lossy(),
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let tmp_p = Path::new(&tmp);
        let fd = openat_nofollow(dir_fd, tmp_p, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)?;
        let mut f = std::fs::File::from(fd);
        let nc = cs(name_p)?;
        let res = (|| {
            f.write_all(data)?;
            if let Some(m) = mode {
                unsafe { libc::fchmod(f.as_raw_fd(), m as libc::mode_t) };
            } else {
                let mut st: libc::stat = unsafe { std::mem::zeroed() };
                if unsafe { libc::fstatat(dir_fd, nc.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } == 0
                    && (st.st_mode & libc::S_IFMT) != libc::S_IFLNK
                {
                    unsafe { libc::fchmod(f.as_raw_fd(), st.st_mode & 0o7777) };
                } else {
                    let umask = process_umask();
                    unsafe { libc::fchmod(f.as_raw_fd(), 0o666 & !umask) };
                }
            }
            f.sync_all()
        })();
        drop(f);
        let tc = cs(tmp_p)?;
        let res = res.and_then(|()| {
            let rc = if create_new {
                unsafe { libc::linkat(dir_fd, tc.as_ptr(), dir_fd, nc.as_ptr(), 0) }
            } else {
                unsafe { libc::renameat(dir_fd, tc.as_ptr(), dir_fd, nc.as_ptr()) }
            };
            if rc != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
        });
        if create_new || res.is_err() {
            unsafe { libc::unlinkat(dir_fd, tc.as_ptr(), 0) };
        }
        return res;
    }
    let mut flags = libc::O_WRONLY | libc::O_CREAT;
    flags |= if create_new {
        libc::O_EXCL
    } else if append {
        libc::O_APPEND
    } else {
        libc::O_TRUNC
    };
    let fd = openat_nofollow(dir_fd, name_p, flags, perm)?;
    let mut f = std::fs::File::from(fd);
    if let Some(m) = mode {
        unsafe { libc::fchmod(f.as_raw_fd(), m as libc::mode_t) };
    }
    f.write_all(data)
}

#[cfg(unix)]
pub(super) fn write_dir_fd(
    dir: std::os::fd::BorrowedFd<'_>,
    name: &std::ffi::OsStr,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    mode: Option<u32>,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(res) = write_dir_fd_beneath(dir, name, data, append, atomic, create_new, mode) {
        return res;
    }
    write_dir_fd_nofollow(dir, name, data, append, atomic, create_new, mode)
}

#[cfg(unix)]
fn write_anchored_nofollow(
    root_fd: std::os::fd::BorrowedFd<'_>,
    rel: &Path,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    mode: Option<u32>,
) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let cs = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let openat_nofollow = |dir_fd: i32, name: &Path, flags: i32, mode: u32| -> std::io::Result<OwnedFd> {
        let c = cs(name)?;
        let fd = unsafe { libc::openat(dir_fd, c.as_ptr(), flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode) };
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    };

    let name = rel
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name"))?
        .to_owned();
    let parent = rel.parent().unwrap_or(Path::new(""));
    let dir_mode: u32 = if mode.is_some_and(|m| m & 0o077 == 0) { 0o700 } else { 0o777 };
    let mut held: Option<OwnedFd> = None;
    for comp in parent.components() {
        let cur_fd = held.as_ref().map_or(root_fd.as_raw_fd(), |f| f.as_raw_fd());
        let c = Path::new(comp.as_os_str());
        let next = match openat_nofollow(cur_fd, c, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Ok(fd) => fd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cc = cs(c)?;
                if unsafe { libc::mkdirat(cur_fd, cc.as_ptr(), dir_mode as libc::mode_t) } != 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(err);
                    }
                }
                openat_nofollow(cur_fd, c, libc::O_RDONLY | libc::O_DIRECTORY, 0)?
            }
            Err(e) => return Err(e),
        };
        held = Some(next);
    }
    let dir = held.as_ref().map_or(root_fd, |f| f.as_fd());
    write_dir_fd_nofollow(dir, &name, data, append, atomic, create_new, mode)
}

pub(super) fn write_anchored(
    root: &Path,
    path: &Path,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    mode: Option<u32>,
) -> std::io::Result<()> {
    use std::path::Component;
    let norm_root = normalize_lexical(root);
    let norm_path = normalize_lexical(path);
    let rel = norm_path.strip_prefix(&norm_root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("anchored write path {} is outside root {}", path.display(), root.display()),
        )
    })?;
    if rel.as_os_str().is_empty()
        || rel.file_name().is_none()
        || !rel.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid anchored write relative path: {}", rel.display()),
        ));
    }
    let private = mode.is_some_and(|m| m & 0o077 == 0);
    if !norm_root.exists() {
        if private {
            mare_shared::private::create_dir_all(&norm_root)?;
        } else {
            std::fs::create_dir_all(&norm_root)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        use std::os::unix::fs::OpenOptionsExt;
        let root_file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&norm_root)?;
        #[cfg(target_os = "linux")]
        if let Some(res) = write_beneath_fd(root_file.as_fd(), rel, data, append, atomic, create_new, mode) {
            return res;
        }
        write_anchored_nofollow(root_file.as_fd(), rel, data, append, atomic, create_new, mode)
    }
    #[cfg(windows)]
    {
        write_anchored_windows(&norm_root, rel, data, append, atomic, create_new, mode, private)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (data, append, atomic, create_new, mode, private);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "anchored root writes are unsupported"))
    }
}

#[cfg(windows)]
fn is_windows_reparse_or_symlink(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

#[cfg(windows)]
fn open_win_pinned_dir(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let meta = dir.metadata()?;
    if is_windows_reparse_or_symlink(&meta) || !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing reparse point or non-directory: {}", path.display()),
        ));
    }
    Ok(dir)
}

#[cfg(windows)]
fn win_path_within(child: &Path, root: &Path) -> bool {
    dunce::simplified(child).starts_with(dunce::simplified(root))
}

#[cfg(windows)]
fn open_win_parent_dir_from_root(
    norm_root: &Path,
    rel: &Path,
    create_parents: bool,
    private: bool,
    policy: Option<&crate::ossandbox::Policy>,
) -> std::io::Result<(Vec<std::fs::File>, PathBuf, std::ffi::OsString)> {
    let name = rel
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name"))?
        .to_owned();
    let root_handle = open_win_pinned_dir(norm_root)?;
    let root_opened = crate::fs::opened_path(&root_handle)?;
    let mut cur_opened = root_opened.clone();
    let mut pinned = vec![root_handle];
    let parent = rel.parent().unwrap_or(Path::new(""));
    for comp in parent.components() {
        let next = cur_opened.join(comp.as_os_str());
        let handle = match open_win_pinned_dir(&next) {
            Ok(h) => h,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create_parents => {
                if let Some(pol) = policy
                    && !pol.can_write(&next)
                {
                    return Err(deny_write(&next, pol));
                }
                let res =
                    if private { mare_shared::private::create_dir_new(&next) } else { std::fs::create_dir(&next) };
                match res {
                    Ok(()) => open_win_pinned_dir(&next)?,
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => open_win_pinned_dir(&next)?,
                    Err(err) => return Err(err),
                }
            }
            Err(e) => return Err(e),
        };
        cur_opened = crate::fs::opened_path(&handle)?;
        if !win_path_within(&cur_opened, &root_opened) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("anchored path escaped root: {}", cur_opened.display()),
            ));
        }
        pinned.push(handle);
    }
    Ok((pinned, cur_opened, name))
}

#[cfg(windows)]
fn win_rename_in_dir(src_path: &Path, dst_path: &Path, replace_if_exists: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_RENAME_INFO, FILE_RENAME_INFO_0,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileRenameInfo, SYNCHRONIZE, SetFileInformationByHandle,
    };
    let name_wide: Vec<u16> = dst_path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing Windows file name"))?
        .encode_wide()
        .collect();
    if name_wide.is_empty()
        || name_wide.contains(&0)
        || name_wide.contains(&(b'\\' as u16))
        || name_wide.contains(&(b'/' as u16))
    {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid Windows file name"));
    }
    let wide: Vec<u16> = dst_path.as_os_str().encode_wide().collect();
    if wide.is_empty() || wide.contains(&0) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid Windows destination path"));
    }
    let src_file = std::fs::OpenOptions::new()
        .access_mode(DELETE | SYNCHRONIZE)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(src_path)?;
    let byte_len = u32::try_from(wide.len().saturating_mul(2))
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file name too long"))?;
    let header_bytes = std::mem::offset_of!(FILE_RENAME_INFO, FileName);
    let total_bytes = header_bytes + ((wide.len() + 1) * 2);
    let mut storage = vec![0u64; total_bytes.div_ceil(std::mem::size_of::<u64>())];
    let info_ptr = storage.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    unsafe {
        std::ptr::write(
            info_ptr,
            FILE_RENAME_INFO {
                Anonymous: FILE_RENAME_INFO_0 { ReplaceIfExists: replace_if_exists },
                RootDirectory: std::ptr::null_mut(),
                FileNameLength: byte_len,
                FileName: [0],
            },
        );
        let dst_slice = std::slice::from_raw_parts_mut((*info_ptr).FileName.as_mut_ptr(), wide.len() + 1);
        dst_slice[..wide.len()].copy_from_slice(&wide);
        dst_slice[wide.len()] = 0;
        let ok =
            SetFileInformationByHandle(src_file.as_raw_handle(), FileRenameInfo, info_ptr.cast(), total_bytes as u32);
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub(super) fn write_anchored_windows(
    norm_root: &Path,
    rel: &Path,
    data: &[u8],
    append: bool,
    atomic: bool,
    create_new: bool,
    _mode: Option<u32>,
    private: bool,
) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let (pinned, dir_path, name) = open_win_parent_dir_from_root(norm_root, rel, true, private, None)?;
    let target = dir_path.join(&name);
    let existing_meta = match std::fs::symlink_metadata(&target) {
        Ok(meta) if is_windows_reparse_or_symlink(&meta) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing reparse point target in anchored root write: {}", target.display()),
            ));
        }
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if atomic && !append {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp_name = format!(
            ".{}.tmp-{}-{}",
            name.to_string_lossy(),
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let tmp_path = dir_path.join(&tmp_name);
        let mut oo = std::fs::OpenOptions::new();
        oo.write(true).create_new(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let mut f = if private { mare_shared::private::create_new(&tmp_path)? } else { oo.open(&tmp_path)? };
        let written = f.write_all(data).and_then(|()| f.sync_all()).and_then(|()| match existing_meta.as_ref() {
            Some(m) => f.set_permissions(m.permissions()),
            None => Ok(()),
        });
        drop(f);
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(e);
        }
        let res = win_rename_in_dir(&tmp_path, &target, !create_new).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp_path);
        });
        drop(pinned);
        return res;
    }
    if create_new {
        if private {
            return mare_shared::private::create_new(&target)?.write_all(data);
        }
        return std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&target)?
            .write_all(data);
    }
    if private && existing_meta.is_some() {
        mare_shared::private::restrict(&target)?;
    }
    let mut oo = std::fs::OpenOptions::new();
    oo.write(true).create(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    if append {
        oo.append(true);
    } else {
        oo.truncate(true);
    }
    if private {
        return mare_shared::private::open(&mut oo, &target)?.write_all(data);
    }
    oo.open(&target)?.write_all(data)
}

fn outside_writable_err(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("sandbox: {} changed after the policy check (now outside the writable roots)", path.display()),
    )
}

#[cfg(unix)]
fn check_dir_entry_policy(path: &Path, pol: &crate::ossandbox::Policy, write_only: bool) -> std::io::Result<()> {
    if !pol.can_write(path) {
        return Err(deny_write(path, pol));
    }
    if !write_only && !pol.can_read(path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("sandbox: read {} denied {}", path.display(), pol.denied_marker()),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn remove_dir_all_fd(
    parent_fd: i32,
    name: &Path,
    target_path: &Path,
    pol: &crate::ossandbox::Policy,
) -> std::io::Result<()> {
    use std::ffi::{CStr, CString};
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let cs = |p: &Path| CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let open_child_dir = |dfd: i32, entry: &Path| -> std::io::Result<OwnedFd> {
        #[cfg(target_os = "linux")]
        match open_beneath_fd(dfd, entry, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC, 0) {
            Ok(fd) => return Ok(fd),
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => {}
            Err(e) => return Err(e),
        }
        let c = cs(entry)?;
        let fd = unsafe {
            libc::openat(dfd, c.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC, 0)
        };
        if fd < 0 { Err(std::io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
    };
    check_dir_entry_policy(target_path, pol, true)?;
    let dir_fd = open_child_dir(parent_fd, name)?;
    let dup_fd = dir_fd.try_clone()?;
    let dir_ptr = unsafe { libc::fdopendir(dup_fd.into_raw_fd()) };
    if dir_ptr.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    let mut entries = Vec::new();
    loop {
        let next = unsafe { libc::readdir(dir_ptr) };
        if next.is_null() {
            break;
        }
        let raw_name = unsafe { CStr::from_ptr((*next).d_name.as_ptr()) }.to_bytes();
        if raw_name == b"." || raw_name == b".." {
            continue;
        }
        entries.push(std::ffi::OsString::from_vec(raw_name.to_vec()));
    }
    unsafe { libc::closedir(dir_ptr) };
    for entry in entries {
        let entry_p = Path::new(&entry);
        let full = target_path.join(entry_p);
        check_dir_entry_policy(&full, pol, true)?;
        let c_entry = cs(entry_p)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir_fd.as_raw_fd(), c_entry.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::NotFound {
                continue;
            }
            return Err(err);
        }
        if (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            remove_dir_all_fd(dir_fd.as_raw_fd(), entry_p, &full, pol)?;
        } else if unsafe { libc::unlinkat(dir_fd.as_raw_fd(), c_entry.as_ptr(), 0) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::NotFound {
                return Err(err);
            }
        }
    }
    let c_name = cs(name)?;
    if unsafe { libc::unlinkat(parent_fd, c_name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn remove_dir_all_win(dir: &Path, pol: &crate::ossandbox::Policy) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(dir)?;
    if is_windows_reparse_or_symlink(&meta) || !meta.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing reparse point during anchored remove: {}", dir.display()),
        ));
    }
    let _pinned = open_win_pinned_dir(dir)?;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let child = entry.path();
        if !pol.can_write(&child) {
            return Err(deny_write(&child, pol));
        }
        let cmeta = std::fs::symlink_metadata(&child)?;
        if cmeta.is_dir() && !is_windows_reparse_or_symlink(&cmeta) {
            remove_dir_all_win(&child, pol)?;
        } else if cmeta.is_dir() {
            std::fs::remove_dir(&child)?;
        } else {
            std::fs::remove_file(&child)?;
        }
    }
    std::fs::remove_dir(dir)
}

pub(crate) fn anchored_mkdir_all(path: &Path, policy: Option<&crate::ossandbox::Policy>) -> std::io::Result<()> {
    let Some(pol) = policy else {
        return std::fs::create_dir_all(path);
    };
    let physical = crate::ossandbox::resolve(path)?;
    if !pol.can_write(path) || !pol.can_write(&physical) {
        return Err(deny_write(&physical, pol));
    }
    let (root, rel) = anchor(&pol.writable, &physical).ok_or_else(|| outside_writable_err(&physical))?;
    if rel.as_os_str().is_empty() {
        if !root.exists() {
            std::fs::create_dir_all(&root)?;
        }
        let meta = std::fs::symlink_metadata(&root)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing symlink as anchored directory: {}", root.display()),
            ));
        }
        return Ok(());
    }
    if !root.exists() {
        std::fs::create_dir_all(&root)?;
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let (dir_fd, final_parent, name) = open_parent_dir_from_root_opts(&root, &rel, None, Some(pol), true)?;
        let leaf_path = final_parent.join(&name);
        check_dir_entry_policy(&leaf_path, pol, true)?;
        let c_name = CString::new(name.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        if unsafe { libc::mkdirat(dir_fd.as_raw_fd(), c_name.as_ptr(), 0o777) } != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(err);
            }
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstatat(dir_fd.as_raw_fd(), c_name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
                return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, err));
            }
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let (_pinned, final_parent, name) = open_win_parent_dir_from_root(&root, &rel, true, false, Some(pol))?;
        let leaf_path = final_parent.join(&name);
        if !pol.can_write(&leaf_path) {
            return Err(deny_write(&leaf_path, pol));
        }
        match std::fs::create_dir(&leaf_path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(&leaf_path)?;
                if is_windows_reparse_or_symlink(&meta) || !meta.is_dir() {
                    return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, err));
                }
                Ok(())
            }
            Err(err) => Err(err),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "anchored mkdir unsupported"))
    }
}

pub(crate) fn anchored_remove(
    path: &Path,
    recursive: bool,
    policy: Option<&crate::ossandbox::Policy>,
) -> std::io::Result<()> {
    let Some(pol) = policy else {
        let meta = std::fs::symlink_metadata(path)?;
        if meta.is_dir() && !meta.file_type().is_symlink() {
            return if recursive { std::fs::remove_dir_all(path) } else { std::fs::remove_dir(path) };
        }
        return std::fs::remove_file(path);
    };
    let leaf =
        path.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name"))?;
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let parent_phys =
        if parent.as_os_str().is_empty() { std::env::current_dir()? } else { crate::ossandbox::resolve(parent)? };
    let target_phys = parent_phys.join(leaf);
    if !pol.can_write(path) || !pol.can_write(&target_phys) {
        return Err(deny_write(&target_phys, pol));
    }
    let (root, rel) = anchor(&pol.writable, &target_phys).ok_or_else(|| outside_writable_err(&target_phys))?;
    if rel.as_os_str().is_empty() {
        return Err(deny_write(&target_phys, pol));
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let (dir_fd, final_parent, name) = open_parent_dir_from_root_opts(&root, &rel, None, Some(pol), false)?;
        let leaf_path = final_parent.join(&name);
        check_dir_entry_policy(&leaf_path, pol, true)?;
        let c_name = CString::new(name.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dir_fd.as_raw_fd(), c_name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            if recursive {
                remove_dir_all_fd(dir_fd.as_raw_fd(), Path::new(&name), &leaf_path, pol)
            } else if unsafe { libc::unlinkat(dir_fd.as_raw_fd(), c_name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        } else if unsafe { libc::unlinkat(dir_fd.as_raw_fd(), c_name.as_ptr(), 0) } != 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    #[cfg(windows)]
    {
        let (_pinned, final_parent, name) = open_win_parent_dir_from_root(&root, &rel, false, false, Some(pol))?;
        let leaf_path = final_parent.join(&name);
        if !pol.can_write(&leaf_path) {
            return Err(deny_write(&leaf_path, pol));
        }
        let meta = std::fs::symlink_metadata(&leaf_path)?;
        if meta.is_dir() && !is_windows_reparse_or_symlink(&meta) {
            if recursive { remove_dir_all_win(&leaf_path, pol) } else { std::fs::remove_dir(&leaf_path) }
        } else if meta.is_dir() {
            std::fs::remove_dir(&leaf_path)
        } else {
            std::fs::remove_file(&leaf_path)
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (root, rel, recursive);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "anchored remove unsupported"))
    }
}

pub(crate) fn anchored_rename(
    src: &Path,
    dst: &Path,
    policy: Option<&crate::ossandbox::Policy>,
) -> std::io::Result<()> {
    let Some(pol) = policy else {
        return std::fs::rename(src, dst);
    };
    let resolve_parent_target = |p: &Path| -> std::io::Result<PathBuf> {
        let leaf =
            p.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing file name"))?;
        let parent = p.parent().unwrap_or_else(|| Path::new(""));
        let parent_phys =
            if parent.as_os_str().is_empty() { std::env::current_dir()? } else { crate::ossandbox::resolve(parent)? };
        Ok(parent_phys.join(leaf))
    };
    let src_phys = resolve_parent_target(src)?;
    let dst_phys = resolve_parent_target(dst)?;
    if !pol.can_write(src) || !pol.can_write(&src_phys) {
        return Err(deny_write(&src_phys, pol));
    }
    if !pol.can_write(dst) || !pol.can_write(&dst_phys) {
        return Err(deny_write(&dst_phys, pol));
    }
    if !pol.can_read(src) || !pol.can_read(&src_phys) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("sandbox: read {} denied {}", src_phys.display(), pol.denied_marker()),
        ));
    }
    if !pol.can_read(dst) || !pol.can_read(&dst_phys) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("sandbox: read {} denied {}", dst_phys.display(), pol.denied_marker()),
        ));
    }
    let (src_root, src_rel) = anchor(&pol.writable, &src_phys).ok_or_else(|| outside_writable_err(&src_phys))?;
    let (dst_root, dst_rel) = anchor(&pol.writable, &dst_phys).ok_or_else(|| outside_writable_err(&dst_phys))?;
    if src_rel.as_os_str().is_empty() || dst_rel.as_os_str().is_empty() {
        return Err(deny_write(&src_phys, pol));
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let (src_dir, src_parent, src_name) =
            open_parent_dir_from_root_opts(&src_root, &src_rel, None, Some(pol), false)?;
        let (dst_dir, dst_parent, dst_name) =
            open_parent_dir_from_root_opts(&dst_root, &dst_rel, None, Some(pol), false)?;
        let src_target = src_parent.join(&src_name);
        let dst_target = dst_parent.join(&dst_name);
        check_dir_entry_policy(&src_target, pol, false)?;
        check_dir_entry_policy(&dst_target, pol, false)?;
        let c_src = CString::new(src_name.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        let c_dst = CString::new(dst_name.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
        if unsafe { libc::renameat(src_dir.as_raw_fd(), c_src.as_ptr(), dst_dir.as_raw_fd(), c_dst.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let (_src_pinned, src_parent, src_name) =
            open_win_parent_dir_from_root(&src_root, &src_rel, false, false, Some(pol))?;
        let (dst_pinned, dst_parent, dst_name) =
            open_win_parent_dir_from_root(&dst_root, &dst_rel, false, false, Some(pol))?;
        let src_target = src_parent.join(&src_name);
        let dst_target = dst_parent.join(&dst_name);
        if !pol.can_write(&src_target) || !pol.can_read(&src_target) {
            return Err(deny_write(&src_target, pol));
        }
        if !pol.can_write(&dst_target) || !pol.can_read(&dst_target) {
            return Err(deny_write(&dst_target, pol));
        }
        if let Ok(meta) = std::fs::symlink_metadata(&dst_target)
            && is_windows_reparse_or_symlink(&meta)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing reparse point target in anchored rename: {}", dst_target.display()),
            ));
        }
        let res = win_rename_in_dir(&src_target, &dst_target, true);
        drop((_src_pinned, dst_pinned));
        res
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (src_root, src_rel, dst_root, dst_rel);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "anchored rename unsupported"))
    }
}
