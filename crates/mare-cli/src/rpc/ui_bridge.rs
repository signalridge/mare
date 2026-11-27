//! External UI attachment, component takeover, and editor state sync.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use mare_host::SessionRuntime;
use mare_protocol::{self as proto, methods};
use mlua::Value;
use serde_json::{Value as J, json};

use super::*;

fn invalid(e: String) -> proto::RpcError {
    proto::RpcError::core(proto::ErrorCode::InvalidArguments, e)
}

fn denied(msg: &str) -> proto::RpcError {
    proto::RpcError::coded("denied", msg)
}

#[cfg(test)]
pub(super) fn denied_for_test(msg: &str) -> proto::RpcError {
    denied(msg)
}

async fn apply_viewport(host: &Arc<Host>, v: &proto::UiViewport) {
    let mut changed = false;
    if let Some(w) = v.width
        && host.ui.width.swap(w, Ordering::Relaxed) != w
    {
        changed = true;
    }
    if let Some(h) = v.height
        && host.ui.height.swap(h, Ordering::Relaxed) != h
    {
        changed = true;
    }
    if changed {
        host.kernel.emit_json("ui/resize", json!({})).await;
    }
}

fn validate_image_marker(marker: &str) -> Result<(), proto::RpcError> {
    let trimmed = marker.trim();
    if trimmed.is_empty() || trimmed.len() > 256 || trimmed.chars().any(char::is_control) {
        return Err(invalid("invalid editor image marker".into()));
    }
    if trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("file://")
        || (trimmed.len() >= 3 && trimmed.as_bytes()[1] == b':' && matches!(trimmed.as_bytes()[2], b'/' | b'\\'))
    {
        return Err(invalid("editor image markers cannot carry local filesystem paths".into()));
    }
    Ok(())
}

pub(super) fn validate_editor_attachments(
    markers: Option<&[String]>,
    attachments: Option<&[proto::UiEditorAttachment]>,
) -> Result<(), proto::RpcError> {
    if let Some(list) = markers {
        if list.len() > 64 {
            return Err(invalid("too many editor image markers (max 64)".into()));
        }
        for m in list {
            validate_image_marker(m)?;
        }
    }
    if let Some(list) = attachments {
        if list.len() > 64 {
            return Err(invalid("too many editor attachments (max 64)".into()));
        }
        for a in list {
            validate_image_marker(&a.attachment_id)?;
            if let Some(label) = &a.label {
                validate_image_marker(label)?;
            }
        }
    }
    Ok(())
}

async fn release_ui_detach_registrations_for(conn: &Conn, session_id: &str, host: &Host) {
    let reg_ids = conn.global_registrations.borrow_mut().take_ui_detach_ids(session_id);
    if reg_ids.is_empty() {
        return;
    }
    let Some(ctx_id) = conn.session_ctxs.borrow().get(session_id).map(|(_, cid)| *cid) else {
        return;
    };
    for reg_id in reg_ids {
        let _ = host.kernel.release_registration(ctx_id, &reg_id).await;
    }
}

fn release_ui_responder_for_session(conn: &Conn, session_id: &str, host: &Host, reason: &str) {
    if !conn.is_answerer_on(host) {
        return;
    }
    conn.cancel_ui_pending_for_session(session_id, reason);
    let caps = conn.caps.borrow().clone();
    if !caps.approvals && !caps.elicitation {
        host.answerer.release(host.lua(), conn.id, reason);
    } else {
        host.answerer.restore_ui(host.lua(), conn.id);
    }
}

pub(super) async fn detach_shell_and_restore_viewport(
    rpc: &RpcRuntime,
    conn_id: mare_host::ConnId,
    rt: &SessionRuntime,
) {
    let (removed, editor_changed) = rt.host.shell.detach(conn_id, Some(&rt.session_id));
    if let Some(ed) = editor_changed {
        rpc.broadcast_editor_state(&rt.session_id, &rt.host, &ed);
    }
    if removed.and_then(|att| att.viewport).is_some()
        && let Some(vp) = rt.host.shell.active_viewport()
    {
        apply_viewport(&rt.host, &vp).await;
    }
}

async fn detach_from_runtime(conn: &Rc<Conn>, rt: &Rc<SessionRuntime>) {
    detach_shell_and_restore_viewport(&conn.rpc, conn.id, rt).await;
    release_ui_responder_for_session(conn, &rt.session_id, &rt.host, "ui detached");
    release_ui_detach_registrations_for(conn, &rt.session_id, &rt.host).await;
}

pub(super) async fn cleanup_conn(conn: &Rc<Conn>) {
    for rt in conn.rpc.active_sessions() {
        detach_from_runtime(conn, &rt).await;
    }
    conn.global_registrations.borrow_mut().clear_ui_detach_ids();
}

pub(super) async fn handle_ui_attach(conn: &Rc<Conn>, p: &Option<J>) -> Result<J, proto::RpcError> {
    let ap: proto::UiAttachParams = params_req(p).map_err(invalid)?;
    let (rt, identity) = resolve_session_target(conn, &ap.session_id, None).await?;
    if !access::authorize(conn, &rt.host, access::Op::Protocol(methods::UI_ATTACH)).is_allowed() {
        return Err(denied("not authorized to attach UI to this session"));
    }
    let takeover_vec = mare_protocol::validate_takeovers(&ap.takeover).map_err(invalid)?;
    if let Some(ref vp) = ap.viewport {
        vp.validate().map_err(|m| invalid(m.into()))?;
    }

    forward_ui::install_shell_mirror_on(&conn.rpc, &rt.session_id, &rt.host).map_err(|e| failure(e.into()))?;

    // identity/lease: claiming the UI responder lease requires admin owner authority
    let was_responder = rt.host.shell.is_responder(conn.id);
    let answerer = access::authorize(conn, &rt.host, access::Op::Answerer);
    let can_answer = answerer.is_allowed();
    let wants_responder = ap.responder.unwrap_or_else(|| can_answer && (conn.caps.borrow().ui || was_responder));
    if wants_responder {
        answerer.into_error("ui responder")?;
    }

    if wants_responder {
        let _permit = conn
            .cap_gate
            .acquire()
            .await
            .map_err(|_| proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"))?;
        if conn.closed.get() {
            return Err(proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"));
        }
        let ctx_id = conn.ensure_session_ctx(&rt.session_id, &rt.host).await?;
        if conn.closed.get() {
            return Err(proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"));
        }
        conn.acquire_answerer(&rt.host, &rt.session_id);
        forward_ui::forward_ui_on(conn, &rt.session_id, &rt.host, ctx_id).map_err(|e| failure(e.into()))?;
    } else if ap.responder == Some(false) || was_responder {
        let _permit = conn
            .cap_gate
            .acquire()
            .await
            .map_err(|_| proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"))?;
        if conn.closed.get() {
            return Err(proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"));
        }
        if !can_answer {
            conn.cancel_ui_pending_for_session(&rt.session_id, "answerer authority revoked");
            rt.host.answerer.release(rt.host.lua(), conn.id, "answerer authority revoked");
        } else {
            release_ui_responder_for_session(conn, &rt.session_id, &rt.host, "ui responder released");
        }
    }

    let att = mare_host::UiAttachment {
        conn_id: conn.id,
        responder: wants_responder,
        takeover: takeover_vec.into_iter().collect(),
        viewport: ap.viewport.clone(),
    };
    let (accepted_takeover, snapshot) = rt.host.shell.attach(att, Some(&rt.session_id));
    if let Some(ref vp) = ap.viewport {
        apply_viewport(&rt.host, vp).await;
    }
    if conn.closed.get() {
        detach_from_runtime(conn, &rt).await;
        return Err(proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"));
    }
    let snapshot = if ap.viewport.is_some() {
        rt.host
            .shell
            .attach(
                mare_host::UiAttachment {
                    conn_id: conn.id,
                    responder: wants_responder,
                    takeover: accepted_takeover.iter().cloned().collect(),
                    viewport: ap.viewport,
                },
                Some(&rt.session_id),
            )
            .1
    } else {
        snapshot
    };

    Ok(serde_json::to_value(proto::UiAttachResult {
        session_id: rt.session_id.clone(),
        session_epoch: identity.session_epoch,
        responder: wants_responder,
        takeover: accepted_takeover,
        snapshot,
    })
    .unwrap_or(J::Null))
}

pub(super) async fn handle_ui_detach(conn: &Rc<Conn>, p: &Option<J>) -> Result<J, proto::RpcError> {
    let dp: proto::UiDetachParams = params_req(p).map_err(invalid)?;
    let (rt, _) = resolve_session_target(conn, &dp.session_id, None).await?;
    if !access::authorize(conn, &rt.host, access::Op::Protocol(methods::UI_DETACH)).is_allowed() {
        return Err(denied("not authorized to detach UI"));
    }
    detach_from_runtime(conn, &rt).await;
    Ok(serde_json::to_value(proto::UiDetachResult { session_id: rt.session_id.clone() }).unwrap_or(J::Null))
}

fn coded_result(code: &str, message: impl Into<String>) -> J {
    json!({ "error": { "code": code, "message": message.into() } })
}

#[derive(serde::Deserialize)]
struct ContractAttachmentInput {
    attachment_id: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
}

fn contract_atts_to_proto(raw: Option<J>) -> Result<Option<Vec<proto::UiEditorAttachment>>, String> {
    let Some(val) = raw else { return Ok(None) };
    if val.is_null() {
        return Ok(None);
    }
    let parsed: Vec<ContractAttachmentInput> = serde_json::from_value(val).map_err(|e| e.to_string())?;
    Ok(Some(
        parsed
            .into_iter()
            .map(|a| proto::UiEditorAttachment {
                attachment_id: a.attachment_id,
                label: a.label,
                mime_type: a.mime_type,
                width: a.width,
                height: a.height,
            })
            .collect(),
    ))
}

fn contract_atts_from_proto(atts: &[proto::UiEditorAttachment]) -> Vec<J> {
    atts.iter()
        .map(|a| {
            let mut obj = serde_json::Map::new();
            obj.insert("attachment_id".into(), J::String(a.attachment_id.clone()));
            if let Some(ref l) = a.label {
                obj.insert("label".into(), J::String(l.clone()));
            }
            if let Some(ref m) = a.mime_type {
                obj.insert("mime_type".into(), J::String(m.clone()));
            }
            if let Some(w) = a.width {
                obj.insert("width".into(), json!(w));
            }
            if let Some(h) = a.height {
                obj.insert("height".into(), json!(h));
            }
            J::Object(obj)
        })
        .collect()
}

pub(super) fn rpc_update_editor(rpc: &RpcRuntime, host: &Host, session_id: &str, text: String, opts: J) -> J {
    let obj = opts.as_object();
    let offset = obj.and_then(|o| o.get("offset")).and_then(J::as_u64).map(|n| n as usize);
    let shell_mode = obj.and_then(|o| o.get("shell_mode")).and_then(J::as_bool);
    let image_markers: Option<Vec<String>> =
        obj.and_then(|o| o.get("image_markers")).and_then(|v| serde_json::from_value(v.clone()).ok());
    let attachments = match contract_atts_to_proto(obj.and_then(|o| o.get("attachments")).cloned()) {
        Ok(a) => a,
        Err(e) => return coded_result("invalid_arguments", e),
    };
    if let Err(err) = validate_editor_attachments(image_markers.as_deref(), attachments.as_deref()) {
        return coded_result("invalid_arguments", err.message());
    }
    let next =
        match host.shell.update_editor_input(Some(session_id), text, offset, shell_mode, image_markers, attachments) {
            Ok(st) => st,
            Err(msg) => return coded_result("busy", msg),
        };
    rpc.broadcast_editor_state(session_id, host, &next);
    json!({
        "value": {
            "text": next.text,
            "offset": next.cursor,
            "shell_mode": next.shell_mode,
            "image_markers": next.image_markers,
            "attachments": contract_atts_from_proto(&next.attachments),
            "external_editor_running": next.external_editor_running,
            "revision": next.revision,
        }
    })
}

async fn editor_content_parts(
    conn: &Conn,
    host: &Host,
    text: &str,
    attachments: &[proto::Attachment],
) -> Result<J, proto::RpcError> {
    let mut parts = Vec::with_capacity(attachments.len() + 1);
    if !text.is_empty() {
        parts.push(json!({ "type": "text", "text": text }));
    }
    for a in attachments {
        if let Some(kind) = a.media_kind() {
            let (cap, what) = match kind {
                proto::MediaKind::Video => ("video", "video attachment"),
                proto::MediaKind::Audio => ("audio", "audio attachment"),
            };
            return Err(proto::RpcError::coded("unsupported", format!("{what} is unsupported"))
                .with_data(json!({ "capability": cap })));
        }
        if let Some(data) = &a.data {
            if a.kind == Some(proto::AttachmentKind::File) {
                return Err(super::dispatch::rpc_err(
                    proto::ErrorCode::InvalidArguments,
                    "file attachment requires `path`",
                ));
            }
            parts.push(json!({
                "type": "image",
                "data": data,
                "mime": a.mime.clone().unwrap_or_else(|| "image/png".into()),
            }));
        } else if let Some(url) = &a.url
            && a.path.is_none()
        {
            let Some(rest) = url.strip_prefix("data:") else {
                return Err(super::dispatch::rpc_err(
                    proto::ErrorCode::InvalidArguments,
                    "attachment `url` only supports `data:image/...` URIs; use `path` or base64 `data`",
                ));
            };
            let Some((meta, b64)) = rest.split_once(";base64,") else {
                return Err(super::dispatch::rpc_err(
                    proto::ErrorCode::InvalidArguments,
                    "attachment `url` only supports `data:image/...` URIs; use `path` or base64 `data`",
                ));
            };
            if !meta.to_ascii_lowercase().starts_with("image/") {
                return Err(super::dispatch::rpc_err(
                    proto::ErrorCode::InvalidArguments,
                    "attachment `url` only supports `data:image/...` URIs; use `path` or base64 `data`",
                ));
            }
            parts.push(json!({ "type": "image", "data": b64, "mime": meta }));
        } else if let Some(p) = &a.path {
            if a.kind == Some(proto::AttachmentKind::File) {
                let mut part = json!({ "type": "file", "path": p });
                if let Some(name) = a.name.as_deref().filter(|s| !s.is_empty()) {
                    part["name"] = J::String(name.to_owned());
                }
                if let Some(mime) = a.mime.as_deref().filter(|s| !s.is_empty()) {
                    part["mime"] = J::String(mime.to_owned());
                }
                parts.push(part);
            } else {
                let encoded = super::dispatch::read_policy_image(conn, host, p).await?;
                parts.push(json!({ "type": "image", "data": encoded.data, "mime": encoded.mime }));
            }
        } else {
            return Err(super::dispatch::rpc_err(
                proto::ErrorCode::InvalidArguments,
                "attachment requires `path`, `data`, or `url`",
            ));
        }
    }
    Ok(J::Array(parts))
}

pub(super) async fn rpc_send_editor(
    rpc: &RpcRuntime,
    host: &Arc<Host>,
    session_id: &str,
    text: Option<String>,
    mode: String,
    raw_attachments: Option<J>,
    allow_empty_from_keymap: bool,
) -> J {
    let caller_conn = super::services::CALL_CONN.try_with(std::rc::Weak::clone).ok().and_then(|w| w.upgrade());
    let sp_attachments: Vec<proto::Attachment> = match raw_attachments {
        Some(v) if !v.is_null() => match serde_json::from_value(v) {
            Ok(a) => a,
            Err(e) => return coded_result("invalid_arguments", format!("invalid attachments: {e}")),
        },
        _ => Vec::new(),
    };
    let (draft_text, draft_atts, peeked_rev) = host.shell.peek_editor_with_revision();
    let uses_shared_draft = text.is_none();
    let preview_text = text.as_deref().unwrap_or(draft_text.as_str());
    if uses_shared_draft && !draft_atts.is_empty() && sp_attachments.is_empty() {
        return coded_result(
            "invalid_arguments",
            "editor submission with staged attachments requires attachment payloads (`attachments`)",
        );
    }
    if preview_text.trim().is_empty() && sp_attachments.is_empty() {
        if allow_empty_from_keymap {
            return json!({ "empty_reason": "editor buffer is empty" });
        }
        return coded_result("invalid_arguments", "submitted editor text must not be empty");
    }
    let built_parts = if !sp_attachments.is_empty() {
        let Some(ref conn) = caller_conn else {
            return coded_result("invalid_arguments", "attachment resolution requires an RPC connection");
        };
        match editor_content_parts(conn, host, preview_text, &sp_attachments).await {
            Ok(p) => Some(p),
            Err(err) => {
                let code = err.error_code().unwrap_or("invalid_arguments");
                return coded_result(code, err.message());
            }
        }
    } else {
        None
    };

    let submitted = preview_text.to_owned();
    let opts = json!({
        "source": if caller_conn.as_ref().is_some_and(|c| !c.is_owner()) { "peer" } else { "rpc" }
    });

    // Clear the shared draft only if unchanged since peek, right before `input/submit` so
    // handlers calling `set_editor(..., { keep_draft = true | prepend = true })` observe an
    // empty buffer and their restored text is preserved; on agent submit failure, roll back
    // only if no handler or other client mutated the editor in the meantime.
    let cleared_draft = if uses_shared_draft {
        if let Some((saved_text, saved_markers, saved_atts, next)) =
            host.shell.take_editor_draft_if_revision(Some(session_id), peeked_rev)
        {
            rpc.broadcast_editor_state(session_id, host, &next);
            Some((saved_text, saved_markers, saved_atts, next.revision))
        } else {
            None
        }
    } else {
        None
    };

    let lua = host.lua();
    let Ok(ev) = lua.create_table() else {
        if let Some((saved_text, saved_markers, saved_atts, cleared_rev)) = cleared_draft
            && let Some(restored) = host.shell.restore_editor_if_revision(
                Some(session_id),
                cleared_rev,
                saved_text,
                saved_markers,
                saved_atts,
            )
        {
            rpc.broadcast_editor_state(session_id, host, &restored);
        }
        return coded_result("internal", "lua error");
    };
    let _ = ev.set("text", submitted.as_str());
    let _ = ev.set("mode", mode.as_str());
    let _ = ev.set("handled", false);
    let _ = host.kernel.emit("input/submit", mlua::MultiValue::from_vec(vec![Value::Table(ev.clone())])).await;
    let handled = ev.get::<Option<bool>>("handled").ok().flatten().unwrap_or(false);
    let result = if handled {
        None
    } else {
        let raw_input = built_parts.unwrap_or_else(|| json!(submitted));
        let service_method = mode.as_str();
        let req_cancel = request_cancel();
        let session_cancel = host.session_cancel_token();
        let submit_res = match mare_shared::cancel::or_cancel_both(
            host.invoke_agent_submit(service_method, raw_input, opts),
            &req_cancel,
            Some(&session_cancel),
        )
        .await
        {
            Ok(Ok(res)) => Ok(res),
            Ok(Err(err)) => Err(failure(coded(host.lua(), err))),
            Err(_) => Err(proto::RpcError::core(
                proto::ErrorCode::Cancelled,
                format!("editor submit `{service_method}` cancelled"),
            )),
        };
        match submit_res {
            Ok(res) => Some(res),
            Err(err) => {
                if let Some((saved_text, saved_markers, saved_atts, cleared_rev)) = cleared_draft
                    && let Some(restored) = host.shell.restore_editor_if_revision(
                        Some(session_id),
                        cleared_rev,
                        saved_text,
                        saved_markers,
                        saved_atts,
                    )
                {
                    rpc.broadcast_editor_state(session_id, host, &restored);
                }
                let code = err.error_code().unwrap_or("invalid_arguments");
                return coded_result(code, err.message());
            }
        }
    };
    let revision = host.shell.editor_revision();
    let mut out = json!({
        "revision": revision,
        "handled": handled,
    });
    if let Some(res) = result
        && res.is_object()
    {
        out["result"] = res;
    }
    json!({ "value": out })
}

pub(super) fn rpc_set_external_editor(
    rpc: &RpcRuntime,
    host: &Host,
    session_id: &str,
    action: &str,
    text: Option<String>,
) -> J {
    let conn_id = super::services::CALL_CONN
        .try_with(std::rc::Weak::clone)
        .ok()
        .and_then(|w| w.upgrade())
        .map_or(mare_host::ConnId(0), |c| c.id);
    let state = match action {
        "start" => match host.shell.start_external_editor(Some(session_id), conn_id) {
            Ok(st) => st,
            Err(msg) => return coded_result("busy", msg),
        },
        "commit" => match host.shell.commit_external_editor(Some(session_id), conn_id, text) {
            Ok(st) => st,
            Err(msg) => return coded_result("invalid_arguments", msg),
        },
        "cancel" => match host.shell.cancel_external_editor(Some(session_id), conn_id) {
            Ok(st) => st,
            Err(msg) => return coded_result("invalid_arguments", msg),
        },
        other => return coded_result("invalid_arguments", format!("unknown external editor action `{other}`")),
    };
    rpc.broadcast_editor_state(session_id, host, &state);
    let program = (action == "start").then(|| crate::editor_cmd::program(host));
    let mut out = json!({
        "running": state.external_editor_running,
        "text": state.text,
        "revision": state.revision,
    });
    if let Some(p) = program {
        out["program"] = J::String(p);
    }
    json!({ "value": out })
}
