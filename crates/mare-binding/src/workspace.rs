//! `mare.workspace`: the session's workspace directories (default: cwd). `@` completion,
//! project trust and tool path hints use it; `/add-dir` and ACP `additionalDirectories` extend it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use mlua::{Lua, Table};

#[derive(Clone)]
struct WorkspaceState(Arc<RwLock<Vec<PathBuf>>>);

fn canon(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn overlaps(a: &Path, b: &Path) -> bool {
    crate::realpath::path_within(a, b) || crate::realpath::path_within(b, a)
}

/// Validate a single workspace directory candidate: must be an existing physical absolute directory
/// (no symlinks anywhere), and must not overlap host-protected or project-protected paths.
pub fn validate_single_dir(cwd: &Path, raw: &Path) -> Result<PathBuf, String> {
    let physical = crate::ossandbox::validate_root(raw)?;
    for entry in crate::ossandbox::protected::host_protected() {
        let hp = crate::realpath::resolve(&entry.path).map_err(|e| {
            format!(
                "{}: cannot resolve protected path {}: {e}",
                crate::ossandbox::SANDBOX_INVALID_ROOT,
                entry.path.display()
            )
        })?;
        if overlaps(&physical, &hp) {
            return Err(format!(
                "{}: additional workspace directory {} overlaps host-protected path {}",
                crate::ossandbox::SANDBOX_INVALID_ROOT,
                physical.display(),
                hp.display()
            ));
        }
    }
    for pp in crate::ossandbox::root_protected_subpaths(&canon(cwd)).map_err(|e| {
        format!(
            "{}: cannot resolve protected paths under {}: {e}",
            crate::ossandbox::SANDBOX_INVALID_ROOT,
            cwd.display()
        )
    })? {
        if crate::realpath::path_within(&physical, &pp) {
            return Err(format!(
                "{}: additional workspace directory {} is inside protected path {}",
                crate::ossandbox::SANDBOX_INVALID_ROOT,
                physical.display(),
                pp.display()
            ));
        }
    }
    Ok(physical)
}

/// Validate ACP `additionalDirectories` and CLI `--add-dir` upfront before starting a host: all
/// entries must be valid existing physical directories, deduplicated against `cwd` and repeated
/// entries, and none may overlap protected subpaths (`.git`, `.mare`, `.mcp.json`) of `cwd` or any
/// other candidate root in the batch.
pub fn validate_additional_dirs(cwd: &Path, raw_dirs: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let cwd_canon = canon(cwd);
    let mut out = Vec::new();
    for raw in raw_dirs {
        if raw.to_str().is_none() {
            return Err(format!(
                "{}: additional workspace directory path is not valid UTF-8: {}",
                crate::ossandbox::SANDBOX_INVALID_ROOT,
                raw.to_string_lossy()
            ));
        }
        let physical = validate_single_dir(cwd, raw)?;
        if physical != cwd_canon && !out.contains(&physical) {
            out.push(physical);
        }
    }
    let mut all_roots = Vec::with_capacity(1 + out.len());
    all_roots.push(cwd_canon);
    all_roots.extend(out.iter().cloned());
    for root in &all_roots {
        let protected = crate::ossandbox::root_protected_subpaths(root).map_err(|e| {
            format!(
                "{}: cannot resolve protected paths under {}: {e}",
                crate::ossandbox::SANDBOX_INVALID_ROOT,
                root.display()
            )
        })?;
        for candidate in &out {
            if let Some(pp) = protected.iter().find(|pp| crate::realpath::path_within(candidate, pp)) {
                return Err(format!(
                    "{}: additional workspace directory {} is inside protected path {}",
                    crate::ossandbox::SANDBOX_INVALID_ROOT,
                    candidate.display(),
                    pp.display()
                ));
            }
        }
    }
    Ok(out)
}

/// Validate one additional workspace directory against static rules AND the Lua VM's active
/// sandbox (`deny_read`, `protected_subpaths`, and inherited ceiling when `workspace-write`).
pub fn validate_workspace_root_in_lua(lua: &Lua, cwd: &Path, raw: &str) -> Result<PathBuf, String> {
    let physical = validate_single_dir(cwd, Path::new(raw))?;
    if let Some(policy) = crate::ossandbox::current(lua) {
        for denied in &policy.deny_read {
            if overlaps(&physical, &denied.target) {
                return Err(format!(
                    "{}: additional workspace directory {} conflicts with deny_read path {}",
                    crate::ossandbox::SANDBOX_INVALID_ROOT,
                    physical.display(),
                    denied.target.display()
                ));
            }
        }
        for pp in policy.protected_subpaths() {
            if crate::realpath::path_within(&physical, &pp) {
                return Err(format!(
                    "{}: additional workspace directory {} is inside protected path {}",
                    crate::ossandbox::SANDBOX_INVALID_ROOT,
                    physical.display(),
                    pp.display()
                ));
            }
        }
        if policy.level == crate::ossandbox::Level::WorkspaceWrite
            && let Some(ceil) = crate::ossandbox::ceiling(lua)
        {
            let covered = match ceil.level {
                crate::ossandbox::Level::FullAccess => true,
                crate::ossandbox::Level::WorkspaceWrite => {
                    ceil.writable.iter().any(|w| crate::realpath::path_within(&physical, w))
                }
                crate::ossandbox::Level::ReadOnly => false,
            };
            if !covered {
                return Err(format!(
                    "{}: additional workspace directory {} exceeds the inherited sandbox ceiling",
                    crate::ossandbox::SANDBOX_INVALID_ROOT,
                    physical.display()
                ));
            }
        }
    }
    Ok(physical)
}

fn dirs_checked(lua: &Lua) -> mlua::Result<Vec<PathBuf>> {
    let policy = crate::exec_ctx::active(lua)?.map(|a| a.policy);
    let Some(state) = lua.app_data_ref::<WorkspaceState>() else {
        return Ok(Vec::new());
    };
    Ok(state.0.read().unwrap().iter().filter(|p| policy.as_ref().is_none_or(|pcy| pcy.can_read(p))).cloned().collect())
}

/// Current readable workspace directories for this Lua VM (`[cwd, ...extras]`).
pub fn dirs(lua: &Lua) -> Vec<PathBuf> {
    dirs_checked(lua).unwrap_or_default()
}

fn check_protected_overlap(candidate: &Path, existing: &Path) -> Result<(), String> {
    if candidate == existing {
        return Ok(());
    }
    let existing_protected = crate::ossandbox::root_protected_subpaths(existing).map_err(|e| e.to_string())?;
    if existing_protected.iter().any(|pp| crate::realpath::path_within(candidate, pp)) {
        return Err(format!(
            "{}: additional workspace directory {} is inside protected path",
            crate::ossandbox::SANDBOX_INVALID_ROOT,
            candidate.display()
        ));
    }
    let candidate_protected = crate::ossandbox::root_protected_subpaths(candidate).map_err(|e| e.to_string())?;
    if candidate_protected.iter().any(|pp| crate::realpath::path_within(existing, pp)) {
        return Err(format!(
            "{}: workspace directory {} overlaps protected path under {}",
            crate::ossandbox::SANDBOX_INVALID_ROOT,
            existing.display(),
            candidate.display()
        ));
    }
    Ok(())
}

pub fn install(lua: &Lua, mare: &Table, cwd: PathBuf) -> mlua::Result<()> {
    let dirs = Arc::new(RwLock::new(vec![canon(&cwd)]));
    lua.set_app_data(WorkspaceState(dirs.clone()));
    let resolve = {
        let cwd = cwd.clone();
        move |p: &str| -> PathBuf {
            let p = mare_shared::paths::expand(p);
            if p.is_absolute() { p } else { cwd.join(p) }
        }
    };
    let t = lua.create_table()?;
    t.set(
        "dirs",
        lua.create_function(|lua, ()| {
            Ok(dirs_checked(lua)?.into_iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>())
        })?,
    )?;
    let check_cwd = cwd.clone();
    t.set(
        "check",
        lua.create_function(move |lua, (path, peers): (String, Option<Vec<String>>)| {
            crate::exec_ctx::check(lua, "read", &path)?;
            let physical = match validate_workspace_root_in_lua(lua, &check_cwd, &path) {
                Ok(p) => p,
                Err(e) => return Ok((None, Some(e))),
            };
            if let Some(peers) = peers {
                for peer in peers {
                    let peer_canon = canon(Path::new(&peer));
                    if let Err(e) = check_protected_overlap(&physical, &peer_canon) {
                        return Ok((None, Some(e)));
                    }
                }
            }
            Ok((Some(physical.to_string_lossy().into_owned()), None))
        })?,
    )?;
    let (d, set_cwd) = (dirs.clone(), cwd.clone());
    t.set(
        "set_dirs",
        lua.create_function(move |lua, extra: Vec<String>| {
            let cwd_canon = d.read().unwrap()[0].clone();
            let mut validated = Vec::with_capacity(extra.len());
            for raw in extra {
                crate::exec_ctx::check(lua, "read", &raw)?;
                let physical = validate_workspace_root_in_lua(lua, &set_cwd, &raw).map_err(mlua::Error::runtime)?;
                if physical != cwd_canon && !validated.contains(&physical) {
                    validated.push(physical);
                }
            }
            let mut all_roots = Vec::with_capacity(1 + validated.len());
            all_roots.push(cwd_canon.clone());
            all_roots.extend(validated.iter().cloned());
            for (i, candidate) in all_roots.iter().enumerate() {
                for (j, other) in all_roots.iter().enumerate() {
                    if i != j {
                        check_protected_overlap(candidate, other).map_err(mlua::Error::runtime)?;
                    }
                }
            }
            {
                let mut v = d.write().unwrap();
                v.truncate(1);
                v.extend(validated);
            }
            Ok(dirs_checked(lua)?.into_iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>())
        })?,
    )?;
    let (d, r, add_cwd) = (dirs.clone(), resolve.clone(), cwd.clone());
    t.set(
        "add",
        lua.create_function(move |lua, p: String| {
            let p = r(&p);
            crate::exec_ctx::check(lua, "read", &p.to_string_lossy())?;
            let p_str = p.to_str().ok_or_else(|| mlua::Error::runtime("path is not valid UTF-8"))?;
            let physical = validate_workspace_root_in_lua(lua, &add_cwd, p_str).map_err(mlua::Error::runtime)?;
            let mut v = d.write().unwrap();
            for existing in v.iter() {
                check_protected_overlap(&physical, existing).map_err(mlua::Error::runtime)?;
            }
            if !v.contains(&physical) {
                v.push(physical);
            }
            Ok(true)
        })?,
    )?;
    let (d, r) = (dirs.clone(), resolve.clone());
    t.set(
        "remove",
        lua.create_function(move |lua, p: String| {
            let p = r(&p);
            crate::exec_ctx::check(lua, "read", &p.to_string_lossy())?;
            let p = canon(&p);
            let mut v = d.write().unwrap();
            // the first entry (cwd) is permanent
            if v.first() != Some(&p) {
                v.retain(|x| x != &p);
            }
            Ok(())
        })?,
    )?;
    let (d, r) = (dirs, resolve);
    t.set(
        "contains",
        lua.create_function(move |lua, p: String| {
            let p = r(&p);
            crate::exec_ctx::check(lua, "read", &p.to_string_lossy())?;
            let p = canon(&p);
            Ok(d.read().unwrap().iter().any(|dir| crate::realpath::path_within(&p, dir)))
        })?,
    )?;
    mare.set("workspace", t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_dirs_is_all_or_nothing_and_enforces_protected_subpaths() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        let extra1 = tmp.path().join("extra1");
        let repo = tmp.path().join("repo");
        let repo_git = repo.join(".git");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&extra1).unwrap();
        std::fs::create_dir_all(&repo_git).unwrap();
        let cwd_c = dunce::canonicalize(&cwd).unwrap();
        let extra1_c = dunce::canonicalize(&extra1).unwrap();
        let repo_c = dunce::canonicalize(&repo).unwrap();
        let repo_git_c = dunce::canonicalize(&repo_git).unwrap();

        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare, cwd_c.clone()).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let dirs: Vec<String> =
            lua.load("return mare.workspace.set_dirs({ ... })").call(extra1_c.to_string_lossy().into_owned()).unwrap();
        assert_eq!(dirs.len(), 2);
        assert_eq!(PathBuf::from(&dirs[1]), extra1_c);

        // Pair containing a protected subpath (.git) under a sibling root must fail all-or-nothing,
        // leaving existing dirs untouched.
        let ok: bool = lua
            .load(
                r#"
                local a, b = ...
                local ok = pcall(mare.workspace.set_dirs, { a, b })
                return ok
                "#,
            )
            .call((repo_c.to_string_lossy().into_owned(), repo_git_c.to_string_lossy().into_owned()))
            .unwrap();
        assert!(!ok, "set_dirs must reject protected subpath pair");
        let after: Vec<String> = lua.load("return mare.workspace.dirs()").eval().unwrap();
        assert_eq!(after, dirs, "failed set_dirs must not mutate existing dirs");

        // mare.workspace.check returns (canon, nil) for valid dirs and (nil, err) for invalid ones.
        let (canon_ok, err_none): (Option<String>, Option<String>) =
            lua.load("return mare.workspace.check(...)").call(repo_c.to_string_lossy().into_owned()).unwrap();
        assert_eq!(canon_ok.map(PathBuf::from), Some(repo_c));
        assert!(err_none.is_none());

        let (bad_none, bad_err): (Option<String>, Option<String>) =
            lua.load("return mare.workspace.check('relative/path')").eval().unwrap();
        assert!(bad_none.is_none());
        assert!(bad_err.is_some());
    }
}
