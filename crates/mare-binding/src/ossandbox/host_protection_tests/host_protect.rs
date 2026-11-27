//! Host-protect ancestor, capability gating, and bwrap/landlock escape tests.

use super::*;

fn run_with_mare_home_subprocess(env_key: &str, test_name: &str, marker: &str) {
    super::run_with_mare_home_subprocess_at(
        env_key,
        &format!("ossandbox::host_protection_tests::host_protect::{test_name}"),
        marker,
    );
}

#[test]
fn host_token_ancestors_are_readable_while_token_dir_and_files_are_unreadable() {
    const CHILD: &str = "MARE_TEST_HOST_TOKEN_ANCESTORS";
    let Ok(ws_str) = std::env::var(CHILD) else {
        run_with_mare_home_subprocess(
            CHILD,
            "host_token_ancestors_are_readable_while_token_dir_and_files_are_unreadable",
            "HOST_TOKEN_ANCESTORS_VERIFIED",
        );
        return;
    };
    let ws = std::path::Path::new(&ws_str);
    let mare_home = ws.join("mare-home");

    let token_file = mare_shared::paths::host_token_file();
    let host_dir = token_file.parent().unwrap();
    let state_dir = host_dir.parent().unwrap();
    std::fs::create_dir_all(host_dir).unwrap();
    std::fs::write(&token_file, "secret").unwrap();

    let p = make(&Lua::new(), ws, &[]).unwrap();
    assert!(p.deny_read.is_empty(), "host token protection must not populate explicit deny_read");
    assert!(p.can_read(ws), "workspace ancestor of token dir must remain readable");
    assert!(p.can_read(&mare_home), "MARE_HOME ancestor of token dir must remain readable");
    assert!(p.can_read(state_dir), "state ancestor of token dir must remain readable");
    assert!(!p.can_read(host_dir), "token dir itself must be unreadable");
    assert!(!p.can_read(&token_file), "token file must be unreadable");
    assert!(!p.can_read(&host_dir.join(".token.999.tmp")), "rotation temp file must be unreadable");
    assert!(!p.can_write(ws), "ancestor write/rename must remain blocked");
    println!("HOST_TOKEN_ANCESTORS_VERIFIED");
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_masks_preexisting_host_token_and_temp_while_control_readable() {
    const CHILD: &str = "MARE_TEST_BWRAP_PREEXISTING";
    let Ok(ws_str) = std::env::var(CHILD) else {
        if !require_bwrap_or_skip("bwrap_masks_preexisting_host_token_and_temp_while_control_readable") {
            return;
        }
        run_with_mare_home_subprocess(
            CHILD,
            "bwrap_masks_preexisting_host_token_and_temp_while_control_readable",
            "PREEXISTING_TOKEN_VERIFIED",
        );
        return;
    };
    assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
    let ws = std::path::Path::new(&ws_str);

    let token_file = mare_shared::paths::host_token_file();
    let host_dir = token_file.parent().unwrap().to_path_buf();
    let temp_token_file = host_dir.join(".token.preexisting.tmp");
    let state_dir = host_dir.parent().unwrap().to_path_buf();
    mare_shared::private::create_dir_all(&host_dir).unwrap();
    std::fs::write(&token_file, "preexisting-secret").unwrap();
    std::fs::write(&temp_token_file, "preexisting-temp-secret").unwrap();
    let control_file = state_dir.join("control.txt");
    std::fs::write(&control_file, "control-visible").unwrap();

    let p = make(&Lua::new(), ws, &[]).unwrap();
    let prepared = Prepared::from_policy(p, ws).unwrap();
    let wrapped = prepared.wrap(
        "/bin/sh",
        &[
            "-c".into(),
            "echo CTRL=$(cat \"$1\" 2>&1); cat \"$2\" 2>&1 || echo TOK_DENIED; cat \"$3\" 2>&1 || echo TMP_DENIED; \
             ls \"$4\" >/dev/null 2>&1 && echo ANCESTOR_LISTABLE; \
             mv \"$5\" \"$4/host-moved\" 2>&1 || echo RENAME_DENIED"
                .into(),
            "sh".into(),
            control_file.to_string_lossy().into_owned(),
            token_file.to_string_lossy().into_owned(),
            temp_token_file.to_string_lossy().into_owned(),
            state_dir.to_string_lossy().into_owned(),
            host_dir.to_string_lossy().into_owned(),
        ],
    );
    let out = std::process::Command::new(&wrapped.prog).args(&wrapped.args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "bwrap failed: {out:?}");
    assert!(stdout.contains("CTRL=control-visible"), "control must be readable: {stdout:?}");
    assert!(stdout.contains("TOK_DENIED"), "token must be unreadable: {stdout:?}");
    assert!(stdout.contains("TMP_DENIED"), "temp token must be unreadable: {stdout:?}");
    assert!(stdout.contains("ANCESTOR_LISTABLE"), "state ancestor must be listable: {stdout:?}");
    assert!(stdout.contains("RENAME_DENIED"), "renaming host dir must be denied: {stdout:?}");
    assert!(!stdout.contains("preexisting-secret"), "leaked token: {stdout:?}");
    assert!(!stdout.contains("preexisting-temp-secret"), "leaked temp token: {stdout:?}");
    println!("PREEXISTING_TOKEN_VERIFIED");
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_refuses_when_protected_host_dir_creation_fails() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "MARE_TEST_BWRAP_DIR_CREATION_FAILS";
    let Ok(ws_str) = std::env::var(CHILD) else {
        if !require_bwrap_or_skip("bwrap_refuses_when_protected_host_dir_creation_fails") {
            return;
        }
        run_with_mare_home_subprocess(
            CHILD,
            "bwrap_refuses_when_protected_host_dir_creation_fails",
            "DIR_CREATION_FAILS_VERIFIED",
        );
        return;
    };
    assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
    let ws = std::path::Path::new(&ws_str);
    let state_dir = ws.join("mare-home/state");
    std::fs::create_dir_all(&state_dir).unwrap();
    struct PermGuard(std::path::PathBuf);
    impl Drop for PermGuard {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _perm_guard = PermGuard(state_dir.clone());

    // Make <state> read-only so creating <state>/host fails (W1).
    std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let p = make(&Lua::new(), ws, &[]).unwrap();
    let err = Prepared::from_policy(p, ws).err().unwrap().to_string();
    assert!(err.contains(super::SANDBOX_INSUFFICIENT), "expected SANDBOX_INSUFFICIENT, got: {err}");
    assert!(err.contains("fs_host_protect"), "expected fs_host_protect in error, got: {err}");
    println!("DIR_CREATION_FAILS_VERIFIED");
}

#[test]
fn raw_seatbelt_profile_never_satisfies_fs_host_protect() {
    const CHILD: &str = "MARE_TEST_RAW_SEATBELT_HOST_PROTECT";
    let Ok(ws_str) = std::env::var(CHILD) else {
        run_with_mare_home_subprocess(
            CHILD,
            "raw_seatbelt_profile_never_satisfies_fs_host_protect",
            "RAW_SEATBELT_VERIFIED",
        );
        return;
    };
    let ws = std::path::Path::new(&ws_str);

    let token_file = mare_shared::paths::host_token_file();
    mare_shared::private::create_dir_all(token_file.parent().unwrap()).unwrap();
    std::fs::write(&token_file, "secret").unwrap();

    let mut p_ws = make(&Lua::new(), ws, &[]).unwrap();
    p_ws.restrict_read = true;
    p_ws.profile = Some("(version 1)(allow default)".into());
    let ws_missing = p_ws.missing(Backend::Seatbelt);
    assert!(ws_missing.contains(&Cap::FsProtect) && ws_missing.contains(&Cap::FsHostProtect), "{ws_missing:?}");

    #[allow(unused_mut)]
    let mut p = p_ws.with_level(Level::ReadOnly).unwrap();
    assert!(p.missing(Backend::Seatbelt).contains(&Cap::FsHostProtect));

    #[cfg(target_os = "macos")]
    {
        if expect::require("raw_seatbelt_profile_never_satisfies_fs_host_protect", backend().name(), &["seatbelt"]) {
            let err = Prepared::from_policy(p.clone(), ws).err().unwrap().to_string();
            assert!(err.contains("fs_host_protect"), "{err}");
            p.accept_partial.push(Cap::FsHostProtect);
            let prep = Prepared::from_policy(p, ws).unwrap();
            assert_eq!(prep.report(&Lua::new()).unwrap().get::<String>("enforcement").unwrap(), "partial");
        }
    }
    println!("RAW_SEATBELT_VERIFIED");
}

#[cfg(windows)]
#[test]
fn winacl_default_tier_gates_fs_host_protect_and_spawns_restricted_command() {
    const CHILD: &str = "MARE_TEST_WINACL_HOST_PROTECT";
    let Ok(ws_str) = std::env::var(CHILD) else {
        if !expect::require(
            "winacl_default_tier_gates_fs_host_protect_and_spawns_restricted_command",
            backend().name(),
            &["windows-acl"],
        ) {
            return;
        }
        run_with_mare_home_subprocess(
            CHILD,
            "winacl_default_tier_gates_fs_host_protect_and_spawns_restricted_command",
            "WINACL_HOST_PROTECT_VERIFIED",
        );
        return;
    };
    assert_eq!(backend(), Backend::WinAcl, "child must run under winacl backend");
    let ws = std::path::Path::new(&ws_str);

    let p = make(&Lua::new(), ws, &[]).unwrap().with_level(Level::ReadOnly).unwrap();
    let prep_absent = Prepared::from_policy(p.clone(), ws).unwrap();
    let denied_file = ws.join("winacl_denied.txt");
    let cmd_absent = format!("echo WINACL_ABSENT & echo x > \"{}\"", denied_file.display());
    let w_absent = prep_absent.wrap("cmd", &["/C".into(), cmd_absent]);
    let out_absent = std::process::Command::new(&w_absent.prog).args(&w_absent.args).output().unwrap();
    assert!(String::from_utf8_lossy(&out_absent.stdout).contains("WINACL_ABSENT"), "{out_absent:?}");
    assert!(!denied_file.exists(), "ReadOnly WinAcl must deny write to workspace");

    let rot_err = crate::http_serve::rotate_host_token_with_partial(false).unwrap_err();
    assert!(rot_err.contains("fs_host_protect"), "{rot_err}");
    let token_file = mare_shared::paths::host_token_file();
    assert!(!token_file.exists(), "producer refusal must not create token file");

    assert!(crate::http_serve::rotate_host_token_with_partial(true).is_ok());
    assert!(token_file.exists());

    let err = Prepared::from_policy(p.clone(), ws).err().unwrap().to_string();
    assert!(err.contains("fs_host_protect"), "{err}");

    let mut accepted = p;
    accepted.accept_partial.push(Cap::FsHostProtect);
    let prep_partial = Prepared::from_policy(accepted, ws).unwrap();
    assert_eq!(prep_partial.report(&Lua::new()).unwrap().get::<String>("enforcement").unwrap(), "partial");
    let w_partial = prep_partial.wrap("cmd", &["/C".into(), "echo WINACL_PARTIAL".into()]);
    let out_partial = std::process::Command::new(&w_partial.prog).args(&w_partial.args).output().unwrap();
    assert!(out_partial.status.success(), "{out_partial:?}");
    assert!(String::from_utf8_lossy(&out_partial.stdout).contains("WINACL_PARTIAL"), "{out_partial:?}");
    println!("WINACL_HOST_PROTECT_VERIFIED");
}

#[tokio::test]
async fn fs_walk_and_index_prune_host_token_dir_under_exec_ctx() {
    const CHILD: &str = "MARE_TEST_FS_WALK_PRUNE_HOST_TOKEN";
    let Ok(ws_str) = std::env::var(CHILD) else {
        run_with_mare_home_subprocess(
            CHILD,
            "fs_walk_and_index_prune_host_token_dir_under_exec_ctx",
            "FS_WALK_INDEX_PRUNE_VERIFIED",
        );
        return;
    };
    let ws = std::path::Path::new(&ws_str);

    let token_file = mare_shared::paths::host_token_file();
    let host_dir = token_file.parent().unwrap().to_path_buf();
    let state_dir = host_dir.parent().unwrap().to_path_buf();
    mare_shared::private::create_dir_all(&host_dir).unwrap();
    std::fs::write(&token_file, "secret-token").unwrap();
    std::fs::write(state_dir.join("visible.txt"), "ok").unwrap();

    let lua = Lua::new();
    let mare = lua.create_table().unwrap();
    lua.globals().set("mare", mare.clone()).unwrap();
    crate::exec_ctx::install(&lua, &mare).unwrap();
    crate::fs::install(&lua, &mare, ws.to_path_buf()).unwrap();

    let res: (Vec<String>, bool, Vec<String>) = lua
        .load(
            r#"
            local state_dir, host_dir = ...
            local walked, indexed
            local direct_ok = true
            local ctx = {
                tool = "test_walk",
                cwd = state_dir,
                sandbox = { level = "read-only" },
            }
            mare.exec.run(ctx, function()
                walked = mare.fs.walk(state_dir, { hidden = true, gitignore = false })
                local ok = pcall(mare.fs.walk, host_dir, { hidden = true, gitignore = false })
                direct_ok = ok
                local idx = mare.fs.index(state_dir, { hidden = true, gitignore = false })
                idx:refresh({ min_interval_ms = 0 })
                indexed = idx:files()
            end)
            return walked, direct_ok, indexed
            "#,
        )
        .call_async((state_dir.to_string_lossy().into_owned(), host_dir.to_string_lossy().into_owned()))
        .await
        .unwrap();

    let (walked, direct_ok, indexed) = res;
    assert!(walked.iter().any(|p| p == "visible.txt"), "ancestor walk must include visible.txt: {walked:?}");
    assert!(!walked.iter().any(|p| p.contains("host") || p.contains("token")), "walk leaked host dir: {walked:?}");
    assert!(!direct_ok, "walking host_dir directly must be refused");
    assert!(indexed.iter().any(|p| p == "visible.txt"), "index must include visible.txt: {indexed:?}");
    assert!(!indexed.iter().any(|p| p.contains("host") || p.contains("token")), "index leaked host dir: {indexed:?}");
    println!("FS_WALK_INDEX_PRUNE_VERIFIED");
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_blocks_parent_dir_rename_and_symlink_escape() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    const CHILD: &str = "MARE_TEST_BWRAP_PARENT_ESCAPE";
    let Ok(ws_str) = std::env::var(CHILD) else {
        if !require_bwrap_or_skip("bwrap_blocks_parent_dir_rename_and_symlink_escape") {
            return;
        }
        run_with_mare_home_subprocess(
            CHILD,
            "bwrap_blocks_parent_dir_rename_and_symlink_escape",
            "PARENT_ESCAPE_VERIFIED",
        );
        return;
    };
    assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
    let ws = std::path::Path::new(&ws_str);
    let mare_home = std::path::PathBuf::from(std::env::var("MARE_HOME").unwrap());
    let state_dir = mare_home.join("state");
    let evil_dir = ws.join("evil");
    std::fs::create_dir_all(evil_dir.join("host")).unwrap();

    // 1. Real process barrier test: token does not exist yet when bwrap starts.
    // Child attempts to rename `state` or `mare_home` and replace with symlink to `evil`,
    // then waits for parent to create token, and tries to read it.
    let p = make(&Lua::new(), ws, &[]).unwrap();
    let prep = Prepared::from_policy(p.clone(), ws).unwrap();
    let token_file = mare_shared::paths::host_token_file();
    let script = format!(
        "set +e; \
         mv \"{state}\" \"{state}.old\" 2>/dev/null && ln -s \"{evil}\" \"{state}\"; \
         mv \"{mh}\" \"{mh}.old\" 2>/dev/null && ln -s \"{evil}\" \"{mh}\"; \
         echo READY; \
         read _go; \
         test ! -r \"{tok}\" && test ! -r \"{evil}/host/token\" && echo BLOCKED",
        state = state_dir.display(),
        mh = mare_home.display(),
        evil = evil_dir.display(),
        tok = token_file.display(),
    );
    let w = prep.wrap("/bin/sh", &["-c".into(), script]);
    let mut child = Command::new(&w.prog)
        .args(&w.args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "READY", "bwrap child did not reach READY barrier");

    // Parent creates the token after child tried to redirect parent dirs.
    let tok = crate::http_serve::ensure_host_token_at(&token_file).unwrap();
    assert_eq!(tok.len(), 43);

    writeln!(child.stdin.as_mut().unwrap(), "go").unwrap();
    let mut rest = String::new();
    stdout.read_line(&mut rest).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "bwrap child failed (parent rename/symlink escape succeeded?): {status:?}");
    assert_eq!(rest.trim(), "BLOCKED");

    // 2. Symlinked ancestor inside writable root cannot be locked -> refused unless accept_partial.
    let link_home = ws.join("symlink-home");
    std::os::unix::fs::symlink(ws.join("real-target"), &link_home).unwrap();
    std::fs::create_dir_all(ws.join("real-target")).unwrap();
    let orig_home = std::env::var_os("MARE_HOME").unwrap();
    // SAFETY: single-threaded test subprocess, no concurrent env readers.
    unsafe { std::env::set_var("MARE_HOME", &link_home) };
    let err = Prepared::from_policy(p.clone(), ws).err().unwrap().to_string();
    assert!(err.contains("fs_host_protect"), "symlinked MARE_HOME ancestor must be refused: {err}");
    unsafe { std::env::set_var("MARE_HOME", orig_home) };
    println!("PARENT_ESCAPE_VERIFIED");
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_recognizes_ancestor_deny_read_mask_without_relaxing_protection() {
    const CHILD: &str = "MARE_TEST_BWRAP_ANCESTOR_DENY_READ";
    let Ok(ws_str) = std::env::var(CHILD) else {
        if !require_bwrap_or_skip("bwrap_recognizes_ancestor_deny_read_mask_without_relaxing_protection") {
            return;
        }
        run_with_mare_home_subprocess(
            CHILD,
            "bwrap_recognizes_ancestor_deny_read_mask_without_relaxing_protection",
            "ANCESTOR_DENY_READ_VERIFIED",
        );
        return;
    };
    assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
    let ws = std::path::Path::new(&ws_str);
    let token_file = mare_shared::paths::host_token_file();
    let state_dir = token_file.parent().unwrap().parent().unwrap().to_path_buf();
    let mare_home = state_dir.parent().unwrap().to_path_buf();
    let tok = crate::http_serve::ensure_host_token_at(&token_file).unwrap();
    std::fs::write(state_dir.join("sibling-secret"), "hidden").unwrap();
    std::fs::write(ws.join("control.txt"), "visible").unwrap();

    // `deny_read` masks `state_dir` (ancestor of `host_token_dir`). Accept only `fs_deny_read`,
    // NOT `fs_host_protect` — Prepared must recognize the ancestor mask satisfies `fs_host_protect`.
    let lua = Lua::new();
    let mut p = make(&lua, ws, &[state_dir.to_str().unwrap()]).unwrap();
    p.accept_partial.push(Cap::FsDenyRead);
    let prep = Prepared::from_policy(p, ws).unwrap();
    let report = prep.report(&lua).unwrap();
    let unavailable: Vec<String> =
        report.get::<mlua::Table>("unavailable").unwrap().sequence_values::<String>().map(|v| v.unwrap()).collect();
    assert!(
        !unavailable.iter().any(|c| c == "fs_host_protect"),
        "ancestor deny_read mask must satisfy fs_host_protect, got unavailable={unavailable:?}"
    );

    // Positive & negative controls inside real bwrap child:
    // - control.txt is readable
    // - token_file and sibling-secret are unreadable
    // - parent `mare_home` cannot be renamed/redirected
    let script = format!(
        "test \"$(cat '{ctrl}')\" = visible && test ! -r '{tokf}' && test ! -r '{sib}' && ! mv '{mh}' '{mh}.old' 2>/dev/null",
        ctrl = ws.join("control.txt").display(),
        tokf = token_file.display(),
        sib = state_dir.join("sibling-secret").display(),
        mh = mare_home.display(),
    );
    let w = prep.wrap("/bin/sh", &["-c".into(), script]);
    let out = std::process::Command::new(&w.prog).args(&w.args).output().unwrap();
    assert!(out.status.success(), "bwrap ancestor deny_read check failed: {out:?}");
    assert_eq!(std::fs::read_to_string(&token_file).unwrap(), tok);
    println!("ANCESTOR_DENY_READ_VERIFIED");
}

#[test]
fn strict_list_on_ancestor_of_host_token_dir_trims_protected_subtree() {
    const CHILD: &str = "MARE_TEST_STRICT_LIST_ANCESTOR";
    let Ok(ws_str) = std::env::var(CHILD) else {
        run_with_mare_home_subprocess(
            CHILD,
            "strict_list_on_ancestor_of_host_token_dir_trims_protected_subtree",
            "STRICT_LIST_ANCESTOR_VERIFIED",
        );
        return;
    };
    let ws = std::path::Path::new(&ws_str);
    let token_file = mare_shared::paths::host_token_file();
    let host_dir = token_file.parent().unwrap().to_path_buf();
    let state_dir = host_dir.parent().unwrap().to_path_buf();
    mare_shared::private::create_dir_all(&host_dir).unwrap();
    std::fs::write(&token_file, "secret").unwrap();
    std::fs::write(state_dir.join("visible.txt"), "ok").unwrap();

    let p = make(&Lua::new(), ws, &[]).unwrap();
    let entries = crate::fs::list_dir_entries(&state_dir, Some(&p), true).unwrap();
    assert!(entries.iter().any(|(name, _)| name == "visible.txt"), "expected visible.txt in {entries:?}");
    assert!(!entries.iter().any(|(name, _)| name == "host"), "protected host dir must be trimmed: {entries:?}");

    let direct_err = crate::fs::list_dir_entries(&host_dir, Some(&p), true).unwrap_err();
    assert_eq!(direct_err.kind(), std::io::ErrorKind::PermissionDenied);
    println!("STRICT_LIST_ANCESTOR_VERIFIED");
}
