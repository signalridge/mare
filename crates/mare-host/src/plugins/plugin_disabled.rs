//! Plugins the user turned off: `mare.plugin(name, { enabled = false })`
//! closes the name slot — an explicit choice only: `plugins.preset = "minimal"` keeps every
//! built-in provider. `mare.plugins.status(name).disabled` tells the llm plugin that a model's
//! provider is missing because its plugin is disabled — the session's model then stays selected
//! and its prompts are refused, as for a failed provider, instead of silently switching to
//! another one.

/// Whether the user turned plugin `name` off in `config` (`plugins.<name>.enabled = false`).
pub(crate) fn disabled(config: &serde_json::Value, name: &str) -> bool {
    config
        .get(mare_shared::PLUGINS_KEY)
        .and_then(|p| p.get(name))
        .and_then(|p| p.get("enabled"))
        .and_then(|e| e.as_bool())
        == Some(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_an_explicit_disable_counts() {
        let cfg = json!({ "plugins": { "provider-x": { "enabled": false } } });
        assert!(disabled(&cfg, "provider-x"));
        assert!(!disabled(&cfg, "provider-y"));
        let cfg = json!({ "model": "faux/echo", "plugins": { "preset": "minimal" } });
        assert!(!disabled(&cfg, "provider-anthropic"), "the minimal preset keeps every provider");
    }
}
