//! Sync: install missing packages at their locked commits.

use super::runner::{failures, install_one, run_each};
use crate::effective::health::checkout_matches_lock;
use crate::effective::specs::{declared_names, validate_checkout_constraints};
use crate::lock::SharedLock;
use crate::*;

/// Before plugins load: install declared packages that aren't there yet (lazy.nvim
/// `install.missing`). A package that can't be installed (offline, bad URL, no credentials) is
/// reported to the caller, which decides whether startup may continue. Returns failures in
/// declaration order; successful installs are passed to `on_installed`.
pub fn install_missing(cfg: &serde_json::Value, on_installed: &mut dyn FnMut(&str)) -> Vec<String> {
    install_missing_with_progress(cfg, on_installed, &mut |_| ())
}

/// [`install_missing`] reporting progress as it goes (shown in the activity area):
/// `Installing packages (0/2): a, b` first, then `(k/n)` and the package that just finished.
pub fn install_missing_with_progress(
    cfg: &serde_json::Value,
    on_installed: &mut dyn FnMut(&str),
    on_progress: &mut dyn FnMut(String),
) -> Vec<String> {
    let o = PkgOptions::from_config(cfg);
    let all = specs(cfg);
    if let Err(err) = validate_checkout_constraints(&all) {
        return vec![format!("packages not installed: {err:#}")];
    }
    let disk_lock = match read_lock(cfg) {
        Ok(lock) => lock,
        Err(err) => return vec![format!("packages not installed: {err:#}")],
    };
    let user_cfg = serde_json::json!({});
    let user_disk = match read_lock(&user_cfg) {
        Ok(lock) => lock,
        Err(err) => return vec![format!("packages not installed: {err:#}")],
    };
    let missing: Vec<PackageSpec> = all
        .into_iter()
        .filter(|s| {
            s.dir.is_none() && !checkout_matches_lock(&o, s, if s.project_lock { &disk_lock } else { &user_disk })
        })
        .collect();
    if missing.is_empty() {
        return vec![];
    }
    if mare_shared::env::is_offline_env() {
        return missing.into_iter().map(|s| format!("package {} not installed (MARE_OFFLINE=1)", s.name)).collect();
    }
    let lock = match SharedLock::new(cfg) {
        Ok(l) => l,
        Err(e) => return vec![format!("packages not installed: {e:#}")],
    };
    let user_lock = match SharedLock::new(&user_cfg) {
        Ok(l) => l,
        Err(e) => return vec![format!("packages not installed: {e:#}")],
    };
    let total = missing.len();
    let shown = |s: &PackageSpec| s.display.clone().unwrap_or_else(|| s.name.clone());
    on_progress(format!(
        "Installing packages (0/{total}): {}",
        missing.iter().map(shown).collect::<Vec<_>>().join(", ")
    ));
    let mut done = 0;
    let results = run_each(
        &o,
        &missing,
        &mut |l| {
            // run_each opens each finished package's block with `● name`
            if let Some(name) = l.strip_prefix("● ") {
                done += 1;
                on_progress(format!("Installing packages ({done}/{total}): {name} done"));
            }
        },
        |s, log| install_one(&o, s, if s.project_lock { &lock } else { &user_lock }, log),
    );
    let mut notices = vec![];
    for (name, r) in results {
        match r {
            Ok(()) => on_installed(&name),
            Err(e) => {
                let msg = format!("{e:#}");
                if msg == "cancelled" {
                    if !notices.iter().any(|n: &String| n == "package installation cancelled") {
                        notices.push("package installation cancelled".into());
                    }
                    continue;
                }
                let why = if msg.starts_with("authentication failed") { "" } else { " (offline?)" };
                notices.push(format!("package {name} not installed{why}: {msg}"));
            }
        }
    }
    notices
}

/// Install missing packages; respect the lock file (lazy.nvim `sync`).
pub fn sync(cfg: &serde_json::Value, log: Log) -> Result<()> {
    sync_only(cfg, None, log)
}

/// [`sync`] for one package (`/plugins` row action) or all. Each package succeeds or fails on its
/// own and is locked as soon as it's done; packages removed from init.lua leave the lock
/// (disabled ones keep their entries — lazy.nvim `lock.lua`).
pub fn sync_only(cfg: &serde_json::Value, only: Option<&str>, log: Log) -> Result<()> {
    if mare_shared::env::is_offline_env() {
        bail!("offline mode is active (MARE_OFFLINE); cannot sync plugins");
    }
    let o = PkgOptions::from_config(cfg);
    let specs = specs(cfg);
    validate_checkout_constraints(&specs)?;
    let lock = SharedLock::new(cfg)?;
    let user_lock = SharedLock::new(&serde_json::json!({}))?;
    let todo: Vec<PackageSpec> = specs.iter().filter(|s| only.is_none_or(|n| n == s.name)).cloned().collect();
    if let Some(n) = only
        && todo.is_empty()
    {
        bail!("no enabled package named {n}");
    }
    let results = run_each(&o, &todo, log, |s, log| {
        if let Some(d) = &s.dir {
            log(format!("  ⎿ local directory {}", d.display()));
            return Ok(());
        }
        install_one(&o, s, if s.project_lock { &lock } else { &user_lock }, log)
    });
    if only.is_none() && !cancelled() {
        if o.project_scope {
            let keep: BTreeSet<String> = specs
                .iter()
                .cloned()
                .chain(disabled_specs(cfg))
                .filter(|spec| spec.project_lock && spec.dir.is_none())
                .map(|spec| spec.name)
                .collect();
            for checkout in lock.prune_undeclared(&keep)? {
                let _pkg_lock = crate::lock::checkout_lock(&checkout)?;
                crate::lock::unregister_checkout_reference(&checkout, lock.path());
            }
        } else {
            for checkout in user_lock.prune_undeclared(&declared_names(cfg))? {
                let _pkg_lock = crate::lock::checkout_lock(&checkout)?;
                crate::lock::unregister_checkout_reference(&checkout, user_lock.path());
            }
        }
    } else {
        if todo.iter().any(|s| s.project_lock) {
            lock.ensure_exists()?;
        }
        if todo.iter().any(|s| !s.project_lock) {
            user_lock.ensure_exists()?;
        }
    }
    failures(results)
}
