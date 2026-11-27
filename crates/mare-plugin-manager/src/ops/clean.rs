//! Clean: remove checkouts no longer declared and stale lock entries.

use super::find_spec;
use crate::effective::specs::declared_names;
use crate::extras::catalog::catalog_dirs;
use crate::lock::{
    checkout_lock, lockfile_guard, other_scope_reference, remove_all_checkout_references,
    unregister_checkout_reference, write_lock,
};
use crate::repo::checkout::{prune_empty_parents, worktree_status};
use crate::*;

/// Remove checkouts no longer declared (at any depth: `<host>/<group>/<sub>/<repo>`), and the
/// directories that leaves empty; lock entries of packages no longer declared at all are
/// dropped (disabled ones keep theirs). A checkout with local changes or untracked files is
/// kept and reported. The extras catalogs are kept: the panel lists them.
pub fn remove_unused(cfg: &serde_json::Value, force: bool, log: Log) -> Result<()> {
    clean_only(cfg, None, force, log)
}

/// [`remove_unused`] for one (disabled or undeclared) package's checkout, or all.
pub fn clean_only(cfg: &serde_json::Value, only: Option<&str>, force: bool, log: Log) -> Result<()> {
    let o = PkgOptions::from_config(cfg);
    clean_with_options(&o, cfg, only, force, false, log)
}

impl PkgOptions {
    /// Describe the checkouts and lock entries a clean would remove, without changing them.
    pub fn preview_clean(&self, cfg: &serde_json::Value, only: Option<&str>, force: bool, log: Log) -> Result<()> {
        clean_with_options(self, cfg, only, force, true, log)
    }
}

fn clean_with_options(
    o: &PkgOptions,
    cfg: &serde_json::Value,
    only: Option<&str>,
    force: bool,
    preview: bool,
    log: Log,
) -> Result<()> {
    let _ = read_lock(cfg)?; // a corrupt lock stops every pkg command, this one included
    let specs = specs(cfg);
    let mut keep: Vec<PathBuf> = specs.iter().filter(|s| s.dir.is_none()).map(|s| checkout_dir(&s.url)).collect();
    keep.extend(catalog_dirs(o));
    let target = match only {
        Some(n) => {
            let d = match find_spec(cfg, n) {
                Ok(s) => checkout_dir(&s.url),
                Err(_) => {
                    let expanded = checkout_dir(&source::expand(n, &o.url_format));
                    if mare_shared::git::git_dir(&expanded).is_some() {
                        expanded
                    } else {
                        bail!("no declared or removable package named {n}");
                    }
                }
            };
            if keep.contains(&d) {
                bail!("{n} is enabled: only disabled or undeclared packages are cleaned");
            }
            Some(d)
        }
        None => None,
    };
    #[derive(Clone, Copy)]
    struct CleanMode {
        force: bool,
        preview: bool,
    }
    fn walk(
        o: &PkgOptions,
        d: &Path,
        keep: &[PathBuf],
        target: Option<&Path>,
        own_lock: &Path,
        mode: CleanMode,
        log: Log,
    ) -> Result<()> {
        let CleanMode { force, preview } = mode;
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if mare_shared::git::git_dir(&p).is_some() {
                if keep.iter().any(|k| k.starts_with(&p)) || target.is_some_and(|t| t != p) {
                    continue;
                }
                let _pkg_lock = if preview { None } else { Some(checkout_lock(&p)?) };
                if mare_shared::git::git_dir(&p).is_none() {
                    continue;
                }
                match other_scope_reference(&p, own_lock, None, !preview) {
                    Ok(Some(path)) => {
                        log(format!("kept {} — referenced by {}", p.display(), path.display()));
                        continue;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        log(format!("kept {} — cannot inspect other scopes: {err:#}", p.display()));
                        continue;
                    }
                }
                let state = worktree_status(o, &p);
                if state.has_changes() || (state.ignored && !force) {
                    let reason = if let Some(err) = &state.error {
                        format!("cannot inspect local changes: {err}")
                    } else if state.has_changes() {
                        "it has local changes or untracked files".into()
                    } else {
                        "it has ignored local files".into()
                    };
                    log(format!("kept {} — {reason} (delete it yourself)", p.display()));
                    continue;
                }
                if !force {
                    match o.git(Some(&p), &["for-each-ref", "--count=1", "--format=%(refname)", "--", "refs/stash"]) {
                        Ok(stashes) if stashes.is_empty() => {}
                        Ok(_) => {
                            log(format!("kept {} — it contains saved git stashes", p.display()));
                            continue;
                        }
                        Err(err) => {
                            log(format!("kept {} — cannot inspect saved git stashes: {err:#}", p.display()));
                            continue;
                        }
                    }
                    let branch_unpushed = o.git(Some(&p), &["rev-list", "--count", "--branches", "--not", "--remotes"]);
                    let head_unpushed = if o.git(Some(&p), &["symbolic-ref", "-q", "HEAD"]).is_err()
                        && crate::repo::checkout::head(o, &p).is_some()
                    {
                        o.git(Some(&p), &["rev-list", "--count", "HEAD", "--not", "--remotes"])
                    } else {
                        Ok("0".into())
                    };
                    match (branch_unpushed, head_unpushed) {
                        (Ok(b), Ok(h)) if b.trim().parse::<u64>() == Ok(0) && h.trim().parse::<u64>() == Ok(0) => {}
                        (Ok(_), Ok(_)) => {
                            log(format!(
                                "kept {} — it has local-only commits not on any remote branch (use --force to remove)",
                                p.display()
                            ));
                            continue;
                        }
                        (Err(err), _) | (_, Err(err)) => {
                            log(format!("kept {} — cannot inspect local commits: {err:#}", p.display()));
                            continue;
                        }
                    }
                }
                if preview {
                    log(format!("would remove {}", p.display()));
                } else {
                    log(format!("removing {}", p.display()));
                    std::fs::remove_dir_all(&p)?;
                    remove_all_checkout_references(&p);
                    prune_empty_parents(&p);
                }
            } else if mare_shared::git::marker_present(&p) {
                log(format!("kept {} — its .git entry is invalid or inaccessible", p.display()));
            } else if !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                walk(o, &p, keep, target, own_lock, mode, log)?;
            }
        }
        Ok(())
    }
    walk(
        o,
        &mare_shared::paths::packages_dir(),
        &keep,
        target.as_deref(),
        &lock_path(cfg),
        CleanMode { force, preview },
        log,
    )?;
    let declared = declared_names(cfg);
    let path = lock_path(cfg);
    let _lock_guard = if preview { None } else { Some(lockfile_guard(&path)?) };
    let mut lock = read_lock(cfg)?;
    let stale: Vec<String> = match only {
        Some(n) => lock.packages.keys().filter(|k| k.as_str() == n && !declared.contains(*k)).cloned().collect(),
        None => lock.packages.keys().filter(|k| !declared.contains(*k)).cloned().collect(),
    };
    if !stale.is_empty() {
        let mut unref_checkouts = Vec::new();
        for k in &stale {
            if preview {
                log(format!("would drop lock entry {k} (no longer declared)"));
            } else {
                log(format!("lock: dropped {k} (no longer declared)"));
                if let Some(removed) = lock.packages.remove(k) {
                    unref_checkouts.push(checkout_dir(&removed.url));
                }
            }
        }
        if !preview {
            let active: BTreeSet<PathBuf> = lock.packages.values().map(|e| checkout_dir(&e.url)).collect();
            unref_checkouts.retain(|d| !active.contains(d));
            unref_checkouts.sort();
            unref_checkouts.dedup();
            write_lock(&path, &lock)?;
            drop(_lock_guard);
            for checkout in unref_checkouts {
                let _pkg_lock = checkout_lock(&checkout)?;
                unregister_checkout_reference(&checkout, &path);
            }
        }
    }
    Ok(())
}
