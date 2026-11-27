use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct MarkdownPresentation {
    pub document_id: String,
    pub source: String,
    pub revision: u64,
    pub render_seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_epoch: Option<String>,
    pub pending: usize,
    pub enhanced: bool,
    pub supports_artifact_images: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
    /// Existing Node description. Image bytes become bounded canonical base64 `data`; no paths,
    /// callbacks, tokens, or userdata. Deserialize via mare.ui.node_from_wire in a Lua frontend.
    pub node: Value,
    /// Same Markdown document with each enhanced fence followed by its complete source.
    pub expanded_node: Value,
}

/// JSON payload of an image resource key in the built-in `output` namespace. The locator is
/// opaque and must be read on the originating RPC connection; these facts grant no access.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputImageKey {
    pub session_id: String,
    pub session_epoch: String,
    pub spill_id: String,
    #[schemars(length(min = 64, max = 64), regex(pattern = "^[0-9a-f]{64}$"))]
    pub sha256: String,
}
