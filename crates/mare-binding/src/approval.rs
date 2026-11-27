//! Native evidence of an actual UI/RPC/parent human response. Lua can consume but cannot issue it.
use mare_shared::once::OneTimeRegistry;
use mlua::{Lua, Table};
use serde_json::Value as J;
use std::fmt::Write as _;
use std::sync::{
    Arc, Mutex, Weak,
    atomic::{AtomicU64, Ordering},
};

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Binding {
    owner: String,
    turn: String,
    request: String,
    snapshot: String,
}

impl Binding {
    pub fn from_json(value: &J) -> Option<Self> {
        let object = value.as_object()?;
        if object.len() != 4 {
            return None;
        }
        let field = |name| object.get(name)?.as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        Some(Self {
            owner: field("owner")?,
            turn: field("turn")?,
            request: field("request")?,
            snapshot: field("snapshot")?,
        })
    }
    pub fn from_lua(lua: &Lua, table: Table) -> mlua::Result<Self> {
        let value = mare_shared::lua::strict_lua_to_json(lua, mlua::Value::Table(table))?;
        Self::from_json(&value).ok_or_else(|| mlua::Error::runtime("invalid human approval request binding"))
    }
}

#[derive(Default)]
struct Responses(Mutex<OneTimeRegistry<String, Binding, Response>>);

#[derive(Clone)]
pub struct Snapshot {
    pub tool: String,
    pub call_id: Option<String>,
    pub arguments: J,
    pub requested_permissions: Option<J>,
    pub unconfined: bool,
    pub protected_arguments: bool,
    pub origin: Option<J>,
    pub scoped: bool,
    pub reason: Option<String>,
    pub amendment_description: Option<String>,
}

struct SnapshotEntry {
    snapshot: Snapshot,
    lease_id: u64,
}

#[derive(Default)]
struct Snapshots {
    entries: Arc<Mutex<std::collections::HashMap<Binding, SnapshotEntry>>>,
    next_lease: AtomicU64,
}

pub struct SnapshotLease {
    entries: Weak<Mutex<std::collections::HashMap<Binding, SnapshotEntry>>>,
    binding: Binding,
    lease_id: u64,
    released: bool,
}

impl SnapshotLease {
    pub fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let Some(entries) = self.entries.upgrade() {
            let mut guard = entries.lock().unwrap_or_else(|e| e.into_inner());
            if guard.get(&self.binding).is_some_and(|entry| entry.lease_id == self.lease_id) {
                guard.remove(&self.binding);
            }
        }
    }
}

impl Drop for SnapshotLease {
    fn drop(&mut self) {
        self.release();
    }
}

impl mlua::UserData for SnapshotLease {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("release", |_, this, ()| {
            this.release();
            Ok(())
        });
        methods.add_meta_method_mut(mlua::MetaMethod::Close, |_, this, _err: Option<mlua::Value>| {
            this.release();
            Ok(())
        });
    }
}

impl Snapshot {
    pub fn from_action_lua(lua: &Lua, action: Table) -> mlua::Result<Self> {
        let field = |name: &str| -> mlua::Result<Option<J>> {
            match action.get::<mlua::Value>(name)? {
                mlua::Value::Nil => Ok(None),
                value => mare_shared::lua::strict_lua_to_json(lua, value).map(Some),
            }
        };
        let call = field("call")?;
        let info = field("info")?;
        let verdict = field("verdict")?;
        let call = call.as_ref();
        let info = info.as_ref();
        let verdict = verdict.as_ref();
        let tool = call
            .and_then(|c| c.get("name"))
            .and_then(J::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| "tool".to_string());
        let call_id = call
            .and_then(|c| c.get("id"))
            .and_then(J::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let arguments = call.and_then(|c| c.get("arguments")).cloned().unwrap_or(J::Null);
        let requested_permissions =
            verdict.and_then(|v| v.get("requested_permissions")).filter(|v| v.is_object()).cloned();
        let protected_arguments = call.and_then(|c| c.get("protected_arguments")).and_then(J::as_bool).unwrap_or(false);
        let origin = call.and_then(|c| c.get("origin")).filter(|v| !v.is_null()).cloned();
        let unconfined = info.and_then(|i| i.get("unconfined")).and_then(J::as_bool).unwrap_or(false);
        let scoped = info.and_then(|i| i.get("permission_request")).and_then(J::as_bool).unwrap_or(false)
            || verdict.and_then(|v| v.get("permission_scope")).is_some_and(|v| !v.is_null());
        let reason = verdict.and_then(|v| v.get("reason")).and_then(J::as_str).map(str::to_owned);
        let amendment_description = verdict.and_then(|v| v.get("amendment")).and_then(|a| {
            if origin.is_some() {
                a.is_object().then(|| describe_rule(a))
            } else {
                a.get("description")
                    .and_then(J::as_str)
                    .map(str::to_owned)
                    .or_else(|| a.is_object().then(|| describe_rule(a)))
            }
        });
        Ok(Self {
            tool,
            call_id,
            arguments,
            requested_permissions,
            unconfined,
            protected_arguments,
            origin,
            scoped,
            reason,
            amendment_description,
        })
    }
}

pub fn describe_rule(rule: &J) -> String {
    let show = |v: &J| match v {
        J::String(s) => s.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    let s = |k: &str| rule.get(k).filter(|v| !v.is_null()).map(show);
    let words = |k: &str| -> Vec<String> {
        rule.get(k).and_then(J::as_array).map(|arr| arr.iter().map(show).collect()).unwrap_or_default()
    };
    let argv = words("argv");
    let what = if !argv.is_empty() {
        format!("commands that start with `{}`", argv.join(" "))
    } else if let Some(tool) = s("tool") {
        format!("`{tool}` calls")
    } else {
        return format!("this rule: {}", serde_json::to_string(rule).unwrap_or_default());
    };
    let d = s("decision");
    let mut text = match d.as_deref() {
        Some("allow") => format!("don't ask again for {what}"),
        Some("prompt") => format!("always ask before {what}"),
        Some("forbidden") => format!("always refuse {what}"),
        _ => format!("{} {what}", d.as_deref().unwrap_or("(no decision)")),
    };
    if let Some(scope) = s("scope") {
        let _ = write!(text, ", with `{scope}` access");
    }
    if !argv.is_empty() {
        match s("cwd") {
            Some(cwd) => {
                let _ = write!(text, ", in {cwd}");
            }
            None => text.push_str(", in any directory"),
        }
        let deny = words("deny_args");
        if !deny.is_empty() {
            let _ = write!(text, " (not with {})", deny.join(" "));
        }
    }
    if let Some(expires) = s("expires").filter(|e| e != "never") {
        if expires == "session" {
            text.push_str(", for this sub-agent's session");
        } else {
            let _ = write!(text, ", expires {expires}");
        }
    }
    text
}

pub fn register_snapshot(lua: &Lua, binding: Binding, snapshot: Snapshot) -> mlua::Result<SnapshotLease> {
    if lua.app_data_ref::<Snapshots>().is_none() {
        lua.set_app_data(Snapshots::default());
    }
    let snapshots = lua.app_data_ref::<Snapshots>().expect("installed above");
    let mut entries = snapshots.entries.lock().unwrap_or_else(|e| e.into_inner());
    if entries.contains_key(&binding) {
        return Err(mlua::Error::runtime("approval snapshot already registered for binding"));
    }
    let lease_id = snapshots.next_lease.fetch_add(1, Ordering::Relaxed);
    entries.insert(binding.clone(), SnapshotEntry { snapshot, lease_id });
    Ok(SnapshotLease { entries: Arc::downgrade(&snapshots.entries), binding, lease_id, released: false })
}

pub fn get_snapshot(lua: &Lua, binding: &Binding) -> Option<Snapshot> {
    let snapshots = lua.app_data_ref::<Snapshots>()?;
    snapshots.entries.lock().unwrap_or_else(|e| e.into_inner()).get(binding).map(|entry| entry.snapshot.clone())
}
#[derive(Clone)]
pub struct Response {
    arguments: J,
    decision: String,
}
impl Response {
    fn allows(&self) -> bool {
        matches!(self.decision.as_str(), "allow" | "allow_session" | "allow_amend")
    }
}

/// Only native code calls this after receiving a real frontend response. The token remains JSON,
/// while the owner/turn/request/snapshot record is private to this Lua VM and consumed once.
pub fn issue(lua: &Lua, binding: Binding, arguments: J, decision: &str) -> String {
    if lua.app_data_ref::<Responses>().is_none() {
        lua.set_app_data(Responses::default());
    }
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes[..]);
    let token = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let responses = lua.app_data_ref::<Responses>().expect("installed above");
    responses.0.lock().unwrap_or_else(|e| e.into_inner()).insert(
        token.clone(),
        binding,
        Response { arguments, decision: decision.into() },
    );
    token
}

pub fn consume(lua: &Lua, token: Option<String>, binding: &Binding, arguments: Option<&J>) -> Option<Response> {
    let token = token?;
    let responses = lua.app_data_ref::<Responses>()?;
    let mut records = responses.0.lock().unwrap_or_else(|e| e.into_inner());
    if arguments
        .is_some_and(|expected| records.get(&token, binding).map(|response| &response.arguments) != Some(expected))
    {
        return None;
    }
    records.take(&token, binding)
}

pub fn check(
    lua: &Lua,
    token: Option<String>,
    binding: &Binding,
    arguments: Option<&J>,
    decision: Option<&str>,
) -> bool {
    let Some(token) = token else { return false };
    let Some(responses) = lua.app_data_ref::<Responses>() else { return false };
    let records = responses.0.lock().unwrap_or_else(|e| e.into_inner());
    let Some(approved) = records.get(&token, binding) else { return false };
    arguments.is_none_or(|expected| expected == &approved.arguments)
        && decision.is_none_or(|expected| expected == approved.decision)
}

pub fn consume_receipt(lua: &Lua, receipt: &J, snapshot: Option<&str>, arguments: Option<&J>) -> bool {
    let Some(binding) = receipt.get("binding").and_then(Binding::from_json) else { return false };
    if snapshot.is_some_and(|snapshot| snapshot != binding.snapshot) {
        return false;
    }
    let token = receipt.get("token").and_then(|v| v.as_str()).map(str::to_owned);
    consume(lua, token, &binding, arguments).is_some_and(|response| {
        response.allows() && receipt.get("decision").and_then(J::as_str) == Some(response.decision.as_str())
    })
}

/// Public audit data is deliberately independent of the full decision payload. In particular
/// no argument, schema, path, permission profile, amendment, snapshot or free-form reason leaves.
pub fn public_result(value: &J) -> J {
    let call = value.get("call").unwrap_or(&J::Null);
    let call_id = call.get("tool_call_id").and_then(J::as_str).unwrap_or_default();
    let mut result = serde_json::json!({"call":{"tool_call_id":call_id,
        "name":call.get("name").and_then(J::as_str).unwrap_or("tool")},
        "decision": value
            .get("decision")
            .and_then(J::as_str)
            .filter(|v| matches!(*v, "allow" | "allow_session" | "allow_amend" | "deny" | "abort" | "unavailable"))
            .unwrap_or("deny"),
        "source": value
            .get("source")
            .and_then(J::as_str)
            .filter(|v| matches!(*v, "user" | "policy" | "reviewer" | "rule" | "approval_policy" | "cancelled" | "error" | "none" | "hook" | "host"))
            .unwrap_or("policy")});
    if let Some(outcome @ ("timeout" | "cancelled")) = value.get("outcome").and_then(J::as_str) {
        result["outcome"] = outcome.into();
    }
    if let Some(id) = value.get("child_approval_id").and_then(J::as_u64) {
        result["child_approval_id"] = id.into();
    }
    if let Some(scope @ ("turn" | "session")) = value.get("scope").and_then(J::as_str) {
        result["scope"] = scope.into();
    }
    result
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let approvals = lua.create_table()?;
    approvals.set(
        "public_result",
        lua.create_function(|lua, full: Table| {
            let call = full.get::<Option<Table>>("call")?;
            let call_id = call.as_ref().and_then(|call| call.get::<Option<String>>("tool_call_id").ok().flatten());
            let mut data = serde_json::json!({"call":{"tool_call_id":call_id,
            "name":call.as_ref().and_then(|call|call.get::<Option<String>>("name").ok().flatten())}});
            for field in ["decision", "source", "outcome", "scope"] {
                if let Some(value) = full.get::<Option<String>>(field)? {
                    data[field] = value.into();
                }
            }
            if let Some(id) = full.get::<Option<u64>>("child_approval_id")? {
                data["child_approval_id"] = id.into();
            }
            mare_shared::lua::json_to_lua(lua, &public_result(&data))
        })?,
    )?;

    approvals.set(
        "check_human",
        lua.create_function(
            |lua, (token, binding, arguments, decision): (Option<String>, Table, mlua::Value, String)| {
                let arguments =
                    if arguments.is_nil() { J::Null } else { mare_shared::lua::strict_lua_to_json(lua, arguments)? };
                Ok(check(lua, token, &Binding::from_lua(lua, binding)?, Some(&arguments), Some(&decision)))
            },
        )?,
    )?;
    approvals.set(
        "consume_human",
        lua.create_function(
            |lua,
             (token, binding, arguments, revision, decision): (
                Option<String>,
                Table,
                mlua::Value,
                Option<u64>,
                String,
            )| {
                let arguments =
                    if arguments.is_nil() { J::Null } else { mare_shared::lua::strict_lua_to_json(lua, arguments)? };
                let binding = Binding::from_lua(lua, binding)?;
                let Some(approved) = consume(lua, token, &binding, Some(&arguments)) else {
                    return Ok((false, None));
                };
                if !approved.allows() || approved.decision != decision {
                    return Ok((false, None));
                }
                let authorization = revision
                    .map(|revision| crate::projection::authorize(lua, approved.arguments, revision, &binding.snapshot))
                    .transpose()?;
                Ok((true, authorization))
            },
        )?,
    )?;
    approvals.set(
        "register_snapshot",
        lua.create_function(|lua, (binding, action): (Table, Table)| {
            let binding = Binding::from_lua(lua, binding)?;
            let snapshot = Snapshot::from_action_lua(lua, action)?;
            register_snapshot(lua, binding, snapshot)
        })?,
    )?;
    mare.set("approvals", approvals)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_evidence_is_bound_to_owner_turn_request_snapshot_vm_and_one_consumption() {
        let lua = Lua::new();
        let json = serde_json::json!({"owner":"service","turn":"turn","request":"request","snapshot":"snapshot"});
        let binding = Binding::from_json(&json).unwrap();
        let args = serde_json::json!({"token":"approved reference"});
        let token = issue(&lua, binding.clone(), args.clone(), "allow");
        for key in ["owner", "turn", "request", "snapshot"] {
            let mut wrong = json.clone();
            wrong[key] = "different".into();
            assert!(consume(&lua, Some(token.clone()), &Binding::from_json(&wrong).unwrap(), Some(&args)).is_none());
        }
        assert!(consume(&Lua::new(), Some(token.clone()), &binding, Some(&args)).is_none());
        assert!(!check(
            &lua,
            Some(token.clone()),
            &binding,
            Some(&serde_json::json!({"token":"other reference"})),
            Some("allow")
        ));
        assert!(check(&lua, Some(token.clone()), &binding, Some(&args), Some("allow")));
        assert!(
            consume(&lua, Some(token.clone()), &binding, Some(&serde_json::json!({"token":"other reference"})))
                .is_none()
        );
        assert!(consume(&lua, Some(token.clone()), &binding, Some(&args)).is_some());
        assert!(consume(&lua, Some(token), &binding, Some(&args)).is_none());
    }

    #[test]
    fn snapshot_cannot_be_overwritten_or_unregistered_without_lease() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let json = serde_json::json!({"owner":"service","turn":"turn","request":"req-1","snapshot":"snap-1"});
        let binding = Binding::from_json(&json).unwrap();
        let original = Snapshot {
            tool: "secret_tool".into(),
            call_id: Some("call-1".into()),
            arguments: serde_json::json!({"token": "ref-1"}),
            requested_permissions: None,
            unconfined: true,
            protected_arguments: true,
            origin: None,
            scoped: false,
            reason: None,
            amendment_description: None,
        };
        let mut lease = register_snapshot(&lua, binding.clone(), original).unwrap();

        let (has_unreg, user_ok): (bool, bool) = lua
            .load(
                r#"
                local has_unreg = mare.approvals.unregister_snapshot ~= nil
                local ok, lease = pcall(mare.approvals.register_snapshot,
                    { owner = "user", turn = "turn", request = "req-user", snapshot = "snap-user" },
                    { call = { name = "harmless_read", arguments = { token = "ref-1" }, protected_arguments = false } })
                if ok and lease then lease:release() end
                return has_unreg, ok
                "#,
            )
            .set_name("@user/plugin.lua")
            .eval()
            .unwrap();
        assert!(!has_unreg, "unregister_snapshot must not be exposed to Lua");
        assert!(user_ok, "register_snapshot must be available to user plugins for fresh bindings");

        let (overwrite_ok, overwrite_err): (bool, String) = lua
            .load(
                r#"
                local function attempt()
                    return mare.approvals.register_snapshot(
                        { owner = "service", turn = "turn", request = "req-1", snapshot = "snap-1" },
                        { call = { name = "harmless_read", arguments = { token = "ref-1" }, protected_arguments = false } }
                    )
                end
                local ok, err = pcall(attempt)
                return ok, tostring(err)
                "#,
            )
            .set_name("@user/plugin.lua")
            .eval()
            .unwrap();
        assert!(!overwrite_ok, "register_snapshot must reject overwriting an active binding");
        assert!(overwrite_err.contains("already registered"), "unexpected error: {overwrite_err}");

        let current = get_snapshot(&lua, &binding).unwrap();
        assert_eq!(current.tool, "secret_tool");
        assert!(current.protected_arguments);

        lease.release();
        assert!(get_snapshot(&lua, &binding).is_none());
    }

    #[test]
    fn consume_human_requires_matching_evidence_once_and_child_cannot_spoof_amendment_description() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let json = serde_json::json!({"owner":"service","turn":"turn","request":"req-2","snapshot":"snap-2"});
        let binding = Binding::from_json(&json).unwrap();
        let args = serde_json::json!({"token":"ref-2"});
        let token = issue(&lua, binding.clone(), args.clone(), "allow");

        let mismatch: bool = lua
            .load(
                r#"
                local tok, b = ...
                local ok = mare.approvals.consume_human(tok, b, { token = "wrong" }, nil, "allow")
                return ok
                "#,
            )
            .call((token.clone(), mare_shared::lua::json_to_lua(&lua, &json).unwrap()))
            .unwrap();
        assert!(!mismatch, "mismatched arguments must not consume human evidence");
        assert!(check(&lua, Some(token.clone()), &binding, Some(&args), Some("allow")));

        let (consumed, replay): (bool, bool) = lua
            .load(
                r#"
                local tok, b, a = ...
                local first = mare.approvals.consume_human(tok, b, a, nil, "allow")
                local second = mare.approvals.consume_human(tok, b, a, nil, "allow")
                return first, second
                "#,
            )
            .set_name("@user/custom_pipeline.lua")
            .call((
                token,
                mare_shared::lua::json_to_lua(&lua, &json).unwrap(),
                mare_shared::lua::json_to_lua(&lua, &args).unwrap(),
            ))
            .unwrap();
        assert!(consumed, "matching evidence must consume once");
        assert!(!replay, "replaying consumed evidence must fail");

        let action = mare_shared::lua::json_to_lua(
            &lua,
            &serde_json::json!({
                "call": {
                    "name": "shell",
                    "arguments": { "command": "rm -rf /" },
                    "origin": { "agent_id": "child-1", "label": "worker" }
                },
                "info": { "unconfined": false },
                "verdict": {
                    "amendment": {
                        "argv": ["rm", "-rf"],
                        "decision": "allow",
                        "description": "harmless read-only ls"
                    }
                }
            }),
        )
        .unwrap();
        let snap = Snapshot::from_action_lua(
            &lua,
            match action {
                mlua::Value::Table(t) => t,
                _ => unreachable!(),
            },
        )
        .unwrap();
        let desc = snap.amendment_description.unwrap();
        assert!(!desc.contains("harmless"), "child description must be ignored: {desc}");
        assert!(desc.contains("rm -rf"), "parent rule fields must be described: {desc}");
    }

    #[test]
    fn describe_rule_formats_decisions_expiry_and_fallbacks() {
        let prompt = describe_rule(&serde_json::json!({
            "tool": "write",
            "decision": "prompt",
            "expires": "2026-12-31"
        }));
        assert!(prompt.contains("always ask before `write` calls"), "{prompt}");
        assert!(prompt.contains("expires 2026-12-31"), "{prompt}");

        let forbidden = describe_rule(&serde_json::json!({
            "argv": ["rm", 42],
            "decision": "forbidden",
            "deny_args": ["-f"]
        }));
        assert!(forbidden.contains("always refuse commands that start with `rm 42`"), "{forbidden}");

        let custom = describe_rule(&serde_json::json!({
            "tool": "bash",
            "decision": "audit"
        }));
        assert!(custom.contains("audit `bash` calls"), "{custom}");

        let fallback = describe_rule(&serde_json::json!({ "decision": "allow" }));
        assert!(fallback.starts_with("this rule: "), "{fallback}");
    }

    #[test]
    fn public_result_emits_call_id_and_omits_bare_id() {
        let out = public_result(&serde_json::json!({
            "call": { "tool_call_id": "call-7", "name": "bash", "arguments": { "command": "secret" } },
            "decision": "allow",
            "source": "user",
        }));
        assert_eq!(out["call"], serde_json::json!({ "tool_call_id": "call-7", "name": "bash" }));
        assert!(out["call"].get("id").is_none(), "{out}");
    }
}
