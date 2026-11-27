//! Enabled extras: plugins.json plus init.lua's hand declarations, closed over what each needs,
//! conflicts skipped; recommendations; toggling; the packages they bring along.

use super::catalog::catalog_spec;
use super::state::{read_extras_at, write_extras};
use crate::spec::{declarations, is_disabled, parse_any};
use crate::*;

/// One extra: a plugin of a catalog with its manifest.
#[derive(Debug, Clone, Serialize)]
pub struct Extra {
    /// `category.name`
    pub name: String,
    pub scope: &'static str,
    pub plugin: String,
    /// the catalog repository it comes from
    pub source: String,
    pub category: String,
    pub description: String,
    pub required: Vec<String>,
    pub optional: Vec<String>,
    pub conflicts: Vec<String>,
    /// extras this one needs
    pub plugins: Vec<String>,
    /// enabled extras that need this one
    pub needed_by: Vec<String>,
    pub enabled: bool,
    /// false when init.lua declares it by hand (the panel doesn't toggle it)
    pub managed: bool,
    pub recommended: bool,
    pub provides: Option<PluginProvides>,
    pub platforms: Vec<PluginPlatform>,
    pub version: Option<String>,
    pub news: Option<String>,
}

/// A marker file of the project, from `cwd` up to the filesystem root; `.git` as Git discovers a
/// repository (an invalid `.git` file means none).
fn has_root_marker(cwd: &Path, markers: &[String]) -> bool {
    markers.iter().any(|m| {
        if m == ".git" {
            mare_shared::git::git_root(cwd).is_some()
        } else {
            cwd.ancestors().any(|d| d.join(m).exists())
        }
    })
}

pub(crate) fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p).any(|d| {
            let f = d.join(cmd);
            f.is_file() || (cfg!(windows) && d.join(format!("{cmd}.exe")).is_file())
        })
    })
}

/// Extras enabled by hand in init.lua: a catalog repository declared with `only` naming the
/// extra's plugin (or without `only`: all of them). Not managed by the panel.
fn hand_sources(cfg: &serde_json::Value, cat: &[CatalogEntry]) -> BTreeMap<String, String> {
    let o = PkgOptions::from_config(cfg);
    let mut out = BTreeMap::new();
    for t in declarations(cfg) {
        if is_disabled(&t) {
            continue;
        }
        let Some(s) = parse_any(&t, &o) else { continue };
        for e in cat {
            let repo_url = source::expand(&e.repo, DEFAULT_URL_FORMAT);
            if (s.name == e.repo || s.url == repo_url) && s.only.as_ref().is_none_or(|only| only.contains(&e.plugin()))
            {
                out.entry(e.name.clone()).or_insert_with(|| e.repo.clone());
            }
        }
    }
    out
}

fn hand_enabled(cfg: &serde_json::Value, cat: &[CatalogEntry]) -> BTreeSet<String> {
    hand_sources(cfg, cat).into_keys().collect()
}

fn resolved_catalog(
    cfg: &serde_json::Value,
    all: &[CatalogEntry],
    selections: &[PluginSelection],
) -> Vec<CatalogEntry> {
    let hand = hand_sources(cfg, all);
    let mut chosen: Vec<CatalogEntry> = Vec::new();
    for entry in all {
        let source = selections
            .iter()
            .find(|selection| selection.name == entry.name)
            .map(|selection| &selection.source)
            .or_else(|| hand.get(&entry.name));
        if source.is_some_and(|source| source != &entry.repo) || chosen.iter().any(|other| other.name == entry.name) {
            continue;
        }
        chosen.push(entry.clone());
    }
    chosen
}

fn selection_names(selections: &[PluginSelection]) -> Vec<String> {
    selections.iter().map(|selection| selection.name.clone()).collect()
}

/// Enabled extras with the reasons: the plugins.json list plus init.lua's hand declarations,
/// closed over the extras each needs; conflicting ones are skipped (the later one, with a
/// notice). Returns (enabled, needed_by, notices).
#[allow(clippy::type_complexity)]
fn effective_extras(
    cfg: &serde_json::Value,
    cat: &[CatalogEntry],
    listed: &[String],
) -> (Vec<String>, BTreeMap<String, Vec<String>>, Vec<String>) {
    let mut wanted: Vec<String> = listed.to_vec();
    wanted.extend(hand_enabled(cfg, cat));
    let mut needed_by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut i = 0;
    while i < wanted.len() {
        let e = wanted[i].clone();
        if let Some(c) = cat.iter().find(|c| c.name == e) {
            for dep in &c.manifest.plugins {
                needed_by.entry(dep.clone()).or_default().push(e.clone());
                if !wanted.contains(dep) {
                    wanted.push(dep.clone());
                }
            }
        }
        i += 1;
    }
    let conflicts_of =
        |n: &str| cat.iter().find(|c| c.name == n).map(|c| c.manifest.conflicts.clone()).unwrap_or_default();
    let mut on: Vec<String> = vec![];
    let mut notices = vec![];
    for e in wanted {
        if on.contains(&e) {
            continue;
        }
        let clash = on.iter().find(|o| conflicts_of(&e).contains(o) || conflicts_of(o).contains(&e)).cloned();
        match clash {
            Some(o) => notices.push(format!("plugin {e} skipped: it conflicts with the enabled plugin {o}")),
            None => on.push(e),
        }
    }
    (on, needed_by, notices)
}

/// The catalog with enabled / recommended flags for this project. Recommended: its `root`
/// markers are found from `cwd` up, or a `cmd` is on PATH; never one that conflicts with an
/// enabled extra (LazyVim `util/extras.lua:118`). Only a mark — nothing is installed.
pub fn extras(cwd: &Path) -> Result<Vec<Extra>> {
    extras_for(cwd, &serde_json::json!({}))
}

pub fn extras_for(cwd: &Path, cfg: &serde_json::Value) -> Result<Vec<Extra>> {
    let selections = read_extras(cfg)?.plugins;
    let listed = selection_names(&selections);
    let all = catalog_for(cfg)?;
    let cat = resolved_catalog(cfg, &all, &selections);
    let hand = hand_enabled(cfg, &cat);
    let user_path = extras_path(&serde_json::json!({}));
    let path = extras_path(cfg);
    let user = read_extras_at(&user_path, cfg)?.plugins;
    let project = if path != user_path { read_extras_at(&path, cfg)?.plugins } else { Vec::new() };
    let (enabled, needed_by, _) = effective_extras(cfg, &cat, &listed);
    let conflicts_enabled = |name: &str, m: &Manifest| {
        m.conflicts.iter().any(|c| enabled.contains(c))
            || cat.iter().any(|o| enabled.contains(&o.name) && o.manifest.conflicts.iter().any(|c| c == name))
    };
    Ok(all
        .iter()
        .map(|c| {
            let (name, dir, m) = (&c.name, &c.dir, &c.manifest);
            let on = enabled.contains(name) && cat.iter().any(|chosen| chosen.name == *name && chosen.repo == c.repo);
            let rec = m.recommended.as_ref().is_some_and(|r| {
                (!r.root.is_empty() && has_root_marker(cwd, &r.root)) || r.cmd.iter().any(|c| on_path(c))
            }) && !on
                && !conflicts_enabled(name, m);
            let news = crate::effective::contents::safe_pkg_subpath(dir, &m.news);
            Extra {
                name: name.clone(),
                scope: if hand.contains(name) && cat.iter().any(|entry| entry.name == *name && entry.repo == c.repo) {
                    "init.lua"
                } else if project.iter().any(|entry| entry.name == *name && entry.source == c.repo) {
                    "project"
                } else if user.iter().any(|entry| entry.name == *name && entry.source == c.repo) {
                    "user"
                } else if path != user_path {
                    "project"
                } else {
                    "user"
                },
                plugin: c.plugin(),
                source: c.repo.clone(),
                category: m.category.clone().unwrap_or_default(),
                description: m.description.clone().unwrap_or_default(),
                required: m.deps.required.clone(),
                optional: m.deps.optional.clone(),
                conflicts: m.conflicts.clone(),
                plugins: m.plugins.clone(),
                needed_by: needed_by.get(name).cloned().unwrap_or_default(),
                enabled: on,
                managed: !hand.contains(name),
                recommended: rec,
                provides: m.provides.clone(),
                platforms: m.platforms.clone(),
                version: m.version.clone(),
                news: news.filter(|path| path.exists()).map(|path| path.to_string_lossy().into_owned()),
            }
        })
        .collect())
}

fn validate_enable_graph(name: &str, cat: &[CatalogEntry], enabled: &[String]) -> Result<()> {
    fn visit(
        name: &str,
        cat: &[CatalogEntry],
        path: &mut Vec<String>,
        closure: &mut Vec<(String, Vec<String>)>,
    ) -> Result<()> {
        path.push(name.to_string());
        let Some(entry) = cat.iter().find(|entry| entry.name == name) else {
            bail!("unknown plugin dependency: {}", path.join(" -> "));
        };
        if !closure.iter().any(|(seen, _)| seen == name) {
            closure.push((name.to_string(), path.clone()));
            for dependency in &entry.manifest.plugins {
                visit(dependency, cat, path, closure)?;
            }
        }
        path.pop();
        Ok(())
    }

    let mut closure = Vec::new();
    visit(name, cat, &mut Vec::new(), &mut closure)?;
    let conflicts = |a: &str, b: &str| {
        cat.iter().find(|entry| entry.name == a).is_some_and(|entry| entry.manifest.conflicts.iter().any(|c| c == b))
            || cat
                .iter()
                .find(|entry| entry.name == b)
                .is_some_and(|entry| entry.manifest.conflicts.iter().any(|c| c == a))
    };
    for (i, (item, path)) in closure.iter().enumerate() {
        for other in enabled.iter().chain(closure.iter().skip(i + 1).map(|(other, _)| other)) {
            if item != other && conflicts(item, other) {
                bail!("{} conflicts with {other}", path.join(" -> "));
            }
        }
    }
    Ok(())
}

/// Enable or disable a catalog entry in the current scope, preserving its source.
pub fn set_extra(name: &str, on: bool, source: Option<&str>, cfg: &serde_json::Value) -> Result<()> {
    change_extras(cfg, &[(name, on, source)])
}

fn change_extras(cfg: &serde_json::Value, changes: &[(&str, bool, Option<&str>)]) -> Result<()> {
    let path = extras_path(cfg);
    let _guard = crate::lock::extras_lock_guard(&path)?;
    let mut f = read_extras_at(&path, cfg)?;
    let user_path = extras_path(&serde_json::json!({}));
    let user = if path != user_path { Some(read_extras_at(&user_path, cfg)?) } else { None };
    let all = catalog_for(cfg)?;
    for &(name, on, source) in changes {
        let mut combined = user.clone().unwrap_or_default();
        for selection in &f.plugins {
            combined.plugins.retain(|entry| entry.name != selection.name);
            combined.plugins.push(selection.clone());
        }
        let cat = resolved_catalog(cfg, &all, &combined.plugins);
        if hand_enabled(cfg, &cat).contains(name) {
            bail!("{name} is enabled in init.lua (not managed by the panel) — change it there");
        }
        let (enabled, needed_by, _) = effective_extras(cfg, &cat, &selection_names(&combined.plugins));
        if on {
            let selected = source.or_else(|| {
                combined.plugins.iter().find(|entry| entry.name == name).map(|entry| entry.source.as_str())
            });
            let entry = all
                .iter()
                .find(|entry| entry.name == name && selected.is_none_or(|source| entry.repo == source))
                .with_context(|| {
                    format!("plugin {name} source {} is unavailable", selected.unwrap_or("(unspecified)"))
                })?;
            let selection = PluginSelection { name: name.into(), source: entry.repo.clone() };
            let mut candidate = combined.plugins.clone();
            candidate.retain(|entry| entry.name != name);
            candidate.push(selection.clone());
            let desired = resolved_catalog(cfg, &all, &candidate);
            validate_enable_graph(name, &desired, &enabled)?;
            f.plugins.retain(|entry| entry.name != name);
            f.plugins.push(selection);
        } else {
            if !f.plugins.iter().any(|e| e.name == name) && combined.plugins.iter().any(|e| e.name == name) {
                bail!("{name} is enabled in user scope — change it there");
            }
            let needers: Vec<&String> =
                needed_by.get(name).map(|v| v.iter().filter(|n| enabled.contains(n)).collect()).unwrap_or_default();
            if !needers.is_empty() {
                let list: Vec<&str> = needers.iter().map(|s| s.as_str()).collect();
                bail!("{name} is needed by {} — turn that off first", list.join(", "));
            }
            f.plugins.retain(|e| e.name != name);
        }
    }
    write_extras(&path, &f)
}

/// Resolve a catalog plugin's short name to its source repository.
pub fn plugin_source(cfg: &serde_json::Value, name: &str) -> Result<Option<String>> {
    let selections = read_extras(cfg)?.plugins;
    let selected = selections.iter().find(|entry| entry.name == name);
    let all = catalog_for(cfg)?;
    if let Some(selected) = selected {
        if all.iter().any(|entry| entry.name == name && entry.repo == selected.source) {
            return Ok(Some(selected.source.clone()));
        }
        bail!("plugin {name} source {} is unavailable", selected.source);
    }
    Ok(all.into_iter().find(|entry| entry.name == name).map(|entry| entry.repo))
}

/// Enable catalog plugins and install the sources they require.
pub fn install_plugins(cfg: &serde_json::Value, names: &[String], log: Log) -> Result<()> {
    if catalog_for(cfg)?.is_empty() {
        fetch_catalog_for(cfg, log)?;
    }
    let changes: Vec<_> = names.iter().map(|name| (name.as_str(), true, None)).collect();
    change_extras(cfg, &changes)?;
    sync(cfg, log)
}

/// Disable catalog plugins and clean sources no longer needed.
pub fn uninstall_plugins(cfg: &serde_json::Value, names: &[String], log: Log) -> Result<()> {
    for name in names {
        if plugin_source(cfg, name)?.is_none() {
            bail!("unknown plugin {name}");
        }
    }
    let changes: Vec<_> = names.iter().map(|name| (name.as_str(), false, None)).collect();
    change_extras(cfg, &changes)?;
    remove_unused(cfg, false, log)
}

/// Packages the enabled extras need: each catalog repo with just those plugins, plus each
/// extra's required packages (explicit init.lua declarations take precedence in [`specs`]).
/// Also returns notices (conflicting extras skipped). An unreadable plugins.json enables
/// nothing (reported by [`problems`]).
pub(crate) fn implicit_extra_specs(cfg: &serde_json::Value) -> (Vec<PackageSpec>, Vec<String>) {
    let Ok(f) = read_extras(cfg) else { return (vec![], vec![]) };
    if f.plugins.is_empty() {
        return (vec![], vec![]);
    }
    let all = match catalog_for(cfg) {
        Ok(cat) => cat,
        Err(e) => return (vec![], vec![format!("plugin catalog: {e:#}")]),
    };
    let cat = resolved_catalog(cfg, &all, &f.plugins);
    let (enabled, _, mut notices) = effective_extras(cfg, &cat, &selection_names(&f.plugins));
    let hand = hand_enabled(cfg, &cat);
    let mut plugins: BTreeMap<(String, bool), Vec<String>> = BTreeMap::new();
    let mut required: BTreeMap<bool, Vec<String>> = BTreeMap::new();
    let user_path = extras_path(&serde_json::json!({}));
    let path = extras_path(cfg);
    let user = read_extras_at(&user_path, cfg).map(|f| f.plugins).unwrap_or_default();
    let project =
        if path != user_path { read_extras_at(&path, cfg).map(|f| f.plugins).unwrap_or_default() } else { Vec::new() };
    // A transitive plugin can be required by both scopes, so carry every originating scope.
    let mut scopes: BTreeMap<String, BTreeSet<bool>> = BTreeMap::new();
    let mut pending = Vec::new();
    for (selections, project_scope) in [(&user, false), (&project, true)] {
        for selection in selections {
            if f.plugins.contains(selection) && scopes.entry(selection.name.clone()).or_default().insert(project_scope)
            {
                pending.push((selection.name.clone(), project_scope));
            }
        }
    }
    while let Some((name, project_scope)) = pending.pop() {
        if let Some(entry) = cat.iter().find(|entry| entry.name == name) {
            for dependency in &entry.manifest.plugins {
                if scopes.entry(dependency.clone()).or_default().insert(project_scope) {
                    pending.push((dependency.clone(), project_scope));
                }
            }
        }
    }
    for e in enabled.iter().filter(|e| !hand.contains(*e)) {
        match cat.iter().find(|c| &c.name == e) {
            Some(c) => {
                for project_scope in scopes.get(e).into_iter().flatten() {
                    plugins.entry((c.repo.clone(), *project_scope)).or_default().push(c.plugin());
                    let needed = required.entry(*project_scope).or_default();
                    for r in &c.manifest.deps.required {
                        if !needed.contains(r) {
                            needed.push(r.clone());
                        }
                    }
                }
            }
            None => {
                if let Some(selection) = f.plugins.iter().find(|selection| selection.name == *e) {
                    notices.push(format!("plugin {e} source {} is unavailable", selection.source));
                }
            }
        }
    }
    let mut out: Vec<PackageSpec> = required
        .iter()
        .flat_map(|(project_scope, names)| {
            names.iter().filter_map(|r| catalog_spec(r)).map(|mut spec| {
                spec.scope = if *project_scope { "project" } else { "user" };
                spec.project_lock = *project_scope;
                spec
            })
        })
        .collect();
    for ((repo, project_scope), list) in plugins {
        if let Some(mut s) = catalog_spec(&repo) {
            s.scope = if project_scope { "project" } else { "user" };
            s.project_lock = project_scope;
            s.only = Some(list);
            s.dependencies = required.get(&project_scope).cloned().unwrap_or_default();
            out.push(s);
        }
    }
    (out, notices)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_git_recommendation_requires_a_real_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert!(!has_root_marker(&root, &[".git".into()]));
        std::fs::write(root.join("custom.marker"), "").unwrap();
        assert!(has_root_marker(&root, &["custom.marker".into()]));
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join(".git/refs")).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert!(has_root_marker(&root, &[".git".into()]));
    }
}
