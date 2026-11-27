//! Runtime directory: read-only copies of the builtin
//! plugin sources and type definitions under `~/.local/share/mare/runtime/`, refreshed when the
//! mare version changes. mare itself still loads builtins from the binary; this directory is for
//! reading, copying into `~/.config/mare/plugins/` to override, and for LuaLS.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::loader::BUILTIN_NAMES;
#[cfg(debug_assertions)]
use crate::loader::{builtin_module_texts, builtin_text};
#[cfg(debug_assertions)]
use crate::{keymap_lua, types_texts, ui_components, ui_prelude, ui_shell};

pub fn dir() -> PathBuf {
    mare_shared::paths::data_dir().join("runtime")
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

/// Extract builtins + types if the recorded version differs. Cheap when up to date.
pub fn ensure() -> Result<PathBuf> {
    let dir = dir();
    let stamp = dir.join("VERSION");
    let version = env!("CARGO_PKG_VERSION");
    if std::fs::read_to_string(&stamp).ok().as_deref().map(str::trim) == Some(version) {
        return Ok(dir);
    }
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
        for (key, text) in &archive {
            if let Some(rel) = key.strip_prefix("types/") {
                write_ro(&dir.join("types").join(rel), text)?;
                type_keys.push(rel);
            }
        }
        prune_types_by_name(&dir.join("types"), &type_keys);
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
        prune_types_by_name(&dir.join("types"), &type_keys);
        write_ro(&dir.join("NEWS.md"), &crate::news_text())?;
    }
    std::fs::write(&stamp, version)?;
    Ok(dir)
}

/// Remove `*.lua` under the runtime's `types/` that this version no longer ships (a namespace
/// file dropped or renamed), so LuaLS doesn't keep reading stale definitions. Best effort.
fn prune_types_by_name(types_dir: &Path, keep: &[&str]) {
    let mut stack = vec![types_dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "lua") {
                let rel = p.strip_prefix(types_dir).map(|r| r.to_string_lossy().replace('\\', "/")).unwrap_or_default();
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
