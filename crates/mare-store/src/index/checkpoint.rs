use std::path::Path;

use anyhow::Result;
use rusqlite::{OptionalExtension, TransactionBehavior, params};

use super::reconcile::OPEN;
use super::schema::by_path;
use super::{Index, Row, norm, scan};

/// A source file's physical identity and rollback generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointGeneration {
    pub file_dev: i64,
    pub file_ino: i64,
    pub lock_dev: i64,
    pub lock_ino: i64,
    pub lock_len: i64,
}

type CheckpointColumns = (Option<i64>, Option<i64>, i64, i64, i64, i64, i64, i64);

pub(super) fn same_source_generation(a: &Row, b: &Row) -> bool {
    (a.indexed_dev, a.indexed_ino, a.indexed_lock_dev, a.indexed_lock_ino, a.indexed_lock_len)
        == (b.indexed_dev, b.indexed_ino, b.indexed_lock_dev, b.indexed_lock_ino, b.indexed_lock_len)
}

pub(super) fn checkpoint_anchor_holds(len: u64, mtime_ns: u64, current: &Row) -> bool {
    len > 0
        && len <= current.indexed_len.max(0) as u64
        && (current.indexed_len as u64 > len || current.indexed_mtime as u64 == mtime_ns)
}

pub(super) fn checkpoint_anchor_matches(previous: &Row, current: &Row) -> bool {
    match (previous.verified_len, previous.verified_mtime) {
        (Some(len), Some(mtime)) => checkpoint_anchor_holds(len.max(0) as u64, mtime.max(0) as u64, current),
        _ => false,
    }
}

fn checkpoint_generation_for_row(row: &Row) -> CheckpointGeneration {
    CheckpointGeneration {
        file_dev: row.indexed_dev,
        file_ino: row.indexed_ino,
        lock_dev: row.indexed_lock_dev,
        lock_ino: row.indexed_lock_ino,
        lock_len: row.indexed_lock_len,
    }
}

/// Stable file and rollback generation for binding a checkpoint produced by a live writer.
pub fn checkpoint_generation(path: &Path) -> Result<CheckpointGeneration> {
    let source = scan::source_generation(path)?;
    Ok(CheckpointGeneration {
        file_dev: source.stamp.dev as i64,
        file_ino: source.stamp.ino as i64,
        lock_dev: source.rollback.0,
        lock_ino: source.rollback.1,
        lock_len: source.rollback.2,
    })
}

fn generation_matches_row(generation: CheckpointGeneration, row: &Row) -> bool {
    generation == checkpoint_generation_for_row(row)
}

impl Index {
    /// The resume checkpoint recorded for a session file, bound to the generation whose prefix
    /// was verified. A stale checkpoint or one beyond the current indexed watermark is ignored.
    pub fn checkpoint(&self, path: &Path) -> Result<Option<crate::Checkpoint>> {
        let r: Option<CheckpointColumns> = self
            .conn
            .query_row(
                "SELECT verified_len, verified_mtime, indexed_len, indexed_dev, indexed_ino, \
                 indexed_lock_dev, indexed_lock_ino, indexed_lock_len FROM sessions WHERE path = ?1",
                [norm(path)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)),
            )
            .optional()?;
        let Some((Some(len), Some(m), indexed_len, dev, ino, lock_dev, lock_ino, lock_len)) = r else {
            return Ok(None);
        };
        if len > indexed_len {
            return Ok(None);
        }
        let checkpoint = crate::Checkpoint { len: len as u64, mtime_ns: m as u64 };
        let row_generation = CheckpointGeneration { file_dev: dev, file_ino: ino, lock_dev, lock_ino, lock_len };
        let Some(row) = by_path(&self.conn, &norm(path))? else { return Ok(None) };
        Ok((checkpoint_anchor_holds(checkpoint.len, checkpoint.mtime_ns, &row)
            && checkpoint_is_current_in(self, path, &checkpoint, row_generation))
        .then_some(checkpoint))
    }

    /// Record a resume checkpoint (catching the row up first, so it exists).
    pub fn set_checkpoint(&mut self, path: &Path, c: &crate::Checkpoint) -> Result<()> {
        let stamp = crate::stamp(path)?;
        if c.len != stamp.len || c.mtime_ns != stamp.mtime_ns {
            return Ok(());
        }
        let generation = checkpoint_generation(path)?;
        self.set_checkpoint_for_generation(path, c, generation)
    }

    /// Store a checkpoint only when the writer's observed generation still owns this path.
    pub fn set_checkpoint_for_generation(
        &mut self,
        path: &Path,
        c: &crate::Checkpoint,
        generation: CheckpointGeneration,
    ) -> Result<()> {
        self.update(path, None)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(row) = by_path(&tx, &norm(path))?
            && checkpoint_anchor_holds(c.len, c.mtime_ns, &row)
            && generation_matches_row(generation, &row)
            && checkpoint_generation(path).is_ok_and(|current| current == generation)
        {
            tx.execute(
                "UPDATE sessions SET verified_len = ?2, verified_mtime = ?3 WHERE path = ?1",
                params![norm(path), c.len as i64, c.mtime_ns as i64],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn checkpoint_matches(&self, path: &Path, checkpoint: &crate::Checkpoint, file_id: (u64, u64)) -> Result<bool> {
        let Some(row) = by_path(&self.conn, &norm(path))? else { return Ok(false) };
        let generation = checkpoint_generation(path)?;
        Ok(row.verified_len == Some(checkpoint.len as i64)
            && row.verified_mtime == Some(checkpoint.mtime_ns as i64)
            && checkpoint_anchor_holds(checkpoint.len, checkpoint.mtime_ns, &row)
            && generation_matches_row(generation, &row)
            && (file_id.0 as i64, file_id.1 as i64) == (row.indexed_dev, row.indexed_ino))
    }
}

fn checkpoint_is_current_in(
    index: &Index,
    path: &Path,
    checkpoint: &crate::Checkpoint,
    generation: CheckpointGeneration,
) -> bool {
    index
        .checkpoint_matches(path, checkpoint, (generation.file_dev as u64, generation.file_ino as u64))
        .unwrap_or(false)
}

/// Recheck an index-provided checkpoint after the writer has opened the source. The binding's
/// indexed path always has a registered shared index; direct `SessionFile::open_with` callers
/// retain responsibility for the checkpoint they pass.
pub(crate) fn checkpoint_is_current(path: &Path, checkpoint: &crate::Checkpoint, file_id: (u64, u64)) -> Result<bool> {
    let shared = {
        let open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.iter()
            .filter(|(root, _)| path.starts_with(root))
            .max_by_key(|(root, _)| root.components().count())
            .map(|(_, index)| index.clone())
    };
    let Some(shared) = shared else { return Ok(true) };
    shared.lock().unwrap_or_else(|e| e.into_inner()).checkpoint_matches(path, checkpoint, file_id)
}
