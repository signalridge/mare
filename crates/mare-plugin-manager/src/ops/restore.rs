//! Restore: put checkouts back on the lock.

use super::runner::{failures, run_each};
use crate::effective::specs::validate_checkout_constraints;
use crate::lock::{checkout_lock, lockfile_guard, register_checkout_reference, write_lock};
use crate::repo::checkout::{
    PreCheckout, checkout, ensure_clone, ensure_other_scopes_allow, fetch, flag_local_changes, short,
    stop_if_cancelled, worktree_status,
};
use crate::repo::hash::{HashCheck, verify_hash};
use crate::*;

/// Put every checkout back on the commit recorded in the lock (lazy.nvim `restore`); the lock
/// itself is never changed. A checkout with local edits is left alone and reported.
pub fn restore(cfg: &serde_json::Value, log: Log) -> Result<()> {
    restore_only(cfg, None, log)
}

/// Rebuild a missing or corrupt lock from clean, already installed checkouts.
pub fn repair_lock(cfg: &serde_json::Value) -> Result<()> {
    let mut specs = specs(cfg);
    specs.extend(disabled_specs(cfg));
    validate_checkout_constraints(&specs)?;
    let o = PkgOptions::from_config(cfg);
    let mut paths: BTreeSet<PathBuf> =
        specs.iter().filter(|spec| spec.dir.is_none()).map(|spec| crate::lock::spec_lock_path(cfg, spec)).collect();
    paths.insert(lock_path(cfg));
    let user_path = mare_shared::paths::lock_file();
    if user_path.exists() {
        paths.insert(user_path);
    }
    let mut dirs: Vec<PathBuf> =
        specs.iter().filter(|spec| spec.dir.is_none()).map(|spec| checkout_dir(&spec.url)).collect();
    dirs.sort();
    dirs.dedup();
    let mut guards = Vec::new();
    for dir in &dirs {
        guards.push(checkout_lock(dir)?);
    }
    for path in &paths {
        guards.push(lockfile_guard(path)?);
    }
    let mut rebuilt_locks = BTreeMap::new();
    for path in paths {
        match std::fs::read(&path) {
            Ok(bytes) => {
                if let Ok(value) =
                    serde_json::from_slice::<serde_json::Value>(mare_shared::strip_utf8_bom_bytes(&bytes))
                    && let Some(version) = value.get("version").and_then(serde_json::Value::as_u64)
                    && version != u64::from(crate::lock::LOCK_VERSION)
                {
                    bail!("lock file {} has unsupported version {version}; nothing was changed", path.display());
                }
                if crate::lock::read_lock_at(&path).is_ok() {
                    continue;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        }
        let mut rebuilt = LockFile::default();
        for spec in specs.iter().filter(|spec| spec.dir.is_none() && crate::lock::spec_lock_path(cfg, spec) == path) {
            let dir = checkout_dir(&spec.url);
            if mare_shared::git::git_dir(&dir).is_none() {
                bail!("cannot repair {}: checkout {} is missing", spec.name, dir.display());
            }
            let state = worktree_status(&o, &dir);
            if !state.is_clean() {
                bail!("cannot repair {}: checkout {} is not clean or cannot be inspected", spec.name, dir.display());
            }
            let commit = crate::repo::checkout::head(&o, &dir).context("reading checkout HEAD")?;
            let origin = source::strip_url_credentials(
                &crate::repo::checkout::configured_origin(&o, &dir).context("reading checkout origin")?,
            );
            let branch = crate::repo::checkout::branch_of(&o, &dir, &spec.target);
            let hash = Some(crate::repo::hash::content_hash(&dir)?);
            rebuilt.packages.insert(
                spec.name.clone(),
                LockEntry { url: origin, commit, rev: spec.target.descriptor(), branch, hash },
            );
        }
        rebuilt_locks.insert(path, rebuilt);
    }
    for (path, rebuilt) in rebuilt_locks {
        for spec in specs.iter().filter(|spec| spec.dir.is_none() && crate::lock::spec_lock_path(cfg, spec) == path) {
            register_checkout_reference(&checkout_dir(&spec.url), &path)?;
        }
        write_lock(&path, &rebuilt)?;
    }
    Ok(())
}

pub fn restore_only(cfg: &serde_json::Value, only: Option<&str>, log: Log) -> Result<()> {
    let o = PkgOptions::from_config(cfg);
    let _ = read_lock(cfg)?;
    let specs = specs(cfg);
    validate_checkout_constraints(&specs)?;
    if let Some(name) = only
        && !specs.iter().chain(disabled_specs(cfg).iter()).any(|s| s.name == name)
    {
        bail!("no declared package named {name}");
    }
    let todo: Vec<PackageSpec> =
        specs.into_iter().filter(|s| s.dir.is_none() && only.is_none_or(|n| n == s.name)).collect();
    let results = run_each(&o, &todo, log, |s, log| {
        stop_if_cancelled()?;
        let _pkg_lock = checkout_lock(&checkout_dir(&s.url))?;
        let lock = crate::lock::read_spec_lock(cfg, s)?;
        let lock_url = source::strip_url_credentials(&s.url);
        let Some(e) = lock.packages.get(&s.name).filter(|e| e.url == lock_url) else {
            log("  ⎿ not in the lock (run `mare plugin sync`): skipped".into());
            return Ok(());
        };
        let pre = PreCheckout::capture(&o, checkout_dir(&s.url));
        let res = (|| -> Result<()> {
            let (dir, _) = ensure_clone(&o, s, log)?;
            let scope_lock = crate::lock::spec_lock_path(cfg, s);
            register_checkout_reference(&dir, &scope_lock)?;
            let state = worktree_status(&o, &dir);
            if let Some(err) = &state.error {
                bail!("cannot inspect local changes in {}: {err}", dir.display());
            }
            if state.modified_or_staged {
                flag_local_changes(&state, log);
                log("  ⎿ left as is (restoring would discard local changes)".into());
                return Ok(());
            }
            ensure_other_scopes_allow(&o, s, &dir, &scope_lock, &e.commit)?;
            let commit = match checkout(&o, &dir, &e.commit, s.submodules) {
                Ok(c) => c,
                Err(_) => {
                    fetch(&o, &dir)?;
                    checkout(&o, &dir, &e.commit, s.submodules)?
                }
            };
            log(format!("  ⎿ {}", short(&commit)));
            match verify_hash(&o, &dir, e) {
                HashCheck::Ok => Ok(()),
                HashCheck::Skipped(why) => {
                    log(format!("  ⎿ content hash not checked: {why}"));
                    Ok(())
                }
                HashCheck::Mismatch(msg) => Err(anyhow::anyhow!("{msg}")),
            }
        })();
        if let Err(err) = res {
            if let Err(rb_err) = pre.rollback(&o, s.submodules, log) {
                return Err(anyhow::anyhow!("{err:#}; rollback failed: {rb_err:#}"));
            }
            return Err(err);
        }
        Ok(())
    });
    failures(results)
}
