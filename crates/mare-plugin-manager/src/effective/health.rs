//! Installed, loadable packages and startup problems: manifests that block, missing checkouts,
//! content-hash mismatches.

use crate::extras::enabled::implicit_extra_specs;
use crate::repo::hash::{HashCheck, verify_hash};
use crate::*;

pub(crate) fn manifest_compatibility(m: &Manifest) -> Option<(String, String)> {
    if let Some(req) = &m.requires {
        let mine = semver::Version::parse(env!("CARGO_PKG_VERSION")).ok()?;
        match semver::VersionReq::parse(req) {
            Ok(r) if r.matches(&mine) => {}
            Ok(_) => {
                return Some((format!("requires = {req}"), format!("needs mare {req} (this is {mine}) — update mare")));
            }
            Err(e) => return Some((format!("requires = {req}"), format!("invalid version range {req}: {e}"))),
        }
    }
    if !m.platforms.is_empty() && !m.platforms.iter().any(|platform| platform.as_str() == std::env::consts::OS) {
        let platforms = m.platforms.iter().map(PluginPlatform::as_str).collect::<Vec<_>>().join(", ");
        return Some((
            format!("platforms = [{platforms}]"),
            format!("needs platform {platforms} (this is {})", std::env::consts::OS),
        ));
    }
    None
}

pub(crate) fn manifest_diagnostic(dir: &Path, name: &str) -> Option<ManifestDiagnostic> {
    let path = dir.join("mare-plugin.toml").to_string_lossy().into_owned();
    match manifest(dir) {
        Ok(Some(m)) => manifest_compatibility(&m).map(|(constraint, message)| ManifestDiagnostic {
            plugin: name.into(),
            path,
            constraint: Some(constraint),
            message,
        }),
        Ok(None) => None,
        Err(err) => {
            Some(ManifestDiagnostic { plugin: name.into(), path, constraint: None, message: format!("{err:#}") })
        }
    }
}

/// Why a package can't load (unsatisfied constraints or a broken manifest).
pub(crate) fn blocked(dir: &Path) -> Option<String> {
    manifest_diagnostic(dir, &dir.to_string_lossy()).map(|diagnostic| diagnostic.message)
}

fn subdir_escapes_checkout(s: &PackageSpec, dir: &Path) -> bool {
    if s.dir.is_some() || s.subdir.is_none() || !dir.exists() {
        return false;
    }
    let checkout = checkout_dir(&s.url);
    let (Ok(canon_checkout), Ok(canon_dir)) = (dunce::canonicalize(&checkout), dunce::canonicalize(dir)) else {
        return true;
    };
    !canon_dir.starts_with(&canon_checkout)
}

pub(crate) fn checkout_matches_lock(o: &PkgOptions, spec: &PackageSpec, lock: &LockFile) -> bool {
    if spec.dir.is_some() {
        return true;
    }
    let dir = checkout_dir(&spec.url);
    let lock_url = source::strip_url_credentials(&spec.url);
    let Some(entry) = lock.packages.get(&spec.name).filter(|entry| entry.url == lock_url) else { return false };
    crate::lock::is_valid_commit_sha(&entry.commit)
        && entry.rev == spec.target.descriptor()
        && crate::repo::checkout::head(o, &dir).as_deref() == Some(entry.commit.as_str())
        && !matches!(verify_hash(o, &dir, entry), HashCheck::Mismatch(_))
}

fn subplugins(dir: &Path, only: Option<&[String]>) -> (Vec<String>, Vec<ManifestDiagnostic>) {
    let mut allowed = Vec::new();
    let mut diagnostics = Vec::new();
    let mut paths: Vec<_> =
        std::fs::read_dir(dir.join("plugins")).into_iter().flatten().flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        let is_dir = path.is_dir();
        let name = if is_dir && path.join("init.lua").exists() {
            path.file_name().and_then(|name| name.to_str())
        } else if !is_dir && path.extension().and_then(|ext| ext.to_str()) == Some("lua") {
            path.file_stem().and_then(|name| name.to_str())
        } else {
            None
        };
        let Some(name) = name else { continue };
        if name.starts_with('_')
            || name.starts_with('.')
            || only.is_some_and(|only| !only.iter().any(|item| item == name))
        {
            continue;
        }
        if is_dir && let Some(diagnostic) = manifest_diagnostic(&path, name) {
            diagnostics.push(diagnostic);
        } else {
            allowed.push(name.to_string());
        }
    }
    (allowed, diagnostics)
}

/// Installed, loadable packages in load order — used by the plugin loader.
pub fn installed(cfg: &serde_json::Value) -> Vec<InstalledPackage> {
    let specs = specs(cfg);
    let lock = read_lock(cfg).ok();
    let user_lock = read_lock(&serde_json::json!({})).ok();
    let options = PkgOptions::from_config(cfg);
    specs
        .into_iter()
        .filter_map(|s| {
            let dir = s.root();
            if !dir.exists()
                || subdir_escapes_checkout(&s, &dir)
                || blocked(&dir).is_some()
                || (s.dir.is_none()
                    && !(if s.project_lock { &lock } else { &user_lock })
                        .as_ref()
                        .is_some_and(|lock| checkout_matches_lock(&options, &s, lock)))
            {
                return None;
            }
            let (allowed, diagnostics) = subplugins(&dir, s.only.as_deref());
            let only = if diagnostics.is_empty() { s.only } else { Some(allowed) };
            Some(InstalledPackage { name: s.name, scope: s.scope, dir, only })
        })
        .collect()
}

/// Catalog and installed subplugin problems, with their source paths and constraints.
pub fn diagnostics(cfg: &serde_json::Value) -> Vec<ManifestDiagnostic> {
    let mut out = crate::extras::catalog::scan_catalog(cfg).map(|(_, diagnostics)| diagnostics).unwrap_or_default();
    for spec in specs(cfg) {
        let dir = spec.root();
        if dir.exists() && blocked(&dir).is_none() {
            out.extend(subplugins(&dir, spec.only.as_deref()).1);
        }
    }
    out
}

/// Startup problems with declared packages (shown as notices): missing after install, kept
/// from loading by their manifest, an unreadable plugins.json, conflicting extras.
pub fn problems(cfg: &serde_json::Value) -> Vec<String> {
    let mut out = vec![];
    if let Err(err) = read_lock(cfg) {
        out.push(format!("package lock: {err:#}"));
    }
    if let Err(e) = read_extras(cfg) {
        out.push(format!("plugins not loaded: {e:#} — fix it or enable plugins again in /plugins"));
    }
    out.extend(implicit_extra_specs(cfg).1);
    let specs = specs(cfg);
    if let Err(err) = crate::effective::specs::validate_checkout_constraints(&specs) {
        out.push(format!("packages not loaded: {err:#}"));
    }
    for s in specs {
        let dir = s.root();
        if !dir.exists() {
            if s.dir.is_some() {
                out.push(format!("package {}: local directory {} not found", s.name, dir.display()));
            }
            continue;
        }
        if subdir_escapes_checkout(&s, &dir) {
            out.push(format!("package {} not loaded: subdir escapes checkout", s.name));
            continue;
        }
        if let Some(why) = blocked(&dir) {
            out.push(format!("package {} not loaded: {why}", s.name));
        }
    }
    for diagnostic in diagnostics(cfg) {
        out.push(format!("plugin {} not loaded ({}): {}", diagnostic.plugin, diagnostic.path, diagnostic.message));
    }
    out.extend(hash_problems(cfg));
    out
}

/// Startup check of the lock's content hashes: a checkout at its locked commit,
/// with nothing changed per git, whose files still differ from the hash is reported. Checkouts
/// with local edits, untracked files or another commit checked out are what `/plugins` flags
/// already; an entry without a hash (an older lock) is accepted.
fn hash_problems(cfg: &serde_json::Value) -> Vec<String> {
    let o = PkgOptions::from_config(cfg);
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for s in specs(cfg).into_iter().filter(|s| s.dir.is_none()) {
        let Ok(lock) = crate::lock::read_spec_lock(cfg, &s) else { continue };
        let lock_url = source::strip_url_credentials(&s.url);
        let Some(e) = lock.packages.get(&s.name).filter(|e| e.url == lock_url && e.hash.is_some()) else { continue };
        let dir = checkout_dir(&s.url);
        if mare_shared::git::git_dir(&dir).is_none() || !seen.insert(dir.clone()) {
            continue;
        }
        if let HashCheck::Mismatch(msg) = verify_hash(&o, &dir, e) {
            out.push(format!("package {}: {msg}", s.name));
        }
    }
    out
}

/// Whether startup would need to install any enabled remote package. This performs only local
/// filesystem checks; callers can use it to show the first frame before starting network work.
pub fn needs_install(cfg: &serde_json::Value) -> bool {
    let all = specs(cfg);
    let Ok(lock) = read_lock(cfg) else { return true };
    let Ok(user_lock) = read_lock(&serde_json::json!({})) else { return true };
    let options = PkgOptions::from_config(cfg);
    all.into_iter().any(|s| {
        s.dir.is_none() && !checkout_matches_lock(&options, &s, if s.project_lock { &lock } else { &user_lock })
    })
}

/// Enabled packages which cannot currently be loaded, including missing or incompatible ones.
pub fn unavailable(cfg: &serde_json::Value) -> Vec<String> {
    let present: std::collections::HashSet<_> = installed(cfg).into_iter().map(|p| p.name).collect();
    specs(cfg).into_iter().filter(|s| !present.contains(&s.name)).map(|s| s.name).collect()
}
