//! Single source of truth for core protocol built-in events.
//!
//! Each entry in [`protocol_events!`] declares an event's wire name, payload type, dispatch
//! mode, delivery category, client ignorability, retention, listener error handling, documentation,
//! and a schema-validating example payload. [`PROTOCOL_EVENT_NAMES`], [`ProtocolEvent`],
//! [`PROTOCOL_EVENTS`], schema entries, tap backpressure classification, and `events.list` derive
//! from this table. Domain events live in each owning service contract's `events` section.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::events;

/// Dispatch mode declared for a protocol event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventMode {
    Emit,
    Parallel,
    Serial,
    Bail,
    Waterfall,
    Policy,
    Rewrite,
}

/// Execution scope of a registered event (`host`, `session`, or `turn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    Host,
    #[default]
    Session,
    Turn,
}

impl EventScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Session => "session",
            Self::Turn => "turn",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "host" => Some(Self::Host),
            "session" => Some(Self::Session),
            "turn" => Some(Self::Turn),
            _ => None,
        }
    }
}

/// Stream delivery class under consumer backpressure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventDelivery {
    Required,
    Lossy,
}

/// How listener errors are handled during dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventOnError {
    Continue,
    Abort,
}

/// Static declaration of one built-in public event.
#[derive(Clone, Copy, Debug)]
pub struct EventDefinition {
    pub name: &'static str,
    pub mode: EventMode,
    pub scope: EventScope,
    pub delivery: EventDelivery,
    pub ignorable: bool,
    pub retain: bool,
    pub on_error: EventOnError,
    pub doc: &'static str,
    pub host_emitted: Option<&'static str>,
    pub data_schema: fn() -> Value,
    pub result_schema: Option<fn() -> Value>,
    pub example: fn() -> Value,
}

impl EventDefinition {
    /// Materialize this static declaration into an `events.list` entry.
    pub fn to_info(&self) -> EventInfo {
        EventInfo {
            name: self.name.to_string(),
            mode: self.mode,
            scope: self.scope,
            delivery: self.delivery,
            ignorable: self.ignorable,
            retain: self.retain,
            on_error: self.on_error,
            public: true,
            doc: self.doc.to_string(),
            data_schema: crate::schema::expand_local_refs((self.data_schema)()),
            result_schema: self.result_schema.map(|f| crate::schema::expand_local_refs(f())),
            example: (self.example)(),
        }
    }
}

fn default_public() -> bool {
    true
}

/// Serialized event declaration returned by `events.list`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventInfo {
    pub name: String,
    pub mode: EventMode,
    #[serde(default)]
    pub scope: EventScope,
    pub delivery: EventDelivery,
    pub ignorable: bool,
    pub retain: bool,
    pub on_error: EventOnError,
    #[serde(default = "default_public")]
    pub public: bool,
    pub doc: String,
    pub data_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_schema: Option<Value>,
    pub example: Value,
}

/// `events.list` result. Currently returns the built-in `protocol_events!` registry entries;
/// service contract `events` declarations are not supported yet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EventsListResult {
    pub data: Vec<EventInfo>,
}

/// Check whether a list of event names contains any duplicate.
pub const fn has_duplicate_names(names: &[&str]) -> bool {
    let mut i = 0;
    while i < names.len() {
        let mut j = i + 1;
        while j < names.len() {
            let a = names[i].as_bytes();
            let b = names[j].as_bytes();
            if a.len() == b.len() {
                let mut k = 0;
                let mut same = true;
                while k < a.len() {
                    if a[k] != b[k] {
                        same = false;
                        break;
                    }
                    k += 1;
                }
                if same {
                    return true;
                }
            }
            j += 1;
        }
        i += 1;
    }
    false
}

macro_rules! opt_result_schema {
    () => {
        None
    };
    ($result:ty) => {
        Some(crate::schema::s::<$result> as fn() -> Value)
    };
}

macro_rules! opt_str {
    () => {
        None
    };
    ($s:expr) => {
        Some($s)
    };
}

macro_rules! opt_scope {
    () => {
        EventScope::Session
    };
    ($scope:ident) => {
        EventScope::$scope
    };
}

macro_rules! protocol_events {
    (
        $(
            $name:literal => $variant:ident ($payload:ty $(, result = $result:ty )?) {
                mode: $mode:ident,
                $(scope: $scope:ident,)?
                delivery: $delivery:ident,
                ignorable: $ignorable:expr,
                retain: $retain:expr,
                on_error: $on_error:ident,
                doc: $doc:expr,
                $(host_emitted: $host_emitted:expr,)?
                example: $example:expr $(,)?
            }
        ),* $(,)?
    ) => {
        /// Core built-in protocol event names.
        pub const PROTOCOL_EVENT_NAMES: &[&str] = &[ $($name),* ];

        /// Discriminated core built-in protocol events.
        #[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
        #[serde(tag = "type", content = "data")]
        pub enum ProtocolEvent {
            $(
                #[serde(rename = $name)]
                $variant($payload),
            )*
        }

        /// Complete table of protocol-level public event declarations.
        pub const PROTOCOL_EVENTS: &[EventDefinition] = &[
            $(
                EventDefinition {
                    name: $name,
                    mode: EventMode::$mode,
                    scope: opt_scope!($($scope)?),
                    delivery: EventDelivery::$delivery,
                    ignorable: $ignorable,
                    retain: $retain,
                    on_error: EventOnError::$on_error,
                    doc: $doc,
                    host_emitted: opt_str!($($host_emitted)?),
                    data_schema: crate::schema::s::<$payload> as fn() -> Value,
                    result_schema: opt_result_schema!($($result)?),
                    example: || $example,
                },
            )*
        ];

        const _: () = assert!(!has_duplicate_names(PROTOCOL_EVENT_NAMES), "duplicate event name in protocol_events!");
    };
}

protocol_events! {
    // Lifecycle boundaries & non-delta agent items: Required (`ignorable: false, retain: false`).
    "turn/started" => TurnStarted(events::TurnStarted) {
        mode: Emit,
        scope: Turn,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Agent turn started.",
        example: json!({"turn_id": "t-1", "trigger": "prompt"}),
    },
    "turn/completed" => TurnCompleted(events::TurnCompleted) {
        mode: Emit,
        scope: Turn,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Agent turn completed after run-end durability confirmation.",
        example: json!({"turn_id": "t-1", "status": "completed", "usage": {}}),
    },
    "step/started" => StepStarted(events::StepStarted) {
        mode: Emit,
        scope: Turn,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Model step started within a turn.",
        example: json!({"turn_id": "t-1", "step_id": "s-1", "turn": 1, "attempt": 0}),
    },
    "step/completed" => StepCompleted(events::StepCompleted) {
        mode: Emit,
        scope: Turn,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Model step finished (completed, failed, or retrying).",
        example: json!({"turn_id": "t-1", "step_id": "s-1", "status": "completed", "will_retry": false}),
    },
    "item/started" => ItemStarted(events::ItemStarted) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Semantic item (message, tool call, custom record, or compaction) started. Oversized wire payloads are bounded at the outbound tap (`truncated: true` on truncated items/blocks; fetch full content via `session.get_item`).",
        example: json!({"turn_id": "t-1", "item_id": "it-1", "item": {"type": "assistant_message"}}),
    },
    "item/delta" => ItemDelta(events::ItemDelta) {
        mode: Emit,
        scope: Turn,
        delivery: Required,
        ignorable: true,
        retain: false,
        on_error: Continue,
        doc: "Append-only incremental delta for an in-flight item.",
        example: json!({"turn_id": "t-1", "item_id": "it-1", "kind": "text", "delta": "hi", "offset": 0}),
    },
    "item/completed" => ItemCompleted(events::ItemCompleted) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Authoritative completed item. For `tool_call`, `item.message` is the sole carrier of `content`/`details`. Oversized wire payloads project images to `{ type: \"image\", mime, path?, bytes, truncated: true }`, truncate long strings with `truncated: true`, or fall back to `item_omitted: true`; fetch full content via `session.get_item`.",
        example: json!({
            "turn_id": "t-1",
            "item_id": "c-1",
            "status": "completed",
            "item": {
                "type": "tool_call",
                "tool_call_id": "c-1",
                "name": "read",
                "status": "completed",
                "message": {"role": "tool_result", "tool_call_id": "c-1", "tool_name": "read", "content": [], "is_error": false},
                "result_status": "success",
                "execution_status": "unknown",
            },
        }),
    },
    "queue/changed" => QueueChanged(events::QueueChanged) {
        mode: Emit,
        delivery: Lossy,
        ignorable: false,
        retain: true,
        on_error: Continue,
        doc: "Agent steer/follow-up queue snapshot changed.",
        example: json!({"items": [{"input_id": "q-1", "lane": "steer", "text": "stop"}]}),
    },
    // Advisory UI/diagnostic notices: unknown clients may skip (`ignorable: true`) and bursts drop under pressure (`Lossy`).
    "host/notice" => Notice(events::Notice) {
        mode: Emit,
        delivery: Lossy,
        ignorable: true,
        retain: false,
        on_error: Continue,
        doc: "Informational or warning notice from the agent or host.",
        host_emitted: "crates/mare-host/src/frontend.rs",
        example: json!({"level": "info", "text": "ok"}),
    },
    "subagent/started" => SubagentStarted(events::SubagentStarted) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Subagent run started.",
        host_emitted: "crates/mare-host/src/approvals/child.rs",
        example: json!({"subagent_id": "child-1", "parent_session_id": "parent-1"}),
    },
    "subagent/progress" => SubagentProgress(events::SubagentProgress) {
        mode: Emit,
        delivery: Lossy,
        ignorable: true,
        retain: false,
        on_error: Continue,
        doc: "Latest coalesced progress snapshot for a subagent.",
        host_emitted: "crates/mare-host/src/approvals/child.rs",
        example: json!({"subagent_id": "child-1", "parent_session_id": "parent-1", "progress": {"state": "working"}}),
    },
    "subagent/completed" => SubagentCompleted(events::SubagentCompleted) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Subagent run completed.",
        host_emitted: "crates/mare-host/src/approvals/child.rs",
        example: json!({"subagent_id": "child-1", "status": "completed", "parent_session_id": "parent-1"}),
    },
    "approval/requested" => ApprovalRequested(events::ApprovalRequested) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Approval prompt opened or requested for an action.",
        example: json!({"approval_id": "ap-1", "category": "tool", "tool_call_id": "c-1", "snapshot": "snap-1"}),
    },
    "approval/resolved" => ApprovalResolved(events::ApprovalResolved) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Approval prompt resolved with its decision outcome.",
        example: json!({
            "approval_id": "ap-1",
            "category": "tool",
            "tool_call_id": "c-1",
            "result": {"call": {"tool_call_id": "c-1", "name": "write"}, "decision": "allow", "source": "user"},
        }),
    },
    "status/changed" => StatusChanged(events::StatusChanged) {
        mode: Emit,
        scope: Host,
        delivery: Lossy,
        ignorable: false,
        retain: true,
        on_error: Continue,
        doc: "Unified agent status changed.",
        example: json!({"state": "idle", "message": "ready", "session": {}, "seq": 1}),
    },
    "interaction/requested" => InteractionRequested(events::InteractionRequested) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Interactive prompt opened and waiting for a responder.",
        example: json!({"interaction_id": "int-1", "kind": "select", "title": "Pick"}),
    },
    "interaction/resolved" => InteractionResolved(events::InteractionResolved) {
        mode: Emit,
        delivery: Required,
        ignorable: false,
        retain: false,
        on_error: Continue,
        doc: "Interactive prompt finished with answered, cancelled, or unavailable.",
        example: json!({"interaction_id": "int-1", "kind": "select", "outcome": "answered"}),
    },
}

/// Event wire name constant for unified process status changes (carried inside `event` envelope).
pub const EVENT_STATUS_CHANGED: &str = "status/changed";
pub const EVENT_NOTICE: &str = "host/notice";
pub const EVENT_QUEUE_CHANGED: &str = "queue/changed";
pub const EVENT_TURN_STARTED: &str = "turn/started";
pub const EVENT_TURN_COMPLETED: &str = "turn/completed";
pub const EVENT_STEP_STARTED: &str = "step/started";
pub const EVENT_STEP_COMPLETED: &str = "step/completed";
pub const EVENT_ITEM_STARTED: &str = "item/started";
pub const EVENT_ITEM_DELTA: &str = "item/delta";
pub const EVENT_ITEM_COMPLETED: &str = "item/completed";
pub const EVENT_SUBAGENT_STARTED: &str = "subagent/started";
pub const EVENT_SUBAGENT_PROGRESS: &str = "subagent/progress";
pub const EVENT_SUBAGENT_COMPLETED: &str = "subagent/completed";

/// Whether `name` is one of the static core protocol events in [`PROTOCOL_EVENTS`].
pub fn is_protocol_event(name: &str) -> bool {
    PROTOCOL_EVENT_NAMES.contains(&name)
}

/// Look up a built-in protocol event declaration by wire name.
pub fn find(name: &str) -> Option<&'static EventDefinition> {
    PROTOCOL_EVENTS.iter().find(|def| def.name == name)
}

/// Scope (`host`, `session`, or `turn`) of a built-in protocol event, if declared in
/// [`PROTOCOL_EVENTS`].
pub fn scope_of(name: &str) -> Option<EventScope> {
    find(name).map(|def| def.scope)
}

/// Delivery class of a built-in protocol event, if declared in [`PROTOCOL_EVENTS`].
pub fn delivery_of(name: &str) -> Option<EventDelivery> {
    find(name).map(|def| def.delivery)
}

/// Whether a built-in protocol event is marked ignorable (safe for clients to skip when unknown
/// or delta-filtered).
pub fn is_ignorable(name: &str) -> bool {
    find(name).is_some_and(|def| def.ignorable)
}

/// Materialize all built-in protocol event declarations for `events.list`.
pub fn list_events() -> Vec<EventInfo> {
    PROTOCOL_EVENTS.iter().map(EventDefinition::to_info).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{events, schema};
    use std::collections::BTreeSet;

    #[test]
    fn duplicate_event_names_are_rejected() {
        assert!(has_duplicate_names(&["turn/started", "turn/completed", "turn/started"]));
        assert!(!has_duplicate_names(PROTOCOL_EVENT_NAMES));
    }

    fn contains_schema_keyword(node: &Value, needle: &str) -> bool {
        match node {
            Value::Object(map) => {
                if map.contains_key(needle) {
                    return true;
                }
                map.iter()
                    .filter(|(k, _)| !matches!(k.as_str(), "default" | "examples" | "const" | "enum"))
                    .any(|(_, v)| contains_schema_keyword(v, needle))
            }
            Value::Array(arr) => arr.iter().any(|v| contains_schema_keyword(v, needle)),
            _ => false,
        }
    }

    #[test]
    fn every_public_event_has_one_declaration_with_schema_delivery_ignorable_and_example() {
        let catalog = schema::schema();
        let schema_public: Vec<&str> = catalog["headless"]["protocol_events"]
            .as_array()
            .expect("headless.protocol_events must be an array")
            .iter()
            .filter_map(Value::as_str)
            .collect();

        let table_names: Vec<&str> = PROTOCOL_EVENTS.iter().map(|def| def.name).collect();
        let unique: BTreeSet<&str> = table_names.iter().copied().collect();
        assert_eq!(unique.len(), table_names.len(), "PROTOCOL_EVENTS must not contain duplicate names");
        assert_eq!(table_names, PROTOCOL_EVENT_NAMES, "PROTOCOL_EVENTS must match PROTOCOL_EVENT_NAMES 1:1");
        assert_eq!(schema_public, table_names, "schema headless.protocol_events must match PROTOCOL_EVENTS");

        assert!(!table_names.contains(&"hook/start"), "hook/start must stay internal");
        assert!(!table_names.contains(&"hook/end"), "hook/end must stay internal");
        assert!(find("unknown/event").is_none());
        assert_eq!(delivery_of("unknown/event"), None);
        assert!(!is_ignorable("unknown/event"));

        let listed = list_events();
        assert_eq!(listed.len(), PROTOCOL_EVENTS.len());

        for (def, info) in PROTOCOL_EVENTS.iter().zip(listed.iter()) {
            assert_eq!(info, &def.to_info());
            assert_eq!(info.name, def.name);
            assert_eq!(info.mode, EventMode::Emit);
            assert_eq!(info.on_error, EventOnError::Continue);
            assert_eq!(info.retain, def.name.ends_with("/changed") || def.name.ends_with("-changed"));
            assert_eq!(info.result_schema, None);
            assert!(!def.doc.trim().is_empty(), "{} must have non-empty doc", def.name);
            let data_schema = (def.data_schema)();
            assert!(data_schema.is_object(), "{} data_schema must be a JSON Schema object: {data_schema}", def.name);
            assert!(
                !contains_schema_keyword(&info.data_schema, "$ref"),
                "{} expanded data_schema still has $ref",
                def.name
            );
            assert!(
                !contains_schema_keyword(&info.data_schema, "$defs"),
                "{} expanded data_schema still has $defs",
                def.name
            );
            let example = (def.example)();
            assert!(
                events::validate_protocol(def.name, &example).is_ok(),
                "{} example must pass validate_protocol",
                def.name
            );
            assert!(
                schema::validate_schema_at(&data_schema, "#", &example).is_ok(),
                "{} example must validate against its data_schema",
                def.name
            );
            assert!(
                schema::validate_schema_at(&info.data_schema, "#", &example).is_ok(),
                "{} example must validate against expanded data_schema",
                def.name
            );
            let envelope = json!({ "type": def.name, "seq": 1, "data": example });
            for pointer in ["#/notifications/event", "#/headless/line"] {
                assert!(
                    schema::validate_schema_at(&catalog, pointer, &envelope).is_ok(),
                    "{} example failed {pointer}",
                    def.name
                );
            }
            let builtin = json!({ "type": def.name, "data": (def.example)() });
            assert!(
                schema::validate_schema_at(&catalog, "#/headless/protocol_event", &builtin).is_ok(),
                "{} example failed #/headless/protocol_event",
                def.name
            );
            assert!(contains_schema_keyword(&json!({"$ref": "#/$defs/X"}), "$ref"));
            // `lossy` is only permitted when `retain` or `ignorable` is true.
            if def.delivery == EventDelivery::Lossy {
                assert!(
                    def.retain || def.ignorable,
                    "{} violates invariant: Lossy requires retain or ignorable",
                    def.name
                );
            }
            let expected_delivery =
                if matches!(def.name, "host/notice" | "queue/changed" | "status/changed" | "subagent/progress") {
                    EventDelivery::Lossy
                } else {
                    EventDelivery::Required
                };
            assert_eq!(def.delivery, expected_delivery, "{} delivery mismatch", def.name);
            assert_eq!(delivery_of(def.name), Some(expected_delivery));
            let expected_ignorable = matches!(def.name, "item/delta" | "host/notice" | "subagent/progress");
            assert_eq!(def.ignorable, expected_ignorable, "{} ignorable mismatch", def.name);
            assert_eq!(is_ignorable(def.name), expected_ignorable);
        }
    }
}
