//! Host-owned callback and streaming provider registration types.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `callback.call`: server → client request invoking one contract callback on a registration the
/// client holds. Arguments match the callback's params schema; result is
/// checked against its result schema.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallbackCallParams {
    pub registration_id: String,
    pub callback: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_id: Option<String>,
    pub arguments: serde_json::Value,
}

/// `callback.update`: client → server notification streaming a partial update for an in-flight
/// `callback.call` request identified by `id`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallbackUpdateParams {
    pub id: crate::RequestId,
    pub partial: serde_json::Value,
}
