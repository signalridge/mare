//! Declarations → `PackageSpec`: targets, `mare.o.plugins` options, parsing a declaration.

use crate::*;

/// What a package checks out.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Commit(String),
    Tag(String),
    Branch(String),
    /// any git revision (the older `rev = …` form); `=<commit>` is an exact pin
    Rev(String),
    /// a semver range over the repo's tags (lazy.nvim `version`)
    Version(String),
    /// newest semver tag, else the default branch
    Latest,
}

impl Target {
    /// The constraint as recorded in the lock (a changed constraint re-resolves on sync).
    pub(crate) fn descriptor(&self) -> Option<String> {
        match self {
            Self::Commit(c) => Some(format!("commit:{c}")),
            Self::Tag(t) => Some(format!("tag:{t}")),
            Self::Branch(b) => Some(format!("branch:{b}")),
            Self::Rev(r) => Some(r.clone()),
            Self::Version(v) => Some(format!("version:{v}")),
            Self::Latest => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackageSpec {
    pub name: String,
    pub scope: &'static str,
    pub project_lock: bool,
    /// `name = "…"` next to the source: only what's shown (lazy.nvim)
    pub display: Option<String>,
    pub url: String,
    pub target: Target,
    pub only: Option<Vec<String>>,
    pub subdir: Option<String>,
    /// a local package (lazy.nvim `dir`): loaded from there, never installed or locked
    pub dir: Option<PathBuf>,
    pub dependencies: Vec<String>,
    /// `pin = true`: `update` leaves it alone
    pub pin: bool,
    /// `submodules = false` skips git submodules (lazy.nvim `submodules`)
    pub submodules: bool,
}

impl PackageSpec {
    pub(super) fn exact_pin(&self) -> bool {
        self.pin
            || matches!(&self.target, Target::Commit(_))
            || matches!(&self.target, Target::Rev(r) if r.starts_with('='))
    }
    /// Where the package's files are (checkout + subdir, or the local dir).
    pub fn root(&self) -> PathBuf {
        if let Some(d) = &self.dir {
            return d.clone();
        }
        let mut d = checkout_dir(&self.url);
        if let Some(sub) = &self.subdir {
            d = d.join(sub);
        }
        d
    }
}

#[derive(Debug, Clone)]
pub struct InstalledPackage {
    pub name: String,
    pub scope: &'static str,
    pub dir: PathBuf,
    pub only: Option<Vec<String>>,
}

/// A log line sink (the CLI prints; `/plugins` collects).
pub type Log<'a> = &'a mut dyn FnMut(String);

/// `mare.o.plugins = { url_format, allow_insecure, timeout, concurrency, sources }`.
#[derive(Debug, Clone)]
pub struct PkgOptions {
    /// short names expand through this (`%s` = the name); lazy.nvim `git.url_format`
    pub url_format: String,
    /// allow plain `http://` and `git://`
    pub allow_insecure: bool,
    /// per git operation (`timeout` in seconds); lazy.nvim `git.timeout`
    pub timeout: Duration,
    /// packages fetched at the same time (lazy.nvim `concurrency`)
    pub concurrency: usize,
    /// more extras catalogs besides `signalridge/mare-plugins`
    pub sources: Vec<String>,
    pub project_scope: bool,
}

impl Default for PkgOptions {
    fn default() -> Self {
        Self {
            url_format: DEFAULT_URL_FORMAT.into(),
            allow_insecure: false,
            timeout: DEFAULT_TIMEOUT,
            concurrency: DEFAULT_CONCURRENCY,
            sources: vec![],
            project_scope: false,
        }
    }
}

impl PkgOptions {
    pub fn from_config(cfg: &serde_json::Value) -> Self {
        let mut o = Self {
            project_scope: cfg.get(crate::lock::PROJECT_SCOPE_KEY).and_then(serde_json::Value::as_str).is_some(),
            ..Self::default()
        };
        let Some(p) = cfg.get("plugins") else { return o };
        if let Some(f) = p.get("url_format").and_then(|v| v.as_str()).filter(|f| f.contains("%s")) {
            o.url_format = f.to_string();
        }
        if let Some(b) = p.get("allow_insecure").and_then(|v| v.as_bool()) {
            o.allow_insecure = b;
        }
        if let Some(t) = p.get("timeout").and_then(|v| v.as_f64()).filter(|t| t.is_finite() && *t > 0.0) {
            o.timeout = Duration::from_secs_f64(t);
        }
        if let Some(n) = p.get("concurrency").and_then(|v| v.as_u64()).filter(|n| *n > 0) {
            o.concurrency = n.min(64) as usize;
        }
        o.sources = str_list(p.get("sources"));
        o
    }

    pub(crate) fn git(&self, dir: Option<&Path>, args: &[&str]) -> Result<String> {
        git::run(dir, args, self.timeout, self.allow_insecure)
    }
}

pub(super) fn expand_home(p: &str) -> PathBuf {
    if p == "~" {
        return dirs_home();
    }
    match p.strip_prefix("~/").or_else(|| if cfg!(windows) { p.strip_prefix("~\\") } else { None }) {
        Some(rest) => dirs_home().join(rest),
        None => PathBuf::from(p),
    }
}

pub(super) fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| if cfg!(windows) { std::env::var_os("USERPROFILE") } else { None })
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn str_list(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => vec![],
    }
}

pub(crate) fn is_disabled(t: &serde_json::Value) -> bool {
    t.get("enabled").and_then(|v| v.as_bool()) == Some(false)
}

/// A declaration, enabled or not (`None` when it can't name a package or its subdir escapes).
pub(crate) fn parse_any(t: &serde_json::Value, o: &PkgOptions) -> Option<PackageSpec> {
    let get = |k: &str| t.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let dir = get("dir").map(|d| expand_home(&d));
    let name =
        get("name").or_else(|| dir.as_ref().and_then(|d| d.file_name().map(|n| n.to_string_lossy().into_owned())))?;
    let short_subdir = matches!(source::classify(&name), source::Source::Short).then(|| name.split_once(':')).flatten();
    let subdir = get("subdir").or_else(|| short_subdir.map(|(_, sub)| sub.to_string()));
    // a subdir is joined onto the checkout to load from: it stays inside it
    if subdir.as_deref().is_some_and(|d| {
        d.is_empty()
            || !Path::new(d).components().any(|c| matches!(c, std::path::Component::Normal(_)))
            || Path::new(d)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_) | std::path::Component::CurDir))
    }) {
        return None;
    }
    let source_name =
        get("source").unwrap_or_else(|| short_subdir.map_or_else(|| name.clone(), |(repo, _)| repo.into()));
    let url = get("url").unwrap_or_else(|| source::expand(&source_name, &o.url_format));
    let safe_ref = |r: &str| !r.is_empty() && !r.starts_with('-') && !r.contains(['\0', '\n', '\r']);
    let target = if let Some(c) = get("commit") {
        if !(4..=64).contains(&c.len()) || !c.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        Target::Commit(c)
    } else if let Some(tag) = get("tag") {
        if !safe_ref(&tag) {
            return None;
        }
        Target::Tag(tag)
    } else if let Some(b) = get("branch") {
        if !safe_ref(&b) {
            return None;
        }
        Target::Branch(b)
    } else if let Some(v) = get("version") {
        Target::Version(v)
    } else if let Some(r) = get("rev") {
        if !safe_ref(r.strip_prefix('=').unwrap_or(&r)) {
            return None;
        }
        Target::Rev(r)
    } else {
        Target::Latest
    };
    let only = t.get("only").map(|v| str_list(Some(v)));
    Some(PackageSpec {
        scope: "init.lua",
        project_lock: match get("lock_scope").as_deref() {
            Some("user") => false,
            Some("project") => o.project_scope,
            _ => o.project_scope,
        },
        display: get("display"),
        url,
        target,
        only,
        subdir,
        dir,
        dependencies: str_list(t.get("dependencies")),
        pin: t.get("pin").and_then(|v| v.as_bool()).unwrap_or(false),
        submodules: t.get("submodules").and_then(|v| v.as_bool()).unwrap_or(true),
        name,
    })
}

pub(crate) fn parse_spec(t: &serde_json::Value, o: &PkgOptions) -> Option<PackageSpec> {
    if is_disabled(t) {
        return None;
    }
    parse_any(t, o)
}

pub(crate) fn declarations(cfg: &serde_json::Value) -> Vec<serde_json::Value> {
    cfg.get("packages").and_then(|v| v.as_array()).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_spec_rejects_flag_like_refs_and_invalid_commits_and_formats_tag() {
        let o = PkgOptions::from_config(&json!({}));

        // Valid tag & string-form `only` / `dependencies`.
        let tagged =
            parse_spec(&json!({"name": "owner/repo", "tag": "v1.2.3", "only": "foo", "dependencies": "dep/a"}), &o)
                .unwrap();
        assert_eq!(tagged.target.descriptor().as_deref(), Some("tag:v1.2.3"));
        assert_eq!(tagged.only, Some(vec!["foo".to_string()]));
        assert_eq!(tagged.dependencies, vec!["dep/a".to_string()]);

        // Invalid commit hex/length and flag-like refs (`-u...`, `\n`) must be rejected.
        for bad in [
            json!({"name": "owner/repo", "commit": "abc"}),
            json!({"name": "owner/repo", "commit": "zzzzzzzz"}),
            json!({"name": "owner/repo", "tag": "-evil"}),
            json!({"name": "owner/repo", "branch": "--upload-pack=sh"}),
            json!({"name": "owner/repo", "rev": "=-bad"}),
        ] {
            assert!(parse_spec(&bad, &o).is_none(), "expected rejection for {bad}");
        }
    }
}
