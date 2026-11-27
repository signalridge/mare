//! The writer's lock: a non-blocking flock on `<session>.lock` re-checked against the inode,
//! the in-process registry of open sessions, file identity, and private dirs and files.

use super::*;

/// fsync the directory holding `path` (Unix; a no-op elsewhere): makes a newly created or
/// renamed entry in it durable.
pub fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    if let Some(dir) = path.parent() {
        let dir = if dir.as_os_str().is_empty() { Path::new(".") } else { dir };
        let directory = File::open(dir)?;
        if !directory.metadata()?.is_dir() {
            return Err(std::io::Error::other("session parent is not a directory"));
        }
        directory.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The lock file of a session: `<session>.jsonl.lock`. Separate from the session file so that
/// replacing a session file never invalidates the lock.
pub fn lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// Private rollback intent written before an odd lock revision. It preserves the append batch's
/// start offset if the process exits before the journal truncate is durable.
pub fn rollback_path(path: &Path) -> PathBuf {
    let mut s = lock_path(path).into_os_string();
    s.push(".rollback");
    PathBuf::from(s)
}

/// Rebuildable durable-prefix watermark used by lock-free fork readers.
pub fn commit_path(path: &Path) -> PathBuf {
    let mut s = lock_path(path).into_os_string();
    s.push(".commit");
    PathBuf::from(s)
}

const ROLLBACK_MAGIC: &[u8; 8] = b"MRROLL01";
const ROLLBACK_RECORD_LEN: usize = 32;

fn rollback_record(path: &Path) -> std::io::Result<Option<(u64, u64, u64)>> {
    use std::io::Read;

    let mut file = match File::open(rollback_path(path)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::with_capacity(ROLLBACK_RECORD_LEN);
    file.read_to_end(&mut bytes)?;
    if bytes.len() != ROLLBACK_RECORD_LEN || &bytes[..ROLLBACK_MAGIC.len()] != ROLLBACK_MAGIC {
        return Err(std::io::Error::other("invalid session rollback marker"));
    }
    let read = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed rollback record"));
    Ok(Some((read(8), read(16), read(24))))
}

pub(crate) fn rollback_pending(path: &Path) -> std::io::Result<bool> {
    Ok(rollback_record(path)?.is_some())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CommittedPrefix {
    pub dev: u64,
    pub ino: u64,
    pub len: u64,
    pub lock_dev: u64,
    pub lock_ino: u64,
    pub revision: u64,
}

const COMMIT_MAGIC: &[u8; 8] = b"MRCMIT01";

pub(crate) fn read_committed_prefix(path: &Path) -> std::io::Result<Option<CommittedPrefix>> {
    use std::io::Read;

    let mut file = match File::open(commit_path(path)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::with_capacity(56);
    file.read_to_end(&mut bytes)?;
    if bytes.len() != 56 || &bytes[..COMMIT_MAGIC.len()] != COMMIT_MAGIC {
        return Err(std::io::Error::other("invalid session commit marker"));
    }
    let read = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed commit record"));
    Ok(Some(CommittedPrefix {
        dev: read(8),
        ino: read(16),
        len: read(24),
        lock_dev: read(32),
        lock_ino: read(40),
        revision: read(48),
    }))
}

pub(crate) fn write_committed_prefix(path: &Path, file: &File, len: u64) -> std::io::Result<()> {
    let (dev, ino) = file_id(file)?;
    let lock = match crate::stamp(&lock_path(path)) {
        Ok(stamp) => (stamp.dev, stamp.ino, stamp.len),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, 0, 0),
        Err(error) => return Err(error),
    };
    let mut marker = Vec::with_capacity(56);
    marker.extend_from_slice(COMMIT_MAGIC);
    marker.extend_from_slice(&dev.to_le_bytes());
    marker.extend_from_slice(&ino.to_le_bytes());
    marker.extend_from_slice(&len.to_le_bytes());
    marker.extend_from_slice(&lock.0.to_le_bytes());
    marker.extend_from_slice(&lock.1.to_le_bytes());
    marker.extend_from_slice(&lock.2.to_le_bytes());
    mare_shared::paths::write_atomic(&commit_path(path), &marker, mare_shared::paths::WriteMode::Private)
}

fn clear_rollback_marker(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(rollback_path(path)) {
        Ok(()) => sync_dir(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// The OS lock on a session plus its entry in the in-process registry (removed on drop).
pub struct LockGuard {
    _file: File,
    key: PathBuf,
}

impl LockGuard {
    /// Check the actual locked file, including names that differ by case or separators.
    pub(crate) fn matches_path(&self, session_path: &Path) -> bool {
        self.matches_path_checked(session_path).unwrap_or(false)
    }

    pub(crate) fn matches_path_checked(&self, session_path: &Path) -> std::io::Result<bool> {
        if !cfg!(any(unix, windows)) {
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "session lock identity is unavailable"));
        }
        let held = file_id(&self._file)?;
        match path_id(&lock_path(session_path)) {
            Ok(current) => Ok(held == current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Begin a rollback by making the persistent per-session revision odd. While this writer holds
    /// the session lock, the index treats the odd revision as in progress and won't publish a row.
    pub(crate) fn begin_rollback_revision(&self, session_path: &Path, file: &File, offset: u64) -> std::io::Result<()> {
        let (dev, ino) = file_id(file)?;
        let mut marker = Vec::with_capacity(ROLLBACK_RECORD_LEN);
        marker.extend_from_slice(ROLLBACK_MAGIC);
        marker.extend_from_slice(&dev.to_le_bytes());
        marker.extend_from_slice(&ino.to_le_bytes());
        marker.extend_from_slice(&offset.to_le_bytes());
        mare_shared::paths::write_atomic(
            &rollback_path(session_path),
            &marker,
            mare_shared::paths::WriteMode::Private,
        )?;

        let next = self
            ._file
            .metadata()?
            .len()
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("session rollback revision overflow"))?;
        if next % 2 == 0 {
            return Err(std::io::Error::other("session rollback revision is already in progress"));
        }
        self._file.set_len(next)?;
        self._file.sync_data()
    }

    /// Complete a rollback after the JSONL truncate is durable, making the revision even again.
    pub(crate) fn finish_rollback_revision(&self) -> std::io::Result<()> {
        let next = self
            ._file
            .metadata()?
            .len()
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("session rollback revision overflow"))?;
        if next % 2 != 0 {
            return Err(std::io::Error::other("session rollback revision was not in progress"));
        }
        self._file.set_len(next)?;
        self._file.sync_data()
    }

    pub(crate) fn clear_rollback_marker(&self, session_path: &Path) -> std::io::Result<()> {
        clear_rollback_marker(session_path)
    }

    /// Complete an odd revision after this new writer has acquired the session lock following an
    /// interrupted rollback. Only the journal writer repairs the durable lock marker.
    pub(crate) fn recover_rollback_revision(&self, session_path: &Path) -> std::io::Result<()> {
        if let Some((dev, ino, offset)) = rollback_record(session_path)? {
            let session = OpenOptions::new().write(true).open(session_path)?;
            if file_id(&session)? != (dev, ino) {
                return Err(std::io::Error::other("session file changed during rollback recovery"));
            }
            if session.metadata()?.len() < offset {
                return Err(std::io::Error::other("session file is shorter than its rollback offset"));
            }
            session.set_len(offset)?;
            session.sync_data()?;
            drop(session);
            clear_rollback_marker(session_path)?;
        }
        let len = self._file.metadata()?.len();
        if len % 2 != 0 {
            self._file.set_len(len + 1)?;
            self._file.sync_data()?;
        }
        Ok(())
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Ok(mut g) = OPEN.lock()
            && let Some(set) = g.as_mut()
        {
            set.remove(&self.key);
        }
    }
}

/// Sessions this process has open for writing (keyed by the canonical lock path).
static OPEN: std::sync::Mutex<Option<std::collections::HashSet<PathBuf>>> = std::sync::Mutex::new(None);

fn session_lock_key(session_path: &Path) -> PathBuf {
    let lp = lock_path(session_path);
    dunce::canonicalize(session_path.parent().unwrap_or(Path::new(".")))
        .map(|d| d.join(lp.file_name().unwrap_or_default()))
        .unwrap_or(lp)
}

/// Whether a live session writer currently owns the lock. The index uses this only for an odd
/// rollback revision: odd plus a held writer means the rollback is in progress; odd plus no writer
/// means it was interrupted and the next SessionFile open can finish recovery.
pub(crate) fn session_locked(session_path: &Path) -> Result<bool> {
    let key = session_lock_key(session_path);
    if OPEN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().is_some_and(|open| open.contains(&key)) {
        return Ok(true);
    }
    let lock_path = lock_path(session_path);
    let file = match OpenOptions::new().read(true).write(true).open(&lock_path) {
        Ok(file) => file,
        // Do not create a lock file while probing an old rollback marker: deletion may have
        // removed it after the marker was stat'ed.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    match file.try_lock() {
        Ok(()) => Ok(path_id(&lock_path).ok() != Some(file_id(&file)?)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Identity of an open file: device + inode on Unix; volume serial number + file index on
/// Windows (`GetFileInformationByHandle`; std's `MetadataExt::{volume_serial_number,
/// file_index}` are unstable). Taken from the handle, not the path, so the lock re-check
/// compares what we locked with what the path names now.
pub(crate) fn file_id(f: &File) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = f.metadata()?;
        Ok((m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid handle owned by `f`; `info` is a properly sized out parameter
        if unsafe { GetFileInformationByHandle(f.as_raw_handle() as _, &mut info) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((info.dwVolumeSerialNumber as u64, ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = f;
        Ok((0, 0))
    }
}

/// The identity of the file at `path` as "dev:ino" (Windows: "volume:index"), for keys that must
/// not depend on how the path is spelled (symlinks, case-insensitive file systems).
/// None when the file doesn't exist.
pub fn file_identity(path: &Path) -> Option<String> {
    path_id(path).ok().map(|(d, i)| format!("{d}:{i}"))
}

/// [`file_id`] of the file currently at `path`.
pub(crate) fn path_id(path: &Path) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path)?;
        Ok((m.dev(), m.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS;
        let file = OpenOptions::new().read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
        file_id(&file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        file_id(&File::open(path)?)
    }
}

#[cfg(test)]
thread_local! {
    /// test seam: runs between opening the lock file and locking it (to stage the race the
    /// inode re-check exists for)
    pub(crate) static BETWEEN_OPEN_AND_LOCK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

/// Non-blocking exclusive flock on `lp`, then one check that the file at `lp` is still the one
/// locked (a lock names an inode, not a path: unlinked and recreated meanwhile → lock the new
/// one). One retry: a lock file that keeps being replaced is an error.
fn lock_file(lp: &Path) -> Result<File> {
    for _ in 0..2 {
        let f = open_private(OpenOptions::new().truncate(false).read(true).write(true), lp, true)?;
        #[cfg(test)]
        if let Some(hook) = BETWEEN_OPEN_AND_LOCK.with(|h| h.borrow_mut().take()) {
            hook();
        }
        match f.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!("{LOCKED}: {}", lp.display()),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        if path_id(lp).ok() == Some(file_id(&f)?) {
            return Ok(f);
        }
    }
    anyhow::bail!("{LOCKED}: lock file {} keeps changing", lp.display())
}

/// Session directories are private: created 0700 on Unix, with the private DACL
/// on Windows (`mare_shared::private`); existing ones untouched (`mare doctor --fix` tightens
/// those).
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    mare_shared::private::create_dir_all(dir)
}

/// Open a session or lock file; one created here is private: 0600 on Unix, the
/// private DACL on Windows. `create = false`: it must exist.
pub(crate) fn open_private(o: &mut OpenOptions, path: &Path, create: bool) -> std::io::Result<File> {
    if create { mare_shared::private::open(o.create(true), path) } else { o.open(path) }
}

/// Take the session's lock for writing. Order for writers: lock → read / validate →
/// write. A session this process already has open is refused ([`OPEN_HERE`]).
pub fn lock_session(path: &Path) -> Result<LockGuard> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let lp = lock_path(path);
    let key = session_lock_key(path);
    let mut g = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    let set = g.get_or_insert_with(Default::default);
    if set.contains(&key) {
        anyhow::bail!("{OPEN_HERE}: {}", path.display());
    }
    let f = lock_file(&lp)?;
    set.insert(key.clone());
    Ok(LockGuard { _file: f, key })
}
