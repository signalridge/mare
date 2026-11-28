//! Runtime directory: read-only copies of the builtin
//! plugin sources and type definitions under `~/.local/share/mare/runtime/`, refreshed when the
//! mare version changes. mare itself still loads builtins from the binary; this directory is for
//! reading, copying into `~/.config/mare/plugins/` to override, and for LuaLS.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};

use crate::loader::BUILTIN_NAMES;
#[cfg(debug_assertions)]
use crate::loader::{builtin_module_texts, builtin_text};
#[cfg(debug_assertions)]
use crate::{keymap_lua, skill_texts, types_texts, ui_components, ui_prelude, ui_shell};

const LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const LOCK_POLL: Duration = Duration::from_millis(10);
const FINGERPRINT_FILE: &str = "FINGERPRINT";
const RUNTIME_FINGERPRINT: &str = env!("MARE_RUNTIME_FINGERPRINT");

pub fn dir() -> PathBuf {
    mare_shared::paths::data_dir().join("runtime")
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.with_file_name("runtime.lock")
}

fn version_matches(stamp: &Path, expected: &str) -> bool {
    std::fs::read_to_string(stamp).ok().as_deref().map(str::trim) == Some(expected)
}

fn stamp_matches(dir: &Path, version: &str, fingerprint: &str) -> bool {
    version_matches(&dir.join("VERSION"), version) && version_matches(&dir.join(FINGERPRINT_FILE), fingerprint)
}

/// Fast lock-free check for whether the runtime directory already matches this binary's version
/// and bundled asset fingerprint.
pub fn is_up_to_date() -> bool {
    stamp_matches(&dir(), env!("CARGO_PKG_VERSION"), RUNTIME_FINGERPRINT)
}

fn lock_runtime(dir: &Path) -> Result<mare_shared::private::LockGuard> {
    let lp = lock_path(dir);
    mare_shared::private::lock_file_blocking(&lp, Some(LOCK_TIMEOUT), LOCK_POLL, true, None).map_err(|err| match err {
        mare_shared::private::LockError::TimedOut => {
            anyhow::anyhow!("timed out waiting for runtime lock {}", lp.display())
        }
        mare_shared::private::LockError::Cancelled => {
            anyhow::anyhow!("cancelled waiting for runtime lock {}", lp.display())
        }
        mare_shared::private::LockError::Io(e) => {
            anyhow::Error::new(e).context(format!("runtime lock {}", lp.display()))
        }
    })
}

fn write_ro(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    // previous copy is read-only: make it writable before replacing
    #[cfg(unix)]
    if path.exists() {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644));
    }
    std::fs::write(path, content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))?;
    }
    Ok(())
}

/// Extract builtins + types + bundled skills if the recorded version or asset fingerprint differs.
/// Cheap (lock-free) when already up to date.
pub fn ensure() -> Result<PathBuf> {
    let dir = dir();
    ensure_with(&dir, env!("CARGO_PKG_VERSION"), RUNTIME_FINGERPRINT, extract_all_files)?;
    Ok(dir)
}

fn ensure_with(dir: &Path, version: &str, fingerprint: &str, extract: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    if stamp_matches(dir, version, fingerprint) {
        return Ok(());
    }
    let _guard = lock_runtime(dir)?;
    if stamp_matches(dir, version, fingerprint) {
        return Ok(());
    }
    extract(dir)?;
    std::fs::create_dir_all(dir)?;
    let version_stamp = dir.join("VERSION");
    mare_shared::paths::write_atomic(&version_stamp, version.as_bytes(), mare_shared::paths::WriteMode::Shared)
        .with_context(|| format!("write runtime version stamp {}", version_stamp.display()))?;
    // Publish the asset fingerprint LAST so lock-free readers on a same-version upgrade never
    // observe an up-to-date stamp before all extracted files and VERSION are in place.
    let fp_stamp = dir.join(FINGERPRINT_FILE);
    mare_shared::paths::write_atomic(&fp_stamp, fingerprint.as_bytes(), mare_shared::paths::WriteMode::Shared)
        .with_context(|| format!("write runtime asset fingerprint {}", fp_stamp.display()))?;
    Ok(())
}

fn extract_all_files(dir: &Path) -> Result<()> {
    #[cfg(not(debug_assertions))]
    {
        // Single-pass decompression; `archive` is dropped at the end of this block so the
        // decompressed sources do not stay resident in heap memory.
        let archive = crate::runtime_archive::extract_all();
        for name in BUILTIN_NAMES {
            if let Some(code) = archive.get(&format!("plugins/{name}.lua")) {
                write_ro(&dir.join("plugins").join(format!("{name}.lua")), code)?;
            }
        }
        for (key, code) in &archive {
            if let Some(rel) = key.strip_prefix("plugins/lua/") {
                write_ro(&dir.join("plugins/lua").join(rel), code)?;
            }
        }
        for file in ["ui_components.lua", "ui_shell.lua", "keymap.lua", "ui_prelude.lua"] {
            if let Some(code) = archive.get(file) {
                write_ro(&dir.join("lua").join(file), code)?;
            }
        }
        let mut type_keys: Vec<&str> = Vec::new();
        let mut skill_keys: Vec<&str> = Vec::new();
        for (key, text) in &archive {
            if let Some(rel) = key.strip_prefix("types/") {
                write_ro(&dir.join("types").join(rel), text)?;
                type_keys.push(rel);
            } else if let Some(rel) = key.strip_prefix("skills/") {
                write_ro(&dir.join("skills").join(rel), text)?;
                skill_keys.push(rel);
            }
        }
        prune_dir_by_name(&dir.join("types"), &type_keys, Some("lua"));
        prune_dir_by_name(&dir.join("skills"), &skill_keys, None);
        write_ro(&dir.join("NEWS.md"), &crate::news_text())?;
    }
    #[cfg(debug_assertions)]
    {
        for name in BUILTIN_NAMES {
            if let Some(code) = builtin_text(name) {
                write_ro(&dir.join("plugins").join(format!("{name}.lua")), &code)?;
            }
        }
        for (rel, code) in builtin_module_texts() {
            write_ro(&dir.join("plugins/lua").join(rel), &code)?;
        }
        write_ro(&dir.join("lua/ui_components.lua"), &ui_components())?;
        write_ro(&dir.join("lua/ui_shell.lua"), &ui_shell())?;
        write_ro(&dir.join("lua/keymap.lua"), &keymap_lua())?;
        write_ro(&dir.join("lua/ui_prelude.lua"), &ui_prelude())?;
        let types = types_texts();
        for (rel, text) in &types {
            write_ro(&dir.join("types").join(rel), text)?;
        }
        let type_keys: Vec<&str> = types.iter().map(|(k, _)| *k).collect();
        prune_dir_by_name(&dir.join("types"), &type_keys, Some("lua"));
        let skills = skill_texts();
        for (rel, text) in &skills {
            write_ro(&dir.join("skills").join(rel), text)?;
        }
        let skill_keys: Vec<&str> = skills.iter().map(|(k, _)| k.as_str()).collect();
        prune_dir_by_name(&dir.join("skills"), &skill_keys, None);
        write_ro(&dir.join("NEWS.md"), &crate::news_text())?;
    }
    Ok(())
}

/// Remove stale extracted files under `root` that this version no longer ships (for example a
/// dropped type namespace or bundled skill), so readers don't see stale files. Best effort.
fn prune_dir_by_name(root: &Path, keep: &[&str], ext_filter: Option<&str>) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if ext_filter.is_none_or(|ext| p.extension().is_some_and(|x| x == ext)) {
                let rel = p.strip_prefix(root).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
                if !keep.iter().any(|k| *k == rel) {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }
}

const DEFAULT_INIT: &str = r##"-- mare configuration (Lua 5.5). Types: see .luarc.json next to this file.
-- Runtime changes (/model, /effort) are saved to ~/.local/state/mare/state.json, not here.

mare.setup {
  -- model = "openai/gpt-6.1-terra",
  -- effort = "high",
}

-- Override plugin defaults:
-- mare.plugin("tools", { bash_timeout_ms = 120000, edit_mode = "auto" })

-- Packages (installed with `mare plugin sync`):
-- mare.plugins.add { { "signalridge/mare-plugins", only = { "todo" } } }

-- Command tools (grep/find are not built in):
-- mare.tool.command {
--   name = "grep",
--   description = "Search file contents with ripgrep.",
--   command = { "rg", "--line-number", "--no-heading", "--max-columns=500", "{pattern}", "{path}" },
--   params = { pattern = { type = "string", required = true }, path = { type = "string", default = "." } },
-- }

-- UI: styles, components, keys
-- mare.ui.style("MareAccent", { fg = "#7aa2f7" })
"##;

fn default_init_lua() -> String {
    format!(
        "{DEFAULT_INIT}-- mare.keymap.set(\"editor\", \"<C-o>\", \"toggle-expand\") -- {} expands output; {} opens the transcript\n",
        mare_shared::bindings::key_label("ctrl+o"),
        mare_shared::bindings::key_label("ctrl+t"),
    )
}

/// `mare init`: default init.lua (never overwritten) + .luarc.json pointing at runtime/types and
/// the packages' mirrored types (rewritten only while it is still mare's own).
pub fn init() -> Result<Vec<String>> {
    let rt = ensure()?;
    let cfg = mare_shared::paths::config_dir();
    std::fs::create_dir_all(&cfg)?;
    let mut out = vec![];
    let init = cfg.join("init.lua");
    if init.exists() {
        out.push(format!("kept     {}", init.display()));
    } else {
        std::fs::write(&init, default_init_lua())?;
        out.push(format!("created  {}", init.display()));
    }
    let luarc = cfg.join(".luarc.json");
    let json = serde_json::json!({
        "$schema": "https://raw.githubusercontent.com/LuaLS/vscode-lua/master/setting/schema.json",
        "runtime.version": "Lua 5.5",
        // mare's own types, then the packages' `types/`
        "workspace.library": [rt.join("types").to_string_lossy(), mare_plugin_manager::types_dir().to_string_lossy()],
        "workspace.checkThirdParty": false,
        "diagnostics.globals": ["mare", "ui"],
    });
    let text = serde_json::to_string_pretty(&json)? + "\n";
    // a .luarc.json the user changed is theirs: only a missing one, or one still exactly as mare
    // last wrote it (recorded next to the state), is (re)written
    let record = mare_shared::paths::state_dir().join("luarc.generated");
    let current = std::fs::read_to_string(&luarc).ok();
    let ours = current.is_none()
        || current.as_deref() == Some(text.as_str())
        || std::fs::read_to_string(&record).ok().is_some_and(|prev| current.as_deref() == Some(prev.as_str()));
    if ours {
        std::fs::write(&luarc, &text)?;
        if let Some(d) = record.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&record, &text);
        out.push(format!("wrote    {}", luarc.display()));
    } else {
        out.push(format!("kept     {} (changed by you; delete it to get the default)", luarc.display()));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let id = COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("mare-runtime-{label}-{}-{id}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn ensure_rechecks_version_after_acquiring_lock() {
        let tmp = TestDir::new("recheck");
        let rt_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&rt_dir).unwrap();
        std::fs::write(rt_dir.join("VERSION"), "0.0.0-stale").unwrap();

        let guard = lock_runtime(&rt_dir).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_bg = Arc::clone(&calls);
        let rt_bg = rt_dir.clone();
        let handle = std::thread::spawn(move || {
            ensure_with(&rt_bg, "9.9.9-target", "fp-target", |_| {
                calls_bg.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        });

        // Wait briefly so the waiter enters lock acquisition, then complete the refresh and unlock.
        std::thread::sleep(Duration::from_millis(50));
        std::fs::write(rt_dir.join("VERSION"), "9.9.9-target").unwrap();
        std::fs::write(rt_dir.join(FINGERPRINT_FILE), "fp-target").unwrap();
        drop(guard);

        handle.join().unwrap().unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "second process must skip extraction once VERSION and FINGERPRINT match"
        );
    }

    #[test]
    fn ensure_does_not_write_version_stamp_when_extraction_fails() {
        let tmp = TestDir::new("fail");
        let rt_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&rt_dir).unwrap();
        std::fs::write(rt_dir.join("VERSION"), "0.0.0-stale").unwrap();

        let err = ensure_with(&rt_dir, "9.9.9-target", "fp-target", |_| anyhow::bail!("simulated extraction failure"))
            .unwrap_err();
        assert!(err.to_string().contains("simulated extraction failure"), "{err:#}");
        assert_eq!(
            std::fs::read_to_string(rt_dir.join("VERSION")).unwrap(),
            "0.0.0-stale",
            "VERSION must remain unchanged when extraction fails"
        );
        assert!(!rt_dir.join(FINGERPRINT_FILE).exists(), "FINGERPRINT must not be written when extraction fails");
    }

    #[test]
    fn ensure_refreshes_same_version_stale_runtime_cache() {
        let tmp = TestDir::new("same-version-stale");
        let rt_dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&rt_dir).unwrap();
        // Pre-existing same-version runtime directory written before bundled skills / fingerprint.
        std::fs::write(rt_dir.join("VERSION"), "1.0.0").unwrap();

        let calls = AtomicUsize::new(0);
        ensure_with(&rt_dir, "1.0.0", "fp-new", |d| {
            calls.fetch_add(1, Ordering::SeqCst);
            write_ro(&d.join("skills/plugin-authoring/SKILL.md"), "fresh")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "missing FINGERPRINT must trigger same-version refresh");
        assert!(rt_dir.join("skills/plugin-authoring/SKILL.md").is_file());
        assert_eq!(std::fs::read_to_string(rt_dir.join(FINGERPRINT_FILE)).unwrap().trim(), "fp-new");

        // Second call with matching VERSION + FINGERPRINT takes fast path.
        ensure_with(&rt_dir, "1.0.0", "fp-new", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Same VERSION with an updated fingerprint must refresh again.
        ensure_with(&rt_dir, "1.0.0", "fp-newer", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2, "changed FINGERPRINT must trigger same-version refresh");
        assert_eq!(std::fs::read_to_string(rt_dir.join(FINGERPRINT_FILE)).unwrap().trim(), "fp-newer");
    }
}
