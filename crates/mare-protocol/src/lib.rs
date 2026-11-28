#![recursion_limit = "512"]
//! mare RPC protocol: JSON-RPC 2.0 over JSONL on stdin/stdout (`mare --rpc`).
//!
//! All wire types live here and a catalog of JSON Schemas
//! is generated from them ([`schema`]); a snapshot test fails when the protocol changes
//! silently. Versioning: [`PROTOCOL_VERSION`] is `major.minor`; the `3.0` wire shape is anchored
//! to mare 1.0.0, and removals or renames after 1.0.0 require a new major version (backward-compatible
//! additions bump the minor version). Field names are snake_case on the wire.
//!
//! Directions:
//! * host → mare requests: [`methods`] (`initialize`, `session.*`, `prompt`, …)
//! * host → mare notifications: `callback.update` ([`CallbackUpdateParams`]), `$/cancel_request`
//!   ([`CancelRequestParams`])
//! * mare → host notifications: `event` ([`EventParams`], identical to `mare -p --json` lines;
//!   includes `status/changed` with [`events::StatusChanged`]), `ui.notify`, `ui.markdown_changed`
//! * mare → host requests (callbacks): `callback.call`, `approval.request`,
//!   `ui.select`, `ui.input`, `ui.confirm` — only sent when the host declared the capability in
//!   `initialize`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
mod state_namespace;
pub use state_namespace::{
    StateCompareExchangeRequest, StateCompareExchangeResult, StateErrorCode, StateFailure, StateSnapshot,
    StateSnapshotRequest, StateSnapshotResult,
};

/// Single source of truth for protocol built-in public events (`protocol_events!`).
pub mod event_registry;
pub use event_registry::{EventDelivery, EventInfo, EventMode, EventOnError, EventScope, EventsListResult};
/// Typed built-in event payloads.
pub mod events;
/// Policy-constrained filesystem and process RPC payloads.
pub mod fs_proc;
/// The headless `--json` event stream (public event whitelist and line types).
pub mod headless;
/// Pure time query types shared by `mare.time.query` and its RPC contract.
mod time;
pub use time::{TimePattern, TimeQuery, TimeQueryResult};
/// MCP management RPC types and elicitation callbacks.
pub mod mcp;
pub use mcp::*;
/// Package management, resource directory discovery, and CLI flag query types.
pub mod packages;
pub use packages::*;
mod presentations;
pub use presentations::*;
mod schema;
#[cfg(test)]
mod tests;

pub use schema::{
    MethodAccess, method_access, method_admin, protocol_admin_methods, protocol_components, protocol_methods, schema,
    schema_text, validate_schema_at,
};
mod registrations;
/// Host tool registration, invocation, and incremental progress wire types.
pub mod tools;
pub use registrations::*;

pub use tools::{ToolResultBlock, ToolResultContent};
/// Typed option and config introspection payloads (`options.get`, `options.set`, `config.describe`).
pub mod options;

pub use options::*;
/// Unified media content types (`Attachment`, `ContentPart`) and audio/video detection.
pub mod content;

pub use content::{AUDIO_EXTENSIONS, Attachment, AttachmentKind, ContentPart, MediaKind, VIDEO_EXTENSIONS};
pub mod services;

pub use fs_proc::FsChangedNotification;

/// Structured agent run outcomes.
pub mod outcome;
/// Message components shared by session records and public payloads.
pub mod session_records;

pub use outcome::{AgentTerminationReason, CancellationCause};

/// Current protocol version (`major.minor`).
pub const PROTOCOL_VERSION: &str = "3.0";
/// Longest accepted `prompt` `request_id`, in UTF-8 bytes (the schema states it as `maxLength`,
/// which counts characters, so for non-ASCII ids the byte limit is the binding one).
pub const PROMPT_REQUEST_ID_MAX_BYTES: usize = 1024;

/// Default timeout for callbacks into the host.
pub const DEFAULT_CALLBACK_TIMEOUT_MS: u64 = 30_000;
/// Maximum timeout accepted for `initialize.callback_timeout_ms` (aligned with `3_600_000`ms / 1h host cap).
pub const MAX_CALLBACK_TIMEOUT_MS: u64 = 3_600_000;

pub const JSONRPC: &str = "2.0";

// ---------------------------------------------------------------------------
// JSON-RPC envelopes
// ---------------------------------------------------------------------------

/// Request id: number or string.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    String(String),
}

/// Any incoming line: request (method + id), notification (method, no id) or response (id +
/// result/error). An error response always carries `id`, `null` when the request's id could not
/// be read (JSON-RPC 2.0 §5).
#[derive(Clone, Debug, JsonSchema)]
#[schemars(extend("if" = { "required": ["error"] }, "then" = { "required": ["id"] }))]
pub struct Message {
    #[schemars(extend("const" = "2.0"))]
    pub jsonrpc: String,
    pub id: Option<RequestId>,
    #[schemars(skip)]
    pub id_present: bool,
    pub method: Option<String>,
    pub params: Option<Value>,
    pub result: Option<Value>,
    #[schemars(with = "RpcError")]
    pub error: Option<RpcError>,
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{self, MapAccess, Visitor};

        struct MessageVisitor;

        impl<'de> Visitor<'de> for MessageVisitor {
            type Value = Message;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON-RPC 2.0 message object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Message, A::Error> {
                let mut jsonrpc: Option<String> = None;
                let mut id: Option<RequestId> = None;
                let mut id_present = false;
                let mut method: Option<String> = None;
                let mut params: Option<Value> = None;
                let mut result: Option<Value> = None;
                let mut error: Option<RpcError> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "jsonrpc" => {
                            if jsonrpc.is_some() {
                                return Err(de::Error::duplicate_field("jsonrpc"));
                            }
                            jsonrpc = Some(map.next_value()?);
                        }
                        "id" => {
                            if id_present {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id_present = true;
                            id = map.next_value::<Option<RequestId>>()?;
                        }
                        "method" => {
                            if method.is_some() {
                                return Err(de::Error::duplicate_field("method"));
                            }
                            method = map.next_value()?;
                        }
                        "params" => {
                            if params.is_some() {
                                return Err(de::Error::duplicate_field("params"));
                            }
                            params = map.next_value()?;
                        }
                        "result" => {
                            if result.is_some() {
                                return Err(de::Error::duplicate_field("result"));
                            }
                            result = Some(map.next_value::<Value>()?);
                        }
                        "error" => {
                            if error.is_some() {
                                return Err(de::Error::duplicate_field("error"));
                            }
                            error = Some(map.next_value::<RpcError>()?);
                        }
                        _ => {
                            let _ = map.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }

                let jsonrpc = jsonrpc.ok_or_else(|| de::Error::missing_field("jsonrpc"))?;
                if method.is_some() && (result.is_some() || error.is_some()) {
                    return Err(de::Error::custom(
                        "JSON-RPC message cannot contain both `method` and `result`/`error`",
                    ));
                }
                if result.is_some() && error.is_some() {
                    return Err(de::Error::custom("JSON-RPC response cannot contain both `result` and `error`"));
                }
                if method.is_some() && id_present && id.is_none() {
                    return Err(de::Error::custom("JSON-RPC request `id` cannot be null"));
                }

                Ok(Message { jsonrpc, id, id_present, method, params, result, error })
            }
        }

        deserializer.deserialize_map(MessageVisitor)
    }
}

impl Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("jsonrpc", &self.jsonrpc)?;
        if self.id.is_some() || self.error.is_some() {
            map.serialize_entry("id", &self.id)?;
        }
        if let Some(method) = &self.method {
            map.serialize_entry("method", method)?;
        }
        if let Some(params) = &self.params {
            map.serialize_entry("params", params)?;
        }
        if let Some(result) = &self.result {
            map.serialize_entry("result", result)?;
        }
        if let Some(error) = &self.error {
            map.serialize_entry("error", error)?;
        }
        map.end()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RpcError {
    code: i64,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// JSON-RPC error numbers: the standard five, `-32800` for cancellation and
/// `-32000` for everything else. What an error is says `error.data.code`, from
/// [`ErrorCode`]; `error.data.data` holds the code's additional data.
pub mod codes {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const SERVER_ERROR: i64 = -32000;
    pub const CANCELLED: i64 = -32800;
}

/// The error vocabulary shared by Lua error values and RPC `error.data.code` (`ErrorCode`).
pub mod errors {
    pub use mare_shared::errors::*;
}

/// Exhaustive core error code enum (`ErrorCode`).
pub use mare_shared::errors::CoreErrorCode as ErrorCode;

impl RpcError {
    /// Construct an error directly from a core [`ErrorCode`]: its JSON-RPC number follows from the code.
    pub fn core(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::coded(code.as_str(), message)
    }

    /// An error with a vocabulary code (`CORE` or `<service>.<name>`): its JSON-RPC number follows from the code.
    pub fn coded(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: errors::rpc_number(code),
            message: message.into(),
            data: Some(serde_json::json!({ "code": code })),
        }
    }

    /// The JSON-RPC numeric code derived from this error's code.
    pub fn code(&self) -> i64 {
        self.code
    }

    /// The human-readable error message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Raw JSON `error.data` payload, if present.
    pub fn data(&self) -> Option<&Value> {
        self.data.as_ref()
    }

    /// Redact secrets in the error message and `data` payload at outbound wire boundaries.
    pub fn project_with(
        &mut self,
        mut project_str: impl FnMut(&str) -> String,
        mut project_val: impl FnMut(&mut Value),
    ) {
        self.message = project_str(&self.message);
        if let Some(data) = &mut self.data {
            project_val(data);
        }
    }

    /// The same error carrying the code's additional data (`error.data.data`).
    pub fn with_data(mut self, data: Value) -> Self {
        if let Some(Value::Object(object)) = &mut self.data {
            object.insert("data".into(), data);
        }
        self
    }
    /// The vocabulary code of this error (`error.data.code`).
    pub fn error_code(&self) -> Option<&str> {
        self.data.as_ref()?.get("code")?.as_str()
    }
    /// Every error sent has `data.code` and the number that code implies. A call site that
    /// gave only a number gets the code that number stands for; other fields it put in `data`
    /// move under `data.data`.
    pub fn normalized(self) -> Self {
        let mut object = match self.data {
            Some(Value::Object(object)) => object,
            Some(other) => serde_json::Map::from_iter([("data".to_owned(), other)]),
            None => serde_json::Map::new(),
        };
        // a core code, or a service's own `<service>.<name>` (the kernel checked it against the
        // contract); anything else is `internal`
        let code = match object.remove("code") {
            Some(Value::String(code)) if errors::is_core(&code) || errors::is_domain_code(&code) => code,
            Some(Value::String(_)) => "internal".to_owned(),
            _ => match self.code {
                codes::PARSE_ERROR => "parse_error",
                codes::INVALID_REQUEST => "invalid_request",
                codes::METHOD_NOT_FOUND => "method_not_found",
                codes::INVALID_PARAMS => "invalid_arguments",
                codes::CANCELLED => "cancelled",
                _ => "internal",
            }
            .to_owned(),
        };
        // the code's data as given; an older call site's other `data` fields join it
        let inner = match (object.remove("data"), object.is_empty()) {
            (inner, true) => inner,
            (Some(Value::Object(mut inner)), false) => {
                inner.extend(object);
                Some(Value::Object(inner))
            }
            (None, false) => Some(Value::Object(object)),
            (Some(other), false) => {
                object.insert("data".to_owned(), other);
                Some(Value::Object(object))
            }
        };
        let mut data = serde_json::Map::from_iter([("code".to_owned(), Value::String(code.clone()))]);
        // an internal error leaves the process with a fixed message and no data; what it said
        // goes to the log. Messages already made safe (the kernel's
        // "<service>.<method> failed (details are in the log)") pass as they are.
        let message = if code == "internal" {
            if !self.message.ends_with("(details are in the log)") {
                tracing::warn!(target: "mare::rpc", "internal error: {} {:?}", self.message, inner);
                "internal error (details are in the log)".to_owned()
            } else {
                self.message
            }
        } else {
            if let Some(inner) = inner {
                data.insert("data".to_owned(), inner);
            }
            self.message
        };
        Self { code: errors::rpc_number(&code), message, data: Some(Value::Object(data)) }
    }
}

impl Message {
    pub fn request(id: RequestId, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id: Some(id),
            id_present: true,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }
    pub fn notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id: None,
            id_present: false,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }
    pub fn response(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id: Some(id),
            id_present: true,
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }
    /// Construct a JSON-RPC error response from a vocabulary [`ErrorCode`].
    pub fn error(id: Option<RequestId>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::rpc_error(id, RpcError::core(code, message))
    }
    pub fn rpc_error(id: Option<RequestId>, error: RpcError) -> Self {
        Self {
            jsonrpc: JSONRPC.into(),
            id,
            id_present: true,
            method: None,
            params: None,
            result: None,
            error: Some(error.normalized()),
        }
    }
}

/// Method names (host → mare requests).
pub mod methods {
    pub const INITIALIZE: &str = "initialize";
    pub const CONNECTION_SET_CAPABILITIES: &str = "connection.set_capabilities";
    pub const SERVICES_LIST: &str = "services.list";
    pub const SERVICES_REGISTER: &str = "services.register";
    /// Release one registration returned to this connection (`{registration_id}`).
    pub const REGISTRATIONS_RELEASE: &str = "registrations.release";
    /// server → client: invoke one contract callback the client implements
    pub const CALLBACK_CALL: &str = "callback.call";
    /// client → server notification: partial update for an in-flight `callback.call`
    pub const CALLBACK_UPDATE: &str = "callback.update";
    pub const OPTIONS_GET: &str = "options.get";
    pub const OPTIONS_SET: &str = "options.set";
    pub const CONFIG_DESCRIBE: &str = "config.describe";
    pub const STATE_SNAPSHOT: &str = "state.snapshot";
    pub const STATE_COMPARE_EXCHANGE: &str = "state.compare_exchange";
    pub const SHUTDOWN: &str = "shutdown";
    pub const EVENTS_LIST: &str = "events.list";
    pub const EVENTS_SUBSCRIBE: &str = "events.subscribe";
    pub const BASELINE_READ: &str = "baseline.read";
    /// list registered extension CLI flags and their resolved values
    pub const FLAGS_LIST: &str = "flags.list";
    /// read a registered extension CLI flag value
    pub const FLAGS_GET: &str = "flags.get";
    /// peer socket: who this session is
    pub const PEER_INFO: &str = "peer.info";
    /// peer socket: the only entry point for messages from other sessions
    pub const PEER_MESSAGE: &str = "peer.message";

    /// mare → host notifications
    pub const MARKDOWN_CHANGED: &str = "ui.markdown_changed";
    pub const EVENT: &str = "event";
    pub const SNAPSHOT: &str = "snapshot";
    pub const LAGGED: &str = "lagged";
    pub const DROPPED: &str = "dropped";
    pub const STREAM_FAILED: &str = "stream.failed";
    pub const UI_NOTIFY: &str = "ui.notify";
    pub const UI_SELECT_UPDATE: &str = "ui/select-updated";
    pub const UI_SELECT_CLOSE: &str = "ui/select-closed";
    pub const UI_SET_EDITOR: &str = "ui.set_editor";
    pub const UI_EDITOR_HINT: &str = "ui.editor_hint";
    pub const UI_EDITOR_CONFIG: &str = "ui.editor_config";
    pub const UI_WIDGET: &str = "ui.widget";
    pub const UI_STATUS: &str = "ui.status";
    pub const UI_OVERLAY: &str = "ui.overlay";
    pub const UI_COMPONENT_UPDATE: &str = "ui/component-updated";
    pub const UI_EDITOR_STATE: &str = "ui.editor_state";
    pub const UI_ATTACH: &str = "ui.attach";
    pub const UI_DETACH: &str = "ui.detach";
    /// peer socket: the receiving user's decision on a held `peer.message`
    pub const PEER_MESSAGE_STATUS: &str = "peer.message_status";
    /// watched filesystem target changed (policy-filtered)
    pub const FS_CHANGED: &str = "fs/changed";

    /// mare → host requests (callbacks)
    /// typed approval request
    pub const APPROVAL_REQUEST: &str = "approval.request";
    /// MCP server elicitation request (primary connection only)
    pub const MCP_ELICITATION: &str = "mcp.elicitation";
    /// Server cancels a previously issued callback by its JSON-RPC request ID.
    pub const CANCEL_REQUEST: &str = "$/cancel_request";
    pub const UI_SELECT: &str = "ui.select";
    pub const UI_INPUT: &str = "ui.input";
    pub const UI_CONFIRM: &str = "ui.confirm";
    pub const UI_EDITOR: &str = "ui.editor";
    pub const UI_QUESTIONS: &str = "ui.questions";
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InitializeParams {
    /// Protocol version the host speaks (`major.minor`); the major version must match.
    pub protocol_version: String,
    #[serde(default)]
    pub client: Option<ClientInfo>,
    /// Callbacks the host can answer. Undeclared callbacks are never sent (defaults apply).
    #[serde(default)]
    pub capabilities: ClientCapabilities,
    /// Timeout for callbacks into the host in milliseconds (default 30000, clamped to 1..=3600000).
    #[serde(default)]
    pub callback_timeout_ms: Option<u64>,
    /// Optional human approval timeout in milliseconds; absent means wait without a deadline.
    #[serde(default)]
    pub approval_timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClientCapabilities {
    /// host answers `callback.call` for contract callbacks, providers, tools, services, web, and renderers it registers
    #[serde(default)]
    pub callbacks: bool,
    /// host answers `ui.select` / `ui.input` / `ui.confirm` and shows `ui.notify`
    #[serde(default)]
    pub ui: bool,
    /// host answers `approval.request` (tool calls and network hosts that need approval);
    /// primary connection only
    #[serde(default)]
    pub approvals: bool,
    /// host answers `mcp.elicitation` (MCP server elicitation requests); primary connection only
    #[serde(default)]
    pub elicitation: bool,
}

/// Full capability snapshot for `connection.set_capabilities`: all four known capabilities
/// are required booleans; unknown future capability keys are ignored (treated as false).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StrictClientCapabilities {
    pub callbacks: bool,
    pub ui: bool,
    pub approvals: bool,
    pub elicitation: bool,
}

impl From<StrictClientCapabilities> for ClientCapabilities {
    fn from(c: StrictClientCapabilities) -> Self {
        Self { callbacks: c.callbacks, ui: c.ui, approvals: c.approvals, elicitation: c.elicitation }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetCapabilitiesParams {
    pub capabilities: StrictClientCapabilities,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct SetCapabilitiesResult {
    pub capabilities: ClientCapabilities,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct InitializeResult {
    pub protocol_version: String,
    pub server: ServerInfo,
    pub capabilities: ServerCapabilities,
    /// Live `mare.Session` snapshot of the opened initial session, if any (`session_id` and
    /// `session_epoch` live on this object; session-scoped contract methods take `session_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ServerCapabilities {
    /// host → mare methods implemented by this server
    pub methods: Vec<String>,
    /// callbacks the server will use (subset of what the host declared)
    pub callbacks: ClientCapabilities,
}

/// Connection registration / subscription scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationScopeKind {
    /// Scoped to a single open session (`session_id` required).
    Session,
    /// Host-wide across sessions (primary connection only).
    All,
}

// ---------------------------------------------------------------------------
// prompting
// ---------------------------------------------------------------------------
// empty params
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyParams {}

// ---------------------------------------------------------------------------
// options / state
// ---------------------------------------------------------------------------

/// Empty object result for side-effect methods with no resource payload (`{}`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct EmptyResult {}

/// Uniform registration result (`{ registration_id }`) for `registrations.release`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegistrationResult {
    pub registration_id: String,
}

// ---------------------------------------------------------------------------
// peer socket
// ---------------------------------------------------------------------------

/// `peer.info` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct PeerInfo {
    pub name: String,
    pub pid: u32,
    pub cwd: String,
    /// mare version
    pub version: String,
    /// methods this connection may call (default set + the session's `peer.allow`)
    pub allowed: Vec<String>,
    /// supported inbound message delivery modes (e.g. `trigger_turn`, `queue_only`)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub delivery_modes: Vec<String>,
}

/// The sender of a `peer.message` as it describes itself — shown to the receiving user (with
/// whether the session registry knows the name), never grounds for allowing anything.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeerRef {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// `peer.message` params: a message from another session. Where the socket reports the
/// connecting process (Unix credentials), a `from` that disagrees with it is refused: a `pid`
/// other than the connecting process's, or the `name` of a registered session that isn't it.
/// Everything else in `from`, and `policy`, is self-reported: shown only, never grounds for
/// allowing anything. Always held until the receiving user confirms; a plugin can
/// take it over via the `peer/inbound` event. Never an approval.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PeerMessageParams {
    #[serde(default)]
    pub from: PeerRef,
    pub text: String,
    /// enqueue the message without waking an idle receiver
    #[serde(default)]
    pub queue_only: bool,
    /// the sender's policy as it reports it (display only until identities can be verified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<Value>,
}

/// `peer.message` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct PeerMessageResult {
    /// id of this message (repeated in `peer.message_status`)
    pub message_id: String,
    /// "held" (waiting for the receiving user; the outcome follows as `peer.message_status`) or
    /// "handled" (a plugin took it over via `peer/inbound`)
    pub status: String,
}

/// `peer.message_status` notification (mare → peer): the receiving user's decision on a held
/// message.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct PeerMessageStatus {
    pub message_id: String,
    /// "delivered" | "rejected"
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// events
// ---------------------------------------------------------------------------

/// `events.subscribe`: subscribe to (or filter) a session's event stream.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribeParams {
    /// session id to subscribe to (required unless `scope = "all"` is explicitly set by the owner)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// last observed `session_epoch`; mismatches fall back to `mode = "baseline"`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_epoch: Option<String>,
    /// last delivered `seq` within `session_epoch` for resumptive replay
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seq: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<RegistrationScopeKind>,
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<Vec<String>>,
}

/// How an `events.subscribe` cursor starts streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum SubscribeMode {
    Live,
    Replay,
    Baseline,
}

/// Result of `events.subscribe`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EventsSubscribeResult {
    pub mode: SubscribeMode,
    pub session_epoch: String,
    /// sequence number at which the stream continues (`next_seq` for `live`/`baseline`, `last_seq + 1` for `replay`)
    pub seq: u64,
}

/// `snapshot` notification (mare → host): emitted immediately after an `events.subscribe` reply
/// with `mode = "baseline"`, and immediately after `lagged` when a live cursor is evicted.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct BaselineSnapshotParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub session_epoch: String,
    /// Ring tail sequence number `S` at the moment of the non-yielding cut; following events start at `S + 1`.
    pub seq: u64,
    /// Opaque handle for paging `parts` via `baseline.read`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_id: Option<String>,
    /// Retained state (`events`, `status`, `queue`, `active_turn`, `active_items` skeletons, pending approvals/interactions).
    #[serde(default)]
    pub retained: std::collections::BTreeMap<String, Value>,
    /// Baseline parts available to page via `baseline.read` (e.g. `["entries", "active_items"]`).
    #[serde(default)]
    pub parts: Vec<String>,
}

/// Parameters for `baseline.read`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BaselineReadParams {
    pub baseline_id: String,
    pub part: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Optional maximum number of items to return in one page (>= 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1))]
    pub limit: Option<u64>,
}

/// Wire payload returned by `baseline.read`.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BaselineReadResult {
    pub data: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// `event` notification params — exactly one `mare -p --json` line.
/// An event stream is incomplete and this connection will close.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct StreamFailedParams {
    pub error: String,
}

/// Envelope of an `event` notification: unified across RPC and headless streams.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct EventParams {
    /// kernel event name, e.g. `turn/started`, `item/delta`, `item/completed`, `turn/completed`
    #[serde(rename = "type")]
    pub kind: String,
    /// session the event belongs to
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// opaque in-memory epoch identifier that rotates on every session create, load, or switch
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_epoch: Option<String>,
    /// per-session, per-epoch sequence number (1-based, contiguous)
    #[serde(default)]
    pub seq: u64,
    /// run (prompt) the event belongs to — matches `agent.prompt` `turn_id`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// whether a client that does not recognize this event type may skip it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignorable: Option<bool>,
    #[serde(default)]
    pub data: Value,
}

// ---------------------------------------------------------------------------
// host tools / hooks
// ---------------------------------------------------------------------------

/// Public web capability kind, shared by direct calls and reverse backend registration.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WebKind {
    Search,
    Fetch,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebProvidersParams {
    pub kind: WebKind,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct WebProviderInfo {
    pub provider_id: String,
    pub available: bool,
    #[serde(default)]
    pub unconfined: bool,
}

// ---------------------------------------------------------------------------
// UI callbacks
// ---------------------------------------------------------------------------

/// Cancels an outstanding mare-to-host request. Late replies to this ID are ignored.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CancelRequestParams {
    pub id: RequestId,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiSelectParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub select_id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_right: Option<Value>,
    /// Explanatory lines shown above the options (strings or wire `Node` objects).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub body: Vec<Value>,
    pub items: Vec<UiSelectItem>,
    /// 0-based preselected index
    #[serde(default)]
    pub selected: usize,
    /// Allow checking multiple items before confirming.
    #[serde(default)]
    pub multi: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accept_label: Option<String>,
    /// Defaults to true when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_search: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<UiSelectTab>,
    /// 0-based initial tab index
    #[serde(default)]
    pub tab: usize,
    /// "list" | "confirm"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<UiSelectKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_highlight: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiSelectTab {
    pub name: String,
    pub items: Vec<UiSelectItem>,
    /// 0-based preselected index
    #[serde(default)]
    pub selected: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiSelectKey {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiSelectItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub label: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<bool>,
    /// Visible and searchable, but a conforming UI must not select this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub checked: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<Value>,
}

/// 0-based index/tab, optional action key and filter text, or null index when cancelled/interrupted
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiSelectResult {
    #[serde(default)]
    pub index: Option<usize>,
    /// 0-based checked item indices for a multi-select; an empty array is a valid answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_indices: Option<Vec<usize>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Dismissal reason when cancelled (`"cancelled"` | `"interrupted"`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `item_id` of the chosen item as rendered by the client, so callers using `ui/select/update`
    /// resolve by identity even when a reply races with a live update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiSelectUpdateParams {
    pub select_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<UiSelectItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<UiSelectTab>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiSelectCloseParams {
    pub select_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiInputParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub placeholder: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    #[serde(default)]
    pub secret: bool,
}

/// Result of a text prompt dialog (`ui.input` / `ui.editor`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiTextPromptResult {
    /// null when cancelled
    #[serde(default)]
    pub text: Option<String>,
    /// Dismissal reason when cancelled (`"cancelled"` | `"interrupted"`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiConfirmParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub prompt: String,
    #[serde(default)]
    pub default: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiConfirmResult {
    /// true/false when answered; null when cancelled or interrupted
    #[serde(default)]
    pub confirmed: Option<bool>,
    /// Dismissal reason when cancelled (`"cancelled"` | `"interrupted"`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiNotifyParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub text: String,
    /// info | success | warning | error
    #[serde(default)]
    pub level: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiEditorParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub prefill: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiSetEditorParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub text: String,
    #[serde(default)]
    pub keep_draft: bool,
    #[serde(default)]
    pub prepend: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiEditorHintParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub text: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiEditorConfigParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub busy_placeholder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiWidgetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub widget_id: String,
    pub slot: String,
    #[serde(default)]
    pub order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiStatusParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub status_id: String,
    pub align: String,
    #[serde(default)]
    pub order: i64,
    #[serde(default)]
    pub keep: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiOverlayParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<Value>,
    pub open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiComponentUpdateParams {
    pub session_id: String,
    pub component: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiViewport {
    #[schemars(range(min = 1, max = 10000))]
    pub width: Option<u16>,
    #[schemars(range(min = 1, max = 10000))]
    pub height: Option<u16>,
    /// This consumer can display image data. Does not alter native terminal capabilities.
    pub supports_images: Option<bool>,
    /// This consumer resolves output-backed image references through the same connection.
    pub supports_artifact_images: Option<bool>,
}

impl UiViewport {
    /// Validate optional terminal dimensions against `1..=10000`.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.width.is_some_and(|w| w == 0 || w > 10000) || self.height.is_some_and(|h| h == 0 || h > 10000) {
            return Err("invalid viewport size (expected 1..=10000)");
        }
        Ok(())
    }
}

const NAMED_UI_TAKEOVERS: &[&str] =
    &["select", "confirm", "input", "questions", "editor", "working", "status", "notices", "overlay"];

/// Validate and deduplicate external UI component takeover identifiers.
pub fn validate_takeovers(raw: &[String]) -> std::result::Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for item in raw {
        let trimmed = item.trim();
        let valid = NAMED_UI_TAKEOVERS.contains(&trimmed)
            || trimmed.strip_prefix("widget:").is_some_and(|rest| !rest.is_empty());
        if !valid {
            return Err(format!("unknown takeover component `{item}`"));
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiAttachParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responder: Option<bool>,
    #[serde(default)]
    pub takeover: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub viewport: Option<UiViewport>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiAttachResult {
    pub session_id: String,
    pub session_epoch: String,
    pub responder: bool,
    pub takeover: Vec<String>,
    pub snapshot: UiShellSnapshot,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiDetachParams {
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiDetachResult {
    pub session_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiWorkingSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default)]
    pub background: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiEditorAttachment {
    pub attachment_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiEditorStateParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub text: String,
    pub cursor: usize,
    #[serde(default)]
    pub shell_mode: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_markers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<UiEditorAttachment>,
    #[serde(default)]
    pub external_editor_running: bool,
    pub revision: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiShellSnapshot {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widgets: Vec<UiWidgetParams>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub statuses: Vec<UiStatusParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working: Option<UiWorkingSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_config: Option<UiEditorConfigParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub editor_hint: Option<String>,
    pub editor_state: UiEditorStateParams,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overlays: Vec<UiOverlayParams>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub flags: std::collections::BTreeMap<String, bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommended: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiQuestionItem {
    pub question_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub question: String,
    pub options: Vec<UiQuestionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub multi_select: Option<bool>,
    /// Defaults to true when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_custom: Option<bool>,
    /// Defaults to true when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_skip: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiQuestionsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub questions: Vec<UiQuestionItem>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiQuestionAnswer {
    #[serde(default)]
    pub selected: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UiQuestionsResult {
    #[serde(default)]
    pub answers: std::collections::HashMap<String, UiQuestionAnswer>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

// ---------------------------------------------------------------------------
// schema
// ---------------------------------------------------------------------------

/// Parse a strict `major.minor` version: two non-negative ASCII integers separated by `.`.
pub fn parse_version(version: &str) -> Option<(u64, u64)> {
    let (major, minor) = version.split_once('.')?;
    let part =
        |s: &str| (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u64>().ok()).flatten();
    Some((part(major)?, part(minor)?))
}

/// Is a host's protocol version compatible (strict `major.minor`, same major)?
pub fn compatible(version: &str) -> bool {
    matches!(
        (parse_version(version), parse_version(PROTOCOL_VERSION)),
        (Some((maj, _)), Some((cur, _))) if maj == cur
    )
}

// ---------------------------------------------------------------------------
// approvals and history
// ---------------------------------------------------------------------------

/// An approval decision (Codex `ReviewDecision`): allow-class decisions run the call;
/// `allow_session` / `allow_amend` are reported to the permissions plugin to cache / persist;
/// `abort` denies and ends the current turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    AllowSession,
    AllowAmend,
    Deny,
    Abort,
}

impl ApprovalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::AllowSession => "allow_session",
            Self::AllowAmend => "allow_amend",
            Self::Deny => "deny",
            Self::Abort => "abort",
        }
    }
}

/// Additional rights requested or granted by a scoped permission tool. Paths are native
/// absolute paths in callbacks; write roots must be existing physical directories.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PermissionProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<PermissionNetwork>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PermissionNetwork {
    Enabled(bool),
    Hosts(Vec<String>),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionGrantScope {
    Turn,
    Session,
}
impl PermissionGrantScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Session => "session",
        }
    }
}

/// `approval.request` (mare → host): a tool call or a network host needs approval.
/// No answer (timeout, disconnect, error, invalid result) counts as `deny`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalRequestParams {
    /// session that needs approval
    pub session_id: String,
    /// active turn id, when known
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    /// "tool" or "network"
    pub kind: String,
    /// unique approval lifecycle id (pairs with `approval/requested` and `approval/resolved`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    /// tool call id (kind = tool)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// tool name (kind = tool)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// the call's arguments (kind = tool)
    #[serde(default)]
    pub arguments: Value,
    /// shell command, when the tool runs one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// proposed change (patch / edit), when the tool writes files
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// host:port (kind = network)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub cwd: String,
    /// why approval is needed (from the policy vote)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// "read" | "write" | "external"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect: Option<String>,
    /// runs outside the local sandbox (MCP / RPC-host tools)
    #[serde(default)]
    pub unconfined: bool,
    /// a rule the policy proposes for `allow_amend` (opaque to core)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed_amendment: Option<Value>,
    /// Explicit permission increment requested by this tool call (read/write paths, network hosts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_permissions: Option<PermissionProfile>,
    /// Requested maximum lifetime, when this is a structured permission request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<PermissionGrantScope>,
}

// Omission keeps legacy full-approval behavior; an explicit null must not accidentally
// become an omitted partial grant and authorize the complete request.
fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequestResult {
    pub decision: ApprovalDecision,
    /// Optional structured subset of the requested read/write/network increment.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_present")]
    #[schemars(with = "PermissionProfile")]
    pub permissions: Option<PermissionProfile>,
    /// Lifetime of the grant: turn or session (the request remains an upper bound).
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_present")]
    #[schemars(with = "PermissionGrantScope")]
    pub scope: Option<PermissionGrantScope>,
    /// with `allow_amend`: the rule to persist (opaque to core)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amendment: Option<Value>,
    /// shown to the model when denied
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod cov_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rpc_message_rejects_duplicate_fields_and_non_objects_and_ignores_unknown_fields() {
        // Non-object triggers Visitor::expecting.
        assert!(serde_json::from_str::<Message>("123").is_err());

        // Duplicate params / result / error fields must be rejected.
        for bad in [
            r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{},"params":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":1,"result":2}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"a"},"error":{"code":-1,"message":"b"}}"#,
        ] {
            assert!(serde_json::from_str::<Message>(bad).is_err(), "expected duplicate field error for {bad}");
        }

        // Unknown top-level fields are ignored during deserialization.
        let msg: Message =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":1,"method":"ping","extra_ignored":{"a":1}}"#).unwrap();
        assert_eq!(msg.method.as_deref(), Some("ping"));
    }

    #[test]
    fn rpc_error_code_mapping_data_merging_and_bounds() {
        for (code, expected) in [
            (codes::PARSE_ERROR, "parse_error"),
            (codes::INVALID_REQUEST, "invalid_request"),
            (codes::METHOD_NOT_FOUND, "method_not_found"),
            (codes::INVALID_PARAMS, "invalid_arguments"),
            (codes::CANCELLED, "cancelled"),
            (-32099, "internal"),
        ] {
            let err = RpcError { code, message: "boom".into(), data: None }.normalized();
            assert_eq!(err.error_code(), Some(expected), "code={code}");
        }

        // Non-object error data is wrapped under `data` during normalization.
        let err_scalar =
            RpcError { code: codes::INVALID_PARAMS, message: "m".into(), data: Some(json!("raw")) }.normalized();
        assert_eq!(err_scalar.error_code(), Some("invalid_arguments"));
        assert_eq!(err_scalar.data.as_ref().unwrap()["data"], "raw");

        // Object `data.data` merges extra sibling fields; non-object `data.data` wraps sibling fields.
        let err_merge_obj = RpcError {
            code: codes::INVALID_PARAMS,
            message: "m".into(),
            data: Some(json!({"code": "invalid_arguments", "data": {"a": 1}, "extra": 2})),
        }
        .normalized();
        assert_eq!(err_merge_obj.data.as_ref().unwrap()["data"], json!({"a": 1, "extra": 2}));

        let err_merge_scalar = RpcError {
            code: codes::INVALID_PARAMS,
            message: "m".into(),
            data: Some(json!({"code": "invalid_arguments", "data": "inner", "extra": 2})),
        }
        .normalized();
        assert_eq!(err_merge_scalar.data.as_ref().unwrap()["data"], json!({"data": "inner", "extra": 2}));

        // with_data & project_with redact both message and data.
        let mut proj = RpcError {
            code: codes::INVALID_PARAMS,
            message: "secret".into(),
            data: Some(json!({"code": "invalid_arguments"})),
        }
        .with_data(json!({"tok": "secret"}));
        proj.project_with(|s| s.replace("secret", "***"), |v| *v = json!({"redacted": true}));
        assert_eq!(proj.message, "***");
        assert_eq!(proj.data, Some(json!({"redacted": true})));

        // UiViewport bounds validation and ApprovalDecision::AllowAmend.
        let bad_vp = UiViewport { width: Some(0), height: Some(24), ..Default::default() };
        assert!(bad_vp.validate().is_err());
        let huge_vp = UiViewport { width: Some(10001), height: Some(24), ..Default::default() };
        assert!(huge_vp.validate().is_err());
        assert_eq!(ApprovalDecision::AllowAmend.as_str(), "allow_amend");
    }
}
