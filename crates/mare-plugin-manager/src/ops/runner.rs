//! Installing one package and running an operation over many in parallel.

use crate::lock::{SharedLock, checkout_lock, register_checkout_reference};
use crate::repo::checkout::{
    PreCheckout, branch_of, checkout, ensure_clone, ensure_other_scopes_allow, fetch, flag_local_changes, resolve,
    short, stop_if_cancelled, worktree_status,
};
use crate::repo::hash::lock_hash;
use crate::*;

/// Install one package at its locked commit (or the resolved target) and lock it.
pub(super) fn install_one(o: &PkgOptions, s: &PackageSpec, lock: &SharedLock, log: Log) -> Result<()> {
    stop_if_cancelled()?;
    let target_dir = checkout_dir(&s.url);
    let _pkg_lock = checkout_lock(&target_dir)?;
    let pre = PreCheckout::capture(o, target_dir);
    let res = (|| -> Result<()> {
        let lock_url = source::strip_url_credentials(&s.url);
        let (dir, changed) = ensure_clone(o, s, log)?;
        let locked = lock.refresh_get(&s.name)?.filter(|e| e.url == lock_url && !changed);
        // init.lua's constraint changed since this entry was locked: init.lua wins
        let desc = s.target.descriptor();
        let constraint_changed = locked.as_ref().is_some_and(|e| e.rev != desc);
        if constraint_changed {
            log(format!(
                "  constraint changed in init.lua ({} → {}): checking out the new one",
                locked.as_ref().and_then(|e| e.rev.clone()).unwrap_or_else(|| "latest".into()),
                desc.clone().unwrap_or_else(|| "latest".into())
            ));
        }
        let target = match locked.as_ref().filter(|_| !constraint_changed) {
            Some(e) => e.commit.clone(),
            None => {
                fetch(o, &dir)?;
                resolve(o, &dir, &s.target)?
            }
        };
        ensure_other_scopes_allow(o, s, &dir, lock.path(), &target)?;
        let commit = match checkout(o, &dir, &target, s.submodules) {
            Ok(c) => c,
            Err(_) => {
                fetch(o, &dir)?;
                checkout(o, &dir, &target, s.submodules)?
            }
        };
        flag_local_changes(&worktree_status(o, &dir), log);
        let branch = branch_of(o, &dir, &s.target);
        let hash = lock_hash(o, &dir, &commit, locked.as_ref())?;
        register_checkout_reference(&dir, lock.path())?;
        lock.commit(&s.name, LockEntry { url: lock_url, commit: commit.clone(), rev: desc, branch, hash })?;
        log(format!("  ⎿ {}", short(&commit)));
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

/// Run `f` for each package on up to `concurrency` threads (lazy.nvim runs tasks in parallel;
/// Yazi installs each package on its own). Packages sharing a checkout run one after the other.
/// The log gets each package's block (`● name` + its lines) as it finishes; the result has one
/// entry per package, in `items` order.
pub(super) fn run_each<F>(o: &PkgOptions, items: &[PackageSpec], log: Log, f: F) -> Vec<(String, Result<()>)>
where
    F: Fn(&PackageSpec, Log) -> Result<()> + Sync,
{
    use std::sync::atomic::{AtomicUsize, Ordering};
    // group by checkout: two declarations of one repository never clone it at the same time
    let mut groups: Vec<Vec<usize>> = vec![];
    let mut seen: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for (i, s) in items.iter().enumerate() {
        let key = s.dir.clone().unwrap_or_else(|| checkout_dir(&s.url));
        match seen.get(&key) {
            Some(&g) => groups[g].push(i),
            None => {
                seen.insert(key, groups.len());
                groups.push(vec![i]);
            }
        }
    }
    let token = git::current_token();
    let next = AtomicUsize::new(0);
    let workers = o.concurrency.max(1).min(groups.len().max(1));
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Vec<String>, Result<()>)>();
    let mut results: Vec<Option<Result<()>>> = items.iter().map(|_| None).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            let tx = tx.clone();
            let (groups, next, f, token) = (&groups, &next, &f, token.clone());
            scope.spawn(move || {
                let work = || {
                    loop {
                        let g = next.fetch_add(1, Ordering::Relaxed);
                        let Some(group) = groups.get(g) else { break };
                        for &i in group {
                            let mut lines = vec![];
                            let r = if cancelled() {
                                Err(anyhow::anyhow!("cancelled"))
                            } else {
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    f(&items[i], &mut |l| lines.push(l))
                                }))
                                .unwrap_or_else(|_| Err(anyhow::anyhow!("internal error (panic)")))
                            };
                            if tx.send((i, lines, r)).is_err() {
                                return;
                            }
                        }
                    }
                };
                match &token {
                    Some(t) => with_cancel(t, work),
                    None => work(),
                }
            });
        }
        drop(tx);
        for (i, lines, r) in rx {
            log(format!("● {}", items[i].display.as_deref().unwrap_or(&items[i].name)));
            for l in lines {
                log(l);
            }
            if let Err(e) = &r {
                log(format!("  ✗ {e:#}"));
            }
            results[i] = Some(r);
        }
    });
    items
        .iter()
        .zip(results)
        .map(|(s, r)| (s.name.clone(), r.unwrap_or_else(|| Err(anyhow::anyhow!("not run")))))
        .collect()
}

/// One error naming every package that failed (the others succeeded and are locked).
pub(super) fn failures(results: Vec<(String, Result<()>)>) -> Result<()> {
    let failed: Vec<(String, anyhow::Error)> =
        results.into_iter().filter_map(|(n, r)| r.err().map(|e| (n, e))).collect();
    if failed.is_empty() {
        return Ok(());
    }
    let mut msg = format!("{} package(s) failed (the others are done and locked):", failed.len());
    for (n, e) in &failed {
        use std::fmt::Write;
        let _ = write!(msg, "\n  {n}: {e:#}");
    }
    Err(anyhow::anyhow!(msg))
}
