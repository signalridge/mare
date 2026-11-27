//! A sub-agent's proposed "always allow" rule, as the parent's approval dialog shows it and as
//! the parent persists it.
//!
//! The child proposes the rule and the parent keeps exactly what was approved: the
//! delegate forwards its proposed prefix rule to the parent's prompt
//! ("don't ask again for commands that start with …"),
//! and the approval persists that same rule. The parent neither rebuilds nor validates it; instead the
//! object is snapshotted at display time (the child's free-text `description` removed), the
//! dialog (the agent-approvals plugin) renders its text from a copy of the snapshot's fields,
//! and the snapshot itself — never anything the dialog answers — is handed to the persisting
//! side under a one-time token. A rule arriving any other way with a sub-agent's call
//! (a later, different amendment) has no token and is not saved.

use std::sync::{Arc, Mutex, Weak};

use mlua::{Function, Lua, Table, Value};
use serde_json::Value as J;

/// The rule as proposed, minus the child's display text: the object shown and persisted.
pub(crate) fn snapshot(verdict: &J) -> Option<J> {
    let mut rule = verdict.get("amendment").filter(|a| a.is_object())?.clone();
    if let Some(o) = rule.as_object_mut() {
        o.remove("description");
    }
    Some(rule)
}

/// Is `other` (an answer's amendment) the proposed rule, display text aside?
pub(crate) fn is(proposed: &J, other: &J) -> bool {
    snapshot(&serde_json::json!({ "amendment": other })).as_ref() == Some(proposed)
}

/// Rules shown in an approved dialog, waiting for the persisting side (oldest first, bounded).
struct ApprovedRule {
    rule: J,
    call: J,
    info: J,
    kernel: Weak<mare_kernel::KernelInner>,
    stamp: (u64, u64, u64),
}

fn policy_stamp(kernel: &mare_kernel::Kernel) -> Option<(u64, u64, u64)> {
    let service = super::shared_rule::permissions_service(kernel)?;
    let revision: Function = service.get("policy_revision").ok()?;
    let revision: u64 = revision.call(service).ok()?;
    Some((kernel.policy_epoch(), kernel.reload_generation(), revision))
}

#[derive(Default)]
struct Shown(Mutex<(u64, mare_shared::once::OneTimeRegistry<u64, (), ApprovedRule>)>);

/// Hand the approved, displayed `rule` to the persisting side: returns its one-time token.
pub(crate) fn approve(host: &crate::Host, rule: &J, call: &J, info: &J) -> Option<u64> {
    let stamp = policy_stamp(&host.kernel)?;
    let shown = host.lua().app_data_ref::<Shown>()?;
    let mut g = shown.0.lock().unwrap_or_else(|e| e.into_inner());
    g.0 += 1;
    let token = g.0;
    g.1.insert(
        token,
        (),
        ApprovedRule {
            rule: rule.clone(),
            call: call.clone(),
            info: info.clone(),
            kernel: Arc::downgrade(&host.kernel.0),
            stamp,
        },
    );
    Some(token)
}

/// Revoke a previously issued one-time rule token (e.g. if the sub-agent cancelled before commit).
pub(crate) fn revoke(lua: &Lua, token: u64) {
    let Some(shown) = lua.app_data_ref::<Shown>() else { return };
    let mut g = shown.0.lock().unwrap_or_else(|e| e.into_inner());
    let _ = g.1.take(&token, &());
}

/// `mare.subagent.take_rule(token) -> rule | nil`: the rule the parent's dialog showed and the user
/// approved, once (the permissions plugin persists it).
pub(crate) fn install(lua: &Lua, agent: &Table) -> mlua::Result<()> {
    lua.set_app_data(Shown::default());
    agent.set(
        "take_rule",
        lua.create_function(|lua, token: Option<u64>| {
            let Some(token) = token else { return Ok(Value::Nil) };
            let Some(shown) = lua.app_data_ref::<Shown>() else { return Ok(Value::Nil) };
            let mut g = shown.0.lock().unwrap_or_else(|e| e.into_inner());
            let rule = g.1.take(&token, &());
            drop(g);
            match rule {
                Some(rule)
                    if rule
                        .kernel
                        .upgrade()
                        .map(mare_kernel::Kernel)
                        .is_some_and(|kernel| policy_stamp(&kernel) == Some(rule.stamp))
                        && super::shared_rule::validate_access_lua(lua, &rule.call, &rule.info).is_ok() =>
                {
                    mare_shared::lua::json_to_lua(lua, &rule.rule)
                }
                _ => Ok(Value::Nil),
            }
        })?,
    )
}
