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

/// Grants already verified in this process (the DACL check is cheap, the rewrite is not).
fn grant_once(path: &Path, sid: &str) -> Result<(), String> {
    use std::collections::HashSet;
    use std::sync::Mutex;
    static DONE: Mutex<Option<HashSet<(PathBuf, String)>>> = Mutex::new(None);
    let key = (path.to_path_buf(), sid.to_string());
    if DONE.lock().unwrap().get_or_insert_with(HashSet::new).contains(&key) {
        return Ok(());
    }
    #[cfg(windows)]
    win::grant_write(path, sid)?;
    #[cfg(not(windows))]
    let _ = (path, sid);
    DONE.lock().unwrap().get_or_insert_with(HashSet::new).insert(key);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
