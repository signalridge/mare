use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::schema::{kv_set, now_ms};
use super::{Index, norm, scan, session_files};

/// One connection per database per process, shared by the Lua bindings and the background
/// reconcile.
pub type Shared = Arc<Mutex<Index>>;

pub(super) static OPEN: Mutex<Vec<(PathBuf, Shared)>> = Mutex::new(vec![]);

/// The process's index of the sessions under `root` (opened on first use). An error means
/// the index can't be used now ("degraded": list by scanning the files instead).
pub fn shared(root: &Path) -> Result<Shared> {
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, s)) = open.iter().find(|(r, _)| r == root) {
        return Ok(s.clone());
    }
    let s = Arc::new(Mutex::new(Index::open(root)?));
    open.push((root.to_path_buf(), s.clone()));
    Ok(s)
}

/// Close an idle cached index before its root is removed by an embedding owner.
/// Active users keep ownership: closing with any outstanding `Shared` fails.
/// Callers must stop work using this root before calling; later use may reopen it.
pub fn close_idle(root: &Path) -> Result<()> {
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(position) = open.iter().position(|(path, _)| path == root) else { return Ok(()) };
    let (path, shared) = open.remove(position);
    // Weak owners can upgrade without taking OPEN's lock; try_unwrap makes the
    // ownership decision atomically instead of relying on a prior strong_count.
    let index = match Arc::try_unwrap(shared) {
        Ok(index) => index.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(shared) => {
            open.insert(position, (path, shared));
            anyhow::bail!("session index {} still has active owners", root.display());
        }
    };
    if let Err((conn, error)) = index.conn.close() {
        open.push((path, Arc::new(Mutex::new(Index { conn, ..index }))));
        return Err(error.into());
    }
    Ok(())
}

/// [`shared`], built from the session files on first use.
pub fn ready(root: &Path) -> Result<Shared> {
    let s = shared(root)?;
    s.lock().unwrap_or_else(|e| e.into_inner()).ensure_built()?;
    Ok(s)
}

/// What a reconcile did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Reconciled {
    /// session files seen
    pub files: usize,
    /// rows caught up or added
    pub updated: usize,
    /// indexed rows removed because their source vanished, became unsupported, or lost an ID collision
    pub removed: usize,
    /// files that could not be indexed (unreadable, no header)
    pub failed: usize,
    /// files whose session writer is currently rolling back; callers may retry later
    pub pending: usize,
    /// Files skipped because their session format version is unsupported.
    pub unsupported: Vec<String>,
}

/// Make the index agree with the session files: stat every file and its session lock (never parsed
/// when the source watermark, physical identity, and rollback revision match), catch up changed
/// rows, add files the index doesn't know, and drop rows whose file is gone. The index lock is
/// taken per file, so listings in this process keep working meanwhile.
fn record_rollback_outcome(path: &Path, out: &mut Reconciled) {
    if crate::lock::session_locked(path).unwrap_or(false) {
        out.pending += 1;
    } else {
        out.failed += 1;
    }
}

pub fn reconcile(s: &Shared) -> Result<Reconciled> {
    let lock = || s.lock().unwrap_or_else(|e| e.into_inner());
    let (root, marks) = {
        let ix = lock();
        (ix.root.clone(), ix.watermarks()?)
    };
    let mut out = Reconciled::default();
    let mut on_disk = HashSet::new();
    let mut unsupported = BTreeSet::new();
    for f in session_files(&root) {
        out.files += 1;
        let key = norm(&f);
        on_disk.insert(key.clone());
        let source = match scan::source_generation(&f) {
            Ok(source) => source,
            Err(e) if e.to_string().contains(crate::ROLLBACK_IN_PROGRESS) => {
                record_rollback_outcome(&f, &mut out);
                continue;
            }
            Err(_) => {
                out.failed += 1;
                continue;
            }
        };
        let st = source.stamp;
        let rollback = source.rollback;
        if marks.get(&key).is_some_and(|&(len, m, dev, ino, lock_dev, lock_ino, lock_len)| {
            len == st.len as i64
                && m == st.mtime_ns as i64
                && dev == st.dev as i64
                && ino == st.ino as i64
                && rollback == (lock_dev, lock_ino, lock_len)
        }) {
            continue;
        }
        let result = { lock().update_with_change(&f, None) };
        match result {
            Ok(outcome) => {
                out.updated += outcome.updated;
                out.removed += outcome.removed;
            }
            Err(e) if e.to_string().contains(crate::UNSUPPORTED_VERSION) => {
                #[cfg(test)]
                super::unsupported_update_complete();
                if scan::source_generation(&f).is_ok_and(|current| current == source) {
                    unsupported.insert(f.to_string_lossy().into_owned());
                    if let Some(&expected) = marks.get(&key) {
                        let removed = lock().remove_path_if_watermark(&f, expected, Some(source))?;
                        out.removed += removed.unwrap_or(0);
                        // The filesystem and SQLite cannot share a transaction. If the path was
                        // replaced just after the generation check, immediately rebuild its row.
                        if removed.is_none() || scan::source_generation(&f).is_ok_and(|current| current != source) {
                            unsupported.remove(&f.to_string_lossy().into_owned());
                            out.removed -= removed.unwrap_or(0);
                            match lock().update_with_change(&f, None) {
                                Ok(outcome) => {
                                    out.updated += outcome.updated;
                                    out.removed += outcome.removed;
                                }
                                Err(error) if error.to_string().contains(crate::ROLLBACK_IN_PROGRESS) => {
                                    record_rollback_outcome(&f, &mut out);
                                }
                                Err(_) => out.failed += 1,
                            }
                        }
                    }
                } else {
                    // A valid replacement may have appeared after this scan's error. Retry it
                    // while preserving any newer row already committed by another reconciler.
                    match lock().update_with_change(&f, None) {
                        Ok(outcome) => {
                            out.updated += outcome.updated;
                            out.removed += outcome.removed;
                        }
                        Err(error) if error.to_string().contains(crate::ROLLBACK_IN_PROGRESS) => {
                            record_rollback_outcome(&f, &mut out);
                        }
                        Err(_) => out.failed += 1,
                    }
                }
            }
            Err(e) if e.to_string().contains(crate::ROLLBACK_IN_PROGRESS) => record_rollback_outcome(&f, &mut out),
            Err(_) => out.failed += 1,
        }
    }
    for path in marks.keys() {
        if !on_disk.contains(path) && !Path::new(path).exists() {
            #[cfg(test)]
            super::before_stale_delete();
            let source = Path::new(path);
            let removed = lock().remove_path_if_watermark(source, marks[path], None)?;
            out.removed += removed.unwrap_or(0);
            if removed.is_none() || source.exists() {
                // Recreated after inventory or deletion: catch up from the surviving row.
                out.removed -= removed.unwrap_or(0);
                match lock().update_with_change(source, None) {
                    Ok(outcome) => {
                        out.updated += outcome.updated;
                        out.removed += outcome.removed;
                    }
                    Err(error) if error.to_string().contains(crate::ROLLBACK_IN_PROGRESS) => {
                        record_rollback_outcome(source, &mut out);
                    }
                    Err(_) => out.failed += 1,
                }
            }
        }
    }
    let mut ix = lock();
    ix.unsupported = unsupported;
    out.unsupported = ix.unsupported();
    kv_set(&ix.conn, "reconciled_at", now_ms())?;
    Ok(out)
}

/// Errors that must reach the caller rather than silently falling back to a directory scan.
pub fn must_report_error(error: &anyhow::Error) -> bool {
    let detail = format!("{error:#}");
    detail.contains(crate::UNSUPPORTED_VERSION)
        || detail.contains("cannot rebuild")
        || detail.contains("removing session index")
        || error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                || cause.downcast_ref::<rusqlite::Error>().is_some_and(|e| {
                    matches!(
                        e.sqlite_error_code(),
                        Some(rusqlite::ffi::ErrorCode::ReadOnly | rusqlite::ffi::ErrorCode::PermissionDenied)
                    )
                })
        })
}
