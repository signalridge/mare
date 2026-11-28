//! Public event payloads. Unknown object fields are retained for plugin extensibility.
//! Nested provider, message and tool objects remain open because replacement services own them.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

type Extra = BTreeMap<String, Value>;
type Object = BTreeMap<String, Value>;

macro_rules! payload {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        pub struct $name {
            $(pub $field: $ty,)*
            /// True when the outbound event tap truncated strings or projected images to stay within `EVENT_MAX_BYTES`.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            pub truncated: Option<bool>,
            #[serde(flatten)] pub extra: Extra,
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnStarted {
    pub turn_id: NonEmptyString,
    pub trigger: NonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_item_ids: Option<Vec<NonEmptyString>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnCompleted {
    pub turn_id: NonEmptyString,
    pub status: TurnStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<crate::AgentTerminationReason>,
    /// Origin of cancellation when `termination_reason` is `interrupted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_cause: Option<crate::CancellationCause>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Object>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<EventError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_sent: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepStarted {
    pub turn_id: NonEmptyString,
    pub step_id: NonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Structured error payload matching protocol component `error` (`{ code, message, data? }`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StepCompleted {
    pub turn_id: NonEmptyString,
    pub step_id: NonEmptyString,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<EventError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub will_retry: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}
/// Non-empty string required on core protocol fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct NonEmptyString(String);

impl NonEmptyString {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for NonEmptyString {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for NonEmptyString {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl TryFrom<String> for NonEmptyString {
    type Error = &'static str;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() { Err("string must be non-empty") } else { Ok(Self(value)) }
    }
}

impl<'de> Deserialize<'de> for NonEmptyString {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for NonEmptyString {
    fn inline_schema() -> bool {
        true
    }
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "NonEmptyString".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        serde_json::json!({
            "type": "string",
            "minLength": 1
        })
        .try_into()
        .expect("valid non-empty string schema")
    }
}

/// Text block inside an outbound wire event message (`item/completed` tool results, user messages).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EventTextBlock {
    #[serde(rename = "type")]
    pub kind: EventTextKind,
    pub text: String,
    /// True when the outbound event tap truncated `text` to stay within `EVENT_MAX_BYTES`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EventTextKind {
    #[serde(rename = "text")]
    Text,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum EventImageKind {
    #[serde(rename = "image")]
    Image,
}

/// Image block inside an outbound wire event message.
///
/// Either carries inline base64 `data` (`truncated` absent or false), or a truncated reference
/// `{ type: "image", mime, path?, bytes, truncated: true }` without `data` (full content is
/// retrieved via `session.get_item`).
#[derive(Debug, Clone, Serialize)]
pub struct EventImageBlock {
    #[serde(rename = "type")]
    pub kind: EventImageKind,
    pub mime: NonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<NonEmptyString>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl<'de> Deserialize<'de> for EventImageBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(rename = "type")]
            kind: EventImageKind,
            mime: NonEmptyString,
            #[serde(default)]
            data: Option<NonEmptyString>,
            #[serde(default)]
            path: Option<String>,
            #[serde(default)]
            bytes: Option<u64>,
            #[serde(default)]
            truncated: Option<bool>,
            #[serde(flatten)]
            extra: Extra,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.truncated == Some(true) {
            if raw.data.is_some() {
                return Err(serde::de::Error::custom("truncated image block must not carry inline `data`"));
            }
            if raw.bytes.is_none() {
                return Err(serde::de::Error::custom("truncated image block requires integer `bytes`"));
            }
        } else if raw.data.is_none() {
            return Err(serde::de::Error::custom(
                "image block requires inline `data` unless `truncated` is true with `bytes`",
            ));
        }
        Ok(Self {
            kind: raw.kind,
            mime: raw.mime,
            data: raw.data,
            path: raw.path,
            bytes: raw.bytes,
            truncated: raw.truncated,
            extra: raw.extra,
        })
    }
}

/// Content block inside wire event messages (`text`, inline or truncated `image`, or extension block).
#[derive(Debug, Clone)]
pub enum EventContentBlock {
    Text(EventTextBlock),
    Image(EventImageBlock),
    Other(Object),
}

impl Serialize for EventContentBlock {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Text(b) => b.serialize(serializer),
            Self::Image(b) => b.serialize(serializer),
            Self::Other(obj) => obj.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for EventContentBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let val = Value::deserialize(deserializer)?;
        let Some(obj) = val.as_object() else {
            return Err(serde::de::Error::custom("content block must be a JSON object"));
        };
        let Some(kind) = obj.get("type").and_then(Value::as_str).filter(|s| !s.is_empty()) else {
            return Err(serde::de::Error::custom("content block requires non-empty string `type`"));
        };
        match kind {
            "text" => serde_json::from_value(val).map(Self::Text).map_err(serde::de::Error::custom),
            "image" => serde_json::from_value(val).map(Self::Image).map_err(serde::de::Error::custom),
            _ => Ok(Self::Other(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())),
        }
    }
}

impl JsonSchema for EventContentBlock {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "EventContentBlock".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        serde_json::json!({
            "description": "Content block inside wire event messages (`text`, inline `image`, truncated image reference `{ type: \"image\", mime, path?, bytes, truncated: true }`, or extension block).",
            "oneOf": [
                {
                    "type": "object",
                    "required": ["type", "text"],
                    "properties": {
                        "type": { "const": "text" },
                        "text": { "type": "string" },
                        "truncated": { "type": "boolean" }
                    }
                },
                {
                    "type": "object",
                    "required": ["type", "mime", "data"],
                    "properties": {
                        "type": { "const": "image" },
                        "mime": { "type": "string", "minLength": 1 },
                        "data": { "type": "string", "minLength": 1 },
                        "path": { "type": "string" },
                        "bytes": { "type": "integer", "minimum": 0 },
                        "truncated": { "const": false }
                    }
                },
                {
                    "type": "object",
                    "required": ["type", "mime", "bytes", "truncated"],
                    "properties": {
                        "type": { "const": "image" },
                        "mime": { "type": "string", "minLength": 1 },
                        "path": { "type": "string" },
                        "bytes": { "type": "integer", "minimum": 0 },
                        "truncated": { "const": true }
                    },
                    "not": { "required": ["data"] }
                },
                {
                    "type": "object",
                    "required": ["type"],
                    "properties": {
                        "type": { "type": "string", "minLength": 1, "not": { "enum": ["text", "image"] } }
                    }
                }
            ]
        })
        .try_into()
        .expect("valid EventContentBlock schema")
    }
}

/// Lightweight scalar metadata on `item.result` for a completed `tool_call`.
///
/// `item.message` is the sole carrier of `content` and `details`; `item.result` must not duplicate them.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct ToolCallResultMeta(pub Object);

impl<'de> Deserialize<'de> for ToolCallResultMeta {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = Object::deserialize(deserializer)?;
        if map.contains_key("content") || map.contains_key("details") || map.contains_key("truncated") {
            return Err(serde::de::Error::custom(
                "item.result must not duplicate `content`/`details` or use wire-reserved `truncated`; use `item.message` and `output_truncated`",
            ));
        }
        Ok(Self(map))
    }
}

impl JsonSchema for ToolCallResultMeta {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ToolCallResultMeta".into()
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        serde_json::json!({
            "description": "Lightweight scalar metadata for a completed tool call (`item.message` is the sole carrier of `content` and `details`; model output truncation uses `output_truncated`).",
            "type": "object",
            "not": {
                "anyOf": [
                    { "required": ["content"] },
                    { "required": ["details"] },
                    { "required": ["truncated"] }
                ]
            }
        })
        .try_into()
        .expect("valid ToolCallResultMeta schema")
    }
}

/// Canonical wire field key for item/turn status shared with outbound tap bounding.
pub const FIELD_STATUS: &str = "status";

/// Role of the model-visible message submitted on a `tool_call` result (`item/completed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ToolResultRole {
    #[serde(rename = "tool_result")]
    ToolResult,
}

/// Model-visible tool result message on a `tool_call` result (`item/completed`); extra fields remain open to plugins.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ToolResultMessage {
    pub role: ToolResultRole,
    pub tool_call_id: NonEmptyString,
    pub content: Vec<EventContentBlock>,
    #[serde(flatten)]
    pub extra: Extra,
}

/// Terminal `tool_call` result payload (`item/completed`) enforcing call/message shape and ID correspondence.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ToolResult {
    pub tool_call_id: NonEmptyString,
    pub name: NonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<ToolCallResultMeta>,
    pub message: ToolResultMessage,
    pub result_status: ResultStatus,
    pub execution_status: ExecutionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl<'de> Deserialize<'de> for ToolResult {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            tool_call_id: NonEmptyString,
            name: NonEmptyString,
            #[serde(default)]
            result: Option<ToolCallResultMeta>,
            message: ToolResultMessage,
            result_status: ResultStatus,
            execution_status: ExecutionStatus,
            #[serde(default)]
            truncated: Option<bool>,
            #[serde(flatten)]
            extra: Extra,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.message.tool_call_id != raw.tool_call_id {
            return Err(serde::de::Error::custom(format!(
                "item/completed tool_call message.tool_call_id ({:?}) must match tool_call_id ({:?})",
                raw.message.tool_call_id.as_str(),
                raw.tool_call_id.as_str(),
            )));
        }
        Ok(Self {
            tool_call_id: raw.tool_call_id,
            name: raw.name,
            result: raw.result,
            message: raw.message,
            result_status: raw.result_status,
            execution_status: raw.execution_status,
            truncated: raw.truncated,
            extra: raw.extra,
        })
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    Success,
    Error,
    Denied,
    Cancelled,
    PersistFailed,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Denied,
    Cancelled,
    PersistFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemDeltaKind {
    Text,
    Reasoning,
    ToolArguments,
    ToolOutput,
    Event,
}

/// Discriminated item payload shared across `item/started` and `item/completed`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    UserMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Object>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
        /// True when outbound tap projection truncated strings or projected images on this item.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
    AssistantMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Object>,
        /// True when outbound tap projection truncated strings or projected images on this item.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
    ToolCall {
        tool_call_id: NonEmptyString,
        name: NonEmptyString,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider_call_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        arguments: Option<Value>,
        status: ItemStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<ToolCallResultMeta>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Box<ToolResultMessage>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_status: Option<ResultStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_status: Option<ExecutionStatus>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        info: Option<Object>,
        /// True when outbound tap projection truncated arguments/text or projected images on this item (`session.get_item` retrieves full content).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
    Custom {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<Object>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry: Option<Object>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
    Compaction {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        entry_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_after: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pruned_count: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offloaded_count: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        changed: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stalled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<EventError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cancelled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        truncated: Option<bool>,
        #[serde(flatten)]
        extra: Extra,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ItemStarted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<NonEmptyString>,
    pub item_id: NonEmptyString,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_id: Option<NonEmptyString>,
    pub item: Item,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ItemDelta {
    pub turn_id: NonEmptyString,
    pub item_id: NonEmptyString,
    pub kind: ItemDeltaKind,
    pub delta: String,
    pub offset: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partial: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ItemCompleted {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<NonEmptyString>,
    pub item_id: NonEmptyString,
    pub status: ItemStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<Item>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_omitted: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

impl<'de> Deserialize<'de> for ItemCompleted {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            turn_id: Option<NonEmptyString>,
            item_id: NonEmptyString,
            status: ItemStatus,
            item: Option<Item>,
            item_omitted: Option<bool>,
            #[serde(flatten)]
            extra: Extra,
        }
        let raw = Raw::deserialize(deserializer)?;
        if raw.item.is_none() && raw.item_omitted != Some(true) {
            return Err(serde::de::Error::custom("item/completed requires `item` unless `item_omitted` is true"));
        }
        if let Some(Item::ToolCall { tool_call_id, message, .. }) = &raw.item
            && let Some(msg) = message
            && &msg.tool_call_id != tool_call_id
        {
            return Err(serde::de::Error::custom(format!(
                "item/completed tool_call message.tool_call_id ({:?}) must match tool_call_id ({:?})",
                msg.tool_call_id.as_str(),
                tool_call_id.as_str(),
            )));
        }
        Ok(Self {
            turn_id: raw.turn_id,
            item_id: raw.item_id,
            status: raw.status,
            item: raw.item,
            item_omitted: raw.item_omitted,
            extra: raw.extra,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueueLane {
    Steer,
    FollowUp,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct QueueItem {
    pub input_id: Option<String>,
    pub lane: QueueLane,
    pub text: Option<String>,
    #[serde(flatten)]
    pub extra: Extra,
}
payload!(QueueChanged {
    items: Vec<QueueItem>,
    action: Option<String>,
});
payload!(Notice { level: String, text: String });
payload!(SubagentStarted {
    subagent_id: String,
    agent_id: Option<String>,
    name: Option<String>,
    label: Option<String>,
    run_id: Option<String>,
    seq: Option<u64>,
    task: Option<String>,
    parent_session_id: Option<String>,
    parent_session_epoch: Option<String>
});
payload!(SubagentProgress {
    subagent_id: String,
    parent_session_id: Option<String>,
    parent_session_epoch: Option<String>,
    progress: Object
});
payload!(SubagentCompleted {
    subagent_id: String,
    agent_id: Option<String>,
    name: Option<String>,
    label: Option<String>,
    status: String,
    run_id: Option<String>,
    seq: Option<u64>,
    parent_session_id: Option<String>,
    parent_session_epoch: Option<String>,
    text: Option<String>,
    error: Option<EventError>,
    error_kind: Option<String>,
    usage: Option<Value>
});
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalAuditCall {
    pub tool_call_id: String,
    pub name: String,
}
pub type CallIdentity = ApprovalAuditCall;
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAuditDecision {
    Allow,
    AllowSession,
    AllowAmend,
    Deny,
    Abort,
    Unavailable,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAuditSource {
    User,
    Policy,
    Reviewer,
    Rule,
    ApprovalPolicy,
    Cancelled,
    Error,
    None,
    Hook,
    Host,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAuditOutcome {
    Timeout,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResult {
    pub call: ApprovalAuditCall,
    pub decision: ApprovalAuditDecision,
    pub source: Option<ApprovalAuditSource>,
    pub outcome: Option<ApprovalAuditOutcome>,
    pub child_approval_id: Option<u64>,
    pub scope: Option<crate::PermissionGrantScope>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequested {
    pub approval_id: String,
    pub category: String,
    pub tool_call_id: Option<String>,
    pub snapshot: Option<Value>,
    pub child_approval_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolved {
    pub approval_id: String,
    pub category: String,
    pub tool_call_id: Option<String>,
    pub result: ApprovalResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}
payload!(CompactStage { stage: String, phase: String, tokens: Option<f64>, changed: Option<bool>, tokens_before: Option<f64>, tokens_after: Option<f64>, reason: Option<String> });
payload!(JobsChanged { job: Object });
payload!(StatusChanged { state: String, message: String, session: Object, seq: u64 });
payload!(ApprovalChanged { policy: Value, revision: u64 });
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalReviewResult {
    pub category: String,
    pub result: ApprovalResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}
payload!(PermissionChanged { preset: Option<String>, label: Option<String>, partial: Option<bool> });
payload!(SessionChanged { session_id: String, path: Option<String>, reason: String });
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModelChanged {
    pub provider_id: String,
    pub model_id: String,
    #[serde(rename = "ref")]
    pub model_ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}
payload!(UsageChanged {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    cache_write_1h: Option<f64>,
    cost: f64,
    cost_unknown: Option<bool>,
    requests: f64,
    subscription_requests: f64,
    side_requests: f64,
    last_context: f64,
    last_cache_read: f64
});
payload!(WebProvidersChanged { kind: String });

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Select,
    Input,
    Confirm,
    Questions,
    Editor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InteractionOutcome {
    Answered,
    Cancelled,
    Unavailable,
}

payload!(InteractionRequested {
    interaction_id: NonEmptyString,
    kind: InteractionKind,
    title: Option<String>,
    session_id: Option<String>,
});
payload!(InteractionResolved {
    interaction_id: NonEmptyString,
    kind: InteractionKind,
    outcome: InteractionOutcome,
    title: Option<String>,
    session_id: Option<String>,
});
payload!(UiPrint { text: String, lines: Option<Vec<String>>, node: Option<Value> });
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct UiCard {
    pub card_id: Option<String>,
    pub block_id: Option<u64>,
    pub title: String,
    pub markdown: String,
    pub kind: String,
    pub persistent: bool,
    pub after_entry_id: Option<String>,
    pub meta: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    #[serde(flatten)]
    pub extra: Extra,
}

pub use crate::event_registry::ProtocolEvent;

/// Validate protocol event payloads before delivery. Domain/extension event names stay open here
/// (validated against their owning service contract declarations). Validation never reconstructs or
/// strips unknown fields.
pub fn validate_protocol(name: &str, data: &Value) -> Result<(), serde_json::Error> {
    if !crate::event_registry::is_protocol_event(name) {
        return Ok(());
    }
    serde_json::from_value::<ProtocolEvent>(serde_json::json!({"type": name, "data": data})).map(|_| ())
}

#[cfg(test)]
pub(crate) mod test_fixtures {
    use serde_json::{Value, json};

    pub(crate) fn valid_tool_result_with(status: &str, content: Value) -> Value {
        json!({
            "type": "tool_call",
            "tool_call_id": "call_1",
            "name": "read",
            "status": "completed",
            "arguments": { "path": "a.txt" },
            "message": {
                "role": "tool_result",
                "tool_call_id": "call_1",
                "tool_name": "read",
                "content": content,
                "is_error": status != "success",
                "plugin_msg_tag": 1
            },
            "result_status": status,
            "execution_status": "unknown",
            "plugin_top_tag": true
        })
    }

    pub(crate) fn valid_tool_result_cases() -> Vec<(String, Value)> {
        let mut cases: Vec<(String, Value)> = ["success", "error", "denied", "cancelled", "persist_failed"]
            .into_iter()
            .map(|status| {
                (format!("status_{status}"), valid_tool_result_with(status, json!([{ "type": "text", "text": "ok" }])))
            })
            .collect();
        cases.push(("empty_content_array".into(), valid_tool_result_with("success", json!([]))));
        cases.push((
            "truncated_text_block".into(),
            valid_tool_result_with("success", json!([{ "type": "text", "text": "prefix", "truncated": true }])),
        ));
        cases.push((
            "inline_image_block".into(),
            valid_tool_result_with(
                "success",
                json!([{ "type": "image", "mime": "image/png", "data": "iVBORw0KGgo=" }]),
            ),
        ));
        cases.push((
            "truncated_image_ref_with_path".into(),
            valid_tool_result_with(
                "success",
                json!([{
                    "type": "image",
                    "mime": "image/png",
                    "path": "shot.png",
                    "bytes": 430_080,
                    "truncated": true
                }]),
            ),
        ));
        cases.push((
            "truncated_image_ref_without_path".into(),
            valid_tool_result_with(
                "success",
                json!([{
                    "type": "image",
                    "mime": "image/png",
                    "bytes": 430_080,
                    "truncated": true
                }]),
            ),
        ));
        let mut with_output_truncated = valid_tool_result_with("success", json!([{ "type": "text", "text": "ok" }]));
        with_output_truncated["result"] = json!({ "is_error": false, "output_truncated": true });
        cases.push(("result_with_output_truncated".into(), with_output_truncated));
        cases
    }

    /// `(label, payload, schema_rejects)`. `mismatched_tool_call_id` is enforced in Rust
    /// deserialization (`ToolResult` / `ItemCompleted`) because JSON Schema cannot express cross-field equality.
    pub(crate) fn invalid_tool_result_cases() -> Vec<(&'static str, Value, bool)> {
        vec![
            (
                "empty_message",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": {},
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "missing_item_tool_call_id",
                json!({
                    "type": "tool_call",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "empty_item_tool_call_id",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "missing_call_name",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "empty_call_name",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "missing_message_role",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "wrong_message_role_assistant",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "assistant", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "wrong_message_role_camel",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "toolResult", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "missing_message_content",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1" },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "object_message_content",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": {} },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "string_message_content",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": "ok" },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "null_message_content",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": null },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "missing_tool_call_id",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "empty_tool_call_id",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "mismatched_tool_call_id",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_2", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                false,
            ),
            (
                "invalid_result_status",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "not-a-status",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "invalid_execution_status",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "failed",
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "completed"
                }),
                true,
            ),
            (
                "image_truncated_with_inline_data",
                valid_tool_result_with(
                    "success",
                    json!([{
                        "type": "image",
                        "mime": "image/png",
                        "data": "iVBORw0KGgo=",
                        "bytes": 8,
                        "truncated": true
                    }]),
                ),
                true,
            ),
            (
                "image_truncated_missing_bytes",
                valid_tool_result_with(
                    "success",
                    json!([{
                        "type": "image",
                        "mime": "image/png",
                        "truncated": true
                    }]),
                ),
                true,
            ),
            (
                "image_missing_data_and_truncated",
                valid_tool_result_with(
                    "success",
                    json!([{
                        "type": "image",
                        "mime": "image/png"
                    }]),
                ),
                true,
            ),
            (
                "text_non_boolean_truncated",
                valid_tool_result_with(
                    "success",
                    json!([{
                        "type": "text",
                        "text": "ok",
                        "truncated": "yes"
                    }]),
                ),
                true,
            ),
            (
                "result_carrying_duplicate_content",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "completed",
                    "result": { "content": [] },
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "result_carrying_duplicate_details",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "completed",
                    "result": { "details": {} },
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
            (
                "result_carrying_wire_truncated",
                json!({
                    "type": "tool_call",
                    "tool_call_id": "call_1",
                    "name": "read",
                    "status": "completed",
                    "result": { "truncated": true },
                    "message": { "role": "tool_result", "tool_call_id": "call_1", "content": [] },
                    "result_status": "success",
                    "execution_status": "unknown"
                }),
                true,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{invalid_tool_result_cases, valid_tool_result_cases};
    use super::*;
    use serde_json::json;

    #[test]
    fn approval_audit_is_closed_and_never_accepts_execution_data() {
        let result = json!({"call":{"tool_call_id":"call-1","name":"effect"},"decision":"allow","source":"user"});
        let resolved = json!({
            "approval_id": "ap-1",
            "category": "tool",
            "tool_call_id": "call-1",
            "result": result
        });
        assert!(validate_protocol("approval/resolved", &resolved).is_ok());
        for field in ["arguments", "schema", "cwd", "snapshot", "origin"] {
            let mut invalid = resolved.clone();
            invalid["result"]["call"][field] = json!("private execution data");
            assert!(validate_protocol("approval/resolved", &invalid).is_err(), "accepted result.call.{field}");
        }
        for field in ["info", "permissions", "amendment", "reason", "human_approval"] {
            let mut invalid = resolved.clone();
            invalid["result"][field] = json!("private execution data");
            assert!(validate_protocol("approval/resolved", &invalid).is_err(), "accepted result.{field}");
        }
        let requested = json!({
            "approval_id": "ap-1",
            "category": "tool",
            "tool_call_id": "call-1",
            "snapshot": "sha256:abc"
        });
        assert!(validate_protocol("approval/requested", &requested).is_ok());
        let mut bad_req = requested.clone();
        bad_req["arguments"] = json!({"secret": "x"});
        assert!(validate_protocol("approval/requested", &bad_req).is_err(), "accepted unexpected request field");

        // Non-item and closed (`deny_unknown_fields`) events accept boolean root `truncated` and reject non-boolean.
        let catalog = crate::schema();
        for (name, mut payload) in [
            ("turn/started", json!({"turn_id": "t-1", "trigger": "prompt"})),
            ("step/completed", json!({"turn_id": "t-1", "step_id": "s-1", "status": "completed"})),
            ("host/notice", json!({"level": "info", "text": "trimmed"})),
            ("approval/requested", requested),
            ("approval/resolved", resolved),
        ] {
            payload["truncated"] = json!(true);
            validate_protocol(name, &payload).unwrap_or_else(|e| panic!("{name} must accept truncated=true: {e}"));
            let env = json!({ "type": name, "data": payload });
            crate::validate_schema_at(&catalog, "#/notifications/event", &env)
                .unwrap_or_else(|e| panic!("{name} schema must accept truncated=true: {e}"));
            let mut bad = payload.clone();
            bad["truncated"] = json!("yes");
            assert!(validate_protocol(name, &bad).is_err(), "{name} must reject non-boolean truncated");
            let bad_env = json!({ "type": name, "data": bad });
            assert!(
                crate::validate_schema_at(&catalog, "#/notifications/event", &bad_env).is_err(),
                "{name} schema must reject non-boolean truncated"
            );
        }
    }

    #[test]
    fn builtins_validate_and_preserve_unknown_fields() {
        let input = json!({
            "type": "item/completed",
            "data": {
                "turn_id": "t-1",
                "item_id": "m-1",
                "status": "completed",
                "item": { "type": "assistant_message", "message": { "role": "assistant" } },
                "future": { "x": 1 }
            }
        });
        let parsed: ProtocolEvent = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), input);
        assert!(validate_protocol("item/completed", &json!({"item_id": "m-1", "status": "completed"})).is_err());
        for cause in ["user", "parent", "hook", "disposed"] {
            assert!(
                validate_protocol(
                    "turn/completed",
                    &json!({
                        "turn_id": "t-1",
                        "status": "cancelled",
                        "termination_reason": "interrupted",
                        "cancellation_cause": cause,
                    }),
                )
                .is_ok(),
                "turn/completed must accept cancellation_cause={cause}"
            );
        }
        assert!(
            validate_protocol(
                "item/completed",
                &json!({"item_id": "m-1", "status": "completed", "item_omitted": true})
            )
            .is_ok()
        );
        assert!(validate_protocol("plugin/future", &json!({"anything": [1, 2]})).is_ok());
        assert!(
            validate_protocol(
                "item/delta",
                &json!({"turn_id": "t-1", "item_id": "m-1", "kind": "text", "delta": 12, "offset": 0})
            )
            .is_err()
        );
    }

    #[test]
    fn tool_result_enforces_nested_contract_and_id_correspondence() {
        for (label, payload) in valid_tool_result_cases() {
            validate_protocol(
                "item/completed",
                &json!({
                    "turn_id": "t-1",
                    "item_id": "call_1",
                    "status": "completed",
                    "item": payload,
                }),
            )
            .unwrap_or_else(|e| panic!("{label} must validate: {e}"));
            let parsed: ToolResult =
                serde_json::from_value(payload.clone()).unwrap_or_else(|e| panic!("{label} must deserialize: {e}"));
            assert_eq!(parsed.tool_call_id, parsed.message.tool_call_id, "{label}");
            assert_eq!(serde_json::to_value(&parsed).unwrap(), payload, "{label} roundtrip");
        }

        for (label, payload, _) in invalid_tool_result_cases() {
            assert!(
                validate_protocol(
                    "item/completed",
                    &json!({
                        "turn_id": "t-1",
                        "item_id": "call_1",
                        "status": "failed",
                        "item": payload,
                    })
                )
                .is_err(),
                "{label} must be rejected by validate_protocol"
            );
            assert!(
                serde_json::from_value::<ToolResult>(payload).is_err(),
                "{label} must be rejected by ToolResult::deserialize"
            );
        }
    }

    #[test]
    fn all_public_events_have_typed_builtin_contracts() {
        for def in crate::event_registry::PROTOCOL_EVENTS {
            let mut payload = (def.example)();
            if !matches!(def.name, "approval/requested" | "approval/resolved")
                && let Some(obj) = payload.as_object_mut()
            {
                obj.insert("_ext".to_string(), json!(1));
            }
            validate_protocol(def.name, &payload).unwrap_or_else(|e| panic!("{} must validate: {e}", def.name));
        }

        // Key negative contracts: missing required fields or wrong types must be rejected.
        for (name, bad) in [
            ("turn/started", json!({"turn_id": "", "trigger": "prompt"})),
            ("turn/completed", json!({"turn_id": "t-1", "status": "unknown"})),
            ("turn/completed", json!({"turn_id": "t-1", "status": "cancelled", "cancellation_cause": "bogus"})),
            ("step/started", json!({"turn_id": "t-1", "step_id": ""})),
            ("step/completed", json!({"turn_id": "t-1", "step_id": "s-1"})),
            ("item/started", json!({"item_id": "m-1", "item": "not-an-object"})),
            ("item/delta", json!({"turn_id": "t-1", "item_id": "m-1", "kind": "text", "delta": 12, "offset": 0})),
            ("item/completed", json!({})),
            ("queue/changed", json!({})),
        ] {
            assert!(validate_protocol(name, &bad).is_err(), "{name} must reject invalid payload {bad}");
        }
    }

    #[test]
    fn interaction_and_ui_event_payloads_enforce_closed_variants_and_fields() {
        assert!(
            validate_protocol(
                "interaction/requested",
                &json!({"interaction_id": "int-1", "kind": "select", "title": "Pick"})
            )
            .is_ok()
        );
        assert!(
            validate_protocol(
                "interaction/resolved",
                &json!({"interaction_id": "int-1", "kind": "confirm", "outcome": "answered"})
            )
            .is_ok()
        );

        for (name, bad) in [
            ("interaction/requested", json!({"interaction_id": "", "kind": "select"})),
            ("interaction/requested", json!({"interaction_id": "int-1", "kind": "bogus"})),
            ("interaction/resolved", json!({"interaction_id": "int-1", "kind": "select", "outcome": "unknown"})),
            ("interaction/resolved", json!({"interaction_id": "int-1", "kind": "select", "outcome": "disconnected"})),
        ] {
            assert!(validate_protocol(name, &bad).is_err(), "{name} must reject invalid payload {bad}");
        }
    }

    #[test]
    fn tool_call_id_mismatch_and_reasoning_delta_are_validated() {
        let non_empty: NonEmptyString = serde_json::from_value(json!("abc")).unwrap();
        assert_eq!(non_empty.as_ref(), "abc");

        // item/delta accepts kind="reasoning" and rejects legacy kind="thinking".
        assert!(
            validate_protocol(
                "item/delta",
                &json!({"turn_id": "t-1", "item_id": "m-1", "kind": "reasoning", "delta": "step", "offset": 0})
            )
            .is_ok()
        );
        assert!(
            validate_protocol(
                "item/delta",
                &json!({"turn_id": "t-1", "item_id": "m-1", "kind": "thinking", "delta": "step", "offset": 0})
            )
            .is_err()
        );

        // item/completed tool_call rejects mismatched message.tool_call_id vs tool_call_id.
        let mismatch = json!({
            "turn_id": "t-1",
            "item_id": "call_1",
            "status": "completed",
            "item": {
                "type": "tool_call",
                "tool_call_id": "call_1",
                "name": "read",
                "status": "completed",
                "message": {"role": "tool_result", "tool_call_id": "other_call", "content": [{"type": "text", "text": "ok"}]},
                "result_status": "success",
                "execution_status": "unknown"
            }
        });
        let err = validate_protocol("item/completed", &mismatch).unwrap_err().to_string();
        assert!(err.contains("must match tool_call_id"), "{err}");
    }
}
