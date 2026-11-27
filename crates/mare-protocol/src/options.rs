//! Wire types for `options.get`, `options.set`, and `config.describe`.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Read effective option values (matching `mare.o.<name>`). Sensitive fields (`config_sensitive`, `providers` keys/headers/query, credentials) are always redacted like `/config`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OptionsGetParams {
    pub session_id: String,
    /// option names or dotted paths (default: all)
    #[serde(default)]
    pub keys: Option<Vec<String>>,
    /// return per-key metadata (`effective`, `default`, `source`, `description`, `redacted`, …) instead of bare values
    #[serde(default)]
    pub describe: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OptionsSetParams {
    pub session_id: String,
    /// name → value (e.g. `{"model": "openai/gpt-6.1-terra", "effort": "high"}`)
    pub values: serde_json::Map<String, Value>,
}

/// Return redacted config metadata, provenance, and redacted warnings.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigDescribeParams {
    /// option names or dotted paths (default: all)
    #[serde(default)]
    pub keys: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OptionSource {
    pub layer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#where: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct OptionDescriptor {
    /// effective value (redacted for sensitive fields; `null` when unset)
    #[serde(default)]
    pub effective: Value,
    /// registered default value (redacted for sensitive fields), when defined
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<OptionSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub value_type: Option<String>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub redacted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigDescribeResult {
    pub options: BTreeMap<String, OptionDescriptor>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_params_and_describe_roundtrip() {
        assert!(serde_json::from_value::<OptionsGetParams>(serde_json::json!({})).is_err());
        assert!(serde_json::from_value::<OptionsSetParams>(serde_json::json!({ "values": {} })).is_err());

        let bare: OptionsGetParams = serde_json::from_value(serde_json::json!({ "session_id": "s1" })).unwrap();
        assert_eq!(bare.session_id, "s1");
        assert!(bare.keys.is_none());
        assert!(!bare.describe);

        let described: OptionsGetParams =
            serde_json::from_value(serde_json::json!({ "session_id": "s1", "keys": ["providers"], "describe": true }))
                .unwrap();
        assert_eq!(described.keys, Some(vec!["providers".to_string()]));
        assert!(described.describe);

        let desc: OptionDescriptor = serde_json::from_value(serde_json::json!({
            "effective": "[REDACTED]",
            "default": "[REDACTED]",
            "source": { "layer": "user", "where": "init.lua:4" },
            "description": "secret token",
            "type": "string",
            "sensitive": true,
            "redacted": true
        }))
        .unwrap();
        let out = ConfigDescribeResult {
            options: BTreeMap::from([("plugins.probe.token".to_string(), desc)]),
            warnings: vec!["warn".to_string()],
        };
        let encoded = serde_json::to_value(&out).unwrap();
        let decoded: ConfigDescribeResult = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, out);
    }
}
