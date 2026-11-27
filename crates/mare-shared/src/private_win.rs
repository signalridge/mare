//! Windows side of [`super`]: private objects by DACL, and the check the tests (and `mare doctor`)
//! use to tell whether a file, directory or pipe is private to the current user.

use std::fs::File;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::Path;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW, ConvertSidToStringSidW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, ConvertStringSidToSidW, GetNamedSecurityInfoW,
    GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, GetTokenInformation,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CREATE_NEW, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

/// A path for the `W` APIs: a long absolute one in its verbatim form (`\\?\C:\…`,
/// `\\?\UNC\server\…`) so `MAX_PATH` doesn't apply — what std does for its own calls (a session
/// directory's name is derived from the workspace path and can be long).
fn wide_path(p: &Path) -> Vec<u16> {
    let s = p.as_os_str().to_string_lossy();
    if s.len() < 240 || s.starts_with(r"\\?\") || !p.is_absolute() {
        return wide(p.as_os_str());
    }
    let s = s.replace('/', "\\");
    let v = match s.strip_prefix(r"\\") {
        Some(unc) => format!(r"\\?\UNC\{unc}"),
        None => format!(r"\\?\{s}"),
    };
    wide(std::ffi::OsStr::new(&v))
}

/// A `LocalAlloc`ed buffer handed out by an API, freed on drop.
struct Local(*mut core::ffi::c_void);

impl Drop for Local {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { LocalFree(self.0) };
        }
    }
}

fn utf16_z(p: *const u16) -> String {
    let mut n = 0;
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// The current user's SID as a string (`S-1-5-21-…`), from the process token.
fn me() -> io::Result<&'static str> {
    static ME: OnceLock<String> = OnceLock::new();
    if let Some(s) = ME.get() {
        return Ok(s);
    }
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
        let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), needed, &mut needed);
        let err = io::Error::last_os_error();
        CloseHandle(token);
        if ok == 0 {
            return Err(err);
        }
        let user = &*(buf.as_ptr() as *const TOKEN_USER);
        let mut s: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut s) == 0 {
            return Err(io::Error::last_os_error());
        }
        let _free = Local(s.cast());
        Ok(ME.get_or_init(|| utf16_z(s)))
    }
}

/// The current user's SID (`S-1-5-21-…`), the token's user — the same whether or not the
/// process runs elevated (unlike its default owner, which elevated is Administrators).
pub fn user_sid() -> io::Result<&'static str> {
    me()
}

/// Immutable `LocalAlloc`ed security descriptor cached for the lifetime of the process.
struct CachedSd(PSECURITY_DESCRIPTOR);

// SAFETY: a self-relative security descriptor returned by
// `ConvertStringSecurityDescriptorToSecurityDescriptorW` is immutable after construction and is
// never mutated or freed for the lifetime of the process.
unsafe impl Send for CachedSd {}
unsafe impl Sync for CachedSd {}

/// The private DACL: full control for SYSTEM, Administrators and the current user, nothing
/// inherited from the parent (`P`); `OICI` on a directory so its contents get the same.
fn private_sd(dir: bool) -> io::Result<PSECURITY_DESCRIPTOR> {
    static DIR_SD: OnceLock<CachedSd> = OnceLock::new();
    static FILE_SD: OnceLock<CachedSd> = OnceLock::new();
    let slot = if dir { &DIR_SD } else { &FILE_SD };
    if let Some(cached) = slot.get() {
        return Ok(cached.0);
    }
    let f = if dir { "OICI" } else { "" };
    let built = sd_from(&format!("D:P(A;{f};FA;;;SY)(A;{f};FA;;;BA)(A;{f};FA;;;{})", me()?))?;
    // Transfer ownership to the static slot if we win the race; otherwise `built` frees on drop.
    let ptr = built.0;
    if slot.set(CachedSd(ptr)).is_ok() {
        std::mem::forget(built);
    }
    Ok(slot.get().expect("initialized security descriptor").0)
}

fn sd_from(sddl: &str) -> io::Result<Local> {
    let w = wide(std::ffi::OsStr::new(sddl));
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Local(sd))
}

fn attributes(sd: PSECURITY_DESCRIPTOR) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    }
}

pub(super) fn create_dir_all(dir: &Path) -> io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let sd = private_sd(true)?;
    let sa = attributes(sd);
    if unsafe { CreateDirectoryW(wide_path(dir).as_ptr(), &sa) } == 0 {
        let e = io::Error::last_os_error();
        // created meanwhile by someone else: like `create_dir_all`, that's fine
        if !(e.kind() == io::ErrorKind::AlreadyExists && dir.is_dir()) {
            return Err(e);
        }
    }
    Ok(())
}

pub(super) fn create_dir_new(dir: &Path) -> io::Result<()> {
    let sd = private_sd(true)?;
    let sa = attributes(sd);
    if unsafe { CreateDirectoryW(wide_path(dir).as_ptr(), &sa) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn create_new(path: &Path) -> io::Result<File> {
    let sd = private_sd(false)?;
    let sa = attributes(sd);
    let h = unsafe {
        CreateFileW(
            wide_path(path).as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            // as std's `OpenOptions`: others may open, rename or delete it (a replace by rename)
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            &sa,
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_handle(h as RawHandle) })
}

fn set_dacl(path: &Path, sd: PSECURITY_DESCRIPTOR) -> io::Result<()> {
    let (mut present, mut defaulted) = (0, 0);
    let mut dacl: *mut ACL = std::ptr::null_mut();
    if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let r = unsafe {
        SetNamedSecurityInfoW(
            wide_path(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if r != 0 { Err(io::Error::from_raw_os_error(r as i32)) } else { Ok(()) }
}

/// Make an existing file or directory private (the `chmod 0600` / `0700` of Unix): its DACL is
/// replaced by the private one (a directory's is inherited by its contents).
pub fn restrict(path: &Path) -> io::Result<()> {
    set_dacl(path, private_sd(path.is_dir())?)
}

/// `Ok` when the security descriptor's DACL grants access to nobody but the current user, SYSTEM,
/// Administrators or the object's owner (`OWNER RIGHTS`); otherwise the DACL (SDDL) in the error.
fn check_sd(sd: PSECURITY_DESCRIPTOR) -> Result<(), String> {
    let mut s: *mut u16 = std::ptr::null_mut();
    if unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            sd,
            SDDL_REVISION_1,
            DACL_SECURITY_INFORMATION,
            &mut s,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(format!("reading the DACL: {}", io::Error::last_os_error()));
    }
    let _free = Local(s.cast());
    let sddl = utf16_z(s);
    let me = me().map_err(|e| e.to_string())?;
    // no DACL at all, or a null one: open to everyone (an empty one admits nobody)
    let dacl = sddl.strip_prefix("D:").ok_or_else(|| format!("no DACL (open to everyone): {sddl:?}"))?;
    if dacl.contains("NO_ACCESS_CONTROL") {
        return Err(format!("null DACL (open to everyone): {sddl:?}"));
    }
    for ace in dacl.split('(').skip(1) {
        // type;flags;rights;object;inherited object;sid)
        let f: Vec<&str> = ace.trim_end_matches(')').split(';').collect();
        let (Some(kind), Some(_flags), Some(sid)) = (f.first(), f.get(1), f.get(5)) else {
            return Err(format!("unreadable ACE ({ace}) in {sddl:?}"));
        };
        // Only pure deny ACE types grant nothing; all allow (A, OA, XA, ZA, etc.) and unknown
        // ACE types (including IO inherit-only ACEs on private directories) must be checked.
        if matches!(*kind, "D" | "OD" | "XD" | "ZD") {
            continue;
        }
        if *sid == me || matches!(*sid, "SY" | "BA" | "OW") || canonical(sid).as_deref() == Some(me) {
            continue;
        }
        return Err(format!("grants {sid} access ({ace}): {sddl:?} (current user {me})"));
    }
    Ok(())
}

/// A SID in its `S-1-…` form (SDDL may write a well-known one as an alias, e.g. `LA`).
fn canonical(sid: &str) -> Option<String> {
    let w = wide(std::ffi::OsStr::new(sid));
    let mut p = std::ptr::null_mut();
    if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut p) } == 0 {
        return None;
    }
    let _free = Local(p);
    let mut s: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(p, &mut s) } == 0 {
        return None;
    }
    let _s = Local(s.cast());
    Some(utf16_z(s))
}

/// Is `path` (file or directory) private to the current user? `Err` names who else has access.
pub fn check(path: &Path) -> Result<(), String> {
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let r = unsafe {
        GetNamedSecurityInfoW(
            wide_path(path).as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(format!("{}: {}", path.display(), io::Error::from_raw_os_error(r as i32)));
    }
    let _free = Local(sd);
    check_sd(sd).map_err(|e| format!("{}: {e}", path.display()))
}

/// [`check`] for an open kernel object (a named pipe): the handle needs `READ_CONTROL`.
pub fn check_handle(h: RawHandle) -> Result<(), String> {
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let r = unsafe {
        GetSecurityInfo(
            h as HANDLE,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32).to_string());
    }
    let _free = Local(sd);
    check_sd(sd)
}

/// Is the open kernel object `h` (the client end of a named pipe) owned by the current user?
/// What a client checks before trusting a pipe server: pipe names are global, so another local
/// user could create one under a mare name, and only the current user (or an administrator with
/// SeRestorePrivilege) can make an object whose owner is this user's SID. The handle needs
/// `READ_CONTROL` (part of `GENERIC_READ`). `Err` names the owner.
pub fn check_owner_handle(h: RawHandle) -> Result<(), String> {
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let r = unsafe {
        GetSecurityInfo(
            h as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(format!("reading the owner: {}", io::Error::from_raw_os_error(r as i32)));
    }
    let _free = Local(sd);
    check_owner_sd(sd)
}

fn check_owner_sd(sd: PSECURITY_DESCRIPTOR) -> Result<(), String> {
    let me = me().map_err(|e| e.to_string())?;
    let mut owner: PSID = std::ptr::null_mut();
    let mut defaulted = 0;
    if unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) } == 0 {
        return Err(format!("reading the owner: {}", io::Error::last_os_error()));
    }
    if owner.is_null() {
        return Err("no owner".into());
    }
    let mut s: *mut u16 = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(owner, &mut s) } == 0 {
        return Err(format!("reading the owner: {}", io::Error::last_os_error()));
    }
    let _s = Local(s.cast());
    let owner = utf16_z(s);
    if owner == me { Ok(()) } else { Err(format!("owned by {owner}, not the current user {me}")) }
}

/// Test setup only.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod testing {
    /// Give Everyone inheritable read access to `dir` (a test's temp directory): under the user
    /// profile everything is private by inheritance already, so a privacy assertion only means
    /// something below a directory open to others — the Windows counterpart of a 022 umask.
    pub fn open_to_everyone(dir: &std::path::Path) {
        let me = super::me().unwrap();
        let sd =
            super::sd_from(&format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{me})(A;OICI;FR;;;WD)")).unwrap();
        super::set_dacl(dir, sd.0).unwrap();
        assert!(super::check(dir).is_err(), "{} should be open to everyone now", dir.display());
    }

    /// An empty protected DACL: nobody may read `path`, not even its owner (who can still change
    /// the DACL, and delete it through the parent directory) — the counterpart of `chmod 000`.
    pub fn deny_all(path: &std::path::Path) {
        let sd = super::sd_from("D:P").unwrap();
        super::set_dacl(path, sd.0).unwrap();
    }
}

#[cfg(test)]
mod tests {
    /// The owner check on crafted security descriptors: another user's or a group's (what an
    /// elevated process's objects default to) ownership is refused, the user's own accepted.
    #[test]
    fn only_an_object_owned_by_the_current_user_passes() {
        let me = super::me().unwrap();
        let mine = super::sd_from(&format!("O:{me}D:P(A;;GA;;;{me})")).unwrap();
        assert_eq!(super::check_owner_sd(mine.0), Ok(()));
        for other in ["SY", "BA", "S-1-5-21-1-2-3-1001"] {
            let sd = super::sd_from(&format!("O:{other}D:P(A;;GA;;;{me})")).unwrap();
            let err = super::check_owner_sd(sd.0).unwrap_err();
            assert!(err.contains("not the current user"), "{other}: {err}");
        }
        let no_owner = super::sd_from(&format!("D:P(A;;GA;;;{me})")).unwrap();
        assert!(super::check_owner_sd(no_owner.0).is_err());
    }

    #[test]
    fn check_sd_rejects_callback_object_and_inherit_only_allow_aces() {
        let me = super::me().unwrap();
        let ok_sd =
            super::sd_from(&format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{me})(D;;FA;;;WD)")).unwrap();
        assert_eq!(super::check_sd(ok_sd.0), Ok(()));

        // Inherit-only allow ACE on a private directory grants access to children created inside it.
        let io_sd = super::sd_from(&format!("D:P(A;OICI;FA;;;{me})(A;OICIIO;FR;;;WD)")).unwrap();
        assert!(super::check_sd(io_sd.0).is_err());

        // Callback allow ACE (XA) must also be checked.
        let xa_sd = super::sd_from(&format!("D:P(A;;FA;;;{me})(XA;;FR;;;WD;(Member_of {{SID(WD)}}))")).unwrap();
        assert!(super::check_sd(xa_sd.0).is_err());
    }
}
