//! Copyright 2025 OpenAI (Apache-2.0); modified for mare UTF-16 IPC, capability NUL grants, and console inheritance.
//! Default tier, Win32 side: restricted token, ACL grants, the runner and its ConPTY.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, GetLastError, HANDLE, HANDLE_FLAG_INHERIT, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
    SetHandleInformation,
};
use windows_sys::Win32::Security::Authorization::ConvertStringSidToSidW;
use windows_sys::Win32::Security::{CreateWellKnownSid, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, WELL_KNOWN_SID_TYPE};
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
    GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, INFINITE, InitializeProcThreadAttributeList, OpenProcess,
    OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::desktop::LaunchDesktop;
use super::{RunnerArgs, argv_to_command_line};

#[path = "default_acl_win.rs"]
mod acl;
#[path = "default_pty_win.rs"]
mod pty;
#[path = "default_token_win.rs"]
mod token;

#[cfg(test)]
#[path = "default_tests_win.rs"]
mod tests;

pub use acl::{grant_write, has_standing_grant};
pub(crate) use acl::{verify_dir_inheritance, with_path_acl_lock};
pub use token::probe;

use acl::allow_null_device;
use pty::run_pty;
use token::{logon_sid, restricted_token, user_sid};

pub(super) fn wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

pub(super) fn last(what: &str) -> String {
    format!("{what} failed: {}", std::io::Error::from_raw_os_error(unsafe { GetLastError() } as i32))
}

pub fn pid_alive(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            // a process we may not open (another user's, protected) exists all the same
            return GetLastError() == 5; // ERROR_ACCESS_DENIED
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(h, &mut code) != 0;
        CloseHandle(h);
        ok && code == 259 // STILL_ACTIVE
    }
}

pub fn pid_start_time(pid: u32) -> Option<u64> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut creation = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut exit = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut kernel = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let mut user = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let ok = GetProcessTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) != 0;
        CloseHandle(h);
        ok.then_some(((creation.dwHighDateTime as u64) << 32) | (creation.dwLowDateTime as u64))
    }
}

/// A SID owned by this process (`ConvertStringSidToSidW` → `LocalFree`).
pub(super) struct LocalSid(pub(super) *mut c_void);
impl LocalSid {
    pub(super) fn parse(s: &str) -> Result<Self, String> {
        let mut p: *mut c_void = std::ptr::null_mut();
        if unsafe { ConvertStringSidToSidW(wide(s).as_ptr(), &mut p) } == 0 {
            return Err(last(&format!("ConvertStringSidToSidW({s})")));
        }
        Ok(Self(p))
    }
}
impl Drop for LocalSid {
    fn drop(&mut self) {
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

/// A SID in a byte buffer (well-known SIDs, a copied logon SID).
pub(super) struct BufSid(pub(super) Vec<u8>);
impl BufSid {
    pub(super) fn well_known(kind: WELL_KNOWN_SID_TYPE) -> Result<Self, String> {
        let mut buf = vec![0u8; SECURITY_MAX_SID_SIZE as usize];
        let mut size = SECURITY_MAX_SID_SIZE;
        if unsafe { CreateWellKnownSid(kind, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) } == 0 {
            return Err(last("CreateWellKnownSid"));
        }
        buf.truncate(size as usize);
        Ok(Self(buf))
    }
    pub(super) fn ptr(&self) -> *mut c_void {
        self.0.as_ptr() as *mut c_void
    }
}

/// The string form of a SID (`S-1-…`, for SDDL).
pub(super) fn sid_string(sid: *mut c_void) -> Result<String, String> {
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    unsafe {
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(sid, &mut s) == 0 {
            return Err(last("ConvertSidToStringSidW"));
        }
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        let out = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
        LocalFree(s as HLOCAL);
        Ok(out)
    }
}

/// The user SID and the logon SID of this process's token (the confined token keeps both).
pub(super) fn current_sids() -> Result<(BufSid, BufSid), String> {
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(last("OpenProcessToken"));
        }
        let sids = user_sid(token).and_then(|u| Ok((u, logon_sid(token)?)));
        CloseHandle(token);
        sids
    }
}

/// The desktop owner lives until the confined process exits, in both pipe and PTY modes.
pub(super) fn launch_desktop(account: Option<&str>) -> Result<(Option<LaunchDesktop>, Vec<u16>), String> {
    let (user, logon) = current_sids()?;
    let d = if account.is_some() {
        LaunchDesktop::enhanced(logon.ptr())?
    } else {
        LaunchDesktop::private(user.ptr(), logon.ptr())?
    };
    let name = d.startup_name();
    Ok((Some(d), name))
}

/// Serialize the runner environment as UTF-16, preserving non-ASCII values and
/// private TEMP/proxy settings. CreateProcessAsUserW requires the Unicode flag.
fn environment_block(entries: &[(Vec<u16>, Vec<u16>)]) -> Vec<u16> {
    let mut entries: Vec<_> = entries.iter().collect();
    entries.sort_by_cached_key(|(key, _)| String::from_utf16_lossy(key).to_uppercase());
    let mut block = Vec::new();
    for (key, value) in entries {
        block.extend_from_slice(key);
        block.push(b'=' as u16);
        block.extend_from_slice(value);
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    block
}

/// Owns the attribute storage through process creation. Missing or unusable stdio is
/// fatal: TRUE inheritance must always be paired with an explicit handle allowlist.
struct PipeStdio {
    storage: Vec<usize>,
    _inherited: Vec<HANDLE>,
}

impl PipeStdio {
    fn prepare(si: &mut STARTUPINFOEXW, stdio: [HANDLE; 3]) -> Result<Self, String> {
        if stdio.iter().any(|h| h.is_null() || *h == INVALID_HANDLE_VALUE) {
            return Err("pipe launch requires valid stdin, stdout, and stderr handles".into());
        }
        unsafe {
            for h in stdio {
                if SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) == 0 {
                    return Err(last("SetHandleInformation(stdio)"));
                }
            }
            si.StartupInfo.dwFlags |= STARTF_USESTDHANDLES;
            si.StartupInfo.hStdInput = stdio[0];
            si.StartupInfo.hStdOutput = stdio[1];
            si.StartupInfo.hStdError = stdio[2];
            let mut attribute_bytes = 0usize;
            InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attribute_bytes);
            let mut attribute_storage = vec![0usize; attribute_bytes.div_ceil(std::mem::size_of::<usize>())];
            let attributes = attribute_storage.as_mut_ptr().cast();
            if InitializeProcThreadAttributeList(attributes, 1, 0, &mut attribute_bytes) == 0 {
                return Err(last("InitializeProcThreadAttributeList(stdio)"));
            }
            let mut inherited = stdio.to_vec();
            inherited.sort_unstable();
            inherited.dedup();
            if UpdateProcThreadAttribute(
                attributes,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                inherited.as_mut_ptr().cast(),
                inherited.len() * std::mem::size_of::<HANDLE>(),
                std::ptr::null_mut(),
                std::ptr::null(),
            ) == 0
            {
                let error = last("UpdateProcThreadAttribute(HANDLE_LIST)");
                DeleteProcThreadAttributeList(attributes);
                return Err(error);
            }
            si.lpAttributeList = attributes;
            Ok(Self { storage: attribute_storage, _inherited: inherited })
        }
    }
}

impl Drop for PipeStdio {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.storage.as_mut_ptr().cast()) };
    }
}

/// The runner: start `a.argv` under the confined token on this process's stdio, wait, return
/// its exit code. With `--pty` the command gets a ConPTY instead ([`run_pty`]).
pub fn run(a: &RunnerArgs) -> Result<i32, String> {
    let environment: Vec<_> =
        std::env::vars_os().map(|(key, value)| (key.encode_wide().collect(), value.encode_wide().collect())).collect();
    run_with_env(a, &environment)
}

/// Launch with the IPC request's exact UTF-16 environment, including hidden drive
/// entries and unpaired surrogates. No process-global environment changes are needed.
pub(super) fn run_with_env(a: &RunnerArgs, env: &[(Vec<u16>, Vec<u16>)]) -> Result<i32, String> {
    run_with_env_ready(a, env, &mut || Ok(()))
}

pub(super) fn run_with_env_ready(
    a: &RunnerArgs,
    env: &[(Vec<u16>, Vec<u16>)],
    ready: &mut dyn FnMut() -> Result<(), String>,
) -> Result<i32, String> {
    if env.iter().any(|(key, value)| key.is_empty() || key.contains(&0) || value.contains(&0)) {
        return Err("sandbox environment contains an empty key or embedded NUL".into());
    }
    let caps: Vec<LocalSid> = a.sids.iter().map(|s| LocalSid::parse(s)).collect::<Result<_, _>>()?;
    let _null_guard = allow_null_device(&caps, &a.sids);
    let environment = environment_block(env);
    if let Some(size) = a.pty {
        return run_pty(a, &caps, size, &environment, ready);
    }
    unsafe {
        // Ctrl+C belongs to the confined child; the runner stays to mirror its exit code
        SetConsoleCtrlHandler(None, 1);
        let token = restricted_token(&caps, a.account.as_deref())?;
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        // PowerShell fails with STATUS_DLL_INIT_FAILED under a restricted token unless the
        // desktop is named explicitly
        // a desktop the confined token may use: without one, user32 fails to initialise and
        // the command dies with STATUS_DLL_INIT_FAILED (PowerShell; every such command over SSH,
        // where the runner's window station is not WinSta0)
        let (_desktop, mut desktop) = match launch_desktop(a.account.as_deref()) {
            Ok(desktop) => desktop,
            Err(error) => {
                CloseHandle(token);
                return Err(error);
            }
        };
        si.StartupInfo.lpDesktop = desktop.as_mut_ptr();
        let stdio = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE].map(|k| GetStdHandle(k));
        let pipe_stdio = match PipeStdio::prepare(&mut si, stdio) {
            Ok(stdio) => stdio,
            Err(error) => {
                CloseHandle(token);
                return Err(error);
            }
        };
        let mut cmdline = wide(argv_to_command_line(&a.argv));
        let cwd = wide(&a.cwd);
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        // Inherit console mode for ordinary pipe launches.
        let ok = CreateProcessAsUserW(
            token,
            std::ptr::null(),
            cmdline.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            cwd.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        );
        let launch_error =
            (ok == 0).then(|| last(&format!("starting {:?} confined (CreateProcessAsUserW)", a.argv[0])));
        drop(pipe_stdio);
        CloseHandle(token);
        if let Some(error) = launch_error {
            return Err(error);
        }
        if let Err(error) = ready() {
            windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            return Err(error);
        }
        CloseHandle(pi.hThread);
        WaitForSingleObject(pi.hProcess, INFINITE);
        let mut code = 1u32;
        GetExitCodeProcess(pi.hProcess, &mut code);
        CloseHandle(pi.hProcess);
        Ok(code as i32)
    }
}
