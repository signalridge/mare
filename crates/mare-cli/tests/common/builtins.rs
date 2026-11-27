//! Opt in to built-in plugins for tests that need more than the minimal preset.

use super::HostBuilder;
use serde_json::{Map, Value};

impl HostBuilder {
    /// Enable the named plugins without replacing their other configuration.
    pub fn builtins(mut self, names: &[&str]) -> Self {
        if names.is_empty() {
            return self;
        }
        let config = self.opts.config.as_object_mut().expect("test host config must be an object");
        let plugins = config
            .entry("plugins")
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .expect("test host plugins config must be an object");
        for name in names {
            let plugin = plugins
                .entry(*name)
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .expect("test host plugin config must be an object");
            plugin.insert("enabled".to_string(), Value::Bool(true));
        }
        self
    }

    /// Remove the preset while keeping all explicit plugin settings.
    pub fn all_builtins(mut self) -> Self {
        let config = self.opts.config.as_object_mut().expect("test host config must be an object");
        if let Some(plugins) = config.get_mut("plugins") {
            plugins.as_object_mut().expect("test host plugins config must be an object").remove("preset");
        }
        self
    }
}
