//! Resolve and revalidate explicit denied-read paths without silent lexical fallback.

use std::path::{Path, PathBuf};

use super::{Backend, DenyReadPath, SANDBOX_INVALID_ROOT};

pub(super) fn gap_reason(backend: Backend, raw_profile: bool) -> &'static str {
    match backend {
        Backend::Landlock { .. } => "Landlock cannot exclude a child of an allowed read root",
        Backend::Bwrap => {
            "bubblewrap masks supported paths, but path changes, aliases and special mounts are not fully protected"
        }
        Backend::Seatbelt if raw_profile => "an opaque Seatbelt profile cannot be combined with verified denied reads",
        Backend::Seatbelt => "Seatbelt applies denied-read rules, but path changes and aliases have not been verified",
        _ => "per-policy denied-read paths are not enforced by this backend",
    }
}

fn identity(m: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        None
    }
}

pub(super) fn build(raw: &str, cwd: &Path) -> Result<DenyReadPath, String> {
    if raw.is_empty() {
        return Err(format!("{SANDBOX_INVALID_ROOT}: deny_read path is empty"));
    }
    let cwd = if cwd.is_absolute() {
        cwd.to_path_buf()
    } else {
        std::env::current_dir().map_err(|e| format!("{SANDBOX_INVALID_ROOT}: current directory: {e}"))?.join(cwd)
    };
    let input = crate::realpath::expand(raw, &cwd);
    let target = match dunce::canonicalize(&input) {
        Ok(target) => target,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Preserve unresolved entries for explicit partial acceptance. Never treat them as
            // safely maskable, even if the path is created later during this policy's lifetime.
            return Ok(DenyReadPath { target: input.clone(), input, identity: None, is_dir: None });
        }
        Err(e) => return Err(format!("{SANDBOX_INVALID_ROOT}: deny_read {} cannot be resolved: {e}", input.display())),
    };
    if target.parent().is_none() {
        return Err(format!("{SANDBOX_INVALID_ROOT}: deny_read cannot cover a filesystem root"));
    }
    let metadata = std::fs::metadata(&target)
        .map_err(|e| format!("{SANDBOX_INVALID_ROOT}: deny_read {} cannot be inspected: {e}", target.display()))?;
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(format!(
            "{SANDBOX_INVALID_ROOT}: deny_read {} is not a regular file or directory",
            target.display()
        ));
    }
    Ok(DenyReadPath { input, target, identity: identity(&metadata), is_dir: Some(metadata.is_dir()) })
}

pub(super) fn validate(entry: &DenyReadPath) -> Result<(), String> {
    if entry.is_dir.is_none() {
        return Ok(());
    }
    let target = dunce::canonicalize(&entry.input).map_err(|e| {
        format!("{SANDBOX_INVALID_ROOT}: deny_read {} changed or cannot be resolved: {e}", entry.input.display())
    })?;
    let m = std::fs::metadata(&target)
        .map_err(|e| format!("{SANDBOX_INVALID_ROOT}: deny_read {} cannot be inspected: {e}", target.display()))?;
    if target != entry.target
        || identity(&m) != entry.identity
        || Some(m.is_dir()) != entry.is_dir
        || (!m.is_file() && !m.is_dir())
    {
        return Err(format!("{SANDBOX_INVALID_ROOT}: deny_read {} changed identity", entry.input.display()));
    }
    Ok(())
}

/// Resolve an access target, allowing missing trailing components above a resolved ancestor.
/// This is a policy-layer check, not a race-free open.
pub(super) fn access_path(path: &Path) -> Option<PathBuf> {
    crate::realpath::resolve(path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_path_resolves_multi_level_missing_suffix_and_rejects_dangling_link() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        let nested = tmp.path().join("missing-a").join("missing-b");
        assert_eq!(access_path(&nested), Some(root.join("missing-a").join("missing-b")));

        #[cfg(unix)]
        {
            let dangling = tmp.path().join("dangling");
            std::os::unix::fs::symlink(tmp.path().join("nonexistent-target"), &dangling).unwrap();
            assert_eq!(access_path(&dangling), None);
            assert_eq!(access_path(&dangling.join("child")), None);
        }
    }

    #[test]
    fn deny_read_gap_reasons_and_validation_reject_root_and_non_regular_files() {
        assert!(gap_reason(Backend::Seatbelt, true).contains("opaque"));
        assert!(gap_reason(Backend::Seatbelt, false).contains("Seatbelt"));
        assert!(gap_reason(Backend::None, false).contains("not enforced"));

        let tmp = tempfile::tempdir().unwrap();
        // Missing deny_read entry stays unresolved (is_dir == None) and validates without error.
        let missing = build("does_not_exist_yet", tmp.path()).unwrap();
        assert!(missing.is_dir.is_none());
        assert!(validate(&missing).is_ok());

        #[cfg(unix)]
        {
            let root_err = build("/", tmp.path()).unwrap_err();
            assert!(root_err.contains("filesystem root"), "{root_err}");

            let fifo = tmp.path().join("secret.fifo");
            let st = std::process::Command::new("mkfifo").arg(&fifo).status().unwrap();
            assert!(st.success());
            let fifo_err = build(fifo.to_str().unwrap(), tmp.path()).unwrap_err();
            assert!(fifo_err.contains("not a regular file or directory"), "{fifo_err}");
        }
    }
}
