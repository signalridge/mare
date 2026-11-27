//! Single authorization decision point for RPC connections: every protocol method, contract
//! operation, event stream, baseline read, registration scope, hook, image path, answerer claim,
//! and path-based session load is decided by [`authorize`] from protocol/contract metadata and
//! live host config.

use super::*;

/// Public event types any peer may subscribe to via `events.subscribe` without listing
/// `events.subscribe` in `peer.allow`.
pub(super) const PEER_DEFAULT_EVENTS: &[&str] = &[proto::event_registry::EVENT_STATUS_CHANGED];

/// Every authorizable operation across the RPC layer.
#[derive(Clone, Copy, Debug)]
pub(super) enum Op<'a> {
    /// Protocol-layer RPC method (`options.get`, `services.list`, `ui.attach`, ...).
    Protocol(&'a str),
    /// Contract service operation resolved against `host.kernel.service_method`.
    Contract { service: &'a str, method: &'a str },
    /// Contract service operation whose `access` and `admin` metadata are already known.
    ContractMeta { service: &'a str, method: &'a str, access: &'a str, admin: bool },
    /// Full event stream access (auto-subscriptions, retained snapshot state).
    EventsStream,
    /// `events.subscribe` call with optional `include` / `exclude` filters.
    EventsSubscribe { include: Option<&'a [String]>, exclude: &'a [String] },
    /// Live delivery check for one event name on an active subscription.
    Event(&'a str),
    /// `scope = "all"` registration or subscription.
    GlobalScope,
    /// Re-injecting `scope = "all"` global registrations into a newly opened session runtime.
    GlobalRegistrations,
    /// Claiming or exercising UI/approval/elicitation answerer/responder authority.
    Answerer,
    /// Loading a session by arbitrary filesystem path (and restoring uncapped session settings).
    SessionLoadByPath,
    /// Active remote hook channel check (re-evaluates the hook subscription contract, e.g. `hooks.subscribe`).
    HookActive { service: &'a str, method: &'a str },
    /// Subscribing a remote hook to a bus event name.
    HookEvent(&'a str),
    /// Reading a local filesystem path as an image attachment.
    ImagePath,
}

/// Structured reason an authorization check was denied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DenyReason {
    RemoteAdminRequired,
    PeerNotAllowed,
    PrimaryOnly,
    EventsFilterRestricted,
    EventNotAllowed,
    ProcessEventForbidden,
    PeerHookEventNotPublic,
}

/// Result of [`authorize`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Allow,
    Deny(DenyReason),
}

impl Decision {
    pub(super) fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }

    pub(super) fn into_protocol_error(self, method: &str) -> Result<(), proto::RpcError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny(DenyReason::RemoteAdminRequired) => {
                Err(proto::RpcError::coded("denied", format!("`{method}` requires remote_admin")))
            }
            Self::Deny(_) => Err(proto::RpcError::core(
                proto::ErrorCode::MethodNotFound,
                format!(
                    "`{method}` is not allowed on the peer socket (the session can allow it: peer = {{ allow = {{ \"{method}\" }} }})"
                ),
            )),
        }
    }

    pub(super) fn into_service_error(self, service: &str, method: &str) -> Result<(), proto::RpcError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny(DenyReason::RemoteAdminRequired) => {
                Err(proto::RpcError::coded("denied", format!("`{service}.{method}` requires remote_admin")))
            }
            Self::Deny(_) => {
                Err(proto::RpcError::coded("denied", format!("`{service}.{method}` is not allowed on this connection")))
            }
        }
    }

    pub(super) fn into_workspace_service_error(self, service: &str, method: &str) -> Result<(), proto::RpcError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny(DenyReason::RemoteAdminRequired) => Err(proto::RpcError::coded(
                "denied",
                format!("`{service}.{method}` requires remote_admin for every session in the workspace"),
            )),
            Self::Deny(_) => Err(proto::RpcError::coded(
                "denied",
                format!("`{service}.{method}` is not allowed on every session in the workspace"),
            )),
        }
    }

    pub(super) fn into_error(self, op_label: &str) -> Result<(), proto::RpcError> {
        match self {
            Self::Allow => Ok(()),
            Self::Deny(DenyReason::RemoteAdminRequired) => {
                Err(proto::RpcError::coded("denied", format!("`{op_label}` requires remote_admin")))
            }
            Self::Deny(DenyReason::EventsFilterRestricted) => Err(proto::RpcError::coded(
                "denied",
                format!(
                    "default peers may only subscribe explicitly to `{}`",
                    proto::event_registry::EVENT_STATUS_CHANGED
                ),
            )),
            Self::Deny(DenyReason::PrimaryOnly) => {
                Err(proto::RpcError::coded("denied", format!("{op_label} is only allowed on the primary connection")))
            }
            Self::Deny(DenyReason::ProcessEventForbidden) => Err(proto::RpcError::coded(
                "denied",
                format!("`{op_label}` is `expose = \"process\"` and cannot be subscribed remotely"),
            )),
            Self::Deny(DenyReason::PeerHookEventNotPublic) => {
                Err(proto::RpcError::coded("denied", format!("not a public event for a peer hook: {op_label}")))
            }
            Self::Deny(_) => {
                Err(proto::RpcError::coded("denied", format!("`{op_label}` is not allowed on this connection")))
            }
        }
    }
}

pub(super) fn has_method(method: &str) -> bool {
    mare_protocol::protocol_methods().contains_key(method)
}

pub(super) fn protocol_methods() -> impl Iterator<Item = &'static str> {
    mare_protocol::protocol_methods().keys().map(String::as_str)
}

pub(super) fn is_peer_default_method(method: &str) -> bool {
    mare_protocol::method_access(method) == mare_protocol::MethodAccess::PeerDefault
}

pub(super) fn matches_event_glob(pat: &str, event: &str) -> bool {
    match pat.strip_suffix('*') {
        Some(prefix) => event.starts_with(prefix),
        None => pat == event,
    }
}

fn peer_allow_matches(cfg: &serde_json::Value, mut pred: impl FnMut(&str) -> bool) -> bool {
    cfg.get("peer")
        .and_then(|p| p.get("allow"))
        .and_then(|a| a.as_array())
        .is_some_and(|a| a.iter().filter_map(|v| v.as_str()).any(&mut pred))
}

fn with_shared_config<R>(host: &Host, f: impl FnOnce(&serde_json::Value) -> R) -> R {
    let shared = host.kernel.shared_config();
    let guard = shared.lock().unwrap_or_else(|e| e.into_inner());
    f(&guard)
}

fn peer_allow_exact(host: &Host, name: &str) -> bool {
    with_shared_config(host, |cfg| peer_allow_matches(cfg, |entry| entry == name))
}

fn peer_allow_contract_exact(host: &Host, service: &str, method: &str) -> bool {
    with_shared_config(host, |cfg| {
        peer_allow_matches(cfg, |entry| entry.split_once('.').is_some_and(|(s, m)| s == service && m == method))
    })
}

pub(super) fn events_stream_allowed_in(primary: bool, cfg: &serde_json::Value) -> bool {
    primary || peer_allow_matches(cfg, |entry| entry == methods::EVENTS_SUBSCRIBE)
}

pub(super) fn baseline_page_allowed_in(primary: bool, cfg: &serde_json::Value) -> bool {
    events_stream_allowed_in(primary, cfg)
        && (primary || peer_allow_matches(cfg, |entry| entry == methods::BASELINE_READ))
}

pub(super) fn baseline_page_access(conn: &Conn, host: &Host) -> Option<mare_kernel::BaselineAccess> {
    let primary = conn.is_owner();
    let allowed = primary || with_shared_config(host, |cfg| baseline_page_allowed_in(false, cfg));
    allowed
        .then(|| Arc::new(move |config: &J| baseline_page_allowed_in(primary, config)) as mare_kernel::BaselineAccess)
}

/// Whether `include`/`exclude` strictly matches [`PEER_DEFAULT_EVENTS`] (`status/changed` only).
fn is_default_peer_events_filter(include: Option<&[String]>, exclude: &[String]) -> bool {
    include.is_some_and(|inc| {
        inc.len() == PEER_DEFAULT_EVENTS.len() && inc.iter().zip(PEER_DEFAULT_EVENTS).all(|(a, b)| a == b)
    }) && exclude.is_empty()
}

/// Whether event `name` may pass a connection's active event subscription filter (`events.subscribe`
/// grant or default-peer `status/changed` subscription, restricted to external events for peers).
/// Used inside kernel tap closures holding `&J`.
pub(super) fn event_allowed_in(
    primary: bool,
    cfg: &serde_json::Value,
    event_log: &mare_kernel::EventLog,
    name: &str,
) -> bool {
    primary
        || (event_log.is_external_event(name)
            && (events_stream_allowed_in(false, cfg) || PEER_DEFAULT_EVENTS.contains(&name)))
}

/// Single authorization decision point for every RPC operation on `conn` targeting `host`.
pub(super) fn authorize(conn: &Conn, host: &Host, op: Op<'_>) -> Decision {
    match op {
        Op::Protocol(method) => {
            if conn.is_owner() {
                if conn.is_remote_admin_on(host) || !mare_protocol::method_admin(method) {
                    Decision::Allow
                } else {
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                }
            } else {
                match mare_protocol::method_access(method) {
                    mare_protocol::MethodAccess::PeerDefault => Decision::Allow,
                    mare_protocol::MethodAccess::Peer if peer_allow_exact(host, method) => Decision::Allow,
                    mare_protocol::MethodAccess::Peer | mare_protocol::MethodAccess::Primary => {
                        Decision::Deny(DenyReason::PeerNotAllowed)
                    }
                }
            }
        }
        Op::Contract { service, method } => {
            let spec = host.kernel.service_method(service, method);
            if conn.is_owner() {
                let admin = spec.is_some_and(|s| s.admin);
                if conn.is_remote_admin_on(host) || !admin {
                    Decision::Allow
                } else {
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                }
            } else {
                match spec {
                    Some(s) => authorize(
                        conn,
                        host,
                        Op::ContractMeta { service, method, access: s.access_name(), admin: s.admin },
                    ),
                    None => Decision::Deny(DenyReason::PeerNotAllowed),
                }
            }
        }
        Op::ContractMeta { service, method, access, admin } => {
            if conn.is_owner() {
                if conn.is_remote_admin_on(host) || !admin {
                    Decision::Allow
                } else {
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                }
            } else {
                match access {
                    "peer_default" => Decision::Allow,
                    "peer" if peer_allow_contract_exact(host, service, method) => Decision::Allow,
                    _ => Decision::Deny(DenyReason::PeerNotAllowed),
                }
            }
        }
        Op::EventsStream => {
            if conn.is_owner() || with_shared_config(host, |cfg| events_stream_allowed_in(false, cfg)) {
                Decision::Allow
            } else {
                Decision::Deny(DenyReason::PeerNotAllowed)
            }
        }
        Op::EventsSubscribe { include, exclude } => {
            if authorize(conn, host, Op::EventsStream).is_allowed() || is_default_peer_events_filter(include, exclude) {
                Decision::Allow
            } else {
                Decision::Deny(DenyReason::EventsFilterRestricted)
            }
        }
        Op::Event(name) => {
            // Hot path in `stream_pump::drain_all`: zero-allocation, single-lock check.
            // Delegates to `event_allowed_in` so peer external-only & grant rules live in one place.
            if conn.is_owner()
                || with_shared_config(host, |cfg| event_allowed_in(false, cfg, host.kernel.event_log(), name))
            {
                Decision::Allow
            } else {
                Decision::Deny(DenyReason::EventNotAllowed)
            }
        }
        Op::HookActive { service, method } => authorize(conn, host, Op::Contract { service, method }),
        Op::GlobalScope | Op::ImagePath => {
            if conn.is_owner() {
                Decision::Allow
            } else {
                Decision::Deny(DenyReason::PrimaryOnly)
            }
        }
        Op::GlobalRegistrations | Op::Answerer | Op::SessionLoadByPath => {
            if !conn.is_owner() {
                Decision::Deny(DenyReason::PrimaryOnly)
            } else if conn.is_remote_admin_on(host) {
                Decision::Allow
            } else {
                Decision::Deny(DenyReason::RemoteAdminRequired)
            }
        }
        Op::HookEvent(event) => {
            if host.kernel.is_process_event(event) {
                Decision::Deny(DenyReason::ProcessEventForbidden)
            } else if !conn.is_owner() && !host.kernel.is_public_declared_event(event) {
                Decision::Deny(DenyReason::PeerHookEventNotPublic)
            } else {
                Decision::Allow
            }
        }
    }
}

pub(super) fn audit_remote_protocol(conn: &Conn, host: &Host, method: &str, decision: Decision) {
    if !conn.is_remote_transport() {
        return;
    }
    match decision {
        Decision::Deny(DenyReason::RemoteAdminRequired) => {
            tracing::warn!(
                target: "mare::web_audit",
                conn_id = conn.id.0,
                method = %method,
                "remote protocol call denied (remote_admin = false)"
            );
        }
        Decision::Allow if !is_peer_default_method(method) => {
            tracing::info!(
                target: "mare::web_audit",
                conn_id = conn.id.0,
                method = %method,
                remote_admin = conn.is_remote_admin_on(host),
                "remote protocol call"
            );
        }
        _ => {}
    }
}

pub(super) fn audit_remote_service(
    conn: &Conn,
    host: &Host,
    service: &str,
    method: &str,
    access: &str,
    decision: Decision,
) {
    if !conn.is_remote_transport() {
        return;
    }
    if !decision.is_allowed() {
        tracing::warn!(
            target: "mare::web_audit",
            conn_id = conn.id.0,
            operation = %format!("{service}.{method}"),
            remote_admin = conn.is_remote_admin_on(host),
            "remote service call denied"
        );
    } else if access == "primary" {
        tracing::info!(
            target: "mare::web_audit",
            conn_id = conn.id.0,
            operation = %format!("{service}.{method}"),
            remote_admin = conn.is_remote_admin_on(host),
            "remote primary service call"
        );
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn non_admin_owner_cannot_register_subagent_types() {
        let lua = mlua::Lua::new();
        let val: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let contracts = mare_kernel::contracts::parse_contracts(&lua, val).unwrap();
        assert!(contracts["subagent"].methods["register_type"].admin);
        assert!(!contracts["subagent"].methods["list_types"].admin);
        assert!(contracts["workspace"].methods["set_trust"].admin);
        assert!(!contracts["workspace"].methods["get"].admin);
        assert!(mare_protocol::method_admin(methods::OPTIONS_SET));
        assert!(mare_protocol::method_admin(methods::SERVICES_REGISTER));
        assert!(!mare_protocol::method_admin(methods::OPTIONS_GET));
    }

    /// Every protocol method's `admin` attribute must match `mare_protocol::method_admin`.
    #[test]
    fn protocol_admin_attribute_matches_runtime_lookup() {
        let schema = mare_protocol::schema();
        for (method, entry) in schema["methods"].as_object().unwrap() {
            let declared = entry.get("admin").and_then(serde_json::Value::as_bool).unwrap_or(false);
            assert_eq!(
                declared,
                mare_protocol::method_admin(method),
                "{method}: protocol admin={declared} disagrees with mare_protocol::method_admin"
            );
        }
    }

    #[test]
    fn undeclared_and_omitted_protocol_methods_default_closed() {
        assert_eq!(mare_protocol::MethodAccess::from_schema_value(None), mare_protocol::MethodAccess::Primary);
        assert_eq!(
            mare_protocol::MethodAccess::from_schema_value(Some("unexpected")),
            mare_protocol::MethodAccess::Primary
        );
        assert_eq!(mare_protocol::method_access("nonexistent.method"), mare_protocol::MethodAccess::Primary);
    }

    #[test]
    fn history_methods_are_grantable_to_peers_and_confined() {
        let lua = mlua::Lua::new();
        let contracts: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let session: mlua::Table = contracts.get("session").unwrap();
        let methods_tbl: mlua::Table = session.get("methods").unwrap();
        for name in ["search", "search_items", "get_item", "trace", "trace_item"] {
            let spec: mlua::Table = methods_tbl.get(name).unwrap();
            assert_eq!(spec.get::<String>("expose").unwrap(), "both", "{name}");
            assert_eq!(
                spec.get::<Option<String>>("access").unwrap().as_deref(),
                Some("peer"),
                "{name} must declare access = peer"
            );
            assert_eq!(
                spec.get::<Option<bool>>("confined").unwrap(),
                Some(true),
                "{name} must declare confined = true"
            );
        }
    }

    #[test]
    fn session_write_methods_are_grantable_to_peers_and_transition_is_lua_only() {
        let lua = mlua::Lua::new();
        let contracts: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let session: mlua::Table = contracts.get("session").unwrap();
        let methods: mlua::Table = session.get("methods").unwrap();

        // All remote session read and write methods are grantable to peers by full
        // operation name (default-denied unless granted in `peer.allow`).
        for name in [
            "fork",
            "snapshot",
            "close",
            "rewind",
            "add_record",
            "update",
            "label_item",
            "archive",
            "unarchive",
            "delete",
        ] {
            let spec: mlua::Table = methods.get(name).unwrap();
            assert_eq!(
                spec.get::<Option<String>>("access").unwrap().as_deref(),
                Some("peer"),
                "session.{name} must declare access = peer"
            );
            assert_eq!(spec.get::<Option<String>>("expose").unwrap().as_deref(), Some("both"), "{name}");
        }

        let transition: mlua::Table = methods.get("transition").unwrap();
        let tr_expose: Option<String> = transition.get("expose").unwrap();
        assert!(matches!(tr_expose.as_deref(), None | Some("lua")), "session.transition must stay expose=lua");
    }

    #[test]
    fn primary_only_methods_cannot_be_granted_by_schema() {
        for method in [methods::SERVICES_REGISTER, methods::OPTIONS_SET, methods::STATE_COMPARE_EXCHANGE] {
            assert_eq!(
                mare_protocol::method_access(method),
                mare_protocol::MethodAccess::Primary,
                "{method} must declare access = primary"
            );
            assert!(has_method(method), "{method} must be in protocol catalog");
        }
    }

    #[test]
    fn peer_allow_requires_exact_events_subscribe_grant() {
        assert!(!events_stream_allowed_in(
            false,
            &json!({ "peer": { "allow": ["*", "events", "events*", "status*"] } })
        ));
        assert!(events_stream_allowed_in(false, &json!({ "peer": { "allow": ["events.subscribe"] } })));
    }

    /// Every protocol method root is either reserved (never a service name) or a built-in
    /// service whose contract owns the operation, so a grant names one thing.
    #[test]
    fn protocol_roots_are_reserved_or_owned_by_a_builtin_contract() {
        let lua = mlua::Lua::new();
        let contracts: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        for method in protocol_methods() {
            let root = method.split('.').next().unwrap();
            assert!(
                mare_kernel::contracts::PROTOCOL_ROOTS.contains(&root) || contracts.contains_key(root).unwrap(),
                "protocol root `{root}` ({method}) is neither reserved nor a built-in service"
            );
        }
    }

    /// Return the prefix of `src` before the first top-level inline `#[cfg(test)]\nmod <name> { ... }`
    /// block, preserving production code that follows external `#[cfg(test)] mod foo_tests;` declarations.
    pub(in crate::rpc) fn strip_inline_cfg_test_mod(src: &str) -> &str {
        let mut offset = 0;
        let lines: Vec<&str> = src.split_inclusive('\n').collect();
        for (i, &line) in lines.iter().enumerate() {
            if line.trim_end() == "#[cfg(test)]" {
                let mut j = i + 1;
                while j < lines.len() {
                    let next = lines[j].trim();
                    if next.is_empty() || next.starts_with("#[") || next.starts_with("//") {
                        j += 1;
                        continue;
                    }
                    let is_inline_mod = (next.starts_with("mod ") || next.contains(" mod ")) && next.contains('{');
                    if is_inline_mod {
                        return &src[..offset];
                    }
                    break;
                }
            }
            offset += line.len();
        }
        src
    }

    /// `access::authorize` must be the sole authorization decision point in `src/rpc/`,
    /// owning all `peer.allow` and `remote_admin` policy checks (except `is_remote_admin_on`
    /// transport/config lookup on `Conn` in `mod.rs`).
    #[test]
    fn authorize_is_the_single_authorization_decision_point() {
        let rpc_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rpc");
        let access_src = std::fs::read_to_string(rpc_dir.join("access.rs")).unwrap();
        let access_prod = strip_inline_cfg_test_mod(&access_src);

        assert!(
            access_prod.contains("fn authorize("),
            "access.rs must define a single `authorize(conn, host, op)` decision point"
        );
        assert!(
            access_prod.contains("enum Decision"),
            "access.rs must define a typed `Decision` enum returned by `authorize`"
        );
        assert!(access_prod.contains("enum Op"), "access.rs must define a typed `Op` enum passed to `authorize`");

        let mut scanned_lines: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut scattered = Vec::new();
        for ent in std::fs::read_dir(&rpc_dir).unwrap() {
            let path = ent.unwrap().path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.ends_with(".rs") || name.ends_with("_tests.rs") || name == "access.rs" {
                continue;
            }
            let src = std::fs::read_to_string(&path).unwrap();
            let prod = strip_inline_cfg_test_mod(&src);
            scanned_lines.insert(name.to_owned(), prod.lines().count());
            for (idx, line) in prod.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains("peer_allow")
                    || trimmed.contains("\"peer\"") && trimmed.contains("\"allow\"")
                    || trimmed.contains("withheld_for_non_admin_contract")
                    || trimmed.contains("is_remote_admin_protocol_method")
                    || (name != "mod.rs" && trimmed.contains("is_remote_admin_on("))
                {
                    scattered.push(format!("{name}:{}: {trimmed}", idx + 1));
                }
            }
        }
        assert!(scanned_lines["mod.rs"] >= 2000, "mod.rs scan truncated: {:?}", scanned_lines.get("mod.rs"));
        assert!(
            scanned_lines["ui_bridge.rs"] >= 450,
            "ui_bridge.rs scan truncated: {:?}",
            scanned_lines.get("ui_bridge.rs")
        );
        assert!(scanned_lines["serve.rs"] >= 700, "serve.rs scan truncated: {:?}", scanned_lines.get("serve.rs"));
        assert!(
            scattered.is_empty(),
            "peer.allow / remote_admin authorization checks must go through `access::authorize`, not scattered call sites:\n{}",
            scattered.join("\n")
        );
    }

    /// `access.rs` production code must read policy from protocol/contract metadata rather than
    /// hardcoding service or method names.
    #[test]
    fn access_module_contains_no_operation_name_literals() {
        let access_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rpc/access.rs");
        let src = std::fs::read_to_string(&access_path).unwrap();
        let prod = strip_inline_cfg_test_mod(&src);

        assert!(
            !prod.contains("REMOTE_ADMIN_PROTOCOL_METHODS"),
            "access.rs must not hardcode REMOTE_ADMIN_PROTOCOL_METHODS"
        );
        assert!(
            !prod.contains("withheld_for_non_admin_contract"),
            "access.rs must not hardcode withheld_for_non_admin_contract"
        );

        // Extract quoted string literals outside comments and check none name a service or `a.b` op.
        let mut bad = Vec::new();
        for (idx, line) in prod.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") {
                continue;
            }
            let bytes = line.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'"' {
                    let start = i + 1;
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == b'\\' {
                            i += 2;
                            continue;
                        }
                        if bytes[i] == b'"' {
                            break;
                        }
                        i += 1;
                    }
                    if i < bytes.len() {
                        let lit = &line[start..i];
                        let is_op_shape = lit.contains('.')
                            && lit.split('.').count() == 2
                            && lit
                                .chars()
                                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
                        let is_hardcoded_service = matches!(
                            lit,
                            "llm"
                                | "tools"
                                | "hooks"
                                | "commands"
                                | "ui"
                                | "mcp"
                                | "web"
                                | "plugins"
                                | "fs"
                                | "proc"
                                | "process"
                                | "terminal"
                                | "cells"
                                | "sandbox"
                                | "subagent"
                                | "webserver"
                                | "permissions"
                                | "approvals"
                                | "workspace"
                                | "session"
                        );
                        if is_op_shape || is_hardcoded_service {
                            bad.push(format!("access.rs:{}: {lit:?} in `{trimmed}`", idx + 1));
                        }
                    }
                }
                i += 1;
            }
        }
        assert!(
            bad.is_empty(),
            "access.rs production code must not contain hardcoded service/operation literals:\n{}",
            bad.join("\n")
        );
    }

    #[tokio::test]
    async fn decision_table() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = crate::HOST_TEST_LOCK.lock().await;
                let dir = tempfile::tempdir().unwrap();
                let opts = mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Rpc,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": { "permissions": { "enabled": false } },
                        "webserver": { "remote_admin": false },
                        "peer": {
                            "allow": [
                                "services.list",
                                "session.search",
                                "session.update",
                                "options.set",
                                "session.*",
                                "events.subscribe"
                            ]
                        }
                    }),
                    load_init: false,
                    project_trusted: true,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                };
                let manager = mare_host::HostManager::new();
                let rt = manager.create_session(opts.clone()).await.unwrap();
                let rpc = RpcRuntime::new(manager, opts, rt.host.clone(), Some(rt.session_id.clone()), false);

                let mk_conn = |principal: Principal| -> Conn {
                    let (out, _rx) = mpsc::channel(8);
                    let (event_barrier, _eb_rx) = mpsc::channel(4);
                    Conn {
                        id: mare_host::ConnId(1),
                        rpc: rpc.clone(),
                        host: rt.host.clone(),
                        out,
                        out_budget: Arc::new(tokio::sync::Semaphore::new(1024)),
                        out_slots: Arc::new(tokio::sync::Semaphore::new(8)),
                        pending: Pending::default(),
                        conn_cancel: tokio_util::sync::CancellationToken::new(),
                        requests: RefCell::new(HashMap::new()),
                        session_ctxs: RefCell::new(HashMap::new()),
                        anchor_ctx: RefCell::new(None),
                        global_registrations: RefCell::new(GlobalRegistrations::default()),
                        reg_gate: Rc::new(tokio::sync::Semaphore::new(1)),
                        next_id: Cell::new(1),
                        caps: RefCell::new(proto::ClientCapabilities::default()),
                        cap_revision: Cell::new(0),
                        cap_gate: Rc::new(tokio::sync::Semaphore::new(1)),
                        timeout: Cell::new(Duration::from_secs(5)),
                        approval_timeout: Cell::new(None),
                        default_filter: RefCell::new(EventFilter::default()),
                        default_subscribe_all: Cell::new(principal.is_owner()),
                        explicit_sessions: RefCell::new(std::collections::HashSet::new()),
                        suppressed_subscriptions: RefCell::new(std::collections::HashSet::new()),
                        deferred_subscriptions: RefCell::new(HashMap::new()),
                        subscriptions: RefCell::new(HashMap::new()),
                        tracking_failures: RefCell::new(HashMap::new()),
                        sub_wake: Arc::new(tokio::sync::Notify::new()),
                        handshake: Cell::new(HandshakeState::Ready),
                        shutdown_requested: Cell::new(false),
                        shutdown: Cell::new(false),
                        shutdown_notify: tokio::sync::Notify::new(),
                        closed: Cell::new(false),
                        overflowed: Cell::new(false),
                        overflow_notify: tokio::sync::Notify::new(),
                        ws_close_code: None,
                        event_barrier,
                        output_lost: Cell::new(false),
                        inflight: Cell::new(0),
                        inflight_notify: tokio::sync::Notify::new(),
                        principal,
                        remote_transport: !principal.is_remote_admin(),
                        peer_pid: None,
                    }
                };

                let local = mk_conn(Principal::Owner { remote_admin: true });
                let remote = mk_conn(Principal::Owner { remote_admin: false });
                let peer = mk_conn(Principal::Peer);

                // Protocol ops
                assert_eq!(authorize(&local, &rt.host, Op::Protocol(methods::OPTIONS_SET)), Decision::Allow);
                assert_eq!(
                    authorize(&remote, &rt.host, Op::Protocol(methods::OPTIONS_SET)),
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                );
                assert_eq!(authorize(&remote, &rt.host, Op::Protocol(methods::OPTIONS_GET)), Decision::Allow);
                assert_eq!(authorize(&peer, &rt.host, Op::Protocol(methods::PEER_INFO)), Decision::Allow);
                assert_eq!(authorize(&peer, &rt.host, Op::Protocol(methods::SERVICES_LIST)), Decision::Allow);
                assert_eq!(
                    authorize(&peer, &rt.host, Op::Protocol(methods::OPTIONS_SET)),
                    Decision::Deny(DenyReason::PeerNotAllowed)
                );

                // Contract ops (exact match only; wildcard `session.*` must not grant `session.label_item`)
                assert_eq!(
                    authorize(&peer, &rt.host, Op::Contract { service: "session", method: "search" }),
                    Decision::Allow
                );
                assert_eq!(
                    authorize(&peer, &rt.host, Op::Contract { service: "session", method: "update" }),
                    Decision::Allow
                );
                assert_eq!(
                    authorize(&peer, &rt.host, Op::Contract { service: "session", method: "label_item" }),
                    Decision::Deny(DenyReason::PeerNotAllowed)
                );
                assert_eq!(
                    authorize(&remote, &rt.host, Op::Contract { service: "hooks", method: "subscribe" }),
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                );
                assert_eq!(
                    authorize(&local, &rt.host, Op::Contract { service: "hooks", method: "subscribe" }),
                    Decision::Allow
                );

                // Events, global scope, image path, options, hooks
                assert_eq!(authorize(&peer, &rt.host, Op::EventsStream), Decision::Allow);
                assert_eq!(authorize(&peer, &rt.host, Op::Event("turn/started")), Decision::Allow);
                // Non-external internal events are denied to peers by `authorize`, even with `events.subscribe`.
                assert_eq!(
                    authorize(&peer, &rt.host, Op::Event("internal/private-kernel-event")),
                    Decision::Deny(DenyReason::EventNotAllowed)
                );
                assert_eq!(authorize(&local, &rt.host, Op::Event("internal/private-kernel-event")), Decision::Allow);
                assert_eq!(authorize(&peer, &rt.host, Op::GlobalScope), Decision::Deny(DenyReason::PrimaryOnly));
                assert_eq!(authorize(&local, &rt.host, Op::GlobalScope), Decision::Allow);
                assert_eq!(
                    authorize(&remote, &rt.host, Op::GlobalRegistrations),
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                );
                assert_eq!(authorize(&local, &rt.host, Op::GlobalRegistrations), Decision::Allow);
                assert_eq!(authorize(&peer, &rt.host, Op::ImagePath), Decision::Deny(DenyReason::PrimaryOnly));
                assert_eq!(
                    authorize(&peer, &rt.host, Op::HookActive { service: "hooks", method: "subscribe" }),
                    Decision::Deny(DenyReason::PeerNotAllowed)
                );
                assert_eq!(
                    authorize(&remote, &rt.host, Op::HookActive { service: "hooks", method: "subscribe" }),
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                );
                assert_eq!(
                    authorize(&local, &rt.host, Op::HookActive { service: "hooks", method: "subscribe" }),
                    Decision::Allow
                );
                assert_eq!(authorize(&peer, &rt.host, Op::Answerer), Decision::Deny(DenyReason::PrimaryOnly));
                assert_eq!(authorize(&remote, &rt.host, Op::Answerer), Decision::Deny(DenyReason::RemoteAdminRequired));
                assert_eq!(authorize(&local, &rt.host, Op::Answerer), Decision::Allow);
                assert_eq!(authorize(&peer, &rt.host, Op::SessionLoadByPath), Decision::Deny(DenyReason::PrimaryOnly));
                assert_eq!(
                    authorize(&remote, &rt.host, Op::SessionLoadByPath),
                    Decision::Deny(DenyReason::RemoteAdminRequired)
                );
                assert_eq!(authorize(&local, &rt.host, Op::SessionLoadByPath), Decision::Allow);

                // Live toggle: `webserver.remote_admin = true` unlocks admin ops (including HookActive,
                // Answerer, and SessionLoadByPath) for remote owner, and clearing `peer.allow` restricts
                // peer to default events (`status/changed`) only.
                let mut cfg = rt.host.kernel.config();
                cfg["webserver"] = json!({ "remote_admin": true });
                cfg["peer"] = json!({ "allow": [] });
                rt.host.kernel.set_config(cfg);

                assert_eq!(authorize(&remote, &rt.host, Op::Protocol(methods::OPTIONS_SET)), Decision::Allow);
                assert_eq!(
                    authorize(&remote, &rt.host, Op::HookActive { service: "hooks", method: "subscribe" }),
                    Decision::Allow
                );
                assert_eq!(authorize(&remote, &rt.host, Op::GlobalRegistrations), Decision::Allow);
                assert_eq!(authorize(&remote, &rt.host, Op::Answerer), Decision::Allow);
                assert_eq!(authorize(&remote, &rt.host, Op::SessionLoadByPath), Decision::Allow);
                assert_eq!(authorize(&peer, &rt.host, Op::EventsStream), Decision::Deny(DenyReason::PeerNotAllowed));
                assert_eq!(
                    authorize(&peer, &rt.host, Op::Event("turn/started")),
                    Decision::Deny(DenyReason::EventNotAllowed)
                );
                assert_eq!(
                    authorize(&peer, &rt.host, Op::Event(proto::event_registry::EVENT_STATUS_CHANGED)),
                    Decision::Allow
                );
                let default_inc = vec![proto::event_registry::EVENT_STATUS_CHANGED.to_string()];
                assert_eq!(
                    authorize(&peer, &rt.host, Op::EventsSubscribe { include: Some(&default_inc), exclude: &[] }),
                    Decision::Allow
                );
                assert_eq!(
                    authorize(&peer, &rt.host, Op::EventsSubscribe { include: None, exclude: &[] }),
                    Decision::Deny(DenyReason::EventsFilterRestricted)
                );

                let _ = rpc.manager.shutdown_all().await;
            })
            .await;
    }
}
