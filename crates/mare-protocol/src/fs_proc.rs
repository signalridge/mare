//! Typed filesystem RPC notification payload.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FsChangedNotification {
    pub watch_id: String,
    pub changed_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}
