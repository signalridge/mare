//! One JSON bridge to the kernel service exports. No provider names belong here.
use mare_host::OrchestratorError;
use tokio_util::sync::CancellationToken;

use super::session_ext::{caller_for_conn, orchestrator_error_to_rpc};
use super::*;

/// A service error on the wire: the number follows from the code, `data.code` is
/// the code a Lua caller sees, `data.data` the code's additional data.
pub(super) fn error(error: mare_kernel::ServiceCallError) -> proto::RpcError {
    let wire = proto::RpcError::coded(error.code(), error.public_message());
    match error.data() {
        Some(data) => wire.with_data(data.clone()),
        None => wire,
    }
}

fn failure(kind: &str, message: impl Into<String>) -> proto::RpcError {
    proto::RpcError::coded(kind, message)
}

fn require_method_capabilities(host: &Host, service: &str, method: &str) -> Result<(), proto::RpcError> {
    if let Some(missing) = host.kernel.method_missing_capability(service, method) {
        return Err(failure(
            "unsupported",
            format!("`{service}` provider does not support `{missing}` capability required by `{service}.{method}`"),
        ));
    }
    Ok(())
}

fn caller_owner(conn: &Conn) -> String {
    // identity: distinguish owner vs peer transaction ownership tags
    if conn.is_owner() {
        format!("rpc:owner:{}:{}", conn.id.0, conn.is_remote_admin())
    } else {
        format!("rpc:peer:{}", conn.id.0)
    }
}

/// May this connection perform contract operation `service.method` on `host`?
pub(super) fn permits_on(conn: &Conn, host: &Host, service: &str, method: &str) -> bool {
    access::authorize(conn, host, access::Op::Contract { service, method }).is_allowed()
}

/// Connection-level caller access policy registered on `host.kernel` for `ensure_session_ctx`.
pub(super) fn caller_access(conn: &Rc<Conn>, host: &Arc<Host>) -> Option<mare_kernel::CallerAccess> {
    // fast path: local owner with static remote_admin never needs per-call authorization closures
    if conn.is_remote_admin() && !conn.is_remote_transport() {
        return None;
    }
    let is_peer = !conn.is_owner();
    let conn_weak = Rc::downgrade(conn);
    let host_weak = Arc::downgrade(host);
    let path_conn = conn_weak.clone();
    let path_host = host_weak.clone();
    Some(mare_kernel::CallerAccess {
        is_peer,
        allows: Rc::new(move |service, method, access| {
            conn_weak.upgrade().zip(host_weak.upgrade()).is_some_and(|(conn, host)| {
                let admin = host.kernel.service_method(service, method).is_some_and(|s| s.admin);
                access::authorize(&conn, &host, access::Op::ContractMeta { service, method, access, admin })
                    .is_allowed()
            })
        }),
        allows_session_path: Rc::new(move || {
            path_conn.upgrade().zip(path_host.upgrade()).is_some_and(|(conn, host)| {
                let decision = access::authorize(&conn, &host, access::Op::SessionLoadByPath);
                if !decision.is_allowed() {
                    access::audit_remote_protocol(&conn, &host, "session_path", decision);
                }
                decision.is_allowed()
            })
        }),
    })
}

/// The remote operations this connection may call on `host`, as `service.method` names.
pub(super) fn operations(conn: &Conn, host: &Host) -> Vec<String> {
    let mut names = vec![];
    for service in host.kernel.service_exports() {
        let name = service["name"].as_str().unwrap_or_default();
        for (method, spec) in service["methods"].as_object().into_iter().flatten() {
            let access = spec["access"].as_str().unwrap_or_default();
            let admin = spec["admin"].as_bool().unwrap_or(false);
            if access::authorize(conn, host, access::Op::ContractMeta { service: name, method, access, admin })
                .is_allowed()
            {
                names.push(format!("{name}.{method}"));
            }
        }
    }
    names
}

async fn invoke_export_on(
    target: &Arc<Host>,
    ctx_id: u64,
    service: &str,
    method: &str,
    arguments: J,
    confined: bool,
    cancel: tokio_util::sync::CancellationToken,
) -> Result<J, mare_kernel::ServiceCallError> {
    invoke_export_on_with_id(target, ctx_id, service, method, arguments, confined, cancel, None).await
}

#[allow(clippy::too_many_arguments)]
async fn invoke_export_on_with_id(
    target: &Arc<Host>,
    ctx_id: u64,
    service: &str,
    method: &str,
    arguments: J,
    confined: bool,
    cancel: tokio_util::sync::CancellationToken,
    registration_id: Option<String>,
) -> Result<J, mare_kernel::ServiceCallError> {
    let reader = super::tree_hooks::connection_ctx_on(target, ctx_id);
    if !confined {
        return target
            .kernel
            .call_service_export_with_id(ctx_id, reader, service, method, arguments, cancel, registration_id)
            .await;
    }
    let lua = target.lua();
    let host = target.clone();
    let service = service.to_owned();
    let method = method.to_owned();
    let slot: Rc<std::cell::RefCell<Option<Result<J, mare_kernel::ServiceCallError>>>> =
        Rc::new(std::cell::RefCell::new(None));
    let slot_write = slot.clone();
    let call_fn = lua
        .create_async_function(move |_, ()| {
            let host = host.clone();
            let reader = reader.clone();
            let service = service.clone();
            let method = method.clone();
            let arguments = arguments.clone();
            let cancel = cancel.clone();
            let registration_id = registration_id.clone();
            let slot_write = slot_write.clone();
            async move {
                let res = host
                    .kernel
                    .call_service_export_with_id(ctx_id, reader, &service, &method, arguments, cancel, registration_id)
                    .await;
                *slot_write.borrow_mut() = Some(res);
                Ok(())
            }
        })
        .map_err(mare_kernel::ServiceCallError::from)?;
    mare_binding::exec_ctx::run_host_query(lua, &target.cwd, call_fn, mlua::MultiValue::new())
        .await
        .map_err(mare_kernel::ServiceCallError::from)?;
    slot.borrow_mut().take().unwrap_or_else(|| Err(mare_kernel::ServiceCallError::Cancelled))
}

pub(super) fn canonical_dir(path: &std::path::Path) -> std::path::PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

async fn rollback_installed(installed: &[(String, Arc<Host>, u64)], id: &str) {
    for (_, prev_host, prev_ctx) in installed {
        let _ = prev_host.kernel.release_registration(*prev_ctx, id).await;
    }
}

fn authorize_resolved_target(
    conn: &Conn,
    perm_host: &Arc<Host>,
    target_rt: Option<&Rc<SessionRuntime>>,
    target_host: &Arc<Host>,
    service: &str,
    method: &str,
) -> Result<(), proto::RpcError> {
    if Arc::ptr_eq(target_host, perm_host) {
        return Ok(());
    }
    let target_spec = target_host
        .kernel
        .service_method(service, method)
        .ok_or_else(|| failure("method_not_found", format!("unknown method `{service}.{method}`")))?;
    if (target_spec.project_scoped || !conn.is_owner())
        && conn.rpc.attached_host.is_none()
        && let Some(rt) = target_rt
    {
        let caller = caller_for_conn(conn, &rt.host);
        let fallback_cwd = conn.rpc.base_opts.borrow().cwd.clone();
        conn.rpc
            .manager
            .orchestrator()
            .check_project_scope(&caller, &format!("{service}.{method}"), &fallback_cwd, Some(&rt.host.cwd), || {
                OrchestratorError::unknown_session(format!("unknown session: {}", rt.session_id))
            })
            .map_err(orchestrator_error_to_rpc)?;
    }
    let decision = access::authorize(
        conn,
        target_host,
        access::Op::ContractMeta { service, method, access: target_spec.access_name(), admin: target_spec.admin },
    );
    access::audit_remote_service(conn, target_host, service, method, target_spec.access_name(), decision);
    decision.into_service_error(service, method)
}

/// Call one contract method for a JSON caller.
pub(super) async fn call(conn: &Rc<Conn>, service: &str, method: &str, mut arguments: J) -> Result<J, proto::RpcError> {
    let invalid = |message: &str| failure("invalid_arguments", message);
    let anchor = if conn.is_owner() {
        conn.rpc.ensure_live_anchor_host().await?
    } else {
        conn.rpc.ensure_project_anchor_host().await?
    };
    let obj = arguments.as_object();
    let ContractTarget { named_rt, workspace_rt, spec } = resolve_contract_target(conn, &anchor, service, method, obj);
    let spec = spec.ok_or_else(|| failure("method_not_found", format!("unknown method `{service}.{method}`")))?;
    let session_rt = spec_accepts_session_id(&spec).then(|| named_rt.clone()).flatten();
    if (spec.project_scoped || !conn.is_owner())
        && conn.rpc.attached_host.is_none()
        && let Some(ref srt) = session_rt
    {
        let caller = caller_for_conn(conn, &srt.host);
        let fallback_cwd = conn.rpc.base_opts.borrow().cwd.clone();
        conn.rpc
            .manager
            .orchestrator()
            .check_project_scope(&caller, &format!("{service}.{method}"), &fallback_cwd, Some(&srt.host.cwd), || {
                OrchestratorError::unknown_session(format!("unknown session: {}", srt.session_id))
            })
            .map_err(orchestrator_error_to_rpc)?;
    }
    let perm_host = Arc::clone(workspace_rt.as_ref().or(session_rt.as_ref()).map(|r| &r.host).unwrap_or(&anchor));
    let decision = access::authorize(
        conn,
        &perm_host,
        access::Op::ContractMeta { service, method, access: spec.access_name(), admin: spec.admin },
    );
    access::audit_remote_service(conn, &perm_host, service, method, spec.access_name(), decision);
    decision.into_service_error(service, method)?;
    if spec.supports_registration_scope() {
        let _reg_permit = conn.reg_gate.acquire().await.map_err(|_| failure("internal", "registration gate closed"))?;
        let obj_mut = arguments.as_object_mut().ok_or_else(|| invalid("arguments must be an object"))?;
        let scope = match obj_mut.remove("scope") {
            Some(J::Null) | None => None,
            Some(v) => {
                Some(serde_json::from_value::<proto::RegistrationScopeKind>(v).map_err(|e| invalid(&e.to_string()))?)
            }
        };
        let session_id = match obj_mut.remove("session_id") {
            Some(J::Null) | None => None,
            Some(J::String(s)) => {
                if s.trim().is_empty() {
                    return Err(invalid("session_id must not be empty"));
                }
                Some(s)
            }
            Some(_) => return Err(invalid("session_id must be a string")),
        };
        let resolved_scope =
            validate_registration_scope(conn, scope, session_id.as_deref(), spec.require_explicit_scope).await?;
        let mut targets = resolve_registration_targets(conn, &resolved_scope)?;
        targets.sort_by(|a, b| a.session_id.cmp(&b.session_id));

        let check_host = targets.first().map(|rt| &rt.host).unwrap_or(&anchor);
        let callback_names =
            check_host.kernel.validate_export_call(service, method, arguments.clone()).map_err(error)?;
        if !callback_names.is_empty() && !conn.caps.borrow().callbacks {
            return Err(error(mare_kernel::ServiceCallError::coded(
                "unsupported",
                "this connection does not answer callback.call",
            )));
        }

        let seq = conn.next_id.get();
        conn.next_id.set(seq + 1);
        let logical_id = format!("rpc-reg-{}-{seq}", conn.id.0);

        let cancel = request_cancel();
        if cancel.is_cancelled() {
            return Err(error(mare_kernel::ServiceCallError::Cancelled));
        }
        let mut installed: Vec<(String, Arc<Host>, u64)> = Vec::with_capacity(targets.len());
        for rt in &targets {
            let target_spec = rt
                .host
                .kernel
                .service_method(service, method)
                .ok_or_else(|| failure("method_not_found", format!("unknown method `{service}.{method}`")))?;
            let decision = access::authorize(
                conn,
                &rt.host,
                access::Op::ContractMeta {
                    service,
                    method,
                    access: target_spec.access_name(),
                    admin: target_spec.admin,
                },
            );
            if let Err(err) = decision.into_service_error(service, method) {
                rollback_installed(&installed, &logical_id).await;
                return Err(err);
            }
            if let Err(err) = require_method_capabilities(&rt.host, service, method) {
                rollback_installed(&installed, &logical_id).await;
                return Err(err);
            }
            let ctx_id = match conn.ensure_session_ctx(&rt.session_id, &rt.host).await {
                Ok(id) => id,
                Err(err) => {
                    rollback_installed(&installed, &logical_id).await;
                    return Err(err);
                }
            };
            let res = CALL_SESSION
                .scope(
                    rt.session_id.clone(),
                    CALL_CONN.scope(
                        Rc::downgrade(conn),
                        invoke_export_on_with_id(
                            &rt.host,
                            ctx_id,
                            service,
                            method,
                            arguments.clone(),
                            target_spec.confined,
                            cancel.clone(),
                            Some(logical_id.clone()),
                        ),
                    ),
                )
                .await;
            if let Err(err) = res {
                rollback_installed(&installed, &logical_id).await;
                return Err(error(err));
            }
            installed.push((rt.session_id.clone(), rt.host.clone(), ctx_id));
        }

        if conn.closed.get() {
            rollback_installed(&installed, &logical_id).await;
            return Err(error(mare_kernel::ServiceCallError::CallbackDisconnected));
        }

        if let Some(ref replace_key) = spec.replace_key
            && arguments.get("replace") == Some(&J::Bool(true))
            && let Some(key_val) = arguments.get(replace_key).and_then(J::as_str)
        {
            let installed_sids: std::collections::BTreeSet<String> =
                installed.iter().map(|(sid, _, _)| sid.clone()).collect();
            let superseded: Vec<(String, bool)> = {
                let globals = conn.global_registrations.borrow();
                globals
                    .by_id
                    .iter()
                    .filter_map(|(gid, rec)| match &rec.kind {
                        GlobalRegistrationKind::Contract {
                            service: s, method: m, validated_arguments: args, ..
                        } if s == service
                            && m == method
                            && args.get(replace_key).and_then(J::as_str) == Some(key_val) =>
                        {
                            let fully_replaced =
                                matches!(resolved_scope, ResolvedRegistrationScope::All) || rec.scope == resolved_scope;
                            Some((gid.clone(), fully_replaced))
                        }
                        _ => None,
                    })
                    .collect()
            };
            for (old_id, fully_replaced) in superseded {
                if fully_replaced {
                    for (host, cid) in conn.all_host_ctxs() {
                        let _ = host.kernel.release_registration(cid, &old_id).await;
                    }
                    conn.global_registrations.borrow_mut().remove(&old_id);
                } else {
                    for (_, host, cid) in &installed {
                        let _ = host.kernel.release_registration(*cid, &old_id).await;
                    }
                    if let Some(rec) = conn.global_registrations.borrow_mut().by_id.get_mut(&old_id) {
                        for sid in &installed_sids {
                            rec.active_session_instances.remove(sid);
                        }
                    }
                }
            }
        }

        let active_session_instances = installed.into_iter().map(|(sid, _, _)| sid).collect();
        conn.global_registrations.borrow_mut().insert(
            logical_id.clone(),
            GlobalRegistrationRecord {
                kind: GlobalRegistrationKind::Contract {
                    service: service.to_owned(),
                    method: method.to_owned(),
                    validated_arguments: arguments,
                    callback_names,
                    release_on: spec.release_on,
                },
                scope: resolved_scope,
                active_session_instances,
            },
        );
        return Ok(json!({ "registration_id": logical_id }));
    }
    let owner = caller_owner(conn);
    mare_shared::txn_scope::scope_caller_owner(Some(owner), async move {
        let object = arguments.as_object_mut().ok_or_else(|| invalid("arguments must be an object"))?;
        match spec.scope {
            mare_kernel::contracts::Scope::Host => {
                let accepts_expected_epoch = spec
                    .params
                    .get("properties")
                    .and_then(J::as_object)
                    .is_some_and(|props| props.contains_key("expected_epoch"));
                let expected_epoch = if !spec.is_host() && accepts_expected_epoch {
                    match object.remove("expected_epoch") {
                        Some(J::String(s)) if !s.trim().is_empty() => Some(s),
                        Some(_) => return Err(invalid("expected_epoch must be a non-empty string")),
                        None => None,
                    }
                } else {
                    None
                };
                let resolved_target = if let Some(ref exp) = expected_epoch {
                    let named_sid = object
                        .get("session_id")
                        .and_then(J::as_str)
                        .ok_or_else(|| invalid("session_id is required when expected_epoch is set"))?;
                    if named_sid.trim().is_empty() {
                        return Err(invalid("session_id must not be empty"));
                    }
                    match resolve_session_target(conn, named_sid, Some(exp)).await {
                        Ok(target) => Some(target),
                        Err(e) if e.error_code() == Some("unknown_session") => {
                            return Err(failure(
                                "stale_session",
                                "the session changed since expected_epoch was obtained",
                            ));
                        }
                        Err(e) => return Err(e),
                    }
                } else {
                    None
                };
                let cancel = request_cancel();
                let resolved_rt = resolved_target.as_ref().map(|(rt, _)| rt);
                let is_anchor_call = spec.host_anchor || spec.is_host();
                let fallback_named_rt = if !is_anchor_call
                    && resolved_rt.is_none()
                    && named_rt.is_none()
                    && spec_accepts_session_id(&spec)
                    && let Some(sid) = object.get("session_id").and_then(J::as_str).filter(|s| !s.trim().is_empty())
                {
                    find_session_runtime(conn, sid).await.ok()
                } else {
                    None
                };
                let target_rt = if is_anchor_call {
                    None
                } else {
                    resolved_rt.or(named_rt.as_ref()).or(session_rt.as_ref()).or(fallback_named_rt.as_ref())
                };
                let target_host = target_rt.map(|rt| &rt.host).unwrap_or(&anchor);
                authorize_resolved_target(conn, &perm_host, target_rt, target_host, service, method)?;
                require_method_capabilities(target_host, service, method)?;
                // Host-anchored and host-provided calls run on `anchor` with `anchor_ctx`
                // (`sid = ""`) so targeting a non-default session S2 never overwrites
                // `conn.session_ctxs["S2"]`.
                let sid = if is_anchor_call {
                    String::new()
                } else if let Some(rt) = target_rt {
                    rt.session_id.clone()
                } else {
                    conn.rpc
                        .anchor_session()
                        .ok()
                        .filter(|rt| Arc::ptr_eq(&rt.host, target_host))
                        .map(|rt| rt.session_id.clone())
                        .unwrap_or_default()
                };
                let ctx_id = conn.ensure_session_ctx(&sid, target_host).await?;
                if let Some(ref exp) = expected_epoch {
                    let live_matches = match resolved_rt {
                        Some(rt) => find_session_runtime(conn, &rt.session_id)
                            .await
                            .is_ok_and(|cur| Arc::ptr_eq(&cur.host, target_host)),
                        None => true,
                    };
                    let cur_epoch = target_host.kernel.event_log().session_identity().1;
                    if !live_matches || cur_epoch != *exp {
                        return Err(failure("stale_session", "the session changed since expected_epoch was obtained"));
                    }
                }
                let call = invoke_export_on(target_host, ctx_id, service, method, arguments, spec.confined, cancel);
                let scoped = CALL_CONN.scope(Rc::downgrade(conn), call);
                if let Some(rt) = target_rt {
                    CALL_SESSION.scope(rt.session_id.clone(), scoped).await.map_err(error)
                } else {
                    scoped.await.map_err(error)
                }
            }
            mare_kernel::contracts::Scope::Workspace => {
                let cwd = object
                    .remove("cwd")
                    .as_ref()
                    .and_then(J::as_str)
                    .ok_or_else(|| invalid("cwd is required"))?
                    .to_owned();
                let rt = resolve_workspace_host(conn, &cwd)?;
                for other in conn.rpc.active_sessions().into_iter().filter(|other| other.workspace == rt.workspace) {
                    access::authorize(
                        conn,
                        &other.host,
                        access::Op::ContractMeta { service, method, access: spec.access_name(), admin: spec.admin },
                    )
                    .into_workspace_service_error(service, method)?;
                }
                require_method_capabilities(&rt.host, service, method)?;
                let ctx_id = conn.ensure_session_ctx(&rt.session_id, &rt.host).await?;
                CALL_CONN
                    .scope(
                        Rc::downgrade(conn),
                        invoke_export_on(&rt.host, ctx_id, service, method, arguments, spec.confined, request_cancel()),
                    )
                    .await
                    .map_err(error)
            }
            mare_kernel::contracts::Scope::Session => {
                // There is no current session on the wire: the caller names it. The call is bound to
                // that session instance `(id, session_epoch)` and holds it: no switch starts until
                // the call ends, so it never acts on or reports another session.
                if object.contains_key("expected_session") {
                    return Err(invalid("unknown field `expected_session`; use `session_id` and `expected_epoch`"));
                }
                let named = object
                    .remove("session_id")
                    .as_ref()
                    .and_then(J::as_str)
                    .ok_or_else(|| invalid("session_id is required"))?
                    .to_owned();
                let expected_epoch = match object.remove("expected_epoch") {
                    Some(J::String(s)) if !s.trim().is_empty() => Some(s),
                    Some(_) => return Err(invalid("expected_epoch must be a non-empty string")),
                    None => None,
                };
                let (rt, instance) = {
                    let cancel_tok = conn.rpc.manager.session(&named).ok().map(|rt| rt.host.session_cancel_token());
                    mare_shared::cancel::or_cancel_both(
                        resolve_session_target(conn, &named, expected_epoch.as_deref()),
                        &request_cancel(),
                        cancel_tok.as_ref(),
                    )
                    .await
                    .map_err(|_| failure("cancelled", "connection closed"))??
                };
                if conn.output_lost.get() {
                    return Err(proto::RpcError::core(proto::ErrorCode::Internal, "RPC output disconnected"));
                }
                authorize_resolved_target(conn, &perm_host, Some(&rt), &rt.host, service, method)?;
                require_method_capabilities(&rt.host, service, method)?;
                if let Some(J::Array(parts)) = object.get_mut("input") {
                    for part in parts.iter_mut() {
                        let Some(part_obj) = part.as_object_mut() else { continue };
                        if part_obj.get("type").and_then(J::as_str) == Some("image")
                            && part_obj.get("data").and_then(J::as_str).is_none_or(str::is_empty)
                            && let Some(path) = part_obj.get("path").and_then(J::as_str).map(str::to_owned)
                        {
                            if !access::authorize(conn, &rt.host, access::Op::ImagePath).is_allowed() {
                                return Err(invalid(
                                    "peer connections cannot attach image paths; pass inline base64 `data` instead",
                                ));
                            }
                            let encoded = super::dispatch::read_policy_image(conn, &rt.host, &path).await?;
                            part_obj.insert("mime".into(), J::String(encoded.mime.to_string()));
                            part_obj.insert("data".into(), J::String(encoded.data));
                            part_obj.remove("path");
                        }
                    }
                }
                if spec_has_optional_source(&spec) {
                    if !conn.is_owner() {
                        object.insert("source".into(), J::String("peer".into()));
                    } else if object.get("source").is_none_or(J::is_null) {
                        object.insert("source".into(), J::String("rpc".into()));
                    }
                }
                let allow_poisoned = rt.host.kernel.allows_when_poisoned(service, method);
                let ctx_id = conn.ensure_session_ctx(&rt.session_id, &rt.host).await?;
                // Re-verify session identity after async admission / image reads, then hold the session.
                if conn.rpc.attached_host.is_none()
                    && !find_session_runtime(conn, &named).await.is_ok_and(|cur| Arc::ptr_eq(&cur.host, &rt.host))
                {
                    return Err(failure(
                        "stale_session",
                        format!("session `{named}` changed before `{service}.{method}` ran"),
                    ));
                }
                let hold = bind_session_call_on(&rt.host, allow_poisoned)?;
                let before = (instance.id, instance.session_epoch);
                let cur = open_session(&rt.host).await;
                if cur.as_ref() != Some(&before) || rt.host.session_cancel_token().is_cancelled() {
                    drop(hold);
                    return Err(failure(
                        "stale_session",
                        format!("session `{named}` changed before `{service}.{method}` ran"),
                    ));
                }
                let session_cancel = rt.host.session_cancel_token();
                let call =
                    invoke_export_on(&rt.host, ctx_id, service, method, arguments, spec.confined, request_cancel());
                let scoped = CALL_SESSION.scope(named.clone(), CALL_CONN.scope(Rc::downgrade(conn), call));
                let value = match mare_shared::cancel::OrCancelExt::or_cancel(scoped, &session_cancel).await {
                    Ok(res) => res.map_err(error),
                    Err(_) => Err(failure("cancelled", format!("`{service}.{method}` cancelled"))),
                };
                let after = open_session(&rt.host).await;
                drop(hold);
                let value = value?;
                if after.as_ref() != Some(&before) {
                    return Err(failure(
                        "stale_session",
                        format!("session `{named}` changed while `{service}.{method}` ran"),
                    ));
                }
                if spec.release_on == mare_kernel::contracts::ReleaseOn::UiDetach
                    && let Some(reg_id) = value.get("registration_id").and_then(J::as_str)
                {
                    let mut active_session_instances = std::collections::BTreeSet::new();
                    active_session_instances.insert(rt.session_id.clone());
                    conn.global_registrations.borrow_mut().insert(
                        reg_id.to_owned(),
                        GlobalRegistrationRecord {
                            kind: GlobalRegistrationKind::Contract {
                                service: service.to_owned(),
                                method: method.to_owned(),
                                validated_arguments: J::Null,
                                callback_names: Vec::new(),
                                release_on: spec.release_on,
                            },
                            scope: ResolvedRegistrationScope::Session(rt.session_id.clone()),
                            active_session_instances,
                        },
                    );
                }
                Ok(value)
            }
        }
    })
    .await
}

/// Whether `spec` declares an optional string `source` option (injected with `"peer"` or default `"rpc"`).
fn spec_has_optional_source(spec: &mare_kernel::contracts::MethodSpec) -> bool {
    let is_string = spec
        .params
        .get("properties")
        .and_then(|p| p.get("source"))
        .and_then(|s| s.get("type"))
        .is_some_and(|t| t == "string");
    let is_required = spec
        .params
        .get("required")
        .and_then(J::as_array)
        .is_some_and(|req| req.iter().any(|v| v.as_str() == Some("source")));
    is_string && !is_required
}

/// Bind a session-scoped call to `host`'s active session via [`Host::bind_session_call`].
///
/// While the returned [`mare_host::SessionCallGuard`] is held, `kernel.begin_session_switch` and
/// single-host session switches are refused with `busy`. If a session switch is already in
/// progress, binding fails immediately with `stale_session` — even when the session provider does
/// not implement a Lua `hold` helper.
fn bind_session_call_on(
    host: &Host,
    allow_poisoned: bool,
) -> Result<Option<mare_host::SessionCallGuard>, proto::RpcError> {
    let poisoned = mare_binding::exec_gate::poison_reason(host.lua()).is_some();
    if allow_poisoned && poisoned {
        return Ok(None);
    }
    if host.is_session_switching_or_provider(poisoned) {
        return Err(failure("stale_session", "Cannot bind a call to the session while it is switching; retry"));
    }
    host.bind_session_call()
        .map(Some)
        .map_err(|_| failure("stale_session", "Cannot bind a call to the session while it is switching; retry"))
}

tokio::task_local! {
    /// The session a session-scoped call is bound to; reverse callbacks carry it.
    static CALL_SESSION: String;
    /// Active JSON-RPC caller connection for UI shell contract methods.
    pub(super) static CALL_CONN: std::rc::Weak<Conn>;
    /// Request-local subscription activations queued by host session-lifecycle hooks during one RPC call.
    pub(super) static REQUEST_ACTIVATIONS: std::cell::RefCell<Vec<PendingActivation>>;
}

/// The current session instance: its id and in-memory `session_epoch` (rotates on every
/// session create, load, or switch).
pub(super) async fn open_session(host: &Host) -> Option<(String, String)> {
    let identity = SessionIdentity::current(host).await?;
    Some((identity.id, identity.session_epoch))
}

pub(super) async fn handle(conn: &Rc<Conn>, method: &str, params: &Option<J>) -> Result<J, proto::RpcError> {
    let invalid = |message: String| failure("invalid_arguments", message);
    match method {
        methods::SERVICES_LIST => {
            let _: proto::EmptyParams = super::params(params).map_err(invalid)?;
            let host = conn.rpc.ensure_anchor_host_for(conn).await?;
            let mut list = vec![];
            for mut service in host.kernel.service_exports() {
                let name = service["name"].as_str().unwrap_or_default().to_owned();
                if let Some(methods) = service["methods"].as_object_mut() {
                    methods.retain(|method, spec| {
                        access::authorize(
                            conn,
                            &host,
                            access::Op::ContractMeta {
                                service: &name,
                                method,
                                access: spec["access"].as_str().unwrap_or_default(),
                                admin: spec["admin"].as_bool().unwrap_or(false),
                            },
                        )
                        .is_allowed()
                    });
                    if !methods.is_empty() {
                        list.push(service);
                    }
                }
            }
            Ok(json!({ "data": list }))
        }
        methods::SERVICES_REGISTER => {
            let _reg_permit =
                conn.reg_gate.acquire().await.map_err(|_| failure("internal", "registration gate closed"))?;
            let definition: proto::services::RegisterParams = params_req(params).map_err(invalid)?;
            let scope =
                validate_registration_scope(conn, definition.scope, definition.session_id.as_deref(), false).await?;
            if matches!(scope, ResolvedRegistrationScope::All) {
                for rt in resolve_registration_targets(conn, &scope)? {
                    access::authorize(conn, &rt.host, access::Op::Protocol(methods::SERVICES_REGISTER))
                        .into_error(methods::SERVICES_REGISTER)?;
                }
            }
            register(conn, &scope, definition).await.map_err(|err| {
                if err.downcast_ref::<mare_kernel::ServiceCallError>().is_some() {
                    super::failure(err)
                } else {
                    invalid(err.to_string())
                }
            })
        }
        methods::REGISTRATIONS_RELEASE => {
            let _reg_permit =
                conn.reg_gate.acquire().await.map_err(|_| failure("internal", "registration gate closed"))?;
            let request: proto::services::ReleaseParams = params_req(params).map_err(invalid)?;
            conn.global_registrations.borrow_mut().remove(&request.registration_id);
            for (host, ctx_id) in conn.all_host_ctxs() {
                let _ = host.kernel.release_registration(ctx_id, &request.registration_id).await.map_err(error)?;
            }
            Ok(serde_json::to_value(proto::services::ReleaseResult {}).unwrap_or_else(|_| json!({})))
        }
        _ => Err(failure("method_not_found", "unknown service operation")),
    }
}

#[derive(Clone)]
struct RemoteMethod {
    connection: std::rc::Weak<Conn>,
    host: SyncWeak<Host>,
    session_id: String,
    registration: String,
    method: String,
    arguments: mare_shared::schema::Schema,
    /// the parameters' JSON Schema: a Lua caller's empty tables take its shape
    params: J,
    response: mare_shared::schema::Schema,
    cancel: tokio_util::sync::CancellationToken,
    /// session-scoped: every callback names the session it acts on
    session_scoped: bool,
}

impl RemoteMethod {
    async fn invoke(&self, lua: &mlua::Lua, caller: Value, mut arguments: J) -> mlua::Result<Value> {
        use mare_kernel::ServiceCallError as E;
        mare_shared::schema::shape_empty(&mut arguments, &self.params);
        self.arguments.validate(&arguments).map_err(|error| mlua::Error::external(E::Arguments(error)))?;
        let connection = self.connection.upgrade().ok_or_else(|| mlua::Error::external(E::CallbackDisconnected))?;
        let host = self.host.upgrade().ok_or_else(|| mlua::Error::external(E::Unavailable))?;
        host.kernel.check_service_scope(true).map_err(mlua::Error::external)?;
        let (caller_id, caller_cancel) = match &caller {
            Value::Nil => (None, tokio_util::sync::CancellationToken::new()),
            Value::Integer(owner) => {
                let (id, cancel) = host.kernel.service_caller(*owner as u64).map_err(mlua::Error::external)?;
                (Some(id.to_string()), cancel)
            }
            _ => return Err(mlua::Error::external(E::CallerDisposed)),
        };
        if self.cancel.is_cancelled() || !connection.caps.borrow().callbacks {
            return Err(mlua::Error::external(E::Unavailable));
        }
        let cap_rev = connection.cap_revision.get();
        let explicit_session = CALL_SESSION.try_with(Clone::clone).ok();
        let (session_id, turn_id) = if let Some(id) = explicit_session {
            let turn_id = host.state().await.get("turn_id").and_then(J::as_str).map(str::to_string);
            (Some(id), turn_id)
        } else if self.session_scoped {
            let (pre_sid, pre_epoch) = host.kernel.event_log().session_identity();
            let state = host.state().await;
            let (post_sid, post_epoch) = host.kernel.event_log().session_identity();
            if pre_epoch != post_epoch {
                return Err(mlua::Error::external(E::Unavailable));
            }
            let turn_id = state.get("turn_id").and_then(J::as_str).map(str::to_string);
            let sid = post_sid.or(pre_sid).filter(|s| !s.is_empty()).unwrap_or_else(|| self.session_id.clone());
            (Some(sid), turn_id)
        } else {
            let turn_id = host.state().await.get("turn_id").and_then(J::as_str).map(str::to_string);
            (None, turn_id)
        };
        if connection.closed.get() || connection.cap_revision.get() != cap_rev || !connection.caps.borrow().callbacks {
            return Err(mlua::Error::external(E::Unavailable));
        }
        let params = serde_json::to_value(proto::CallbackCallParams {
            registration_id: self.registration.clone(),
            callback: self.method.clone(),
            session_id,
            turn_id,
            tool_call_id: None,
            caller_id,
            arguments,
        })
        .map_err(mlua::Error::external)?;
        let value = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(mlua::Error::external(E::Unavailable)),
            _ = caller_cancel.cancelled() => return Err(mlua::Error::external(E::CallerDisposed)),
            value = connection.call_result_with_updates(methods::CALLBACK_CALL, params, Some(connection.timeout.get()), None) => {
                value.map_err(mlua::Error::external)?
            }
        };
        if caller_cancel.is_cancelled() {
            return Err(mlua::Error::external(E::CallerDisposed));
        }
        if self.cancel.is_cancelled()
            || connection.closed.get()
            || connection.cap_revision.get() != cap_rev
            || !connection.caps.borrow().callbacks
        {
            return Err(mlua::Error::external(E::Unavailable));
        }
        self.response.validate(&json!({"value":value})).map_err(|error| mlua::Error::external(E::Result(error)))?;
        mare_shared::lua::json_to_lua_lossless(lua, &value)
    }
}

/// One remote method as the kernel sees it: its contract entry and its positional mapping.
struct RemoteSpec {
    session_scoped: bool,
    params: J,
    result: J,
    order: Vec<String>,
    rest: bool,
    entry: Option<Table>,
}

/// The wire form of a method: the declared contract's entry when the service already has a
/// contract (the remote implements that interface), else the registrant's own declaration.
fn remote_spec(
    lua: &mlua::Lua,
    declared: Option<&mare_kernel::ServiceContract>,
    service: &str,
    name: &str,
    spec: &proto::services::MethodSpec,
) -> mlua::Result<RemoteSpec> {
    let invalid = |message: String| mlua::Error::runtime(message);
    let size = |value: &Option<J>| {
        value.as_ref().is_some_and(|value| value.to_string().len() > mare_kernel::SERVICE_SCHEMA_MAX_BYTES)
    };
    if size(&spec.parameters) || size(&spec.result) {
        return Err(invalid("service schema exceeds 64 KiB".into()));
    }
    if !mare_kernel::contracts::method_name(name) {
        return Err(invalid(format!("invalid service method `{name}`")));
    }
    if let Some(contract) = declared {
        let method = contract
            .methods
            .get(name)
            .ok_or_else(|| invalid(format!("`{service}.{name}` is not a method of the declared contract")))?;
        let same = |given: &Option<J>, declared: &J| -> mlua::Result<bool> {
            match given {
                Some(given) => mare_shared::schema::try_json_equal(given, declared).map_err(mlua::Error::runtime),
                None => Ok(true),
            }
        };
        let order_matches = spec.order.is_empty() || spec.order == method.order;
        // expose, access and scope belong to the declared contract: given, they must repeat it
        let named = |given: Option<&str>, declared: &str| given.is_none_or(|given| given == declared);
        let model_matches = named(spec.expose.map(proto::services::MethodExpose::as_str), method.expose_name())
            && named(spec.access.map(proto::MethodAccess::as_str), method.access_name())
            && named(spec.scope.map(proto::services::MethodScope::as_str), method.scope_name());
        if !same(&spec.parameters, &method.params)?
            || !same(&spec.result, &method.result)?
            || !order_matches
            || !model_matches
        {
            return Err(invalid(format!(
                "`{service}.{name}` differs from the declared contract (omit parameters, result, order, expose, access and scope, or repeat them exactly)"
            )));
        }
        return Ok(RemoteSpec {
            session_scoped: method.scope == mare_kernel::contracts::Scope::Session,
            params: method.params.clone(),
            result: method.result.clone(),
            order: method.order.clone(),
            rest: method.rest,
            entry: None,
        });
    }
    let parameters =
        spec.parameters.clone().ok_or_else(|| invalid("parameters are required for a new service".into()))?;
    let result = spec.result.clone().ok_or_else(|| invalid("result is required for a new service".into()))?;
    if spec.order.len() > 64 {
        return Err(invalid("invalid service method".into()));
    }
    // Parameters the order does not place travel in the trailing opts table.
    let properties = parameters.get("properties").and_then(J::as_object);
    let rest = !(properties.is_none_or(|properties| properties.keys().all(|key| spec.order.contains(key)))
        && parameters.get("additionalProperties") == Some(&J::Bool(false)));
    let mut order = spec.order.clone();
    if rest {
        order.push(mare_kernel::contracts::REST.into());
    }
    let entry = lua.create_table()?;
    let doc =
        if spec.doc.is_empty() { format!("{service}.{name}, provided by an RPC connection") } else { spec.doc.clone() };
    entry.set("doc", doc)?;
    entry.set("params", mare_shared::lua::json_to_lua_lossless(lua, &parameters)?)?;
    entry.set("result", mare_shared::lua::json_to_lua_lossless(lua, &result)?)?;
    entry.set("order", lua.create_sequence_from(order)?)?;
    // the contract model's own fields and defaults (parsed and checked by the kernel like any
    // contract): access primary, scope host; a registration is exposed to both by default
    entry.set("expose", spec.expose.map_or("both", proto::services::MethodExpose::as_str))?;
    if let Some(access) = spec.access {
        entry.set("access", access.as_str())?;
    }
    if let Some(scope) = spec.scope {
        entry.set("scope", scope.as_str())?;
    }
    entry.set("optional", spec.optional)?;
    Ok(RemoteSpec {
        session_scoped: spec.scope == Some(proto::services::MethodScope::Session),
        params: parameters,
        result,
        order: spec.order.clone(),
        rest,
        entry: Some(entry),
    })
}

/// Can a remote connection provide the whole of an existing contract? Host-provided methods,
/// baseline cuts, registrations, Lua-only parameter/result shapes, lossless history results,
/// sandbox-confined methods, and contracts declaring process-only events cannot cross the wire
/// from a remote service registration.
fn remotely_implementable(service: &str, contract: &mare_kernel::ServiceContract) -> Result<(), String> {
    use mare_kernel::contracts::EventReach;
    type Blocks = fn(&mare_kernel::contracts::MethodSpec) -> bool;
    let blockers: [(&str, Blocks); 7] = [
        ("is host-provided", |spec| spec.is_host()),
        ("is an in-process baseline cut (`baseline = true`)", |spec| spec.baseline),
        ("returns a registration, which cannot cross the wire from a remote service registration", |spec| {
            spec.registration
        }),
        ("takes Lua-only callbacks or `lua` parameters with no wire form", |spec| !spec.callbacks.is_empty()),
        ("uses Lua-only unchecked or multi-value types with no wire form", |spec| {
            spec.lua_unchecked || spec.lua_result.is_some() || spec.lua_kind_errors
        }),
        ("returns a lossless in-process history payload (`history_result = true`)", |spec| spec.history_result),
        ("requires local host sandbox confinement (`confined = true`)", |spec| spec.confined),
    ];
    for (reason, blocks) in blockers {
        if let Some((method, _)) = contract.methods.iter().find(|(_, spec)| blocks(spec)) {
            return Err(format!("`{service}` cannot be provided over RPC: `{method}` {reason}"));
        }
    }
    if let Some((_, ev)) = contract.events.iter().find(|(_, ev)| ev.expose == EventReach::Process) {
        return Err(format!("`{service}` cannot be provided over RPC: event `{}` is process-only", ev.name));
    }
    Ok(())
}

/// A JSON caller's service becomes an ordinary provider: of the declared contract when the
/// service has one, else of the contract it declares. The kernel derives its JSON adapter like
/// any other; Lua callers reach it positionally. Running outside local confinement is a
/// property of this backend, not of the contract.
#[allow(clippy::too_many_arguments)]
async fn install_service_on(
    conn: &Rc<Conn>,
    session_id: &str,
    host: &Arc<Host>,
    ctx_id: u64,
    id: &str,
    cancel: CancellationToken,
    definition: &proto::services::RegisterParams,
) -> mlua::Result<()> {
    let declared = host.kernel.declared_contract(&definition.name);
    if let Some(contract) = &declared {
        remotely_implementable(&definition.name, contract).map_err(mlua::Error::runtime)?;
    }
    let lua = host.lua();
    let implementation = lua.create_table()?;
    let methods = lua.create_table()?;
    for (name, spec) in &definition.methods {
        let wire = remote_spec(lua, declared.as_ref(), &definition.name, name, spec)?;
        let arguments =
            mare_shared::schema::Schema::compile_contract(wire.params.clone()).map_err(mlua::Error::runtime)?;
        let response =
            mare_shared::schema::Schema::compile_contract(mare_kernel::contracts::result_wrapper(&wire.result))
                .map_err(mlua::Error::runtime)?;
        let remote = RemoteMethod {
            connection: Rc::downgrade(conn),
            host: Arc::downgrade(host),
            session_id: session_id.to_owned(),
            registration: id.to_owned(),
            method: name.clone(),
            arguments,
            params: wire.params.clone(),
            response,
            cancel: cancel.clone(),
            session_scoped: wire.session_scoped,
        };
        let (positional, rest) = (wire.order.clone(), wire.rest);
        implementation.set(
            name.as_str(),
            lua.create_async_function(move |lua, (_implementation, caller, args): (Value, Value, MultiValue)| {
                let remote = remote.clone();
                let order = positional.clone();
                async move {
                    if args.len() > order.len() + usize::from(rest) {
                        return Err(mlua::Error::runtime("too many positional service arguments"));
                    }
                    let mut object = serde_json::Map::new();
                    for (index, value) in args.into_iter().enumerate() {
                        if value.is_nil() {
                            continue;
                        }
                        match order.get(index) {
                            Some(key) => {
                                object.insert(key.clone(), mare_shared::lua::strict_lua_to_json(&lua, value)?);
                            }
                            None => match mare_shared::lua::strict_lua_to_json(&lua, value)? {
                                J::Object(options) => {
                                    for (key, value) in options {
                                        if order.contains(&key) || object.contains_key(&key) {
                                            return Err(mlua::Error::runtime(format!(
                                                "service option `{key}` is also a positional parameter"
                                            )));
                                        }
                                        object.insert(key, value);
                                    }
                                }
                                _ => return Err(mlua::Error::runtime("service options must be a table")),
                            },
                        }
                    }
                    remote.invoke(&lua, caller, J::Object(object)).await
                }
            })?,
        )?;
        if let Some(entry) = wire.entry {
            methods.set(name.as_str(), entry)?;
        }
    }
    let options = lua.create_table()?;
    if declared.is_none() {
        let contract = lua.create_table()?;
        contract.set("methods", methods)?;
        options.set("contract", contract)?;
    }
    options.set("unconfined", true)?;
    host.kernel.check_admission(
        ctx_id,
        &definition.name,
        &mlua::Value::Table(implementation.clone()),
        Some(options.clone()),
        false,
    )?;
    let remove =
        super::tree_hooks::provide_on(host, ctx_id, &definition.name, mlua::Value::Table(implementation), options)
            .await?;
    host.kernel
        .insert_connection_registration(ctx_id, Some(id.to_owned()), remove, Some(cancel))
        .map_err(mlua::Error::external)?;
    Ok(())
}

async fn register(
    conn: &Rc<Conn>,
    scope: &ResolvedRegistrationScope,
    definition: proto::services::RegisterParams,
) -> anyhow::Result<J> {
    let invalid = |message: &str| anyhow::anyhow!(message.to_owned());
    if !conn.caps.borrow().callbacks {
        return Err(invalid("callbacks capability is required"));
    }
    if !mare_kernel::contracts::service_name(&definition.name)
        || definition.methods.is_empty()
        || definition.methods.len() > 64
    {
        return Err(invalid("invalid service name or method count"));
    }
    let targets = resolve_registration_targets(conn, scope).map_err(|e| mlua::Error::runtime(e.message()))?;
    let mut target_ctxs = Vec::with_capacity(targets.len());
    for rt in &targets {
        let ctx_id =
            conn.ensure_session_ctx(&rt.session_id, &rt.host).await.map_err(|e| mlua::Error::runtime(e.message()))?;
        let existing = rt.host.kernel.owner_registrations_matching(ctx_id, "service:");
        if existing.len() >= 64 {
            return Err(invalid("service registration limit exceeded"));
        }
        target_ctxs.push((rt.session_id.clone(), rt.host.clone(), ctx_id));
    }
    let n = conn.next_id.get();
    conn.next_id.set(n + 1);
    let id = format!("service:{}:{n}", definition.name);
    let mut installed_sids = std::collections::BTreeSet::new();
    for (idx, (sid, host, ctx_id)) in target_ctxs.iter().enumerate() {
        let cancel = tokio_util::sync::CancellationToken::new();
        if let Err(err) = install_service_on(conn, sid, host, *ctx_id, &id, cancel, &definition).await {
            for (_, prev_host, prev_ctx) in &target_ctxs[..idx] {
                let _ = prev_host.kernel.release_registration(*prev_ctx, &id).await;
            }
            return Err(super::coded(host.lua(), err.into()));
        }
        installed_sids.insert(sid.clone());
    }
    {
        let mut globals = conn.global_registrations.borrow_mut();
        globals.insert(
            id.clone(),
            GlobalRegistrationRecord {
                kind: GlobalRegistrationKind::Service { definition: definition.clone() },
                scope: scope.clone(),
                active_session_instances: installed_sids,
            },
        );
    }
    Ok(json!({"registration_id":id,"name":definition.name}))
}

pub(super) async fn inject_global_contract_on(
    conn: &Rc<Conn>,
    rt: &Rc<SessionRuntime>,
    id: &str,
    service: &str,
    method: &str,
    arguments: &J,
    callback_names: &[String],
) -> Result<(), String> {
    if !callback_names.is_empty() && !conn.caps.borrow().callbacks {
        return Err("this connection does not answer callback.call".into());
    }
    let spec =
        rt.host.kernel.service_method(service, method).ok_or_else(|| format!("unknown method `{service}.{method}`"))?;
    access::authorize(
        conn,
        &rt.host,
        access::Op::ContractMeta { service, method, access: spec.access_name(), admin: spec.admin },
    )
    .into_service_error(service, method)
    .map_err(|e| e.message().to_owned())?;
    let ctx_id = conn.ensure_session_ctx(&rt.session_id, &rt.host).await.map_err(|e| e.message().to_owned())?;
    CALL_SESSION
        .scope(
            rt.session_id.clone(),
            CALL_CONN.scope(
                Rc::downgrade(conn),
                invoke_export_on_with_id(
                    &rt.host,
                    ctx_id,
                    service,
                    method,
                    arguments.clone(),
                    spec.confined,
                    CancellationToken::new(),
                    Some(id.to_owned()),
                ),
            ),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub(super) async fn inject_global_service_on(
    conn: &Rc<Conn>,
    rt: &Rc<SessionRuntime>,
    id: &str,
    def: &proto::services::RegisterParams,
) -> Result<(), String> {
    let ctx_id = conn.ensure_session_ctx(&rt.session_id, &rt.host).await.map_err(|e| e.message().to_owned())?;
    install_service_on(conn, &rt.session_id, &rt.host, ctx_id, id, CancellationToken::new(), def)
        .await
        .map_err(|e| e.to_string())
}

/// How this connection answers contract callbacks: `callback.call` reverse requests carrying
/// `{registration_id, callback, session_id?, turn_id?, tool_call_id?, arguments}` under the connection's
/// callback timeout.
struct ConnCallbacks {
    conn: std::rc::Weak<Conn>,
    session_id: String,
    host: SyncWeak<Host>,
}

impl mare_kernel::CallbackChannel for ConnCallbacks {
    fn call(
        &self,
        registration_id: String,
        callback: String,
        call_id: Option<String>,
        arguments: J,
        update: Option<mpsc::Sender<J>>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<J, mare_kernel::ServiceCallError>>>> {
        let (conn, sid, weak_host) = (self.conn.clone(), self.session_id.clone(), self.host.clone());
        Box::pin(async move {
            let Some(conn) = conn.upgrade() else {
                return Err(mare_kernel::ServiceCallError::CallbackDisconnected);
            };
            if !conn.caps.borrow().callbacks || conn.closed.get() {
                return Err(mare_kernel::ServiceCallError::coded(
                    "unsupported",
                    "this connection no longer answers callback.call",
                ));
            }
            let cap_rev = conn.cap_revision.get();
            let host = weak_host.upgrade().unwrap_or_else(|| conn.host.clone());
            let session_id = match CALL_SESSION.try_with(Clone::clone) {
                Ok(id) => Some(id),
                Err(_) => match open_session(&host).await {
                    Some((id, _)) => Some(id),
                    None if !sid.is_empty() => Some(sid),
                    None => None,
                },
            };
            let turn_id = host.state().await.get("turn_id").and_then(J::as_str).map(str::to_owned);
            if conn.closed.get() || conn.cap_revision.get() != cap_rev || !conn.caps.borrow().callbacks {
                return Err(mare_kernel::ServiceCallError::coded(
                    "unsupported",
                    "this connection no longer answers callback.call",
                ));
            }
            let params = serde_json::to_value(proto::CallbackCallParams {
                registration_id,
                callback,
                session_id,
                turn_id,
                tool_call_id: call_id,
                caller_id: None,
                arguments,
            })
            .map_err(|error| mare_kernel::ServiceCallError::Result(error.to_string()))?;
            let out =
                conn.call_result_with_updates(methods::CALLBACK_CALL, params, Some(conn.timeout.get()), update).await?;
            if conn.closed.get() || conn.cap_revision.get() != cap_rev || !conn.caps.borrow().callbacks {
                return Err(mare_kernel::ServiceCallError::coded(
                    "unsupported",
                    "this connection no longer answers callback.call",
                ));
            }
            Ok(out)
        })
    }
}

/// Forward a `callback.update { id, partial }` client notification to the matching active
/// `callback.call` request (pending request lookup by `RequestId`).
pub(super) fn handle_callback_update(conn: &Conn, params: proto::CallbackUpdateParams) {
    if conn.closed.get() || conn.shutdown.get() || conn.output_lost.get() || !conn.caps.borrow().callbacks {
        return;
    }
    let key = match &params.id {
        RequestId::String(s) => s.clone(),
        RequestId::Number(n) => n.to_string(),
    };
    let tx = {
        let pending = conn.pending.borrow();
        let Some(entry) = pending.get(&key) else {
            return;
        };
        if entry.method != methods::CALLBACK_CALL {
            return;
        }
        let Some(tx) = entry.update_tx.as_ref() else {
            return;
        };
        tx.clone()
    };
    if let Err(err) = tx.try_send(params.partial) {
        tracing::debug!("rpc: dropping callback.update for {key}: {err}");
    }
}

pub(super) fn callback_channel(conn: &Rc<Conn>, session_id: &str) -> Rc<dyn mare_kernel::CallbackChannel> {
    let host = conn
        .rpc
        .manager
        .session(session_id)
        .map(|rt| Arc::downgrade(&rt.host))
        .unwrap_or_else(|_| Arc::downgrade(&conn.host));
    Rc::new(ConnCallbacks { conn: Rc::downgrade(conn), session_id: session_id.to_owned(), host })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_provider_refuses_typed_methods_with_lua_callbacks() {
        let lua = mlua::Lua::new();
        for extra in
            ["callbacks = { sink = 'fun()' }, order = { '...' }", "lua = { sink = 'fun()' }, order = { 'sink' }"]
        {
            let declaration: Value = lua
                .load(format!(
                    "return {{ probe = {{ methods = {{ call = {{ doc = 'call', params = {{type = 'object', properties = {{}}, \
                     additionalProperties = false}}, result = {{type = 'object'}}, {extra} }} }} }} }}"
                ))
                .eval()
                .unwrap();
            let contracts = mare_kernel::contracts::parse_contracts(&lua, declaration).unwrap();
            let contract = &contracts["probe"];
            assert!(!contract.methods["call"].is_host());
            let error = remotely_implementable("probe", contract).expect_err("Lua callbacks have no wire form");
            assert!(error.contains("call"), "{error}");
        }
    }

    #[test]
    fn remote_provider_refuses_host_provided_methods() {
        let lua = mlua::Lua::new();
        let declaration: Value = lua
            .load(
                "return { probe = { methods = { native = { doc = 'host op', expose = 'both', provider = 'host', \
                 params = {type = 'object', properties = {}, additionalProperties = false}, order = {}, \
                 result = {type = 'object'} } } } }",
            )
            .eval()
            .unwrap();
        let contracts = mare_kernel::contracts::parse_contracts(&lua, declaration).unwrap();
        let contract = &contracts["probe"];
        assert!(contract.methods["native"].is_host());
        let error = remotely_implementable("probe", contract).expect_err("host-provided methods cannot be remote");
        assert!(error.contains("native") && error.contains("host-provided"), "{error}");
    }

    #[test]
    fn rpc_bridge_has_no_service_name_branches() {
        let lua = mlua::Lua::new();
        let contracts_tbl: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let mut service_names: std::collections::BTreeSet<String> = contracts_tbl
            .pairs::<String, mlua::Table>()
            .filter_map(|p| {
                let (k, tbl) = p.ok()?;
                tbl.contains_key("methods").ok().filter(|&has| has).map(|_| k)
            })
            .collect();
        service_names.insert("process".into());
        service_names.insert("sandbox".into());

        // Non-service JSON/Lua field or global names in `src/rpc/` whose word collides with a built-in
        // service name. Every entry must remain matched so stale exemptions cannot linger.
        const FIELD_LITERAL_ALLOWLIST: &[(&str, &str, &str)] = &[
            ("forward_approvals.rs", "permissions", r#"t.set("#),
            ("forward_ui.rs", "ui", r#"lua.globals().get::<Table>("mare")?.get("ui")?"#),
            ("forward_ui.rs", "status", r#"broadcast_component(&sid, &host, Some(methods::UI_STATUS), "status""#),
            ("forward_ui.rs", "context", r#"o.get::<Option<String>>("context")"#),
            ("forward_ui.rs", "options", r#"qt.get::<Option<Table>>("options")"#),
            ("mcp.rs", "message", r#"s("message")"#),
            ("mod.rs", "message", r#"value.get::<String>("message")"#),
            ("mod.rs", "webserver", r#"host.kernel.config().get("webserver")"#),
            ("tree_hooks.rs", "context", r#"(rpc, "context", ctx_id)"#),
            ("ui_bridge.rs", "message", r#""message": message.into()"#),
        ];
        let mut allowlist_hits = vec![false; FIELD_LITERAL_ALLOWLIST.len()];

        let rpc_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rpc");
        let mut scanned_lines: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut violations = Vec::new();
        for entry in std::fs::read_dir(&rpc_dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.ends_with("_tests.rs") || name == "rpc_harness.rs" {
                continue;
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            let non_test = access::tests::strip_inline_cfg_test_mod(&raw);
            let lines: Vec<&str> = non_test.lines().collect();
            scanned_lines.insert(name.clone(), lines.len());
            for (idx, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if trimmed.starts_with("//") {
                    continue;
                }
                if trimmed.contains("service == \"")
                    || trimmed.contains("service != \"")
                    || trimmed.contains("withheld_for_non_admin_contract")
                    || trimmed.contains("REMOTE_ADMIN_PROTOCOL_METHODS")
                    || trimmed.contains("_SERVICE_NAME")
                {
                    violations.push(format!("{name}:{}: service-name branch or hardcoded table: {trimmed}", idx + 1));
                }
                if trimmed.contains("service_method(\"") || trimmed.contains("service_provider(\"") {
                    violations.push(format!("{name}:{}: hardcoded service lookup: {trimmed}", idx + 1));
                }
                if trimmed.contains("call_service(")
                    || trimmed.contains("call_service_scoped(")
                    || trimmed.contains("call_service_checked(")
                {
                    let mut rest = trimmed;
                    while let Some(start) = rest.find('"') {
                        let after = &rest[start + 1..];
                        let Some(end) = after.find('"') else { break };
                        let lit = &after[..end];
                        if service_names.contains(lit) {
                            violations.push(format!("{name}:{}: hardcoded service call `{lit}`: {trimmed}", idx + 1));
                        }
                        rest = &after[end + 1..];
                    }
                }
                // Check every non-test file under `src/rpc/` for built-in service name literals.
                let prev_line = idx.checked_sub(1).map(|p| lines[p].trim()).unwrap_or("");
                let mut rest = trimmed;
                while let Some(start) = rest.find('"') {
                    let after = &rest[start + 1..];
                    let Some(end) = after.find('"') else { break };
                    let lit = &after[..end];
                    if service_names.contains(lit) {
                        let allowed = FIELD_LITERAL_ALLOWLIST.iter().enumerate().any(|(i, &(f, l, needle))| {
                            let matched =
                                f == name && l == lit && (trimmed.contains(needle) || prev_line.contains(needle));
                            if matched {
                                allowlist_hits[i] = true;
                            }
                            matched
                        });
                        if !allowed {
                            violations.push(format!(
                                "{name}:{}: built-in service literal `{lit}` in rpc: {trimmed}",
                                idx + 1
                            ));
                        }
                    }
                    rest = &after[end + 1..];
                }
                if name == "mod.rs"
                    && (trimmed.contains("\"baseline.read\"")
                        || trimmed.contains("baseline_target")
                        || trimmed.contains("protocol_method_uses_workspace"))
                {
                    violations.push(format!("{name}:{}: RPC/mod.rs special case: {trimmed}", idx + 1));
                }
                if name == "dispatch.rs"
                    && (trimmed.contains("WORKSPACE_GET") || trimmed.contains("WORKSPACE_SET_TRUST"))
                {
                    violations.push(format!("{name}:{}: hand-written workspace dispatch: {trimmed}", idx + 1));
                }
                if name == "stream_pump.rs" && trimmed.contains("maybe_finish_session_switch") {
                    violations.push(format!("{name}:{}: session switch special case: {trimmed}", idx + 1));
                }
            }
        }
        // Guard against early `#[cfg(test)]` truncation on files with top-of-file test module decls.
        assert!(scanned_lines["mod.rs"] >= 2000, "mod.rs scan truncated: {:?}", scanned_lines.get("mod.rs"));
        assert!(
            scanned_lines["ui_bridge.rs"] >= 450,
            "ui_bridge.rs scan truncated: {:?}",
            scanned_lines.get("ui_bridge.rs")
        );
        assert!(scanned_lines["serve.rs"] >= 700, "serve.rs scan truncated: {:?}", scanned_lines.get("serve.rs"));
        for (i, &(f, l, needle)) in FIELD_LITERAL_ALLOWLIST.iter().enumerate() {
            assert!(allowlist_hits[i], "stale field literal allowlist entry: ({f}, {l}, {needle})");
        }
        assert!(
            violations.is_empty(),
            "RPC/ must not branch on service names or hardcode service literals/tables outside tests:\n{}",
            violations.join("\n")
        );
    }

    #[test]
    fn optional_source_detection_covers_agent_continue() {
        let lua = mlua::Lua::new();
        let val: Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let contracts = mare_kernel::contracts::parse_contracts(&lua, val).unwrap();
        let agent = &contracts["agent"];
        for method in ["prompt", "continue", "steer"] {
            assert!(
                spec_has_optional_source(&agent.methods[method]),
                "agent.{method} must be detected as accepting an optional string source"
            );
        }
        assert!(!spec_has_optional_source(&agent.methods["cancel"]));
        assert!(!spec_has_optional_source(&contracts["session"].methods["get"]));
        assert!(!spec_has_optional_source(&contracts["workspace"].methods["get"]));
    }

    #[test]
    fn agent_contract_declares_cancel_capability_for_turns() {
        let lua = mlua::Lua::new();
        let contracts: mlua::Table = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let agent: mlua::Table = contracts.get("agent").unwrap();
        let caps: mlua::Table = agent
            .get::<Option<mlua::Table>>("capabilities")
            .unwrap()
            .expect("agent contract must declare `capabilities` table");
        let cancel_cap: mlua::Table = caps
            .get::<Option<mlua::Table>>("cancel")
            .unwrap()
            .expect("agent contract must declare `capabilities.cancel`");
        assert_eq!(cancel_cap.get::<String>("method").unwrap(), "cancel");
        let methods_tbl: mlua::Table = agent.get("methods").unwrap();
        for op in ["prompt", "continue"] {
            let m: mlua::Table = methods_tbl.get(op).unwrap();
            let reqs: Vec<String> = m
                .get::<Option<mlua::Table>>("requires_capabilities")
                .unwrap()
                .unwrap_or_else(|| panic!("agent.{op} must declare `requires_capabilities`"))
                .sequence_values::<String>()
                .collect::<mlua::Result<Vec<_>>>()
                .unwrap();
            assert!(reqs.iter().any(|c| c == "cancel"), "agent.{op} must require `cancel` capability: {reqs:?}");
        }
        // Parsed contract metadata preserves capabilities and method requirements, and rejects unknown names.
        let parsed = mare_kernel::contracts::parse_contracts(&lua, Value::Table(contracts)).unwrap();
        assert_eq!(parsed["agent"].capabilities["cancel"].method, "cancel");
        assert_eq!(parsed["agent"].methods["prompt"].requires_capabilities, vec!["cancel"]);
        assert_eq!(parsed["agent"].methods["continue"].requires_capabilities, vec!["cancel"]);
        assert!(parsed["agent"].methods["steer"].requires_capabilities.is_empty());

        let bad_cap: Value = lua
            .load(
                "return { probe = { capabilities = { stop = { method = 'missing', doc = 'stop' } }, \
                 methods = { run = { doc = 'run', expose = 'both', params = { type = 'object', additionalProperties = false }, result = { type = 'object' } } } } }",
            )
            .eval()
            .unwrap();
        assert!(mare_kernel::contracts::parse_contracts(&lua, bad_cap).is_err());

        let bad_req: Value = lua
            .load(
                "return { probe = { methods = { run = { doc = 'run', expose = 'both', requires_capabilities = { 'missing' }, \
                 params = { type = 'object', additionalProperties = false }, result = { type = 'object' } } } } }",
            )
            .eval()
            .unwrap();
        assert!(mare_kernel::contracts::parse_contracts(&lua, bad_req).is_err());
    }

    #[tokio::test]
    async fn contract_capability_enforcement_gates_methods_and_host_cancellation_check() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let _host_guard = crate::HOST_TEST_LOCK.lock().await;
                let home = tempfile::tempdir().unwrap();
                struct EnvRestore(Option<std::ffi::OsString>);
                impl Drop for EnvRestore {
                    fn drop(&mut self) {
                        match &self.0 {
                            Some(v) => unsafe { std::env::set_var("MARE_HOME", v) },
                            None => unsafe { std::env::remove_var("MARE_HOME") },
                        }
                    }
                }
                let _home_guard = EnvRestore(std::env::var_os("MARE_HOME"));
                unsafe { std::env::set_var("MARE_HOME", home.path()) };
                let plugins = home.path().join("config/plugins");
                std::fs::create_dir_all(&plugins).unwrap();
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(
                    plugins.join("cap_probe.lua"),
                    r#"return {
    name = "cap-probe",
    provide = { "cap-worker" },
    contracts = {
        ["cap-worker"] = {
            doc = "Worker with optional cancel capability.",
            capabilities = {
                cancel = { method = "cancel", doc = "Turn cancellation." },
            },
            methods = {
                run = {
                    doc = "Run a session-scoped turn.",
                    expose = "both",
                    access = "peer",
                    scope = "session",
                    requires_capabilities = { "cancel" },
                    params = { type = "object", properties = {}, additionalProperties = false },
                    order = {},
                    result = { type = "object", properties = { ok = { type = "boolean" } }, required = { "ok" }, additionalProperties = false },
                },
                host_run = {
                    doc = "Run a host-scoped op.",
                    expose = "both",
                    access = "peer",
                    scope = "host",
                    requires_capabilities = { "cancel" },
                    params = { type = "object", properties = {}, additionalProperties = false },
                    order = {},
                    result = { type = "object", properties = { ok = { type = "boolean" } }, required = { "ok" }, additionalProperties = false },
                },
                ws_run = {
                    doc = "Run a workspace-scoped op.",
                    expose = "both",
                    access = "peer",
                    scope = "workspace",
                    requires_capabilities = { "cancel" },
                    params = { type = "object", properties = {}, additionalProperties = false },
                    order = {},
                    result = { type = "object", properties = { ok = { type = "boolean" } }, required = { "ok" }, additionalProperties = false },
                },
                cancel = {
                    doc = "Cancel a turn.",
                    expose = "both",
                    scope = "session",
                    optional = true,
                    params = { type = "object", properties = {}, additionalProperties = false },
                    order = {},
                    result = { type = "object", properties = {}, additionalProperties = false },
                },
            },
        },
    },
    apply = function(ctx)
        local Worker = require("cordis").class("Worker", mare.Service)
        function Worker:constructor(c)
            Worker.super.constructor(self, c, "cap-worker")
        end
        function Worker:run() return { ok = true } end
        function Worker:host_run() return { ok = true } end
        function Worker:ws_run() return { ok = true } end
        ctx:plugin(Worker)
    end,
}"#,
                )
                .unwrap();

                let host = Host::new(mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Rpc,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": { "permissions": { "enabled": false } },
                        "peer": { "allow": ["cap-worker.run", "cap-worker.host_run", "cap-worker.ws_run"] }
                    }),
                    load_init: false,
                    project_trusted: true,
                    args: json!({ "no_session": true }),
                    no_user_plugins: false,
                })
                .await
                .unwrap();

                // Built-in agent implements its declared `cancel` capability.
                assert!(host.supports_turn_cancellation());
                assert_eq!(host.kernel.method_missing_capability("agent", "prompt"), None);
                assert_eq!(host.kernel.method_missing_capability("agent", "continue"), None);

                assert!(!host.kernel.service_has_capability("cap-worker", "cancel"));
                assert_eq!(host.kernel.method_missing_capability("cap-worker", "run").as_deref(), Some("cancel"));
                assert_eq!(host.kernel.method_missing_capability("cap-worker", "host_run").as_deref(), Some("cancel"));
                assert_eq!(host.kernel.method_missing_capability("cap-worker", "ws_run").as_deref(), Some("cancel"));

                use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
                let send = async |cw: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
                                  cr: &mut tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
                                  id: i64,
                                  method: &str,
                                  params: J| {
                    let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
                    cw.write_all(format!("{req}\n").as_bytes()).await.unwrap();
                    cw.flush().await.unwrap();
                    loop {
                        let mut line = String::new();
                        cr.read_line(&mut line).await.unwrap();
                        let v: J = serde_json::from_str(&line).unwrap();
                        if v["id"] == id {
                            return v;
                        }
                    }
                };

                let cwd_str = dir.path().display().to_string();
                for is_primary in [false, true] {
                    let (crate::rpc::rpc_harness::TestRpc { server, mut cr, mut cw, .. }, shutdown_tx) =
                        crate::rpc::rpc_harness::TestRpc::connect_with_shutdown(host.clone(), is_primary);
                    let init = send(
                        &mut cw,
                        &mut cr,
                        1,
                        "initialize",
                        json!({ "protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {} }),
                    )
                    .await;
                    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_owned();
                    for (idx, (method, params)) in [
                        ("cap-worker.run", json!({ "session_id": sid })),
                        ("cap-worker.host_run", json!({})),
                        ("cap-worker.ws_run", json!({ "cwd": cwd_str })),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        let res = send(&mut cw, &mut cr, (idx + 2) as i64, method, params).await;
                        assert_eq!(
                            res["error"]["data"]["code"].as_str(),
                            Some("unsupported"),
                            "primary={is_primary} {method} requiring unimplemented capability must return unsupported: {res}"
                        );
                    }
                    let _ = shutdown_tx.send(());
                    let _ = server.await;
                }
                host.shutdown().await;
            })
            .await;
    }
}
