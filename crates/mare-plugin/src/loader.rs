//! Plugin discovery. Load order (precedence order):
//! builtin (embedded) → installed packages → `~/.config/mare/plugins` → trusted `.mare/plugins`.
//! On a name clash a later source replaces an earlier one only if its definition says
//! `override = true`; otherwise it is refused.
//! A package's other directories are picked up by the plugins that own
//! them through `mare.plugins.dirs(kind)`: `skills/` and `prompts/` by context, `themes/` by
//! theme-default; `types/` is mirrored for LuaLS ([`mare_plugin_manager::refresh_types`]).

use std::path::{Path, PathBuf};

use mare_kernel::{Origin, PluginSource};

/// The built-in plugins, listed once. Release builds embed their sources and precompiled
/// bytecode (Yazi `plugin_preset!` style); debug builds read `preset/plugins/*.lua` live, so
/// editing one never recompiles anything — `/reload` picks it up.
macro_rules! builtins {
    ($($m:ident)::+) => {
        $($m)::+![
            "keymap-default",
            "theme-default",
            "llm",
            "provider-openai",
            "provider-anthropic",
            "provider-openai-compat",
            "provider-google",
            "provider-opencode-go",
            "session",
            "conversations",
            "query",
            "query-disk",
            "context",
            "tools",
            "cells",
            "tools-apply-patch",
            "proc",
            "terminal",
            "jobs",
            "fs",
            "web",
            "webserver",
            "web-fetch-http",
            "mcp",
            "agent",
            "message",
            "repeat-guard",
            "permissions",
            "approvals",
            "approval-review-model",
            "agent-approvals",
            "commands",
            "review",
            "changes",
            "plugins",
            "lua-console",
            "ui",
            "images",
            "copy",
            "usage",
            "statusline",
            "file-ref",
            "compact",
            "shell-mode",
            "status",
            "agents",
            "subagent",
            "notify",
            "profile",
            "update-check",
            "onboarding",
            "spill",
            "user-config",
        ]
    };
}

/// Names of the built-in plugins (load order).
pub const BUILTIN_NAMES: &[&str] = builtins!(mare_macro::plugin_names);

/// Built-in plugins precompiled to Lua 5.5 bytecode at build time (what release mare loads).
#[cfg(not(debug_assertions))]
pub const BUILTIN_BYTECODE: &[(&str, &[u8])] = builtins!(mare_macro::plugin_bytecode);

/// Public Lua modules declared with `---@meta <name>` in `preset/types/**/*.lua` (excluding `_`),
/// generated at build time in both debug and release builds.
pub const PUBLIC_MODULES: &[&str] = include!(concat!(env!("OUT_DIR"), "/public_modules.rs"));

/// Implementation modules of the two-layer built-ins (`preset/plugins/lua/<plugin>/<mod>.lua`):
/// `(module name, bytecode)`. Sources are stored compressed in `runtime_archive` for
/// `runtime::ensure()`. Debug builds `require` them from the source tree instead.
#[cfg(not(debug_assertions))]
pub const BUILTIN_MODULES: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/builtin_modules.rs"));

#[cfg(not(debug_assertions))]
fn module_bytecode() -> &'static [(&'static str, &'static [u8])] {
    BUILTIN_MODULES
}

/// The scripted `faux` provider: tests, debug builds and `just bench` only —
/// release builds without the `faux` feature don't ship it.
#[cfg(all(not(debug_assertions), feature = "faux"))]
const FAUX: &[(&str, &[u8])] = mare_macro::plugin_bytecode!["provider-faux"];
#[cfg(all(not(debug_assertions), not(feature = "faux")))]
const FAUX: &[(&str, &[u8])] = &[];

/// Source of a built-in plugin as text (runtime-dir extraction).
pub fn builtin_text(name: &str) -> Option<std::borrow::Cow<'static, str>> {
    #[cfg(debug_assertions)]
    {
        std::fs::read_to_string(Path::new(mare_macro::preset_dir!()).join("plugins").join(format!("{name}.lua")))
            .ok()
            .map(std::borrow::Cow::Owned)
    }
    #[cfg(not(debug_assertions))]
    {
        crate::runtime_archive::get(&format!("plugins/{name}.lua")).map(std::borrow::Cow::Owned)
    }
}

/// Every built-in implementation module as (path relative to `plugins/lua/`, source): the
/// runtime directory mirrors them next to the plugins, so a copied built-in (or
/// `MARE_DEV_PLUGINS` pointed at those files) still finds its `lua/` modules.
pub fn builtin_module_texts() -> Vec<(String, std::borrow::Cow<'static, str>)> {
    #[cfg(debug_assertions)]
    {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, std::borrow::Cow<'static, str>)>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else if p.extension().and_then(|s| s.to_str()) == Some("lua")
                    && let Ok(text) = std::fs::read_to_string(&p)
                {
                    let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                    out.push((rel, std::borrow::Cow::Owned(text)));
                }
            }
        }
        let root = Path::new(mare_macro::preset_dir!()).join("plugins").join("lua");
        let mut out = vec![];
        walk(&root, &root, &mut out);
        out
    }
    #[cfg(not(debug_assertions))]
    {
        crate::runtime_archive::list_prefix("plugins/lua/")
            .into_iter()
            .map(|(rel, src)| (rel, std::borrow::Cow::Owned(src)))
            .collect()
    }
}

/// The plugin-set presets the host provides.
pub const PLUGIN_PRESETS: &[&str] = &["minimal"];

/// The known `plugins.preset` of `cfg`, or None (unset, or an unknown name: a configuration error
/// reported by the config layer, then treated as no preset).
pub fn plugin_preset(cfg: &serde_json::Value) -> Option<&'static str> {
    let name = cfg.get("plugins")?.get("preset")?.as_str()?;
    PLUGIN_PRESETS.iter().find(|p| **p == name).copied()
}

/// The effective `plugins.<name>.enabled` of a name slot:
/// a value set in any layer wins; otherwise `plugins.preset = "minimal"` gives `false` to a slot
/// whose effective definition is a built-in (`builtin`) tagged neither `minimal` nor `provider`.
/// A provider is never decided by the preset: a session or `/model` may pick any of them (every
/// built-in provider is merged regardless of profile).
pub fn slot_enabled(name: &str, builtin: bool, tags: &[String], cfg: &serde_json::Value) -> bool {
    let explicit =
        cfg.get("plugins").and_then(|p| p.get(name)).and_then(|p| p.get("enabled")).and_then(|e| e.as_bool());
    if let Some(enabled) = explicit {
        return enabled;
    }
    match plugin_preset(cfg) {
        Some("minimal") if builtin => tags.iter().any(|t| t == "minimal" || t == "provider"),
        _ => true,
    }
}

pub fn builtin_sources() -> Vec<PluginSource> {
    mare_kernel::set_public_modules(PUBLIC_MODULES);
    // debug builds: the live source tree (no embedded copy exists)
    #[cfg(debug_assertions)]
    {
        let dir = std::env::var("MARE_DEV_PLUGINS")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(concat!(mare_macro::preset_dir!(), "/plugins")));
        let lua_dir = dir.join("lua");
        if lua_dir.is_dir() {
            mare_kernel::set_builtin_lua_dir(lua_dir);
        }
        BUILTIN_NAMES
            .iter()
            .chain(std::iter::once(&"provider-faux"))
            .map(|name| PluginSource {
                name_hint: name.to_string(),
                origin: Origin::Builtin,
                path: Some(dir.join(format!("{name}.lua"))),
                code: None,
            })
            .collect()
    }
    // release builds: embedded bytecode; MARE_DEV_PLUGINS=<dir> overrides with files on disk
    #[cfg(not(debug_assertions))]
    {
        mare_kernel::set_builtin_modules(module_bytecode());
        let dev = std::env::var("MARE_DEV_PLUGINS").ok().map(PathBuf::from);
        BUILTIN_BYTECODE
            .iter()
            .chain(FAUX.iter())
            .map(|(name, code)| {
                let path = dev.as_ref().map(|d| d.join(format!("{name}.lua"))).filter(|p| p.exists());
                PluginSource { name_hint: name.to_string(), origin: Origin::Builtin, path, code: Some(code) }
            })
            .collect()
    }
}

/// Plugins in a directory: `name.lua` files or `name/init.lua` directories.
pub fn dir_sources(dir: &Path, origin: Origin, only: Option<&[String]>) -> Vec<PluginSource> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        let name = if p.is_dir() {
            if !p.join("init.lua").exists() {
                continue;
            }
            p.file_name().and_then(|s| s.to_str()).map(str::to_string)
        } else {
            match p.extension().and_then(|s| s.to_str()) {
                Some("lua") => p.file_stem().and_then(|s| s.to_str()).map(str::to_string),
                _ => None,
            }
        };
        let Some(name) = name else { continue };
        if name.starts_with('_') || name.starts_with('.') {
            continue;
        }
        if let Some(only) = only
            && !only.iter().any(|o| o == &name)
        {
            continue;
        }
        out.push(PluginSource { name_hint: name, origin, path: Some(p), code: None });
    }
    out
}

/// Collect every plugin source in precedence order.
/// The plugins the installed, enabled packages of `cfg` ship (`<package>/plugins`, `only` applied).
pub fn package_sources(cfg: &serde_json::Value) -> Vec<PluginSource> {
    mare_plugin_manager::installed(cfg)
        .into_iter()
        .flat_map(|pkg| dir_sources(&pkg.dir.join("plugins"), Origin::Package, pkg.only.as_deref()))
        .collect()
}

pub fn discover(cfg: &serde_json::Value, cwd: &Path, project_trusted: bool, no_user: bool) -> Vec<PluginSource> {
    let mut out = builtin_sources();
    if no_user {
        return out;
    }
    out.extend(package_sources(cfg));
    out.extend(dir_sources(&mare_shared::paths::user_plugins_dir(), Origin::User, None));
    if project_trusted && let Some(mare_dir) = mare_shared::project::resource_root(cwd).mare_dir() {
        out.extend(dir_sources(&mare_dir.join("plugins"), Origin::Project, None));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_sources_skips_non_lua_hidden_and_underscore_entries() {
        let dir = std::env::temp_dir().join(format!("mare-loader-dir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("empty_sub")).unwrap();
        std::fs::write(dir.join("readme.txt"), "ignore").unwrap();
        std::fs::write(dir.join("_helper.lua"), "return {}").unwrap();
        std::fs::write(dir.join(".hidden.lua"), "return {}").unwrap();
        std::fs::write(dir.join("good.lua"), "return {}").unwrap();

        let found = dir_sources(&dir, Origin::User, None);
        let filtered = dir_sources(&dir, Origin::User, Some(&["only_missing".to_string()]));
        let _ = std::fs::remove_dir_all(&dir);
        let names: Vec<&str> = found.iter().map(|s| s.name_hint.as_str()).collect();
        assert_eq!(names, vec!["good"]);
        assert!(filtered.is_empty());

        let min_cfg = serde_json::json!({ "plugins": { "preset": "minimal", "extra": { "enabled": true } } });
        assert_eq!(plugin_preset(&min_cfg), Some("minimal"));
        assert_eq!(plugin_preset(&serde_json::json!({})), None);
        assert!(slot_enabled("extra", true, &[], &min_cfg));
        assert!(slot_enabled("core", true, &["minimal".into()], &min_cfg));
        assert!(slot_enabled("prov", true, &["provider".into()], &min_cfg));
        assert!(slot_enabled("user_pl", false, &[], &min_cfg));
        assert!(!slot_enabled("opt", true, &[], &min_cfg));
        assert!(!builtin_module_texts().is_empty());
        assert!(!discover(&min_cfg, &dir, false, true).is_empty());
    }
}
