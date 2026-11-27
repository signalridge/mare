//! Configuration is Lua: `~/.config/mare/init.lua`, then a trusted project's
//! `.mare/init.lua`. Both run in a short-lived VM with the full `mare` API; the config calls
//! (`mare.setup`, `mare.plugin`, `mare.plugins.add`, plus whatever plugins declare in `config_api`,
//! record plain data, which becomes the JSON config handed to the host and `mare plugin`.
//!
//! Runtime choices (`/model`, `/effort`, …) are persisted to `~/.local/state/mare/state.json`
//! and used as defaults on the next start; values set explicitly in init.lua win.

pub mod options_gen;
pub mod state;
pub mod trust;
pub mod versioned_json;

pub use options_gen::CORE_OPTIONS;

/// `mare-config`-owned persistence artifacts.
pub const ARTIFACTS: &[mare_shared::artifact::Artifact] = &[mare_shared::artifact::Artifact {
    id: mare_shared::artifact::ArtifactId::Trust,
    root: mare_shared::artifact::Root::State,
    pattern: "trust.json",
    sidecars: &["trust.json.lock"],
    owner: "mare-config::trust",
    format: mare_shared::artifact::Format::JsonDoc,
    version: Some(trust::VERSION as u32),
    write: mare_shared::artifact::WriteKind::Atomic,
    lock: mare_shared::artifact::LockKind::FlockTimeoutMs(5000),
    corrupt: mare_shared::artifact::CorruptPolicy::Refuse,
    mode: mare_shared::paths::WriteMode::Private,
    bound: mare_shared::artifact::Bound::Files(1),
    content: mare_shared::artifact::ContentClass::Plain,
}];

use std::path::PathBuf;

use serde_json::{Map, Value};

#[derive(Clone, Debug)]
pub struct Config {
    pub value: Value,
    /// config warnings (unknown options, wrong types) with did-you-mean hints
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self { value: Value::Object(Map::new()), warnings: vec![] }
    }
}

/// Install the `config_api` functions declared by plugin definitions:
/// `config_api = { ["tool.command"] = function(C, def) ... end }` becomes `mare.tool.command(def)`,
/// writing into the config table `C`. Later definitions win on name clashes.
pub const PLUGIN_CONFIG_API: &str = include_str!("../preset/plugin_config_api.lua");

/// Config API installed into every VM (config VM and host VM). `C` is the table that collects
/// settings (in the host VM it is the live `mare.config`). Option registry (`mare.o`,
/// Neovim `vim.o` style) with sources (`/config`, Neovim `:verbose set`), did-you-mean warnings,
/// lazy.nvim-style deep merge for plugin opts and `mare.on`.
pub const CONFIG_API: &str = include_str!("../preset/config_api.lua");

/// UI layout validation submodule invoked directly by [`CONFIG_API`].
pub const CONFIG_API_UI_LAYOUT: &str = include_str!("../preset/config_api_ui_layout.lua");

/// Option and plugin validation submodule invoked directly by [`CONFIG_API`].
pub const CONFIG_API_VALIDATE: &str = include_str!("../preset/config_api_validate.lua");

/// Package declaration and fragment-finalization API (`mare.plugins.add`,
/// `core.config.state.finalize_packages`), loaded alongside [`CONFIG_API`].
pub const CONFIG_PACKAGES: &str = include_str!("../preset/config_packages.lua");

/// Shared Lua helpers for config redaction (`mare.config_redact`, `mare.config_is_sensitive`)
/// and metadata description (`mare.config_describe`), shared by `/config` and `ctx.options:get`.
pub const CONFIG_INTROSPECT: &str = include_str!("../preset/config_introspect.lua");

/// The runtime config write transaction (`core.config.state.config_write`,
/// `core.config.state.config_clear_runtime`): loaded with the runtime surface, after startup.
pub const CONFIG_TXN: &str = include_str!("../preset/config_txn.lua");

/// Environment layer: `MARE_<OPTION>` for every core option (value parsed as JSON, else string:
/// `MARE_REDUCE_MOTION=true`, `MARE_PERF='{"highlight_cache":64}'`, `MARE_BORDER=single`).
/// `MARE_EFFORT` is an invocation-only model parameter override. A
/// value that looks like JSON but doesn't parse is skipped (see [`env_warnings`]).
pub fn env_overrides() -> Vec<(String, Value)> {
    CORE_OPTIONS
        .iter()
        .copied()
        .filter_map(|name| {
            let v = std::env::var(format!("MARE_{}", name.to_uppercase())).ok()?;
            Some((name.to_string(), parse_value(&v).ok()?))
        })
        .collect()
}

/// Syntax problems in the `MARE_<OPTION>` layer (values that look like JSON but do not parse).
/// Option names and types use the shared Lua registry checker during evaluation.
/// Redacted via `mare_shared::redact::redact_str` so malformed credential values never leak.
pub fn env_warnings() -> Vec<String> {
    CORE_OPTIONS
        .iter()
        .copied()
        .filter_map(|name| {
            let var = format!("MARE_{}", name.to_uppercase());
            let v = std::env::var(&var).ok()?;
            match parse_value(&v) {
                Err(e) => Some(mare_shared::redact::redact_str(&format!("{var}: {e} (ignored)"))),
                Ok(_) => None,
            }
        })
        .collect()
}

/// An override value: JSON when it parses, else the plain string. Something that is clearly
/// meant as JSON (starts with `{`, `[` or `"`) but doesn't parse is an error, not a string.
pub fn parse_value(v: &str) -> Result<Value, String> {
    match serde_json::from_str(v) {
        Ok(j) => Ok(j),
        Err(e) if v.trim_start().starts_with(['{', '[', '"']) => {
            let safe = mare_shared::redact::redact_str(v);
            Err(format!("invalid JSON value {safe:?}: {e}"))
        }
        Err(_) => Ok(Value::String(v.to_string())),
    }
}

pub fn user_init() -> PathBuf {
    mare_shared::paths::config_dir().join("init.lua")
}

fn merge_defaults_by_leaf(target: &mut Value, defaults: Value, prefix: &str, taken: &mut Vec<String>) {
    let (Some(dst), Value::Object(src)) = (target.as_object_mut(), defaults) else { return };
    for (key, val) in src {
        let path = if prefix.is_empty() { key.clone() } else { format!("{prefix}.{key}") };
        match (dst.get_mut(&key), val) {
            (Some(existing), Value::Object(child)) if existing.is_object() => {
                if !child.is_empty() {
                    merge_defaults_by_leaf(existing, Value::Object(child), &path, taken);
                }
            }
            (Some(_), _) => {
                // Explicit higher-layer value wins.
            }
            (None, Value::Object(child)) if !child.is_empty() => {
                let mut slot = Value::Object(Map::new());
                merge_defaults_by_leaf(&mut slot, Value::Object(child), &path, taken);
                if slot.as_object().is_some_and(|m| !m.is_empty()) {
                    dst.insert(key, slot);
                }
            }
            (None, leaf) => {
                taken.push(path);
                dst.insert(key, leaf);
            }
        }
    }
}

impl Config {
    /// Fill unset leaves from a validated `state.json` object (explicit init.lua values win).
    /// Returns the dotted leaf keys taken from state.json (for `/config` sources).
    pub fn apply_state_defaults_from(&mut self, defaults: Value) -> Vec<String> {
        if !self.value.is_object() {
            self.value = Value::Object(Map::new());
        }
        let mut taken = vec![];
        merge_defaults_by_leaf(&mut self.value, defaults, "", &mut taken);
        taken
    }

    /// Fill unset leaves from state.json (runtime choices persisted by plugins, e.g. `/model`).
    /// Generic: whatever keys plugins stored there; explicit init.lua values win.
    /// Returns the dotted leaf keys taken from state.json (for `/config` sources).
    pub fn apply_state_defaults(&mut self) -> Result<Vec<String>, String> {
        let Some(state) = read_state()? else { return Ok(vec![]) };
        Ok(self.apply_state_defaults_from(state))
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.value.get(key).and_then(|v| v.as_str())
    }

    /// Set a dotted key like `plugins.tools.edit_mode`.
    pub fn set_dotted(&mut self, key: &str, value: Value) {
        let parts: Vec<&str> = key.split('.').collect();
        let mut cur = &mut self.value;
        for p in &parts[..parts.len() - 1] {
            if !cur.get(*p).is_some_and(|v| v.is_object()) {
                cur[*p] = Value::Object(Map::new());
            }
            cur = cur.get_mut(*p).unwrap();
        }
        cur[parts[parts.len() - 1]] = value;
    }
}

/// Read supported state.json choices, rejecting malformed or unsupported versions.
pub fn read_state() -> Result<Option<Value>, String> {
    state::read(&mare_shared::paths::state_file()).map(|v| v.map(Value::Object))
}

/// Parse a `KEY=VALUE` CLI override (`--set`); the value is JSON when it parses, else a string
/// ([`parse_value`]). A missing `=`, an empty key or broken JSON is an error.
pub fn parse_override(s: &str) -> Result<(String, Value), String> {
    let (k, v) = s.split_once('=').ok_or_else(|| format!("expected KEY=VALUE, got {s:?}"))?;
    let k = k.trim();
    if k.is_empty() || k.split('.').any(str::is_empty) {
        return Err(format!("invalid key {k:?} in {s:?}"));
    }
    Ok((k.to_string(), parse_value(v)?))
}

pub fn merge_json(base: &mut Value, other: Value) {
    match (base, other) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(bv) if bv.is_object() && v.is_object() => merge_json(bv, v),
                    _ => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (b, o) => *b = o,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_process_uses_nextest_private_home() {
        let home = std::path::PathBuf::from(std::env::var_os("MARE_HOME").expect("private MARE_HOME"));
        assert_eq!(std::env::var("MARE_NEXTEST_HOME_DEPTH").as_deref(), Ok("1"));
        assert!(mare_shared::paths::state_file().starts_with(&home));
        assert!(mare_shared::paths::trust_file().starts_with(home));
    }

    /// An unsupported version is rejected without modifying the stored bytes.
    #[test]
    fn state_defaults_reject_invalid_versions() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let path = home.path().join("state/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let cases = [
            r#"{"model":"must-not-load"}"#,
            r#"{"version":"1","model":"must-not-load"}"#,
            r#"{"version":1.0,"model":"must-not-load"}"#,
            r#"{"version":2,"model":"must-not-load"}"#,
        ];
        for raw in cases {
            std::fs::write(&path, raw).unwrap();
            assert!(read_state().is_err(), "read_state accepted invalid state version: {raw}");
            assert!(Config::default().apply_state_defaults().is_err(), "state defaults accepted {raw}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);
        }
        std::fs::write(&path, r#"{"version":1,"model":"faux/echo"}"#).unwrap();
        let mut cfg = Config::default();
        assert_eq!(cfg.apply_state_defaults().unwrap(), vec!["model"]);
        assert_eq!(cfg.value["model"], "faux/echo");
        assert!(cfg.value.get("version").is_none());
    }

    #[test]
    fn state_defaults_merge_by_leaf_without_clobbering_sibling_keys() {
        let home = tempfile::tempdir().unwrap();
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var("MARE_HOME", home.path()) };
        let path = home.path().join("state/state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"version":1,"model":"from-state","plugins":{"tools":{"bash_timeout_ms":9000,"max_lines":123}}}"#,
        )
        .unwrap();
        let mut cfg = Config {
            value: serde_json::json!({
                "plugins": { "tools": { "bash_timeout_ms": 5000 } }
            }),
            warnings: vec![],
        };
        let mut taken = cfg.apply_state_defaults().unwrap();
        taken.sort();
        assert_eq!(taken, vec!["model", "plugins.tools.max_lines"]);
        assert_eq!(cfg.value["model"], "from-state");
        assert_eq!(cfg.value["plugins"]["tools"]["bash_timeout_ms"], 5000);
        assert_eq!(cfg.value["plugins"]["tools"]["max_lines"], 123);
    }

    /// The `MARE_<OPTION>` layer covers every option in the `mare.options` registry.
    #[test]
    fn env_layer_covers_every_registered_option() {
        let start = CONFIG_API.find("mare.options = mare.options").expect("registry");
        let open = start + CONFIG_API[start..].find('{').expect("registry open");
        let body = &CONFIG_API[open..];
        let body = &body[..body.find("\n}\n").expect("registry end")];
        let mut registered: Vec<&str> = body
            .lines()
            .skip(1)
            .filter_map(|l| l.strip_prefix("\t\t")?.split_once(" = {").map(|(k, _)| k))
            .filter(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
            .collect();
        registered.sort_unstable();
        let mut listed = CORE_OPTIONS.to_vec();
        listed.sort_unstable();
        assert_eq!(listed, registered);
    }

    /// `MARE_SESSION_*` is what the host injects into child processes:
    /// no core option may make it part of the config env layer.
    #[test]
    fn no_option_reads_the_injected_session_variables() {
        assert!(!CORE_OPTIONS.iter().any(|o| o.starts_with("session")), "{CORE_OPTIONS:?}");
    }

    #[test]
    fn env_values_parse_as_json_or_string() {
        assert_eq!(parse_value("true"), Ok(Value::Bool(true)));
        assert_eq!(parse_value("single"), Ok(Value::String("single".into())));
        assert_eq!(parse_value(r#"{"highlight_cache":64}"#).unwrap()["highlight_cache"], 64);
        assert!(parse_value("{broken").is_err());
    }

    #[test]
    fn cli_override_rejects_empty_segments_and_replaces_non_object_values() {
        assert!(parse_override("=1").is_err());
        assert!(parse_override("a..b=1").is_err());
        assert!(parse_override(".a=1").is_err());

        let mut cfg = Config { value: serde_json::json!("not-an-object"), warnings: vec![] };
        let taken = cfg.apply_state_defaults_from(serde_json::json!({"model": "x"}));
        assert_eq!(taken, vec!["model"]);
        assert_eq!(cfg.value["model"], "x");

        let mut base = serde_json::json!({"ui": {"theme": "dark"}});
        merge_json(&mut base, serde_json::json!("plain"));
        assert_eq!(base, serde_json::json!("plain"));
    }
}
