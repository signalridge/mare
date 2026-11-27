//! Event declarations inside service contracts.
//!
//! Validates declaration metadata (`mode`, `expose`, `scope`, `delivery`, `ignorable`, `retain`,
//! `on_error`, `data`, `result`, `example`), naming rules (`<root>/<kebab-name>`, snake_case
//! payload fields, no `$ref`), and protocol reserved roots so built-in and third-party contracts
//! share one unified event registry with `mare-protocol`.

use std::collections::BTreeMap;

use mare_protocol::{EventDelivery, EventInfo, EventMode, EventOnError, event_registry::PROTOCOL_EVENTS};
use serde_json::{Map as JsonMap, Value as Json, json};

/// Event visibility / reach:
/// - `process`: in-process only (never leaves the process via streams or remote hooks)
/// - `lua`: in-process + primary remote hooks (not pushed on public event streams)
/// - `both`: in-process + remote hooks + public event streams (`events.subscribe` / headless `--json`)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventReach {
    Process,
    Lua,
    Both,
}

/// Core/protocol-owned event roots: no contract (service or
/// event-only) may declare events under these roots. Service-owned protocol roots (`status`, `ui`,
/// `subagent`, etc.) are reserved against third-party contracts and conflicting event names, while
/// their owner service may declare non-conflicting events under its own root.
pub const CORE_ONLY_EVENT_ROOTS: &[&str] =
    &["turn", "step", "item", "queue", "interaction", "hook", "app", "option", "trust", "plugin", "reload"];

pub use mare_protocol::EventScope;

/// Session-level retention policy for a contract event (`"last"` or `{ key, cleared_by }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventRetain {
    Last,
    Key { key: String, cleared_by: String },
}

/// One event declared inside `contracts.<root>.events`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventSpec {
    pub name: String,
    pub short_name: String,
    pub root: String,
    pub owner: String,
    pub doc: String,
    pub data: Json,
    pub has_data_schema: bool,
    pub mode: EventMode,
    pub expose: EventReach,
    pub scope: EventScope,
    pub delivery: EventDelivery,
    pub ignorable: bool,
    pub retain: Option<EventRetain>,
    pub on_error: EventOnError,
    pub affirmative: bool,
    pub join_ask_reasons: bool,
    pub authority_service: Option<String>,
    pub example: Option<Json>,
    pub result: Option<Json>,
    pub emitters: Vec<String>,
}

impl EventSpec {
    /// Derive [`crate::PolicyMetadata`] when this declaration is a `mode = "policy"` point.
    pub fn policy_metadata(&self) -> Option<crate::PolicyMetadata> {
        if self.mode != EventMode::Policy {
            return None;
        }
        let props = self.result.as_ref().and_then(|s| s.get("properties")).and_then(Json::as_object);
        let has_prop = |names: &[&str]| props.is_some_and(|m| names.iter().any(|k| m.contains_key(*k)));
        let permissions = has_prop(&["permission_constraints", "scope"]);
        let sandbox_grant = has_prop(&["sandbox_grant"]);
        let fold = if self.affirmative {
            crate::PolicyFold::Affirmative
        } else if permissions {
            crate::PolicyFold::Approval
        } else {
            crate::PolicyFold::Restrictive
        };
        Some(crate::PolicyMetadata {
            fold,
            permissions,
            sandbox_grant,
            join_ask_reasons: self.join_ask_reasons,
            authority_service: self.authority_service.clone(),
        })
    }

    /// Materialize into the `events.list` wire shape shared with `protocol_events!`.
    pub fn to_info(&self) -> EventInfo {
        EventInfo {
            name: self.name.clone(),
            mode: self.mode,
            scope: self.scope,
            delivery: self.delivery,
            ignorable: self.ignorable,
            retain: self.retain.is_some(),
            on_error: self.on_error,
            public: self.expose == EventReach::Both,
            doc: self.doc.clone(),
            data_schema: self.data.clone(),
            result_schema: self.result.clone(),
            example: self.example.clone().unwrap_or(Json::Null),
        }
    }
}

fn err(root: &str, msg: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(format!("event contract `{root}`: {msg}"))
}

/// Return true if `contract` is a host-only service contract (at least one method and every
/// method has `provider = "host"`).
pub fn is_host_only_contract(contract: &crate::ServiceContract) -> bool {
    !contract.methods.is_empty() && contract.methods.values().all(super::contracts::MethodSpec::is_host)
}

/// Return true if `root` is a protocol-reserved event root for the given caller context.
/// - Core-only roots (`turn`, `step`, `item`, `queue`, `option`, ...) are allowed only when
///   `owner_is_host_only` is true (the owning contract's methods are all `provider = "host"`).
/// - Other roots appearing in `PROTOCOL_EVENTS` are forbidden for third-party contracts and for
///   event-only contracts whose `owner != root`.
pub fn is_protocol_reserved_event_root(
    root: &str,
    owner: &str,
    owner_is_host_only: bool,
    is_third_party: bool,
) -> bool {
    if CORE_ONLY_EVENT_ROOTS.contains(&root) {
        return !owner_is_host_only;
    }
    let in_protocol_table = PROTOCOL_EVENTS.iter().any(|def| def.name.split('/').next() == Some(root));
    if !in_protocol_table {
        return false;
    }
    if is_third_party {
        return true;
    }
    !(owner == root || owner.strip_suffix('s') == Some(root))
}

/// Validate `<kebab-name>` in `<root>/<kebab-name>`.
pub fn valid_event_kebab_name(short: &str) -> bool {
    let bytes = short.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 64
        || !bytes[0].is_ascii_lowercase()
        || bytes[bytes.len() - 1] == b'-'
        || !bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        || short.contains("--")
    {
        return false;
    }
    !matches!(short.rsplit('-').next(), Some("start" | "end" | "done" | "finished"))
}

fn valid_snake_field(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && name != "ok"
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// Reject `$ref` / `$defs` (contracts must pass expanded component schemas directly) and enforce
/// `snake_case`, non-`_`-prefixed, non-`ok` property names on object schemas.
fn validate_event_schema(root: &str, what: &str, schema: &Json) -> mlua::Result<()> {
    fn walk(root: &str, what: &str, node: &Json) -> mlua::Result<()> {
        match node {
            Json::Object(map) => {
                if map.contains_key("$ref") || map.contains_key("$defs") {
                    return Err(err(
                        root,
                        format!("{what} must use expanded schemas (`components`), not `$ref` or `$defs`"),
                    ));
                }
                if let Some(Json::Object(props)) = map.get("properties") {
                    for (field, child) in props {
                        if !valid_snake_field(field) {
                            return Err(err(
                                root,
                                format!(
                                    "{what} field `{field}` must be lowercase snake_case and not `ok` or `_`-prefixed"
                                ),
                            ));
                        }
                        walk(root, what, child)?;
                    }
                }
                for (key, child) in map {
                    if matches!(key.as_str(), "properties" | "default" | "examples" | "const" | "enum") {
                        continue;
                    }
                    walk(root, what, child)?;
                }
                Ok(())
            }
            Json::Array(items) => {
                for child in items {
                    walk(root, what, child)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    walk(root, what, schema)?;
    mare_shared::schema::Schema::validate_schema(schema)
        .map_err(|e| err(root, format!("{what} invalid JSON Schema: {e}")))
}

fn parse_mode(root: &str, full_name: &str, entry: &JsonMap<String, Json>) -> mlua::Result<EventMode> {
    match entry.get("mode") {
        None | Some(Json::Null) => Ok(EventMode::Emit),
        Some(Json::String(s)) => match s.as_str() {
            "emit" => Ok(EventMode::Emit),
            "parallel" => Ok(EventMode::Parallel),
            "serial" => Ok(EventMode::Serial),
            "bail" => Ok(EventMode::Bail),
            "waterfall" => Ok(EventMode::Waterfall),
            "policy" => Ok(EventMode::Policy),
            "rewrite" => Ok(EventMode::Rewrite),
            other => Err(err(root, format!("`{full_name}` invalid mode `{other}`"))),
        },
        _ => Err(err(root, format!("`{full_name}` `mode` must be a string"))),
    }
}

fn parse_expose(root: &str, full_name: &str, entry: &JsonMap<String, Json>) -> mlua::Result<EventReach> {
    match entry.get("expose") {
        None | Some(Json::Null) => Ok(EventReach::Lua),
        Some(Json::String(s)) => match s.as_str() {
            "process" => Ok(EventReach::Process),
            "lua" => Ok(EventReach::Lua),
            "both" => Ok(EventReach::Both),
            "spi" => Err(err(
                root,
                format!("`{full_name}`: `expose = \"spi\"` was removed; use `expose = \"process\"` instead"),
            )),
            other => Err(err(root, format!("`{full_name}` invalid expose `{other}` (expected process|lua|both)"))),
        },
        _ => Err(err(root, format!("`{full_name}` `expose` must be a string"))),
    }
}

fn parse_scope(root: &str, full_name: &str, entry: &JsonMap<String, Json>) -> mlua::Result<EventScope> {
    match entry.get("scope") {
        None | Some(Json::Null) => Ok(EventScope::default()),
        Some(Json::String(text)) => {
            EventScope::parse(text.as_str()).ok_or_else(|| err(root, format!("`{full_name}` invalid scope `{text}`")))
        }
        _ => Err(err(root, format!("`{full_name}` `scope` must be a string"))),
    }
}

fn parse_delivery(
    root: &str,
    full_name: &str,
    expose: EventReach,
    entry: &JsonMap<String, Json>,
) -> mlua::Result<EventDelivery> {
    match entry.get("delivery") {
        None | Some(Json::Null) if expose == EventReach::Both => {
            Err(err(root, format!("`{full_name}` with expose = \"both\" must declare `delivery`")))
        }
        None | Some(Json::Null) => Ok(EventDelivery::Required),
        Some(Json::String(s)) => match s.as_str() {
            "required" => Ok(EventDelivery::Required),
            "lossy" => Ok(EventDelivery::Lossy),
            other => Err(err(root, format!("`{full_name}` invalid delivery `{other}`"))),
        },
        _ => Err(err(root, format!("`{full_name}` `delivery` must be a string"))),
    }
}

fn parse_on_error(
    root: &str,
    full_name: &str,
    mode: EventMode,
    entry: &JsonMap<String, Json>,
) -> mlua::Result<EventOnError> {
    match entry.get("on_error") {
        None | Some(Json::Null) => Ok(EventOnError::Continue),
        _ if matches!(mode, EventMode::Policy | EventMode::Rewrite) => {
            Err(err(root, format!("`{full_name}` cannot set `on_error` when mode is policy or rewrite")))
        }
        Some(Json::String(s)) => match s.as_str() {
            "continue" => Ok(EventOnError::Continue),
            "abort" => Ok(EventOnError::Abort),
            other => Err(err(root, format!("`{full_name}` invalid on_error `{other}`"))),
        },
        _ => Err(err(root, format!("`{full_name}` `on_error` must be a string"))),
    }
}

fn parse_policy_flag(
    root: &str,
    full_name: &str,
    mode: EventMode,
    entry: &JsonMap<String, Json>,
    key: &str,
) -> mlua::Result<bool> {
    match entry.get(key) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => {
            if mode != EventMode::Policy {
                return Err(err(root, format!("`{full_name}` `{key}` is only allowed when mode = \"policy\"")));
            }
            Ok(*b)
        }
        _ => Err(err(root, format!("`{full_name}` `{key}` must be a boolean"))),
    }
}

fn parse_authority_service(
    root: &str,
    full_name: &str,
    mode: EventMode,
    entry: &JsonMap<String, Json>,
) -> mlua::Result<Option<String>> {
    match entry.get("authority_service") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => {
            if mode != EventMode::Policy {
                return Err(err(
                    root,
                    format!("`{full_name}` `authority_service` is only allowed when mode = \"policy\""),
                ));
            }
            if !super::contracts::service_name(s) {
                return Err(err(root, format!("`{full_name}` invalid `authority_service` `{s}`")));
            }
            Ok(Some(s.clone()))
        }
        _ => Err(err(root, format!("`{full_name}` `authority_service` must be a string"))),
    }
}

fn parse_retain(root: &str, full_name: &str, entry: &JsonMap<String, Json>) -> mlua::Result<Option<EventRetain>> {
    match entry.get("retain") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) if s == "last" => Ok(Some(EventRetain::Last)),
        Some(Json::Object(t)) => {
            for k in t.keys() {
                if !matches!(k.as_str(), "key" | "cleared_by") {
                    return Err(err(root, format!("`{full_name}` retain has unknown field `{k}`")));
                }
            }
            let key = t
                .get("key")
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| err(root, format!("`{full_name}` retain table requires `key`")))?
                .to_owned();
            let cleared_by = t
                .get("cleared_by")
                .and_then(Json::as_str)
                .filter(|s| s.contains('/'))
                .ok_or_else(|| err(root, format!("`{full_name}` retain table requires `cleared_by` (`root/name`)")))?
                .to_owned();
            Ok(Some(EventRetain::Key { key, cleared_by }))
        }
        _ => Err(err(root, format!("`{full_name}` `retain` must be \"last\" or `{{ key, cleared_by }}`"))),
    }
}

fn protocol_event_names() -> &'static std::collections::HashSet<&'static str> {
    static NAMES: std::sync::OnceLock<std::collections::HashSet<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| PROTOCOL_EVENTS.iter().map(|def| def.name).collect())
}

/// Parse `contracts.<root>.events` for one contract root (service or event-only root), consuming
/// the JSON value by move to avoid deep-cloning event schemas and examples.
pub(crate) fn parse_events_json_owned(
    root: &str,
    owner: &str,
    value: Option<Json>,
) -> mlua::Result<BTreeMap<String, EventSpec>> {
    let table = match value {
        None | Some(Json::Null) => return Ok(BTreeMap::new()),
        Some(Json::Object(t)) => t,
        Some(Json::Array(_)) => return Err(err(root, "`events` keys must be kebab-case event names")),
        _ => return Err(err(root, "`events` must be a table")),
    };
    let protocol_names = protocol_event_names();
    let mut out = BTreeMap::new();
    for (short, entry_val) in table {
        if !valid_event_kebab_name(&short) {
            return Err(err(
                root,
                format!(
                    "invalid event name `{short}`: expected lowercase kebab-case without `_`, `:`, or trailing start/end/done/finished"
                ),
            ));
        }
        let full_name = format!("{root}/{short}");
        if protocol_names.contains(full_name.as_str()) {
            return Err(err(root, format!("event `{full_name}` duplicates a built-in protocol event")));
        }
        let Json::Object(mut entry) = entry_val else {
            return Err(err(root, format!("`{full_name}` declaration must be a table")));
        };
        for field in entry.keys() {
            if !matches!(
                field.as_str(),
                "doc"
                    | "data"
                    | "mode"
                    | "expose"
                    | "scope"
                    | "delivery"
                    | "ignorable"
                    | "retain"
                    | "on_error"
                    | "affirmative"
                    | "join_ask_reasons"
                    | "authority_service"
                    | "example"
                    | "result"
                    | "emitters"
            ) {
                return Err(err(root, format!("`{full_name}` has unknown field `{field}`")));
            }
        }
        let mode = parse_mode(root, &full_name, &entry)?;
        let expose = parse_expose(root, &full_name, &entry)?;
        let scope = parse_scope(root, &full_name, &entry)?;
        let delivery = parse_delivery(root, &full_name, expose, &entry)?;
        let on_error = parse_on_error(root, &full_name, mode, &entry)?;
        let affirmative = parse_policy_flag(root, &full_name, mode, &entry, "affirmative")?;
        let join_ask_reasons = parse_policy_flag(root, &full_name, mode, &entry, "join_ask_reasons")?;
        let authority_service = parse_authority_service(root, &full_name, mode, &entry)?;
        let retain = parse_retain(root, &full_name, &entry)?;
        let doc = match entry.remove("doc") {
            None | Some(Json::Null) => String::new(),
            Some(Json::String(s)) => s,
            _ => return Err(err(root, format!("`{full_name}` `doc` must be a string"))),
        };
        if expose == EventReach::Both && doc.trim().is_empty() {
            return Err(err(root, format!("`{full_name}` with expose = \"both\" requires `doc`")));
        }
        let ignorable = match entry.get("ignorable") {
            None | Some(Json::Null) if expose == EventReach::Both => {
                return Err(err(root, format!("`{full_name}` with expose = \"both\" requires `ignorable`")));
            }
            None | Some(Json::Null) => false,
            Some(Json::Bool(b)) => *b,
            _ => return Err(err(root, format!("`{full_name}` `ignorable` must be a boolean"))),
        };
        if delivery == EventDelivery::Lossy && retain.is_none() && !ignorable {
            return Err(err(
                root,
                format!("`{full_name}` with delivery = \"lossy\" must declare `retain` or `ignorable = true`"),
            ));
        }
        if (short == "changed" || short.ends_with("-changed")) && retain.is_none() {
            return Err(err(root, format!("`{full_name}` ending in `changed` must declare `retain`")));
        }
        let raw_data = entry.remove("data").filter(|v| !v.is_null());
        let has_data_schema = raw_data.is_some();
        let data = match raw_data {
            None if expose == EventReach::Both => {
                return Err(err(root, format!("`{full_name}` with expose = \"both\" requires `data` schema")));
            }
            None => json!({ "type": "object" }),
            Some(value) => value,
        };
        if data.get("type").and_then(Json::as_str) != Some("object") {
            return Err(err(root, format!("`{full_name}` `data` schema must have object root (`type = \"object\"`)")));
        }
        validate_event_schema(root, &format!("`{full_name}` data"), &data)?;
        if scope == EventScope::Turn && (has_data_schema || expose == EventReach::Both) {
            let turn_prop = data
                .get("properties")
                .and_then(|p| p.get("turn_id"))
                .and_then(|t| t.get("type"))
                .and_then(Json::as_str);
            if turn_prop != Some("string") {
                return Err(err(
                    root,
                    format!("`{full_name}` has scope = \"turn\" so `data` must declare string field `turn_id`"),
                ));
            }
        }
        if let Some(EventRetain::Key { ref key, .. }) = retain
            && let Some(props) = data.get("properties").and_then(Json::as_object)
            && !props.contains_key(key)
        {
            return Err(err(root, format!("`{full_name}` retain.key `{key}` is not in `data.properties`")));
        }
        let example = match entry.remove("example").filter(|v| !v.is_null()) {
            None if expose == EventReach::Both => {
                return Err(err(root, format!("`{full_name}` with expose = \"both\" requires `example`")));
            }
            None => None,
            Some(example) => {
                if let Err(e) = mare_protocol::validate_schema_at(&data, "#", &example) {
                    return Err(err(root, format!("`{full_name}` example does not match `data` schema: {e}")));
                }
                Some(example)
            }
        };
        let result = match entry.remove("result").filter(|v| !v.is_null()) {
            None => None,
            Some(schema) => {
                if !matches!(mode, EventMode::Bail | EventMode::Serial | EventMode::Waterfall | EventMode::Policy) {
                    return Err(err(
                        root,
                        format!(
                            "`{full_name}` declares `result`, which is only allowed for bail/serial/waterfall/policy"
                        ),
                    ));
                }
                validate_event_schema(root, &format!("`{full_name}` result"), &schema)?;
                Some(schema)
            }
        };
        let emitters = match entry.remove("emitters") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Object(map)) if map.is_empty() => Vec::new(),
            Some(Json::Array(items)) => {
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    let Json::String(s) = item else {
                        return Err(err(root, format!("`{full_name}` `emitters` entries must be strings")));
                    };
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        return Err(err(root, format!("`{full_name}` `emitters` entries cannot be empty")));
                    }
                    list.push(if trimmed.len() == s.len() { s } else { trimmed.to_owned() });
                }
                list
            }
            _ => return Err(err(root, format!("`{full_name}` `emitters` must be a list of strings"))),
        };
        out.insert(
            short.clone(),
            EventSpec {
                name: full_name,
                short_name: short,
                root: root.to_owned(),
                owner: owner.to_owned(),
                doc,
                data,
                has_data_schema,
                mode,
                expose,
                scope,
                delivery,
                ignorable,
                retain,
                on_error,
                affirmative,
                join_ask_reasons,
                authority_service,
                example,
                result,
                emitters,
            },
        );
    }
    Ok(out)
}
