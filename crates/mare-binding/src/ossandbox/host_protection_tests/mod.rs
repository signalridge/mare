//! Deny-read policy behavior. These use real paths and do not assume a sandbox backend.

use super::*;
use mlua::{Lua, Value};
use std::path::Path;

pub(super) fn make(lua: &Lua, cwd: &Path, paths: &[&str]) -> mlua::Result<Policy> {
    let t = lua.create_table()?;
    t.set("level", "workspace-write")?;
    t.set("network", true)?;
    t.set("deny_read", paths.to_vec())?;
    Ok(Policy::from_lua(lua, &Value::Table(t), cwd)?.unwrap())
}

#[cfg(target_os = "linux")]
fn env_truthy(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !v.is_empty() && v != "0")
}

#[cfg(target_os = "linux")]
pub(super) fn require_bwrap_or_skip(test_name: &str) -> bool {
    expect::require(test_name, backend().name(), &["bwrap"])
}

#[cfg(target_os = "linux")]
fn require_unshare_or_skip(test_name: &str, args: &[&str]) -> bool {
    let probe = std::process::Command::new("unshare").args(args).arg("true").output();
    if probe.as_ref().is_ok_and(|o| o.status.success()) {
        return true;
    }
    let detail = match probe {
        Ok(o) => format!("status={}, stderr={}", o.status, String::from_utf8_lossy(&o.stderr).trim()),
        Err(e) => format!("exec error: {e}"),
    };
    let reason = format!("SKIPPED[capability=unshare]: {test_name} (unshare {} unavailable: {detail})", args.join(" "));
    assert!(
        !env_truthy("MARE_TEST_REQUIRE_UNSHARE") && std::env::var("MARE_TEST_EXPECT_BACKEND").as_deref() != Ok("bwrap"),
        "MARE_TEST_REQUIRE_UNSHARE/MARE_TEST_EXPECT_BACKEND is set, refusing silent skip: {reason}"
    );
    eprintln!("{reason}");
    false
}

fn run_with_mare_home_subprocess_at(env_key: &str, test_path: &str, marker: &str) {
    let ws = tempfile::tempdir().unwrap();
    let mare_home = ws.path().join("mare-home");
    std::fs::create_dir_all(&mare_home).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_path, "--nocapture"])
        .env(env_key, ws.path())
        .env("MARE_HOME", &mare_home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "{test_path} child failed: status={:?}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        out.status,
    );
    for line in stderr.lines().filter(|l| l.contains("SKIPPED") || l.contains("N/A[")) {
        eprintln!("{line}");
    }
    assert!(
        stdout.contains(marker),
        "{test_path} child did not complete assertions (missing {marker}):\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

#[cfg(target_os = "linux")]
fn run_with_mare_home_subprocess(env_key: &str, test_name: &str, marker: &str) {
    run_with_mare_home_subprocess_at(env_key, &format!("ossandbox::host_protection_tests::{test_name}"), marker);
}

#[cfg(target_os = "linux")]
pub(super) use super::SANDBOX_INSUFFICIENT;

#[cfg(target_os = "linux")]
fn spawn_landlock_wrapped(wrapped: super::prepared::Wrapped, cwd: &std::path::Path) -> std::process::Output {
    use std::os::unix::process::CommandExt;
    let mut cmd = std::process::Command::new(&wrapped.prog);
    cmd.args(&wrapped.args).current_dir(cwd).env_clear();
    for (k, v) in &wrapped.env {
        cmd.env(k, v);
    }
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    let landlock = wrapped.landlock.clone();
    let filter = wrapped.socket_block.any().then(|| super::seccomp_filter(wrapped.socket_block).unwrap());
    // SAFETY: `child_restrict` only invokes Landlock/prctl syscalls in the child before exec.
    unsafe {
        cmd.pre_exec(move || super::child_restrict(landlock.as_ref(), filter.as_deref()));
    }
    cmd.output().unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_masks_host_token_dir_even_when_token_created_after_start() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    const CHILD: &str = "MARE_TEST_BWRAP_CREATED_AFTER_START";
    let Ok(ws_str) = std::env::var(CHILD) else {
        if !require_bwrap_or_skip("bwrap_masks_host_token_dir_even_when_token_created_after_start") {
            return;
        }
        run_with_mare_home_subprocess(
            CHILD,
            "bwrap_masks_host_token_dir_even_when_token_created_after_start",
            "CREATED_AFTER_START_VERIFIED",
        );
        return;
    };
    assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
    let ws = std::path::Path::new(&ws_str);
    assert!(Backend::Bwrap.caps().contains(&Cap::FsHostProtect), "bwrap must declare fs_host_protect after probe");

    let token_file = mare_shared::paths::host_token_file();
    let host_dir = token_file.parent().unwrap().to_path_buf();
    let temp_token_file = host_dir.join(".token.1234.tmp");
    let state_dir = host_dir.parent().unwrap().to_path_buf();
    std::fs::create_dir_all(&state_dir).unwrap();
    let control_file = state_dir.join("control.txt");
    std::fs::write(&control_file, "control-visible").unwrap();
    assert!(!token_file.exists(), "token file must not exist before sandbox starts");

    let p = make(&Lua::new(), ws, &[]).unwrap();
    let mut args = p.bwrap_args(ws);
    args.extend([
        "--".into(),
        "/bin/sh".into(),
        "-c".into(),
        "echo READY; read _; echo CTRL=$(cat \"$1\" 2>&1); cat \"$2\" 2>&1 || echo TOK_DENIED; cat \"$3\" 2>&1 || echo TMP_DENIED".into(),
        "sh".into(),
        control_file.to_string_lossy().into_owned(),
        token_file.to_string_lossy().into_owned(),
        temp_token_file.to_string_lossy().into_owned(),
    ]);

    let mut child = Command::new("bwrap")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    stdout.read_line(&mut ready).unwrap();
    assert_eq!(ready.trim(), "READY", "sandboxed process must start before token creation");

    // Now create the token file AND a rotation temp file on the host while the sandbox is running.
    std::fs::write(&token_file, "host-secret-created-after-sandbox-start").unwrap();
    std::fs::write(&temp_token_file, "host-temp-secret-created-after-sandbox-start").unwrap();

    // Release the sandboxed process to read the control file and attempt reading both secret files.
    child.stdin.take().unwrap().write_all(b"go\n").unwrap();

    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stdout, &mut rest).unwrap();
    let _status = child.wait().unwrap();

    assert!(rest.contains("CTRL=control-visible"), "control file in bound workspace must be readable: {rest:?}");
    assert!(rest.contains("TOK_DENIED"), "token file must be unreadable: {rest:?}");
    assert!(rest.contains("TMP_DENIED"), "rotation temp file must be unreadable: {rest:?}");
    assert!(
        !rest.contains("host-secret-created-after-sandbox-start"),
        "sandboxed process leaked host token created after start: {rest:?}"
    );
    assert!(
        !rest.contains("host-temp-secret-created-after-sandbox-start"),
        "sandboxed process leaked rotation temp token created after start: {rest:?}"
    );
    println!("CREATED_AFTER_START_VERIFIED");
}

#[cfg(target_os = "linux")]
#[test]
fn fs_host_protect_gates_landlock_and_winacl_when_token_exists() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "MARE_TEST_LANDLOCK_HOST_PROTECT";
    if let Ok(ws_str) = std::env::var(CHILD) {
        let ws = std::path::PathBuf::from(ws_str);
        let token_file = mare_shared::paths::host_token_file();
        let host_dir = token_file.parent().unwrap().to_path_buf();
        let p = make(&Lua::new(), &ws, &[]).unwrap().with_level(Level::ReadOnly).unwrap();

        if !expect::require(
            "fs_host_protect_gates_landlock_and_winacl_when_token_exists",
            backend().name(),
            &["landlock"],
        ) {
            std::process::exit(42);
        }
        assert!(matches!(backend(), Backend::Landlock { .. }));
        assert!(!backend().caps().contains(&Cap::FsHostProtect));

        // 1. Token absent -> policy does not require fs_host_protect, real restricted command runs
        //    and cannot write inside ReadOnly workspace.
        assert_eq!(
            super::protected::secret_status(&super::protected::host_protected()[0]),
            super::protected::SecretStatus::Absent
        );
        assert!(!p.required().contains(&Cap::FsHostProtect));
        let prep_absent = Prepared::from_policy(p.clone(), &ws).unwrap();
        assert_ne!(prep_absent.report(&Lua::new()).unwrap().get::<String>("enforcement").unwrap(), "partial");
        let denied_file = ws.join("readonly_denied.txt");
        let cmd_absent = format!("echo ABSENT_OK; touch {} && echo WROTE_UNEXPECTED", denied_file.display());
        let out_absent = spawn_landlock_wrapped(prep_absent.wrap("/bin/sh", &["-c".into(), cmd_absent]), &ws);
        assert!(!out_absent.status.success(), "ReadOnly write must fail: {out_absent:?}");
        assert!(String::from_utf8_lossy(&out_absent.stdout).contains("ABSENT_OK"));
        assert!(!String::from_utf8_lossy(&out_absent.stdout).contains("WROTE_UNEXPECTED"));
        assert!(!denied_file.exists());

        // 2. Producer gate and rotate_host_token_with_partial refuse without explicit accept_partial, writing NO files.
        let err = super::protected::producer_gate(false).unwrap_err();
        assert!(err.contains("fs_host_protect"), "{err}");
        let rot_err = crate::http_serve::rotate_host_token_with_partial(false).unwrap_err();
        assert!(rot_err.contains("fs_host_protect"), "{rot_err}");
        assert!(!host_dir.exists(), "producer refusal must not create host dir");
        assert!(super::protected::producer_gate(true).is_ok());
        assert!(crate::http_serve::rotate_host_token_with_partial(true).is_ok());
        assert!(token_file.exists());
        std::fs::remove_dir_all(&host_dir).unwrap();

        // 3. Unreadable empty host_dir (W3 Unknown) -> treated as Present -> refuses unless accepted.
        std::fs::create_dir_all(&host_dir).unwrap();
        std::fs::set_permissions(&host_dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(
            super::protected::secret_status(&super::protected::host_protected()[0]),
            super::protected::SecretStatus::Unknown
        );
        assert!(p.required().contains(&Cap::FsHostProtect));
        let err_unknown = Prepared::from_policy(p.clone(), &ws).err().unwrap().to_string();
        assert!(err_unknown.contains("fs_host_protect"), "{err_unknown}");
        std::fs::set_permissions(&host_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        // 4. Once token exists, policy requires fs_host_protect and Landlock refuses unless accepted.
        std::fs::write(&token_file, "secret").unwrap();
        assert!(p.required().contains(&Cap::FsHostProtect));
        assert!(p.missing(Backend::WinAcl).contains(&Cap::FsHostProtect));
        let err = Prepared::from_policy(p.clone(), &ws).err().unwrap().to_string();
        assert!(err.contains("fs_host_protect"), "{err}");

        let mut accepted = p.clone();
        accepted.accept_partial.push(Cap::FsHostProtect);
        let prepared = Prepared::from_policy(accepted, &ws).unwrap();
        let lua = Lua::new();
        let report = prepared.report(&lua).unwrap();
        assert_eq!(report.get::<String>("enforcement").unwrap(), "partial");
        assert!(
            report
                .get::<mlua::Table>("unavailable")
                .unwrap()
                .sequence_values::<String>()
                .any(|v| v.unwrap() == "fs_host_protect")
        );
        let out_partial =
            spawn_landlock_wrapped(prepared.wrap("/bin/sh", &["-c".into(), "echo PARTIAL_OK".into()]), &ws);
        assert!(out_partial.status.success(), "landlock partial run failed: {out_partial:?}");
        assert!(String::from_utf8_lossy(&out_partial.stdout).contains("PARTIAL_OK"));

        // 5. W4(b): Read-allowlist policy whose writable root covers MARE_HOME must NOT be satisfied.
        let mut restricted_covering_home = make(&Lua::new(), &ws, &[]).unwrap();
        restricted_covering_home.restrict_read = true;
        let err_covered = Prepared::from_policy(restricted_covering_home, &ws).err().unwrap().to_string();
        assert!(err_covered.contains("fs_host_protect"), "writable root covering MARE_HOME must fail: {err_covered}");

        // 6. W4(b): Read-allowlist policy that does NOT touch MARE_HOME is satisfied without PARTIAL.
        let outside_ws = tempfile::tempdir().unwrap();
        let mut restricted = make(&Lua::new(), outside_ws.path(), &[]).unwrap().with_level(Level::ReadOnly).unwrap();
        restricted.restrict_read = true;
        let prep_read = Prepared::from_policy(restricted, outside_ws.path()).unwrap();
        assert_ne!(prep_read.report(&Lua::new()).unwrap().get::<String>("enforcement").unwrap(), "partial");
        let out_read = spawn_landlock_wrapped(
            prep_read.wrap("/bin/sh", &["-c".into(), "echo READMASK_OK".into()]),
            outside_ws.path(),
        );
        assert!(out_read.status.success(), "landlock readmask run failed: {out_read:?}");
        assert!(String::from_utf8_lossy(&out_read.stdout).contains("READMASK_OK"));
        println!("LANDLOCK_HOST_PROTECT_VERIFIED");
        std::process::exit(0);
    }

    let ws = tempfile::tempdir().unwrap();
    let mare_home = ws.path().join("mare-home");
    std::fs::create_dir_all(&mare_home).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ossandbox::host_protection_tests::fs_host_protect_gates_landlock_and_winacl_when_token_exists",
            "--nocapture",
        ])
        .env(CHILD, ws.path())
        .env("MARE_SANDBOX_BACKEND", "landlock")
        .env("MARE_HOME", &mare_home)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.code() == Some(42) {
        for line in stderr.lines().filter(|l| l.contains("SKIPPED[") || l.contains("N/A[")) {
            eprintln!("{line}");
        }
        let require_landlock =
            crate::ossandbox::landlock_abi().is_some() || std::env::var_os("MARE_TEST_REQUIRE_LANDLOCK").is_some();
        assert!(!require_landlock, "Landlock is expected in this environment, but child skipped: {stderr}");
        return;
    }
    assert!(out.status.success(), "landlock child failed: {out:?}\nstderr:\n{stderr}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("LANDLOCK_HOST_PROTECT_VERIFIED"),
        "landlock child did not complete assertions:\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_treats_dev_shm_unmaskable_path_as_gap_even_before_token_created() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    const CHILD: &str = "MARE_TEST_BWRAP_DEV_SHM_GAP";
    if let Ok(ws_str) = std::env::var(CHILD) {
        assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
        let ws = std::path::Path::new(&ws_str);
        let token_file = mare_shared::paths::host_token_file();
        let p = make(&Lua::new(), ws, &[]).unwrap();

        // Token is absent, yet path under /dev/shm cannot be masked -> both producer_gate and Prepared refuse.
        let gate_err = super::protected::producer_gate(false).unwrap_err();
        assert!(gate_err.contains("fs_host_protect"), "{gate_err}");
        let prep_err = Prepared::from_policy(p.clone(), ws).err().unwrap().to_string();
        assert!(prep_err.contains("fs_host_protect"), "{prep_err}");

        // With explicit accept_partial, Prepared reports PARTIAL and producer_gate(true) succeeds.
        let mut accepted = p;
        accepted.accept_partial.push(Cap::FsHostProtect);
        let prep = Prepared::from_policy(accepted, ws).unwrap();
        assert_eq!(prep.report(&Lua::new()).unwrap().get::<String>("enforcement").unwrap(), "partial");
        assert!(super::protected::producer_gate(true).is_ok());

        // Start bwrap before token exists, create token after start -> confirms /dev/shm is exposed!
        let script = format!("echo READY; read _go; cat \"{}\"", token_file.display());
        let w = prep.wrap("/bin/sh", &["-c".into(), script]);
        let mut child =
            Command::new(&w.prog).args(&w.args).stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "READY");
        let tok = crate::http_serve::ensure_host_token_at(&token_file).unwrap();
        writeln!(child.stdin.as_mut().unwrap(), "go").unwrap();
        let mut read_tok = String::new();
        stdout.read_line(&mut read_tok).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success());
        assert_eq!(read_tok.trim(), tok);
        println!("DEV_SHM_GAP_VERIFIED");
        return;
    }

    if !require_bwrap_or_skip("bwrap_treats_dev_shm_unmaskable_path_as_gap_even_before_token_created") {
        return;
    }
    let Ok(shm_ws) = tempfile::tempdir_in("/dev/shm") else {
        eprintln!("SKIPPED: /dev/shm not writable");
        return;
    };
    let mare_home = shm_ws.path().join("mare-home");
    std::fs::create_dir_all(&mare_home).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_treats_dev_shm_unmaskable_path_as_gap_even_before_token_created",
            "--nocapture",
        ])
        .env(CHILD, shm_ws.path())
        .env("MARE_HOME", &mare_home)
        .output()
        .unwrap();
    assert!(out.status.success(), "dev/shm child failed: {out:?}\nstderr:\n{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("DEV_SHM_GAP_VERIFIED"),
        "dev/shm child did not complete assertions:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_blocks_relative_mare_home_parent_rename_and_refuses_relative_symlink() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    const CHILD: &str = "MARE_TEST_BWRAP_RELATIVE_MARE_HOME";
    if let Ok(ws_str) = std::env::var(CHILD) {
        // the child never skips: the parent already probed bwrap and requires it here
        assert_eq!(backend(), Backend::Bwrap, "relative MARE_HOME child needs bwrap");
        let ws = std::path::Path::new(&ws_str);
        std::env::set_current_dir(ws).unwrap();

        // 1. Relative symlink `MARE_HOME=link-home` inside writable root must be refused.
        std::fs::create_dir_all(ws.join("real-target")).unwrap();
        std::os::unix::fs::symlink("real-target", ws.join("link-home")).unwrap();
        // SAFETY: single-threaded test subprocess, no concurrent env readers.
        unsafe { std::env::set_var("MARE_HOME", "link-home") };
        let p = make(&Lua::new(), ws, &[]).unwrap();
        let err = Prepared::from_policy(p.clone(), ws).err().unwrap().to_string();
        assert!(err.contains("fs_host_protect"), "relative symlink MARE_HOME must be refused: {err}");

        // 2. Relative real dir `MARE_HOME=rel-home` inside writable root:
        //    real bwrap child barrier test before token exists.
        std::fs::create_dir_all(ws.join("rel-home")).unwrap();
        // SAFETY: single-threaded test subprocess, no concurrent env readers.
        unsafe { std::env::set_var("MARE_HOME", "rel-home") };
        let prep = Prepared::from_policy(p, ws).unwrap();
        let evil_dir = ws.join("evil");
        std::fs::create_dir_all(evil_dir.join("host")).unwrap();
        let token_file = ws.join("rel-home/state/host/token");
        let script = format!(
            "set +e; \
             mv \"{ws}/rel-home/state\" \"{ws}/rel-home/state.old\" 2>/dev/null && ln -s \"{evil}\" \"{ws}/rel-home/state\"; \
             mv \"{ws}/rel-home\" \"{ws}/rel-home.old\" 2>/dev/null && ln -s \"{evil}\" \"{ws}/rel-home\"; \
             echo READY; \
             read _go; \
             test ! -r \"{tok}\" && test ! -r \"{evil}/host/token\" && echo BLOCKED",
            ws = ws.display(),
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

        let tok = crate::http_serve::ensure_host_token_at(&token_file).unwrap();
        assert_eq!(tok.len(), 43);

        writeln!(child.stdin.as_mut().unwrap(), "go").unwrap();
        let mut rest = String::new();
        stdout.read_line(&mut rest).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "bwrap relative MARE_HOME barrier failed: {status:?}");
        assert_eq!(rest.trim(), "BLOCKED");
        // printed only after every assertion above held; the parent requires it
        println!("RELATIVE_MARE_HOME_VERIFIED");
        return;
    }

    if !require_bwrap_or_skip("bwrap_blocks_relative_mare_home_parent_rename_and_refuses_relative_symlink") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = dunce::canonicalize(tmp.path()).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_blocks_relative_mare_home_parent_rename_and_refuses_relative_symlink",
            "--nocapture",
        ])
        .env(CHILD, &ws)
        .env("MARE_TEST_EXPECT_BACKEND", "bwrap")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "relative MARE_HOME subprocess failed: {out:?}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("RELATIVE_MARE_HOME_VERIFIED"),
        "the child did not complete its assertions:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_masks_and_locks_bind_mount_aliases_of_host_protected() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};

    const CHILD: &str = "MARE_TEST_BWRAP_BIND_ALIAS_HOST_PROTECT";
    if let Ok(base_str) = std::env::var(CHILD) {
        assert_eq!(backend(), Backend::Bwrap, "bind-alias child must run under bwrap backend");
        let base = std::path::Path::new(&base_str);
        let srv_app = base.join("srv-app");
        let work_alias = base.join("work-alias");
        let evil_dir = work_alias.join("evil");
        std::fs::create_dir_all(&srv_app).unwrap();
        std::fs::create_dir_all(&work_alias).unwrap();

        assert!(
            Command::new("mount").arg("--bind").arg(&srv_app).arg(&work_alias).status().unwrap().success(),
            "mount --bind failed"
        );
        std::fs::create_dir_all(evil_dir.join("host")).unwrap();
        // SAFETY: single-threaded test subprocess, no concurrent env readers.
        unsafe { std::env::set_var("MARE_HOME", &srv_app) };

        let p = make(&Lua::new(), &work_alias, &[]).unwrap();
        let prep = Prepared::from_policy(p, &work_alias).unwrap();

        // Real bwrap child: verify evil_dir is visible/writable inside sandbox, assert parent
        // directory rename fails, and verify token is masked via both original and alias paths
        // after token creation.
        let orig_token = srv_app.join("state/host/token");
        let alias_token = work_alias.join("state/host/token");
        let script = format!(
            "set -e; \
             test -d \"{evil}\" && touch \"{evil}/probe\"; \
             if mv \"{work}/state\" \"{work}/state.old\" 2>/dev/null; then \
                 echo RENAME_SUCCEEDED; \
                 exit 1; \
             fi; \
             echo RENAME_BLOCKED; \
             echo READY; \
             read _go; \
             test ! -r \"{orig}\" && test ! -r \"{alias}\" && test ! -r \"{evil}/host/token\" && echo BLOCKED",
            work = work_alias.display(),
            evil = evil_dir.display(),
            orig = orig_token.display(),
            alias = alias_token.display(),
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
        let mut rename_line = String::new();
        stdout.read_line(&mut rename_line).unwrap();
        assert_eq!(rename_line.trim(), "RENAME_BLOCKED", "alias parent dir rename must fail");
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "READY", "bwrap bind-alias child did not reach READY barrier");

        let tok = crate::http_serve::ensure_host_token_at(&orig_token).unwrap();
        assert_eq!(tok.len(), 43);

        writeln!(child.stdin.as_mut().unwrap(), "go").unwrap();
        let mut rest = String::new();
        stdout.read_line(&mut rest).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "bwrap bind-alias child escaped: {status:?}");
        assert_eq!(rest.trim(), "BLOCKED");
        println!("BIND_ALIAS_VERIFIED");
        return;
    }

    if !require_bwrap_or_skip("bwrap_masks_and_locks_bind_mount_aliases_of_host_protected") {
        return;
    }
    if !require_unshare_or_skip(
        "bwrap_masks_and_locks_bind_mount_aliases_of_host_protected",
        &["--user", "--map-root-user", "--mount"],
    ) {
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_masks_and_locks_bind_mount_aliases_of_host_protected",
            "--nocapture",
        ])
        .env(CHILD, &base)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "bind-alias subprocess failed: {out:?}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("BIND_ALIAS_VERIFIED"),
        "bind-alias subprocess did not complete assertions:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_refuses_relative_symlink_dotdot_mare_home_before_token() {
    const CHILD: &str = "MARE_TEST_BWRAP_SYMLINK_DOTDOT_MARE_HOME";
    if let Ok(ws_str) = std::env::var(CHILD) {
        assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
        let ws = std::path::Path::new(&ws_str);
        std::env::set_current_dir(ws).unwrap();

        std::fs::create_dir_all(ws.join("safe/sub")).unwrap();
        std::fs::create_dir_all(ws.join("safe/app")).unwrap();
        std::os::unix::fs::symlink(ws.join("safe/sub"), ws.join("link")).unwrap();
        // SAFETY: single-threaded test subprocess, no concurrent env readers.
        unsafe { std::env::set_var("MARE_HOME", "link/../app") };

        let p = make(&Lua::new(), ws, &[]).unwrap();
        let err = Prepared::from_policy(p, ws)
            .err()
            .expect("relative MARE_HOME=link/../app crossing symlink inside writable root must be refused")
            .to_string();
        assert!(err.contains("fs_host_protect"), "expected fs_host_protect refusal, got: {err}");
        println!("DOTDOT_SYMLINK_REFUSED");
        return;
    }

    if !require_bwrap_or_skip("bwrap_refuses_relative_symlink_dotdot_mare_home_before_token") {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = dunce::canonicalize(tmp.path()).unwrap();
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_refuses_relative_symlink_dotdot_mare_home_before_token",
            "--nocapture",
        ])
        .env(CHILD, &ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "dotdot symlink MARE_HOME subprocess failed: {out:?}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("DOTDOT_SYMLINK_REFUSED"),
        "child did not complete assertions:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bwrap_refuses_restrict_read_when_mount_inspection_fails_with_alias() {
    use std::process::Command;

    const CHILD: &str = "MARE_TEST_BWRAP_MOUNT_FAIL_RESTRICT_READ_ALIAS";
    if let Ok(base_str) = std::env::var(CHILD) {
        assert_eq!(backend(), Backend::Bwrap, "child must run under bwrap backend");
        let base = std::path::Path::new(&base_str);
        let srv_app = base.join("srv-app");
        let work_alias = base.join("work-alias");
        std::fs::create_dir_all(&srv_app).unwrap();
        std::fs::create_dir_all(&work_alias).unwrap();

        assert!(
            Command::new("mount").arg("--bind").arg(&srv_app).arg(&work_alias).status().unwrap().success(),
            "mount --bind failed"
        );
        let token_path = srv_app.join("state/host/token");
        crate::http_serve::ensure_host_token_at(&token_path).unwrap();

        // SAFETY: single-threaded test subprocess, no concurrent env readers.
        unsafe { std::env::set_var("MARE_HOME", &srv_app) };

        // ReadOnly + restrict_read = true with readable = [work_alias] while /proc NSpid mismatches
        // (mount_inspection::protection_aliases fails): W4(b) must NOT fall back to lexical prefix!
        let mut p = make(&Lua::new(), &work_alias, &[]).unwrap().with_level(Level::ReadOnly).unwrap();
        p.restrict_read = true;
        p.readable = vec![work_alias.clone()];
        assert!(
            p.missing(Backend::Bwrap).contains(&Cap::FsHostProtect),
            "Policy::missing must report FsHostProtect when mount inspection fails"
        );
        let err = Prepared::from_policy(p, &work_alias)
            .err()
            .expect("Prepared::from_policy must refuse restrict_read when mount inspection fails")
            .to_string();
        assert!(err.contains("fs_host_protect"), "expected fs_host_protect refusal, got: {err}");
        println!("MOUNT_FAIL_RESTRICT_READ_REFUSED");
        return;
    }

    if !require_bwrap_or_skip("bwrap_refuses_restrict_read_when_mount_inspection_fails_with_alias") {
        return;
    }
    if !require_unshare_or_skip(
        "bwrap_refuses_restrict_read_when_mount_inspection_fails_with_alias",
        &["--user", "--map-root-user", "--mount", "--pid", "--fork"],
    ) {
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--pid", "--fork"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "ossandbox::host_protection_tests::bwrap_refuses_restrict_read_when_mount_inspection_fails_with_alias",
            "--nocapture",
        ])
        .env(CHILD, &base)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "mount-fail restrict_read subprocess failed: {out:?}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("MOUNT_FAIL_RESTRICT_READ_REFUSED"),
        "child did not complete assertions:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

mod host_protect;
mod policy;
