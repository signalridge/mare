//! The content hash of a checkout (Yazi `hash.rs`): taking it, locking it, verifying it.

use super::checkout::{head, short, worktree_status};
use crate::*;

/// Every file under `dir` (symlinks not followed, `.git` skipped) as (`/`-separated relative
/// path, full path), sorted by the relative path.
pub(crate) fn walk_files(dir: &Path) -> Vec<(String, PathBuf)> {
    fn go(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if e.file_name() == ".git" {
                continue;
            }
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                go(base, &p, out);
            } else {
                let rel = p.strip_prefix(base).unwrap_or(&p);
                let rel: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                out.push((rel.join("/"), p));
            }
        }
    }
    let mut out = vec![];
    go(dir, dir, &mut out);
    out.sort();
    out
}

/// `sha256:<hex>` over a checkout's files (paths and contents; a symlink by its target; `.git`
/// left out): what the lock records next to the commit.
pub fn content_hash(dir: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for (rel, p) in walk_files(dir) {
        let meta = std::fs::symlink_metadata(&p)?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&p)?;
            h.update(b"L\0");
            h.update(rel.as_bytes());
            h.update(b"\0");
            h.update(target.to_string_lossy().replace('\\', "/").as_bytes());
        } else {
            let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
            h.update(b"F\0");
            h.update(rel.as_bytes());
            h.update(b"\0");
            h.update((bytes.len() as u64).to_le_bytes());
            h.update(&bytes);
        }
        h.update(b"\0");
    }
    Ok(format!("sha256:{}", h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()))
}

/// Nothing in the checkout differs from its HEAD per git: no edits, no untracked or ignored
/// files (a hash taken then is the commit's content).
fn pristine(o: &PkgOptions, dir: &Path) -> bool {
    worktree_status(o, dir).is_clean()
}

/// The hash to lock for a checkout at `commit`: taken when the checkout is pristine (and then it
/// must match the previous entry's hash for the same commit — never silently re-locked);
/// otherwise the previous entry's hash for the same commit is kept (none for a new commit).
pub(crate) fn lock_hash(o: &PkgOptions, dir: &Path, commit: &str, prev: Option<&LockEntry>) -> Result<Option<String>> {
    let prev = prev.filter(|e| e.commit == commit).and_then(|e| e.hash.clone());
    if !pristine(o, dir) {
        return Ok(prev);
    }
    let have = content_hash(dir)?;
    if let Some(want) = prev
        && want != have
    {
        bail!(
            "content hash mismatch at the locked commit {} (lock {want}, checkout {have}): its files differ from what was locked — delete {} and run `mare plugin sync` to reinstall",
            short(commit),
            dir.display()
        );
    }
    Ok(Some(have))
}

pub(crate) enum HashCheck {
    /// matches, or there's no hash to compare (an older lock)
    Ok,
    /// not comparable: local edits / untracked files / another commit checked out
    Skipped(String),
    Mismatch(String),
}

pub(crate) fn verify_hash(o: &PkgOptions, dir: &Path, e: &LockEntry) -> HashCheck {
    let Some(want) = &e.hash else { return HashCheck::Ok };
    let have = match content_hash(dir) {
        Ok(h) => h,
        Err(err) => return HashCheck::Mismatch(format!("content hash not computed: {err:#}")),
    };
    if &have == want {
        return HashCheck::Ok;
    }
    if head(o, dir).as_deref() != Some(e.commit.as_str()) {
        return HashCheck::Skipped("not at the locked commit".into());
    }
    if !pristine(o, dir) {
        return HashCheck::Skipped("local changes or untracked files".into());
    }
    HashCheck::Mismatch(format!(
        "content hash mismatch at the locked commit {} (lock {want}, checkout {have}): its files differ from what was locked — delete {} and run `mare plugin sync` to reinstall",
        short(&e.commit),
        dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_includes_symlinks_and_verify_hash_skips_mismatched_head() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", dir.path().join("link.txt")).unwrap();
        let h1 = content_hash(dir.path()).unwrap();
        assert!(h1.starts_with("sha256:"));

        #[cfg(unix)]
        {
            std::fs::remove_file(dir.path().join("link.txt")).unwrap();
            std::os::unix::fs::symlink("other.txt", dir.path().join("link.txt")).unwrap();
            let h2 = content_hash(dir.path()).unwrap();
            assert_ne!(h1, h2, "changing symlink target must change content hash");
        }

        // verify_hash returns Skipped when directory HEAD does not match locked commit.
        let o = PkgOptions::from_config(&serde_json::json!({}));
        let entry = LockEntry {
            url: "https://example.com/a/b.git".into(),
            commit: "0000000000000000000000000000000000000000".into(),
            rev: None,
            branch: None,
            hash: Some("sha256:deadbeef".into()),
        };
        assert!(matches!(verify_hash(&o, dir.path(), &entry), HashCheck::Skipped(_)));

        // Unreadable file inside checkout triggers content_hash Err -> HashCheck::Mismatch.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let secret = dir.path().join("unreadable.txt");
            std::fs::write(&secret, "x").unwrap();
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
            let res = verify_hash(&o, dir.path(), &entry);
            let _ = std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600));
            if unsafe { libc::geteuid() } != 0 {
                assert!(matches!(res, HashCheck::Mismatch(_)));
            }
        }
    }
}
