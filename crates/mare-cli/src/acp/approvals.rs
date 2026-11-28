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

use mare_binding::ui::{non_empty_str, split_reason_parts, title_first_line};
use mare_host::Host;

fn acp_tool_kind(info: &J) -> ToolKind {
    match super::helpers::acp_tool_kind_str(info) {
        "read" => ToolKind::Read,
        "edit" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" => ToolKind::Move,
        "search" => ToolKind::Search,
        "execute" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        "switch_mode" => ToolKind::SwitchMode,
        _ => ToolKind::Other,
    }
}

fn permission_options(kind: &str, a: &[J]) -> (Vec<PermissionOption>, bool) {
    let allow_always = if kind == "tool" {
        let (call, info, verdict) = (&a[0], &a[1], &a[2]);
        info.pointer("/approval_detail/scope/session_allowed").and_then(J::as_bool).unwrap_or_else(|| {
            let protected = call.get("protected_arguments").and_then(|v| v.as_bool()).unwrap_or(false);
            let scoped = info.get("permission_request").and_then(|v| v.as_bool()).unwrap_or(false)
                || verdict.get("permission_scope").is_some_and(|v| !v.is_null());
            let scope_is_session = verdict.get("permission_scope").and_then(|v| v.as_str()) == Some("session");
            !protected && (!scoped || scope_is_session)
        })
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

fn build_tool_call_update(kind: &str, a: &[J], host_cwd: &Path) -> ToolCallUpdate {
    if kind == "tool" {
        let call = &a[0];
        let info = a.get(1).unwrap_or(&J::Null);
        let verdict = a.get(2).unwrap_or(&J::Null);
        let detail = info.get("approval_detail");
        let call_id = call.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let name = call.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
        let args = call.get("arguments").cloned().unwrap_or(J::Null);
        let cwd =
            non_empty_str(call, "cwd").map(str::to_owned).unwrap_or_else(|| host_cwd.to_string_lossy().into_owned());
        let cmd = detail.and_then(|d| non_empty_str(d, "command")).or_else(|| non_empty_str(&args, "command"));
        let path = non_empty_str(&args, "path").or_else(|| non_empty_str(&args, "file_path"));
        let url = non_empty_str(&args, "url");
        let title_arg = if let Some(c) = cmd {
            Some(title_first_line(c, 80))
        } else if let Some(p) = path {
            Some(p.to_string())
        } else if let Some(u) = url {
            Some(u.to_string())
        } else if args.as_object().is_some_and(|m| !m.is_empty()) {
            serde_json::to_string(&args).ok().map(|s| title_first_line(&s, 80))
        } else {
            None
        };
        let title = match title_arg {
            Some(arg) if !arg.is_empty() => format!("{name} {arg}"),
            _ => name.to_string(),
        };

        let mut content: Vec<ToolCallContent> = Vec::new();
        let mut locations: Vec<ToolCallLocation> = Vec::new();
        if let Some(locs) = detail.and_then(|d| d.get("locations")).and_then(J::as_array) {
            for loc in locs {
                if let Some(p) = loc.get("path").and_then(J::as_str).filter(|s| !s.is_empty()) {
                    let mut entry = ToolCallLocation::new(PathBuf::from(p));
                    if let Some(line) = loc.get("line").and_then(J::as_u64).and_then(|n| u32::try_from(n).ok()) {
                        entry = entry.line(line);
                    }
                    locations.push(entry);
                }
            }
        }

        let mut preview_emitted = false;
        let mut preview_has_cmd = false;
        if let Some(blocks) = detail.and_then(|d| d.get("preview")).and_then(J::as_array) {
            for block in blocks {
                match block.get("type").and_then(J::as_str) {
                    Some("diff") => {
                        if let (Some(p), Some(new_text)) = (
                            block.get("path").and_then(J::as_str).filter(|s| !s.is_empty()),
                            block.get("new").and_then(J::as_str),
                        ) {
                            let is_new = block.get("is_new").and_then(J::as_bool) == Some(true);
                            let diff = if is_new {
                                Diff::new(PathBuf::from(p), new_text)
                            } else if let Some(old_text) = block.get("old").and_then(J::as_str) {
                                Diff::new(PathBuf::from(p), new_text).old_text(old_text)
                            } else {
                                Diff::new(PathBuf::from(p), new_text)
                            };
                            content.push(ToolCallContent::from(diff));
                            preview_emitted = true;
                        }
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(J::as_str).filter(|s| !s.is_empty()) {
                            if let Some(c) = cmd
                                && text.contains(c)
                            {
                                preview_has_cmd = true;
                            }
                            content.push(ToolCallContent::from(text.to_string()));
                            preview_emitted = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(c) = cmd
            && !preview_has_cmd
        {
            content.push(ToolCallContent::from(c.to_string()));
        }
        if !preview_emitted && let Some(diff) = non_empty_str(&args, "patch").or_else(|| non_empty_str(&args, "diff")) {
            content.push(ToolCallContent::from(diff.to_string()));
        }
        if !cwd.is_empty() {
            content.push(ToolCallContent::from(format!("Working directory: {cwd}")));
        }
        let reason = non_empty_str(verdict, "reason");
        let shown_path = (cmd.is_none() || !locations.is_empty()).then_some(path).flatten();
        if let Some(r) = reason
            && let Some(clean) = split_reason_parts(r, name, shown_path, cmd, !cwd.is_empty(), false, false, None)
        {
            content.push(ToolCallContent::from(format!("Reason: {clean}")));
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
        if let Some(scope) = detail.and_then(|d| d.get("scope")).filter(|v| v.is_object()) {
            meta.insert("scope".into(), scope.clone());
        }
        if let Some(shell) = detail.and_then(|d| non_empty_str(d, "command_shell")) {
            meta.insert("command_shell".into(), J::String(shell.to_string()));
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
                let tool_call = build_tool_call_update(kind, &a, &host_cwd);
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
        let raw_reason = format!("{long_cmd} · cwd /repo/work · runs outside sandbox");
        let args = [
            json!({
                "id": "call-1",
                "name": "bash",
                "cwd": "/repo/work",
                "arguments": { "command": long_cmd }
            }),
            json!({ "command_tool": true, "side_effect": "external", "unconfined": true }),
            json!({ "reason": raw_reason }),
        ];
        let (opts, offered) = permission_options("tool", &args);
        assert!(offered);
        let always = opts.iter().find(|o| o.option_id.0.as_ref() == "allow-always").expect("allow-always");
        assert_eq!(always.name, "Allow for this session");
        assert_eq!(always.kind, PermissionOptionKind::AllowAlways);

        let update = build_tool_call_update("tool", &args, Path::new("/fallback"));
        let serialized = serde_json::to_value(&update).unwrap();
        let content_blocks = serialized["content"].as_array().expect("content array");
        let content_str = serialized["content"].to_string();
        assert!(content_str.contains("TAIL_END"), "full command must appear in ACP content: {serialized}");
        assert!(content_str.contains("Working directory: /repo/work"), "{serialized}");
        assert!(
            content_blocks.iter().any(|c| c["content"]["text"] == "Reason: runs outside sandbox"),
            "ACP visible Reason must strip duplicate command & cwd while preserving policy warning: {serialized}"
        );
        assert!(content_str.contains("Runs outside the local sandbox"), "{serialized}");
        assert_eq!(serialized["_meta"]["unconfined"], json!(true));
        assert_eq!(serialized["_meta"]["reason"], json!(raw_reason));
        assert!(serialized["_meta"].get("diff").is_none(), "_meta.diff must not duplicate patch content: {serialized}");

        // Shared `approval_detail` drives Diff, text preview, locations, resolved command, and scope
        // for both built-in and custom tools without hardcoding tool names.
        let custom_diff_args = [
            json!({
                "id": "call-2",
                "name": "custom_editor",
                "cwd": "/repo",
                "arguments": { "target": "src/lib.rs" }
            }),
            json!({
                "side_effect": "write",
                "approval_detail": {
                    "locations": [{ "path": "/repo/src/lib.rs" }],
                    "preview": [{
                        "type": "diff",
                        "path": "/repo/src/lib.rs",
                        "old": "foo one\nfoo two\n",
                        "new": "bar one\nbar two\n"
                    }],
                    "scope": { "scoped": false, "session_allowed": true, "kind": "exact_call" }
                }
            }),
            json!({ "reason": "custom editor approval" }),
        ];
        let custom_diff_json =
            serde_json::to_value(build_tool_call_update("tool", &custom_diff_args, Path::new("/repo"))).unwrap();
        assert_eq!(custom_diff_json["locations"][0]["path"], json!("/repo/src/lib.rs"), "{custom_diff_json}");
        assert!(
            custom_diff_json["content"].as_array().unwrap().iter().any(|c| {
                c["type"] == "diff"
                    && c["path"] == "/repo/src/lib.rs"
                    && c["oldText"] == "foo one\nfoo two\n"
                    && c["newText"] == "bar one\nbar two\n"
            }),
            "custom tool preview diff must project to ACP diff content: {custom_diff_json}"
        );
        assert_eq!(custom_diff_json["_meta"]["scope"]["kind"], json!("exact_call"), "{custom_diff_json}");

        // New-file diff (`is_new: true`) omits `oldText`, and custom text preview blocks are forwarded.
        let new_file_args = [
            json!({
                "id": "call-3",
                "name": "custom_writer",
                "cwd": "/repo",
                "arguments": { "target": "new.txt" }
            }),
            json!({
                "side_effect": "write",
                "approval_detail": {
                    "locations": [{ "path": "/repo/new.txt" }],
                    "preview": [
                        { "type": "text", "text": "Creating config file" },
                        { "type": "diff", "path": "/repo/new.txt", "new": "hello\n", "is_new": true }
                    ]
                }
            }),
            json!({ "reason": "write file" }),
        ];
        let new_file_json =
            serde_json::to_value(build_tool_call_update("tool", &new_file_args, Path::new("/repo"))).unwrap();
        let new_blocks = new_file_json["content"].as_array().unwrap();
        assert!(new_blocks.iter().any(|c| c["content"]["text"] == "Creating config file"), "{new_file_json}");
        assert!(
            new_blocks.iter().any(|c| c["type"] == "diff" && c.get("oldText").is_none() && c["newText"] == "hello\n"),
            "new-file diff must omit oldText: {new_file_json}"
        );

        // Tools without `approval_detail.preview`/`locations` (e.g. remote MCP tools or shadowed
        // `"write"` tools) must NEVER emit fabricated local Diff content or local file locations.
        let shadowed_write_args = [
            json!({
                "id": "call-4",
                "name": "write",
                "cwd": "/repo",
                "arguments": { "path": "docs/spec.md", "content": "remote body" }
            }),
            json!({
                "side_effect": "external",
                "unconfined": true,
                "approval_detail": {
                    "preview": [{ "type": "text", "text": "Remote write to docs/spec.md" }],
                    "scope": { "scoped": false, "session_allowed": false, "kind": "exact_call" }
                }
            }),
            json!({ "reason": "unconfined tool" }),
        ];
        let (_, shadowed_allow_always) = permission_options("tool", &shadowed_write_args);
        assert!(!shadowed_allow_always, "permission_options must honor approval_detail.scope.session_allowed");
        let shadowed_json =
            serde_json::to_value(build_tool_call_update("tool", &shadowed_write_args, Path::new("/repo"))).unwrap();
        assert!(
            shadowed_json["content"].as_array().unwrap().iter().all(|c| c["type"] != "diff"),
            "shadowed write tool without diff preview must not fabricate local Diff: {shadowed_json}"
        );
        assert!(
            shadowed_json["content"].to_string().contains("Remote write to docs/spec.md"),
            "shadowed write tool must forward its own preview text: {shadowed_json}"
        );
        assert!(
            shadowed_json.get("locations").is_none()
                || shadowed_json["locations"].as_array().is_some_and(Vec::is_empty),
            "tool without declared locations must not emit local file locations: {shadowed_json}"
        );

        // Resolved command from `#128` (`approval_detail.command`) populates title and content even
        // when the tool argument is not named `command`.
        let dynamic_cmd_args = [
            json!({
                "id": "call-5",
                "name": "multi_shell",
                "cwd": "/repo",
                "arguments": { "dialect": "powershell", "script": "Get-ChildItem -Force" }
            }),
            json!({
                "command_tool": true,
                "side_effect": "external",
                "approval_detail": {
                    "command": "Get-ChildItem -Force",
                    "command_shell": "powershell",
                    "scope": { "scoped": false, "session_allowed": true, "kind": "exact_command" }
                }
            }),
            json!({ "reason": "powershell: Get-ChildItem -Force · ask" }),
        ];
        let dyn_json =
            serde_json::to_value(build_tool_call_update("tool", &dynamic_cmd_args, Path::new("/repo"))).unwrap();
        assert_eq!(dyn_json["title"], json!("multi_shell Get-ChildItem -Force"), "{dyn_json}");
        assert!(dyn_json["content"].to_string().contains("Get-ChildItem -Force"), "{dyn_json}");
        assert_eq!(dyn_json["_meta"]["command_shell"], json!("powershell"), "{dyn_json}");
    }
}
