//! Copyright 2025 OpenAI (Apache-2.0); modified for mare.
//! The desktop a confined process starts on (default tier).
//!
//! A process with a GUI subsystem DLL (user32: PowerShell, most of Windows) connects to a window
//! station and a desktop while its DLLs initialise; without access it dies with
//! `STATUS_DLL_INIT_FAILED` (0xC0000142) before running a line. The confined token is
//! write-restricted (logon SID, Everyone, capability SIDs) and Low integrity, so it needs a
//! desktop whose DACL grants the logon SID and whose label lets Low integrity in.
//!
//! The runner creates a private desktop in its *own* window station (Chromium
//! `sandbox/win/src/window.cc` `CreateAltDesktop` + `GetFullDesktopName`
//! `windows-sandbox-rs/src/desktop.rs` `PrivateDesktop`): the DACL grants the runner's user full
//! control and the confined token's logon SID participant access, with a Low
//! no-write-up label. The window station is named as it really is: WinSta0 in an interactive
//! session, `Service-0x0-<luid>$` in a non-interactive one (OpenSSH, services). Measured over
//! SSH (Windows 11, PowerShell 5.1): the old hard-coded `winsta0\default` names another
//! session's window station and PowerShell dies with 0xC0000142; the real `Service-…$\Default`
//! grants only the user SID, so the write-restricted token fails there too; a private desktop
//! without the Low label fails the same way; with it, PowerShell and ConPTY run both over SSH
//! and in an interactive session (prefixing `Winsta0\` unconditionally over SSH names a desktop
//! it did not create there).
//! A private desktop also keeps confined windows (hooks, messages, journaling) off the user's own
//! desktop. Failing to create one is a sandbox start failure (the runner's `mare-sandbox:`
//! diagnostic), never a command that crashes while its DLLs initialise.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, EXPLICIT_ACCESS_W, GRANT_ACCESS, SE_WINDOW_OBJECT,
    SetEntriesInAclW, SetSecurityInfo, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W,
};
use windows_sys::Win32::Security::{DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, DESKTOP_CREATEMENU, DESKTOP_CREATEWINDOW, DESKTOP_DELETE, DESKTOP_ENUMERATE,
    DESKTOP_HOOKCONTROL, DESKTOP_JOURNALPLAYBACK, DESKTOP_JOURNALRECORD, DESKTOP_READ_CONTROL, DESKTOP_READOBJECTS,
    DESKTOP_SWITCHDESKTOP, DESKTOP_WRITE_DAC, DESKTOP_WRITE_OWNER, DESKTOP_WRITEOBJECTS, GetProcessWindowStation,
    GetUserObjectInformationW, HDESK, UOI_NAME,
};

use super::win::{last, sid_string, wide};

const DESKTOP_ALL_ACCESS: u32 = DESKTOP_READOBJECTS
    | DESKTOP_CREATEWINDOW
    | DESKTOP_CREATEMENU
    | DESKTOP_HOOKCONTROL
    | DESKTOP_JOURNALRECORD
    | DESKTOP_JOURNALPLAYBACK
    | DESKTOP_ENUMERATE
    | DESKTOP_WRITEOBJECTS
    | DESKTOP_SWITCHDESKTOP
    | DESKTOP_DELETE
    | DESKTOP_READ_CONTROL
    | DESKTOP_WRITE_DAC
    | DESKTOP_WRITE_OWNER;
/// What the confined process may do on its desktop: everything but changing or deleting it
/// .
const DESKTOP_PARTICIPANT_ACCESS: u32 =
    DESKTOP_ALL_ACCESS & !(DESKTOP_WRITE_DAC | DESKTOP_WRITE_OWNER | DESKTOP_DELETE);

/// The name of a window station or desktop handle (`UOI_NAME`).
fn object_name(h: *mut c_void) -> Result<String, String> {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    let ok =
        unsafe { GetUserObjectInformationW(h, UOI_NAME, buf.as_mut_ptr().cast(), (buf.len() * 2) as u32, &mut needed) };
    if ok == 0 {
        return Err(last("GetUserObjectInformationW(UOI_NAME)"));
    }
    let n = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    Ok(String::from_utf16_lossy(&buf[..n]))
}

/// The station inherited by a child when STARTUPINFO.lpDesktop is null.
pub(crate) fn process_station_name() -> Result<String, String> {
    object_name(unsafe { GetProcessWindowStation() })
}

/// A desktop created for one confined process tree; closed (and so destroyed once the tree's own
/// handles are gone) when the runner is done with it.
pub struct LaunchDesktop {
    handle: HDESK,
    /// `<station>\<desktop>`, NUL-terminated, for `STARTUPINFO.lpDesktop`
    startup: Vec<u16>,
}

impl LaunchDesktop {
    /// A private desktop in the runner's window station, owned by `user` (the runner's user)
    /// and open to a confined token whose logon SID is `logon`.
    pub fn private(user: *mut c_void, logon: *mut c_void) -> Result<Self, String> {
        let station = unsafe { GetProcessWindowStation() };
        if station.is_null() {
            return Err(last("GetProcessWindowStation"));
        }
        let station = object_name(station)?;
        let sddl = format!(
            "D:P(A;;0x{DESKTOP_ALL_ACCESS:x};;;{})(A;;0x{DESKTOP_PARTICIPANT_ACCESS:x};;;{})S:(ML;;NW;;;LW)",
            sid_string(user)?,
            sid_string(logon)?,
        );
        let mut sd: *mut c_void = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(wide(&sddl).as_ptr(), 1, &mut sd, std::ptr::null_mut())
        } == 0
        {
            return Err(last("ConvertStringSecurityDescriptorToSecurityDescriptorW(desktop)"));
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let name = format!("mare-sandbox-{}-{:016x}", std::process::id(), rand::random::<u64>());
        let handle = unsafe {
            CreateDesktopW(wide(&name).as_ptr(), std::ptr::null(), std::ptr::null(), 0, DESKTOP_ALL_ACCESS, &sa)
        };
        let err = handle.is_null().then(|| {
            last(&format!(
                "creating the sandboxed command's desktop {name} in window station {station} (CreateDesktopW)"
            ))
        });
        unsafe { LocalFree(sd as HLOCAL) };
        if let Some(e) = err {
            return Err(e);
        }
        Ok(Self { handle, startup: wide(format!("{station}\\{name}")) })
    }

    /// Enhanced runner: create a fresh desktop on WinSta0 with only this login's SID.
    /// The runner must already be attached to WinSta0; refuse other stations instead of
    /// silently launching on a service desktop or the interactive Default desktop.
    pub fn enhanced(logon: *mut c_void) -> Result<Self, String> {
        let station = object_name(unsafe { GetProcessWindowStation() })?;
        if !station.eq_ignore_ascii_case("WinSta0") {
            return Err(format!("enhanced sandbox requires WinSta0, runner is attached to {station}"));
        }
        let name = format!("mare-sandbox-{:032x}", rand::random::<u128>());
        let handle = unsafe {
            CreateDesktopW(
                wide(&name).as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                DESKTOP_ALL_ACCESS,
                std::ptr::null(),
            )
        };
        if handle.is_null() {
            return Err(last("creating enhanced sandbox private desktop"));
        }
        // Replace the inherited DACL with a single ACE for this login (create -> grant).
        // Close the new desktop on every grant failure.
        let entry = EXPLICIT_ACCESS_W {
            grfAccessPermissions: DESKTOP_ALL_ACCESS,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: 0,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: std::ptr::null_mut(),
                MultipleTrusteeOperation: 0,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_UNKNOWN,
                ptstrName: logon.cast(),
            },
        };
        let mut acl = std::ptr::null_mut();
        let result = unsafe { SetEntriesInAclW(1, &entry, std::ptr::null(), &mut acl) };
        if result != 0 {
            unsafe { CloseDesktop(handle) };
            return Err(format!("SetEntriesInAclW(enhanced desktop) failed: {result}"));
        }
        let result = unsafe {
            SetSecurityInfo(
                handle,
                SE_WINDOW_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null_mut(),
            )
        };
        unsafe { LocalFree(acl as HLOCAL) };
        if result != 0 {
            unsafe { CloseDesktop(handle) };
            return Err(format!("SetSecurityInfo(enhanced desktop) failed: {result}"));
        }
        Ok(Self { handle, startup: wide(format!("WinSta0\\{name}")) })
    }

    /// The `startup` name, for the caller's `STARTUPINFO`.
    pub fn startup_name(&self) -> Vec<u16> {
        self.startup.clone()
    }
}

impl Drop for LaunchDesktop {
    fn drop(&mut self) {
        unsafe { CloseDesktop(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_WINDOW_OBJECT};
    use windows_sys::Win32::Security::{ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce};
    use windows_sys::Win32::System::StationsAndDesktops::OpenDesktopW;

    #[test]
    fn enhanced_desktop_has_per_login_acl_unique_name_and_scoped_lifetime() {
        if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
            return;
        }
        let (_, logon) = super::super::win::current_sids().unwrap();
        let station = object_name(unsafe { GetProcessWindowStation() }).unwrap();
        if !station.eq_ignore_ascii_case("WinSta0") {
            assert!(LaunchDesktop::enhanced(logon.ptr()).is_err());
            return;
        }
        let first = LaunchDesktop::enhanced(logon.ptr()).unwrap();
        let second = LaunchDesktop::enhanced(logon.ptr()).unwrap();
        assert_ne!(first.startup_name(), second.startup_name());
        let name = object_name(first.handle).unwrap();
        assert_eq!(first.startup_name(), wide(format!("WinSta0\\{name}")));
        unsafe {
            let mut dacl = std::ptr::null_mut();
            let mut descriptor = std::ptr::null_mut();
            assert_eq!(
                GetSecurityInfo(
                    first.handle,
                    SE_WINDOW_OBJECT,
                    DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut descriptor
                ),
                0
            );
            assert!(!dacl.is_null());
            assert_eq!((*dacl).AceCount, 1);
            let mut ace = std::ptr::null_mut();
            assert_ne!(GetAce(dacl, 0, &mut ace), 0);
            let ace = &*(ace as *const ACCESS_ALLOWED_ACE);
            assert_eq!(ace.Header.AceType, 0);
            assert_eq!(ace.Mask, DESKTOP_ALL_ACCESS);
            assert_ne!(EqualSid((&ace.SidStart as *const u32).cast_mut().cast(), logon.ptr()), 0);
            LocalFree(descriptor as HLOCAL);
        }
        drop(first);
        let reopened = unsafe { OpenDesktopW(wide(name).as_ptr(), 0, 0, DESKTOP_READOBJECTS) };
        assert!(reopened.is_null(), "private desktop survived its owner without any child handles");
    }
}
