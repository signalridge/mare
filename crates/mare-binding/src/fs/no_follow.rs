use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use super::open_beneath_fd;

pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::Prefix(p) => out.push(p.as_os_str()),
            std::path::Component::RootDir => out.push(comp.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    // Don't pop past root; keep `..` only for relative paths with no parent left.
                    if !path.is_absolute() {
                        out.push(comp.as_os_str());
                    }
                }
            }
            std::path::Component::Normal(s) => out.push(s),
        }
    }
    out
}

#[cfg(target_os = "macos")]
fn normalize_macos_system_alias(abs: &Path) -> PathBuf {
    for alias in ["var", "tmp", "etc"] {
        if let Ok(rest) = abs.strip_prefix(Path::new("/").join(alias)) {
            return Path::new("/private").join(alias).join(rest);
        }
    }
    abs.to_path_buf()
}

/// Open `path` without following symlinks in any component, anchoring relative paths to `root`
/// (or `current_dir`) and walking from `/` without prior lexical `..` collapse so missing or
/// symlinked intermediate components cannot be bypassed.
pub fn open_no_follow(path: &Path, root: Option<&Path>) -> std::io::Result<std::fs::File> {
    open_no_follow_inner(path, root, false)
}

/// Open directory `path` without following symlinks in any component.
pub fn open_dir_no_follow(path: &Path, root: Option<&Path>) -> std::io::Result<std::fs::File> {
    open_no_follow_inner(path, root, true)
}

/// Open relative regular file `rel` strictly beneath `root`, verifying `root` itself is a
/// non-symlink directory and refusing symlinks/traversal in every component of `rel`.
pub fn open_no_follow_beneath(root: &Path, rel: &Path) -> std::io::Result<std::fs::File> {
    if rel.is_absolute()
        || !rel.components().all(|c| matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "relative non-traversing path required beneath project root",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;

        let root_dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)?;
        let root_meta = root_dir.metadata()?;
        if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
            return Err(std::io::Error::other("project root must be a directory, not a symlink"));
        }
        let leaf_flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        let file = {
            #[cfg(target_os = "linux")]
            {
                match open_beneath_fd(root_dir.as_raw_fd(), rel, leaf_flags, 0) {
                    Ok(fd) => Some(std::fs::File::from(fd)),
                    Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => None,
                    Err(e) => return Err(e),
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                None::<std::fs::File>
            }
        };
        let file = match file {
            Some(f) => f,
            None => {
                let parts: Vec<&std::ffi::OsStr> = rel
                    .components()
                    .filter_map(|c| match c {
                        std::path::Component::Normal(p) => Some(p),
                        _ => None,
                    })
                    .collect();
                let Some((last_name, dirs)) = parts.split_last() else {
                    return Err(std::io::Error::other("path is a directory, not a regular file"));
                };
                let mut cur = root_dir.try_clone()?;
                for seg in dirs {
                    let c = std::ffi::CString::new(seg.as_bytes())
                        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
                    let fd = unsafe {
                        libc::openat(
                            cur.as_raw_fd(),
                            c.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    cur = unsafe { std::fs::File::from_raw_fd(fd) };
                }
                let c = std::ffi::CString::new(last_name.as_bytes())
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL byte"))?;
                let fd = unsafe { libc::openat(cur.as_raw_fd(), c.as_ptr(), leaf_flags) };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                unsafe { std::fs::File::from_raw_fd(fd) }
            }
        };
        let meta = file.metadata()?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let canon_root = dunce::canonicalize(opened_path(&root_dir)?)?;
        let canon_file = dunce::canonicalize(opened_path(&file)?)?;
        if !canon_file.starts_with(&canon_root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "opened path escaped verified project root",
            ));
        }
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let root_dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(root)?;
        let root_meta = root_dir.metadata()?;
        if root_meta.file_type().is_symlink() || (root_meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(std::io::Error::other("project root must not be a symlink or junction"));
        }
        let canon_root = dunce::canonicalize(opened_path(&root_dir)?)?;
        let file = open_no_follow(rel, Some(&canon_root))?;
        let canon_file = dunce::canonicalize(opened_path(&file)?)?;
        if !canon_file.starts_with(&canon_root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "opened path escaped verified project root",
            ));
        }
        Ok(file)
    }
}

fn open_no_follow_inner(path: &Path, root: Option<&Path>, expect_dir: bool) -> std::io::Result<std::fs::File> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(r) = root {
        r.join(path)
    } else {
        std::env::current_dir()?.join(path)
    };
    #[cfg(target_os = "macos")]
    let abs = normalize_macos_system_alias(&abs);
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;

        let rel = abs.strip_prefix("/").map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "expected absolute path after anchoring")
        })?;
        let base =
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC).open("/")?;
        if rel.as_os_str().is_empty() {
            return if expect_dir {
                Ok(base)
            } else {
                Err(std::io::Error::other("path is a directory, not a regular file"))
            };
        }

        let leaf_flags = if expect_dir {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC
        };

        let file = {
            #[cfg(target_os = "linux")]
            {
                match open_beneath_fd(base.as_raw_fd(), rel, leaf_flags, 0) {
                    Ok(fd) => Some(std::fs::File::from(fd)),
                    Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => None,
                    Err(e) => return Err(e),
                }
            }
            #[cfg(not(target_os = "linux"))]
            {
                None::<std::fs::File>
            }
        };

        let file = match file {
            Some(f) => f,
            None => {
                let mut parts = Vec::new();
                for comp in rel.components() {
                    match comp {
                        std::path::Component::Normal(p) => parts.push(std::path::Component::Normal(p)),
                        std::path::Component::ParentDir => parts.push(std::path::Component::ParentDir),
                        std::path::Component::CurDir => {}
                        _ => {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "path contains disallowed traversal component",
                            ));
                        }
                    }
                }
                let Some((last, dirs)) = parts.split_last() else {
                    return if expect_dir {
                        Ok(base)
                    } else {
                        Err(std::io::Error::other("path is a directory, not a regular file"))
                    };
                };
                let std::path::Component::Normal(last_name) = last else {
                    return Err(std::io::Error::other("path ends with traversal component"));
                };
                let mut cur = base;
                let mut depth: usize = 0;
                for dir in dirs {
                    let seg: &[u8] = match dir {
                        std::path::Component::Normal(p) => {
                            depth += 1;
                            p.as_bytes()
                        }
                        std::path::Component::ParentDir => {
                            if depth == 0 {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidInput,
                                    "path escapes filesystem root",
                                ));
                            }
                            depth -= 1;
                            b".."
                        }
                        _ => continue,
                    };
                    let c = std::ffi::CString::new(seg).map_err(std::io::Error::other)?;
                    let fd = unsafe {
                        libc::openat(
                            cur.as_raw_fd(),
                            c.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if fd < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    cur = unsafe { std::fs::File::from_raw_fd(fd) };
                }
                let c = std::ffi::CString::new(last_name.as_bytes()).map_err(std::io::Error::other)?;
                let fd = unsafe { libc::openat(cur.as_raw_fd(), c.as_ptr(), leaf_flags) };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                unsafe { std::fs::File::from_raw_fd(fd) }
            }
        };

        let meta = file.metadata()?;
        if expect_dir {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(std::io::Error::other("not a directory"));
            }
        } else if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

        let mut prefix = PathBuf::new();
        let mut rest = Vec::new();
        for comp in abs.components() {
            match comp {
                std::path::Component::Prefix(p) => prefix.push(p.as_os_str()),
                std::path::Component::RootDir => prefix.push(comp.as_os_str()),
                std::path::Component::Normal(s) => rest.push(std::path::Component::Normal(s)),
                std::path::Component::ParentDir => rest.push(std::path::Component::ParentDir),
                std::path::Component::CurDir => {}
            }
        }
        let Some((last, dirs)) = rest.split_last() else {
            if expect_dir {
                let dir = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(&prefix)?;
                return Ok(dir);
            }
            return Err(std::io::Error::other("path is a directory, not a regular file"));
        };
        let std::path::Component::Normal(last_name) = last else {
            return Err(std::io::Error::other("path ends with traversal component"));
        };
        let mut cur_path = prefix;
        let mut depth: usize = 0;
        for dir in dirs {
            match dir {
                std::path::Component::Normal(s) => {
                    depth += 1;
                    cur_path.push(s);
                }
                std::path::Component::ParentDir => {
                    if depth == 0 {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "path escapes volume root"));
                    }
                    depth -= 1;
                    let probe = cur_path.join("..");
                    let dir_handle = std::fs::OpenOptions::new()
                        .read(true)
                        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                        .open(&probe)?;
                    let meta = dir_handle.metadata()?;
                    if meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                        return Err(std::io::Error::other("symbolic links and junctions are not allowed"));
                    }
                    cur_path.pop();
                    let actual = opened_path(&dir_handle)?;
                    if !dunce::simplified(&actual)
                        .as_os_str()
                        .eq_ignore_ascii_case(dunce::simplified(&cur_path).as_os_str())
                    {
                        return Err(std::io::Error::other("directory component changed during open"));
                    }
                    continue;
                }
                _ => continue,
            }
            let dir_handle = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&cur_path)?;
            let meta = dir_handle.metadata()?;
            if meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
                return Err(std::io::Error::other("symbolic links and junctions are not allowed"));
            }
            let actual = opened_path(&dir_handle)?;
            if !dunce::simplified(&actual).as_os_str().eq_ignore_ascii_case(dunce::simplified(&cur_path).as_os_str()) {
                return Err(std::io::Error::other("directory component changed during open"));
            }
        }
        cur_path.push(last_name);
        let flags = if expect_dir {
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT
        } else {
            FILE_FLAG_OPEN_REPARSE_POINT
        };
        let file = std::fs::OpenOptions::new().read(true).custom_flags(flags).open(&cur_path)?;
        let meta = file.metadata()?;
        if meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
            return Err(std::io::Error::other("symbolic links and junctions are not allowed"));
        }
        if expect_dir && !meta.is_dir() {
            return Err(std::io::Error::other("not a directory"));
        }
        if !expect_dir && !meta.is_file() {
            return Err(std::io::Error::other("not a regular file"));
        }
        let actual = opened_path(&file)?;
        if !dunce::simplified(&actual).as_os_str().eq_ignore_ascii_case(dunce::simplified(&cur_path).as_os_str()) {
            return Err(std::io::Error::other("file target changed during open"));
        }
        Ok(file)
    }
}

/// Resolve the kernel path of an already-opened file handle.
pub fn opened_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
    }
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        use std::os::fd::AsRawFd;
        use std::os::unix::ffi::OsStrExt;
        let mut buf = [0u8; libc::PATH_MAX as usize];
        let rc = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) };
        if rc < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let cstr = unsafe { CStr::from_ptr(buf.as_ptr().cast()) };
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(cstr.to_bytes())))
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_DOS};
        let handle = file.as_raw_handle();
        let needed = unsafe { GetFinalPathNameByHandleW(handle, std::ptr::null_mut(), 0, VOLUME_NAME_DOS) };
        if needed == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut buf = vec![0u16; needed as usize];
        let len = unsafe { GetFinalPathNameByHandleW(handle, buf.as_mut_ptr(), needed, VOLUME_NAME_DOS) };
        if len == 0 || len >= needed {
            return Err(std::io::Error::last_os_error());
        }
        let raw = std::ffi::OsString::from_wide(&buf[..len as usize]);
        let s = raw.to_string_lossy();
        if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
            Ok(PathBuf::from(format!(r"\\{unc}")))
        } else if let Some(stripped) = s.strip_prefix(r"\\?\") {
            Ok(PathBuf::from(stripped))
        } else {
            Ok(PathBuf::from(raw))
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = file;
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "opened_path is not supported on this platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_lexical_and_open_no_follow_beneath_reject_traversal() {
        assert_eq!(normalize_lexical(Path::new("./a/./b/../c")), PathBuf::from("a/c"));
        assert_eq!(normalize_lexical(Path::new("../x/y")), PathBuf::from("../x/y"));
        assert_eq!(normalize_lexical(Path::new("/../../etc")), PathBuf::from("/etc"));

        let dir = tempfile::tempdir().unwrap();
        let err = open_no_follow_beneath(dir.path(), Path::new("../escape")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err_abs = open_no_follow_beneath(dir.path(), Path::new("/etc/passwd")).unwrap_err();
        assert_eq!(err_abs.kind(), std::io::ErrorKind::InvalidInput);
    }
}
