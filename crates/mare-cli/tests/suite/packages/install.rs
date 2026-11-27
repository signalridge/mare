//! Declaring and installing packages: specs, imports, locks, reload and loaded content.

use super::*;

#[tokio::test]
async fn missing_packages_install_at_startup_at_the_matching_tag_lock_and_load() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("hello", None, "v1.2.0");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = \"faux/echo\" }}\nmare.plugins.add {{ {{ \"me/hello\", url = {:?}, version = \"^1\" }} }}\n",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        let h = env.host().user_config(false).build().await;
        let loaded: bool = eval(&h, "return mare.g.hello == true").await;
        assert!(loaded, "the installed package's plugin loads");
        let lock: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(env.config_dir().join("mare-lock.json")).unwrap()).unwrap();
        let tag_commit = String::from_utf8(
            fixture_git(repo.path()).args(["rev-parse", "v1.2.0^{commit}"]).output().unwrap().stdout,
        )
        .unwrap();
        assert_eq!(lock["packages"]["me/hello"]["commit"], tag_commit.trim(), "{lock}");
        assert_eq!(lock["packages"]["me/hello"]["rev"], "version:^1");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_package_needing_a_newer_mare_is_not_loaded_and_says_why() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("future", Some("requires = \">=99.0\"\n"), "v1.0.0");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!("mare.plugins.add {{ {{ \"me/future\", url = {:?} }} }}\n", repo.path().to_string_lossy()),
        )
        .unwrap();
        let mut b = env.host().user_config(false);
        b.opts.config = json!({ "model": "faux/echo" });
        let h = mare_host::Host::new(b.opts).await.expect("startup must not refuse an unavailable package");
        assert_eq!(blocked_reason(&h).await, None);
        let warnings = h.plugin_warnings();
        assert!(warnings.iter().any(|w| w.contains("me/future") && w.contains("needs mare >=99.0")), "{warnings:?}");
        assert!(h.kernel.problems().iter().any(|p| p.contains("me/future")), "{:?}", h.kernel.problems());
        let prompt = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(prompt["status"], "completed", "{prompt}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn monorepo_subdirectories_load_with_independent_options() {
    local(async {
        let env = TestEnv::new().await;
        let repo = tempfile::tempdir().unwrap();
        for (subdir, name) in [("a", "first"), ("b", "second")] {
            let dir = repo.path().join(subdir).join("plugins").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("init.lua"),
                format!(
                    "return {{ name = {name:?}, config_schema = {{ value = {{ type = 'integer', default = 0, desc = 'value' }} }}, apply = function(_, cfg) mare.g[{name:?}] = cfg.value end }}\n"
                ),
            )
            .unwrap();
        }
        git(repo.path(), &["init", "-q", "-b", "main"]);
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "plugins"]);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.plugins.add {{ {{ 'me/mono:a', url = {0:?}, opts = {{ first = {{ value = 11 }} }} }}, {{ 'me/mono:b', url = {0:?}, opts = {{ second = {{ value = 22 }} }} }} }}\n",
                repo.path().to_string_lossy()
            ),
        )
        .unwrap();
        let h = env.host().user_config(false).build().await;
        let values: (Option<i64>, Option<i64>) = eval(&h, "return mare.g.first, mare.g.second").await;
        assert_eq!(values, (Some(11), Some(22)));
        let lock = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap();
        assert!(lock.packages.contains_key("me/mono:a"));
        assert!(lock.packages.contains_key("me/mono:b"));
        assert!(mare_plugin_manager::checkout_dir(&repo.path().to_string_lossy()).join("a/plugins/first/init.lua").is_file());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn declarations_normalize_like_lazy_nvim() {
    local(async {
        let env = TestEnv::new().await;
        let cfg_dir = env.config_dir();
        std::fs::create_dir_all(cfg_dir.join("lua/packages")).unwrap();
        std::fs::write(cfg_dir.join("lua/packages/extra.lua"), "return { \"e/five\" }\n").unwrap();
        std::fs::write(
            cfg_dir.join("init.lua"),
            r#"
mare.plugins.add {
  "a/one:sub",
  { "b/two", enabled = function() return false end },
  { "c/three", dependencies = { "d/four" }, opts = { three = { k = 1, keep = true } } },
  { "c/three", version = "^2", opts = { three = { extra = 1 } } },
  { dir = "~/code/localplug" },
  { import = "packages" },
  { "corp@git.example.com:g/sub/tool.git", dependencies = { "ops@git.example.com:g/lib.git" } },
}
mare.plugin("three", { k = 2 })
"#,
        )
        .unwrap();
        let cfg = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        let specs = mare_plugin_manager::specs(&cfg.value);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "a/one:sub",
                "d/four",
                "c/three",
                "localplug",
                "e/five",
                "ops@git.example.com:g/lib.git",
                "corp@git.example.com:g/sub/tool.git"
            ],
            "b/two disabled; dependency first; scp addresses keep their colon (not `repo:subdir`)"
        );
        assert_eq!(specs[6].url, "corp@git.example.com:g/sub/tool.git");
        assert_eq!(specs[6].subdir, None);
        assert_eq!(specs[0].subdir.as_deref(), Some("sub"));
        assert_eq!(specs[2].target, mare_plugin_manager::Target::Version("^2".into()), "a repeated declaration merges");
        assert!(specs[3].dir.as_ref().unwrap().ends_with("code/localplug"));
        // package opts sit under the plugin's options; mare.plugin wins
        let three = &cfg.value["plugins"]["three"];
        assert_eq!(three["k"], 2, "{three}");
        assert_eq!(three["keep"], true);
    })
    .await
}

#[tokio::test]
async fn repeated_declarations_merge_last_write_wins_and_disabled_or_optional_ones_stay_out() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
-- G1: a dotted write to a table option goes to the user's config, never to the default
local DEFAULT_URL = mare.options.plugins.default.url_format
mare.o.plugins.url_format = "https://git.example.com/%s.git"
mare.o.plugins.timeout = 5
assert(mare.options.plugins.default.url_format == DEFAULT_URL, "the default was changed")
assert(mare.o.plugins.concurrency == mare.options.plugins.default.concurrency, "unset fields read the default")
mare.o.copy.always_full = true
mare.plugins.add {
  { "a/one", only = { "p1" }, dependencies = { "d/one" }, opts_extend = { "one.list" },
    opts = { one = { k = 1, list = { "x" }, deep = { a = 1 } } } },
  { "a/one", only = { "p2" }, dependencies = { "d/two" }, version = "^2",
    opts = { one = { k = 2, list = { "y" }, deep = { b = 2 } } } },
  { "a/one", opts = function(pkg, o) o.one.by = pkg.name end },
  { "b/off", enabled = false, dependencies = { "d/three" }, opts = { off = { v = 1 } } },
  { "c/maybe", optional = true },
  { "e/both", optional = true, opts = { both = { o = 1 } } },
  "e/both",
  { "f/plain", name = "Plain", config = true, init = function() end },
}
mare.plugin("one", { mine = true })
"#,
        )
        .unwrap();
        let cfg = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        assert!(!cfg.warnings.iter().any(|w| w.contains("option")), "no unknown-option warnings: {:?}", cfg.warnings);
        assert_eq!(cfg.value["plugins"]["url_format"], "https://git.example.com/%s.git");
        assert_eq!(cfg.value["plugins"]["timeout"], 5);
        assert_eq!(cfg.value["copy"]["always_full"], true);
        let specs = mare_plugin_manager::specs(&cfg.value);
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["d/one", "d/two", "a/one", "e/both", "f/plain"],
            "disabled, its dependency and optional-only dropped"
        );
        let one = &specs[2];
        assert_eq!(one.url, "https://git.example.com/a/one.git", "mare.o.plugins.url_format applies");
        assert_eq!(one.only.as_deref(), Some(&["p1".to_string(), "p2".to_string()][..]), "only is merged");
        assert_eq!(one.dependencies, ["d/one", "d/two"], "dependencies are merged");
        assert_eq!(one.target, mare_plugin_manager::Target::Version("^2".into()));
        assert_eq!(specs[4].display.as_deref(), Some("Plain"), "name next to the source is only shown");
        // opts: the last write wins (lazy Util.merge), opts_extend appends, a function transforms
        let p = &cfg.value["plugins"]["one"];
        assert_eq!(p["k"], 2, "{p}");
        assert_eq!(p["list"], json!(["x", "y"]));
        assert_eq!(p["deep"], json!({ "a": 1, "b": 2 }));
        assert_eq!(p["by"], "a/one");
        assert_eq!(p["mine"], true, "mare.plugin still wins over package opts");
        assert_eq!(cfg.value["plugins"]["both"]["o"], 1, "declared elsewhere too: the optional one counts");
        assert!(cfg.value["plugins"].get("off").is_none(), "a disabled package's opts stay out");
        let off: Vec<String> = mare_plugin_manager::disabled_specs(&cfg.value).into_iter().map(|s| s.name).collect();
        assert_eq!(off, ["b/off", "d/three"], "listed (and kept in the lock), not installed");
    })
    .await
}

#[tokio::test]
async fn imports_read_modules_once_and_refuse_cycles_and_deep_nesting() {
    local(async {
        let env = TestEnv::new().await;
        let lua_dir = env.config_dir().join("lua");
        for (rel, body) in [
            ("packages/init.lua", "return { \"i/init\" }"),
            ("packages/a.lua", "return { \"i/a\", { import = \"packages.sub\" } }"),
            ("packages/sub/init.lua", "return { \"i/sub\", { import = \"packages\" } }"),
            ("off.lua", "return { \"i/off\" }"),
            ("empty/README", "nothing here"),
        ] {
            let p = lua_dir.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        // a chain d1 → d2 → … → d20
        for i in 1..=20 {
            std::fs::write(
                lua_dir.join(format!("d{i}.lua")),
                format!("return {{ \"c/d{i}\", {{ import = \"d{}\" }} }}", i + 1),
            )
            .unwrap();
        }
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.plugins.add {
  { import = "packages" },
  { import = "packages" },
  { import = "off", enabled = function() return false end },
  { import = "empty" },
  { import = "d1" },
}"#,
        )
        .unwrap();
        let cfg = mare_plugin::config_load::load(&env.cwd, false).await.unwrap();
        let names: Vec<String> = mare_plugin_manager::specs(&cfg.value).into_iter().map(|s| s.name).collect();
        assert_eq!(&names[..3], ["i/init", "i/a", "i/sub"], "{names:?}");
        assert!(!names.iter().any(|n| n == "i/off"), "an import with enabled = false is skipped");
        assert_eq!(names.iter().filter(|n| n.starts_with("c/d")).count(), 16, "{names:?}");
        let w = cfg.warnings.join("\n");
        assert!(w.contains("import cycle packages → packages.sub → packages"), "{w}");
        assert!(w.contains("import empty found no package declarations"), "{w}");
        assert!(w.contains("nested more than 16 deep"), "{w}");
    })
    .await
}

#[tokio::test]
async fn reload_installs_new_packages_unloads_removed_ones_and_runs_config_again() {
    local(async {
        let env = TestEnv::new().await;
        let alpha = package_repo("alpha", None, "v1.0.0");
        let beta = package_repo("beta", None, "v1.0.0");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init = |body: String| std::fs::write(env.config_dir().join("init.lua"), body).unwrap();
        init(format!(
            r#"mare.setup {{ model = "faux/echo" }}
mare.plugins.add {{
  {{ "me/alpha", url = {:?},
     init = function(pkg) mare.g.init_before_apply = (mare.g.alpha == nil) end,
     config = function(pkg, opts) mare.g.config_runs = (mare.g.config_runs or 0) + 1 end }},
  {{ "me/broken", url = {:?}, enabled = false, config = function() mare.g.broken_config = true end }},
}}
"#,
            alpha.path().to_string_lossy(),
            alpha.path().join("missing").to_string_lossy()
        ));
        let mut b = env.host().user_config(false);
        b.opts.config = json!({ "model": "faux/echo" });
        let h = mare_host::Host::new(b.opts).await.unwrap();
        assert!(crate::common::plugin_active(&h.kernel, "alpha"));
        let (before, runs, broken): (bool, i64, bool) =
            eval(&h, "return mare.g.init_before_apply, mare.g.config_runs, mare.g.broken_config == true").await;
        assert!(before, "init runs before the package's plugins load");
        assert_eq!(runs, 1);
        assert!(!broken, "config() of a disabled package doesn't run");
        // init.lua now declares beta instead of alpha: /reload installs beta, unloads alpha
        init(format!(
            r#"mare.setup {{ model = "faux/echo" }}
mare.plugins.add {{
  {{ "me/beta", url = {:?}, config = function() mare.g.config_runs = (mare.g.config_runs or 0) + 1 end }},
}}
"#,
            beta.path().to_string_lossy()
        ));
        let (added, removed): (Vec<String>, Vec<String>) =
            eval(&h, "local r = mare.reload(); return r.added, r.removed").await;
        assert_eq!(added, ["beta"]);
        assert_eq!(removed, ["alpha"]);
        assert!(crate::common::plugin_active(&h.kernel, "beta") && !crate::common::plugin_active(&h.kernel, "alpha"));
        let runs: i64 = eval(&h, "return mare.g.config_runs").await;
        assert_eq!(runs, 2, "packages' config() runs again after the reload");
        let lock = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap();
        assert!(lock.packages.contains_key("me/beta"));
        h.shutdown().await;
    })
    .await
}

/// a package's `skills/`, `prompts/`, `themes/` and `types/` are loaded
/// besides its `plugins/` — skills and prompt templates by context (the user's own win on a
/// name), themes by theme-default (`flavor = "<name>"`), types mirrored for LuaLS.
#[tokio::test]
async fn package_skills_prompts_themes_and_types_are_loaded() {
    local(async {
        let env = TestEnv::new().await;
        let repo = tempfile::tempdir().unwrap();
        let r = repo.path();
        let put = |rel: &str, text: &str| {
            std::fs::create_dir_all(r.join(rel).parent().unwrap()).unwrap();
            std::fs::write(r.join(rel), text).unwrap();
        };
        put("skills/pkg-skill/SKILL.md", "---\nname: pkg-skill\ndescription: from a package\n---\nbody\n");
        put("skills/shadowed/SKILL.md", "---\nname: shadowed\ndescription: the package's\n---\n");
        put("prompts/pkgprompt.md", "package prompt $1\n");
        put("themes/pkgstyles.lua", "return { MareBorderSearch = { fg = '#123456' } }\n");
        put("themes/pkgpal.lua", "return { base = 'light', palette = { accent2 = '#0000aa' } }\n");
        put("types/pkg.lua", "---@meta\n---@class PkgThing\n");
        git(r, &["init", "-q", "-b", "main"]);
        git(r, &["add", "-A"]);
        git(r, &["commit", "-q", "-m", "content"]);
        let user_skill = env.config_dir().join("skills/shadowed/SKILL.md");
        std::fs::create_dir_all(user_skill.parent().unwrap()).unwrap();
        std::fs::write(&user_skill, "---\nname: shadowed\ndescription: the user's\n---\n").unwrap();
        // the user's skill directory (absolute: `~/.config/mare/skills` is outside MARE_HOME)
        let user_skills = env.config_dir().join("skills").to_string_lossy().into_owned();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                "mare.setup {{ model = 'faux/echo', flavor = 'pkgstyles' }}\nmare.plugin('context', {{ skill_dirs = {{ {user_skills:?} }} }})\nmare.plugins.add {{ {{ 'me/content', url = {:?} }} }}\n",
                format!("file://{}", r.display())
            ),
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let (skill, shadowed, prompt, border): (bool, String, bool, String) = eval(
            &h,
            r#"local skill, shadowed = false, nil
               for _, s in ipairs(X:skills()) do
                 if s.name == "pkg-skill" then skill = true end
                 if s.name == "shadowed" then shadowed = s.description end
               end
               local prompt = false
               for _, t in ipairs(X:templates()) do prompt = prompt or t.name == "pkgprompt" end
               return skill, shadowed, prompt, tostring((mare.ui.style("MareBorderSearch") or {}).fg)"#,
        )
        .await;
        assert!(skill, "the package's skill is listed");
        assert_eq!(shadowed, "the user's", "the user's skill shadows the package's");
        assert!(prompt, "the package's prompt template is listed");
        assert_eq!(border.to_lowercase(), "#123456", "the package's theme is the chosen flavor");
        // a palette theme: the built-in light palette with the package's accent2
        exec(&h, "mare.o.flavor = 'pkgpal'").await;
        h.kernel.emit_json("option/changed", json!({ "name": "flavor", "value": "pkgpal" })).await;
        let border: String = eval(&h, "return tostring((mare.ui.style('MareBorderSearch') or {}).fg)").await;
        assert_eq!(border.to_lowercase(), "#0000aa", "palette theme");
        let types = mare_plugin_manager::types_dir();
        assert!(std::fs::read_dir(&types).unwrap().flatten().any(|d| d.path().join("pkg.lua").exists()), "{types:?}");
        h.shutdown().await;
    })
    .await
}

/// `--no-user-plugins` loads no package, so none of a package's content either: `mare.plugins.dirs`
/// is empty and context lists no package skill or prompt.
#[tokio::test]
async fn no_user_plugins_loads_no_package_content() {
    local(async {
        let env = TestEnv::new().await;
        let repo = tempfile::tempdir().unwrap();
        let r = repo.path();
        let put = |rel: &str, text: &str| {
            std::fs::create_dir_all(r.join(rel).parent().unwrap()).unwrap();
            std::fs::write(r.join(rel), text).unwrap();
        };
        put("skills/pkg-skill/SKILL.md", "---\nname: pkg-skill\ndescription: from a package\n---\nbody\n");
        put("prompts/pkgprompt.md", "package prompt $1\n");
        git(r, &["init", "-q", "-b", "main"]);
        git(r, &["add", "-A"]);
        git(r, &["commit", "-q", "-m", "content"]);
        let pkgs = json!({ "packages": [{ "name": "me/content", "url": format!("file://{}", r.display()) }] });
        mare_plugin_manager::sync(&pkgs, &mut |_| ()).unwrap();
        // the test host runs with no_user_plugins (the --no-user-plugins of the CLI)
        let h = env.host().config(pkgs).no_session().build().await;
        global(&h, "X", h.service_table("context").unwrap());
        let (flag, dirs, skill, prompt): (bool, i64, bool, bool) = eval(
            &h,
            r#"local skill, prompt = false, false
               for _, s in ipairs(X:skills()) do skill = skill or s.name == "pkg-skill" end
               for _, t in ipairs(X:templates()) do prompt = prompt or t.name == "pkgprompt" end
               return mare.no_user_plugins, #mare.plugins.dirs("skills") + #mare.plugins.dirs("prompts")
                 + #mare.plugins.dirs("themes") + #mare.plugins.dirs("types"), skill, prompt"#,
        )
        .await;
        assert!(flag, "the flag reaches Lua");
        assert_eq!(dirs, 0, "no package directory");
        assert!(!skill && !prompt, "no package skill or prompt");
        // the same package without the flag: its directories are there (the guard is the flag)
        let n: i64 = eval(&h, r#"mare.no_user_plugins = false return #mare.plugins.dirs("skills")"#).await;
        assert_eq!(n, 1);
        h.shutdown().await;
    })
    .await
}

/// lock: an entry carries the checkout's content hash; `restore` and startup verify it
/// (a mismatch is a clear error), an older lock without one is accepted.
#[tokio::test]
async fn lock_content_hash_is_recorded_and_verified_by_restore_and_at_startup() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("hashed", None, "v1.0.0");
        let cfg = json!({ "packages": [{ "name": "me/hashed", "url": repo.path() }] });
        mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
        let lock_path = mare_shared::paths::lock_file();
        let read =
            || -> serde_json::Value { serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap() };
        let hash = read()["packages"]["me/hashed"]["hash"].as_str().unwrap().to_string();
        assert!(hash.starts_with("sha256:") && hash.len() == 7 + 64, "{hash}");
        assert!(mare_plugin_manager::problems(&cfg).is_empty(), "{:?}", mare_plugin_manager::problems(&cfg));
        mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap();
        // a lock whose hash no longer matches the (unchanged per git) checkout
        let mut lock = read();
        lock["packages"]["me/hashed"]["hash"] = json!(format!("sha256:{}", "0".repeat(64)));
        std::fs::write(&lock_path, lock.to_string()).unwrap();
        let problems = mare_plugin_manager::problems(&cfg).join("\n");
        assert!(problems.contains("me/hashed") && problems.contains("content hash mismatch"), "{problems}");
        let err = mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap_err().to_string();
        assert!(err.contains("content hash mismatch"), "{err}");
        let err = mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap_err().to_string();
        assert!(err.contains("content hash mismatch"), "sync never re-locks over a mismatch: {err}");
        // an older lock without a hash: accepted, nothing reported
        lock["packages"]["me/hashed"].as_object_mut().unwrap().remove("hash");
        std::fs::write(&lock_path, lock.to_string()).unwrap();
        assert!(mare_plugin_manager::problems(&cfg).is_empty(), "{:?}", mare_plugin_manager::problems(&cfg));
        mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap();
        // local edits are flagged elsewhere, not reported as a hash mismatch
        mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
        assert!(read()["packages"]["me/hashed"]["hash"].is_string(), "sync adds the hash to an older entry");
        let checkout = mare_plugin_manager::checkout_dir(&repo.path().to_string_lossy());
        std::fs::write(checkout.join("plugins/hashed/init.lua"), "return { name = 'hashed' }\n").unwrap();
        assert!(mare_plugin_manager::problems(&cfg).is_empty(), "{:?}", mare_plugin_manager::problems(&cfg));
        assert_registered(env.home.path());
    })
    .await
}

/// A host with `me/review` (a package whose plugin is named like the built-in `review`) installed.
async fn review_package_host(env: &TestEnv, pkg: &str, repo: &Path) -> std::sync::Arc<mare_host::Host> {
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(
        env.config_dir().join("init.lua"),
        format!("mare.plugins.add {{ {{ \"{pkg}\", url = {:?} }} }}\n", repo.to_string_lossy()),
    )
    .unwrap();
    let mut b = env.host().user_config(false).no_session();
    b.opts.config = json!({ "model": "faux/echo" });
    mare_host::Host::new(b.opts).await.unwrap()
}

/// a package plugin named like a built-in needs `override = true`
/// too; refused, it is a warning (the built-in stays, even though it is a package plugin), with
/// it the package plugin wins.
#[tokio::test]
async fn a_package_plugin_colliding_with_a_builtin_needs_override() {
    local(async {
        let env = TestEnv::new().await;
        let repo = package_repo("review", None, "v1.0.0");
        let h = review_package_host(&env, "me/review", repo.path()).await;
        let (_, origin, active) = h.kernel.plugin_status().into_iter().find(|p| p.0 == "review").unwrap();
        assert_eq!((origin.as_str(), active), ("builtin", true), "the built-in stays");
        assert!(!eval::<bool>(&h, "return mare.g.review == true").await, "the package plugin didn't run");
        let warnings = h.plugin_warnings();
        assert!(warnings.len() == 1 && warnings[0].contains("built-in `review`"), "{warnings:?}");
        assert!(warnings[0].contains("override = true") && warnings[0].contains("requests continue"), "{warnings:?}");
        assert_eq!(blocked_reason(&h).await, None, "a refused collision doesn't block");
        h.shutdown().await;

        // the same plugin with `override = true`
        let repo = package_repo("review", None, "v1.0.0");
        std::fs::write(
            repo.path().join("plugins/review/init.lua"),
            "return { name = 'review', override = true, apply = function() mare.g.review = true end }",
        )
        .unwrap();
        git(repo.path(), &["add", "-A"]);
        git(repo.path(), &["commit", "-q", "-m", "override"]);
        git(repo.path(), &["tag", "-f", "v1.0.0"]);
        let h = review_package_host(&env, "me/review2", repo.path()).await;
        let (_, origin, active) = h.kernel.plugin_status().into_iter().find(|p| p.0 == "review").unwrap();
        assert_eq!((origin.as_str(), active), ("package", true));
        assert!(eval::<bool>(&h, "return mare.g.review == true").await);
        assert_eq!(h.kernel.plugin_overrides().len(), 1);
        h.shutdown().await;
    })
    .await
}

#[test]
fn concurrent_pkg_processes_serialize_and_merge_lockfile() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();

    let r1 = package_repo("one", None, "v1.0.0");
    let r2 = package_repo("two", None, "v1.0.0");
    std::fs::write(
        cfg.join("init.lua"),
        format!(
            "mare.setup {{ model = \"faux/echo\" }}\nmare.plugins.add {{\n  {{ \"me/one\", url = {:?}, branch = \"main\" }},\n  {{ \"me/two\", url = {:?}, branch = \"main\" }},\n}}\n",
            r1.path().to_string_lossy(),
            r2.path().to_string_lossy(),
        ),
    )
    .unwrap();

    let run_pkg =
        |args: &[&str]| mare_cmd().args(args).env("MARE_HOME", home.path()).current_dir(home.path()).run_output();

    let init = run_pkg(&["plugin", "sync"]);
    assert!(init.status.success(), "{}", String::from_utf8_lossy(&init.stderr));

    // Advance both upstream repos.
    std::fs::write(r1.path().join("v2"), "2").unwrap();
    git(r1.path(), &["add", "v2"]);
    git(r1.path(), &["commit", "-q", "-m", "v2"]);
    std::fs::write(r2.path().join("v2"), "2").unwrap();
    git(r2.path(), &["add", "v2"]);
    git(r2.path(), &["commit", "-q", "-m", "v2"]);

    let rev_head = |p: &Path| {
        String::from_utf8(fixture_git(p).args(["rev-parse", "HEAD"]).output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let h1 = rev_head(r1.path());
    let h2 = rev_head(r2.path());

    // Spawn three concurrent `mare plugin update` processes: two racing on `me/one`, one on `me/two`.
    let spawn = |pkg: &str| {
        let mut cmd = mare_cmd();
        cmd.args(["plugin", "update", pkg])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        ChildGroup::spawn(&mut cmd).unwrap()
    };
    let c1a = spawn("me/one");
    let c1b = spawn("me/one");
    let c2 = spawn("me/two");

    for child in [c1a, c1b, c2] {
        let out = wait_output_timeout(child, DEFAULT_CMD_TIMEOUT);
        assert!(
            out.status.success(),
            "concurrent update failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let lock: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(cfg.join("mare-lock.json")).unwrap()).unwrap();
    assert_eq!(lock["packages"]["me/one"]["commit"], h1, "{lock}");
    assert_eq!(lock["packages"]["me/two"]["commit"], h2, "{lock}");
}

/// (:509-515): real `mare plugin list | restore | clean` CLI subcommands and corrupt-lock
/// isolation (non-zero exit, neither lockfile nor checkouts modified).
/// Ref: Codex `codex-rs/cli/src/plugin_cmd_tests.rs:55` (preserving errors at CLI boundary);
/// reference implementation.
#[test]
fn plugin_cli_loads_only_trusted_project_configuration() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let plugin = tempfile::tempdir().unwrap();
    let project_config = project.path().join(".mare");
    std::fs::create_dir_all(&project_config).unwrap();
    let init = project_config.join("init.lua");
    std::fs::write(&init, "error('untrusted project init was evaluated')\n").unwrap();
    let run = |trust: &str| {
        mare_cmd()
            .args([trust, "plugin", "list", "--json"])
            .env("MARE_HOME", home.path())
            .current_dir(project.path())
            .stdin(std::process::Stdio::null())
            .run_output()
    };
    let denied = run("--no-trust");
    assert!(denied.status.success(), "{}", String::from_utf8_lossy(&denied.stderr));
    assert!(!String::from_utf8_lossy(&denied.stderr).contains("untrusted project init was evaluated"));
    std::fs::write(
        &init,
        format!("mare.plugins.add {{ {{ 'project/example', dir = {:?} }} }}\n", plugin.path().to_string_lossy()),
    )
    .unwrap();
    let allowed = run("--trust");
    assert!(allowed.status.success(), "{}", String::from_utf8_lossy(&allowed.stderr));
    let rows: serde_json::Value = serde_json::from_slice(&allowed.stdout).unwrap();
    assert!(rows.as_array().unwrap().iter().any(|row| row["name"] == "project/example"), "{rows}");
}

#[tokio::test]
async fn trusted_project_sync_and_clean_preserve_other_project_and_offline_load() {
    local(async {
        let mut env = TestEnv::new().await;
        let a = env.work.path().join("a");
        let b = env.work.path().join("b");
        let repo_a = package_repo("scope-a", None, "v1.0.0");
        let repo_b = package_repo("scope-b", None, "v1.0.0");
        for (dir, name, repo) in [(&a, "me/a", repo_a.path()), (&b, "me/b", repo_b.path())] {
            std::fs::create_dir_all(dir.join(".mare")).unwrap();
            std::fs::write(
                dir.join(".mare/init.lua"),
                format!("mare.plugins.add {{ {{ {name:?}, url = {:?}, branch = 'main' }} }}\n", repo.to_string_lossy()),
            )
            .unwrap();
        }
        let run = |dir: &Path, action: &str| {
            mare_cmd()
                .args(["--trust", "plugin", action])
                .env("MARE_HOME", env.home.path())
                .current_dir(dir)
                .stdin(std::process::Stdio::null())
                .run_output()
        };
        let a_sync = run(&a, "sync");
        assert!(a_sync.status.success(), "{}", String::from_utf8_lossy(&a_sync.stderr));
        let a_lock = a.join(".mare/mare-lock.json");
        let before = std::fs::read(&a_lock).ok();
        let a_checkout = checkout_dir_for(env.home.path(), &repo_a.path().to_string_lossy());
        assert!(a_checkout.exists());
        let b_sync = run(&b, "sync");
        assert!(b_sync.status.success(), "{}", String::from_utf8_lossy(&b_sync.stderr));
        let b_clean = run(&b, "clean");
        assert!(b_clean.status.success(), "{}", String::from_utf8_lossy(&b_clean.stderr));
        assert!(a_checkout.exists(), "project B must not remove A's checkout");
        let after = std::fs::read(&a_lock).expect("project A keeps its own lock");
        assert_eq!(before.as_deref(), Some(after.as_slice()), "project B must not alter A's lock");
        assert!(b.join(".mare/mare-lock.json").exists());

        std::fs::rename(repo_a.path().join(".git"), repo_a.path().join("offline-git")).unwrap();
        env.cwd = a;
        let h = env.host().user_config(true).build().await;
        let loaded: Option<bool> = eval(&h, "return mare.g['scope-a']").await;
        assert_eq!(loaded, Some(true), "project A must load its plugin without the source repository");
        h.shutdown().await;
    })
    .await
}

#[test]
fn user_and_project_declarations_write_their_own_locks() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let user_repo = package_repo("user-plugin", None, "v1.0.0");
    let project_repo = package_repo("project-plugin", None, "v1.0.0");
    let config = home.path().join("config");
    let local_config = project.path().join(".mare");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&local_config).unwrap();
    std::fs::write(
        config.join("init.lua"),
        format!("mare.plugins.add {{ {{ 'me/user', url = {:?} }} }}\n", user_repo.path().to_string_lossy()),
    )
    .unwrap();
    std::fs::write(
        local_config.join("init.lua"),
        format!("mare.plugins.add {{ {{ 'me/project', url = {:?} }} }}\n", project_repo.path().to_string_lossy()),
    )
    .unwrap();
    let result = mare_cmd()
        .args(["--trust", "plugin", "sync"])
        .env("MARE_HOME", home.path())
        .current_dir(project.path())
        .stdin(std::process::Stdio::null())
        .run_output();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let user: serde_json::Value =
        serde_json::from_slice(&std::fs::read(config.join("mare-lock.json")).expect("user lock must be written"))
            .unwrap();
    let local: serde_json::Value = serde_json::from_slice(
        &std::fs::read(local_config.join("mare-lock.json")).expect("project lock must be written"),
    )
    .unwrap();
    assert!(user["packages"].get("me/user").is_some() && user["packages"].get("me/project").is_none(), "{user}");
    assert!(local["packages"].get("me/project").is_some() && local["packages"].get("me/user").is_none(), "{local}");

    let user_path = config.join("mare-lock.json");
    let local_path = local_config.join("mare-lock.json");
    let local_before = std::fs::read(&local_path).unwrap();
    std::fs::write(&user_path, "{ corrupt user lock").unwrap();
    let repaired = mare_cmd()
        .args(["--trust", "plugin", "restore", "--repair"])
        .env("MARE_HOME", home.path())
        .current_dir(project.path())
        .stdin(std::process::Stdio::null())
        .run_output();
    assert!(repaired.status.success(), "{}", String::from_utf8_lossy(&repaired.stderr));
    let user_repaired: serde_json::Value = serde_json::from_slice(&std::fs::read(&user_path).unwrap()).unwrap();
    assert_eq!(user_repaired["packages"]["me/user"]["commit"], user["packages"]["me/user"]["commit"]);
    assert_eq!(std::fs::read(&local_path).unwrap(), local_before);

    std::fs::write(&user_path, "{ corrupt user lock").unwrap();
    std::fs::write(&local_path, "{ corrupt project lock").unwrap();
    let doctor = mare_cmd()
        .args(["--trust", "doctor", "--fix"])
        .env("MARE_HOME", home.path())
        .current_dir(project.path())
        .stdin(std::process::Stdio::null())
        .run_output();
    let user_fixed: serde_json::Value = serde_json::from_slice(&std::fs::read(&user_path).unwrap())
        .unwrap_or_else(|_| panic!("doctor did not fix user lock: {}", String::from_utf8_lossy(&doctor.stderr)));
    let local_fixed: serde_json::Value = serde_json::from_slice(&std::fs::read(&local_path).unwrap())
        .unwrap_or_else(|_| panic!("doctor did not fix project lock: {}", String::from_utf8_lossy(&doctor.stderr)));
    assert_eq!(user_fixed["packages"]["me/user"]["commit"], user["packages"]["me/user"]["commit"]);
    assert_eq!(local_fixed["packages"]["me/project"]["commit"], local["packages"]["me/project"]["commit"]);

    let corrupt = b"{ corrupt user lock";
    let future = br#"{"version":999,"packages":{},"future_data":"keep me"}"#;
    std::fs::write(&user_path, corrupt).unwrap();
    std::fs::write(&local_path, future).unwrap();
    let unsupported = mare_cmd()
        .args(["--trust", "plugin", "restore", "--repair"])
        .env("MARE_HOME", home.path())
        .current_dir(project.path())
        .stdin(std::process::Stdio::null())
        .run_output();
    assert!(!unsupported.status.success());
    assert_eq!(std::fs::read(&user_path).unwrap(), corrupt, "preflight all scopes before writing any lock");
    assert_eq!(std::fs::read(&local_path).unwrap(), future);
}

#[test]
fn plugin_cli_can_stash_local_work_and_force_clean_ignored_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let repo = package_repo("probe", None, "v1.0.0");
    std::fs::write(repo.path().join(".gitignore"), "local-build/\n").unwrap();
    git(repo.path(), &["add", ".gitignore"]);
    git(repo.path(), &["commit", "-q", "-m", "ignore artifacts"]);
    let init = config.join("init.lua");
    let declaration = format!(
        "mare.plugins.add {{ {{ 'me/probe', url = {:?}, branch = 'main' }} }}\n",
        repo.path().to_string_lossy()
    );
    std::fs::write(&init, &declaration).unwrap();
    let run = |args: &[&str]| {
        mare_cmd()
            .args(args)
            .env("MARE_HOME", home.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .current_dir(home.path())
            .stdin(std::process::Stdio::null())
            .run_output()
    };
    let output = |out: &std::process::Output| {
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    };
    let sync = run(&["plugin", "sync"]);
    assert!(sync.status.success(), "{}", output(&sync));
    let dir = checkout_dir_for(home.path(), &repo.path().to_string_lossy());
    std::fs::create_dir_all(dir.join("local-build")).unwrap();
    std::fs::write(dir.join("local-build/artifact"), "local output\n").unwrap();
    std::fs::write(&init, "-- no packages\n").unwrap();
    let protected = run(&["plugin", "clean"]);
    assert!(protected.status.success(), "{}", output(&protected));
    assert!(dir.join("local-build/artifact").exists());
    let forced = run(&["plugin", "clean", "--force"]);
    assert!(forced.status.success(), "{}", output(&forced));
    assert!(!dir.exists());
    std::fs::write(&init, &declaration).unwrap();
    let sync = run(&["plugin", "sync"]);
    assert!(sync.status.success(), "{}", output(&sync));

    std::fs::write(dir.join("plugins/probe/init.lua"), "local staged work\n").unwrap();
    git(&dir, &["add", "plugins/probe/init.lua"]);
    std::fs::write(dir.join("notes.txt"), "local untracked work\n").unwrap();
    std::fs::write(repo.path().join("new"), "new\n").unwrap();
    git(repo.path(), &["add", "new"]);
    git(repo.path(), &["commit", "-q", "-m", "update"]);
    let update = run(&["plugin", "update", "--stash"]);
    assert!(update.status.success(), "{}", output(&update));
    assert!(output(&update).contains("stash"), "{}", output(&update));
    assert!(dir.join("new").exists());
    let saved = fixture_git(&dir).args(["show", "stash@{0}:plugins/probe/init.lua"]).output().unwrap();
    assert!(saved.status.success(), "{}", String::from_utf8_lossy(&saved.stderr));
    assert_eq!(String::from_utf8_lossy(&saved.stdout), "local staged work\n");
    let untracked = fixture_git(&dir).args(["show", "stash@{0}^3:notes.txt"]).output().unwrap();
    assert!(untracked.status.success(), "{}", String::from_utf8_lossy(&untracked.stderr));
    assert_eq!(String::from_utf8_lossy(&untracked.stdout), "local untracked work\n");
    std::fs::write(&init, "-- no packages\n").unwrap();
    let protected = run(&["plugin", "clean"]);
    assert!(protected.status.success(), "{}", output(&protected));
    assert!(dir.exists(), "clean must retain saved stashes: {}", output(&protected));
    assert!(output(&protected).contains("stash"), "{}", output(&protected));
}

#[test]
fn plugin_check_reports_updates_and_offline_without_moving_checkout_or_lock() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let repo = package_repo("check", None, "v1.0.0");
    std::fs::write(
        config.join("init.lua"),
        format!(
            "mare.plugins.add {{ {{ 'me/check', url = {:?}, branch = 'main' }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        mare_cmd()
            .args(args)
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(std::process::Stdio::null())
            .run_output()
    };
    let sync = run(&["plugin", "sync"]);
    assert!(sync.status.success(), "{}", String::from_utf8_lossy(&sync.stderr));
    let dir = checkout_dir_for(home.path(), &repo.path().to_string_lossy());
    let lock_path = config.join("mare-lock.json");
    let before = std::fs::read(&lock_path).unwrap();
    let head = fixture_git(&dir).args(["rev-parse", "HEAD"]).output().unwrap().stdout;
    std::fs::write(repo.path().join("change"), "new\n").unwrap();
    git(repo.path(), &["add", "change"]);
    git(repo.path(), &["commit", "-q", "-m", "feat!: change"]);

    let check = run(&["plugin", "check", "--json"]);
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));
    let rows: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(rows[0]["status"], "outdated", "{rows}");
    assert_eq!(rows[0]["behind_count"], 1, "{rows}");
    assert_eq!(rows[0]["breaking"], true, "{rows}");
    assert!(rows[0]["commits"].to_string().contains("feat!: change"), "{rows}");
    assert_eq!(std::fs::read(&lock_path).unwrap(), before);
    assert_eq!(fixture_git(&dir).args(["rev-parse", "HEAD"]).output().unwrap().stdout, head);
    let unknown = run(&["plugin", "check", "absent"]);
    assert!(!unknown.status.success() && String::from_utf8_lossy(&unknown.stderr).contains("absent"));

    std::fs::rename(repo.path().join(".git"), repo.path().join("offline-git")).unwrap();
    let offline = run(&["plugin", "check", "--json"]);
    assert!(offline.status.success(), "{}", String::from_utf8_lossy(&offline.stderr));
    let rows: serde_json::Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(rows[0]["status"], "unreachable", "{rows}");
    assert_eq!(std::fs::read(&lock_path).unwrap(), before);
    assert_eq!(fixture_git(&dir).args(["rev-parse", "HEAD"]).output().unwrap().stdout, head);
}

#[test]
fn plugin_restore_repairs_corrupt_lock_only_from_clean_checkouts() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    let repo = package_repo("repair", None, "v1.0.0");
    std::fs::write(
        config.join("init.lua"),
        format!(
            "mare.plugins.add {{ {{ 'me/repair', url = {:?}, branch = 'main' }} }}\n",
            repo.path().to_string_lossy()
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        mare_cmd()
            .args(args)
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(std::process::Stdio::null())
            .run_output()
    };
    let sync = run(&["plugin", "sync"]);
    assert!(sync.status.success(), "{}", String::from_utf8_lossy(&sync.stderr));
    let dir = checkout_dir_for(home.path(), &repo.path().to_string_lossy());
    let head = fixture_git(&dir).args(["rev-parse", "HEAD"]).output().unwrap().stdout;
    let path = config.join("mare-lock.json");
    let corrupt = b"{ invalid json";
    std::fs::write(&path, corrupt).unwrap();
    let normal = run(&["plugin", "restore"]);
    assert!(!normal.status.success());
    assert!(String::from_utf8_lossy(&normal.stderr).contains(&path.to_string_lossy().to_string()));
    assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    std::fs::write(dir.join("notes.txt"), "local work\n").unwrap();
    let dirty = run(&["plugin", "restore", "--repair"]);
    assert!(!dirty.status.success());
    assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    std::fs::remove_file(dir.join("notes.txt")).unwrap();
    let repaired = run(&["plugin", "restore", "--repair"]);
    assert!(repaired.status.success(), "{}", String::from_utf8_lossy(&repaired.stderr));
    let lock: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(lock["packages"]["me/repair"]["commit"], String::from_utf8_lossy(&head).trim());
    assert_eq!(lock["packages"]["me/repair"]["url"], repo.path().to_string_lossy().as_ref());
    assert!(lock["packages"]["me/repair"]["hash"].as_str().unwrap().starts_with("sha256:"));
    std::fs::write(&path, corrupt).unwrap();
    let doctor = run(&["doctor", "--fix"]);
    let repaired_by_doctor: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap())
        .unwrap_or_else(|_| panic!("doctor did not repair the lock: {}", String::from_utf8_lossy(&doctor.stderr)));
    assert_eq!(repaired_by_doctor["packages"]["me/repair"]["commit"], String::from_utf8_lossy(&head).trim());

    let future = br#"{"version":999,"packages":{},"future_data":"keep me"}"#;
    std::fs::write(&path, future).unwrap();
    let unsupported = run(&["plugin", "restore", "--repair"]);
    assert!(!unsupported.status.success(), "a newer lock format must never be overwritten");
    assert!(String::from_utf8_lossy(&unsupported.stderr).contains("unsupported version"));
    assert_eq!(std::fs::read(&path).unwrap(), future);
}

#[test]
fn pkg_cli_restore_list_clean_and_corrupt_lock_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("config");
    std::fs::create_dir_all(&cfg).unwrap();

    let alpha = package_repo("alpha", None, "v1.0.0");
    std::fs::write(alpha.path().join("VERSION"), "v2").unwrap();
    git(alpha.path(), &["add", "VERSION"]);
    git(alpha.path(), &["commit", "-q", "-m", "v2"]);
    git(alpha.path(), &["tag", "v2.0.0"]);

    let beta = package_repo("beta", None, "v1.0.0");

    let write_init = |with_beta: bool| {
        let mut body = format!(
            "mare.setup {{ model = \"faux/echo\" }}\nmare.plugins.add {{\n  {{ \"me/alpha\", url = {:?}, version = \"^1\" }},\n",
            alpha.path().to_string_lossy()
        );
        if with_beta {
            use std::fmt::Write as _;
            let _ = writeln!(body, "  {{ \"me/beta\", url = {:?}, version = \"^1\" }},", beta.path().to_string_lossy());
        }
        body.push_str("}\n");
        std::fs::write(cfg.join("init.lua"), body).unwrap();
    };
    write_init(true);

    let run_pkg = |args: &[&str]| {
        mare_cmd()
            .args(args)
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(std::process::Stdio::null())
            .run_output()
    };
    let combined = |out: &std::process::Output| {
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    };

    // 1. Initial `mare plugin sync` installs both at v1.0.0 and writes lock.
    let sync = run_pkg(&["plugin", "sync"]);
    assert!(sync.status.success(), "{}", combined(&sync));

    let lock_path = cfg.join("mare-lock.json");
    let lock_bytes_v1 = std::fs::read(&lock_path).unwrap();
    let lock_v1: serde_json::Value = serde_json::from_slice(&lock_bytes_v1).unwrap();
    let alpha_locked = lock_v1["packages"]["me/alpha"]["commit"].as_str().unwrap().to_string();
    let alpha_dir = checkout_dir_for(home.path(), &alpha.path().to_string_lossy());
    let beta_dir = checkout_dir_for(home.path(), &beta.path().to_string_lossy());
    assert!(alpha_dir.join("plugins/alpha/init.lua").is_file());
    assert!(beta_dir.join("plugins/beta/init.lua").is_file());

    // 2. `mare plugin list` reports installed status and short commit.
    let list1 = run_pkg(&["plugin", "list"]);
    assert!(list1.status.success(), "{}", combined(&list1));
    let list1_txt = combined(&list1);
    assert!(list1_txt.contains("me/alpha") && list1_txt.contains(&alpha_locked[..7]), "{list1_txt}");
    assert!(list1_txt.contains("me/beta") && list1_txt.contains("installed"), "{list1_txt}");

    // 3. Move alpha checkout off the locked commit -> `list` flags drift, `restore` puts it back without editing lock.
    git(&alpha_dir, &["checkout", "-q", "v2.0.0"]);
    let head_at = |dir: &Path| {
        String::from_utf8(fixture_git(dir).args(["rev-parse", "HEAD"]).output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    assert_ne!(head_at(&alpha_dir), alpha_locked);

    let list_drift = run_pkg(&["plugin", "list"]);
    assert!(list_drift.status.success(), "{}", combined(&list_drift));
    assert!(combined(&list_drift).contains("not at the locked commit"), "{}", combined(&list_drift));

    let restore = run_pkg(&["plugin", "restore"]);
    assert!(restore.status.success(), "{}", combined(&restore));
    assert_eq!(head_at(&alpha_dir), alpha_locked, "restore must return checkout to locked commit");
    assert_eq!(std::fs::read(&lock_path).unwrap(), lock_bytes_v1, "restore must not rewrite mare-lock.json");

    // 4. Local dirty edit is preserved (not overwritten) by `mare plugin restore`.
    let alpha_init = alpha_dir.join("plugins/alpha/init.lua");
    let orig_alpha_init = std::fs::read_to_string(&alpha_init).unwrap();
    std::fs::write(&alpha_init, "-- local tweak\n").unwrap();
    let restore_dirty = run_pkg(&["plugin", "restore"]);
    assert!(restore_dirty.status.success(), "{}", combined(&restore_dirty));
    assert!(combined(&restore_dirty).contains("local changes"), "{}", combined(&restore_dirty));
    assert_eq!(std::fs::read_to_string(&alpha_init).unwrap(), "-- local tweak\n");
    std::fs::write(&alpha_init, orig_alpha_init).unwrap();

    // 5. Remove me/beta from init.lua -> `mare plugin clean` removes checkout dir and drops beta from lockfile.
    write_init(false);
    let clean = run_pkg(&["plugin", "clean"]);
    assert!(clean.status.success(), "{}", combined(&clean));
    assert!(combined(&clean).contains("dropped me/beta"), "{}", combined(&clean));
    assert!(!beta_dir.exists(), "beta checkout should be deleted by clean");
    assert!(alpha_dir.exists(), "alpha checkout must remain");
    let lock_after_clean: serde_json::Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    assert!(lock_after_clean["packages"].get("me/beta").is_none(), "{lock_after_clean}");
    assert!(lock_after_clean["packages"].get("me/alpha").is_some(), "{lock_after_clean}");

    // 6. Corrupt `mare-lock.json`: all 5 `mare plugin` CLI subcommands fail non-zero and modify nothing.
    let corrupt_bytes = b"{not-valid-json";
    std::fs::write(&lock_path, corrupt_bytes).unwrap();
    let alpha_marker = alpha_dir.join("plugins/alpha/init.lua");
    let alpha_before = std::fs::read(&alpha_marker).unwrap();
    for args in [
        &["plugin", "list"][..],
        &["plugin", "sync"][..],
        &["plugin", "update"][..],
        &["plugin", "restore"][..],
        &["plugin", "clean"][..],
    ] {
        let out = run_pkg(args);
        assert!(!out.status.success(), "`mare {}` must fail on corrupt lock: {}", args.join(" "), combined(&out));
        let txt = combined(&out);
        assert!(txt.contains("corrupt"), "`mare {}` should mention corrupt lock: {txt}", args.join(" "));
        assert_eq!(std::fs::read(&lock_path).unwrap(), corrupt_bytes, "corrupt lock must stay untouched");
        assert_eq!(head_at(&alpha_dir), alpha_locked, "checkout HEAD must stay untouched");
        assert_eq!(std::fs::read(&alpha_marker).unwrap(), alpha_before, "checkout file must stay untouched");
    }

    // Even when `init.lua` removes `me/alpha`, corrupt-lock `clean` must fail before deleting `alpha_dir`.
    std::fs::write(cfg.join("init.lua"), "mare.setup { model = \"faux/echo\" }\n").unwrap();
    let clean_undeclared = run_pkg(&["plugin", "clean"]);
    assert!(!clean_undeclared.status.success(), "{}", combined(&clean_undeclared));
    assert!(alpha_dir.exists(), "corrupt-lock `clean` must not delete undeclared checkouts before failing");
}
