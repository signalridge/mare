//! A shared "always allow" rule escalates every sub-agent whose configuration layers match
//! its parent's: a prefix `Allow` match skips approval and runs with `bypass_sandbox`, except where
//! the policy restricts reads — then it stays sandboxed.
//!
//! In mare the child shares its parent's rules when it runs with the parent's permission
//! configuration: no spawn-time `config.plugins.permissions`. The parent's own `permissions`
//! plugin then judges the child's call by its configured and remembered rules
//! (`permissions.rule_verdict`, its session-only rules excluded); a rule allow lifts the child's
//! inherited ceiling for that one call to the sandbox the rule grants (the rule's scope, as for
//! the main agent), host-side, so nothing in the child VM can grant itself one. A child with
//! spawn-time permission rules stays capped. Under a ceiling that restricts reads the lift still
//! widens what the rule grants (network, writable roots) but stays sandboxed with those reads
//! denied (`mare_binding::deny_read`, applied to every call's effective policy). Deny rules keep
//! blocking: the child's own vote and the parent's judgement both refuse a forbidden command.
//!
//! The same judgement resolves a child's queued approval request without a dialog once a rule
//! (saved by an earlier answer, for instance) covers it: evaluated like a fresh call, so a deny
//! rule refuses it and a read restriction keeps it sandboxed (no blanket "same action, approve
//! the rest" shortcut).

use std::path::{Path, PathBuf};

use mlua::{Function, Lua, Table, Value};
use serde_json::Value as J;
use tokio::sync::{mpsc, oneshot};

use crate::Host;
use crate::approvals::child_events::ChildEvent;
use mare_shared::lua::{json_to_lua_lossless, lua_to_json};

/// A child's question to its parent: what do your rules alone decide about this call?
pub struct RuleCheck {
    pub call: J,
    pub info: J,
    /// `{decision = "allow", sandbox}` | `{decision = "deny", reason}` | null (no rule decides)
    pub reply: oneshot::Sender<J>,
}

/// Does a child spawned with `spec` share its parent's permission configuration?
pub(crate) fn shares_parent_rules(spec: &J) -> bool {
    spec.get("config")
        .and_then(|c| c.get(mare_shared::PLUGINS_KEY))
        .and_then(|p| p.get(mare_shared::PERMISSIONS_SERVICE_NAME))
        .is_none()
}

pub(crate) fn permissions_service(kernel: &mare_kernel::Kernel) -> Option<Table> {
    crate::ui_wait::service_table_for_kernel(kernel, mare_shared::PERMISSIONS_SERVICE_NAME)
}

/// Parent side: the parent's `permissions` plugin judges the call by its rules.
/// Returns `Ok(None)` when no rule decides (or permissions is not loaded), and `Err` if
/// rule evaluation fails so callers fail closed.
pub(crate) fn validate_access(host: &Host, call: &J, info: &J) -> Result<(), String> {
    validate_access_lua(host.lua(), call, info)
}

/// The same check at the instant a one-time approval rule is consumed.
pub(crate) fn validate_access_lua(lua: &Lua, call: &J, info: &J) -> Result<(), String> {
    let access = call.get("access").and_then(J::as_object).ok_or("missing frozen access")?;
    let state = access.get("state").and_then(J::as_str).ok_or("missing access state")?;
    let info_state = info.get("access_state").and_then(J::as_str).ok_or("missing access state in policy info")?;
    if state != info_state
        || info.get("access_version") != info.get("tool_version")
        || info.get("tool_version").is_none()
        || call.get("snapshot").and_then(J::as_str).is_none_or(str::is_empty)
        || call.get("approval_key").and_then(J::as_str).is_none_or(str::is_empty)
    {
        return Err("frozen access binding changed".into());
    }
    match state {
        "declared" if access.get("entries").is_some_and(J::is_array) && access.get("entries") == info.get("access") => {
            for entry in access["entries"].as_array().ok_or("invalid access list")? {
                let logical = entry.get("logical").and_then(J::as_str).ok_or("missing logical path")?;
                let physical = entry.get("physical").and_then(J::as_str).ok_or("missing physical path")?;
                let identity = entry.get("identity").and_then(J::as_str).ok_or("missing path identity")?;
                let current = mare_binding::fs::resolve_policy_path(lua, Path::new(logical))
                    .map_err(|_| "child access path cannot be resolved")?;
                if current.logical != Path::new(logical)
                    || current.physical != Path::new(physical)
                    || mare_store::file_identity(&current.existing_ancestor).as_deref() != Some(identity)
                {
                    return Err("child access path changed".into());
                }
            }
        }
        "unknown" if access.get("entries").is_none() && info.get("access").is_none_or(J::is_null) => {}
        _ => return Err("frozen access declaration is invalid or changed".into()),
    }
    Ok(())
}

pub(crate) async fn evaluate(host: &Host, call: &J, info: &J) -> Result<Option<J>, String> {
    validate_access(host, call, info)?;
    let Some(svc) = permissions_service(&host.kernel) else {
        return Ok(None);
    };
    let lua = host.lua();
    let f: Function = svc.get("rule_verdict").map_err(|e| format!("missing rule_verdict: {e}"))?;
    let call_lua = json_to_lua_lossless(lua, call).map_err(|e| e.to_string())?;
    let info_lua = json_to_lua_lossless(lua, info).map_err(|e| e.to_string())?;
    let v: Value = f.call_async((svc, call_lua, info_lua)).await.map_err(|e| e.to_string())?;
    if v.is_nil() {
        return Ok(None);
    }
    let v = lua_to_json(lua, v).map_err(|e| e.to_string())?;
    match v.get("decision").and_then(|d| d.as_str()) {
        Some("allow" | "deny") => Ok(Some(v)),
        Some("ask") | None if v.is_null() => Ok(None),
        _ => Err("unexpected permission rule verdict".into()),
    }
}

/// Parent side, a child's approval request: resolved by the parent's rules like a fresh call
/// (allow: no dialog, `rule_lift` carries the sandbox for the child's host; deny: refused), or
/// `Ok(None)` to ask as usual, or `Err` if the parent rule check failed.
pub(crate) async fn resolve_request(host: &Host, shares: bool, call: &J, info: &J) -> Result<Option<J>, String> {
    if !shares {
        return Ok(None);
    }
    let Some(v) = evaluate(host, call, info).await? else {
        return Ok(None);
    };
    Ok(Some(match v["decision"].as_str() {
        Some("allow") => serde_json::json!({ "decision": "allow", "grant": "allow",
            "reason": "allowed by a permission rule", "rule_lift": v.get("sandbox").cloned().unwrap_or(J::Null) }),
        _ => serde_json::json!({ "decision": "deny",
            "reason": v.get("reason").and_then(|r| r.as_str()).unwrap_or("forbidden by a permission rule") }),
    }))
}

/// Child side: lift the inherited ceiling for this one call to the sandbox a parent rule grants
/// (`spec`: a sandbox spec, `"danger-full-access"` = unconfined; `false` / null = none). Under a
/// ceiling that restricts reads the call still runs sandboxed with them:
/// `mare_binding::deny_read::keep`.
pub(crate) fn lift(lua: &Lua, call: &J, spec: &J) -> mlua::Result<bool> {
    let (Some(key), Some(id)) =
        (call.get("approval_key").and_then(|k| k.as_str()), call.get("id").and_then(|i| i.as_str()))
    else {
        return Ok(false);
    };
    if spec.is_null() || *spec == J::Bool(false) {
        return Ok(false);
    }
    let cwd = lua
        .globals()
        .get::<Table>("mare")
        .and_then(|m| m.get::<String>("cwd"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    let policy = mare_binding::ossandbox::Policy::from_lua(lua, &json_to_lua_lossless(lua, spec)?, &cwd)?;
    mare_binding::ossandbox::lift_ceiling(lua, Some(id), key, policy);
    Ok(true)
}

/// Child side: `mare.subagent.rule_lift(call, info) -> bool`, called by the tools service for a
/// call the child's own vote allowed with a per-call sandbox: asks the parent and lifts the
/// ceiling for this call when a parent rule allows it.
pub(crate) fn install(lua: &Lua, tx: mpsc::UnboundedSender<ChildEvent>) -> mlua::Result<()> {
    let f = lua.create_async_function(move |lua, (call, info): (Value, Value)| {
        let tx = tx.clone();
        async move {
            let call = lua_to_json(&lua, call)?;
            let info = lua_to_json(&lua, info)?;
            let (reply, rx) = oneshot::channel();
            let check = RuleCheck { call: call.clone(), info, reply };
            if tx.send(ChildEvent::RuleCheck(Box::new(check))).is_err() {
                return Ok(false);
            }
            match rx.await {
                Ok(v) if v.get("decision").and_then(|d| d.as_str()) == Some("allow") => {
                    lift(&lua, &call, v.get("sandbox").unwrap_or(&J::Null))
                }
                _ => Ok(false),
            }
        }
    })?;
    crate::agent::subagent_table(lua)?.set("rule_lift", f)
}
