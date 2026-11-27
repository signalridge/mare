//! Physical path resolution as the kernel sees it (symlinks per component, `..` after them).

use std::path::{Component, Path, PathBuf};

pub fn expand(p: &str, cwd: &Path) -> PathBuf {
    let pb = mare_shared::paths::expand(p);
    if pb.is_absolute() { pb } else { cwd.join(pb) }
}

pub(crate) fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// The physical path the kernel would open for `p`: symlinks are resolved component by
/// component and `..` applies to the *resolved* parent, as the kernel does (so `ws/link/..` is
/// the parent of link's target, never `ws/`). Components that don't exist yet (a file about to
/// be created) are appended lexically — a missing directory can't be a link. Callers check the
/// returned path and then write to that same path (return the canonical target and act on it).
/// One implementation for every backend (Windows included, `dunce` strips `\\?\`).
pub fn resolve(p: &Path) -> std::io::Result<PathBuf> {
    Ok(mare_shared::paths::resolve_path(p)?.physical)
}

/// Case-folding key for comparing paths on case-insensitive file systems (macOS, Windows):
/// resolved, separators unified, lowercased there; unchanged elsewhere.
pub fn canonical_path_key(p: &Path) -> std::io::Result<String> {
    let s = resolve(p)?.to_string_lossy().into_owned();
    Ok(if cfg!(windows) { s.replace('\\', "/").to_lowercase() } else { s })
}

fn normalize_within_path(s: &str, windows: bool, fold_case: bool) -> String {
    let mut norm =
        if windows { mare_shared::paths::strip_windows_verbatim_str(&s.replace('\\', "/")) } else { s.to_owned() };
    if fold_case {
        norm = norm.to_lowercase();
    }
    while norm.ends_with('/') && norm.len() > 1 {
        if windows && (norm == "//" || (norm.len() == 3 && norm.as_bytes()[1] == b':')) {
            break;
        }
        norm.pop();
    }
    norm
}

fn path_within_normalized(p: &Path, root: &Path, windows: bool, fold_case: bool) -> bool {
    let p = normalize_within_path(&p.to_string_lossy(), windows, fold_case);
    let r = normalize_within_path(&root.to_string_lossy(), windows, fold_case);
    if r.is_empty() {
        return p.is_empty();
    }
    p == r
        || if r == "/" || (windows && r.len() == 3 && r.ends_with(":/")) {
            p.starts_with(&r)
        } else {
            p.strip_prefix(&r).is_some_and(|rest| rest.starts_with('/'))
        }
}

/// `p` is `root` or inside it, normalizing trailing slashes on both sides and folding case when
/// required by the platform or `conservative` mode.
pub fn path_within_with(p: &Path, root: &Path, conservative: bool) -> bool {
    let fold_case = cfg!(windows) || (cfg!(target_os = "macos") && conservative);
    path_within_normalized(p, root, cfg!(windows), fold_case)
}

/// `p` is `root` or inside it, compared case-insensitively where the file system is
/// (`.GIT/config` is `.git/config` on macOS and Windows). Both sides are already resolved.
pub fn path_within(p: &Path, root: &Path) -> bool {
    path_within_with(p, root, true)
}

const MAX_DOT_GIT_BYTES: u64 = 65_536;

/// Read a `.git` pointer file (`gitdir: <path>`) with a strict 64 KiB cap; returns `Err` on
/// I/O failure or when the file exceeds the limit, and `Ok(None)` when not a pointer or target
/// does not exist.
pub(crate) fn read_gitdir_from_file(dot_git: &Path) -> std::io::Result<Option<PathBuf>> {
    use std::io::Read;

    let file = std::fs::File::open(dot_git)?;
    let mut buf = Vec::new();
    file.take(MAX_DOT_GIT_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_DOT_GIT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds {MAX_DOT_GIT_BYTES}-byte limit", dot_git.display()),
        ));
    }
    let Ok(text) = std::str::from_utf8(&buf) else {
        return Ok(None);
    };
    let Some((prefix, raw)) = text.trim().split_once(':') else {
        return Ok(None);
    };
    if prefix.trim() != "gitdir" || raw.trim().is_empty() {
        return Ok(None);
    }
    let Some(parent) = dot_git.parent() else {
        return Ok(None);
    };
    let p = parent.join(raw.trim());
    Ok(resolve(&p).ok().filter(|p| p.exists()))
}

/// Read `<gitdir>/commondir` (bounded and symlink-safe) for a linked Git worktree.
/// Returns `Ok(None)` when absent or empty, `Err` if symlinked, non-regular, unreadable,
/// oversized, or pointing outside a valid directory.
pub(crate) fn read_commondir_from_gitdir(gitdir: &Path) -> std::io::Result<Option<PathBuf>> {
    use std::io::Read;

    let commondir_file = gitdir.join("commondir");
    let meta = match std::fs::symlink_metadata(&commondir_file) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("refusing symlinked commondir: {}", commondir_file.display()),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing reparse point commondir: {}", commondir_file.display()),
            ));
        }
    }
    if !meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("commondir is not a regular file: {}", commondir_file.display()),
        ));
    }
    let mut oo = std::fs::OpenOptions::new();
    oo.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        oo.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = oo.open(&commondir_file)?;
    let opened_meta = file.metadata()?;
    if !opened_meta.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("commondir is not a regular file: {}", commondir_file.display()),
        ));
    }
    let mut buf = Vec::new();
    file.take(MAX_DOT_GIT_BYTES + 1).read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_DOT_GIT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{} exceeds {MAX_DOT_GIT_BYTES}-byte limit", commondir_file.display()),
        ));
    }
    let text = std::str::from_utf8(&buf).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{} is not valid UTF-8", commondir_file.display()))
    })?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let candidate = gitdir.join(trimmed);
    let resolved = resolve(&candidate)?;
    if !resolved.is_dir() || resolved.parent().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid commondir target: {}", resolved.display()),
        ));
    }
    Ok(Some(resolved))
}

/// The gitdir a `.git` pointer file names (`gitdir: <path>`, relative to the file's directory);
/// `None` when it isn't a pointer, exceeds the bounded read limit, or the target doesn't exist.
pub fn gitdir_from_file(dot_git: &Path) -> Option<PathBuf> {
    read_gitdir_from_file(dot_git).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_within_handles_trailing_slashes_and_case_folding() {
        assert!(path_within_with(Path::new("/ws/docs"), Path::new("/ws/docs/"), false));
        assert!(path_within_with(Path::new("/ws/docs/"), Path::new("/ws/docs"), false));
        assert!(path_within_with(Path::new("/ws/docs/a.md"), Path::new("/ws/docs/"), false));
        assert!(path_within_with(Path::new("/"), Path::new("/"), false));
        assert!(!path_within_with(Path::new("/ws/docs2"), Path::new("/ws/docs/"), false));

        assert!(path_within_normalized(Path::new(r"C:\WS\Docs\"), Path::new("c:/ws/docs/"), true, true));
        assert!(path_within_normalized(Path::new(r"\\?\C:\WS\Docs\"), Path::new("c:/ws/docs/"), true, true));
        assert!(path_within_normalized(
            Path::new(r"\\?\UNC\srv\share\Docs\"),
            Path::new(r"\\SRV\share\docs"),
            true,
            true
        ));
        assert!(path_within_normalized(Path::new(r"C:\ws"), Path::new(r"C:\"), true, true));
        assert!(!path_within_normalized(Path::new(r"C:\ws2"), Path::new(r"C:\ws\"), true, true));
    }

    #[test]
    fn commondir_from_gitdir_resolves_and_rejects_symlinks_and_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        let common = root.join("main-repo/.git");
        let worktree_gd = common.join("worktrees/wt1");
        std::fs::create_dir_all(&worktree_gd).unwrap();
        std::fs::write(worktree_gd.join("commondir"), "../..\n").unwrap();
        assert_eq!(read_commondir_from_gitdir(&worktree_gd).unwrap(), Some(common.clone()));

        #[cfg(unix)]
        {
            let linked_gd = root.join("linked-gd");
            std::fs::create_dir_all(&linked_gd).unwrap();
            let real_commondir = root.join("real-commondir");
            std::fs::write(&real_commondir, "../..\n").unwrap();
            std::os::unix::fs::symlink(&real_commondir, linked_gd.join("commondir")).unwrap();
            assert!(read_commondir_from_gitdir(&linked_gd).is_err());
        }

        let big_gd = root.join("big-gd");
        std::fs::create_dir_all(&big_gd).unwrap();
        let mut content = format!("{}\n", common.display());
        content.push_str(&" ".repeat(70_000));
        std::fs::write(big_gd.join("commondir"), content).unwrap();
        assert!(read_commondir_from_gitdir(&big_gd).is_err());
    }

    #[test]
    fn gitdir_from_file_bounds_oversized_dot_git() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real-gd");
        std::fs::create_dir_all(&target).unwrap();
        let dot_git = dir.path().join(".git");
        let mut content = format!("gitdir: {}\n", target.display());
        content.push_str(&" ".repeat(70_000));
        std::fs::write(&dot_git, content).unwrap();

        let err = read_gitdir_from_file(&dot_git).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(gitdir_from_file(&dot_git).is_none());
    }
}
