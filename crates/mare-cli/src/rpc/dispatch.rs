//! Host → mare requests: one `handle` match over every method, delegating input media,
//! prompt admission, and stream subscriptions to dedicated submodules.

use super::access::{has_method, protocol_methods};
use super::inbound::inbound_message;
use super::options::{config_describe, options_get, options_set};
use super::stream_pump::handle_events_subscribe;
use super::*;

pub(super) fn rpc_err(code: proto::ErrorCode, message: impl Into<String>) -> proto::RpcError {
    proto::RpcError::core(code, message)
}

pub(super) async fn read_policy_image(
    conn: &Conn,
    host: &Host,
    raw_path: &str,
) -> Result<mare_binding::image::Encoded, proto::RpcError> {
    if !access::authorize(conn, host, access::Op::ImagePath).is_allowed() {
        return Err(rpc_err(
            proto::ErrorCode::InvalidArguments,
            "peer connections cannot attach image paths; pass inline base64 `data` instead",
        ));
    }
    host.read_policy_image(raw_path).await.map_err(|e| match e {
        mare_host::PolicyImageError::InvalidArguments(msg) => rpc_err(proto::ErrorCode::InvalidArguments, msg),
        mare_host::PolicyImageError::Internal(msg) => rpc_err(proto::ErrorCode::Internal, msg),
    })
}

/// Handle one host → mare request and return any stream cursor activations to install after sending the reply.
pub(super) async fn handle_with_activations(
    conn: &Rc<Conn>,
    method: &str,
    p: &Option<J>,
) -> (Result<J, proto::RpcError>, Vec<PendingActivation>) {
    let mut activations = Vec::new();
    let res = handle(conn, method, p, &mut activations).await;
    (res, activations)
}

/// Handle one host → mare request. Errors become JSON-RPC errors.
async fn handle(
    conn: &Rc<Conn>,
    method: &str,
    p: &Option<J>,
    activations: &mut Vec<PendingActivation>,
) -> Result<J, proto::RpcError> {
    let anchor = if conn.rpc.attached_host.is_some() {
        conn.rpc.anchor_host().unwrap_or_else(|| conn.host.clone())
    } else if conn.is_owner() {
        match conn.rpc.anchor_host().filter(|h| h.kernel.has_service_exports()) {
            Some(h) => h,
            None if method != methods::SHUTDOWN => conn.rpc.ensure_live_anchor_host().await?,
            None => conn.rpc.anchor_host().unwrap_or_else(|| conn.host.clone()),
        }
    } else {
        match conn.rpc.anchor_host_for(conn) {
            Some(h) => h,
            None => conn.rpc.ensure_project_anchor_host().await?,
        }
    };
    let internal = super::failure;
    let invalid = |e: String| rpc_err(proto::ErrorCode::InvalidArguments, e);
    // Domain operations are derived from service contracts; the protocol layer knows no names.
    if !has_method(method)
        && let Some((service, operation)) = method.split_once('.')
        && resolve_contract_target(conn, &anchor, service, operation, p.as_ref().and_then(J::as_object)).spec.is_some()
    {
        if !conn.is_ready() {
            return Err(rpc_err(
                proto::ErrorCode::InvalidRequest,
                "connection not initialized; call `initialize` first",
            ));
        }
        let arguments = p.clone().unwrap_or_else(|| json!({}));
        let (result, pending) = super::services::REQUEST_ACTIVATIONS
            .scope(RefCell::new(Vec::new()), async {
                let res = super::services::call(conn, service, operation, arguments).await;
                let pending = super::services::REQUEST_ACTIVATIONS.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
                (res, pending)
            })
            .await;
        let value = result?;
        activations.extend(pending);
        return Ok(value);
    }
    // peer socket or restricted remote_admin=false owner
    let target_host = request_target_host(conn, method, p);
    let decision = access::authorize(conn, &target_host, access::Op::Protocol(method));
    access::audit_remote_protocol(conn, &target_host, method, decision);
    decision.into_protocol_error(method)?;
    if !has_method(method) {
        return Err(rpc_err(proto::ErrorCode::MethodNotFound, format!("unknown method `{method}`")));
    }
    if !conn.is_ready() && !matches!(method, methods::INITIALIZE | methods::SHUTDOWN) {
        return Err(rpc_err(proto::ErrorCode::InvalidRequest, "connection not initialized; call `initialize` first"));
    }
    match method {
        methods::SERVICES_LIST | methods::SERVICES_REGISTER | methods::REGISTRATIONS_RELEASE => {
            super::services::handle(conn, method, p).await
        }
        methods::INITIALIZE => {
            if conn.handshake.get() != HandshakeState::Uninitialized {
                return Err(rpc_err(proto::ErrorCode::InvalidRequest, "connection already initialized"));
            }
            let ip: proto::InitializeParams = params_req(p).map_err(invalid)?;
            if !proto::compatible(&ip.protocol_version) {
                return Err(proto::RpcError::coded(
                    "unsupported_version",
                    format!(
                        "protocol {} is not supported (server speaks {})",
                        ip.protocol_version,
                        proto::PROTOCOL_VERSION
                    ),
                ));
            }
            let permit = conn
                .cap_gate
                .acquire()
                .await
                .map_err(|_| rpc_err(proto::ErrorCode::InvalidRequest, "connection closed"))?;
            if conn.closed.get() {
                return Err(rpc_err(proto::ErrorCode::InvalidRequest, "connection closed"));
            }
            if conn.handshake.get() != HandshakeState::Uninitialized {
                return Err(rpc_err(proto::ErrorCode::InvalidRequest, "connection already initialized"));
            }
            struct InitGuard<'a> {
                conn: &'a Conn,
                armed: bool,
            }
            impl Drop for InitGuard<'_> {
                fn drop(&mut self) {
                    if self.armed {
                        self.conn.handshake.set(HandshakeState::Uninitialized);
                    }
                }
            }
            conn.handshake.set(HandshakeState::Initializing);
            let mut guard = InitGuard { conn, armed: true };
            let prev_timeout = conn.timeout.get();
            let prev_approval_timeout = conn.approval_timeout.get();
            conn.timeout.set(Duration::from_millis(
                ip.callback_timeout_ms
                    .unwrap_or(proto::DEFAULT_CALLBACK_TIMEOUT_MS)
                    .clamp(1, proto::MAX_CALLBACK_TIMEOUT_MS),
            ));
            conn.approval_timeout.set(ip.approval_timeout_ms.map(Duration::from_millis));
            let effective_caps = match apply_connection_capabilities(conn, ip.capabilities).await {
                Ok(c) => c,
                Err(e) => {
                    conn.timeout.set(prev_timeout);
                    conn.approval_timeout.set(prev_approval_timeout);
                    return Err(e);
                }
            };
            if conn.closed.get() {
                conn.timeout.set(prev_timeout);
                conn.approval_timeout.set(prev_approval_timeout);
                return Err(rpc_err(proto::ErrorCode::InvalidRequest, "connection closed"));
            }
            guard.armed = false;
            conn.handshake.set(HandshakeState::Ready);
            drop(permit);

            let session = if let Ok(rt) = conn.rpc.anchor_session_for(conn) {
                rt.host.session_snapshot(&rt.session_id).await.ok().filter(J::is_object)
            } else {
                None
            };
            let mut methods: Vec<String> = protocol_methods()
                .filter(|m| access::authorize(conn, &anchor, access::Op::Protocol(m)).is_allowed())
                .map(|s| s.to_string())
                .collect();
            methods.extend(super::services::operations(conn, &anchor));
            let r = proto::InitializeResult {
                protocol_version: proto::PROTOCOL_VERSION.into(),
                server: proto::ServerInfo { name: "mare".into(), version: env!("CARGO_PKG_VERSION").into() },
                capabilities: proto::ServerCapabilities { methods, callbacks: effective_caps },
                session,
            };
            Ok(serde_json::to_value(r).unwrap_or(J::Null))
        }
        methods::CONNECTION_SET_CAPABILITIES => {
            let sp: proto::SetCapabilitiesParams = params_req(p).map_err(invalid)?;
            let _permit = conn
                .cap_gate
                .acquire()
                .await
                .map_err(|_| rpc_err(proto::ErrorCode::InvalidRequest, "connection closed"))?;
            if conn.closed.get() {
                return Err(rpc_err(proto::ErrorCode::InvalidRequest, "connection closed"));
            }
            let capabilities = apply_connection_capabilities(conn, sp.capabilities.into()).await?;
            Ok(serde_json::to_value(proto::SetCapabilitiesResult { capabilities }).unwrap_or(J::Null))
        }
        methods::OPTIONS_GET => {
            let gp: proto::OptionsGetParams = params_req(p).map_err(invalid)?;
            let (rt, _) = resolve_session_target(conn, &gp.session_id, None).await?;
            if !Arc::ptr_eq(&rt.host, &target_host) {
                let d = access::authorize(conn, &rt.host, access::Op::Protocol(method));
                access::audit_remote_protocol(conn, &rt.host, method, d);
                d.into_protocol_error(method)?;
            }
            options_get(&rt.host, gp.keys, gp.describe).await.map_err(internal)
        }
        methods::OPTIONS_SET => {
            let sp: proto::OptionsSetParams = params_req(p).map_err(invalid)?;
            let (rt, _) = resolve_session_target(conn, &sp.session_id, None).await?;
            if !Arc::ptr_eq(&rt.host, &target_host) {
                let d = access::authorize(conn, &rt.host, access::Op::Protocol(method));
                access::audit_remote_protocol(conn, &rt.host, method, d);
                d.into_protocol_error(method)?;
            }
            // identity: only owner writes on the anchor session propagate into runtime defaults
            let sync_base = conn.is_owner()
                && conn.rpc.anchor_session_id.borrow().as_deref().is_none_or(|d| d == rt.session_id.as_str());
            options_set(conn, &rt, sp.values, sync_base).await?;
            Ok(ok())
        }
        methods::CONFIG_DESCRIBE => {
            let dp: proto::ConfigDescribeParams = params(p).map_err(invalid)?;
            config_describe(&anchor, dp.keys).await.map_err(internal)
        }
        methods::STATE_SNAPSHOT => serde_json::to_value(
            mare_binding::state_namespace::snapshot(anchor.lua(), p.clone().unwrap_or(J::Null)).await,
        )
        .map_err(|e| internal(e.into())),
        methods::STATE_COMPARE_EXCHANGE => serde_json::to_value(
            mare_binding::state_namespace::compare_exchange(anchor.lua(), p.clone().unwrap_or(J::Null), None).await,
        )
        .map_err(|e| internal(e.into())),
        methods::SHUTDOWN => {
            conn.cancel_pending(None, "shutdown");
            conn.shutdown_requested.set(true);
            Ok(ok())
        }
        methods::EVENTS_LIST => {
            let _: proto::EmptyParams = params(p).map_err(invalid)?;
            Ok(serde_json::to_value(proto::EventsListResult { data: anchor.kernel.list_events() }).unwrap_or(J::Null))
        }
        methods::EVENTS_SUBSCRIBE => {
            let ep: proto::EventsSubscribeParams = params(p).map_err(invalid)?;
            if let Some(epoch) = ep.session_epoch.as_deref()
                && epoch.trim().is_empty()
            {
                return Err(invalid("session_epoch must not be empty".into()));
            }
            let res = handle_events_subscribe(conn, ep, activations).await?;
            Ok(serde_json::to_value(res).unwrap_or_else(|_| ok()))
        }
        methods::BASELINE_READ => {
            let bp: proto::BaselineReadParams = params_req(p).map_err(invalid)?;
            if bp.baseline_id.is_empty() || bp.part.is_empty() {
                return Err(invalid("baseline_id and part must be non-empty strings".into()));
            }
            if bp.limit == Some(0) {
                return Err(invalid("limit must be an integer >= 1".into()));
            }
            let Some((host, owner_ctx)) = conn.baseline_host(&bp.baseline_id) else {
                return Err(proto::RpcError::coded(
                    "baseline.stale",
                    "baseline handle is stale; re-subscribe to obtain a fresh baseline",
                ));
            };
            let res = host
                .kernel
                .read_baseline_page(owner_ctx, &bp.baseline_id, &bp.part, bp.cursor.as_deref(), bp.limit)
                .map_err(super::services::error)?;
            Ok(serde_json::to_value(res).unwrap_or(J::Null))
        }
        methods::PEER_INFO => {
            let me = mare_binding::peers::self_entry().unwrap_or(J::Null);
            let info = proto::PeerInfo {
                name: me.get("name").and_then(J::as_str).unwrap_or("mare").to_string(),
                pid: std::process::id(),
                cwd: anchor.cwd.to_string_lossy().into_owned(),
                version: env!("CARGO_PKG_VERSION").into(),
                allowed: protocol_methods()
                    .filter(|m| access::authorize(conn, &anchor, access::Op::Protocol(m)).is_allowed())
                    .map(|s| s.to_string())
                    .chain(super::services::operations(conn, &anchor))
                    .collect(),
                delivery_modes: vec!["trigger_turn".into(), "queue_only".into()],
            };
            Ok(serde_json::to_value(info).unwrap_or(J::Null))
        }
        methods::PEER_MESSAGE => {
            let mp: proto::PeerMessageParams = params_req(p).map_err(invalid)?;
            let status =
                inbound_message(conn, mp).await.map_err(|(code, message)| proto::RpcError::coded(code, message))?;
            Ok(serde_json::to_value(status).unwrap_or(J::Null))
        }
        methods::FLAGS_LIST => Ok(super::packages::flags_list(&anchor)),
        methods::FLAGS_GET => super::packages::flags_get(&anchor, p),
        methods::UI_ATTACH => ui_bridge::handle_ui_attach(conn, p).await,
        methods::UI_DETACH => ui_bridge::handle_ui_detach(conn, p).await,
        other => Err(rpc_err(proto::ErrorCode::MethodNotFound, format!("unknown method `{other}`"))),
    }
}

#[cfg(test)]
#[path = "dispatch_tests.rs"]
mod tests;
