//! ACP approval forwarding (`tool/approval`, `net/approval` -> `session/request_permission`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    Diff, PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest, ToolCallContent,
    ToolCallLocation, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use mlua::{MultiValue, Value};
use serde_json::{Map, Value as J};
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
        options.push(PermissionOption::new(
            "allow-always",
            "Allow for this session",
            PermissionOptionKind::AllowAlways,
        ));
    }
    options.push(PermissionOption::new("reject-once", "Reject", PermissionOptionKind::RejectOnce));
    (options, allow_always)
}

fn non_empty_str<'a>(obj: &'a J, key: &str) -> Option<&'a str> {
    obj.get(key).and_then(J::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn first_line_preview(text: &str, max_chars: usize) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    if first.chars().count() <= max_chars {
        first.to_string()
    } else {
        let mut s: String = first.chars().take(max_chars.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

fn resolve_abs_path(cwd: &str, raw: &str) -> PathBuf {
    mare_binding::fs::resolve_path(Path::new(cwd), raw)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WriteTargetState {
    NewFile,
    ExistingText(String),
    ExistingUnreadable,
}

/// Inspect an existing `write` target through `mare.fs` with the active sandbox policy so
/// `deny_read` rules are honored. Returns `NewFile` only when the path does not exist,
/// `ExistingText` when the old file is readable text within `MAX_DIFF_BYTES`, and
/// `ExistingUnreadable` otherwise (so we never misreport an overwrite as a new file).
fn inspect_write_target(lua: &mlua::Lua, abs: &Path) -> WriteTargetState {
    const MAX_DIFF_BYTES: usize = 512 * 1024;
    let path_str = abs.to_string_lossy();
    let script = r#"
        local path, max_bytes = ...
        local policy = mare and mare.proc and mare.proc.sandbox and mare.proc.sandbox.current and mare.proc.sandbox.current()
        local policies = policy and { policy } or nil
        local st = mare.fs.stat(path, { policies = policies })
        if not st then
            if mare.fs.exists(path) then
                return "unreadable", nil
            end
            return "new", nil
        end
        if st.kind ~= "file" or (st.size and st.size > max_bytes) then
            return "unreadable", nil
        end
        local ok, text = pcall(mare.fs.read, path, { policies = policies, max_bytes = max_bytes })
        if ok and type(text) == "string" then
            return "existing", text
        end
        return "unreadable", nil
    "#;
    match lua.load(script).call::<(String, Option<String>)>((path_str.as_ref(), MAX_DIFF_BYTES)) {
        Ok((tag, Some(old))) if tag == "existing" => WriteTargetState::ExistingText(old),
        Ok((tag, _)) if tag == "new" => WriteTargetState::NewFile,
        _ => WriteTargetState::ExistingUnreadable,
    }
}

fn build_tool_call_update(
    kind: &str,
    a: &[J],
    host_cwd: &Path,
    write_state: Option<WriteTargetState>,
) -> ToolCallUpdate {
    if kind == "tool" {
        let call = &a[0];
        let info = a.get(1).unwrap_or(&J::Null);
        let verdict = a.get(2).unwrap_or(&J::Null);
        let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
        let args = call.get("arguments").cloned().unwrap_or(J::Null);
        let cwd =
            non_empty_str(call, "cwd").map(str::to_owned).unwrap_or_else(|| host_cwd.to_string_lossy().into_owned());
        let cmd = non_empty_str(&args, "command");
        let path = non_empty_str(&args, "path").or_else(|| non_empty_str(&args, "file_path"));
        let url = non_empty_str(&args, "url");
        let title_arg = if let Some(c) = cmd {
            Some(first_line_preview(c, 80))
        } else if let Some(p) = path {
            Some(p.to_string())
        } else if let Some(u) = url {
            Some(u.to_string())
        } else if args.as_object().is_some_and(|m| !m.is_empty()) {
            serde_json::to_string(&args).ok().map(|s| first_line_preview(&s, 80))
        } else {
            None
        };
        let title = match title_arg {
            Some(arg) if !arg.is_empty() => format!("{name} {arg}"),
            _ => name.to_string(),
        };

        let mut content: Vec<ToolCallContent> = Vec::new();
        let mut locations: Vec<ToolCallLocation> = Vec::new();
        if let Some(c) = cmd {
            content.push(ToolCallContent::from(c.to_string()));
        }
        if let Some(p) = path {
            let abs = resolve_abs_path(&cwd, p);
            locations.push(ToolCallLocation::new(abs.clone()));
            if let (Some(old_s), Some(new_s)) =
                (args.get("old_string").and_then(J::as_str), args.get("new_string").and_then(J::as_str))
            {
                content.push(ToolCallContent::from(Diff::new(abs, new_s).old_text(old_s)));
            } else if let Some(new_s) = args.get("content").and_then(J::as_str) {
                match write_state.unwrap_or(WriteTargetState::ExistingUnreadable) {
                    WriteTargetState::NewFile => {
                        content.push(ToolCallContent::from(Diff::new(abs, new_s)));
                    }
                    WriteTargetState::ExistingText(old_s) => {
                        content.push(ToolCallContent::from(Diff::new(abs, new_s).old_text(old_s)));
                    }
                    WriteTargetState::ExistingUnreadable => {
                        content.push(ToolCallContent::from(format!(
                            "Overwrite {} ({} bytes):\n{new_s}",
                            abs.display(),
                            new_s.len()
                        )));
                    }
                }
            }
        }
        let diff_text = non_empty_str(&args, "patch")
            .or_else(|| non_empty_str(&args, "diff"))
            .or_else(|| (name == "apply_patch").then(|| non_empty_str(&args, "input")).flatten());
        if let Some(diff) = diff_text {
            content.push(ToolCallContent::from(diff.to_string()));
        }
        if !cwd.is_empty() {
            content.push(ToolCallContent::from(format!("Working directory: {cwd}")));
        }
        let reason = non_empty_str(verdict, "reason");
        if let Some(r) = reason {
            content.push(ToolCallContent::from(format!("Reason: {r}")));
        }
        let unconfined = info.get("unconfined").and_then(J::as_bool).unwrap_or(false);
        if unconfined {
            content.push(ToolCallContent::from("Runs outside the local sandbox"));
        }

        let mut meta = Map::new();
        meta.insert("cwd".into(), J::String(cwd));
        meta.insert("unconfined".into(), J::Bool(unconfined));
        if let Some(r) = reason {
            meta.insert("reason".into(), J::String(r.to_string()));
        }
        if let Some(se) = non_empty_str(info, "side_effect") {
            meta.insert("side_effect".into(), J::String(se.to_string()));
        }

        let mut fields = ToolCallUpdateFields::new()
            .title(title)
            .kind(acp_tool_kind(info))
            .status(ToolCallStatus::Pending)
            .raw_input(args);
        if !content.is_empty() {
            fields = fields.content(content);
        }
        if !locations.is_empty() {
            fields = fields.locations(locations);
        }
        ToolCallUpdate::new(call_id.to_string(), fields).meta(meta)
    } else {
        let req = &a[0];
        let host = match (req.get("host").and_then(|v| v.as_str()), req.get("port").and_then(|p| p.as_u64())) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => "unknown".to_string(),
        };
        let mut fields = ToolCallUpdateFields::new()
            .title(format!("network {host}"))
            .kind(ToolKind::Fetch)
            .status(ToolCallStatus::Pending)
            .raw_input(req.clone());
        if let Some(reason) = non_empty_str(req, "reason") {
            fields = fields.content(vec![ToolCallContent::from(format!("Reason: {reason}"))]);
        }
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
        let host_cwd = host.cwd.clone();
        let session_cancel = session_cancel.clone();
        let projector = projector.clone();
        let f = lua.create_async_function(move |lua, args: MultiValue| {
            let conn = conn.clone();
            let sid = sid.clone();
            let host_cwd = host_cwd.clone();
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
                let write_state = if kind == "tool"
                    && let Some(args) = a[0].get("arguments")
                    && args.get("content").and_then(J::as_str).is_some()
                    && (args.get("old_string").is_none() || args.get("new_string").is_none())
                    && let Some(p) = non_empty_str(args, "path").or_else(|| non_empty_str(args, "file_path"))
                {
                    let cwd = non_empty_str(&a[0], "cwd")
                        .map(str::to_owned)
                        .unwrap_or_else(|| host_cwd.to_string_lossy().into_owned());
                    let abs = resolve_abs_path(&cwd, p);
                    Some(inspect_write_target(&lua, &abs))
                } else {
                    None
                };
                let tool_call = build_tool_call_update(kind, &a, &host_cwd, write_state);
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

    #[test]
    fn acp_permission_options_label_session_scope_and_tool_call_includes_details() {
        let long_cmd = format!("echo start && {} && echo TAIL_END", "x".repeat(200));
        let args = [
            json!({
                "id": "call-1",
                "name": "bash",
                "cwd": "/repo/work",
                "arguments": { "command": long_cmd }
            }),
            json!({ "command_tool": true, "side_effect": "external", "unconfined": true }),
            json!({ "reason": "runs outside sandbox" }),
        ];
        let (opts, offered) = permission_options("tool", &args);
        assert!(offered);
        let always = opts.iter().find(|o| o.option_id.0.as_ref() == "allow-always").expect("allow-always");
        assert_eq!(always.name, "Allow for this session");
        assert_eq!(always.kind, PermissionOptionKind::AllowAlways);

        let update = build_tool_call_update("tool", &args, Path::new("/fallback"), None);
        let serialized = serde_json::to_value(&update).unwrap();
        let content_str = serialized["content"].to_string();
        assert!(content_str.contains("TAIL_END"), "full command must appear in ACP content: {serialized}");
        assert!(content_str.contains("Working directory: /repo/work"), "{serialized}");
        assert!(content_str.contains("Reason: runs outside sandbox"), "{serialized}");
        assert!(content_str.contains("Runs outside the local sandbox"), "{serialized}");
        assert_eq!(serialized["_meta"]["unconfined"], json!(true));
        assert_eq!(serialized["_meta"]["reason"], json!("runs outside sandbox"));
        assert!(serialized["_meta"].get("diff").is_none(), "_meta.diff must not duplicate patch content: {serialized}");

        let edit_args = [
            json!({
                "id": "call-2",
                "name": "edit",
                "cwd": "/repo",
                "arguments": { "path": "src/lib.rs", "old_string": "old", "new_string": "new" }
            }),
            json!({ "side_effect": "write" }),
            json!({ "reason": "edit file" }),
        ];
        let edit_json =
            serde_json::to_value(build_tool_call_update("tool", &edit_args, Path::new("/repo"), None)).unwrap();
        assert!(
            edit_json["content"].as_array().unwrap().iter().any(|c| c["type"] == "diff" && c["newText"] == "new"),
            "edit approval must include ACP diff content: {edit_json}"
        );

        // `write` onto existing readable file includes `oldText`; unreadable existing file falls back
        // to text content instead of misreporting a new-file Diff; `~` expands instead of joining cwd.
        let write_args = [
            json!({
                "id": "call-3",
                "name": "write",
                "cwd": "/repo",
                "arguments": { "path": "~/notes.txt", "content": "updated" }
            }),
            json!({ "side_effect": "write" }),
            json!({ "reason": "write file" }),
        ];
        let overwrite_json = serde_json::to_value(build_tool_call_update(
            "tool",
            &write_args,
            Path::new("/repo"),
            Some(WriteTargetState::ExistingText("previous".into())),
        ))
        .unwrap();
        let loc_path = overwrite_json["locations"][0]["path"].as_str().unwrap_or("");
        assert!(!loc_path.contains("/~/"), "tilde path must expand instead of joining cwd: {overwrite_json}");
        assert!(
            overwrite_json["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["type"] == "diff" && c["oldText"] == "previous" && c["newText"] == "updated"),
            "overwrite diff must include oldText: {overwrite_json}"
        );

        let unreadable_json = serde_json::to_value(build_tool_call_update(
            "tool",
            &write_args,
            Path::new("/repo"),
            Some(WriteTargetState::ExistingUnreadable),
        ))
        .unwrap();
        assert!(
            unreadable_json["content"].as_array().unwrap().iter().all(|c| c["type"] != "diff"),
            "unreadable overwrite must not emit a misleading new-file Diff: {unreadable_json}"
        );
        assert!(
            unreadable_json["content"].to_string().contains("Overwrite"),
            "unreadable overwrite must emit text content: {unreadable_json}"
        );
    }
}
