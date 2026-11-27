use super::*;
use std::path::{Path, PathBuf};

#[test]
fn temp_and_workspace_must_not_nest() {
    let root = std::env::temp_dir().join("mare-disjoint-test");
    let (ws, tmp) = (root.join("ws"), root.join("ws").join("tmp"));
    assert!(!disjoint(&ws, &tmp) && !disjoint(&tmp, &ws), "nested either way");
    assert!(!disjoint(&ws, &ws), "equal");
    assert!(disjoint(&ws, &root.join("ws2")), "a shared name prefix isn't nesting");
    assert!(disjoint(&root.join("a"), &root.join("b")));
    // `..` and (macOS/Windows) case can't hide the nesting
    assert!(!disjoint(&ws, &root.join("x").join("..").join("ws").join("t")));
    if cfg!(windows) {
        assert!(!disjoint(&root.join("WS"), &tmp));
        // the capability SID follows the canonical key, not the spelling
        assert_eq!(workspace_write_sid(&root.join("WS")), workspace_write_sid(&ws));
    }
    assert!(prepare_grants(std::slice::from_ref(&ws), &tmp).is_err());
}

/// a project under %TEMP% (extracted archive, temp clone) is a sibling of the
/// private temp dir (`%TEMP%\mare-sbx\<pid>-<id>`), not nested with it: granted, not refused.
#[test]
fn a_workspace_under_temp_is_disjoint_from_the_private_temp() {
    let ws = tempfile::tempdir().unwrap();
    let ws = crate::realpath::resolve(ws.path()).unwrap();
    let tmp = session_temp_dir().unwrap();
    assert!(disjoint(&ws, &tmp), "{ws:?} vs {tmp:?}");
}

/// (2026-11-09): a writable root containing %TEMP% shares its write access with the
/// private temp directory instead of being refused; every other overlap is still refused.
#[test]
fn a_root_containing_temp_shares_instead_of_refusing() {
    let d = tempfile::tempdir().unwrap();
    let home = crate::realpath::resolve(d.path()).unwrap();
    let base = home.join("AppData").join("Local").join("Temp");
    let private = base.join("mare-sbx").join("1-abc");
    let g = |roots: &[PathBuf]| temp_grant(roots, &base, &private);
    // root == %TEMP% (mare started in %TEMP%)
    assert_eq!(g(std::slice::from_ref(&base)), Ok(TempGrant::Shared(base.clone())));
    // root contains %TEMP%: the home folder, the drive root (`C:\` / `/`)
    assert_eq!(g(std::slice::from_ref(&home)), Ok(TempGrant::Shared(home.clone())));
    let drive = home.ancestors().last().unwrap().to_path_buf();
    assert_eq!(g(std::slice::from_ref(&drive)), Ok(TempGrant::Shared(drive.clone())));
    // whichever root contains it, among disjoint ones
    let ws = home.join("work").join("repo");
    assert_eq!(g(&[ws.clone(), home.clone()]), Ok(TempGrant::Shared(home.clone())));
    // disjoint: a project elsewhere, a project under %TEMP% (a sibling of mare-sbx), a
    // shared name prefix
    assert_eq!(g(std::slice::from_ref(&ws)), Ok(TempGrant::Isolated));
    assert_eq!(g(&[base.join("proj")]), Ok(TempGrant::Isolated));
    assert_eq!(g(&[home.join("AppData").join("Local").join("Temp2")]), Ok(TempGrant::Isolated));
    assert_eq!(g(&[]), Ok(TempGrant::Isolated));
    // still refused: a root between %TEMP% and the private directory, or inside it
    assert!(g(&[base.join("mare-sbx")]).is_err());
    assert!(g(std::slice::from_ref(&private)).is_err());
    assert!(g(&[private.join("x")]).is_err());
    // spelling doesn't hide containment where the file system folds case
    if cfg!(windows) {
        let upper = PathBuf::from(home.to_string_lossy().to_uppercase());
        assert!(matches!(g(&[upper]), Ok(TempGrant::Shared(_))));
    }
}

#[test]
fn sids_are_stable_and_distinct() {
    let a = workspace_write_sid(Path::new("C:\\work\\repo")).unwrap();
    assert_eq!(a, workspace_write_sid(Path::new("C:\\work\\repo")).unwrap());
    assert_ne!(a, workspace_write_sid(Path::new("C:\\work\\other")).unwrap());
    assert!(a.starts_with("S-1-4-") && a.matches('-').count() == 4, "{a}");
    let t = temp_write_sid(Path::new("C:\\work\\repo")).unwrap();
    assert!(t.ends_with("-1") && t.matches('-').count() == 5, "{t}");
    assert_ne!(t.trim_end_matches("-1"), a, "temp identities never collide with workspace ones");
}

#[test]
fn runner_args_roundtrip_and_validation() {
    let a = RunnerArgs {
        mode: "workspace-write".into(),
        sids: vec!["S-1-4-1-2".into(), "S-1-4-3-4-1".into()],
        cwd: PathBuf::from("C:\\w"),
        argv: vec!["bash.exe".into(), "-c".into(), "echo --mode x".into()],
        as_user: None,
        account: None,
        pty: None,
    };
    assert_eq!(RunnerArgs::parse(&a.to_args()).unwrap(), a);
    let b = RunnerArgs { as_user: Some("offline".into()), pty: Some((120, 40)), ..a.clone() };
    assert_eq!(RunnerArgs::parse(&b.to_args()).unwrap(), b);
    // the inner runner of the enhanced tier: the account SID travels, a non-account SID doesn't
    let c = RunnerArgs { account: Some("S-1-5-21-1-2-3-1001".into()), ..a.clone() };
    assert_eq!(RunnerArgs::parse(&c.to_args()).unwrap(), c);
    let mut bad = a.to_args();
    bad.splice(0..0, ["--account".to_string(), "S-1-4-1-2".into()]);
    assert!(RunnerArgs::parse(&bad).is_err(), "a capability SID is not an account");
    let both = RunnerArgs { as_user: Some("offline".into()), ..c.clone() };
    assert!(RunnerArgs::parse(&both.to_args()).is_err(), "the outer runner never takes --account");
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(RunnerArgs::parse(&s(&["--mode", "read-only", "--cwd", "x", "--sid", "S-1", "--", "a"])).is_err());
    assert!(RunnerArgs::parse(&s(&["--mode", "workspace-write", "--cwd", "x", "--", "a"])).is_err());
    assert!(RunnerArgs::parse(&s(&["--mode", "read-only", "--cwd", "x", "--"])).is_err());
    assert!(RunnerArgs::parse(&s(&["--mode", "read-only", "--cwd", "x", "--", "a"])).is_ok());
}

#[test]
fn windows_quoting() {
    assert_eq!(quote_windows_arg("plain"), "plain");
    assert_eq!(quote_windows_arg(""), "\"\"");
    assert_eq!(quote_windows_arg("a b"), "\"a b\"");
    assert_eq!(quote_windows_arg(r#"say "hi""#), r#""say \"hi\"""#);
    assert_eq!(quote_windows_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    assert_eq!(argv_to_command_line(&["a".into(), "b c".into()]), "a \"b c\"");
}

/// Win32 ACL calls on a real directory (real Windows: CI or MARE_WIN_SANDBOX_TEST=1). The
/// enforcement itself is covered by mare-cli's `win_sandbox` integration test.
#[cfg(windows)]
#[test]
fn grant_write_is_idempotent() {
    // Wine refuses to write mandatory-label SACLs (ERROR_ACCESS_DENIED); real Windows only
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let sid = workspace_write_sid(d.path()).unwrap();
    win::grant_write(d.path(), &sid).unwrap();
    win::grant_write(d.path(), &sid).unwrap();
    let _ = win::probe();
}

#[test]
fn runner_refuses_bad_args() {
    assert_eq!(runner_main(&["--mode".into()]), RUNNER_FAILURE_EXIT);
}

/// The confined process's desktop is a private one in the runner's own window station (WinSta0
/// interactively, `Service-0x0-…$` over SSH) — never a hard-coded `winsta0\default`.
#[cfg(windows)]
#[test]
fn the_confined_desktop_is_private_and_in_the_runners_window_station() {
    use windows_sys::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_NAME,
    };
    if std::env::var("CI").is_err() && std::env::var("MARE_WIN_SANDBOX_TEST").is_err() {
        return;
    }
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(
            GetProcessWindowStation(),
            UOI_NAME,
            buf.as_mut_ptr().cast(),
            (buf.len() * 2) as u32,
            &mut needed,
        )
    };
    assert_ne!(ok, 0);
    let station = String::from_utf16_lossy(&buf[..buf.iter().position(|c| *c == 0).unwrap()]);
    let (user, logon) = win::current_sids().unwrap();
    let d = desktop::LaunchDesktop::private(user.ptr(), logon.ptr()).unwrap();
    let name = d.startup_name();
    let name = String::from_utf16_lossy(&name[..name.len() - 1]);
    assert!(name.starts_with(&format!("{station}\\mare-sandbox-")), "{name} (window station {station})");
}

#[test]
fn explicit_root_grants_do_not_materialize_an_implicit_temp_capability() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("root");
    let temp = fixture.path().join("private-temp");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&temp).unwrap();
    let roots = vec![crate::realpath::resolve(&root).unwrap()];
    let exact = prepare_root_grants(&roots).unwrap();
    assert_eq!(exact, vec![workspace_write_sid(&roots[0]).unwrap()]);
    assert!(!exact.contains(&temp_write_sid(&temp).unwrap()));
    let with_temp = prepare_grants(&roots, &temp).unwrap();
    assert!(with_temp.contains(&temp_write_sid(&temp).unwrap()));
}

#[test]
fn enhanced_readonly_has_an_independent_ungranted_capability() {
    let sid = readonly_capability_sid();
    assert!(sid.ends_with("-2"));
    assert_eq!(sid, readonly_capability_sid(), "reuse the ungranted identity without growing NUL ACL per call");
    let a = RunnerArgs {
        mode: "read-only".into(),
        sids: vec![sid],
        cwd: "C:/workspace".into(),
        argv: vec!["cmd.exe".into(), "/c".into(), "echo read-only".into()],
        as_user: Some("offline".into()),
        account: None,
        pty: None,
    };
    assert_eq!(RunnerArgs::parse(&a.to_args()).unwrap(), a);
    let no_cap = RunnerArgs { sids: vec![], ..a.clone() };
    assert!(RunnerArgs::parse(&no_cap.to_args()).is_err());
    let inner = RunnerArgs { as_user: None, account: Some("S-1-5-21-1-2-3-1001".into()), ..a };
    assert_eq!(RunnerArgs::parse(&inner.to_args()).unwrap(), inner);
}
