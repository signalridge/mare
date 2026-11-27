//! `~/.config/mare/plugins.json`: reading, version migration, writing, and the catalogs' renames.

use super::catalog::{catalog_dir_of, catalog_sources};
use crate::lock::{parse_versioned_json_object, write_json_atomic};
use crate::*;

/// `~/.config/mare/plugins.json` (LazyVim `lazyvim.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtrasFile {
    pub version: u32,
    #[serde(default)]
    pub plugins: Vec<PluginSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct PluginSelection {
    pub source: String,
    pub name: String,
}

impl Default for ExtrasFile {
    fn default() -> Self {
        Self { version: EXTRAS_VERSION, plugins: vec![] }
    }
}

pub fn extras_path(cfg: &serde_json::Value) -> PathBuf {
    cfg.get(crate::lock::PROJECT_SCOPE_KEY)
        .and_then(serde_json::Value::as_str)
        .map(|dir| PathBuf::from(dir).join("plugins.json"))
        .unwrap_or_else(|| mare_shared::paths::config_dir().join("plugins.json"))
}

/// plugins.json; a newer format than this mare knows is refused (never rewritten), older ones
/// are migrated in memory and written back by the next change (LazyVim `lazyvim.json` version).
pub(super) fn read_extras_at(p: &Path, cfg: &serde_json::Value) -> Result<ExtrasFile> {
    match std::fs::read_to_string(p) {
        Ok(s) => {
            let raw: serde_json::Value =
                parse_versioned_json_object(&s, p).with_context(|| format!("{} is not valid", p.display()))?;
            let version = raw["version"].as_u64().unwrap_or(u64::MAX);
            if version > u64::from(EXTRAS_VERSION) {
                bail!(
                    "{} has version {} (this mare knows {EXTRAS_VERSION}): written by a newer mare — update mare",
                    p.display(),
                    version
                );
            }
            if version == u64::from(EXTRAS_VERSION) {
                return serde_json::from_value(raw).with_context(|| format!("{} is not valid", p.display()));
            }
            let names: Vec<String> =
                serde_json::from_value(raw.get("plugins").cloned().unwrap_or_else(|| serde_json::json!([])))
                    .with_context(|| format!("{} is not valid", p.display()))?;
            let catalog = super::catalog::scan_catalog(cfg)?.0;
            Ok(ExtrasFile {
                version: EXTRAS_VERSION,
                plugins: names
                    .into_iter()
                    .map(|name| {
                        let source = catalog
                            .iter()
                            .find(|entry| entry.name == name)
                            .map(|entry| entry.repo.clone())
                            .unwrap_or_else(|| DEFAULT_SOURCE.into());
                        PluginSelection { source, name }
                    })
                    .collect(),
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ExtrasFile::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}

/// Read user selections plus this trusted project's selections, without merging their files.
pub fn read_extras(cfg: &serde_json::Value) -> Result<ExtrasFile> {
    let user_path = extras_path(&serde_json::json!({}));
    let mut user = read_extras_at(&user_path, cfg)?;
    let scoped_path = extras_path(cfg);
    if scoped_path != user_path {
        for selection in read_extras_at(&scoped_path, cfg)?.plugins {
            if let Some(existing) = user.plugins.iter_mut().find(|item| item.name == selection.name) {
                *existing = selection;
            } else {
                user.plugins.push(selection);
            }
        }
    }
    Ok(user)
}

/// Saved sorted and without duplicates (LazyVim sorts `lazyvim.json`).
pub(super) fn write_extras(path: &Path, f: &ExtrasFile) -> Result<()> {
    let mut f = f.clone();
    f.version = EXTRAS_VERSION;
    f.plugins.sort_by(|a, b| (&a.name, &a.source).cmp(&(&b.name, &b.source)));
    f.plugins.dedup_by(|a, b| a.name == b.name);
    write_json_atomic(path, &f)
}

/// The catalog's rename table (`plugins.toml` at the repo root, LazyVim `renamed_extras` /
/// `deprecated_extras`).
#[derive(Debug, Default, Deserialize)]
struct Renames {
    #[serde(default)]
    renamed: BTreeMap<String, String>,
    #[serde(default)]
    deprecated: BTreeMap<String, String>,
}

/// At startup: apply the catalogs' renames to plugins.json and drop deprecated extras, writing
/// it back when something changed. Returns notices.
pub fn migrate_extras() -> Vec<String> {
    migrate_extras_for(&serde_json::json!({}))
}

pub fn migrate_extras_for(cfg: &serde_json::Value) -> Vec<String> {
    let o = PkgOptions::from_config(cfg);
    let mut table = Renames::default();
    let mut notices = vec![];
    for repo in catalog_sources(&o) {
        let Ok(s) = std::fs::read_to_string(catalog_dir_of(&repo).join("plugins.toml")) else { continue };
        match toml::from_str::<Renames>(&s) {
            Ok(t) => {
                table.renamed.extend(t.renamed);
                table.deprecated.extend(t.deprecated);
            }
            Err(_) => notices.push(format!("{repo}/plugins.toml is not valid; plugins.json left as is")),
        }
    }
    if table.renamed.is_empty() && table.deprecated.is_empty() {
        return notices;
    }
    let user_path = extras_path(&serde_json::json!({}));
    let scoped_path = extras_path(cfg);
    let mut paths = vec![user_path];
    if scoped_path != paths[0] {
        paths.push(scoped_path);
    }
    for path in paths {
        let Ok(_guard) = crate::lock::extras_lock_guard(&path) else { continue };
        // an unreadable plugins.json is reported by `problems`
        let Ok(mut f) = read_extras_at(&path, cfg) else { continue };
        let mut out: Vec<PluginSelection> = vec![];
        for e in &f.plugins {
            if let Some(new) = table.renamed.get(&e.name) {
                notices.push(format!("plugin {} is now {new} (plugins.json updated)", e.name));
                if !out.iter().any(|selection| selection.name == *new) {
                    out.push(PluginSelection { source: e.source.clone(), name: new.clone() });
                }
            } else if let Some(why) = table.deprecated.get(&e.name) {
                notices.push(format!("plugin {} was removed: {why} (dropped from plugins.json)", e.name));
            } else if !out.iter().any(|selection| selection.name == e.name) {
                out.push(e.clone());
            }
        }
        if out != f.plugins {
            f.plugins = out;
            if let Err(e) = write_extras(&path, &f) {
                notices.push(format!("could not update plugins.json: {e:#}"));
            }
        }
    }
    notices
}
