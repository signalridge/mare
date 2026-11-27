//! Copyright 2025 OpenAI (Apache-2.0); modified for mare.
//! Win32 side of the enhanced tier: accounts, DPAPI, ACLs, WFP, run-as-account, self-test.

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use base64::Engine as _;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL, LocalFree, SetHandleInformation,
};
use windows_sys::Win32::NetworkManagement::NetManagement::{
    LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_3, NERR_Success, NetLocalGroupAdd, NetLocalGroupAddMembers,
    NetLocalGroupDel, NetUserAdd, NetUserDel, NetUserSetInfo, UF_DONT_EXPIRE_PASSWD, UF_SCRIPT, USER_INFO_1,
    USER_INFO_1003, USER_PRIV_USER,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSidToSidW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetNamedSecurityInfoW, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SE_FILE_OBJECT, SET_ACCESS, SetEntriesInAclW,
    SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, GetTokenInformation, LookupAccountNameW,
    NO_INHERITANCE, PROTECTED_DACL_SECURITY_INFORMATION, SE_DACL_PROTECTED, SID_NAME_USE,
    SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessWithLogonW, GetCurrentProcess,
    GetExitCodeProcess, INFINITE, OpenProcessToken, PROCESS_INFORMATION, ResumeThread, STARTUPINFOW, TerminateProcess,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};

#[path = "enhanced_ipc_win.rs"]
mod ipc;
pub(crate) fn run_ipc(raw: &[String]) -> Result<i32, String> {
    ipc::run_client(raw)
}

#[path = "enhanced_wfp_win.rs"]
mod wfp;

use wfp::{install_wfp, remove_wfp};

use super::{
    Account, GROUP, PROXY_PORT_RANGE, SELFTEST_ARG, SETUP_VERSION, SelfTest, SelfTester, SetupState, USER_OFFLINE,
    USER_ONLINE, sandbox_dir, sha256_file,
};
use crate::winsandbox::{RunnerArgs, argv_to_command_line, quote_windows_arg};

const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
const DELETE: u32 = 0x0001_0000;
const READ_MASK: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
const WRITE_ONLY_MASK: u32 = FILE_GENERIC_WRITE | DELETE;
const MODIFY_MASK: u32 = READ_MASK | FILE_GENERIC_WRITE | DELETE;
const ERROR_ALIAS_EXISTS: u32 = 1379;
const NERR_GROUP_EXISTS: u32 = 2223;

fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

fn last(what: &str) -> String {
    format!("{what} failed: {}", std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32))
}

// ---- DPAPI (machine scope: the elevated setup and the unelevated runner both decrypt) --------

fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
    let flags = CRYPTPROTECT_UI_FORBIDDEN | CRYPTPROTECT_LOCAL_MACHINE;
    let ok = unsafe {
        if protect {
            CryptProtectData(&input, null(), null(), null_mut(), null_mut(), flags, &mut out)
        } else {
            CryptUnprotectData(&input, null_mut(), null(), null_mut(), null_mut(), flags, &mut out)
        }
    };
    if ok == 0 {
        return Err(last(if protect { "CryptProtectData" } else { "CryptUnprotectData" }));
    }
    let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
    unsafe { LocalFree(out.pbData as HLOCAL) };
    Ok(v)
}

fn seal_password(pw: &str) -> Result<String, String> {
    Ok(base64::engine::general_purpose::STANDARD.encode(dpapi(pw.as_bytes(), true)?))
}

fn open_password(blob: &str) -> Result<String, String> {
    let raw = base64::engine::general_purpose::STANDARD.decode(blob).map_err(|e| format!("password blob: {e}"))?;
    String::from_utf8(dpapi(&raw, false)?).map_err(|_| "password is not UTF-8".into())
}

fn random_password() -> String {
    // 32 chars over a set that satisfies complexity rules (upper, lower, digit, symbol)
    const SETS: [&[u8]; 4] = [b"ABCDEFGHJKLMNPQRSTUVWXYZ", b"abcdefghijkmnopqrstuvwxyz", b"23456789", b"!#%+-=?@^_"];
    let mut buf = [0u8; 32];
    rand::fill(&mut buf[..]);
    buf.iter()
        .enumerate()
        .map(|(i, b)| {
            let set = SETS[i % 4];
            set[*b as usize % set.len()] as char
        })
        .collect()
}

// ---- accounts ----------------------------------------------------------------------------------

pub fn is_elevated() -> bool {
    unsafe {
        let mut tok: HANDLE = null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return false;
        }
        let mut e: TOKEN_ELEVATION = std::mem::zeroed();
        let mut n = 0u32;
        let ok = GetTokenInformation(
            tok,
            TokenElevation,
            &mut e as *mut _ as *mut c_void,
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut n,
        );
        CloseHandle(tok);
        ok != 0 && e.TokenIsElevated != 0
    }
}

/// Re-run this executable elevated (UAC prompt) with `args` and wait for it.
pub fn relaunch_elevated(args: &[String]) -> Result<i32, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let verb = wide("runas");
    let file = wide(&exe);
    let params = wide(args.iter().map(|a| quote_windows_arg(a)).collect::<Vec<_>>().join(" "));
    unsafe {
        let mut sei: SHELLEXECUTEINFOW = std::mem::zeroed();
        sei.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS;
        sei.lpVerb = verb.as_ptr();
        sei.lpFile = file.as_ptr();
        sei.lpParameters = params.as_ptr();
        sei.nShow = 1;
        if ShellExecuteExW(&mut sei) == 0 {
            return Err(last("ShellExecuteExW(runas)"));
        }
        WaitForSingleObject(sei.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(sei.hProcess, &mut code);
        CloseHandle(sei.hProcess);
        Ok(code as i32)
    }
}

fn ensure_group() -> Result<(), String> {
    let name = wide(GROUP);
    let comment = wide("mare sandbox accounts (managed by `mare sandbox setup`)");
    let info =
        LOCALGROUP_INFO_1 { lgrpi1_name: name.as_ptr() as *mut u16, lgrpi1_comment: comment.as_ptr() as *mut u16 };
    let mut parm = 0u32;
    let rc = unsafe { NetLocalGroupAdd(null(), 1, &info as *const _ as *const u8, &mut parm) };
    if rc != NERR_Success && rc != ERROR_ALIAS_EXISTS && rc != NERR_GROUP_EXISTS {
        return Err(format!("NetLocalGroupAdd({GROUP}) failed: {rc}"));
    }
    Ok(())
}

fn add_member(group: &str, member: &str) {
    let g = wide(group);
    let m = wide(member);
    let info = LOCALGROUP_MEMBERS_INFO_3 { lgrmi3_domainandname: m.as_ptr() as *mut u16 };
    // already a member → an error code we don't care about
    unsafe { NetLocalGroupAddMembers(null(), g.as_ptr(), 3, &info as *const _ as *const u8, 1) };
}

fn ensure_user(name: &str, password: &str) -> Result<(), String> {
    let n = wide(name);
    let p = wide(password);
    let info = USER_INFO_1 {
        usri1_name: n.as_ptr() as *mut u16,
        usri1_password: p.as_ptr() as *mut u16,
        usri1_password_age: 0,
        usri1_priv: USER_PRIV_USER,
        usri1_home_dir: null_mut(),
        usri1_comment: null_mut(),
        usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD,
        usri1_script_path: null_mut(),
    };
    let rc = unsafe { NetUserAdd(null(), 1, &info as *const _ as *const u8, null_mut()) };
    if rc != NERR_Success {
        // exists: reset the password to the new one
        let upd = USER_INFO_1003 { usri1003_password: p.as_ptr() as *mut u16 };
        let rc2 = unsafe { NetUserSetInfo(null(), n.as_ptr(), 1003, &upd as *const _ as *const u8, null_mut()) };
        if rc2 != NERR_Success {
            return Err(format!("creating account {name} failed: {rc}/{rc2}"));
        }
    }
    add_member("Users", name);
    add_member(GROUP, name);
    Ok(())
}

/// Account name → string SID.
fn account_sid(name: &str) -> Result<String, String> {
    let n = wide(name);
    let mut sid = vec![0u8; 256];
    let mut sid_len = sid.len() as u32;
    let mut dom = vec![0u16; 256];
    let mut dom_len = dom.len() as u32;
    let mut use_: SID_NAME_USE = 0;
    unsafe {
        if LookupAccountNameW(
            null(),
            n.as_ptr(),
            sid.as_mut_ptr().cast(),
            &mut sid_len,
            dom.as_mut_ptr(),
            &mut dom_len,
            &mut use_,
        ) == 0
        {
            return Err(last(&format!("LookupAccountNameW({name})")));
        }
        let mut s: *mut u16 = null_mut();
        if ConvertSidToStringSidW(sid.as_mut_ptr().cast(), &mut s) == 0 {
            return Err(last("ConvertSidToStringSidW"));
        }
        let mut len = 0;
        while *s.add(len) != 0 {
            len += 1;
        }
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
        LocalFree(s as HLOCAL);
        Ok(out)
    }
}

// ---- ACLs --------------------------------------------------------------------------------------

/// Apply one ACE (`mode` = GRANT/DENY/REVOKE) for `sid` on `path` (inheritable for directories).
fn set_ace(path: &Path, sid: &str, mode: i32, mask: u32) -> Result<(), String> {
    crate::winsandbox::win::with_path_acl_lock(path, || set_ace_locked(path, sid, mode, mask))
}

fn set_ace_locked(path: &Path, sid: &str, mode: i32, mask: u32) -> Result<(), String> {
    let wpath = wide(path);
    let wsid = wide(sid);
    unsafe {
        let mut psid: *mut c_void = null_mut();
        if ConvertStringSidToSidW(wsid.as_ptr(), &mut psid) == 0 {
            return Err(last(&format!("ConvertStringSidToSidW({sid})")));
        }
        let (mut dacl, mut sd): (*mut ACL, *mut c_void) = (null_mut(), null_mut());
        let rc = GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            LocalFree(psid as HLOCAL);
            return Err(format!("GetNamedSecurityInfoW({}) failed: {rc}", path.display()));
        }
        let inherit = if path.is_dir() { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE };
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: mode,
            grfInheritance: inherit,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: psid.cast(),
            },
        };
        let mut new_dacl: *mut ACL = null_mut();
        let rc = SetEntriesInAclW(1, &ea, dacl, &mut new_dacl);
        LocalFree(sd as HLOCAL);
        if rc != 0 {
            LocalFree(psid as HLOCAL);
            return Err(format!("SetEntriesInAclW({}) failed: {rc}", path.display()));
        }
        let rc = SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new_dacl,
            null(),
        );
        LocalFree(new_dacl as HLOCAL);
        LocalFree(psid as HLOCAL);
        if rc != 0 {
            return Err(format!("SetNamedSecurityInfoW({}) failed: {rc}", path.display()));
        }
    }
    Ok(())
}

fn runtime_grants_file() -> PathBuf {
    mare_shared::paths::state_dir().join("win_sandbox_grants.json")
}

const RUNTIME_GRANT_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const RUNTIME_GRANT_LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(10);

fn record_runtime_grant(path: &Path, add: bool) {
    let record_file = runtime_grants_file();
    let lock_path = record_file.with_extension("lock");
    let _guard = mare_shared::private::lock_file_blocking(
        &lock_path,
        Some(RUNTIME_GRANT_LOCK_TIMEOUT),
        RUNTIME_GRANT_LOCK_POLL,
        true,
        None,
    )
    .ok();
    let mut entries: Vec<PathBuf> =
        std::fs::read_to_string(&record_file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let key = super::path_key(path);
    if add {
        if !entries.iter().any(|p| super::path_key(p) == key) {
            entries.push(path.to_path_buf());
        }
    } else {
        entries.retain(|p| super::path_key(p) != key);
    }
    if let Ok(bytes) = serde_json::to_vec(&entries) {
        let _ = mare_shared::paths::write_atomic(&record_file, &bytes, mare_shared::paths::WriteMode::Private);
    }
}

type ActiveGroupGrants = std::sync::Mutex<super::GroupGrantTracker>;

fn active_group_grants() -> &'static ActiveGroupGrants {
    static ACTIVE: std::sync::OnceLock<ActiveGroupGrants> = std::sync::OnceLock::new();
    ACTIVE.get_or_init(Default::default)
}

/// RAII guard tracking runtime `grant_group` ACEs for the sandbox accounts and downgrading or
/// revoking them on teardown according to the remaining effective mask union for `(path, group_sid)`.
pub struct GroupGrantGuard {
    group_sid: String,
    applied: Vec<(PathBuf, u32)>,
}

impl Drop for GroupGrantGuard {
    fn drop(&mut self) {
        let mut active = active_group_grants().lock().unwrap_or_else(|e| e.into_inner());
        for (path, mask) in &self.applied {
            let path_key = super::path_key(path);
            match active.release(&path_key, &self.group_sid, *mask) {
                Some(super::GrantTransition::SetMask(effective)) => {
                    if path.exists() {
                        let _ = set_ace(path, &self.group_sid, SET_ACCESS, effective);
                    }
                }
                Some(super::GrantTransition::Revoke) => {
                    if path.exists() {
                        let _ = set_ace(path, &self.group_sid, REVOKE_ACCESS, 0);
                    }
                    record_runtime_grant(path, false);
                }
                None => {}
            }
        }
    }
}

/// Scoped runtime grants for the sandbox accounts: `MODIFY_MASK` on `writable_rw`, `WRITE_ONLY_MASK`
/// (without `READ_MASK`) on `writable_wo`, and `READ_MASK` on `readable`. Downgraded or revoked when
/// the returned [`GroupGrantGuard`] drops.
pub fn grant_group_scoped(
    group_sid: &str,
    writable_rw: &[PathBuf],
    writable_wo: &[PathBuf],
    readable: &[PathBuf],
) -> Result<GroupGrantGuard, String> {
    let mut desired: Vec<(PathBuf, u32)> = Vec::new();
    for p in writable_rw {
        desired.push((p.clone(), MODIFY_MASK));
    }
    for p in writable_wo {
        desired.push((p.clone(), WRITE_ONLY_MASK));
    }
    for p in readable {
        desired.push((p.clone(), READ_MASK));
    }
    let mut guard = GroupGrantGuard { group_sid: group_sid.to_string(), applied: Vec::new() };
    let mut active = active_group_grants().lock().unwrap_or_else(|e| e.into_inner());
    for (p, mask) in desired {
        let path_key = super::path_key(&p);
        let (was_empty, next_mask) = active.acquire(path_key.clone(), group_sid.to_string(), mask);
        if let Some(effective) = next_mask {
            if let Err(err) = set_ace(&p, group_sid, SET_ACCESS, effective) {
                let _ = active.release(&path_key, group_sid, mask);
                drop(active);
                return Err(err);
            }
            if was_empty {
                record_runtime_grant(&p, true);
            }
        }
        guard.applied.push((p, mask));
    }
    Ok(guard)
}

/// Runtime grants for the sandbox accounts (run by the real user, who owns these paths): read on
/// `readable`, read + modify on `writable` (the capability-SID grants of the default tier still
/// gate the write-restricted token). Recorded for cleanup on `remove()`.
pub fn grant_group(group_sid: &str, writable: &[PathBuf], readable: &[PathBuf]) -> Result<(), String> {
    for (p, w) in writable.iter().map(|p| (p, true)).chain(readable.iter().map(|p| (p, false))) {
        set_ace(p, group_sid, GRANT_ACCESS, if w { MODIFY_MASK } else { READ_MASK })?;
        record_runtime_grant(p, true);
    }
    Ok(())
}

/// Replace the DACL of `path` with a protected one (no inherited ACEs): SYSTEM and
/// Administrators full control, Users and the sandbox group read + execute (protected DACL because
/// %ProgramData% lets Users create files in its subdirectories). Children inherit it.
fn lock(path: &Path, group_sid: &str) -> Result<(), String> {
    const FULL: u32 = 0x001F_01FF;
    let inherit = if path.is_dir() { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE };
    let entries = [("S-1-5-18", FULL), ("S-1-5-32-544", FULL), ("S-1-5-32-545", READ_MASK), (group_sid, READ_MASK)];
    let wpath = wide(path);
    unsafe {
        let mut sids: Vec<*mut c_void> = vec![];
        let free = |sids: &[*mut c_void]| {
            for s in sids {
                LocalFree(*s as HLOCAL);
            }
        };
        let mut eas = vec![];
        for (sid, mask) in entries {
            let mut psid: *mut c_void = null_mut();
            if ConvertStringSidToSidW(wide(sid).as_ptr(), &mut psid) == 0 {
                free(&sids);
                return Err(last(&format!("ConvertStringSidToSidW({sid})")));
            }
            sids.push(psid);
            eas.push(EXPLICIT_ACCESS_W {
                grfAccessPermissions: mask,
                grfAccessMode: GRANT_ACCESS,
                grfInheritance: inherit,
                Trustee: TRUSTEE_W {
                    pMultipleTrustee: null_mut(),
                    MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                    TrusteeForm: TRUSTEE_IS_SID,
                    TrusteeType: TRUSTEE_IS_UNKNOWN,
                    ptstrName: psid.cast(),
                },
            });
        }
        let mut dacl: *mut ACL = null_mut();
        let rc = SetEntriesInAclW(eas.len() as u32, eas.as_ptr(), null(), &mut dacl);
        free(&sids);
        if rc != 0 {
            return Err(format!("SetEntriesInAclW(lock {}) failed: {rc}", path.display()));
        }
        let rc = SetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            dacl,
            null(),
        );
        LocalFree(dacl as HLOCAL);
        if rc != 0 {
            return Err(format!("locking {}: SetNamedSecurityInfoW failed: {rc}", path.display()));
        }
    }
    Ok(())
}

/// The DACL of `path` as (protected, [(ACE type, mask, SID)]) for [`super::lock_violations`].
fn dacl_entries(path: &Path) -> Result<(bool, Vec<super::Ace>), String> {
    let wpath = wide(path);
    unsafe {
        let (mut dacl, mut sd): (*mut ACL, *mut c_void) = (null_mut(), null_mut());
        let rc = GetNamedSecurityInfoW(
            wpath.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(format!("reading the DACL of {}: GetNamedSecurityInfoW failed: {rc}", path.display()));
        }
        let (mut control, mut rev) = (0u16, 0u32);
        let protected =
            GetSecurityDescriptorControl(sd, &mut control, &mut rev) != 0 && control & SE_DACL_PROTECTED != 0;
        let mut aces = vec![];
        if dacl.is_null() {
            // a NULL DACL grants everyone everything
            aces.push((0, 0x1000_0000, "S-1-1-0 (NULL DACL)".to_string()));
        } else {
            let base = dacl as *const u8;
            let size = std::ptr::read_unaligned(base.add(2) as *const u16) as usize;
            let count = std::ptr::read_unaligned(base.add(4) as *const u16) as usize;
            let mut off = 8;
            for _ in 0..count {
                if off + 8 > size {
                    break;
                }
                let t = *base.add(off);
                let ace_size = std::ptr::read_unaligned(base.add(off + 2) as *const u16) as usize;
                if ace_size < 8 || off + ace_size > size {
                    break;
                }
                let mask = std::ptr::read_unaligned(base.add(off + 4) as *const u32);
                let sid = match t {
                    // ACCESS_ALLOWED / ACCESS_DENIED: the SID follows the mask
                    0 | 1 => {
                        let mut s: *mut u16 = null_mut();
                        if ConvertSidToStringSidW(base.add(off + 8) as *mut c_void, &mut s) == 0 {
                            "unreadable SID".to_string()
                        } else {
                            let mut len = 0;
                            while *s.add(len) != 0 {
                                len += 1;
                            }
                            let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, len));
                            LocalFree(s as HLOCAL);
                            out
                        }
                    }
                    // object / callback allow ACEs: a layout we don't parse — treat as a writer
                    5 | 9 | 11 => format!("ACE type {t}"),
                    _ => {
                        off += ace_size;
                        continue;
                    }
                };
                let t = if matches!(t, 5 | 9 | 11) { 0 } else { t };
                aces.push((t, mask, sid));
                off += ace_size;
            }
        }
        LocalFree(sd as HLOCAL);
        Ok((protected, aces))
    }
}

/// `mare doctor`: the sandbox directory, `bin` and the helper must not be writable by
/// anyone but SYSTEM and Administrators — the precondition for adding the sandbox account's SID to
/// the restricting SIDs. `Err` names every problem.
pub fn check_lock(state: &SetupState) -> Result<(), String> {
    let dir = sandbox_dir();
    let mut problems = vec![];
    for p in [dir.clone(), dir.join("bin"), state.helper.clone()] {
        match dacl_entries(&p) {
            Ok((protected, aces)) => {
                for v in super::lock_violations(protected, &aces) {
                    problems.push(format!("{}: {v}", p.display()));
                }
            }
            Err(e) => problems.push(e),
        }
    }
    if problems.is_empty() { Ok(()) } else { Err(problems.join("; ")) }
}

// ---- setup / remove ----------------------------------------------------------------------------

pub struct SetupOpts {
    pub deny_read: Vec<PathBuf>,
    /// the user-configured part of `deny_read` (the rest are built-in defaults)
    pub deny_read_user: Vec<PathBuf>,
    /// invoking user's host-protected directories (resolved before UAC elevation)
    pub host_protected: Vec<PathBuf>,
}

/// `mare sandbox setup` (must run elevated). Idempotent: re-running refreshes passwords, the
/// helper copy, ACEs and filters.
pub fn setup(opts: &SetupOpts, log: &mut dyn FnMut(&str)) -> Result<SetupState, String> {
    if !is_elevated() {
        return Err("`mare sandbox setup` needs an elevated (administrator) process".into());
    }
    let dir = sandbox_dir();
    std::fs::create_dir_all(dir.join("bin")).map_err(|e| format!("{}: {e}", dir.display()))?;
    ensure_group()?;
    let group_sid = account_sid(GROUP)?;
    let mut make = |name: &str| -> Result<Account, String> {
        let pw = random_password();
        ensure_user(name, &pw)?;
        log(&format!("account {name} ready"));
        Ok(Account { name: name.into(), sid: account_sid(name)?, password: seal_password(&pw)? })
    };
    let offline = make(USER_OFFLINE)?;
    let online = make(USER_ONLINE)?;

    // the accounts can't read the user's profile: run a machine-wide copy of mare
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let helper = dir.join("bin").join(format!("mare-sandbox{}", std::env::consts::EXE_SUFFIX));
    // copy beside, then swap in (a running helper can't be overwritten in place)
    let tmp = helper.with_extension("new");
    std::fs::copy(&exe, &tmp).map_err(|e| format!("copying {}: {e}", exe.display()))?;
    mare_shared::paths::rename_retry(&tmp, &helper).map_err(|e| format!("installing {}: {e}", helper.display()))?;
    let helper_sha256 = sha256_file(&helper).ok_or("hashing the helper failed")?;
    // nothing here may be writable by the sandbox accounts (their SID is a restricting SID):
    // a fresh, protected DACL on the directory, `bin` and the helper (protected because
    // %ProgramData% lets Users create files in its subdirectories)
    for p in [dir.clone(), dir.join("bin"), helper.clone()] {
        lock(&p, &group_sid)?;
    }
    log("sandbox directory locked (SYSTEM and Administrators write, everyone else read)");

    // canary the self-test must fail to read; the setup file itself is off limits too
    let canary_dir = dir.join("canary");
    std::fs::create_dir_all(&canary_dir).map_err(|e| e.to_string())?;
    let canary = canary_dir.join("secret.txt");
    std::fs::write(&canary, b"mare sandbox canary").map_err(|e| e.to_string())?;
    set_ace(&canary_dir, &group_sid, DENY_ACCESS, READ_MASK)?;

    let protected_dirs: Vec<PathBuf> = if opts.host_protected.is_empty() {
        crate::ossandbox::protected::host_protected()
            .into_iter()
            .filter(|e| e.shape == crate::ossandbox::protected::HostShape::Dir)
            .map(|e| e.path)
            .collect()
    } else {
        opts.host_protected.clone()
    };
    let mut all_deny: Vec<PathBuf> = Vec::new();
    let mut seen_deny = std::collections::HashSet::new();
    for p in opts.deny_read.iter().chain(protected_dirs.iter()) {
        if seen_deny.insert(super::path_key(p)) {
            all_deny.push(p.clone());
        }
    }
    let mut denied = vec![];
    for p in &all_deny {
        if protected_dirs.iter().any(|d| super::path_key(d) == super::path_key(p))
            && !p.exists()
            && let Err(e) = mare_shared::private::create_dir_all(p)
        {
            log(&format!("deny-read create {}: {e}", p.display()));
            continue;
        }
        if !p.exists() {
            continue;
        }
        match set_ace(p, &group_sid, DENY_ACCESS, READ_MASK) {
            Ok(()) => denied.push(p.clone()),
            Err(e) => log(&format!("deny-read {}: {e}", p.display())),
        }
    }
    log(&format!("deny-read ACEs on {} path(s)", denied.len()));

    let wfp_filters = match install_wfp(USER_OFFLINE) {
        Ok(n) => n,
        Err(e) => {
            // reported, never hidden: the gate refuses "full" without filters
            log(&format!("WFP filters could not be installed: {e}"));
            0
        }
    };
    log(&format!("WFP filters installed: {wfp_filters}"));

    let state = SetupState {
        version: SETUP_VERSION,
        offline,
        online,
        group_sid: group_sid.clone(),
        helper,
        helper_sha256,
        deny_read_user: Some({
            let user: std::collections::HashSet<String> =
                opts.deny_read_user.iter().map(|p| super::path_key(p)).collect();
            denied.iter().filter(|p| user.contains(&super::path_key(p))).cloned().collect()
        }),
        deny_read: denied,
        canary,
        proxy_range: PROXY_PORT_RANGE,
        wfp_filters,
    };
    state.save(&dir)?;
    // the accounts must not read the setup file (password blobs)
    set_ace(&super::setup_file(&dir), &group_sid, DENY_ACCESS, READ_MASK)?;
    Ok(state)
}

/// `mare sandbox setup --remove`: filters, ACEs, accounts, group, files.
pub fn remove(log: &mut dyn FnMut(&str)) -> Result<(), String> {
    if !is_elevated() {
        return Err("`mare sandbox setup --remove` needs an elevated (administrator) process".into());
    }
    let dir = sandbox_dir();
    let state = SetupState::load(&dir).unwrap_or(None);
    if let Err(e) = remove_wfp() {
        log(&format!("WFP: {e}"));
    }
    if let Some(s) = &state {
        let runtime_paths: Vec<PathBuf> = std::fs::read_to_string(runtime_grants_file())
            .ok()
            .and_then(|txt| serde_json::from_str(&txt).ok())
            .unwrap_or_default();
        for p in s.deny_read.iter().chain([&s.canary]).chain(runtime_paths.iter()) {
            if p.exists()
                && let Err(e) = set_ace(p, &s.group_sid, REVOKE_ACCESS, 0)
            {
                log(&format!("revoking {}: {e}", p.display()));
            }
        }
        let _ = std::fs::remove_file(runtime_grants_file());
    }
    for u in [USER_OFFLINE, USER_ONLINE] {
        let n = wide(u);
        let rc = unsafe { NetUserDel(null(), n.as_ptr()) };
        log(&format!(
            "account {u}: {}",
            if rc == NERR_Success { "removed".into() } else { format!("NetUserDel {rc}") }
        ));
    }
    let g = wide(GROUP);
    unsafe { NetLocalGroupDel(null(), g.as_ptr()) };
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(super::selftest_workspace(&dir));
    Ok(())
}

// ---- run as a sandbox account ------------------------------------------------------------------

/// A kill-on-close job: the account's process (created by the secondary logon service, outside
/// our job) dies with the runner.
fn kill_on_close_job() -> Result<HANDLE, String> {
    unsafe {
        let job = CreateJobObjectW(null(), null());
        if job.is_null() {
            return Err(last("CreateJobObjectW"));
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            CloseHandle(job);
            return Err(last("SetInformationJobObject"));
        }
        Ok(job)
    }
}

fn check_environment() -> Result<(), String> {
    let station = crate::winsandbox::desktop::process_station_name()
        .map_err(|e| format!("cannot determine enhanced helper parent window station: {e}"))?;
    super::check_enhanced_station(&station)
}

/// Start `helper args…` as `acct` with the given stdio, wait, return the exit code.
fn run_as(acct: &Account, helper: &Path, args: &[String], cwd: &Path, stdio: [HANDLE; 3]) -> Result<i32, String> {
    check_environment()?;
    let password = open_password(&acct.password)?;
    let user = wide(&acct.name);
    let domain = wide(".");
    let pw = wide(&password);
    let app = wide(helper);
    let request_args = args
        .strip_prefix(&[crate::winsandbox::RUNNER_ARG.to_string()])
        .ok_or("account runner request is missing runner marker")?;
    // Validate before creating a different-account process; payload stays out of logon argv.
    let request = RunnerArgs::parse(request_args)?;
    if request.account.as_deref() != Some(acct.sid.as_str()) || request.as_user.is_some() {
        return Err("account runner request identity mismatch".into());
    }
    // No executable request may be delivered before all outer endpoints are owned/valid.
    let stdio = ipc::prepare_stdio(stdio)?;
    let transport = ipc::Server::new(&acct.sid)?;
    let mut argv = vec![helper.to_string_lossy().into_owned()];
    argv.extend(transport.args());
    let mut cmdline = wide(argv_to_command_line(&argv));
    let wcwd = wide(cwd);
    unsafe {
        let job = kill_on_close_job()?;
        let mut si: STARTUPINFOW = std::mem::zeroed();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        let previous_error_mode = windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode(0x0001 | 0x0002);
        let ok = CreateProcessWithLogonW(
            user.as_ptr(),
            domain.as_ptr(),
            pw.as_ptr(),
            0,
            app.as_ptr(),
            cmdline.as_mut_ptr(),
            CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            null(),
            wcwd.as_ptr(),
            &si,
            &mut pi,
        );
        // Save the launch error before restoring process-wide error mode.
        let launch_error = GetLastError();
        windows_sys::Win32::System::Diagnostics::Debug::SetErrorMode(previous_error_mode);
        if ok == 0 {
            windows_sys::Win32::Foundation::SetLastError(launch_error);
            let e = last(&format!("starting the sandbox as {} (CreateProcessWithLogonW)", acct.name));
            CloseHandle(job);
            return Err(e);
        }
        // CREATE_SUSPENDED prevents the account runner from executing before assignment;
        // unlike resume-then-assign there is no unassigned execution window.
        if AssignProcessToJobObject(job, pi.hProcess) == 0 {
            let e = last("AssignProcessToJobObject");
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            CloseHandle(job);
            return Err(e);
        }
        if ResumeThread(pi.hThread) == u32::MAX {
            let error = last("ResumeThread(sandbox runner)");
            TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            CloseHandle(job);
            return Err(error);
        }
        CloseHandle(pi.hThread);
        let relay = transport.relay(pi.dwProcessId, pi.hProcess, request_args, stdio);
        if relay.is_err() {
            TerminateProcess(pi.hProcess, 1);
        }
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        CloseHandle(job);
        relay.map(|_| code as i32).map_err(|error| format!("{error} (account runner exit {code:#010x})"))
    }
}

/// Outer runner (`__sandbox-exec --as offline|online …`, the real user): run the inner runner —
/// the same arguments without `--as` — as the sandbox account on this process's stdio.
pub fn run_outer(a: &RunnerArgs, which: &str) -> Result<i32, String> {
    let state = SetupState::load(&sandbox_dir())?.ok_or("the enhanced sandbox is not set up")?;
    let acct = state.account(which == "online");
    // the inner runner restricts to the account's own SID too after
    // checking it really runs as that account
    let inner = RunnerArgs { as_user: None, account: Some(acct.sid.clone()), ..a.clone() };
    let mut args = vec![crate::winsandbox::RUNNER_ARG.to_string()];
    args.extend(inner.to_args());
    let stdio = unsafe { [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|k| GetStdHandle(k)) };
    run_as(acct, &state.helper, &args, &a.cwd, stdio)
}

// ---- self-test ---------------------------------------------------------------------------------

pub struct RealTester;

impl SelfTester for RealTester {
    fn check_environment(&self) -> Result<(), String> {
        check_environment()
    }

    fn setup(&self) -> Result<Option<SetupState>, String> {
        SetupState::load(&sandbox_dir())
    }

    fn current_exe_sha256(&self) -> Option<String> {
        sha256_file(&std::env::current_exe().ok()?)
    }

    fn run(&self, s: &SetupState) -> Result<SelfTest, String> {
        use std::io::Read;
        use std::net::TcpListener;
        // a scratch workspace the test may write to (granted like a real root), outside the
        // locked sandbox directory
        let ws = super::selftest_workspace(&sandbox_dir());
        std::fs::create_dir_all(&ws).map_err(|e| e.to_string())?;
        let temp = crate::winsandbox::session_temp_dir()?;
        let sids = crate::winsandbox::prepare_grants(std::slice::from_ref(&ws), &temp)?;
        let _grant_guard = grant_group_scoped(&s.group_sid, &[ws.clone(), temp.clone()], &[], &[])?;
        // a place only the real user may write
        let outside_dir = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("mare")
            .join("selftest-outside");
        std::fs::create_dir_all(&outside_dir).map_err(|e| e.to_string())?;
        let outside = outside_dir.join("probe.txt");
        // listeners: one inside the proxy range (must be reachable), one outside (must not)
        let inside = (PROXY_PORT_RANGE.0..=PROXY_PORT_RANGE.1)
            .find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok())
            .ok_or("no free port in the proxy range")?;
        let out_l = std::iter::repeat_with(|| TcpListener::bind("127.0.0.1:0"))
            .take(20)
            .filter_map(Result::ok)
            .find(|l| l.local_addr().is_ok_and(|a| !super::in_proxy_range(a.port())))
            .ok_or("no free port outside the proxy range")?;
        let (inside_port, outside_port) = (inside.local_addr().unwrap().port(), out_l.local_addr().unwrap().port());

        let test_argv = vec![
            s.helper.to_string_lossy().into_owned(),
            SELFTEST_ARG.to_string(),
            s.canary.to_string_lossy().into_owned(),
            outside.to_string_lossy().into_owned(),
            outside_port.to_string(),
            inside_port.to_string(),
            sandbox_dir().to_string_lossy().into_owned(),
            s.helper.to_string_lossy().into_owned(),
        ];
        // exactly the chain commands get: the account's SID among the restricting SIDs
        let inner = RunnerArgs {
            mode: "workspace-write".into(),
            sids,
            cwd: ws.clone(),
            argv: test_argv,
            as_user: None,
            account: Some(s.offline.sid.clone()),
            pty: None,
        };
        let mut args = vec![crate::winsandbox::RUNNER_ARG.to_string()];
        args.extend(inner.to_args());

        // stdout through an inheritable pipe
        let (mut rd, mut wr): (HANDLE, HANDLE) = (null_mut(), null_mut());
        unsafe {
            let sa = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: null_mut(),
                bInheritHandle: 1,
            };
            if CreatePipe(&mut rd, &mut wr, &sa, 0) == 0 {
                return Err(last("CreatePipe"));
            }
            SetHandleInformation(rd, HANDLE_FLAG_INHERIT, 0);
        }
        // Stdin-closed mode: self-test must never read host/TUI input.
        use std::os::windows::io::AsRawHandle;
        let stdin = ipc::eof_input()?;
        let code = run_as(&s.offline, &s.helper, &args, &ws, [stdin.as_raw_handle().cast(), wr, wr]);
        unsafe { CloseHandle(wr) };
        let mut out = String::new();
        {
            use std::os::windows::io::FromRawHandle;
            let mut f = unsafe { std::fs::File::from_raw_handle(rd as _) };
            let _ = f.read_to_string(&mut out);
        }
        drop((inside, out_l));
        let code = code?;
        let line = out.lines().rev().find(|l| l.trim_start().starts_with('{')).ok_or_else(|| {
            format!("self-test produced no result (exit {code}): {}", out.trim().chars().take(300).collect::<String>())
        })?;
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("self-test output: {e}"))?;
        SelfTest::from_json(&v)
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    #[test]
    fn invalid_outer_streams_never_execute_an_account_payload() {
        if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
            return;
        }
        if let Err(reason) = check_environment() {
            assert!(
                std::env::var("MARE_REQUIRE_ENHANCED_SANDBOX").as_deref() != Ok("1"),
                "required interactive account payload regression cannot run: {reason}"
            );
            eprintln!("not executed: account payload regression requires WinSta0 ({reason})");
            return;
        }
        let Some(state) = SetupState::load(&sandbox_dir()).unwrap() else {
            assert!(
                std::env::var("MARE_REQUIRE_ENHANCED_SANDBOX").as_deref() != Ok("1"),
                "run formal sandbox setup before enhanced regression"
            );
            eprintln!("not executed: formal sandbox setup not run");
            return;
        };
        let workspace_guard = tempfile::tempdir().unwrap();
        let workspace = workspace_guard.path().to_path_buf();
        let marker = workspace.join("must-not-run.txt");
        // Make the baseline executable: both normal account access and capability access
        // must allow this owned workspace, or an absent marker would be a false positive.
        let sids = crate::winsandbox::prepare_root_grants(std::slice::from_ref(&workspace)).unwrap();
        grant_group(&state.group_sid, std::slice::from_ref(&workspace), &[]).unwrap();
        let args = RunnerArgs {
            mode: "workspace-write".into(),
            sids,
            cwd: workspace.clone(),
            argv: vec!["cmd.exe".into(), "/c".into(), "echo forbidden>must-not-run.txt".into()],
            as_user: None,
            account: Some(state.offline.sid.clone()),
            pty: None,
        };
        let mut request = vec![crate::winsandbox::RUNNER_ARG.into()];
        request.extend(args.to_args());
        let input = ipc::eof_input().unwrap();
        let output = std::fs::OpenOptions::new().write(true).open("NUL").unwrap();
        let valid = [input.as_raw_handle().cast(), output.as_raw_handle().cast(), output.as_raw_handle().cast()];
        assert_eq!(run_as(&state.offline, &state.helper, &request, &workspace, valid).unwrap(), 0);
        assert!(marker.exists(), "valid stdio baseline must execute the account payload");
        std::fs::remove_file(&marker).unwrap();
        let event = unsafe { windows_sys::Win32::System::Threading::CreateEventW(null(), 1, 0, null()) };
        assert!(!event.is_null());
        for index in 0..3 {
            for invalid in [null_mut(), windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE, event] {
                let mut stdio = valid;
                stdio[index] = invalid;
                let error = run_as(&state.offline, &state.helper, &request, &workspace, stdio).unwrap_err();
                assert!(error.contains("stdio"), "{error}");
                assert!(!marker.exists(), "an account payload ran before stdio validation");
            }
        }
        unsafe { CloseHandle(event) };
        workspace_guard.close().expect("remove owned invalid-stdio workspace");
    }
}
