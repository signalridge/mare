//! Update: move packages to the newest allowed revision, with cross-process locking and
//! automatic rollback on failure.

use super::runner::{failures, run_each};
use crate::effective::health::blocked;
use crate::effective::specs::validate_checkout_constraints;
use crate::lock::{
    SharedLock, acquire_file_lock, checkout_lock, lock_path, register_checkout_reference, sibling_lock_path,
    spec_lock_path, write_json_atomic,
};
use crate::repo::checkout::{
    PreCheckout, branch_of, checkout, ensure_clone, ensure_other_scopes_allow, new_commits, remote_error, resolve,
    short, stop_if_cancelled, worktree_status,
};
use crate::repo::hash::lock_hash;
use crate::*;

/// Fetch and move every (or the named) package to its newest allowed revision; the new commits
/// are listed (update log). Any failure rolls the package's checkout back to its pre-update
/// state without updating `mare-lock.json`.
pub fn update(cfg: &serde_json::Value, only: Option<&str>, stash: bool, log: Log) -> Result<()> {
    if mare_shared::env::is_offline_env() {
        bail!("offline mode is active (MARE_OFFLINE); cannot update plugins");
    }
    let o = PkgOptions::from_config(cfg);
    let specs = specs(cfg);
    validate_checkout_constraints(&specs)?;
    let lock = SharedLock::new(cfg)?;
    let user_lock = SharedLock::new(&serde_json::json!({}))?;
    let todo: Vec<PackageSpec> =
        specs.into_iter().filter(|s| only.is_none_or(|n| n == s.name) && s.dir.is_none()).collect();
    if let Some(n) = only
        && todo.is_empty()
    {
        bail!("no installed package named {n} (local directories aren't updated)");
    }
    let results = run_each(&o, &todo, log, |s, log| {
        update_one(&o, s, if s.project_lock { &lock } else { &user_lock }, stash, log)
    });
    if todo.iter().any(|s| s.project_lock) {
        lock.ensure_exists()?;
    }
    if todo.iter().any(|s| !s.project_lock) {
        user_lock.ensure_exists()?;
    }
    failures(results)
}

fn update_one(o: &PkgOptions, s: &PackageSpec, lock: &SharedLock, stash: bool, log: Log) -> Result<()> {
    stop_if_cancelled()?;
    let target_dir = checkout_dir(&s.url);
    let _pkg_lock = checkout_lock(&target_dir)?;
    let locked = lock.refresh_get(&s.name)?;
    let lock_url = source::strip_url_credentials(&s.url);
    if s.exact_pin() && locked.as_ref().is_some_and(|e| e.url == lock_url) {
        log("  ⎿ pinned (commit / rev = \"=…\" / pin = true): skipped".into());
        return Ok(());
    }
    let pre = PreCheckout::capture(o, target_dir);
    let res = (|| -> Result<()> {
        let (dir, _changed) = ensure_clone(o, s, log)?;
        let state = worktree_status(o, &dir);
        if let Some(err) = &state.error {
            bail!("cannot inspect local changes in {}: {err}", dir.display());
        }
        if state.has_changes() && !stash {
            bail!("{} has local changes or untracked files; update refused (use --stash)", dir.display());
        }
        o.git(Some(&dir), &["fetch", "--quiet", "--tags", "--", "origin"]).map_err(|e| remote_error(e, &s.url))?;
        let target = resolve(o, &dir, &s.target)?;
        ensure_other_scopes_allow(o, s, &dir, lock.path(), &target)?;
        if state.has_changes() {
            o.git(Some(&dir), &["stash", "push", "--include-untracked", "--message", "mare plugin update"])
                .context("stashing local changes before update")?;
            let after = worktree_status(o, &dir);
            if after.has_changes() {
                bail!("{} still has local changes after stashing; update refused", dir.display());
            }
            log("  ⎿ local changes saved in git stash".into());
        }
        let old = locked.as_ref().filter(|e| e.url == lock_url).map(|e| e.commit.clone());
        let commit = match checkout(o, &dir, &target, s.submodules) {
            Ok(c) => c,
            Err(e) => {
                stop_if_cancelled()?;
                bail!("{e:#} (resolve the local changes, then update again)");
            }
        };
        if let Some(sub) = &s.subdir {
            let root = s.root();
            if !root.exists() {
                bail!("package subdirectory {sub} missing in {}", short(&commit));
            }
            let Ok(canon_checkout) = dunce::canonicalize(&dir) else {
                bail!("cannot resolve checkout directory for {}", s.name);
            };
            let Ok(canon_root) = dunce::canonicalize(&root) else {
                bail!("package subdirectory {sub} cannot be resolved in {}", short(&commit));
            };
            if !canon_root.starts_with(&canon_checkout) {
                bail!("package subdirectory {sub} escapes checkout in {}", short(&commit));
            }
        }
        if let Some(why) = blocked(&s.root()) {
            bail!("{why}");
        }
        let branch = branch_of(o, &dir, &s.target);
        let hash = lock_hash(o, &dir, &commit, locked.as_ref().filter(|e| e.url == lock_url))?;
        register_checkout_reference(&dir, lock.path())?;
        lock.commit(
            &s.name,
            LockEntry { url: lock_url, commit: commit.clone(), rev: s.target.descriptor(), branch, hash },
        )?;
        match &old {
            Some(o2) if *o2 == commit => log("  ⎿ up to date".into()),
            Some(o2) => {
                log(format!("  ⎿ {} → {}", short(o2), short(&commit)));
                for c in new_commits(o, &dir, o2, &commit) {
                    log(format!("    {c}"));
                }
            }
            None => log(format!("  ⎿ {}", short(&commit))),
        }
        Ok(())
    })();
    if let Err(err) = res {
        if let Err(rb_err) = pre.rollback(o, s.submodules, log) {
            return Err(anyhow::anyhow!("{err:#}; rollback failed: {rb_err:#}"));
        }
        return Err(err);
    }
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct PendingPluginUpdates {
    packages: Vec<String>,
}

fn canonicalize_lockfile_key(lockfile: &Path) -> PathBuf {
    if let Ok(canon) = dunce::canonicalize(lockfile) {
        return canon;
    }
    if let (Some(parent), Some(name)) = (lockfile.parent(), lockfile.file_name())
        && let Ok(canon_parent) = dunce::canonicalize(parent)
    {
        return canon_parent.join(name);
    }
    lockfile.to_path_buf()
}

fn pending_updates_path_for(lockfile: &Path) -> PathBuf {
    use sha2::{Digest, Sha256};
    let key = canonicalize_lockfile_key(lockfile);
    let hex: String = Sha256::digest(key.to_string_lossy().as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
    mare_shared::paths::state_dir().join("pending-plugin-updates").join(format!("{hex}.json"))
}

fn read_pending_updates(path: &Path) -> PendingPluginUpdates {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(mare_shared::strip_utf8_bom(&s)).ok())
        .unwrap_or_default()
}

/// Record declared packages that requested background `auto_update = true` partitioned by their
/// owning lockfile scope so checkouts switch safely before plugins load on `/reload` or next
/// startup (never under a live session, and never clobbering another project's queued entries).
pub fn queue_pending_updates(cfg: &serde_json::Value, names: &[String]) -> Result<usize> {
    if names.is_empty() {
        return Ok(0);
    }
    let all = specs(cfg);
    let default_lock = lock_path(cfg);
    let mut by_scope: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
    for name in names {
        let target_lock = all
            .iter()
            .find(|s| s.name == *name && s.dir.is_none())
            .map(|s| spec_lock_path(cfg, s))
            .unwrap_or_else(|| default_lock.clone());
        let list = by_scope.entry(canonicalize_lockfile_key(&target_lock)).or_default();
        if !list.contains(name) {
            list.push(name.clone());
        }
    }
    let mut total = 0usize;
    for (scope_lock, scope_names) in by_scope {
        let path = pending_updates_path_for(&scope_lock);
        let _guard = acquire_file_lock(&sibling_lock_path(&path))?;
        let mut current = read_pending_updates(&path);
        for name in scope_names {
            if !current.packages.contains(&name) {
                current.packages.push(name);
            }
        }
        write_json_atomic(&path, &current)?;
        total += current.packages.len();
    }
    Ok(total)
}

/// Apply queued background `auto_update = true` package updates for `cfg`'s active lockfile scopes
/// before plugins load on startup or `/reload`. Respects `pin`, commit locks, and `MARE_OFFLINE`.
/// Unmatched entries remain queued for their owning scope. Returns any non-fatal notices.
pub fn apply_pending_updates_with_progress(cfg: &serde_json::Value, log: Log) -> Vec<String> {
    if mare_shared::env::is_offline_env() {
        return Vec::new();
    }
    let all = specs(cfg);
    let mut scopes = vec![canonicalize_lockfile_key(&mare_shared::paths::lock_file())];
    let project_scope = canonicalize_lockfile_key(&lock_path(cfg));
    if !scopes.contains(&project_scope) {
        scopes.push(project_scope);
    }

    let mut claimed_names = Vec::new();
    for scope_lock in scopes {
        let path = pending_updates_path_for(&scope_lock);
        if !path.is_file() {
            continue;
        }
        let Ok(_guard) = acquire_file_lock(&sibling_lock_path(&path)) else {
            continue;
        };
        let pending = read_pending_updates(&path);
        if pending.packages.is_empty() {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let mut remaining = Vec::new();
        for name in pending.packages {
            let eligible = all.iter().any(|s| {
                s.name == name
                    && s.dir.is_none()
                    && s.auto_update
                    && canonicalize_lockfile_key(&spec_lock_path(cfg, s)) == scope_lock
            });
            if eligible {
                if !claimed_names.contains(&name) {
                    claimed_names.push(name);
                }
            } else {
                remaining.push(name);
            }
        }
        if remaining.is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            let _ = write_json_atomic(&path, &PendingPluginUpdates { packages: remaining });
        }
    }

    let mut notices = Vec::new();
    for name in claimed_names {
        if let Err(err) = update(cfg, Some(&name), false, log) {
            notices.push(format!("auto-update for package {name} failed: {err:#}"));
        }
    }
    notices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_updates_are_scoped_per_lockfile_and_preserved_across_projects() {
        let _g = crate::tests::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let tmp = tempfile::tempdir().unwrap();
        let proj_a = tmp.path().join("proj-a").join(".mare");
        let proj_b = tmp.path().join("proj-b").join(".mare");
        std::fs::create_dir_all(&proj_a).unwrap();
        std::fs::create_dir_all(&proj_b).unwrap();

        let cfg_a = serde_json::json!({
            crate::lock::PROJECT_SCOPE_KEY: proj_a.to_string_lossy(),
            "packages": [
                { "name": "a/pkg-a", "url": "https://example.com/a/pkg-a.git", "auto_update": true, "lock_scope": "project" }
            ]
        });
        let cfg_b = serde_json::json!({
            crate::lock::PROJECT_SCOPE_KEY: proj_b.to_string_lossy(),
            "packages": [
                { "name": "b/pkg-b", "url": "https://example.com/b/pkg-b.git", "auto_update": true, "lock_scope": "project" }
            ]
        });

        let queued = queue_pending_updates(&cfg_a, &["a/pkg-a".into()]).unwrap();
        assert_eq!(queued, 1);

        // Prepend UTF-8 BOM to verify BOM-safe reading (matching #163).
        let path_a = pending_updates_path_for(&proj_a.join("mare-lock.json"));
        let orig = std::fs::read(&path_a).unwrap();
        let mut with_bom = vec![0xEF, 0xBB, 0xBF];
        with_bom.extend_from_slice(&orig);
        std::fs::write(&path_a, with_bom).unwrap();

        // Starting Project B must NOT touch or delete Project A's queued package.
        let notices_b = apply_pending_updates_with_progress(&cfg_b, &mut |_| {});
        assert!(notices_b.is_empty());
        assert!(path_a.is_file(), "Project A's queue must survive Project B startup");
        assert_eq!(read_pending_updates(&path_a).packages, vec!["a/pkg-a".to_string()]);

        // User-scope queue with an unclaimed package is also preserved when a config doesn't enable auto_update for it.
        let user_cfg_disabled = serde_json::json!({
            "packages": [
                { "name": "u/pkg-u", "url": "https://example.com/u/pkg-u.git", "auto_update": false, "lock_scope": "user" }
            ]
        });
        queue_pending_updates(&user_cfg_disabled, &["u/pkg-u".into()]).unwrap();
        let user_path = pending_updates_path_for(&mare_shared::paths::lock_file());
        let _ = apply_pending_updates_with_progress(&user_cfg_disabled, &mut |_| {});
        assert_eq!(read_pending_updates(&user_path).packages, vec!["u/pkg-u".to_string()]);
    }

    #[test]
    fn concurrent_queue_pending_updates_does_not_drop_entries() {
        let _g = crate::tests::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let cfg = serde_json::json!({});
        let n = 12usize;
        std::thread::scope(|s| {
            for i in 0..n {
                let cfg_ref = &cfg;
                s.spawn(move || {
                    queue_pending_updates(cfg_ref, &[format!("me/pkg-{i}")]).unwrap();
                });
            }
        });
        let path = pending_updates_path_for(&mare_shared::paths::lock_file());
        let stored = read_pending_updates(&path);
        assert_eq!(stored.packages.len(), n, "lost concurrent updates: {:?}", stored.packages);
        for i in 0..n {
            assert!(stored.packages.contains(&format!("me/pkg-{i}")));
        }
    }
}
