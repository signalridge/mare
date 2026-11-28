// exec-ctx: not a tool entry (Windows sandbox ACL/SID setup helpers; invoked from sandboxed process launch)
//! ACL and NUL-device capability grants for the Windows default sandbox tier.

use std::ffi::c_void;
use std::path::Path;

use windows_sys::Win32::Foundation::{HLOCAL, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE,
    REVOKE_ACCESS, SE_FILE_OBJECT, SE_KERNEL_OBJECT, SET_ACCESS, SetEntriesInAclW, SetNamedSecurityInfoW,
    SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{
    ACL, ACL_REVISION, AddMandatoryAce, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, GetLengthSid,
    GetSecurityDescriptorControl, InitializeAcl, LABEL_SECURITY_INFORMATION, SE_DACL_PROTECTED,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, WinLowLabelSid, WinWorldSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_SHARE_READ, FILE_SHARE_WRITE,
    LOCKFILE_EXCLUSIVE_LOCK, LockFileEx, OPEN_EXISTING, UnlockFileEx,
};
use windows_sys::Win32::System::IO::OVERLAPPED;

use super::{BufSid, LocalSid, last, wide};

const SYSTEM_MANDATORY_LABEL_NO_WRITE_UP: u32 = 0x1;
pub(super) const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
const DELETE: u32 = 0x0001_0000;
const FILE_DELETE_CHILD: u32 = 0x0000_0040;
const STANDARD_RIGHTS_WRITE: u32 = 0x0002_0000;
/// Write + delete grant, without READ_CONTROL (reads come from the user SID).
const GRANT_MASK: u32 = (FILE_GENERIC_WRITE | DELETE | FILE_DELETE_CHILD) & !STANDARD_RIGHTS_WRITE;

pub(super) fn explicit(sid: *mut c_void, mode: i32, mask: u32, inherit: u32) -> EXPLICIT_ACCESS_W {
    EXPLICIT_ACCESS_W {
        grfAccessPermissions: mask,
        grfAccessMode: mode,
        grfInheritance: inherit,
        Trustee: TRUSTEE_W {
            pMultipleTrustee: std::ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_UNKNOWN,
            ptstrName: sid.cast(),
        },
    }
}

/// Does `acl` already hold this exact ACE (type, flags, mask, SID)?
pub(super) unsafe fn has_exact(acl: *const ACL, ace_type: u8, flags: u8, mask: u32, sid: *mut c_void) -> bool {
    if acl.is_null() {
        return false;
    }
    unsafe {
        let base = acl as *const u8;
        let size = std::ptr::read_unaligned(base.add(2) as *const u16) as usize;
        let count = std::ptr::read_unaligned(base.add(4) as *const u16) as usize;
        if !(8..=1 << 20).contains(&size) {
            return false;
        }
        let sid_len = GetLengthSid(sid) as usize;
        let want = std::slice::from_raw_parts(sid as *const u8, sid_len);
        let mut off = 8;
        for _ in 0..count {
            if off + 8 > size {
                return false;
            }
            let ace_size = std::ptr::read_unaligned(base.add(off + 2) as *const u16) as usize;
            if ace_size < 8 || off + ace_size > size {
                return false;
            }
            let exact = *base.add(off) == ace_type
                && *base.add(off + 1) == flags
                && std::ptr::read_unaligned(base.add(off + 4) as *const u32) == mask
                && ace_size >= 8 + sid_len
                && std::slice::from_raw_parts(base.add(off + 8), sid_len) == want;
            if exact {
                return true;
            }
            off += ace_size;
        }
        false
    }
}

/// Does `acl` hold an allow ACE granting at least `mask` to `sid` (with any inheritance flags)?
unsafe fn has_allow_mask(acl: *const ACL, mask: u32, sid: *mut c_void) -> bool {
    if acl.is_null() {
        return false;
    }
    unsafe {
        let base = acl as *const u8;
        let size = std::ptr::read_unaligned(base.add(2) as *const u16) as usize;
        let count = std::ptr::read_unaligned(base.add(4) as *const u16) as usize;
        if !(8..=1 << 20).contains(&size) {
            return false;
        }
        let sid_len = GetLengthSid(sid) as usize;
        let want = std::slice::from_raw_parts(sid as *const u8, sid_len);
        let mut off = 8;
        for _ in 0..count {
            if off + 8 > size {
                return false;
            }
            let ace_size = std::ptr::read_unaligned(base.add(off + 2) as *const u16) as usize;
            if ace_size < 8 || off + ace_size > size {
                return false;
            }
            let ace_mask = std::ptr::read_unaligned(base.add(off + 4) as *const u32);
            if *base.add(off) == ACCESS_ALLOWED_ACE_TYPE
                && (ace_mask & mask) == mask
                && ace_size >= 8 + sid_len
                && std::slice::from_raw_parts(base.add(off + 8), sid_len) == want
            {
                return true;
            }
            off += ace_size;
        }
        false
    }
}

unsafe fn is_standing_grant(dacl: *const ACL, sacl: *const ACL, cap: &LocalSid, world: &BufSid, low: &BufSid) -> bool {
    unsafe {
        has_exact(dacl, ACCESS_ALLOWED_ACE_TYPE, SUB_CONTAINERS_AND_OBJECTS_INHERIT as u8, GRANT_MASK, cap.0)
            && has_exact(dacl, ACCESS_DENIED_ACE_TYPE, CONTAINER_INHERIT_ACE as u8, FILE_DELETE_CHILD, world.ptr())
            && has_exact(
                sacl,
                SYSTEM_MANDATORY_LABEL_ACE_TYPE,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT as u8,
                SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                low.ptr(),
            )
    }
}

/// Fast read-only check whether `path` currently carries the exact standing write grant for `sid`.
pub fn has_standing_grant(path: &Path, sid: &str) -> Result<bool, String> {
    let cap = LocalSid::parse(sid)?;
    let low = BufSid::well_known(WinLowLabelSid)?;
    let world = BufSid::well_known(WinWorldSid)?;
    let wpath = wide(path);
    unsafe {
        let (mut dacl, mut sacl, mut sd): (*mut ACL, *mut ACL, *mut c_void) =
            (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        let rc = GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            &mut sacl,
            &mut sd,
        );
        if rc != 0 {
            return Err(format!("GetNamedSecurityInfoW({}) failed: {rc}", path.display()));
        }
        let standing = is_standing_grant(dacl, sacl, &cap, &world, &low);
        LocalFree(sd as HLOCAL);
        Ok(standing)
    }
}

/// Verify that `target` and every intermediate directory up to `root` can receive the write grant
/// for `sid`. Preserves administrator-set protected DACLs (`SE_DACL_PROTECTED`), returning
/// `RootInheritance::Blocked` with a targeted diagnostic when inheritance is disabled and no
/// explicit grant for `sid` is present.
pub(crate) fn verify_dir_inheritance(
    root: &Path,
    target: &Path,
    sid: &str,
) -> Result<super::super::grants::RootInheritance, String> {
    use super::super::grants::RootInheritance;
    let cap = LocalSid::parse(sid)?;
    let root_canon = crate::realpath::canonical_path_key(root).ok();
    let mut cur = target;
    loop {
        if let (Some(rk), Ok(ck)) = (&root_canon, crate::realpath::canonical_path_key(cur))
            && ck == *rk
        {
            break;
        }
        if cur == root {
            break;
        }
        let wpath = wide(cur);
        unsafe {
            let (mut dacl, mut sd): (*mut ACL, *mut c_void) = (std::ptr::null_mut(), std::ptr::null_mut());
            let rc = GetNamedSecurityInfoW(
                wpath.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            );
            if rc != 0 {
                return Err(format!("GetNamedSecurityInfoW({}) failed: {rc}", cur.display()));
            }
            let mut control: u16 = 0;
            let mut revision: u32 = 0;
            let got_ctrl = GetSecurityDescriptorControl(sd, &mut control, &mut revision) != 0;
            let protected = got_ctrl && (control & SE_DACL_PROTECTED) != 0;
            let has_grant = has_allow_mask(dacl, GRANT_MASK, cap.0);
            LocalFree(sd as HLOCAL);
            if has_grant && cur == target {
                return Ok(RootInheritance::Allowed);
            }
            if protected {
                if has_grant {
                    return Ok(RootInheritance::Allowed);
                }
                return Ok(RootInheritance::Blocked(super::super::grants::format_protected_dacl_diagnostic(cur, root)));
            }
        }
        let Some(parent) = cur.parent() else {
            break;
        };
        if parent == cur {
            break;
        }
        cur = parent;
    }
    Ok(RootInheritance::Allowed)
}

/// Serialize read-modify-write DACL updates per canonical path across processes using `LockFileEx`.
pub(crate) fn with_path_acl_lock<T>(path: &Path, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    use sha2::{Digest, Sha256};
    use std::os::windows::io::AsRawHandle;

    let key = crate::realpath::canonical_path_key(path)
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/").to_lowercase());
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    let dir = mare_shared::paths::state_dir().join("acl-locks");
    mare_shared::private::create_dir_all(&dir).map_err(|e| format!("acl lock dir: {e}"))?;
    let lock_path = dir.join(format!("{hex}.lock"));
    let mut oo = std::fs::OpenOptions::new();
    oo.read(true).write(true).create(true).truncate(false);
    let file = mare_shared::private::open(&mut oo, &lock_path).map_err(|e| format!("acl lock open: {e}"))?;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let handle = file.as_raw_handle();
    if unsafe { LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, 1, 0, &mut overlapped) } == 0 {
        return Err(last("LockFileEx(acl)"));
    }
    let res = f();
    let mut unlock_ov: OVERLAPPED = unsafe { std::mem::zeroed() };
    unsafe { UnlockFileEx(handle, 0, 1, 0, &mut unlock_ov) };
    res
}

/// Grant `sid` write access under `path` (inheritable), deny ambient child deletes for
/// Everyone and label the tree Low. Idempotent: an exact standing grant is
/// left alone (no tree-wide re-propagation). Serialized per path via `LockFileEx`.
pub fn grant_write(path: &Path, sid: &str) -> Result<(), String> {
    with_path_acl_lock(path, || grant_write_locked(path, sid))
}

fn grant_write_locked(path: &Path, sid: &str) -> Result<(), String> {
    let cap = LocalSid::parse(sid)?;
    let low = BufSid::well_known(WinLowLabelSid)?;
    let world = BufSid::well_known(WinWorldSid)?;
    let wpath = wide(path);
    unsafe {
        let (mut dacl, mut sacl, mut sd): (*mut ACL, *mut ACL, *mut c_void) =
            (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        let rc = GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            &mut sacl,
            &mut sd,
        );
        if rc != 0 {
            return Err(format!("GetNamedSecurityInfoW({}) failed: {rc}", path.display()));
        }
        let standing = is_standing_grant(dacl, sacl, &cap, &world, &low);
        if standing {
            LocalFree(sd as HLOCAL);
            return Ok(());
        }
        // Low mandatory label ACL: header + one mandatory ACE (8 bytes + SID), 4-aligned
        let low_len = GetLengthSid(low.ptr());
        let acl_len = (8 + 8 + low_len).next_multiple_of(4);
        let mut label_buf = vec![0u32; acl_len as usize / 4];
        let label = label_buf.as_mut_ptr() as *mut ACL;
        if InitializeAcl(label, acl_len, ACL_REVISION) == 0
            || AddMandatoryAce(
                label,
                ACL_REVISION,
                SUB_CONTAINERS_AND_OBJECTS_INHERIT,
                SYSTEM_MANDATORY_LABEL_NO_WRITE_UP,
                low.ptr(),
            ) == 0
        {
            let e = last("building the Low label ACL");
            LocalFree(sd as HLOCAL);
            return Err(e);
        }
        let entries = [
            explicit(world.ptr(), DENY_ACCESS, FILE_DELETE_CHILD, CONTAINER_INHERIT_ACE),
            explicit(cap.0, GRANT_ACCESS, GRANT_MASK, SUB_CONTAINERS_AND_OBJECTS_INHERIT),
        ];
        let mut new_dacl: *mut ACL = std::ptr::null_mut();
        let rc = SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut new_dacl);
        LocalFree(sd as HLOCAL);
        if rc != 0 {
            return Err(format!("SetEntriesInAclW({}) failed: {rc}", path.display()));
        }
        let rc = SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            new_dacl,
            label,
        );
        LocalFree(new_dacl as HLOCAL);
        if rc != 0 {
            return Err(format!("SetNamedSecurityInfoW({}) failed: {rc}", path.display()));
        }
    }
    Ok(())
}

const NUL_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE;

/// RAII guard revoking ephemeral per-session temp capability SIDs from `\\.\NUL` on drop so the
/// 64 KiB DACL cannot be exhausted across sessions.
pub(super) struct NullDeviceGuard {
    ephemeral_sids: Vec<String>,
}

impl Drop for NullDeviceGuard {
    fn drop(&mut self) {
        if self.ephemeral_sids.is_empty() {
            return;
        }
        let sids: Vec<LocalSid> = self.ephemeral_sids.iter().filter_map(|s| LocalSid::parse(s).ok()).collect();
        if sids.is_empty() {
            return;
        }
        let _ = with_path_acl_lock(Path::new(r"\\.\NUL"), || {
            revoke_null_device(&sids);
            Ok(())
        });
    }
}

fn open_null_device_dac() -> Option<std::os::windows::io::OwnedHandle> {
    use std::os::windows::io::FromRawHandle;
    let handle = unsafe {
        CreateFileW(
            wide(r"\\.\NUL").as_ptr(),
            0x0002_0000 | 0x0004_0000,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        None
    } else {
        Some(unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) })
    }
}

fn revoke_null_device(sids: &[LocalSid]) {
    use std::os::windows::io::AsRawHandle;
    let Some(handle) = open_null_device_dac() else { return };
    unsafe {
        let mut sd = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let code = GetSecurityInfo(
            handle.as_raw_handle(),
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if code == 0 {
            let entries: Vec<_> = sids.iter().map(|cap| explicit(cap.0, REVOKE_ACCESS, 0, 0)).collect();
            let mut updated = std::ptr::null_mut();
            if SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut updated) == 0 {
                let _ = SetSecurityInfo(
                    handle.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    updated,
                    std::ptr::null_mut(),
                );
            }
            if !updated.is_null() {
                LocalFree(updated.cast());
            }
        }
        if !sd.is_null() {
            LocalFree(sd);
        }
    }
}

/// Best-effort access to the null device for capability-restricted processes that open NUL for
/// redirection. Preserve its existing DACL and grant only missing capability SIDs, without
/// inheritance, and return a guard that revokes ephemeral SIDs on teardown.
pub(super) fn allow_null_device(caps: &[LocalSid], cap_strs: &[String]) -> NullDeviceGuard {
    use std::os::windows::io::AsRawHandle;
    let mut guard = NullDeviceGuard { ephemeral_sids: Vec::new() };
    if caps.is_empty() {
        return guard;
    }
    let _ = with_path_acl_lock(Path::new(r"\\.\NUL"), || {
        let Some(handle) = open_null_device_dac() else { return Ok(()) };
        unsafe {
            let mut sd = std::ptr::null_mut();
            let mut dacl = std::ptr::null_mut();
            let code = GetSecurityInfo(
                handle.as_raw_handle(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            );
            if code == 0 {
                let missing: Vec<(usize, &LocalSid)> = caps
                    .iter()
                    .enumerate()
                    .filter(|(_, cap)| !has_exact(dacl, ACCESS_ALLOWED_ACE_TYPE, 0, NUL_MASK, cap.0))
                    .collect();
                if !missing.is_empty() {
                    let entries: Vec<_> =
                        missing.iter().map(|(_, cap)| explicit(cap.0, SET_ACCESS, NUL_MASK, 0)).collect();
                    let mut updated = std::ptr::null_mut();
                    if SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), dacl, &mut updated) == 0 {
                        let set_rc = SetSecurityInfo(
                            handle.as_raw_handle(),
                            SE_KERNEL_OBJECT,
                            DACL_SECURITY_INFORMATION,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                            updated,
                            std::ptr::null_mut(),
                        );
                        if set_rc == 0 {
                            for (idx, _) in &missing {
                                if let Some(s) = cap_strs.get(*idx)
                                    && crate::winsandbox::grants::is_ephemeral_temp_sid(s)
                                {
                                    guard.ephemeral_sids.push(s.clone());
                                }
                            }
                        }
                    }
                    if !updated.is_null() {
                        LocalFree(updated.cast());
                    }
                }
            }
            if !sd.is_null() {
                LocalFree(sd);
            }
        }
        Ok(())
    });
    guard
}
