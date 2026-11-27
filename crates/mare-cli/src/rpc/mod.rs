//! Bidirectional RPC: JSON-RPC 2.0, one message per line on
//! stdin/stdout. Types and schema live in `mare-protocol`.
//!
//! * host → mare requests are handled concurrently (each in its own local task), so a long
//!   `prompt {wait: true}` never blocks the responses to mare's own callbacks;
//! * mare → host callbacks (`callback.call`, `ui.*`) are JSON-RPC requests with ids
//!   `mare-<n>`; ordinary callbacks time out after `callback_timeout_ms` (default 30s, max 600s).
//!   Human approvals have no deadline unless `approval_timeout_ms` is explicitly set;
//! * everything the host registers (tools, hooks, UI forwarding) belongs to one connection ctx
//!   and is disposed when the connection closes;
//! * stdout carries protocol messages only — logs go to the log file.
//!
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Weak as SyncWeak};
use std::time::Duration;

use anyhow::Result;
use mlua::{Function, MultiValue, Table, Value};
use serde_json::{Value as J, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use mare_host::{Host, HostManager, HostOptions, SessionError, SessionRuntime};
#[cfg(test)]
use mare_protocol::codes;
use mare_protocol::{self as proto, Message, RequestId, methods};
use mare_shared::lua::{json_to_lua, lua_to_json};

mod access;
mod dispatch;
mod envelope;
mod forward_approvals;
mod forward_ui;
mod inbound;
mod mcp;
mod options;
mod packages;
#[cfg(test)]
#[path = "rpc_harness_tests.rs"]
pub(in crate::rpc) mod rpc_harness;
pub(crate) mod serve;
mod services;
mod session_ext;
mod stream_pump;
mod tree_hooks;
mod ui_bridge;
#[cfg(test)]
mod wait_probe;

pub use serve::{run, run_manager, serve, serve_manager, serve_peer, serve_peer_manager};

/// `mare rpc schema`: the protocol layer (Rust types) plus every remote method derived from the
/// built-in service contracts. No provider runs; the contracts are data.
/// The RPC methods derived from the built-in service contracts.
fn contract_methods() -> Result<std::collections::BTreeMap<String, J>> {
    Ok(mare_kernel::contracts::rpc_methods_from_contracts(mare_plugin::builtin_service_contracts()?.as_ref()))
}

/// The static event catalog (`PROTOCOL_EVENTS` merged with built-in contract `events`).
fn static_events() -> Result<std::collections::BTreeMap<String, J>> {
    Ok(mare_kernel::contracts::rpc_events_from_contracts(mare_plugin::builtin_service_contracts()?.as_ref())?)
}

pub fn schema_text() -> Result<String> {
    let mut catalog = proto::schema();
    let methods = catalog["methods"].as_object_mut().ok_or_else(|| anyhow::anyhow!("schema has no methods"))?;
    for (name, entry) in contract_methods()? {
        anyhow::ensure!(!methods.contains_key(&name), "contract method `{name}` collides with a protocol method");
        methods.insert(name, entry);
    }
    let events: serde_json::Map<String, J> = static_events()?.into_iter().collect();
    catalog["events"] = J::Object(events);
    let mut text = serde_json::to_string_pretty(&catalog)?;
    text.push('\n');
    Ok(text)
}

tokio::task_local! {
    static REQUEST_CANCEL: tokio_util::sync::CancellationToken;
}

pub(super) fn request_cancel() -> tokio_util::sync::CancellationToken {
    REQUEST_CANCEL.try_with(Clone::clone).unwrap_or_default()
}

struct PendingRequest {
    method: String,
    session_id: Option<String>,
    params: J,
    update_tx: Option<mpsc::Sender<J>>,
    reply: oneshot::Sender<Result<J, proto::RpcError>>,
}
type Pending = RefCell<HashMap<String, PendingRequest>>;

struct RequestGuard {
    connection: Rc<Conn>,
    id: RequestId,
}
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.connection.requests.borrow_mut().remove(&self.id);
        let remaining = self.connection.inflight.get().saturating_sub(1);
        self.connection.inflight.set(remaining);
        if remaining == 0 {
            self.connection.inflight_notify.notify_waiters();
        }
    }
}

/// A dropped callback future must not leave a stale request at the host.
struct PendingGuard<'a> {
    conn: &'a Conn,
    id: String,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.conn.pending.borrow_mut().remove(&self.id).is_some() && !self.conn.closed.get() {
            self.conn.notify(methods::CANCEL_REQUEST, json!({ "id": self.id }));
        }
    }
}

/// Caller privilege principal for an RPC connection, separated from UI/approval answering
/// (`mare_host::AnswererSlot`) and process shutdown ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Principal {
    Owner { remote_admin: bool },
    Peer,
}

impl Principal {
    pub(super) fn is_owner(self) -> bool {
        matches!(self, Self::Owner { .. })
    }

    pub(super) fn is_remote_admin(self) -> bool {
        matches!(self, Self::Owner { remote_admin: true })
    }
}

/// Runtime option defaults applied when spawning brand-new sessions via `session.create`
/// without polluting CLI-layer `base_opts.config` (`options.set` must not elevate runtime
/// writes into the `cli` config layer).
#[derive(Clone, Debug, Default)]
pub(super) struct RuntimeDefaults {
    pub model: Option<J>,
    pub effort: Option<J>,
    pub options: serde_json::Map<String, J>,
}

/// Resolve project trust for a newly spawned RPC sibling session via [`mare_host::resolve_workspace_trust`].
fn sync_trust_args(
    args: &mut J,
    target_cwd: &std::path::Path,
    project_trusted: bool,
    anchor_cli_trust: Option<bool>,
    anchor_load_init: bool,
    no_user_plugins: bool,
) {
    if !args.is_object() {
        *args = json!({});
    }
    let Some(map) = args.as_object_mut() else { return };
    if no_user_plugins {
        map.remove("trust_source");
        map.insert("trust".to_string(), J::String("untrusted".to_string()));
        return;
    }
    if let Some(cli_trust) = anchor_cli_trust {
        map.insert("trust_source".to_string(), J::String("cli".to_string()));
        map.insert("trust".to_string(), J::String(if cli_trust { "trusted" } else { "untrusted" }.to_string()));
        return;
    }
    map.remove("trust_source");
    let state = if project_trusted {
        "trusted"
    } else if !anchor_load_init {
        "untrusted"
    } else {
        match mare_config::trust::lookup_checked(target_cwd) {
            Ok(Some(false)) | Err(_) => "untrusted",
            Ok(Some(true)) => "trusted",
            Ok(None) => "undecided",
        }
    };
    map.insert("trust".to_string(), J::String(state.to_string()));
}

/// Process-level multi-session RPC coordinator holding [`HostManager`], active connections, and
/// `Scope::All` injection notifications across all loaded [`SessionRuntime`]s.
pub struct RpcRuntime {
    pub manager: Rc<HostManager>,
    pub(super) base_opts: RefCell<HostOptions>,
    anchor_cwd: std::path::PathBuf,
    anchor_startup_trusted: bool,
    anchor_cli_trust: Option<bool>,
    pub(super) runtime_defaults: RefCell<RuntimeDefaults>,
    pub(super) fallback_host: RefCell<Option<Arc<Host>>>,
    pub(super) anchor_session_id: RefCell<Option<String>>,
    connections: RefCell<HashMap<mare_host::ConnId, Weak<Conn>>>,
    pub(super) event_wake: tokio::sync::watch::Sender<u64>,
    pub(super) shutdown_on_drop: bool,
    pub(super) attached_host: Option<Arc<Host>>,
    peer_cleanup: RefCell<Option<Box<dyn FnOnce()>>>,
    ext_layer_id: Cell<Option<u64>>,
    rekey_hook_id: Cell<Option<u64>>,
}

impl Drop for RpcRuntime {
    fn drop(&mut self) {
        if let Some(id) = self.ext_layer_id.take() {
            self.manager.orchestrator().remove_ext(id);
        }
        if let Some(id) = self.rekey_hook_id.take() {
            self.manager.remove_session_rekey(id);
        }
        if let Some(cleanup) = self.peer_cleanup.borrow_mut().take() {
            cleanup();
        }
    }
}

impl RpcRuntime {
    pub fn new(
        manager: Rc<HostManager>,
        base_opts: HostOptions,
        anchor_host: Arc<Host>,
        anchor_session_id: Option<String>,
        shutdown_on_drop: bool,
    ) -> Rc<Self> {
        let (event_wake, _) = tokio::sync::watch::channel(0);
        let anchor_cwd = dunce::canonicalize(&anchor_host.cwd).unwrap_or_else(|_| anchor_host.cwd.clone());
        let anchor_startup_trusted = anchor_host.startup_project_trusted();
        let anchor_cli_trust = anchor_host.startup_cli_trust();
        anchor_host.set_orchestrator(manager.orchestrator().clone());
        let rpc = Rc::new(Self {
            manager,
            base_opts: RefCell::new(base_opts),
            anchor_cwd,
            anchor_startup_trusted,
            anchor_cli_trust,
            runtime_defaults: RefCell::new(RuntimeDefaults::default()),
            fallback_host: RefCell::new(Some(anchor_host)),
            anchor_session_id: RefCell::new(anchor_session_id),
            connections: RefCell::new(HashMap::new()),
            event_wake,
            shutdown_on_drop,
            attached_host: None,
            peer_cleanup: RefCell::new(None),
            ext_layer_id: Cell::new(None),
            rekey_hook_id: Cell::new(None),
        });
        rpc.manager.orchestrator().set_project_cwd(&rpc.base_opts.borrow().cwd);
        rpc.manager.orchestrator().set_ext(Rc::new(session_ext::RpcSessionExt::new(Rc::downgrade(&rpc))));
        Self::attach_rekey_hook(&rpc);
        rpc
    }

    fn attach_rekey_hook(rpc: &Rc<Self>) {
        let weak = Rc::downgrade(rpc);
        let hook_id = rpc.manager.on_session_rekey(move |old_sid, new_sid, new_rt| {
            let Some(rpc) = weak.upgrade() else { return };
            if rpc.anchor_session_id.borrow().as_deref() == Some(old_sid) {
                *rpc.anchor_session_id.borrow_mut() = Some(new_sid.to_owned());
            }
            let conns: Vec<Rc<Conn>> = rpc.connections.borrow().values().filter_map(Weak::upgrade).collect();
            for conn in conns {
                conn.rekey_session(old_sid, new_sid, &new_rt.host);
            }
            rpc.wake_events();
        });
        rpc.rekey_hook_id.set(Some(hook_id));
    }

    pub(crate) fn set_peer_cleanup(&self, cleanup: Box<dyn FnOnce()>) {
        if let Some(prev) = self.peer_cleanup.borrow_mut().replace(cleanup) {
            prev();
        }
    }

    pub(super) async fn from_host(host: &Arc<Host>) -> Rc<Self> {
        let manager = HostManager::new_single(host);
        let (ev_id, _) = host.kernel.event_log().session_identity();
        let ephemeral = ev_id.is_none()
            || host
                .lua()
                .globals()
                .get::<Table>("mare")
                .and_then(|m| m.get::<Option<Table>>("args"))
                .ok()
                .flatten()
                .and_then(|a| a.get::<Option<bool>>("no_session").ok().flatten())
                .unwrap_or(false);
        let sid =
            if let Ok(rt) = manager.register_host_keeping_capacity(host.clone(), ev_id.as_deref(), ephemeral).await {
                Some(rt.session_id.clone())
            } else {
                ev_id
            };
        let mut args = json!({});
        if ephemeral {
            args["no_session"] = J::Bool(true);
        }
        let anchor_cwd = dunce::canonicalize(&host.cwd).unwrap_or_else(|_| host.cwd.clone());
        let anchor_startup_trusted = host.startup_project_trusted();
        let anchor_cli_trust = host.startup_cli_trust();
        let anchor_load_init = host.load_init();
        let no_user_plugins = host.no_user_plugins();
        let initial_trusted = mare_host::resolve_workspace_trust(
            &anchor_cwd,
            true,
            anchor_startup_trusted,
            anchor_cli_trust,
            no_user_plugins,
        );
        sync_trust_args(&mut args, &anchor_cwd, initial_trusted, anchor_cli_trust, anchor_load_init, no_user_plugins);
        let base_opts = HostOptions {
            cwd: host.cwd.clone(),
            mode: host.mode,
            config: host.startup_config(),
            load_init: anchor_load_init,
            project_trusted: initial_trusted,
            args,
            no_user_plugins: host.no_user_plugins(),
        };
        let (event_wake, _) = tokio::sync::watch::channel(0);
        let rpc = Rc::new(Self {
            manager,
            base_opts: RefCell::new(base_opts),
            anchor_cwd,
            anchor_startup_trusted,
            anchor_cli_trust,
            runtime_defaults: RefCell::new(RuntimeDefaults::default()),
            fallback_host: RefCell::new(Some(host.clone())),
            anchor_session_id: RefCell::new(sid),
            connections: RefCell::new(HashMap::new()),
            event_wake,
            shutdown_on_drop: false,
            attached_host: Some(host.clone()),
            peer_cleanup: RefCell::new(None),
            ext_layer_id: Cell::new(None),
            rekey_hook_id: Cell::new(None),
        });
        rpc.manager.orchestrator().set_project_cwd(&rpc.base_opts.borrow().cwd);
        let layer_id =
            rpc.manager.orchestrator().push_ext(Rc::new(session_ext::RpcSessionExt::new(Rc::downgrade(&rpc))));
        rpc.ext_layer_id.set(Some(layer_id));
        Self::attach_rekey_hook(&rpc);
        rpc
    }

    pub(super) fn active_sessions(&self) -> Vec<Rc<SessionRuntime>> {
        let mut ids = self.manager.active_ids();
        ids.sort();
        ids.into_iter().filter_map(|id| self.manager.session(&id).ok()).collect()
    }

    /// The process's anchor session runtime (used only for non-target anchor metadata, never as an
    /// implicit request target when `session_id` is omitted).
    pub(super) fn anchor_session(&self) -> Result<Rc<SessionRuntime>, proto::RpcError> {
        let sid = self.anchor_session_id.borrow().clone();
        match sid {
            Some(id) => match self.manager.session(&id) {
                Ok(rt) => Ok(rt),
                Err(_) if self.attached_host.is_some() => self
                    .active_sessions()
                    .into_iter()
                    .next()
                    .ok_or_else(|| proto::RpcError::coded("unknown_session", "no session is open in this process")),
                Err(e) => Err(session_error_to_rpc(e)),
            },
            None => Err(proto::RpcError::coded("unknown_session", "no session is open in this process")),
        }
    }

    pub(super) fn anchor_host(&self) -> Option<Arc<Host>> {
        if let Ok(rt) = self.anchor_session() {
            return Some(rt.host.clone());
        }
        if let Some(rt) = self.active_sessions().into_iter().next() {
            return Some(rt.host.clone());
        }
        self.fallback_host.borrow().clone()
    }

    async fn shutdown_detached_host(&self, host: Option<Arc<Host>>) {
        if let Some(h) = host
            && !self.active_sessions().iter().any(|rt| Arc::ptr_eq(&rt.host, &h))
        {
            h.shutdown().await;
        }
    }

    pub(super) async fn shutdown_fallback_host(&self, skip: Option<&Arc<Host>>) {
        let prev = self.fallback_host.borrow_mut().take();
        if let Some(h) = prev
            && skip.is_none_or(|s| !Arc::ptr_eq(&h, s))
        {
            self.shutdown_detached_host(Some(h)).await;
        }
    }

    pub(super) async fn ensure_live_anchor_host(&self) -> Result<Arc<Host>, proto::RpcError> {
        if let Some(h) = self.anchor_host()
            && h.kernel.has_service_exports()
        {
            return Ok(h);
        }
        let opts = self.clone_clean_session_opts(None, Some(true))?;
        let host =
            Host::new_with_web_and_orchestrator(opts, self.manager.web().clone(), self.manager.orchestrator().clone())
                .await
                .map_err(failure)?;
        let prev = self.fallback_host.borrow_mut().replace(host.clone());
        self.shutdown_detached_host(prev).await;
        Ok(host)
    }

    fn conn_by_id(&self, id: mare_host::ConnId) -> Option<Rc<Conn>> {
        self.connections.borrow().get(&id).and_then(Weak::upgrade)
    }

    /// Anchor host pinned to this server's host project (`base_opts.cwd`), so restricted peer
    /// calls (`session.list`, `session.get`, `session.archive`/`unarchive`/`delete`/`snapshot`,
    /// etc.) never execute inside a sibling-project VM just because the owner switched
    /// `anchor_session_id` to another project.
    pub(super) async fn ensure_project_anchor_host(&self) -> Result<Arc<Host>, proto::RpcError> {
        if self.attached_host.is_some() {
            return self.ensure_live_anchor_host().await;
        }
        let project_cwd = services::canonical_dir(&self.base_opts.borrow().cwd);
        let in_project =
            |h: &Arc<Host>| h.kernel.has_service_exports() && services::canonical_dir(&h.cwd) == project_cwd;
        if let Ok(rt) = self.anchor_session()
            && in_project(&rt.host)
        {
            return Ok(rt.host.clone());
        }
        if let Some(rt) = self.active_sessions().into_iter().find(|rt| in_project(&rt.host)) {
            return Ok(rt.host.clone());
        }
        if let Some(h) = self.fallback_host.borrow().clone()
            && in_project(&h)
        {
            return Ok(h);
        }
        let opts = self.clone_clean_session_opts(None, Some(true))?;
        let host =
            Host::new_with_web_and_orchestrator(opts, self.manager.web().clone(), self.manager.orchestrator().clone())
                .await
                .map_err(failure)?;
        let prev = self.fallback_host.borrow_mut().replace(host.clone());
        self.shutdown_detached_host(prev).await;
        Ok(host)
    }

    /// Anchor host visible to `conn`: owners (and attached single-host servers) see the global
    /// anchor host; peers in multi-session servers are confined to the host project (`base_opts.cwd`).
    fn anchor_host_for(&self, conn: &Conn) -> Option<Arc<Host>> {
        if conn.is_owner() || self.attached_host.is_some() {
            return self.anchor_host();
        }
        let project_cwd = services::canonical_dir(&self.base_opts.borrow().cwd);
        let in_project =
            |h: &Arc<Host>| h.kernel.has_service_exports() && services::canonical_dir(&h.cwd) == project_cwd;
        if let Ok(rt) = self.anchor_session()
            && in_project(&rt.host)
        {
            return Some(rt.host.clone());
        }
        if let Some(rt) = self.active_sessions().into_iter().find(|rt| in_project(&rt.host)) {
            return Some(rt.host.clone());
        }
        self.fallback_host.borrow().clone().filter(in_project)
    }

    async fn ensure_anchor_host_for(&self, conn: &Conn) -> Result<Arc<Host>, proto::RpcError> {
        if conn.is_owner() || self.attached_host.is_some() {
            self.ensure_live_anchor_host().await
        } else {
            self.ensure_project_anchor_host().await
        }
    }

    /// Anchor session visible to `conn` for `initialize` metadata: owners (and attached single-host
    /// servers) see the process's anchor session directly; peers in multi-session servers only see
    /// a session belonging to the server's host project (`base_opts.cwd`).
    fn anchor_session_for(&self, conn: &Conn) -> Result<Rc<SessionRuntime>, proto::RpcError> {
        let rt = self.anchor_session()?;
        // identity: owners see the global anchor session; peers are confined to the host project
        if conn.is_owner() || self.attached_host.is_some() {
            return Ok(rt);
        }
        let project_cwd = services::canonical_dir(&self.base_opts.borrow().cwd);
        if services::canonical_dir(&rt.host.cwd) == project_cwd {
            return Ok(rt);
        }
        self.active_sessions()
            .into_iter()
            .find(|cand| services::canonical_dir(&cand.host.cwd) == project_cwd)
            .ok_or_else(|| proto::RpcError::coded("unknown_session", "no active session in this project"))
    }

    pub(super) fn conn_id_for_ctx(&self, host: &Arc<Host>, ctx_id: u64) -> Option<mare_host::ConnId> {
        let map = self.connections.borrow();
        map.values().filter_map(Weak::upgrade).find_map(|c| {
            let matches_anchor = c
                .anchor_ctx
                .borrow()
                .as_ref()
                .is_some_and(|(h, cid)| *cid == ctx_id && h.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)));
            let matches_session = c
                .session_ctxs
                .borrow()
                .values()
                .any(|(h, cid)| *cid == ctx_id && h.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)));
            (matches_anchor || matches_session).then_some(c.id)
        })
    }

    pub(super) fn wake_events(&self) {
        self.event_wake.send_modify(|v| *v = v.wrapping_add(1));
    }

    pub(crate) fn subscribe_wake(&self) -> tokio::sync::watch::Receiver<u64> {
        self.event_wake.subscribe()
    }

    pub(crate) fn active_peer_host(&self) -> Option<Arc<Host>> {
        self.anchor_session().ok().map(|rt| rt.host.clone()).or_else(|| self.anchor_host())
    }

    pub(super) fn broadcast_ui_for(&self, host: &Host, method: &str, component: Option<&str>, params: J) {
        let mut targets = std::collections::BTreeSet::new();
        if let Some(holder) = host.answerer.holder() {
            targets.insert(holder.0);
        }
        for cid in host.shell.attached_conns() {
            targets.insert(cid.0);
        }
        let conns: Vec<Rc<Conn>> = {
            let map = self.connections.borrow();
            targets.into_iter().filter_map(|id| map.get(&mare_host::ConnId(id)).and_then(Weak::upgrade)).collect()
        };
        for c in conns {
            if c.closed.get() {
                continue;
            }
            if let Some(comp) = component
                && host.shell.has_takeover(c.id, comp)
            {
                continue;
            }
            c.notify(method, params.clone());
        }
    }

    pub(super) fn broadcast_ui(&self, host: &Host, method: &str, params: J) {
        self.broadcast_ui_for(host, method, None, params);
    }

    pub(super) fn broadcast_takeover(&self, session_id: &str, host: &Host, component: &str, payload: J) {
        let holders = host.shell.takeover_holders(component);
        if holders.is_empty() {
            return;
        }
        let params = serde_json::to_value(proto::UiComponentUpdateParams {
            session_id: session_id.to_string(),
            component: component.to_string(),
            payload,
        })
        .unwrap_or(J::Null);
        let conns: Vec<Rc<Conn>> = {
            let map = self.connections.borrow();
            holders.into_iter().filter_map(|cid| map.get(&cid).and_then(Weak::upgrade)).collect()
        };
        for c in conns {
            if !c.closed.get() {
                c.notify(methods::UI_COMPONENT_UPDATE, params.clone());
            }
        }
    }

    pub(super) fn broadcast_component(
        &self,
        session_id: &str,
        host: &Host,
        method: Option<&str>,
        component: &str,
        payload: J,
    ) {
        if let Some(m) = method {
            self.broadcast_ui_for(host, m, Some(component), payload.clone());
        }
        self.broadcast_takeover(session_id, host, component, payload);
    }

    pub(super) fn broadcast_editor_state(&self, session_id: &str, host: &Host, state: &proto::UiEditorStateParams) {
        self.broadcast_component(
            session_id,
            host,
            Some(methods::UI_EDITOR_STATE),
            "editor",
            serde_json::to_value(state).unwrap_or(J::Null),
        );
    }

    pub(super) fn clone_base_opts(&self, cwd_override: Option<&str>) -> Result<HostOptions, proto::RpcError> {
        let mut opts = self.base_opts.borrow().clone();
        if let Some(raw) = cwd_override {
            if raw.trim().is_empty() {
                return Err(proto::RpcError::core(proto::ErrorCode::InvalidArguments, "cwd must not be empty"));
            }
            let path = std::path::PathBuf::from(raw);
            let resolved = if path.is_absolute() { path } else { opts.cwd.join(path) };
            let canonical = dunce::canonicalize(&resolved).unwrap_or(resolved);
            if !canonical.is_dir() {
                return Err(proto::RpcError::core(
                    proto::ErrorCode::InvalidArguments,
                    format!("cwd directory does not exist: {}", canonical.display()),
                ));
            }
            opts.cwd = canonical;
        }
        let target_norm = mare_shared::paths::normalize_workspace_path(&opts.cwd).ok();
        let anchor_norm = mare_shared::paths::normalize_workspace_path(&self.anchor_cwd).ok();
        let same_ws = target_norm.is_some() && target_norm == anchor_norm;
        opts.project_trusted = mare_host::resolve_workspace_trust(
            &opts.cwd,
            same_ws,
            self.anchor_startup_trusted,
            self.anchor_cli_trust,
            opts.no_user_plugins,
        );
        sync_trust_args(
            &mut opts.args,
            &opts.cwd,
            opts.project_trusted,
            self.anchor_cli_trust,
            opts.load_init,
            opts.no_user_plugins,
        );
        Ok(opts)
    }

    pub(super) fn clone_clean_session_opts(
        &self,
        cwd_override: Option<&str>,
        no_session: Option<bool>,
    ) -> Result<HostOptions, proto::RpcError> {
        let mut opts = self.clone_base_opts(cwd_override)?;
        if !opts.args.is_object() {
            opts.args = json!({});
        }
        if let Some(args) = opts.args.as_object_mut() {
            for key in [
                "session_id",
                "name",
                "session_new",
                "resume",
                "continue",
                "fork_session",
                "fork_before_entry",
                "add_dirs",
                "restore_permission_ceiling",
                "restore_sandbox_ceiling",
            ] {
                args.remove(key);
            }
            if let Some(ns) = no_session {
                args.insert("no_session".into(), J::Bool(ns));
            }
        }
        Ok(opts)
    }

    async fn on_session_opened(&self, rt: &Rc<SessionRuntime>, creator: Option<&Rc<Conn>>, make_default: bool) {
        if make_default || self.anchor_session_id.borrow().is_none() {
            *self.anchor_session_id.borrow_mut() = Some(rt.session_id.clone());
        }
        let prev = self.fallback_host.borrow_mut().replace(rt.host.clone());
        self.shutdown_detached_host(prev).await;
        let mut conns: Vec<Rc<Conn>> = self.connections.borrow().values().filter_map(Weak::upgrade).collect();
        conns.sort_by_key(|c| c.id.0);
        for conn in &conns {
            let is_creator = creator.is_some_and(|c| c.id == conn.id);
            let Ok(_permit) = conn.cap_gate.acquire().await else { continue };
            if !conn.is_ready() || conn.closed.get() {
                continue;
            }
            // identity/lease: only the owner connection holds answerer/UI/callback leases across sessions
            if is_creator && conn.is_owner() {
                let caps = conn.caps.borrow().clone();
                let _ = apply_capabilities_on_session(conn, &rt.session_id, &rt.host, &caps).await;
            } else if !is_creator
                && conn.is_owner()
                && conn.caps.borrow().callbacks
                && let Ok(ctx_id) = conn.ensure_session_ctx(&rt.session_id, &rt.host).await
            {
                rt.host.kernel.set_callback_channel(ctx_id, Some(services::callback_channel(conn, &rt.session_id)));
            }
            inject_global_registrations_on(conn, rt).await;
        }
        // Fallback for sessions opened from Lua (`creator = None`) or by a non-admin remote owner:
        // if no connection holds the answerer lease yet, grant it to the earliest ready owner
        // connection authorized as an answerer that declared UI/approvals/elicitation capabilities.
        if rt.host.answerer.holder().is_none() {
            for conn in &conns {
                if !conn.is_owner() {
                    continue;
                }
                let Ok(_permit) = conn.cap_gate.acquire().await else { continue };
                if !conn.is_ready() || conn.closed.get() || rt.host.answerer.holder().is_some() {
                    continue;
                }
                let caps = conn.caps.borrow().clone();
                let wants_answerer = caps.ui || caps.approvals || caps.elicitation;
                if wants_answerer && access::authorize(conn, &rt.host, access::Op::Answerer).is_allowed() {
                    let _ = apply_capabilities_on_session(conn, &rt.session_id, &rt.host, &caps).await;
                    break;
                }
            }
        }
        self.wake_events();
    }

    pub(super) fn on_session_closed(&self, session_id: &str) {
        if self.anchor_session_id.borrow().as_deref() == Some(session_id) {
            *self.anchor_session_id.borrow_mut() = None;
        }
        let conns: Vec<Rc<Conn>> = self.connections.borrow().values().filter_map(Weak::upgrade).collect();
        for conn in conns {
            conn.on_session_closed(session_id);
        }
        self.wake_events();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HandshakeState {
    Uninitialized,
    Initializing,
    Ready,
}

#[derive(Clone)]
pub(super) enum GlobalRegistrationKind {
    Contract {
        service: String,
        method: String,
        validated_arguments: J,
        callback_names: Vec<String>,
        release_on: mare_kernel::contracts::ReleaseOn,
    },
    Service {
        definition: proto::services::RegisterParams,
    },
}

#[derive(Clone)]
pub(super) struct GlobalRegistrationRecord {
    pub kind: GlobalRegistrationKind,
    pub scope: ResolvedRegistrationScope,
    pub active_session_instances: std::collections::BTreeSet<String>,
}

#[derive(Default)]
pub(super) struct GlobalRegistrations {
    pub by_id: std::collections::BTreeMap<String, GlobalRegistrationRecord>,
    pub order: Vec<String>,
}

impl GlobalRegistrations {
    pub fn insert(&mut self, id: String, record: GlobalRegistrationRecord) {
        if !self.by_id.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.by_id.insert(id, record);
    }

    pub fn remove(&mut self, id: &str) -> Option<GlobalRegistrationRecord> {
        self.order.retain(|gid| gid != id);
        self.by_id.remove(id)
    }

    /// Remove registrations that release on client `callbacks` withdrawal (`services.register` and
    /// contract registrations with declared callback names and `release_on == ReleaseOn::Callbacks`)
    /// and return their registration IDs so active session kernels can release them immediately.
    /// Registrations with `release_on == ReleaseOn::Never` stay installed and fail closed when the
    /// callback channel is cleared.
    pub fn take_withdrawn_callback_ids(&mut self) -> Vec<String> {
        let removed: Vec<String> = self
            .by_id
            .iter()
            .filter_map(|(id, rec)| match &rec.kind {
                GlobalRegistrationKind::Service { .. } => Some(id.clone()),
                GlobalRegistrationKind::Contract { callback_names, release_on, .. }
                    if !callback_names.is_empty() && *release_on == mare_kernel::contracts::ReleaseOn::Callbacks =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in &removed {
            self.remove(id);
        }
        removed
    }

    /// Remove and return registration IDs owned by `session_id` that declare `release_on = "ui_detach"`.
    pub fn take_ui_detach_ids(&mut self, session_id: &str) -> Vec<String> {
        let removed: Vec<String> = self
            .by_id
            .iter()
            .filter_map(|(id, rec)| match (&rec.kind, &rec.scope) {
                (GlobalRegistrationKind::Contract { release_on, .. }, ResolvedRegistrationScope::Session(sid))
                    if *release_on == mare_kernel::contracts::ReleaseOn::UiDetach && sid == session_id =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in &removed {
            self.remove(id);
        }
        removed
    }

    /// Remove any remaining `release_on = "ui_detach"` records on connection cleanup.
    pub fn clear_ui_detach_ids(&mut self) {
        let removed: Vec<String> = self
            .by_id
            .iter()
            .filter_map(|(id, rec)| match &rec.kind {
                GlobalRegistrationKind::Contract { release_on, .. }
                    if *release_on == mare_kernel::contracts::ReleaseOn::UiDetach =>
                {
                    Some(id.clone())
                }
                _ => None,
            })
            .collect();
        for id in &removed {
            self.remove(id);
        }
    }

    pub fn all_scoped_entries(&self) -> Vec<(String, GlobalRegistrationRecord)> {
        self.order
            .iter()
            .filter_map(|gid| {
                self.by_id
                    .get(gid)
                    .filter(|rec| matches!(rec.scope, ResolvedRegistrationScope::All))
                    .map(|rec| (gid.clone(), rec.clone()))
            })
            .collect()
    }

    pub fn on_session_closed(&mut self, session_id: &str) {
        let mut to_remove = Vec::new();
        for (gid, rec) in self.by_id.iter_mut() {
            rec.active_session_instances.remove(session_id);
            if matches!(&rec.scope, ResolvedRegistrationScope::Session(sid) if sid == session_id)
                && rec.active_session_instances.is_empty()
            {
                to_remove.push(gid.clone());
            }
        }
        for gid in to_remove {
            self.remove(&gid);
        }
    }
}

/// One RPC connection (single-threaded: everything runs on the main LocalSet).
struct Conn {
    id: mare_host::ConnId,
    rpc: Rc<RpcRuntime>,
    host: Arc<Host>,
    out: mpsc::Sender<OutLine>,
    out_budget: Arc<tokio::sync::Semaphore>,
    out_slots: Arc<tokio::sync::Semaphore>,
    pending: Pending,
    /// connection-wide cancellation token (cancelled on disconnect)
    conn_cancel: tokio_util::sync::CancellationToken,
    requests: RefCell<HashMap<RequestId, tokio_util::sync::CancellationToken>>,
    /// Per-session Kernel owner contexts (`session_id -> (weak Host, ctx_id)`).
    session_ctxs: RefCell<HashMap<String, (SyncWeak<Host>, u64)>>,
    /// Host-level fallback Kernel context used only when zero sessions are loaded.
    anchor_ctx: RefCell<Option<(SyncWeak<Host>, u64)>>,
    global_registrations: RefCell<GlobalRegistrations>,
    reg_gate: Rc<tokio::sync::Semaphore>,
    next_id: Cell<u64>,
    caps: RefCell<proto::ClientCapabilities>,
    cap_revision: Cell<u64>,
    cap_gate: Rc<tokio::sync::Semaphore>,
    timeout: Cell<Duration>,
    /// Human approvals wait indefinitely unless the host explicitly sets a limit.
    approval_timeout: Cell<Option<Duration>>,
    /// default event filter applied to unscoped `events.subscribe` & newly created/resumed/forked sessions
    default_filter: RefCell<EventFilter>,
    /// whether this connection auto-subscribes to newly opened sessions (`events.subscribe` without `session_id`)
    default_subscribe_all: Cell<bool>,
    /// sessions explicitly subscribed via `events.subscribe { session_id }`
    explicit_sessions: RefCell<std::collections::HashSet<String>>,
    /// sessions explicitly created/loaded with `subscribe = false`
    suppressed_subscriptions: RefCell<std::collections::HashSet<String>>,
    /// sessions whose subscription activation is currently deferred in an in-flight request's
    /// `REQUEST_ACTIVATIONS` scope (prevents `stream_pump` from auto-subscribing before reply)
    deferred_subscriptions: RefCell<HashMap<String, usize>>,
    /// per-session event stream cursors (`session_id` -> cursor + filter + failure watch)
    subscriptions: RefCell<HashMap<String, SubscriptionSlot>>,
    /// failure watches for internal per-session `status/changed` tracking cursors
    tracking_failures: RefCell<HashMap<String, tokio::sync::watch::Receiver<Option<String>>>>,
    /// shared notification across all `EventCursor`s on this connection
    sub_wake: Arc<tokio::sync::Notify>,
    /// explicit connection handshake state (admission gate for other methods)
    handshake: Cell<HandshakeState>,
    shutdown_requested: Cell<bool>,
    shutdown: Cell<bool>,
    shutdown_notify: tokio::sync::Notify,
    /// the host disconnected: new callbacks fail at once
    closed: Cell<bool>,
    /// the outgoing queue filled up (the host stopped reading): the connection is dropped
    overflowed: Cell<bool>,
    overflow_notify: tokio::sync::Notify,
    ws_close_code: Option<Arc<std::sync::atomic::AtomicU16>>,
    event_barrier: mpsc::Sender<oneshot::Sender<()>>,
    /// The output failed; newly scheduled turns must not start.
    output_lost: Cell<bool>,
    /// requests being handled right now (bounded by [`MAX_INFLIGHT`])
    inflight: Cell<usize>,
    inflight_notify: tokio::sync::Notify,
    /// authorization principal (Owner vs Peer)
    principal: Principal,
    /// whether this connection arrived over a network/WebSocket listener
    remote_transport: bool,
    /// peer socket: the connecting process's pid from the socket credentials (SO_PEERCRED /
    /// LOCAL_PEERPID), when the platform reports one
    peer_pid: Option<u32>,
}

#[derive(Clone, Default)]
pub(super) struct EventFilter {
    include: Option<Vec<String>>,
    exclude: Vec<String>,
}

pub(super) struct SubscriptionSlot {
    host: SyncWeak<Host>,
    ctx_id: u64,
    cursor: mare_kernel::EventCursor,
    filter: Arc<std::sync::RwLock<EventFilter>>,
    failure: tokio::sync::watch::Receiver<Option<String>>,
}

/// RAII guard suppressing `stream_pump` auto-subscription while a session's activation is queued
/// in an in-flight request's `REQUEST_ACTIVATIONS` scope.
pub(super) struct DeferredSubscriptionGuard {
    conn: Weak<Conn>,
    session_id: String,
}

impl Drop for DeferredSubscriptionGuard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.upgrade() {
            let mut wake = false;
            {
                let mut map = conn.deferred_subscriptions.borrow_mut();
                if let Some(count) = map.get_mut(&self.session_id) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        map.remove(&self.session_id);
                        wake = true;
                    }
                }
            }
            if wake {
                conn.sub_wake.notify_one();
            }
        }
    }
}

/// Queued cursor replacement/addition or filter update activated strictly *after* the JSON-RPC
/// response line is sent so replay/snapshot records never precede the reply frame.
pub(super) enum PendingActivation {
    Cursor {
        session_id: String,
        slot: Box<SubscriptionSlot>,
        default_filter: Option<EventFilter>,
        narrow_to_session: bool,
        snapshot: Option<proto::BaselineSnapshotParams>,
        deferred: Option<DeferredSubscriptionGuard>,
    },
    Filter {
        session_ids: Vec<String>,
        filter: EventFilter,
        default_subscribe_all: Option<bool>,
        narrow_to_session: Option<String>,
    },
}

fn event_selected_for_config(
    tap_config: &Arc<std::sync::Mutex<J>>,
    event_log: &mare_kernel::EventLog,
    primary: bool,
    filter: &Arc<std::sync::RwLock<EventFilter>>,
    name: &str,
) -> bool {
    let cfg = tap_config.lock().unwrap_or_else(|e| e.into_inner());
    let allowed = access::event_allowed_in(primary, &cfg, event_log, name);
    drop(cfg);
    allowed && filter.read().expect("event filter poisoned").wants(event_log, name)
}

fn event_selected_for_rpc(
    host: &Arc<Host>,
    primary: bool,
    filter: &Arc<std::sync::RwLock<EventFilter>>,
    name: &str,
) -> bool {
    let tap_config = Arc::clone(&host.kernel.shared_config());
    event_selected_for_config(&tap_config, host.kernel.event_log(), primary, filter, name)
}

fn build_rpc_filter(
    host: &Arc<Host>,
    primary: bool,
    filter: Arc<std::sync::RwLock<EventFilter>>,
) -> mare_kernel::ReliableFilter {
    let tap_config = Arc::downgrade(&host.kernel.shared_config());
    let event_log = host.kernel.event_log().clone();
    Arc::new(move |name| {
        let tap_config = tap_config.upgrade()?;
        event_selected_for_config(&tap_config, &event_log, primary, &filter, name).then_some(true)
    })
}
impl EventFilter {
    fn wants(&self, event_log: &mare_kernel::EventLog, event: &str) -> bool {
        let matches = |pat: &String| access::matches_event_glob(pat, event);
        !self.exclude.iter().any(matches)
            && match &self.include {
                Some(include) => include.iter().any(matches),
                None => event_log.is_external_event(event),
            }
    }
}

/// Outgoing lines queued for the writer; a host that stops reading is disconnected when full.
pub(crate) const OUT_QUEUE: usize = 4096;
pub(crate) const OUT_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct OutLine {
    pub(crate) text: String,
    _budget: tokio::sync::OwnedSemaphorePermit,
    _slot: tokio::sync::OwnedSemaphorePermit,
}

const UI_PROMPT_METHODS: &[&str] =
    &[methods::UI_SELECT, methods::UI_INPUT, methods::UI_CONFIRM, methods::UI_EDITOR, methods::UI_QUESTIONS];

fn release_answerer_leases(
    conn: &Conn,
    host: &Host,
    release_ui: bool,
    release_approvals: bool,
    release_elicitation: bool,
    release_slot: bool,
) {
    if release_ui {
        host.answerer.restore_ui(host.lua(), conn.id);
    }
    if release_approvals {
        for release in host.answerer.take_approval_voters(conn.id) {
            if let Err(e) = release.call::<()>(()) {
                tracing::warn!("approval voter release: {e}");
            }
        }
    }
    if release_elicitation
        && let Some(release) = host.answerer.take_elicitation_release(conn.id)
        && let Err(e) = release.call::<()>(())
    {
        tracing::warn!("elicitation listener release: {e}");
    }
    if release_slot {
        host.answerer.release(host.lua(), conn.id, "answerer capabilities withdrawn");
    }
}

impl Conn {
    fn is_ready(&self) -> bool {
        self.handshake.get() == HandshakeState::Ready
    }

    fn is_owner(&self) -> bool {
        self.principal.is_owner()
    }

    pub(super) fn is_remote_admin(&self) -> bool {
        self.principal.is_remote_admin()
    }

    /// A local owner is always administrative; a remote owner uses the target host's live policy.
    pub(super) fn is_remote_admin_on(&self, host: &Host) -> bool {
        self.is_owner()
            && (self.principal.is_remote_admin()
                || host.kernel.config().get("webserver").and_then(|v| v.get("remote_admin")).and_then(J::as_bool)
                    == Some(true))
    }

    pub(super) fn is_remote_transport(&self) -> bool {
        self.remote_transport
    }

    fn is_answerer_on(&self, host: &Host) -> bool {
        host.answerer.is_holder(self.id)
    }

    fn ctx_id_for_host(&self, host: &Host) -> Option<u64> {
        self.session_ctxs
            .borrow()
            .values()
            .find_map(|(w, cid)| w.upgrade().filter(|h| std::ptr::eq(Arc::as_ptr(h), host)).map(|_| *cid))
            .or_else(|| {
                self.anchor_ctx
                    .borrow()
                    .as_ref()
                    .and_then(|(w, cid)| w.upgrade().filter(|h| std::ptr::eq(Arc::as_ptr(h), host)).map(|_| *cid))
            })
    }

    pub(super) fn all_session_host_ctxs(&self) -> Vec<(String, Arc<Host>, u64)> {
        let mut out: Vec<(String, Arc<Host>, u64)> = self
            .session_ctxs
            .borrow()
            .iter()
            .filter_map(|(sid, (w, c))| w.upgrade().map(|h| (sid.clone(), h, *c)))
            .collect();
        if let Some((w, c)) = self.anchor_ctx.borrow().as_ref()
            && let Some(h) = w.upgrade()
        {
            out.push((String::new(), h, *c));
        }
        out
    }

    pub(super) fn all_host_ctxs(&self) -> Vec<(Arc<Host>, u64)> {
        self.all_session_host_ctxs().into_iter().map(|(_, h, c)| (h, c)).collect()
    }

    fn baseline_host(&self, baseline_id: &str) -> Option<(Arc<Host>, u64)> {
        self.all_host_ctxs().into_iter().find(|(host, ctx_id)| host.kernel.owns_baseline(*ctx_id, baseline_id))
    }

    pub(super) fn drain_all_ctxs(&self) -> Vec<(SyncWeak<Host>, u64)> {
        let mut out: Vec<(SyncWeak<Host>, u64)> = self.session_ctxs.borrow_mut().drain().map(|(_, v)| v).collect();
        if let Some(entry) = self.anchor_ctx.borrow_mut().take() {
            out.push(entry);
        }
        out
    }

    async fn ensure_session_ctx(self: &Rc<Self>, session_id: &str, host: &Arc<Host>) -> Result<u64, proto::RpcError> {
        let closed_err = || proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed");
        if self.closed.get() {
            return Err(closed_err());
        }
        let cached = if session_id.is_empty() {
            self.anchor_ctx.borrow().as_ref().cloned()
        } else {
            self.session_ctxs.borrow().get(session_id).cloned()
        };
        if let Some((weak, cid)) = cached
            && weak.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host))
        {
            return Ok(cid);
        }
        let ctx_id = host.kernel.open_owner("<root>", "root").map_err(|e| failure(e.into()))?;
        host.kernel.set_caller_access(ctx_id, services::caller_access(self, host));
        let fiber_uid = tree_hooks::connect_host(self, session_id, host, ctx_id).await;
        if self.closed.get() {
            tree_hooks::disconnect_host(host, ctx_id).await;
            let _ = host.kernel.dispose_ctx(ctx_id).await;
            return Err(closed_err());
        }
        let target_sid = if session_id.is_empty() {
            String::new()
        } else {
            host.kernel
                .event_log()
                .session_identity()
                .0
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| session_id.to_owned())
        };
        let existing = if target_sid.is_empty() {
            self.anchor_ctx
                .borrow()
                .as_ref()
                .and_then(|(w, cid)| w.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)).then_some(*cid))
        } else {
            self.session_ctxs
                .borrow()
                .values()
                .find_map(|(w, cid)| w.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)).then_some(*cid))
        };
        if let Some(existing_cid) = existing {
            tree_hooks::disconnect_host(host, ctx_id).await;
            let _ = host.kernel.dispose_ctx(ctx_id).await;
            return Ok(existing_cid);
        }
        stream_pump::attach_connection_listeners_on(self, &target_sid, host, ctx_id, fiber_uid);
        if self.caps.borrow().callbacks {
            host.kernel.set_callback_channel(ctx_id, Some(services::callback_channel(self, &target_sid)));
        }
        let owner = (Arc::downgrade(host), ctx_id);
        if target_sid.is_empty() {
            *self.anchor_ctx.borrow_mut() = Some(owner);
        } else {
            self.session_ctxs.borrow_mut().insert(target_sid, owner);
        }
        Ok(ctx_id)
    }

    fn on_session_closed(&self, session_id: &str) {
        self.session_ctxs.borrow_mut().remove(session_id);
        self.explicit_sessions.borrow_mut().remove(session_id);
        self.suppressed_subscriptions.borrow_mut().remove(session_id);
        self.subscriptions.borrow_mut().remove(session_id);
        self.tracking_failures.borrow_mut().remove(session_id);
        self.global_registrations.borrow_mut().on_session_closed(session_id);
        self.cancel_answerer_pending_for_session(session_id, "session closed");
        self.sub_wake.notify_one();
    }

    pub(super) fn remove_session_ctx_if_host(&self, session_id: &str, host: &Arc<Host>) {
        let matched = {
            let mut ctxs = self.session_ctxs.borrow_mut();
            let is_same =
                ctxs.get(session_id).is_some_and(|(weak, _)| weak.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)));
            if is_same {
                ctxs.remove(session_id);
            }
            is_same
        };
        if matched {
            self.cancel_answerer_pending_for_session(session_id, "session load failed");
        }
    }

    fn rekey_session(&self, old_sid: &str, new_sid: &str, host: &Arc<Host>) {
        if old_sid == new_sid {
            return;
        }
        let mut matched_host = host.shell.is_attached(self.id);
        {
            let mut ctxs = self.session_ctxs.borrow_mut();
            if ctxs.get(old_sid).is_some_and(|(w, _)| w.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)))
                && let Some(entry) = ctxs.remove(old_sid)
            {
                ctxs.insert(new_sid.to_owned(), entry);
                matched_host = true;
            }
        }
        {
            let mut subs = self.subscriptions.borrow_mut();
            if subs.get(old_sid).is_some_and(|s| s.host.upgrade().is_some_and(|h| Arc::ptr_eq(&h, host)))
                && let Some(slot) = subs.remove(old_sid)
            {
                subs.insert(new_sid.to_owned(), slot);
                matched_host = true;
            }
        }
        if matched_host {
            self.cancel_answerer_pending_for_session(old_sid, "session changed");
        }
        let mut migrated = false;
        {
            let mut explicit = self.explicit_sessions.borrow_mut();
            if explicit.remove(old_sid) {
                explicit.insert(new_sid.to_owned());
                migrated = true;
            }
        }
        {
            let mut suppressed = self.suppressed_subscriptions.borrow_mut();
            if suppressed.remove(old_sid) {
                suppressed.insert(new_sid.to_owned());
                migrated = true;
            }
        }
        {
            let mut failures = self.tracking_failures.borrow_mut();
            if let Some(fail) = failures.remove(old_sid) {
                failures.insert(new_sid.to_owned(), fail);
                migrated = true;
            }
        }
        for rec in self.global_registrations.borrow_mut().by_id.values_mut() {
            if matches!(&rec.scope, ResolvedRegistrationScope::Session(sid) if sid == old_sid) {
                rec.scope = ResolvedRegistrationScope::Session(new_sid.to_owned());
                migrated = true;
            }
            if rec.active_session_instances.remove(old_sid) {
                rec.active_session_instances.insert(new_sid.to_owned());
                migrated = true;
            }
        }
        if matched_host || migrated {
            self.sub_wake.notify_one();
        }
    }

    pub(super) fn acquire_answerer(self: &Rc<Self>, host: &Arc<Host>, session_id: &str) {
        let sid_for_revoke = session_id.to_owned();
        let weak = Rc::downgrade(self);
        let weak_host = Arc::downgrade(host);
        host.answerer.acquire(
            host.lua(),
            self.id,
            Box::new(move |reason| {
                if let Some(c) = weak.upgrade() {
                    let sid = weak_host
                        .upgrade()
                        .and_then(|h| h.kernel.event_log().session_identity().0)
                        .filter(|sid| !sid.is_empty())
                        .unwrap_or_else(|| sid_for_revoke.clone());
                    c.cancel_answerer_pending_for_session(&sid, reason);
                }
            }),
        );
    }

    pub(super) fn cancel_ui_pending_for_session(&self, session_id: &str, reason: &str) {
        let err = proto::RpcError::core(proto::ErrorCode::Internal, reason);
        for m in UI_PROMPT_METHODS {
            self.cancel_pending_filtered(Some(m), Some(session_id), err.clone());
        }
    }

    fn cancel_answerer_pending_for_session(&self, session_id: &str, reason: &str) {
        let err = proto::RpcError::core(proto::ErrorCode::Internal, reason);
        for m in
            [methods::APPROVAL_REQUEST, methods::MCP_ELICITATION].into_iter().chain(UI_PROMPT_METHODS.iter().copied())
        {
            self.cancel_pending_filtered(Some(m), Some(session_id), err.clone());
        }
    }

    /// Queue one outgoing line. The queue is bounded ([`OUT_QUEUE`]): a host that stops reading
    /// while the session keeps producing is disconnected instead of growing memory without limit
    /// .
    fn fail_stream(&self) {
        if !self.overflowed.replace(true) {
            self.closed.set(true);
            if let Some(code) = &self.ws_close_code {
                code.store(1013, std::sync::atomic::Ordering::Release);
            }
            // This control record bypasses the normal admission check. If the output queue is
            // itself full, disconnect/EOF is the only available incomplete-stream signal.
            if let Ok(line) = serde_json::to_string(&Message::notification(
                methods::STREAM_FAILED,
                json!({"error": "event delivery failed; stream is incomplete"}),
            )) {
                let _ = self.queue_line(line);
            }
            self.overflow_notify.notify_one();
        }
    }

    fn has_tap_failure(&self) -> bool {
        self.tracking_failures.borrow().values().any(|rx| rx.borrow().is_some())
            || self.subscriptions.borrow().values().any(|s| s.failure.borrow().is_some())
    }

    pub(super) fn defer_subscription(self: &Rc<Self>, session_id: String) -> DeferredSubscriptionGuard {
        *self.deferred_subscriptions.borrow_mut().entry(session_id.clone()).or_insert(0) += 1;
        DeferredSubscriptionGuard { conn: Rc::downgrade(self), session_id }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn open_subscription_on(
        &self,
        session_id: &str,
        host: &Arc<Host>,
        ctx_id: u64,
        requested_epoch: Option<&str>,
        last_seq: Option<u64>,
        live_only: bool,
        event_filter: EventFilter,
        update_default: bool,
        narrow_to_session: bool,
    ) -> Result<(proto::SubscribeMode, String, u64, PendingActivation), proto::RpcError> {
        let default_filter = update_default.then(|| event_filter.clone());
        let filter = Arc::new(std::sync::RwLock::new(event_filter));
        let wants = build_rpc_filter(host, self.is_owner(), Arc::clone(&filter));
        let (outcome, snapshot) = if live_only {
            (host.kernel.subscribe_cursor(requested_epoch, last_seq, true, wants, Some(self.sub_wake.clone())), None)
        } else {
            let retain_filter = |name: &str| event_selected_for_rpc(host, self.is_owner(), &filter, name);
            let (extra, include_state, page_access) = self.baseline_access(host);
            host.kernel
                .subscribe_with_baseline(
                    ctx_id,
                    requested_epoch,
                    last_seq,
                    false,
                    wants,
                    Some(self.sub_wake.clone()),
                    &retain_filter,
                    extra,
                    include_state,
                    page_access,
                )
                .map_err(|e| failure(anyhow::anyhow!("failed to cut baseline snapshot: {e}")))?
        };
        let mode = outcome.mode;
        let session_epoch = outcome.session_epoch;
        let seq = outcome.seq;
        let activation = PendingActivation::Cursor {
            session_id: session_id.to_owned(),
            slot: Box::new(SubscriptionSlot {
                host: Arc::downgrade(host),
                ctx_id,
                failure: outcome.cursor.failure(),
                cursor: outcome.cursor,
                filter,
            }),
            default_filter,
            narrow_to_session,
            snapshot,
            deferred: None,
        };
        Ok((mode, session_epoch, seq, activation))
    }

    /// Re-align `filter_only` reply `(session_epoch, seq)` to the post-flush tail right before
    /// sending the response so `reply.seq` never trails events drained during `flush_events().await`.
    pub(super) fn refresh_post_flush_filter_outcome(
        &self,
        outcome: &mut Result<J, proto::RpcError>,
        activations: &[PendingActivation],
    ) {
        let Ok(value) = outcome else { return };
        let anchor_sid = self.rpc.anchor_session_id.borrow().clone();
        for act in activations {
            if let PendingActivation::Filter { session_ids, .. } = act
                && let Some(sid) = anchor_sid
                    .as_deref()
                    .filter(|d| session_ids.iter().any(|s| s == d))
                    .or_else(|| session_ids.first().map(String::as_str))
                && let Some(host) = self
                    .rpc
                    .manager
                    .session(sid)
                    .ok()
                    .map(|rt| rt.host.clone())
                    .or_else(|| self.rpc.attached_host.clone())
            {
                let (cur_epoch, cur_tail) = host.kernel.event_log().session_tail();
                *value = serde_json::to_value(proto::EventsSubscribeResult {
                    mode: proto::SubscribeMode::Live,
                    session_epoch: cur_epoch,
                    seq: cur_tail + 1,
                })
                .unwrap_or(J::Null);
            }
        }
    }

    pub(super) fn activate_subscriptions(&self, activations: Vec<PendingActivation>) {
        if activations.is_empty() {
            return;
        }
        let mut snapshots = Vec::new();
        let mut touched_hosts: Vec<Arc<Host>> = Vec::new();
        {
            let mut subs = self.subscriptions.borrow_mut();
            for act in activations {
                match act {
                    PendingActivation::Cursor {
                        session_id,
                        slot,
                        default_filter,
                        narrow_to_session,
                        snapshot,
                        deferred,
                    } => {
                        if let Some(df) = default_filter {
                            *self.default_filter.borrow_mut() = df;
                        }
                        if narrow_to_session {
                            self.default_subscribe_all.set(false);
                            let mut explicit = self.explicit_sessions.borrow_mut();
                            explicit.insert(session_id.clone());
                            subs.retain(|k, old| {
                                let keep = k == &session_id || explicit.contains(k);
                                if !keep && let Some(h) = old.host.upgrade() {
                                    touched_hosts.push(h);
                                }
                                keep
                            });
                        }
                        if let Some(h) = slot.host.upgrade() {
                            let mut explicit = self.explicit_sessions.borrow_mut();
                            subs.retain(|k, old| {
                                if old.host.upgrade().is_some_and(|old_h| Arc::ptr_eq(&old_h, &h)) {
                                    if k != &session_id {
                                        explicit.remove(k);
                                    }
                                    return false;
                                }
                                true
                            });
                            if let Some(snap) = snapshot {
                                snapshots.push((h.clone(), slot.ctx_id, snap));
                            }
                            touched_hosts.push(h);
                        }
                        subs.insert(session_id, *slot);
                        drop(deferred);
                    }
                    PendingActivation::Filter { session_ids, filter, default_subscribe_all, narrow_to_session } => {
                        *self.default_filter.borrow_mut() = filter.clone();
                        if let Some(all) = default_subscribe_all {
                            self.default_subscribe_all.set(all);
                            if all {
                                self.explicit_sessions.borrow_mut().clear();
                            }
                        }
                        if let Some(ref target_sid) = narrow_to_session {
                            self.default_subscribe_all.set(false);
                            let mut explicit = self.explicit_sessions.borrow_mut();
                            explicit.insert(target_sid.clone());
                            subs.retain(|k, old| {
                                let keep = k == target_sid || explicit.contains(k);
                                if !keep && let Some(h) = old.host.upgrade() {
                                    touched_hosts.push(h);
                                }
                                keep
                            });
                        }
                        for sid in session_ids {
                            if let Some(slot) = subs.get(&sid) {
                                *slot.filter.write().expect("event filter poisoned") = filter.clone();
                                slot.cursor.clear_decisions();
                                if let Some(h) = slot.host.upgrade() {
                                    touched_hosts.push(h);
                                }
                            }
                        }
                    }
                }
            }
        }
        for h in touched_hosts {
            h.kernel.taps_changed();
        }
        for (host, ctx_id, snap) in snapshots {
            self.send_snapshot_on(&host, ctx_id, snap);
        }
        self.sub_wake.notify_one();
    }

    fn send_snapshot_on(&self, host: &Host, ctx_id: u64, mut snap: proto::BaselineSnapshotParams) {
        if !access::authorize(self, host, access::Op::EventsStream).is_allowed() {
            snap.retained.retain(|name, _| name == "events");
            if let Some(events) = snap.retained.get_mut("events").and_then(J::as_object_mut) {
                events.retain(|name, _| access::PEER_DEFAULT_EVENTS.contains(&name.as_str()));
            }
            snap.baseline_id = None;
            snap.parts.clear();
        } else if snap.baseline_id.as_deref().is_some_and(|id| !host.kernel.owns_baseline(ctx_id, id)) {
            snap.baseline_id = None;
            snap.parts.clear();
        }
        self.notify(methods::SNAPSHOT, json!(snap));
    }

    fn baseline_access(
        &self,
        host: &Host,
    ) -> (std::collections::BTreeMap<String, J>, bool, Option<mare_kernel::BaselineAccess>) {
        let include_state = access::authorize(self, host, access::Op::EventsStream).is_allowed();
        let extra = if include_state { self.baseline_extra_retained() } else { Default::default() };
        (extra, include_state, access::baseline_page_access(self, host))
    }

    fn baseline_extra_retained(&self) -> std::collections::BTreeMap<String, J> {
        let mut pending_approvals = Vec::new();
        let mut pending_interactions = Vec::new();
        for (id, req) in self.pending.borrow().iter() {
            let item = json!({ "id": id, "method": req.method, "params": req.params });
            if req.method == methods::APPROVAL_REQUEST {
                pending_approvals.push(item);
            } else if UI_PROMPT_METHODS.contains(&req.method.as_str()) || req.method == methods::MCP_ELICITATION {
                pending_interactions.push(item);
            }
        }
        let mut map = std::collections::BTreeMap::new();
        map.insert("pending_approvals".to_string(), J::Array(pending_approvals));
        map.insert("pending_interactions".to_string(), J::Array(pending_interactions));
        map
    }

    fn active_projectors(&self) -> Vec<mare_shared::wire_projection::WireProjector> {
        let mut out = Vec::new();
        let mut seen: Vec<*const Host> = Vec::new();
        let mut push_host = |host: &Arc<Host>| {
            let ptr = Arc::as_ptr(host);
            if seen.contains(&ptr) {
                return;
            }
            seen.push(ptr);
            if let Some(proj) = mare_shared::wire_projection::active_projector(host.lua()) {
                out.push(proj);
            }
        };
        push_host(&self.host);
        for rt in self.rpc.active_sessions() {
            push_host(&rt.host);
        }
        if let Some(anchor) = self.rpc.anchor_host() {
            push_host(&anchor);
        }
        if let Some(fallback) = self.rpc.fallback_host.borrow().as_ref() {
            push_host(fallback);
        }
        for (weak_host, _) in self.session_ctxs.borrow().values() {
            if let Some(host) = weak_host.upgrade() {
                push_host(&host);
            }
        }
        out
    }

    fn send(&self, m: &Message) {
        if self.has_tap_failure() {
            self.fail_stream();
        }
        if self.overflowed.get() {
            return;
        }
        let projectors = self.active_projectors();
        let projected;
        let outbound = if !projectors.is_empty() {
            projected = project_message_all(m, &projectors);
            &projected
        } else {
            m
        };
        let Ok(s) = serde_json::to_string(outbound) else {
            self.fail_stream();
            return;
        };
        if !self.queue_line(s) {
            self.fail_stream();
        }
    }

    fn send_raw_event(&self, params_json: &str) {
        if self.has_tap_failure() {
            self.fail_stream();
        }
        if self.overflowed.get() {
            return;
        }
        let projectors = self.active_projectors();
        let projected_params;
        let params = if projectors.len() > 1 || (!projectors.is_empty() && self.rpc.active_sessions().len() > 1) {
            if let Ok(mut val) = serde_json::from_str::<J>(params_json) {
                for proj in &projectors {
                    proj.project_json(&mut val);
                }
                if let Ok(serialized) = serde_json::to_string(&val) {
                    projected_params = serialized;
                    projected_params.as_str()
                } else {
                    params_json
                }
            } else {
                params_json
            }
        } else {
            params_json
        };
        let line = format!(r#"{{"jsonrpc":"2.0","method":"event","params":{params}}}"#);
        if !self.queue_line(line) {
            self.fail_stream();
        }
    }

    fn queue_line(&self, text: String) -> bool {
        let Some(bytes) = text.len().checked_add(1).and_then(|n| u32::try_from(n).ok()) else { return false };
        let Ok(slot) = self.out_slots.clone().try_acquire_owned() else { return false };
        let Ok(permit) = self.out_budget.clone().try_acquire_many_owned(bytes) else { return false };
        self.out.try_send(OutLine { text, _budget: permit, _slot: slot }).is_ok()
    }

    async fn flush_events(&self) -> bool {
        if self.closed.get() {
            return false;
        }
        let (tx, rx) = oneshot::channel();
        if self.event_barrier.send(tx).await.is_err() || self.closed.get() {
            return false;
        }
        let delivered = rx.await.is_ok();
        if self.closed.get() {
            return false;
        }
        if !delivered {
            self.fail_stream();
            return false;
        }
        true
    }

    fn notify(&self, method: &str, params: J) {
        self.send(&Message::notification(method, params));
    }

    /// Cancel pending callbacks matching a method, refusing their results and notifying the host.
    fn cancel_pending(&self, method: Option<&str>, reason: &str) {
        self.cancel_pending_with(method, proto::RpcError::core(proto::ErrorCode::Internal, reason));
    }

    /// Cancel pending callbacks matching a method on a specific session, leaving other sessions intact.
    fn cancel_pending_for_session(&self, method: Option<&str>, session_id: &str, reason: &str) {
        self.cancel_pending_filtered(
            method,
            Some(session_id),
            proto::RpcError::core(proto::ErrorCode::Internal, reason),
        );
    }

    fn cancel_pending_with(&self, method: Option<&str>, error: proto::RpcError) {
        self.cancel_pending_filtered(method, None, error);
    }

    fn cancel_pending_filtered(&self, method: Option<&str>, session_id: Option<&str>, error: proto::RpcError) {
        let pending = {
            let mut map = self.pending.borrow_mut();
            let ids: Vec<_> = map
                .iter()
                .filter(|(_, p)| {
                    method.is_none_or(|m| p.method == m)
                        && session_id.is_none_or(|sid| p.session_id.as_deref() == Some(sid))
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter().filter_map(|id| map.remove(&id).map(|p| (id, p))).collect::<Vec<_>>()
        };
        for (id, p) in pending {
            if !self.closed.get() {
                self.notify(methods::CANCEL_REQUEST, json!({ "id": id }));
            }
            let _ = p.reply.send(Err(error.clone()));
        }
    }

    /// Human approval has no default timeout; disconnect and cancellation still end the wait.
    async fn call_with_timeout_for_session(
        &self,
        method: &str,
        session_id: &str,
        params: J,
        timeout: Option<Duration>,
    ) -> Option<J> {
        self.call_result_tagged(method, Some(session_id.to_owned()), None, params, timeout).await.ok()
    }

    async fn call_result_with_updates(
        &self,
        method: &str,
        params: J,
        timeout: Option<Duration>,
        update_tx: Option<mpsc::Sender<J>>,
    ) -> Result<J, mare_kernel::ServiceCallError> {
        let session_id = params.get("session_id").and_then(J::as_str).map(str::to_owned);
        self.call_result_tagged(method, session_id, update_tx, params, timeout).await
    }

    async fn call_result_tagged(
        &self,
        method: &str,
        session_id: Option<String>,
        update_tx: Option<mpsc::Sender<J>>,
        params: J,
        timeout: Option<Duration>,
    ) -> Result<J, mare_kernel::ServiceCallError> {
        if method == methods::CALLBACK_CALL {
            let arg = params.get("arguments").unwrap_or(&params);
            for proj in self.active_projectors() {
                proj.check_json_keys(arg).map_err(mare_kernel::ServiceCallError::Arguments)?;
            }
        }
        if self.closed.get() {
            return Err(mare_kernel::ServiceCallError::CallbackDisconnected);
        }
        let n = self.next_id.get();
        self.next_id.set(n + 1);
        let id = format!("mare-{n}");
        let (tx, rx) = oneshot::channel();
        self.pending.borrow_mut().insert(
            id.clone(),
            PendingRequest { method: method.into(), session_id, params: params.clone(), update_tx, reply: tx },
        );
        let _guard = PendingGuard { conn: self, id: id.clone() };
        self.send(&Message::request(RequestId::String(id), method, params));
        if self.closed.get() {
            return Err(mare_kernel::ServiceCallError::CallbackDisconnected);
        }
        let waited = wait_callback(rx, timeout);
        #[cfg(test)]
        let waited = wait_probe::observe(method, waited);
        match waited.await {
            Some(Ok(Ok(v))) => Ok(v),
            Some(Ok(Err(error))) => {
                if self.closed.get() {
                    Err(mare_kernel::ServiceCallError::CallbackDisconnected)
                } else if let Some(code) = error.error_code().map(str::to_owned) {
                    // a coded error from the remote implementation: checked against the
                    // contract like any provider error (a code it does not declare is `internal`)
                    let data = error.data().and_then(|data| data.get("data")).cloned();
                    Err(mare_kernel::ServiceCallError::Coded(mare_kernel::ErrorValue {
                        code,
                        message: mare_shared::errors::bounded(error.message()),
                        data,
                    }))
                } else {
                    // no structured code: an unexpected failure of the remote implementation,
                    // bounded and logged like any other `internal`
                    Err(mare_kernel::internal_error(&format!("remote implementation of {method}"), error.message()))
                }
            }
            Some(Err(_)) => Err(mare_kernel::ServiceCallError::CallbackDisconnected),
            None => Err(mare_kernel::ServiceCallError::CallbackTimeout),
        }
    }
}

fn project_message_all(m: &Message, projs: &[mare_shared::wire_projection::WireProjector]) -> Message {
    let mut copy = m.clone();
    for proj in projs {
        if let Some(params) = &mut copy.params {
            proj.project_json(params);
        }
        if let Some(result) = &mut copy.result {
            proj.project_json(result);
        }
        if let Some(err) = &mut copy.error {
            err.project_with(|s| proj.project_str(s), |data| proj.project_json(data));
        }
    }
    copy
}

#[cfg(test)]
fn project_message(m: &Message, proj: &mare_shared::wire_projection::WireProjector) -> Message {
    project_message_all(m, std::slice::from_ref(proj))
}

#[cfg(test)]
mod projection_tests;

async fn wait_callback(
    rx: oneshot::Receiver<Result<J, proto::RpcError>>,
    timeout: Option<Duration>,
) -> Option<Result<Result<J, proto::RpcError>, oneshot::error::RecvError>> {
    match timeout {
        Some(duration) => tokio::time::timeout(duration, rx).await.ok(),
        None => Some(rx.await),
    }
}

fn params<T: serde::de::DeserializeOwned + Default>(p: &Option<J>) -> Result<T, String> {
    match p {
        None | Some(J::Null) => Ok(T::default()),
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| e.to_string()),
    }
}

fn params_req<T: serde::de::DeserializeOwned>(p: &Option<J>) -> Result<T, String> {
    serde_json::from_value(p.clone().unwrap_or(J::Null)).map_err(|e| e.to_string())
}

/// A Lua failure as it reaches a hand-written RPC method: a mare.Error the Lua side raised is the
/// coded error it was (recalled by its print, mare_shared::lua_error); anything else stays as it
/// is and leaves as `internal`.
pub(super) fn coded(lua: &mlua::Lua, error: anyhow::Error) -> anyhow::Error {
    let Some(value) = mare_shared::lua_error::recall_error(lua, &error.to_string()) else { return error };
    let (Ok(code), Ok(message)) = (value.get::<String>("code"), value.get::<String>("message")) else { return error };
    if code == "internal" || !(mare_shared::errors::is_core(&code) || mare_shared::errors::is_domain_code(&code)) {
        return error;
    }
    let data = match value.get::<Value>("data") {
        Ok(Value::Nil) | Err(_) => None,
        Ok(data) => mare_shared::lua::strict_lua_to_json(lua, data).ok(),
    };
    anyhow::Error::new(mare_kernel::ServiceCallError::Coded(mare_kernel::ErrorValue { code, message, data }))
}

/// The RPC error of a hand-written method's failure: a coded error keeps its code, anything else
/// is `internal` (sent with a fixed message, see RpcError::normalized).
pub(crate) fn failure(error: anyhow::Error) -> proto::RpcError {
    for cause in error.chain() {
        if let Some(coded) = cause.downcast_ref::<mare_kernel::ServiceCallError>() {
            return services::error(coded.clone());
        }
        if let Some(coded) = cause.downcast_ref::<mare_shared::errors::Coded>() {
            return proto::RpcError::coded(coded.code, coded.message.clone());
        }
        // an mlua error keeps an external cause out of the std chain
        if let Some(lua) = cause.downcast_ref::<mlua::Error>()
            && let Some(coded) = lua.downcast_ref::<mare_shared::errors::Coded>()
        {
            return proto::RpcError::coded(coded.code, coded.message.clone());
        }
    }
    proto::RpcError::core(proto::ErrorCode::Internal, error.to_string())
}

async fn call_service_inner(host: &Host, service: &str, method: &str, args: Vec<J>, check_tuple: bool) -> Result<J> {
    let lua = host.lua();
    let mut mv = MultiValue::new();
    for a in args {
        mv.push_back(json_to_lua(lua, &a)?);
    }
    let r = host.call_service(service, method, mv).await.map_err(|error| coded(lua, error))?;
    let mut iter = r.into_iter();
    let first = iter.next();
    if check_tuple
        && matches!(first, None | Some(Value::Nil) | Some(Value::Boolean(false)))
        && let Some(Value::String(err)) = iter.next()
    {
        anyhow::bail!("{}", err.to_string_lossy());
    }
    Ok(match first {
        Some(Value::Nil) | None => J::Null,
        Some(v) => lua_to_json(lua, v)?,
    })
}

pub(crate) async fn call_service(host: &Host, service: &str, method: &str, args: Vec<J>) -> Result<J> {
    call_service_inner(host, service, method, args, false).await
}

pub(crate) async fn call_service_checked(host: &Host, service: &str, method: &str, args: Vec<J>) -> Result<J> {
    call_service_inner(host, service, method, args, true).await
}

/// Active session identity `(id, session_epoch)` from `EventLog` + `session.info`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SessionIdentity {
    pub id: String,
    pub session_epoch: String,
}

impl SessionIdentity {
    pub(super) async fn current(host: &Host) -> Option<Self> {
        let (ev_id, ev_epoch) = host.kernel.event_log().session_identity();
        let info = host.session_info_json().await.ok();
        let info_id = info.as_ref().and_then(|v| v.get("session_id")).and_then(J::as_str).filter(|s| !s.is_empty());
        let id = info_id.map(str::to_string).or(ev_id)?;
        let session_epoch = info
            .as_ref()
            .and_then(|v| v.get("session_epoch"))
            .and_then(J::as_str)
            .map(str::to_string)
            .unwrap_or(ev_epoch);
        Some(Self { id, session_epoch })
    }

    pub(super) async fn current_for_runtime(rt: &SessionRuntime) -> Self {
        Self::current(&rt.host).await.unwrap_or_else(|| {
            let (_, ev_epoch) = rt.host.kernel.event_log().session_identity();
            Self { id: rt.session_id.clone(), session_epoch: ev_epoch }
        })
    }

    pub(super) fn expected_session(&self) -> Option<J> {
        Some(json!({ "session_id": self.id, "session_epoch": self.session_epoch }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ResolvedRegistrationScope {
    All,
    Session(String),
}

pub(super) fn session_error_to_rpc(err: SessionError) -> proto::RpcError {
    match err {
        SessionError::NotLoaded(sid) | SessionError::Closing(sid) => {
            proto::RpcError::coded("unknown_session", format!("session `{sid}` is not open in this process"))
        }
        SessionError::ShuttingDown | SessionError::ConfigWorkerClosed => {
            proto::RpcError::coded("unavailable", "host is shutting down")
        }
        SessionError::Unavailable(msg) => proto::RpcError::coded("unavailable", msg),
        SessionError::AlreadyLoaded(sid) | SessionError::Loading(sid) => {
            proto::RpcError::coded("busy", format!("session `{sid}` is already loaded"))
        }
        SessionError::ResumeFailed { code, message } => {
            let code = match code.as_deref() {
                Some("cancelled") => "cancelled",
                Some("session.unknown_item") => "session.unknown_item",
                _ => "invalid_arguments",
            };
            proto::RpcError::coded(code, message)
        }
        SessionError::WorkspaceMismatch { .. }
        | SessionError::SessionIdMismatch { .. }
        | SessionError::MissingSessionId => proto::RpcError::coded("invalid_arguments", err.to_string()),
        SessionError::HostFailed(e) => failure(e),
    }
}

fn protocol_method_uses_session_id(method: &str) -> bool {
    matches!(
        method,
        methods::OPTIONS_GET
            | methods::OPTIONS_SET
            | methods::EVENTS_SUBSCRIBE
            | methods::SERVICES_REGISTER
            | methods::UI_ATTACH
            | methods::UI_DETACH
    )
}

pub(super) fn spec_accepts_session_id(spec: &mare_kernel::contracts::MethodSpec) -> bool {
    spec.scope == mare_kernel::contracts::Scope::Session
        || (spec.scope == mare_kernel::contracts::Scope::Host
            && spec
                .params
                .get("properties")
                .and_then(J::as_object)
                .is_some_and(|props| props.contains_key("session_id")))
}

struct ContractTarget {
    named_rt: Option<Rc<SessionRuntime>>,
    workspace_rt: Option<Rc<SessionRuntime>>,
    spec: Option<mare_kernel::contracts::MethodSpec>,
}

fn resolve_contract_target(
    conn: &Conn,
    anchor: &Host,
    service: &str,
    method: &str,
    obj: Option<&serde_json::Map<String, J>>,
) -> ContractTarget {
    let named_rt = obj
        .and_then(|o| o.get("session_id"))
        .and_then(J::as_str)
        .filter(|s| !s.trim().is_empty())
        .and_then(|sid| conn.rpc.manager.session(sid).ok());
    let spec = named_rt
        .as_ref()
        .and_then(|rt| rt.host.kernel.service_method(service, method))
        .filter(spec_accepts_session_id)
        .or_else(|| anchor.kernel.service_method(service, method));
    let workspace_rt = if spec.as_ref().is_some_and(|spec| spec.scope == mare_kernel::contracts::Scope::Workspace) {
        obj.and_then(|o| o.get("cwd")).and_then(J::as_str).and_then(|cwd| resolve_workspace_host(conn, cwd).ok())
    } else {
        None
    };
    ContractTarget { named_rt, workspace_rt, spec }
}

fn enforce_peer_project_scope(conn: &Conn, action: &str, rt: &SessionRuntime) -> Result<(), proto::RpcError> {
    if conn.is_owner() || conn.rpc.attached_host.is_some() {
        return Ok(());
    }
    let caller = session_ext::caller_for_conn(conn, &rt.host);
    let fallback_cwd = conn.rpc.base_opts.borrow().cwd.clone();
    conn.rpc
        .manager
        .orchestrator()
        .check_project_scope(&caller, action, &fallback_cwd, Some(&rt.host.cwd), || {
            mare_host::OrchestratorError::unknown_session(format!("unknown session: {}", rt.session_id))
        })
        .map_err(session_ext::orchestrator_error_to_rpc)
}

fn request_target_host(conn: &Conn, method: &str, params: &Option<J>) -> Arc<Host> {
    let anchor = conn.rpc.anchor_host_for(conn).unwrap_or_else(|| conn.host.clone());
    let Some(obj) = params.as_ref().and_then(J::as_object) else { return anchor };
    if access::has_method(method) {
        if method == methods::BASELINE_READ {
            return obj
                .get("baseline_id")
                .and_then(J::as_str)
                .and_then(|id| conn.baseline_host(id))
                .map(|(h, _)| h)
                .unwrap_or(anchor);
        }
        if protocol_method_uses_session_id(method)
            && let Some(rt) = obj
                .get("session_id")
                .and_then(J::as_str)
                .filter(|s| !s.trim().is_empty())
                .and_then(|sid| conn.rpc.manager.session(sid).ok())
                .filter(|rt| enforce_peer_project_scope(conn, method, rt).is_ok())
        {
            return rt.host.clone();
        }
        return anchor;
    }
    let Some((service, operation)) = method.split_once('.') else { return anchor };
    let ContractTarget { named_rt, workspace_rt, spec, .. } =
        resolve_contract_target(conn, &anchor, service, operation, Some(obj));
    if let Some(spec) = spec {
        if spec.scope == mare_kernel::contracts::Scope::Workspace {
            if let Some(rt) = workspace_rt {
                return rt.host.clone();
            }
            return anchor;
        }
        if spec_accepts_session_id(&spec)
            && let Some(rt) = named_rt
        {
            return rt.host.clone();
        }
    }
    anchor
}

async fn find_session_runtime(conn: &Conn, sid: &str) -> Result<Rc<SessionRuntime>, proto::RpcError> {
    let rt = match conn.rpc.manager.session(sid) {
        Ok(rt) => rt,
        Err(SessionError::NotLoaded(_)) => {
            let mut found = None;
            for cand in conn.rpc.active_sessions() {
                if let Some(id) = SessionIdentity::current(&cand.host).await
                    && id.id == sid
                {
                    found = Some(cand);
                    break;
                }
            }
            found.ok_or_else(|| session_error_to_rpc(SessionError::NotLoaded(sid.to_owned())))?
        }
        Err(e) => return Err(session_error_to_rpc(e)),
    };
    enforce_peer_project_scope(conn, "session.target", &rt)?;
    Ok(rt)
}

async fn resolve_session_target(
    conn: &Conn,
    session_id: &str,
    expected_epoch: Option<&str>,
) -> Result<(Rc<SessionRuntime>, SessionIdentity), proto::RpcError> {
    if session_id.trim().is_empty() {
        return Err(proto::RpcError::core(proto::ErrorCode::InvalidArguments, "session_id must not be empty"));
    }
    if let Some(epoch) = expected_epoch
        && epoch.trim().is_empty()
    {
        return Err(proto::RpcError::core(proto::ErrorCode::InvalidArguments, "expected_epoch must not be empty"));
    }
    let rt = find_session_runtime(conn, session_id).await?;
    let identity = SessionIdentity::current_for_runtime(&rt).await;
    if let Some(epoch) = expected_epoch
        && identity.session_epoch != epoch
    {
        return Err(proto::RpcError::coded("stale_session", "the session changed since expected_epoch was obtained"));
    }
    Ok((rt, identity))
}

fn error_pair(e: proto::RpcError) -> (&'static str, String) {
    let code = match e.error_code() {
        Some("unknown_session") => "unknown_session",
        Some("stale_session") => "stale_session",
        Some("denied") => "denied",
        Some("unavailable") => "unavailable",
        Some("busy") => "busy",
        _ => "invalid_arguments",
    };
    (code, e.message().to_owned())
}

fn resolve_workspace_host(conn: &Conn, cwd: &str) -> Result<Rc<SessionRuntime>, proto::RpcError> {
    if cwd.trim().is_empty() {
        return Err(proto::RpcError::core(proto::ErrorCode::InvalidArguments, "cwd must not be empty"));
    }
    let req = dunce::canonicalize(std::path::Path::new(cwd)).unwrap_or_else(|_| std::path::PathBuf::from(cwd));
    let mut sessions = conn.rpc.active_sessions();
    // Prefer anchor session if its workspace matches so single-workspace calls stay deterministic.
    if let Ok(anchor_rt) = conn.rpc.anchor_session()
        && let Some(pos) = sessions.iter().position(|r| r.session_id == anchor_rt.session_id)
    {
        sessions.swap(0, pos);
    }
    for rt in sessions {
        let host_cwd = dunce::canonicalize(&rt.host.cwd).unwrap_or_else(|_| rt.host.cwd.clone());
        let host_root = rt.host.workspace_root();
        if req == host_cwd || req == host_root {
            enforce_peer_project_scope(conn, "workspace.target", &rt)?;
            return Ok(rt);
        }
    }
    Err(proto::RpcError::coded("unknown_session", format!("workspace `{cwd}` is not open in this process")))
}

async fn validate_registration_scope(
    conn: &Conn,
    scope: Option<proto::RegistrationScopeKind>,
    session_id: Option<&str>,
    require_explicit_scope: bool,
) -> Result<ResolvedRegistrationScope, proto::RpcError> {
    match scope {
        Some(proto::RegistrationScopeKind::All) => {
            let anchor = conn.rpc.anchor_host().unwrap_or_else(|| conn.host.clone());
            access::authorize(conn, &anchor, access::Op::GlobalScope).into_error("scope `all`")?;
            if session_id.is_some() {
                return Err(proto::RpcError::core(
                    proto::ErrorCode::InvalidArguments,
                    "session_id cannot be set when scope is `all`",
                ));
            }
            Ok(ResolvedRegistrationScope::All)
        }
        Some(proto::RegistrationScopeKind::Session) => {
            let sid = session_id.ok_or_else(|| {
                proto::RpcError::core(
                    proto::ErrorCode::InvalidArguments,
                    "session_id is required when scope is `session`",
                )
            })?;
            let (rt, _) = resolve_session_target(conn, sid, None).await?;
            Ok(ResolvedRegistrationScope::Session(rt.session_id.clone()))
        }
        None => {
            if let Some(sid) = session_id {
                let (rt, _) = resolve_session_target(conn, sid, None).await?;
                Ok(ResolvedRegistrationScope::Session(rt.session_id.clone()))
            } else if !conn.is_owner() {
                // identity: peers cannot default to global scope or implicit anchor sessions
                Err(proto::RpcError::core(proto::ErrorCode::InvalidArguments, "session_id is required"))
            } else if require_explicit_scope {
                Err(proto::RpcError::core(
                    proto::ErrorCode::InvalidArguments,
                    "session_id is required (or pass scope = \"all\")",
                ))
            } else {
                Ok(ResolvedRegistrationScope::All)
            }
        }
    }
}

fn resolve_registration_targets(
    conn: &Conn,
    scope: &ResolvedRegistrationScope,
) -> Result<Vec<Rc<SessionRuntime>>, proto::RpcError> {
    match scope {
        ResolvedRegistrationScope::All => Ok(conn.rpc.active_sessions()),
        ResolvedRegistrationScope::Session(sid) => {
            Ok(vec![conn.rpc.manager.session(sid).map_err(session_error_to_rpc)?])
        }
    }
}

async fn apply_capabilities_on_session(
    conn: &Rc<Conn>,
    session_id: &str,
    host: &Arc<Host>,
    caps: &proto::ClientCapabilities,
) -> Result<(), proto::RpcError> {
    let ctx_id = conn.ensure_session_ctx(session_id, host).await?;
    forward_ui::install_shell_mirror_on(&conn.rpc, session_id, host).map_err(|e| failure(e.into()))?;
    let can_answer = access::authorize(conn, host, access::Op::Answerer).is_allowed();
    let ui = (caps.ui || host.shell.is_responder(conn.id)) && can_answer;
    let approvals = caps.approvals && can_answer;
    let elicitation = caps.elicitation && can_answer;
    let wants_answerer = ui || approvals || elicitation;
    if wants_answerer {
        conn.acquire_answerer(host, session_id);
    }
    if ui {
        forward_ui::forward_ui_on(conn, session_id, host, ctx_id).map_err(|e| failure(e.into()))?;
    } else {
        host.answerer.restore_ui(host.lua(), conn.id);
    }
    forward_approvals::set_approvals_on(conn, session_id, host, ctx_id, approvals).map_err(|e| failure(e.into()))?;
    mcp::set_elicitation_on(conn, session_id, host, ctx_id, elicitation).map_err(|e| failure(e.into()))?;
    host.kernel.set_callback_channel(ctx_id, caps.callbacks.then(|| services::callback_channel(conn, session_id)));
    if !wants_answerer {
        host.answerer.release(host.lua(), conn.id, "answerer capabilities withdrawn");
    }
    Ok(())
}

/// Apply `desired` capabilities across the connection under `conn.cap_gate` (already held by caller).
/// Immediately enforces `old ∩ new` before awaiting, bumps `cap_revision` on changes, keeps policy
/// voters active (failing closed to deny when callbacks are withdrawn), and rolls back strictly to
/// `old ∩ new` on any failure or disconnect — never re-enabling withdrawn capabilities.
async fn apply_connection_capabilities(
    conn: &Rc<Conn>,
    mut desired: proto::ClientCapabilities,
) -> Result<proto::ClientCapabilities, proto::RpcError> {
    let is_owner = conn.is_owner();
    let anchor = conn.rpc.anchor_host().unwrap_or_else(|| conn.host.clone());
    let can_answer = access::authorize(conn, &anchor, access::Op::Answerer).is_allowed();
    desired.ui &= can_answer;
    desired.approvals &= can_answer;
    desired.elicitation &= can_answer;

    let old_caps = conn.caps.borrow().clone();
    let safe_caps = proto::ClientCapabilities {
        callbacks: old_caps.callbacks && desired.callbacks,
        ui: old_caps.ui && desired.ui,
        approvals: old_caps.approvals && desired.approvals,
        elicitation: old_caps.elicitation && desired.elicitation,
    };
    if desired != old_caps {
        conn.cap_revision.set(conn.cap_revision.get().wrapping_add(1));
    }

    // Withdraw before any await so in-flight callbacks/approvals/UI/elicitation fail closed immediately.
    *conn.caps.borrow_mut() = safe_caps.clone();
    let withdraw_ui = old_caps.ui && !desired.ui;
    let withdraw_approvals = old_caps.approvals && !desired.approvals;
    let withdraw_elicitation = old_caps.elicitation && !desired.elicitation;
    let withdraw_callbacks = old_caps.callbacks && !desired.callbacks;

    if withdraw_ui {
        for m in UI_PROMPT_METHODS {
            conn.cancel_pending(Some(m), "ui capability withdrawn");
        }
    }
    if withdraw_approvals {
        conn.cancel_pending(Some(methods::APPROVAL_REQUEST), "approvals capability withdrawn");
    }
    if withdraw_elicitation {
        conn.cancel_pending(Some(methods::MCP_ELICITATION), "elicitation capability withdrawn");
    }
    let withdraw_reg_permit = if withdraw_callbacks { conn.reg_gate.acquire().await.ok() } else { None };
    let withdrawn_callback_ids = if withdraw_callbacks {
        conn.cancel_pending_with(
            Some(methods::CALLBACK_CALL),
            proto::RpcError::coded("unsupported", "this connection no longer answers callback.call"),
        );
        tokio::task::yield_now().await;
        conn.global_registrations.borrow_mut().take_withdrawn_callback_ids()
    } else {
        Vec::new()
    };

    let mut ctx_hosts: Vec<(Arc<Host>, u64)> = conn.all_host_ctxs();
    for rt in conn.rpc.active_sessions() {
        if !ctx_hosts.iter().any(|(h, _)| Arc::ptr_eq(h, &rt.host))
            && let Some(cid) = conn.ctx_id_for_host(&rt.host)
        {
            ctx_hosts.push((rt.host.clone(), cid));
        }
    }
    if withdraw_ui || !can_answer {
        for (h, _) in &ctx_hosts {
            h.shell.clear_responder(conn.id);
        }
    }
    for (h, cid) in &ctx_hosts {
        let keep_attached_ui = !withdraw_ui && can_answer && h.shell.is_responder(conn.id);
        let release_slot = !desired.ui && !desired.approvals && !desired.elicitation && !keep_attached_ui;
        release_answerer_leases(conn, h, withdraw_ui, withdraw_approvals, withdraw_elicitation, release_slot);
        if withdraw_callbacks {
            h.kernel.set_callback_channel(*cid, None);
            for id in h.kernel.owner_registrations_matching(*cid, "service:") {
                let _ = h.kernel.release_registration(*cid, &id).await;
            }
            for id in &withdrawn_callback_ids {
                let _ = h.kernel.release_registration(*cid, id).await;
            }
        }
    }
    drop(withdraw_reg_permit);

    let mut applied: Vec<(String, Arc<Host>)> = Vec::new();
    let install_result: Result<(), proto::RpcError> = async {
        if is_owner {
            let sessions = conn.rpc.active_sessions();
            if sessions.is_empty() {
                let anchor = conn.rpc.anchor_host().unwrap_or_else(|| conn.host.clone());
                applied.push((String::new(), anchor.clone()));
                apply_capabilities_on_session(conn, "", &anchor, &desired).await?;
            } else {
                for rt in sessions {
                    if conn.closed.get() {
                        return Err(proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"));
                    }
                    applied.push((rt.session_id.clone(), rt.host.clone()));
                    apply_capabilities_on_session(conn, &rt.session_id, &rt.host, &desired).await?;
                }
            }
        } else {
            for (sid, h, cid) in conn.all_session_host_ctxs() {
                if conn.closed.get() {
                    return Err(proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"));
                }
                h.kernel.set_callback_channel(cid, desired.callbacks.then(|| services::callback_channel(conn, &sid)));
            }
        }
        if conn.closed.get() {
            return Err(proto::RpcError::core(proto::ErrorCode::InvalidRequest, "connection closed"));
        }
        Ok(())
    }
    .await;

    if let Err(err) = install_result {
        if safe_caps != old_caps || desired != safe_caps {
            conn.cap_revision.set(conn.cap_revision.get().wrapping_add(1));
        }
        if conn.closed.get() {
            *conn.caps.borrow_mut() = Default::default();
            if is_owner {
                for (sid, h) in applied {
                    h.shell.clear_responder(conn.id);
                    release_answerer_leases(conn, &h, true, true, true, true);
                    conn.cancel_answerer_pending_for_session(&sid, "connection closed");
                }
            }
            return Err(err);
        }
        *conn.caps.borrow_mut() = safe_caps.clone();
        if is_owner {
            for (sid, h) in applied {
                let _ = apply_capabilities_on_session(conn, &sid, &h, &safe_caps).await;
            }
        } else {
            for (sid, h, cid) in conn.all_session_host_ctxs() {
                h.kernel.set_callback_channel(cid, safe_caps.callbacks.then(|| services::callback_channel(conn, &sid)));
            }
        }
        return Err(err);
    }

    *conn.caps.borrow_mut() = desired.clone();
    Ok(desired)
}

async fn inject_global_registrations_on(conn: &Rc<Conn>, rt: &Rc<SessionRuntime>) {
    if !access::authorize(conn, &rt.host, access::Op::GlobalRegistrations).is_allowed() {
        return;
    }
    let Ok(_reg_permit) = conn.reg_gate.acquire().await else {
        return;
    };
    if !conn.is_ready() || conn.closed.get() {
        return;
    }
    let entries = conn.global_registrations.borrow().all_scoped_entries();
    if entries.is_empty() {
        return;
    }
    for (id, record) in entries {
        if !conn.global_registrations.borrow().by_id.contains_key(&id) {
            continue;
        }
        let outcome = match &record.kind {
            GlobalRegistrationKind::Contract { service, method, validated_arguments, callback_names, .. } => {
                services::inject_global_contract_on(conn, rt, &id, service, method, validated_arguments, callback_names)
                    .await
            }
            GlobalRegistrationKind::Service { definition } => {
                services::inject_global_service_on(conn, rt, &id, definition).await
            }
        };
        match outcome {
            Ok(()) => {
                if let Some(rec) = conn.global_registrations.borrow_mut().by_id.get_mut(&id) {
                    rec.active_session_instances.insert(rt.session_id.clone());
                }
            }
            Err(err) => {
                tracing::warn!("rpc global registration `{id}` inject into {}: {err}", rt.session_id);
                let plugin_label = match &record.kind {
                    GlobalRegistrationKind::Contract { service, .. } => service.as_str(),
                    GlobalRegistrationKind::Service { definition } => definition.name.as_str(),
                };
                rt.host.kernel.notify_error(
                    plugin_label,
                    &format!("global registration `{id}` inject into {}", rt.session_id),
                    &err,
                );
            }
        }
    }
}

fn ok() -> J {
    json!({})
}

fn new_turn_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros()).unwrap_or(0);
    format!("{:x}{:x}", t as u64, N.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod callback_tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn human_approval_can_wait_past_thirty_seconds_without_a_deadline() {
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(wait_callback(rx, None));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(31)).await;
        assert!(!task.is_finished(), "a human approval must not time out by default");
        tx.send(Ok(json!({"decision":"allow"}))).unwrap();
        assert_eq!(task.await.unwrap().unwrap().unwrap().unwrap()["decision"], "allow");

        let (_tx, rx) = oneshot::channel();
        let task = tokio::spawn(wait_callback(rx, Some(Duration::from_millis(10))));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(11)).await;
        assert!(task.await.unwrap().is_none(), "an explicit deadline must still expire");
    }

    #[test]
    fn rpc_error_mapping_and_session_identity_preserve_wire_codes() {
        // `failure` preserves ServiceCallError, Coded, mlua::Error::external(Coded), and falls back to Internal.
        let svc_err = mare_kernel::ServiceCallError::coded("agent.busy", "busy now");
        assert_eq!(failure(anyhow::Error::new(svc_err)).error_code(), Some("agent.busy"));

        let coded_err = mare_shared::errors::Coded { code: "denied", message: "nope".into() };
        assert_eq!(failure(anyhow::Error::new(coded_err)).error_code(), Some("denied"));

        let lua_coded =
            mlua::Error::external(mare_shared::errors::Coded { code: "stale_session", message: "stale".into() });
        assert_eq!(failure(anyhow::Error::new(lua_coded)).error_code(), Some("stale_session"));

        let plain = failure(anyhow::anyhow!("boom"));
        assert_eq!(plain.code(), -32603);
        assert_eq!(plain.error_code(), Some("internal"));

        // `session_error_to_rpc` maps every lifecycle error variant to its exact wire error code.
        assert_eq!(session_error_to_rpc(SessionError::ShuttingDown).error_code(), Some("unavailable"));
        assert_eq!(session_error_to_rpc(SessionError::ConfigWorkerClosed).error_code(), Some("unavailable"));
        assert_eq!(session_error_to_rpc(SessionError::Unavailable("down".into())).error_code(), Some("unavailable"));
        assert_eq!(session_error_to_rpc(SessionError::AlreadyLoaded("s1".into())).error_code(), Some("busy"));
        assert_eq!(session_error_to_rpc(SessionError::Loading("s1".into())).error_code(), Some("busy"));
        assert_eq!(
            session_error_to_rpc(SessionError::ResumeFailed { code: Some("cancelled".into()), message: "stop".into() })
                .error_code(),
            Some("cancelled")
        );
        assert_eq!(
            session_error_to_rpc(SessionError::ResumeFailed {
                code: Some("session.unknown_item".into()),
                message: "missing".into(),
            })
            .error_code(),
            Some("session.unknown_item")
        );
        assert_eq!(
            session_error_to_rpc(SessionError::ResumeFailed { code: None, message: "bad".into() }).error_code(),
            Some("invalid_arguments")
        );
        assert_eq!(session_error_to_rpc(SessionError::MissingSessionId).error_code(), Some("invalid_arguments"));
        assert_eq!(
            session_error_to_rpc(SessionError::HostFailed(anyhow::Error::new(mare_shared::errors::Coded {
                code: "denied",
                message: "blocked".into(),
            })))
            .error_code(),
            Some("denied")
        );

        // `error_pair` preserves structured codes and defaults unrecognised codes to `invalid_arguments`.
        for code in ["unknown_session", "stale_session", "denied", "unavailable", "busy"] {
            assert_eq!(error_pair(proto::RpcError::coded(code, "m")), (code, "m".into()));
        }
        assert_eq!(error_pair(proto::RpcError::coded("other", "m")), ("invalid_arguments", "m".into()));

        // `SessionIdentity::expected_session` encodes both id and epoch.
        let id = SessionIdentity { id: "s-9".into(), session_epoch: "ep-3".into() };
        assert_eq!(id.expected_session(), Some(json!({ "session_id": "s-9", "session_epoch": "ep-3" })));
    }
}

#[cfg(test)]
mod contract_schema_tests {
    /// Snapshot of the contract-derived half of `mare rpc schema` (the protocol half is
    /// mare-protocol's `schema.json`). After an intentional contract change:
    /// `UPDATE_SCHEMA=1 cargo test -p mare-cli contract_schema`.
    #[test]
    fn contract_schema_snapshot() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("contract-schema.json");
        let now = serde_json::to_value(super::contract_methods().unwrap()).unwrap();
        if std::env::var_os("UPDATE_SCHEMA").is_some() {
            std::fs::write(&path, serde_json::to_string_pretty(&now).unwrap() + "\n").unwrap();
        }
        let snap: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default()).unwrap_or_default();
        assert!(snap == now, "contract RPC schema changed — review, then UPDATE_SCHEMA=1");
        let methods = now.as_object().expect("contract methods map");
        for m in ["session.create", "session.load", "session.fork", "session.rewind", "session.update"] {
            let res = &methods[m]["result"];
            let req: Vec<&str> =
                res["required"].as_array().unwrap().iter().filter_map(serde_json::Value::as_str).collect();
            assert!(req.contains(&"session_epoch"), "{m} must require session_epoch in result schema: {res}");
        }
        let fork_props = methods["session.fork"]["params"]["properties"].as_object().unwrap();
        for removed in ["switch", "model", "effort"] {
            assert!(!fork_props.contains_key(removed), "session.fork must omit {removed}: {fork_props:?}");
        }
        for (name, spec) in methods {
            if let Some(props) = spec["params"]["properties"].as_object() {
                assert!(!props.contains_key("expected_session"), "{name} must not expose expected_session");
                assert!(!props.contains_key("generation"), "{name} must not expose generation");
            }
        }
        // the full catalog builds: no contract method collides with a protocol method
        super::schema_text().unwrap();
    }
}
