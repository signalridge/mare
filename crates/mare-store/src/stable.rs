//! Lock-free reads checked by a `stat` before and after (`FileStamp`), read again on change.

use super::*;
use crate::lock::file_id;

/// What a `stat` says about a file: identity, length and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: u64,
}

/// Stronger fork observation identity. `ctime` detects same-inode rewrites whose size and mtime
/// are restored; it is kept separate so rename-and-restore of an existing held reader remains
/// valid under [`FileStamp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForkStamp {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub mtime_ns: u64,
    pub ctime_ns: u64,
}

/// [`FileStamp`] of the file at `path` now.
pub fn stamp(path: &Path) -> std::io::Result<FileStamp> {
    stamp_file(&File::open(path)?)
}

/// Stamp an already-open handle so validation can be bound to the file that is written.
pub(crate) fn stamp_file(f: &File) -> std::io::Result<FileStamp> {
    let m = f.metadata()?;
    let (dev, ino) = file_id(f)?;
    let mtime_ns =
        m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as u64);
    Ok(FileStamp { dev, ino, len: m.len(), mtime_ns })
}

/// [`ForkStamp`] of the file at `path` now.
pub(crate) fn fork_stamp(path: &Path) -> std::io::Result<ForkStamp> {
    fork_stamp_file(&File::open(path)?)
}

/// Add change time to the stricter, lock-free fork observation stamp.
pub(crate) fn fork_stamp_file(f: &File) -> std::io::Result<ForkStamp> {
    let base = stamp_file(f)?;
    #[cfg(unix)]
    let ctime_ns = {
        use std::os::unix::fs::MetadataExt;
        let m = f.metadata()?;
        (m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128) as u64
    };
    #[cfg(not(unix))]
    let ctime_ns = 0;
    Ok(ForkStamp { dev: base.dev, ino: base.ino, len: base.len, mtime_ns: base.mtime_ns, ctime_ns })
}

/// Reads per lock-free read: the read and one re-read when the file changed during it.
pub const STABLE_READ_TRIES: usize = 2;

/// Run a lock-free read with a `stat` (inode, length, mtime) before and after it
/// (the length may only grow): if anything changed, the
/// result may include a batch the writer is rolling back — read once more. Returns the last
/// result and whether it was stable (unstable: refuse forks, mark read-only views).
pub fn stable_read<T>(path: &Path, mut read: impl FnMut() -> Result<T>) -> Result<(T, bool)> {
    let mut last = None;
    for _ in 0..STABLE_READ_TRIES {
        let before = stamp(path).with_context(|| format!("opening {}", path.display()))?;
        let v = read()?;
        let after = stamp(path).ok();
        if after == Some(before) && !forced_unstable(path) {
            return Ok((v, true));
        }
        last = Some(v);
    }
    Ok((last.expect("STABLE_READ_TRIES > 0"), false))
}

/// Test seam (debug builds only; release ignores it): `MARE_TEST_UNSTABLE_READ=<file name>`
/// makes every read of that file look like it changed mid-read, so "still being written" paths
/// are tested deterministically instead of racing a real writer.
pub(crate) fn forced_unstable(path: &Path) -> bool {
    #[cfg(debug_assertions)]
    {
        std::env::var_os("MARE_TEST_UNSTABLE_READ")
            .is_some_and(|n| !n.is_empty() && path.file_name() == Some(n.as_os_str()))
    }
    #[cfg(not(debug_assertions))]
    {
        let _ = path;
        false
    }
}
