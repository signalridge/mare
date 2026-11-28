//! Faux TUI end-to-end tests for combined release + plugin background update notifications.

use std::process::Command;

use super::*;

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

#[tokio::test]
async fn faux_tui_combines_release_and_plugin_notice_and_notifies_once_per_revision() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v99.0.0"}"#)]);
        let url = format!("{}/releases/latest", mock.base);

        let repo_a = tempfile::tempdir().unwrap();
        init_git_repo(repo_a.path(), "alpha", "v1.0.0");

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ logo = false, model = \"faux/echo\" }}\nmare.plugin(\"onboarding\", {{ plugins = false }})\nmare.plugin(\"update-check\", {{ url = {url:?} }})\nmare.plugins.add {{\n  {{ \"me/alpha\", url = {:?}, version = \"^1\" }},\n}}\n",
                repo_a.path().to_string_lossy(),
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

        // Advance remote plugin to v1.1.0 before TUI launches
        commit_tag(repo_a.path(), "v1.1.0", "alpha v1.1");

        let mut opts = env.host().user_config(false).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 140, 30).await;
        let mut parser = vt100::Parser::new(nz(30), nz(140), 2000);
        pump_until(&mut vt, &mut parser, "initial ui/ready settled", |vt, _| vt.startup_finished()).await;
        expose(&vt.host);

        // Trigger ui/ready in non-debug TUI mode; verify one combined notice is emitted & rendered.
        exec(
            &vt.host,
            r#"
            _G.CAPTURED_NOTICES = {}
            local orig = mare.ui.notify
            mare.ui.notify = function(msg, lvl, o)
                table.insert(_G.CAPTURED_NOTICES, tostring(msg))
                return orig(msg, lvl, o)
            end
            mare.debug_build = false
            C.cordis:get("bus"):emit("ui/ready", {})
            "#,
        )
        .await;

        pump_until(&mut vt, &mut parser, "combined update notice on screen", |_, screen| {
            screen.contains("99.0.0") && screen.contains("plugin update")
        })
        .await;

        wait_until(WAIT_SECS, "captured notice", || {
            let h = vt.host.clone();
            async move { eval::<i64>(&h, "return #_G.CAPTURED_NOTICES").await >= 1 }
        })
        .await;

        let first_notice: String = eval(&vt.host, "return _G.CAPTURED_NOTICES[1]").await;
        assert!(
            first_notice.contains("mare 99.0.0 is available") && first_notice.contains("1 plugin update (mare update --plugins)"),
            "expected one combined notice, got: {first_notice:?}"
        );
        assert_eq!(eval::<i64>(&vt.host, "return #_G.CAPTURED_NOTICES").await, 1);

        // Expire interval only (keep update_notices state): repeat check must NOT duplicate notice.
        exec(
            &vt.host,
            r#"
            mare.state.set("update_check_at", 0)
            C.cordis:get("bus"):emit("ui/ready", {})
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "second release check", || async { mock.count("/releases/latest") >= 2 }).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(eval::<i64>(&vt.host, "return #_G.CAPTURED_NOTICES").await, 1, "same version/revision must not notify twice");

        // Simulate a transient plugin check failure (mare.plugins.check throws):
        // update_notices.plugins MUST be preserved so the subsequent recovery check does NOT re-notify v1.1.0.
        exec(
            &vt.host,
            r#"
            _G.ORIG_PLUG_CHECK = mare.plugins.check
            mare.plugins.check = function() error("transient git failure") end
            mare.state.set("update_check_at", 0)
            C.cordis:get("bus"):emit("ui/ready", {})
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "failed plugin check run", || async { mock.count("/releases/latest") >= 3 }).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let preserved: String = eval(&vt.host, "return ((mare.state.read().update_notices or {}).plugins or {})['me/alpha'] or ''").await;
        assert!(!preserved.is_empty(), "failed mare.plugins.check() must NOT clear update_notices.plugins");

        // Restore mare.plugins.check with the SAME v1.1.0 revision: must still NOT emit a duplicate notice.
        exec(
            &vt.host,
            r#"
            mare.plugins.check = _G.ORIG_PLUG_CHECK
            mare.state.set("update_check_at", 0)
            C.cordis:get("bus"):emit("ui/ready", {})
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "recovered plugin check run", || async { mock.count("/releases/latest") >= 4 }).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(
            eval::<i64>(&vt.host, "return #_G.CAPTURED_NOTICES").await,
            1,
            "recovered plugin check at same revision must not re-notify after transient failure"
        );

        // Advance remote plugin to v1.2.0 and reset interval: only new plugin revision notifies (not mare 99.0.0 again).
        commit_tag(repo_a.path(), "v1.2.0", "alpha v1.2");
        exec(
            &vt.host,
            r#"
            mare.state.set("update_check_at", 0)
            C.cordis:get("bus"):emit("ui/ready", {})
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "new revision notice", || {
            let h = vt.host.clone();
            async move { eval::<i64>(&h, "return #_G.CAPTURED_NOTICES").await >= 2 }
        })
        .await;
        let second_notice: String = eval(&vt.host, "return _G.CAPTURED_NOTICES[2]").await;
        assert!(second_notice.contains("1 plugin update"), "expected plugin notice: {second_notice:?}");
        assert!(!second_notice.contains("mare 99.0.0"), "already-noticed mare version must not repeat: {second_notice:?}");

        vt.host.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn background_plugin_auto_update_respects_pin_and_keeps_live_session_modules_consistent_until_reload() {
    local(async {
        let env = TestEnv::new().await;
        let mock = MockServer::start().await;
        mock.route("/releases/latest", vec![Resp::status(200, r#"{"tag_name":"v0.0.1"}"#)]);
        let url = format!("{}/releases/latest", mock.base);

        let repo_auto = tempfile::tempdir().unwrap();
        let repo_pinned = tempfile::tempdir().unwrap();
        let repo_default = tempfile::tempdir().unwrap();
        // Trusted package with BOTH entry `init.lua` and a lazily-required module `auto_p.sub` at v1.0.0.
        let write_pkg = |dir: &std::path::Path, ver: &str| {
            let pdir = dir.join("plugins").join("auto_p");
            let ldir = pdir.join("lua").join("auto_p");
            std::fs::create_dir_all(&ldir).unwrap();
            std::fs::write(ldir.join("sub.lua"), format!("return {{ version = {ver:?} }}\n")).unwrap();
            std::fs::write(
                pdir.join("init.lua"),
                format!("return {{ name = \"auto_p\", apply = function() mare.g.AUTO_INIT_VER = {ver:?} end }}\n"),
            )
            .unwrap();
        };
        write_pkg(repo_auto.path(), "1.0.0");
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.name", "Test"],
            vec!["config", "user.email", "test@example.com"],
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", "v1.0.0"],
            vec!["tag", "v1.0.0"],
        ] {
            assert!(Command::new("git").current_dir(repo_auto.path()).args(args).output().unwrap().status.success());
        }
        init_git_repo(repo_pinned.path(), "pinned_p", "v1.0.0");
        init_git_repo(repo_default.path(), "default_p", "v1.0.0");

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ logo = false, model = \"faux/echo\" }}\nmare.plugin(\"onboarding\", {{ plugins = false }})\nmare.plugin(\"update-check\", {{ url = {url:?} }})\nmare.plugins.add {{\n  {{ \"me/auto\", url = {:?}, version = \"^1\", auto_update = true }},\n  {{ \"me/pinned\", url = {:?}, version = \"^1\", auto_update = true, pin = true }},\n  {{ \"me/default\", url = {:?}, version = \"^1\" }},\n}}\n",
                repo_auto.path().to_string_lossy(),
                repo_pinned.path().to_string_lossy(),
                repo_default.path().to_string_lossy(),
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

        // Remote advances all three repos to v1.1.0, including both init.lua and lazy `auto_p.sub` in `me/auto`.
        write_pkg(repo_auto.path(), "1.1.0");
        for args in [
            vec!["add", "-A"],
            vec!["commit", "-q", "-m", "v1.1.0"],
            vec!["tag", "v1.1.0"],
        ] {
            assert!(Command::new("git").current_dir(repo_auto.path()).args(args).output().unwrap().status.success());
        }
        commit_tag(repo_pinned.path(), "v1.1.0", "pinned v1.1");
        commit_tag(repo_default.path(), "v1.1.0", "default v1.1");

        let lock_path = env.config_dir().join("mare-lock.json");
        let rev = |dir: &std::path::Path, tag: &str| {
            String::from_utf8(Command::new("git").current_dir(dir).args(["rev-parse", &format!("{tag}^{{commit}}")]).output().unwrap().stdout)
                .unwrap()
                .trim()
                .to_string()
        };

        let mut opts = env.host().user_config(false).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 140, 30).await;
        let mut parser = vt100::Parser::new(nz(30), nz(140), 2000);
        pump_until(&mut vt, &mut parser, "initial ui/ready settled", |vt, _| vt.startup_finished()).await;
        expose(&vt.host);

        assert_eq!(eval::<String>(&vt.host, "return mare.g.AUTO_INIT_VER").await, "1.0.0");

        exec(
            &vt.host,
            r#"
            _G.AUTO_NOTICES = {}
            local orig = mare.ui.notify
            mare.ui.notify = function(msg, lvl, o)
                table.insert(_G.AUTO_NOTICES, tostring(msg))
                return orig(msg, lvl, o)
            end
            mare.debug_build = false
            C.cordis:get("bus"):emit("ui/ready", {})
            "#,
        )
        .await;

        wait_until(WAIT_SECS, "auto-update queued and notify notice", || {
            let h = vt.host.clone();
            async move { eval::<i64>(&h, "return #_G.AUTO_NOTICES").await >= 1 }
        })
        .await;

        let notice: String = eval(&vt.host, "return _G.AUTO_NOTICES[1]").await;
        assert!(
            notice.contains("1 plugin update (mare update --plugins)") && notice.contains("1 plugin update ready (/reload to apply)"),
            "expected default notified + auto_update queued for reload, got: {notice:?}"
        );

        // BEFORE /reload: checkout MUST NOT have switched yet, so lazy first-time `require("auto_p.sub")`
        // still sees v1.0.0 matching the running `init.lua` (`AUTO_INIT_VER == "1.0.0"`).
        let lock_before: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(lock_before["packages"]["me/auto"]["commit"], rev(repo_auto.path(), "v1.0.0"));
        let lazy_before: String = eval(&vt.host, "return require('auto_p.sub').version").await;
        assert_eq!(lazy_before, "1.0.0", "lazy module require before /reload must match loaded revision 1.0.0");

        // Trigger `/reload`: queued `auto_update = true` switches checkout before plugins reload,
        // while pinned & default packages stay untouched at v1.0.0.
        exec(&vt.host, "mare.reload()").await;
        assert_eq!(eval::<String>(&vt.host, "return mare.g.AUTO_INIT_VER").await, "1.1.0");
        assert_eq!(eval::<String>(&vt.host, "return require('auto_p.sub').version").await, "1.1.0");

        let lock_after: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
        assert_eq!(lock_after["packages"]["me/auto"]["commit"], rev(repo_auto.path(), "v1.1.0"));
        assert_eq!(lock_after["packages"]["me/pinned"]["commit"], rev(repo_pinned.path(), "v1.0.0"));
        assert_eq!(lock_after["packages"]["me/default"]["commit"], rev(repo_default.path(), "v1.0.0"));

        vt.host.shutdown().await;
    })
    .await
}
