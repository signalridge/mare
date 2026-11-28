//! `mare.lock.file(path, {timeout_ms?, cancel?}) -> guard`: a cross-process exclusive lock on
//! `path` (created if missing), for credential stores refreshed by several mare processes at once
//! .
//! Waiting polls a non-blocking flock (`File::try_lock`, LockFileEx on Windows); `cancel` and
//! `timeout_ms` only end the *wait* — once acquired the guard is held until `guard:release()` or
//! until it is garbage-collected.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use mlua::{Lua, Table, UserData, UserDataMethods};
use sha2::{Digest, Sha256};

use mare_shared::lua::rt_err;
use mare_shared::private::{LockError, LockGuard, create_dir_all, lock_file_async, lock_opened_file_async};

struct Guard(std::sync::Mutex<Option<LockGuard>>);

impl UserData for Guard {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("release", |_, this, ()| {
            if let Ok(mut g) = this.0.lock()
                && let Some(mut guard) = g.take()
            {
                let _ = guard.release();
            }
            Ok(())
        });
    }
}

fn open_anchored_lock_file(root: &Path, path: &Path, create_parent: bool) -> std::io::Result<std::fs::File> {
    let norm_root = crate::fs::normalize_lexical(root);
    let norm_path = crate::fs::normalize_lexical(path);
    let rel = norm_path.strip_prefix(&norm_root).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("anchored lock path {} is outside root {}", path.display(), root.display()),
        )
    })?;
    if rel.as_os_str().is_empty()
        || rel.file_name().is_none()
        || !rel.components().all(|c| matches!(c, Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("invalid anchored lock relative path: {}", rel.display()),
        ));
    }
    if create_parent && !norm_root.exists() {
        create_dir_all(&norm_root)?;
    }
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        let (dir_fd, _dir_path, name) = crate::fs::open_parent_dir_from_root(&norm_root, rel, Some(0o600), None)?;
        let name_p = Path::new(&name);
        let flags = libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        let open_openat = || -> std::io::Result<std::fs::File> {
            let c = CString::new(name_p.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
            let fd = unsafe { libc::openat(dir_fd.as_raw_fd(), c.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(unsafe { std::fs::File::from_raw_fd(fd) })
        };
        #[cfg(target_os = "linux")]
        let file = match crate::fs::open_beneath_fd(dir_fd.as_raw_fd(), name_p, flags, 0o600) {
            Ok(fd) => std::fs::File::from(fd),
            Err(e) if e.raw_os_error() == Some(libc::ENOSYS) => open_openat()?,
            Err(e) => return Err(e),
        };
        #[cfg(not(target_os = "linux"))]
        let file = open_openat()?;
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing non-regular lock file: {}", path.display()),
            ));
        }
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
        let is_reparse = |meta: &std::fs::Metadata| {
            meta.file_type().is_symlink() || (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
        };
        let root_meta = std::fs::symlink_metadata(&norm_root)?;
        if is_reparse(&root_meta) || !root_meta.is_dir() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing reparse point lock root: {}", norm_root.display()),
            ));
        }
        let mut cur = norm_root.clone();
        for comp in rel.parent().into_iter().flat_map(|p| p.components()) {
            cur.push(comp.as_os_str());
            match std::fs::symlink_metadata(&cur) {
                Ok(m) if is_reparse(&m) || !m.is_dir() => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!("refusing reparse point in lock path: {}", cur.display()),
                    ));
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && create_parent => {
                    match mare_shared::private::create_dir_new(&cur) {
                        Ok(()) => {}
                        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                            let m = std::fs::symlink_metadata(&cur)?;
                            if is_reparse(&m) || !m.is_dir() {
                                return Err(std::io::Error::new(
                                    std::io::ErrorKind::PermissionDenied,
                                    format!("refusing reparse point in lock path: {}", cur.display()),
                                ));
                            }
                        }
                        Err(err) => return Err(err),
                    }
                }
                Err(e) => return Err(e),
            }
        }
        if let Ok(m) = std::fs::symlink_metadata(&norm_path)
            && is_reparse(&m)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing reparse point lock file: {}", norm_path.display()),
            ));
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        let file = mare_shared::private::open(&mut opts, &norm_path)?;
        let handle_meta = file.metadata()?;
        if is_reparse(&handle_meta) || !handle_meta.file_type().is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("refusing reparse or non-regular lock file: {}", norm_path.display()),
            ));
        }
        let canon_root = dunce::canonicalize(&norm_root)?;
        let canon_file = dunce::canonicalize(&norm_path)?;
        if !canon_file.starts_with(&canon_root) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("lock path escaped root: {}", norm_path.display()),
            ));
        }
        Ok(file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "anchored lock files are unsupported"))
    }
}

/// Resolve an OS-user-shared, `MARE_HOME`-independent lock root so separate Mare processes
/// running under different profiles still serialize keyed resource locks for the same OS user.
fn key_lock_root() -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let euid = unsafe { libc::geteuid() };
        if let Some(dir) =
            std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute())
            && let Ok(meta) = std::fs::symlink_metadata(&dir)
            && meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == euid
        {
            return dir.join("mare").join("locks");
        }
        let tmp_root = std::env::temp_dir().join(format!("mare-locks-{euid}"));
        let squatted = std::fs::symlink_metadata(&tmp_root)
            .ok()
            .is_some_and(|m| m.file_type().is_symlink() || !m.is_dir() || m.uid() != euid || (m.mode() & 0o077) != 0);
        if !squatted {
            return tmp_root;
        }
        if let Some(home) = dirs::home_dir() {
            return home.join(".local").join("state").join("mare").join("locks");
        }
        tmp_root
    }
    #[cfg(windows)]
    {
        if let Some(local) = dirs::data_local_dir() {
            return local.join("mare").join("locks");
        }
        std::env::temp_dir().join("mare-locks")
    }
    #[cfg(not(any(unix, windows)))]
    {
        std::env::temp_dir().join("mare-locks")
    }
}

fn key_lock_paths(key: &str) -> mlua::Result<(PathBuf, PathBuf)> {
    if key.is_empty() {
        return Err(rt_err("lock key must not be empty"));
    }
    let root = key_lock_root();
    let digest = Sha256::digest(key.as_bytes());
    let mut hex = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    let path = root.join(format!("{hex}.lock"));
    Ok((root, path))
}

async fn acquire(
    path: String,
    root: Option<PathBuf>,
    timeout: Option<Duration>,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> mlua::Result<Guard> {
    let p = PathBuf::from(&path);
    let poll = Duration::from_millis(10);
    let res = match root {
        Some(r) => match open_anchored_lock_file(&r, &p, true) {
            Ok(file) => lock_opened_file_async(file, &p, timeout, poll, cancel.as_ref()).await,
            Err(e) => Err(LockError::Io(e)),
        },
        None => lock_file_async(&p, timeout, poll, true, cancel.as_ref()).await,
    };
    match res {
        Ok(guard) => Ok(Guard(std::sync::Mutex::new(Some(guard)))),
        Err(LockError::Cancelled) => Err(rt_err("aborted")),
        Err(LockError::TimedOut) => Err(rt_err(format!("timed out waiting for the lock on {path}"))),
        Err(LockError::Io(e)) => Err(rt_err(e)),
    }
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let t = lua.create_table()?;
    t.set(
        "file",
        lua.create_async_function(|lua, (path, opts): (String, Option<Table>)| async move {
            let path = crate::exec_ctx::checked_process_path(&lua, "write", &path)?.to_string_lossy().into_owned();
            let root = match opts.as_ref().and_then(|o| o.get::<Option<String>>("root").ok().flatten()) {
                Some(r) => Some(crate::exec_ctx::checked_process_path(&lua, "write", &r)?),
                None => None,
            };
            let timeout = opts
                .as_ref()
                .and_then(|o| o.get::<Option<f64>>("timeout_ms").ok().flatten())
                .map(|ms| Duration::from_millis(ms as u64));
            let cancel = crate::task::opt_token(&opts);
            acquire(path, root, timeout, cancel).await
        })?,
    )?;
    t.set(
        "key",
        lua.create_async_function(|_lua, (key, opts): (String, Option<Table>)| async move {
            let (root, path) = key_lock_paths(&key)?;
            let timeout = opts
                .as_ref()
                .and_then(|o| o.get::<Option<f64>>("timeout_ms").ok().flatten())
                .map(|ms| Duration::from_millis(ms as u64));
            let cancel = crate::task::opt_token(&opts);
            acquire(path.to_string_lossy().into_owned(), Some(root), timeout, cancel).await
        })?,
    )?;
    mare.set("lock", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_holder_waits_until_the_first_releases() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("auth.json.lock").to_string_lossy().to_string();
        let g = acquire(p.clone(), None, None, None).await.unwrap();
        let busy = acquire(p.clone(), None, Some(Duration::from_millis(80)), None).await;
        assert!(busy.is_err(), "held elsewhere: the wait times out");
        g.0.lock().unwrap().take().unwrap().release().unwrap();
        assert!(acquire(p, None, Some(Duration::from_millis(500)), None).await.is_ok());
    }

    /// Windows: private DACLs (`mare_shared::private`), below a directory open to everyone.
    #[tokio::test]
    async fn lock_files_and_new_parent_dirs_are_private() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(windows)]
        mare_shared::private::testing::open_to_everyone(dir.path());
        let sub = dir.path().join("state");
        let p = sub.join("x.lock");
        let g = acquire(p.to_string_lossy().to_string(), None, None, None).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&p), 0o600);
            assert_eq!(mode(&sub), 0o700);
        }
        #[cfg(windows)]
        for p in [&p, &sub] {
            assert_eq!(mare_shared::private::check(p), Ok(()));
        }
        drop(g);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn anchored_lock_rejects_symlink_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let outside = dir.path().join("outside.lock");
        std::fs::create_dir_all(&state).unwrap();
        std::os::unix::fs::symlink(&outside, state.join("news.json.lock")).unwrap();
        let res = acquire(
            state.join("news.json.lock").to_string_lossy().to_string(),
            Some(state),
            Some(Duration::from_millis(100)),
            None,
        )
        .await;
        assert!(res.is_err(), "symlink lock sidecar must be rejected");
        assert!(!outside.exists(), "external lock target must not be created");

        // Anchored lock rejects path outside root and empty relative path, and creates missing root.
        let outside_res = acquire(
            outside.to_string_lossy().to_string(),
            Some(dir.path().join("state")),
            Some(Duration::from_millis(100)),
            None,
        )
        .await;
        assert!(outside_res.is_err(), "path outside anchor root must be rejected");

        let same_as_root = acquire(
            dir.path().join("state").to_string_lossy().to_string(),
            Some(dir.path().join("state")),
            Some(Duration::from_millis(100)),
            None,
        )
        .await;
        assert!(same_as_root.is_err(), "empty relative anchored path must be rejected");

        let new_root = dir.path().join("fresh_state");
        let created = acquire(
            new_root.join("ok.lock").to_string_lossy().to_string(),
            Some(new_root.clone()),
            Some(Duration::from_millis(100)),
            None,
        )
        .await
        .unwrap();
        assert!(new_root.is_dir());
        drop(created);
    }

    #[tokio::test]
    async fn keyed_lock_serializes_holders_and_rejects_empty_key() {
        assert!(key_lock_paths("").is_err(), "empty lock key must be rejected");
        let (root, path) = key_lock_paths("fs:path:/tmp/example.txt").unwrap();
        assert_eq!(path.parent(), Some(root.as_path()));
        if std::env::var_os("MARE_HOME").is_some_and(|v| !v.is_empty()) {
            assert!(
                !root.starts_with(mare_shared::paths::state_dir()),
                "keyed lock root must be independent of MARE_HOME profile state_dir (root={}, state={})",
                root.display(),
                mare_shared::paths::state_dir().display()
            );
        }
        let g =
            acquire(path.to_string_lossy().into_owned(), Some(root.clone()), Some(Duration::from_millis(200)), None)
                .await
                .unwrap();
        let busy =
            acquire(path.to_string_lossy().into_owned(), Some(root.clone()), Some(Duration::from_millis(60)), None)
                .await;
        assert!(busy.is_err(), "second holder of same key must block and time out");
        g.0.lock().unwrap().take().unwrap().release().unwrap();
        let g2 = acquire(path.to_string_lossy().into_owned(), Some(root), Some(Duration::from_millis(200)), None)
            .await
            .unwrap();
        drop(g2);
    }
}
