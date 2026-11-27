//! Top-level aggregation of persistence artifact registries across all owner crates
//! .

use std::fmt::Write as _;
use std::path::Path;

use mare_shared::artifact::{
    Artifact, ArtifactId, Bound, ContentClass, CorruptPolicy, Format, LockKind, Root, WriteKind,
};
use mare_shared::paths::WriteMode;

/// Format version for `$XDG_STATE_HOME/mare/news.json`, re-exported from its owner (`mare-plugin`).
pub use mare_plugin::NEWS_VERSION;

/// `mare-host`-owned persistence artifacts.
pub const ARTIFACTS: &[Artifact] = &[Artifact {
    id: ArtifactId::News,
    root: Root::State,
    pattern: "news.json",
    sidecars: &["news.json.lock"],
    owner: "preset/plugins/news",
    format: Format::JsonDoc,
    version: Some(NEWS_VERSION),
    write: WriteKind::Atomic,
    lock: LockKind::FlockTimeoutMs(5000),
    corrupt: CorruptPolicy::Refuse,
    mode: WriteMode::Private,
    bound: Bound::Files(1),
    content: ContentClass::Plain,
}];

/// Every persistence artifact slice contributed by its owning crate.
const REGISTRY_SLICES: &[&[Artifact]] = &[
    mare_shared::ARTIFACTS,
    mare_store::ARTIFACTS,
    mare_config::ARTIFACTS,
    mare_plugin_manager::ARTIFACTS,
    mare_kernel::ARTIFACTS,
    mare_binding::ARTIFACTS,
    ARTIFACTS,
];

/// Aggregate every registered persistence artifact across the workspace.
pub fn all() -> Vec<&'static Artifact> {
    let mut out = Vec::new();
    for slice in REGISTRY_SLICES {
        out.extend(slice.iter());
    }
    out
}

/// Look up an artifact row by [`ArtifactId`].
pub fn get(id: ArtifactId) -> Option<&'static Artifact> {
    all().into_iter().find(|a| a.id == id)
}

/// Render the Markdown artifacts table (`render_spec_table`).
pub fn render_spec_table() -> String {
    let mut out = String::from(
        "| ID | 根 / 模式（含旁路） | 归属 | 格式 · 版本 | 写入 · 锁 | 损坏策略 | 权限 | 上界 | 内容 |\n\
         |---|---|---|---|---|---|---|---|---|\n",
    );
    for a in all() {
        let version = a.version.map_or_else(|| "None".to_string(), |v| v.to_string());
        let sidecars =
            if a.sidecars.is_empty() { String::new() } else { format!(" (+ `{}`)", a.sidecars.join("`, `")) };
        let mode = match a.mode {
            mare_shared::paths::WriteMode::Private => "Private (0600)",
            mare_shared::paths::WriteMode::Shared => "Shared (0644)",
        };
        let _ = writeln!(
            out,
            "| `{}` | {} `{}`{} | `{}` | {} · {} | {} · {} | {} | {} | {} | {} |",
            a.id.as_str(),
            a.root.as_str(),
            a.pattern,
            sidecars,
            a.owner,
            a.format.as_str(),
            version,
            a.write.as_str(),
            a.lock.display(),
            a.corrupt.as_str(),
            mode,
            a.bound.display(),
            a.content.as_str(),
        );
    }
    out
}

/// Directory closure check: verify every file left under `home/state` and `home/cache` matches
/// a registered [`Artifact`] pattern and has Unix permissions consistent with its `mode`.
pub fn assert_registered(home: &Path) -> Result<(), String> {
    let artifacts = all();
    let mut errors = Vec::new();
    for (root, sub) in [(Root::State, "state"), (Root::Cache, "cache")] {
        let base = home.join(sub);
        if !base.exists() {
            continue;
        }
        walk_root(&base, &base, root, &artifacts, &mut errors)?;
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!("assert_registered({}) found violation(s):\n{}", home.display(), errors.join("\n")))
    }
}

fn walk_root(
    base: &Path,
    dir: &Path,
    root: Root,
    artifacts: &[&'static Artifact],
    errors: &mut Vec<String>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("dir_entry {}: {e}", dir.display()))?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("metadata {}: {e}", path.display()))?;
        let rel = path.strip_prefix(base).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
        if meta.is_dir() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perm = meta.permissions().mode() & 0o777;
                if perm != 0o700 {
                    errors.push(format!("dir {}:{rel} has mode {perm:04o} (expected 0700)", root.as_str()));
                }
            }
            walk_root(base, &path, root, artifacts, errors)?;
            continue;
        }
        let matched: Vec<&'static Artifact> = artifacts.iter().copied().filter(|a| a.matches_rel(root, &rel)).collect();
        if matched.is_empty() {
            errors.push(format!("unregistered file {}:{rel}", root.as_str()));
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = meta.permissions().mode() & 0o777;
            let expected = matched[0].mode.unix_mode();
            if perm != expected {
                errors.push(format!(
                    "file {}:{rel} (artifact {}) has mode {perm:04o} (expected {expected:04o})",
                    root.as_str(),
                    matched[0].id.as_str()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_artifact_id_is_registered_once() {
        let items = all();
        assert_eq!(items.len(), 28, "update exhaustive count when adding/removing ArtifactId");
        let mut seen = HashSet::new();
        for a in items {
            assert!(seen.insert(a.id), "duplicate artifact registration for {:?}", a.id);
        }
    }

    #[cfg(unix)]
    #[test]
    fn assert_registered_reports_directory_mode_and_unregistered_file_violations() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("mare-artifacts-mode-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let state = dir.join("state");
        let sub = state.join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(sub.join("stray.txt"), "x").unwrap();

        let err = assert_registered(&dir).unwrap_err();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(err.contains("expected 0700"), "{err}");
        assert!(err.contains("unregistered file State:nested/stray.txt"), "{err}");
    }
}
