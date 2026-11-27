//! Pre-startup CLI extension flag discovery (`init.lua` `mare.flags.register` and plugin
//! `PluginDef.flags`), respecting `--clean` and project trust without executing `apply()`.

use std::path::Path;

use anyhow::Result;

use crate::config::eval::evaluate_config;
use crate::plugins::defs::wanted_defs;
use mare_kernel::Kernel;
use mare_plugin::{self as lua, Env, Mode};
use mare_shared::flags::{FlagSpec, FlagStore};
use mare_shared::ui::UiShared;

fn has_extension_sources(cwd: &Path, trusted: bool) -> bool {
    mare_config::user_init().exists()
        || mare_shared::paths::user_plugins_dir().exists()
        || mare_shared::paths::packages_dir().exists()
        || mare_plugin_manager::extras_path(&serde_json::json!({})).exists()
        || (trusted
            && mare_shared::project::resource_root(cwd).mare_dir().is_some_and(|m| {
                m.join("init.lua").exists()
                    || m.join(mare_shared::PLUGINS_KEY).exists()
                    || m.join("plugins.json").exists()
            }))
}

async fn discover_inner(cwd: &Path, trusted: bool, no_plugins: bool) -> Result<Vec<FlagSpec>> {
    let lua = lua::new_lua()?;
    let env = Env {
        cwd: cwd.to_path_buf(),
        mode: Mode::Print,
        ui: UiShared::new(),
        config: serde_json::Value::Object(Default::default()),
        args: serde_json::Value::Object(Default::default()),
    };
    let mare = lua::install(&lua, &env)?;
    mare.set("trusted", trusted)?;
    mare.set("no_user_plugins", no_plugins)?;
    let config = evaluate_config(&lua, &mare, cwd, trusted, &env.config, &[], None).await?;
    let kernel = Kernel::new(lua, config.clone())?;
    let (defs, _) = wanted_defs(&kernel, &config, cwd, trusted, no_plugins, &mut |_, e, _| {
        tracing::debug!("cli flag discovery skipped plugin definition: {e}");
    });
    kernel.set_defs(defs);
    Ok(FlagStore::of(kernel.lua()).specs())
}

/// Discover extension CLI flags before `clap` parses command-line arguments.
///
/// Untrusted projects (neither saved as trusted nor passed `--trust`) never evaluate
/// `<root>/.mare/init.lua` or `<root>/.mare/plugins/`.
pub async fn discover_cli_flags(cwd: &Path, no_plugins: bool, cli_trust: Option<bool>) -> Vec<FlagSpec> {
    if no_plugins {
        return vec![];
    }
    let trusted = match cli_trust {
        Some(t) => t,
        None => mare_config::trust::decision(cwd).unwrap_or(false),
    };
    if !has_extension_sources(cwd, trusted) {
        return vec![];
    }
    match discover_inner(cwd, trusted, no_plugins).await {
        Ok(specs) => specs,
        Err(e) => {
            tracing::debug!("cli flag discovery failed: {e:#}");
            vec![]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn discover_cli_flags_respects_no_plugins_and_handles_broken_project_init() {
        let dir = std::env::temp_dir().join(format!("mare-flags-test-{}", uuid::Uuid::new_v4()));
        let mare_dir = dir.join(".mare");
        std::fs::create_dir_all(&mare_dir).unwrap();
        std::fs::write(mare_dir.join("plugins.json"), "{}").unwrap();
        assert!(has_extension_sources(&dir, true));

        // --no-plugins skips discovery completely even when extension sources exist.
        assert!(discover_cli_flags(&dir, true, Some(true)).await.is_empty());

        // Broken init.lua in a trusted project fails discovery cleanly without panicking.
        std::fs::write(mare_dir.join("init.lua"), "error('broken init for flag discovery')").unwrap();
        assert!(discover_cli_flags(&dir, false, Some(true)).await.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
