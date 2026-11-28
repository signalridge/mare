//! The writer: `SessionFile` opens under the lock, validates, then appends batches
//! with rollback on failure.

use super::*;
use crate::validate::validate;

#[cfg(test)]
thread_local! {
    pub(crate) static BEFORE_OPEN_VALIDATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static AFTER_OPEN_VALIDATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static AFTER_VALIDATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    /// Runs after a failed batch is visible but before its bytes and rollback revision are restored.
    pub(crate) static BEFORE_ROLLBACK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    /// Runs after the JSONL truncate is durable but before the rollback revision is completed.
    pub(crate) static AFTER_ROLLBACK_TRUNCATE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    /// Runs after the rollback offset and odd revision are durable but before the JSONL truncate.
    pub(crate) static AFTER_ROLLBACK_MARKER: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    /// The next fsync on this thread fails (tests run concurrently).
    pub(crate) static BEFORE_DURABLE_SYNC: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static FAIL_NEXT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static FAIL_NEXT_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static BEFORE_SNAPSHOT_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static AFTER_SNAPSHOT_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    pub(crate) static OPEN_BASELINE_SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub struct SessionFile {
    path: PathBuf,
    file: File,
    _lock: std::sync::Arc<LockGuard>,
    /// written but not yet fsynced (made durable at the next semantic boundary / flush)
    dirty: bool,
    /// file or lock identity was lost on disk: no further writes until reopened
    identity_lost: bool,
    /// offset of a failed append whose rollback did not finish; revalidated before retrying
    pending_rollback: Option<u64>,
    /// a torn final write starts here; cut before the next append
    torn_at: Option<u64>,
    /// bytes of complete lines (where a failed batch is truncated back to)
    len: u64,
    stats: OpenStats,
    durable_syncs: usize,
}

impl SessionFile {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, None)
    }

    /// Open a session for writing: lock first, then validate the format and contents strictly —
    /// all under the lock. A corrupt file ([`CORRUPT`]), one locked elsewhere
    /// ([`LOCKED`]) or already open in this process ([`OPEN_HERE`]) is refused and left
    /// untouched; a torn final write is cut before the first append. With the session's last
    /// checkpoint (length + mtime), only the bytes after it are validated when it still holds;
    /// otherwise the whole file is, before anything can be written. The file is stat'ed before
    /// and after reading and read once more if it changed meanwhile.
    pub fn open_with(path: &Path, checkpoint: Option<&Checkpoint>) -> Result<Self> {
        Self::open_impl(path, checkpoint, false, false)
    }

    /// [`open_with`](Self::open_with) for resuming: the session file must already exist
    /// ([`NOT_FOUND`] otherwise) — a stale path never becomes a new, headerless session.
    /// Nothing is created,
    /// not even the lock file, when it is missing.
    pub fn open_existing(path: &Path, checkpoint: Option<&Checkpoint>) -> Result<Self> {
        Self::open_impl(path, checkpoint, true, false)
    }

    /// Create a new session without ever opening an existing path.
    pub fn create_new(path: &Path) -> Result<Self> {
        Self::open_impl(path, None, false, true)
    }

    fn open_impl(path: &Path, checkpoint: Option<&Checkpoint>, must_exist: bool, must_create: bool) -> Result<Self> {
        if must_exist && !path.is_file() {
            anyhow::bail!("{NOT_FOUND}: {}", path.display());
        }
        let lock = lock_session(path)?;
        // A crash during a batch rollback leaves an odd revision. The new exclusive owner
        // completes it before validating or appending to the recovered file.
        lock.recover_rollback_revision(path)?;
        let mut stats = OpenStats::default();
        let (mut len, mut torn_at) = (0u64, None);
        let existed = path.exists();
        if must_exist && !existed {
            anyhow::bail!("{NOT_FOUND}: {}", path.display());
        }
        if must_create && existed {
            anyhow::bail!("session file already exists: {}", path.display());
        }
        // Open once under the lock; every byte used to validate must come from the handle
        // retained for writing, even if the path is temporarily replaced and restored.
        // A read/write handle supports set_len on Windows; writes seek to the complete end.
        let mut file = if existed {
            OpenOptions::new().read(true).write(true).open(path)
        } else {
            mare_shared::private::create_new(path)
        }
        .with_context(|| format!("opening session {}", path.display()))?;
        let matches_path = |file: &File| {
            lock.matches_path(path)
                && matches!((crate::lock::file_id(file), crate::lock::path_id(path)), (Ok(a), Ok(b)) if a == b)
        };
        if !matches_path(&file) {
            anyhow::bail!("session file {} changed while it was opened; try again", path.display());
        }
        let mut validated_stamp = None;
        if existed {
            let mut stable = false;
            for _ in 0..crate::stable::STABLE_READ_TRIES {
                let before = crate::stable::stamp_file(&file)?;
                #[cfg(test)]
                if let Some(hook) = BEFORE_OPEN_VALIDATION.with(|h| h.borrow_mut().take()) {
                    hook();
                }
                let result = (|| {
                    // A failed first batch may leave a retryable empty file; resume may not.
                    if !must_exist && before.len == 0 {
                        return Ok((0, None, OpenStats::default()));
                    }
                    crate::format::check_file_header_held(&mut file, path)?;
                    validate(&mut file, path, checkpoint)
                })();
                #[cfg(test)]
                if let Some(hook) = AFTER_OPEN_VALIDATION.with(|h| h.borrow_mut().take()) {
                    hook();
                }
                (len, torn_at, stats) = result?;
                if crate::stable::stamp_file(&file)? == before
                    && matches_path(&file)
                    && !crate::stable::forced_unstable(path)
                {
                    stable = true;
                    validated_stamp = Some(before);
                    break;
                }
            }
            if !stable {
                anyhow::bail!("session file {} keeps changing while it is opened; try again", path.display());
            }
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_VALIDATION.with(|h| h.borrow_mut().take()) {
            hook();
        }
        if !matches_path(&file)
            || validated_stamp.is_some_and(|expected| crate::stable::stamp_file(&file).ok() != Some(expected))
        {
            anyhow::bail!("session file {} changed while it was opened; try again", path.display());
        }
        if !existed {
            // the new file's directory entry is durable before anything relies on it
            // (fsync the parent after creating)
            sync_dir(path)?;
        }
        let mut durable_syncs = 0;
        let file_id = crate::lock::file_id(&file)?;
        let lock_stamp = crate::stamp(&crate::lock_path(path))?;
        let committed = crate::lock::read_committed_prefix(path)?;
        let needs_commit_point = !committed.is_some_and(|point| {
            (point.dev, point.ino) == file_id
                && point.len <= len
                && (point.lock_dev, point.lock_ino, point.revision) == (lock_stamp.dev, lock_stamp.ino, lock_stamp.len)
        });
        if needs_commit_point {
            // The marker is a rebuildable projection. Establish a durable baseline for legacy
            // journals before any new writer can append to them; for empty sessions, record the
            // zero-length commit point so an interrupted first batch cannot be mistaken for a
            // legacy journal without a marker.
            if len > 0 {
                #[cfg(test)]
                OPEN_BASELINE_SYNCS.with(|c| c.set(c.get() + 1));
                file.sync_data()?;
                durable_syncs += 1;
            }
            crate::lock::write_committed_prefix(path, &file, len)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            _lock: std::sync::Arc::new(lock),
            dirty: false,
            identity_lost: false,
            pending_rollback: None,
            torn_at,
            len,
            stats,
            durable_syncs,
        })
    }

    /// How this open validated the file.
    pub fn stats(&self) -> OpenStats {
        self.stats
    }

    /// Number of durable `sync_data` + commit-marker cycles performed by this handle.
    pub fn durable_syncs(&self) -> usize {
        self.durable_syncs
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reuse is safe only when the target has the same lock name and still names this handle.
    pub fn matches_path(&self, path: &Path) -> bool {
        self.path_binding(path).unwrap_or(false)
    }

    fn is_poisoned(&self) -> bool {
        self.identity_lost || self.pending_rollback.is_some()
    }

    /// False proves the requested path does not identify the healthy held file/lock (missing
    /// or different identities). Other IO failures and a poisoned writer are errors.
    pub fn matches_path_checked(&self, path: &Path) -> std::io::Result<bool> {
        if self.is_poisoned() {
            return Err(std::io::Error::other("session writer is poisoned; reopen it"));
        }
        self.path_binding(path)
    }

    fn path_binding(&self, path: &Path) -> std::io::Result<bool> {
        let lock_matches = self._lock.matches_path_checked(path)?;
        let held = crate::lock::file_id(&self.file)?;
        let file_matches = match crate::lock::path_id(path) {
            Ok(current) => held == current,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        Ok(lock_matches && file_matches)
    }

    /// Clone the held identity and session lock without sharing a file cursor with readers.
    pub fn snapshot_reader(&self) -> Result<SessionSnapshot> {
        Ok(SessionSnapshot { held: self.file.try_clone()?, lock: self._lock.clone(), path: self.path.clone() })
    }

    pub fn read_snapshot(&mut self, expected: Option<FileStamp>) -> Result<Vec<Value>> {
        self.snapshot_reader()?.read_snapshot(expected)
    }

    pub fn read_tail_snapshot(&mut self) -> Result<(crate::reverse::Tail, Value, FileStamp)> {
        self.snapshot_reader()?.read_tail_snapshot()
    }

    /// Confirm that the handle, lock and recorded bytes have not changed since a held read.
    pub fn matches_snapshot(&self, stamp: FileStamp) -> bool {
        self.matches_path(&self.path) && crate::stable::stamp_file(&self.file).ok() == Some(stamp)
    }

    /// A resume checkpoint of everything written so far (fsyncs pending writes first). `None`
    /// after a failed write that could not be rolled back, or when the file's length isn't what
    /// this handle wrote. The caller records it in the session index.
    pub fn checkpoint(&mut self) -> Result<Option<Checkpoint>> {
        if self.is_poisoned() || self.torn_at.is_some() || !self.matches_path(&self.path) {
            return Ok(None);
        }
        self.flush()?;
        let st = stamp(&self.path)?;
        if st.len != self.len || !self.matches_path(&self.path) {
            return Ok(None); // something else touched the file: no anchor to vouch for
        }
        Ok(Some(Checkpoint { len: self.len, mtime_ns: st.mtime_ns }))
    }

    fn ensure_live_identity(&mut self) -> std::io::Result<()> {
        if !self.matches_path(&self.path) {
            self.identity_lost = true;
            return Err(std::io::Error::other(
                "session file identity changed before durable acknowledgement; reopen it",
            ));
        }
        Ok(())
    }

    /// Revalidate writer identity and finish any rollback that failed on an earlier append
    /// (for example due to `ENOSPC` when recording or clearing rollback state).
    fn recover_pending_rollback(&mut self) -> Result<()> {
        if self.identity_lost {
            anyhow::bail!("session file identity changed before durable acknowledgement; reopen it");
        }
        let Some(target) = self.pending_rollback else {
            return Ok(());
        };
        self.ensure_live_identity()?;
        let cur_len = self.file.metadata().context("session recovery failed: reading file metadata")?.len();
        if cur_len < target {
            self.identity_lost = true;
            anyhow::bail!(
                "session recovery failed: file length ({cur_len}) is shorter than rollback offset ({target}); reopen it"
            );
        }
        let rev = self._lock.revision().context("session recovery failed: reading rollback revision")?;
        let marker =
            crate::lock::rollback_record(&self.path).context("session recovery failed: reading rollback marker")?;
        if let Some((dev, ino, offset)) = marker {
            let held = crate::lock::file_id(&self.file).context("session recovery failed: reading file identity")?;
            if (dev, ino) != held || offset != target {
                self.identity_lost = true;
                anyhow::bail!("session recovery failed: rollback marker does not match active writer; reopen it");
            }
        }
        if cur_len == target && rev % 2 == 0 && marker.is_none() {
            self.file.sync_data().context("session recovery failed: syncing journal")?;
            self._lock.sync_data().context("session recovery failed: syncing lock file")?;
            self.ensure_live_identity()?;
            let _ = crate::lock::write_committed_prefix(&self.path, &self.file, target);
            self.pending_rollback = None;
            return Ok(());
        }
        if rev % 2 == 0 {
            self._lock.begin_rollback_revision(&self.path, &self.file, target).with_context(|| {
                format!("session recovery failed: rollback to {target} bytes could not be recorded")
            })?;
        } else if cur_len > target && marker.is_none() {
            crate::lock::write_rollback_marker(&self.path, &self.file, target).with_context(|| {
                format!("session recovery failed: rollback marker for {target} bytes could not be recorded")
            })?;
        }
        self.file
            .set_len(target)
            .and_then(|_| self.file.sync_data())
            .with_context(|| format!("session recovery failed: could not truncate journal to {target} bytes"))?;
        self.ensure_live_identity()?;
        self._lock
            .clear_rollback_marker(&self.path)
            .context("session recovery failed: rollback marker could not be cleared")?;
        self._lock
            .finish_rollback_revision()
            .context("session recovery failed: rollback revision could not be finalized")?;
        let _ = crate::lock::write_committed_prefix(&self.path, &self.file, target);
        self.pending_rollback = None;
        Ok(())
    }

    fn sync(&mut self, committed_len: u64) -> std::io::Result<()> {
        self.ensure_live_identity()?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_DURABLE_SYNC.with(|h| h.borrow_mut().take()) {
            hook();
        }
        #[cfg(test)]
        if FAIL_NEXT_SYNC.with(|f| f.replace(false)) {
            return Err(std::io::Error::other("injected fsync failure"));
        }
        self.file.sync_data()?;
        self.durable_syncs += 1;
        self.ensure_live_identity()?;
        // JSONL is authoritative. If this rebuildable projection cannot advance, retain the last
        // known durable prefix; a fork may be refused until the next successful publication.
        let _ = crate::lock::write_committed_prefix(&self.path, &self.file, committed_len);
        self.ensure_live_identity()?;
        Ok(())
    }

    /// Append one entry. See [`append_batch`](Self::append_batch).
    pub fn append(&mut self, entry: &Value, sync: bool) -> Result<()> {
        self.append_batch(std::slice::from_ref(entry), sync)
    }

    /// Append entries as one write. With `sync`, fsync now at semantic boundaries
    /// (before a model call, before a side-effecting tool, at each step start); without, a
    /// plain O_APPEND write, marked dirty until the next sync / [`flush`](Self::flush). If the write
    /// or its fsync fails, the file is truncated back to its length before the batch
    /// and the error returned; if rollback itself fails (for example due to `ENOSPC`), the
    /// unfinished rollback offset is retained and revalidated before the next append or flush.
    pub fn append_batch(&mut self, entries: &[Value], sync: bool) -> Result<()> {
        if !self.matches_path(&self.path) {
            anyhow::bail!("session file identity changed before append; reopen it");
        }
        self.recover_pending_rollback()?;
        if self.len == 0 && !entries.is_empty() {
            crate::format::check_header(&entries[0])?;
        }
        // a core record is read back through the same types: a malformed one refuses the
        // whole batch before any byte, the torn-tail cut included, is written
        if crate::record::validation_enabled() {
            for (index, entry) in entries.iter().enumerate() {
                if let Err(error) = crate::record::validate(entry) {
                    let kind = entry.get("type").and_then(Value::as_str).unwrap_or_default();
                    anyhow::bail!("{}: record {index} (`{kind}`), {error}", crate::INVALID_RECORD);
                }
            }
        }
        if let Some(at) = self.torn_at {
            // cut the half-written line of an earlier crash before anything lands after it
            self.file.set_len(at)?;
            self.sync(at)?;
            self.torn_at = None;
        }
        let mut buf = String::new();
        for e in entries {
            buf.push_str(&serde_json::to_string(e)?);
            buf.push('\n');
        }
        let before = self.len;
        let res = self
            .file
            .seek(SeekFrom::Start(before))
            .and_then(|_| {
                #[cfg(test)]
                if FAIL_NEXT_WRITE.with(|f| f.replace(false)) {
                    return Err(std::io::Error::other("injected write failure before append"));
                }
                self.file.write_all(buf.as_bytes())
            })
            .and_then(|_| if sync { self.sync(before + buf.len() as u64) } else { Ok(()) });
        if let Err(e) = res {
            #[cfg(test)]
            if let Some(hook) = BEFORE_ROLLBACK.with(|h| h.borrow_mut().take()) {
                hook();
            }
            self.ensure_live_identity()?;
            if self.file.metadata().ok().is_some_and(|m| m.len() == before)
                && self._lock.revision().ok().is_some_and(|r| r % 2 == 0)
                && crate::lock::rollback_pending(&self.path).ok() == Some(false)
            {
                return Err(e.into());
            }
            if let Err(revision_error) = self._lock.begin_rollback_revision(&self.path, &self.file, before) {
                self.pending_rollback = Some(before);
                anyhow::bail!("session batch failed ({e}) and rollback could not be recorded: {revision_error}");
            }
            #[cfg(test)]
            if let Some(hook) = AFTER_ROLLBACK_MARKER.with(|h| h.borrow_mut().take()) {
                hook();
            }
            let rollback = self.file.set_len(before).and_then(|_| self.file.sync_data());
            if rollback.is_err() {
                self.pending_rollback = Some(before);
            }
            if let Err(rollback_error) = rollback {
                anyhow::bail!("session batch failed ({e}) and rollback failed: {rollback_error}");
            }
            #[cfg(test)]
            if let Some(hook) = AFTER_ROLLBACK_TRUNCATE.with(|h| h.borrow_mut().take()) {
                hook();
            }
            self.ensure_live_identity()?;
            if let Err(rollback_error) = self._lock.clear_rollback_marker(&self.path) {
                self.pending_rollback = Some(before);
                anyhow::bail!(
                    "session batch failed ({e}) and its rollback marker could not be cleared: {rollback_error}"
                );
            }
            if let Err(revision_error) = self._lock.finish_rollback_revision() {
                self.pending_rollback = Some(before);
                anyhow::bail!(
                    "session batch failed ({e}) and its rollback revision could not be recorded: {revision_error}"
                );
            }
            let _ = crate::lock::write_committed_prefix(&self.path, &self.file, before);
            return Err(e.into());
        }
        self.ensure_live_identity()?;
        self.len += buf.len() as u64;
        if sync {
            self.dirty = false;
        } else if !entries.is_empty() {
            self.dirty = true;
        }
        Ok(())
    }

    /// fsync pending writes. Returns whether anything was flushed. On
    /// failure the writes stay in place and `dirty`, so the next flush retries them.
    pub fn flush(&mut self) -> Result<bool> {
        self.recover_pending_rollback()?;
        self.ensure_live_identity()?;
        if !self.dirty {
            return Ok(false);
        }
        self.sync(self.len)?;
        self.dirty = false;
        Ok(true)
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }
}

/// A detached snapshot source retains the original identity and exclusive session lock.
/// The held clone retains identity; each read opens and verifies a separate cursor.
pub struct SessionSnapshot {
    held: File,
    lock: std::sync::Arc<LockGuard>,
    path: PathBuf,
}

impl SessionSnapshot {
    fn matches_path(&self) -> bool {
        self.lock.matches_path(&self.path)
            && matches!((crate::lock::file_id(&self.held), crate::lock::path_id(&self.path)), (Ok(a), Ok(b)) if a == b)
    }

    fn open_reader(&self) -> Result<File> {
        let file = File::open(&self.path)?;
        if !self.matches_path()
            || !matches!((crate::lock::file_id(&self.held), crate::lock::file_id(&file)), (Ok(a), Ok(b)) if a == b)
        {
            anyhow::bail!("session source file changed before it was read");
        }
        Ok(file)
    }

    /// Read frozen complete lines from an independent handle verified against the held identity.
    /// Refuse if either handle's path identity or the file's metadata changes during the read.
    pub fn read_snapshot(&self, expected: Option<FileStamp>) -> Result<Vec<Value>> {
        use std::io::Read;
        let mut file = self.open_reader()?;
        let before = crate::stable::stamp_file(&file)?;
        if expected.is_some_and(|stamp| stamp != before) {
            anyhow::bail!("session source file changed since the tail was read");
        }
        #[cfg(test)]
        if let Some(hook) = BEFORE_SNAPSHOT_READ.with(|h| h.borrow_mut().take()) {
            hook();
        }
        file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new((&mut file).take(before.len));
        let mut line = Vec::new();
        let mut out = Vec::new();
        let mut line_no = 0;
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 || line.last() != Some(&b'\n') {
                break;
            }
            line_no += 1;
            let bytes = &line[..n - 1];
            if line_no > 1 && crate::validate::blank(bytes) {
                continue;
            }
            let value = parse_line(bytes).ok_or_else(|| anyhow::anyhow!("{CORRUPT}: line {line_no} does not parse"))?;
            if line_no == 1 {
                crate::format::check_header(&value)?;
            }
            out.push(value);
        }
        drop(reader);
        #[cfg(test)]
        if let Some(hook) = AFTER_SNAPSHOT_READ.with(|h| h.borrow_mut().take()) {
            hook();
        }
        if out.is_empty() {
            anyhow::bail!("{CORRUPT}: missing complete session header");
        }
        if crate::stable::stamp_file(&file)? != before || !self.matches_path() {
            anyhow::bail!("session source file changed while it was read");
        }
        Ok(out)
    }

    /// Read a header and compaction tail through this same held file description.
    pub fn read_tail_snapshot(&self) -> Result<(crate::reverse::Tail, Value, FileStamp)> {
        let mut file = self.open_reader()?;
        let before = crate::stable::stamp_file(&file)?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_SNAPSHOT_READ.with(|h| h.borrow_mut().take()) {
            hook();
        }
        let (tail, header) = crate::reverse::read_from_compaction_held(&mut file, &self.path)?;
        #[cfg(test)]
        if let Some(hook) = AFTER_SNAPSHOT_READ.with(|h| h.borrow_mut().take()) {
            hook();
        }
        if crate::stable::stamp_file(&file)? != before || !self.matches_path() {
            anyhow::bail!("session source file changed while it was read");
        }
        Ok((tail, header, before))
    }
}

#[cfg(test)]
mod independent_snapshot_tests {
    use super::*;

    #[test]
    fn snapshot_io_does_not_move_writer_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let mut writer = SessionFile::create_new(&path).unwrap();
        writer.append(&serde_json::json!({"type":"session", "version":3, "session_id":"s"}), true).unwrap();
        let reader = writer.snapshot_reader().unwrap();
        writer.file.seek(SeekFrom::Start(3)).unwrap();
        reader.read_snapshot(None).unwrap();
        assert_eq!(writer.file.stream_position().unwrap(), 3);
        reader.read_tail_snapshot().unwrap();
        assert_eq!(writer.file.stream_position().unwrap(), 3);
    }

    #[test]
    fn new_session_open_establishes_zero_commit_marker_without_baseline_data_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new-session.jsonl");
        OPEN_BASELINE_SYNCS.with(|c| c.set(0));
        let mut writer = SessionFile::create_new(&path).unwrap();
        assert_eq!(OPEN_BASELINE_SYNCS.with(|c| c.get()), 0);
        assert_eq!(writer.durable_syncs(), 0);
        let point = crate::lock::read_committed_prefix(&path)
            .unwrap()
            .expect("empty newly created session must establish a zero-length commit marker");
        assert_eq!(point.len, 0);
        assert!(crate::read_fork_snapshot(&path, Some("s")).is_err());

        writer.append_batch(&[serde_json::json!({"type":"session", "version":3, "session_id":"s"})], true).unwrap();
        assert_eq!(writer.durable_syncs(), 1);
        assert!(crate::lock::read_committed_prefix(&path).unwrap().is_some_and(|p| p.len > 0));
        assert_eq!(crate::read_fork_snapshot(&path, Some("s")).unwrap().len(), 1);
    }

    #[test]
    fn legacy_non_empty_open_without_commit_marker_establishes_baseline_sync() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-session.jsonl");
        std::fs::write(&path, "{\"type\":\"session\",\"version\":3,\"session_id\":\"s\"}\n").unwrap();
        OPEN_BASELINE_SYNCS.with(|c| c.set(0));
        let _writer = SessionFile::open_existing(&path, None).unwrap();
        assert_eq!(OPEN_BASELINE_SYNCS.with(|c| c.get()), 1);
        assert!(crate::lock::read_committed_prefix(&path).unwrap().is_some());
    }

    /// Child process for first-batch crash-point tests (self-written, modeled after `rollback_crash_child`).
    #[test]
    fn first_batch_crash_child() {
        let Some(path) = std::env::var_os("MARE_STORE_FIRST_BATCH_CRASH_PATH") else { return };
        let mode = std::env::var("MARE_STORE_FIRST_BATCH_CRASH_MODE").unwrap();
        let path = std::path::PathBuf::from(path);
        let mut writer = SessionFile::create_new(&path).unwrap();
        let entries = match mode.as_str() {
            "before_sync" => {
                BEFORE_DURABLE_SYNC.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(|| std::process::exit(74)));
                });
                vec![
                    serde_json::json!({"type":"session", "version":3, "session_id":"s"}),
                    serde_json::json!({"type":"state", "state":"running", "created_at_ms":1}),
                    serde_json::json!({"type":"message", "entry_id":"m1", "parent_entry_id":null, "created_at_ms":2, "message":{"role":"user","content":"partial fork"}}),
                ]
            }
            "before_rollback" => {
                BEFORE_ROLLBACK.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(|| std::process::exit(75)));
                });
                FAIL_NEXT_SYNC.with(|fail| fail.set(true));
                vec![
                    serde_json::json!({"type":"session", "version":3, "session_id":"s"}),
                    serde_json::json!({"type":"custom", "kind":"agent/run-end", "data":{"run_id":"uncommitted"}}),
                ]
            }
            other => panic!("unknown crash mode: {other}"),
        };
        let _ = writer.append_batch(&entries, true);
        panic!("child must exit at crash point {mode}");
    }

    #[test]
    fn first_batch_crash_before_sync_does_not_enter_fork_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash-before-sync.jsonl");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session_file::independent_snapshot_tests::first_batch_crash_child"])
            .env("MARE_STORE_FIRST_BATCH_CRASH_PATH", &path)
            .env("MARE_STORE_FIRST_BATCH_CRASH_MODE", "before_sync")
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(74), "child must exit before initial sync_data");
        let err = crate::read_fork_snapshot(&path, Some("s")).unwrap_err().to_string();
        assert!(err.contains(CORRUPT), "uncommitted first batch must not be promoted into a forkable snapshot: {err}");
    }

    #[test]
    fn first_batch_sync_failure_before_rollback_intent_does_not_enter_fork_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crash-before-rollback.jsonl");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session_file::independent_snapshot_tests::first_batch_crash_child"])
            .env("MARE_STORE_FIRST_BATCH_CRASH_PATH", &path)
            .env("MARE_STORE_FIRST_BATCH_CRASH_MODE", "before_rollback")
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(75), "child must exit after sync failure before rollback intent");
        let err = crate::read_fork_snapshot(&path, Some("s")).unwrap_err().to_string();
        assert!(
            err.contains(CORRUPT),
            "uncommitted first batch with agent/run-end must not be promoted into a forkable snapshot: {err}"
        );
    }

    /// Issue #183: when a failed append also fails to write its rollback marker (e.g. ENOSPC /
    /// unwritable directory), the writer is poisoned permanently and refuses later appends even
    /// after the directory/disk condition recovers.
    #[test]
    fn poisoned_writer_recovers_after_rollback_marker_failure_clears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recover.jsonl");
        let mut writer = SessionFile::create_new(&path).unwrap();
        writer.append(&serde_json::json!({"type":"session", "version":3, "session_id":"s"}), true).unwrap();

        // Deterministically inject a durable sync failure followed by two rollback-marker write
        // failures (works identically under root/DAC_OVERRIDE and non-Unix platforms).
        crate::lock::FAIL_WRITE_ROLLBACK_MARKER.with(|fail| fail.set(2));
        FAIL_NEXT_SYNC.with(|fail| fail.set(true));
        let first_err = writer
            .append(&serde_json::json!({"type":"custom", "kind":"test/fail", "data":{"n":1}}), true)
            .unwrap_err()
            .to_string();
        assert!(
            first_err.contains("rollback could not be recorded"),
            "expected rollback-marker failure, got: {first_err}"
        );

        // While marker writes are still failing, retries report an actionable recovery failure
        // and retain rollback state without corrupting the file.
        let still_blocked = writer
            .append(&serde_json::json!({"type":"custom", "kind":"test/still_blocked", "data":{"n":2}}), true)
            .unwrap_err()
            .to_string();
        assert!(
            still_blocked.contains("session recovery failed"),
            "expected actionable recovery failure while marker writes fail, got: {still_blocked}"
        );
        assert!(writer.matches_path_checked(&path).is_err(), "unrecovered writer remains poisoned");

        // Once the injected marker write failures clear, subsequent append recovers safely.
        let retry = writer.append(&serde_json::json!({"type":"custom", "kind":"test/recovered", "data":{"n":2}}), true);
        assert!(
            retry.is_ok(),
            "writer should recover after rollback failure condition clears, got: {:?}",
            retry.unwrap_err()
        );
        assert!(writer.matches_path_checked(&path).unwrap(), "recovered writer is healthy");
        assert!(!crate::lock::rollback_pending(&path).unwrap(), "rollback marker cleared after recovery");
        let entries = crate::read_fork_snapshot(&path, Some("s")).unwrap();
        assert_eq!(entries.len(), 2, "failed partial batch rolled back, recovered entry persisted: {entries:?}");
        assert_eq!(entries[1]["kind"], "test/recovered");
    }

    #[test]
    fn writer_recovers_after_mid_rollback_marker_clear_failure_and_pre_append_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recover-stages.jsonl");
        let mut writer = SessionFile::create_new(&path).unwrap();
        writer.append(&serde_json::json!({"type":"session", "version":3, "session_id":"s"}), true).unwrap();

        // 1. Failure BEFORE a partial append (0 bytes written) must not poison the writer or try to
        //    write a rollback marker.
        crate::lock::FAIL_WRITE_ROLLBACK_MARKER.with(|fail| fail.set(1));
        FAIL_NEXT_WRITE.with(|fail| fail.set(true));
        let pre_err = writer
            .append(&serde_json::json!({"type":"custom", "kind":"test/pre", "data":{}}), true)
            .unwrap_err()
            .to_string();
        assert!(pre_err.contains("injected write failure"), "{pre_err}");
        assert_eq!(
            crate::lock::FAIL_WRITE_ROLLBACK_MARKER.with(|f| f.replace(0)),
            1,
            "pre-append failure must not attempt to write a rollback marker"
        );
        assert!(writer.matches_path_checked(&path).unwrap(), "pre-append failure must not poison writer");
        writer.append(&serde_json::json!({"type":"custom", "kind":"test/after_pre", "data":{}}), true).unwrap();

        // 2. Failure AFTER truncate (at clear_rollback_marker) leaves an odd lock revision and
        //    leftover .rollback marker; flush() recovers both once marker clearing succeeds.
        crate::lock::FAIL_CLEAR_ROLLBACK_MARKER.with(|fail| fail.set(1));
        FAIL_NEXT_SYNC.with(|fail| fail.set(true));
        let mid_err = writer
            .append(&serde_json::json!({"type":"custom", "kind":"test/mid", "data":{}}), true)
            .unwrap_err()
            .to_string();
        assert!(mid_err.contains("rollback marker could not be cleared"), "{mid_err}");
        assert!(crate::lock::rollback_pending(&path).unwrap());
        assert_eq!(writer._lock.revision().unwrap() % 2, 1, "lock revision must be odd while rollback is incomplete");

        assert!(!writer.flush().unwrap(), "flush recovers pending rollback even when not dirty");
        assert!(!crate::lock::rollback_pending(&path).unwrap());
        assert_eq!(writer._lock.revision().unwrap() % 2, 0, "lock revision must be even after recovery");

        // 3. Failure at finish_rollback_revision's lock-file sync leaves an even revision, correct
        //    length, and cleared marker; retry must still sync the lock file before clearing
        //    pending_rollback.
        crate::lock::FAIL_LOCK_SYNC.with(|fail| fail.set(2));
        FAIL_NEXT_SYNC.with(|fail| fail.set(true));
        let lock_sync_err = writer
            .append(&serde_json::json!({"type":"custom", "kind":"test/lock_sync", "data":{}}), true)
            .unwrap_err()
            .to_string();
        assert!(lock_sync_err.contains("rollback revision could not be recorded"), "{lock_sync_err}");
        assert!(!crate::lock::rollback_pending(&path).unwrap(), "marker already cleared before lock sync failed");
        assert_eq!(writer._lock.revision().unwrap() % 2, 0, "revision already bumped to even before lock sync failed");

        // Second injected lock-sync failure: recovery must NOT clear pending_rollback yet.
        let retry_lock_err = writer.flush().unwrap_err().to_string();
        assert!(
            retry_lock_err.contains("syncing lock file"),
            "even-revision recovery must retry lock-file sync, got: {retry_lock_err}"
        );
        assert!(writer.matches_path_checked(&path).is_err(), "writer stays pending rollback until lock sync succeeds");

        // Third attempt: lock sync succeeds, pending_rollback clears, and new appends persist.
        assert!(!writer.flush().unwrap(), "flush succeeds once lock sync succeeds");
        assert!(writer.matches_path_checked(&path).unwrap(), "writer healthy after lock sync recovery");

        writer.append(&serde_json::json!({"type":"custom", "kind":"test/after_mid", "data":{}}), true).unwrap();
        let entries = crate::read_fork_snapshot(&path, Some("s")).unwrap();
        let kinds: Vec<_> = entries.iter().filter_map(|e| e.get("kind").and_then(Value::as_str)).collect();
        assert_eq!(kinds, ["test/after_pre", "test/after_mid"]);
    }
}
