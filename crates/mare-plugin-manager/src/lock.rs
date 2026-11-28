//! The lock file (`mare-lock.json`): its entries, atomic reads and writes, cross-process file
//! locks, and the lock shared by parallel workers.

use crate::*;

/// Exclusive cross-process file lock guard (`File::try_lock`, released on drop).
pub(crate) struct ProcessLockGuard(Option<std::fs::File>);

impl Drop for ProcessLockGuard {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            let _ = f.unlock();
        }
    }
}

pub(crate) fn sibling_lock_path(path: &Path) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

pub(crate) fn checkout_lock_path(checkout_dir: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(checkout_dir.to_string_lossy().as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    mare_shared::paths::packages_dir().join(".locks").join(format!("{hex}.lock"))
}

/// Acquire a cross-process exclusive lock on `path`, polling `File::try_lock` so cancellation
/// stops the wait promptly.
pub(crate) fn acquire_file_lock(path: &Path) -> Result<ProcessLockGuard> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        mare_shared::private::create_dir_all(dir)?;
    }
    let f = mare_shared::private::open(
        std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false),
        path,
    )
    .with_context(|| format!("opening lock {}", path.display()))?;
    loop {
        if cancelled() {
            return Err(crate::git::AbortError::Cancelled { what: None }.into());
        }
        match f.try_lock() {
            Ok(()) => return Ok(ProcessLockGuard(Some(f))),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(15)),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }
    }
}

pub(crate) fn lockfile_guard(path: &Path) -> Result<ProcessLockGuard> {
    acquire_file_lock(&sibling_lock_path(path))
}

pub(crate) fn extras_lock_guard(path: &Path) -> Result<ProcessLockGuard> {
    acquire_file_lock(&sibling_lock_path(path))
}

pub(crate) fn checkout_lock(checkout_dir: &Path) -> Result<ProcessLockGuard> {
    acquire_file_lock(&checkout_lock_path(checkout_dir))
}

fn checkout_reference_dir(checkout_dir: &Path) -> PathBuf {
    checkout_lock_path(checkout_dir).with_extension("refs")
}

pub(crate) const CHECKOUT_REFERENCE_VERSION: u32 = 1;

pub(crate) fn is_valid_commit_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn checkout_reference_file(checkout_dir: &Path, lockfile: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(lockfile.to_string_lossy().as_bytes());
    let name: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
    checkout_reference_dir(checkout_dir).join(name)
}

/// Record the lock file using this checkout while the caller holds its checkout lock.
pub(crate) fn register_checkout_reference(checkout_dir: &Path, lockfile: &Path) -> Result<()> {
    let path = checkout_reference_file(checkout_dir, lockfile);
    write_json_atomic(&path, &serde_json::json!({ "version": CHECKOUT_REFERENCE_VERSION, "lockfile": lockfile }))
}

/// Remove a single scope's checkout reference file while holding the checkout lock.
pub(crate) fn unregister_checkout_reference(checkout_dir: &Path, lockfile: &Path) {
    let refs = checkout_reference_dir(checkout_dir);
    let path = checkout_reference_file(checkout_dir, lockfile);
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_dir(refs);
}

/// Remove all checkout reference records when a checkout is deleted.
pub(crate) fn remove_all_checkout_references(checkout_dir: &Path) {
    let refs = checkout_reference_dir(checkout_dir);
    let _ = std::fs::remove_dir_all(refs);
}

/// A checkout referenced by another scope's lock cannot be removed by this scope.
/// When `prune_stale` is true (caller holds `checkout_lock`), confirmed-missing lockfiles or
/// lockfiles that no longer reference `checkout` have their `.refs` entry removed.
pub(crate) fn other_scope_reference(
    checkout: &Path,
    own_lockfile: &Path,
    desired_commit: Option<&str>,
    prune_stale: bool,
) -> Result<Option<PathBuf>> {
    let refs = checkout_reference_dir(checkout);
    let entries = match std::fs::read_dir(&refs) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", refs.display())),
    };
    for entry in entries {
        let path = entry?.path();
        let data = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let record: serde_json::Value = parse_versioned_json_object(&data, &path)?;
        if record["version"].as_u64() != Some(u64::from(CHECKOUT_REFERENCE_VERSION)) {
            bail!("{} has an unsupported checkout reference version", path.display());
        }
        let lockfile = record
            .get("lockfile")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .with_context(|| format!("{} has no lockfile", path.display()))?;
        if lockfile == own_lockfile {
            continue;
        }
        match std::fs::metadata(&lockfile) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => {
                if prune_stale {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if prune_stale {
                    let _ = std::fs::remove_file(&path);
                }
                continue;
            }
            Err(err) => return Err(err).with_context(|| format!("checking {}", lockfile.display())),
        }
        let lock = read_lock_at(&lockfile)?;
        let matching: Vec<_> = lock.packages.values().filter(|entry| checkout_dir(&entry.url) == checkout).collect();
        if matching.is_empty() {
            if prune_stale {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        if matching.iter().any(|entry| desired_commit.is_none_or(|commit| entry.commit != commit)) {
            return Ok(Some(lockfile));
        }
    }
    if prune_stale {
        let _ = std::fs::remove_dir(&refs);
    }
    Ok(None)
}

pub const LOCK_VERSION: u32 = 1;

/// Parse a JSON document enforcing an object root and an explicit integer `version` field
/// (`CorruptPolicy::Refuse` forbids serde seq `[1]`/`[]` or missing-version `{}` coercions).
pub(crate) fn parse_versioned_json_object<T: serde::de::DeserializeOwned>(s: &str, path: &Path) -> Result<T> {
    let val: serde_json::Value = serde_json::from_str(mare_shared::strip_utf8_bom(s))?;
    let obj = val.as_object().ok_or_else(|| anyhow::anyhow!("{} must be a JSON object", path.display()))?;
    if !obj.get("version").is_some_and(|v| v.is_u64()) {
        anyhow::bail!("{} is missing an integer `version` field", path.display());
    }
    Ok(serde_json::from_value(val)?)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LockFile {
    pub version: u32,
    #[serde(default)]
    pub packages: BTreeMap<String, LockEntry>,
}

impl Default for LockFile {
    fn default() -> Self {
        Self { version: LOCK_VERSION, packages: BTreeMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LockEntry {
    pub url: String,
    pub commit: String,
    /// the constraint from init.lua this entry was locked for
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
    /// the branch the package follows (the declared one, else the default branch; lazy.nvim
    /// `lock.lua` records it too)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `sha256:<hex>` of the checkout's files when it was locked (Yazi `hash.rs`), verified by
    /// `restore` and at startup ([`problems`]); an older lock without one is accepted as is
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// The lock file. Missing → empty. Unparseable or unsupported version → an error: every
/// `mare plugin` command stops without touching anything (Yazi `package.rs`; never silently empty
/// like lazy.nvim).
/// Carry the trusted project location with the effective configuration used by package operations.
pub const PROJECT_SCOPE_KEY: &str = "__plugin_project_dir";

pub fn set_project_scope(cfg: &mut serde_json::Value, cwd: &Path, trusted: bool) {
    let Some(root) = cfg.as_object_mut() else { return };
    root.remove(PROJECT_SCOPE_KEY);
    if !trusted {
        return;
    }
    if let Some(dir) = mare_shared::project::resource_root(cwd).mare_dir()
        && (dir.join("init.lua").is_file() || dir.join("plugins.json").is_file())
    {
        root.insert(PROJECT_SCOPE_KEY.into(), dir.to_string_lossy().into_owned().into());
    }
}

pub fn lock_path(cfg: &serde_json::Value) -> PathBuf {
    cfg.get(PROJECT_SCOPE_KEY)
        .and_then(serde_json::Value::as_str)
        .map(|dir| PathBuf::from(dir).join("mare-lock.json"))
        .unwrap_or_else(mare_shared::paths::lock_file)
}

pub fn read_lock(cfg: &serde_json::Value) -> Result<LockFile> {
    read_lock_at(&lock_path(cfg))
}

pub(crate) fn spec_lock_path(cfg: &serde_json::Value, spec: &PackageSpec) -> PathBuf {
    if spec.project_lock { lock_path(cfg) } else { mare_shared::paths::lock_file() }
}

pub(crate) fn read_spec_lock(cfg: &serde_json::Value, spec: &PackageSpec) -> Result<LockFile> {
    read_lock_at(&spec_lock_path(cfg, spec))
}

pub(crate) fn read_lock_at(p: &Path) -> Result<LockFile> {
    match std::fs::read_to_string(p) {
        Ok(s) => {
            let lock: LockFile = parse_versioned_json_object(&s, p).map_err(|e| {
                anyhow::anyhow!(
                    "lock file {} is corrupt ({e}); nothing was changed — fix it, or delete it and run `mare plugin update` to rebuild",
                    p.display()
                )
            })?;
            if lock.version != LOCK_VERSION {
                anyhow::bail!(
                    "lock file {} has unsupported version {} (expected {LOCK_VERSION}); nothing was changed — fix it, or delete it and run `mare plugin update` to rebuild",
                    p.display(),
                    lock.version
                );
            }
            for (name, entry) in &lock.packages {
                if !is_valid_commit_sha(&entry.commit) {
                    anyhow::bail!(
                        "lock file {} has invalid commit for {name} (expected 40 or 64 hex chars); nothing was changed — fix it, or delete it and run `mare plugin update` to rebuild",
                        p.display()
                    );
                }
                if source::has_url_credentials(&entry.url) {
                    anyhow::bail!(
                        "lock file {} contains URL credentials for {name}; remove credentials from the lock file and run `mare plugin update`",
                        p.display()
                    );
                }
            }
            Ok(lock)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LockFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

/// Atomic replace via [`mare_shared::paths::write_atomic`].
pub(crate) fn write_json_atomic<T: Serialize>(p: &Path, v: &T) -> Result<()> {
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        mare_shared::private::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(v)? + "\n";
    mare_shared::paths::write_atomic(p, json.as_bytes(), mare_shared::paths::WriteMode::Private)?;
    Ok(())
}

#[cfg(test)]
pub(crate) type BeforeLockCommitHook = Box<dyn FnOnce() -> Result<()> + Send>;

#[cfg(test)]
/// Test seam: runs inside `SharedLock::commit` right before writing `mare-lock.json`.
pub(crate) static BEFORE_LOCK_COMMIT: std::sync::Mutex<Option<BeforeLockCommitHook>> = std::sync::Mutex::new(None);

pub(crate) fn write_lock(path: &Path, l: &LockFile) -> Result<()> {
    write_json_atomic(path, l)
}

/// The lock shared by parallel workers: each finished package is merged into the on-disk lock
/// under a cross-process file lock as soon as it is done.
///
/// Lock order: always acquire `self.0` (in-process mutex) BEFORE [`lockfile_guard`] (cross-process
/// file lock) so concurrent `refresh_get` and `commit` calls cannot deadlock.
pub(crate) struct SharedLock {
    data: Mutex<LockFile>,
    path: PathBuf,
}

impl SharedLock {
    pub(crate) fn new(cfg: &serde_json::Value) -> Result<Self> {
        let path = lock_path(cfg);
        Ok(Self { data: Mutex::new(read_lock_at(&path)?), path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn refresh_get(&self, name: &str) -> Result<Option<LockEntry>> {
        let mut l = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = lockfile_guard(&self.path)?;
        let disk = read_lock_at(&self.path)?;
        let entry = disk.packages.get(name).cloned();
        *l = disk;
        Ok(entry)
    }

    pub(crate) fn commit(&self, name: &str, e: LockEntry) -> Result<()> {
        let mut l = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = lockfile_guard(&self.path)?;
        #[cfg(test)]
        if let Some(hook) = BEFORE_LOCK_COMMIT.lock().unwrap_or_else(|e| e.into_inner()).take() {
            hook()?;
        }
        let mut disk = read_lock_at(&self.path)?;
        if disk.packages.get(name) == Some(&e) {
            *l = disk;
            return Ok(());
        }
        disk.packages.insert(name.to_string(), e);
        write_lock(&self.path, &disk).context("writing the lock file")?;
        *l = disk;
        Ok(())
    }

    pub(crate) fn prune_undeclared(&self, keep: &BTreeSet<String>) -> Result<Vec<PathBuf>> {
        let mut l = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = lockfile_guard(&self.path)?;
        let mut disk = read_lock_at(&self.path)?;
        let before = disk.packages.len();
        let mut removed_checkouts = Vec::new();
        let mut retained = BTreeMap::new();
        for (k, v) in disk.packages {
            if keep.contains(&k) {
                retained.insert(k, v);
            } else {
                removed_checkouts.push(checkout_dir(&v.url));
            }
        }
        disk.packages = retained;
        let active_checkouts: BTreeSet<PathBuf> = disk.packages.values().map(|e| checkout_dir(&e.url)).collect();
        removed_checkouts.retain(|d| !active_checkouts.contains(d));
        removed_checkouts.sort();
        removed_checkouts.dedup();
        if disk.packages.len() != before || !self.path.exists() {
            write_lock(&self.path, &disk)?;
        }
        *l = disk;
        Ok(removed_checkouts)
    }

    pub(crate) fn ensure_exists(&self) -> Result<()> {
        let mut l = self.data.lock().unwrap_or_else(|e| e.into_inner());
        let _guard = lockfile_guard(&self.path)?;
        if !self.path.exists() {
            let disk = read_lock_at(&self.path)?;
            write_lock(&self.path, &disk)?;
            *l = disk;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn lock_creates_private_parent_and_file() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("packages/.locks");
        let path = dir.join("checkout.lock");
        let _guard = acquire_file_lock(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(dir.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn read_lock_rejects_missing_or_unsupported_version() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mare-lock.json");

        // Missing file -> default v1
        assert_eq!(read_lock_at(&p).unwrap().version, LOCK_VERSION);

        // Missing version field or non-object payload -> rejected
        for bad in ["[1]", "[]", "{}", r#"{"packages":{}}"#] {
            std::fs::write(&p, bad).unwrap();
            assert!(read_lock_at(&p).is_err(), "expected {bad} to fail");
        }

        // Unsupported version -> rejected
        std::fs::write(&p, r#"{"version":99,"packages":{}}"#).unwrap();
        let err = read_lock_at(&p).unwrap_err();
        assert!(err.to_string().contains("unsupported version 99"), "{err}");

        // Valid v1 -> accepted
        write_json_atomic(&p, &LockFile::default()).unwrap();
        assert_eq!(read_lock_at(&p).unwrap().version, LOCK_VERSION);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
