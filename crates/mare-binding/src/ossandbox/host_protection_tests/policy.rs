use super::*;

#[test]
fn explicit_denies_override_readable_and_writable_and_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let secret = tmp.path().join("secret");
    std::fs::create_dir(&secret).unwrap();
    std::fs::write(secret.join("token"), "secret").unwrap();
    let lua = Lua::new();
    let p = make(&lua, tmp.path(), &["secret"]).unwrap();
    assert!(!p.can_read(&secret.join("token")));
    assert!(!p.can_write(&secret.join("token")));
    assert!(!p.can_write(tmp.path()), "an ancestor rename must not move the denied subtree");
    assert!(p.can_read(&tmp.path().join("secret-sibling")));
    assert!(p.can_write(&tmp.path().join("secret-sibling")));
    assert!(p.required().iter().any(|c| c.name() == "fs_deny_read"));
    let restored = Policy::from_lua(&lua, &Value::Table(p.to_lua(&lua).unwrap()), tmp.path()).unwrap().unwrap();
    assert!(!restored.can_read(&secret.join("token")));
}

#[test]
fn fs_read_acceptance_does_not_accept_deny_read() {
    let caps = Cap::parse_accept("fs_read").unwrap();
    assert!(caps.iter().all(|c| c.name() != "fs_deny_read"));
    assert_eq!(Cap::parse_accept("fs_deny_read").unwrap().len(), 1);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("secret")).unwrap();
    let p = make(&Lua::new(), tmp.path(), &["secret"]).unwrap();
    assert!(p.missing(Backend::Landlock { partial: false }).iter().any(|c| c.name() == "fs_deny_read"));
    let (ok, line) = deny_read::explicit_capability_line(Backend::Landlock { partial: false }, &[Cap::FsRead]);
    assert!(!ok && line.contains("fs_deny_read") && line.contains("PARTIAL"));
}

#[test]
fn full_access_table_cannot_silently_discard_denies() {
    let lua = Lua::new();
    let t = lua.create_table().unwrap();
    t.set("level", "danger-full-access").unwrap();
    t.set("deny_read", vec!["secret"]).unwrap();
    assert!(Policy::from_lua(&lua, &Value::Table(t), Path::new("/")).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn strict_spawn_refuses_and_explicit_partial_is_reported() {
    if !require_bwrap_or_skip("strict_spawn_refuses_and_explicit_partial_is_reported") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("secret"), "secret").unwrap();
    let mut p = make(&Lua::new(), tmp.path(), &["secret"]).unwrap().with_level(Level::ReadOnly).unwrap();
    assert!(Prepared::from_policy(p.clone(), tmp.path()).err().unwrap().to_string().contains("fs_deny_read"));
    p.accept_partial.push(Cap::FsDenyRead);
    let prepared = Prepared::from_policy(p, tmp.path()).unwrap();
    let lua = Lua::new();
    let report = prepared.report(&lua).unwrap();
    assert_eq!(report.get::<String>("enforcement").unwrap(), "partial");
    assert!(
        report
            .get::<mlua::Table>("unavailable")
            .unwrap()
            .sequence_values::<String>()
            .any(|v| v.unwrap() == "fs_deny_read")
    );
}

#[test]
fn missing_paths_are_never_dropped_and_require_the_deny_dimension() {
    let tmp = tempfile::tempdir().unwrap();
    let lua = Lua::new();
    let p = make(&lua, tmp.path(), &["later"]);
    // Rejecting an unresolved path is a valid fail-closed result. If accepted, it must remain
    // represented and cause a capability gap instead of becoming an apparently full policy.
    if let Ok(p) = p {
        assert!(p.required().iter().any(|c| c.name() == "fs_deny_read"));
        std::fs::create_dir(tmp.path().join("later")).unwrap();
        assert!(!p.can_read(&tmp.path().join("later/token")));
    }
}

#[test]
fn denies_survive_intersection_and_approved_unconfined_escalation() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("a")).unwrap();
    std::fs::create_dir(tmp.path().join("b")).unwrap();
    let lua = Lua::new();
    let a = make(&lua, tmp.path(), &["a"]).unwrap();
    let b = make(&lua, tmp.path(), &["b"]).unwrap();
    let both = a.intersect(&b);
    assert!(!both.can_read(&tmp.path().join("a/x")));
    assert!(!both.can_read(&tmp.path().join("b/x")));
    let kept = deny_read::decide(None, Some(a), false).expect("deny-read must retain the sandbox");
    assert!(!kept.can_read(&tmp.path().join("a/x")));
}

#[test]
fn empty_input_is_not_silently_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(make(&Lua::new(), tmp.path(), &[""]).is_err());
}

#[test]
fn real_symlinks_and_dotdot_follow_kernel_path_order() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let target = tmp.path().join("outer/target");
    std::fs::create_dir_all(&target).unwrap();
    symlink(&target, tmp.path().join("link")).unwrap();
    let lua = Lua::new();
    let p = make(&lua, tmp.path(), &["link/.."]).unwrap();
    assert!(!p.can_read(&tmp.path().join("outer/target/secret")));
    assert!(p.can_read(&tmp.path().join("sibling")));
}

#[test]
fn a_symlink_retarget_or_replacement_fails_closed() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
    std::fs::create_dir(&a).unwrap();
    std::fs::create_dir(&b).unwrap();
    let link = tmp.path().join("link");
    symlink(&a, &link).unwrap();
    let p = make(&Lua::new(), tmp.path(), &["link"]).unwrap();
    std::fs::remove_file(&link).unwrap();
    symlink(&b, &link).unwrap();
    assert!(p.revalidate().is_err());
    assert!(!p.can_read(&b.join("secret")));
}

#[test]
fn an_escalation_cannot_accept_a_session_deny_gap() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("secret")).unwrap();
    let lua = Lua::new();
    let session = make(&lua, tmp.path(), &["secret"]).unwrap();
    let t = lua.create_table().unwrap();
    t.set("network", true).unwrap();
    t.set("accept_partial", vec!["fs_deny_read"]).unwrap();
    let escalation = Policy::from_lua(&lua, &Value::Table(t), tmp.path()).unwrap().unwrap();
    let kept = deny_read::decide(Some(escalation), Some(session), false).unwrap();
    assert!(kept.accept_partial.iter().all(|c| c.name() != "fs_deny_read"));
    assert!(!kept.can_read(&tmp.path().join("secret/data")));
}

#[test]
fn session_acceptance_does_not_weaken_additional_strict_denies() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join("session")).unwrap();
    std::fs::create_dir(tmp.path().join("nested")).unwrap();
    let lua = Lua::new();
    let mut session = make(&lua, tmp.path(), &["session"]).unwrap();
    session.accept_partial.push(Cap::FsDenyRead);
    let nested = make(&lua, tmp.path(), &["nested"]).unwrap();
    let kept = deny_read::decide(Some(nested), Some(session), false).unwrap();
    assert_eq!(kept.deny_read.len(), 2);
    assert!(!kept.accept_partial.contains(&Cap::FsDenyRead));
}

#[test]
fn unresolved_entries_are_partial_and_do_not_drop_resolved_masks() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("secret"), "hidden").unwrap();
    let p = make(&Lua::new(), tmp.path(), &["secret", "later"]).unwrap();
    assert!(p.missing(Backend::Bwrap).contains(&Cap::FsDenyRead));
    assert!(!p.can_read(&tmp.path().join("secret")));
    std::fs::write(tmp.path().join("later"), "created later").unwrap();
    assert!(!p.can_read(&tmp.path().join("later")));
    let args = p.bwrap_args(tmp.path());
    assert!(args.windows(3).any(|w| w == ["--ro-bind", "/dev/null", &tmp.path().join("secret").to_string_lossy()]));
    assert!(!args.windows(3).any(|w| w == ["--ro-bind", "/dev/null", &tmp.path().join("later").to_string_lossy()]));
}

#[test]
fn seatbelt_unresolved_denies_do_not_become_global_rules() {
    let tmp = tempfile::tempdir().unwrap();
    let lua = Lua::new();
    let mut empty = make(&lua, tmp.path(), &[]).unwrap();
    let mut unresolved = make(&lua, tmp.path(), &["later"]).unwrap();
    assert!(unresolved.missing(Backend::Seatbelt).contains(&Cap::FsDenyRead));
    assert_eq!(unresolved.seatbelt(None), empty.seatbelt(None));
    std::fs::create_dir(tmp.path().join("later")).unwrap();
    assert_eq!(unresolved.seatbelt(None), empty.seatbelt(None));
    assert!(unresolved.missing(Backend::Seatbelt).contains(&Cap::FsDenyRead));
    empty.restrict_read = true;
    unresolved.restrict_read = true;
    assert_eq!(unresolved.seatbelt(None), empty.seatbelt(None));
}

#[test]
fn seatbelt_mixed_denies_keep_only_resolved_path_rules() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("secret"), "hidden").unwrap();
    let lua = Lua::new();
    let resolved = make(&lua, tmp.path(), &["secret"]).unwrap();
    let mixed = make(&lua, tmp.path(), &["secret", "later"]).unwrap();
    assert_eq!(mixed.seatbelt(None), resolved.seatbelt(None));
    assert!(mixed.seatbelt(None).contains(&format!("(subpath \"{}\")", tmp.path().join("secret").display())));
    assert!(mixed.missing(Backend::Seatbelt).contains(&Cap::FsDenyRead));
}

#[test]
fn seatbelt_denies_follow_read_grants_and_raw_profiles_remain_partial() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("secret"), "hidden").unwrap();
    let mut p = make(&Lua::new(), tmp.path(), &["secret"]).unwrap();
    p.restrict_read = true;
    let profile = p.seatbelt(None);
    assert!(profile.rfind("(deny file-read*").unwrap() > profile.rfind("(allow file-read*").unwrap());
    assert!(profile.contains(&format!("(subpath \"{}\")", tmp.path().join("secret").display())));
    p.profile = Some("(version 1)\n(allow default)".into());
    assert_eq!(p.seatbelt(None), p.profile.clone().unwrap());
    assert!(p.missing(Backend::Seatbelt).contains(&Cap::FsDenyRead));
}

#[cfg(target_os = "linux")]
#[cfg(target_os = "linux")]
#[test]
fn bwrap_masks_files_and_directories_after_all_binds() {
    if !require_bwrap_or_skip("bwrap_masks_files_and_directories_after_all_binds") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("private/nested")).unwrap();
    std::fs::write(tmp.path().join("private/nested/token"), "directory secret").unwrap();
    std::fs::write(tmp.path().join("secret"), "file secret").unwrap();
    std::fs::write(tmp.path().join("sibling"), "visible").unwrap();
    let p = make(&Lua::new(), tmp.path(), &["private", "private/nested", "secret"]).unwrap();
    let mut args = p.bwrap_args_with(tmp.path(), Some(&[&tmp.path().join("sibling")]));
    let script = "test ! -s secret && test ! -e private/nested/token && test \"$(cat sibling)\" = visible";
    args.extend(["/bin/sh".into(), "-c".into(), script.into()]);
    let output = std::process::Command::new("bwrap").args(&args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(p.missing(Backend::Bwrap).contains(&Cap::FsDenyRead));
}

#[test]
fn windows_enhanced_w4b_not_applied_and_no_defaults_keeps_host_protect() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let mut p = make(&Lua::new(), ws.path(), &[]).unwrap().with_level(Level::ReadOnly).unwrap();
    p.restrict_read = true;
    // W4(b) read-allowlist inference must NOT satisfy `fs_host_protect` on Backend::WinUser
    // when `host_protect_covered()` is false (enhanced_win adds grants rather than excluding).
    if !crate::winsandbox_user::host_protect_covered() {
        assert!(!p.host_protect_satisfied(Backend::WinUser, &[], false));
    }
    let defaults = crate::winsandbox_user::default_deny_read(home.path(), None, None);
    assert!(
        !defaults.iter().any(|d| d.to_string_lossy().replace('\\', "/").ends_with(".local/state/mare/host")),
        "default_deny_read must not hardcode .local/state/mare/host"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_capability_skip_is_explicit_and_enforced_when_required() {
    use std::process::Command;

    let exe = std::env::current_exe().unwrap();

    // 1. When MARE_TEST_EXPECT_BACKEND=bwrap and bwrap backend is unavailable, test must fail (not silent pass).
    let out_req_bwrap = Command::new(&exe)
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_refuses_relative_symlink_dotdot_mare_home_before_token",
            "--nocapture",
        ])
        .env("MARE_SANDBOX_BACKEND", "none")
        .env("MARE_TEST_EXPECT_BACKEND", "bwrap")
        .output()
        .unwrap();
    assert!(
        !out_req_bwrap.status.success(),
        "bwrap test silently passed despite MARE_TEST_EXPECT_BACKEND=bwrap and MARE_SANDBOX_BACKEND=none"
    );
    let stderr_req_bwrap = String::from_utf8_lossy(&out_req_bwrap.stderr);
    assert!(
        stderr_req_bwrap.contains("MARE_TEST_EXPECT_BACKEND"),
        "expected MARE_TEST_EXPECT_BACKEND failure message, got stderr:\n{stderr_req_bwrap}"
    );

    // 2. When MARE_TEST_EXPECT_BACKEND is unset and bwrap is unavailable, skip must be explicitly marked.
    let out_skip_bwrap = Command::new(&exe)
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_refuses_relative_symlink_dotdot_mare_home_before_token",
            "--nocapture",
        ])
        .env("MARE_SANDBOX_BACKEND", "none")
        .env_remove("MARE_TEST_EXPECT_BACKEND")
        .env_remove("CI")
        .output()
        .unwrap();
    assert!(out_skip_bwrap.status.success(), "bwrap skip path failed unexpectedly: {out_skip_bwrap:?}");
    let stderr_skip_bwrap = String::from_utf8_lossy(&out_skip_bwrap.stderr);
    assert!(
        stderr_skip_bwrap
            .contains("SKIPPED[capability=bwrap]: bwrap_refuses_relative_symlink_dotdot_mare_home_before_token"),
        "expected explicit SKIPPED[capability=bwrap] marker, got stderr:\n{stderr_skip_bwrap}"
    );

    // 3. When bwrap works but unshare is unavailable, MARE_TEST_REQUIRE_UNSHARE=1 must fail and unset must mark skip.
    if backend() == Backend::Bwrap {
        let find_bin = |name: &str| {
            std::env::var_os("PATH")
                .and_then(|p| std::env::split_paths(&p).map(|dir| dir.join(name)).find(|cand| cand.is_file()))
                .unwrap_or_else(|| panic!("{name} binary must exist in PATH when backend() == Backend::Bwrap"))
        };
        let fake_bin = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(find_bin("bwrap"), fake_bin.path().join("bwrap")).unwrap();
        std::os::unix::fs::symlink(find_bin("true"), fake_bin.path().join("true")).unwrap();

        let out_req_unshare = Command::new(&exe)
            .args([
                "--exact",
                "ossandbox::host_protection_tests::bwrap_refuses_restrict_read_when_mount_inspection_fails_with_alias",
                "--nocapture",
            ])
            .env("PATH", fake_bin.path())
            .env("MARE_TEST_REQUIRE_UNSHARE", "1")
            .env_remove("MARE_TEST_EXPECT_BACKEND")
            .env_remove("CI")
            .output()
            .unwrap();
        assert!(
            !out_req_unshare.status.success(),
            "unshare test silently passed despite MARE_TEST_REQUIRE_UNSHARE=1 and missing unshare"
        );
        let stderr_req_unshare = String::from_utf8_lossy(&out_req_unshare.stderr);
        assert!(
            stderr_req_unshare.contains("MARE_TEST_REQUIRE_UNSHARE"),
            "expected MARE_TEST_REQUIRE_UNSHARE failure message, got stderr:\n{stderr_req_unshare}"
        );

        let out_skip_unshare = Command::new(&exe)
            .args([
                "--exact",
                "ossandbox::host_protection_tests::bwrap_refuses_restrict_read_when_mount_inspection_fails_with_alias",
                "--nocapture",
            ])
            .env("PATH", fake_bin.path())
            .env_remove("MARE_TEST_REQUIRE_UNSHARE")
            .env_remove("MARE_TEST_EXPECT_BACKEND")
            .env_remove("CI")
            .output()
            .unwrap();
        assert!(out_skip_unshare.status.success(), "unshare skip path failed unexpectedly: {out_skip_unshare:?}");
        let stderr_skip_unshare = String::from_utf8_lossy(&out_skip_unshare.stderr);
        assert!(
            stderr_skip_unshare.contains(
                "SKIPPED[capability=unshare]: bwrap_refuses_restrict_read_when_mount_inspection_fails_with_alias"
            ),
            "expected explicit SKIPPED[capability=unshare] marker, got stderr:\n{stderr_skip_unshare}"
        );
    }

    // 4. Child-branch of subprocess security tests must fail (not silently skip) when backend is unavailable.
    let child_ws = tempfile::tempdir().unwrap();
    let child_home = child_ws.path().join("mare-home");
    std::fs::create_dir_all(&child_home).unwrap();
    let out_child_no_bwrap = Command::new(&exe)
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_masks_host_token_dir_even_when_token_created_after_start",
            "--nocapture",
        ])
        .env("MARE_TEST_BWRAP_CREATED_AFTER_START", child_ws.path())
        .env("MARE_HOME", &child_home)
        .env("MARE_SANDBOX_BACKEND", "none")
        .env_remove("MARE_TEST_EXPECT_BACKEND")
        .env_remove("CI")
        .output()
        .unwrap();
    assert!(
        !out_child_no_bwrap.status.success(),
        "subprocess child branch must fail instead of silently skipping when backend is unavailable"
    );
}
