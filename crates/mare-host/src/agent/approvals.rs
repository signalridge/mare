use std::path::PathBuf;

use mlua::{Function, Lua, MultiValue, Table, Value};
use tokio::sync::{mpsc, oneshot};

use super::{ChildApproval, ChildEvent};
use crate::Host;
use mare_shared::lua::json_to_lua;

/// What the parent shows as the source of a child's approval requests.
pub(super) fn origin_of(child_id: u64, subagent_id: &str, spec: &serde_json::Value) -> serde_json::Value {
    let s = |k: &str| spec.get(k).and_then(|v| v.as_str()).map(str::to_string);
    // the task: an explicit `task` / `description`, else the prompt's first line (shortened)
    let task = s("task").or_else(|| s("description")).or_else(|| {
        let line = s("prompt")?.lines().find(|l| !l.trim().is_empty())?.trim().to_string();
        Some(if line.chars().count() > 80 { format!("{}…", line.chars().take(79).collect::<String>()) } else { line })
    });
    let mut o = serde_json::json!({ "agent_id": format!("agent-{child_id}"), "subagent_id": subagent_id });
    for (k, v) in
        [("label", s("label").or_else(|| s("name"))), ("task", task), ("run_id", s("run_id")), ("phase", s("phase"))]
    {
        if let Some(v) = v {
            o[k] = v.into();
        }
    }
    o
}

/// The child's `tool/approval` listener: every request goes to the parent and waits for its
/// answer. Parent gone / channel closed → `unavailable`.
pub(super) fn bridge_approvals(
    host: &Host,
    tx: mpsc::UnboundedSender<ChildEvent>,
    origin: serde_json::Value,
    shares_rules: bool,
    inherits_parent_preset: bool,
) -> mlua::Result<()> {
    let lua = host.lua();
    let host_review_required = host
        .service_table(mare_shared::APPROVALS_SERVICE_NAME)
        .map(|approvals| {
            approvals.get::<Function>("review_required").and_then(|f| f.call::<bool>(approvals)).unwrap_or(true)
        })
        .unwrap_or(false);
    let permission_grants = crate::approvals::permission_grants::install(lua, &host.kernel)?;
    if shares_rules {
        crate::approvals::shared_rule::install(lua, tx.clone())?;
    }
    let f = lua.create_async_function(move |lua, args: MultiValue| {
        let tx = tx.clone();
        let origin = origin.clone();
        let permission_grants = permission_grants.clone();
        async move {
            let deny_reason = |reason: String| async {
                let t = lua.create_table()?;
                t.set("decision", "deny")?;
                t.set("reason", reason)?;
                Ok::<Value, mlua::Error>(Value::Table(t))
            };
            let event_name = "tool/approval";
            let args = mare_kernel::strip_event_envelope(args);
            let projected = match mare_kernel::project_event_args(&lua, event_name, args) {
                Ok(projected) => projected,
                Err(e) => {
                    return deny_reason(format!("approval argument conversion failed: {e}")).await;
                }
            };
            let cancel = projected.cancel_token;
            let mut a = projected.json_args;
            a.resize(3, serde_json::Value::Null);
            let verdict = a.pop().unwrap_or_default();
            let info = a.pop().unwrap_or_default();
            let call = a.pop().unwrap_or_default();
            let forwarded_review_required = verdict.get("review_required").and_then(|v| v.as_bool()).unwrap_or(false);
            let (rtx, rrx) = oneshot::channel();
            let req = ChildApproval {
                origin,
                verdict,
                info,
                call,
                shares_rules,
                inherits_parent_preset,
                review_required: host_review_required || forwarded_review_required,
                reply: rtx,
            };
            let gone = || crate::approvals::child_events::ChildApprovalReply {
                vote: serde_json::json!({ "decision": "deny", "unavailable": true,
                    "reason": "approval required but the parent session can't answer" }),
                human_approved: false,
            };
            let (call, info, verdict) = (req.call.clone(), req.info.clone(), req.verdict.clone());
            let reply = if tx.send(ChildEvent::Approval(Box::new(req))).is_ok() {
                if let Some(cancel) = cancel {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => gone(),
                        reply = rrx => if cancel.is_cancelled() { gone() } else { reply.unwrap_or_else(|_| gone()) },
                    }
                } else {
                    rrx.await.unwrap_or_else(|_| gone())
                }
            } else {
                gone()
            };
            let mut vote = reply.vote;
            let human_approved = reply.human_approved;
            crate::approvals::permission_grants::record(&lua, &permission_grants, &call, &info, &vote)?;
            // resolved by a parent rule (no dialog): lifted like a fresh call the rule allows
            let rule_lift = vote.as_object_mut().and_then(|o| o.remove("rule_lift"));
            if vote.get("decision").and_then(|d| d.as_str()) == Some("allow") {
                match rule_lift {
                    Some(spec) => {
                        crate::approvals::shared_rule::lift(&lua, &call, &spec)?;
                    }
                    None => lift_approved(&lua, &call, &verdict, vote.get("grant").and_then(|g| g.as_str()))?,
                }
            }
            // The parent consumed its own human evidence before this native reply. Rebind a
            // fresh one-time receipt to the child's request owner/turn/nonce/snapshot only when
            // the parent actually consumed a native human receipt over the trusted channel.
            if human_approved
                && vote["source"] == "user"
                && vote.get("decision").and_then(|v| v.as_str()) == Some("allow")
                && let Some(binding) = call.get("approval_binding").and_then(mare_binding::approval::Binding::from_json)
            {
                vote["human_approval"] = mare_binding::approval::issue(
                    &lua,
                    binding,
                    call.get("arguments").cloned().unwrap_or(serde_json::Value::Null),
                    vote.get("grant").or_else(|| vote.get("decision")).and_then(|v| v.as_str()).unwrap_or("deny"),
                )
                .into();
            }
            json_to_lua(&lua, &vote)
        }
    })?;
    // a voter of the host's fiber that waits for a person in the parent: no answer time
    let (ctx, owner) = (host.kernel.host_ctx()?, host.kernel.host_owner()?);
    let opts = mare_kernel::HostVoter { timeout: Some(std::time::Duration::ZERO), ..Default::default() };
    host.kernel.host_voter(&ctx, owner, "tool/approval-host", f, opts)?;
    Ok(())
}

/// a child call the parent's approval path approved runs the way the main
/// agent's approved call does — with the per-call policy it was approved under (an escalation
/// outside the sandbox included), not clipped to the inherited ceiling.
/// "Allow once" covers this call; "this session" / "remember"
/// cover every identical call of this child. Recorded host-side: nothing in the child VM can
/// grant itself a lift, and a vote with no per-call policy keeps the ceiling. A ceiling that
/// restricts reads keeps them for the lifted call too: it stays sandboxed.
fn lift_approved(
    lua: &Lua,
    call: &serde_json::Value,
    verdict: &serde_json::Value,
    grant: Option<&str>,
) -> mlua::Result<()> {
    let (Some(key), Some(spec)) = (
        call.get("approval_key").and_then(|k| k.as_str()),
        verdict.get("sandbox_grant").filter(|s| !s.is_null() && **s != serde_json::Value::Bool(false)),
    ) else {
        // no per-call policy (`nil` / `false` = the current one): the ceiling applies as usual
        return Ok(());
    };
    // resolved against the root the tool's execution context uses for the same spec
    let cwd = lua
        .globals()
        .get::<Table>("mare")
        .and_then(|m| m.get::<String>("cwd"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    let policy = mare_binding::ossandbox::Policy::from_lua(lua, &json_to_lua(lua, spec)?, &cwd)?;
    let id = match grant {
        Some("allow_session" | "allow_amend") => None,
        _ => call.get("id").and_then(|i| i.as_str()),
    };
    mare_binding::ossandbox::lift_ceiling(lua, id, key, policy);
    Ok(())
}
