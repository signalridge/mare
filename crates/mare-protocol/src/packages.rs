//! Extension CLI flag query protocol types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Registered extension CLI flag descriptor (`flags.list`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct FlagEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub flag_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// `flags.list` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct FlagsListResult {
    pub data: Vec<FlagEntry>,
}

/// `flags.get` params.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlagsGetParams {
    pub name: String,
}

/// `flags.get` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct FlagsGetResult {
    pub name: String,
    pub registered: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}
