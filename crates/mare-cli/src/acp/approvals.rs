//! ACP approval forwarding (`tool/approval`, `net/approval` -> `session/request_permission`).

use std::sync::Arc;

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, ToolCallStatus,
    ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use mlua::{MultiValue, Value};
use serde_json::Value as J;
use tokio_util::sync::CancellationToken;

use mare_host::Host;

fn acp_tool_kind(info: &J) -> ToolKind {
    match super::helpers::acp_tool_kind_str(info) {
        "execute" => ToolKind::Execute,
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        _ => ToolKind::Other,
    }
}

fn permission_options(kind: &str, a: &[J]) -> (Vec<PermissionOption>, bool) {
    let allow_always = if kind == "tool" {
        let (call, info, verdict) = (&a[0], &a[1], &a[2]);
        let protected = call.get("protected_arguments").and_then(|v| v.as_bool()).unwrap_or(false);
        let scoped = info.get("permission_request").and_then(|v| v.as_bool()).unwrap_or(false)
            || verdict.get("permission_scope").is_some_and(|v| !v.is_null());
        let scope_is_session = verdict.get("permission_scope").and_then(|v| v.as_str()) == Some("session");
        !protected && (!scoped || scope_is_session)
    } else {
        true
    };
    let mut options = vec![PermissionOption::new("allow-once", "Allow once", PermissionOptionKind::AllowOnce)];
    if allow_always {
        options.push(PermissionOption::new("allow-always", "Always allow", PermissionOptionKind::AllowAlways));
    }
    options.push(PermissionOption::new("reject-once", "Reject", PermissionOptionKind::RejectOnce));
    (options, allow_always)
}

fn build_tool_call_update(kind: &str, a: &[J]) -> ToolCallUpdate {
    if kind == "tool" {
        let call = &a[0];
        let info = a.get(1).unwrap_or(&J::Null);
        let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
        let args = call.get("arguments").cloned().unwrap_or(J::Null);
        let arg = args
            .as_object()
            .and_then(|o| o.get("path").or_else(|| o.get("command")))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title = if arg.is_empty() { name.to_string() } else { format!("{name} {arg}") };
        let fields = ToolCallUpdateFields::new()
            .title(title)
            .kind(acp_tool_kind(info))
            .status(ToolCallStatus::Pending)
            .raw_input(args);
        ToolCallUpdate::new(call_id.to_string(), fields)
    } else {
        let req = &a[0];
        let host = match (req.get("host").and_then(|v| v.as_str()), req.get("port").and_then(|p| p.as_u64())) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => "unknown".to_string(),
        };
        let fields = ToolCallUpdateFields::new()
            .title(format!("network {host}"))
            .kind(ToolKind::Fetch)
            .status(ToolCallStatus::Pending)
            .raw_input(req.clone());
        ToolCallUpdate::new(format!("net:{host}"), fields)
    }
}

/// Install `tool/approval-host` and `net/approval-host` host-answer listeners on `host.kernel` that
/// forward approval requests to the ACP client via `session/request_permission`.
/// Fail-closed: disconnect, error, or cancellation denies the request.
pub(super) fn install(
    host: &Arc<Host>,
    session_id: &str,
    conn: &ConnectionTo<Client>,
    session_cancel: CancellationToken,
) -> anyhow::Result<()> {
    let lua = host.lua();
    let projector = mare_shared::wire_projection::hook(lua);
    // voters of the host's fiber
    let (ctx, owner) = (host.kernel.host_ctx()?, host.kernel.host_owner()?);
    for (event, kind) in [("tool/approval-host", "tool"), ("net/approval-host", "network")] {
        let conn = conn.clone();
        let sid = session_id.to_string();
        let session_cancel = session_cancel.clone();
        let projector = projector.clone();
        let f = lua.create_async_function(move |lua, args: MultiValue| {
            let conn = conn.clone();
            let sid = sid.clone();
            let session_cancel = session_cancel.clone();
            let projector = projector.clone();
            async move {
                let vote = |decision: &str,
                            grant: Option<&str>,
                            abort: bool,
                            reason: Option<&str>,
                            human_approval: Option<String>| {
                    let t = lua.create_table()?;
                    t.set("decision", decision)?;
                    if let Some(g) = grant {
                        t.set("grant", g)?;
                    }
                    if abort {
                        t.set("abort", true)?;
                    }
                    if let Some(r) = reason {
                        t.set("reason", r)?;
                    }
                    if let Some(token) = human_approval {
                        t.set("human_approval", token)?;
                    }
                    Ok(Value::Table(t))
                };
                let unavailable = |reason: &str| -> mlua::Result<Value> {
                    let t = lua.create_table()?;
                    t.set("decision", "deny")?;
                    t.set("unavailable", true)?;
                    t.set("reason", reason)?;
                    Ok(Value::Table(t))
                };
                let event_name = format!("{kind}/approval");
                let args = mare_kernel::strip_event_envelope(args);
                let projected = match mare_kernel::project_event_args(&lua, &event_name, args) {
                    Ok(projected) => projected,
                    Err(e) => {
                        return vote(
                            "deny",
                            None,
                            false,
                            Some(&format!("approval argument conversion failed: {e}")),
                            None,
                        );
                    }
                };
                let cancel = projected.cancel_token;
                let mut a = projected.json_args;
                a.resize(3, J::Null);
                let tool_call = build_tool_call_update(kind, &a);
                let (options, allow_always_offered) = permission_options(kind, &a);
                let req = RequestPermissionRequest::new(sid, tool_call, options);
                let fut = super::wire::request_permission(&conn, req, projector.as_slice());
                let res = if let Some(cancel) = cancel {
                    tokio::select! {
                        biased;
                        _ = session_cancel.cancelled() => return unavailable("ACP session closed"),
                        _ = cancel.cancelled() => return unavailable("approval request cancelled"),
                        r = fut => r,
                    }
                } else {
                    tokio::select! {
                        biased;
                        _ = session_cancel.cancelled() => return unavailable("ACP session closed"),
                        r = fut => r,
                    }
                };
                match res {
                    Ok(resp) => match resp.outcome {
                        RequestPermissionOutcome::Selected(sel) => {
                            let (decision, grant) = match sel.option_id.0.as_ref() {
                                "allow-once" => ("allow", Some("allow")),
                                "allow-always" if allow_always_offered => ("allow_session", Some("allow_session")),
                                "allow-always" => {
                                    return vote(
                                        "deny",
                                        None,
                                        false,
                                        Some("option not allowed for this request"),
                                        None,
                                    );
                                }
                                _ => return vote("deny", None, false, Some("rejected by ACP client"), None),
                            };
                            let human_approval = a[0]
                                .get("approval_binding")
                                .and_then(mare_binding::approval::Binding::from_json)
                                .map(|binding| {
                                    let arguments = if kind == "tool" {
                                        a[0].get("arguments").cloned().unwrap_or(J::Null)
                                    } else {
                                        J::Null
                                    };
                                    mare_binding::approval::issue(&lua, binding, arguments, decision)
                                });
                            vote("allow", grant, false, None, human_approval)
                        }
                        RequestPermissionOutcome::Cancelled => {
                            vote("deny", None, true, Some("cancelled by ACP client"), None)
                        }
                        _ => vote("deny", None, false, Some("unknown permission outcome"), None),
                    },
                    Err(e) => unavailable(&format!("ACP client permission request failed: {e}")),
                }
            }
        })?;
        // it waits for a person: no answer time
        let opts = mare_kernel::HostVoter { timeout: Some(std::time::Duration::ZERO), ..Default::default() };
        host.kernel.host_voter(&ctx, owner, event, f, opts)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn acp_tool_kind_derives_from_command_tool_and_side_effect() {
        assert_eq!(acp_tool_kind(&json!({ "command_tool": true, "side_effect": "external" })), ToolKind::Execute);
        assert_eq!(acp_tool_kind(&json!({ "command_tool": true, "side_effect": "read" })), ToolKind::Execute);
        assert_eq!(acp_tool_kind(&json!({ "side_effect": "read" })), ToolKind::Read);
        assert_eq!(acp_tool_kind(&json!({ "side_effect": "write" })), ToolKind::Edit);
        assert_eq!(acp_tool_kind(&json!({ "side_effect": "external" })), ToolKind::Other);
        assert_eq!(acp_tool_kind(&json!({})), ToolKind::Other);
    }
}
