//! First-open extras onboarding, exercised through the virtual TUI.

use super::*;

const OFFER: &str = "Choose optional plugins";

async fn start(env: TestEnv, load_init: bool) -> Term {
    start_with(env, load_init, json!({}), json!({})).await
}

async fn start_with(env: TestEnv, load_init: bool, config: Value, args: Value) -> Term {
    let mut b = env
        .host()
        .builtins(&["onboarding", "plugins"])
        .faux(json!([]))
        .user_plugins()
        .config(json!({ "logo": false }))
        .config(config)
        .args(args);
    b.opts.load_init = load_init;
    b.opts.mode = Mode::Tui;
    let owned = Box::pin(OwnedVt::new(env, b.opts, 110, 32)).await;
    let mut t = Term { owned, parser: vt100::Parser::new(nz(32), nz(110), 2000), ended: 0 };
    t.step().await;
    t
}

async fn stop(mut t: Term) -> TestEnv {
    let mut resources = t.owned.0.take().unwrap();
    Box::pin(resources.vt.shutdown()).await;
    resources._env
}

fn state(t: &Term) -> Value {
    serde_json::from_slice(&std::fs::read(t._env.home.path().join("state/state.json")).unwrap()).unwrap()
}

fn has_plugin_choice(path: &std::path::Path) -> bool {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).unwrap().pointer("/onboarding/plugins").is_some(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => panic!("could not read {}: {err}", path.display()),
    }
}

#[tokio::test]
async fn onboarding_discover_shows_all_catalog_categories_on_one_page() {
    local(async {
        let mut env = TestEnv::new().await;
        let catalog = super::super::packages::catalog(&mut env);
        for (name, category) in [("git-helper", "git"), ("worktree-helper", "worktree")] {
            let dir = catalog.path().join("plugins").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("mare-plugin.toml"), format!("name = {name:?}\ncategory = {category:?}\n"))
                .unwrap();
            std::fs::write(dir.join("init.lua"), format!("return {{ name = {name:?} }}\n")).unwrap();
        }
        for args in [
            vec!["add", "-A"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "categories",
            ],
        ] {
            let out = fixture_git(catalog.path()).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        let mut t = start(env, false).await;
        t.until("first plugin choice", |s| s.contains(OFFER) || s.contains("Discover")).await;
        let screen = t.screen();
        assert!(screen.contains("Discover"), "expected a single Discover page: {screen}");
        assert!(screen.contains("git-helper") && screen.contains("worktree-helper"), "{screen}");
        assert!(screen.contains("git") && screen.contains("worktree"), "{screen}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("skip recorded", |t| {
            std::fs::read_to_string(t._env.home.path().join("state/state.json")).is_ok_and(|s| s.contains("skipped"))
        })
        .await;
        let env = stop(t).await;
        let mut t = start(env, false).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        settled(&mut t).await;
        assert!(!t.screen().contains("Discover"), "skipping should suppress another offer");
    })
    .await;
}

#[tokio::test]
async fn onboarding_persists_the_selected_source_for_duplicate_names() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let other = env.home.path().join("data/packages/.catalogs/github.com/other/catalog/plugins/docs");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("mare-plugin.toml"), "name = 'docs'\ncategory = 'git'\n").unwrap();
        std::fs::write(other.join("init.lua"), "return { name = 'docs' }\n").unwrap();
        let h = env
            .host()
            .builtins(&["plugins"])
            .config(json!({"plugins": {"sources": ["other/catalog"]}}))
            .no_session()
            .build()
            .await;
        global(&h, "P", h.service_table("plugins").unwrap());
        eval::<()>(
            &h,
            r#"mare.plugins.fetch_catalog = function() return { ok = true, log = {} } end
               mare.plugins.run = function() return { ok = true, log = {} } end
               mare.ui.select = function(o)
                   for i, item in ipairs(o.items) do
                       if item.item_id == 'other/catalog#docs' then return {i} end
                   end
                   error('alternate source not offered')
               end
               P:choose_plugins()"#,
        )
        .await;
        let saved: Value =
            serde_json::from_slice(&std::fs::read(env.config_dir().join("plugins.json")).unwrap()).unwrap();
        assert_eq!(saved["plugins"], json!([{ "source": "other/catalog", "name": "docs" }]));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn onboarding_reports_when_two_sources_for_one_name_are_selected() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let other = env.home.path().join("data/packages/.catalogs/github.com/other/catalog/plugins/docs");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("mare-plugin.toml"), "name = 'docs'\ncategory = 'tools'\n").unwrap();
        let h = env
            .host()
            .builtins(&["plugins"])
            .config(json!({"plugins": {"sources": ["other/catalog"]}}))
            .no_session()
            .build()
            .await;
        global(&h, "P", h.service_table("plugins").unwrap());
        let summary: String = eval(
            &h,
            r#"mare.plugins.fetch_catalog = function() return { ok = true, log = {} } end
               mare.plugins.run = function() return { ok = true, log = {} } end
               mare.ui.select = function(o)
                   local selected = {}
                   for i, item in ipairs(o.items) do
                       if item.plugin and item.plugin.name == 'docs' then selected[#selected + 1] = i end
                   end
                   assert(#selected == 2)
                   return selected
               end
               return mare.json.encode(P:choose_plugins())"#,
        )
        .await;
        let summary: Value = serde_json::from_str(&summary).unwrap();
        assert_eq!(summary["enabled"], json!(["docs"]), "{summary}");
        assert_eq!(summary["refused"].as_array().unwrap().len(), 1, "{summary}");
        assert!(summary["refused"][0]["reason"].as_str().unwrap().contains("source"), "{summary}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn plugin_panel_keeps_selections_across_tabs_and_applies_them() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let mut t = Term::in_env(env, json!([]), json!({}), json!({}), 110, 32).await;
        t.submit("/plugins").await;
        t.until("Discover", |s| s.contains("Discover") && s.contains("[ ] alt")).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("selection checked", |s| s.contains("[x] alt")).await;
        t.vt.type_str("alt");
        t.until("filtered selection", |s| s.contains("/ alt") && s.contains("[x] alt")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("installed tab", |s| s.contains("Check updates") && s.contains("Sync")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("sources tab", |s| s.contains("Failed / Errors") && s.contains("Refresh catalogs")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("selection after switching tabs", |s| s.contains("[x] alt")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("sync result", |s| s.contains("plugins sync — done")).await;
        let saved: Value =
            serde_json::from_slice(&std::fs::read(t._env.config_dir().join("plugins.json")).unwrap()).unwrap();
        assert_eq!(saved["plugins"], json!([{ "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "alt" }]));
    })
    .await;
}

async fn choose_now(t: &mut Term) {
    t.until(OFFER, |s| s.contains(OFFER)).await;
    t.until("catalog plugins", |s| s.contains("rust") && s.contains("alt")).await;
}

async fn settled(t: &mut Term) {
    for _ in 0..20 {
        t.step().await;
        // sleep-ok: pump async/TUI steps to verify no delayed offer or sync runs
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

#[tokio::test]
async fn plugins_command_opens_the_panel_and_runtime_tree() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        t.submit("/plugins tree").await;
        t.until("runtime plugin tree", |s| s.contains("Plugins") && s.contains("builtin")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("tree closed", |s| !s.contains("Plugins")).await;
        t.submit("/plugins").await;
        t.until("plugin panel", |s| s.contains("Discover") && s.contains("Installed (") && s.contains("Sources")).await;
    })
    .await
}

#[tokio::test]
async fn credentials_do_not_suppress_the_first_extras_offer() {
    local(async {
        // The faux provider has credentials, so no login step is required.
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "onboarding-test-key");
        let _catalog = super::super::packages::catalog(&mut env);
        let mut t = start(env, false).await;
        t.until(OFFER, |s| s.contains(OFFER)).await;
        assert!(!t.screen().contains("pick a provider to log in"));
    })
    .await
}

#[tokio::test]
async fn disabled_offer_neither_asks_nor_writes_state() {
    local(async {
        let env = TestEnv::new().await;
        let mut t = start_with(env, false, json!({"plugins": {"onboarding": {"plugins": false}}}), json!({})).await;
        settled(&mut t).await;
        assert!(!t.screen().contains(OFFER));
        let path = t._env.home.path().join("state/state.json");
        assert!(!has_plugin_choice(&path));
    })
    .await
}

#[tokio::test]
async fn absent_pkg_service_leaves_onboarding_state_untouched() {
    local(async {
        let env = TestEnv::new().await;
        let mut t = start_with(env, false, json!({"plugins": {"plugins": {"enabled": false}}}), json!({})).await;
        settled(&mut t).await;
        assert!(!t.screen().contains(OFFER));
        let path = t._env.home.path().join("state/state.json");
        assert!(!has_plugin_choice(&path));
    })
    .await
}

#[tokio::test]
async fn resumed_session_does_not_ask_or_write_state() {
    local(async {
        let env = TestEnv::new().await;
        env.write_session(
            "saved.jsonl",
            &[json!({ "type": "message", "entry_id": "m1", "parent_entry_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": "previous session" }] } })],
        );
        let mut t = start_with(env, false, json!({}), json!({"continue": true})).await;
        t.until("resumed session", |s| s.contains("previous session")).await;
        settled(&mut t).await;
        assert!(!t.screen().contains(OFFER));
        let path = t._env.home.path().join("state/state.json");
        assert!(!has_plugin_choice(&path));
    })
    .await
}

#[tokio::test]
async fn child_agent_has_no_extras_question_or_output() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        settled(&mut t).await;
        let child: String = tokio::time::timeout(
            Duration::from_secs(20),
            eval(
                &t.vt.host,
                r#"local a = mare.subagent.start({prompt="child probe", no_session=true,
                     config={model="faux/echo", plugins={onboarding={plugins=true},
                       ["provider-faux"]={script_json='[{"text":"child-ok"}]'}}}})
                   return mare.json.encode(a:wait())"#,
            ),
        )
        .await
        .expect("child must finish without a UI question");
        assert!(child.contains("child-ok"), "{child}");
        assert!(!child.to_lowercase().contains("optional plugins"), "{child}");
        assert!(!t.history().contains(OFFER));
        let path = t._env.home.path().join("state/state.json");
        assert!(!has_plugin_choice(&path));
    })
    .await
}

#[tokio::test]
async fn existing_extras_or_answer_suppress_offer_and_existing_file_is_recorded() {
    local(Box::pin(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let env = Box::pin(async move {
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(env.config_dir().join("plugins.json"), r#"{"version":1,"plugins":[]}"#).unwrap();
            let mut t = start(env, false).await;
            t.until_vt("chosen state", |t| {
                std::fs::read_to_string(t._env.home.path().join("state/state.json")).is_ok_and(|s| s.contains("chosen"))
            })
            .await;
            assert_eq!(state(&t)["onboarding"]["plugins"]["result"], "chosen");
            assert!(!t.screen().contains(OFFER));
            stop(t).await
        })
        .await;
        let env = Box::pin(async move {
            std::fs::remove_file(env.config_dir().join("plugins.json")).unwrap();
            let mut t = start(env, false).await;
            t.until_vt("startup", |t| !t.vt.starting()).await;
            settled(&mut t).await;
            assert!(!t.screen().contains(OFFER));
            stop(t).await
        })
        .await;
        let env = Box::pin(async move {
            std::fs::remove_file(env.home.path().join("state/state.json")).unwrap();
            let mut t = start(env, false).await;
            t.until(OFFER, |s| s.contains(OFFER)).await;
            t.key(KeyCode::Escape, Modifiers::NONE);
            t.until_vt("skipped state", |t| {
                std::fs::read_to_string(t._env.home.path().join("state/state.json"))
                    .is_ok_and(|s| s.contains("skipped"))
            })
            .await;
            stop(t).await
        })
        .await;
        Box::pin(async move {
            let mut t = start(env, false).await;
            settled(&mut t).await;
            assert!(!t.screen().contains(OFFER));
        })
        .await;
    }))
    .await
}

#[tokio::test]
async fn first_answer_is_saved_before_install_and_escape_counts_as_skip() {
    local(Box::pin(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let env = Box::pin(async move {
            let mut t = start(env, false).await;
            choose_now(&mut t).await;
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("selection summary", |s| s.contains("Enabled: none")).await;
            assert_eq!(state(&t)["onboarding"]["plugins"]["result"], "chosen", "must persist before installation");
            stop(t).await
        })
        .await;
        let env = Box::pin(async move {
            let mut t = start(env, false).await;
            t.until_vt("startup", |t| !t.vt.starting()).await;
            settled(&mut t).await;
            assert!(!t.screen().contains(OFFER));
            stop(t).await
        })
        .await;
        let env = Box::pin(async move {
            std::fs::remove_file(env.home.path().join("state/state.json")).unwrap();
            let mut t = start(env, false).await;
            t.until(OFFER, |s| s.contains(OFFER)).await;
            t.key(KeyCode::Escape, Modifiers::NONE);
            t.until_vt("skip saved", |t| {
                std::fs::read_to_string(t._env.home.path().join("state/state.json"))
                    .is_ok_and(|s| s.contains("skipped"))
            })
            .await;
            assert_eq!(state(&t)["onboarding"]["plugins"]["result"], "skipped");
            stop(t).await
        })
        .await;
        Box::pin(async move {
            let mut t = start(env, false).await;
            t.until_vt("startup", |t| !t.vt.starting()).await;
            settled(&mut t).await;
            assert!(!t.screen().contains(OFFER));
        })
        .await;
    }))
    .await
}

#[tokio::test]
async fn recommended_is_first_unchecked_and_empty_selection_does_not_sync() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        let mut t = start(env, false).await;
        choose_now(&mut t).await;
        let screen = t.screen();
        assert!(screen.contains("[ ] ★ rust"), "{screen}");
        assert!(screen.find("rust") < screen.find("alt"), "{screen}");
        t.key(KeyCode::Enter, Modifiers::NONE); // confirm an empty selection
        t.until("empty summary", |s| s.contains("Enabled: none")).await;
        assert!(!t.history().contains("Install selected plugins"), "{}", t.screen());
        assert!(!t._env.config_dir().join("plugins.json").exists());
        assert!(!t._env.config_dir().join("mare-lock.json").exists());
    })
    .await
}

#[tokio::test]
async fn large_catalog_opens_the_extras_panel() {
    local(async {
        let mut env = TestEnv::new().await;
        let catalog = super::super::packages::catalog(&mut env);
        for n in 0..31 {
            let dir = catalog.path().join("plugins").join(format!("extra{n}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("mare-plugin.toml"),
                format!("name = 'extra{n}'\ncategory = 'tools'\ndescription = 'plugin'\n"),
            )
            .unwrap();
            std::fs::write(dir.join("init.lua"), format!("return {{ name = 'extra{n}' }}\n")).unwrap();
        }
        for args in [
            vec!["add", "-A"],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "large"],
        ] {
            let out = fixture_git(catalog.path()).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        let package = super::super::packages::package_repo("rust_tools", None, "v1.0.0");
        super::super::packages::package_mirror(&mut env, &package.path().display().to_string());
        let mut t = start(env, false).await;
        t.until(OFFER, |s| s.contains(OFFER)).await;
        t.vt.type_str("rust");
        t.until("filtered rust", |s| s.contains("/ rust")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("installed summary", |s| s.contains("Installed:")).await;
        let screen = t.screen();
        let installed = screen.lines().find(|line| line.contains("Installed:")).unwrap();
        assert!(installed.contains("someone/rust-tools"), "{screen}");
        assert_eq!(state(&t)["onboarding"]["plugins"]["result"], "chosen");
    })
    .await
}

#[tokio::test]
async fn uncategorized_catalog_plugins_can_be_selected_in_onboarding() {
    local(async {
        let mut env = TestEnv::new().await;
        let catalog = super::super::packages::catalog(&mut env);
        let dir = catalog.path().join("plugins/loose-tool");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mare-plugin.toml"), "name = 'loose-tool'\n").unwrap();
        std::fs::write(dir.join("init.lua"), "return { name = 'loose-tool' }\n").unwrap();
        for args in [
            vec!["add", "-A"],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "add"],
        ] {
            let out = fixture_git(catalog.path()).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        let mut t = start(env, false).await;
        choose_now(&mut t).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("selected plugin", |s| s.contains("[x] loose-tool")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("installation summary", |s| s.contains("Installed:")).await;
        let selected: Value =
            serde_json::from_slice(&std::fs::read(t._env.config_dir().join("plugins.json")).unwrap()).unwrap();
        assert_eq!(selected["version"], 2);
        assert_eq!(
            selected["plugins"],
            json!([{ "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "loose-tool" }])
        );
    })
    .await
}

#[tokio::test]
async fn selected_extras_are_installed_and_loaded_after_reload() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let package = super::super::packages::package_repo("rust_tools", None, "v1.0.0");
        super::super::packages::package_mirror(&mut env, &package.path().display().to_string());
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        let mut t = start(env, false).await;
        choose_now(&mut t).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("rust selected", |s| s.contains("[x] ★ rust")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("docs selected", |s| s.contains("[x] docs")).await;
        assert!(t.screen().contains("permissions"), "{}", t.screen());
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("installation summary", |s| s.contains("Installed:")).await;
        let extras: Value =
            serde_json::from_slice(&std::fs::read(t._env.config_dir().join("plugins.json")).unwrap()).unwrap();
        assert_eq!(extras["version"], 2);
        assert_eq!(
            extras["plugins"],
            json!([
                { "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "docs" },
                { "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "rust" }
            ])
        );
        let lock: Value =
            serde_json::from_slice(&std::fs::read(t._env.config_dir().join("mare-lock.json")).unwrap()).unwrap();
        assert!(lock["packages"]["someone/rust-tools"].is_object(), "{lock}");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            t.step().await;
            if eval::<bool>(
                &t.vt.host,
                "return mare.plugins.status('rust').active and mare.plugins.status('docs').active",
            )
            .await
            {
                break;
            }
            assert!(Instant::now() < deadline, "plugins did not load: {}", t.screen());
            // sleep-ok: poll plugin activation status until 30s deadline
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
}

#[tokio::test]
async fn failed_package_is_reported_without_rolling_back_the_selection() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        super::super::packages::package_mirror(&mut env, "/missing-rust-tools-repository");
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        let mut t = start(env, false).await;
        choose_now(&mut t).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("rust selected", |s| s.contains("[x] ★ rust")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("failed summary", |s| s.contains("Failed:")).await;
        let screen = t.screen();
        let failed = screen.lines().find(|line| line.contains("Failed:")).unwrap();
        assert!(failed.contains("someone/rust-tools"), "{screen}");
        let extras: Value =
            serde_json::from_slice(&std::fs::read(t._env.config_dir().join("plugins.json")).unwrap()).unwrap();
        assert_eq!(extras["version"], 2);
        assert_eq!(extras["plugins"], json!([{ "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "rust" }]));
    })
    .await
}

#[tokio::test]
async fn offline_retry_stops_after_three_attempts() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("GIT_CONFIG_COUNT", "1");
        env.set_var("GIT_CONFIG_KEY_0", "url.file:///does-not-exist-mare-catalog.insteadOf");
        env.set_var("GIT_CONFIG_VALUE_0", "https://github.com/signalridge/mare-plugins.git");
        for attempt in 1..=3 {
            let mut t = start(env, false).await;
            t.until_vt("unavailable saved", |t| {
                std::fs::read(t._env.home.path().join("state/state.json"))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .is_some_and(|state| state["onboarding"]["plugins"]["tries"] == attempt)
            })
            .await;
            assert_eq!(state(&t)["onboarding"]["plugins"]["tries"], attempt);
            t.until("unavailable notice", |s| s.contains("Plugins catalog unavailable:")).await;
            assert_eq!(t.screen().matches("Plugins catalog unavailable:").count(), 1, "{}", t.screen());
            assert!(!t._env.config_dir().join("plugins.json").exists());
            env = stop(t).await;
        }
        let mut t = start(env, false).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        settled(&mut t).await;
        assert!(!t.screen().contains(OFFER));
    })
    .await
}

#[tokio::test]
async fn missing_git_is_reported_as_the_unavailable_cause() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("PATH", env.cwd.clone());
        let mut t = start(env, false).await;
        t.until("unavailable notice", |s| s.contains("Plugins catalog unavailable:")).await;
        let screen = t.screen();
        assert!(screen.contains("git is") && screen.contains("required"), "{screen}");
        assert_eq!(state(&t)["onboarding"]["plugins"]["result"], "unavailable");
        assert!(!t._env.config_dir().join("plugins.json").exists());
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn an_old_git_reports_the_actual_fetch_error() {
    use std::os::unix::fs::PermissionsExt;
    local(async {
        let mut env = TestEnv::new().await;
        let bin = env.cwd.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let git = bin.join("git");
        std::fs::write(
            &git,
            "#!/bin/sh\nfor arg in \"$@\"; do\n  if [ \"$arg\" = \"--version\" ]; then echo 'git version 2.18.0'; exit 0; fi\ndone\necho 'old-git-fetch-error' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        env.set_var("PATH", &bin);
        let mut t = start(env, false).await;
        t.until("unavailable notice", |s| s.contains("Plugins catalog unavailable:")).await;
        let screen = t.screen();
        assert!(screen.contains("old-git-fetch-error"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn headless_is_quiet_and_doctor_reports_only_the_unanswered_step() {
    let home = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let out = mare_cmd()
            .args(["-p", "--model", "faux/echo", "--no-session", "hello"])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .run_output();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!String::from_utf8_lossy(&out.stdout).to_lowercase().contains("optional plugins"));
        assert!(!String::from_utf8_lossy(&out.stderr).to_lowercase().contains("optional plugins"));
    }
    let doctor = || {
        let out = mare_cmd().arg("doctor").env("MARE_HOME", home.path()).current_dir(home.path()).run_output();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let has_hint = |text: &str| text.lines().any(|line| line.contains("plugins") && line.contains("not chosen yet"));
    let before = doctor();
    assert!(has_hint(&before), "{before}");
    std::fs::create_dir_all(home.path().join("state")).unwrap();
    std::fs::write(
        home.path().join("state/state.json"),
        r#"{"version":1,"onboarding":{"plugins":{"v":1,"result":"skipped"}}}"#,
    )
    .unwrap();
    assert!(!has_hint(&doctor()));
}

#[tokio::test]
async fn conflicting_selection_enables_only_the_first_and_explains_refusal() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let package = super::super::packages::package_repo("rust_tools", None, "v1.0.0");
        super::super::packages::package_mirror(&mut env, &package.path().display().to_string());
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        let mut t = start(env, false).await;
        choose_now(&mut t).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("rust selected", |s| s.contains("[x] ★ rust")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("alt selected", |s| s.contains("[x] alt")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("conflicts notice", |s| s.contains("conflicts")).await;
        assert!(t.screen().contains("alt"));
        let extras: Value =
            serde_json::from_slice(&std::fs::read(t._env.config_dir().join("plugins.json")).unwrap()).unwrap();
        assert_eq!(extras["version"], 2);
        assert_eq!(extras["plugins"], json!([{ "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "rust" }]));
    })
    .await
}

#[tokio::test]
async fn rpc_with_a_connected_ui_never_requests_extras_questions() {
    let home = tempfile::tempdir().unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(std::process::Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let mut requests = Vec::new();
    let init = rpc.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |m| {
        requests.push(m["method"].as_str().unwrap_or_default().to_string());
        json!(null)
    });
    assert!(init.get("error").is_none(), "{init}");
    let response = rpc.call_raw("commands.run", json!({"line": "/help"}), |m| {
        requests.push(m["method"].as_str().unwrap_or_default().to_string());
        json!(null)
    });
    assert!(response.get("error").is_none(), "{response}");
    assert!(!requests.iter().any(|method| method == "ui.questions" || method == "ui.select"), "{requests:?}");
    rpc.kill_and_wait();
    let path = home.path().join("state/state.json");
    assert!(!has_plugin_choice(&path));
}

#[tokio::test]
async fn hand_declared_plugin_is_visible_but_not_selected() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.plugins.add { { 'signalridge/mare-plugins', only = { 'docs' } } }",
        )
        .unwrap();
        let mut t = start(env, true).await;
        choose_now(&mut t).await;
        assert!(t.screen().contains("✓ docs"), "{}", t.screen());
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("empty summary", |s| s.contains("Enabled: none")).await;
        assert!(!t._env.config_dir().join("plugins.json").exists());
    })
    .await
}

#[cfg(unix)]
fn install_git_wrapper(env: &mut TestEnv, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let orig_path = std::env::var("PATH").unwrap_or_default();
    let real_git =
        std::env::split_paths(&orig_path).map(|d| d.join("git")).find(|p| p.is_file()).expect("git must exist on PATH");
    let bin = env.cwd.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let git = bin.join("git");
    std::fs::write(&git, format!("#!/bin/sh\nREAL_GIT={:?}\n{body}", real_git.display())).unwrap();
    std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.set_var("PATH", format!("{}:{orig_path}", bin.display()));
}

#[cfg(unix)]
#[tokio::test]
async fn ctrl_c_cancels_catalog_fetch() {
    local(async {
        let mut env = TestEnv::new().await;
        let started = env.cwd.join("catalog.started");
        install_git_wrapper(
            &mut env,
            &format!(
                "for a in \"$@\"; do\n  case \"$a\" in\n    *mare-plugins*) touch {:?}; exec sleep 60;;\n  esac\ndone\nexec \"$REAL_GIT\" \"$@\"\n",
                started.display()
            ),
        );
        let mut t = start(env, false).await;
        t.until_vt("catalog fetch started", |_| started.exists()).await;
        t.ctrl('c');
        t.until("catalog cancelled", |s| s.contains("Plugins catalog unavailable:")).await;
        assert_eq!(state(&t)["onboarding"]["plugins"]["result"], "unavailable");
        assert!(!t._env.config_dir().join("plugins.json").exists());
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn failed_sync_does_not_reload() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let package = super::super::packages::package_repo("rust_tools", None, "v1.0.0");
        super::super::packages::package_mirror(&mut env, &package.path().display().to_string());
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        let clones = env.cwd.join("rust_tools.clones");
        install_git_wrapper(
            &mut env,
            &format!(
                "is_clone=0; is_pkg=0\nfor a in \"$@\"; do\n  case \"$a\" in clone) is_clone=1;; *rust-tools*) is_pkg=1;; esac\ndone\nif [ \"$is_clone\" = 1 ] && [ \"$is_pkg\" = 1 ]; then\n  echo clone >> {:?}\n  if [ \"$(wc -l < {:?})\" -eq 1 ]; then\n    echo 'first-clone-failed' >&2\n    exit 1\n  fi\nfi\nexec \"$REAL_GIT\" \"$@\"\n",
                clones.display(),
                clones.display()
            ),
        );
        let mut t = start(env, true).await;
        choose_now(&mut t).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("rust selected", |s| s.contains("[x] ★ rust")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("failed summary", |s| s.contains("Failed:")).await;
        settled(&mut t).await;
        assert_eq!(std::fs::read_to_string(&clones).unwrap_or_default().lines().count(), 1);
        let lock = std::fs::read_to_string(t._env.config_dir().join("mare-lock.json")).unwrap_or_default();
        assert!(!lock.contains("someone/rust-tools"), "{lock}");
        assert!(eval::<bool>(&t.vt.host, "local s = mare.plugins.status('rust'); return not s.defined and not s.active").await);
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn ctrl_c_cancels_sync_without_reloading() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let package = super::super::packages::package_repo("rust_tools", None, "v1.0.0");
        super::super::packages::package_mirror(&mut env, &package.path().display().to_string());
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        let clones = env.cwd.join("rust_tools.clones");
        let started = env.cwd.join("sync.started");
        install_git_wrapper(
            &mut env,
            &format!(
                "is_clone=0; is_pkg=0\nfor a in \"$@\"; do\n  case \"$a\" in clone) is_clone=1;; *rust-tools*) is_pkg=1;; esac\ndone\nif [ \"$is_clone\" = 1 ] && [ \"$is_pkg\" = 1 ]; then\n  echo clone >> {:?}\n  if [ \"$(wc -l < {:?})\" -eq 1 ]; then\n    touch {:?}\n    exec sleep 60\n  fi\nfi\nexec \"$REAL_GIT\" \"$@\"\n",
                clones.display(),
                clones.display(),
                started.display()
            ),
        );
        let mut t = start(env, true).await;
        choose_now(&mut t).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("rust selected", |s| s.contains("[x] ★ rust")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("sync clone started", |_| started.exists()).await;
        assert_eq!(state(&t)["onboarding"]["plugins"]["result"], "chosen", "save the answer before syncing");
        t.ctrl('c');
        t.until("failed summary", |s| s.contains("Failed:")).await;
        settled(&mut t).await;
        assert_eq!(std::fs::read_to_string(&clones).unwrap_or_default().lines().count(), 1);
        let lock = std::fs::read_to_string(t._env.config_dir().join("mare-lock.json")).unwrap_or_default();
        assert!(!lock.contains("someone/rust-tools"), "{lock}");
        assert!(eval::<bool>(&t.vt.host, "local s = mare.plugins.status('rust'); return not s.defined and not s.active").await);
    })
    .await
}

#[tokio::test]
async fn dependency_extra_is_confirmed_and_reported_when_failed() {
    local(async {
        let mut env = TestEnv::new().await;
        let catalog = super::super::packages::catalog(&mut env);
        let dep_dir = catalog.path().join("plugins/dep");
        std::fs::create_dir_all(&dep_dir).unwrap();
        std::fs::write(
            dep_dir.join("mare-plugin.toml"),
            "name = \"dep\"\ncategory = \"tools\"\ndescription = \"dep\"\n[deps]\nrequired = [\"someone/dep-tools\"]\n",
        )
        .unwrap();
        std::fs::write(dep_dir.join("init.lua"), "return { name = \"dep\" }\n").unwrap();
        std::fs::write(
            catalog.path().join("plugins/rust/mare-plugin.toml"),
            "name = \"rust\"\ncategory = \"lang\"\ndescription = \"Rust\"\nplugins = [\"dep\"]\nconflicts = [\"alt\"]\n[deps]\nrequired = [\"someone/rust-tools\"]\n[recommended]\nroot = [\"Cargo.toml\"]\n",
        )
        .unwrap();
        for args in [
            vec!["add", "-A"],
            vec!["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "dep"],
        ] {
            let out = fixture_git(catalog.path()).args(args).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        let package = super::super::packages::package_repo("rust_tools", None, "v1.0.0");
        super::super::packages::package_mirror(&mut env, &package.path().display().to_string());
        env.set_var("GIT_CONFIG_COUNT", "3");
        env.set_var("GIT_CONFIG_KEY_2", "url.file:///missing-dep-tools-repository.insteadOf");
        env.set_var("GIT_CONFIG_VALUE_2", "https://github.com/someone/dep-tools.git");
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        let mut t = start(env, false).await;
        choose_now(&mut t).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("rust selected", |s| s.contains("[x] ★ rust")).await;
        let preview = t.screen();
        assert!(preview.contains("dep") && preview.contains("someone/rust-tools"), "{preview}");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("failed summary", |s| s.contains("Failed:")).await;
        let screen = t.screen();
        let failed = screen.lines().find(|line| line.contains("Failed:")).unwrap();
        assert!(failed.contains("someone/dep-tools"), "{screen}");
        assert!(!failed.contains("someone/rust-tools"), "{screen}");
    })
    .await
}

/// `plugins.watch` emits a `/reload` hint when a watched config file's content changes,
/// and stays silent on a no-op re-save with identical content.
#[tokio::test]
async fn config_watch_notifies_reload_on_content_change_and_ignores_same_content_save() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init_path = env.config_dir().join("init.lua");
        let initial = "mare.setup { logo = false, plugins = { onboarding = { plugins = false }, plugins = { watch_ms = 20 } } }\n";
        std::fs::write(&init_path, initial).unwrap();
        let mut b = env.host().user_config(true).faux(json!([])).no_session();
        b.opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, b.opts, 120, 20).await;
        let mut t = Term { owned: vt, parser: vt100::Parser::new(nz(20), nz(120), 2000), ended: 0 };
        t.until_vt("startup", |t| !t.vt.starting()).await;

        // Re-writing identical content should not trigger a reload notice.
        std::fs::write(&init_path, initial).unwrap();
        t.step().await;
        assert!(!t.screen().contains("changed — /reload applies it"), "no-op save must not notify:\n{}", t.screen());

        // Changing content triggers the reload hint.
        std::fs::write(&init_path, format!("{initial}-- edited\n")).unwrap();
        t.until("reload notice", |s| s.contains("init.lua changed") && s.contains("/reload")).await;
    })
    .await
}

#[tokio::test]
async fn onboarding_defers_discover_without_stealing_focus_when_user_chats_during_catalog_fetch() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        // Hold fetch_catalog until after the user's first prompt has started a turn, then let it
        // succeed so onboarding reaches the post-fetch Discover check mid-conversation.
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
local orig_fetch = mare.plugins.fetch_catalog
local tx, rx = mare.task.channel()
mare._release_catalog = function() tx:send(true) end
mare.plugins.fetch_catalog = function(...)
  mare._catalog_fetch_started = true
  rx:recv()
  return orig_fetch(...)
end
"#,
        )
        .unwrap();
        let mut b = env
            .host()
            .builtins(&["onboarding", "plugins"])
            .faux(json!([{ "text": "hello-back" }, { "text": "second-ok" }]))
            .user_plugins()
            .config(json!({ "logo": false, "plugins": { "onboarding": { "plugins": true } } }));
        b.opts.load_init = true;
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 110, 32).await;
        let mut t = Term { owned, parser: vt100::Parser::new(nz(32), nz(110), 2000), ended: 0 };
        t.until_vt("catalog fetch started", |t| {
            t.vt.host.lua().load("return mare._catalog_fetch_started == true").eval::<bool>().unwrap_or(false)
        })
        .await;

        // User starts chatting while catalog fetch is still in flight.
        t.submit("hi there").await;
        t.until("first answer", |s| s.contains("hello-back")).await;

        // Now release the catalog fetch: Discover must NOT steal focus; a /plugins notice appears.
        exec(&t.vt.host, "mare._release_catalog()").await;
        t.until("deferred plugins notice", |s| s.contains("/plugins")).await;
        settled(&mut t).await;
        assert!(!t.screen().contains(OFFER), "Discover modal must not pop up after chat started:\n{}", t.screen());
        assert!(!has_plugin_choice(&t._env.home.path().join("state/state.json")));

        // Input box must still accept a follow-up prompt without pressing Escape.
        t.submit("follow up").await;
        t.until("second answer", |s| s.contains("second-ok")).await;
        assert!(!t.screen().contains(OFFER), "Discover modal must stay closed:\n{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn onboarding_defers_discover_when_session_already_has_messages() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = super::super::packages::catalog(&mut env);
        let h = env
            .host()
            .builtins(&["onboarding", "plugins"])
            .faux(json!([{ "text": "prior-reply" }]))
            .user_plugins()
            .config(json!({ "logo": false, "plugins": { "onboarding": { "plugins": false } } }))
            .no_session()
            .build()
            .await;
        global(&h, "P", h.service_table("plugins").unwrap());

        let res = h.prompt("existing message", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");

        let (is_nil, fetched, selected, notice): (bool, bool, bool, String) = eval(
            &h,
            r#"
            local fetched, selected, notice = false, false, ""
            mare.plugins.fetch_catalog = function() fetched = true; return { ok = true, log = {} } end
            mare.ui.select = function() selected = true; return nil end
            local orig_notify = mare.ui.notify
            mare.ui.notify = function(msg, level)
              notice = tostring(msg)
              if orig_notify then return orig_notify(msg, level) end
            end
            local s = P:choose_plugins()
            return s == nil, fetched, selected, notice
            "#,
        )
        .await;
        h.shutdown().await;

        assert!(is_nil, "choose_plugins must return nil when deferred for active conversation");
        assert!(!fetched, "must not fetch catalog when conversation already has messages");
        assert!(!selected, "must not open Discover modal when conversation already has messages");
        assert!(notice.contains("/plugins"), "expected /plugins hint in notification: {notice:?}");
    })
    .await
}
