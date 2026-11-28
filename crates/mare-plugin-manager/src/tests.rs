use super::*;
use crate::effective::health::blocked;
use crate::effective::specs::declared_names;
use crate::spec::{dirs_home, expand_home};

#[test]
fn plugin_manifest_reads_plugin_dependencies_and_ignores_the_old_filename() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(concat!("mare-", "pkg.toml")), "name = 'old'\n").unwrap();
    assert!(manifest(dir.path()).unwrap().is_none());
    std::fs::write(
        dir.path().join("mare-plugin.toml"),
        "name = 'planner'\ncategory = 'agent'\nplugins = ['helper']\n[deps]\nrequired = ['owner/repo']\noptional = ['other/repo']\n",
    )
    .unwrap();
    let manifest = serde_json::to_value(manifest(dir.path()).unwrap().unwrap()).unwrap();
    assert_eq!(manifest["name"], "planner");
    assert_eq!(manifest["category"], "agent");
    assert_eq!(manifest["plugins"], serde_json::json!(["helper"]));
    assert_eq!(manifest["deps"]["required"], serde_json::json!(["owner/repo"]));
    assert_eq!(manifest["deps"]["optional"], serde_json::json!(["other/repo"]));
}

#[test]
fn catalog_uses_manifest_short_names_and_category_is_optional() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let root = crate::extras::catalog::catalog_dir_of(DEFAULT_SOURCE).join("plugins");
    for (dir, body) in [("plan-mode", "name = 'plan-mode'\ncategory = 'agent'\n"), ("helper", "name = 'helper'\n")] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("mare-plugin.toml"), body).unwrap();
    }
    let items = extras_for(home.path(), &serde_json::json!({})).unwrap();
    let names: Vec<_> = items.iter().map(|item| item.name.as_str()).collect();
    assert_eq!(names, ["helper", "plan-mode"]);
    assert_eq!(items[1].category, "agent");
}

#[test]
fn catalog_rejects_duplicate_names_from_one_source() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let root = crate::extras::catalog::catalog_dir_of(DEFAULT_SOURCE).join("plugins");
    for dir in ["first", "second"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join("mare-plugin.toml"), "name = 'same'\ncategory = 'agent'\n").unwrap();
    }
    let err = extras_for(home.path(), &serde_json::json!({})).unwrap_err().to_string();
    assert!(err.contains("same"), "{err}");
    assert!(err.contains("duplicate"), "{err}");
}

#[test]
fn trust_checkout_detection_rejects_empty_git_directory() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let url = "https://example.com/me/empty.git";
    let dir = checkout_dir(url);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::write(dir.join("keep.txt"), "local data").unwrap();
    let cfg = serde_json::json!({ "packages": [{ "name": "me/empty", "url": url }] });
    assert!(crate::effective::health::needs_install(&cfg));
    let spec = specs(&cfg).remove(0);
    let err = crate::repo::checkout::ensure_clone(&PkgOptions::default(), &spec, &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("invalid .git"), "{err:#}");
    assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "local data");
}

#[test]
fn trust_clean_does_not_traverse_an_invalid_git_marker() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let dir = checkout_dir("https://example.com/me/broken.git");
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    git_cmd(&nested, &["init", "-q"]);
    let mut lines = vec![];
    clean_only(&serde_json::json!({ "packages": [] }), None, false, &mut |l| lines.push(l)).unwrap();
    assert!(nested.join(".git").is_dir(), "clean removed a repository inside an invalid checkout");
    // kept for its invalid `.git`, not for a dirty status (the old code ran `git status` in it,
    // which fails outside a repository and only looked dirty by accident)
    assert!(
        lines
            .iter()
            .any(|l| l.contains(&format!("kept {} — its .git entry is invalid or inaccessible", dir.display()))),
        "{lines:?}"
    );
}

#[test]
fn specs_parse_targets_subdirs_local_dirs_and_order_dependencies_first() {
    let cfg = serde_json::json!({ "packages": [
        { "name": "a/app", "dependencies": ["b/lib"], "version": "^1.2" },
        { "name": "me/mono", "subdir": "mare", "tag": "v2" },
        { "name": "b/lib", "commit": "abc123" },
        { "dir": "/tmp/local-plugin" },
        { "name": "off/pkg", "enabled": false },
    ]});
    let s = specs(&cfg);
    let names: Vec<&str> = s.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(names, ["b/lib", "a/app", "me/mono", "local-plugin"], "dependency first, disabled dropped");
    assert_eq!(s[1].target, Target::Version("^1.2".into()));
    assert!(s[0].exact_pin(), "a commit is an exact pin");
    assert_eq!(s[2].subdir.as_deref(), Some("mare"));
    assert!(s[2].root().ends_with("me/mono/mare"));
    assert_eq!(s[3].dir.as_deref(), Some(Path::new("/tmp/local-plugin")));
    assert_eq!(s[1].url, "https://github.com/a/app.git");
    assert_eq!(disabled_specs(&cfg).iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["off/pkg"]);
    assert!(declared_names(&cfg).contains("off/pkg"), "a disabled package's lock entry is kept");
}

#[test]
fn pkg_options_short_names_and_unsafe_subdirs() {
    let cfg = serde_json::json!({
        "plugins": { "url_format": "https://gitlab.corp.example/%s.git", "allow_insecure": true, "timeout": 5, "concurrency": 2 },
        "packages": [
            { "name": "group/sub/repo" },
            { "name": "corp@git.example.com:g/r.git" },
            { "name": "x/escape", "subdir": "../../etc" },
        ],
    });
    let o = PkgOptions::from_config(&cfg);
    assert!(o.allow_insecure && o.timeout == Duration::from_secs(5) && o.concurrency == 2);
    let s = specs(&cfg);
    assert_eq!(s.len(), 2, "a subdir leaving the checkout is refused");
    assert_eq!(s[0].url, "https://gitlab.corp.example/group/sub/repo.git");
    assert_eq!(s[1].url, "corp@git.example.com:g/r.git");
    assert_eq!(PkgOptions::from_config(&serde_json::json!({})).timeout, DEFAULT_TIMEOUT);
    assert_eq!(expand_home("~"), dirs_home());
    assert_eq!(expand_home("~/x"), dirs_home().join("x"));
    assert!(source::climbs_out("https://example.com/a/../../b.git"));
    assert!(!source::climbs_out("https://example.com/a/b.git"));
}

#[test]
fn requires_blocks_packages_for_another_mare_version() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("mare-plugin.toml"), "requires = \">=99.0\"\n").unwrap();
    assert!(blocked(d.path()).unwrap().contains("needs mare >=99.0"));
    std::fs::write(d.path().join("mare-plugin.toml"), "requires = \">=0.0.1\"\n").unwrap();
    assert!(blocked(d.path()).is_none());
}

#[test]
fn missing_dependency_and_cycle_surface_in_status_and_doctor() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let dir_c = tempfile::tempdir().unwrap();
    let cfg = serde_json::json!({
        "packages": [
            { "name": "pkg-a", "dir": dir_a.path(), "dependencies": ["pkg-b", "missing-dep"] },
            { "name": "pkg-b", "dir": dir_b.path(), "dependencies": ["pkg-a"] },
            { "name": "pkg-c", "dir": dir_c.path() }
        ]
    });

    let issues = dependency_issues(&cfg);
    assert_eq!(issues.missing.get("pkg-a").map(Vec::as_slice), Some(&["missing-dep".to_string()][..]));
    assert!(!issues.cycles.is_empty(), "expected cycle between pkg-a and pkg-b");

    let doc = doctor_for(&cfg);
    assert!(
        doc.iter().any(|(name, ok, detail)| name == "package dependencies" && !ok && detail.contains("missing-dep"))
    );
    assert!(
        doc.iter().any(|(name, ok, detail)| name == "package dependency cycles" && !ok && detail.contains("pkg-a"))
    );

    let rows = status(&cfg).unwrap();
    let row_a = rows.iter().find(|r| r.name == "pkg-a").unwrap();
    let prob = row_a.problem.as_deref().unwrap_or("");
    assert!(prob.contains("missing dependency: missing-dep"), "{prob}");
    assert!(prob.contains("dependency cycle:"), "{prob}");
}

/// Item 6 (self-written): duplicate package declarations with the same name must be
/// deduplicated (keeping the first) while preserving dependency ordering.
#[test]
fn duplicate_package_declarations_are_deduplicated_by_name() {
    let dir_dep = tempfile::tempdir().unwrap();
    let dir_a1 = tempfile::tempdir().unwrap();
    let dir_a2 = tempfile::tempdir().unwrap();
    let cfg = serde_json::json!({
        "packages": [
            { "name": "dup-pkg", "dir": dir_a1.path(), "dependencies": ["dep-pkg"] },
            { "name": "dep-pkg", "dir": dir_dep.path() },
            { "name": "dup-pkg", "dir": dir_a2.path() }
        ]
    });
    let s = specs(&cfg);
    let names: Vec<&str> = s.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["dep-pkg", "dup-pkg"], "duplicate declaration must be deduplicated");
    assert_eq!(s[1].dir.as_deref(), Some(dir_a1.path()));
}

#[test]
fn manifest_and_resource_dirs_support_custom_declarations() {
    let custom_pkg = tempfile::tempdir().unwrap();
    let default_pkg = tempfile::tempdir().unwrap();

    std::fs::write(
        custom_pkg.path().join("mare-plugin.toml"),
        r#"
name = "custom-pkg"
skills = ["custom/skills", "extra-skills", "../escape", "/abs", "missing"]
prompts = ["my-prompts"]
themes = ["my-themes"]
"#,
    )
    .unwrap();
    for rel in ["skills", "custom/skills", "extra-skills", "my-prompts", "my-themes", "types"] {
        std::fs::create_dir_all(custom_pkg.path().join(rel)).unwrap();
    }
    for kind in ["skills", "prompts", "themes", "types"] {
        std::fs::create_dir_all(default_pkg.path().join(kind)).unwrap();
    }

    let parsed = manifest(custom_pkg.path()).unwrap().unwrap();
    assert_eq!(parsed.skills, vec!["custom/skills", "extra-skills", "../escape", "/abs", "missing"]);
    assert_eq!(parsed.prompts, vec!["my-prompts"]);
    assert_eq!(parsed.themes, vec!["my-themes"]);

    let cfg = serde_json::json!({
        "packages": [
            { "name": "me/custom", "dir": custom_pkg.path() },
            { "name": "me/default", "dir": default_pkg.path() },
        ]
    });

    assert_eq!(
        resource_dirs(&cfg, Some("skills")).unwrap(),
        vec![
            ("me/custom".into(), custom_pkg.path().join("custom/skills")),
            ("me/custom".into(), custom_pkg.path().join("extra-skills")),
            ("me/default".into(), default_pkg.path().join("skills")),
        ]
    );
    assert_eq!(
        resource_dirs(&cfg, Some("prompts")).unwrap(),
        vec![
            ("me/custom".into(), custom_pkg.path().join("my-prompts")),
            ("me/default".into(), default_pkg.path().join("prompts")),
        ]
    );
    assert_eq!(
        resource_dirs(&cfg, Some("themes")).unwrap(),
        vec![
            ("me/custom".into(), custom_pkg.path().join("my-themes")),
            ("me/default".into(), default_pkg.path().join("themes")),
        ]
    );
    assert_eq!(
        resource_dirs(&cfg, Some("types")).unwrap(),
        vec![
            ("me/custom".into(), custom_pkg.path().join("types")),
            ("me/default".into(), default_pkg.path().join("types")),
        ]
    );
    assert!(resource_dirs(&cfg, Some("bogus")).is_err());
}

pub(crate) static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn git_cmd(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_test_repo(dir: &Path, file: &str) -> String {
    std::fs::create_dir_all(dir.join("plugins")).unwrap();
    std::fs::write(dir.join("plugins").join(file), "return { ok = true }\n").unwrap();
    git_cmd(dir, &["init", "-q", "-b", "main"]);
    git_cmd(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git_cmd(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", file]);
    git_cmd(dir, &["rev-parse", "HEAD"])
}

#[test]
fn checkout_lock_blocks_concurrent_holder_and_cancels_cleanly() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };

    let dir = checkout_dir("https://example.com/me/locked.git");
    let guard = crate::lock::checkout_lock(&dir).unwrap();

    let ready = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ready_worker = ready.clone();
    let token = tokio_util::sync::CancellationToken::new();
    let token_worker = token.clone();
    let dir_worker = dir.clone();

    let handle = std::thread::spawn(move || {
        with_cancel(&token_worker, || {
            ready_worker.wait();
            crate::lock::checkout_lock(&dir_worker).map(|_| ())
        })
    });

    ready.wait();
    token.cancel();
    let err = handle.join().unwrap().unwrap_err().to_string();
    assert!(err.contains("cancelled"), "expected cancellation while waiting for lock, got: {err}");

    drop(guard);
    assert!(crate::lock::checkout_lock(&dir).is_ok(), "lock must be acquirable once released");
}

#[test]
fn update_rolls_back_when_lock_commit_fails() {
    if std::process::Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };

    let src = tempfile::tempdir().unwrap();
    let v1 = init_test_repo(src.path(), "v1.lua");
    let url = src.path().to_string_lossy().to_string();
    let cfg = serde_json::json!({ "packages": [{ "name": "me/p", "url": url, "branch": "main" }] });

    sync(&cfg, &mut |_| ()).unwrap();
    let dir = checkout_dir(&url);
    assert!(dir.join("plugins/v1.lua").exists());

    std::fs::write(src.path().join("plugins/v2.lua"), "return { v2 = true }\n").unwrap();
    git_cmd(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git_cmd(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "v2"]);

    *crate::lock::BEFORE_LOCK_COMMIT.lock().unwrap() = Some(Box::new(|| bail!("injected lock write failure")));

    let mut log = vec![];
    let err = update(&cfg, None, false, &mut |l| log.push(l)).unwrap_err().to_string();
    assert!(err.contains("injected lock write failure"), "{err}");
    assert!(log.iter().any(|l| l.contains("rolled back")), "expected rollback log: {log:?}");

    // Checkout and lockfile must both be back at v1.
    assert_eq!(git_cmd(&dir, &["rev-parse", "HEAD"]), v1);
    assert!(dir.join("plugins/v1.lua").exists());
    assert!(!dir.join("plugins/v2.lua").exists());
    assert_eq!(read_lock(&serde_json::json!({})).unwrap().packages["me/p"].commit, v1);
}

#[test]
fn shared_lock_concurrent_refresh_get_and_commit_do_not_deadlock() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };

    let lock = std::sync::Arc::new(crate::lock::SharedLock::new(&serde_json::json!({})).unwrap());
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let l1 = lock.clone();
    let b1 = barrier.clone();
    let t1 = std::thread::spawn(move || {
        for i in 0..32 {
            b1.wait();
            let entry = LockEntry {
                url: format!("https://example.com/me/p{i}.git"),
                commit: format!("deadbeef{i:032}"),
                rev: None,
                branch: Some("main".into()),
                hash: None,
            };
            l1.commit(&format!("me/p{i}"), entry).unwrap();
        }
    });

    let l2 = lock.clone();
    let b2 = barrier.clone();
    let t2 = std::thread::spawn(move || {
        for i in 0..32 {
            b2.wait();
            let _ = l2.refresh_get(&format!("me/p{i}")).unwrap();
        }
    });

    t1.join().unwrap();
    t2.join().unwrap();

    let disk = read_lock(&serde_json::json!({})).unwrap();
    assert_eq!(disk.packages.len(), 32);
}

#[test]
fn pre_checkout_rollback_preserves_existing_dir_without_git() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };

    let o = PkgOptions::default();

    // Pre-existing directory without .git must not be recursively deleted on rollback.
    let existing = home.path().join("existing-no-git");
    std::fs::create_dir_all(&existing).unwrap();
    let keep = existing.join("keep.txt");
    std::fs::write(&keep, "user-data").unwrap();
    let pre_existing = crate::repo::checkout::PreCheckout::capture(&o, existing.clone());
    assert!(pre_existing.dir_existed);
    assert!(!pre_existing.has_git);
    pre_existing.rollback(&o, false, &mut |_| ()).unwrap();
    assert!(existing.exists(), "existing dir without .git must not be removed");
    assert_eq!(std::fs::read_to_string(&keep).unwrap(), "user-data");

    // Newly created directory that did not exist before capture is removed on rollback.
    let fresh = home.path().join("fresh-checkout");
    let pre_fresh = crate::repo::checkout::PreCheckout::capture(&o, fresh.clone());
    assert!(!pre_fresh.dir_existed);
    assert!(!pre_fresh.has_git);
    std::fs::create_dir_all(&fresh).unwrap();
    std::fs::write(fresh.join("partial"), "x").unwrap();
    pre_fresh.rollback(&o, false, &mut |_| ()).unwrap();
    assert!(!fresh.exists(), "fresh partial checkout dir must be removed on rollback");
}

#[test]
fn source_and_spec_security_guards_hold() {
    let flags = crate::source::protocol_flags(false);
    assert!(flags.contains(&"protocol.file.allow=user"), "{flags:?}");
    assert!(
        !flags.iter().any(|f| f.starts_with("protocol.file.allow=") && *f != "protocol.file.allow=user"),
        "{flags:?}"
    );

    assert!(!matches!(crate::source::classify("-oProxyCommand=id:a/b"), crate::source::Source::Url(_)));
    assert!(!matches!(crate::source::classify("user@-host:a/b"), crate::source::Source::Url(_)));
    assert!(crate::source::check("-oProxyCommand=id:a/b", false).is_err());
    assert!(crate::source::check("https://-bad.example/a/b.git", false).is_err());

    assert!(crate::source::has_url_credentials("https://user:pass@example.com/a/b.git"));
    assert_eq!(
        crate::source::strip_url_credentials("https://user:pass@example.com/a/b.git"),
        "https://example.com/a/b.git"
    );

    let o = PkgOptions::default();
    let bad_sub =
        crate::spec::parse_any(&serde_json::json!({"url": "https://example.com/a/b.git", "subdir": "../etc"}), &o);
    assert!(bad_sub.is_none());
    let bad_rev = crate::spec::parse_any(
        &serde_json::json!({"url": "https://example.com/a/b.git", "branch": "--upload-pack=evil"}),
        &o,
    );
    assert!(bad_rev.is_none());
    let bad_commit =
        crate::spec::parse_any(&serde_json::json!({"url": "https://example.com/a/b.git", "commit": "not-hex!"}), &o);
    assert!(bad_commit.is_none());
}

#[test]
fn lock_validation_prunes_stale_refs_and_rejects_bad_entries() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };

    let bad_sha = proj.path().join("bad-sha.json");
    std::fs::write(
        &bad_sha,
        r#"{"version":1,"packages":{"me/p":{"url":"https://example.com/me/p.git","commit":"--bad","hash":"00"}}}"#,
    )
    .unwrap();
    assert!(crate::lock::read_lock_at(&bad_sha).is_err());

    let bad_cred = proj.path().join("bad-cred.json");
    std::fs::write(
        &bad_cred,
        format!(
            r#"{{"version":1,"packages":{{"me/p":{{"url":"https://tok@example.com/me/p.git","commit":"{}","hash":"00"}}}}}}"#,
            "a".repeat(40)
        ),
    )
    .unwrap();
    assert!(crate::lock::read_lock_at(&bad_cred).is_err());

    // Missing referenced lockfile is ignored and pruned when prune_stale=true.
    let checkout = home.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let own_lock = home.path().join("own-mare-lock.json");
    let stale_lock = proj.path().join("gone-mare-lock.json");
    crate::lock::register_checkout_reference(&checkout, &stale_lock).unwrap();
    let refs_dir = crate::lock::checkout_lock_path(&checkout).with_extension("refs");
    assert!(refs_dir.is_dir(), "ref dir should exist before pruning");
    let sha = "a".repeat(40);
    assert!(crate::lock::other_scope_reference(&checkout, &own_lock, Some(&sha), false).unwrap().is_none());
    assert!(refs_dir.is_dir(), "preview must not mutate .refs");
    assert!(crate::lock::other_scope_reference(&checkout, &own_lock, Some(&sha), true).unwrap().is_none());
    assert!(!refs_dir.exists(), "stale ref should be pruned");
}

#[test]
fn clean_protects_unpushed_local_commits_on_branch_and_detached_head() {
    let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    let remote = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };

    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };

    let bare = remote.path().join("upstream.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--bare", "-b", "main"]);
    let seed = remote.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    std::fs::write(seed.join("README.md"), "hi\n").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "init"]);
    git(&seed, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&seed, &["push", "-u", "origin", "main"]);

    let url = "https://example.com/me/demo.git";
    let cfg = serde_json::json!({});
    let checkout_dir = crate::checkout_dir(url);
    std::fs::create_dir_all(checkout_dir.parent().unwrap()).unwrap();
    git(remote.path(), &["clone", bare.to_str().unwrap(), checkout_dir.to_str().unwrap()]);

    // 1. Unpushed commit on a local branch
    std::fs::write(checkout_dir.join("local.txt"), "local\n").unwrap();
    git(&checkout_dir, &["add", "local.txt"]);
    git(&checkout_dir, &["commit", "-m", "unpushed branch commit"]);

    let mut logs = Vec::new();
    crate::ops::clean::remove_unused(&cfg, false, &mut |line| logs.push(line)).unwrap();
    assert!(checkout_dir.exists(), "unpushed branch checkout must not be deleted: {logs:?}");
    assert!(logs.iter().any(|l| l.contains("local-only commits")), "{logs:?}");

    // 2. Unpushed commit on detached HEAD
    logs.clear();
    git(&checkout_dir, &["checkout", "--detach", "HEAD"]);
    crate::ops::clean::remove_unused(&cfg, false, &mut |line| logs.push(line)).unwrap();
    assert!(checkout_dir.exists(), "unpushed detached HEAD checkout must not be deleted: {logs:?}");
    assert!(logs.iter().any(|l| l.contains("local-only commits")), "{logs:?}");
}
