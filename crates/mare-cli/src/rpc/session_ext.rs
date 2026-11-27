//! RPC transport extension for [`mare_host::SessionOrchestrator`].
//!
//! Keeps connection-scoped side-effects (clean `HostOptions` cloning, pre-`app/ready` capability
//! binding & rollback, runtime LLM defaults, `suppressed_subscriptions` / `anchor_session_id`
//! updates, and `session.list` selector resolution) in `mare-cli/src/rpc/` instead of `mare-host`.

use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::Arc;

use mare_host::{
    CallerIdentity, Host, HostOptions, OrchestratorCaller, OrchestratorError, OrchestratorExt, SessionRuntime,
};
use mare_protocol as proto;

use crate::session_addr::{SelectorError, resolve_session};

use super::{Conn, RpcRuntime};

/// Build an [`OrchestratorCaller`] for an RPC wire connection via the single
/// [`OrchestratorCaller::from_access`] constructor (owner-only `unrestricted`, admin-only
/// `allow_path` / uncapped restore).
pub(super) fn caller_for_conn(conn: &Conn, anchor: &Arc<Host>) -> OrchestratorCaller {
    let ctx_id = conn
        .anchor_ctx
        .borrow()
        .as_ref()
        .and_then(|(weak, id)| weak.upgrade().is_some_and(|h| Arc::ptr_eq(&h, anchor)).then_some(*id));
    let receipt_scope = if conn.is_owner() { "primary".to_owned() } else { format!("rpc:peer:{}", conn.id.0) };
    let is_admin = super::access::authorize(conn, anchor, super::access::Op::SessionLoadByPath).is_allowed();
    OrchestratorCaller::from_access(anchor, ctx_id, Some(conn.id), conn.is_owner(), is_admin, receipt_scope)
}

/// Convert an [`OrchestratorError`] into a wire [`proto::RpcError`].
pub(super) fn orchestrator_error_to_rpc(err: OrchestratorError) -> proto::RpcError {
    match err.code.as_str() {
        "invalid_arguments" => proto::RpcError::core(proto::ErrorCode::InvalidArguments, err.message),
        "internal" => proto::RpcError::core(proto::ErrorCode::Internal, err.message),
        _ => proto::RpcError::coded(&err.code, err.message),
    }
}

fn rpc_error_to_orchestrator(err: proto::RpcError) -> OrchestratorError {
    let code = err.error_code().unwrap_or("internal").to_owned();
    OrchestratorError::coded(code, err.message())
}

fn map_selector_err(may_list: bool, e: anyhow::Error) -> OrchestratorError {
    match e.downcast_ref::<SelectorError>() {
        Some(err) => OrchestratorError::invalid_arguments(err.for_caller(may_list)),
        None => rpc_error_to_orchestrator(super::failure(e)),
    }
}

/// [`OrchestratorExt`] implementation wired onto [`RpcRuntime`].
pub(super) struct RpcSessionExt {
    rpc: Weak<RpcRuntime>,
}

impl RpcSessionExt {
    pub(super) fn new(rpc: Weak<RpcRuntime>) -> Self {
        Self { rpc }
    }

    fn resolve_conn(&self, caller: &OrchestratorCaller, host: Option<&Arc<Host>>) -> Option<Rc<Conn>> {
        if let Some(conn) = super::services::CALL_CONN.try_with(|w| w.upgrade()).ok().flatten() {
            return Some(conn);
        }
        let rpc = self.rpc.upgrade()?;
        if let (Some(ctx_id), Some(host)) = (caller.ctx_id(), host)
            && let Some(conn_id) = rpc.conn_id_for_ctx(host, ctx_id)
            && let Some(conn) = rpc.conn_by_id(conn_id)
        {
            return Some(conn);
        }
        let conn_id = caller.conn_id()?;
        rpc.conn_by_id(conn_id)
    }
}

impl OrchestratorExt for RpcSessionExt {
    fn caller_identity(&self, host: &Arc<Host>, ctx_id: u64) -> Option<CallerIdentity> {
        let rpc = self.rpc.upgrade()?;
        let conn_id = rpc.conn_id_for_ctx(host, ctx_id)?;
        let conn = rpc.conn_by_id(conn_id)?;
        let receipt_scope = if conn.is_owner() { "primary".to_owned() } else { format!("rpc:peer:{}", conn.id.0) };
        let is_admin = super::access::authorize(&conn, host, super::access::Op::SessionLoadByPath).is_allowed();
        Some(CallerIdentity { conn_id: Some(conn.id), is_owner: conn.is_owner(), is_admin, receipt_scope })
    }

    fn conn_id_for_ctx(&self, host: &Arc<Host>, ctx_id: u64) -> Option<mare_host::ConnId> {
        let rpc = self.rpc.upgrade()?;
        rpc.conn_id_for_ctx(host, ctx_id)
    }

    fn clean_opts(
        &self,
        cwd_override: Option<&str>,
        no_session_override: Option<bool>,
    ) -> Result<HostOptions, OrchestratorError> {
        if let Some(rpc) = self.rpc.upgrade() {
            return rpc.clone_clean_session_opts(cwd_override, no_session_override).map_err(rpc_error_to_orchestrator);
        }
        Err(OrchestratorError::coded("unavailable", "RPC runtime has shut down"))
    }

    fn before_ready(
        &self,
        caller: &OrchestratorCaller,
        pre_sid: Option<String>,
        host: Arc<Host>,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        let conn = self.resolve_conn(caller, Some(&host));
        Box::pin(async move {
            let Some(conn) = conn else { return };
            if !conn.is_owner() || !conn.is_ready() || conn.closed.get() {
                return;
            }
            let Ok(_permit) = conn.cap_gate.acquire().await else {
                return;
            };
            if !conn.is_ready() || conn.closed.get() {
                return;
            }
            let caps = conn.caps.borrow().clone();
            let sid = pre_sid.or_else(|| host.kernel.event_log().session_identity().0).unwrap_or_default();
            if !sid.is_empty() {
                let _ = super::apply_capabilities_on_session(&conn, &sid, &host, &caps).await;
            }
        })
    }

    fn rollback_before_ready(&self, caller: &OrchestratorCaller, sid: &str, host: &Arc<Host>) {
        if let Some(conn) = self.resolve_conn(caller, Some(host)) {
            conn.remove_session_ctx_if_host(sid, host);
        }
    }

    fn apply_defaults(&self, rt: &Rc<SessionRuntime>) -> Pin<Box<dyn Future<Output = ()>>> {
        let Some(rpc) = self.rpc.upgrade() else {
            return Box::pin(async {});
        };
        let defaults = rpc.runtime_defaults.borrow().clone();
        let rt = Rc::clone(rt);
        Box::pin(async move {
            mare_host::config::options::apply_runtime_defaults(
                &rt.host,
                &rt.session_id,
                defaults.model,
                defaults.effort,
                defaults.options,
            )
            .await;
        })
    }

    fn opened(
        &self,
        caller: &OrchestratorCaller,
        rt: &Rc<SessionRuntime>,
        make_default: bool,
        subscribe: Option<bool>,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        let rpc = self.rpc.upgrade();
        let conn = self.resolve_conn(caller, Some(&rt.host));
        let rt = Rc::clone(rt);
        Box::pin(async move {
            let Some(rpc) = rpc else { return };
            let is_call_conn = conn.as_ref().is_some_and(|c| {
                super::services::CALL_CONN
                    .try_with(|w| w.upgrade().is_some_and(|cur| Rc::ptr_eq(&cur, c)))
                    .unwrap_or(false)
            });
            let in_request = is_call_conn && super::services::REQUEST_ACTIVATIONS.try_with(|_| ()).is_ok();
            let mut deferred_guard = conn
                .as_ref()
                .filter(|_| subscribe != Some(false) && in_request)
                .map(|c| c.defer_subscription(rt.session_id.clone()));
            if let Some(ref conn) = conn {
                if subscribe == Some(false) {
                    conn.suppressed_subscriptions.borrow_mut().insert(rt.session_id.clone());
                } else {
                    conn.suppressed_subscriptions.borrow_mut().remove(&rt.session_id);
                }
            }
            rpc.on_session_opened(&rt, conn.as_ref(), make_default).await;
            let Some(ref conn) = conn else { return };
            if subscribe == Some(false) {
                return;
            }
            if !conn.default_subscribe_all.get() && !conn.subscriptions.borrow().is_empty() {
                return;
            }
            if conn.rpc.attached_host.is_some() && !conn.subscriptions.borrow().is_empty() {
                return;
            }
            if !super::access::authorize(conn, &rt.host, super::access::Op::EventsStream).is_allowed() {
                return;
            }
            match conn.ensure_session_ctx(&rt.session_id, &rt.host).await {
                Ok(ctx_id) => {
                    let (_, cur_epoch) = rt.host.kernel.event_log().session_identity();
                    let filter = conn.default_filter.borrow().clone();
                    match conn.open_subscription_on(
                        &rt.session_id,
                        &rt.host,
                        ctx_id,
                        Some(&cur_epoch),
                        Some(0),
                        false,
                        filter,
                        false,
                        false,
                    ) {
                        Ok((_, _, _, mut activation)) => {
                            if let super::PendingActivation::Cursor { deferred, .. } = &mut activation {
                                *deferred = deferred_guard.take();
                            }
                            let mut slot = Some(activation);
                            let same_call_conn = super::services::CALL_CONN
                                .try_with(|w| w.upgrade().is_some_and(|cur| Rc::ptr_eq(&cur, conn)))
                                .unwrap_or(false);
                            let queued = same_call_conn
                                && super::services::REQUEST_ACTIVATIONS
                                    .try_with(|list| {
                                        if let Some(act) = slot.take() {
                                            list.borrow_mut().push(act);
                                        }
                                    })
                                    .is_ok();
                            if !queued && let Some(act) = slot.take() {
                                conn.activate_subscriptions(vec![act]);
                            }
                        }
                        Err(e) => tracing::warn!("baseline subscribe on session switch failed: {}", e.message()),
                    }
                }
                Err(e) => tracing::warn!("baseline subscribe session ctx init failed: {}", e.message()),
            }
        })
    }

    fn closed(&self, session_id: &str) {
        if let Some(rpc) = self.rpc.upgrade() {
            rpc.on_session_closed(session_id);
        }
    }

    fn resolve_selector(
        &self,
        anchor: &Arc<Host>,
        caller: &OrchestratorCaller,
        selector: &str,
        include_archived: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, OrchestratorError>>>> {
        let anchor = Arc::clone(anchor);
        let selector = selector.to_owned();
        let allow_path = caller.allow_path();
        let may_list = caller.may_list();
        Box::pin(async move {
            resolve_session(&anchor, &selector, allow_path, include_archived)
                .await
                .map_err(|e| map_selector_err(may_list, e))
        })
    }
}
