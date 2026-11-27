//! Package contents besides `plugins/`: skills, prompts, themes, types.

use std::path::Component;

use crate::repo::hash::walk_files;
use crate::*;

/// What a package may ship besides `plugins/`: `skills/`, `prompts/`, `themes/`
/// and `types/` (LuaLS definitions).
pub const CONTENT_KINDS: &[&str] = &["skills", "prompts", "themes", "types"];

/// `(package, <root>)` of the installed, loadable packages in load order.
pub fn package_dirs(cfg: &serde_json::Value) -> Vec<(String, PathBuf)> {
    installed(cfg).into_iter().map(|p| (p.name, p.dir)).collect()
}

/// `(package, <dir>)` when `kind` is `Some` (`CONTENT_KINDS`), or package roots when `None`, in
/// load order.
pub fn resource_dirs(cfg: &serde_json::Value, kind: Option<&str>) -> Result<Vec<(String, PathBuf)>> {
    match kind {
        Some(k) => {
            if !CONTENT_KINDS.contains(&k) {
                bail!("unknown kind {k} ({})", CONTENT_KINDS.join(" | "));
            }
            Ok(content_dirs(cfg, k))
        }
        None => Ok(package_dirs(cfg)),
    }
}

pub(crate) fn safe_pkg_subpath(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() {
        return None;
    }
    let mut out = root.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    if std::fs::symlink_metadata(&out).is_ok() {
        let canon_root = dunce::canonicalize(root).ok()?;
        let canon_out = dunce::canonicalize(&out).ok()?;
        if !canon_out.starts_with(&canon_root) {
            return None;
        }
    }
    Some(out)
}

fn manifest_kind_paths<'a>(m: &'a Manifest, kind: &str) -> &'a [String] {
    match kind {
        "skills" => &m.skills,
        "prompts" => &m.prompts,
        "themes" => &m.themes,
        _ => &[],
    }
}

/// `(package, <dir>)` of the installed, loadable packages that have that resource directory, in
/// load order. Uses custom paths declared in `mare-plugin.toml` when present and non-empty, or
/// falls back to `<root>/<kind>`. `only` narrows a package's plugins, not its other contents.
pub fn content_dirs(cfg: &serde_json::Value, kind: &str) -> Vec<(String, PathBuf)> {
    if !CONTENT_KINDS.contains(&kind) {
        return vec![];
    }
    let mut out = Vec::new();
    for p in installed(cfg) {
        let declared = manifest(&p.dir).ok().flatten();
        let custom = declared.as_ref().map(|m| manifest_kind_paths(m, kind)).unwrap_or(&[]);
        if !custom.is_empty() {
            let mut seen = BTreeSet::new();
            for rel in custom {
                if let Some(d) = safe_pkg_subpath(&p.dir, rel)
                    && d.is_dir()
                    && seen.insert(d.clone())
                {
                    out.push((p.name.clone(), d));
                }
            }
        } else if let Some(d) = safe_pkg_subpath(&p.dir, kind)
            && d.is_dir()
        {
            out.push((p.name, d));
        }
    }
    out
}

/// Where the packages' `types/` are mirrored for LuaLS (`.luarc.json` `workspace.library` lists
/// it next to the runtime types): one directory per package.
pub fn types_dir() -> PathBuf {
    mare_shared::paths::data_dir().join("package-types")
}

/// Mirror every installed package's `types/*.lua` into [`types_dir`]; files already identical
/// are left alone and those of packages no longer installed are removed.
pub fn refresh_types(cfg: &serde_json::Value) -> Result<()> {
    let root = types_dir();
    let mut want: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    for (name, dir) in content_dirs(cfg, "types") {
        let Ok(canon_dir) = dunce::canonicalize(&dir) else { continue };
        let seg = source::safe_segment(&name);
        for (rel, f) in walk_files(&dir) {
            if rel.ends_with(".lua")
                && std::fs::symlink_metadata(&f).is_ok_and(|m| m.file_type().is_file())
                && dunce::canonicalize(&f).is_ok_and(|cf| cf.starts_with(&canon_dir))
            {
                want.insert(root.join(&seg).join(&rel), std::fs::read(&f)?);
            }
        }
    }
    if want.is_empty() && !root.exists() {
        return Ok(());
    }
    for (_, f) in walk_files(&root) {
        if !want.contains_key(&f) {
            let _ = std::fs::remove_file(&f);
            let mut p = f.parent().map(Path::to_path_buf);
            while let Some(d) = p.filter(|d| *d != root && std::fs::remove_dir(d).is_ok()) {
                p = d.parent().map(Path::to_path_buf);
            }
        }
    }
    for (p, bytes) in want {
        if std::fs::read(&p).ok().as_deref() == Some(bytes.as_slice()) {
            continue;
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, bytes).with_context(|| format!("writing {}", p.display()))?;
    }
    Ok(())
}
