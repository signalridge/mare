//! End-to-end tests for `mare update [--self|--plugins|--all] [-y] [--method <m>]`.

use std::io::Write;
use std::process::Command;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::json;

use crate::common::*;

fn fake_tar_gz(entry_name: &str, content: &[u8]) -> Vec<u8> {
    let mut tar_buf = Vec::new();
    let mut header = [0u8; 512];
    let nb = entry_name.as_bytes();
    header[..nb.len()].copy_from_slice(nb);
    header[100..108].copy_from_slice(b"0000755\0");
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    let size_oct = format!("{:011o}\0", content.len());
    header[124..136].copy_from_slice(size_oct.as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0");
    header[148..156].copy_from_slice(b"        ");
    header[156] = b'0';
    let sum: u32 = header.iter().map(|&b| b as u32).sum();
    let cksum = format!("{sum:06o}\0 ");
    header[148..156].copy_from_slice(cksum.as_bytes());
    tar_buf.extend_from_slice(&header);
    tar_buf.extend_from_slice(content);
    let pad = (512 - (content.len() % 512)) % 512;
    tar_buf.resize(tar_buf.len() + pad, 0);
    tar_buf.resize(tar_buf.len() + 1024, 0);

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&tar_buf).unwrap();
    gz.finish().unwrap()
}

fn init_git_repo(dir: &std::path::Path, plugin_name: &str, tag: &str) {
    let pdir = dir.join("plugins").join(plugin_name);
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(pdir.join("init.lua"), format!("return {{ name = {plugin_name:?}, apply = function() end }}\n"))
        .unwrap();
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.com"],
        vec!["add", "-A"],
        vec!["commit", "-q", "-m", "init"],
        vec!["tag", tag],
    ] {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

fn commit_tag(dir: &std::path::Path, tag: &str, msg: &str) {
    std::fs::write(dir.join("VERSION"), format!("{tag}\n")).unwrap();
    for args in [
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.com"],
        vec!["add", "-A"],
        vec!["commit", "-q", "-m", msg],
        vec!["tag", tag],
    ] {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "{out:?}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn mare_update_managed_method_runs_channel_command_after_yes_without_mutating_binary() {
    use std::os::unix::fs::PermissionsExt;

    local(async {
        let env = TestEnv::new().await;
        let fake_bin = tempfile::tempdir().unwrap();
        let invoked_log = fake_bin.path().join("invoked.txt");
        let brew_script = fake_bin.path().join("brew");
        std::fs::write(&brew_script, format!("#!/bin/sh\necho \"$@\" > {}\nexit 0\n", invoked_log.display())).unwrap();
        std::fs::set_permissions(&brew_script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let test_path = format!("{}:{orig_path}", fake_bin.path().display());

        // Non-interactive without -y must refuse before running `brew`
        let refused = run_output(
            mare_cmd()
                .env("MARE_HOME", env.home.path())
                .env("PATH", &test_path)
                .args(["update", "--self", "--method", "brew", "--json"]),
        );
        assert!(!refused.status.success(), "expected refusal without -y");
        assert!(!invoked_log.exists(), "brew must not run without -y");

        // With -y, invokes `brew upgrade mare` cleanly
        let ok = run_output(
            mare_cmd()
                .env("MARE_HOME", env.home.path())
                .env("PATH", &test_path)
                .args(["update", "--self", "--method", "brew", "-y", "--json"]),
        );
        assert!(ok.status.success(), "stderr: {}", String::from_utf8_lossy(&ok.stderr));
        let recorded = std::fs::read_to_string(&invoked_log).unwrap();
        assert_eq!(recorded.trim(), "upgrade mare");
        let report: serde_json::Value = serde_json::from_slice(&ok.stdout).unwrap();
        assert_eq!(report["self_update"]["channel"], "brew");
        assert_eq!(report["self_update"]["status"], "updated");
    })
    .await
}

#[tokio::test]
async fn mare_update_other_channel_prints_manual_and_offline_aborts() {
    local(async {
        let env = TestEnv::new().await;
        let manual = run_output(
            mare_cmd().env("MARE_HOME", env.home.path()).args(["update", "--self", "--method", "other", "--json"]),
        );
        assert!(manual.status.success(), "stderr: {}", String::from_utf8_lossy(&manual.stderr));
        let report: serde_json::Value = serde_json::from_slice(&manual.stdout).unwrap();
        assert_eq!(report["self_update"]["channel"], "other");
        assert_eq!(report["self_update"]["status"], "manual");

        let offline = run_output(
            mare_cmd().env("MARE_HOME", env.home.path()).env("MARE_OFFLINE", "1").args(["update", "--self"]),
        );
        assert!(!offline.status.success());
        assert!(String::from_utf8_lossy(&offline.stderr).contains("MARE_OFFLINE"));
    })
    .await
}

#[cfg(all(unix, target_os = "linux"))]
#[tokio::test]
async fn mare_update_standalone_picks_musl_archive_from_independent_manifest_and_stages() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        let fake_bin_bytes = b"#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo \"mare 99.0.0\"; exit 0; fi\n";

        // Independent release manifest fixture mirroring `.github/workflows/release.yaml` (musl on Linux).
        // Do NOT construct expected archive names from `release_target_for(...)`.
        let x86_archive_name = "mare-v99.0.0-x86_64-unknown-linux-musl.tar.gz";
        let arm_archive_name = "mare-v99.0.0-aarch64-unknown-linux-musl.tar.gz";
        let x86_archive = fake_tar_gz("mare-v99.0.0-x86_64-unknown-linux-musl/mare", fake_bin_bytes);
        let arm_archive = fake_tar_gz("mare-v99.0.0-aarch64-unknown-linux-musl/mare", fake_bin_bytes);
        let x86_sha = mare_binding::update::sha256_hex(&x86_archive);
        let arm_sha = mare_binding::update::sha256_hex(&arm_archive);
        let checksums = format!(
            "{x86_sha}  {x86_archive_name}\n{arm_sha}  {arm_archive_name}\n{}",
            "b".repeat(64) + "  mare-v99.0.0-aarch64-apple-darwin.tar.gz\n"
        );

        let x86_path = format!("/download/v99.0.0/{x86_archive_name}");
        let arm_path = format!("/download/v99.0.0/{arm_archive_name}");
        mock.route(
            "/releases/latest",
            vec![Resp::status(
                200,
                json!({
                    "tag_name": "v99.0.0",
                    "assets": [
                        { "name": x86_archive_name, "browser_download_url": format!("{}{x86_path}", mock.base) },
                        { "name": arm_archive_name, "browser_download_url": format!("{}{arm_path}", mock.base) },
                        { "name": "checksums.txt", "browser_download_url": format!("{}/download/v99.0.0/checksums.txt", mock.base) }
                    ]
                })
                .to_string(),
            )],
        );
        mock.route("/download/v99.0.0/checksums.txt", vec![Resp::status(200, checksums)]);
        let mut r_x86 = Resp::status(200, "");
        r_x86.raw = Some(x86_archive);
        mock.route(&x86_path, vec![r_x86]);
        let mut r_arm = Resp::status(200, "");
        r_arm.raw = Some(arm_archive);
        mock.route(&arm_path, vec![r_arm]);

        let url = format!("{}/releases/latest", mock.base);
        let home_path = env.home.path().to_path_buf();
        let out = tokio::task::spawn_blocking(move || {
            run_output(
                mare_cmd()
                    .env("MARE_HOME", &home_path)
                    .env("MARE_UPDATE_URL", &url)
                    .args(["update", "--self", "--method", "standalone", "-y", "--json"]),
            )
        })
        .await
        .unwrap();
        assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["self_update"]["channel"], "standalone");
        assert_eq!(report["self_update"]["status"], "staged");
        assert_eq!(report["self_update"]["latest_version"], "99.0.0");

        let staged_file = env.home.path().join("state").join("update-staging").join("staged.json");
        assert!(staged_file.is_file(), "staged.json must exist at {}", staged_file.display());
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn mare_update_rejects_standalone_override_on_managed_install_before_network() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v99.0.0"}"#)]);

        // Place a copy of the mare binary inside a Homebrew Cellar path so `detect_current()` sees Brew
        // (symlinks resolve back to target/debug on Linux via /proc/self/exe).
        let cellar_bin = env.home.path().join("Cellar").join("mare").join("1.0.0").join("bin");
        std::fs::create_dir_all(&cellar_bin).unwrap();
        let managed_exe = cellar_bin.join("mare");
        std::fs::copy(mare_bin(), &managed_exe).unwrap();

        let url = format!("{}/releases/latest", mock.base);
        let home_path = env.home.path().to_path_buf();
        let out = tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(&managed_exe);
            cmd.env("MARE_HOME", &home_path).env("MARE_UPDATE_URL", &url).env("MARE_TEST_OFFLINE", "1").args([
                "update",
                "--self",
                "--method",
                "standalone",
                "-y",
                "--json",
            ]);
            run_output(&mut cmd)
        })
        .await
        .unwrap();
        assert!(!out.status.success(), "must refuse --method standalone on brew-managed binary");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("brew"), "stderr should name owning channel brew: {stderr}");
        assert_eq!(mock.count("/releases/latest"), 0, "must reject before any HTTP request");
    })
    .await
}

#[tokio::test]
async fn mare_update_self_uses_shared_semver_release_policy_for_prereleases() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        // Stable running binary must NOT treat a higher prerelease tag as a newer release.
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v99.0.0-rc.1"}"#)]);
        let url = format!("{}/releases/latest", mock.base);
        let home_path = env.home.path().to_path_buf();
        let out = tokio::task::spawn_blocking(move || {
            run_output(mare_cmd().env("MARE_HOME", &home_path).env("MARE_UPDATE_URL", &url).args([
                "update",
                "--self",
                "--method",
                "standalone",
                "-y",
                "--json",
            ]))
        })
        .await
        .unwrap();
        assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["self_update"]["status"], "up_to_date");
        assert_eq!(report["self_update"]["latest_version"], "99.0.0-rc.1");
    })
    .await
}

#[tokio::test]
async fn mare_update_plugins_updates_unpinned_and_respects_pins() {
    local(async {
        let env = TestEnv::new().await;
        let repo_a = tempfile::tempdir().unwrap();
        let repo_b = tempfile::tempdir().unwrap();
        init_git_repo(repo_a.path(), "alpha", "v1.0.0");
        init_git_repo(repo_b.path(), "beta", "v1.0.0");

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.plugins.add {{\n  {{ \"me/alpha\", url = {:?}, version = \"^1\" }},\n  {{ \"me/beta\", url = {:?}, version = \"^1\", pin = true }},\n}}\n",
                repo_a.path().to_string_lossy(),
                repo_b.path().to_string_lossy(),
            ),
        )
        .unwrap();

        // Install initial v1.0.0 checkouts
        let sync = run_output(
            mare_cmd()
                .env("MARE_HOME", env.home.path())
                .args(["plugin", "sync"]),
        );
        assert!(sync.status.success(), "sync failed: {}", String::from_utf8_lossy(&sync.stderr));

        commit_tag(repo_a.path(), "v1.1.0", "new alpha");
        commit_tag(repo_b.path(), "v1.1.0", "new beta");

        let out = run_output(
            mare_cmd()
                .env("MARE_HOME", env.home.path())
                .args(["update", "--plugins", "--json"]),
        );
        assert!(out.status.success(), "update --plugins failed: {}", String::from_utf8_lossy(&out.stderr));
        let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(report["plugins"]["status"], "updated");
        assert_eq!(report["plugins"]["reload_reminder"], true);

        let lock: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(env.config_dir().join("mare-lock.json")).unwrap()).unwrap();
        let alpha_v11 = String::from_utf8(
            Command::new("git").current_dir(repo_a.path()).args(["rev-parse", "v1.1.0^{commit}"]).output().unwrap().stdout,
        )
        .unwrap();
        let beta_v10 = String::from_utf8(
            Command::new("git").current_dir(repo_b.path()).args(["rev-parse", "v1.0.0^{commit}"]).output().unwrap().stdout,
        )
        .unwrap();
        assert_eq!(lock["packages"]["me/alpha"]["commit"], alpha_v11.trim());
        assert_eq!(lock["packages"]["me/beta"]["commit"], beta_v10.trim());
    })
    .await
}

#[tokio::test]
async fn update_check_opt_out_suppresses_background_plugin_and_release_checks() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_NO_UPDATE_CHECK", "1");
        let mock = MockServer::start().await;
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v99.0.0"}"#)]);
        let url = format!("{}/releases/latest", mock.base);

        let h = env
            .host()
            .builtins(&["update-check"])
            .config(json!({ "plugins": { "update-check": { "url": url } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            _G.OPT_OUT_NOTICES = 0
            local orig = mare.ui.notify
            mare.ui.notify = function(...) _G.OPT_OUT_NOTICES = _G.OPT_OUT_NOTICES + 1; return orig(...) end
            mare.mode, mare.debug_build = "tui", false
            C.cordis:get("bus"):emit("ui/ready", {})
            "#,
        )
        .await;
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert_eq!(mock.count("/releases/latest"), 0, "MARE_NO_UPDATE_CHECK=1 must suppress release check");
        assert_eq!(eval::<i64>(&h, "return _G.OPT_OUT_NOTICES").await, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mare_offline_blocks_plugin_check_and_startup_clone_while_keeping_installed() {
    local(async {
        let mut env = TestEnv::new().await;
        let repo_ok = tempfile::tempdir().unwrap();
        let repo_missing = tempfile::tempdir().unwrap();
        init_git_repo(repo_ok.path(), "installed_ok", "v1.0.0");
        init_git_repo(repo_missing.path(), "missing_remote", "v1.0.0");

        // Install only repo_ok first
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.plugins.add {{\n  {{ \"me/installed\", url = {:?}, version = \"^1\" }},\n}}\n",
                repo_ok.path().to_string_lossy(),
            ),
        )
        .unwrap();
        let home_sync = env.home.path().to_path_buf();
        let sync = tokio::task::spawn_blocking(move || {
            run_output(mare_cmd().env("MARE_HOME", &home_sync).args(["plugin", "sync"]))
        })
        .await
        .unwrap();
        assert!(sync.status.success(), "sync failed: {}", String::from_utf8_lossy(&sync.stderr));

        // Now add repo_missing to init.lua and enable MARE_OFFLINE=1
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.plugins.add {{\n  {{ \"me/installed\", url = {:?}, version = \"^1\" }},\n  {{ \"me/missing\", url = {:?}, version = \"^1\" }},\n}}\n",
                repo_ok.path().to_string_lossy(),
                repo_missing.path().to_string_lossy(),
            ),
        )
        .unwrap();

        // Explicit `mare plugin check` under MARE_OFFLINE=1 fails cleanly without fetching
        let chk = run_output(
            mare_cmd()
                .env("MARE_HOME", env.home.path())
                .env("MARE_OFFLINE", "1")
                .args(["plugin", "check"]),
        );
        assert!(!chk.status.success());
        assert!(String::from_utf8_lossy(&chk.stderr).contains("MARE_OFFLINE"));

        // Offline startup skips missing clone while still loading already-installed plugin cleanly
        env.set_var("MARE_OFFLINE", "1");
        let h = mare_host::Host::new(env.host().user_config(false).faux(json!([])).no_session().opts)
            .await
            .unwrap();
        let problems = h.kernel.problems();
        assert!(
            problems.iter().any(|p| p.contains("me/missing") && p.contains("MARE_OFFLINE")),
            "expected offline notice for missing package: {problems:?}"
        );
        let warnings = h.plugin_warnings();
        assert!(
            warnings.iter().any(|w| w.contains("me/missing") && w.contains("MARE_OFFLINE")),
            "expected offline warning for missing package: {warnings:?}"
        );
        let ok_active: bool = eval(&h, "return mare.plugins.status('installed_ok').active").await;
        assert!(ok_active, "installed plugin must load offline");
        h.shutdown().await;
    })
    .await
}
