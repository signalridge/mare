use super::*;
use std::path::Path;

use crate::realpath::lexical;

/// Git Bash under the Windows sandbox dies in its runtime's init: the tool result is a coded
/// `SANDBOX_MSYS_UNSUPPORTED` instead of a raw crash; nothing else is.
#[test]
fn msys_crash_under_the_windows_sandbox_maps_to_a_coded_error() {
    let git = Path::new("/Program Files/Git");
    let dlls = [git.join("usr/bin/msys-2.0.dll"), PathBuf::from("/cygwin64/bin/cygwin1.dll")];
    let exists = |p: &Path| dlls.iter().any(|d| lexical(p) == lexical(d));
    // usr\bin\bash.exe (next to the DLL), the Git\bin launcher, plain Cygwin
    assert!(is_msys_program(&git.join("usr/bin/bash.exe"), exists));
    assert!(is_msys_program(&git.join("bin/bash.exe"), exists));
    assert!(is_msys_program(Path::new("/cygwin64/bin/sh.exe"), exists));
    assert!(!is_msys_program(Path::new("/Windows/System32/cmd.exe"), exists));
    assert!(!is_msys_program(Path::new("bash"), exists));

    let crash = "0 [main] bash (7124) C:\\Program Files\\Git\\usr\\bin\\bash.exe: *** fatal error - \
                 couldn't create signal pipe, Win32 error 5";
    let code = Some(STATUS_DLL_INIT_FAILED);
    assert_eq!(STATUS_DLL_INIT_FAILED, -1073741502);
    let e = msys_failure("windows-acl", code, true, "").unwrap();
    assert!(e.starts_with("SANDBOX_MSYS_UNSUPPORTED: ") && e.contains("powershell"), "{e}");
    // the three ways out — PowerShell, the enhanced tier, approval outside the sandbox
    assert!(e.contains("mare sandbox setup") && e.contains("outside the sandbox"), "{e}");
    // the runtime's own fatal line identifies MSYS even when the program wasn't recognised;
    // the enhanced tier runs Git Bash, so a crash there points at the setup
    let e = msys_failure("windows-user", code, false, crash).unwrap();
    assert!(e.contains("`mare sandbox setup` again"), "{e}");
    assert!(msys_failure("windows-acl", Some(0xC000_0142), true, "").is_some(), "unsigned spelling");
    // not this failure: another exit code, a native program, another sandbox
    assert_eq!(msys_failure("windows-acl", Some(1), true, crash), None);
    assert_eq!(msys_failure("windows-acl", None, true, crash), None);
    assert_eq!(msys_failure("windows-acl", code, false, "ordinary output"), None);
    assert_eq!(msys_failure("seatbelt", code, true, crash), None);
}

/// `mare doctor`'s Git Bash line: fine in a locked enhanced tier, a failure when
/// the enhanced tier runs with an unlocked directory, a warning naming the way out otherwise.
#[test]
fn doctor_reports_git_bash_per_tier() {
    let (ok, warn, d) = msys_verdict(Backend::WinUser, Some(Ok(()))).unwrap();
    assert!(ok && !warn && d.contains("Git Bash runs"), "{d}");
    let (ok, warn, d) = msys_verdict(Backend::WinUser, Some(Err("Users may write".into()))).unwrap();
    assert!(!ok && !warn && d.contains("Users may write"), "{d}");
    let (ok, warn, d) = msys_verdict(Backend::WinAcl, None).unwrap();
    assert!(!ok && warn && d.contains("PowerShell") && d.contains("mare sandbox setup"), "{d}");
    let (_, warn, d) = msys_verdict(Backend::WinAcl, Some(Err("inherits".into()))).unwrap();
    assert!(warn && d.contains("not locked: inherits"), "{d}");
    assert_eq!(msys_verdict(Backend::Seatbelt, None), None);
}

/// The enhanced Windows account shares proxy ports across calls, so neither network
/// capability is safe to advertise until proxy access is bound to an individual call.
#[test]
fn windows_enhanced_does_not_claim_per_call_network_isolation() {
    let caps = super::backend::probe_caps(Backend::WinUser);
    assert!(!caps.contains(&Cap::FsRead), "WinUser must not falsely claim allowlist fs_read: {caps:?}");
    assert!(caps.contains(&Cap::FsWrite));
    assert!(!caps.contains(&Cap::NetBlock), "a call can reach another call's proxy: {caps:?}");
    assert!(!caps.contains(&Cap::NetProxy), "proxy policy is not bound to a call: {caps:?}");
}

/// Missing .mare and existing .git require process-level enforcement.
#[test]
fn protected_paths_participate_in_capability_matching() {
    let dir = tempfile::tempdir().unwrap();
    let policy = Policy::build(Level::WorkspaceWrite, dir.path(), None, &mlua::Value::Boolean(true)).unwrap();
    assert!(policy.required().contains(&Cap::FsProtect), "missing .mare still needs protection");
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    assert!(policy.required().contains(&Cap::FsProtect));
    for backend in [Backend::Landlock { partial: false }, Backend::WinAcl, Backend::WinUser] {
        assert!(policy.missing(backend).contains(&Cap::FsProtect), "{backend:?} falsely claims protection");
    }
}

#[test]
fn regular_mare_file_at_mcp_project_root_remains_protected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".mcp.json"), "{}").unwrap();
    let protected = dir.path().join(".mare");
    std::fs::write(&protected, "do not write").unwrap();
    let policy = Policy::build(Level::WorkspaceWrite, dir.path(), None, &mlua::Value::Boolean(true)).unwrap();
    assert!(policy.project_protected.contains(&protected), "{policy:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn bubblewrap_masks_missing_mare_and_rejects_symlinked_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let p = Policy::build(Level::WorkspaceWrite, dir.path(), None, &mlua::Value::Boolean(true)).unwrap();
    assert!(p.required().contains(&Cap::FsProtect));
    assert!(!p.bwrap_metadata_gap());
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    assert!(p.required().contains(&Cap::FsProtect));
    assert!(!p.bwrap_metadata_gap());
    std::os::unix::fs::symlink(dir.path().join(".git"), dir.path().join(".mare")).unwrap();
    assert!(p.bwrap_metadata_gap());
    let readonly = Policy::build(Level::ReadOnly, dir.path(), None, &mlua::Value::Boolean(true)).unwrap();
    assert!(!readonly.required().contains(&Cap::FsProtect));
}

#[test]
fn temp_exclusion_flags_are_independent_booleans() {
    let lua = Lua::new();
    let root = tempfile::tempdir().unwrap();
    for env in [false, true] {
        for slash in [false, true] {
            let spec = mare_shared::lua::json_to_lua(
                &lua,
                &serde_json::json!({"level":"workspace-write", "write":[],
            "exclude_tmpdir_env_var":env,"exclude_slash_tmp":slash}),
            )
            .unwrap();
            let policy = Policy::from_lua(&lua, &spec, root.path()).unwrap().unwrap();
            let value = policy.to_lua(&lua).unwrap();
            assert_eq!(value.get::<bool>("exclude_tmpdir_env_var").unwrap(), env);
            assert_eq!(value.get::<bool>("exclude_slash_tmp").unwrap(), slash);
            if env && slash {
                assert!(policy.writable.is_empty());
            }
        }
    }
    for field in ["exclude_tmpdir_env_var", "exclude_slash_tmp"] {
        let mut spec = serde_json::json!({"level":"workspace-write"});
        spec[field] = serde_json::json!("false");
        assert!(Policy::from_lua(&lua, &mare_shared::lua::json_to_lua(&lua, &spec).unwrap(), root.path()).is_err());
    }
}
