//! Interactive start with packages: first frame and input come before a slow install.

use super::*;

#[tokio::test]
async fn reinstalling_package_binding_preserves_runtime_plugin_apis_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        let package = env.cwd.join("namespace-package");
        std::fs::create_dir_all(package.join("plugins")).unwrap();
        std::fs::write(
            package.join("plugins/namespace-probe.lua"),
            "return { name = 'namespace-probe', apply = function() end }\n",
        )
        .unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("mare.plugins.add {{ {{ dir = {:?} }} }}", package.to_string_lossy()),
        )
        .unwrap();
        let h = env.host().user_config(false).builtins(&["plugins"]).no_session().build().await;
        let coherent = r#"
            local p = mare.plugins
            if type(p.add) ~= 'function' or type(p.status) ~= 'function'
                or type(p.list) ~= 'function' or type(p.list_installed) ~= 'function' then return false end
            local found = false
            for _, item in ipairs(p.list_installed()) do
                if item.name == 'namespace-package' then found = true end
            end
            return found and p.status('namespace-probe').active
                and p.status('plugins').defined and type(p.list()) == 'table'
        "#;
        assert!(eval::<bool>(&h, coherent).await, "initial plugin APIs must coexist");
        let lua = h.kernel.lua();
        let mare: mlua::Table = lua.globals().get("mare").unwrap();
        mare_plugin::pkg_binding::install(lua, &mare).unwrap();
        assert!(eval::<bool>(&h, coherent).await, "reinstall must preserve runtime and declaration APIs");
        exec(&h, "mare.reload()").await;
        assert!(eval::<bool>(&h, coherent).await, "the same APIs must coexist after reload");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn subplugin_manifests_skip_incompatible_entries_and_report_each_problem() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("parent", None, "v1.0.0");
        let other_platform = if cfg!(target_os = "windows") { "linux" } else { "windows" };
        for (name, manifest) in [
            ("allowed", "requires = '>=0.0.1'\n".to_string()),
            ("future", "requires = '>=99.0'\n".to_string()),
            ("other-platform", format!("platforms = [{other_platform:?}]\n")),
            ("broken", "requires = [\n".to_string()),
        ] {
            let dir = repo.path().join("plugins").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("init.lua"),
                format!("return {{ name = {name:?}, apply = function() mare.g[{name:?}] = true end }}\n"),
            )
            .unwrap();
            std::fs::write(dir.join("mare-plugin.toml"), manifest).unwrap();
        }
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "subplugin manifests"]);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.plugins.add {{ {{ 'me/subplugins', url = {:?}, branch = 'main' }} }}\n",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        let mut builder = env.host().user_config(false);
        builder.opts.config = json!({ "model": "faux/echo" });
        let h = mare_host::Host::new(builder.opts).await.unwrap();
        let loaded: (Option<bool>, Option<bool>, Option<bool>, Option<bool>) =
            eval(&h, "return mare.g.allowed, mare.g.future, mare.g['other-platform'], mare.g.broken").await;
        assert_eq!(loaded, (Some(true), None, None, None));
        let encoded: String = eval(&h, "return mare.json.encode(mare.plugins.diagnostics())").await;
        let diagnostics: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let entries = diagnostics.as_array().unwrap();
        let find = |name: &str| entries.iter().find(|entry| entry["plugin"] == name).unwrap();
        assert!(find("future")["constraint"].as_str().unwrap().contains(">=99.0"), "{diagnostics}");
        assert!(find("other-platform")["constraint"].as_str().unwrap().contains(other_platform), "{diagnostics}");
        assert!(
            find("broken")["path"].as_str().unwrap().replace('\\', "/").ends_with("broken/mare-plugin.toml"),
            "{diagnostics}"
        );
        assert!(find("broken")["message"].as_str().unwrap().contains("TOML"), "{diagnostics}");
        assert_eq!(blocked_reason(&h).await, None, "incompatible optional plugins must not block the host");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn interactive_start_waits_for_first_frame_before_installing_required_package() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("startup_probe", None, "v1.0.0");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ {{ 'me/startup', url = {:?} }} }}",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let startup = tokio::task::spawn_local(mare_host::Host::with_ui_first_frame(opts, ui, tx));
        let mare_host::InstallStart { shown, mut progress } =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), rx).await.unwrap().unwrap();
        assert!(!startup.is_finished(), "host must wait for the input frame");
        assert!(
            mare_plugin_manager::installed(&json!({ "packages": [{ "name": "me/startup", "url": repo.path() }] }))
                .is_empty()
        );
        shown.send(()).unwrap();
        let h = tokio::time::timeout(std::time::Duration::from_secs(30), startup).await.unwrap().unwrap().unwrap();
        // the activity area's progress lines, closed once installation is over
        let mut lines = vec![];
        while let Some(l) = progress.recv().await {
            lines.push(l);
        }
        assert_eq!(
            lines,
            ["Installing packages (0/1): me/startup", "Installing packages (1/1): me/startup done"],
            "{lines:?}"
        );
        assert!(eval::<bool>(&h, "return mare.g.startup_probe == true").await);
        assert!(h.kernel.plugin_status().iter().any(|(name, _, active)| name == "permissions" && *active));
        assert!(h.prompt("hello", json!({})).await.unwrap()["error"].is_null());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn interactive_start_warns_for_a_missing_package() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let missing = env.cwd.join("no-such-local-repo.git");
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ {{ 'me/missing', url = {:?} }} }}",
                missing.to_string_lossy()
            ),
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let startup =
            tokio::task::spawn_local(mare_host::Host::with_ui_first_frame(opts, mare_shared::ui::UiShared::new(), tx));
        tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), rx)
            .await
            .unwrap()
            .unwrap()
            .shown
            .send(())
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), startup).await.unwrap().unwrap();
        let h = result.expect("startup must not refuse a missing package");
        let failure: (String, bool) = eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks").await;
        assert_eq!(failure, ("me/missing".into(), false));
        assert_eq!(blocked_reason(&h).await, None);
        let turn = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(turn["status"], "completed", "{turn}");
        h.shutdown().await;
    })
    .await
}

// Unix-only: drives the TUI through openpty and stalls `git` with an sh wrapper on PATH
#[cfg(unix)]
#[tokio::test]
async fn interactive_frame_and_input_precede_slow_package_install() {
    use std::io::{Read, Write};
    use std::os::fd::{FromRawFd, RawFd};
    use std::os::unix::fs::PermissionsExt;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("slow_start", None, "v1.0.0");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), format!(
            "mare.setup {{ model = 'faux/echo', logo = false }}; mare.plugins.add {{ {{ 'me/slow', url = {:?} }} }}",
            repo.path().to_string_lossy()
        )).unwrap();
        let real_git = Command::new("which").arg("git").output().unwrap();
        let real_git = String::from_utf8(real_git.stdout).unwrap().trim().to_string();
        let bin = env.home.path().join("test-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let wrapper = bin.join("git");
        std::fs::write(&wrapper, format!("#!/bin/sh\nprintf started > \"$MARE_TEST_GIT_STARTED\"\nwhile [ ! -f \"$MARE_TEST_GIT_RELEASE\" ]; do /bin/sleep 0.05; done\nexec {:?} \"$@\"\n", real_git)).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = env.home.path().join("git-started");
        let release = env.home.path().join("git-release");
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        let mut size = libc::winsize { ws_row: 30, ws_col: 100, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: openpty writes two descriptors and uses a valid winsize; the descriptors are
        // transferred to owned File values exactly once below.
        assert_eq!(unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::addr_of_mut!(size)) }, 0);
        // SAFETY: openpty returned valid owned descriptors; dup creates distinct owned handles.
        let mut master = unsafe { std::fs::File::from_raw_fd(master) };
        let slave = unsafe { std::fs::File::from_raw_fd(slave) };
        let flags = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&master), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&master), libc::F_SETFL, flags | libc::O_NONBLOCK) }, 0);
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let temporary = env.home.path().join("tmp");
        std::fs::create_dir(&temporary).unwrap();
        let mut cmd = mare_cmd();
        cmd.args(["--no-session", "--set", "plugins.onboarding.plugins=false"])
            .env("MARE_HOME", env.home.path())
            .env("MARE_NO_TERM_PROBE", "1")
            .env("TMPDIR", &temporary)
            .env("TEMP", &temporary)
            .env("TMP", &temporary)
            .env("MARE_TEST_GIT_STARTED", &started)
            .env("MARE_TEST_GIT_RELEASE", &release)
            .env("PATH", path)
            .env("TERM", "xterm-256color")
            .current_dir(&env.cwd)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        let mut child = ChildGroup::spawn(&mut cmd).unwrap();
        let mut parser = vt100::Parser::new(std::num::NonZeroU16::new(30).unwrap(), std::num::NonZeroU16::new(100).unwrap(), 2000);
        let mut bytes = [0u8; 8192];
        let start = Instant::now();
        // Enter during the install sends nothing and holds nothing (pi `handleStartupSubmit`): the
        // text stays in the input box; once startup is ready nothing goes on its own, and Enter
        // sends it
        let mut typed = false;
        let mut released = false;
        let mut ready_at: Option<Instant> = None;
        let mut last_enter = Instant::now();
        let mut answered = false;
        while start.elapsed() < Duration::from_secs(30) {
            match master.read(&mut bytes) {
                Ok(n) if n > 0 => parser.process(&bytes[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {},
                Ok(_) => {},
                Err(e) => panic!("reading startup pty: {e}"),
            }
            let screen = parser.screen().contents();
            if started.exists() && screen.contains("Installing packages (0/1): me/slow") && !typed {
                master.write_all(b"hello\r").unwrap();
                typed = true;
            }
            if typed && screen.contains("startup is still in progress") && screen.contains("hello") && !released {
                assert!(!screen.contains("echo: hello"), "request started before installation: {screen}");
                std::fs::write(&release, "go").unwrap();
                released = true;
            }
            if released && ready_at.is_none() && !screen.contains("Installing packages") {
                ready_at = Some(Instant::now());
            }
            if let Some(at) = ready_at {
                if at.elapsed() < Duration::from_millis(800) {
                    assert!(!screen.contains("echo: hello"), "sent on its own after the install: {screen}");
                } else if last_enter.elapsed() > Duration::from_millis(300) {
                    // Enter until startup (trust, session) is ready: a refused Enter keeps the text
                    master.write_all(b"\r").unwrap();
                    last_enter = Instant::now();
                }
            }
            if released && screen.contains("echo: hello") { answered = true; break; }
            if let Some(status) = child.0.try_wait().unwrap() { panic!("mare exited {status}; screen: {screen}"); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let screen = parser.screen().contents();
        drop(child);
        assert!(typed && released && answered, "typed={typed} released={released} answered={answered}; screen: {screen}");
    }).await
}

#[tokio::test]
async fn interactive_start_warns_for_a_failed_package_plugin() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("broken_start", None, "v1.0.0");
        std::fs::write(
            repo.path().join("plugins/broken_start/init.lua"),
            "return { name = 'broken_start', apply = function() error('startup plugin sentinel') end }",
        )
        .unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "break plugin"]);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ {{ 'me/broken', url = {:?}, branch = 'main' }} }}",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        let opts = env.host().user_config(false).no_session().opts;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let startup =
            tokio::task::spawn_local(mare_host::Host::with_ui_first_frame(opts, mare_shared::ui::UiShared::new(), tx));
        tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), rx).await.unwrap().unwrap().shown.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), startup).await.unwrap().unwrap();
        let h = result.expect("startup must not refuse a failed plugin");
        let failure: (String, bool, String) = eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks, f.error").await;
        assert_eq!((&*failure.0, failure.1), ("broken_start", false));
        assert!(failure.2.contains("startup plugin sentinel"), "{failure:?}");
        assert_eq!(blocked_reason(&h).await, None);
        let turn = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(turn["status"], "completed", "{turn}");
        h.reload_all().await.unwrap();
        let failure: (String, bool, String) =
            eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks, f.error").await;
        assert_eq!((&*failure.0, failure.1), ("broken_start", false));
        assert!(failure.2.contains("startup plugin sentinel"), "{failure:?}");
        assert_eq!(blocked_reason(&h).await, None);
        let turn = h.prompt("hello again", json!({})).await.unwrap();
        assert_eq!(turn["status"], "completed", "{turn}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn noninteractive_modes_warn_for_an_unavailable_package() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let missing = env.cwd.join("missing-required-directory");
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ {{ dir = {:?}, name = 'local-missing' }} }}",
                missing.to_string_lossy()
            ),
        )
        .unwrap();
        for mode in [
            mare_shared::ui::Mode::Print,
            mare_shared::ui::Mode::Json,
            mare_shared::ui::Mode::Rpc,
            mare_shared::ui::Mode::Acp,
        ] {
            let mut opts = env.host().user_config(false).no_session().opts;
            opts.mode = mode;
            let h = mare_host::Host::new(opts).await.expect("startup must not refuse a missing package");
            let failure: (String, bool) =
                eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks").await;
            assert_eq!(failure, ("local-missing".into(), false), "{mode:?}");
            assert_eq!(blocked_reason(&h).await, None, "{mode:?}");
            let prompt = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(prompt["status"], "completed", "{mode:?}: {prompt}");
            h.shutdown().await;
        }
    })
    .await
}

/// A manifest that excludes this mare version should isolate that package on startup and reload.
#[tokio::test]
async fn incompatible_package_warns_and_turns_continue_after_reload() {
    local(async {
        let env = TestEnv::new().await;
        let package = env.cwd.join("incompatible");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("mare-plugin.toml"), "requires = '>=999.0.0'\n").unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ {{ dir = {:?}, name = 'incompatible' }} }}",
                package.to_string_lossy()
            ),
        )
        .unwrap();
        let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
        for reload in [false, true] {
            if reload {
                h.reload_all().await.unwrap();
            }
            let failure: (String, bool, String) =
                eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks, f.error").await;
            assert_eq!((&*failure.0, failure.1), ("incompatible", false), "reload={reload}");
            assert!(failure.2.contains("999.0.0"), "{failure:?}");
            assert!(h.kernel.problems().iter().any(|p| p.contains("999.0.0")), "{:?}", h.kernel.problems());
            assert_eq!(blocked_reason(&h).await, None);
            let turn = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["status"], "completed", "{turn}");
        }
        h.shutdown().await;
    })
    .await
}

/// Each package hook has its own diagnostic and leaves unrelated requests usable after reload.
#[tokio::test]
async fn optional_package_hook_failures_are_isolated_on_startup_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        let a = env.cwd.join("package-a");
        let b = env.cwd.join("package-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ \
             {{ dir = {:?}, name = 'package-a', init = function() error('init sentinel') end }}, \
             {{ dir = {:?}, name = 'package-b', config = function() error('config sentinel') end }} }}",
                a.to_string_lossy(),
                b.to_string_lossy()
            ),
        )
        .unwrap();
        let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
        for reload in [false, true] {
            if reload {
                h.reload_all().await.unwrap();
            }
            let rows: (bool, bool, usize) = eval(
                &h,
                r#"
                local found = {}
                local failures = mare.plugins.failures()
                for _, f in ipairs(failures) do
                  if not f.blocks and f.error:find('init sentinel', 1, true) then found[f.plugin] = true end
                  if not f.blocks and f.error:find('config sentinel', 1, true) then found[f.plugin] = true end
                end
                return found['package-a'] == true, found['package-b'] == true, #failures
            "#,
            )
            .await;
            assert_eq!(rows, (true, true, 2), "reload={reload}");
            let problems = h.kernel.problems().join("\n");
            assert!(problems.contains("init sentinel") && problems.contains("config sentinel"), "{problems}");
            assert_eq!(blocked_reason(&h).await, None);
            let turn = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["status"], "completed", "{turn}");
        }
        std::fs::write(env.config_dir().join("init.lua"), "mare.setup { model = 'faux/echo' }").unwrap();
        h.reload_all().await.unwrap();
        let problems = h.kernel.problems().join("\n");
        assert!(!problems.contains("init sentinel") && !problems.contains("config sentinel"), "{problems}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn package_init_failure_tracks_runtime_provision_of_an_essential_service() {
    local(async {
        let env = TestEnv::new().await;
        let package = env.cwd.join("runtime-package");
        let plugin = package.join("plugins/runtime-provider/init.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(plugin, r#"
          return { name = 'runtime-provider', apply = function(ctx)
            local Perm = require('cordis').class('Perm', mare.Service)
            function Perm:constructor(c) Perm.super.constructor(self, c, 'permissions') end
            ctx:plugin(Perm)
          end }
        "#).unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), format!(
            "mare.setup {{ model = 'faux/echo' }}; mare.plugin('permissions', {{ enabled = false }}); \
             mare.plugins.add {{ {{ dir = {:?}, name = 'runtime-package', init = function() error('runtime init sentinel') end }} }}",
            package.to_string_lossy()
        )).unwrap();
        let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
        assert!(h.kernel.provided("runtime-provider").iter().any(|s| s == "permissions"), "{:?}", h.kernel.problems());
        let blocks: bool = eval(&h, "for _, f in ipairs(mare.plugins.failures()) do if f.plugin == 'runtime-package' then return f.blocks end end return false").await;
        assert!(blocks, "the package init failure must include the runtime-provided essential service");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn package_hook_failure_blocks_when_package_provides_an_essential_service() {
    local(async {
        let env = TestEnv::new().await;
        let package = env.cwd.join("essential-package");
        let plugin = package.join("plugins/permissions/init.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(plugin, r#"
          return { name = 'permissions', override = true, provide = { 'permissions' }, apply = function(ctx)
            local Perm = require('cordis').class('Perm', mare.Service)
            function Perm:constructor(c) Perm.super.constructor(self, c, 'permissions') end
            function Perm:preset() return 'custom' end
            for _, m in ipairs({ 'get', 'list', 'set', 'presets', 'rules', 'check', 'rule_verdict', 'describe', 'add_rules',
                'request', 'grants', 'describe_request', 'resolve_request', 'commit_request', 'policy_revision', 'is_partial' }) do
              Perm[m] = function() end
            end
            ctx:plugin(Perm)
          end }
        "#).unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), format!(
            "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ \
             {{ dir = {:?}, name = 'essential-package', config = function() \
             if not mare.g.hook_fixed then error('essential hook sentinel') end end }} }}",
            package.to_string_lossy()
        )).unwrap();
        let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
        assert!(crate::common::plugin_active(&h.kernel, "permissions"), "{:?}", h.kernel.problems());
        assert!(h.kernel.service("permissions").is_some());
        for reload in [false, true] {
            if reload { h.reload_all().await.unwrap(); }
            let failure: (String, bool) = eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks").await;
            assert_eq!(failure, ("essential-package".into(), true));
            let reason = blocked_reason(&h).await.unwrap();
            assert!(reason.contains("essential hook sentinel") && reason.contains("permissions"), "{reason}");
            let turn = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["not_sent"], true, "{turn}");
        }
        exec(&h, "mare.g.hook_fixed = true").await;
        h.reload_all().await.unwrap();
        assert_eq!(blocked_reason(&h).await, None);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn explicitly_required_package_plugin_failure_blocks_requests() {
    local(async {
        let env = TestEnv::new().await;
        let package = env.cwd.join("required-package");
        let plugin = package.join("plugins/required-plugin/init.lua");
        std::fs::create_dir_all(plugin.parent().unwrap()).unwrap();
        std::fs::write(
            plugin,
            "return { name = 'required-plugin', apply = function() error('package plugin sentinel') end }",
        )
        .unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ \
             {{ dir = {:?}, name = 'required-package', required = true }} }}",
                package.to_string_lossy()
            ),
        )
        .unwrap();
        let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
        for reload in [false, true] {
            if reload {
                h.reload_all().await.unwrap();
            }
            let failure: (String, bool) =
                eval(&h, "local f = mare.plugins.failures()[1]; return f.plugin, f.blocks").await;
            assert_eq!(failure, ("required-plugin".into(), true));
            let reason = blocked_reason(&h).await.unwrap();
            assert!(reason.contains("required-plugin") && reason.contains("package plugin sentinel"), "{reason}");
            let turn = h.prompt("hello", json!({})).await.unwrap();
            assert_eq!(turn["not_sent"], true, "{turn}");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicitly_required_package_failures_block_on_startup_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        let package = env.cwd.join("required-package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = env.config_dir().join("init.lua");
        for hook in ["init", "config"] {
            std::fs::write(&init, format!(
                "mare.setup {{ model = 'faux/echo' }}; mare.plugins.add {{ \
                 {{ dir = {:?}, name = 'required-package', required = true, {hook} = function() error('required hook') end }} }}",
                package.to_string_lossy()
            )).unwrap();
            let h = mare_host::Host::new(env.host().user_config(false).no_session().opts).await.unwrap();
            for reload in [false, true] {
                if reload { h.reload_all().await.unwrap(); }
                let reason = blocked_reason(&h).await.expect("required package hook must block");
                assert!(reason.contains("required-package") && reason.contains("required hook"), "{reason}");
                let turn = h.prompt("hello", json!({})).await.unwrap();
                assert_eq!(turn["not_sent"], true, "{turn}");
            }
            h.shutdown().await;
        }
    }).await
}
