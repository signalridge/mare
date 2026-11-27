//! Filesystem-only discovery of Git worktree markers.

use std::io::Read;
use std::path::{Path, PathBuf};

/// What a directory's `.git` entry means to Git's upward discovery (setup.c
/// `setup_git_directory_gently_1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Marker {
    /// No `.git`, or one discovery passes over: a directory that is not a Git directory (an empty
    /// mount point), something neither a file nor a directory, or one that cannot be stat'ed.
    Skip,
    /// A valid repository: its (worktree-specific) Git directory.
    Found(PathBuf),
    /// A regular `.git` file that is not a gitfile naming a valid Git directory: Git stops here
    /// (`GIT_DIR_INVALID_GITFILE`, "not a git repository") instead of looking further up.
    Stop,
}

/// Return a worktree's validated Git directory, without consulting Git environment overrides.
pub fn git_dir(root: &Path) -> Option<PathBuf> {
    git_dir_checked(root, |_| true)
}

/// Like [`git_dir`], but check every path before accessing it (for sandboxed callers).
pub fn git_dir_checked(root: &Path, can_read: impl FnMut(&Path) -> bool) -> Option<PathBuf> {
    match marker_checked(root, can_read) {
        Marker::Found(dir) => Some(dir),
        Marker::Skip | Marker::Stop => None,
    }
}

/// The meaning of `root/.git` for discovery, without consulting Git environment overrides.
pub fn marker(root: &Path) -> Marker {
    marker_checked(root, |_| true)
}

/// Whether `dir/.git` is there at all (or cannot be told apart from missing): the guard for code
/// that must not replace or delete a directory holding any `.git` entry, valid or not.
pub fn marker_present(dir: &Path) -> bool {
    !matches!(std::fs::symlink_metadata(dir.join(".git")), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Like [`marker`], but check every path before accessing it (for sandboxed callers).
pub fn marker_checked(root: &Path, mut can_read: impl FnMut(&Path) -> bool) -> Marker {
    let marker = root.join(".git");
    if !can_read(&marker) {
        return Marker::Skip;
    }
    let Ok(meta) = std::fs::metadata(&marker) else { return Marker::Skip };
    if meta.is_dir() {
        return if is_git_directory(&marker, &mut can_read) { Marker::Found(marker) } else { Marker::Skip };
    }
    if !meta.is_file() {
        return Marker::Skip;
    }
    // a regular file: a gitfile naming a valid Git directory, or the end of the walk
    let named = (meta.len() <= 1 << 20).then(|| read(&marker, 1 << 20)).flatten().and_then(|bytes| {
        let name = trim_newlines(bytes.strip_prefix(b"gitdir: ")?);
        if name.is_empty() { None } else { path(name).map(|p| root.join(p)) }
    });
    match named {
        Some(gitdir) if is_git_directory(&gitdir, &mut can_read) => Marker::Found(gitdir),
        _ => Marker::Stop,
    }
}

fn read(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path).ok()?.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn path(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.contains(&0) {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        Some(PathBuf::from(std::str::from_utf8(bytes).ok()?))
    }
}

fn trim_newlines(mut bytes: &[u8]) -> &[u8] {
    while bytes.last().is_some_and(|c| *c == b'\n' || *c == b'\r') {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

/// Git setup.c `is_git_directory`: a valid HEAD in the worktree-specific directory, then `objects`
/// and `refs` in its common directory (named by a worktree's relative `commondir` file).
fn is_git_directory(gitdir: &Path, can_read: &mut impl FnMut(&Path) -> bool) -> bool {
    is_git_directory_inner(gitdir, can_read).is_some()
}

fn is_git_directory_inner(gitdir: &Path, can_read: &mut impl FnMut(&Path) -> bool) -> Option<()> {
    let head = gitdir.join("HEAD");
    if !can_read(&head) {
        return None;
    }
    let head_meta = std::fs::symlink_metadata(&head).ok()?;
    let valid_head = if head_meta.file_type().is_symlink() {
        std::fs::read_link(&head).ok()?.to_string_lossy().starts_with("refs/")
    } else if head_meta.is_file() {
        let mut content = Vec::new();
        std::fs::File::open(&head).ok()?.take(255).read_to_end(&mut content).ok()?;
        let symbolic = content.strip_prefix(b"ref:").is_some_and(|s| {
            let s = s.iter().position(|c| !c.is_ascii_whitespace()).map_or(&s[s.len()..], |i| &s[i..]);
            s.starts_with(b"refs/")
        });
        symbolic || content.len() >= 40 && content[..40].iter().all(u8::is_ascii_hexdigit)
    } else {
        false
    };
    if !valid_head {
        return None;
    }
    let common_file = gitdir.join("commondir");
    if !can_read(&common_file) {
        return None;
    }
    let common = if common_file.exists() {
        if !common_file.is_file() {
            return None;
        }
        let bytes = read(&common_file, 1 << 20)?;
        let name = trim_newlines(&bytes);
        if name.is_empty() {
            return None;
        }
        gitdir.join(path(name)?)
    } else {
        gitdir.to_path_buf()
    };
    let objects = common.join("objects");
    let refs = common.join("refs");
    (can_read(&objects) && objects.join(".").is_dir() && can_read(&refs) && refs.join(".").is_dir()).then_some(())
}

/// The nearest containing worktree, as Git discovers it: an invalid `.git` directory is passed
/// over, an invalid `.git` file ends the walk with no repository ([`Marker::Stop`]).
pub fn git_root(cwd: &Path) -> Option<PathBuf> {
    for dir in cwd.ancestors() {
        match marker(dir) {
            Marker::Found(_) => return Some(dir.to_path_buf()),
            Marker::Stop => return None,
            Marker::Skip => {}
        }
    }
    None
}

/// Synchronously read the current branch name (or 7-character detached HEAD commit prefix) for the
/// nearest validated Git worktree containing `root`, without spawning `git` or consulting Git
/// environment overrides.
pub fn git_branch(root: &Path) -> Option<String> {
    git_branch_checked(root, |_| true)
}

/// Like [`git_branch`], but check every path before accessing it (for sandboxed callers).
pub fn git_branch_checked(root: &Path, mut can_read: impl FnMut(&Path) -> bool) -> Option<String> {
    for dir in root.ancestors() {
        match marker_checked(dir, &mut can_read) {
            Marker::Found(gitdir) => return read_head_branch(&gitdir, &mut can_read),
            Marker::Stop => return None,
            Marker::Skip => {}
        }
    }
    None
}

fn read_head_branch(gitdir: &Path, can_read: &mut impl FnMut(&Path) -> bool) -> Option<String> {
    let head = gitdir.join("HEAD");
    if !can_read(&head) {
        return None;
    }
    let meta = std::fs::symlink_metadata(&head).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(&head).ok()?;
        let text = target.to_string_lossy();
        let branch = text.strip_prefix("refs/heads/")?.split_ascii_whitespace().next()?;
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    if !meta.is_file() {
        return None;
    }
    let mut content = Vec::new();
    std::fs::File::open(&head).ok()?.take(1024).read_to_end(&mut content).ok()?;
    parse_head_content(&content)
}

fn parse_head_content(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    if let Some(rest) = text.strip_prefix("ref:") {
        let target = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let branch = target.strip_prefix("refs/heads/")?.split_ascii_whitespace().next()?;
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    let trimmed = text.trim();
    if trimmed.len() >= 7 && trimmed.as_bytes()[..7].iter().all(u8::is_ascii_hexdigit) {
        return Some(trimmed[..7].to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_git_dir(root: &Path, head: &str) {
        let git = root.join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("refs")).unwrap();
        std::fs::write(git.join("HEAD"), head).unwrap();
    }

    #[test]
    fn git_branch_reads_symbolic_detached_nested_and_stopped_markers() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(git_branch(dir.path()), None);

        let repo = dir.path().join("repo");
        let nested = repo.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        make_git_dir(&repo, "ref: refs/heads/feat/sync-branch\n");
        assert_eq!(git_branch(&repo).as_deref(), Some("feat/sync-branch"));
        assert_eq!(git_branch(&nested).as_deref(), Some("feat/sync-branch"));

        // Detached HEAD returns the 7-character commit prefix.
        std::fs::write(repo.join(".git/HEAD"), "0123456789abcdef0123456789abcdef01234567\n").unwrap();
        assert_eq!(git_branch(&nested).as_deref(), Some("0123456"));

        // An invalid `.git` file stops upward discovery before reaching the outer repo.
        let blocked = repo.join("blocked/sub");
        std::fs::create_dir_all(&blocked).unwrap();
        std::fs::write(repo.join("blocked/.git"), "not a gitfile\n").unwrap();
        assert_eq!(git_branch(&blocked), None);

        // `can_read` returning false prevents reading HEAD.
        assert_eq!(git_branch_checked(&repo, |p| !p.ends_with("HEAD")), None);
    }

    #[cfg(unix)]
    #[test]
    fn git_branch_reads_symlink_head() {
        let dir = tempfile::tempdir().unwrap();
        let git = dir.path().join(".git");
        std::fs::create_dir_all(git.join("objects")).unwrap();
        std::fs::create_dir_all(git.join("refs")).unwrap();
        std::os::unix::fs::symlink("refs/heads/trunk", git.join("HEAD")).unwrap();
        assert_eq!(git_branch(dir.path()).as_deref(), Some("trunk"));
    }
}
