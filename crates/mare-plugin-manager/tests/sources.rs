//! Package sources: transport allowlist, never prompting, timeouts and
//! cancellation, `<host>/<path>` checkouts and moving checkouts from the old layout.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

/// MARE_HOME and the git environment are process-wide: tests in this binary take turns.
static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn head(dir: &Path) -> String {
    let out = Command::new("git").current_dir(dir).args(["rev-parse", "HEAD"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with one file `file` containing `body`, committed on `main`.
fn work_repo(dir: &Path, file: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join(file), body).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

/// A bare repository at `at` (like a hosted one) whose `host.txt` says `body`.
fn bare_repo(at: &Path, body: &str) {
    let work = tempfile::tempdir().unwrap();
    work_repo(work.path(), "host.txt", body);
    std::fs::create_dir_all(at.parent().unwrap()).unwrap();
    git(work.path(), &["clone", "-q", "--bare", ".", &at.to_string_lossy()]);
}

fn file_url(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') { format!("file://{s}") } else { format!("file:///{s}") }
}

fn home() -> tempfile::TempDir {
    let h = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", h.path()) };
    h
}

#[test]
fn remote_helper_transports_are_refused_before_git_runs() {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let scratch = tempfile::tempdir().unwrap();
    let marker = scratch.path().join("pwned");
    let url = format!("ext::sh -c touch% {}", marker.to_string_lossy());
    let cfg =
        serde_json::json!({ "plugins": { "allow_insecure": true }, "packages": [{ "name": "me/evil", "url": url }] });
    let e = mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap_err().to_string();
    assert!(e.contains("never allowed"), "{e}");
    let notices = mare_plugin_manager::install_missing(&cfg, &mut |_| ()).join("\n");
    assert!(notices.contains("never allowed"), "{notices}");
    assert!(!marker.exists());
    // plain http is refused unless allowed
    let cfg = serde_json::json!({ "packages": [{ "name": "me/plain", "url": "http://127.0.0.1:9/r.git" }] });
    let e = mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap_err().to_string();
    assert!(e.contains("allow_insecure"), "{e}");
}

/// Answers every HTTP request with 401 + Basic auth: git would ask for a username.
fn auth_server() -> u16 {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf);
            let _ = c.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"private\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    port
}

#[test]
fn startup_install_never_waits_for_credentials_and_says_how_to_check() {
    if !have_git() {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    // only a recording credential helper from the config (none of the machine's), and a GUI
    // askpass in the environment, as Git Bash on Windows exports (`/etc/profile.d/env.sh`
    // SSH_ASKPASS=git-askpass.exe) and Linux desktops do: git must not run it — it opens a dialog
    // and waits. The helper still runs (stored credentials keep working), told never to prompt
    // (Git Credential Manager: GCM_INTERACTIVE=never)
    let scratch = tempfile::tempdir().unwrap();
    let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let script = |name: &str, body: &str| {
        let p = scratch.path().join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        p
    };
    let asked = scratch.path().join("asked");
    let askpass = script("askpass", &format!("echo asked >> '{}'\necho x", fwd(&asked)));
    let helper_log = scratch.path().join("helper.log");
    let helper = script("helper", &format!("echo \"$1 GCM_INTERACTIVE=$GCM_INTERACTIVE\" >> '{}'", fwd(&helper_log)));
    let global = scratch.path().join("gitconfig");
    std::fs::write(&global, format!("[credential]\n\thelper = \"!'{}'\"\n", fwd(&helper))).unwrap();
    unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", &global);
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("SSH_ASKPASS", fwd(&askpass));
        std::env::set_var("GIT_ASKPASS", fwd(&askpass));
        // a git hook's environment must not point package commands at another repository
        std::env::set_var("GIT_DIR", &global);
    }
    let port = auth_server();
    let private = format!("http://127.0.0.1:{port}/team/private.git");
    let cfg = serde_json::json!({ "plugins": { "allow_insecure": true }, "packages": [
        { "name": "team/private", "url": private },
        { "name": "team/gone", "url": "https://nonexistent.invalid/team/gone.git" },
    ]});
    let t = Instant::now();
    let notices = mare_plugin_manager::install_missing(&cfg, &mut |_| ());
    let elapsed = t.elapsed();
    unsafe {
        std::env::remove_var("GIT_CONFIG_GLOBAL");
        std::env::remove_var("GIT_CONFIG_NOSYSTEM");
        std::env::remove_var("GIT_DIR");
        std::env::remove_var("SSH_ASKPASS");
        std::env::remove_var("GIT_ASKPASS");
    }
    assert!(!asked.exists(), "git ran the askpass program");
    let helper_log = std::fs::read_to_string(&helper_log).unwrap_or_default();
    assert!(helper_log.contains("get GCM_INTERACTIVE=never"), "the credential helper: {helper_log:?}");
    assert!(elapsed < Duration::from_secs(60), "took {elapsed:?}");
    let all = notices.join("\n");
    assert_eq!(notices.len(), 2, "{all}");
    assert!(notices[0].contains("authentication failed") && notices[0].contains("terminal prompts disabled"), "{all}");
    assert!(notices[0].contains(&format!("git ls-remote {private}")), "{all}");
    assert!(notices[1].contains("git ls-remote https://nonexistent.invalid/team/gone.git"), "{all}");
}

#[test]
fn subgroup_short_names_and_the_same_path_on_two_hosts_get_their_own_checkouts() {
    if !have_git() {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let srv = tempfile::tempdir().unwrap();
    let gitlab = srv.path().join("gitlab.com");
    let github = srv.path().join("github.com");
    bare_repo(&gitlab.join("a/x.git"), "gitlab");
    bare_repo(&gitlab.join("group/sub/repo.git"), "subgroup");
    bare_repo(&github.join("a/x.git"), "github");
    let gh_url = file_url(&github.join("a/x.git"));
    let cfg = serde_json::json!({
        "plugins": { "url_format": format!("{}/%s.git", file_url(&gitlab)) },
        "packages": [{ "name": "a/x" }, { "name": "group/sub/repo" }, { "name": gh_url }],
    });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let pkgs = mare_plugin_manager::installed(&cfg);
    assert_eq!(pkgs.len(), 3);
    let body = |i: usize| std::fs::read_to_string(pkgs[i].dir.join("host.txt")).unwrap();
    assert_eq!((body(0).as_str(), body(1).as_str(), body(2).as_str()), ("gitlab", "subgroup", "github"));
    assert_ne!(pkgs[0].dir, pkgs[2].dir);
    assert!(pkgs[1].dir.ends_with("gitlab.com/group/sub/repo"), "{}", pkgs[1].dir.display());
    let lock = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap();
    assert_eq!(lock.packages["a/x"].url, file_url(&gitlab.join("a/x.git")), "the lock keeps the full address");
    assert_eq!(lock.packages[&gh_url].url, gh_url);
}

#[test]
fn status_is_read_only_and_does_not_migrate_old_layout() {
    if !have_git() {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let src = tempfile::tempdir().unwrap();
    work_repo(src.path(), "a.lua", "return {}\n");
    let url = src.path().to_string_lossy().to_string();

    // Even if a checkout exists at the old name-only path, status/installed must not migrate it.
    let old = mare_shared::paths::packages_dir().join("me/old");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    git(src.path(), &["clone", "-q", &url, &old.to_string_lossy()]);
    std::fs::write(old.join("mine.txt"), "kept").unwrap();
    let new_dir = mare_plugin_manager::checkout_dir(&url);

    // Also include a normally installed package so status exercises git inspection too.
    let src2 = tempfile::tempdir().unwrap();
    work_repo(src2.path(), "b.lua", "return {}\n");
    let cfg = serde_json::json!({
        "packages": [
            { "name": "me/old", "url": url },
            { "name": "me/installed", "url": src2.path().to_string_lossy() }
        ]
    });
    mare_plugin_manager::sync(
        &serde_json::json!({ "packages": [{ "name": "me/installed", "url": src2.path().to_string_lossy() }] }),
        &mut |_| (),
    )
    .unwrap();

    #[cfg(unix)]
    struct RestoreMode(PathBuf, std::fs::Permissions);
    #[cfg(unix)]
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            let _ = std::fs::set_permissions(&self.0, self.1.clone());
        }
    }
    #[cfg(unix)]
    let _restore = {
        use std::os::unix::fs::PermissionsExt;
        let pkg_root = mare_shared::paths::packages_dir();
        let orig = std::fs::metadata(&pkg_root).unwrap().permissions();
        std::fs::set_permissions(&pkg_root, std::fs::Permissions::from_mode(0o555)).unwrap();
        RestoreMode(pkg_root, orig)
    };

    let rows = mare_plugin_manager::status(&cfg).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(!rows[0].installed, "old-layout directory must not be migrated or treated as installed");
    assert!(rows[1].installed);
    assert!(old.join("mine.txt").exists(), "old-layout directory must remain untouched");
    assert!(!new_dir.exists(), "status must not create or move into the new checkout path");
}

#[test]
fn restore_ignores_untracked_files() {
    if !have_git() {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let src = tempfile::tempdir().unwrap();
    work_repo(src.path(), "a.lua", "return {}\n");
    let cfg = serde_json::json!({ "packages": [{ "name": "me/u", "url": src.path().to_string_lossy() }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let locked = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/u"].commit.clone();
    let dir = mare_plugin_manager::installed(&cfg).remove(0).dir;
    std::fs::write(src.path().join("b.lua"), "return {}\n").unwrap();
    git(src.path(), &["add", "."]);
    git(src.path(), &["commit", "-q", "-m", "b"]);
    git(&dir, &["fetch", "-q", "origin"]);
    git(&dir, &["checkout", "-q", "--detach", "origin/main"]);
    std::fs::write(dir.join("notes.txt"), "untracked").unwrap();
    mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap();
    assert_eq!(head(&dir), locked, "an untracked file does not block restore");
    assert!(dir.join("notes.txt").exists());
}

/// `safe_clone` always configures `core.autocrlf=false` and `--filter=blob:none`, and
/// passes `--recurse-submodules` only when `submodules` is not `false`.
#[test]
fn clone_sets_autocrlf_false_blob_none_and_respects_submodules_flag() {
    if !have_git() {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let src_with_sub = tempfile::tempdir().unwrap();
    work_repo(src_with_sub.path(), "a.lua", "return {}\n");
    let src_no_sub = tempfile::tempdir().unwrap();
    work_repo(src_no_sub.path(), "b.lua", "return {}\n");

    // Check both default (submodules = true) and explicit submodules = false via git config on the clones.
    let cfg = serde_json::json!({
        "packages": [
            { "name": "me/with-sub", "url": src_with_sub.path().to_string_lossy() },
            { "name": "me/no-sub", "url": src_no_sub.path().to_string_lossy(), "submodules": false },
        ]
    });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let installed = mare_plugin_manager::installed(&cfg);
    assert_eq!(installed.len(), 2);
    for pkg in &installed {
        let autocrlf = Command::new("git")
            .args(["-C", &pkg.dir.to_string_lossy(), "config", "--local", "--get", "core.autocrlf"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&autocrlf.stdout).trim(),
            "false",
            "core.autocrlf=false required on {}",
            pkg.name
        );
        let filter = Command::new("git")
            .args(["-C", &pkg.dir.to_string_lossy(), "config", "--local", "--get", "remote.origin.partialclonefilter"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&filter.stdout).trim(),
            "blob:none",
            "--filter=blob:none required on {}",
            pkg.name
        );
    }

    // Verify `submodules = false` suppresses `--recurse-submodules` by recording git argv on a fresh clone.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bin = tempfile::tempdir().unwrap();
        let argv_log = bin.path().join("git-argv.log");
        let orig_path = std::env::var("PATH").unwrap_or_default();
        let real_git = std::env::split_paths(&orig_path)
            .map(|d| d.join("git"))
            .find(|p| p.is_file())
            .expect("git must exist on PATH");
        let wrapper = bin.path().join("git");
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {:?}\nexec {:?} \"$@\"\n",
                argv_log.display().to_string(),
                real_git.display().to_string()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        struct RestorePath(String);
        impl Drop for RestorePath {
            fn drop(&mut self) {
                unsafe { std::env::set_var("PATH", &self.0) };
            }
        }
        let _restore_path = RestorePath(orig_path.clone());
        unsafe { std::env::set_var("PATH", format!("{}:{orig_path}", bin.path().display())) };

        let src_a = tempfile::tempdir().unwrap();
        work_repo(src_a.path(), "a.lua", "return {}\n");
        let src_b = tempfile::tempdir().unwrap();
        work_repo(src_b.path(), "b.lua", "return {}\n");
        let cfg2 = serde_json::json!({
            "packages": [
                { "name": "me/flag-on", "url": src_a.path().to_string_lossy() },
                { "name": "me/flag-off", "url": src_b.path().to_string_lossy(), "submodules": false },
            ]
        });
        mare_plugin_manager::sync(&cfg2, &mut |_| ()).unwrap();
        let log = std::fs::read_to_string(&argv_log).unwrap();
        let clone_lines: Vec<&str> =
            log.lines().filter(|l| l.contains("clone ") && l.contains("--filter=blob:none")).collect();
        let on_line = clone_lines
            .iter()
            .find(|l| l.contains(&src_a.path().to_string_lossy().to_string()))
            .unwrap_or_else(|| panic!("missing clone line for flag-on: {log}"));
        let off_line = clone_lines
            .iter()
            .find(|l| l.contains(&src_b.path().to_string_lossy().to_string()))
            .unwrap_or_else(|| panic!("missing clone line for flag-off: {log}"));
        assert!(on_line.contains("core.autocrlf=false") && on_line.contains("--recurse-submodules"), "{on_line}");
        assert!(off_line.contains("core.autocrlf=false") && !off_line.contains("--recurse-submodules"), "{off_line}");
    }
}

/// A fake ssh that never answers (and records its pid): git through it hangs until stopped.
#[cfg(unix)]
fn slow_ssh(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let pid = dir.join("ssh.pid");
    let script = dir.join("slow-ssh");
    std::fs::write(&script, format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 60\n", pid.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, pid)
}

#[cfg(unix)]
fn gone(pid_file: &Path) -> bool {
    let Ok(pid) = std::fs::read_to_string(pid_file) else { return false };
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let alive = Command::new("kill").args(["-0", pid.trim()]).output().is_ok_and(|o| o.status.success());
        if !alive {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[cfg(unix)]
#[test]
fn a_slow_git_is_killed_with_its_children_on_timeout_or_cancel() {
    if !have_git() {
        return;
    }
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _h = home();
    let scratch = tempfile::tempdir().unwrap();
    let (script, pid) = slow_ssh(scratch.path());
    unsafe { std::env::set_var("GIT_SSH_COMMAND", &script) };
    let url = "ssh://example.invalid/slow.git";
    // timeout
    // long enough for git to reach the fake ssh even on a loaded machine
    let cfg = serde_json::json!({ "plugins": { "timeout": 5 }, "packages": [{ "name": "me/slow", "url": url }] });
    let t = Instant::now();
    let e = mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap_err().to_string();
    assert!(e.contains("timed out"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    assert!(pid.exists(), "git never reached the fake ssh within the timeout");
    assert!(gone(&pid), "the ssh grandchild was killed with git");
    assert!(!mare_plugin_manager::checkout_dir(url).exists(), "no half-written checkout is left");
    // cancellation (default timeout)
    let _ = std::fs::remove_file(&pid);
    let cfg = serde_json::json!({ "packages": [{ "name": "me/slow", "url": url }] });
    let token = tokio_util::sync::CancellationToken::new();
    let (t2, pid2) = (token.clone(), pid.clone());
    std::thread::spawn(move || {
        // cancel once the fake ssh runs (or after 5s)
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pid2.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        t2.cancel();
    });
    let t = Instant::now();
    let e = mare_plugin_manager::with_cancel(&token, || mare_plugin_manager::sync(&cfg, &mut |_| ()))
        .unwrap_err()
        .to_string();
    unsafe { std::env::remove_var("GIT_SSH_COMMAND") };
    assert!(e.contains("cancelled"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    assert!(gone(&pid));
}
