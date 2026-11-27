//! A config file (init.lua, the trusted project's `.mare/init.lua`) that failed at startup, for
//! the frontend's startup dialog: where it failed and the error's first line, and the retry after the user edited it.

use std::path::PathBuf;
use std::sync::Arc;

use crate::Host;
use crate::plugins::plugin_failures::{PluginFailure, error_location};

/// A failed config file: the file, the line the error names in it (if it names one in this
/// file), and the error's first line (a place in another file, a required module, stays in it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub message: String,
}

impl ConfigError {
    fn of(f: &PluginFailure) -> Option<Self> {
        let path = f.path.clone()?;
        let (src, line, msg) = error_location(&f.error);
        let own = src.is_some_and(|s| path.to_string_lossy().ends_with(s.trim_start_matches("...")));
        let (line, message) = match (src, line) {
            (Some(_), Some(l)) if own => (Some(l), msg.to_string()),
            (Some(s), Some(l)) => (None, format!("{s}:{l}: {msg}")),
            _ => (None, msg.to_string()),
        };
        Some(Self { path, line, message })
    }
}

impl Host {
    /// The config files that failed (at startup, or at the last [`Self::retry_config`]).
    pub fn config_errors(&self) -> Vec<ConfigError> {
        self.plugin_failures.borrow().iter().filter(|f| f.phase == "config").filter_map(ConfigError::of).collect()
    }

    /// Run the config again after the user edited it: a full reload (init.lua, packages,
    /// plugins), so a fixed file leaves nothing blocked. When it still fails, its new error
    /// replaces the recorded ones (requests stay blocked). Returns what fails now.
    pub async fn retry_config(self: &Arc<Self>) -> Vec<ConfigError> {
        if let Err(e) = self.reload_all().await {
            let error = format!("{e:#}");
            let mut failures = self.plugin_failures.borrow_mut();
            let configs: Vec<PathBuf> =
                failures.iter().filter(|f| f.phase == "config").filter_map(|f| f.path.clone()).collect();
            // the file the error names (Lua shortens long chunk names to "...tail"), else the
            // first one that failed before
            let (src, _, _) = error_location(&error);
            let path = configs
                .iter()
                .find(|p| src.is_some_and(|s| p.to_string_lossy().ends_with(s.trim_start_matches("..."))))
                .or(configs.first())
                .cloned()
                .unwrap_or_else(mare_config::user_init);
            failures.retain(|f| f.phase != "config");
            failures.push(PluginFailure::config_file(path, error));
        }
        self.config_errors()
    }
}
