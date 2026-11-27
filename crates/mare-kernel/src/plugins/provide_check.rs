//! Static single-slot check: before the tree is built, two enabled definitions
//! that are responsible for the same service ([`PluginDef::services`]: `provide` plus what an
//! `override = true` definition took over) are a conflict. The later one in load order is refused
//! (its row is disabled, `rows.rs`) and both files are named, instead of waiting for the runtime
//! `service "x" has been registered`, which stays as the backstop. Same-name replacement keeps its
//! meaning (`overrides.rs`): it merges definitions before this check ever sees them.

use crate::PluginDef;

/// The later definition `refused` provides `service`, which `kept` (earlier in load order) provides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvideConflict {
    pub service: String,
    pub refused: String,
    pub refused_file: String,
    pub kept: String,
    pub kept_file: String,
}

impl ProvideConflict {
    /// The problem shown in `mare doctor` / `/plugins`: both plugins and both files.
    pub fn message(&self) -> String {
        format!(
            "plugin {} ({}) not loaded: service {} is already provided by plugin {} ({})",
            self.refused, self.refused_file, self.service, self.kept, self.kept_file
        )
    }

    /// The short reason kept on the refused row (`--dump`).
    pub fn reason(&self) -> String {
        format!("provides {} already provided by {} ({})", self.service, self.kept, self.kept_file)
    }
}

/// Where a definition came from, for the messages.
fn file(d: &PluginDef) -> String {
    match &d.source.path {
        Some(p) => p.display().to_string(),
        None => format!("<{}>", d.source.origin.as_str()),
    }
}

/// Check `defs` (merged, in load order). A refused definition is out: it claims none of its
/// services, so a third definition providing the same name conflicts with the first, not with it.
pub(crate) fn check(defs: &[PluginDef]) -> Vec<ProvideConflict> {
    check_by(defs.iter().map(|d| (d.name.as_str(), file(d), d.services())))
}

fn check_by<'a>(defs: impl Iterator<Item = (&'a str, String, Vec<String>)>) -> Vec<ProvideConflict> {
    let mut owner: std::collections::HashMap<String, (&'a str, String)> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for (name, path, services) in defs {
        let clash = services.iter().find_map(|s| owner.get(s).map(|o| (s, o)));
        if let Some((service, (kept, kept_file))) = clash {
            out.push(ProvideConflict {
                service: service.clone(),
                refused: name.to_string(),
                refused_file: path,
                kept: (*kept).to_string(),
                kept_file: kept_file.clone(),
            });
            continue;
        }
        for s in services {
            owner.insert(s, (name, path.clone()));
        }
    }
    out
}

/// A definition on a cycle of required injects: `plugin`
/// injects a service whose provider, following injects, leads back to it (a definition that
/// injects its own service included). Cordis would leave every member PENDING for good, so the
/// host refuses them statically, as it refuses a second provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InjectCycle {
    pub plugin: String,
    pub file: String,
    /// the cycle as plugin names, starting and ending with `plugin`
    pub path: Vec<String>,
}

impl InjectCycle {
    pub fn message(&self) -> String {
        format!("plugin {} ({}) not loaded: dependency cycle {}", self.plugin, self.file, self.path.join(" -> "))
    }

    pub fn reason(&self) -> String {
        format!("dependency cycle {}", self.path.join(" -> "))
    }
}

/// The definitions of `defs` that sit on a cycle of required injects (each inject goes to the
/// definition providing that service; a service nobody provides is no edge).
pub(crate) fn cycles(defs: &[&PluginDef]) -> Vec<InjectCycle> {
    cycles_by(defs.iter().map(|d| (d.name.as_str(), file(d), d.services(), d.inject.clone())))
}

fn cycles_by<'a>(defs: impl Iterator<Item = (&'a str, String, Vec<String>, Vec<String>)>) -> Vec<InjectCycle> {
    let defs: Vec<_> = defs.collect();
    let mut provider: std::collections::HashMap<&str, usize> = std::collections::HashMap::with_capacity(defs.len());
    for (idx, (_, _, services, _)) in defs.iter().enumerate() {
        for s in services {
            provider.entry(s.as_str()).or_insert(idx);
        }
    }
    let edges: Vec<Vec<usize>> = defs
        .iter()
        .map(|(_, _, _, inject)| inject.iter().filter_map(|s| provider.get(s.as_str()).copied()).collect())
        .collect();
    let mut out = Vec::new();
    let mut stack: Vec<(usize, usize)> = Vec::new();
    let mut seen = vec![false; defs.len()];
    for start in 0..defs.len() {
        if edges[start].is_empty() {
            continue;
        }
        // a path from `start` back to itself (depth-first, each node once per search)
        stack.clear();
        stack.push((start, 0));
        seen.fill(false);
        let mut found = None;
        while let Some(&(node, next)) = stack.last() {
            if next >= edges[node].len() {
                stack.pop();
                continue;
            }
            stack.last_mut().expect("non-empty").1 += 1;
            let to = edges[node][next];
            if to == start {
                found = Some(stack.iter().map(|(n, _)| *n).collect::<Vec<_>>());
                break;
            }
            if !seen[to] {
                seen[to] = true;
                stack.push((to, 0));
            }
        }
        if let Some(path) = found {
            let mut names: Vec<String> = path.iter().map(|&n| defs[n].0.to_string()).collect();
            names.push(defs[start].0.to_string());
            out.push(InjectCycle { plugin: defs[start].0.to_string(), file: defs[start].1.clone(), path: names });
        }
    }
    out
}

/// Every definition the host refuses before the tree is built: a second provider of a service,
/// then (among the rest) the members of an inject cycle. `(name, problem message, short reason)`.
pub(crate) fn refusals(defs: &[PluginDef]) -> Vec<(String, String, String)> {
    let conflicts = check(defs);
    let refused_names: std::collections::HashSet<&str> = conflicts.iter().map(|c| c.refused.as_str()).collect();
    let mut out: Vec<(String, String, String)> =
        conflicts.iter().map(|c| (c.refused.clone(), c.message(), c.reason())).collect();
    let rest: Vec<&PluginDef> = defs.iter().filter(|d| !refused_names.contains(d.name.as_str())).collect();
    out.extend(cycles(&rest).into_iter().map(|c| (c.plugin.clone(), c.message(), c.reason())));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs(list: &[(&'static str, &'static str, &[&str])]) -> Vec<(&'static str, String, Vec<String>)> {
        list.iter().map(|(n, f, s)| (*n, f.to_string(), s.iter().map(|x| x.to_string()).collect())).collect()
    }

    #[test]
    fn no_conflict_when_every_service_has_one_provider() {
        let d = defs(&[("a", "a.lua", &["x"]), ("b", "b.lua", &["y", "z"]), ("c", "c.lua", &[])]);
        assert!(check_by(d.into_iter()).is_empty());
    }

    #[test]
    fn the_later_provider_is_refused_and_both_files_are_named() {
        let d = defs(&[("builtin-x", "<builtin>", &["x"]), ("user-x", "/u/plugins/x/init.lua", &["w", "x"])]);
        let c = check_by(d.into_iter());
        assert_eq!(
            c,
            vec![ProvideConflict {
                service: "x".into(),
                refused: "user-x".into(),
                refused_file: "/u/plugins/x/init.lua".into(),
                kept: "builtin-x".into(),
                kept_file: "<builtin>".into(),
            }]
        );
        assert_eq!(
            c[0].message(),
            "plugin user-x (/u/plugins/x/init.lua) not loaded: service x is already provided by plugin builtin-x (<builtin>)"
        );
    }

    #[test]
    fn a_refused_definition_claims_none_of_its_services() {
        // b is refused for x, so its y stays free and c's y is fine; c's x conflicts with a
        let d = defs(&[("a", "a", &["x"]), ("b", "b", &["x", "y"]), ("c", "c", &["y"]), ("d", "d", &["x"])]);
        let c = check_by(d.into_iter());
        let pairs: Vec<(String, String, String)> = c.into_iter().map(|c| (c.refused, c.service, c.kept)).collect();
        assert_eq!(pairs, vec![("b".into(), "x".into(), "a".into()), ("d".into(), "x".into(), "a".into())]);
    }

    #[test]
    fn plugins_on_an_inject_cycle_are_found_with_their_path() {
        let d = |n: &'static str, provide: &[&str], inject: &[&str]| {
            (
                n,
                format!("<{n}>"),
                provide.iter().map(|s| s.to_string()).collect(),
                inject.iter().map(|s| s.to_string()).collect(),
            )
        };
        let found = cycles_by(
            vec![d("a", &["x"], &["y"]), d("b", &["y"], &["x"]), d("c", &["z"], &["x"]), d("self", &["s"], &["s"])]
                .into_iter(),
        );
        let names: Vec<_> = found.iter().map(|c| (c.plugin.as_str(), c.path.join(">"))).collect();
        assert_eq!(names, [("a", "a>b>a".to_string()), ("b", "b>a>b".to_string()), ("self", "self>self".to_string())]);
        assert!(cycles_by(vec![d("a", &["x"], &["missing"]), d("b", &["y"], &["x"])].into_iter()).is_empty());
    }
}
