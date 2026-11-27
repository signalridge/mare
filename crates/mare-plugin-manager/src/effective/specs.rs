//! Enabled and disabled packages in load order (dependencies first).

use crate::extras::enabled::implicit_extra_specs;
use crate::spec::{declarations, is_disabled, parse_any, parse_spec};
use crate::*;

/// Dependency problems found among enabled packages.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyIssues {
    /// Enabled package name -> missing (undeclared or disabled) dependency names.
    pub missing: BTreeMap<String, Vec<String>>,
    /// Dependency cycles as chains, e.g. `["a", "b", "a"]`.
    pub cycles: Vec<Vec<String>>,
}

/// Enabled packages in load order: dependencies (declared, or from a package's
/// `mare-plugin.toml`) before the packages that need them, otherwise declaration order (lazy.nvim;
/// no priority table). Includes the packages enabled extras need.
pub fn specs(cfg: &serde_json::Value) -> Vec<PackageSpec> {
    order_with_issues(raw_specs(cfg)).0
}

/// One working tree cannot safely represent different revisions for different declarations.
pub(crate) fn validate_checkout_constraints(specs: &[PackageSpec]) -> Result<()> {
    let mut checkouts: BTreeMap<PathBuf, &PackageSpec> = BTreeMap::new();
    for spec in specs.iter().filter(|spec| spec.dir.is_none()) {
        let dir = checkout_dir(&spec.url);
        if let Some(other) = checkouts.get(&dir) {
            if spec.target != other.target || spec.pin != other.pin {
                bail!(
                    "conflicting checkout constraints for {} and {} at {} ({:?} vs {:?})",
                    other.name,
                    spec.name,
                    dir.display(),
                    other.target,
                    spec.target
                );
            }
        } else {
            checkouts.insert(dir, spec);
        }
    }
    Ok(())
}

/// Missing dependencies and dependency cycles among enabled packages.
pub fn dependency_issues(cfg: &serde_json::Value) -> DependencyIssues {
    order_with_issues(raw_specs(cfg)).1
}

fn raw_specs(cfg: &serde_json::Value) -> Vec<PackageSpec> {
    let o = PkgOptions::from_config(cfg);
    let decl = declarations(cfg);
    // an explicit `enabled = false` in init.lua wins over anything extras would add
    let disabled: BTreeSet<String> = decl
        .iter()
        .filter(|t| is_disabled(t))
        .filter_map(|t| t.get("name").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let mut list: Vec<PackageSpec> = decl.iter().filter_map(|t| parse_spec(t, &o)).collect();
    let mut seen = BTreeSet::new();
    list.retain(|s| seen.insert((s.name.clone(), s.project_lock)));
    for extra in implicit_extra_specs(cfg).0 {
        if disabled.contains(&extra.name) {
            continue;
        }
        match list.iter_mut().find(|s| s.name == extra.name && s.project_lock == extra.project_lock) {
            // declared in init.lua too: keep that declaration, enable the extras' plugins as well
            Some(s) => {
                if let (Some(only), Some(add)) = (s.only.as_mut(), extra.only) {
                    for p in add {
                        if !only.contains(&p) {
                            only.push(p);
                        }
                    }
                }
            }
            None => list.push(extra),
        }
    }
    // a package's own manifest may name dependencies too: they order the load
    for s in &mut list {
        let root = s.root();
        if let Ok(Some(m)) = manifest(&root) {
            for d in m.dependencies {
                if !s.dependencies.contains(&d) {
                    s.dependencies.push(d);
                }
            }
        }
    }
    list
}

/// Declared packages that are disabled (`enabled = false`, or only needed by a disabled one):
/// not installed or loaded, but listed and kept in the lock.
pub fn disabled_specs(cfg: &serde_json::Value) -> Vec<PackageSpec> {
    let o = PkgOptions::from_config(cfg);
    declarations(cfg).iter().filter(|t| is_disabled(t)).filter_map(|t| parse_any(t, &o)).collect()
}

/// Every declared package name, enabled or not (lock entries of these are kept).
pub(crate) fn declared_names(cfg: &serde_json::Value) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = specs(cfg).into_iter().filter(|s| s.dir.is_none()).map(|s| s.name).collect();
    out.extend(disabled_specs(cfg).into_iter().filter(|s| s.dir.is_none()).map(|s| s.name));
    out
}

/// Topological order over `dependencies`, stable for everything else, recording missing
/// dependencies and cycles instead of silently ignoring them.
fn order_with_issues(list: Vec<PackageSpec>) -> (Vec<PackageSpec>, DependencyIssues) {
    let mut out: Vec<PackageSpec> = vec![];
    let mut issues = DependencyIssues::default();
    let mut state = vec![0u8; list.len()]; // 0 = unvisited, 1 = visiting, 2 = done
    let mut stack: Vec<usize> = vec![];
    fn visit(
        i: usize,
        list: &[PackageSpec],
        state: &mut [u8],
        stack: &mut Vec<usize>,
        out: &mut Vec<PackageSpec>,
        issues: &mut DependencyIssues,
    ) {
        if state[i] == 2 {
            return;
        }
        state[i] = 1;
        stack.push(i);
        for d in &list[i].dependencies {
            if let Some(j) = list.iter().position(|s| &s.name == d) {
                if state[j] == 1 {
                    if let Some(pos) = stack.iter().position(|&k| k == j) {
                        let mut cyc: Vec<String> = stack[pos..].iter().map(|&k| list[k].name.clone()).collect();
                        cyc.push(list[j].name.clone());
                        if !issues.cycles.contains(&cyc) {
                            issues.cycles.push(cyc);
                        }
                    }
                } else if state[j] == 0 {
                    visit(j, list, state, stack, out, issues);
                }
            } else {
                let entry = issues.missing.entry(list[i].name.clone()).or_default();
                if !entry.contains(d) {
                    entry.push(d.clone());
                }
            }
        }
        stack.pop();
        state[i] = 2;
        out.push(list[i].clone());
    }
    for i in 0..list.len() {
        visit(i, &list, &mut state, &mut stack, &mut out, &mut issues);
    }
    (out, issues)
}
