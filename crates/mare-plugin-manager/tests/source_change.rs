//! Changing a package's source URL must move the checkout to the new repository (and the lock
//! must describe what is actually checked out).

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git").current_dir(dir).args(args).output().unwrap().status.success();
    assert!(ok, "git {args:?}");
}

fn repo(dir: &Path, file: &str) {
    std::fs::create_dir_all(dir.join("plugins")).unwrap();
    std::fs::write(dir.join("plugins").join(file), "return { name = 'x' }\n").unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
}

/// MARE_HOME is process-wide: tests in this binary take turns.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn commit(dir: &Path, file: &str) -> String {
    std::fs::write(dir.join("plugins").join(file), "return {}\n").unwrap();
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git(dir, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", file]);
    let out = Command::new("git").current_dir(dir).args(["rev-parse", "HEAD"]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn source_change_moves_checkout() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    repo(a.path(), "from_a.lua");
    repo(b.path(), "from_b.lua");
    let cfg = |url: &Path| serde_json::json!({ "packages": [{ "name": "me/pkg", "url": url.to_string_lossy() }] });

    mare_plugin_manager::sync(&cfg(a.path()), &mut |_| ()).unwrap();
    let pkg_a = mare_plugin_manager::installed(&cfg(a.path())).remove(0);
    assert!(pkg_a.dir.join("plugins/from_a.lua").exists());

    // the config now points at B: update must check out B (its own <host>/<path> checkout) and lock B
    mare_plugin_manager::update(&cfg(b.path()), None, false, &mut |_| ()).unwrap();
    let pkg = mare_plugin_manager::installed(&cfg(b.path())).remove(0);
    assert_ne!(pkg.dir, pkg_a.dir, "checkouts are named by source");
    assert!(pkg.dir.join("plugins/from_b.lua").exists(), "still running the old repository's code");
    assert!(!pkg.dir.join("plugins/from_a.lua").exists());
    let lock = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap();
    let e = &lock.packages["me/pkg"];
    assert_eq!(e.url, b.path().to_string_lossy());
    let head = Command::new("git").current_dir(&pkg.dir).args(["rev-parse", "HEAD"]).output().unwrap();
    assert_eq!(e.commit, String::from_utf8_lossy(&head.stdout).trim());

    // and sync after a source change (no update) also follows the new source
    mare_plugin_manager::sync(&cfg(a.path()), &mut |_| ()).unwrap();
    assert!(mare_plugin_manager::installed(&cfg(a.path())).remove(0).dir.join("plugins/from_a.lua").exists());
    assert_eq!(
        mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/pkg"].url,
        a.path().to_string_lossy()
    );
}

#[test]
fn sync_keeps_the_locked_commit_and_only_upgrade_advances_except_exact_pins() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "v1.lua");
    let url = src.path().to_string_lossy().to_string();
    let cfg = |rev: Option<&str>| serde_json::json!({ "packages": [{ "name": "me/p", "url": url, "rev": rev }] });
    mare_plugin_manager::sync(&cfg(None), &mut |_| ()).unwrap();
    let first = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/p"].commit.clone();
    let second = commit(src.path(), "v2.lua");
    // sync restores the lock; it never advances
    mare_plugin_manager::sync(&cfg(None), &mut |_| ()).unwrap();
    assert_eq!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/p"].commit, first);
    // update advances
    mare_plugin_manager::update(&cfg(None), None, false, &mut |_| ()).unwrap();
    assert_eq!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/p"].commit, second);
    // rev = "=<commit>": sync checks it out (init.lua's rev wins, noted); update skips it
    let pin = format!("={first}");
    mare_plugin_manager::sync(&cfg(Some(&pin)), &mut |_| ()).unwrap();
    assert_eq!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/p"].commit, first);
    commit(src.path(), "v3.lua");
    mare_plugin_manager::update(&cfg(Some(&pin)), None, false, &mut |_| ()).unwrap();
    assert_eq!(
        mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/p"].commit,
        first,
        "exact pins never move"
    );
}

#[test]
fn a_corrupt_lock_stops_every_command_and_is_left_untouched() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let lock = mare_shared::paths::lock_file();
    std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
    std::fs::write(&lock, "{ not json").unwrap();
    let cfg = serde_json::json!({ "packages": [] });
    for (what, r) in [
        ("sync", mare_plugin_manager::sync(&cfg, &mut |_| ())),
        ("update", mare_plugin_manager::update(&cfg, None, false, &mut |_| ())),
        ("list", mare_plugin_manager::list(&cfg, &mut |_| ())),
        ("clean", mare_plugin_manager::remove_unused(&cfg, false, &mut |_| ())),
    ] {
        let e = r.expect_err(what).to_string();
        assert!(e.contains("corrupt"), "{what}: {e}");
    }
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), "{ not json", "never treated as empty or rewritten");
}

#[test]
fn local_changes_are_flagged_not_refused_and_restore_returns_to_the_lock() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let cfg = serde_json::json!({ "packages": [{ "name": "me/h", "url": src.path().to_string_lossy() }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let locked = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/h"].commit.clone();
    let dir = mare_plugin_manager::installed(&cfg).remove(0).dir;
    // a locally edited checkout still syncs and loads (flagged only)
    std::fs::write(dir.join("plugins/a.lua"), "return { mine = true }\n").unwrap();
    mare_plugin_manager::sync(&cfg, &mut |_| ()).expect("local changes are not refused");
    assert_eq!(std::fs::read_to_string(dir.join("plugins/a.lua")).unwrap(), "return { mine = true }\n");
    // restore leaves an edited checkout alone…
    mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap();
    assert_eq!(std::fs::read_to_string(dir.join("plugins/a.lua")).unwrap(), "return { mine = true }\n");
    git(&dir, &["checkout", "--", "."]);
    // …and otherwise moves a checkout back to the locked commit, without touching the lock
    let newer = commit(src.path(), "b.lua");
    git(&dir, &["fetch", "-q", "origin"]);
    git(&dir, &["checkout", "-q", "--detach", &newer]);
    let lock_before = std::fs::read_to_string(mare_shared::paths::lock_file()).unwrap();
    mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap();
    let head = Command::new("git").current_dir(&dir).args(["rev-parse", "HEAD"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), locked);
    assert_eq!(std::fs::read_to_string(mare_shared::paths::lock_file()).unwrap(), lock_before);
}

fn have_git() -> bool {
    Command::new("git").arg("--version").output().is_ok()
}

fn fresh_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("MARE_HOME", home.path()) };
    home
}

#[test]
fn each_package_succeeds_on_its_own_and_is_locked_as_soon_as_it_is_done() {
    if !have_git() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let good = tempfile::tempdir().unwrap();
    repo(good.path(), "g.lua");
    let gone = good.path().join("no-such-repo");
    let cfg = serde_json::json!({ "plugins": { "concurrency": 4 }, "packages": [
        { "name": "me/bad", "url": gone.to_string_lossy() },
        { "name": "me/good", "url": good.path().to_string_lossy() },
    ]});
    let mut log = vec![];
    let e = mare_plugin_manager::sync(&cfg, &mut |l| log.push(l)).unwrap_err().to_string();
    assert!(e.contains("1 package(s) failed") && e.contains("me/bad"), "{e}");
    // the good one is installed and locked although the bad one (declared first) failed
    let lock = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap();
    assert!(!lock.packages.contains_key("me/bad"));
    let entry = &lock.packages["me/good"];
    assert_eq!(entry.branch.as_deref(), Some("main"), "the lock records the branch");
    assert!(log.iter().any(|l| l == "● me/good") && log.iter().any(|l| l == "● me/bad"), "{log:?}");
    // a safe clone leaves no temporary directory behind
    let parent = mare_plugin_manager::checkout_dir(&good.path().to_string_lossy()).parent().unwrap().to_path_buf();
    let leftovers: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains(".cloning-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    // update: per package too, and it lists the new commits (update log)
    commit(good.path(), "two.lua");
    let mut log = vec![];
    let _ = mare_plugin_manager::update(&cfg, None, false, &mut |l| log.push(l));
    assert!(log.iter().any(|l| l.contains("two.lua")), "new commits listed: {log:?}");
    assert_ne!(
        mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/good"].commit,
        entry.commit
    );
}

#[test]
fn disabled_packages_keep_their_lock_entry_and_clean_drops_stale_ones_but_never_local_work() {
    if !have_git() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let url = src.path().to_string_lossy().to_string();
    let decl = |enabled: bool| serde_json::json!({ "packages": [{ "name": "me/p", "url": url, "enabled": enabled }] });
    mare_plugin_manager::sync(&decl(true), &mut |_| ()).unwrap();
    let locked = mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/p"].clone();
    // disabled on this machine: sync keeps the entry (dotfiles don't flip back and forth)
    mare_plugin_manager::sync(&decl(false), &mut |_| ()).unwrap();
    assert_eq!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages.get("me/p"), Some(&locked));
    let rows = mare_plugin_manager::status(&decl(false)).unwrap();
    assert!(rows.iter().any(|r| r.name == "me/p" && r.disabled), "disabled packages are listed");
    // HEAD is shown, not the lock's commit
    let dir = mare_plugin_manager::checkout_dir(&url);
    let newer = commit(src.path(), "b.lua");
    git(&dir, &["fetch", "-q", "origin"]);
    git(&dir, &["checkout", "-q", "--detach", &newer]);
    let row = mare_plugin_manager::status(&decl(true)).unwrap().into_iter().find(|r| r.name == "me/p").unwrap();
    assert_eq!(row.commit.as_deref(), Some(newer.as_str()));
    assert_eq!(row.locked.as_deref(), Some(locked.commit.as_str()));
    // clean: a disabled package's checkout goes, its lock entry stays
    mare_plugin_manager::remove_unused(&decl(false), false, &mut |_| ()).unwrap();
    assert!(!dir.exists());
    assert!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages.contains_key("me/p"));
    // undeclared: its stale lock entry is dropped; a checkout with local work is kept
    mare_plugin_manager::sync(&decl(true), &mut |_| ()).unwrap();
    std::fs::write(dir.join("mine.txt"), "untracked work").unwrap();
    let none = serde_json::json!({ "packages": [] });
    let mut log = vec![];
    mare_plugin_manager::remove_unused(&none, false, &mut |l| log.push(l)).unwrap();
    assert!(dir.join("mine.txt").exists(), "{log:?}");
    assert!(log.iter().any(|l| l.contains("local changes")), "{log:?}");
    assert!(!mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages.contains_key("me/p"));
    // the extras catalog checkout is never cleaned (the panel lists it)
    let catalog = catalog_cache_dir();
    std::fs::create_dir_all(&catalog).unwrap();
    git(&catalog, &["init", "-q"]);
    mare_plugin_manager::remove_unused(&none, false, &mut |_| ()).unwrap();
    assert!(catalog.exists());
}

#[test]
fn a_corrupt_extras_file_is_reported_at_startup() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let p = mare_plugin_manager::extras_path(&serde_json::json!({}));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(&p, "{ broken").unwrap();
    let problems = mare_plugin_manager::problems(&serde_json::json!({})).join("\n");
    assert!(problems.contains("plugins not loaded") && problems.contains("plugins.json"), "{problems}");
    // a newer format is refused, not rewritten
    std::fs::write(&p, r#"{"version":99,"extras":[]}"#).unwrap();
    assert!(mare_plugin_manager::read_extras(&serde_json::json!({})).unwrap_err().to_string().contains("newer mare"));
    assert!(mare_plugin_manager::set_extra("x", false, None, &serde_json::json!({})).is_err());
    assert_eq!(std::fs::read_to_string(&p).unwrap(), r#"{"version":99,"extras":[]}"#);
}

fn head_of(dir: &Path) -> String {
    let out = Command::new("git").current_dir(dir).args(["rev-parse", "HEAD"]).output().unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn restore_hash_mismatch_preserves_existing_checkout_and_origin() {
    if !have_git() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let cfg = serde_json::json!({ "packages": [{ "name": "me/restore", "url": src.path().to_string_lossy() }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::installed(&cfg).remove(0).dir;
    let second = commit(src.path(), "b.lua");
    git(&dir, &["fetch", "-q", "origin"]);
    git(&dir, &["checkout", "-q", "--detach", &second]);
    let old_origin = src.path().join("alternate").to_string_lossy().into_owned();
    git(&dir, &["remote", "set-url", "origin", &old_origin]);

    let lock_path = mare_shared::paths::lock_file();
    let mut lock: serde_json::Value = serde_json::from_slice(&std::fs::read(&lock_path).unwrap()).unwrap();
    lock["packages"]["me/restore"]["hash"] = "sha256:bad".into();
    let before = serde_json::to_vec_pretty(&lock).unwrap();
    std::fs::write(&lock_path, &before).unwrap();

    let err = mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap_err().to_string();
    assert!(err.contains("hash mismatch"), "{err}");
    assert_eq!(head_of(&dir), second, "failed restore must roll HEAD back");
    assert!(dir.join("plugins/b.lua").exists(), "failed restore must roll the worktree back");
    let origin = Command::new("git").current_dir(&dir).args(["remote", "get-url", "origin"]).output().unwrap();
    assert!(origin.status.success());
    assert_eq!(String::from_utf8_lossy(&origin.stdout).trim(), old_origin);
    assert_eq!(std::fs::read(&lock_path).unwrap(), before, "the lock file must remain byte-for-byte unchanged");
}

#[test]
fn restore_rejects_unknown_named_target_but_empty_restore_is_a_noop() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let cfg = serde_json::json!({ "packages": [] });
    mare_plugin_manager::restore(&cfg, &mut |_| ()).unwrap();
    let err = mare_plugin_manager::restore_only(&cfg, Some("absent"), &mut |_| ()).unwrap_err().to_string();
    assert!(err.contains("absent"), "{err}");
    assert!(!mare_shared::paths::lock_file().exists());
}

#[test]
fn diff_does_not_claim_to_be_at_a_missing_lock_entry() {
    if !have_git() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let cfg = serde_json::json!({ "packages": [{ "name": "me/unlocked", "url": src.path().to_string_lossy() }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    std::fs::remove_file(mare_shared::paths::lock_file()).unwrap();

    let mut log = vec![];
    mare_plugin_manager::diff_of(&cfg, "me/unlocked", &mut |line| log.push(line)).unwrap();
    let output = log.join("\n");
    assert!(output.contains("not in the lock"), "{output}");
    assert!(!output.contains("at the locked commit"), "{output}");
}

fn assert_update_refuses_dirty_checkout(staged: bool) {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let cfg = serde_json::json!({ "packages": [{ "name": "me/dirty", "url": src.path().to_string_lossy(), "branch": "main" }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::installed(&cfg).remove(0).dir;
    let initial = head_of(&dir);
    let lock_before = std::fs::read(mare_shared::paths::lock_file()).unwrap();
    commit(src.path(), "b.lua");
    let changed = if staged { "plugins/a.lua" } else { "notes.txt" };
    std::fs::write(dir.join(changed), "local work\n").unwrap();
    if staged {
        git(&dir, &["add", changed]);
    }
    let err = mare_plugin_manager::update(&cfg, None, false, &mut |_| ()).unwrap_err().to_string();
    assert!(err.contains("local") || err.contains("dirty"), "{err}");
    assert_eq!(head_of(&dir), initial);
    assert_eq!(std::fs::read_to_string(dir.join(changed)).unwrap(), "local work\n");
    assert_eq!(std::fs::read(mare_shared::paths::lock_file()).unwrap(), lock_before);
}

#[test]
fn update_refuses_staged_only_changes() {
    assert_update_refuses_dirty_checkout(true);
}

#[test]
fn update_refuses_untracked_only_changes() {
    assert_update_refuses_dirty_checkout(false);
}

#[test]
fn clean_keeps_checkout_with_ignored_files() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    std::fs::write(src.path().join(".gitignore"), "private-build/\n").unwrap();
    git(src.path(), &["add", ".gitignore"]);
    git(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "ignore local output"]);
    let cfg = serde_json::json!({ "packages": [{ "name": "me/ignored", "url": src.path().to_string_lossy() }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::installed(&cfg).remove(0).dir;
    std::fs::create_dir_all(dir.join("private-build")).unwrap();
    std::fs::write(dir.join("private-build/local"), "local artifact\n").unwrap();
    let mut log = vec![];
    mare_plugin_manager::remove_unused(&serde_json::json!({ "packages": [] }), false, &mut |line| log.push(line))
        .unwrap();
    assert!(dir.join("private-build/local").exists(), "{log:?}");
    assert!(log.iter().any(|line| line.contains("ignored")), "{log:?}");
}

#[cfg(unix)]
#[test]
fn status_fails_safe_when_git_index_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let cfg = serde_json::json!({ "packages": [{ "name": "me/index", "url": src.path().to_string_lossy() }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::installed(&cfg).remove(0).dir;
    let index = dir.join(".git/index");
    let mode = std::fs::metadata(&index).unwrap().permissions().mode();
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o000)).unwrap();
    let git_status = Command::new("git").current_dir(&dir).args(["status", "--porcelain"]).output().unwrap();
    let row = mare_plugin_manager::status(&cfg).unwrap().remove(0);
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(mode)).unwrap();
    assert!(!git_status.status.success(), "fixture requires git status to fail");
    assert!(row.dirty || row.problem.is_some(), "an unreadable index must not be reported clean");
}

fn catalog_manifest(root: &Path, name: &str, fields: &str) {
    let dir = root.join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mare-plugin.toml"), format!("name = {name:?}\n{fields}\n")).unwrap();
}

fn catalog_cache_dir() -> std::path::PathBuf {
    catalog_cache_dir_for("signalridge/mare-plugins")
}

fn catalog_cache_dir_for(source: &str) -> std::path::PathBuf {
    let root = mare_shared::paths::packages_dir();
    let checkout = mare_plugin_manager::checkout_dir(&mare_plugin_manager::source::expand(
        source,
        mare_plugin_manager::DEFAULT_URL_FORMAT,
    ));
    root.join(".catalogs").join(checkout.strip_prefix(&root).unwrap())
}

fn assert_invalid_dependency_graph_is_not_persisted(entries: &[(&str, &str)], expected: &str) {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let root = catalog_cache_dir();
    catalog_manifest(&root, "d", "");
    for (name, fields) in entries {
        catalog_manifest(&root, name, fields);
    }
    mare_plugin_manager::set_extra("d", true, None, &serde_json::json!({})).unwrap();
    let path = mare_plugin_manager::extras_path(&serde_json::json!({}));
    let before = std::fs::read(&path).unwrap();
    let err = mare_plugin_manager::set_extra("a", true, None, &serde_json::json!({})).unwrap_err().to_string();
    assert!(err.contains(expected), "{err}");
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let state = mare_plugin_manager::extras(&root).unwrap();
    assert!(state.iter().find(|item| item.name == "d").unwrap().enabled);
    assert!(!state.iter().find(|item| item.name == "a").unwrap().enabled);
}

#[test]
fn enabling_plugin_rejects_transitive_conflicts_before_writing() {
    assert_invalid_dependency_graph_is_not_persisted(
        &[("a", "plugins = [\"b\"]"), ("b", "plugins = [\"c\"]"), ("c", "conflicts = [\"d\"]")],
        "a -> b -> c conflicts with d",
    );
}

#[test]
fn enabling_plugin_rejects_conflicts_between_direct_dependencies_before_writing() {
    assert_invalid_dependency_graph_is_not_persisted(
        &[("a", "plugins = [\"b\", \"c\"]"), ("b", "conflicts = [\"c\"]"), ("c", "")],
        "conflicts with c",
    );
}

#[test]
fn enabling_plugin_rejects_unknown_transitive_dependency_before_writing() {
    assert_invalid_dependency_graph_is_not_persisted(
        &[("a", "plugins = [\"b\"]"), ("b", "plugins = [\"missing\"]")],
        "a -> b -> missing",
    );
}

#[test]
fn refreshing_catalog_does_not_move_an_installed_pinned_checkout() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    std::fs::remove_file(src.path().join("plugins/a.lua")).unwrap();
    catalog_manifest(src.path(), "catalogued", "version = \"1.0.0\"");
    git(src.path(), &["add", "."]);
    git(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "catalog one"]);
    let first = head_of(src.path());
    let mirror = format!("url.file://{}.insteadOf", src.path().display());
    unsafe {
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", &mirror);
        std::env::set_var("GIT_CONFIG_VALUE_0", "https://github.com/signalridge/mare-plugins.git");
    }
    let cfg = serde_json::json!({ "packages": [{ "name": "signalridge/mare-plugins", "commit": first }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::installed(&cfg).remove(0).dir;
    let before = std::fs::read(mare_shared::paths::lock_file()).unwrap();
    catalog_manifest(src.path(), "catalogued", "version = \"2.0.0\"");
    git(src.path(), &["add", "."]);
    git(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "catalog two"]);

    for _ in 0..2 {
        mare_plugin_manager::fetch_catalog_for(&cfg, &mut |_| ()).unwrap();
        assert_eq!(head_of(&dir), first, "catalog refresh must not move the installed checkout");
        assert_eq!(std::fs::read(mare_shared::paths::lock_file()).unwrap(), before);
        let installed = mare_plugin_manager::manifest(&dir.join("plugins/catalogued")).unwrap().unwrap();
        assert_eq!(installed.version.as_deref(), Some("1.0.0"));
        let catalog = mare_plugin_manager::catalog_for(&cfg).unwrap();
        assert_eq!(catalog[0].manifest.version.as_deref(), Some("2.0.0"));
    }
}

#[test]
fn legacy_extensions_manifest_is_rejected_with_its_path() {
    let dir = tempfile::tempdir().unwrap();
    let manifest_path = dir.path().join("mare-plugin.toml");
    std::fs::write(&manifest_path, "name = \"example\"\nextensions = [\"old\"]\n").unwrap();
    let err = format!("{:#}", mare_plugin_manager::manifest(dir.path()).unwrap_err());
    assert!(err.contains(&manifest_path.display().to_string()), "{err}");
    assert!(err.contains("extensions"), "{err}");
    assert!(mare_plugin_manager::resource_dirs(&serde_json::json!({}), Some("extensions")).is_err());
}

#[test]
fn conflicting_pins_for_the_same_checkout_are_rejected_before_install() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let first = head_of(src.path());
    let second = commit(src.path(), "b.lua");
    let url = src.path().to_string_lossy().to_string();
    let cfg = serde_json::json!({ "packages": [
        { "name": "me/mono:a", "url": url, "subdir": "a", "commit": first },
        { "name": "me/mono:b", "url": url, "subdir": "b", "commit": second },
    ] });
    let err = mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap_err().to_string();
    assert!(err.contains("me/mono:a") && err.contains("me/mono:b") && err.contains("conflict"), "{err}");
    assert!(!mare_plugin_manager::checkout_dir(&url).exists());
    assert!(!mare_shared::paths::lock_file().exists());
}

#[test]
fn a_second_project_cannot_move_a_checkout_locked_by_the_first() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let workspace = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let first = head_of(src.path());
    let second = commit(src.path(), "b.lua");
    let url = src.path().to_string_lossy().to_string();
    let mut configs = Vec::new();
    for (name, commit) in [("a", &first), ("b", &second)] {
        let cwd = workspace.path().join(name);
        std::fs::create_dir_all(cwd.join(".mare")).unwrap();
        std::fs::write(cwd.join(".mare/init.lua"), "").unwrap();
        let mut cfg = serde_json::json!({ "packages": [{ "name": "me/shared", "url": url, "commit": commit }] });
        mare_plugin_manager::set_project_scope(&mut cfg, &cwd, true);
        configs.push(cfg);
    }
    mare_plugin_manager::sync(&configs[0], &mut |_| ()).unwrap();
    let lock_a = std::fs::read(mare_plugin_manager::lock_path(&configs[0])).unwrap();
    assert!(mare_plugin_manager::needs_install(&configs[1]), "project B has no lock for this checkout");
    assert!(mare_plugin_manager::installed(&configs[1]).is_empty(), "project B must not load project A's commit");
    let notices = mare_plugin_manager::install_missing(&configs[1], &mut |_| ());
    assert!(notices.iter().any(|notice| notice.contains("conflict")), "{notices:?}");
    let err = mare_plugin_manager::sync(&configs[1], &mut |_| ()).unwrap_err().to_string();
    assert!(err.contains("conflict") && err.contains("me/shared"), "{err}");
    assert_eq!(head_of(&mare_plugin_manager::checkout_dir(&url)), first);
    assert_eq!(std::fs::read(mare_plugin_manager::lock_path(&configs[0])).unwrap(), lock_a);
}

#[test]
fn project_plugin_selections_do_not_overwrite_user_or_other_projects() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let workspace = tempfile::tempdir().unwrap();
    let root = catalog_cache_dir();
    for name in ["common", "a", "b"] {
        catalog_manifest(&root, name, "");
    }
    mare_plugin_manager::set_extra("common", true, None, &serde_json::json!({})).unwrap();
    let user_file = mare_plugin_manager::extras_path(&serde_json::json!({}));
    let user_before = std::fs::read(&user_file).unwrap();
    let mut projects = Vec::new();
    for name in ["a", "b"] {
        let cwd = workspace.path().join(name);
        std::fs::create_dir_all(cwd.join(".mare")).unwrap();
        std::fs::write(cwd.join(".mare/init.lua"), "").unwrap();
        let mut cfg = serde_json::json!({});
        mare_plugin_manager::set_project_scope(&mut cfg, &cwd, true);
        projects.push((cwd, cfg));
    }
    mare_plugin_manager::set_extra("a", true, None, &projects[0].1).unwrap();
    let a_path = projects[0].0.join(".mare/plugins.json");
    let a_before = std::fs::read(&a_path).ok();
    mare_plugin_manager::set_extra("b", true, None, &projects[1].1).unwrap();
    assert_eq!(std::fs::read(&user_file).unwrap(), user_before, "project choices must not change user choices");
    let a_after = std::fs::read(&a_path).expect("project A has its own plugin selection");
    assert_eq!(a_before.as_deref(), Some(a_after.as_slice()));
    assert!(projects[1].0.join(".mare/plugins.json").exists());
    for (i, own, other) in [(0, "a", "b"), (1, "b", "a")] {
        let list = mare_plugin_manager::extras_for(&projects[i].0, &projects[i].1).unwrap();
        let enabled = |name: &str| list.iter().find(|entry| entry.name == name).unwrap().enabled;
        assert!(enabled("common") && enabled(own) && !enabled(other), "{list:?}");
        let json = serde_json::to_value(&list).unwrap();
        let scope = |name: &str| {
            json.as_array().unwrap().iter().find(|entry| entry["name"] == name).unwrap()["scope"].as_str().unwrap_or("")
        };
        assert_eq!(scope("common"), "user");
        assert_eq!(scope(own), "project");
    }
}

#[test]
fn plugin_manifest_exposes_static_metadata_and_default_news() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("mare-plugin.toml"),
        r#"name = "metadata"
platforms = ["linux", "windows"]
keywords = ["planning", "tools"]
license = "MIT"
homepage = "https://example.invalid/plugin"
[provides]
tools = ["search"]
commands = ["plan"]
hooks = ["before"]
skills = ["review"]
prompts = ["plan"]
themes = ["dark"]
services = ["index"]
flags = ["feature"]
mcp = ["server"]
"#,
    )
    .unwrap();
    let manifest = serde_json::to_value(mare_plugin_manager::manifest(dir.path()).unwrap().unwrap()).unwrap();
    assert_eq!(manifest["news"], "NEWS.md");
    assert_eq!(manifest["platforms"], serde_json::json!(["linux", "windows"]));
    assert_eq!(manifest["keywords"], serde_json::json!(["planning", "tools"]));
    assert_eq!(manifest["license"], "MIT");
    assert_eq!(manifest["homepage"], "https://example.invalid/plugin");
    for field in ["tools", "commands", "hooks", "skills", "prompts", "themes", "services", "flags", "mcp"] {
        assert_eq!(manifest["provides"][field].as_array().unwrap().len(), 1, "{manifest}");
    }
    std::fs::write(dir.path().join("mare-plugin.toml"), "platforms = [\"invalid\"]\n").unwrap();
    assert!(mare_plugin_manager::manifest(dir.path()).is_err(), "unknown platforms must be diagnosed");
}

#[test]
fn malformed_catalog_manifest_is_reported_without_hiding_valid_plugins() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let root = catalog_cache_dir();
    catalog_manifest(&root, "valid", "");
    catalog_manifest(&root, "broken", "requires = [");
    let cfg = serde_json::json!({});
    let catalog = mare_plugin_manager::catalog_for(&cfg).unwrap();
    assert!(catalog.iter().any(|entry| entry.name == "valid"));
    let issues = mare_plugin_manager::problems(&cfg).join("\n");
    assert!(
        issues.contains(&root.join("plugins").join("broken").join("mare-plugin.toml").display().to_string()),
        "{issues}"
    );
    assert!(issues.contains("broken") && issues.contains("TOML"), "{issues}");
}

#[test]
fn catalog_selection_keeps_its_source_when_sources_change_order_or_disappear() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let first = "team/first";
    let second = "team/second";
    catalog_manifest(&catalog_cache_dir_for(first), "foo", "description = 'first'");
    catalog_manifest(&catalog_cache_dir_for(second), "foo", "description = 'second'");
    let initial = serde_json::json!({ "plugins": { "sources": [first, second] } });
    mare_plugin_manager::set_extra("foo", true, Some(second), &initial).unwrap();
    let saved: serde_json::Value =
        serde_json::from_slice(&std::fs::read(mare_plugin_manager::extras_path(&initial)).unwrap()).unwrap();
    assert_eq!(saved["version"], 2, "{saved}");
    assert_eq!(saved["plugins"], serde_json::json!([{ "source": second, "name": "foo" }]));

    let reordered = serde_json::json!({ "plugins": { "sources": [second, first] } });
    let specs = mare_plugin_manager::specs(&reordered);
    assert!(specs.iter().any(|spec| spec.name == second), "{specs:?}");
    assert!(!specs.iter().any(|spec| spec.name == first), "{specs:?}");
    let removed = serde_json::json!({ "plugins": { "sources": [first] } });
    let problems = mare_plugin_manager::problems(&removed).join("\n");
    assert!(problems.contains(second) && problems.contains("foo"), "{problems}");
    assert!(!mare_plugin_manager::specs(&removed).iter().any(|spec| spec.name == first));
}

#[test]
fn installing_conflicting_catalog_plugins_does_not_partially_persist() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let root = catalog_cache_dir();
    catalog_manifest(&root, "a", "conflicts = [\"b\"]");
    catalog_manifest(&root, "b", "");
    let cfg = serde_json::json!({});
    let err =
        mare_plugin_manager::install_plugins(&cfg, &["a".into(), "b".into()], &mut |_| ()).unwrap_err().to_string();
    assert!(err.contains("conflicts") && err.contains("a") && err.contains("b"), "{err}");
    assert!(!mare_plugin_manager::extras_path(&cfg).exists(), "a failed batch must not persist the first selection");
    assert!(mare_plugin_manager::read_extras(&cfg).unwrap().plugins.is_empty());
}

#[test]
fn doctor_reports_checkout_drift_dirty_orphans_and_missing_recommended_commands() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "a.lua");
    let cfg = serde_json::json!({ "packages": [{ "name": "me/health", "url": src.path().to_string_lossy() }] });
    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::checkout_dir(&src.path().to_string_lossy());
    let newer = commit(src.path(), "b.lua");
    git(&dir, &["fetch", "-q", "origin"]);
    git(&dir, &["checkout", "-q", "--detach", &newer]);
    git(&dir, &["remote", "set-url", "origin", "/different/source"]);
    std::fs::write(dir.join("notes.txt"), "local work\n").unwrap();
    let orphan = mare_plugin_manager::checkout_dir("https://example.invalid/unused/repo.git");
    std::fs::create_dir_all(&orphan).unwrap();
    git(&orphan, &["init", "-q"]);
    let root = catalog_cache_dir();
    catalog_manifest(&root, "needs-cmd", "[recommended]\ncmd = [\"mare-command-that-cannot-exist-482913\"]");
    mare_plugin_manager::set_extra("needs-cmd", true, None, &cfg).unwrap();
    let checks = mare_plugin_manager::doctor_for(&cfg);
    let issues = checks
        .iter()
        .filter(|(_, ok, _)| !ok)
        .map(|(name, _, detail)| format!("{name}: {detail}"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(issues.contains("HEAD") && issues.contains("me/health"), "{issues}");
    assert!(issues.contains("origin") && issues.contains("me/health"), "{issues}");
    assert!(issues.contains("local changes") || issues.contains("dirty"), "{issues}");
    assert!(issues.contains("orphan") && issues.contains(&orphan.display().to_string()), "{issues}");
    assert!(issues.contains("mare-command-that-cannot-exist-482913"), "{issues}");
}

#[test]
fn plugin_metadata_renames_migrate_existing_selections() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let root = catalog_cache_dir();
    catalog_manifest(&root, "new", "");
    std::fs::write(root.join("plugins.toml"), "[renamed]\nold = 'new'\n").unwrap();
    let path = mare_plugin_manager::extras_path(&serde_json::json!({}));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"version":1,"plugins":["old"]}"#).unwrap();
    let notices = mare_plugin_manager::migrate_extras();
    assert!(notices.iter().any(|notice| notice.contains("old is now new")), "{notices:?}");
    let selections = mare_plugin_manager::read_extras(&serde_json::json!({})).unwrap().plugins;
    assert_eq!(
        selections,
        [mare_plugin_manager::PluginSelection {
            source: mare_plugin_manager::DEFAULT_SOURCE.into(),
            name: "new".into()
        }]
    );
}

#[test]
fn checkout_reference_artifact_is_registered() {
    let artifact = mare_plugin_manager::ARTIFACTS.iter().find(|artifact| artifact.id.as_str() == "pkg.checkout_refs");
    let artifact = artifact.expect("checkout reference files need their own persistence registration");
    assert!(artifact.matches_rel(mare_shared::artifact::Root::Data, "packages/.locks/example.refs/reference"));
    assert_eq!(artifact.version, Some(1));
}

#[test]
fn doctor_reports_conflicting_saved_plugin_selections() {
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();
    let root = catalog_cache_dir();
    catalog_manifest(&root, "a", "conflicts = [\"b\"]");
    catalog_manifest(&root, "b", "");
    let cfg = serde_json::json!({});
    let path = mare_plugin_manager::extras_path(&cfg);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({ "version": 2, "plugins": [
            { "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "a" },
            { "source": mare_plugin_manager::DEFAULT_SOURCE, "name": "b" }
        ] }))
        .unwrap(),
    )
    .unwrap();
    let checks = mare_plugin_manager::doctor_for(&cfg);
    assert!(
        checks
            .iter()
            .any(|(_, ok, detail)| !ok && detail.contains("conflict") && detail.contains("a") && detail.contains("b")),
        "{checks:?}"
    );
}

#[test]
fn update_rolls_back_on_broken_submodule() {
    if !have_git() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();

    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "v1.lua");
    let v1 = head_of(src.path());
    let url = src.path().to_string_lossy().to_string();
    let cfg = serde_json::json!({ "packages": [{ "name": "me/sub", "url": url, "branch": "main" }] });

    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::checkout_dir(&url);
    assert!(dir.join("plugins/v1.lua").exists());

    // v2 adds a new plugin file and a submodule pointing to a non-existent repo.
    std::fs::write(src.path().join("plugins/v2.lua"), "return {}\n").unwrap();
    std::fs::write(
        src.path().join(".gitmodules"),
        "[submodule \"missing\"]\n\tpath = missing\n\turl = /no/such/submodule/repo\n",
    )
    .unwrap();
    git(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git(
        src.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{v1},missing"),
        ],
    );
    git(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "v2 broken submodule"]);

    let mut log = vec![];
    assert!(mare_plugin_manager::update(&cfg, None, false, &mut |l| log.push(l)).is_err());
    assert!(log.iter().any(|l| l.contains("rolled back")), "{log:?}");
    assert_eq!(head_of(&dir), v1, "checkout must roll back to v1");
    assert!(dir.join("plugins/v1.lua").exists());
    assert!(!dir.join("plugins/v2.lua").exists());
    assert_eq!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/sub"].commit, v1);
}

#[test]
fn update_rolls_back_on_incompatible_manifest_and_fresh_failure() {
    if !have_git() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();

    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "v1.lua");
    let v1 = head_of(src.path());
    let url = src.path().to_string_lossy().to_string();
    let cfg = serde_json::json!({ "packages": [{ "name": "me/compat", "url": url, "branch": "main" }] });

    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::checkout_dir(&url);

    // v2 introduces an incompatible mare-plugin.toml requirement.
    std::fs::write(src.path().join("plugins/v2.lua"), "return {}\n").unwrap();
    std::fs::write(src.path().join("mare-plugin.toml"), "requires = \">=99.0\"\n").unwrap();
    git(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git(src.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "v2 incompatible"]);

    let mut log = vec![];
    let err = mare_plugin_manager::update(&cfg, None, false, &mut |l| log.push(l)).unwrap_err().to_string();
    assert!(err.contains(">=99.0"), "{err}");
    assert!(log.iter().any(|l| l.contains("rolled back")), "{log:?}");
    assert_eq!(head_of(&dir), v1);
    assert!(dir.join("plugins/v1.lua").exists());
    assert!(!dir.join("plugins/v2.lua").exists());
    assert_eq!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/compat"].commit, v1);
    assert_eq!(mare_plugin_manager::installed(&cfg).len(), 1, "original working plugin remains installed");

    // Fresh update on a brand-new incompatible package must remove the partial checkout.
    let bad = tempfile::tempdir().unwrap();
    repo(bad.path(), "bad.lua");
    std::fs::write(bad.path().join("mare-plugin.toml"), "requires = \">=99.0\"\n").unwrap();
    git(bad.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "add", "."]);
    git(bad.path(), &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "bad"]);
    let bad_url = bad.path().to_string_lossy().to_string();
    let bad_cfg = serde_json::json!({ "packages": [{ "name": "me/bad", "url": bad_url }] });
    assert!(mare_plugin_manager::update(&bad_cfg, None, false, &mut |_| ()).is_err());
    assert!(!mare_plugin_manager::checkout_dir(&bad_url).exists(), "partial fresh checkout must be removed");
    assert!(!mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages.contains_key("me/bad"));
}

#[test]
fn update_failure_on_fetch_preserves_existing_checkout_and_lock() {
    if !have_git() {
        return;
    }
    let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _home = fresh_home();

    let src = tempfile::tempdir().unwrap();
    repo(src.path(), "v1.lua");
    let v1 = head_of(src.path());
    let url = src.path().to_string_lossy().to_string();
    let cfg = serde_json::json!({ "packages": [{ "name": "me/gone", "url": url, "branch": "main" }] });

    mare_plugin_manager::sync(&cfg, &mut |_| ()).unwrap();
    let dir = mare_plugin_manager::checkout_dir(&url);

    // Remove the upstream directory so `git fetch` fails.
    drop(src);
    assert!(mare_plugin_manager::update(&cfg, None, false, &mut |_| ()).is_err());
    assert_eq!(head_of(&dir), v1);
    assert!(dir.join("plugins/v1.lua").exists());
    assert_eq!(mare_plugin_manager::read_lock(&serde_json::json!({})).unwrap().packages["me/gone"].commit, v1);
}
