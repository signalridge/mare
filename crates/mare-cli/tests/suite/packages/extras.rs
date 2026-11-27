//! Extras recommended by project markers, and the news after an update.

use super::*;

#[tokio::test]
async fn overlapping_plugin_operations_do_not_lose_the_first_cancellation_token() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        let cancelled: bool = eval(
            &h,
            r#"local entered_tx, entered_rx = mare.task.channel()
               local release_tx, release_rx = mare.task.channel()
               local interrupt
               local ctx = { bus = { emit = function() end } }
               function ctx:inject(_, callback)
                   callback({ ui = { handler = function(_, _, _, _, f) interrupt = f end } })
               end
               mare.ui.working = function() end
               mare.plugins.run = function(_, name, opts)
                   if name == 'first' then
                       entered_tx:send(opts.cancel)
                       release_rx:recv()
                   end
                   return { ok = true, log = {} }
               end
               local run = require('plugins.extras_list').runner(ctx)
               mare.task.spawn(function() run('sync', 'first', true) end)
               local token = entered_rx:recv()
               run('sync', 'second', true)
               interrupt()
               local cancelled = token:is_cancelled()
               release_tx:send(true)
               return cancelled"#,
        )
        .await;
        assert!(cancelled, "the first operation must remain cancellable after a second invocation");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn unenabled_recommendation_code_is_never_run_by_catalog_or_panel() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = catalog(&mut env);
        let root = env.home.path().join("data/packages/.catalogs/github.com/signalridge/mare-plugins/plugins/docs");
        let marker = env.cwd.join("recommendation-executed");
        std::fs::write(
            root.join("mare-plugin.toml"),
            "name = 'docs'\ncategory = 'tools'\n[recommended]\nlua = 'recommended.lua'\n",
        )
        .unwrap();
        std::fs::write(
            root.join("recommended.lua"),
            "return function() mare.fs.write(mare.cwd .. '/recommendation-executed', 'ran'); return true end\n",
        )
        .unwrap();
        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        eval::<()>(&h, "mare.plugins.catalog()").await;
        assert!(!marker.exists(), "catalog loading executed an unenabled plugin");
        eval::<()>(
            &h,
            "ui.select = function() return nil end; require('plugins.panel').create({}, function() end)()",
        )
        .await;
        assert!(!marker.exists(), "opening Discover executed an unenabled plugin");
        let diagnostics: String = eval(&h, "return mare.json.encode(mare.plugins.diagnostics())").await;
        assert!(diagnostics.contains("docs") && diagnostics.contains("lua"), "{diagnostics}");
        global(&h, "P", h.service_table("plugins").unwrap());
        eval::<()>(
            &h,
            "mare.plugins.fetch_catalog = function() return {ok=true, log={}} end; mare.ui.select = function() return nil end; P:choose_plugins()",
        )
        .await;
        assert!(!marker.exists(), "onboarding executed an unenabled plugin");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn plugin_panel_exposes_three_tabs_and_catalog_metadata() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = catalog(&mut env);
        let root = env.home.path().join("data/packages/.catalogs/github.com/signalridge/mare-plugins/plugins/docs");
        std::fs::write(
            root.join("mare-plugin.toml"),
            "name = 'docs'\ncategory = 'git'\nversion = '2.3.4'\nplatforms = ['linux', 'macos', 'windows']\n[provides]\ntools = ['search-docs']\ncommands = ['docs']\n",
        )
        .unwrap();
        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        let result: String = eval(
            &h,
            r#"local captured
               ui.select = function(o) captured = o; return nil end
               require('plugins.panel').create({}, function() end)()
               local names = {}
               for _, t in ipairs(captured.tabs or {}) do names[#names + 1] = t.name end
               local docs
               for _, e in ipairs(mare.plugins.catalog()) do if e.name == 'docs' then docs = e end end
               return mare.json.encode({tabs = names, docs = docs})"#,
        )
        .await;
        let result: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["tabs"][0], "Discover");
        assert!(result["tabs"][1].as_str().unwrap().starts_with("Installed ("));
        assert_eq!(result["tabs"][2], "Sources");
        assert_eq!(result["docs"]["provides"]["tools"], json!(["search-docs"]));
        assert_eq!(result["docs"]["platforms"], json!(["linux", "macos", "windows"]));
        assert_eq!(result["docs"]["version"], "2.3.4");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn plugin_panel_applies_a_duplicate_name_from_the_selected_source() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = catalog(&mut env);
        let other = env.home.path().join("data/packages/.catalogs/github.com/other/catalog/plugins/docs");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("mare-plugin.toml"), "name = 'docs'\ncategory = 'git'\n").unwrap();
        let h = env
            .host()
            .builtins(&["plugins"])
            .config(json!({"plugins": {"sources": ["other/catalog"]}}))
            .no_session()
            .build()
            .await;
        eval::<()>(
            &h,
            r#"local calls = 0
               ui.select = function(o)
                   calls = calls + 1
                   for i, item in ipairs(o.tabs[1].items) do
                       if item.item_id == 'other/catalog#docs' then
                           return i, 1, calls == 1 and ' ' or nil
                       end
                   end
                   error('alternate source not offered')
               end
               local ctx = {bus = {emit = function() end}}
               require('plugins.panel').create(ctx, function() return {ok = true, log = {}} end)()"#,
        )
        .await;
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(env.config_dir().join("plugins.json")).unwrap()).unwrap();
        assert_eq!(saved["plugins"], json!([{ "source": "other/catalog", "name": "docs" }]));
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn rewritten_git_origin_keeps_the_locked_commit_and_repairs_its_source() {
    local(async {
        let mut env = TestEnv::new().await;
        let catalog = catalog(&mut env);
        let cfg = json!({});
        mare_plugin_manager::set_extra("docs", true, None, &cfg).unwrap();
        mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
        let path = env.config_dir().join("mare-lock.json");
        let before = std::fs::read(&path).unwrap();
        let lock: serde_json::Value = serde_json::from_slice(&before).unwrap();
        let first = lock["packages"][mare_plugin_manager::DEFAULT_SOURCE]["commit"].as_str().unwrap();
        let checkout = mare_plugin_manager::checkout_dir("https://github.com/signalridge/mare-plugins.git");

        std::fs::write(catalog.path().join("upstream-change"), "new\n").unwrap();
        git(catalog.path(), &["add", "upstream-change"]);
        git(catalog.path(), &["commit", "-q", "-m", "new upstream commit"]);
        mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
        let head = fixture_git(&checkout).args(["rev-parse", "HEAD"]).output().unwrap().stdout;
        assert_eq!(String::from_utf8_lossy(&head).trim(), first, "sync must honor the existing lock");
        assert_eq!(std::fs::read(&path).unwrap(), before);

        std::fs::write(&path, "{ broken lock").unwrap();
        mare_plugin_manager::repair_lock(&cfg).unwrap();
        let repaired: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            repaired["packages"][mare_plugin_manager::DEFAULT_SOURCE]["url"],
            "https://github.com/signalridge/mare-plugins.git"
        );
        assert_eq!(repaired["packages"][mare_plugin_manager::DEFAULT_SOURCE]["commit"], first);
        mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap();
    })
    .await;
}

#[tokio::test]
async fn selections_from_one_source_keep_user_and_project_locks_separate() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = catalog(&mut env);
        let dependency = package_repo("rust-tools", None, "v1.0.0");
        package_mirror(&mut env, &dependency.path().to_string_lossy());
        let project = env.cwd.join(".mare");
        std::fs::create_dir_all(&project).unwrap();
        let cfg = json!({ mare_plugin_manager::PROJECT_SCOPE_KEY: project.to_string_lossy() });
        mare_plugin_manager::set_extra("docs", true, None, &json!({})).unwrap();
        mare_plugin_manager::set_extra("rust", true, None, &cfg).unwrap();

        let specs = mare_plugin_manager::specs(&cfg);
        let from_source: Vec<_> =
            specs.iter().filter(|spec| spec.name == mare_plugin_manager::DEFAULT_SOURCE).collect();
        assert_eq!(from_source.len(), 2, "both scopes must retain a source declaration: {from_source:?}");
        assert!(
            from_source.iter().any(|spec| !spec.project_lock && spec.only.as_deref() == Some(&["docs".into()][..]))
        );
        assert!(from_source.iter().any(|spec| spec.project_lock && spec.only.as_deref() == Some(&["rust".into()][..])));
        let mut log = Vec::new();
        mare_plugin_manager::sync(&cfg, &mut |line| log.push(line)).unwrap_or_else(|err| panic!("{err:#}: {log:?}"));
        let user: serde_json::Value =
            serde_json::from_slice(&std::fs::read(env.config_dir().join("mare-lock.json")).unwrap()).unwrap();
        let local: serde_json::Value =
            serde_json::from_slice(&std::fs::read(project.join("mare-lock.json")).unwrap()).unwrap();
        assert_eq!(
            user["packages"][mare_plugin_manager::DEFAULT_SOURCE]["commit"],
            local["packages"][mare_plugin_manager::DEFAULT_SOURCE]["commit"]
        );
        assert!(local["packages"].get("someone/rust-tools").is_some());
        assert!(user["packages"].get("someone/rust-tools").is_none());
        let names: Vec<_> =
            mare_plugin::loader::package_sources(&cfg).into_iter().map(|source| source.name_hint).collect();
        assert!(names.iter().any(|name| name == "docs"), "{names:?}");
        assert!(names.iter().any(|name| name == "rust"), "{names:?}");
    })
    .await;
}

#[tokio::test]
async fn plugin_service_installs_and_uninstalls_a_catalog_short_name() {
    local(async {
        let mut env = TestEnv::new().await;
        let catalog = catalog(&mut env);
        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        global(&h, "P", h.service_table("plugins").unwrap());
        let installed: bool = eval(
            &h,
            "P:install('docs'); for _, p in ipairs(mare.plugins.catalog()) do if p.name == 'docs' then return p.enabled end end return false",
        )
        .await;
        assert!(installed, "install must enable the selected catalog plugin");
        std::fs::write(catalog.path().join("service-update"), "new\n").unwrap();
        git(catalog.path(), &["add", "service-update"]);
        git(catalog.path(), &["commit", "-q", "-m", "service update"]);
        eval::<()>(&h, "P:update('docs')").await;
        assert!(
            mare_plugin_manager::checkout_dir("https://github.com/signalridge/mare-plugins.git").join("service-update").exists(),
            "update must resolve the plugin's source"
        );
        eval::<()>(&h, "P:uninstall('docs')").await;
        let selected = mare_plugin_manager::read_extras(&serde_json::json!({})).unwrap();
        assert!(selected.plugins.is_empty());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cli_installs_updates_restores_and_uninstalls_a_short_plugin_name() {
    local(async {
        let mut env = TestEnv::new().await;
        let catalog = catalog(&mut env);
        let run =
            |args: &[&str]| mare_cmd().args(args).env("MARE_HOME", env.home.path()).current_dir(&env.cwd).run_output();
        let install = run(&["plugin", "install", "docs"]);
        assert!(install.status.success(), "{}", String::from_utf8_lossy(&install.stderr));
        let path = env.config_dir().join("plugins.json");
        let selected: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(selected["version"], 2);
        assert_eq!(selected["plugins"], json!([{ "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "docs" }]));

        std::fs::write(catalog.path().join("new-version"), "new\n").unwrap();
        git(catalog.path(), &["add", "new-version"]);
        git(catalog.path(), &["commit", "-q", "-m", "new version"]);
        let update = run(&["plugin", "update", "docs"]);
        assert!(update.status.success(), "{}", String::from_utf8_lossy(&update.stderr));
        let checkout = mare_plugin_manager::checkout_dir("https://github.com/signalridge/mare-plugins.git");
        assert!(checkout.join("new-version").exists(), "the plugin source was not updated");
        let restore = run(&["plugin", "restore", "docs"]);
        assert!(restore.status.success(), "{}", String::from_utf8_lossy(&restore.stderr));

        let uninstall = run(&["plugin", "uninstall", "docs"]);
        assert!(uninstall.status.success(), "{}", String::from_utf8_lossy(&uninstall.stderr));
        let selected: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(selected["plugins"], json!([]));
    })
    .await
}

#[tokio::test]
async fn extras_are_recommended_by_project_markers_never_conflicting_and_renames_migrate() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = catalog(&mut env);
        std::fs::write(env.cwd.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(env.cwd.join("alt.marker"), "").unwrap();
        // recommended by the project's marker file; nothing enabled yet
        let list = mare_plugin_manager::extras(&env.cwd).unwrap();
        let get = |l: &[mare_plugin_manager::Extra], n: &str| l.iter().find(|e| e.name == n).unwrap().clone();
        assert!(get(&list, "rust").recommended && get(&list, "alt").recommended);
        assert!(!get(&list, "docs").recommended);
        // enabling one: a conflicting extra is neither recommended nor allowed
        mare_plugin_manager::set_extra("rust", true, None, &serde_json::json!({})).unwrap();
        let list = mare_plugin_manager::extras(&env.cwd).unwrap();
        assert!(get(&list, "rust").enabled && !get(&list, "alt").recommended);
        assert!(
            mare_plugin_manager::set_extra("alt", true, None, &serde_json::json!({}))
                .unwrap_err()
                .to_string()
                .contains("conflicts")
        );
        // enabled extras declare the catalog repo (only their plugins) and their required packages
        let specs = mare_plugin_manager::specs(&json!({}));
        let repo = specs.iter().find(|s| s.name == mare_plugin_manager::DEFAULT_SOURCE).unwrap();
        assert_eq!(repo.only.as_deref(), Some(&["rust".to_string()][..]));
        assert!(specs.iter().any(|s| s.name == "someone/rust-tools"));
        // an explicit `enabled = false` in init.lua wins
        let off = json!({ "packages": [{ "name": "someone/rust-tools", "enabled": false }] });
        assert!(!mare_plugin_manager::specs(&off).iter().any(|s| s.name == "someone/rust-tools"));
        // the catalog's rename table migrates plugins.json and writes it back
        std::fs::write(env.config_dir().join("plugins.json"), r#"{"version":1,"plugins":["rusty","old"]}"#).unwrap();
        let notices = mare_plugin_manager::migrate_extras().join("\n");
        assert!(notices.contains("rusty is now rust") && notices.contains("old was removed"), "{notices}");
        assert_eq!(
            mare_plugin_manager::read_extras(&serde_json::json!({})).unwrap().plugins,
            [mare_plugin_manager::PluginSelection {
                source: mare_plugin_manager::DEFAULT_SOURCE.into(),
                name: "rust".into()
            }]
        );
    })
    .await
}

#[tokio::test]
async fn news_is_shown_once_after_an_upgrade_and_not_on_a_fresh_install() {
    local(async {
        let mut env = TestEnv::new().await;
        let _catalog = catalog(&mut env);
        mare_plugin_manager::set_extra("docs", true, None, &serde_json::json!({})).unwrap();
        let h = env.host().builtins(&["plugins"]).no_session().build().await;
        global(&h, "P", h.service_table("plugins").unwrap());
        // Keep the test self-contained with its own mare version and NEWS.md section so it
        // does not depend on Cargo.toml's package version matching preset/NEWS.md (e.g. on a
        // release-please PR branch before release notes are written).
        eval::<()>(
            &h,
            r###"
            mare.version = "9.9.9-fixture"
            mare.plugins.mare_news = function()
                return "## 9.9.9-fixture\n- fixture mare release notes\n"
            end
            "###,
        )
        .await;
        // fresh install: versions recorded, nothing shown
        let shown: Vec<String> = eval(&h, "return P:news()").await;
        assert!(shown.is_empty(), "{shown:?}");
        // pretend both were older: their sections show once
        let state = env.home.path().join("state/news.json");
        let mut seen: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        assert_eq!(seen["seen"]["mare"], json!("9.9.9-fixture"), "{seen}");
        seen["seen"]["mare"] = json!("0.0.1");
        seen["seen"]["plugin:docs"] = json!("1.0.0");
        std::fs::write(&state, seen.to_string()).unwrap();
        let shown: Vec<String> = eval(&h, "return P:news()").await;
        assert!(shown.iter().any(|s| s == "What's new in mare 9.9.9-fixture"), "{shown:?}");
        assert!(shown.iter().any(|s| s == "What's new in docs 1.1.0"), "{shown:?}");
        let again: Vec<String> = eval(&h, "return P:news()").await;
        assert!(again.is_empty(), "shown once: {again:?}");

        // Concurrent upgrade checks serialized under the cross-process lock: only the winning writer shows news.
        let mut stale_doc: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
        stale_doc["seen"]["mare"] = json!("0.0.1");
        stale_doc["seen"]["plugin:docs"] = json!("1.0.0");
        std::fs::write(&state, stale_doc.to_string()).unwrap();
        let (c1, c2): (Vec<String>, Vec<String>) = eval(
            &h,
            r#"
            local tx1, rx1 = mare.task.channel()
            local tx2, rx2 = mare.task.channel()
            mare.task.spawn(function() tx1:send(P:news()) end)
            mare.task.spawn(function() tx2:send(P:news()) end)
            return rx1:recv(), rx2:recv()
            "#,
        )
        .await;
        assert!(
            c1.is_empty() != c2.is_empty(),
            "exactly one concurrent caller must claim and show news: c1={c1:?} c2={c2:?}"
        );

        // Valid roundtrip preserves existing entries; corrupt/newer-version/null-seen files are left untouched.
        for bad in [
            "{not json",
            r#"{"version":99,"seen":{"mare":"0.0.1"}}"#,
            "[]",
            r#"{"version":1,"seen":null}"#,
            r#"{"version":1,"seen":{"mare":123}}"#,
        ] {
            std::fs::write(&state, bad).unwrap();
            let res: Vec<String> = eval(&h, "return P:news()").await;
            assert!(res.is_empty(), "corrupt/unsupported news.json should not trigger display: {res:?}");
            assert_eq!(
                std::fs::read_to_string(&state).unwrap(),
                bad,
                "corrupt/unsupported news.json must remain intact on disk"
            );
        }

        // Cancelled task inside P:news() must release news.json.lock deterministically via <close>
        // without waiting for GC.
        let cancel_released: bool = eval(
            &h,
            r#"
            local orig_cat = mare.plugins.catalog
            local hold_tx, hold_rx = mare.task.channel()
            local cancel_tx, cancel_rx = mare.task.channel()
            local _, never_rx = mare.task.channel()
            mare.plugins.catalog = function(...)
                hold_tx:send(true)
                never_rx:recv()
                return orig_cat(...)
            end
            mare.task.spawn(function()
                assert(mare.task.timeout(5000, function() return hold_rx:recv() end) == true)
                cancel_tx:send(true)
            end)
            mare.task.select(function()
                P:news()
            end, function()
                cancel_rx:recv()
            end)
            mare.plugins.catalog = orig_cat
            local ok_lock, g = pcall(mare.lock.file, mare.paths.state .. "/news.json.lock", {
                timeout_ms = 100,
                root = mare.paths.state,
            })
            if ok_lock and g then g:release() end
            return ok_lock == true
        "#,
        )
        .await;
        assert!(cancel_released, "cancelled news() coroutine must release news.json.lock via <close> immediately");
        h.shutdown().await;
    })
    .await
}
