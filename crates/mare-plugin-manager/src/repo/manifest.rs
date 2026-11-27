//! `mare-plugin.toml`: a package's or catalog plugin's manifest.

use crate::*;

/// `mare-plugin.toml` at a package root or in a catalog plugin directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: Option<String>,
    pub description: Option<String>,
    /// required mare version, a semver range (`">=0.4"`)
    pub requires: Option<String>,
    pub version: Option<String>,
    pub provides: Option<PluginProvides>,
    #[serde(default)]
    pub platforms: Vec<PluginPlatform>,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub license: Option<String>,
    pub homepage: Option<String>,
    #[serde(default = "default_news")]
    pub news: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    // resource directories (custom overrides relative to package root)
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub themes: Vec<String>,
    // Catalog metadata and dependencies.
    pub category: Option<String>,
    #[serde(default)]
    pub deps: PluginDependencies,
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// Other catalog plugins this one needs.
    #[serde(default)]
    pub plugins: Vec<String>,
    pub recommended: Option<Recommended>,
    /// Declarative static frontend assets (`[web] dir = "dist"`).
    pub web: Option<WebManifest>,
}

fn default_news() -> String {
    "NEWS.md".into()
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PluginProvides {
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub hooks: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub prompts: Vec<String>,
    #[serde(default)]
    pub themes: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub flags: Vec<String>,
    #[serde(default)]
    pub mcp: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PluginPlatform {
    Linux,
    Macos,
    Windows,
}

impl PluginPlatform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Macos => "macos",
            Self::Windows => "windows",
        }
    }
}

/// A skipped plugin's manifest failure or unmet constraint.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestDiagnostic {
    pub plugin: String,
    pub path: String,
    pub constraint: Option<String>,
    pub message: String,
}

/// `[web]` table in `mare-plugin.toml`: pre-built static assets served by the host webserver.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WebManifest {
    pub dir: String,
    pub prefix: Option<String>,
    pub index: Option<String>,
    pub fallback: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PluginDependencies {
    #[serde(default)]
    pub required: Vec<String>,
    #[serde(default)]
    pub optional: Vec<String>,
}

/// Static project markers and programs used to recommend a catalog plugin.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recommended {
    #[serde(default)]
    pub root: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
}

pub fn manifest(dir: &Path) -> Result<Option<Manifest>> {
    let p = dir.join("mare-plugin.toml");
    match std::fs::read_to_string(&p) {
        Ok(s) => {
            let document: toml::Value = toml::from_str(&s).with_context(|| format!("{} is not valid", p.display()))?;
            if document.get("extensions").is_some() {
                bail!("{}: extensions are no longer a supported resource kind", p.display());
            }
            Ok(Some(document.try_into().with_context(|| format!("{} is not valid", p.display()))?))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", p.display())),
    }
}
