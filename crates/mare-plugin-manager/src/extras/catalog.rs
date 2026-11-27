//! The extras catalogs: their repositories, checkouts and entries; fetching them.

use crate::repo::checkout::{checkout, ensure_clone_at, fetch, resolve};
use crate::spec::parse_spec;
use crate::*;

/// The catalog and the packages it names are written by the catalog, not the user: they expand
/// with the default (GitHub) `url_format`, whatever `mare.o.plugins.url_format` says.
pub(super) fn catalog_spec(name: &str) -> Option<PackageSpec> {
    parse_spec(&serde_json::json!({ "name": name }), &PkgOptions::default())
}

/// The catalog repositories: signalridge/mare-plugins, then `mare.o.plugins.sources`.
pub(super) fn catalog_sources(o: &PkgOptions) -> Vec<String> {
    let mut v = vec![DEFAULT_SOURCE.to_string()];
    for s in &o.sources {
        if !v.contains(s) {
            v.push(s.clone());
        }
    }
    v
}

pub(crate) fn catalog_dir_of(repo: &str) -> PathBuf {
    let url = source::expand(repo, DEFAULT_URL_FORMAT);
    let root = mare_shared::paths::packages_dir();
    let checkout = checkout_dir(&url);
    root.join(".catalogs").join(checkout.strip_prefix(&root).expect("checkout is under packages"))
}

pub(crate) fn catalog_dirs(o: &PkgOptions) -> Vec<PathBuf> {
    catalog_sources(o).iter().map(|r| catalog_dir_of(r)).collect()
}

/// One extra of a catalog.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: String,
    pub repo: String,
    pub dir: PathBuf,
    pub manifest: Manifest,
}

impl CatalogEntry {
    pub(super) fn plugin(&self) -> String {
        self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// Every catalog's extras (empty until a catalog is installed), including names from different sources.
pub fn catalog_for(cfg: &serde_json::Value) -> Result<Vec<CatalogEntry>> {
    Ok(scan_catalog(cfg)?.0)
}

pub(crate) fn scan_catalog(cfg: &serde_json::Value) -> Result<(Vec<CatalogEntry>, Vec<ManifestDiagnostic>)> {
    let o = PkgOptions::from_config(cfg);
    let mut out: Vec<CatalogEntry> = vec![];
    let mut diagnostics = Vec::new();
    for repo in catalog_sources(&o) {
        let root = catalog_dir_of(&repo).join("plugins");
        let mut source_names = BTreeSet::new();
        let mut dirs: Vec<PathBuf> =
            std::fs::read_dir(&root).into_iter().flatten().flatten().map(|e| e.path()).collect();
        dirs.sort();
        for d in dirs {
            if !d.is_dir() {
                continue;
            }
            let hint = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let m = match manifest(&d) {
                Ok(Some(m)) => m,
                Ok(None) => continue,
                Err(err) => {
                    diagnostics.push(ManifestDiagnostic {
                        plugin: hint,
                        path: d.join("mare-plugin.toml").to_string_lossy().into_owned(),
                        constraint: None,
                        message: format!("{err:#}"),
                    });
                    continue;
                }
            };
            if let Some((constraint, message)) = crate::effective::health::manifest_compatibility(&m) {
                diagnostics.push(ManifestDiagnostic {
                    plugin: m.name.clone().unwrap_or(hint),
                    path: d.join("mare-plugin.toml").to_string_lossy().into_owned(),
                    constraint: Some(constraint),
                    message,
                });
                continue;
            }
            let name = m
                .name
                .clone()
                .filter(|name| !name.is_empty() && !name.contains('.'))
                .with_context(|| format!("{} needs a short plugin name without dots", d.display()))?;
            if !source_names.insert(name.clone()) {
                bail!("duplicate plugin name {name:?} in source {repo}");
            }
            out.push(CatalogEntry { name, repo: repo.clone(), dir: d, manifest: m });
        }
    }
    Ok((out, diagnostics))
}

/// The default catalog as `(name, dir, manifest)`.
pub fn catalog() -> Result<Vec<(String, PathBuf, Manifest)>> {
    Ok(catalog_for(&serde_json::json!({}))?.into_iter().map(|e| (e.name, e.dir, e.manifest)).collect())
}

/// Fetch the catalogs (clone the extras repos without enabling anything) so `/plugins` can list
/// them.
pub fn fetch_catalog(log: Log) -> Result<()> {
    fetch_catalog_for(&serde_json::json!({}), log)
}

pub fn fetch_catalog_for(cfg: &serde_json::Value, log: Log) -> Result<()> {
    let o = PkgOptions::from_config(cfg);
    let mut first_err = None;
    for repo in catalog_sources(&PkgOptions::from_config(cfg)) {
        if crate::cancelled() {
            first_err.get_or_insert_with(|| anyhow::anyhow!("cancelled"));
            break;
        }
        let s = catalog_spec(&repo).with_context(|| format!("{repo} is not a valid source"))?;
        let r = (|| {
            let catalog_dir = catalog_dir_of(&repo);
            let _pkg_lock = crate::lock::checkout_lock(&catalog_dir)?;
            let (dir, _) = ensure_clone_at(&o, &s, &catalog_dir, log)?;
            fetch(&o, &dir)?;
            let target = resolve(&o, &dir, &s.target)?;
            checkout(&o, &dir, &target, s.submodules)?;
            Ok::<_, anyhow::Error>(())
        })();
        if let Err(e) = r {
            log(format!("{repo}: {e:#}"));
            first_err.get_or_insert(e);
        }
    }
    first_err.map_or(Ok(()), Err)
}
