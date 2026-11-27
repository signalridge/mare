//! Protected metadata target (`.mare` / `.git`) lifecycle, concurrency, and cleanup.

use super::host;
#[cfg(target_os = "linux")]
use super::mechanism;
use crate::common::*;
#[cfg(target_os = "linux")]
use crate::suite::sandbox_expect::require_backend;

/// A missing .mare remains protected without blocking ordinary sandboxed commands.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn missing_mare_is_protected_for_processes_and_file_tools() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "missing_mare_is_protected_for_processes_and_file_tools", &["bwrap"]).await { return h.shutdown().await; }
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let write_ok: bool = eval(&h, r#"
            return mare.exec.run({ cwd = CWD, sandbox = { level = "workspace-write", network = true } }, function()
                return pcall(mare.fs.write, CWD .. "/.mare/init.lua", "malicious")
            end)"#).await;
        if env.cwd.join(".mare").exists() { std::fs::remove_dir_all(env.cwd.join(".mare")).unwrap(); }
        let (ordinary, attempt): (i64, i64) = eval(&h, r#"
            local policy = { level = "workspace-write", network = true }
            local ordinary = mare.proc.run({ cmd = "sh", args = { "-c", "printf ok > ordinary.txt" }, sandbox = policy })
            local attempt = mare.proc.run({ cmd = "sh", args = { "-c", "mkdir -p .mare; printf bad > .mare/init.lua" }, sandbox = policy })
            return ordinary.code, attempt.code"#).await;
        assert!(!write_ok, "file tool must not create missing .mare/init.lua");
        assert_eq!(ordinary, 0, "ordinary sandbox commands still run");
        assert_ne!(attempt, 0, "sandboxed shell must not create .mare/init.lua");
        assert_eq!(std::fs::read_to_string(env.cwd.join("ordinary.txt")).unwrap(), "ok");
        assert!(!env.cwd.join(".mare/init.lua").exists());
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while env.cwd.join(".mare").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("the synthetic mount point must be removed within two seconds in this normal case");
        h.shutdown().await;
    }).await
}

/// Preflight may briefly create a mount point and must not be available to confined tools.
#[tokio::test]
async fn protection_preflight_cannot_be_invoked_from_a_tool() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        global(&h, "OUTSIDE", outside.path().to_string_lossy().to_string());
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            return mare.exec.run({ sandbox = "read-only" }, function()
                local ok, err = pcall(mare.proc.sandbox.preflight_protection,
                  { level = "workspace-write", cwd = OUTSIDE, network = true })
                return ok, tostring(err)
            end)"#,
        )
        .await;
        assert!(!ok && err.contains("tool call"), "{err}");
        assert!(!outside.path().join(".mare").exists());
        h.shutdown().await;
    })
    .await
}

/// Existing metadata retains path-level protection and does not require a downgrade.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn existing_metadata_runs_with_path_level_protection() {
    local(async {
        for metadata in [".git", ".mare"] {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.cwd.join(metadata)).unwrap();
            std::fs::write(env.cwd.join(metadata).join("state"), "original").unwrap();
            let h = host(&env).await;
            if !require_backend(&h, "existing_metadata_runs_with_path_level_protection", &["bwrap"]).await { return h.shutdown().await; }
            global(&h, "META", metadata.to_string());
            let has_python = std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success());
            global(&h, "HAS_PYTHON", has_python);
            let (code, enforcement, link_code): (i64, String, i64) = eval(&h, r#"
                local script = ("! ln %s/state alias 2>/dev/null && ! printf changed > %s/state 2>/dev/null && printf ok > ordinary"):format(META, META)
                local result = mare.proc.run({cmd = "sh", args = {"-c", script}, sandbox = {level = "workspace-write", network = true}})
                local code = ("import os,errno\ntry: os.link('%s/state','alias')\nexcept OSError as e: assert e.errno == errno.EXDEV, e\nelse: raise AssertionError('link unexpectedly succeeded')"):format(META)
                local link_code = 0
                if HAS_PYTHON then
                    local link = mare.proc.run({cmd = "python3", args = {"-c", code}, sandbox = {level = "workspace-write", network = true}})
                    link_code = link.code
                end
                return result.code, result.sandbox.enforcement, link_code
            "#).await;
            assert_eq!((code, enforcement.as_str(), link_code), (0, "full", 0), "{metadata}");
            assert_eq!(std::fs::read_to_string(env.cwd.join(metadata).join("state")).unwrap(), "original");
            assert_eq!(std::fs::read_to_string(env.cwd.join("ordinary")).unwrap(), "ok");
            assert!(!env.cwd.join("alias").exists());
            h.shutdown().await;
        }
    }).await
}

/// Two live commands share the missing-target lifecycle without sharing writable protection.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn concurrent_missing_metadata_sandboxes_keep_protection_until_both_exit() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "concurrent_missing_metadata_sandboxes_keep_protection_until_both_exit", &["bwrap"]).await { return h.shutdown().await; }
        if env.cwd.join(".mare").exists() {
            std::fs::remove_dir_all(env.cwd.join(".mare")).unwrap();
        }
        let (first, second, result): (String, String, i64) = eval(&h, r#"
            local policy = { level = "workspace-write", network = true }
            local script = "echo ready; read release; if rm -rf .mare 2>/dev/null; then exit 7; fi; printf ok > ordinary; exit 0"
            local function start()
                local p = mare.proc.spawn({cmd = "sh", args = {"-c", script}, stdin = true, sandbox = policy})
                assert(p:read_line() == "ready")
                return p
            end
            local a = start()
            local first = a:sandbox().enforcement
            local b = start()
            local second = b:sandbox().enforcement
            a:kill("KILL")
            a:wait()
            a = nil
            collectgarbage("collect")
            b:write("go\n")
            local result = b:wait().code
            b = nil
            collectgarbage("collect")
            return first, second, result
        "#).await;
        assert_eq!((first.as_str(), second.as_str(), result), ("full", "full", 0));
        assert_eq!(std::fs::read_to_string(env.cwd.join("ordinary")).unwrap(), "ok");
        for _ in 0..100 {
            if !env.cwd.join(".mare").exists() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!env.cwd.join(".mare").exists(), "last holder must clean the temporary target");
        h.shutdown().await;
    }).await
}

/// Cancellation and background job stop must not release another live call's protection.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelled_run_and_stopped_job_keep_other_sandbox_protected() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "cancelled_run_and_stopped_job_keep_other_sandbox_protected", &["bwrap"]).await { return h.shutdown().await; }
        if env.cwd.join(".mare").exists() { std::fs::remove_dir_all(env.cwd.join(".mare")).unwrap(); }
        let (cancelled, stopped, code): (bool, bool, i64) = eval(&h, r#"
            local policy = { level = "workspace-write", network = true }
            local keeper = mare.proc.spawn({ cmd = "sh", args = { "-c",
                "echo ready; read x; if rm -rf .mare 2>/dev/null; then exit 9; fi; exit 0" }, stdin = true, sandbox = policy })
            assert(keeper:read_line() == "ready")
            local tok = mare.task.token()
            mare.task.spawn(function() mare.task.sleep(200); tok:cancel() end)
            local run = mare.proc.run({ cmd = "sleep", args = { "30" }, cancel = tok, sandbox = policy })
            local job = mare.proc.start({ cmd = "sleep", args = { "30" }, sandbox = policy })
            local stop = job:stop({ grace_ms = 50 })
            job = nil
            collectgarbage("collect")
            keeper:write("go\n")
            local code = keeper:wait().code
            keeper = nil
            collectgarbage("collect")
            return run.cancelled, stop.stopped, code
        "#).await;
        assert!(cancelled && stopped);
        assert_eq!(code, 0, "the live sandbox's target remains protected");
        for _ in 0..100 {
            if !env.cwd.join(".mare").exists() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!env.cwd.join(".mare").exists());
        h.shutdown().await;
    }).await
}

/// Upper bound for one host's initialization, before its sandbox readiness budget starts.
#[cfg(target_os = "linux")]
pub(super) const SANDBOX_HOST_STARTUP_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Two separate mare hosts run real sandboxes against the same initially empty workspace.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn separate_hosts_keep_concurrent_sandboxes_protected() {
    if let Some(root) = std::env::var_os("MARE_TEST_SANDBOX_SHARED_WORKSPACE") {
        local(async {
            let mut env = TestEnv::new().await;
            env.cwd = std::path::PathBuf::from(root);
            let label = std::env::var("MARE_TEST_SANDBOX_HOST_LABEL").unwrap();
            let preparing = std::time::Instant::now();
            eprintln!("sandbox host {label} initializing");
            let h = host(&env).await;
            eprintln!("sandbox host {label} initialized after {:?}", preparing.elapsed());
            assert_eq!(mechanism(&h).await, "bwrap");
            global(&h, "LABEL", label.clone());
            std::fs::write(env.cwd.join(format!("initialized-{label}")), "ready").unwrap();
            if label == "crash" {
                let _: i64 = eval(&h, r#"
                    CRASH_CHILD = mare.proc.spawn({cmd = "sh", args = {"-c", "echo ready; sleep 2"}, sandbox = {level = "workspace-write", network = true}})
                    assert(CRASH_CHILD:read_line() == "ready", "sandbox command must be running before host crash")
                    return CRASH_CHILD:pid()
                "#).await;
                unsafe { libc::kill(libc::getpid(), libc::SIGKILL) };
                unreachable!("the crash fixture should be terminated");
            }
            let (code, enforcement): (i64, String) = eval(&h, r#"
                local script = ("touch ready-%s; while [ ! -f release-%s ]; do sleep .02; done; if rm -rf .mare 2>/dev/null; then exit 7; fi; printf ok > wrote-%s"):format(LABEL, LABEL, LABEL)
                local result = mare.proc.run({cmd = "sh", args = {"-c", script}, sandbox = {level = "workspace-write", network = true}})
                return result.code, result.sandbox.enforcement
            "#).await;
            assert_eq!((code, enforcement.as_str()), (0, "full"));
            if label == "b" {
                tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    while env.cwd.join(".mare").exists() {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }).await.expect("the last target must be removed within two seconds in this normal case");
            }
            h.shutdown().await;
        }).await;
        return;
    }
    let usable = std::process::Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--unshare-pid",
            "--proc",
            "/proc",
            "--die-with-parent",
            "--",
            "true",
        ])
        .status()
        .is_ok_and(|s| s.success());
    if !usable {
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let root = dunce::canonicalize(workspace.path()).unwrap();
    // A failed assertion must not leave the host processes (and their sandboxes) running.
    struct Hosts(Vec<std::process::Child>);
    impl Drop for Hosts {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let mut hosts = Hosts(Vec::new());
    let children = &mut hosts.0;
    for label in ["a", "b"] {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "suite::sandbox::protection::separate_hosts_keep_concurrent_sandboxes_protected",
                "--nocapture",
            ])
            .env("MARE_TEST_SANDBOX_SHARED_WORKSPACE", &root)
            .env("MARE_TEST_SANDBOX_HOST_LABEL", label)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        children.push(child);
        // Host initialization (plugin load, state files with fsync) is bounded separately: on a
        // filesystem under IO load it alone can exceed the readiness budget below, which bounds
        // only the sandbox command.
        let initializing = std::time::Instant::now();
        while !root.join(format!("initialized-{label}")).exists() {
            if let Some(status) = children.last_mut().unwrap().try_wait().unwrap() {
                panic!("host {label} exited during initialization: {status}");
            }
            assert!(
                initializing.elapsed() < SANDBOX_HOST_STARTUP_LIMIT,
                "host {label} not initialized after {SANDBOX_HOST_STARTUP_LIMIT:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let waiting = std::time::Instant::now();
        eprintln!("S7 parent waiting for host {label}");
        for _ in 0..500 {
            if root.join(format!("ready-{label}")).exists() {
                break;
            }
            if let Some(status) = children.last_mut().unwrap().try_wait().unwrap() {
                panic!("host {label} exited before ready: {status}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(root.join(format!("ready-{label}")).exists(), "host {label} not ready after {:?}", waiting.elapsed());
        eprintln!("sandbox host {label} command ready after {:?}", waiting.elapsed());
    }
    std::fs::write(root.join("release-a"), "go").unwrap();
    assert!(children[0].wait().unwrap().success());
    assert!(root.join(".mare").is_dir(), "the second host still holds the target");
    std::fs::write(root.join("release-b"), "go").unwrap();
    assert!(children[1].wait().unwrap().success());
    assert!(!root.join(".mare").exists());
    for label in ["a", "b"] {
        assert_eq!(std::fs::read_to_string(root.join(format!("wrote-{label}"))).unwrap(), "ok");
    }
}

/// A sandboxed descendant that starts a new session must not outlive a mare stop.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn stopping_sandbox_also_ends_detached_session() {
    local(async {
        if std::process::Command::new("setsid").arg("--version").output().is_err() { return; }
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "stopping_sandbox_also_ends_detached_session", &["bwrap"]).await { return h.shutdown().await; }
        let ready: bool = eval(&h, r#"
            local script = "setsid sh -c 'touch detached-ready; sleep 1; touch detached-survived' & while [ ! -f detached-ready ]; do sleep .01; done; echo ready; wait"
            local p = mare.proc.spawn({cmd = "sh", args = {"-c", script}, sandbox = {level = "workspace-write", network = true}})
            local ready = p:read_line() == "ready"
            p:kill("KILL")
            p:wait()
            p = nil
            collectgarbage("collect")
            return ready
        "#).await;
        assert!(ready);
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        assert!(!env.cwd.join("detached-survived").exists(), "detached sandbox descendant survived mare stop");
        assert!(!env.cwd.join(".mare").exists(), "the last stopped sandbox releases its target");
        h.shutdown().await;
    }).await
}

/// A completed process should release its target even if a caller still retains its handle.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_process_releases_target_before_handle_is_dropped() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "completed_process_releases_target_before_handle_is_dropped", &["bwrap"]).await { return h.shutdown().await; }
        if env.cwd.join(".mare").exists() { std::fs::remove_dir_all(env.cwd.join(".mare")).unwrap(); }
        let _: i64 = eval(&h, r#"P = mare.proc.spawn({cmd = "true", sandbox = {level = "workspace-write", network = true}}); return P:pid()"#).await;
        for _ in 0..200 {
            if !env.cwd.join(".mare").exists() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!env.cwd.join(".mare").exists(), "completed child retains the temporary target");
        let code: i64 = eval(&h, "return P:wait().code").await;
        assert_eq!(code, 0, "automatic reaping preserves the exit status");
        h.shutdown().await;
    }).await
}

/// A completed terminal retains its exit status without retaining the protection target.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn completed_terminal_releases_target_before_handle_is_dropped() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "completed_terminal_releases_target_before_handle_is_dropped", &["bwrap"]).await { return h.shutdown().await; }
        if env.cwd.join(".mare").exists() { std::fs::remove_dir_all(env.cwd.join(".mare")).unwrap(); }
        let _: i64 = eval(&h, r#"P = mare.pty.spawn({cmd = "sh", args = {"-c", "exit 7"}, sandbox = {level = "workspace-write", network = true}}); return P:pid()"#).await;
        for _ in 0..200 {
            if !env.cwd.join(".mare").exists() { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!env.cwd.join(".mare").exists(), "completed terminal retains the temporary target");
        // Mount cleanup follows the namespace init, which can exit before the outer PTY
        // wrapper. Wait for the wrapper itself before checking its retained exit status.
        let code: i64 = tokio::time::timeout(std::time::Duration::from_secs(2), eval(&h, r#"
            while true do
                local code = P:try_wait()
                if code ~= nil then return code end
                mare.task.sleep(10)
            end
        "#)).await.expect("the terminal wrapper must exit");
        assert_eq!(code, 7, "automatic reaping preserves the terminal exit status");
        h.shutdown().await;
    }).await
}

/// Confirm existing process and terminal wait/kill behavior with protected-target monitoring.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn protected_process_and_terminal_keep_wait_and_kill_semantics() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "protected_process_and_terminal_keep_wait_and_kill_semantics", &["bwrap"]).await {
            return h.shutdown().await;
        }
        let (p1, p2, s1, s2, t1, t2, k1, k2): (i64, i64, i64, i64, i64, i64, i64, i64) = eval(
            &h,
            r#"
            local policy = {level = "workspace-write", network = true}
            local p = mare.proc.spawn({cmd = "sh", args = {"-c", "echo ready; exit 7"}, sandbox = policy})
            assert(p:read_line() == "ready")
            local p1, p2 = p:wait().code, p:wait().code
            local q = mare.proc.spawn({cmd = "sh", args = {"-c", "echo ready; sleep 30"}, sandbox = policy})
            assert(q:read_line() == "ready")
            q:kill("KILL")
            local s1, s2 = q:wait().signal, q:wait().signal
            local function wait_pty(t)
                for _ = 1, 200 do
                    local code = t:try_wait()
                    if code ~= nil then return code end
                    mare.task.sleep(10)
                end
                error("terminal did not exit")
            end
            local t = mare.pty.spawn({cmd = "sh", args = {"-c", "exit 7"}, sandbox = policy})
            local t1, t2 = wait_pty(t), wait_pty(t)
            local k = mare.pty.spawn({cmd = "sh", args = {"-c", "echo ready; sleep 30"}, sandbox = policy})
            assert((k:read() or ""):find("ready", 1, true))
            k:kill()
            local k1, k2 = wait_pty(k), wait_pty(k)
            return p1, p2, s1, s2, t1, t2, k1, k2
        "#,
        )
        .await;
        assert_eq!((p1, p2), (7, 7));
        assert_eq!((s1, s2), (libc::SIGKILL as i64, libc::SIGKILL as i64));
        assert_eq!((t1, t2), (7, 7));
        assert_ne!(k1, 0);
        assert_eq!(k1, k2);
        h.shutdown().await;
    })
    .await
}

/// A finished command returns even if lifecycle visibility becomes unavailable; cleanup waits.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn finished_run_returns_while_protection_cleanup_waits_for_procfs() {
    if std::env::var_os("MARE_TEST_SANDBOX_PROC_CHANGE_CHILD").is_some() {
        local(async {
            let env = TestEnv::new().await;
            let h = host(&env).await;
            if !require_backend(&h, "finished_run_returns_while_protection_cleanup_waits_for_procfs", &["bwrap"]).await { return h.shutdown().await; }
            global(&h, "CWD", env.cwd.to_string_lossy().to_string());
            let can_protect: bool = eval(&h, r#"
                local policy = mare.proc.sandbox.policy({level = "workspace-write", network = true})
                for _, cap in ipairs(policy.unavailable or {}) do if cap == "fs_protect" then return false end end
                return true
            "#).await;
            if !can_protect {
                eprintln!("skipped: full protected-target capability is unavailable in the test namespace");
                return h.shutdown().await;
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(3), eval::<(i64, String)>(
                &h,
                r#"
                mare.task.spawn(function()
                    while not mare.fs.exists(CWD .. "/ready") do mare.task.sleep(10) end
                    local result = mare.proc.run({cmd = "mount", args = {"-o", "remount,hidepid=2", "/proc"}})
                    assert(result.code == 0, "procfs remount must succeed")
                    mare.fs.write(CWD .. "/release", "go")
                end)
                local result = mare.proc.run({cmd = "sh", args = {"-c", "touch ready; while [ ! -f release ]; do sleep .01; done; printf done"}, sandbox = {level = "workspace-write", network = true}})
                return result.code, result.stdout
                "#,
            )).await.expect("finished command must not wait indefinitely for procfs recovery");
            assert_eq!(result, (0, "done".to_string()));
            assert!(env.cwd.join(".mare").is_dir(), "unverified cleanup must retain its target");
            assert!(std::process::Command::new("mount").args(["-o", "remount,hidepid=0", "/proc"]).status().unwrap().success());
            for _ in 0..300 {
                if !env.cwd.join(".mare").exists() { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(!env.cwd.join(".mare").exists(), "cleanup should finish after visibility recovers");
            h.shutdown().await;
        }).await;
        return;
    }
    let args = ["--user", "--map-root-user", "--pid", "--mount", "--fork", "--mount-proc"];
    let usable = std::process::Command::new("unshare")
        .args(args)
        .args(["sh", "-c", "mount -o remount,hidepid=2 /proc && mount -o remount,hidepid=0 /proc"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !usable {
        eprintln!("skipped: a private procfs cannot be remounted");
        return;
    }
    let status = std::process::Command::new("unshare")
        .args(args)
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "suite::sandbox::protection::finished_run_returns_while_protection_cleanup_waits_for_procfs",
            "--nocapture",
        ])
        .env("MARE_TEST_SANDBOX_PROC_CHANGE_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

/// An unrelated process name is opaque bytes and must not block a sandboxed command.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn unrelated_non_utf8_process_does_not_block_sandboxed_run() {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    struct Cleanup(Child);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    local(async {
        if Command::new("python3").arg("--version").output().is_err() { return; }
        let env = TestEnv::new().await;
        let h = host(&env).await;
        if !require_backend(&h, "unrelated_non_utf8_process_does_not_block_sandboxed_run", &["bwrap"]).await { return h.shutdown().await; }
        let can_protect: bool = eval(&h, r#"
            local policy = mare.proc.sandbox.policy({level = "workspace-write", network = true})
            for _, cap in ipairs(policy.unavailable or {}) do if cap == "fs_protect" then return false end end
            return true
        "#).await;
        if !can_protect { eprintln!("skipped: full protected-target capability is unavailable"); return h.shutdown().await; }
        let code = r#"import ctypes,time; assert ctypes.CDLL(None).prctl(15,b'\xff\xfe',0,0,0)==0; print('ready',flush=True); time.sleep(30)"#;
        let mut child = Cleanup(Command::new("python3").args(["-c", code]).stdout(Stdio::piped()).spawn().unwrap());
        let mut line = String::new();
        BufReader::new(child.0.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        assert!(std::str::from_utf8(&std::fs::read(format!("/proc/{}/comm", child.0.id())).unwrap()).is_err());
        let result: (i64, String, String) = eval(&h, r#"
            local result = mare.proc.run({cmd = "sh", args = {"-c", "printf ok"}, sandbox = {level = "workspace-write", network = true}})
            return result.code, result.stdout, result.sandbox.enforcement
        "#).await;
        assert_eq!(result, (0, "ok".into(), "full".into()));
        h.shutdown().await;
    }).await
}
