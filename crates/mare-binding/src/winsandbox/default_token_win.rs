//! Restricted token construction and SID inspection for the Windows default sandbox tier.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{EXPLICIT_ACCESS_W, GRANT_ACCESS, SetEntriesInAclW};
use windows_sys::Win32::Security::{
    ACL, AdjustTokenPrivileges, CopySid, CreateRestrictedToken, DISABLE_MAX_PRIVILEGE, EqualSid, GetLengthSid,
    GetTokenInformation, LUA_TOKEN, LookupPrivilegeValueW, SID_AND_ATTRIBUTES, SetTokenInformation,
    TOKEN_ADJUST_DEFAULT, TOKEN_ADJUST_PRIVILEGES, TOKEN_ADJUST_SESSIONID, TOKEN_ASSIGN_PRIMARY, TOKEN_DEFAULT_DACL,
    TOKEN_DUPLICATE, TOKEN_LINKED_TOKEN, TOKEN_MANDATORY_LABEL, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
    TokenDefaultDacl, TokenGroups, TokenIntegrityLevel, TokenLinkedToken, TokenUser, WRITE_RESTRICTED, WinLowLabelSid,
    WinWorldSid,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::acl::explicit;
use super::{BufSid, LocalSid, last, wide};

const SE_GROUP_INTEGRITY: u32 = 0x0000_0020;
const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;

/// Can this process mint the confined token? (its token carries a logon SID)
pub fn probe() -> bool {
    unsafe {
        let mut base: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut base) == 0 {
            return false;
        }
        let ok = logon_sid(base).is_ok();
        CloseHandle(base);
        ok
    }
}

/// The logon SID of `token` (copied out of TOKEN_GROUPS).
pub(super) unsafe fn logon_sid(token: HANDLE) -> Result<BufSid, String> {
    unsafe {
        if let Ok(sid) = scan_logon_sid(token) {
            return Ok(sid);
        }
        let mut linked: TOKEN_LINKED_TOKEN = std::mem::zeroed();
        let mut needed = 0;
        if GetTokenInformation(
            token,
            TokenLinkedToken,
            (&mut linked as *mut TOKEN_LINKED_TOKEN).cast(),
            std::mem::size_of::<TOKEN_LINKED_TOKEN>() as u32,
            &mut needed,
        ) != 0
            && !linked.LinkedToken.is_null()
        {
            let sid = scan_logon_sid(linked.LinkedToken);
            CloseHandle(linked.LinkedToken);
            return sid;
        }
        Err("no logon SID on the current or linked token".into())
    }
}

unsafe fn scan_logon_sid(token: HANDLE) -> Result<BufSid, String> {
    unsafe {
        let mut needed = 0u32;
        GetTokenInformation(token, TokenGroups, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(last("GetTokenInformation(TokenGroups) size"));
        }
        // u64 buffer: TOKEN_GROUPS holds pointers
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        if GetTokenInformation(token, TokenGroups, buf.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            return Err(last("GetTokenInformation(TokenGroups)"));
        }
        let groups = buf.as_ptr() as *const windows_sys::Win32::Security::TOKEN_GROUPS;
        let count = (*groups).GroupCount as usize;
        let first = std::ptr::addr_of!((*groups).Groups) as *const SID_AND_ATTRIBUTES;
        for i in 0..count {
            let g = *first.add(i);
            if g.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID {
                let len = GetLengthSid(g.Sid);
                let mut out = vec![0u8; len as usize];
                if CopySid(len, out.as_mut_ptr().cast(), g.Sid) == 0 {
                    return Err(last("CopySid(logon)"));
                }
                return Ok(BufSid(out));
            }
        }
        Err("no logon SID on the current token".into())
    }
}

/// The user SID of `token` (copied out of TOKEN_USER).
pub(super) unsafe fn user_sid(token: HANDLE) -> Result<BufSid, String> {
    unsafe {
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(last("GetTokenInformation(TokenUser) size"));
        }
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed) == 0 {
            return Err(last("GetTokenInformation(TokenUser)"));
        }
        let sid = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
        let len = GetLengthSid(sid);
        let mut out = vec![0u8; len as usize];
        if CopySid(len, out.as_mut_ptr().cast(), sid) == 0 {
            return Err(last("CopySid(user)"));
        }
        Ok(BufSid(out))
    }
}

/// The token user when it is the sandbox account `expected` (enhanced tier): checked, so a
/// runner started as the real user by mistake fails closed instead of restricting to the real
/// user's SID.
unsafe fn account_sid(token: HANDLE, expected: &str) -> Result<BufSid, String> {
    unsafe {
        let want = LocalSid::parse(expected)?;
        let user = user_sid(token)?;
        if EqualSid(user.ptr(), want.0) == 0 {
            return Err(format!(
                "the inner runner is not running as the sandbox account {expected}; refusing to add its SID"
            ));
        }
        Ok(user)
    }
}

/// A default DACL granting the given SIDs full access to objects the confined process creates
/// (pipes, events): without it PowerShell pipelines fail with ACCESS_DENIED.
unsafe fn set_default_dacl(token: HANDLE, sids: &[*mut c_void]) -> Result<(), String> {
    unsafe {
        let entries: Vec<EXPLICIT_ACCESS_W> = sids.iter().map(|s| explicit(*s, GRANT_ACCESS, GENERIC_ALL, 0)).collect();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let rc = SetEntriesInAclW(entries.len() as u32, entries.as_ptr(), std::ptr::null(), &mut dacl);
        if rc != 0 {
            return Err(format!("SetEntriesInAclW(default DACL) failed: {rc}"));
        }
        let info = TOKEN_DEFAULT_DACL { DefaultDacl: dacl };
        let ok = SetTokenInformation(
            token,
            TokenDefaultDacl,
            &info as *const _ as *const c_void,
            std::mem::size_of::<TOKEN_DEFAULT_DACL>() as u32,
        );
        LocalFree(dacl as HLOCAL);
        if ok == 0 {
            return Err(last("SetTokenInformation(TokenDefaultDacl)"));
        }
        Ok(())
    }
}

// Preserve privilege adjustment order.
unsafe fn enable_single_privilege(h_token: HANDLE, name: &str) -> Result<(), String> {
    unsafe {
        let mut luid = std::mem::zeroed();
        let ok = LookupPrivilegeValueW(std::ptr::null(), wide(name).as_ptr(), &mut luid);
        if ok == 0 {
            return Err(format!("LookupPrivilegeValueW failed: {}", GetLastError()));
        }
        let mut tp: TOKEN_PRIVILEGES = std::mem::zeroed();
        tp.PrivilegeCount = 1;
        tp.Privileges[0].Luid = luid;
        tp.Privileges[0].Attributes = 0x00000002; // SE_PRIVILEGE_ENABLED
        let ok2 = AdjustTokenPrivileges(h_token, 0, &tp, 0, std::ptr::null_mut(), std::ptr::null_mut());
        if ok2 == 0 {
            return Err(format!("AdjustTokenPrivileges failed: {}", GetLastError()));
        }
        let err = GetLastError();
        if err != 0 {
            return Err(format!("AdjustTokenPrivileges error {err}"));
        }
        Ok(())
    }
}

/// Build the confined token: write-restricted to logon + Everyone + `caps` (+ the sandbox
/// account's own SID in the enhanced tier, see [`RunnerArgs::account`]), Low integrity.
pub(super) unsafe fn restricted_token(caps: &[LocalSid], account: Option<&str>) -> Result<HANDLE, String> {
    if account.is_some() && caps.is_empty() {
        return Err("enhanced sandbox requires capability SIDs".into());
    }
    unsafe {
        let mut base: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY
                | TOKEN_DUPLICATE
                | TOKEN_ADJUST_DEFAULT
                | TOKEN_ASSIGN_PRIMARY
                | TOKEN_ADJUST_PRIVILEGES
                | TOKEN_ADJUST_SESSIONID,
            &mut base,
        ) == 0
        {
            return Err(last("OpenProcessToken"));
        }
        let logon = match logon_sid(base) {
            Ok(l) => l,
            Err(e) => {
                CloseHandle(base);
                return Err(e);
            }
        };
        let user = match account.map(|a| account_sid(base, a)).transpose() {
            Ok(u) => u,
            Err(e) => {
                CloseHandle(base);
                return Err(e);
            }
        };
        let world = BufSid::well_known(WinWorldSid)?;
        let low = BufSid::well_known(WinLowLabelSid)?;
        let mut restrict: Vec<SID_AND_ATTRIBUTES> =
            caps.iter().map(|c| SID_AND_ATTRIBUTES { Sid: c.0, Attributes: 0 }).collect();
        // an identity, not a capability: kept out of the default DACL below
        if let Some(u) = &user {
            restrict.push(SID_AND_ATTRIBUTES { Sid: u.ptr(), Attributes: 0 });
        }
        restrict.push(SID_AND_ATTRIBUTES { Sid: logon.ptr(), Attributes: 0 });
        restrict.push(SID_AND_ATTRIBUTES { Sid: world.ptr(), Attributes: 0 });
        let mut token: HANDLE = std::ptr::null_mut();
        let ok = CreateRestrictedToken(
            base,
            DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED | if account.is_some() { LUA_TOKEN } else { 0 },
            0,
            std::ptr::null(),
            0,
            std::ptr::null(),
            restrict.len() as u32,
            restrict.as_ptr(),
            &mut token,
        );
        CloseHandle(base);
        if ok == 0 {
            return Err(last("CreateRestrictedToken"));
        }
        let mut dacl_sids = vec![logon.ptr(), world.ptr()];
        dacl_sids.extend(caps.iter().map(|c| c.0));
        let fail = |e: String| {
            CloseHandle(token);
            Err(e)
        };
        if let Err(e) = set_default_dacl(token, &dacl_sids) {
            return fail(e);
        }
        if account.is_some() {
            if let Err(error) = enable_single_privilege(token, "SeChangeNotifyPrivilege") {
                return fail(error);
            }
            return Ok(token);
        }
        let label =
            TOKEN_MANDATORY_LABEL { Label: SID_AND_ATTRIBUTES { Sid: low.ptr(), Attributes: SE_GROUP_INTEGRITY } };
        if SetTokenInformation(
            token,
            TokenIntegrityLevel,
            &label as *const _ as *const c_void,
            (std::mem::size_of::<TOKEN_MANDATORY_LABEL>() as u32) + GetLengthSid(low.ptr()),
        ) == 0
        {
            return fail(last("SetTokenInformation(TokenIntegrityLevel)"));
        }
        Ok(token)
    }
}
