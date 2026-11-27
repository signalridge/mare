//! MCP elicitation callback schema.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `mcp.elicitation` (mare → host): an MCP server requested user input or URL confirmation.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct McpElicitationParams {
    /// session that requested elicitation
    pub session_id: String,
    /// active turn id, when known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default)]
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elicitation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl McpElicitationAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpElicitationResult {
    pub action: McpElicitationAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Value>,
}
