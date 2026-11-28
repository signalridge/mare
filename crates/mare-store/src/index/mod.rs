//! Session index (`<sessions dir>/index.sqlite`, under STATE next to the session files) — what
//! `/resume`, `-c` and id lookups read, so they never open session files.
//!
//! * the session JSONL files stay the only source of truth: every row is derived from its file
//!   (`scan.rs`) and the whole database can be deleted and rebuilt; no transcript content, no
//!   full-text search — metadata only
//! * a write is "append the JSONL first, then catch the row up": the row keeps a watermark
//!   (`indexed_len`, `indexed_mtime`, physical file identity, and the session lock's rollback
//!   revision) and a catch-up parses only bytes appended to that same file generation.
//!   An index failure is reported to the caller, which logs it; it never fails the session
//! * sessions sort by `seq`, a counter handed out in the same transaction (`kv.seq`) whenever a
//!   row is new or gained messages — not by wall-clock times, which a clock step back misorders.
//!   A rename, label or archive entry changes its column but not the order
//! * one connection per process and database (`shared`), WAL (DELETE where WAL can't be
//!   enabled, e.g. a network file system), `synchronous = NORMAL`, `busy_timeout = 5000`, writes
//!   in `BEGIN IMMEDIATE`; several mare processes share the file safely
//! * unsupported `user_version`: discard the derived index under an exclusive lock and rebuild
//!   from session files; corruption is set aside as `index.sqlite.bad-<unix ts>`
//! * old JSONL indexes are not imported or modified
//! * resume checkpoints are the `verified_len` / `verified_mtime` columns

mod checkpoint;
mod query;
mod reconcile;
mod scan;
mod schema;
mod search;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::types::Value as Sql;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

pub(crate) use checkpoint::checkpoint_is_current;
pub use checkpoint::{CheckpointGeneration, checkpoint_generation};
use checkpoint::{checkpoint_anchor_matches, same_source_generation};
pub use query::{Kinds, Query, scan_list, scan_list_with_unsupported};
#[cfg(test)]
use reconcile::OPEN;
pub use reconcile::{Reconciled, Shared, close_idle, must_report_error, ready, reconcile, shared};
pub use scan::{Row, git_branch, norm, one_line as normalize_title, project_root, session_files};
pub use schema::Status;
use schema::{
    COLS, OpenError, by_path, connect, kv_get, kv_set, next_seq, now_ms, remove_index, row_of, set_aside, write_row,
};
pub use search::{content_snippet, file_contains};

/// The database file's name in the sessions directory.
pub const DB_NAME: &str = "index.sqlite";
pub const SCHEMA_VERSION: i64 = 7;
const SOURCE_CHECK_TRIES: usize = 2;

#[cfg(test)]
thread_local! {
    static AFTER_STALE_DELETE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_STALE_DELETE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_NONE_DELETE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_BUILD_SCAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static AFTER_UNSUPPORTED_UPDATE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BEFORE_UPDATE_TX: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(super) fn before_update_tx(hook: Option<Box<dyn FnMut()>>) {
    BEFORE_UPDATE_TX.with(|slot| *slot.borrow_mut() = hook);
}

#[cfg(test)]
fn update_tx_ready() {
    let hook = BEFORE_UPDATE_TX.with(|slot| slot.borrow_mut().take());
    if let Some(mut hook) = hook {
        hook();
        BEFORE_UPDATE_TX.with(|slot| {
            if slot.borrow().is_none() {
                *slot.borrow_mut() = Some(hook);
            }
        });
    }
}

#[cfg(all(test, unix))]
pub(super) fn before_next_none_delete(hook: impl FnOnce() + 'static) {
    BEFORE_NONE_DELETE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn none_delete_ready() {
    BEFORE_NONE_DELETE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(all(test, unix))]
pub(super) fn after_next_build_scan(hook: impl FnOnce() + 'static) {
    AFTER_BUILD_SCAN.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(all(test, unix))]
pub(super) fn after_next_unsupported_update(hook: impl FnOnce() + 'static) {
    AFTER_UNSUPPORTED_UPDATE.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn build_scan_complete() {
    AFTER_BUILD_SCAN.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn unsupported_update_complete() {
    AFTER_UNSUPPORTED_UPDATE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
fn before_stale_delete() {
    BEFORE_STALE_DELETE.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

/// Assign `seq` 1..n: rows the previous order knew keep their relative order; the others
/// (never indexed) are merged in by time.
fn assign_seq(rows: &mut [Row], prior: &[Row]) {
    let pos: HashMap<&str, (usize, &str, i64)> =
        prior.iter().enumerate().map(|(i, r)| (r.path.as_str(), (i, r.session_id.as_str(), r.updated_at_ms))).collect();
    let mut known: Vec<(usize, i64, usize)> = vec![];
    let mut new: Vec<(i64, usize)> = vec![];
    for (i, r) in rows.iter().enumerate() {
        match pos.get(r.path.as_str()) {
            Some(&(at, id, t)) if id == r.session_id => known.push((at, t, i)),
            None => new.push((r.updated_at_ms, i)),
            Some(_) => new.push((r.updated_at_ms, i)),
        }
    }
    known.sort();
    new.sort();
    let (mut a, mut b, mut seq) = (0, 0, 0);
    let mut order = Vec::with_capacity(rows.len());
    while a < known.len() || b < new.len() {
        let take_known = b >= new.len() || (a < known.len() && known[a].1 <= new[b].0);
        if take_known {
            order.push(known[a].2);
            a += 1;
        } else {
            order.push(new[b].1);
            b += 1;
        }
    }
    for i in order {
        seq += 1;
        rows[i].seq = seq;
    }
}

pub struct Index {
    pub(super) conn: Connection,
    /// Shared for this connection lifetime; replacement requires the exclusive lock.
    _guard: std::fs::File,
    pub(super) root: PathBuf,
    status: Status,
    journal: String,
    /// why the index had to be rebuilt, until [`Index::take_notice`] hands it out
    notice: Option<String>,
    pub(super) unsupported: BTreeSet<String>,
}

type SessionWatermark = (i64, i64, i64, i64, i64, i64, i64);

/// What an index catch-up changed, separately from its public row-returning result. A duplicate
/// source may remove the path's old row and return no row, which still requires readers to refresh.
#[derive(Debug, Default)]
struct UpdateOutcome {
    row: Option<Row>,
    updated: usize,
    removed: usize,
}

impl UpdateOutcome {
    fn changed(&self) -> bool {
        self.updated > 0 || self.removed > 0
    }
}

impl Index {
    /// Open the index of the sessions under `root` (`root/index.sqlite`); an unusable file is set
    /// aside and a new one created. Does not build it: see [`Index::ensure_built`].
    pub fn open(root: &Path) -> Result<Self> {
        mare_shared::private::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        let db = root.join(DB_NAME);
        let guard_path = root.join("index.sqlite.lock");
        let lock_file = || {
            crate::lock::open_private(
                std::fs::OpenOptions::new().read(true).write(true).truncate(false),
                &guard_path,
                true,
            )
        };
        let mut notice: Option<String> = None;
        loop {
            let guard = lock_file()?;
            guard.lock_shared().with_context(|| format!("locking {}", guard_path.display()))?;
            match connect(&db) {
                Ok((conn, fresh, journal)) => {
                    let status = if let Some(why) = &notice {
                        Status::Rebuilt(why.clone())
                    } else if fresh {
                        Status::Created
                    } else {
                        Status::Ok
                    };
                    return Ok(Self {
                        conn,
                        _guard: guard,
                        root: root.to_path_buf(),
                        status,
                        journal,
                        notice,
                        unsupported: BTreeSet::new(),
                    });
                }
                Err(OpenError::Other(e)) => return Err(e.context(format!("opening {}", db.display()))),
                Err(OpenError::Bad(_) | OpenError::Unsupported(_)) => {}
            }
            // Release our shared lock and try the exclusive lock; a live connection prevents
            // replacement. Re-check under the exclusive lock because another opener may win.
            drop(guard);
            let guard = lock_file()?;
            guard
                .try_lock()
                .map_err(|e| anyhow::anyhow!("session index is in use; cannot rebuild {}: {e}", db.display()))?;
            match connect(&db) {
                Ok((conn, _, _)) => drop(conn),
                Err(OpenError::Other(e)) => return Err(e.context(format!("opening {}", db.display()))),
                Err(OpenError::Unsupported(why)) => {
                    remove_index(&db)?;
                    notice = Some(why);
                }
                Err(OpenError::Bad(why)) => {
                    let bad = set_aside(&db)?;
                    notice = Some(format!("{why}; old file kept as {}", bad.display()));
                }
            }
            // Drop exclusive before reopening; no connection can outlive the lock transition.
            drop(guard);
        }
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// The reason the index was rebuilt in this process (corrupt, or a schema version change), once: the caller shows the user one notice.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// Unsupported session files skipped during the latest scan, in stable order.
    pub fn unsupported(&self) -> Vec<String> {
        self.unsupported.iter().cloned().collect()
    }

    /// "wal", or "delete" where WAL couldn't be enabled
    pub fn journal_mode(&self) -> &str {
        &self.journal
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn built(c: &Connection) -> rusqlite::Result<bool> {
        Ok(kv_get(c, "built")?.is_some())
    }

    /// Build the index from the session files unless it was built already. Returns whether it built.
    pub fn ensure_built(&mut self) -> Result<bool> {
        if Self::built(&self.conn)? {
            return Ok(false);
        }
        self.build(true)?;
        Ok(true)
    }

    /// Rebuild every row from the session files in one transaction, keeping what the files
    /// can't tell: the order (`seq`), resume checkpoints, and the directory a live writer
    /// recorded. Returns the row count.
    pub fn rebuild(&mut self) -> Result<usize> {
        self.build(false)
    }

    fn build(&mut self, initial: bool) -> Result<usize> {
        for _ in 0..SOURCE_CHECK_TRIES {
            // Parse outside the write lock (the slow part); ids seen twice keep the newest file.
            let paths = session_files(&self.root);
            let inventory: HashSet<String> = paths.iter().map(|path| norm(path)).collect();
            let mut seen = HashSet::new();
            let mut rows = Vec::new();
            let mut parsed_rows = Vec::new();
            let mut unsupported = BTreeSet::new();
            let mut unsupported_generations = HashMap::new();
            let mut empty_generations = HashMap::new();
            let mut unresolved: HashMap<String, Option<scan::SourceGeneration>> = HashMap::new();
            let mut rollback_pending = false;
            for path in &paths {
                let key = norm(path);
                let before = scan::source_generation(path).ok();
                match scan::parse_full(path) {
                    Ok(Some(row)) => {
                        parsed_rows.push(row.clone());
                        if seen.insert(row.session_id.clone()) {
                            rows.push(row);
                        }
                    }
                    Ok(None) => {
                        if let Some(generation) = before {
                            empty_generations.insert(key, generation);
                        } else {
                            unresolved.insert(key, None);
                        }
                    }
                    Err(e) if e.to_string().contains(crate::UNSUPPORTED_VERSION) => {
                        unsupported.insert(path.to_string_lossy().into_owned());
                        if let Some(generation) = before {
                            unsupported_generations.insert(key, generation);
                        } else {
                            unresolved.insert(key, None);
                        }
                    }
                    Err(e) if e.to_string().contains(crate::ROLLBACK_IN_PROGRESS) => rollback_pending = true,
                    Err(_) => {
                        unresolved.insert(key, before);
                    }
                }
            }
            #[cfg(test)]
            build_scan_complete();
            if rollback_pending {
                continue;
            }

            let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if initial && Self::built(&tx)? {
                self.unsupported = unsupported;
                return Ok(0); // another process built it meanwhile
            }
            let known: Vec<Row> = {
                let mut st = tx.prepare(&format!("SELECT {COLS} FROM sessions ORDER BY seq"))?;
                st.query_map([], row_of)?.collect::<rusqlite::Result<_>>()?
            };
            let known_by_path: HashMap<String, Row> = known.iter().cloned().map(|r| (r.path.clone(), r)).collect();
            for key in unresolved.keys() {
                if let Some(row) = known_by_path.get(key)
                    && !rows.iter().any(|candidate| candidate.session_id == row.session_id)
                {
                    rows.push(row.clone());
                }
            }
            assign_seq(&mut rows, &known);
            let known: HashMap<String, Row> = known.into_iter().map(|r| (r.path.clone(), r)).collect();
            for r in &mut rows {
                if let Some(k) = known.get(&r.path).filter(|k| k.session_id == r.session_id) {
                    if !k.cwd.is_empty() {
                        r.cwd = k.cwd.clone();
                        r.project_root = k.project_root.clone();
                    }
                    r.git_branch = k.git_branch.clone();
                    if same_source_generation(k, r) && checkpoint_anchor_matches(k, r) {
                        (r.verified_len, r.verified_mtime) = (k.verified_len, k.verified_mtime);
                    }
                }
            }
            tx.execute("DELETE FROM sessions", [])?;
            for r in &rows {
                write_row(&tx, r)?;
            }
            kv_set(&tx, "seq", rows.len() as i64)?;
            kv_set(&tx, "built", now_ms())?;
            // The scan and SQLite commit cannot be one filesystem transaction. Recheck every
            // candidate after writing the derived rows but before commit; a replacement during
            // scanning or the SQLite write rolls this transaction back and retries. Also check
            // paths excluded from `rows`: a replacement of an unsupported or unreadable source
            // must not make the rebuild commit a table with that still-valid row missing.
            let inventory_unchanged: HashSet<String> =
                session_files(&self.root).iter().map(|path| norm(path)).collect();
            let rows_unchanged =
                parsed_rows.iter().all(|row| scan::source_matches(Path::new(&row.path), row).is_ok_and(|same| same));
            let unsupported_unchanged = unsupported_generations.iter().all(|(path, expected)| {
                scan::source_generation(Path::new(path)).is_ok_and(|actual| actual == *expected)
            });
            let empty_unchanged = empty_generations.iter().all(|(path, expected)| {
                scan::source_generation(Path::new(path)).is_ok_and(|actual| actual == *expected)
            });
            let unresolved_unchanged = unresolved.iter().all(|(path, expected)| match expected {
                Some(expected) => scan::source_generation(Path::new(path)).is_ok_and(|actual| actual == *expected),
                None => scan::source_generation(Path::new(path)).is_err(),
            });
            if inventory_unchanged != inventory
                || !rows_unchanged
                || !unsupported_unchanged
                || !empty_unchanged
                || !unresolved_unchanged
            {
                drop(tx);
                continue;
            }
            tx.commit()?;
            self.unsupported = unsupported;
            return Ok(rows.len());
        }
        anyhow::bail!("session files changed repeatedly while the index was built; try again")
    }

    /// Bring the row of one session file up to date with the file (a catch-up from its
    /// watermark, or a full parse), after the caller appended to it. `cwd`: the directory a live
    /// writer runs in (else the row keeps its own, or the header's). The row moves to the top
    /// (`seq`) when it is new or gained messages. A file that is gone loses its row. Returns the
    /// row, or `None` when there is no session to index.
    pub fn update(&mut self, path: &Path, cwd: Option<&str>) -> Result<Option<Row>> {
        Ok(self.update_with_change(path, cwd)?.row)
    }

    fn update_with_change(&mut self, path: &Path, cwd: Option<&str>) -> Result<UpdateOutcome> {
        let key = norm(path);
        for _ in 0..SOURCE_CHECK_TRIES {
            let old = by_path(&self.conn, &key)?;
            let meta = match std::fs::metadata(path) {
                Ok(m) => Some(m),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            if meta.as_ref().is_none_or(|m| m.len() == 0) {
                // A failed first batch can be indexed while its complete header is visible, then
                // rolled back to the newly-created empty file before the next reconcile.
                if old.is_none() {
                    return Ok(UpdateOutcome::default());
                }
                let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let current_old = by_path(&tx, &key)?;
                if current_old != old {
                    drop(tx);
                    continue;
                }
                let still_absent_or_empty = match std::fs::metadata(path) {
                    Ok(m) => m.len() == 0,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
                    Err(e) => return Err(e.into()),
                };
                if !still_absent_or_empty {
                    drop(tx);
                    continue;
                }
                let removed = tx.execute("DELETE FROM sessions WHERE path = ?1", [&key])?;
                tx.commit()?;
                return Ok(UpdateOutcome { removed, ..Default::default() });
            }

            let mut scanned = None;
            let mut none_generation: Option<Option<scan::SourceGeneration>> = None;
            for _ in 0..SOURCE_CHECK_TRIES {
                let candidate = match old.clone() {
                    Some(mut r) => match scan::catch_up(&mut r, path)? {
                        scan::Progress::Grew(n) => (r, n),
                        scan::Progress::NeedFull => {
                            let before_gen = scan::source_generation(path).ok();
                            let Some(mut r) = scan::parse_full(path)? else {
                                let after_gen = scan::source_generation(path).ok();
                                if before_gen == after_gen {
                                    none_generation = Some(after_gen);
                                    break;
                                }
                                continue;
                            };
                            let o = old.as_ref().expect("old row");
                            if r.session_id == o.session_id {
                                r.seq = o.seq;
                                r.git_branch = o.git_branch.clone();
                                if !o.cwd.is_empty() {
                                    (r.cwd, r.project_root) = (o.cwd.clone(), o.project_root.clone());
                                }
                            }
                            let gained = r.message_count - o.message_count;
                            (r, gained)
                        }
                    },
                    None => match scan::parse_full(path)? {
                        Some(r) => (r, 1),
                        None => return Ok(UpdateOutcome::default()),
                    },
                };
                if scan::source_matches(path, &candidate.0)? {
                    scanned = Some(candidate);
                    break;
                }
            }
            if let Some(expected_gen) = none_generation {
                #[cfg(test)]
                none_delete_ready();
                let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
                let current_old = by_path(&tx, &key)?;
                if current_old != old {
                    drop(tx);
                    continue;
                }
                let still_same_source = match expected_gen {
                    Some(expected) => scan::source_generation(path).is_ok_and(|actual| actual == expected),
                    None => path.metadata().is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
                };
                if !still_same_source {
                    drop(tx);
                    continue;
                }
                let removed = tx.execute("DELETE FROM sessions WHERE path = ?1", [&key])?;
                tx.commit()?;
                return Ok(UpdateOutcome { removed, ..Default::default() });
            }
            let Some((mut row, gained)) = scanned else {
                anyhow::bail!("session file {} changed repeatedly while it was indexed; try again", path.display());
            };
            if let Some(previous) = &old {
                if same_source_generation(previous, &row) && checkpoint_anchor_matches(previous, &row) {
                    row.verified_len = previous.verified_len;
                    row.verified_mtime = previous.verified_mtime;
                } else {
                    row.verified_len = None;
                    row.verified_mtime = None;
                }
            }
            row.path = key.clone();
            if let Some(c) = cwd.filter(|c| !c.is_empty()) {
                let root = project_root(c);
                row.git_branch = git_branch(&root);
                (row.cwd, row.project_root) = (c.to_string(), Some(root));
            }
            let mut old = old;
            let mut gained = gained;
            let mut id_changed = old.as_ref().is_some_and(|old| old.session_id != row.session_id);

            #[cfg(test)]
            update_tx_ready();
            let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current_old = by_path(&tx, &key)?;
            if current_old != old {
                let Some(cur) = current_old.filter(|cur| {
                    cur.session_id == row.session_id
                        && same_source_generation(cur, &row)
                        && (cur.indexed_len < row.indexed_len
                            || (cur.indexed_len == row.indexed_len && cur.indexed_mtime == row.indexed_mtime))
                }) else {
                    drop(tx);
                    continue;
                };
                // Another connection committed an earlier or identical watermark of this same file
                // generation while we scanned: rebase onto its row instead of retrying.
                row.seq = cur.seq;
                if cwd.filter(|c| !c.is_empty()).is_none() {
                    row.git_branch = cur.git_branch.clone();
                    if !cur.cwd.is_empty() {
                        (row.cwd, row.project_root) = (cur.cwd.clone(), cur.project_root.clone());
                    }
                }
                if checkpoint_anchor_matches(&cur, &row) {
                    row.verified_len = cur.verified_len;
                    row.verified_mtime = cur.verified_mtime;
                } else {
                    row.verified_len = None;
                    row.verified_mtime = None;
                }
                gained = row.message_count - cur.message_count;
                id_changed = false;
                old = Some(cur);
            }
            // The same id under another path is a copied file; keep the first row only while that
            // path's current header still names the ID. A stale row must not make a changed ID vanish.
            let other: Option<String> = tx
                .query_row(
                    "SELECT path FROM sessions WHERE session_id = ?1 AND path <> ?2",
                    params![row.session_id, key],
                    |r| r.get(0),
                )
                .optional()?;
            let mut removed = 0;
            if let Some(o) = other {
                let other_path = Path::new(&o);
                let other_has_id = match scan::session_id(other_path) {
                    Ok(other_id) => other_id.as_deref() == Some(row.session_id.as_str()),
                    Err(e) => {
                        let detail = format!("{e:#}");
                        let unavailable_or_invalid = detail.contains(crate::UNSUPPORTED_VERSION)
                            || detail.contains(crate::CORRUPT)
                            || e.chain().any(|cause| {
                                cause
                                    .downcast_ref::<std::io::Error>()
                                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                            });
                        if unavailable_or_invalid { false } else { return Err(e) }
                    }
                };
                if other_has_id {
                    // The source at this path no longer owns its previous id, or is a duplicate
                    // copy. Keep the existing file's row and drop any row formerly tied to this path.
                    if !scan::source_matches(path, &row)? {
                        anyhow::bail!(
                            "session file {} changed while its duplicate was resolved; try again",
                            path.display()
                        );
                    }
                    let removed = tx.execute("DELETE FROM sessions WHERE path = ?1", [&key])?;
                    tx.commit()?;
                    return Ok(UpdateOutcome { removed, ..Default::default() });
                }
                removed += tx.execute("DELETE FROM sessions WHERE path = ?1", [&o])?;
            }
            if old.is_none() || id_changed || gained > 0 {
                row.seq = next_seq(&tx)?;
            }
            if !scan::source_matches(path, &row)? {
                drop(tx);
                continue;
            }
            let mut outcome = UpdateOutcome { row: None, updated: 0, removed };
            if old.as_ref() != Some(&row) {
                write_row(&tx, &row)?;
                outcome.updated = 1;
            }
            tx.commit()?;
            outcome.row = Some(row);
            return Ok(outcome);
        }
        anyhow::bail!("session file {} changed before its index row was committed; try again", path.display())
    }

    /// Drop a session's row (its file was deleted), with its sub-agents' rows.
    pub fn remove(&mut self, id: Option<&str>, path: Option<&Path>) -> Result<()> {
        self.remove_with_change(id, path).map(|_| ())
    }

    fn remove_with_change(&mut self, id: Option<&str>, path: Option<&Path>) -> Result<usize> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut removed = 0;
        if let Some(p) = path {
            removed += tx.execute("DELETE FROM sessions WHERE path = ?1", [norm(p)])?;
        }
        if let Some(id) = id {
            removed += tx.execute(
                "DELETE FROM sessions WHERE session_id = ?1 OR (kind = 'agent' AND parent_session_id = ?1)",
                [id],
            )?;
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Delete only the observed row while its source remains unsupported or absent.
    /// `None` means the source changed and the deletion was rolled back; callers retry it.
    fn remove_path_if_watermark(
        &mut self,
        path: &Path,
        expected: SessionWatermark,
        source: Option<scan::SourceGeneration>,
    ) -> Result<Option<usize>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute(
            "DELETE FROM sessions WHERE path = ?1 AND indexed_len = ?2 AND indexed_mtime = ?3 \
             AND indexed_dev = ?4 AND indexed_ino = ?5 AND indexed_lock_dev = ?6 \
             AND indexed_lock_ino = ?7 AND indexed_lock_len = ?8",
            params![norm(path), expected.0, expected.1, expected.2, expected.3, expected.4, expected.5, expected.6],
        )?;
        #[cfg(test)]
        AFTER_STALE_DELETE.with(|slot| {
            if let Some(hook) = slot.borrow_mut().take() {
                hook();
            }
        });
        // Keep the old row, including its order and checkpoint, if the source changed
        // during deletion. A separate index writer is fenced by the watermark predicate.
        let unchanged = match source {
            Some(expected) => scan::source_generation(path).is_ok_and(|current| current == expected),
            None => path.metadata().is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
        };
        if !unchanged {
            return Ok(None); // dropping the transaction restores the old row
        }
        tx.commit()?;
        Ok(Some(removed))
    }

    /// One page of sessions, newest first, and how many match in all.
    pub fn list(&self, q: &Query) -> Result<(Vec<Row>, i64)> {
        let (w, mut p) = q.filter();
        let total: i64 =
            self.conn.query_row(&format!("SELECT count(*) FROM sessions{w}"), rusqlite::params_from_iter(&p), |r| {
                r.get(0)
            })?;
        p.push(Sql::Integer(q.limit.unwrap_or(-1)));
        p.push(Sql::Integer(q.offset.max(0)));
        let mut st =
            self.conn.prepare(&format!("SELECT {COLS} FROM sessions{w} ORDER BY seq DESC LIMIT ? OFFSET ?"))?;
        let rows = st.query_map(rusqlite::params_from_iter(&p), row_of)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, total))
    }

    /// Verify changed files on the requested page before exposing their indexed rows. A stale
    /// row whose file has become unsupported must not be selectable while reconcile is running.
    pub fn list_checked(&mut self, q: &Query) -> Result<(Vec<Row>, i64)> {
        for _ in 0..3 {
            let (rows, total) = self.list(q)?;
            let mut changed = false;
            for row in &rows {
                let path = Path::new(&row.path);
                let stamp = crate::stamp(path);
                if stamp.as_ref().is_ok_and(|s| {
                    s.len as i64 == row.indexed_len
                        && s.mtime_ns as i64 == row.indexed_mtime
                        && s.dev as i64 == row.indexed_dev
                        && s.ino as i64 == row.indexed_ino
                }) && scan::rollback_revision(path).is_ok_and(|revision| {
                    revision == (row.indexed_lock_dev, row.indexed_lock_ino, row.indexed_lock_len)
                }) {
                    continue;
                }
                let observed = scan::source_generation(path).ok();
                match self.update_with_change(path, None) {
                    Ok(outcome) => changed |= outcome.changed(),
                    Err(e) if e.to_string().contains(crate::UNSUPPORTED_VERSION) => {
                        if observed.is_some() && scan::source_generation(path).ok() == observed {
                            #[cfg(test)]
                            before_stale_delete();
                            let removed = self.remove_path_if_watermark(
                                path,
                                (
                                    row.indexed_len,
                                    row.indexed_mtime,
                                    row.indexed_dev,
                                    row.indexed_ino,
                                    row.indexed_lock_dev,
                                    row.indexed_lock_ino,
                                    row.indexed_lock_len,
                                ),
                                observed,
                            )?;
                            if removed.is_some() && scan::source_generation(path).ok() == observed {
                                self.unsupported.insert(row.path.clone());
                            } else {
                                self.unsupported.remove(&row.path);
                                self.update_with_change(path, None)?;
                            }
                        }
                        changed = true;
                    }
                    Err(_) => changed = true,
                }
            }
            if !changed {
                return Ok((rows, total));
            }
        }
        anyhow::bail!("session index changed repeatedly while listing; try again")
    }

    /// Immediate lexicographical neighbor IDs (`prev`, `next`) across all main and fork sessions
    /// (including archived sessions and all workspaces), using only the primary-key index.
    pub fn neighbor_session_ids(&self, id: &str) -> Result<(Option<String>, Option<String>)> {
        let prev: Option<String> = self
            .conn
            .query_row(
                "SELECT session_id FROM sessions WHERE kind IN ('main', 'fork') AND session_id < ?1 ORDER BY session_id DESC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let next: Option<String> = self
            .conn
            .query_row(
                "SELECT session_id FROM sessions WHERE kind IN ('main', 'fork') AND session_id > ?1 ORDER BY session_id ASC LIMIT 1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        Ok((prev, next))
    }

    /// All main and fork session IDs sorted lexicographically (no file stats, no row payload).
    pub fn session_ids(&self) -> Result<Vec<String>> {
        let mut st =
            self.conn.prepare("SELECT session_id FROM sessions WHERE kind IN ('main', 'fork') ORDER BY session_id")?;
        let ids = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(ids)
    }

    /// Look up rows for a page of session IDs / paths directly from SQLite without file stats.
    pub fn rows_by_keys(&self, ids: &[&str], paths: &[&str]) -> Result<Vec<Row>> {
        let mut out = Vec::with_capacity(ids.len().max(paths.len()));
        let mut seen = HashSet::new();
        if !ids.is_empty() {
            let mut st = self.conn.prepare(&format!("SELECT {COLS} FROM sessions WHERE session_id = ?1"))?;
            for id in ids {
                if !id.is_empty()
                    && let Some(row) = st.query_row([id], row_of).optional()?
                    && seen.insert(row.path.clone())
                {
                    out.push(row);
                }
            }
        }
        if !paths.is_empty() {
            let mut st = self.conn.prepare(&format!("SELECT {COLS} FROM sessions WHERE path = ?1"))?;
            for path in paths {
                if !path.is_empty()
                    && !seen.contains(*path)
                    && let Some(row) = st.query_row([path], row_of).optional()?
                    && seen.insert(row.path.clone())
                {
                    out.push(row);
                }
            }
        }
        Ok(out)
    }

    /// Number of rows.
    pub fn count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM sessions", [], |r| r.get(0))?)
    }

    /// path → source watermark, physical identity, and rollback revision of every row.
    fn watermarks(&self) -> Result<HashMap<String, SessionWatermark>> {
        let mut st = self.conn.prepare(
            "SELECT path, indexed_len, indexed_mtime, indexed_dev, indexed_ino, indexed_lock_dev, indexed_lock_ino, indexed_lock_len FROM sessions",
        )?;
        let out = st
            .query_map([], |r| {
                Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
