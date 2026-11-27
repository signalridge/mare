//! Files and directories private to the current user: credentials, session
//! files, locks, the peer registry, state.
//!
//! - Unix: directories 0700, files 0600, from creation (whatever the umask);
//! - Windows (no mode bits): a protected DACL granting full control to the current user, SYSTEM
//!   and Administrators only, set by the creating call itself (`SECURITY_ATTRIBUTES`), so the
//!   object never exists with the inherited ACL, even empty (protected setup marker). A private
//!   directory's ACEs are inheritable: what is created inside it is private too. Without this,
//!   files took the parent's ACL — private under the user profile, but open to every local user
//!   under e.g. a `MARE_HOME` in `C:\mare` (`C:\` grants Users read).
//!
//! Existing objects keep their permissions (as with Unix `mode` at creation); `mare doctor --fix`
//! tightens them.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

#[cfg(windows)]
#[path = "private_win.rs"]
mod win;
#[cfg(windows)]
pub use win::{check, check_handle, check_owner_handle, restrict, user_sid};

/// Create `dir` and any missing parents; a directory created here is private (Unix: every
/// created component 0700; Windows: the leaf, whose inheritable DACL covers its contents).
pub fn create_dir_all(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(windows)]
    {
        win::create_dir_all(dir)
    }
}

/// Create one private directory exclusively. The parent must already exist.
pub fn create_dir_new(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(windows)]
    {
        win::create_dir_new(dir)
    }
}

/// Create `path` exclusively (fails if it exists), private from creation, open for writing.
pub fn create_new(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(path)
    }
    #[cfg(windows)]
    {
        win::create_new(path)
    }
}

/// Open `path` with `o` (which must allow creating it); a file created by this call is private.
pub fn open(o: &mut OpenOptions, path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600).open(path)
    }
    #[cfg(windows)]
    {
        // `OpenOptions` can't carry a security descriptor: create the missing file private
        // first (losing that race to another creator is fine), then open it as asked
        match create_new(path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        o.open(path)
    }
}

/// Exclusive advisory file-lock guard that unlocks on explicit [`LockGuard::release`] or drop.
pub struct LockGuard(Option<File>);

impl LockGuard {
    /// Explicitly unlock and release the underlying file handle.
    pub fn release(&mut self) -> io::Result<()> {
        if let Some(f) = self.0.take() {
            f.unlock()?;
        }
        Ok(())
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            let _ = f.unlock();
        }
    }
}

/// Failure modes while waiting for a private advisory file lock.
#[derive(Debug)]
pub enum LockError {
    Cancelled,
    TimedOut,
    Io(io::Error),
}

enum PollWait {
    Retry(Duration),
    TimedOut,
}

fn prepare_lock_file(path: &Path, create_parent: bool) -> Result<File, LockError> {
    if create_parent
        && let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
        && !dir.exists()
    {
        create_dir_all(dir).map_err(LockError::Io)?;
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(LockError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing symlink lock file: {}", path.display()),
        )));
    }
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(false).write(true);
    let file = open(&mut opts, path).map_err(LockError::Io)?;
    let meta = file.metadata().map_err(LockError::Io)?;
    if !meta.file_type().is_file() {
        return Err(LockError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing non-regular lock file: {}", path.display()),
        )));
    }
    Ok(file)
}

fn next_poll_delay(deadline: Option<Instant>, poll: Duration) -> PollWait {
    let Some(dl) = deadline else {
        return PollWait::Retry(poll);
    };
    match dl.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) {
        Some(remaining) => PollWait::Retry(poll.min(remaining)),
        None => PollWait::TimedOut,
    }
}

/// Acquire an exclusive advisory lock on `path` in a blocking loop with `poll` intervals,
/// optional `timeout`, and optional `cancel` token.
pub fn lock_file_blocking(
    path: &Path,
    timeout: Option<Duration>,
    poll: Duration,
    create_parent: bool,
    cancel: Option<&CancellationToken>,
) -> Result<LockGuard, LockError> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        return Err(LockError::Cancelled);
    }
    let file = prepare_lock_file(path, create_parent)?;
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(LockError::Cancelled);
        }
        match file.try_lock() {
            Ok(()) => return Ok(LockGuard(Some(file))),
            Err(std::fs::TryLockError::WouldBlock) => {
                #[cfg(any(debug_assertions, test, feature = "test-support"))]
                testing::notify_contended(path);
                match next_poll_delay(deadline, poll) {
                    PollWait::Retry(delay) => std::thread::sleep(delay),
                    PollWait::TimedOut => return Err(LockError::TimedOut),
                }
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(LockError::Io(e)),
        }
    }
}

/// Acquire an exclusive advisory lock on an already-opened lock file handle asynchronously.
pub async fn lock_opened_file_async(
    file: File,
    path: &Path,
    timeout: Option<Duration>,
    poll: Duration,
    cancel: Option<&CancellationToken>,
) -> Result<LockGuard, LockError> {
    let _ = path;
    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(LockGuard(Some(file))),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(LockError::Io(e)),
        }
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(LockError::Cancelled);
        }
        #[cfg(any(debug_assertions, test, feature = "test-support"))]
        testing::notify_contended(path);
        match next_poll_delay(deadline, poll) {
            PollWait::Retry(delay) => tokio::time::sleep(delay).await,
            PollWait::TimedOut => return Err(LockError::TimedOut),
        }
    }
}

/// Acquire an exclusive advisory lock on `path` asynchronously with `poll` intervals,
/// optional `timeout`, and optional `cancel` token.
pub async fn lock_file_async(
    path: &Path,
    timeout: Option<Duration>,
    poll: Duration,
    create_parent: bool,
    cancel: Option<&CancellationToken>,
) -> Result<LockGuard, LockError> {
    let file = prepare_lock_file(path, create_parent)?;
    lock_opened_file_async(file, path, timeout, poll, cancel).await
}

/// Canonicalize `p` (or its parent + filename when `p` does not exist yet) as a stable lock key.
pub fn canonical_lock_path(p: &Path) -> PathBuf {
    if let Ok(c) = dunce::canonicalize(p) {
        return c;
    }
    if let (Some(parent), Some(name)) = (p.parent(), p.file_name())
        && let Ok(cp) = dunce::canonicalize(parent)
    {
        return cp.join(name);
    }
    p.to_path_buf()
}

/// Ensure parent directory exists privately and canonicalize the lock/credential key off the async executor.
pub async fn canonicalize_lock_key(path: PathBuf) -> PathBuf {
    tokio::task::spawn_blocking(move || {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            let _ = create_dir_all(dir);
        }
        canonical_lock_path(&path)
    })
    .await
    .unwrap_or_default()
}

#[cfg(any(debug_assertions, test, feature = "test-support"))]
pub mod testing {
    #[cfg(all(windows, any(test, feature = "test-support")))]
    pub use super::win::testing::*;

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Condvar, Mutex, OnceLock};
    use std::time::Duration;
    use tokio::sync::Notify;

    struct ContendedSignal {
        notify: Notify,
        state: (Mutex<bool>, Condvar),
    }

    static HOOKS: OnceLock<Mutex<HashMap<PathBuf, Arc<ContendedSignal>>>> = OnceLock::new();

    fn hooks() -> &'static Mutex<HashMap<PathBuf, Arc<ContendedSignal>>> {
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Drop guard that unregisters the contended-wait hook for a lock path.
    pub struct ContendedWait {
        key: PathBuf,
        signal: Arc<ContendedSignal>,
    }

    impl ContendedWait {
        /// Wait asynchronously until a caller hits `WouldBlock` and enters the retry sleep.
        pub async fn wait(&self) {
            let notified = self.signal.notify.notified();
            {
                let fired = self.signal.state.0.lock().expect("contended signal poisoned");
                if *fired {
                    return;
                }
            }
            notified.await;
        }

        /// Wait synchronously until a caller hits `WouldBlock` and enters the retry sleep.
        pub fn wait_blocking(&self, timeout: Duration) -> bool {
            let guard = self.signal.state.0.lock().expect("contended signal poisoned");
            if *guard {
                return true;
            }
            let (guard, _) =
                self.signal.state.1.wait_timeout_while(guard, timeout, |fired| !*fired).expect("contended signal");
            *guard
        }
    }

    impl Drop for ContendedWait {
        fn drop(&mut self) {
            if let Ok(mut map) = hooks().lock() {
                map.remove(&self.key);
            }
        }
    }

    /// Observe when a lock acquisition hits `WouldBlock` and enters contended wait for `path`.
    pub fn watch_contended(path: &Path) -> ContendedWait {
        let key = super::canonical_lock_path(path);
        let signal = Arc::new(ContendedSignal { notify: Notify::new(), state: (Mutex::new(false), Condvar::new()) });
        hooks().lock().expect("filelock hooks poisoned").insert(key.clone(), signal.clone());
        ContendedWait { key, signal }
    }

    pub(super) fn notify_contended(path: &Path) {
        let key = super::canonical_lock_path(path);
        if let Ok(map) = hooks().lock()
            && let Some(signal) = map.get(&key)
        {
            *signal.state.0.lock().expect("contended signal poisoned") = true;
            signal.state.1.notify_all();
            signal.notify.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn exclusive_private_directory_rejects_existing_directory() {
        let root = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        super::testing::open_to_everyone(root.path());
        let dir = root.path().join("attachments");
        super::create_dir_new(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        }
        #[cfg(windows)]
        assert_eq!(super::check(&dir), Ok(()));
        std::fs::write(dir.join("keep"), "original").unwrap();
        assert_eq!(super::create_dir_new(&dir).unwrap_err().kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(dir.join("keep")).unwrap(), "original");
    }

    #[test]
    fn lock_file_rejects_symlinks_non_regular_files_and_pre_cancelled_tokens() {
        let root = tempfile::tempdir().unwrap();

        // Pre-cancelled token must abort immediately without creating the lock file.
        let cancel = super::CancellationToken::new();
        cancel.cancel();
        let cancel_path = root.path().join("never_created.lock");
        let err = match super::lock_file_blocking(
            &cancel_path,
            Some(std::time::Duration::from_millis(50)),
            std::time::Duration::from_millis(5),
            false,
            Some(&cancel),
        ) {
            Ok(_) => panic!("expected Cancelled"),
            Err(e) => e,
        };
        assert!(matches!(err, super::LockError::Cancelled), "expected Cancelled, got {err:?}");
        assert!(!cancel_path.exists());

        // Non-regular target (directory) must be refused.
        let dir_lock = root.path().join("dir.lock");
        std::fs::create_dir(&dir_lock).unwrap();
        let err = match super::lock_file_blocking(
            &dir_lock,
            Some(std::time::Duration::from_millis(50)),
            std::time::Duration::from_millis(5),
            false,
            None,
        ) {
            Ok(_) => panic!("expected Io error for dir lock"),
            Err(e) => e,
        };
        assert!(matches!(err, super::LockError::Io(_)), "expected Io error for dir lock, got {err:?}");

        // Symlink lock path must be refused before opening.
        #[cfg(unix)]
        {
            let target = root.path().join("real.lock");
            std::fs::write(&target, "").unwrap();
            let link = root.path().join("sym.lock");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let err = match super::lock_file_blocking(
                &link,
                Some(std::time::Duration::from_millis(50)),
                std::time::Duration::from_millis(5),
                false,
                None,
            ) {
                Ok(_) => panic!("expected PermissionDenied Io error for symlink"),
                Err(e) => e,
            };
            match err {
                super::LockError::Io(io_err) => {
                    assert_eq!(io_err.kind(), std::io::ErrorKind::PermissionDenied);
                    assert!(io_err.to_string().contains("symlink"), "{io_err}");
                }
                other => panic!("expected PermissionDenied Io error for symlink, got {other:?}"),
            }

            // FIFO is a non-regular file that opens read/write without EACCES and hits the !is_file guard.
            let fifo = root.path().join("fifo.lock");
            let status = std::process::Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
            assert!(status.success(), "mkfifo failed");
            // Keep a reader open so O_WRONLY open does not block, then verify prepare_lock_file rejects it.
            let _reader = std::fs::OpenOptions::new().read(true).write(true).open(&fifo).unwrap();
            let err = match super::lock_file_blocking(
                &fifo,
                Some(std::time::Duration::from_millis(50)),
                std::time::Duration::from_millis(5),
                false,
                None,
            ) {
                Ok(_) => panic!("expected PermissionDenied Io error for FIFO"),
                Err(e) => e,
            };
            match err {
                super::LockError::Io(io_err) => {
                    assert_eq!(io_err.kind(), std::io::ErrorKind::PermissionDenied);
                    assert!(io_err.to_string().contains("non-regular"), "{io_err}");
                }
                other => panic!("expected PermissionDenied Io error for FIFO, got {other:?}"),
            }
        }

        // next_poll_delay without deadline always retries with the given poll interval.
        assert!(matches!(
            super::next_poll_delay(None, std::time::Duration::from_millis(7)),
            super::PollWait::Retry(d) if d == std::time::Duration::from_millis(7)
        ));

        // Explicit release + ContendedWait already-fired fast paths.
        let lock_path = root.path().join("nested/sub/app.lock");
        let mut guard = super::lock_file_blocking(
            &lock_path,
            Some(std::time::Duration::from_millis(50)),
            std::time::Duration::from_millis(5),
            true,
            None,
        )
        .unwrap();
        let watcher = super::testing::watch_contended(&lock_path);
        super::testing::notify_contended(&lock_path);
        assert!(watcher.wait_blocking(std::time::Duration::from_millis(50)));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(watcher.wait());
        drop(watcher);
        guard.release().unwrap();
    }
}
