//! Declared JSON service interfaces; provider-specific names never enter host dispatch.
use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Contract visibility for a service method.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MethodExpose {
    /// Callable from Lua plugins and over remote RPC.
    Both,
    /// Callable from Lua plugins only.
    Lua,
}

impl MethodExpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Lua => "lua",
        }
    }
}

/// Execution scope for a service method.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MethodScope {
    /// Host-wide method.
    Host,
    /// Workspace-scoped method.
    Workspace,
    /// Session-scoped method: takes a required `session_id` and binds callbacks to that session.
    Session,
}

impl MethodScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Workspace => "workspace",
            Self::Session => "session",
        }
    }
}

/// A method of a registered service. For a service that already has a contract, omit
/// parameters, result and order: the remote implements the declared interface.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MethodSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Positional Lua arguments, in order, mapped to keys of the JSON parameters object.
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub doc: String,
    /// The same contract model as Lua: `both` (default for a registration) or `lua`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expose: Option<MethodExpose>,
    /// `primary` (default: never grantable to a peer), `peer` (grantable by `peer.allow`), or
    /// `peer_default` (open to peers without listing in `peer.allow`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<crate::MethodAccess>,
    /// `host` (default), `workspace` or `session`: a session method takes `session_id` and its
    /// callback is bound to that session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<MethodScope>,
}

/// One contract method as callers see it (the runtime catalog, like `nvim_get_api_info`).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MethodInfo {
    pub doc: String,
    /// Contract visibility (`both`; only remotely callable methods are listed).
    pub expose: MethodExpose,
    /// `primary` (never grantable to a peer), `peer` (grantable by `peer.allow`), or
    /// `peer_default` (open to peers without listing in `peer.allow`).
    pub access: crate::MethodAccess,
    /// `host`, `workspace` or `session`; a session method takes a required `session_id`.
    pub scope: MethodScope,
    pub parameters: Value,
    /// Positional Lua arguments; a trailing `...` is the opts table of the remaining ones.
    pub order: Vec<String>,
    pub result: Value,
    /// Lua gets a disposer; JSON callers get `{registration_id}` for `registrations.release`.
    pub registration: bool,
    pub optional: bool,
    pub unconfined: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceInfo {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub methods: BTreeMap<String, MethodInfo>,
}

/// `services.list` result.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServicesListResult {
    pub data: Vec<ServiceInfo>,
}

/// Release one registration this connection holds; answered once it is withdrawn.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseParams {
    pub registration_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReleaseResult {}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterParams {
    pub name: String,
    pub methods: BTreeMap<String, MethodSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<crate::RegistrationScopeKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Registration {
    pub registration_id: String,
    pub name: String,
}
