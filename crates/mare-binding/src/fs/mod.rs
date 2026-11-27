//! `mare.fs`: read/write, stat, list, walk (gitignore-aware), glob.

use std::path::{Path, PathBuf};

mod dir;
mod lua_api;
mod no_follow;
mod path_glob;
mod read;
#[cfg(all(test, unix))]
mod tests;
mod watch;
mod write;

#[cfg(all(test, unix))]
pub(crate) use dir::list_dir_entries;
pub use lua_api::{install, resolve_policy_path};
pub use no_follow::{normalize_lexical, open_dir_no_follow, open_no_follow, open_no_follow_beneath, opened_path};
#[cfg(any(test, debug_assertions))]
pub use read::testing;
pub use read::{read_capped, read_file_capped};
pub use watch::{MAX_WATCHES_PER_CONN, active_watch_count, check_policy};
#[cfg(target_os = "linux")]
pub(crate) use write::open_beneath_fd;
#[cfg(unix)]
pub(crate) use write::open_parent_dir_from_root;
pub use write::{Beneath, WriteOptions, write_file};
pub(crate) use write::{DirectorySyncPlan, capture_directory_sync_plan};

pub fn resolve_path(cwd: &Path, p: &str) -> PathBuf {
    resolve(cwd, p)
}

pub(super) fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let p = mare_shared::paths::expand(p);
    if p.is_absolute() { p } else { cwd.join(p) }
}

pub(super) fn mtime(meta: &std::fs::Metadata) -> f64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}
