//! Extension CLI flag definitions, validation, and runtime store.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use mlua::{Lua, Value};
use serde_json::{Map, Value as J, json};

/// Supported CLI flag types (`"boolean" | "string"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlagType {
    Boolean,
    String,
}

impl FlagType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::String => "string",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "boolean" => Ok(Self::Boolean),
            "string" => Ok(Self::String),
            other => Err(format!("invalid flag type `{other}` (expected \"boolean\" or \"string\")")),
        }
    }
}

/// Typed CLI flag value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlagValue {
    Boolean(bool),
    String(String),
}

impl FlagValue {
    pub fn flag_type(&self) -> FlagType {
        match self {
            Self::Boolean(_) => FlagType::Boolean,
            Self::String(_) => FlagType::String,
        }
    }

    pub fn to_json(&self) -> J {
        match self {
            Self::Boolean(b) => J::Bool(*b),
            Self::String(s) => J::String(s.clone()),
        }
    }

    pub fn from_json(v: &J, expected: FlagType) -> Result<Self, String> {
        match (expected, v) {
            (FlagType::Boolean, J::Bool(b)) => Ok(Self::Boolean(*b)),
            (FlagType::String, J::String(s)) => Ok(Self::String(s.clone())),
            (t, other) => Err(format!("expected {} value, got {}", t.as_str(), json_kind(other))),
        }
    }

    pub fn from_lua(v: Value, expected: FlagType) -> Result<Self, String> {
        match (expected, v) {
            (FlagType::Boolean, Value::Boolean(b)) => Ok(Self::Boolean(b)),
            (FlagType::String, Value::String(s)) => {
                let s = s.to_str().map_err(|e| e.to_string())?;
                Ok(Self::String(s.to_string()))
            }
            (t, other) => Err(format!("expected {} value, got {}", t.as_str(), other.type_name())),
        }
    }

    pub fn to_lua(&self, lua: &Lua) -> mlua::Result<Value> {
        match self {
            Self::Boolean(b) => Ok(Value::Boolean(*b)),
            Self::String(s) => Ok(Value::String(lua.create_string(s)?)),
        }
    }
}

fn json_kind(v: &J) -> &'static str {
    match v {
        J::Null => "null",
        J::Bool(_) => "boolean",
        J::Number(_) => "number",
        J::String(_) => "string",
        J::Array(_) => "array",
        J::Object(_) => "object",
    }
}

/// Registered CLI flag metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagSpec {
    pub name: String,
    pub flag_type: FlagType,
    pub description: Option<String>,
    pub default: Option<FlagValue>,
    pub source: Option<String>,
}

impl FlagSpec {
    pub fn validate(&self) -> Result<(), String> {
        validate_name(&self.name)?;
        if let Some(def) = &self.default
            && def.flag_type() != self.flag_type
        {
            return Err(format!(
                "default value has type {}, expected {}",
                def.flag_type().as_str(),
                self.flag_type.as_str()
            ));
        }
        Ok(())
    }
}

/// Flag entry with resolved effective value (`cli_value` -> `default` -> `None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagInfo {
    pub spec: FlagSpec,
    pub value: Option<FlagValue>,
}

impl FlagInfo {
    pub fn to_json(&self) -> J {
        let mut obj = Map::new();
        obj.insert("name".into(), J::String(self.spec.name.clone()));
        obj.insert("type".into(), J::String(self.spec.flag_type.as_str().into()));
        if let Some(d) = &self.spec.description {
            obj.insert("description".into(), J::String(d.clone()));
        }
        if let Some(def) = &self.spec.default {
            obj.insert("default".into(), def.to_json());
        }
        if let Some(val) = &self.value {
            obj.insert("value".into(), val.to_json());
        }
        if let Some(src) = &self.spec.source {
            obj.insert("source".into(), J::String(src.clone()));
        }
        J::Object(obj)
    }
}

/// Validate a CLI flag name (non-empty, no leading `-`, ASCII alphanumeric / `-` / `_`).
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("flag name cannot be empty".into());
    }
    if name.starts_with('-') {
        return Err(format!("flag name `{name}` must not start with `-`"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(format!("flag name `{name}` may only contain ASCII letters, digits, `-`, and `_`"));
    }
    Ok(())
}

#[derive(Default)]
struct Inner {
    specs: BTreeMap<String, FlagSpec>,
    cli_values: BTreeMap<String, FlagValue>,
}

/// Shared flag registry stored on `Lua` app data and shared with `Kernel` / `Host`.
#[derive(Clone, Default)]
pub struct FlagStore {
    inner: Arc<Mutex<Inner>>,
}

impl FlagStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retrieve the store from `lua` app data, creating one if absent.
    pub fn of(lua: &Lua) -> Self {
        if let Some(store) = lua.app_data_ref::<Self>() {
            return store.clone();
        }
        let store = Self::new();
        lua.set_app_data(store.clone());
        store
    }

    /// Seed CLI-parsed flag values from `mare.args.flags` (`{ [name] = bool|string }`).
    pub fn seed_cli_json(&self, flags: Option<&J>) {
        let Some(obj) = flags.and_then(|v| v.as_object()) else { return };
        let mut inner = self.inner.lock().expect("flag store poisoned");
        inner.cli_values.clear();
        for (k, v) in obj {
            match v {
                J::Bool(b) => {
                    inner.cli_values.insert(k.clone(), FlagValue::Boolean(*b));
                }
                J::String(s) => {
                    inner.cli_values.insert(k.clone(), FlagValue::String(s.clone()));
                }
                _ => {}
            }
        }
    }

    /// Register a flag definition. Returns an optional conflict warning if another source
    /// previously registered the same flag name.
    pub fn register(&self, spec: FlagSpec) -> Result<Option<String>, String> {
        spec.validate()?;
        let mut inner = self.inner.lock().expect("flag store poisoned");
        let warning = inner.specs.get(&spec.name).and_then(|prev| {
            (prev.source != spec.source).then(|| {
                format!(
                    "flag `{}` overridden (previous source: {}, new source: {})",
                    spec.name,
                    prev.source.as_deref().unwrap_or("unknown"),
                    spec.source.as_deref().unwrap_or("unknown")
                )
            })
        });
        inner.specs.insert(spec.name.clone(), spec);
        Ok(warning)
    }

    /// Remove all registrations whose `source` matches `exact`.
    pub fn clear_source(&self, exact: &str) {
        let mut inner = self.inner.lock().expect("flag store poisoned");
        inner.specs.retain(|_, s| s.source.as_deref() != Some(exact));
    }

    /// Remove all registrations whose `source` starts with `prefix`.
    pub fn clear_source_prefix(&self, prefix: &str) {
        let mut inner = self.inner.lock().expect("flag store poisoned");
        inner.specs.retain(|_, s| !s.source.as_deref().is_some_and(|src| src.starts_with(prefix)));
    }

    /// Resolve a flag's value (`cli_value` -> `default` -> `None`), or `None` if unregistered.
    pub fn get(&self, name: &str) -> Option<FlagValue> {
        let inner = self.inner.lock().expect("flag store poisoned");
        let spec = inner.specs.get(name)?;
        if let Some(cli) = inner.cli_values.get(name)
            && cli.flag_type() == spec.flag_type
        {
            return Some(cli.clone());
        }
        spec.default.clone()
    }

    /// Registered flag specs in sorted name order.
    pub fn specs(&self) -> Vec<FlagSpec> {
        self.inner.lock().expect("flag store poisoned").specs.values().cloned().collect()
    }

    /// Snapshot registered specs for atomic config-reload rollback.
    pub fn snapshot_specs(&self) -> BTreeMap<String, FlagSpec> {
        self.inner.lock().expect("flag store poisoned").specs.clone()
    }

    /// Restore registered specs from a snapshot on rollback.
    pub fn restore_specs(&self, snapshot: BTreeMap<String, FlagSpec>) {
        self.inner.lock().expect("flag store poisoned").specs = snapshot;
    }

    /// Registered flags with resolved values in sorted name order.
    pub fn list(&self) -> Vec<FlagInfo> {
        let inner = self.inner.lock().expect("flag store poisoned");
        inner
            .specs
            .values()
            .map(|spec| {
                let value = inner
                    .cli_values
                    .get(&spec.name)
                    .filter(|v| v.flag_type() == spec.flag_type)
                    .cloned()
                    .or_else(|| spec.default.clone());
                FlagInfo { spec: spec.clone(), value }
            })
            .collect()
    }

    /// JSON representation of `list()` (`array` of flag info objects).
    pub fn list_json(&self) -> J {
        json!(self.list().into_iter().map(|i| i.to_json()).collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_cli_over_default_and_ignores_unregistered() {
        let store = FlagStore::new();
        assert_eq!(store.get("missing"), None);

        store.seed_cli_json(Some(&json!({ "verbose": true, "mode": "fast" })));
        // Still unregistered -> None
        assert_eq!(store.get("verbose"), None);

        store
            .register(FlagSpec {
                name: "verbose".into(),
                flag_type: FlagType::Boolean,
                description: Some("verbose logging".into()),
                default: Some(FlagValue::Boolean(false)),
                source: Some("init.lua".into()),
            })
            .unwrap();
        assert_eq!(store.get("verbose"), Some(FlagValue::Boolean(true)));

        store
            .register(FlagSpec {
                name: "retries".into(),
                flag_type: FlagType::String,
                description: None,
                default: Some(FlagValue::String("3".into())),
                source: Some("plugin:demo".into()),
            })
            .unwrap();
        assert_eq!(store.get("retries"), Some(FlagValue::String("3".into())));
    }

    #[test]
    fn rejects_invalid_names_and_mismatched_defaults() {
        let store = FlagStore::new();
        assert!(
            store
                .register(FlagSpec {
                    name: "--bad".into(),
                    flag_type: FlagType::Boolean,
                    description: None,
                    default: None,
                    source: None,
                })
                .is_err()
        );
        assert!(
            store
                .register(FlagSpec {
                    name: "bad=name".into(),
                    flag_type: FlagType::Boolean,
                    description: None,
                    default: None,
                    source: None,
                })
                .is_err()
        );
        assert!(
            store
                .register(FlagSpec {
                    name: "mismatch".into(),
                    flag_type: FlagType::Boolean,
                    description: None,
                    default: Some(FlagValue::String("nope".into())),
                    source: None,
                })
                .is_err()
        );
    }
}
