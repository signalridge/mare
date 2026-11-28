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

fn raw_path_arg(args: &J) -> Option<&str> {
    args.get("path")
        .and_then(J::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| args.get("file_path").and_then(J::as_str).filter(|s| !s.is_empty()))
}

fn resolve_abs_path(cwd: &str, raw: &str) -> PathBuf {
    mare_binding::fs::resolve_path(Path::new(cwd), raw)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileDiffState {
    NewFile,
    DiffTexts { old: String, new: String },
    Unavailable,
}

/// Inspect a `write` or `edit` target through `mare.fs` with the active sandbox policy so
/// `deny_read` rules are honored.
/// - For `write`: returns `NewFile` when absent, `DiffTexts` with full old/new file content when
///   readable within `MAX_DIFF_BYTES`, or `Unavailable` otherwise.
/// - For `edit`: reads the full file and applies `mare.text.replace` (honoring `replace_all`) to
///   produce full-file `DiffTexts`, or `Unavailable` if unreadable / replacement fails.
async fn inspect_file_diff(lua: &mlua::Lua, tool: &str, abs: &Path, args: &J) -> FileDiffState {
    const MAX_DIFF_BYTES: usize = 512 * 1024;
    let path_str = abs.to_string_lossy();
    let script = r#"
        local tool, path, max_bytes, arg1, arg2, replace_all = ...
        local policy = mare and mare.proc and mare.proc.sandbox and mare.proc.sandbox.current and mare.proc.sandbox.current()
        local policies = policy and { policy } or nil
        local st = mare.fs.stat(path, { policies = policies })
        if not st then
            if tool == "write" and not mare.fs.exists(path) then
                return "new", nil, nil
            end
            return "unavailable", nil, nil
        end
        if st.kind ~= "file" or (st.size and st.size > max_bytes) then
            return "unavailable", nil, nil
        end
        local ok, old = pcall(mare.fs.read, path, { policies = policies, max_bytes = max_bytes })
        if not ok or type(old) ~= "string" then
            return "unavailable", nil, nil
        end
        if tool == "write" then
            return "diff", old, arg1
        end
        local rok, new = pcall(mare.text.replace, old, arg1 or "", arg2 or "", replace_all == true)
        if rok and type(new) == "string" then
            return "diff", old, new
        end
        return "unavailable", nil, nil
    "#;
    let (arg1, arg2, replace_all) = if tool == "write" {
        (args.get("content").and_then(J::as_str).unwrap_or(""), "", false)
    } else {
        (
            args.get("old_string").and_then(J::as_str).unwrap_or(""),
            args.get("new_string").and_then(J::as_str).unwrap_or(""),
            args.get("replace_all").and_then(J::as_bool).unwrap_or(false),
        )
    };
    match lua
        .load(script)
        .call_async::<(String, Option<String>, Option<String>)>((
            tool,
            path_str.as_ref(),
            MAX_DIFF_BYTES,
            arg1,
            arg2,
            replace_all,
        ))
        .await
    {
        Ok((tag, Some(old), Some(new))) if tag == "diff" => FileDiffState::DiffTexts { old, new },
        Ok((tag, _, _)) if tag == "new" => FileDiffState::NewFile,
        _ => FileDiffState::Unavailable,
    }
}

async fn inspect_tool_file_diff(lua: &mlua::Lua, kind: &str, a: &[J], host_cwd: &Path) -> Option<FileDiffState> {
    if kind == "tool"
        && let Some(tool_name @ ("write" | "edit")) = a[0].get("name").and_then(J::as_str)
        && let Some(args) = a[0].get("arguments")
        && ((tool_name == "write" && args.get("content").and_then(J::as_str).is_some())
            || (tool_name == "edit"
                && args.get("old_string").and_then(J::as_str).is_some()
                && args.get("new_string").and_then(J::as_str).is_some()))
        && let Some(p) = raw_path_arg(args)
    {
        let cwd =
            non_empty_str(&a[0], "cwd").map(str::to_owned).unwrap_or_else(|| host_cwd.to_string_lossy().into_owned());
        let abs = resolve_abs_path(&cwd, p);
        Some(inspect_file_diff(lua, tool_name, &abs, args).await)
    } else {
        None
    }
}

fn build_tool_call_update(kind: &str, a: &[J], host_cwd: &Path, file_diff: Option<FileDiffState>) -> ToolCallUpdate {
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
        if let Some(c) = cmd {
            content.push(ToolCallContent::from(c.to_string()));
        }
        let is_file_tool = matches!(name, "write" | "edit");
        if is_file_tool && let Some(p) = raw_path_arg(&args) {
            let abs = resolve_abs_path(&cwd, p);
            locations.push(ToolCallLocation::new(abs.clone()));
            if name == "edit"
                && let (Some(old_s), Some(new_s)) =
                    (args.get("old_string").and_then(J::as_str), args.get("new_string").and_then(J::as_str))
            {
                match file_diff.unwrap_or(FileDiffState::Unavailable) {
                    FileDiffState::DiffTexts { old, new } => {
                        content.push(ToolCallContent::from(Diff::new(abs, new).old_text(old)));
                    }
                    FileDiffState::NewFile | FileDiffState::Unavailable => {
                        let mode = if args.get("replace_all").and_then(J::as_bool).unwrap_or(false) {
                            " (replace_all)"
                        } else {
                            ""
                        };
                        content.push(ToolCallContent::from(format!(
                            "Edit {}{mode}:\n--- old\n{old_s}\n+++ new\n{new_s}",
                            abs.display()
                        )));
                    }
                }
            } else if name == "write"
                && let Some(new_s) = args.get("content").and_then(J::as_str)
            {
                match file_diff.unwrap_or(FileDiffState::Unavailable) {
                    FileDiffState::NewFile => {
                        content.push(ToolCallContent::from(Diff::new(abs, new_s)));
                    }
                    FileDiffState::DiffTexts { old, new } => {
                        content.push(ToolCallContent::from(Diff::new(abs, new).old_text(old)));
                    }
                    FileDiffState::Unavailable => {
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
        let shown_path = (cmd.is_none() || is_file_tool).then_some(path).flatten();
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
                let file_diff = inspect_tool_file_diff(&lua, kind, &a, &host_cwd).await;
                let tool_call = build_tool_call_update(kind, &a, &host_cwd, file_diff);
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

        let update = build_tool_call_update("tool", &args, Path::new("/fallback"), None);
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

        let edit_args = [
            json!({
                "id": "call-2",
                "name": "edit",
                "cwd": "/repo",
                "arguments": { "path": "src/lib.rs", "old_string": "foo", "new_string": "bar", "replace_all": true }
            }),
            json!({ "side_effect": "write" }),
            json!({ "reason": "edit file" }),
        ];
        let edit_json = serde_json::to_value(build_tool_call_update(
            "tool",
            &edit_args,
            Path::new("/repo"),
            Some(FileDiffState::DiffTexts { old: "foo one\nfoo two\n".into(), new: "bar one\nbar two\n".into() }),
        ))
        .unwrap();
        assert!(
            edit_json["content"].as_array().unwrap().iter().any(|c| {
                c["type"] == "diff" && c["oldText"] == "foo one\nfoo two\n" && c["newText"] == "bar one\nbar two\n"
            }),
            "edit approval must include full-file ACP diff content: {edit_json}"
        );

        let edit_fallback_json = serde_json::to_value(build_tool_call_update(
            "tool",
            &edit_args,
            Path::new("/repo"),
            Some(FileDiffState::Unavailable),
        ))
        .unwrap();
        assert!(
            edit_fallback_json["content"].as_array().unwrap().iter().all(|c| c["type"] != "diff"),
            "unavailable edit must not emit partial snippet Diff: {edit_fallback_json}"
        );
        let edit_fallback_str = edit_fallback_json["content"].to_string();
        assert!(
            edit_fallback_str.contains("replace_all")
                && edit_fallback_str.contains("foo")
                && edit_fallback_str.contains("bar"),
            "unavailable edit fallback must note replace_all and old/new snippets: {edit_fallback_json}"
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
            Some(FileDiffState::DiffTexts { old: "previous".into(), new: "updated".into() }),
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
            Some(FileDiffState::Unavailable),
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

        // Non-write/edit tools (e.g. remote MCP tools) with `path` + `content` must not emit local
        // Diff content or file locations.
        let mcp_args = [
            json!({
                "id": "call-4",
                "name": "mcp__docs__save",
                "cwd": "/repo",
                "arguments": { "path": "docs/spec.md", "content": "remote body" }
            }),
            json!({ "side_effect": "external", "unconfined": true }),
            json!({ "reason": "unconfined tool" }),
        ];
        let mcp_json =
            serde_json::to_value(build_tool_call_update("tool", &mcp_args, Path::new("/repo"), None)).unwrap();
        assert!(
            mcp_json["content"].as_array().unwrap().iter().all(|c| c["type"] != "diff"),
            "MCP tool with path+content must not emit local Diff: {mcp_json}"
        );
        assert!(
            mcp_json.get("locations").is_none() || mcp_json["locations"].as_array().is_some_and(Vec::is_empty),
            "MCP tool with path+content must not emit local file locations: {mcp_json}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn acp_file_diff_and_locations_preserve_raw_untrimmed_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes"), "from-unspaced\n").unwrap();
        std::fs::write(dir.path().join("notes "), "from-trailing-space\n").unwrap();

        let lua = mlua::Lua::new();
        let mare = lua.create_table().unwrap();
        mare_binding::fs::install(&lua, &mare, dir.path().to_path_buf()).unwrap();
        mare_binding::text::install(&lua, &mare, dir.path().to_path_buf()).unwrap();
        lua.globals().set("mare", mare).unwrap();

        let cwd = dir.path().to_string_lossy().into_owned();
        let expected_path = dir.path().join("notes ").to_string_lossy().into_owned();

        let edit_args = [
            json!({
                "id": "call-edit-space",
                "name": "edit",
                "cwd": cwd,
                "arguments": {
                    "path": "notes ",
                    "old_string": "from-trailing-space",
                    "new_string": "updated-trailing-space"
                }
            }),
            json!({ "side_effect": "write" }),
            json!({ "reason": "edit file" }),
        ];
        let edit_diff = inspect_tool_file_diff(&lua, "tool", &edit_args, dir.path()).await;
        let edit_json =
            serde_json::to_value(build_tool_call_update("tool", &edit_args, dir.path(), edit_diff)).unwrap();
        assert_eq!(edit_json["locations"][0]["path"], json!(expected_path), "{edit_json}");
        assert!(
            edit_json["content"].as_array().unwrap().iter().any(|c| {
                c["type"] == "diff"
                    && c["path"] == expected_path
                    && c["oldText"] == "from-trailing-space\n"
                    && c["newText"] == "updated-trailing-space\n"
            }),
            "edit diff must read and report the raw 'notes ' path, not trimmed 'notes': {edit_json}"
        );

        let write_args = [
            json!({
                "id": "call-write-space",
                "name": "write",
                "cwd": cwd,
                "arguments": { "path": "notes ", "content": "rewritten\n" }
            }),
            json!({ "side_effect": "write" }),
            json!({ "reason": "write file" }),
        ];
        let write_diff = inspect_tool_file_diff(&lua, "tool", &write_args, dir.path()).await;
        let write_json =
            serde_json::to_value(build_tool_call_update("tool", &write_args, dir.path(), write_diff)).unwrap();
        assert_eq!(write_json["locations"][0]["path"], json!(expected_path), "{write_json}");
        assert!(
            write_json["content"].as_array().unwrap().iter().any(|c| {
                c["type"] == "diff"
                    && c["path"] == expected_path
                    && c["oldText"] == "from-trailing-space\n"
                    && c["newText"] == "rewritten\n"
            }),
            "write diff must read and report the raw 'notes ' path, not trimmed 'notes': {write_json}"
        );
    }
}
