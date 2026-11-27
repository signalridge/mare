//! Well-known directories. XDG-style on every platform:
//! config `~/.config/mare`, state `~/.local/state/mare`, data `~/.local/share/mare`.

use std::path::{Component, Path, PathBuf};

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn env_dir(var: &str, fallback: &[&str]) -> PathBuf {
    if let Ok(v) = std::env::var(var)
        && !v.is_empty()
    {
        return PathBuf::from(v).join("mare");
    }
    let mut p = home();
    for seg in fallback {
        p.push(seg);
    }
    p.push("mare");
    p
}

/// `MARE_HOME` overrides everything (used by tests): config/state/data become subdirs.
/// Relative values are anchored to the current working directory while preserving symlink
/// components so sandbox ancestor checks can still detect them.
fn mare_home() -> Option<PathBuf> {
    let raw = std::env::var("MARE_HOME").ok().filter(|s| !s.is_empty())?;
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        Some(p)
    } else if let Ok(cwd) = std::env::current_dir() {
        Some(cwd.join(p))
    } else {
        Some(p)
    }
}

pub fn config_dir() -> PathBuf {
    mare_home().map(|h| h.join("config")).unwrap_or_else(|| env_dir("XDG_CONFIG_HOME", &[".config"]))
}

pub fn state_dir() -> PathBuf {
    mare_home().map(|h| h.join("state")).unwrap_or_else(|| env_dir("XDG_STATE_HOME", &[".local", "state"]))
}

/// The one state file (runtime choices persisted by plugins and the host). Every reader and
/// writer goes through this, so the path is defined in exactly one place.
pub fn state_file() -> PathBuf {
    state_dir().join("state.json")
}

pub fn data_dir() -> PathBuf {
    mare_home().map(|h| h.join("data")).unwrap_or_else(|| env_dir("XDG_DATA_HOME", &[".local", "share"]))
}

pub fn cache_dir() -> PathBuf {
    mare_home().map(|h| h.join("cache")).unwrap_or_else(|| env_dir("XDG_CACHE_HOME", &[".cache"]))
}

pub fn auth_file() -> PathBuf {
    config_dir().join("auth.json")
}

pub fn lock_file() -> PathBuf {
    config_dir().join("mare-lock.json")
}

pub fn packages_dir() -> PathBuf {
    data_dir().join("packages")
}

pub fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

pub fn trust_file() -> PathBuf {
    state_dir().join("trust.json")
}

pub fn log_dir() -> PathBuf {
    state_dir().join("log")
}

pub fn crash_dir() -> PathBuf {
    state_dir().join("crashes")
}

/// Host runtime authentication token file (`0600` in `0700` `<state>/host/`).
pub fn host_token_file() -> PathBuf {
    state_dir().join("host").join("token")
}

/// Running host webserver listener metadata directory (`0700` `<state>/web/`, outside `<state>/host/`).
pub fn web_listeners_dir() -> PathBuf {
    state_dir().join("web")
}

pub fn user_plugins_dir() -> PathBuf {
    config_dir().join("plugins")
}

/// Expand a leading `~/` (and `~\` on Windows).
pub fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| if cfg!(windows) { p.strip_prefix("~\\") } else { None }) {
        return home().join(rest);
    }
    if p == "~" {
        return home();
    }
    PathBuf::from(p)
}

/// A normalized input spelling and the physical target it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedPath {
    pub logical: PathBuf,
    pub physical: PathBuf,
    /// Deepest verified existing path; used to bind a new target to its parent identity.
    pub existing_ancestor: PathBuf,
}

/// Reject Windows NTFS Alternate Data Streams (`:`) and trailing ASCII dots/spaces in a
/// non-prefix path component.
pub fn validate_windows_component(name: &str) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind};
    if name.contains(':') {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "Windows path component cannot contain ':' (alternate data stream)",
        ));
    }
    if name != "." && name != ".." && (name.ends_with('.') || name.ends_with(' ')) {
        return Err(Error::new(ErrorKind::InvalidInput, "Windows path component cannot end with '.' or ' '"));
    }
    Ok(())
}

/// Strip `\\?\` and `\\?\UNC\` verbatim prefixes consistently for policy/containment checks.
pub fn strip_windows_verbatim_str(s: &str) -> String {
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix(r"UNC\").or_else(|| rest.strip_prefix(r"unc\")) {
            return format!(r"\\{unc}");
        }
        return rest.to_string();
    }
    if let Some(rest) = s.strip_prefix("//?/") {
        if let Some(unc) = rest.strip_prefix("UNC/").or_else(|| rest.strip_prefix("unc/")) {
            return format!("//{unc}");
        }
        return rest.to_string();
    }
    s.to_string()
}

fn normalize_resolved_output(p: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let simplified = dunce::simplified(&p);
        PathBuf::from(strip_windows_verbatim_str(&simplified.to_string_lossy()))
    } else {
        p
    }
}

/// Resolve a path without treating inaccessible or ambiguous ancestors as absent.
pub fn resolve_path(p: &Path) -> std::io::Result<ResolvedPath> {
    resolve_path_checked(p, |_| Ok(()))
}

/// The same resolver with a policy check before inspecting each component.
pub fn resolve_path_checked(
    p: &Path,
    mut check: impl FnMut(&Path) -> std::io::Result<()>,
) -> std::io::Result<ResolvedPath> {
    use std::collections::VecDeque;
    use std::ffi::OsString;
    use std::io::{Error, ErrorKind};

    enum Part {
        Name(OsString),
        LinkEnd,
    }
    let abs = if p.is_absolute() { p.to_path_buf() } else { std::env::current_dir()?.join(p) };
    let mut logical = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                logical.pop();
            }
            Component::Normal(s) => {
                #[cfg(windows)]
                validate_windows_component(&s.to_string_lossy())?;
                logical.push(s);
            }
            other => logical.push(other.as_os_str()),
        }
    }
    let logical = normalize_resolved_output(logical);
    let mut pending = VecDeque::new();
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
            other => pending.push_back(Part::Name(other.as_os_str().to_os_string())),
        }
    }
    let mut hops = 0;
    let mut missing = false;
    let mut existing_ancestor = None;
    let mut link_depth = 0usize;
    let mut directory = true;
    while let Some(part) = pending.pop_front() {
        let name = match part {
            Part::Name(name) => name,
            Part::LinkEnd => {
                link_depth -= 1;
                continue;
            }
        };
        if name == "." {
            continue;
        }
        if name == ".." {
            if missing || !directory {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "cannot resolve parent after a missing or non-directory component",
                ));
            }
            out.pop();
            continue;
        }
        #[cfg(windows)]
        validate_windows_component(&name.to_string_lossy())?;
        if !missing && !directory {
            return Err(Error::new(ErrorKind::NotADirectory, "path ancestor is not a directory"));
        }
        let cand = out.join(&name);
        if missing {
            out = cand;
            continue;
        }
        check(&cand)?;
        match std::fs::symlink_metadata(&cand) {
            Ok(m) if m.file_type().is_symlink() => {
                hops += 1;
                if hops > 40 {
                    return Err(Error::new(ErrorKind::InvalidInput, "too many symbolic links"));
                }
                let target = std::fs::read_link(&cand)?;
                let mut front = Vec::new();
                if target.is_absolute() {
                    out = PathBuf::new();
                    for c in target.components() {
                        match c {
                            Component::Prefix(_) | Component::RootDir => out.push(c.as_os_str()),
                            other => front.push(other.as_os_str().to_os_string()),
                        }
                    }
                } else {
                    front.extend(target.components().map(|c| c.as_os_str().to_os_string()));
                }
                link_depth += 1;
                pending.push_front(Part::LinkEnd);
                for part in front.into_iter().rev() {
                    pending.push_front(Part::Name(part));
                }
                directory = true;
            }
            Ok(m) => {
                directory = m.is_dir();
                out = cand;
            }
            Err(e) if e.kind() == ErrorKind::NotFound && link_depth == 0 => {
                out = normalize_resolved_output(dunce::canonicalize(&out)?);
                existing_ancestor = Some(out.clone());
                missing = true;
                out.push(name);
            }
            Err(e) => return Err(e),
        }
    }
    check(&out)?;
    let physical = normalize_resolved_output(if missing { out } else { dunce::canonicalize(&out)? });
    let existing_ancestor = normalize_resolved_output(existing_ancestor.unwrap_or_else(|| physical.clone()));
    Ok(ResolvedPath { logical, physical, existing_ancestor })
}

fn is_wsl_mount_path(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    let Some(rest) = raw.strip_prefix("/mnt/") else { return false };
    let mut bytes = rest.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic()) && matches!(bytes.next(), None | Some(b'/'))
}

/// Normalize a workspace root path for identity and trust-key comparisons: resolves symlinks and
/// `.` / `..` even across missing suffixes, strips `\\?\`, and folds ASCII case on Windows and
/// Linux WSL `/mnt/<drive>` mounts.
pub fn normalize_workspace_path(p: &Path) -> std::io::Result<PathBuf> {
    let resolved = resolve_path(p)?.physical;
    if cfg!(windows) || (cfg!(target_os = "linux") && is_wsl_mount_path(&resolved)) {
        Ok(PathBuf::from(resolved.to_string_lossy().to_ascii_lowercase()))
    } else {
        Ok(resolved)
    }
}

/// `rename(from, to)` that tolerates Windows' "file in use" errors: an open file
/// (an editor, an indexer, antivirus) blocks replacing it for a moment, so the temp-file +
/// rename writes retry for up to ~1 s. Unix renames are atomic and never retried.
/// Durable file replacement: `tmp` must already be written and
/// fsynced; rename it over `to` (retrying Windows sharing violations), then fsync the directory
/// so the rename itself survives a crash (Unix; Windows has no directory fsync).
pub fn replace_durable(tmp: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    rename_retry(tmp, to)?;
    #[cfg(unix)]
    if let Some(dir) = to.parent() {
        let dir = if dir.as_os_str().is_empty() { std::path::Path::new(".") } else { dir };
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

pub fn rename_retry(from: &std::path::Path, to: &std::path::Path) -> std::io::Result<()> {
    let mut delay = 10u64;
    let mut waited = 0u64;
    loop {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            // ERROR_ACCESS_DENIED (5), ERROR_SHARING_VIOLATION (32), ERROR_LOCK_VIOLATION (33)
            Err(e) if cfg!(windows) && matches!(e.raw_os_error(), Some(5 | 32 | 33)) && waited < 1000 => {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                waited += delay;
                delay = (delay * 2).min(200);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Name for a temp file next to `path`: `.<name>.<random>.tmp`. A leading dot keeps it out of
/// listings that skip hidden files; the `.tmp` suffix is what crash cleanups look for.
pub fn temp_sibling(path: &std::path::Path) -> std::path::PathBuf {
    use std::hash::{BuildHasher, Hasher};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    // RandomState is seeded from the OS RNG per process (and perturbed per instance)
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.write_u64(SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
    dir.join(format!(".{name}.{:016x}.tmp", h.finish()))
}

/// File permissions for atomic writes.
/// No implicit platform default: callers must declare whether the file is private or shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteMode {
    /// Owner-only (`0o600` on Unix; private DACL on Windows).
    Private,
    /// Shared (`0o644` on Unix; default ACL on Windows), e.g. project-checked-in files.
    Shared,
}

impl WriteMode {
    #[cfg(unix)]
    pub const fn unix_mode(self) -> u32 {
        match self {
            Self::Private => 0o600,
            Self::Shared => 0o644,
        }
    }
}

/// Replace `path` with `bytes` atomically and durably: a random-named temp file in
/// the same directory, created exclusively (`create_new`) with its final `mode` from the start
/// (Unix; no window where a secret sits at umask permissions), written, fsynced, renamed over
/// `path` (retrying Windows sharing violations), then the directory fsynced. Readers see the old
/// or the new contents, never a mix; a failed write leaves no temp file behind. On Windows
/// [`WriteMode::Private`] creates the temp file with the private DACL of [`crate::private`]. The
/// directory must already exist.
pub fn write_atomic(path: &std::path::Path, bytes: &[u8], mode: WriteMode) -> std::io::Result<()> {
    write_atomic_checked(path, bytes, mode, || Ok(()))
}

/// The same atomic writer with a final, non-yielding check after temp fsync and before
/// publication. A rejected check leaves the old destination intact; after this point a
/// publication error may have changed the destination (e.g. parent-directory fsync failed).
pub fn write_atomic_checked(
    path: &std::path::Path,
    bytes: &[u8],
    mode: WriteMode,
    before_publish: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    write_atomic_impl(path, bytes, mode, true, before_publish)
}

/// Atomically publish a rebuildable cache entry without syncing the file or directory.
/// The same private creation and cleanup rules as [`write_atomic`] still apply.
pub fn write_atomic_cache(path: &std::path::Path, bytes: &[u8], mode: WriteMode) -> std::io::Result<()> {
    write_atomic_impl(path, bytes, mode, false, || Ok(()))
}

fn write_atomic_impl(
    path: &std::path::Path,
    bytes: &[u8],
    mode: WriteMode,
    durable: bool,
    before_publish: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut attempt = 0;
    let (tmp, mut f) = loop {
        let tmp = temp_sibling(path);
        let mut o = std::fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(mode.unix_mode());
        }
        // Windows has no mode bits: Private means a private DACL, set when the file is created
        #[cfg(windows)]
        let opened = if matches!(mode, WriteMode::Private) { crate::private::create_new(&tmp) } else { o.open(&tmp) };
        #[cfg(not(windows))]
        let opened = o.open(&tmp);
        match opened {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 16 => attempt += 1,
            Err(e) => return Err(e),
        }
    };
    #[cfg(not(unix))]
    let _ = mode;
    let res = (|| {
        // the umask can only have narrowed the creation mode; make it exact
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode.unix_mode()))?;
        }
        f.write_all(bytes)?;
        if durable {
            f.sync_all()?;
        }
        drop(f);
        before_publish()?;
        if durable { replace_durable(&tmp, path) } else { rename_retry(&tmp, path) }
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temps(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.json");
        write_atomic(&p, b"one", WriteMode::Shared).unwrap();
        write_atomic(&p, b"two", WriteMode::Shared).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o644);
        }
        assert!(temps(d.path()).is_empty());
    }

    /// The mode is set when the temp file is created (`OpenOptionsExt::mode`), so the new file
    /// never exists with wider permissions; it does not inherit the old file's 0644 either.
    /// Windows: the same for the private DACL (`crate::private`), below a directory open to
    /// everyone (the old file inherits that).
    #[test]
    fn write_atomic_mode_is_private_from_creation() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("secret");
        #[cfg(windows)]
        crate::private::testing::open_to_everyone(d.path());
        std::fs::write(&p, b"old").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        #[cfg(windows)]
        assert!(crate::private::check(&p).is_err(), "the old file is readable by everyone");
        write_atomic(&p, b"token", WriteMode::Private).unwrap();
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        #[cfg(windows)]
        assert_eq!(crate::private::check(&p), Ok(()));
        // a watcher that catches temp files mid-write never sees one readable by others
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = {
            let (dir, stop) = (d.path().to_path_buf(), stop.clone());
            std::thread::spawn(move || {
                let mut seen = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    for e in std::fs::read_dir(&dir).unwrap().flatten() {
                        if !e.file_name().to_string_lossy().ends_with(".tmp") {
                            continue;
                        }
                        #[cfg(unix)]
                        if let Ok(m) = e.metadata() {
                            assert_eq!(m.permissions().mode() & 0o077, 0, "temp file open to others");
                            seen += 1;
                        }
                        // (a temp file renamed away meanwhile can't be read: skipped)
                        #[cfg(windows)]
                        match crate::private::check(&e.path()) {
                            Ok(()) => seen += 1,
                            Err(err) => assert!(!err.contains("grants"), "temp file open to others: {err}"),
                        }
                    }
                }
                seen
            })
        };
        let big = vec![b'x'; 256 * 1024];
        for _ in 0..40 {
            write_atomic(&p, &big, WriteMode::Private).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = watcher.join().unwrap();
        #[cfg(unix)]
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        #[cfg(windows)]
        assert_eq!(crate::private::check(&p), Ok(()));
    }

    #[test]
    fn write_atomic_failure_leaves_no_temp() {
        let d = tempfile::tempdir().unwrap();
        // the target is a non-empty directory: the rename fails after the temp was written
        let p = d.path().join("dir");
        std::fs::create_dir_all(p.join("inner")).unwrap();
        assert!(write_atomic(&p, b"x", WriteMode::Private).is_err());
        assert!(temps(d.path()).is_empty(), "{:?}", temps(d.path()));
        // a missing directory fails before anything is created
        assert!(write_atomic(&d.path().join("nope/f"), b"x", WriteMode::Shared).is_err());
    }

    type AtomicWriter = fn(&Path, &[u8], WriteMode) -> std::io::Result<()>;

    /// Concurrent writers of distinct, large payloads: every read sees one whole payload.
    fn assert_concurrent_writers_never_tear(write: AtomicWriter, mode: WriteMode) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("shared");
        write(&p, &vec![b'a'; 64 * 1024], mode).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(5));
        let reader = {
            let (p, stop, barrier) = (p.clone(), stop.clone(), barrier.clone());
            std::thread::spawn(move || {
                let mut reads = 0;
                barrier.wait();
                loop {
                    // Windows: a read can race a rename; that's a retry, not a torn file.
                    if let Ok(data) = std::fs::read(&p) {
                        assert_eq!(data.len(), 64 * 1024);
                        assert!(data.iter().all(|&byte| byte == data[0]), "torn publication");
                        reads += 1;
                    }
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                }
                reads
            })
        };
        let writers: Vec<_> = (0..4u8)
            .map(|i| {
                let (p, barrier) = (p.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    for _ in 0..10 {
                        write(&p, &vec![b'b' + i; 64 * 1024], mode).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
        assert!(temps(d.path()).is_empty());
    }

    #[test]
    fn write_atomic_concurrent_writers_never_tear() {
        assert_concurrent_writers_never_tear(write_atomic, WriteMode::Shared);
    }

    #[test]
    fn cache_write_uses_private_sibling_and_cleans_up_on_failure() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("cache");
        #[cfg(windows)]
        crate::private::testing::open_to_everyone(d.path());
        write_atomic_impl(&p, b"one", WriteMode::Private, false, || {
            let names = temps(d.path());
            assert_eq!(names.len(), 1);
            assert!(names[0].starts_with(".cache."));
            let tmp = d.path().join(&names[0]);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777, 0o600);
            }
            #[cfg(windows)]
            assert_eq!(crate::private::check(&tmp), Ok(()));
            Ok(())
        })
        .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"one");
        write_atomic_cache(&p, b"two", WriteMode::Private).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        #[cfg(windows)]
        assert_eq!(crate::private::check(&p), Ok(()));
        let occupied = d.path().join("directory");
        std::fs::create_dir_all(occupied.join("inner")).unwrap();
        assert!(write_atomic_cache(&occupied, b"x", WriteMode::Private).is_err());
        assert!(occupied.join("inner").is_dir());
        assert!(temps(d.path()).is_empty());
    }

    #[test]
    fn concurrent_cache_publications_are_never_torn() {
        assert_concurrent_writers_never_tear(write_atomic_cache, WriteMode::Private);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writes_replace_symlinks_without_modifying_the_target() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("target");
        let link = d.path().join("link");
        std::fs::write(&target, b"original").unwrap();

        for write in [write_atomic as AtomicWriter, write_atomic_cache] {
            std::os::unix::fs::symlink(&target, &link).unwrap();
            write(&link, b"replacement", WriteMode::Private).unwrap();
            assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
            assert_eq!(std::fs::read(&link).unwrap(), b"replacement");
            assert_eq!(std::fs::read(&target).unwrap(), b"original");
            std::fs::remove_file(&link).unwrap();
        }
    }

    #[test]
    fn normalize_workspace_path_handles_missing_ancestors_symlinks_and_wsl_case() {
        let d = tempfile::tempdir().unwrap();
        let real = d.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir(real.join("sub")).unwrap();

        #[cfg(unix)]
        {
            let link = d.path().join("link");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let missing_via_real = normalize_workspace_path(&real.join("sub/../missing/leaf")).unwrap();
            let missing_via_link = normalize_workspace_path(&link.join("missing/leaf")).unwrap();
            assert_eq!(missing_via_real, missing_via_link);

            // Standard POSIX paths preserve ASCII case.
            let upper = normalize_workspace_path(&d.path().join("CaseDir")).unwrap();
            let lower = normalize_workspace_path(&d.path().join("casedir")).unwrap();
            assert_ne!(upper, lower);
        }

        #[cfg(target_os = "linux")]
        {
            // Linux WSL /mnt/<drive> paths normalize ASCII case when pointing to Windows mounts,
            // while non-drive /mnt/<dir> names remain case-sensitive.
            assert!(normalize_workspace_path(Path::new("/mnt/Z/Nonexistent-Ws/Sub/../Proj")).is_err());
            assert_ne!(
                normalize_workspace_path(Path::new("/mnt/cc/UpperDir")).unwrap(),
                PathBuf::from("/mnt/cc/upperdir")
            );
        }
    }

    #[test]
    fn windows_component_validation_and_verbatim_stripping() {
        assert!(validate_windows_component("normal.txt").is_ok());
        assert!(validate_windows_component(".git").is_ok());
        assert!(validate_windows_component("foo:stream").is_err());
        assert!(validate_windows_component(".git::$DATA").is_err());
        assert!(validate_windows_component("foo.").is_err());
        assert!(validate_windows_component("foo ").is_err());
        assert!(validate_windows_component(".git.").is_err());
        assert_eq!(strip_windows_verbatim_str(r"\\?\C:\ws\.git"), r"C:\ws\.git");
        assert_eq!(strip_windows_verbatim_str(r"\\?\UNC\srv\share\.git"), r"\\srv\share\.git");
        assert_eq!(strip_windows_verbatim_str(r"\\?\unc\srv\share\.git"), r"\\srv\share\.git");
    }
}
