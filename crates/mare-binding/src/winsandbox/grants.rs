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

/// Volume ACL capability metadata queried via `GetVolumeInformationW`.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VolumeAclInfo {
    pub fs_name: String,
    pub persistent_acls: bool,
}

#[cfg_attr(not(windows), allow(dead_code))]
fn normalized_win_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_ascii_lowercase()
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_wsl_path(path: &Path) -> bool {
    let p = normalized_win_path(path);
    p.starts_with(r"\\wsl.localhost\")
        || p.starts_with(r"\\wsl$\")
        || p.starts_with(r"\\?\unc\wsl.localhost\")
        || p.starts_with(r"\\?\unc\wsl$\")
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn is_unc_path(path: &Path) -> bool {
    let p = normalized_win_path(path);
    (p.starts_with(r"\\") && !p.starts_with(r"\\?\") && !p.starts_with(r"\\.\")) || p.starts_with(r"\\?\unc\")
}

/// Win32 error codes that explicitly mean the target volume/filesystem does not support
/// the requested Windows DACL or mandatory integrity label operation (as opposed to invalid
/// owner/ACL/parameter or access-denied errors on a share that does support persistent ACLs).
#[cfg_attr(not(windows), allow(dead_code))]
fn is_explicit_unsupported_fs_rc(rc: u32) -> bool {
    matches!(
        rc,
        1    // ERROR_INVALID_FUNCTION
        | 50 // ERROR_NOT_SUPPORTED
    )
}

/// Format an explicit failure when a volume reports `FILE_PERSISTENT_ACLS == 0`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn format_unsupported_volume_error(path: &Path, vol: &VolumeAclInfo) -> String {
    let fs = if vol.fs_name.is_empty() { "unknown" } else { vol.fs_name.as_str() };
    if is_wsl_path(path) {
        return format!(
            "filesystem {fs:?} for {} does not support persistent Windows ACLs (FILE_PERSISTENT_ACLS) required by the \
             Windows sandbox; use a writable root on a local NTFS volume or run mare inside WSL with the Linux sandbox \
             (error 50: ERROR_NOT_SUPPORTED)",
            path.display(),
        );
    }
    format!(
        "filesystem {fs:?} for {} does not support persistent Windows ACLs (FILE_PERSISTENT_ACLS) required by the \
         Windows sandbox; move the workspace to a local NTFS volume or choose a writable root on a supported volume \
         (error 50: ERROR_NOT_SUPPORTED)",
        path.display(),
    )
}

/// Enrich a Win32 ACL error (`GetNamedSecurityInfoW` / `SetNamedSecurityInfoW`) with an
/// unsupported-filesystem explanation ONLY when the OS volume capability query reports
/// `FILE_PERSISTENT_ACLS == 0` or the Win32 call returned an explicit unsupported-operation code
/// (`ERROR_INVALID_FUNCTION` = 1 / `ERROR_NOT_SUPPORTED` = 50). All other failures (such as
/// `ERROR_INVALID_OWNER`, `ERROR_INVALID_ACL`, `ERROR_INVALID_SECURITY_DESCR`,
/// `ERROR_INVALID_PARAMETER`, or `ERROR_ACCESS_DENIED`) preserve the original Win32 error without
/// assuming the filesystem or UNC share lacks ACL support.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn diagnose_acl_failure(op: &str, path: &Path, rc: u32, vol: Option<&VolumeAclInfo>) -> String {
    let raw = format!("{op}({}) failed: {rc}", path.display());
    if let Some(v) = vol {
        if !v.persistent_acls {
            let fs = if v.fs_name.is_empty() { "unknown" } else { v.fs_name.as_str() };
            if is_wsl_path(path) {
                return format!(
                    "{raw}: filesystem {fs:?} on this WSL share does not support persistent Windows ACLs \
                     (FILE_PERSISTENT_ACLS) or mandatory labels required by the Windows sandbox; use a writable root \
                     on a local NTFS volume or run mare inside WSL with the Linux sandbox"
                );
            }
            return format!(
                "{raw}: filesystem {fs:?} does not support persistent Windows ACLs (FILE_PERSISTENT_ACLS) or mandatory \
                 labels required by the Windows sandbox; use a writable root on a local NTFS volume"
            );
        }
        // Volume explicitly reports FILE_PERSISTENT_ACLS == 1: only treat explicit
        // ERROR_INVALID_FUNCTION / ERROR_NOT_SUPPORTED as an unsupported ACL/label operation;
        // never blame the filesystem for owner/ACL/parameter/access errors.
        if is_explicit_unsupported_fs_rc(rc) {
            let fs = if v.fs_name.is_empty() { "unknown" } else { v.fs_name.as_str() };
            return format!(
                "{raw}: volume {fs:?} rejected the required Windows DACL or mandatory integrity label operation as \
                 unsupported; use a writable root on a local NTFS volume"
            );
        }
        return raw;
    }
    if is_explicit_unsupported_fs_rc(rc) {
        if is_wsl_path(path) {
            return format!(
                "{raw}: WSL filesystem shares (\\\\wsl.localhost\\ / \\\\wsl$\\) do not support the persistent \
                 Windows DACLs and mandatory integrity labels required by the Windows sandbox; use a writable root on \
                 a local NTFS volume or run mare inside WSL with the Linux sandbox"
            );
        }
        if is_unc_path(path) {
            return format!(
                "{raw}: this UNC/network share rejected the required persistent Windows DACL or mandatory integrity \
                 label operation as unsupported; use a writable root on a local NTFS volume"
            );
        }
        return format!(
            "{raw}: the target filesystem does not support the persistent Windows DACLs or mandatory integrity labels \
             required by the Windows sandbox; use a writable root on a local NTFS volume"
        );
    }
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_acl_failure_explains_wsl_unc_and_non_ntfs_while_keeping_error_code() {
        let wsl_err =
            diagnose_acl_failure("GetNamedSecurityInfoW", Path::new(r"\\wsl.localhost\Ubuntu\home\dev\repo"), 1, None);
        assert!(wsl_err.contains("GetNamedSecurityInfoW(") && wsl_err.contains("failed: 1"), "{wsl_err}");
        assert!(wsl_err.contains("WSL filesystem shares") && wsl_err.contains("local NTFS volume"), "{wsl_err}");

        let wsl_dollar =
            diagnose_acl_failure("SetNamedSecurityInfoW", Path::new(r"\\?\UNC\wsl$\Ubuntu\repo"), 50, None);
        assert!(wsl_dollar.contains("failed: 50") && wsl_dollar.contains("WSL filesystem shares"), "{wsl_dollar}");

        let exfat = VolumeAclInfo { fs_name: "exFAT".into(), persistent_acls: false };
        let exfat_err = diagnose_acl_failure("SetNamedSecurityInfoW", Path::new(r"E:\proj"), 50, Some(&exfat));
        assert!(exfat_err.contains("failed: 50") && exfat_err.contains("exFAT"), "{exfat_err}");
        assert!(exfat_err.contains("FILE_PERSISTENT_ACLS") && exfat_err.contains("local NTFS volume"), "{exfat_err}");

        let unc_unsupported = diagnose_acl_failure("GetNamedSecurityInfoW", Path::new(r"\\nas\share\repo"), 50, None);
        assert!(
            unc_unsupported.contains("failed: 50") && unc_unsupported.contains("UNC/network share"),
            "{unc_unsupported}"
        );

        // A UNC/SMB share supporting persistent ACLs returning invalid-owner/ACL/descriptor/param
        // errors must keep its original Win32 code without misdiagnosing the filesystem as unsupported.
        let ntfs = VolumeAclInfo { fs_name: "NTFS".into(), persistent_acls: true };
        for rc in [5u32, 87, 124, 1307, 1336, 1338] {
            let smb_err =
                diagnose_acl_failure("SetNamedSecurityInfoW", Path::new(r"\\fileserver\share\repo"), rc, Some(&ntfs));
            assert_eq!(smb_err, format!(r"SetNamedSecurityInfoW(\\fileserver\share\repo) failed: {rc}"));

            let unknown_unc_err =
                diagnose_acl_failure("SetNamedSecurityInfoW", Path::new(r"\\fileserver\share\repo"), rc, None);
            assert_eq!(unknown_unc_err, format!(r"SetNamedSecurityInfoW(\\fileserver\share\repo) failed: {rc}"));
        }

        // Access denied (5) on local NTFS must not be misdiagnosed as an unsupported filesystem.
        let denied = diagnose_acl_failure("SetNamedSecurityInfoW", Path::new(r"C:\proj"), 5, Some(&ntfs));
        assert_eq!(denied, r"SetNamedSecurityInfoW(C:\proj) failed: 5");
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
