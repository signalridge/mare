//! Update: move packages to the newest allowed revision, with cross-process locking and
//! automatic rollback on failure.

use super::runner::{failures, run_each};
use crate::effective::health::blocked;
use crate::effective::specs::validate_checkout_constraints;
use crate::lock::{SharedLock, checkout_lock, register_checkout_reference};
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
