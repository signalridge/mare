//! Protocol JSON Schema catalog generation, snapshot verification, and validation helpers.

use crate::{
    ApprovalRequestParams, ApprovalRequestResult, Attachment, BaselineReadParams, BaselineReadResult,
    BaselineSnapshotParams, CallbackCallParams, CallbackUpdateParams, CancelRequestParams, ConfigDescribeParams,
    ConfigDescribeResult, ContentPart, EmptyParams, EmptyResult, EventParams, EventsListResult, EventsSubscribeParams,
    EventsSubscribeResult, FlagsGetParams, FlagsGetResult, FlagsListResult, FsChangedNotification, InitializeParams,
    InitializeResult, MarkdownPresentation, McpElicitationParams, McpElicitationResult, Message, OptionsGetParams,
    OptionsSetParams, OutputImageKey, PROTOCOL_VERSION, PeerInfo, PeerMessageParams, PeerMessageResult,
    PeerMessageStatus, SetCapabilitiesParams, SetCapabilitiesResult, StateCompareExchangeRequest,
    StateCompareExchangeResult, StateSnapshotRequest, StateSnapshotResult, StreamFailedParams, ToolResultBlock,
    ToolResultContent, UiAttachParams, UiAttachResult, UiComponentUpdateParams, UiConfirmParams, UiConfirmResult,
    UiDetachParams, UiDetachResult, UiEditorConfigParams, UiEditorHintParams, UiEditorParams, UiEditorStateParams,
    UiInputParams, UiNotifyParams, UiOverlayParams, UiQuestionsParams, UiQuestionsResult, UiSelectCloseParams,
    UiSelectParams, UiSelectResult, UiSelectUpdateParams, UiSetEditorParams, UiStatusParams, UiTextPromptResult,
    UiWidgetParams, events, headless, methods, services,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Peer-caller admission declared on a protocol method contract.
/// Default when omitted or unrecognized is [`MethodAccess::Primary`] (owner-only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MethodAccess {
    /// Owner / primary connection only (`"primary"`): never grantable to a peer.
    Primary,
    /// Peer connection may call only when granted in `peer.allow` (`"peer"`).
    Peer,
    /// Open to any peer connection without listing in `peer.allow` (`"peer_default"`).
    PeerDefault,
}

impl MethodAccess {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Peer => "peer",
            Self::PeerDefault => "peer_default",
        }
    }

    pub fn from_schema_value(v: Option<&str>) -> Self {
        match v {
            Some("peer_default") => Self::PeerDefault,
            Some("peer") => Self::Peer,
            _ => Self::Primary,
        }
    }
}

pub(crate) fn s<T: JsonSchema>() -> Value {
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or(Value::Null)
}

/// Expand all local `#/$defs/...` references in-place and remove `$defs`
/// (runtime event schemas are self-contained without `$ref`).
pub fn expand_local_refs(mut root: Value) -> Value {
    fn inline(node: &mut Value, defs: &Value, visiting: &mut Vec<String>) {
        match node {
            Value::Object(obj) => {
                if let Some(Value::String(reference)) = obj.get("$ref")
                    && let Some(name) = reference.strip_prefix("#/$defs/")
                    && !visiting.iter().any(|seen| seen == name)
                    && let Some(target) = defs.get(name)
                {
                    let mut expanded = target.clone();
                    visiting.push(name.to_string());
                    inline(&mut expanded, defs, visiting);
                    visiting.pop();
                    let extras: Vec<(String, Value)> =
                        obj.iter().filter(|(k, _)| k.as_str() != "$ref").map(|(k, v)| (k.clone(), v.clone())).collect();
                    if let Value::Object(mut target_obj) = expanded {
                        for (k, mut v) in extras {
                            inline(&mut v, defs, visiting);
                            target_obj.entry(k).or_insert(v);
                        }
                        *node = Value::Object(target_obj);
                    } else {
                        *node = expanded;
                    }
                    return;
                }
                for (key, child) in obj.iter_mut() {
                    if matches!(key.as_str(), "default" | "examples" | "const" | "enum") {
                        continue;
                    }
                    inline(child, defs, visiting);
                }
            }
            Value::Array(arr) => {
                for child in arr.iter_mut() {
                    inline(child, defs, visiting);
                }
            }
            _ => {}
        }
    }

    let defs = root.as_object_mut().and_then(|o| o.remove("$defs")).unwrap_or(Value::Null);
    let mut visiting = Vec::new();
    inline(&mut root, &defs, &mut visiting);
    root
}

/// Expanded protocol component schemas passed to contract files (`components` table).
/// Self-contained JSON Schemas normalized to the contract schema subset (no `$ref`, `$schema`, or `anyOf`).
pub fn protocol_components() -> Value {
    fn normalize(node: &mut Value) {
        match node {
            Value::Object(obj) => {
                obj.remove("$schema");
                obj.remove("format");
                obj.remove("discriminator");
                if let Some(any_of) = obj.remove("anyOf") {
                    obj.entry("oneOf").or_insert(any_of);
                }
                for (key, child) in obj.iter_mut() {
                    if matches!(key.as_str(), "default" | "examples" | "const" | "enum") {
                        continue;
                    }
                    normalize(child);
                }
            }
            Value::Array(arr) => {
                for child in arr.iter_mut() {
                    normalize(child);
                }
            }
            _ => {}
        }
    }
    let strip = |v: Value| {
        let mut expanded = expand_local_refs(v);
        normalize(&mut expanded);
        expanded
    };
    serde_json::json!({
        "attachment": strip(s::<Attachment>()),
        "content_part": strip(s::<ContentPart>()),
        "tool_result_block": strip(s::<ToolResultBlock>()),
        "tool_result_content": strip(s::<ToolResultContent>()),
        "error": serde_json::json!({
            "type": "object",
            "properties": {
                "code": { "type": "string" },
                "message": { "type": "string" },
                "data": {}
            },
            "required": ["code", "message"],
            "additionalProperties": false
        }),
    })
}

/// A discriminator-bound schema: extensions stay open but cannot match an invalid protocol event.
fn event_line_schema(headless: bool) -> Value {
    let mut builtin = s::<events::ProtocolEvent>();
    let mut variants = builtin.get_mut("oneOf").and_then(Value::as_array_mut).map(std::mem::take).unwrap_or_default();
    let mut excluded: Vec<&str> = crate::event_registry::PROTOCOL_EVENT_NAMES.to_vec();
    if headless {
        excluded.extend(["session", "turn.failed", "settled", "lagged", "dropped"]);
        for (name, data) in [
            ("session", s::<headless::SessionRecord>()),
            ("turn.failed", s::<headless::TurnFailed>()),
            ("lagged", s::<headless::Lagged>()),
            ("dropped", s::<headless::Dropped>()),
            ("settled", s::<headless::Settled>()),
        ] {
            variants.push(serde_json::json!({"type":"object", "properties":{"type":{"const":name}, "data":data}, "required":["type","data"]}));
        }
    }
    variants.push(serde_json::json!({"type":"object", "properties":{"type":{"type":"string", "not":{"enum":excluded}}, "data":{}}, "required":["type","data"]}));
    builtin["oneOf"] = Value::Array(variants);
    builtin
}

fn rpc_event_schema() -> Value {
    let mut line = event_line_schema(false);
    let variants = line.as_object_mut().and_then(|o| o.remove("oneOf")).unwrap_or(Value::Null);
    let mut base = s::<EventParams>();
    base["allOf"] = serde_json::json!([{ "oneOf": variants }]);
    if let Some(defs) = line.as_object_mut().and_then(|o| o.remove("$defs")) {
        base["$defs"] = defs;
    }
    base
}

/// Domain error codes returned by built-in protocol methods (`baseline.read`).
pub const PROTOCOL_METHOD_ERRORS: &[(&str, &str)] = &[
    ("baseline.stale", "the baseline handle is unknown, expired, superseded, completed, or from another session epoch"),
    ("baseline.unknown_part", "the requested baseline part is not valid for this baseline"),
    ("baseline.item_too_large", "a single baseline item exceeds the page byte budget"),
];

/// The whole protocol as one JSON document: version, host→mare methods (params/result),
/// mare→host notifications and callbacks. Printed by `mare rpc schema`.
pub fn schema() -> Value {
    use methods::*;
    let m = |params: Value, result: Value, access: MethodAccess| serde_json::json!({ "params": params, "result": result, "access": access.as_str() });
    let m_errors = |params: Value, result: Value, access: MethodAccess, errors: &[&str]| serde_json::json!({ "params": params, "result": result, "access": access.as_str(), "errors": errors });
    let m_admin = |params: Value, result: Value, access: MethodAccess| serde_json::json!({ "params": params, "result": result, "access": access.as_str(), "admin": true });
    let cb = |params: Value, result: Value| serde_json::json!({ "params": params, "result": result });
    let mut catalog = serde_json::json!({
        "$comment": "mare RPC protocol — JSON-RPC 2.0, one message per line on stdin/stdout (mare --rpc)",
        "protocol_version": PROTOCOL_VERSION,
        "envelope": s::<Message>(),
        "methods": {
            SERVICES_LIST: m(s::<EmptyParams>(), s::<services::ServicesListResult>(), MethodAccess::Peer),
            SERVICES_REGISTER: m_admin(s::<services::RegisterParams>(), s::<services::Registration>(), MethodAccess::Primary),
            REGISTRATIONS_RELEASE: m(s::<services::ReleaseParams>(), s::<services::ReleaseResult>(), MethodAccess::Peer),
            INITIALIZE: m(s::<InitializeParams>(), s::<InitializeResult>(), MethodAccess::PeerDefault),
            CONNECTION_SET_CAPABILITIES: m(s::<SetCapabilitiesParams>(), s::<SetCapabilitiesResult>(), MethodAccess::PeerDefault),
            OPTIONS_GET: m(s::<OptionsGetParams>(), serde_json::json!({ "type": "object" }), MethodAccess::Peer),
            OPTIONS_SET: m_admin(s::<OptionsSetParams>(), s::<EmptyResult>(), MethodAccess::Primary),
            CONFIG_DESCRIBE: m(s::<ConfigDescribeParams>(), s::<ConfigDescribeResult>(), MethodAccess::Peer),
            STATE_SNAPSHOT: m(s::<StateSnapshotRequest>(), s::<StateSnapshotResult>(), MethodAccess::Peer),
            STATE_COMPARE_EXCHANGE: m(s::<StateCompareExchangeRequest>(), s::<StateCompareExchangeResult>(), MethodAccess::Primary),
            SHUTDOWN: m(s::<EmptyParams>(), s::<EmptyResult>(), MethodAccess::PeerDefault),
            EVENTS_LIST: m(s::<EmptyParams>(), s::<EventsListResult>(), MethodAccess::Peer),
            EVENTS_SUBSCRIBE: m(s::<EventsSubscribeParams>(), s::<EventsSubscribeResult>(), MethodAccess::PeerDefault),
            BASELINE_READ: m_errors(
                s::<BaselineReadParams>(),
                s::<BaselineReadResult>(),
                MethodAccess::Peer,
                &["baseline.stale", "baseline.unknown_part", "baseline.item_too_large"],
            ),
            FLAGS_LIST: m(s::<EmptyParams>(), s::<FlagsListResult>(), MethodAccess::Peer),
            FLAGS_GET: m(s::<FlagsGetParams>(), s::<FlagsGetResult>(), MethodAccess::Peer),
            PEER_INFO: m(s::<EmptyParams>(), s::<PeerInfo>(), MethodAccess::PeerDefault),
            PEER_MESSAGE: m(s::<PeerMessageParams>(), s::<PeerMessageResult>(), MethodAccess::PeerDefault),
            UI_ATTACH: m(s::<UiAttachParams>(), s::<UiAttachResult>(), MethodAccess::Primary),
            UI_DETACH: m(s::<UiDetachParams>(), s::<UiDetachResult>(), MethodAccess::Primary),
        },
        "client_notifications": {
            CALLBACK_UPDATE: s::<CallbackUpdateParams>(),
            CANCEL_REQUEST: s::<CancelRequestParams>(),
        },
        "notifications": {
            CANCEL_REQUEST: s::<CancelRequestParams>(),
            EVENT: rpc_event_schema(),
            LAGGED: s::<headless::Lagged>(),
            SNAPSHOT: s::<BaselineSnapshotParams>(),
            DROPPED: s::<headless::Dropped>(),
            STREAM_FAILED: s::<StreamFailedParams>(),
            UI_NOTIFY: s::<UiNotifyParams>(),
            UI_SELECT_UPDATE: s::<UiSelectUpdateParams>(),
            UI_SELECT_CLOSE: s::<UiSelectCloseParams>(),
            UI_SET_EDITOR: s::<UiSetEditorParams>(),
            UI_EDITOR_HINT: s::<UiEditorHintParams>(),
            UI_EDITOR_CONFIG: s::<UiEditorConfigParams>(),
            UI_WIDGET: s::<UiWidgetParams>(),
            UI_STATUS: s::<UiStatusParams>(),
            UI_OVERLAY: s::<UiOverlayParams>(),
            UI_COMPONENT_UPDATE: s::<UiComponentUpdateParams>(),
            UI_EDITOR_STATE: s::<UiEditorStateParams>(),
            MARKDOWN_CHANGED: s::<MarkdownPresentation>(),
            PEER_MESSAGE_STATUS: s::<PeerMessageStatus>(),
            FS_CHANGED: s::<FsChangedNotification>(),
        },
        "ui_image_resource": s::<mare_shared::image_resource::Descriptor>(),
        "ui_output_image_key": s::<OutputImageKey>(),
        "callbacks": {
            CALLBACK_CALL: cb(s::<CallbackCallParams>(), serde_json::json!({})),
            APPROVAL_REQUEST: cb(s::<ApprovalRequestParams>(), s::<ApprovalRequestResult>()),
            MCP_ELICITATION: cb(s::<McpElicitationParams>(), s::<McpElicitationResult>()),
            UI_SELECT: cb(s::<UiSelectParams>(), s::<UiSelectResult>()),
            UI_INPUT: cb(s::<UiInputParams>(), s::<UiTextPromptResult>()),
            UI_CONFIRM: cb(s::<UiConfirmParams>(), s::<UiConfirmResult>()),
            UI_EDITOR: cb(s::<UiEditorParams>(), s::<UiTextPromptResult>()),
            UI_QUESTIONS: cb(s::<UiQuestionsParams>(), s::<UiQuestionsResult>()),
        },
        "headless": {
            "$comment": "mare --json: first line session, public events, turn.failed on failure, settled last",
            "line": event_line_schema(true),
            "protocol_event": s::<events::ProtocolEvent>(),
            "session": s::<headless::SessionRecord>(),
            "turn_failed": s::<headless::TurnFailed>(),
            "lagged": s::<headless::Lagged>(),
            "dropped": s::<headless::Dropped>(),
            "settled": s::<headless::Settled>(),
            "protocol_events": crate::event_registry::PROTOCOL_EVENT_NAMES,
        },
        "errors": crate::errors::CORE
            .iter()
            .chain(PROTOCOL_METHOD_ERRORS.iter())
            .map(|(code, meaning)| (code.to_string(), serde_json::json!({ "number": crate::errors::rpc_number(code), "meaning": meaning })))
            .collect::<serde_json::Map<_, _>>(),
    });
    catalog_references(&mut catalog, "", "");
    catalog
}

/// Every protocol method declared in [`schema`], mapped to its admission level.
pub fn protocol_methods() -> &'static BTreeMap<String, MethodAccess> {
    static METHODS: OnceLock<BTreeMap<String, MethodAccess>> = OnceLock::new();
    METHODS.get_or_init(|| {
        schema()["methods"]
            .as_object()
            .expect("schema.methods must be an object")
            .iter()
            .map(|(name, entry)| {
                let access = MethodAccess::from_schema_value(entry.get("access").and_then(Value::as_str));
                (name.clone(), access)
            })
            .collect()
    })
}

/// Admission rule declared on protocol method `method`. Undeclared or unknown methods default to
/// [`MethodAccess::Primary`] (owner-only).
pub fn method_access(method: &str) -> MethodAccess {
    protocol_methods().get(method).copied().unwrap_or(MethodAccess::Primary)
}

/// Protocol methods declaring `"admin": true` in [`schema`].
pub fn protocol_admin_methods() -> &'static std::collections::BTreeSet<String> {
    static ADMIN: OnceLock<std::collections::BTreeSet<String>> = OnceLock::new();
    ADMIN.get_or_init(|| {
        schema()["methods"]
            .as_object()
            .expect("schema.methods must be an object")
            .iter()
            .filter(|(_, entry)| entry.get("admin").and_then(Value::as_bool) == Some(true))
            .map(|(name, _)| name.clone())
            .collect()
    })
}

/// Whether protocol method `method` requires `webserver.remote_admin = true` on remote connections.
pub fn method_admin(method: &str) -> bool {
    protocol_admin_methods().contains(method)
}

// Schemars emits document-relative references. Each generated document is embedded in our
// catalog, so retain its original resource root while making those references catalog-relative.
fn catalog_references(value: &mut Value, path: &str, schema_path: &str) {
    match value {
        Value::Object(object) => {
            let schema_path = if object.get("$schema").is_some_and(Value::is_string) { path } else { schema_path };
            if let Some(Value::String(reference)) = object.get_mut("$ref")
                && let Some(suffix) = reference.strip_prefix("#/$defs/")
            {
                *reference = format!("#{schema_path}/$defs/{suffix}");
            }
            for (key, child) in object {
                // These keywords contain instance data, not schemas.
                if matches!(key.as_str(), "default" | "examples" | "const" | "enum") {
                    continue;
                }
                let escaped = key.replace('~', "~0").replace('/', "~1");
                catalog_references(child, &format!("{path}/{escaped}"), schema_path);
            }
        }
        Value::Array(array) => {
            for (index, child) in array.iter_mut().enumerate() {
                catalog_references(child, &format!("{path}/{index}"), schema_path);
            }
        }
        _ => {}
    }
}

/// Pretty-printed schema text (stable key order via serde_json's BTreeMap-backed maps).
pub fn schema_text() -> String {
    let mut s = serde_json::to_string_pretty(&schema()).unwrap_or_default();
    s.push('\n');
    s
}

/// Validate a JSON instance against a schema entry inside the generated protocol catalog.
#[doc(hidden)]
pub fn validate_schema_at(catalog: &Value, pointer: &str, value: &Value) -> Result<(), String> {
    let root = resolve_pointer(catalog, pointer)?;
    validate_node(catalog, root, value, "$", 0)
}

fn resolve_pointer<'a>(catalog: &'a Value, reference: &str) -> Result<&'a Value, String> {
    let ptr = reference.strip_prefix('#').ok_or_else(|| format!("non-local schema ref {reference}"))?;
    catalog.pointer(ptr).ok_or_else(|| format!("unresolved schema ref {reference}"))
}

fn validate_node(catalog: &Value, schema: &Value, value: &Value, path: &str, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err(format!("{path}: schema recursion limit exceeded"));
    }
    match schema {
        Value::Bool(true) => return Ok(()),
        Value::Bool(false) => return Err(format!("{path}: schema is false")),
        Value::Object(_) => {}
        _ => return Err(format!("{path}: invalid schema node")),
    }
    let obj = schema.as_object().expect("checked object");
    if let Some(Value::String(reference)) = obj.get("$ref") {
        let target = resolve_pointer(catalog, reference)?;
        validate_node(catalog, target, value, path, depth + 1)?;
    }
    if let Some(t) = obj.get("type") {
        let matches = |name: &str| match name {
            "object" => value.is_object(),
            "array" => value.is_array(),
            "string" => value.is_string(),
            "number" => value.is_number(),
            "integer" => {
                value.as_i64().is_some() || value.as_u64().is_some() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
            }
            "boolean" => value.is_boolean(),
            "null" => value.is_null(),
            _ => false,
        };
        let ok = match t {
            Value::String(name) => matches(name),
            Value::Array(names) => names.iter().filter_map(Value::as_str).any(matches),
            _ => false,
        };
        if !ok {
            return Err(format!("{path}: expected type {t}"));
        }
    }
    if let Some(min) = obj.get("minLength").and_then(Value::as_u64)
        && let Some(s) = value.as_str()
        && (s.chars().count() as u64) < min
    {
        return Err(format!("{path}: string length below minLength {min}"));
    }
    if let Some(expected) = obj.get("const")
        && value != expected
    {
        return Err(format!("{path}: expected const {expected}"));
    }
    if let Some(Value::Array(variants)) = obj.get("enum")
        && !variants.iter().any(|v| v == value)
    {
        return Err(format!("{path}: value {value} not in enum"));
    }
    if let Some(Value::Array(branches)) = obj.get("allOf") {
        for branch in branches {
            validate_node(catalog, branch, value, path, depth + 1)?;
        }
    }
    if let Some(Value::Array(branches)) = obj.get("oneOf") {
        let matches =
            branches.iter().filter(|branch| validate_node(catalog, branch, value, path, depth + 1).is_ok()).count();
        if matches != 1 {
            return Err(format!("{path}: oneOf matched {matches} branches"));
        }
    }
    if let Some(Value::Array(branches)) = obj.get("anyOf")
        && !branches.iter().any(|branch| validate_node(catalog, branch, value, path, depth + 1).is_ok())
    {
        return Err(format!("{path}: anyOf matched 0 branches"));
    }
    if let Some(disallowed) = obj.get("not")
        && validate_node(catalog, disallowed, value, path, depth + 1).is_ok()
    {
        return Err(format!("{path}: matched disallowed `not` schema"));
    }
    if let Some(map) = value.as_object() {
        if let Some(Value::Array(required)) = obj.get("required") {
            for key in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(key) {
                    return Err(format!("{path}: missing required key {key:?}"));
                }
            }
        }
        let props = obj.get("properties").and_then(Value::as_object);
        for (key, child) in map {
            let child_path = format!("{path}/{key}");
            if let Some(prop_schema) = props.and_then(|p| p.get(key)) {
                validate_node(catalog, prop_schema, child, &child_path, depth + 1)?;
            } else if let Some(additional) = obj.get("additionalProperties") {
                validate_node(catalog, additional, child, &child_path, depth + 1)?;
            }
        }
    }
    if let (Some(items_value), Some(item_schema)) = (value.as_array(), obj.get("items")) {
        for (i, child) in items_value.iter().enumerate() {
            validate_node(catalog, item_schema, child, &format!("{path}/{i}"), depth + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::test_fixtures::{invalid_tool_result_cases, valid_tool_result_cases};

    #[test]
    fn schema_catalog_local_references_resolve() {
        fn check(node: &Value, catalog: &Value, count: &mut usize) {
            match node {
                Value::Object(object) => {
                    if let Some(Value::String(reference)) = object.get("$ref") {
                        let pointer = reference.strip_prefix('#').expect("expected a local reference");
                        assert!(catalog.pointer(pointer).is_some(), "unresolved schema reference {reference}");
                        *count += 1;
                    }
                    for child in object.values() {
                        check(child, catalog, count);
                    }
                }
                Value::Array(array) => {
                    for child in array {
                        check(child, catalog, count);
                    }
                }
                _ => {}
            }
        }
        let catalog = schema();
        let mut count = 0;
        check(&catalog, &catalog, &mut count);
        assert!(count > 0);
    }

    #[test]
    fn schema_catalog_references_escape_paths_and_preserve_instance_data() {
        let mut catalog = serde_json::json!({"a/b~c": {
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "properties": {"value": {"$ref": "#/$defs/Foo"}},
            "$defs": {"Foo": {"type": "string"}},
            "default": {"$schema": "custom", "$ref": "#/$defs/instance"}
        }});
        catalog_references(&mut catalog, "", "");
        assert_eq!(catalog["a/b~c"]["properties"]["value"]["$ref"], "#/a~1b~0c/$defs/Foo");
        assert_eq!(catalog["a/b~c"]["default"]["$ref"], "#/$defs/instance");
        assert!(catalog.pointer("/a~1b~0c/$defs/Foo").is_some());
    }

    /// Snapshot: the protocol must not change silently. After an intentional change, update the
    /// protocol version as required and run `UPDATE_SCHEMA=1 cargo test -p mare-protocol`.
    #[test]
    fn schema_snapshot() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("schema.json");
        let now = schema_text();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(&path, &now).unwrap();
        }
        // compare values, not text: key order depends on serde_json's `preserve_order`, which
        // feature unification may switch on in workspace builds
        let snap: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).unwrap_or_default();
        assert!(snap == schema(), "protocol schema changed — review, bump PROTOCOL_VERSION, then UPDATE_SCHEMA=1");
    }

    #[test]
    fn tool_result_schema_matches_shared_positive_and_negative_cases() {
        let catalog = schema();
        let targets = ["#/notifications/event", "#/headless/line", "#/headless/protocol_event"];
        for (label, payload) in valid_tool_result_cases() {
            let envelope = serde_json::json!({
                "type": "item/completed",
                "data": {
                    "turn_id": "t-1",
                    "item_id": "call_1",
                    "status": "completed",
                    "item": payload,
                }
            });
            for target in targets {
                validate_schema_at(&catalog, target, &envelope)
                    .unwrap_or_else(|e| panic!("{label} failed {target}: {e}"));
            }
        }
        for (label, payload, schema_rejects) in invalid_tool_result_cases() {
            let envelope = serde_json::json!({
                "type": "item/completed",
                "data": {
                    "turn_id": "t-1",
                    "item_id": "call_1",
                    "status": "failed",
                    "item": payload,
                }
            });
            for target in targets {
                let res = validate_schema_at(&catalog, target, &envelope);
                if schema_rejects {
                    assert!(res.is_err(), "{label} should be rejected by {target}");
                } else {
                    assert!(res.is_ok(), "{label} is a cross-field check beyond JSON Schema on {target}: {res:?}");
                }
            }
        }
    }

    #[test]
    fn removed_hook_events_are_valid_extensions() {
        let catalog = schema();
        for (event, data) in [
            ("hook/start", serde_json::json!({"id":"x", "event":"e", "plugin":"p", "mode":"emit"})),
            (
                "hook/end",
                serde_json::json!({"id":"x", "event":"e", "plugin":"p", "mode":"emit", "status":"completed", "duration_ms":1.0}),
            ),
        ] {
            let envelope = serde_json::json!({"type": event, "data": data});
            for target in ["#/notifications/event", "#/headless/line"] {
                validate_schema_at(&catalog, target, &envelope)
                    .unwrap_or_else(|error| panic!("{event} at {target}: {error}"));
            }
        }
    }
}
