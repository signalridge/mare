// exec-ctx: not a tool entry (Windows sandbox ACL/SID setup helpers; invoked from sandboxed process launch)
//! Capability SIDs of the writable roots and their ACL grants.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::temp::{TempGrant, dunce_canonical, temp_grant};
#[cfg(windows)]
use super::win;

fn sid_parts(domain: &[u8], path: &Path) -> Result<(u32, u32), String> {
    let mut h = Sha256::new();
    h.update(domain);
    // one identity per directory however it's spelled (`C:\\Repo` = `c:\\repo`)
    h.update(
        crate::realpath::canonical_path_key(path).map_err(|e| format!("cannot resolve sandbox path: {e}"))?.as_bytes(),
    );
    let d = h.finalize();
    let word = |i: usize| u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
    // 30-bit subauthorities, never 0
    Ok(((word(0) % ((1 << 30) - 1)) + 1, (word(4) % ((1 << 30) - 1)) + 1))
}

/// The write identity of a workspace (or extra writable) root: deterministic per canonical path.
pub fn workspace_write_sid(root: &Path) -> Result<String, String> {
    let (a, b) = sid_parts(b"", root)?;
    Ok(format!("S-1-4-{a}-{b}"))
}

/// The write identity of one private temp directory (its random path is the identity; the third
/// subauthority keeps it apart from every workspace SID).
pub fn temp_write_sid(dir: &Path) -> Result<String, String> {
    let (a, b) = sid_parts(b"temp\0", dir)?;
    Ok(format!("S-1-4-{a}-{b}-1"))
}

fn random_readonly_sid() -> String {
    let random = uuid::Uuid::new_v4().as_u128();
    let a = ((random as u32) % ((1 << 30) - 1)) + 1;
    let b = (((random >> 32) as u32) % ((1 << 30) - 1)) + 1;
    format!("S-1-4-{a}-{b}-2")
}

fn valid_readonly_sid(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("S-1-4-") else { return false };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 3 && parts[2] == "2" && parts[0].parse::<u32>().is_ok() && parts[1].parse::<u32>().is_ok()
}

const CAP_SID_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const CAP_SID_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// Persistent capability identity for enhanced read-only invocations:
/// reused across processes via `<state>/win_cap_sid` under an exclusive lock so `\\.\NUL` DACL
/// does not grow unboundedly across invocations.
pub fn readonly_capability_sid() -> String {
    static SID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SID.get_or_init(|| {
        let cap_path = mare_shared::paths::state_dir().join("win_cap_sid");
        let lock_path = mare_shared::paths::state_dir().join("win_cap_sid.lock");
        let _guard = mare_shared::private::lock_file_blocking(
            &lock_path,
            Some(CAP_SID_LOCK_TIMEOUT),
            CAP_SID_LOCK_POLL,
            true,
            None,
        )
        .ok();
        if let Ok(existing) = std::fs::read_to_string(&cap_path) {
            let trimmed = existing.trim();
            if valid_readonly_sid(trimmed) {
                return trimmed.to_string();
            }
        }
        let sid = random_readonly_sid();
        let _ = mare_shared::paths::write_atomic(&cap_path, sid.as_bytes(), mare_shared::paths::WriteMode::Private);
        sid
    })
    .clone()
}

/// Whether `sid` is an ephemeral per-session temp capability SID (`S-1-4-x-y-1`) that should be
/// revoked from shared kernel objects like `\\.\NUL` on teardown.
#[cfg(windows)]
pub(crate) fn is_ephemeral_temp_sid(sid: &str) -> bool {
    let Some(rest) = sid.strip_prefix("S-1-4-") else { return false };
    let parts: Vec<&str> = rest.split('-').collect();
    parts.len() == 3 && parts[2] == "1" && parts[0].parse::<u32>().is_ok() && parts[1].parse::<u32>().is_ok()
}

/// Materialize only the explicitly writable roots; do not add a private temporary grant.
pub(crate) fn prepare_root_grants(roots: &[PathBuf]) -> Result<Vec<String>, String> {
    let mut sids = vec![];
    for root in roots {
        let sid = workspace_write_sid(root)?;
        grant_once(root, &sid)?;
        sids.push(sid);
    }
    Ok(sids)
}

/// Materialize workspace roots and the permitted private temp capability.
pub fn prepare_grants(roots: &[PathBuf], temp: &Path) -> Result<Vec<String>, String> {
    let how = temp_grant(roots, &dunce_canonical(&std::env::temp_dir())?, temp)?;
    let mut sids = prepare_root_grants(roots)?;
    match how {
        TempGrant::Isolated => {
            let sid = temp_write_sid(temp)?;
            grant_once(temp, &sid)?;
            sids.push(sid);
        }
        // the root's inheritable ACE already covers the private directory; stating the root's
        // own identity there too keeps it writable even if an ACL between the root and %TEMP%
        // blocks inheritance — still the root's grant, nothing separately revocable
        TempGrant::Shared(root) => grant_once(temp, &workspace_write_sid(&root)?)?,
    }
    Ok(sids)
}

/// Format a targeted diagnostic when a directory under a granted workspace root has a protected
/// DACL (`SE_DACL_PROTECTED`) that blocks inheritance of the sandbox write grant.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn format_protected_dacl_diagnostic(blocked_path: &Path, root: &Path) -> String {
    format!(
        "directory {} has a protected DACL (inheritance disabled) that blocks the Windows sandbox write grant \
         inherited from workspace root {}; mare preserves administrator-set protected DACLs — either grant write \
         access to this directory explicitly or re-enable DACL inheritance on it",
        blocked_path.display(),
        root.display(),
    )
}

/// Outcome of checking whether `cwd` receives effective write access under a single containing root.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootInheritance {
    /// `cwd` inherits or explicitly carries the root's capability ACE.
    Allowed,
    /// A protected DACL (`SE_DACL_PROTECTED`) along the path blocks this root's grant.
    Blocked(String),
}

/// Evaluate all granted writable roots containing `cwd`: succeed if ANY containing root provides
/// effective write access to `cwd`, and only report a protected-DACL block when no valid grant
/// path covers `cwd`. Deepest containing roots are evaluated first so diagnostics name the closest
/// blocked root.
#[cfg_attr(not(windows), allow(dead_code))]
fn verify_cwd_inheritance_with(
    roots: &[PathBuf],
    cwd: &Path,
    mut check_root: impl FnMut(&Path, &Path) -> Result<RootInheritance, String>,
) -> Result<(), String> {
    let mut containing: Vec<&PathBuf> = roots.iter().filter(|root| crate::realpath::path_within(cwd, root)).collect();
    if containing.is_empty() {
        return Ok(());
    }
    containing.sort_by_key(|root| std::cmp::Reverse(root.components().count()));
    let mut first_blocked: Option<String> = None;
    let mut first_err: Option<String> = None;
    for root in containing {
        match check_root(root, cwd) {
            Ok(RootInheritance::Allowed) => return Ok(()),
            Ok(RootInheritance::Blocked(diag)) => {
                if first_blocked.is_none() {
                    first_blocked = Some(diag);
                }
            }
            Err(err) => {
                if first_err.is_none() {
                    first_err = Some(err);
                }
            }
        }
    }
    if let Some(diag) = first_blocked {
        return Err(diag);
    }
    if let Some(err) = first_err {
        return Err(err);
    }
    Ok(())
}

/// Verify that at least one granted writable root containing `cwd` gives `cwd` effective write
/// access (not blocked by a protected DACL).
pub(crate) fn verify_cwd_inheritance(roots: &[PathBuf], cwd: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        verify_cwd_inheritance_with(roots, cwd, |root, target| {
            let sid = workspace_write_sid(root)?;
            win::verify_dir_inheritance(root, target, &sid)
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (roots, cwd);
        Ok(())
    }
}

fn grant_once_with(
    done: &mut std::collections::HashSet<(PathBuf, String)>,
    path: &Path,
    sid: &str,
    mut check_standing: impl FnMut(&Path, &str) -> Result<bool, String>,
    mut apply_grant: impl FnMut(&Path, &str) -> Result<(), String>,
) -> Result<(), String> {
    let key = (path.to_path_buf(), sid.to_string());
    if done.contains(&key) {
        // Re-verify the effective ACL before trusting the process cache so external ACL resets or
        // edits within the same mare process are detected and repaired.
        if check_standing(path, sid)? {
            return Ok(());
        }
        done.remove(&key);
    }
    apply_grant(path, sid)?;
    done.insert(key);
    Ok(())
}

/// Grants already verified in this process: re-verify the cheap standing-grant check before
/// trusting the cache, and rewrite the DACL only when the effective ACL differs.
fn grant_once(path: &Path, sid: &str) -> Result<(), String> {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static DONE: Mutex<Option<HashSet<(PathBuf, String)>>> = Mutex::new(None);
    let mut guard = DONE.lock().unwrap();
    let set = guard.get_or_insert_with(HashSet::new);
    #[cfg(windows)]
    {
        grant_once_with(set, path, sid, win::has_standing_grant, win::grant_write)
    }
    #[cfg(not(windows))]
    {
        grant_once_with(set, path, sid, |_, _| Ok(true), |_, _| Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grant_once_rechecks_standing_acl_and_rewrites_on_drift() {
        let mut cache = std::collections::HashSet::new();
        let dir = Path::new("/workspace/demo");
        let sid = "S-1-4-11-22";

        let standing = std::cell::Cell::new(false);
        let mut checks = 0usize;
        let mut writes = 0usize;

        // First call applies the grant and caches it.
        grant_once_with(
            &mut cache,
            dir,
            sid,
            |_, _| {
                checks += 1;
                Ok(standing.get())
            },
            |_, _| {
                writes += 1;
                standing.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((checks, writes), (0, 1));

        // Second call hits the cache, re-verifies standing == true, and skips rewriting.
        grant_once_with(
            &mut cache,
            dir,
            sid,
            |_, _| {
                checks += 1;
                Ok(standing.get())
            },
            |_, _| {
                writes += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((checks, writes), (1, 1));

        // External ACL drift invalidates the standing grant; next call must rewrite it.
        standing.set(false);
        grant_once_with(
            &mut cache,
            dir,
            sid,
            |_, _| {
                checks += 1;
                Ok(standing.get())
            },
            |_, _| {
                writes += 1;
                standing.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!((checks, writes), (2, 2));
        assert!(cache.contains(&(dir.to_path_buf(), sid.to_string())));

        let msg = format_protected_dacl_diagnostic(Path::new("/workspace/demo/locked"), dir);
        assert!(msg.contains("/workspace/demo/locked"), "{msg}");
        assert!(msg.contains("/workspace/demo"), "{msg}");
        assert!(msg.contains("protected DACL"), "{msg}");
    }

    #[test]
    fn verify_cwd_inheritance_allows_when_any_containing_root_grants_effective_access() {
        let tmp = tempfile::tempdir().unwrap();
        let outer = tmp.path().join("outer");
        let inner = outer.join("protected-inner");
        let leaf = inner.join("leaf");
        let deep_locked = inner.join("deep-locked");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::create_dir_all(&deep_locked).unwrap();

        // Outer root is blocked at `inner` (protected DACL), but `inner` itself is also a granted
        // writable root whose explicit SID covers both `inner` and `inner/leaf`.
        let roots = vec![outer.clone(), inner.clone()];
        let check_nested = |root: &Path, _target: &Path| {
            if root == inner {
                Ok(RootInheritance::Allowed)
            } else {
                Ok(RootInheritance::Blocked(format_protected_dacl_diagnostic(&inner, root)))
            }
        };
        verify_cwd_inheritance_with(&roots, &inner, check_nested).unwrap();
        verify_cwd_inheritance_with(&roots, &leaf, check_nested).unwrap();

        // When only `outer` is granted, `inner` has no effective grant path and must report blocked.
        let err = verify_cwd_inheritance_with(std::slice::from_ref(&outer), &inner, check_nested).unwrap_err();
        assert!(err.contains(&inner.display().to_string()), "{err}");
        assert!(err.contains(&outer.display().to_string()), "{err}");

        // When every containing root is blocked at `deep_locked`, report blocked against the closest root (`inner`).
        let both_blocked = |root: &Path, _target: &Path| {
            Ok(RootInheritance::Blocked(format_protected_dacl_diagnostic(&deep_locked, root)))
        };
        let err = verify_cwd_inheritance_with(&roots, &deep_locked, both_blocked).unwrap_err();
        assert!(err.contains(&deep_locked.display().to_string()), "{err}");
        assert!(err.contains(&inner.display().to_string()), "expected closest root in diagnostic: {err}");
    }

    #[test]
    fn readonly_sid_validation_and_persistence_reuse_valid_sid() {
        assert!(valid_readonly_sid("S-1-4-123-456-2"));
        assert!(!valid_readonly_sid("S-1-5-123-456-2"));
        assert!(!valid_readonly_sid("S-1-4-123-456-1"));
        assert!(!valid_readonly_sid("S-1-4-abc-456-2"));
        assert!(!valid_readonly_sid("S-1-4-123-2"));

        // Pre-seed a valid win_cap_sid in the nextest-isolated state_dir; readonly_capability_sid
        // must reuse it verbatim rather than generating a new SID.
        let cap_path = mare_shared::paths::state_dir().join("win_cap_sid");
        mare_shared::private::create_dir_all(cap_path.parent().unwrap()).unwrap();
        let seeded = "S-1-4-1111-2222-2";
        std::fs::write(&cap_path, format!("{seeded}\n")).unwrap();
        assert_eq!(readonly_capability_sid(), seeded);

        // Shared temp grant path when temp is already covered by a writable root.
        let root = std::env::temp_dir();
        let temp = tempfile::tempdir_in(&root).unwrap();
        let sids = prepare_grants(&[root], temp.path()).unwrap();
        assert_eq!(sids.len(), 1);
    }
}
