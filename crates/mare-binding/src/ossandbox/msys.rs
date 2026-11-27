//! Git Bash (MSYS2/Cygwin) under the Windows sandbox: the coded failure and the doctor verdict.

use std::fmt::Write as _;
use std::path::Path;

use super::Backend;
#[cfg(windows)]
use super::backend::backend;

/// Error code when an MSYS2/Cygwin program (Git Bash) died starting under the Windows sandbox.
pub const SANDBOX_MSYS_UNSUPPORTED: &str = "SANDBOX_MSYS_UNSUPPORTED";
/// `STATUS_DLL_INIT_FAILED` (0xC0000142) as the signed exit code a Windows process reports.
pub const STATUS_DLL_INIT_FAILED: i64 = 0xC000_0142_u32 as i32 as i64;

/// Whether `program` is an MSYS2/Cygwin binary, cheaply: its directory holds the runtime DLL
/// (`msys-2.0.dll`, `cygwin1.dll`), or it is a Git for Windows launcher (`<Git>\bin\bash.exe`)
/// whose `..\usr\bin` holds it. `exists` is the file-system probe (injected for tests).
pub fn is_msys_program(program: &Path, exists: impl Fn(&Path) -> bool) -> bool {
    let Some(dir) = program.parent() else { return false };
    let usr_bin = dir.join("..").join("usr").join("bin");
    ["msys-2.0.dll", "cygwin1.dll"].iter().any(|dll| exists(&dir.join(dll)) || exists(&usr_bin.join(dll)))
}

/// A sandboxed command whose MSYS2/Cygwin runtime couldn't initialise under the Windows
/// sandbox's write-restricted token (it is denied opening the signal pipe it creates:
/// `couldn't create signal pipe, Win32 error 5`, exit 0xC0000142): the coded error to report
/// instead of the raw crash. `mechanism` is the run's sandbox report mechanism; the program is
/// MSYS when [`is_msys_program`] says so or the output carries the Cygwin runtime's own
/// `*** fatal error -` line. `None` for everything else. The default tier can't run MSYS: the
/// message names the three ways out; the enhanced tier normally can, so a crash there points at a
/// stale setup.
pub fn msys_failure(mechanism: &str, code: Option<i64>, msys_program: bool, output: &str) -> Option<String> {
    let windows = matches!(mechanism, "windows-acl" | "windows-user");
    let dll_init = matches!(code, Some(c) if c == STATUS_DLL_INIT_FAILED || c == 0xC000_0142);
    let msys = msys_program || output.contains("*** fatal error -");
    (windows && dll_init && msys).then(|| {
        if mechanism == "windows-user" {
            format!(
                "{SANDBOX_MSYS_UNSUPPORTED}: Git Bash/MSYS couldn't start inside the enhanced Windows sandbox; run \
                 `mare sandbox setup` again (the installed setup may predate Git Bash support), use the powershell \
                 tool, or approve running this command outside the sandbox"
            )
        } else {
            format!(
                "{SANDBOX_MSYS_UNSUPPORTED}: Git Bash/MSYS programs can't start inside the default Windows sandbox \
                 tier; use the powershell tool, run `mare sandbox setup` once (as administrator) to enable the \
                 enhanced tier where Git Bash works, or approve running this command outside the sandbox"
            )
        }
    })
}

/// `mare doctor`: whether Git Bash can run in the sandbox here, as `(ok, warn,
/// detail)`; `None` off Windows or without a sandbox backend. The enhanced tier runs it, which
/// requires the sandbox directory and helper to be locked (`lock`: the `check_lock` outcome when a
/// setup is installed); the default tier can't, so sandboxed commands default to PowerShell.
pub fn msys_verdict(backend: Backend, lock: Option<Result<(), String>>) -> Option<(bool, bool, String)> {
    match (backend, lock) {
        (Backend::WinUser, Some(Ok(()))) => Some((
            true,
            false,
            "Git Bash runs in the sandbox (enhanced tier: the sandbox account's own SID is a restricting SID; \
             the sandbox directory and helper are writable only by SYSTEM and Administrators)"
                .into(),
        )),
        // the account SID is in use: an unlocked directory is a real hole
        (Backend::WinUser, lock) => Some((
            false,
            false,
            format!(
                "the enhanced sandbox's directory is not locked ({}): sandboxed commands could tamper with later \
                 runs — run `mare sandbox setup` again",
                lock.and_then(Result::err).unwrap_or_else(|| "no setup found".into())
            ),
        )),
        (Backend::WinAcl, lock) => {
            let mut s = String::from(
                "default tier: Git Bash (MSYS) can't start inside the sandbox, so sandboxed commands default to \
                 PowerShell; `mare sandbox setup` (once, as administrator) enables the enhanced tier, where Git \
                 Bash works",
            );
            if let Some(Err(e)) = lock {
                let _ = write!(s, " (the installed setup is not locked: {e}; run it again)");
            }
            Some((false, true, s))
        }
        _ => None,
    }
}

/// [`msys_verdict`] for this host.
pub fn msys_report() -> Option<(bool, bool, String)> {
    #[cfg(windows)]
    {
        use crate::winsandbox_user as wsu;
        let b = backend();
        if !matches!(b, Backend::WinAcl | Backend::WinUser) {
            return None;
        }
        let lock = wsu::SetupState::load(&wsu::sandbox_dir()).ok().flatten().map(|s| wsu::win::check_lock(&s));
        msys_verdict(b, lock)
    }
    #[cfg(not(windows))]
    None
}
