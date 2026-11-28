//! Service contracts: the single source of truth for a service's methods.
//!
//! Every service contract declares each method's `doc`, JSON Schema `params` and `result` (or
//! `registration = true`), the Lua argument `order`, `expose`, `provider`, `access` and `scope`.
//! The kernel derives the JSON adapter and LuaLS types from it; providers never write per-method
//! JSON handlers.
use std::collections::{BTreeMap, BTreeSet};

use mlua::{Lua, Value};
use serde_json::{Value as Json, json};

pub use super::contract_events::EventReach;
use crate::ServiceContract;
pub use mare_protocol::{EventDelivery, EventMode, EventOnError};

pub const SERVICE_SCHEMA_MAX_BYTES: usize = 64 * 1024;
const MAX_METHODS: usize = 128;
/// Trailing `order` entry: the remaining parameters form the final Lua `opts` table.
pub const REST: &str = "...";

/// Who may call a method: `lua` (Lua callers only, the default) or `both` (Lua and RPC).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expose {
    Lua,
    Both,
}
/// `primary`: only the primary connection (default), never grantable to a peer. `peer`: a peer
/// may call it when `peer.allow` lists the exact operation name. `peer_default`: open to any peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Primary,
    Peer,
    PeerDefault,
}
/// The resource a method addresses. RPC callers name the session of a `session` method with
/// a required `session_id`; Lua callers address their own session implicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Host,
    Workspace,
    Session,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Workspace => mare_shared::WORKSPACE_SERVICE_NAME,
            Self::Session => mare_shared::SESSION_SCOPE,
        }
    }
}

/// Who implements a contract method: `plugin` (default, implemented by a Lua or remote provider)
/// or `host` (implemented by a Rust function registered by the host).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MethodProvider {
    #[default]
    Plugin,
    Host,
}

impl MethodProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Host => "host",
        }
    }
}

/// When the RPC bridge automatically releases a registration held by a client connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ReleaseOn {
    /// Default: release when `capabilities.callbacks` is withdrawn (if the registration has callbacks).
    #[default]
    Callbacks,
    /// Release automatically when the client detaches its UI from the session.
    UiDetach,
    /// Never auto-release on capability withdrawal (stays installed and fails closed).
    Never,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodSpec {
    pub doc: String,
    pub expose: Expose,
    pub provider: MethodProvider,
    pub access: Access,
    /// Whether calling this operation remotely requires `webserver.remote_admin = true` (owner-admin gate).
    pub admin: bool,
    /// True when the contract table explicitly specified `access = ...` (used to enforce
    /// non-remote methods must not declare `access`).
    pub access_declared: bool,
    pub scope: Scope,
    /// JSON Schema of the arguments object (object root).
    pub params: Json,
    /// Positional Lua arguments, mapped to parameter names.
    pub order: Vec<String>,
    /// Remaining parameters become the trailing Lua `opts` table.
    pub rest: bool,
    /// JSON Schema of the result. For a registration, `{registration_id}`.
    pub result: Json,
    /// Optional LuaLS return types for Lua-only/multi-return methods whose Lua return shape
    /// cannot be expressed by a single JSON Schema value (does not affect wire schemas).
    pub lua_result: Option<Vec<String>>,
    /// Lua returns a disposer; JSON callers get `{registration_id}`.
    pub registration: bool,
    pub optional: bool,
    /// Option fields a Lua caller may pass as functions (`login`'s `say`/`ask`/`choose`), each
    /// with its LuaLS function type. They have no wire form: JSON callers never send them, the
    /// proxy does not encode them.
    pub callbacks: BTreeMap<String, String>,
    /// Callbacks a JSON caller may also implement (table form of `callbacks`): each becomes a
    /// `callback.call` reverse request checked against these schemas.
    pub remote_callbacks: BTreeMap<String, CallbackSpec>,
    /// The service errors this method raises (names in the contract's `errors`); `None` when
    /// the method does not say, then any of them.
    pub errors: Option<Vec<String>>,
    /// Whether this method is allowed when the session barrier is poisoned (`Some(true)` = `"allow"`,
    /// `Some(false)` = `"deny"`, `None` = unspecified).
    pub poisoned: Option<bool>,
    /// Host-query sandbox confinement: when `true`, direct RPC calls enter the host's active
    /// sandbox policy (`run_host_query`) without bypassing netauth/approvals.
    pub confined: bool,
    /// SPI method called synchronously (non-yielding) during a baseline snapshot cut.
    pub baseline: bool,
    /// Remote `Scope::Host` registration whose effect is inherently single-host (not replicated across sessions via `scope = "all"`).
    pub host_local: bool,
    /// `Scope::Host` method that always executes on the anchor host rather than routing via `session_id` / default session.
    pub host_anchor: bool,
    /// Parameter key on a `Scope::Host` registration that supersedes prior registrations of the same method when `replace = true`.
    pub replace_key: Option<String>,
    /// Automatic release trigger for registrations held by a client connection.
    pub release_on: ReleaseOn,
    /// When true on a scoped registration, callers must pass an explicit `session_id` or `scope = "all"` (no implicit default).
    pub require_explicit_scope: bool,
    /// Named capabilities from the service contract that the active provider must implement for this method.
    pub requires_capabilities: Vec<String>,
    /// Service-level flag: RPC calls targeting a named session runtime enforce multi-host project scope.
    pub project_scoped: bool,
    /// Skip JSON-schema parameter/result checks on the in-process Lua guard when the Lua implementation validates itself or returns non-JSON/multi-value Lua results.
    pub lua_unchecked: bool,
    /// Lua-only history results use the existing larger, finite history conversion budget.
    pub history_result: bool,
    /// Lua-only methods that raise structured tables carrying a string `kind` field pass those tables through to in-process Lua callers instead of settling them into `internal`.
    pub lua_kind_errors: bool,
}

/// A callback a JSON caller can implement: its LuaLS function type, named parameters schema (mapped
/// from the provider's positional arguments via `order`), result schema, optionality, and optional
/// incremental `update` schema (`callback.update`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallbackSpec {
    pub luals: String,
    pub params: Json,
    pub order: Vec<String>,
    pub result: Json,
    /// Optional schema of partial payloads sent via `callback.update { id, partial }` while this
    /// callback is in flight.
    pub update: Option<Json>,
    /// a JSON caller may leave it out (the provider then sees nil)
    pub optional: bool,
}

/// One of a service's own errors (`errors = { name = { doc, data? } }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorSpec {
    pub doc: String,
    /// JSON Schema of the error's `data`; without one the error carries no data.
    pub data: Option<Json>,
}

/// One named capability declared by a service contract (`capabilities = { name = { method, doc } }`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilitySpec {
    pub method: String,
    pub doc: String,
}

impl MethodSpec {
    pub fn remote(&self) -> bool {
        self.expose == Expose::Both
    }
    pub fn is_host(&self) -> bool {
        self.provider == MethodProvider::Host
    }
    pub fn provider_name(&self) -> &'static str {
        self.provider.as_str()
    }
    pub fn expose_name(&self) -> &'static str {
        match self.expose {
            Expose::Lua => "lua",
            Expose::Both => "both",
        }
    }
    pub fn access_name(&self) -> &'static str {
        match self.access {
            Access::Primary => "primary",
            Access::Peer => "peer",
            Access::PeerDefault => "peer_default",
        }
    }
    pub fn scope_name(&self) -> &'static str {
        self.scope.as_str()
    }
    pub fn supports_registration_scope(&self) -> bool {
        self.remote() && self.registration && self.scope == Scope::Host && !self.host_local
    }
    /// The arguments as an RPC caller sends them: a session method also names its session, a
    /// scoped host registration adds `{scope, session_id}`, and a method with remote callbacks adds `callbacks: [...]`.
    pub fn wire_params(&self) -> Json {
        fn add_field(params: &mut Json, key: &str, schema: Json, mark_required: bool) {
            let Some(object) = params.as_object_mut() else { return };
            if let Some(properties) = object.entry("properties").or_insert_with(|| json!({})).as_object_mut() {
                properties.insert(key.into(), schema);
            }
            if mark_required
                && let Some(required) = object.entry("required").or_insert_with(|| json!([])).as_array_mut()
                && !required.iter().any(|v| v.as_str() == Some(key))
            {
                required.push(json!(key));
            }
        }
        let mut params = self.params.clone();
        if !self.remote_callbacks.is_empty() {
            let names: Vec<&String> = self.remote_callbacks.keys().collect();
            add_field(
                &mut params,
                "callbacks",
                json!({"type":"array","items":{"enum":names},"uniqueItems":true}),
                self.remote_callbacks.values().any(|cb| !cb.optional),
            );
        }
        if self.supports_registration_scope() {
            add_field(&mut params, "scope", json!({"type":"string","enum":["all", Scope::Session.as_str()]}), false);
            add_field(&mut params, "session_id", json!({"type":"string"}), false);
        }
        if self.scope == Scope::Session {
            // `expected_session` is an internal Lua fencing option; RPC uses `session_id` + `expected_epoch`.
            if let Some(object) = params.as_object_mut() {
                if let Some(properties) = object.get_mut("properties").and_then(Json::as_object_mut) {
                    properties.remove("expected_session");
                }
                if let Some(required) = object.get_mut("required").and_then(Json::as_array_mut) {
                    required.retain(|v| v.as_str() != Some("expected_session"));
                }
            }
            add_field(&mut params, "session_id", json!({"type":"string"}), true);
            add_field(&mut params, "expected_epoch", json!({"type":"string"}), false);
        }
        if self.scope == Scope::Workspace {
            add_field(&mut params, "cwd", json!({"type":"string"}), true);
        }
        params
    }
    /// The catalog entry (`services.list`, docs, generated types).
    pub fn describe(&self) -> Json {
        let mut order = self.order.clone();
        if self.rest {
            order.push(REST.into());
        }
        let mut entry = json!({
            "doc": self.doc, "expose": self.expose_name(), "access": self.access_name(),
            "scope": self.scope_name(), "parameters": self.wire_params(), "order": order,
            "result": self.result, "registration": self.registration, "optional": self.optional,
        });
        if self.is_host() {
            entry["provider"] = json!("host");
        }
        if self.admin {
            entry["admin"] = json!(true);
        }
        if self.history_result {
            entry["history_result"] = json!(true);
        }
        if !self.requires_capabilities.is_empty() {
            entry["requires_capabilities"] = json!(self.requires_capabilities);
        }
        if !self.remote_callbacks.is_empty() {
            let callbacks: serde_json::Map<String, Json> = self
                .remote_callbacks
                .iter()
                .map(|(name, spec)| {
                    let mut cb = json!({
                        "params": spec.params,
                        "order": spec.order,
                        "result": spec.result,
                        "optional": spec.optional,
                    });
                    if let Some(update) = &spec.update {
                        cb["update"] = update.clone();
                    }
                    (name.clone(), cb)
                })
                .collect();
            entry["callbacks"] = Json::Object(callbacks);
        }
        entry
    }
}

/// Roots of protocol-layer methods that no built-in contract owns. A
/// service of the same name would let a grant or a method name mean two things (`command.run`
/// the protocol method vs a service `command`), so these are never service names. A test in
/// mare-cli keeps this list complete against the protocol's method table.
pub const PROTOCOL_ROOTS: &[&str] = &[
    "baseline",
    "config",
    "connection",
    "events",
    "flags",
    "initialize",
    "peer",
    "registrations",
    "services",
    "shutdown",
    "state",
];

/// Lowercase service names never contain the `.` separating service and method, so the
/// operation name `service.method` (and a grant pattern over it) is unambiguous.
pub fn service_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !PROTOCOL_ROOTS.contains(&value)
        && !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(b))
}
/// A contract method name. `constructor` and `super` are the fields the class machinery reads off a
/// provider's class (cordis class.lua `construct`, the class chain), so no provider can have them as
/// methods: a remote one would replace its class's constructor (`new` is a method like any other). `name` and `ctx` are instance fields set by the service constructor.
pub fn method_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    !matches!(value, "constructor" | "super" | "name" | "ctx")
        && !bytes.is_empty()
        && bytes.len() <= 64
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

fn error(service: &str, message: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(format!("service `{service}` contract: {message}"))
}

/// Byte overhead of `result_wrapper(&result)` outside the serialized `result` value itself:
/// `{"additionalProperties":false,"properties":{"value":},"required":["value"],"type":"object"}`.
const RESULT_WRAPPER_OVERHEAD_BYTES: usize = 91;

fn json_string_byte_len(s: &str) -> usize {
    let mut len = 2 + s.len();
    for &b in s.as_bytes() {
        match b {
            b'"' | b'\\' | 0x08 | 0x0c | b'\n' | b'\r' | b'\t' => len += 1,
            0x00..=0x1f => len += 5, // \u00XX
            _ => {}
        }
    }
    len
}

/// Exact compact-JSON serialized byte length check (`serde_json::to_vec(val).len() > limit`)
/// without running `serde_json`'s serializer or allocating a buffer.
fn json_byte_len_exceeds(val: &Json, limit: usize) -> bool {
    fn add_len(val: &Json, acc: &mut usize, limit: usize) -> bool {
        let delta = match val {
            Json::Null => 4,
            Json::Bool(true) => 4,
            Json::Bool(false) => 5,
            Json::Number(n) => {
                struct Counter(usize);
                impl std::io::Write for Counter {
                    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                        self.0 += buf.len();
                        Ok(buf.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        Ok(())
                    }
                }
                let mut c = Counter(0);
                if serde_json::to_writer(&mut c, n).is_err() {
                    return true;
                }
                c.0
            }
            Json::String(s) => json_string_byte_len(s),
            Json::Array(items) => {
                *acc = acc.saturating_add(2 + items.len().saturating_sub(1));
                if *acc > limit {
                    return true;
                }
                for item in items {
                    if add_len(item, acc, limit) {
                        return true;
                    }
                }
                return false;
            }
            Json::Object(map) => {
                *acc = acc.saturating_add(2 + map.len().saturating_sub(1));
                if *acc > limit {
                    return true;
                }
                for (k, v) in map {
                    *acc = acc.saturating_add(json_string_byte_len(k) + 1);
                    if *acc > limit || add_len(v, acc, limit) {
                        return true;
                    }
                }
                return false;
            }
        };
        *acc = acc.saturating_add(delta);
        *acc > limit
    }
    let mut acc = 0usize;
    add_len(val, &mut acc, limit)
}

fn compile(service: &str, what: &str, schema: &Json) -> mlua::Result<()> {
    if json_byte_len_exceeds(schema, SERVICE_SCHEMA_MAX_BYTES) {
        return Err(error(service, format!("{what} schema exceeds 64 KiB")));
    }
    mare_shared::schema::Schema::validate_contract_schema(schema).map_err(|e| error(service, format!("{what}: {e}")))
}

fn compile_result(service: &str, what: &str, result: &Json) -> mlua::Result<()> {
    if json_byte_len_exceeds(result, SERVICE_SCHEMA_MAX_BYTES - RESULT_WRAPPER_OVERHEAD_BYTES) {
        return Err(error(service, format!("{what} schema exceeds 64 KiB")));
    }
    mare_shared::schema::Schema::validate_contract_result_schema(result)
        .map_err(|e| error(service, format!("{what}: {e}")))
}

fn keyword_json<T>(
    service: &str,
    method: &str,
    table: &serde_json::Map<String, Json>,
    key: &str,
    parse: impl Fn(&str) -> Option<T>,
    default: T,
) -> mlua::Result<T> {
    match table.get(key) {
        None | Some(Json::Null) => Ok(default),
        Some(Json::String(value)) => {
            parse(value).ok_or_else(|| error(service, format!("`{method}`: invalid {key} `{value}`")))
        }
        _ => Err(error(service, format!("`{method}`: `{key}` must be a string"))),
    }
}

fn bool_attr(service: &str, method: &str, table: &serde_json::Map<String, Json>, key: &str) -> mlua::Result<bool> {
    match table.get(key) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        _ => Err(error(service, format!("`{method}`: `{key}` must be a boolean"))),
    }
}

/// Results are validated as `{value}` so the root of a result schema can be any JSON type.
pub fn result_wrapper(result: &Json) -> Json {
    json!({"type":"object","properties":{"value":result},"required":["value"],"additionalProperties":false})
}

/// The result schema of every registration: Lua gets the disposer instead.
pub fn registration_schema() -> Json {
    json!({"type":"object","properties":{"registration_id":{"type":"string"}},
        "required":["registration_id"],"additionalProperties":false})
}

fn parse_order_list_json(service: &str, what: &str, value: Option<&Json>) -> mlua::Result<Vec<String>> {
    match value {
        None | Some(Json::Null) => Ok(vec![]),
        Some(Json::Object(map)) if map.is_empty() => Ok(vec![]),
        Some(Json::Array(list)) => {
            let mut names = Vec::with_capacity(list.len());
            for item in list {
                let Some(s) = item.as_str() else {
                    return Err(error(service, format!("{what} order must be a list of strings")));
                };
                names.push(s.to_owned());
            }
            Ok(names)
        }
        _ => Err(error(service, format!("{what} order must be a list"))),
    }
}

fn validate_order_schema(
    service: &str,
    what: &str,
    params: &Json,
    order: &[String],
    rest: bool,
    extra: &BTreeSet<String>,
    require_closed: bool,
) -> mlua::Result<BTreeSet<String>> {
    let object = params.as_object().filter(|o| o.get("type").and_then(Json::as_str) == Some("object"));
    let Some(object) = object else {
        return Err(error(service, format!("{what} params must be an object schema")));
    };
    let properties: BTreeSet<String> =
        object.get("properties").and_then(Json::as_object).map(|p| p.keys().cloned().collect()).unwrap_or_default();
    let mut seen = BTreeSet::new();
    for name in order {
        if name == REST || !(properties.contains(name) || extra.contains(name)) || !seen.insert(name) {
            return Err(error(service, format!("{what} order must name unique declared parameters, `...` last")));
        }
    }
    if !rest {
        if properties.iter().any(|name| !seen.contains(name)) {
            return Err(error(service, format!("{what} order leaves parameters unmapped; add `...`")));
        }
        if require_closed && object.get("additionalProperties").and_then(Json::as_bool) != Some(false) {
            return Err(error(service, format!("{what} params must set additionalProperties = false without `...`")));
        }
    }
    Ok(properties)
}

fn parse_method_json_owned(
    service: &str,
    method: &str,
    mut table: serde_json::Map<String, Json>,
) -> mlua::Result<MethodSpec> {
    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "doc"
                | "expose"
                | "provider"
                | "access"
                | "admin"
                | "scope"
                | "params"
                | "order"
                | "result"
                | "lua_result"
                | "registration"
                | "optional"
                | "callbacks"
                | "lua"
                | "errors"
                | "poisoned"
                | "confined"
                | "baseline"
                | "host_local"
                | "host_anchor"
                | "replace_key"
                | "release_on"
                | "require_explicit_scope"
                | "requires_capabilities"
                | "lua_unchecked"
                | "history_result"
                | "lua_kind_errors"
        ) {
            return Err(error(service, format!("`{method}` has unknown field `{key}`")));
        }
    }
    let doc = match table.remove("doc") {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(s)) => s,
        _ => return Err(error(service, format!("`{method}`: `doc` must be a string"))),
    };
    let expose = match table.get("expose") {
        None | Some(Json::Null) => Expose::Lua,
        Some(Json::String(s)) => match s.as_str() {
            "lua" => Expose::Lua,
            "both" => Expose::Both,
            "remote" => {
                return Err(error(
                    service,
                    format!(
                        "`{method}`: `expose = \"remote\"` was removed; use `expose = \"both\"` or a protocol method"
                    ),
                ));
            }
            "spi" => {
                return Err(error(
                    service,
                    format!("`{method}`: `expose = \"spi\"` was removed; use `expose = \"lua\"` instead"),
                ));
            }
            value => return Err(error(service, format!("`{method}`: invalid expose `{value}`"))),
        },
        _ => return Err(error(service, format!("`{method}`: `expose` must be a string"))),
    };
    let provider = keyword_json(
        service,
        method,
        &table,
        "provider",
        |v| match v {
            "plugin" => Some(MethodProvider::Plugin),
            "host" => Some(MethodProvider::Host),
            _ => None,
        },
        MethodProvider::Plugin,
    )?;
    let access_declared = table.get("access").is_some_and(|v| !v.is_null());
    let access = keyword_json(
        service,
        method,
        &table,
        "access",
        |v| match v {
            "primary" => Some(Access::Primary),
            "peer" => Some(Access::Peer),
            "peer_default" => Some(Access::PeerDefault),
            _ => None,
        },
        Access::Primary,
    )?;
    let scope = keyword_json(
        service,
        method,
        &table,
        "scope",
        |v| match v {
            "host" => Some(Scope::Host),
            s if s == mare_shared::WORKSPACE_SERVICE_NAME => Some(Scope::Workspace),
            mare_shared::SESSION_SCOPE => Some(Scope::Session),
            _ => None,
        },
        Scope::Host,
    )?;
    let registration = bool_attr(service, method, &table, "registration")?;
    let optional = bool_attr(service, method, &table, "optional")?;
    let admin = bool_attr(service, method, &table, "admin")?;
    let confined = bool_attr(service, method, &table, "confined")?;
    let baseline = bool_attr(service, method, &table, "baseline")?;
    let host_local = bool_attr(service, method, &table, "host_local")?;
    let host_anchor = bool_attr(service, method, &table, "host_anchor")?;
    let replace_key = match table.remove("replace_key") {
        None | Some(Json::Null) => None,
        Some(Json::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(error(service, format!("`{method}`: `replace_key` cannot be empty")));
            }
            Some(if trimmed.len() == s.len() { s } else { trimmed.to_owned() })
        }
        _ => return Err(error(service, format!("`{method}`: `replace_key` must be a string"))),
    };
    let release_on_declared = table.get("release_on").is_some_and(|v| !v.is_null());
    let release_on = keyword_json(
        service,
        method,
        &table,
        "release_on",
        |v| match v {
            "callbacks" => Some(ReleaseOn::Callbacks),
            "ui_detach" => Some(ReleaseOn::UiDetach),
            "never" => Some(ReleaseOn::Never),
            _ => None,
        },
        ReleaseOn::Callbacks,
    )?;
    let require_explicit_scope = bool_attr(service, method, &table, "require_explicit_scope")?;
    let lua_unchecked = bool_attr(service, method, &table, "lua_unchecked")?;
    let history_result = bool_attr(service, method, &table, "history_result")?;
    let lua_kind_errors = bool_attr(service, method, &table, "lua_kind_errors")?;
    if provider == MethodProvider::Host {
        if registration {
            return Err(error(service, format!("`{method}`: `provider = \"host\"` cannot be a registration")));
        }
        if optional {
            return Err(error(service, format!("`{method}`: `provider = \"host\"` cannot be `optional`")));
        }
        if baseline {
            return Err(error(service, format!("`{method}`: `provider = \"host\"` cannot be `baseline`")));
        }
        if host_local {
            return Err(error(service, format!("`{method}`: `provider = \"host\"` cannot be `host_local`")));
        }
    }
    if baseline && expose != Expose::Lua {
        return Err(error(service, format!("`{method}`: `baseline = true` requires `expose = \"lua\"`")));
    }
    if baseline && optional {
        return Err(error(service, format!("`{method}`: `baseline = true` cannot be `optional`")));
    }
    let remote_flag = expose == Expose::Both;
    if host_local && !(remote_flag && registration && scope == Scope::Host) {
        return Err(error(
            service,
            format!("`{method}`: `host_local = true` requires remote registration with `scope = \"host\"`"),
        ));
    }
    if host_anchor && scope != Scope::Host {
        return Err(error(service, format!("`{method}`: `host_anchor = true` requires `scope = \"host\"`")));
    }
    if replace_key.is_some() && !(remote_flag && registration && scope == Scope::Host) {
        return Err(error(
            service,
            format!("`{method}`: `replace_key` requires remote registration with `scope = \"host\"`"),
        ));
    }
    if release_on_declared && !registration {
        return Err(error(service, format!("`{method}`: `release_on` requires `registration = true`")));
    }
    if release_on == ReleaseOn::UiDetach && scope != Scope::Session {
        return Err(error(service, format!("`{method}`: `release_on = \"ui_detach\"` requires `scope = \"session\"`")));
    }
    if require_explicit_scope && !(remote_flag && registration && scope == Scope::Host && !host_local) {
        return Err(error(
            service,
            format!("`{method}`: `require_explicit_scope = true` requires a scoped remote registration"),
        ));
    }
    let params = match table.remove("params").filter(|v| !v.is_null()) {
        None => json!({"type":"object","additionalProperties":false}),
        Some(value) => value,
    };
    let declared_result = table.remove("result").filter(|v| !v.is_null());
    if history_result && (expose != Expose::Lua || declared_result.is_none() || registration) {
        return Err(error(
            service,
            format!("`{method}`: `history_result = true` requires a Lua-only non-registration method with `result`"),
        ));
    }
    if lua_kind_errors && (expose != Expose::Lua || registration) {
        return Err(error(
            service,
            format!("`{method}`: `lua_kind_errors = true` requires a Lua-only non-registration method"),
        ));
    }
    let what = format!("`{method}`");
    let mut order = parse_order_list_json(service, &what, table.get("order"))?;
    let rest = order.last().is_some_and(|last| last == REST);
    if rest {
        order.pop();
    }
    let remote = expose == Expose::Both;
    if !remote && access_declared {
        return Err(error(service, format!("non-remote method `{method}` cannot declare `access`")));
    }
    if doc.trim().is_empty() {
        return Err(error(service, format!("method `{method}` needs a doc")));
    }
    if registration && declared_result.is_some() {
        return Err(error(service, format!("registration `{method}` has a fixed result")));
    }
    if !registration && declared_result.is_none() {
        return Err(error(service, format!("method `{method}` needs a result schema")));
    }
    let result = if registration { registration_schema() } else { declared_result.unwrap_or_else(|| json!({})) };
    let lua_result = match table.remove("lua_result") {
        None | Some(Json::Null) => None,
        Some(Json::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return Err(error(service, format!("`{method}` lua_result cannot be empty")));
            }
            Some(vec![if trimmed.len() == s.len() { s } else { trimmed.to_owned() }])
        }
        Some(Json::Array(list)) => {
            let mut types = Vec::with_capacity(list.len());
            for item in list {
                let Json::String(s) = item else {
                    return Err(error(
                        service,
                        format!("`{method}` lua_result must be a non-empty list of LuaLS types"),
                    ));
                };
                if s.trim().is_empty() {
                    return Err(error(
                        service,
                        format!("`{method}` lua_result must be a non-empty list of LuaLS types"),
                    ));
                }
                types.push(s);
            }
            if types.is_empty() {
                return Err(error(service, format!("`{method}` lua_result must be a non-empty list of LuaLS types")));
            }
            Some(types)
        }
        Some(Json::Object(map)) if map.is_empty() => {
            return Err(error(service, format!("`{method}` lua_result must be a non-empty list of LuaLS types")));
        }
        _ => return Err(error(service, format!("`{method}` lua_result must be a string or list of LuaLS types"))),
    };
    let lua_names: BTreeSet<String> = match table.get("lua") {
        Some(Json::Object(map)) => map.keys().cloned().collect(),
        _ => BTreeSet::new(),
    };
    let properties = validate_order_schema(service, &what, &params, &order, rest, &lua_names, true)?;
    let scoped_reg = remote && registration && scope == Scope::Host && !host_local;
    if scoped_reg && properties.contains("scope") {
        return Err(error(service, format!("`{method}`: scope is added by the RPC adapter")));
    }
    if (scope == Scope::Session || scoped_reg) && properties.contains("session_id") {
        return Err(error(service, format!("`{method}`: session_id is added by the RPC adapter")));
    }
    if scope == Scope::Workspace && properties.contains("cwd") {
        return Err(error(service, format!("`{method}`: cwd is added by the RPC adapter")));
    }
    compile(service, &format!("`{method}` params"), &params)?;
    compile_result(service, &format!("`{method}` result"), &result)?;
    let mut callbacks = BTreeMap::new();
    let mut remote_callbacks = BTreeMap::new();
    let shape = || error(service, format!("`{method}` callbacks map each name to its LuaLS function type"));
    match table.remove("callbacks") {
        None | Some(Json::Null) => {}
        Some(Json::Object(map)) => {
            for (name, value) in map {
                match value {
                    Json::String(kind) => {
                        callbacks.insert(name, kind);
                    }
                    Json::Object(entry) => {
                        let spec = parse_callback_json_owned(service, method, &name, entry)?;
                        callbacks.insert(name.clone(), spec.luals.clone());
                        remote_callbacks.insert(name, spec);
                    }
                    _ => return Err(shape()),
                }
            }
        }
        _ => return Err(shape()),
    }
    if provider == MethodProvider::Host && !callbacks.is_empty() {
        return Err(error(service, format!("`{method}`: `provider = \"host\"` cannot declare `callbacks`")));
    }
    if !remote_callbacks.is_empty() && !registration {
        return Err(error(service, format!("`{method}` has callbacks for JSON callers, so it must be a registration")));
    }
    if !remote_callbacks.is_empty() && properties.contains("callbacks") {
        return Err(error(service, format!("`{method}`: callbacks is reserved by the RPC adapter")));
    }
    if let Some((name, _)) = callbacks.iter().find(|(_, t)| !t.starts_with("fun(")) {
        return Err(error(service, format!("`{method}` callback `{name}` needs a LuaLS function type (`fun(...)`)")));
    }
    if !callbacks.is_empty() && !rest {
        return Err(error(service, format!("`{method}` callbacks are fields of the trailing opts (`...`)")));
    }
    match table.remove("lua") {
        None | Some(Json::Null) => {}
        Some(Json::Object(map)) => {
            for (name, val) in map {
                let Json::String(kind) = val else {
                    return Err(error(
                        service,
                        format!("`{method}` lua maps each Lua-only parameter to its LuaLS type"),
                    ));
                };
                if remote && (!rest || order.contains(&name)) {
                    return Err(error(
                        service,
                        format!("`{method}` is exported to JSON callers; Lua-only parameters have no wire form"),
                    ));
                }
                if !rest && !order.contains(&name) {
                    return Err(error(
                        service,
                        format!("`{method}` Lua-only parameter `{name}` is not in order and there are no opts"),
                    ));
                }
                callbacks.insert(name, kind);
            }
        }
        _ => return Err(error(service, format!("`{method}` lua maps each Lua-only parameter to its LuaLS type"))),
    }
    let errors = match table.remove("errors") {
        None | Some(Json::Null) => None,
        Some(Json::Object(map)) if map.is_empty() => Some(Vec::new()),
        Some(Json::Array(list)) => {
            let mut names = Vec::with_capacity(list.len());
            for item in list {
                let Json::String(s) = item else {
                    return Err(error(service, format!("`{method}` errors lists the service error names it raises")));
                };
                names.push(s);
            }
            Some(names)
        }
        _ => return Err(error(service, format!("`{method}` errors lists the service error names it raises"))),
    };
    let poisoned = keyword_json(
        service,
        method,
        &table,
        "poisoned",
        |v| match v {
            "allow" => Some(Some(true)),
            "deny" => Some(Some(false)),
            _ => None,
        },
        None,
    )?;
    let requires_capabilities = match table.remove("requires_capabilities") {
        None | Some(Json::Null) => vec![],
        Some(Json::Object(map)) if map.is_empty() => vec![],
        Some(Json::Array(list)) => {
            let mut caps = Vec::with_capacity(list.len());
            for item in list {
                let Json::String(s) = item else {
                    return Err(error(service, format!("`{method}` requires_capabilities must be a list of strings")));
                };
                if s.trim().is_empty() {
                    return Err(error(
                        service,
                        format!("`{method}` requires_capabilities must be a dense list of non-empty capability names"),
                    ));
                }
                caps.push(s);
            }
            caps
        }
        _ => return Err(error(service, format!("`{method}` requires_capabilities must be a list of strings"))),
    };
    Ok(MethodSpec {
        doc,
        expose,
        provider,
        access,
        admin,
        access_declared,
        scope,
        params,
        order,
        rest,
        result,
        lua_result,
        registration,
        optional,
        callbacks,
        remote_callbacks,
        errors,
        poisoned,
        confined,
        baseline,
        host_local,
        host_anchor,
        replace_key,
        release_on,
        require_explicit_scope,
        requires_capabilities,
        project_scoped: false,
        lua_unchecked,
        history_result,
        lua_kind_errors,
    })
}

fn parse_callback_json_owned(
    service: &str,
    method: &str,
    name: &str,
    mut entry: serde_json::Map<String, Json>,
) -> mlua::Result<CallbackSpec> {
    for key in entry.keys() {
        if !matches!(key.as_str(), "type" | "params" | "order" | "result" | "update" | "optional") {
            return Err(error(service, format!("`{method}` callback `{name}` has unknown field `{key}`")));
        }
    }
    let luals = match entry.remove("type") {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(s)) => s,
        _ => return Err(error(service, format!("`{method}` callback `{name}` type must be a string"))),
    };
    let params = match entry.remove("params").filter(|v| !v.is_null()) {
        None => json!({"type":"object","additionalProperties":false}),
        Some(value) => value,
    };
    let what = format!("`{method}` callback `{name}`");
    let order = parse_order_list_json(service, &what, entry.get("order"))?;
    validate_order_schema(service, &what, &params, &order, false, &BTreeSet::new(), false)?;
    compile(service, &format!("{what} params"), &params)?;
    let result = match entry.remove("result").filter(|v| !v.is_null()) {
        None => json!({}),
        Some(value) => value,
    };
    compile_result(service, &format!("{what} result"), &result)?;
    let update = match entry.remove("update").filter(|v| !v.is_null()) {
        None => None,
        Some(schema) => {
            compile_result(service, &format!("{what} update"), &schema)?;
            Some(schema)
        }
    };
    let optional = match entry.get("optional") {
        None | Some(Json::Null) => false,
        Some(Json::Bool(b)) => *b,
        _ => return Err(error(service, format!("`{method}` callback `{name}` optional must be a boolean"))),
    };
    Ok(CallbackSpec { luals, params, order, result, update, optional })
}

/// The service's own error codes, `<service>.<name>` → `{doc, data?}` (`services.list`).
pub fn declared_errors(service: &str, contract: &ServiceContract) -> BTreeMap<String, Json> {
    contract
        .errors
        .iter()
        .map(|(name, spec)| {
            let mut entry = json!({ "doc": spec.doc });
            if let Some(data) = &spec.data {
                entry["data"] = data.clone();
            }
            let code = if name.contains('.') { name.clone() } else { mare_shared::errors::domain_code(service, name) };
            (code, entry)
        })
        .collect()
}

/// The service errors `method` may raise, by name: its own list, or every one.
pub fn method_errors(contract: &ServiceContract, method: &str) -> BTreeMap<String, ErrorSpec> {
    let listed = contract.methods.get(method).and_then(|spec| spec.errors.as_ref());
    contract
        .errors
        .iter()
        .filter(|(name, _)| listed.is_none_or(|list| list.contains(name)))
        .map(|(name, spec)| (name.clone(), spec.clone()))
        .collect()
}

/// `errors = { name = { doc = "what it means", data = <JSON Schema>? } }`: the service's own
/// codes, raised as `<service>.<name>`.
fn parse_errors_json_owned(service: &str, value: Option<Json>) -> mlua::Result<BTreeMap<String, ErrorSpec>> {
    let shape = || error(service, "`errors` maps each error name to `{ doc = ..., data = <schema>? }`");
    let table = match value {
        None | Some(Json::Null) => return Ok(BTreeMap::new()),
        Some(Json::Object(table)) => table,
        _ => return Err(shape()),
    };
    let mut errors = BTreeMap::new();
    for (key, entry_val) in table {
        let Json::Object(mut entry) = entry_val else { return Err(shape()) };
        let valid_qualified = key
            .split_once('.')
            .is_some_and(|(svc, err_name)| service_name(svc) && mare_shared::errors::valid_name(err_name));
        if !mare_shared::errors::valid_name(&key) && !valid_qualified {
            return Err(error(service, format!("error name `{key}` is not lowercase snake_case")));
        }
        for field in entry.keys() {
            if !matches!(field.as_str(), "doc" | "data") {
                return Err(shape());
            }
        }
        let Some(Json::String(doc)) = entry.remove("doc") else { return Err(shape()) };
        let data = match entry.remove("data").filter(|v| !v.is_null()) {
            None => None,
            Some(schema) => {
                compile(service, &format!("error `{key}` data"), &schema)?;
                Some(schema)
            }
        };
        errors.insert(key, ErrorSpec { doc, data });
    }
    Ok(errors)
}

fn parse_capabilities_json_owned(service: &str, value: Option<Json>) -> mlua::Result<BTreeMap<String, CapabilitySpec>> {
    let shape = || error(service, "`capabilities` maps each capability name to `{ method = ..., doc = ... }`");
    let table = match value {
        None | Some(Json::Null) => return Ok(BTreeMap::new()),
        Some(Json::Object(table)) => table,
        _ => return Err(shape()),
    };
    let mut capabilities = BTreeMap::new();
    for (key, entry_val) in table {
        let Json::Object(mut entry) = entry_val else { return Err(shape()) };
        if !mare_shared::errors::valid_name(&key) {
            return Err(error(service, format!("capability name `{key}` is not lowercase snake_case")));
        }
        for field in entry.keys() {
            if !matches!(field.as_str(), "method" | "doc") {
                return Err(shape());
            }
        }
        let Some(Json::String(method)) = entry.remove("method") else { return Err(shape()) };
        let Some(Json::String(doc)) = entry.remove("doc") else { return Err(shape()) };
        if doc.trim().is_empty() {
            return Err(error(service, format!("capability `{key}` needs a doc")));
        }
        capabilities.insert(key, CapabilitySpec { method, doc });
    }
    Ok(capabilities)
}

fn parse_bool_field_json(name: &str, contract: &serde_json::Map<String, Json>, field: &str) -> mlua::Result<bool> {
    match contract.get(field) {
        None | Some(Json::Null) => Ok(false),
        Some(Json::Bool(b)) => Ok(*b),
        _ => Err(error(name, format!("`{field}` must be a boolean"))),
    }
}

fn parse_doc_field_json_owned(
    name: &str,
    contract: &mut serde_json::Map<String, Json>,
) -> mlua::Result<Option<String>> {
    match contract.remove("doc") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) => Ok(Some(s)),
        _ => Err(error(name, "`doc` must be a string")),
    }
}

fn parse_one_json_owned(name: &str, mut contract: serde_json::Map<String, Json>) -> mlua::Result<ServiceContract> {
    let essential = parse_bool_field_json(name, &contract, "essential")?;
    let project_scoped = parse_bool_field_json(name, &contract, "project_scoped")?;
    let has_methods = contract.get("methods").is_some_and(|v| !v.is_null());
    let has_events = contract.get("events").is_some_and(|v| !v.is_null());
    if !has_methods && has_events {
        for key in contract.keys() {
            if !matches!(key.as_str(), "doc" | "events" | "owner" | "essential") {
                return Err(error(name, format!("unknown field `{key}`")));
            }
        }
        if essential {
            return Err(error(name, "event-only contract root cannot be `essential`"));
        }
        let owner = match contract.remove("owner") {
            Some(Json::String(s)) if service_name(&s) => s,
            _ => return Err(error(name, "event-only contract root must declare a valid `owner` service")),
        };
        if !name.is_empty()
            && name.len() <= 64
            && name.as_bytes()[0].is_ascii_lowercase()
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
        {
            let events = super::contract_events::parse_events_json_owned(name, &owner, contract.remove("events"))?;
            return Ok(ServiceContract {
                doc: parse_doc_field_json_owned(name, &mut contract)?,
                owner: Some(owner),
                events,
                ..Default::default()
            });
        }
        return Err(error(name, "event root names are lowercase letters, digits, `_` and `-`"));
    }
    if !has_methods {
        return Err(error(name, "contract must declare `methods = { ... }` (name-only contracts are not supported)"));
    }
    if !service_name(name) {
        return Err(error(name, "service names are lowercase letters, digits, `_` and `-`"));
    }
    for key in contract.keys() {
        if !matches!(
            key.as_str(),
            "doc" | "methods" | "errors" | "essential" | "events" | "project_scoped" | "capabilities"
        ) {
            return Err(error(name, format!("unknown field `{key}`")));
        }
    }
    let Some(Json::Object(methods_map)) = contract.remove("methods") else {
        return Err(error(name, "`methods` must be a table"));
    };
    let mut methods = BTreeMap::new();
    for (method, entry_val) in methods_map {
        let Json::Object(table) = entry_val else {
            return Err(error(name, format!("method `{method}` must be a table")));
        };
        if !method_name(&method) || methods.len() >= MAX_METHODS {
            return Err(error(name, format!("invalid method name `{method}` or more than {MAX_METHODS} methods")));
        }
        let mut spec = parse_method_json_owned(name, &method, table)?;
        spec.project_scoped = project_scoped;
        methods.insert(method, spec);
    }
    let errors = parse_errors_json_owned(name, contract.remove("errors"))?;
    for (method, spec) in &methods {
        if let Some(unknown) = spec.errors.iter().flatten().find(|listed| !errors.contains_key(*listed)) {
            return Err(error(name, format!("`{method}` lists error `{unknown}`, which `errors` does not declare")));
        }
    }
    let capabilities = parse_capabilities_json_owned(name, contract.remove("capabilities"))?;
    for (cap_name, cap) in &capabilities {
        if !methods.contains_key(&cap.method) {
            return Err(error(name, format!("capability `{cap_name}` names unknown method `{}`", cap.method)));
        }
    }
    for (method, spec) in &methods {
        if let Some(unknown) = spec.requires_capabilities.iter().find(|cap| !capabilities.contains_key(*cap)) {
            return Err(error(name, format!("`{method}` requires unknown capability `{unknown}`")));
        }
    }
    let events = super::contract_events::parse_events_json_owned(name, name, contract.remove("events"))?;
    let poisoned_allow = if methods.values().any(|spec| spec.poisoned.is_some()) {
        Some(methods.iter().filter(|(_, spec)| spec.poisoned == Some(true)).map(|(method, _)| method.clone()).collect())
    } else {
        None
    };
    let (optional, required): (Vec<_>, Vec<_>) =
        methods.iter().filter(|(_, spec)| !spec.is_host()).partition(|(_, spec)| spec.optional);
    Ok(ServiceContract {
        required: required.into_iter().map(|(name, _)| name.clone()).collect(),
        optional: optional.into_iter().map(|(name, _)| name.clone()).collect(),
        doc: parse_doc_field_json_owned(name, &mut contract)?,
        methods,
        errors,
        essential,
        poisoned_allow,
        owner: None,
        events,
        capabilities,
    })
}

/// Canonical service contract parser operating on an owned JSON manifest by move (shared by
/// build-time pre-evaluated built-in contracts and runtime Lua plugin contracts) to avoid
/// duplicating schema trees in memory.
pub fn parse_contracts_json_owned(value: Json) -> mlua::Result<BTreeMap<String, ServiceContract>> {
    let Json::Object(contracts) = value else {
        return Err(mlua::Error::runtime("service contracts must be a table"));
    };
    let mut out = BTreeMap::new();
    let mut seen_events = BTreeSet::new();
    for (name, contract_val) in contracts {
        if name.is_empty() {
            return Err(mlua::Error::runtime("service contract name is empty"));
        }
        let Json::Object(contract) = contract_val else {
            return Err(error(&name, "service contract must be a table"));
        };
        let parsed = parse_one_json_owned(&name, contract)?;
        for spec in parsed.events.values() {
            if !seen_events.insert(spec.name.clone()) {
                return Err(error(&name, format!("duplicate event `{}` across contracts", spec.name)));
            }
        }
        out.insert(name, parsed);
    }
    for (name, contract) in &out {
        if contract.events.is_empty() {
            continue;
        }
        let owner = contract.owner.as_deref().unwrap_or(name);
        let owner_is_host_only = out.get(owner).is_some_and(super::contract_events::is_host_only_contract);
        if super::contract_events::is_protocol_reserved_event_root(name, owner, owner_is_host_only, false) {
            return Err(mlua::Error::runtime(format!(
                "event contract `{name}`: event root `{name}` is reserved by the protocol event table"
            )));
        }
    }
    Ok(out)
}

/// Canonical service contract parser operating on a borrowed JSON manifest.
pub fn parse_contracts_json(value: &Json) -> mlua::Result<BTreeMap<String, ServiceContract>> {
    parse_contracts_json_owned(value.clone())
}

/// Read service contracts from a Lua table without inferring them from any provider implementation.
pub fn parse_contracts(lua: &Lua, value: Value) -> mlua::Result<BTreeMap<String, ServiceContract>> {
    let Value::Table(contracts) = value else {
        return Err(mlua::Error::runtime("service contracts must be a table"));
    };
    let mut map = serde_json::Map::new();
    for entry in contracts.pairs::<Value, Value>() {
        let (key_val, contract_val) = entry?;
        let Value::String(key_str) = key_val else {
            return Err(mlua::Error::runtime("service contract name must be a string"));
        };
        let name = key_str.to_str()?.to_owned();
        if name.is_empty() {
            return Err(mlua::Error::runtime("service contract name is empty"));
        }
        let Value::Table(_) = contract_val else {
            return Err(error(&name, "service contract must be a table"));
        };
        let json_val = mare_shared::lua::strict_lua_to_json_bounded(
            lua,
            contract_val,
            mare_shared::lua::HISTORY_LOSSLESS_MAX_NODES,
        )?;
        map.insert(name, json_val);
    }
    parse_contracts_json_owned(Json::Object(map))
}

/// Policy decision point names declared across a Lua `contracts` manifest (`mode = "policy"`).
pub fn policy_points_from_lua(lua: &Lua, value: Value) -> mlua::Result<Vec<String>> {
    let mut points = Vec::new();
    for contract in parse_contracts(lua, value)?.into_values() {
        for spec in contract.events.into_values() {
            if spec.mode == mare_protocol::EventMode::Policy {
                points.push(spec.name);
            }
        }
    }
    Ok(points)
}

/// RPC catalog entries derived from already-parsed contracts: `service.method` → `{description, params, result}`.
pub fn rpc_methods_from_contracts<C: std::borrow::Borrow<ServiceContract>>(
    contracts: &BTreeMap<String, C>,
) -> BTreeMap<String, Json> {
    let mut out = BTreeMap::new();
    for (service, contract) in contracts {
        let contract = contract.borrow();
        for (method, spec) in contract.methods.iter().filter(|(_, spec)| spec.remote()) {
            let mut entry = json!({"description": spec.doc, "params": spec.wire_params(), "result": spec.result});
            if spec.admin {
                entry["admin"] = json!(true);
            }
            out.insert(format!("{service}.{method}"), entry);
        }
    }
    out
}

/// RPC catalog entries derived from contracts: `service.method` → `{description, params, result}`.
pub fn rpc_methods(lua: &Lua, contracts: Value) -> mlua::Result<BTreeMap<String, Json>> {
    Ok(rpc_methods_from_contracts(&parse_contracts(lua, contracts)?))
}

/// Event catalog entries for `mare rpc schema` from already-parsed contracts.
pub fn rpc_events_from_contracts<C: std::borrow::Borrow<ServiceContract>>(
    contracts: &BTreeMap<String, C>,
) -> mlua::Result<BTreeMap<String, Json>> {
    let mut out = BTreeMap::new();
    for info in mare_protocol::event_registry::list_events() {
        let key = info.name.clone();
        out.insert(key, serde_json::to_value(info).map_err(mlua::Error::runtime)?);
    }
    for contract in contracts.values() {
        let contract = contract.borrow();
        for spec in contract.events.values() {
            if out.contains_key(&spec.name) {
                return Err(mlua::Error::runtime(format!("event `{}` declared twice", spec.name)));
            }
            out.insert(spec.name.clone(), serde_json::to_value(spec.to_info()).map_err(mlua::Error::runtime)?);
        }
    }
    Ok(out)
}

/// Event catalog entries for `mare rpc schema` (`PROTOCOL_EVENTS` merged with built-in contract `events`).
pub fn rpc_events(lua: &Lua, contracts: Value) -> mlua::Result<BTreeMap<String, Json>> {
    rpc_events_from_contracts(&parse_contracts(lua, contracts)?)
}

/// Expanded protocol `components` JSON Schemas serialized as a Lua table expression (no `$ref`).
pub fn protocol_components_lua() -> String {
    fn write_lua(val: &Json, out: &mut String) {
        use std::fmt::Write as _;
        match val {
            Json::Null => out.push_str("nil"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Number(n) => {
                let _ = write!(out, "{n}");
            }
            Json::String(s) => {
                out.push('"');
                for ch in s.chars() {
                    match ch {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        c => out.push(c),
                    }
                }
                out.push('"');
            }
            Json::Array(items) => {
                out.push('{');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    write_lua(item, out);
                }
                out.push('}');
            }
            Json::Object(map) => {
                out.push('{');
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push('[');
                    write_lua(&Json::String(k.clone()), out);
                    out.push_str("] = ");
                    write_lua(v, out);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    write_lua(&mare_protocol::protocol_components(), &mut out);
    out
}

/// Positional Lua arguments → the arguments object, the inverse of [`ADAPTER`]. Each parameter
/// maps once: an opts field never overrides a positional parameter.
/// Mark the positional Lua arguments with the shapes their shaped JSON object has
/// (lua::apply_shape): named parameters by name, the trailing opts table field by field.
pub(crate) fn mark_arguments(lua: &Lua, spec: &MethodSpec, args: &mlua::MultiValue, object: &Json) -> mlua::Result<()> {
    // check first, change nothing: a call refused for a conflict leaves the caller's tables alone
    mark_arguments_with(lua, spec, args, object, mare_shared::lua::ShapeMarks::dry())?;
    mark_arguments_with(lua, spec, args, object, mare_shared::lua::ShapeMarks::default())
}

fn mark_arguments_with(
    lua: &Lua,
    spec: &MethodSpec,
    args: &mlua::MultiValue,
    object: &Json,
    mut marks: mare_shared::lua::ShapeMarks,
) -> mlua::Result<()> {
    for (index, value) in args.iter().enumerate() {
        match spec.order.get(index) {
            Some(key) => {
                if let Some(shaped) = object.get(key) {
                    mare_shared::lua::apply_shape_with(lua, value, shaped, &mut marks)?;
                }
            }
            None => {
                // the opts table as a whole: its fields' shaped values (callbacks stay out). As a
                // whole, the same table also passed in another place for other shapes is found
                // and the call refused (a top-level value has no parent to copy it in)
                if let (Value::Table(options), Some(object)) = (value, object.as_object()) {
                    let mut fields = serde_json::Map::new();
                    for pair in options.clone().pairs::<Value, Value>() {
                        if let (Value::String(key), _) = pair? {
                            let key = key.to_str()?.to_string();
                            if !spec.order.contains(&key)
                                && let Some(shaped) = object.get(&key)
                            {
                                fields.insert(key, shaped.clone());
                            }
                        }
                    }
                    mare_shared::lua::apply_shape_with(lua, value, &Json::Object(fields), &mut marks)?;
                }
            }
        }
    }
    Ok(())
}

/// Whether a hybrid `lua` type annotation lists a top-level `mare.*` userdata handle
/// (outside `fun(...)` parameter/return types), such as `table|mare.ToolSchema|fun(...)`.
fn lua_allows_userdata(kind: &str) -> bool {
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in kind.char_indices() {
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            '|' if depth == 0 => {
                if kind[start..idx].trim().starts_with("mare.") {
                    return true;
                }
                start = idx + 1;
            }
            _ => {}
        }
    }
    kind[start..].trim().starts_with("mare.")
}

pub fn positional_object(lua: &Lua, spec: &MethodSpec, args: mlua::MultiValue) -> mlua::Result<Json> {
    let props = spec.params.get("properties").and_then(Json::as_object);
    let prop_names: BTreeSet<&str> = props.map(|map| map.keys().map(String::as_str).collect()).unwrap_or_default();
    let mut obj = positional_fields(lua, &spec.order, spec.rest, &spec.callbacks, &prop_names, args.clone())?;
    // If a hybrid `table|mare.ToolSchema|fun(...)` parameter in `params.required` (like
    // `tools.register`'s `parameters`) was passed as a Lua function or userdata handle in
    // trailing opts, satisfy required-object presence for contract parameter validation while
    // keeping the original Lua value intact.
    if spec.rest
        && let Some(props) = props
        && let Some(required) = spec.params.get("required").and_then(Json::as_array)
        && let Some(Value::Table(options)) = args.get(spec.order.len())
        && let Some(map) = obj.as_object_mut()
    {
        for (name, kind) in &spec.callbacks {
            if required.iter().any(|v| v.as_str() == Some(name.as_str()))
                && props.contains_key(name)
                && !map.contains_key(name)
            {
                let raw = options.get::<Value>(name.as_str())?;
                let matched_hybrid = (kind.contains("fun(") && matches!(raw, Value::Function(_)))
                    || (lua_allows_userdata(kind) && matches!(raw, Value::UserData(_)));
                if matched_hybrid {
                    let placeholder = match props.get(name).and_then(|s| s.get("type")).and_then(Json::as_str) {
                        Some("string") => json!("_"),
                        Some("array") => json!([]),
                        Some("boolean") => json!(false),
                        Some("integer") => json!(1),
                        Some("number") => json!(0),
                        _ => json!({}),
                    };
                    map.insert(name.clone(), placeholder);
                }
            }
        }
    }
    Ok(obj)
}

pub(crate) fn positional_fields(
    lua: &Lua,
    order: &[String],
    rest: bool,
    callbacks: &BTreeMap<String, String>,
    wire_props: &BTreeSet<&str>,
    args: mlua::MultiValue,
) -> mlua::Result<Json> {
    let limit = order.len() + usize::from(rest);
    if args.iter().skip(limit).any(|v| !v.is_nil()) {
        return Err(mlua::Error::runtime(format!("too many arguments (at most {limit})")));
    }
    let mut object = serde_json::Map::new();
    for (index, value) in args.into_iter().enumerate() {
        if value.is_nil() {
            continue;
        }
        // a Lua-only positional parameter has no wire form: not encoded, not checked
        if order.get(index).is_some_and(|key| callbacks.contains_key(key)) {
            continue;
        }
        // Lua-only callback fields of the opts table stay out of the encoded arguments; hybrid
        // fields declared in `params.properties` keep their non-function wire value.
        let value = match (&value, order.get(index)) {
            (Value::Table(options), None) if !callbacks.is_empty() => {
                let copy = lua.create_table()?;
                for pair in options.pairs::<Value, Value>() {
                    let (key, field) = pair?;
                    let callback = matches!(&key, Value::String(k) if callbacks.iter().any(|(c, kind)| {
                        k.as_bytes().as_ref() == c.as_bytes()
                            && (!wire_props.contains(c.as_str())
                                || (!kind.contains("fun(") && !lua_allows_userdata(kind))
                                || (kind.contains("fun(") && matches!(field, Value::Function(_)))
                                || (lua_allows_userdata(kind) && matches!(field, Value::UserData(_))))
                    }));
                    if !callback {
                        copy.raw_set(key, field)?;
                    }
                }
                Value::Table(copy)
            }
            _ => value,
        };
        let value = mare_shared::lua::strict_lua_to_json(lua, value)?;
        match order.get(index) {
            Some(key) => {
                object.insert(key.clone(), value);
            }
            None => {
                let Json::Object(options) = value else {
                    return Err(mlua::Error::runtime("options must be a table"));
                };
                for (key, value) in options {
                    if object.contains_key(&key) || order.iter().any(|name| name == &key) {
                        return Err(mlua::Error::runtime(format!("option `{key}` is also a positional parameter")));
                    }
                    object.insert(key, value);
                }
            }
        }
    }
    Ok(Json::Object(object))
}

/// Does `result`'s schema explicitly allow `null`? Only then may a typed method return explicit `nil`.
pub(crate) fn allows_null(result: &Json) -> bool {
    let has_null_type = match result.get("type") {
        Some(Json::String(kind)) => kind == "null",
        Some(Json::Array(kinds)) => kinds.iter().any(|k| k.as_str() == Some("null")),
        _ => false,
    };
    has_null_type
        || ["anyOf", "oneOf"]
            .iter()
            .any(|key| result.get(key).and_then(Json::as_array).is_some_and(|arms| arms.iter().any(allows_null)))
}

/// How a contract method's Lua results become its one result, shared by the JSON adapter and
/// the Lua proxy: `nil, message` raises the message; an explicit `nil` without a message is an
/// error unless `nullable` (`result` schema allows `null`); no value at all is a mutation's `{}`.
pub(crate) const SETTLE: &str = r#"
local name, nullable, r = ...
if r.n == 0 then
  if nullable then return nil end
  return {}
end
if r[1] == nil then
  if r[2] ~= nil then error(r[2], 0) end
  if nullable then return nil end
  -- the same coded value the Lua guard gives (invalid_result), not text
  error({ code = "invalid_result", message = ("%s: returned nil"):format(name) }, 0)
end
return r[1]
"#;

/// One generic Lua adapter per method: JSON arguments → the provider's positional call.
/// Parameters named by `order` are positional; with `...` the remaining ones form the opts
/// table. The provider's results are settled by [`SETTLE`].
pub(crate) const ADAPTER: &str = r#"
local name, order, rest, nullable, settle = ...
local n, member = #order, {}
for i = 1, n do member[order[i]] = true end
-- a mare.Service's method takes its arguments after self; the caller is the ctx the service was
-- read through (`self.ctx`)
return function(impl, args)
  local values, count = {}, n
  for i = 1, n do values[i] = args[order[i]] end
  if rest then
    local opts
    for k, v in pairs(args) do
      if not member[k] then opts = opts or {}; opts[k] = v end
    end
    count = n + 1
    values[count] = opts
  end
  while count > 0 and values[count] == nil do count = count - 1 end
  -- a traced method is a callable table
  local f = impl[name]
  if f == nil then error(("service method `%s` is not implemented"):format(name), 0) end
  return settle(name, nullable, table.pack(f(impl, table.unpack(values, 1, count))))
end
"#;

pub use super::contract_events::{
    CORE_ONLY_EVENT_ROOTS, EventRetain, EventScope, EventSpec, is_host_only_contract, is_protocol_reserved_event_root,
    valid_event_kebab_name,
};
pub use super::luals::{luals_events, luals_schema_type, luals_types};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_callbacks_declare_wire_params_and_catalog_schemas() {
        let lua = Lua::new();
        let value: Value = lua
            .load(
                r#"return {
                    watcher = {
                        methods = {
                            watch = {
                                doc = "watch with a remote callback",
                                expose = "both",
                                registration = true,
                                params = {
                                    type = "object",
                                    properties = { prefix = { type = "string" } },
                                    required = { "prefix" },
                                    additionalProperties = false,
                                },
                                order = { "prefix", "..." },
                                callbacks = {
                                    greet = {
                                        type = "fun(name: string): string",
                                        params = {
                                            type = "object",
                                            properties = { name = { type = "string" } },
                                            required = { "name" },
                                            additionalProperties = false,
                                        },
                                        order = { "name" },
                                        result = { type = "string" },
                                    },
                                    done = {
                                        type = "fun()",
                                        optional = true,
                                    },
                                },
                            },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        let parsed = parse_contracts(&lua, value).unwrap();
        let spec = &parsed["watcher"].methods["watch"];
        assert_eq!(spec.remote_callbacks.len(), 2);
        let wire = spec.wire_params();
        assert!(wire["properties"]["callbacks"].is_object(), "{wire}");
        assert!(wire["required"].as_array().unwrap().contains(&json!("callbacks")), "{wire}");
        let described = spec.describe();
        assert_eq!(described["callbacks"]["greet"]["order"], json!(["name"]), "{described}");
        assert_eq!(described["callbacks"]["greet"]["result"], json!({"type": "string"}), "{described}");
        assert_eq!(described["callbacks"]["done"]["optional"], json!(true), "{described}");

        let non_reg: Value = lua
            .load(
                r#"return {
                    bad = {
                        methods = {
                            call = {
                                doc = "not a registration",
                                expose = "both",
                                order = { "..." },
                                result = { type = "string" },
                                callbacks = { cb = { type = "fun()" } },
                            },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        let err = parse_contracts(&lua, non_reg).unwrap_err().to_string();
        assert!(err.contains("registration"), "{err}");
    }

    #[test]
    fn remote_callbacks_reject_property_conflicts_and_invalid_callback_order() {
        let lua = Lua::new();
        for (label, snippet) in [
            (
                "callbacks property conflict",
                r#"return { svc = { methods = { watch = {
                    doc = "conflict", expose = "both", registration = true,
                    params = { type = "object", properties = { callbacks = { type = "array" } }, additionalProperties = false },
                    order = { "callbacks", "..." },
                    callbacks = { cb = { type = "fun()" } },
                } } } }"#,
            ),
            (
                "duplicate callback order",
                r#"return { svc = { methods = { watch = {
                    doc = "dup", expose = "both", registration = true,
                    order = { "..." },
                    callbacks = { cb = {
                        type = "fun(a: string)",
                        params = { type = "object", properties = { a = { type = "string" } }, additionalProperties = false },
                        order = { "a", "a" },
                    } },
                } } } }"#,
            ),
            (
                "sparse callback order",
                r#"return { svc = { methods = { watch = {
                    doc = "sparse", expose = "both", registration = true,
                    order = { "..." },
                    callbacks = { cb = {
                        type = "fun(a: string)",
                        params = { type = "object", properties = { a = { type = "string" } }, additionalProperties = false },
                        order = { [1] = "a", [3] = "a" },
                    } },
                } } } }"#,
            ),
            (
                "uncovered callback property",
                r#"return { svc = { methods = { watch = {
                    doc = "uncovered", expose = "both", registration = true,
                    order = { "..." },
                    callbacks = { cb = {
                        type = "fun(a: string)",
                        params = { type = "object", properties = { a = { type = "string" }, b = { type = "string" } }, required = { "b" }, additionalProperties = false },
                        order = { "a" },
                    } },
                } } } }"#,
            ),
        ] {
            let value: Value = lua.load(snippet).eval().unwrap();
            let err = parse_contracts(&lua, value).expect_err(label).to_string();
            assert!(!err.is_empty(), "{label}");
        }
    }

    #[test]
    fn workspace_scope_adds_required_cwd_and_rejects_declared_cwd() {
        let lua = Lua::new();
        let valid: Value = lua
            .load(
                r#"return { ws = { methods = { get = {
                    doc = "read workspace", expose = "both", scope = "workspace",
                    params = { type = "object", properties = {}, additionalProperties = false },
                    result = { type = "object" },
                } } } }"#,
            )
            .eval()
            .unwrap();
        let parsed = parse_contracts(&lua, valid).unwrap();
        let wire = parsed["ws"].methods["get"].wire_params();
        assert_eq!(wire["properties"]["cwd"], json!({ "type": "string" }));
        assert!(wire["required"].as_array().unwrap().contains(&json!("cwd")));

        let dup: Value = lua
            .load(
                r#"return { ws = { methods = { get = {
                    doc = "conflict", expose = "both", scope = "workspace",
                    params = { type = "object", properties = { cwd = { type = "string" } }, additionalProperties = false },
                    order = { "cwd" },
                    result = { type = "object" },
                } } } }"#,
            )
            .eval()
            .unwrap();
        let err = parse_contracts(&lua, dup).unwrap_err().to_string();
        assert!(err.contains("cwd"), "{err}");
    }

    #[test]
    fn host_provider_parsing_and_combination_validation() {
        let lua = Lua::new();
        let valid: Value = lua
            .load(
                r#"return {
                    kernel = {
                        methods = {
                            request_lease = {
                                doc = "acquire turn lease",
                                expose = "lua",
                                provider = "host",
                                lua = { cancel = "mare.Token" },
                                order = { "..." },
                                result = { type = { "object", "null" } },
                            },
                            list_sessions = {
                                doc = "list loaded sessions",
                                expose = "both",
                                access = "peer",
                                scope = "host",
                                provider = "host",
                                params = { type = "object", additionalProperties = false },
                                result = { type = "object", properties = { data = { type = "array" } }, required = { "data" }, additionalProperties = false },
                            },
                            plugin_method = {
                                doc = "implemented by plugin",
                                expose = "lua",
                                provider = "plugin",
                                params = { type = "object", additionalProperties = false },
                                result = { type = "boolean" },
                            },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        let parsed = parse_contracts(&lua, valid).unwrap();
        let kernel = &parsed["kernel"];
        assert!(kernel.required.iter().all(|m| m != "request_lease" && m != "list_sessions"));
        assert!(kernel.required.iter().any(|m| m == "plugin_method"));

        let both_host: Value = lua
            .load(r#"return { svc = { methods = { f = { doc = "f", expose = "both", provider = "host", result = { type = "boolean" } } } } }"#)
            .eval()
            .unwrap();
        assert!(parse_contracts(&lua, both_host).unwrap()["svc"].methods["f"].is_host());

        for (label, snippet) in [
            (
                "expose remote removed",
                r#"return { svc = { methods = { f = { doc = "f", expose = "remote", provider = "host", result = { type = "boolean" } } } } }"#,
            ),
            (
                "unknown provider",
                r#"return { svc = { methods = { f = { doc = "f", provider = "remote", result = { type = "boolean" } } } } }"#,
            ),
            (
                "host + registration",
                r#"return { svc = { methods = { f = { doc = "f", expose = "both", provider = "host", registration = true } } } }"#,
            ),
            (
                "host + lua callbacks",
                r#"return { svc = { methods = { f = { doc = "f", expose = "lua", provider = "host", order = { "..." }, callbacks = { on_step = "fun()" }, result = { type = "boolean" } } } } }"#,
            ),
            (
                "host + optional",
                r#"return { svc = { methods = { f = { doc = "f", expose = "lua", provider = "host", optional = true, result = { type = "boolean" } } } } }"#,
            ),
            (
                "host + baseline",
                r#"return { svc = { methods = { f = { doc = "f", expose = "spi", provider = "host", baseline = true, result = { type = "boolean" } } } } }"#,
            ),
            (
                "host + expose spi",
                r#"return { svc = { methods = { f = { doc = "f", expose = "spi", provider = "host", result = { type = "boolean" } } } } }"#,
            ),
            (
                "host + access on lua",
                r#"return { svc = { methods = { f = { doc = "f", expose = "lua", access = "peer", provider = "host", result = { type = "boolean" } } } } }"#,
            ),
            (
                "host under protocol root",
                r#"return { config = { methods = { describe = { doc = "describe", expose = "both", provider = "host", result = { type = "object" } } } } }"#,
            ),
        ] {
            let value: Value = lua.load(snippet).eval().unwrap();
            let err = parse_contracts(&lua, value).expect_err(label).to_string();
            assert!(!err.is_empty(), "{label}");
        }
    }

    #[test]
    fn nullable_zero_return_settles_to_nil_and_trailing_nils_are_ignored() {
        let lua = Lua::new();
        let settle: mlua::Function = lua.load(SETTLE).into_function().unwrap();
        let empty = lua.create_table().unwrap();
        empty.set("n", 0).unwrap();

        let nullable_zero: Value = settle.call(("approvals.gate", true, empty.clone())).unwrap();
        assert!(nullable_zero.is_nil(), "nullable method returning 0 values must settle to nil, got {nullable_zero:?}");

        let non_nullable_zero: Value = settle.call(("usage.set_context", false, empty)).unwrap();
        assert!(matches!(non_nullable_zero, Value::Table(_)), "void non-nullable method must still settle to {{}}");

        let mut trailing_nil = mlua::MultiValue::new();
        trailing_nil.push_back(Value::Nil);
        let obj = positional_fields(&lua, &[], false, &BTreeMap::new(), &BTreeSet::new(), trailing_nil)
            .expect("trailing nil arguments must be ignored in positional_fields");
        assert_eq!(obj, json!({}));

        let mut extra_non_nil = mlua::MultiValue::new();
        extra_non_nil.push_back(Value::Integer(1));
        assert!(
            positional_fields(&lua, &[], false, &BTreeMap::new(), &BTreeSet::new(), extra_non_nil).is_err(),
            "non-nil extra arguments must still be rejected"
        );
    }

    #[test]
    fn core_only_event_root_is_allowed_only_for_host_only_owner_contract() {
        let lua = Lua::new();
        let valid: Value = lua
            .load(
                r#"return {
                    kernel = {
                        methods = {
                            session_epoch = { doc = "epoch", expose = "lua", provider = "host", result = { type = "string" } },
                        },
                    },
                    option = {
                        owner = "kernel",
                        doc = "Option transaction events.",
                        events = {
                            apply = { doc = "Apply option.", mode = "emit", expose = "process" },
                            current = { doc = "Read option.", mode = "bail", expose = "process" },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        let parsed = parse_contracts(&lua, valid).unwrap();
        assert_eq!(parsed["option"].events["apply"].expose, EventReach::Process);
        assert_eq!(parsed["option"].events["current"].expose, EventReach::Process);

        for (label, snippet) in [
            (
                "plugin-owned core root",
                r#"return {
                    svc = {
                        methods = {
                            ping = { doc = "ping", expose = "lua", result = { type = "boolean" } },
                        },
                    },
                    option = {
                        owner = "svc",
                        events = { apply = { doc = "Apply", expose = "process" } },
                    },
                }"#,
            ),
            (
                "mixed host+plugin owner core root",
                r#"return {
                    mixed = {
                        methods = {
                            a = { doc = "a", expose = "lua", provider = "host", result = { type = "boolean" } },
                            b = { doc = "b", expose = "lua", provider = "plugin", result = { type = "boolean" } },
                        },
                    },
                    option = {
                        owner = "mixed",
                        events = { apply = { doc = "Apply", expose = "process" } },
                    },
                }"#,
            ),
            (
                "methodless owner core root",
                r#"return {
                    empty = { methods = {} },
                    option = {
                        owner = "empty",
                        events = { apply = { doc = "Apply", expose = "process" } },
                    },
                }"#,
            ),
        ] {
            let value: Value = lua.load(snippet).eval().unwrap();
            let err = parse_contracts(&lua, value).expect_err(label).to_string();
            assert!(err.contains("reserved"), "{label}: {err}");
        }
    }

    #[test]
    fn admin_attribute_parses_and_defaults_false() {
        let lua = Lua::new();
        let valid: Value = lua
            .load(
                r#"return {
                    svc = {
                        methods = {
                            remote_op = { doc = "r", expose = "both", admin = true, result = { type = "object" } },
                            lua_op = { doc = "l", expose = "lua", admin = true, result = { type = "boolean" } },
                            normal_op = { doc = "n", expose = "both", result = { type = "boolean" } },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        let parsed = parse_contracts(&lua, valid).expect("valid admin declarations");
        let svc = &parsed["svc"];
        assert_eq!(svc.methods["remote_op"].describe().get("admin"), Some(&json!(true)));
        assert_eq!(svc.methods["lua_op"].describe().get("admin"), Some(&json!(true)));
        assert_eq!(svc.methods["normal_op"].describe().get("admin"), None);

        let bad_type: Value = lua
            .load(
                r#"return {
                    svc = {
                        methods = {
                            bad_op = { doc = "s", expose = "both", admin = "yes", result = { type = "boolean" } },
                        },
                    },
                }"#,
            )
            .eval()
            .unwrap();
        assert!(parse_contracts(&lua, bad_type).is_err(), "non-boolean admin must be rejected");
    }

    #[test]
    fn contract_validation_covers_provider_names_doc_and_result_invariants() {
        assert_eq!(MethodProvider::Plugin.as_str(), "plugin");
        assert_eq!(MethodProvider::Host.as_str(), "host");
        let lua = Lua::new();
        let valid: Value = lua
            .load(r#"return { svc = { methods = { ping = { doc = "ping", expose = "lua", provider = "host", result = { type = "boolean" } } } } }"#)
            .eval()
            .unwrap();
        let parsed = parse_contracts(&lua, valid).unwrap();
        assert_eq!(parsed["svc"].methods["ping"].provider_name(), "host");

        for (label, snippet) in [
            (
                "host + host_local",
                r#"return { svc = { methods = { f = { doc = "f", expose = "both", provider = "host", host_local = true, result = { type = "boolean" } } } } }"#,
            ),
            (
                "host_local without registration",
                r#"return { svc = { methods = { f = { doc = "f", expose = "both", host_local = true, result = { type = "boolean" } } } } }"#,
            ),
            (
                "empty doc",
                r#"return { svc = { methods = { f = { doc = "   ", expose = "lua", result = { type = "boolean" } } } } }"#,
            ),
            (
                "registration with result",
                r#"return { svc = { methods = { f = { doc = "f", expose = "lua", registration = true, result = { type = "boolean" } } } } }"#,
            ),
            ("missing result", r#"return { svc = { methods = { f = { doc = "f", expose = "lua" } } } }"#),
            (
                "empty lua_result",
                r#"return { svc = { methods = { f = { doc = "f", expose = "lua", result = { type = "boolean" }, lua_result = "  " } } } }"#,
            ),
            (
                "bad lua_result type",
                r#"return { svc = { methods = { f = { doc = "f", expose = "lua", result = { type = "boolean" }, lua_result = 42 } } } }"#,
            ),
        ] {
            let value: Value = lua.load(snippet).eval().unwrap();
            assert!(parse_contracts(&lua, value).is_err(), "expected error for {label}");
        }
    }

    #[test]
    fn json_byte_len_exceeds_matches_serde_json_exact_length() {
        let empty_wrapped = serde_json::to_vec(&result_wrapper(&json!({}))).unwrap().len();
        assert_eq!(empty_wrapped - 2, RESULT_WRAPPER_OVERHEAD_BYTES);

        let samples = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(-123456789),
            json!(1.25),
            json!("plain ascii"),
            json!("quote\"slash\\nl\ncr\rtab\tbs\x08ff\x0cctrl\x01\x1f"),
            json!("unicode 日本語 🚀"),
            json!([]),
            json!([1, "a", true, null, { "k\"": [2, 3] }]),
            json!({}),
            result_wrapper(&json!({
                "type": "object",
                "properties": { "msg": { "type": "string", "description": "line1\n\"line2\"" } },
                "required": ["msg"]
            })),
        ];
        for sample in samples {
            let exact = serde_json::to_vec(&sample).unwrap().len();
            assert!(!json_byte_len_exceeds(&sample, exact), "exact limit {exact} should fit for {sample}");
            if exact > 0 {
                assert!(json_byte_len_exceeds(&sample, exact - 1), "limit {} should fail for {sample}", exact - 1);
            }
        }
    }
}
