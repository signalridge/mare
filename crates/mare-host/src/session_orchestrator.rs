//! Session orchestrator and per-session runtime management.
//!
//! One orchestrator, two capacities:
//! - [`Capacity::Multi`]: each opened session owns an isolated [`SessionRuntime`] backed by its
//!   own `Arc<Host>` (Lua VM, `Kernel`, `EventLog`, `AnswererSlot`, `InboundHeld`,
//!   `SessionPromptReceipts`), running on the single-threaded Tokio `LocalSet`.
//! - [`Capacity::Single`]: a standalone or attached `Host` (holding only `std::sync::Weak<Host>`
//!   so there is never a `Host -> SessionOrchestrator -> SessionRuntime -> Host` strong cycle).
//!
//! Connection-specific concerns (option cloning, pre-`app/ready` capability binding, subscription
//! state, selector resolution via `session.list`) are injected through [`OrchestratorExt`].

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Weak as ArcWeak};
use std::time::Duration;

use mlua::{MultiValue, ObjectLike, Value};
use serde_json::{Value as J, json};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

mod receipts;
use receipts::{InFlightReceipt, InFlightReceiptGuard, ReceiptKey, ReceiptSlot, SessionOpReceipts, reserve_in_flight};

use crate::frontend::SESSION_SERVICE;
use crate::{Host, HostOptions, ShutdownReport, WebRegistry, WorkspaceId, WorkspaceRegistry};

type ConfigTask = Box<dyn FnOnce(Arc<Host>) -> Pin<Box<dyn Future<Output = ()>>>>;

/// Upper bound on automatic `begin_close` confirmation retries once stop is unconfirmed.
pub const DEFAULT_CLOSE_MAX_RETRIES: usize = 16;
/// Delay between `begin_close` retry polls once stop is unconfirmed.
pub const DEFAULT_CLOSE_RETRY_BACKOFF: Duration = Duration::from_millis(50);
/// Maximum number of `request_id` operation receipts retained per orchestrator.
pub const SESSION_OP_RECEIPT_CAPACITY: usize = 256;

/// Runtime capacity of a [`SessionOrchestrator`].
#[derive(Clone)]
pub enum Capacity {
    /// Standalone or attached single-host process; holds only a weak reference to avoid cycles.
    Single(ArcWeak<Host>),
    /// Multi-runtime coordinator (managed RPC, ACP) where each session owns its own `Host` VM.
    Multi,
}

/// Caller metadata passed to [`SessionOrchestrator`] operations.
///
/// The orchestrator never inspects connection ownership (`is_owner`) or plugin [`mare_kernel::Origin`];
/// permission predicates are evaluated at dispatch time and supplied here.
#[derive(Clone, Debug)]
pub struct OrchestratorCaller {
    /// Scope key isolating `request_id` receipts between callers (e.g. `"primary"` or `"peer:<id>"`).
    receipt_scope: String,
    /// Optional kernel export ctx id for mapping back to an RPC connection in [`OrchestratorExt`].
    ctx_id: Option<u64>,
    /// Optional direct connection id fallback when a wire connection has not allocated a ctx on
    /// `anchor` yet; `None` for in-process Lua callers.
    conn_id: Option<crate::ConnId>,
    /// Whether the caller is permitted unrestricted cross-project session operations
    /// (derived strictly from owner identity via [`OrchestratorCaller::from_access`]).
    unrestricted: bool,
    /// Whether selector ambiguity / archived error messages may include session details.
    may_list: bool,
    /// Whether the caller may resolve or load sessions by direct filesystem path.
    allow_path: bool,
    /// Whether the caller holds admin authority (local owner or `remote_admin = true`);
    /// non-admin callers cannot load by path and have restored permission presets / sandbox
    /// policies capped to the current host's effective ceiling.
    is_admin: bool,
}

/// Structured transport-level caller identity resolved by an [`OrchestratorExt`] for an active
/// kernel export caller context id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallerIdentity {
    pub conn_id: Option<crate::ConnId>,
    pub is_owner: bool,
    pub is_admin: bool,
    pub receipt_scope: String,
}

impl OrchestratorCaller {
    /// Host-internal / in-process caller with full local access and no RPC connection.
    pub fn host() -> Self {
        Self {
            receipt_scope: "primary".to_owned(),
            ctx_id: None,
            conn_id: None,
            unrestricted: true,
            may_list: true,
            allow_path: true,
            is_admin: true,
        }
    }

    /// Single constructor for caller capabilities. Only owner callers (`is_owner == true`) are
    /// unrestricted across projects, and only admin owners (`is_owner && is_admin`) are permitted
    /// to load sessions by direct filesystem path or restore presets/sandboxes above the current
    /// host ceiling; granting `session.update` to a peer never escalates confinement or path access.
    pub fn from_access(
        host: &Host,
        ctx_id: Option<u64>,
        conn_id: Option<crate::ConnId>,
        is_owner: bool,
        is_admin: bool,
        receipt_scope: String,
    ) -> Self {
        let admin_owner = is_owner && is_admin;
        let may_list = is_owner || host.kernel.permitted_for_caller(ctx_id, SESSION_SERVICE, "list");
        Self {
            receipt_scope,
            ctx_id,
            conn_id,
            unrestricted: is_owner,
            may_list,
            allow_path: admin_owner,
            is_admin: admin_owner,
        }
    }

    pub fn receipt_scope(&self) -> &str {
        &self.receipt_scope
    }

    pub fn ctx_id(&self) -> Option<u64> {
        self.ctx_id
    }

    pub fn conn_id(&self) -> Option<crate::ConnId> {
        self.conn_id
    }

    pub fn is_unrestricted(&self) -> bool {
        self.unrestricted
    }

    pub fn may_list(&self) -> bool {
        self.may_list
    }

    pub fn allow_path(&self) -> bool {
        self.allow_path
    }

    pub fn is_admin(&self) -> bool {
        self.is_admin
    }
}

/// Coded error returned by [`SessionOrchestrator`] lifecycle operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrchestratorError {
    pub code: String,
    pub message: String,
}

impl OrchestratorError {
    pub fn coded(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }

    pub fn invalid_arguments(message: impl Into<String>) -> Self {
        Self::coded("invalid_arguments", message)
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::coded("denied", message)
    }

    pub fn unknown_session(message: impl Into<String>) -> Self {
        Self::coded("unknown_session", message)
    }

    pub fn stale_session(message: impl Into<String>) -> Self {
        Self::coded("stale_session", message)
    }

    pub fn request_id_conflict(message: impl Into<String>) -> Self {
        Self::coded("request_id_conflict", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::coded("internal", message)
    }

    pub fn from_session_error(err: SessionError) -> Self {
        match err {
            SessionError::NotLoaded(sid) => Self::unknown_session(format!("unknown session: {sid}")),
            SessionError::Closing(sid) => Self::coded("busy", format!("session is closing: {sid}")),
            SessionError::Loading(sid) => Self::coded("busy", format!("session is already loading: {sid}")),
            SessionError::AlreadyLoaded(sid) => {
                Self::coded("session_already_loaded", format!("session is already active: {sid}"))
            }
            SessionError::ShuttingDown => Self::coded("unavailable", "host manager is shutting down"),
            SessionError::Unavailable(msg) => Self::coded("unavailable", msg),
            SessionError::ResumeFailed { code, message } => {
                let code = match code.as_deref() {
                    Some("cancelled") => "cancelled",
                    Some("session.unknown_item") => "session.unknown_item",
                    _ => "invalid_arguments",
                };
                Self::coded(code, message)
            }
            SessionError::WorkspaceMismatch { session_id, workspace } => {
                Self::denied(format!("session `{session_id}` is not attached to workspace `{workspace}`"))
            }
            other => Self::internal(other.to_string()),
        }
    }
}

impl std::fmt::Display for OrchestratorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for OrchestratorError {}

/// Frontend / transport extension hooks for [`SessionOrchestrator`].
///
/// Implemented in `mare-cli` (`rpc/session_ext.rs`) so connection state, capability bindings,
/// runtime defaults, and `session.list` selector resolution stay out of `mare-host`.
pub trait OrchestratorExt {
    /// Clone clean [`HostOptions`] for a newly created, loaded, or forked session.
    fn clean_opts(
        &self,
        cwd_override: Option<&str>,
        no_session_override: Option<bool>,
    ) -> Result<HostOptions, OrchestratorError>;

    /// Hook invoked on a newly loaded [`Host`] immediately before `app/ready` so UI/capability
    /// bridges are wired before startup restore listeners inspect `mare.ui.connected()`.
    fn before_ready(
        &self,
        _caller: &OrchestratorCaller,
        _pre_sid: Option<String>,
        _host: Arc<Host>,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async {})
    }

    /// Clean up any per-connection context bound in [`Self::before_ready`] if loading fails.
    fn rollback_before_ready(&self, _caller: &OrchestratorCaller, _sid: &str, _host: &Arc<Host>) {}

    /// Apply process-wide runtime defaults (e.g. default model / effort) to a newly created session.
    fn apply_defaults(&self, _rt: &Rc<SessionRuntime>) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async {})
    }

    /// Notify frontend/transport that a session was opened or switched to.
    ///
    /// In-process Lua callers have no connection (`caller.ctx_id == None`); implementations must
    /// not perform per-connection subscription side-effects for them.
    fn opened(
        &self,
        _caller: &OrchestratorCaller,
        _rt: &Rc<SessionRuntime>,
        _make_default: bool,
        _subscribe: Option<bool>,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async {})
    }

    /// Notify frontend/transport that a session was closed.
    fn closed(&self, _session_id: &str) {}

    /// Map an active export caller context id on `host` back to its structured transport identity
    /// (`conn_id`, `is_owner`, and structured `receipt_scope`).
    fn caller_identity(&self, _host: &Arc<Host>, _ctx_id: u64) -> Option<CallerIdentity> {
        None
    }

    /// Map an active export caller context id on `host` back to its transport connection id.
    fn conn_id_for_ctx(&self, host: &Arc<Host>, ctx_id: u64) -> Option<crate::ConnId> {
        self.caller_identity(host, ctx_id).and_then(|id| id.conn_id)
    }

    /// Resolve a session selector (`session_id`, prefix, name, or path) to its `.jsonl` path.
    fn resolve_selector(
        &self,
        anchor: &Arc<Host>,
        _caller: &OrchestratorCaller,
        selector: &str,
        include_archived: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, OrchestratorError>>>> {
        let anchor = Arc::clone(anchor);
        let sel = selector.to_owned();
        Box::pin(async move { resolve_selector_via_list(&anchor, &sel, include_archived).await })
    }
}

/// Default no-op [`OrchestratorExt`] for standalone hosts without an attached RPC runtime.
pub struct NoopOrchestratorExt;

impl OrchestratorExt for NoopOrchestratorExt {
    fn clean_opts(
        &self,
        _cwd_override: Option<&str>,
        _no_session_override: Option<bool>,
    ) -> Result<HostOptions, OrchestratorError> {
        Err(OrchestratorError::coded("unavailable", "session options builder is not configured"))
    }
}

/// Composite [`OrchestratorExt`] dispatching across temporarily attached layers (newest first)
/// before falling back to the base extension.
struct LayeredOrchestratorExt {
    layers: Vec<Rc<dyn OrchestratorExt>>,
    base: Rc<dyn OrchestratorExt>,
}

impl OrchestratorExt for LayeredOrchestratorExt {
    fn caller_identity(&self, host: &Arc<Host>, ctx_id: u64) -> Option<CallerIdentity> {
        for layer in &self.layers {
            if let Some(id) = layer.caller_identity(host, ctx_id) {
                return Some(id);
            }
        }
        self.base.caller_identity(host, ctx_id)
    }

    fn conn_id_for_ctx(&self, host: &Arc<Host>, ctx_id: u64) -> Option<crate::ConnId> {
        for layer in &self.layers {
            if let Some(id) = layer.conn_id_for_ctx(host, ctx_id) {
                return Some(id);
            }
        }
        self.base.conn_id_for_ctx(host, ctx_id)
    }

    fn clean_opts(
        &self,
        cwd_override: Option<&str>,
        no_session_override: Option<bool>,
    ) -> Result<HostOptions, OrchestratorError> {
        for layer in &self.layers {
            match layer.clean_opts(cwd_override, no_session_override) {
                Ok(opts) => return Ok(opts),
                Err(err) if err.code == "unavailable" => continue,
                Err(err) => return Err(err),
            }
        }
        self.base.clean_opts(cwd_override, no_session_override)
    }

    fn before_ready(
        &self,
        caller: &OrchestratorCaller,
        pre_sid: Option<String>,
        host: Arc<Host>,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        let layers = self.layers.clone();
        let base = Rc::clone(&self.base);
        let caller = caller.clone();
        Box::pin(async move {
            for layer in &layers {
                layer.before_ready(&caller, pre_sid.clone(), Arc::clone(&host)).await;
            }
            base.before_ready(&caller, pre_sid, host).await;
        })
    }

    fn rollback_before_ready(&self, caller: &OrchestratorCaller, sid: &str, host: &Arc<Host>) {
        for layer in &self.layers {
            layer.rollback_before_ready(caller, sid, host);
        }
        self.base.rollback_before_ready(caller, sid, host);
    }

    fn apply_defaults(&self, rt: &Rc<SessionRuntime>) -> Pin<Box<dyn Future<Output = ()>>> {
        if let Some(top) = self.layers.first() {
            return top.apply_defaults(rt);
        }
        self.base.apply_defaults(rt)
    }

    fn opened(
        &self,
        caller: &OrchestratorCaller,
        rt: &Rc<SessionRuntime>,
        make_default: bool,
        subscribe: Option<bool>,
    ) -> Pin<Box<dyn Future<Output = ()>>> {
        let layers = self.layers.clone();
        let base = Rc::clone(&self.base);
        let caller = caller.clone();
        let rt = Rc::clone(rt);
        Box::pin(async move {
            for layer in &layers {
                layer.opened(&caller, &rt, make_default, subscribe).await;
            }
            base.opened(&caller, &rt, make_default, subscribe).await;
        })
    }

    fn closed(&self, session_id: &str) {
        for layer in &self.layers {
            layer.closed(session_id);
        }
        self.base.closed(session_id);
    }

    fn resolve_selector(
        &self,
        anchor: &Arc<Host>,
        caller: &OrchestratorCaller,
        selector: &str,
        include_archived: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, OrchestratorError>>>> {
        if let Some(top) = self.layers.first() {
            return top.resolve_selector(anchor, caller, selector, include_archived);
        }
        self.base.resolve_selector(anchor, caller, selector, include_archived)
    }
}

/// Errors returned by [`SessionOrchestrator`] / [`crate::HostManager`] session lifecycle and lookup operations.
#[derive(Debug)]
pub enum SessionError {
    NotLoaded(String),
    Closing(String),
    Loading(String),
    AlreadyLoaded(String),
    ShuttingDown,
    Unavailable(String),
    MissingSessionId,
    SessionIdMismatch { expected: String, actual: Option<String> },
    ResumeFailed { code: Option<String>, message: String },
    WorkspaceMismatch { session_id: String, workspace: String },
    ConfigWorkerClosed,
    HostFailed(anyhow::Error),
}

impl SessionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotLoaded(_) => "session_not_loaded",
            Self::Closing(_) => "session_closing",
            Self::Loading(_) => "session_loading",
            Self::AlreadyLoaded(_) => "session_already_loaded",
            Self::ShuttingDown => "shutting_down",
            Self::Unavailable(_) => "unavailable",
            Self::ResumeFailed { .. } => "resume_failed",
            Self::WorkspaceMismatch { .. } => "permission_denied",
            Self::MissingSessionId | Self::SessionIdMismatch { .. } | Self::ConfigWorkerClosed => "internal",
            Self::HostFailed(_) => "internal",
        }
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLoaded(id) => write!(f, "session `{id}` is not loaded"),
            Self::Closing(id) => write!(f, "session is closing: {id}"),
            Self::Loading(id) => write!(f, "session is already loading: {id}"),
            Self::AlreadyLoaded(id) => write!(f, "session is already active: {id}"),
            Self::ShuttingDown => write!(f, "host manager is shutting down"),
            Self::Unavailable(msg) => write!(f, "{msg}"),
            Self::MissingSessionId => write!(f, "session service did not return a session id"),
            Self::SessionIdMismatch { expected, actual } => {
                write!(f, "resumed session id mismatch: expected `{expected}`, got `{actual:?}`")
            }
            Self::ResumeFailed { message, .. } => write!(f, "{message}"),
            Self::WorkspaceMismatch { session_id, workspace } => {
                write!(f, "session `{session_id}` is not attached to workspace `{workspace}`")
            }
            Self::ConfigWorkerClosed => write!(f, "session config worker closed"),
            Self::HostFailed(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// Live runtime state for one loaded session inside [`SessionOrchestrator`].
pub struct SessionRuntime {
    pub session_id: String,
    pub session_epoch: String,
    pub workspace: WorkspaceId,
    pub ephemeral: bool,
    pub host: Arc<Host>,
    pub cancel: CancellationToken,
    config_tx: mpsc::UnboundedSender<ConfigTask>,
}

impl SessionRuntime {
    fn new(session_id: String, ephemeral: bool, host: Arc<Host>) -> Self {
        let (_, session_epoch) = host.kernel.event_log().session_identity();
        let workspace = host.workspace.id().clone();
        let cancel = CancellationToken::new();
        let (config_tx, mut config_rx) = mpsc::unbounded_channel::<ConfigTask>();
        let weak_host = Arc::downgrade(&host);
        let worker_cancel = cancel.clone();
        // Serialize config mutations per session in receive order without blocking other sessions,
        // holding only Weak<Host> and stopping immediately when `cancel` fires.
        tokio::task::spawn_local(async move {
            loop {
                let task = tokio::select! {
                    biased;
                    _ = worker_cancel.cancelled() => break,
                    next = config_rx.recv() => match next {
                        Some(t) => t,
                        None => break,
                    },
                };
                let Some(cfg_host) = weak_host.upgrade() else { break };
                let barrier_ok = tokio::select! {
                    biased;
                    _ = worker_cancel.cancelled() => false,
                    _ = cfg_host.kernel.reload_barrier() => true,
                };
                if !barrier_ok {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = worker_cancel.cancelled() => break,
                    () = task(cfg_host) => {}
                }
            }
        });
        Self { session_id, session_epoch, workspace, ephemeral, host, cancel, config_tx }
    }

    fn with_rekeyed_identity(&self, session_id: String, session_epoch: String) -> Self {
        Self {
            session_id,
            session_epoch,
            workspace: self.workspace.clone(),
            ephemeral: self.ephemeral,
            host: Arc::clone(&self.host),
            cancel: self.cancel.clone(),
            config_tx: self.config_tx.clone(),
        }
    }

    /// Enqueue a configuration mutation on this session's serialized FIFO config queue and return
    /// a receiver for its result.
    pub fn enqueue_config<F, Fut, T>(&self, f: F) -> Result<oneshot::Receiver<T>, SessionError>
    where
        F: FnOnce(Arc<Host>) -> Fut + 'static,
        Fut: Future<Output = T> + 'static,
        T: 'static,
    {
        if self.cancel.is_cancelled() {
            return Err(SessionError::ConfigWorkerClosed);
        }
        let (tx, rx) = oneshot::channel();
        let task: ConfigTask = Box::new(move |host| {
            Box::pin(async move {
                let res = f(host).await;
                let _ = tx.send(res);
            })
        });
        self.config_tx.send(task).map_err(|_| SessionError::ConfigWorkerClosed)?;
        Ok(rx)
    }

    /// Run a configuration mutation on this session's serialized FIFO config queue.
    pub async fn run_config<F, Fut, T>(&self, f: F) -> Result<T, SessionError>
    where
        F: FnOnce(Arc<Host>) -> Fut + 'static,
        Fut: Future<Output = T> + 'static,
        T: 'static,
    {
        self.enqueue_config(f)?.await.map_err(|_| SessionError::ConfigWorkerClosed)
    }
}

struct ClosingEntry {
    done: watch::Sender<bool>,
    handle: Option<tokio::task::JoinHandle<()>>,
    latest_report: Rc<RefCell<Option<ShutdownReport>>>,
}

struct ClosingGuard {
    orch: Weak<SessionOrchestrator>,
    sid: String,
    done: watch::Sender<bool>,
}

impl Drop for ClosingGuard {
    fn drop(&mut self) {
        if let Some(orch) = self.orch.upgrade() {
            orch.closing.borrow_mut().remove(&self.sid);
        }
        self.done.send_replace(true);
    }
}

#[derive(Default)]
struct LoadTracker {
    count: Cell<usize>,
    done: Notify,
}

struct LoadGuard {
    tracker: Rc<LoadTracker>,
}

impl LoadGuard {
    fn acquire(tracker: &Rc<LoadTracker>) -> Self {
        tracker.count.set(tracker.count.get() + 1);
        Self { tracker: Rc::clone(tracker) }
    }
}

impl Drop for LoadGuard {
    fn drop(&mut self) {
        let prev = self.tracker.count.get();
        self.tracker.count.set(prev.saturating_sub(1));
        if prev <= 1 {
            self.tracker.done.notify_waiters();
        }
    }
}

struct LoadingGuard {
    orch: Weak<SessionOrchestrator>,
    sid: Option<String>,
}

impl LoadingGuard {
    fn disarm(&mut self) {
        if let Some(sid) = self.sid.take()
            && let Some(orch) = self.orch.upgrade()
        {
            orch.loading.borrow_mut().remove(&sid);
        }
    }
}

impl Drop for LoadingGuard {
    fn drop(&mut self) {
        self.disarm();
    }
}

fn unconfirmed_shutdown_report(task: &'static str) -> ShutdownReport {
    ShutdownReport { unconfirmed_tasks: vec![task], ..ShutdownReport::default() }
}

fn detach_runtime_host(runtime: Rc<SessionRuntime>) -> Arc<Host> {
    let host = runtime.host.clone();
    runtime.cancel.cancel();
    drop(runtime);
    host
}

async fn abort_and_shutdown(host: &Host) -> ShutdownReport {
    let abort_timed_out = tokio::time::timeout(host.shutdown_grace(), host.abort()).await.is_err();
    let mut report = host.shutdown_report().await;
    if abort_timed_out && !report.unconfirmed_tasks.contains(&"abort") {
        report.unconfirmed_tasks.push("abort");
    }
    report
}

/// Per-session shutdown result collected by [`SessionOrchestrator::shutdown_all_report`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionShutdownEntry {
    pub session_id: String,
    pub report: ShutdownReport,
}

/// Aggregated quiescent drain report returned by [`SessionOrchestrator::shutdown_all_report`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ManagerShutdownReport {
    pub sessions: Vec<SessionShutdownEntry>,
}

impl ManagerShutdownReport {
    pub fn stop_unconfirmed(&self) -> bool {
        self.sessions.iter().any(|entry| entry.report.stop_unconfirmed())
    }
}

fn insert_restore_ceiling(map: &mut serde_json::Map<String, J>, restore_ceiling: Option<(Option<String>, Option<J>)>) {
    if let Some((perm_ceiling, sb_ceiling)) = restore_ceiling {
        if let Some(preset) = perm_ceiling {
            map.insert("restore_permission_ceiling".into(), J::String(preset));
        }
        if let Some(sb) = sb_ceiling {
            map.insert("restore_sandbox_ceiling".into(), sb);
        }
    }
}

fn new_session_switch_args(
    cwd: Option<&str>,
    name: Option<&str>,
    restore_ceiling: Option<(Option<String>, Option<J>)>,
) -> J {
    let mut sw = serde_json::Map::new();
    sw.insert("kind".into(), J::String("new".into()));
    if let Some(c) = cwd {
        sw.insert("cwd".into(), J::String(c.to_owned()));
    }
    if let Some(n) = name {
        sw.insert("name".into(), J::String(n.to_owned()));
    }
    insert_restore_ceiling(&mut sw, restore_ceiling);
    J::Object(sw)
}

fn resolve_header_cwd(header: Option<&J>) -> Result<Option<PathBuf>, OrchestratorError> {
    let Some(raw) = header.and_then(|v| v.get("cwd")).and_then(J::as_str) else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(OrchestratorError::invalid_arguments("session directory is invalid"));
    }
    let canon = canonical_dir(path);
    if !canon.is_dir() {
        return Err(OrchestratorError::invalid_arguments("session directory no longer exists"));
    }
    Ok(Some(canon))
}

/// Outcome of resolving a `session.close` selector in [`Capacity::Single`] mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SingleCloseMatch {
    /// Selector matches the active session (exact id, path, unique resolved id, or ≥4-char prefix).
    CloseActive,
    /// Selector resolved to another persisted session that is not currently active (no-op in Single mode).
    NoopUnloaded,
}

/// Canonicalize a directory or file path best-effort for project-scope and path equality checks.
pub fn canonical_dir(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Match a `session.close` selector against the active session in [`Capacity::Single`] mode,
/// preserving exact id, path, ≥4-char prefix, and ambiguity semantics from `session/lifecycle.lua`.
pub fn match_single_close_selector(
    selector: &str,
    active_id: Option<&str>,
    active_path: Option<&str>,
    resolved_target: Result<Option<&str>, (&str, String)>,
) -> Result<SingleCloseMatch, OrchestratorError> {
    if selector.is_empty() {
        return Err(OrchestratorError::invalid_arguments("session_id must be a non-empty string"));
    }
    let exact_active = active_id == Some(selector)
        || active_path
            .is_some_and(|p| selector == p || canonical_dir(Path::new(selector)) == canonical_dir(Path::new(p)));
    if exact_active {
        return Ok(SingleCloseMatch::CloseActive);
    }
    let prefix_active = active_id.is_some_and(|id| selector.len() >= 4 && id.starts_with(selector));
    match resolved_target {
        Err(("invalid_arguments", msg)) => Err(OrchestratorError::invalid_arguments(msg)),
        Err((code, msg)) => {
            if prefix_active {
                Ok(SingleCloseMatch::CloseActive)
            } else {
                Err(OrchestratorError::coded(code, msg))
            }
        }
        Ok(None) => {
            if prefix_active {
                Ok(SingleCloseMatch::CloseActive)
            } else {
                Err(OrchestratorError::unknown_session(format!("no session matches `{selector}`")))
            }
        }
        Ok(Some(target_id)) => {
            if active_id == Some(target_id) {
                Ok(SingleCloseMatch::CloseActive)
            } else if prefix_active {
                Err(OrchestratorError::invalid_arguments(format!("session id `{selector}` is ambiguous")))
            } else {
                Ok(SingleCloseMatch::NoopUnloaded)
            }
        }
    }
}

type RekeyHook = Box<dyn Fn(&str, &str, &Rc<SessionRuntime>)>;

/// Unified session orchestrator supporting both [`Capacity::Single`] and [`Capacity::Multi`].
pub struct SessionOrchestrator {
    self_weak: Weak<Self>,
    capacity: Capacity,
    sessions: RefCell<HashMap<String, Rc<SessionRuntime>>>,
    closing: RefCell<HashMap<String, ClosingEntry>>,
    loading: RefCell<HashSet<String>>,
    /// Session IDs reserved by in-flight `kernel.begin_session_switch` handles (`sid -> host.instance_id`).
    reserved: RefCell<HashMap<String, u64>>,
    shutting_down: Cell<bool>,
    shutdown_cancel: CancellationToken,
    in_flight_shutdown: RefCell<Option<watch::Receiver<Option<ManagerShutdownReport>>>>,
    load_tracker: Rc<LoadTracker>,
    workspaces: WorkspaceRegistry,
    web: Rc<WebRegistry>,
    next_handle_id: Cell<u64>,
    rekey_hooks: RefCell<Vec<(u64, RekeyHook)>>,
    project_cwd: RefCell<Option<PathBuf>>,
    op_receipts: RefCell<SessionOpReceipts>,
    in_flight_receipts: RefCell<HashMap<ReceiptKey, InFlightReceipt>>,
    ext: RefCell<Rc<dyn OrchestratorExt>>,
    ext_layers: RefCell<Vec<(u64, Rc<dyn OrchestratorExt>)>>,
}

impl SessionOrchestrator {
    fn new_inner(self_weak: Weak<Self>, capacity: Capacity, project_cwd: Option<PathBuf>) -> Self {
        Self {
            self_weak,
            capacity,
            sessions: RefCell::new(HashMap::new()),
            closing: RefCell::new(HashMap::new()),
            loading: RefCell::new(HashSet::new()),
            reserved: RefCell::new(HashMap::new()),
            shutting_down: Cell::new(false),
            shutdown_cancel: CancellationToken::new(),
            in_flight_shutdown: RefCell::new(None),
            load_tracker: Rc::new(LoadTracker::default()),
            workspaces: WorkspaceRegistry::new(),
            web: Rc::new(WebRegistry::new()),
            next_handle_id: Cell::new(1),
            rekey_hooks: RefCell::new(Vec::new()),
            project_cwd: RefCell::new(project_cwd),
            op_receipts: RefCell::new(SessionOpReceipts::new(SESSION_OP_RECEIPT_CAPACITY)),
            in_flight_receipts: RefCell::new(HashMap::new()),
            ext: RefCell::new(Rc::new(NoopOrchestratorExt)),
            ext_layers: RefCell::new(Vec::new()),
        }
    }

    fn alloc_handle_id(&self) -> u64 {
        let id = self.next_handle_id.get();
        self.next_handle_id.set(id.wrapping_add(1).max(1));
        id
    }

    /// Create a [`Capacity::Multi`] session orchestrator.
    pub fn new_multi() -> Rc<Self> {
        Rc::new_cyclic(|weak| Self::new_inner(weak.clone(), Capacity::Multi, None))
    }

    /// Create a [`Capacity::Single`] session orchestrator bound weakly to `host`.
    pub fn new_single(host: &Arc<Host>) -> Rc<Self> {
        let cwd = canonical_dir(&host.cwd);
        Rc::new_cyclic(|weak| Self::new_inner(weak.clone(), Capacity::Single(Arc::downgrade(host)), Some(cwd)))
    }

    /// Whether this orchestrator runs in multi-runtime mode.
    pub fn is_multi(&self) -> bool {
        matches!(self.capacity, Capacity::Multi)
    }

    /// Current capacity (`Single` or `Multi`).
    pub fn capacity(&self) -> &Capacity {
        &self.capacity
    }

    /// Attach the base transport/frontend [`OrchestratorExt`] implementation.
    pub fn set_ext(&self, ext: Rc<dyn OrchestratorExt>) {
        *self.ext.borrow_mut() = ext;
    }

    /// Push a removable [`OrchestratorExt`] layer above the base extension (for attached
    /// connections sharing a single-host orchestrator). Returns a handle for [`Self::remove_ext`].
    pub fn push_ext(&self, ext: Rc<dyn OrchestratorExt>) -> u64 {
        let id = self.alloc_handle_id();
        self.ext_layers.borrow_mut().push((id, ext));
        id
    }

    /// Remove a previously pushed [`OrchestratorExt`] layer by handle id.
    pub fn remove_ext(&self, id: u64) {
        self.ext_layers.borrow_mut().retain(|(hid, _)| *hid != id);
    }

    /// Number of currently attached removable [`OrchestratorExt`] layers.
    pub fn ext_layer_count(&self) -> usize {
        self.ext_layers.borrow().len()
    }

    /// Current [`OrchestratorExt`] implementation.
    pub fn ext(&self) -> Rc<dyn OrchestratorExt> {
        let layers = self.ext_layers.borrow();
        if layers.is_empty() {
            return Rc::clone(&self.ext.borrow());
        }
        Rc::new(LayeredOrchestratorExt {
            layers: layers.iter().rev().map(|(_, ext)| Rc::clone(ext)).collect(),
            base: Rc::clone(&self.ext.borrow()),
        })
    }

    /// Set the canonical project root used by [`Self::check_project_scope`].
    pub fn set_project_cwd(&self, cwd: &Path) {
        *self.project_cwd.borrow_mut() = Some(canonical_dir(cwd));
    }

    /// Canonical project root used for project-scope confinement checks.
    pub fn project_cwd(&self) -> Option<PathBuf> {
        self.project_cwd.borrow().clone()
    }

    /// Single source of truth for project-scope resource confinement checks.
    ///
    /// Unrestricted callers pass; restricted callers may only target sessions/directories whose
    /// canonical path equals the orchestrator's project root.
    pub fn check_project_scope(
        &self,
        caller: &OrchestratorCaller,
        _op: &str,
        fallback_project_cwd: &Path,
        target_cwd: Option<&Path>,
        err: impl FnOnce() -> OrchestratorError,
    ) -> Result<(), OrchestratorError> {
        if caller.unrestricted {
            return Ok(());
        }
        let project_cwd = self.project_cwd().unwrap_or_else(|| canonical_dir(fallback_project_cwd));
        if target_cwd.is_some_and(|dir| canonical_dir(dir) == project_cwd) { Ok(()) } else { Err(err()) }
    }

    /// Look up a `request_id` receipt for `(caller_scope, op, req_id)`, verifying parameter fingerprint.
    pub fn lookup_receipt(
        &self,
        caller_scope: &str,
        op: &str,
        req_id: Option<&str>,
        fp: &J,
    ) -> Result<Option<(String, J)>, OrchestratorError> {
        let Some(rid) = req_id else {
            return Ok(None);
        };
        let Some((prev_fp, prev_sid, prev_snap)) = self.op_receipts.borrow_mut().get(caller_scope, op, rid) else {
            return Ok(None);
        };
        if &prev_fp != fp {
            return Err(OrchestratorError::request_id_conflict("request_id was reused with different parameters"));
        }
        Ok(Some((prev_sid, prev_snap)))
    }

    /// Record a `request_id` receipt in the bounded LRU cache.
    pub fn store_receipt(&self, caller_scope: &str, op: &str, req_id: Option<String>, fp: J, sid: &str, snap: &J) {
        if let Some(rid) = req_id {
            self.op_receipts.borrow_mut().put(
                caller_scope.to_owned(),
                op.to_owned(),
                rid,
                fp,
                sid.to_owned(),
                snap.clone(),
            );
        }
    }

    /// Remove all cached `request_id` receipts associated with `session_id` (e.g. on `session/deleted`).
    pub fn remove_receipts_for_session(&self, session_id: &str) {
        self.op_receipts.borrow_mut().remove_session(session_id);
    }

    /// Current number of cached operation receipts (bounded at [`SESSION_OP_RECEIPT_CAPACITY`]).
    pub fn receipts_len(&self) -> usize {
        self.op_receipts.borrow().len()
    }

    async fn replay_receipt(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        op: &str,
        req_id: Option<&str>,
        fp: &J,
        subscribe: Option<bool>,
    ) -> Result<Option<J>, OrchestratorError> {
        let Some((prev_sid, prev_snap)) = self.lookup_receipt(&caller.receipt_scope, op, req_id, fp)? else {
            return Ok(None);
        };
        let snap_epoch = prev_snap.get("session_epoch").and_then(J::as_str);
        match &self.capacity {
            Capacity::Multi => {
                if let Ok(rt) = self.session(&prev_sid)
                    && snap_epoch == Some(rt.host.kernel.event_log().session_identity().1.as_str())
                {
                    self.ext().opened(caller, &rt, true, subscribe).await;
                    return Ok(Some(prev_snap));
                }
            }
            Capacity::Single(weak_host) => {
                if let Some(host) = weak_host.upgrade()
                    && let (Some(live_sid), live_epoch) = host.kernel.event_log().session_identity()
                    && live_sid == prev_sid
                    && snap_epoch == Some(live_epoch.as_str())
                {
                    self.notify_single_opened(caller, &host, subscribe).await;
                    return Ok(Some(prev_snap));
                }
            }
        }
        self.remove_receipts_for_session(&prev_sid);
        Ok(None)
    }

    async fn acquire_receipt_slot(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        op: &str,
        req_id: Option<&str>,
        fp: &J,
        subscribe: Option<bool>,
    ) -> Result<ReceiptSlot, OrchestratorError> {
        loop {
            if let Some(prev_snap) = self.replay_receipt(caller, op, req_id, fp, subscribe).await? {
                return Ok(ReceiptSlot::Replayed(prev_snap));
            }
            let Some(rid) = req_id else {
                return Ok(ReceiptSlot::Reserved(None));
            };
            let key: ReceiptKey = (caller.receipt_scope.clone(), op.to_owned(), rid.to_owned());
            let mut rx =
                match reserve_in_flight(&mut self.in_flight_receipts.borrow_mut(), Rc::downgrade(self), key, fp)? {
                    Ok(guard) => return Ok(ReceiptSlot::Reserved(Some(guard))),
                    Err(rx) => rx,
                };
            let outcome = loop {
                if let Some(res) = rx.borrow_and_update().clone() {
                    break Some(res);
                }
                if rx.changed().await.is_err() {
                    break None;
                }
            };
            match outcome {
                Some(Ok(_)) | None => continue,
                Some(Err(err)) => return Err(err),
            }
        }
    }

    fn finish_receipt(
        &self,
        caller: &OrchestratorCaller,
        op: &str,
        req_id: Option<String>,
        fp: J,
        mut guard: Option<InFlightReceiptGuard>,
        res: Result<(String, J), OrchestratorError>,
    ) -> Result<J, OrchestratorError> {
        match res {
            Ok((sid, snap)) => {
                if !sid.is_empty() {
                    self.store_receipt(&caller.receipt_scope, op, req_id, fp, &sid, &snap);
                }
                if let Some(ref mut g) = guard {
                    g.finish(Ok((sid, snap.clone())));
                }
                Ok(snap)
            }
            Err(err) => {
                if let Some(ref mut g) = guard {
                    g.finish(Err(err.clone()));
                }
                Err(err)
            }
        }
    }

    /// Register a callback invoked synchronously whenever an active [`Host`] transitions in-place
    /// (`session/changed`) to a new `session_id`. Returns a handle for [`Self::remove_session_rekey`].
    pub fn on_session_rekey(&self, hook: impl Fn(&str, &str, &Rc<SessionRuntime>) + 'static) -> u64 {
        let id = self.alloc_handle_id();
        self.rekey_hooks.borrow_mut().push((id, Box::new(hook)));
        id
    }

    /// Remove a previously registered session rekey callback by handle id.
    pub fn remove_session_rekey(&self, id: u64) {
        self.rekey_hooks.borrow_mut().retain(|(hid, _)| *hid != id);
    }

    /// Number of currently registered session rekey callbacks.
    pub fn rekey_hook_count(&self) -> usize {
        self.rekey_hooks.borrow().len()
    }

    /// Host-wide HTTP/WebSocket route and asset registry.
    pub fn web(&self) -> &Rc<WebRegistry> {
        &self.web
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.get()
    }

    /// Registry indexing loaded sessions by canonical [`WorkspaceId`].
    pub fn workspaces(&self) -> &WorkspaceRegistry {
        &self.workspaces
    }

    /// Look up an active session runtime, returning [`SessionError::NotLoaded`] or
    /// [`SessionError::Closing`] when absent.
    pub fn session(&self, session_id: &str) -> Result<Rc<SessionRuntime>, SessionError> {
        if let Some(rt) = self.sessions.borrow().get(session_id) {
            return Ok(Rc::clone(rt));
        }
        if self.closing.borrow().contains_key(session_id) {
            return Err(SessionError::Closing(session_id.to_string()));
        }
        Err(SessionError::NotLoaded(session_id.to_string()))
    }

    async fn find_runtime(&self, session_id: &str) -> Result<Rc<SessionRuntime>, SessionError> {
        match self.session(session_id) {
            Ok(rt) => Ok(rt),
            Err(SessionError::NotLoaded(_)) => {
                let active: Vec<Rc<SessionRuntime>> = self.sessions.borrow().values().cloned().collect();
                for cand in active {
                    if host_session_id(&cand.host).await.as_deref() == Some(session_id) {
                        return Ok(cand);
                    }
                }
                Err(SessionError::NotLoaded(session_id.to_owned()))
            }
            Err(e) => Err(e),
        }
    }

    pub fn is_closing(&self, session_id: &str) -> bool {
        self.closing.borrow().contains_key(session_id)
    }

    #[cfg(test)]
    pub(crate) fn closing_len(&self) -> usize {
        self.closing.borrow().len()
    }

    pub fn is_loading(&self, session_id: &str) -> bool {
        self.loading.borrow().contains(session_id)
    }

    pub fn len(&self) -> usize {
        self.sessions.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.borrow().is_empty()
    }

    pub fn active_ids(&self) -> Vec<String> {
        self.sessions.borrow().keys().cloned().collect()
    }

    pub fn active_and_closing_ids(&self) -> HashSet<String> {
        let mut ids: HashSet<String> = self.sessions.borrow().keys().cloned().collect();
        ids.extend(self.closing.borrow().keys().cloned());
        ids
    }

    /// Wait until any in-flight unload for `session_id` finishes.
    pub async fn wait_closing(&self, session_id: &str) {
        let mut rx = match self.closing.borrow().get(session_id) {
            Some(entry) => entry.done.subscribe(),
            None => return,
        };
        if *rx.borrow_and_update() {
            return;
        }
        while rx.changed().await.is_ok() {
            if *rx.borrow() || !self.closing.borrow().contains_key(session_id) {
                break;
            }
        }
    }

    fn ensure_session_available(&self, sid: &str, loading_reserved_for: Option<&str>) -> Result<(), SessionError> {
        if self.shutting_down.get() {
            return Err(SessionError::ShuttingDown);
        }
        if self.sessions.borrow().contains_key(sid) {
            return Err(SessionError::AlreadyLoaded(sid.to_string()));
        }
        if self.closing.borrow().contains_key(sid) {
            return Err(SessionError::Closing(sid.to_string()));
        }
        if loading_reserved_for != Some(sid) && self.loading.borrow().contains(sid) {
            return Err(SessionError::Loading(sid.to_string()));
        }
        if self.reserved.borrow().contains_key(sid) {
            return Err(SessionError::AlreadyLoaded(sid.to_string()));
        }
        Ok(())
    }

    /// Register an already-started `Host` into this orchestrator, binding its orchestrator slot to
    /// `self` when `self` is in [`Capacity::Multi`] mode.
    pub async fn register_host(
        &self,
        host: Arc<Host>,
        expected_session_id: Option<&str>,
        ephemeral: bool,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        self.register_host_inner(host, expected_session_id, ephemeral, None, self.is_multi()).await
    }

    /// Register an already-started `Host` while explicitly preserving its existing orchestrator
    /// capacity (used by attached single-host RPC `RpcRuntime::from_host`).
    pub async fn register_host_keeping_capacity(
        &self,
        host: Arc<Host>,
        expected_session_id: Option<&str>,
        ephemeral: bool,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        self.register_host_inner(host, expected_session_id, ephemeral, None, false).await
    }

    async fn register_host_inner(
        &self,
        host: Arc<Host>,
        expected_session_id: Option<&str>,
        ephemeral: bool,
        loading_guard: Option<&mut LoadingGuard>,
        bind_orchestrator: bool,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        if self.shutting_down.get() {
            host.shutdown().await;
            return Err(SessionError::ShuttingDown);
        }
        if expected_session_id.is_some()
            && let Some(err) = host_session_info_field(&host, "resume_error").await
        {
            let code = host_session_info_field(&host, "resume_error_code").await;
            host.shutdown().await;
            return Err(SessionError::ResumeFailed { code, message: err });
        }
        let actual = host_session_id(&host).await;
        let session_id = match (expected_session_id, actual) {
            (Some(expected), Some(actual_id)) if actual_id != expected => {
                host.shutdown().await;
                return Err(SessionError::SessionIdMismatch {
                    expected: expected.to_string(),
                    actual: Some(actual_id),
                });
            }
            (Some(expected), None) => {
                host.shutdown().await;
                return Err(SessionError::SessionIdMismatch { expected: expected.to_string(), actual: None });
            }
            (_, Some(id)) => id,
            (None, None) if ephemeral => {
                let id = uuid::Uuid::new_v4().to_string();
                let _ = host.apply_session_identity(Some(id.clone()), false);
                id
            }
            (None, None) => {
                host.shutdown().await;
                return Err(SessionError::MissingSessionId);
            }
        };

        if !bind_orchestrator
            && let Some(existing) = self.sessions.borrow().get(&session_id).filter(|rt| Arc::ptr_eq(&rt.host, &host))
        {
            return Ok(Rc::clone(existing));
        }

        let reserved = loading_guard.as_ref().and_then(|g| g.sid.as_deref());
        if let Err(err) = self.ensure_session_available(&session_id, reserved) {
            let _ = abort_and_shutdown(&host).await;
            return Err(err);
        }

        let runtime = Rc::new(SessionRuntime::new(session_id.clone(), ephemeral, host));
        if let Some(expected_trust) = self.workspaces.attach(&session_id, &runtime.host) {
            let lua_trusted = runtime
                .host
                .lua()
                .globals()
                .get::<mlua::Table>("mare")
                .and_then(|t| t.get::<Option<bool>>("trusted"))
                .ok()
                .flatten();
            let needs_sync =
                runtime.host.workspace.is_trusted() != expected_trust || lua_trusted != Some(expected_trust);
            if needs_sync
                && let Err(err) =
                    runtime.host.apply_trust(expected_trust, mare_shared::WORKSPACE_TRUST_SOURCE, false).await
            {
                self.workspaces.detach(&runtime.workspace, &session_id);
                let host = detach_runtime_host(runtime);
                let _ = abort_and_shutdown(&host).await;
                return Err(SessionError::HostFailed(err));
            }
        }
        if self.shutting_down.get() {
            self.workspaces.detach(&runtime.workspace, &session_id);
            let host = detach_runtime_host(runtime);
            let _ = abort_and_shutdown(&host).await;
            return Err(SessionError::ShuttingDown);
        }
        if let Some(guard) = loading_guard {
            guard.disarm();
        }
        if bind_orchestrator && let Some(orch) = self.self_weak.upgrade() {
            runtime.host.set_orchestrator(orch);
        }
        self.sessions.borrow_mut().insert(session_id, Rc::clone(&runtime));
        if self.self_weak.strong_count() > 0 {
            let weak_set: Weak<dyn crate::RuntimeSet> = self.self_weak.clone();
            runtime.host.attach_runtime_set(weak_set);
        }
        Ok(runtime)
    }

    fn rekey_host_session(&self, host: &Arc<Host>, new_sid: &str) -> Result<(), SessionError> {
        let rekeyed = {
            let mut sessions = self.sessions.borrow_mut();
            let Some(old_sid) = sessions.iter().find_map(|(sid, rt)| Arc::ptr_eq(&rt.host, host).then(|| sid.clone()))
            else {
                self.reserved.borrow_mut().retain(|sid, owner| !(sid == new_sid && *owner == host.instance_id));
                return Ok(());
            };
            if old_sid == new_sid {
                self.reserved.borrow_mut().retain(|sid, owner| !(sid == new_sid && *owner == host.instance_id));
                return Ok(());
            }
            if sessions.contains_key(new_sid)
                || self.closing.borrow().contains_key(new_sid)
                || self.loading.borrow().contains(new_sid)
                || self.reserved.borrow().get(new_sid).is_some_and(|owner| *owner != host.instance_id)
            {
                return Err(SessionError::AlreadyLoaded(new_sid.to_owned()));
            }
            let Some(old_rt) = sessions.remove(&old_sid) else { return Ok(()) };
            let (_, new_epoch) = host.kernel.event_log().session_identity();
            let new_rt = Rc::new(old_rt.with_rekeyed_identity(new_sid.to_owned(), new_epoch));
            sessions.insert(new_sid.to_owned(), Rc::clone(&new_rt));
            self.reserved.borrow_mut().retain(|sid, owner| !(sid == new_sid && *owner == host.instance_id));
            self.workspaces.detach(&old_rt.workspace, &old_sid);
            let _ = self.workspaces.attach(new_sid, &new_rt.host);
            Some((old_sid, new_rt))
        };
        if let Some((old_sid, new_rt)) = rekeyed {
            new_rt.host.shell.reset_for_session_switch();
            self.remove_receipts_for_session(&old_sid);
            let hooks = self.rekey_hooks.borrow();
            for (_, hook) in hooks.iter() {
                hook(&old_sid, new_sid, &new_rt);
            }
        }
        Ok(())
    }

    fn prepare_opts(&self, mut opts: HostOptions) -> HostOptions {
        let Ok(ws_id) = WorkspaceId::local(&opts.cwd) else {
            opts.project_trusted = false;
            if let Some(obj) = opts.args.as_object_mut() {
                obj.insert("trust".into(), serde_json::Value::String("untrusted".into()));
            }
            return opts;
        };
        if let Some(trusted) = self.workspaces.trusted_override(&ws_id) {
            opts.project_trusted = trusted;
            if let Some(obj) = opts.args.as_object_mut() {
                obj.insert(
                    "trust".into(),
                    serde_json::Value::String(if trusted { "trusted" } else { "untrusted" }.to_string()),
                );
            }
        }
        opts
    }

    /// Create and register a new session without unloading any existing sessions.
    pub async fn create_session(&self, opts: HostOptions) -> Result<Rc<SessionRuntime>, SessionError> {
        self.create_session_inner(opts, None).await
    }

    async fn create_session_inner(
        &self,
        opts: HostOptions,
        fork_images: Option<HashMap<String, String>>,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        if self.shutting_down.get() {
            return Err(SessionError::ShuttingDown);
        }
        let _load_guard = LoadGuard::acquire(&self.load_tracker);
        let opts = self.prepare_opts(opts);
        let ephemeral = opts.args.get("no_session").and_then(|v| v.as_bool()) == Some(true);
        let target_orch = self.is_multi().then(|| self.self_weak.upgrade()).flatten();
        let host = Host::new_with_fork_images_and_web(opts, fork_images, self.web.clone(), target_orch)
            .await
            .map_err(SessionError::HostFailed)?;
        self.register_host(host, None, ephemeral).await
    }

    /// Load/resume a session without unloading any existing sessions, holding a concurrency
    /// reservation on `session_id` across `Host::new(opts).await`.
    pub async fn load_session(
        self: &Rc<Self>,
        session_id: Option<&str>,
        opts: HostOptions,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        self.load_session_with_before_ready(session_id, opts, |_| async {}).await
    }

    /// Load/resume a session with a hook invoked on the newly constructed [`Host`] right before
    /// `app/ready` runs, so RPC callers can wire UI/capability handlers before `ui.select` or
    /// `session/loaded` listeners inspect `mare.ui.connected()`.
    pub async fn load_session_with_before_ready<F, Fut>(
        self: &Rc<Self>,
        session_id: Option<&str>,
        opts: HostOptions,
        before_ready: F,
    ) -> Result<Rc<SessionRuntime>, SessionError>
    where
        F: FnOnce(Arc<Host>) -> Fut,
        Fut: Future<Output = ()>,
    {
        if self.shutting_down.get() {
            return Err(SessionError::ShuttingDown);
        }
        let _load_guard = LoadGuard::acquire(&self.load_tracker);
        let mut guard = if let Some(sid) = session_id {
            self.ensure_session_available(sid, None)?;
            self.loading.borrow_mut().insert(sid.to_string());
            Some(LoadingGuard { orch: Rc::downgrade(self), sid: Some(sid.to_string()) })
        } else {
            None
        };
        let opts = self.prepare_opts(opts);
        let ephemeral = opts.args.get("no_session").and_then(|v| v.as_bool()) == Some(true);
        let target_orch = self.is_multi().then(|| self.self_weak.upgrade()).flatten();
        let host = Host::new_with_web_deferred_and_orchestrator(opts, self.web.clone(), target_orch)
            .await
            .map_err(SessionError::HostFailed)?;
        before_ready(Arc::clone(&host)).await;
        host.emit_app_ready().await;
        if let Some(err) = host_session_info_field(&host, "resume_error").await {
            let code = host_session_info_field(&host, "resume_error_code").await;
            host.shutdown().await;
            return Err(SessionError::ResumeFailed { code, message: err });
        }
        self.register_host_inner(host, session_id, ephemeral, guard.as_mut(), self.is_multi()).await
    }

    /// Fork an already-loaded session into a new independent [`SessionRuntime`] without unloading
    /// or mutating the source session.
    pub async fn fork_session(
        &self,
        source_session_id: &str,
        mut new_opts: HostOptions,
    ) -> Result<Rc<SessionRuntime>, SessionError> {
        if self.shutting_down.get() {
            return Err(SessionError::ShuttingDown);
        }
        let _load_guard = LoadGuard::acquire(&self.load_tracker);
        let source = self.session(source_session_id)?;
        let has_prepare_fork = source
            .host
            .service_table(SESSION_SERVICE)
            .and_then(|svc| svc.get::<Value>("prepare_fork").ok())
            .is_some_and(|v| !v.is_nil());
        if !has_prepare_fork {
            return Err(SessionError::Unavailable("session provider does not implement prepare_fork".into()));
        }
        #[derive(serde::Deserialize)]
        struct PrepareForkResult {
            path: Option<String>,
            images: Option<HashMap<String, String>>,
        }
        let prep = host_call_service_json(&source.host, SESSION_SERVICE, "prepare_fork", vec![])
            .await
            .map_err(|e| SessionError::HostFailed(anyhow::anyhow!("{e}")))?;
        let (path, fork_images) = match prep {
            J::Null => (None, None),
            other => {
                let parsed: PrepareForkResult = serde_json::from_value(other).map_err(|e| {
                    SessionError::HostFailed(anyhow::anyhow!("session.prepare_fork returned unexpected value: {e}"))
                })?;
                let path = parsed.path.filter(|s| !s.is_empty());
                let images = parsed.images.filter(|m| !m.is_empty());
                (path, images)
            }
        };
        let path = path.ok_or_else(|| {
            SessionError::HostFailed(anyhow::anyhow!("source session `{source_session_id}` has no persisted file"))
        })?;
        if let Some(obj) = new_opts.args.as_object_mut() {
            obj.insert("resume".into(), serde_json::Value::String(path));
            obj.insert("fork_session".into(), serde_json::Value::Bool(true));
        } else {
            new_opts.args = serde_json::json!({ "resume": path, "fork_session": true });
        }
        self.create_session_inner(new_opts, fork_images).await
    }

    /// Update trust on canonical `workspace_id`, propagating runtime policy to all active sessions
    /// attached to it and optionally persisting the decision to disk.
    pub async fn set_workspace_trust(
        &self,
        workspace_id: &WorkspaceId,
        trusted: bool,
        persist: bool,
    ) -> Result<bool, SessionError> {
        self.workspaces.set_trust(workspace_id, trusted, persist).await.map_err(SessionError::HostFailed)
    }

    /// Update trust on canonical `workspace_id` on behalf of `session_id`, verifying that
    /// `session_id` is currently loaded and attached to `workspace_id`.
    pub async fn set_workspace_trust_for_session(
        &self,
        session_id: &str,
        workspace_id: &WorkspaceId,
        trusted: bool,
        persist: bool,
    ) -> Result<bool, SessionError> {
        let rt = self.session(session_id)?;
        if &rt.workspace != workspace_id || !self.workspaces.contains_session(workspace_id, session_id) {
            return Err(SessionError::WorkspaceMismatch {
                session_id: session_id.to_string(),
                workspace: workspace_id.cwd.display().to_string(),
            });
        }
        self.set_workspace_trust(workspace_id, trusted, persist).await
    }

    /// Close only `session_id` synchronously (waits for `abort` + `shutdown_report`), keeping the
    /// session marked `closing` for the full `.await` duration.
    pub async fn close_session(self: &Rc<Self>, session_id: &str) -> Result<ShutdownReport, SessionError> {
        let runtime = self.remove_for_close(session_id)?;
        let (done_tx, _) = watch::channel(false);
        let latest_report = Rc::new(RefCell::new(None));
        self.closing.borrow_mut().insert(
            session_id.to_string(),
            ClosingEntry { done: done_tx.clone(), handle: None, latest_report: Rc::clone(&latest_report) },
        );
        let _closing_guard = ClosingGuard { orch: Rc::downgrade(self), sid: session_id.to_string(), done: done_tx };
        let host = detach_runtime_host(runtime);
        let report = abort_and_shutdown(&host).await;
        *latest_report.borrow_mut() = Some(report.clone());
        Ok(report)
    }

    /// Begin closing `session_id` in the background, keeping its id marked `closing` until
    /// shutdown finishes (or up to bounded retries when `retry_until_confirmed` is true, aborted on
    /// manager shutdown).
    pub fn begin_close<F>(
        self: &Rc<Self>,
        session_id: &str,
        retry_until_confirmed: bool,
        on_first_attempt: F,
    ) -> Result<(), SessionError>
    where
        F: FnOnce(Result<(), String>) + 'static,
    {
        if self.shutting_down.get() {
            return Err(SessionError::ShuttingDown);
        }
        let runtime = self.remove_for_close(session_id)?;
        let host = detach_runtime_host(runtime);
        let (done_tx, _) = watch::channel(false);
        let guard_done = done_tx.clone();
        let latest_report = Rc::new(RefCell::new(None));
        let report_slot = Rc::clone(&latest_report);
        let weak = Rc::downgrade(self);
        let sid = session_id.to_string();
        let shutdown_cancel = self.shutdown_cancel.clone();
        let handle = tokio::task::spawn_local(async move {
            let _closing_guard = ClosingGuard { orch: weak, sid: sid.clone(), done: guard_done };
            let abort_timed_out = tokio::time::timeout(host.shutdown_grace(), host.abort()).await.is_err();
            let outcome = close_with_retry(
                retry_until_confirmed,
                DEFAULT_CLOSE_MAX_RETRIES,
                DEFAULT_CLOSE_RETRY_BACKOFF,
                shutdown_cancel,
                || {
                    let host = host.clone();
                    let sid = sid.clone();
                    let report_slot = Rc::clone(&report_slot);
                    async move {
                        let mut report = host.shutdown_report().await;
                        if abort_timed_out && !report.unconfirmed_tasks.contains(&"abort") {
                            report.unconfirmed_tasks.push("abort");
                        }
                        *report_slot.borrow_mut() = Some(report.clone());
                        if report.stop_unconfirmed() {
                            Err(format!(
                                "stop_unconfirmed: session `{sid}` did not confirm stop (tasks: {:?}, trees: {:?})",
                                report.unconfirmed_tasks, report.unconfirmed_trees
                            ))
                        } else {
                            Ok(())
                        }
                    }
                },
                on_first_attempt,
                || {},
            )
            .await;
            if retry_until_confirmed && !outcome.confirmed {
                tracing::warn!(
                    session_id = %sid,
                    cancelled = outcome.cancelled,
                    retries = outcome.retries,
                    "session background close finished without stop confirmation"
                );
            }
        });
        self.closing
            .borrow_mut()
            .insert(session_id.to_string(), ClosingEntry { done: done_tx, handle: Some(handle), latest_report });
        Ok(())
    }

    fn remove_for_close(&self, session_id: &str) -> Result<Rc<SessionRuntime>, SessionError> {
        if let Some(rt) = self.sessions.borrow_mut().remove(session_id) {
            self.reserved.borrow_mut().retain(|_, owner| *owner != rt.host.instance_id);
            self.workspaces.detach(&rt.workspace, session_id);
            return Ok(rt);
        }
        if self.closing.borrow().contains_key(session_id) {
            return Err(SessionError::Closing(session_id.to_string()));
        }
        Err(SessionError::NotLoaded(session_id.to_string()))
    }

    /// Cancel and concurrently drain every active and in-flight closing session runtime, wait for
    /// any in-flight `Host::new` loads to roll back, and return the aggregated quiescent
    /// [`ManagerShutdownReport`]. Concurrent callers share the in-flight shutdown report.
    pub async fn shutdown_all_report(&self) -> ManagerShutdownReport {
        self.shutdown_all_report_inner(true).await
    }

    /// Shut down every active session runtime without calling `host.abort()` (preserving queued
    /// durable inputs across clean EOF/disconnect).
    pub async fn shutdown_all_no_abort(&self) -> ManagerShutdownReport {
        self.shutdown_all_report_inner(false).await
    }

    async fn shutdown_all_report_inner(&self, abort_first: bool) -> ManagerShutdownReport {
        let in_flight = self.in_flight_shutdown.borrow().clone();
        if let Some(mut rx) = in_flight {
            if let Some(report) = rx.borrow_and_update().clone() {
                return report;
            }
            while rx.changed().await.is_ok() {
                if let Some(report) = rx.borrow().clone() {
                    return report;
                }
            }
            return ManagerShutdownReport::default();
        }
        struct InFlightReset<'a>(&'a RefCell<Option<watch::Receiver<Option<ManagerShutdownReport>>>>);
        impl Drop for InFlightReset<'_> {
            fn drop(&mut self) {
                *self.0.borrow_mut() = None;
            }
        }
        let (tx, rx) = watch::channel(None);
        *self.in_flight_shutdown.borrow_mut() = Some(rx);
        let _reset_in_flight = InFlightReset(&self.in_flight_shutdown);

        self.shutting_down.set(true);
        self.shutdown_cancel.cancel();

        let active: Vec<(String, Rc<SessionRuntime>)> = self.sessions.borrow_mut().drain().collect();
        let closing: Vec<(String, ClosingEntry)> = self.closing.borrow_mut().drain().collect();
        let mut tasks: Vec<(String, tokio::task::JoinHandle<ShutdownReport>)> =
            Vec::with_capacity(active.len() + closing.len());

        for (session_id, runtime) in active {
            self.workspaces.detach(&runtime.workspace, &session_id);
            let host = detach_runtime_host(runtime);
            let handle = tokio::task::spawn_local(async move {
                if abort_first { abort_and_shutdown(&host).await } else { host.shutdown_report().await }
            });
            tasks.push((session_id, handle));
        }

        for (session_id, entry) in closing {
            let ClosingEntry { done, handle, latest_report } = entry;
            let task = tokio::task::spawn_local(async move {
                let mut failed = false;
                if let Some(join) = handle {
                    if join.await.is_err() {
                        failed = true;
                    }
                } else {
                    let mut done_rx = done.subscribe();
                    if !*done_rx.borrow_and_update() {
                        while done_rx.changed().await.is_ok() {
                            if *done_rx.borrow() {
                                break;
                            }
                        }
                    }
                }
                done.send_replace(true);
                let mut report =
                    latest_report.borrow().clone().unwrap_or_else(|| unconfirmed_shutdown_report("close_unconfirmed"));
                if failed && !report.unconfirmed_tasks.contains(&"shutdown_task_failed") {
                    report.unconfirmed_tasks.push("shutdown_task_failed");
                }
                report
            });
            tasks.push((session_id, task));
        }

        let mut sessions = Vec::with_capacity(tasks.len());
        for (session_id, task) in tasks {
            let report = match task.await {
                Ok(report) => report,
                Err(_) => unconfirmed_shutdown_report("shutdown_task_failed"),
            };
            sessions.push(SessionShutdownEntry { session_id, report });
        }

        while self.load_tracker.count.get() > 0 {
            self.load_tracker.done.notified().await;
        }

        let report = ManagerShutdownReport { sessions };
        tx.send_replace(Some(report.clone()));
        report
    }

    /// Cancel and shut down every active session runtime concurrently and await all in-flight closes.
    pub async fn shutdown_all(&self) {
        let _ = self.shutdown_all_report().await;
    }

    fn validate_lifecycle_args(anchor: &Host, method: &str, arguments: &J) -> Result<(), OrchestratorError> {
        if let Some(spec) = anchor.kernel.service_method(SESSION_SERVICE, method) {
            let mut shaped = arguments.clone();
            mare_shared::schema::shape_empty(&mut shaped, &spec.wire_params());
            let schema = mare_shared::schema::Schema::compile_contract(spec.wire_params())
                .map_err(OrchestratorError::internal)?;
            schema.validate(&shaped).map_err(OrchestratorError::invalid_arguments)?;
        }
        Ok(())
    }

    /// Orchestrated `session.create` (multi-runtime creates a new `SessionRuntime`; single-runtime
    /// delegates to the host's provider once `session.switch` is wired).
    pub async fn create(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        anchor: &Arc<Host>,
        arguments: &J,
    ) -> Result<J, OrchestratorError> {
        let obj =
            arguments.as_object().ok_or_else(|| OrchestratorError::invalid_arguments("arguments must be an object"))?;
        let req_id = parse_request_id(obj)?;
        let subscribe = parse_subscribe(obj)?;
        Self::validate_lifecycle_args(anchor, "create", arguments)?;

        let cwd = opt_str(obj, "cwd")?;
        let name = opt_str(obj, "name")?;
        let fp = json!({ "cwd": cwd, "name": name });
        let receipt_guard = match self.acquire_receipt_slot(caller, "create", req_id.as_deref(), &fp, subscribe).await?
        {
            ReceiptSlot::Replayed(prev_snap) => return Ok(prev_snap),
            ReceiptSlot::Reserved(guard) => guard,
        };
        let res = self.create_reserved(caller, anchor, cwd, name, subscribe).await;
        self.finish_receipt(caller, "create", req_id, fp, receipt_guard, res)
    }

    async fn create_reserved(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        anchor: &Arc<Host>,
        cwd: Option<String>,
        name: Option<String>,
        subscribe: Option<bool>,
    ) -> Result<(String, J), OrchestratorError> {
        let project_cwd = self.project_cwd().unwrap_or_else(|| canonical_dir(&anchor.cwd));
        let anchor_canon = canonical_dir(&anchor.cwd);
        let target_canon = if let Some(ref raw_cwd) = cwd {
            if raw_cwd.trim().is_empty() {
                return Err(OrchestratorError::invalid_arguments("cwd must not be empty"));
            }
            let path = PathBuf::from(raw_cwd);
            let resolved = if path.is_absolute() { path } else { project_cwd.join(path) };
            self.check_project_scope(caller, "session.create", &anchor.cwd, Some(&resolved), || {
                OrchestratorError::denied("restricted callers cannot create sessions outside the current project")
            })?;
            canonical_dir(&resolved)
        } else {
            project_cwd.clone()
        };
        let mut ceiling_host = Arc::clone(anchor);
        if !caller.is_admin {
            let loaded_host = self
                .sessions
                .borrow()
                .values()
                .filter(|rt| canonical_dir(&rt.host.cwd) == target_canon)
                .min_by(|a, b| a.session_id.cmp(&b.session_id))
                .map(|rt| Arc::clone(&rt.host));
            if target_canon != project_cwd && target_canon != anchor_canon && loaded_host.is_none() {
                return Err(OrchestratorError::denied(
                    "`session.create` outside the current project or loaded workspaces requires remote_admin",
                ));
            }
            if target_canon != anchor_canon
                && let Some(host) = loaded_host
            {
                ceiling_host = host;
            }
        }
        let restore_ceiling =
            (!caller.is_admin).then(|| crate::agent::inheritance::host_permission_ceiling(&ceiling_host));
        if !self.is_multi() {
            if let Some(ref raw_cwd) = cwd {
                let path = PathBuf::from(raw_cwd);
                let resolved = if path.is_absolute() { path } else { anchor.cwd.join(path) };
                if canonical_dir(&resolved) != anchor_canon {
                    return Err(OrchestratorError::coded(
                        "unsupported",
                        "cannot change cwd in single-session mode; start a multi-session runtime or open a host in the target directory",
                    ));
                }
            }
            if anchor.bound_session_calls() > 0 {
                return Err(OrchestratorError::coded(
                    "busy",
                    "cannot switch sessions while a call bound to this session is running; retry",
                ));
            }
            let sw = new_session_switch_args(cwd.as_deref(), name.as_deref(), restore_ceiling);
            let snap = host_call_service_json(anchor, SESSION_SERVICE, "switch", vec![sw]).await?;
            mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
            self.notify_single_opened(caller, anchor, subscribe).await;
            let new_sid = snap
                .get("session_id")
                .and_then(J::as_str)
                .map(str::to_owned)
                .or_else(|| anchor.kernel.event_log().session_identity().0)
                .unwrap_or_default();
            return Ok((new_sid, snap));
        }

        let ext = self.ext();
        let mut opts = ext.clean_opts(cwd.as_deref(), None)?;
        if let Some(args) = opts.args.as_object_mut() {
            if let Some(ref n) = name {
                args.insert("name".into(), J::String(n.clone()));
            }
            insert_restore_ceiling(args, restore_ceiling);
        }
        mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
        let rt = self.create_session(opts).await.map_err(OrchestratorError::from_session_error)?;
        ext.apply_defaults(&rt).await;
        let snap_pre = match host_call_service_json(
            &rt.host,
            SESSION_SERVICE,
            "get",
            vec![J::String(rt.session_id.clone())],
        )
        .await
        {
            Ok(snap) => snap,
            Err(err) => {
                let _ = self.close_session(&rt.session_id).await;
                ext.closed(&rt.session_id);
                return Err(err);
            }
        };
        if !rt.host.kernel.event_log().has_event_in_current_epoch("session/changed") {
            let mut payload = json!({ "session_id": rt.session_id, "reason": "new" });
            if let Some(path) = snap_pre.get("path").and_then(J::as_str)
                && let Some(map) = payload.as_object_mut()
            {
                map.insert("path".into(), J::String(path.to_owned()));
            }
            let _ = rt.host.kernel.event_log().admit_json("session/changed", payload).await;
        }
        ext.opened(caller, &rt, true, subscribe).await;
        Ok((rt.session_id.clone(), snap_pre))
    }

    /// Orchestrated `session.load`.
    pub async fn load(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        anchor: &Arc<Host>,
        arguments: &J,
    ) -> Result<J, OrchestratorError> {
        let obj =
            arguments.as_object().ok_or_else(|| OrchestratorError::invalid_arguments("arguments must be an object"))?;
        let _req_id = parse_request_id(obj)?;
        let subscribe = parse_subscribe(obj)?;
        Self::validate_lifecycle_args(anchor, "load", arguments)?;

        let sid = opt_str(obj, "session_id")?;
        let path_arg = opt_str(obj, "path")?;
        let (selector, by_path) = match (sid, path_arg) {
            (Some(s), None) => (s, false),
            (None, Some(p)) => {
                if !caller.allow_path {
                    if caller.unrestricted {
                        return Err(OrchestratorError::denied("`session.load` by path requires remote_admin"));
                    }
                    return Err(OrchestratorError::invalid_arguments(
                        "only the primary connection may load a session by path",
                    ));
                }
                (p, true)
            }
            (Some(_), Some(_)) => {
                return Err(OrchestratorError::invalid_arguments("pass either `session_id` or `path`, not both"));
            }
            (None, None) => {
                return Err(OrchestratorError::invalid_arguments("`session_id` or `path` is required"));
            }
        };
        let restore_ceiling = (!caller.is_admin).then(|| crate::agent::inheritance::host_permission_ceiling(anchor));
        if !self.is_multi() {
            if anchor.bound_session_calls() > 0 {
                return Err(OrchestratorError::coded(
                    "busy",
                    "cannot switch sessions while a call bound to this session is running; retry",
                ));
            }
            let ext = self.ext();
            let mut sw = serde_json::Map::new();
            sw.insert("kind".into(), J::String("load".into()));
            if by_path {
                let header = mare_store::read_header(Path::new(&selector));
                let header_cwd = header.as_ref().and_then(|v| v.get("cwd")).and_then(J::as_str).map(Path::new);
                self.check_project_scope(caller, "session.load", &anchor.cwd, header_cwd, || {
                    OrchestratorError::invalid_arguments(format!("no session matches `{selector}`"))
                })?;
                sw.insert("path".into(), J::String(selector));
            } else if !caller.allow_path || !caller.unrestricted {
                let resolved_path = ext.resolve_selector(anchor, caller, &selector, false).await?;
                let header = mare_store::read_header(Path::new(&resolved_path));
                let header_cwd = header.as_ref().and_then(|v| v.get("cwd")).and_then(J::as_str).map(Path::new);
                self.check_project_scope(caller, "session.load", &anchor.cwd, header_cwd, || {
                    OrchestratorError::invalid_arguments(format!("no session matches `{selector}`"))
                })?;
                sw.insert("path".into(), J::String(resolved_path));
            } else {
                sw.insert("session_id".into(), J::String(selector));
            }
            insert_restore_ceiling(&mut sw, restore_ceiling);
            authorize_switch_settings(anchor, caller)?;
            let snap = host_call_service_json(anchor, SESSION_SERVICE, "switch", vec![J::Object(sw)]).await?;
            mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
            self.notify_single_opened(caller, anchor, subscribe).await;
            return Ok(snap);
        }

        let ext = self.ext();
        if !by_path && let Ok(rt) = self.session(&selector) {
            self.check_project_scope(caller, "session.load", &anchor.cwd, Some(&rt.host.cwd), || {
                OrchestratorError::invalid_arguments(format!("no session matches `{selector}`"))
            })?;
            let snap = host_call_service_json(&rt.host, SESSION_SERVICE, "get", vec![J::String(rt.session_id.clone())])
                .await?;
            ext.opened(caller, &rt, true, subscribe).await;
            return Ok(snap);
        }
        let resolved_path = ext.resolve_selector(anchor, caller, &selector, false).await?;
        let header = mare_store::read_header(Path::new(&resolved_path));
        let header_cwd = header.as_ref().and_then(|v| v.get("cwd")).and_then(J::as_str).map(Path::new);
        self.check_project_scope(caller, "session.load", &anchor.cwd, header_cwd, || {
            OrchestratorError::invalid_arguments(format!("no session matches `{selector}`"))
        })?;
        let header_sid = header.as_ref().and_then(|v| v.get("session_id")).and_then(J::as_str).map(str::to_owned);
        if let Some(ref existing_id) = header_sid
            && let Ok(rt) = self.session(existing_id)
        {
            self.check_project_scope(caller, "session.load", &anchor.cwd, Some(&rt.host.cwd), || {
                OrchestratorError::invalid_arguments(format!("no session matches `{selector}`"))
            })?;
            let snap = host_call_service_json(&rt.host, SESSION_SERVICE, "get", vec![J::String(rt.session_id.clone())])
                .await?;
            ext.opened(caller, &rt, true, subscribe).await;
            return Ok(snap);
        }

        let load_cwd = resolve_header_cwd(header.as_ref())?;
        let mut opts = ext.clean_opts(load_cwd.as_deref().and_then(Path::to_str), Some(false))?;
        if let Some(args) = opts.args.as_object_mut() {
            args.insert("resume".into(), J::String(resolved_path));
            insert_restore_ceiling(args, restore_ceiling);
        }
        let pre_sid = header_sid.clone();
        type PreparedLoadHost = Rc<RefCell<Option<(String, Arc<Host>)>>>;
        let prepared: PreparedLoadHost = Rc::new(RefCell::new(None));
        mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
        let load_res = self
            .load_session_with_before_ready(header_sid.as_deref(), opts, {
                let ext = Rc::clone(&ext);
                let caller = caller.clone();
                let prepared = Rc::clone(&prepared);
                move |host| async move {
                    let sid =
                        pre_sid.clone().or_else(|| host.kernel.event_log().session_identity().0).unwrap_or_default();
                    if !sid.is_empty() {
                        *prepared.borrow_mut() = Some((sid, Arc::clone(&host)));
                    }
                    ext.before_ready(&caller, pre_sid, host).await;
                }
            })
            .await;
        let rt = match load_res {
            Ok(rt) => rt,
            Err(err) => {
                if let Some((sid, failed_host)) = prepared.borrow_mut().take() {
                    ext.rollback_before_ready(caller, &sid, &failed_host);
                }
                return Err(OrchestratorError::from_session_error(err));
            }
        };
        if let Some(err) = host_session_info_field(&rt.host, "resume_error").await {
            let err_code = host_session_info_field(&rt.host, "resume_error_code").await;
            let _ = self.close_session(&rt.session_id).await;
            if let Some((sid, failed_host)) = prepared.borrow_mut().take() {
                ext.rollback_before_ready(caller, &sid, &failed_host);
            }
            ext.closed(&rt.session_id);
            let code = match err_code.as_deref() {
                Some("cancelled") => "cancelled",
                _ => "invalid_arguments",
            };
            return Err(OrchestratorError::coded(code, err));
        }
        let snap = match host_call_service_json(
            &rt.host,
            SESSION_SERVICE,
            "get",
            vec![J::String(rt.session_id.clone())],
        )
        .await
        {
            Ok(snap) => snap,
            Err(err) => {
                let _ = self.close_session(&rt.session_id).await;
                if let Some((sid, failed_host)) = prepared.borrow_mut().take() {
                    ext.rollback_before_ready(caller, &sid, &failed_host);
                }
                ext.closed(&rt.session_id);
                return Err(err);
            }
        };
        ext.opened(caller, &rt, true, subscribe).await;
        Ok(snap)
    }

    /// Orchestrated `session.fork`.
    pub async fn fork(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        anchor: &Arc<Host>,
        arguments: &J,
    ) -> Result<J, OrchestratorError> {
        let obj =
            arguments.as_object().ok_or_else(|| OrchestratorError::invalid_arguments("arguments must be an object"))?;
        let req_id = parse_request_id(obj)?;
        let subscribe = parse_subscribe(obj)?;
        Self::validate_lifecycle_args(anchor, "fork", arguments)?;

        let sid = opt_str(obj, "session_id")?
            .ok_or_else(|| OrchestratorError::invalid_arguments("session_id is required"))?;
        let expected_epoch = opt_str(obj, "expected_epoch")?;
        if let Some(ref exp) = expected_epoch
            && exp.trim().is_empty()
        {
            return Err(OrchestratorError::invalid_arguments("expected_epoch must not be empty"));
        }
        let before_item_id = opt_str(obj, "before_item_id")?;
        let name = opt_str(obj, "name")?;
        let fp = json!({
            "session_id": sid,
            "expected_epoch": expected_epoch,
            "before_item_id": before_item_id,
            "name": name,
        });
        let receipt_guard = match self.acquire_receipt_slot(caller, "fork", req_id.as_deref(), &fp, subscribe).await? {
            ReceiptSlot::Replayed(prev_snap) => return Ok(prev_snap),
            ReceiptSlot::Reserved(guard) => guard,
        };
        let res = self.fork_reserved(caller, anchor, sid, expected_epoch, before_item_id, name, subscribe).await;
        self.finish_receipt(caller, "fork", req_id, fp, receipt_guard, res)
    }

    #[allow(clippy::too_many_arguments)]
    async fn fork_reserved(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        anchor: &Arc<Host>,
        sid: String,
        expected_epoch: Option<String>,
        before_item_id: Option<String>,
        name: Option<String>,
        subscribe: Option<bool>,
    ) -> Result<(String, J), OrchestratorError> {
        let restore_ceiling = (!caller.is_admin).then(|| crate::agent::inheritance::host_permission_ceiling(anchor));
        if !self.is_multi() {
            if anchor.bound_session_calls() > 0 {
                return Err(OrchestratorError::coded(
                    "busy",
                    "cannot switch sessions while a call bound to this session is running; retry",
                ));
            }
            let (active_id, active_epoch) = anchor.kernel.event_log().session_identity();
            if let Some(ref exp) = expected_epoch
                && (active_id.as_deref() != Some(sid.as_str()) || active_epoch != *exp)
            {
                return Err(OrchestratorError::stale_session("the session changed since expected_epoch was obtained"));
            }
            if (!caller.allow_path || !caller.unrestricted) && active_id.as_deref() != Some(sid.as_str()) {
                let resolved_path = self.ext().resolve_selector(anchor, caller, &sid, false).await?;
                let header = mare_store::read_header(Path::new(&resolved_path));
                let header_cwd = header.as_ref().and_then(|v| v.get("cwd")).and_then(J::as_str).map(Path::new);
                self.check_project_scope(caller, "session.fork", &anchor.cwd, header_cwd, || {
                    OrchestratorError::unknown_session(format!("no session matches `{sid}`"))
                })?;
            }
            let mut sw = serde_json::Map::new();
            sw.insert("kind".into(), J::String("fork".into()));
            sw.insert("session_id".into(), J::String(sid));
            if let Some(ref entry) = before_item_id {
                sw.insert("before_item_id".into(), J::String(entry.clone()));
            }
            if let Some(ref n) = name {
                sw.insert("name".into(), J::String(n.clone()));
            }
            insert_restore_ceiling(&mut sw, restore_ceiling);
            authorize_switch_settings(anchor, caller)?;
            let snap = host_call_service_json(anchor, SESSION_SERVICE, "switch", vec![J::Object(sw)]).await?;
            mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
            self.notify_single_opened(caller, anchor, subscribe).await;
            let new_sid = snap
                .get("session_id")
                .and_then(J::as_str)
                .map(str::to_owned)
                .or_else(|| anchor.kernel.event_log().session_identity().0)
                .unwrap_or_default();
            return Ok((new_sid, snap));
        }

        let source_rt = match self.find_runtime(&sid).await {
            Ok(rt) => {
                self.check_project_scope(caller, "session.fork", &anchor.cwd, Some(&rt.host.cwd), || {
                    OrchestratorError::unknown_session(format!("no session matches `{sid}`"))
                })?;
                Some(rt)
            }
            Err(_) => None,
        };
        if let Some(ref exp) = expected_epoch {
            let Some(ref rt) = source_rt else {
                return Err(OrchestratorError::stale_session("the session changed since expected_epoch was obtained"));
            };
            let (_, cur_epoch) = rt.host.kernel.event_log().session_identity();
            if cur_epoch != *exp {
                return Err(OrchestratorError::stale_session("the session changed since expected_epoch was obtained"));
            }
        }
        let ext = self.ext();
        let (fork_cwd, resolved_unloaded_path) = match &source_rt {
            Some(rt) => {
                let canon = canonical_dir(&rt.host.cwd);
                if !canon.is_dir() {
                    return Err(OrchestratorError::invalid_arguments("session directory no longer exists"));
                }
                (Some(canon), None)
            }
            None => {
                let resolved_path = ext.resolve_selector(anchor, caller, &sid, false).await?;
                let header = mare_store::read_header(Path::new(&resolved_path));
                let header_cwd = header.as_ref().and_then(|v| v.get("cwd")).and_then(J::as_str).map(Path::new);
                self.check_project_scope(caller, "session.fork", &anchor.cwd, header_cwd, || {
                    OrchestratorError::unknown_session(format!("no session matches `{sid}`"))
                })?;
                (resolve_header_cwd(header.as_ref())?, Some(resolved_path))
            }
        };
        let mut opts = ext.clean_opts(fork_cwd.as_deref().and_then(Path::to_str), Some(false))?;
        if let Some(args) = opts.args.as_object_mut() {
            args.insert("fork_session".into(), J::Bool(true));
            if let Some(ref entry) = before_item_id {
                args.insert("fork_before_entry".into(), J::String(entry.clone()));
            }
            if let Some(ref n) = name {
                args.insert("name".into(), J::String(n.clone()));
            }
            insert_restore_ceiling(args, restore_ceiling);
        }
        let rt = match source_rt {
            Some(source_rt) => {
                mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
                self.fork_session(&source_rt.session_id, opts).await.map_err(OrchestratorError::from_session_error)?
            }
            None => {
                if let Some(resolved_path) = resolved_unloaded_path
                    && let Some(args) = opts.args.as_object_mut()
                {
                    args.insert("resume".into(), J::String(resolved_path));
                }
                mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
                self.create_session(opts).await.map_err(OrchestratorError::from_session_error)?
            }
        };
        if let Some(err) = host_session_info_field(&rt.host, "resume_error").await {
            let err_code = host_session_info_field(&rt.host, "resume_error_code").await;
            let _ = self.close_session(&rt.session_id).await;
            ext.closed(&rt.session_id);
            let code = match err_code.as_deref() {
                Some("session.unknown_item") => "session.unknown_item",
                _ => "invalid_arguments",
            };
            return Err(OrchestratorError::coded(code, err));
        }
        let snap = match host_call_service_json(
            &rt.host,
            SESSION_SERVICE,
            "get",
            vec![J::String(rt.session_id.clone())],
        )
        .await
        {
            Ok(snap) => snap,
            Err(err) => {
                let _ = self.close_session(&rt.session_id).await;
                ext.closed(&rt.session_id);
                return Err(err);
            }
        };
        ext.opened(caller, &rt, true, subscribe).await;
        Ok((rt.session_id.clone(), snap))
    }

    /// Orchestrated `session.close`.
    pub async fn close(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        anchor: &Arc<Host>,
        arguments: &J,
    ) -> Result<J, OrchestratorError> {
        let obj =
            arguments.as_object().ok_or_else(|| OrchestratorError::invalid_arguments("arguments must be an object"))?;
        Self::validate_lifecycle_args(anchor, "close", arguments)?;

        let sid = opt_str(obj, "session_id")?
            .ok_or_else(|| OrchestratorError::invalid_arguments("session_id is required"))?;
        if sid.trim().is_empty() {
            return Err(OrchestratorError::invalid_arguments("session_id must not be empty"));
        }
        let expected_epoch = opt_str(obj, "expected_epoch")?;
        if let Some(ref exp) = expected_epoch
            && exp.trim().is_empty()
        {
            return Err(OrchestratorError::invalid_arguments("expected_epoch must not be empty"));
        }
        if !self.is_multi() {
            if anchor.bound_session_calls() > 0 {
                return Err(OrchestratorError::coded(
                    "busy",
                    "cannot switch sessions while a call bound to this session is running; retry",
                ));
            }
            let active_id = host_session_id(anchor).await;
            let active_path = host_session_info_field(anchor, "path").await;
            let exact_active = active_id.as_deref() == Some(sid.as_str())
                || active_path
                    .as_deref()
                    .is_some_and(|p| sid == p || canonical_dir(Path::new(&sid)) == canonical_dir(Path::new(p)));
            let resolved_owned: Result<Option<String>, (&str, String)> =
                if exact_active { Ok(active_id.clone()) } else { resolve_single_close_target(anchor, &sid).await };
            let resolved_ref: Result<Option<&str>, (&str, String)> = match &resolved_owned {
                Ok(opt) => Ok(opt.as_deref()),
                Err((code, msg)) => Err((*code, msg.clone())),
            };
            match match_single_close_selector(&sid, active_id.as_deref(), active_path.as_deref(), resolved_ref)? {
                SingleCloseMatch::NoopUnloaded => {
                    if expected_epoch.is_some() {
                        return Err(OrchestratorError::stale_session(
                            "the session changed since expected_epoch was obtained",
                        ));
                    }
                    return Ok(json!({}));
                }
                SingleCloseMatch::CloseActive => {
                    if let Some(ref exp) = expected_epoch {
                        let (_, cur_epoch) = anchor.kernel.event_log().session_identity();
                        if cur_epoch != *exp {
                            return Err(OrchestratorError::stale_session(
                                "the session changed since expected_epoch was obtained",
                            ));
                        }
                    }
                    let restore_ceiling =
                        (!caller.is_admin).then(|| crate::agent::inheritance::host_permission_ceiling(anchor));
                    let sw = new_session_switch_args(None, None, restore_ceiling);
                    let _ = host_call_service_json(anchor, SESSION_SERVICE, "switch", vec![sw]).await?;
                    if let Some(ref aid) = active_id {
                        self.remove_receipts_for_session(aid);
                    }
                    mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
                    self.notify_single_opened(caller, anchor, None).await;
                    return Ok(json!({}));
                }
            }
        }
        if let Some(ref exp) = expected_epoch {
            match self.find_runtime(&sid).await {
                Ok(rt) => {
                    let (_, cur_epoch) = rt.host.kernel.event_log().session_identity();
                    if cur_epoch != *exp {
                        return Err(OrchestratorError::stale_session(
                            "the session changed since expected_epoch was obtained",
                        ));
                    }
                }
                Err(SessionError::NotLoaded(_)) => {
                    return Err(OrchestratorError::stale_session(
                        "the session changed since expected_epoch was obtained",
                    ));
                }
                Err(err) => return Err(OrchestratorError::from_session_error(err)),
            }
        }
        let closing_self = !mare_kernel::is_direct_host_export_call(anchor.kernel.id())
            && self.session(&sid).is_ok_and(|rt| Arc::ptr_eq(&rt.host, anchor));
        mare_kernel::mark_host_service_call_committed(anchor.kernel.id());
        let close_result = if closing_self {
            // Avoid self-deadlock or mid-shutdown cancellation when Lua inside this session (either
            // in-process or inside a plugin's own export handler) calls `session:close(self)`:
            // detach the session now and shut down the host asynchronously after returning.
            self.begin_close(&sid, false, |_| {})
        } else {
            self.close_session(&sid).await.map(|_| ())
        };
        if let Err(err) = close_result {
            if matches!(err, SessionError::NotLoaded(_)) {
                if expected_epoch.is_some() {
                    return Err(OrchestratorError::stale_session(
                        "the session changed since expected_epoch was obtained",
                    ));
                }
                let ext = self.ext();
                let resolved_path = match ext.resolve_selector(anchor, caller, &sid, true).await {
                    Ok(path) => path,
                    Err(_) => return Err(OrchestratorError::unknown_session(format!("unknown session: {sid}"))),
                };
                let header = mare_store::read_header(Path::new(&resolved_path));
                let header_sid = header.as_ref().and_then(|v| v.get("session_id")).and_then(J::as_str);
                if header_sid != Some(sid.as_str()) {
                    return Err(OrchestratorError::unknown_session(format!("unknown session: {sid}")));
                }
                let header_cwd = header.as_ref().and_then(|v| v.get("cwd")).and_then(J::as_str).map(Path::new);
                self.check_project_scope(caller, "session.close", &anchor.cwd, header_cwd, || {
                    OrchestratorError::unknown_session(format!("unknown session: {sid}"))
                })?;
                return Ok(json!({}));
            }
            return Err(OrchestratorError::from_session_error(err));
        }
        self.remove_receipts_for_session(&sid);
        self.ext().closed(&sid);
        Ok(json!({}))
    }

    async fn notify_single_opened(
        self: &Rc<Self>,
        caller: &OrchestratorCaller,
        anchor: &Arc<Host>,
        subscribe: Option<bool>,
    ) {
        if let (Some(new_sid), _) = anchor.kernel.event_log().session_identity()
            && let Ok(rt) = self.session(&new_sid)
        {
            self.ext().opened(caller, &rt, true, subscribe).await;
        }
    }
}

fn static_error_code(code: &str) -> &'static str {
    if let Some(core) = mare_shared::errors::CoreErrorCode::from_code(code) {
        return core.as_str();
    }
    match code {
        "session.unknown_item" => "session.unknown_item",
        "session.already_loaded" => "session.already_loaded",
        "session_already_loaded" => "session_already_loaded",
        _ => "internal",
    }
}

fn orchestrator_error_to_lua(err: OrchestratorError) -> mlua::Error {
    mlua::Error::external(mare_shared::errors::Coded { code: static_error_code(&err.code), message: err.message })
}

fn authorize_switch_settings(anchor: &Host, caller: &OrchestratorCaller) -> Result<(), OrchestratorError> {
    if anchor.kernel.service(mare_shared::LLM_SERVICE_NAME).is_none() {
        return Ok(());
    }
    for operation in ["set_model", "set_effort"] {
        if !anchor.kernel.permitted_for_caller(caller.ctx_id, mare_shared::LLM_SERVICE_NAME, operation) {
            return Err(OrchestratorError::denied(format!(
                "restoring session settings needs llm.{operation}, not allowed for the connection this runs for"
            )));
        }
    }
    Ok(())
}

fn caller_from_host_call(orch: &SessionOrchestrator, host: &Arc<Host>) -> OrchestratorCaller {
    let ctx_id = mare_kernel::current_export_caller().or_else(mare_kernel::active_owner_id);
    if let Some(cid) = ctx_id
        && let Some(identity) = orch.ext().caller_identity(host, cid)
    {
        return OrchestratorCaller::from_access(
            host,
            ctx_id,
            identity.conn_id,
            identity.is_owner,
            identity.is_admin,
            identity.receipt_scope,
        );
    }
    let is_owner = !host.kernel.is_peer_caller(ctx_id);
    let receipt_scope = if is_owner {
        "primary".to_owned()
    } else {
        ctx_id.map_or_else(|| "peer".to_owned(), |id| format!("peer:{id}"))
    };
    OrchestratorCaller::from_access(host, ctx_id, None, is_owner, is_owner, receipt_scope)
}

/// Register the four host-provided session lifecycle methods (`create`, `load`, `fork`, `close`)
/// on `kernel` via [`mare_kernel::Kernel::register_host_method_async`].
///
/// Calls dispatch through whichever [`SessionOrchestrator`] is currently bound to the [`Host`]
/// (`Capacity::Single` on standalone/attached hosts, `Capacity::Multi` once registered in a
/// multi-runtime [`crate::HostManager`]).
pub(crate) fn register_session_host_methods(
    kernel: &mare_kernel::Kernel,
    host_slot: crate::startup::request_gate::HostSlot,
) -> anyhow::Result<()> {
    let lua = kernel.lua().clone();
    for method in ["create", "load", "fork", "close"] {
        let host_slot = Rc::clone(&host_slot);
        let func = lua.create_async_function(move |lua, raw_args: MultiValue| {
            let host_slot = Rc::clone(&host_slot);
            async move {
                let Some(host) = host_slot.get().and_then(ArcWeak::upgrade) else {
                    return Err(orchestrator_error_to_lua(OrchestratorError::coded(
                        "unavailable",
                        "host is shutting down",
                    )));
                };
                let Some(orch) = host.orchestrator() else {
                    return Err(orchestrator_error_to_lua(OrchestratorError::coded(
                        "unavailable",
                        "session orchestrator is not attached",
                    )));
                };
                let arguments = decode_host_method_args(&lua, method, raw_args).map_err(orchestrator_error_to_lua)?;
                let caller = caller_from_host_call(&orch, &host);
                let out = match method {
                    "create" => orch.create(&caller, &host, &arguments).await,
                    "load" => orch.load(&caller, &host, &arguments).await,
                    "fork" => orch.fork(&caller, &host, &arguments).await,
                    "close" => orch.close(&caller, &host, &arguments).await,
                    _ => unreachable!(),
                }
                .map_err(orchestrator_error_to_lua)?;
                mare_shared::lua::json_to_lua_lossless(&lua, &out)
                    .map_err(|e| orchestrator_error_to_lua(OrchestratorError::internal(e.to_string())))
            }
        })?;
        kernel.register_host_method_async(SESSION_SERVICE, method, func)?;
    }
    Ok(())
}

/// Register the host-provided `workspace` service methods (`workspace.get`, `workspace.set_trust`)
/// on `kernel` so both Lua callers (`ctx.workspace:get()` / `ctx.workspace:set_trust(...)`) and
/// the generic `Scope::Workspace` RPC bridge share one host implementation.
pub(crate) fn register_workspace_host_methods(
    kernel: &mare_kernel::Kernel,
    host_slot: crate::startup::request_gate::HostSlot,
) -> anyhow::Result<()> {
    let lua = kernel.lua().clone();

    let get_slot = Rc::clone(&host_slot);
    let get_fn = lua.create_function(move |lua, _raw_args: MultiValue| {
        let Some(host) = get_slot.get().and_then(ArcWeak::upgrade) else {
            return Err(orchestrator_error_to_lua(OrchestratorError::coded("unavailable", "host is shutting down")));
        };
        let dirs =
            host.workspace_dirs().map_err(|e| orchestrator_error_to_lua(OrchestratorError::internal(e.to_string())))?;
        let payload = json!({
            "root": host.workspace_root().to_string_lossy(),
            "trusted": host.is_project_trusted(),
            "dirs": dirs,
        });
        mare_shared::lua::json_to_lua_lossless(lua, &payload)
            .map_err(|e| orchestrator_error_to_lua(OrchestratorError::internal(e.to_string())))
    })?;
    kernel.register_host_method(mare_shared::WORKSPACE_SERVICE_NAME, "get", get_fn)?;

    let trust_slot = Rc::clone(&host_slot);
    let set_trust_fn = lua.create_async_function(move |lua, raw_args: MultiValue| {
        let trust_slot = Rc::clone(&trust_slot);
        async move {
            let Some(host) = trust_slot.get().and_then(ArcWeak::upgrade) else {
                return Err(orchestrator_error_to_lua(OrchestratorError::coded(
                    "unavailable",
                    "host is shutting down",
                )));
            };
            let mut it = raw_args.into_iter();
            let trusted = match it.next() {
                Some(Value::Boolean(b)) => b,
                _ => {
                    return Err(orchestrator_error_to_lua(OrchestratorError::invalid_arguments(
                        "trusted must be a boolean",
                    )));
                }
            };
            let persist = match it.next() {
                None | Some(Value::Nil) => true,
                Some(Value::Boolean(b)) => b,
                _ => {
                    return Err(orchestrator_error_to_lua(OrchestratorError::invalid_arguments(
                        "save must be a boolean",
                    )));
                }
            };
            let ws_id = host.workspace.id().clone();
            // Mark the host call past its commit boundary before persisting or iterating over
            // workspace hosts so mid-flight cancellation cannot leave sessions in split trust state.
            mare_kernel::mark_host_service_call_committed(host.kernel.id());
            if let Some(orch) = host.orchestrator()
                && orch.workspaces().get(&ws_id).is_some()
            {
                orch.set_workspace_trust(&ws_id, trusted, persist)
                    .await
                    .map_err(|e| orchestrator_error_to_lua(OrchestratorError::from_session_error(e)))?;
            } else {
                host.set_workspace_trust(trusted, persist)
                    .await
                    .map_err(|e| orchestrator_error_to_lua(OrchestratorError::internal(e.to_string())))?;
            }
            Ok(Value::Table(lua.create_table()?))
        }
    })?;
    kernel.register_host_method_async(mare_shared::WORKSPACE_SERVICE_NAME, "set_trust", set_trust_fn)?;
    Ok(())
}

fn decode_host_method_args(lua: &mlua::Lua, method: &str, raw_args: MultiValue) -> Result<J, OrchestratorError> {
    let mut it = raw_args.into_iter();
    if method == "close" {
        let first = it.next();
        let second = it.next();
        let mut obj = match second {
            None | Some(Value::Nil) => serde_json::Map::new(),
            Some(v) => match mare_shared::lua::lua_to_json(lua, v)
                .map_err(|e| OrchestratorError::invalid_arguments(e.to_string()))?
            {
                J::Object(m) => m,
                _ => return Err(OrchestratorError::invalid_arguments("close options must be a table")),
            },
        };
        match first {
            None | Some(Value::Nil) => {}
            Some(Value::String(s)) => {
                obj.insert(
                    "session_id".into(),
                    J::String(s.to_str().map_err(|e| OrchestratorError::invalid_arguments(e.to_string()))?.to_owned()),
                );
            }
            Some(v) => match mare_shared::lua::lua_to_json(lua, v)
                .map_err(|e| OrchestratorError::invalid_arguments(e.to_string()))?
            {
                J::Object(m) => {
                    for (k, val) in m {
                        obj.entry(k).or_insert(val);
                    }
                }
                _ => return Err(OrchestratorError::invalid_arguments("session_id must be a string")),
            },
        }
        return Ok(J::Object(obj));
    }
    match it.next() {
        None | Some(Value::Nil) => Ok(json!({})),
        Some(v) => {
            mare_shared::lua::lua_to_json(lua, v).map_err(|e| OrchestratorError::invalid_arguments(e.to_string()))
        }
    }
}

async fn resolve_selector_via_list(
    anchor: &Host,
    selector: &str,
    include_archived: bool,
) -> Result<String, OrchestratorError> {
    let queries = if include_archived {
        vec![json!({ "all": true }), json!({ "all": true, "archived": true })]
    } else {
        vec![json!({ "all": true })]
    };
    let mut rows: Vec<J> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for query in queries {
        let Ok(list_val) = host_call_service_json(anchor, SESSION_SERVICE, "list", vec![query]).await else {
            continue;
        };
        if let Some(batch) = list_val.get("data").and_then(J::as_array) {
            for row in batch {
                let key = row.get("path").and_then(J::as_str).or_else(|| row.get("session_id").and_then(J::as_str));
                if let Some(k) = key
                    && seen.insert(k.to_owned())
                {
                    rows.push(row.clone());
                }
            }
        }
    }
    let mut matches: Vec<String> = Vec::new();
    for row in &rows {
        let Some(path) = row.get("path").and_then(J::as_str).filter(|p| !p.is_empty()) else {
            continue;
        };
        let id = row.get("session_id").and_then(J::as_str);
        if id == Some(selector) {
            return Ok(path.to_owned());
        }
        let by_prefix = id.is_some_and(|i| i.starts_with(selector));
        let by_name = row.get("name").and_then(J::as_str).is_some_and(|n| n.eq_ignore_ascii_case(selector));
        if by_prefix || by_name {
            matches.push(path.to_owned());
        }
    }
    match matches.len() {
        0 => Err(OrchestratorError::unknown_session(format!("no session matches `{selector}`"))),
        1 => Ok(matches.remove(0)),
        _ => Err(OrchestratorError::invalid_arguments(format!("`{selector}` matches more than one session"))),
    }
}

async fn resolve_single_close_target(anchor: &Host, selector: &str) -> Result<Option<String>, (&'static str, String)> {
    let mut rows: Vec<J> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for query in [json!({ "all": true }), json!({ "all": true, "archived": true })] {
        let Ok(list_val) = host_call_service_json(anchor, SESSION_SERVICE, "list", vec![query]).await else {
            continue;
        };
        if let Some(batch) = list_val.get("data").and_then(J::as_array) {
            for row in batch {
                let key = row.get("session_id").and_then(J::as_str).or_else(|| row.get("path").and_then(J::as_str));
                if let Some(k) = key
                    && seen.insert(k.to_owned())
                {
                    rows.push(row.clone());
                }
            }
        }
    }
    if rows.is_empty() {
        return Ok(None);
    }
    let sel_canon = canonical_dir(Path::new(selector));
    for row in &rows {
        let id = row.get("session_id").and_then(J::as_str);
        let path = row.get("path").and_then(J::as_str);
        if id == Some(selector) || path.is_some_and(|p| selector == p || canonical_dir(Path::new(p)) == sel_canon) {
            return Ok(id.map(str::to_owned));
        }
    }
    let mut matches: Vec<String> = Vec::new();
    if selector.len() >= 4 {
        for row in &rows {
            if let Some(id) = row.get("session_id").and_then(J::as_str)
                && id.starts_with(selector)
            {
                matches.push(id.to_owned());
            }
        }
    }
    if matches.is_empty() {
        for row in &rows {
            if row.get("name").and_then(J::as_str) == Some(selector)
                && let Some(id) = row.get("session_id").and_then(J::as_str)
            {
                matches.push(id.to_owned());
            }
        }
    }
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => Err(("invalid_arguments", format!("session id `{selector}` is ambiguous"))),
    }
}

fn opt_str(obj: &serde_json::Map<String, J>, key: &str) -> Result<Option<String>, OrchestratorError> {
    match obj.get(key) {
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_owned()))
            .ok_or_else(|| OrchestratorError::invalid_arguments(format!("{key} must be a string"))),
        None => Ok(None),
    }
}

fn parse_request_id(obj: &serde_json::Map<String, J>) -> Result<Option<String>, OrchestratorError> {
    match obj.get("request_id") {
        Some(v) => match v.as_str() {
            Some(s) if !s.is_empty() => Ok(Some(s.to_owned())),
            _ => Err(OrchestratorError::invalid_arguments("request_id must be a non-empty string")),
        },
        None => Ok(None),
    }
}

fn parse_subscribe(obj: &serde_json::Map<String, J>) -> Result<Option<bool>, OrchestratorError> {
    match obj.get("subscribe") {
        Some(v) => {
            v.as_bool().map(Some).ok_or_else(|| OrchestratorError::invalid_arguments("subscribe must be a boolean"))
        }
        None => Ok(None),
    }
}

async fn host_call_service_json(
    host: &Host,
    service: &str,
    method: &str,
    args: Vec<J>,
) -> Result<J, OrchestratorError> {
    let Some(Value::Table(svc)) = host.kernel.service(service) else {
        return Err(OrchestratorError::coded("unavailable", format!("service `{service}` is not available")));
    };
    match svc.get::<Value>(method) {
        Ok(Value::Function(_)) => {}
        Ok(Value::Nil) => {
            return Err(OrchestratorError::coded(
                "unavailable",
                format!("service `{service}` does not implement `{method}`"),
            ));
        }
        Ok(_) => {
            return Err(OrchestratorError::coded(
                "unavailable",
                format!("service `{service}.{method}` is not callable"),
            ));
        }
        Err(e) => return Err(OrchestratorError::internal(e.to_string())),
    }
    let lua = host.lua();
    let mut mv = MultiValue::new();
    for a in args {
        mv.push_back(mare_shared::lua::json_to_lua(lua, &a).map_err(|e| OrchestratorError::internal(e.to_string()))?);
    }
    let result = host.call_service(service, method, mv).await.map_err(|err| {
        if let Some(table) = mare_shared::lua_error::recall_error(lua, &err.to_string())
            && let Ok(rendered) = table.to_string()
            && let Some((code, msg)) = rendered.split_once(": ")
            && code != "internal"
            && (mare_shared::errors::is_core(code) || mare_shared::errors::is_domain_code(code))
        {
            return OrchestratorError::coded(code, msg);
        }
        for cause in err.chain() {
            if let Some(coded) = cause.downcast_ref::<mare_shared::errors::Coded>() {
                return OrchestratorError::coded(coded.code, coded.message.clone());
            }
            if let Some(lua_err) = cause.downcast_ref::<mlua::Error>()
                && let Some(coded) = lua_err.downcast_ref::<mare_shared::errors::Coded>()
            {
                return OrchestratorError::coded(coded.code, coded.message.clone());
            }
        }
        OrchestratorError::internal(err.to_string())
    })?;
    Ok(match result.into_iter().next() {
        Some(Value::Nil) | None => J::Null,
        Some(v) => mare_shared::lua::lua_to_json(lua, v).map_err(|e| OrchestratorError::internal(e.to_string()))?,
    })
}

/// Completion metadata returned by [`close_with_retry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseRetryOutcome {
    pub confirmed: bool,
    pub cancelled: bool,
    pub retries: usize,
}

/// Close a session until its stop is confirmed (within default retry bounds), answering once after
/// the first attempt and firing `done` when finished.
pub async fn close_until_confirmed<F, Fut>(attempt: F, answer: impl FnOnce(Result<(), String>), done: impl FnOnce())
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let _ = close_with_retry(
        true,
        DEFAULT_CLOSE_MAX_RETRIES,
        Duration::ZERO,
        CancellationToken::new(),
        attempt,
        answer,
        done,
    )
    .await;
}

/// Close a session with optional bounded retries and cancellation support until `attempt`
/// confirms (`Ok(())`), invoking `answer` after the first attempt and `done` after completion.
pub async fn close_with_retry<F, Fut>(
    retry_until_confirmed: bool,
    max_retries: usize,
    backoff: Duration,
    cancel: CancellationToken,
    mut attempt: F,
    answer: impl FnOnce(Result<(), String>),
    done: impl FnOnce(),
) -> CloseRetryOutcome
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let first = attempt().await;
    let mut confirmed = first.is_ok();
    answer(first);
    let mut retries = 0usize;
    let mut cancelled = false;
    if retry_until_confirmed && !confirmed {
        while retries < max_retries {
            if cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            if !backoff.is_zero() {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        cancelled = true;
                        break;
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
            } else {
                tokio::task::yield_now().await;
                if cancel.is_cancelled() {
                    cancelled = true;
                    break;
                }
            }
            retries += 1;
            let retry_res = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    cancelled = true;
                    break;
                }
                res = attempt() => res,
            };
            if retry_res.is_ok() {
                confirmed = true;
                break;
            }
        }
    }
    done();
    CloseRetryOutcome { confirmed, cancelled, retries }
}

pub(crate) async fn host_session_info_field(host: &Host, key: &str) -> Option<String> {
    let info = host.session_info().ok().flatten()?;
    info.get::<Option<String>>(key).ok().flatten().filter(|s| !s.is_empty())
}

pub(crate) async fn host_session_id(host: &Host) -> Option<String> {
    if let Some(id) = host_session_info_field(host, "session_id").await {
        return Some(id);
    }
    host.kernel.event_log().session_identity().0.filter(|s| !s.is_empty())
}

impl crate::RuntimeSet for SessionOrchestrator {
    fn reserve(&self, host: &Arc<Host>, session_id: &str) -> Result<(), SessionError> {
        if self.shutting_down.get() {
            return Err(SessionError::ShuttingDown);
        }
        if let Some(rt) = self.sessions.borrow().get(session_id)
            && !Arc::ptr_eq(&rt.host, host)
        {
            return Err(SessionError::AlreadyLoaded(session_id.to_owned()));
        }
        if self.closing.borrow().contains_key(session_id) {
            return Err(SessionError::Closing(session_id.to_owned()));
        }
        if self.loading.borrow().contains(session_id) {
            return Err(SessionError::Loading(session_id.to_owned()));
        }
        let stale_owner = {
            let reserved = self.reserved.borrow();
            reserved.get(session_id).copied().filter(|&owner| owner != host.instance_id)
        };
        if let Some(owner) = stale_owner {
            let owner_host =
                self.sessions.borrow().values().find(|rt| rt.host.instance_id == owner).map(|rt| Arc::clone(&rt.host));
            if let Some(owner_host) = owner_host {
                owner_host.identity.purge_cancelled_switch();
            }
        }
        let mut reserved = self.reserved.borrow_mut();
        if let Some(&owner) = reserved.get(session_id)
            && owner != host.instance_id
        {
            return Err(SessionError::AlreadyLoaded(session_id.to_owned()));
        }
        reserved.insert(session_id.to_owned(), host.instance_id);
        Ok(())
    }

    fn rekey(&self, host: &Arc<Host>, new_sid: &str) -> Result<(), SessionError> {
        self.rekey_host_session(host, new_sid)
    }

    fn release(&self, host: &Arc<Host>, session_id: &str) {
        let mut reserved = self.reserved.borrow_mut();
        if reserved.get(session_id) == Some(&host.instance_id) {
            reserved.remove(session_id);
        }
    }

    fn list(&self) -> Vec<crate::LoadedSessionEntry> {
        let mut out: Vec<crate::LoadedSessionEntry> = self
            .sessions
            .borrow()
            .values()
            .map(|rt| crate::LoadedSessionEntry {
                session_id: rt.session_id.clone(),
                session_epoch: Some(rt.host.kernel.event_log().session_identity().1),
            })
            .collect();
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mare_shared::ui::Mode;

    fn test_opts(cwd: PathBuf) -> HostOptions {
        HostOptions {
            cwd,
            mode: Mode::Print,
            config: json!({
                "shutdown_grace_ms": 50,
                "plugins": { "permissions": { "enabled": false } }
            }),
            load_init: false,
            project_trusted: false,
            args: json!({ "no_session": true }),
            no_user_plugins: true,
        }
    }

    #[tokio::test]
    async fn single_orchestrator_holds_weak_host_without_strong_cycle() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = std::env::temp_dir().join(format!(
                    "mare-orch-weak-{}",
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
                ));
                std::fs::create_dir_all(&dir).unwrap();
                let host = Host::new(test_opts(dir.clone())).await.unwrap();
                let orch = host.orchestrator().expect("Host::new attaches Single orchestrator");
                assert!(!orch.is_multi());
                let weak_host = Arc::downgrade(&host);
                host.shutdown().await;
                drop(host);
                assert!(
                    weak_host.upgrade().is_none(),
                    "Single SessionOrchestrator must not keep Host alive via a strong reference cycle"
                );
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[test]
    fn check_project_scope_allows_unrestricted_and_same_dir_and_rejects_outside() {
        let orch = SessionOrchestrator::new_multi();
        let proj = std::env::temp_dir();
        orch.set_project_cwd(&proj);

        let unrestricted = OrchestratorCaller::host();
        assert!(
            orch.check_project_scope(
                &unrestricted,
                "session.create",
                &proj,
                Some(Path::new("/nonexistent/other")),
                || OrchestratorError::denied("denied"),
            )
            .is_ok()
        );

        let restricted = OrchestratorCaller {
            receipt_scope: "peer:1".into(),
            ctx_id: Some(1),
            conn_id: None,
            unrestricted: false,
            may_list: false,
            allow_path: false,
            is_admin: false,
        };
        assert!(
            orch.check_project_scope(&restricted, "session.create", &proj, Some(&proj), || {
                OrchestratorError::denied("denied")
            })
            .is_ok()
        );
        let err = orch
            .check_project_scope(&restricted, "session.create", &proj, Some(Path::new("/nonexistent/other")), || {
                OrchestratorError::denied("outside")
            })
            .unwrap_err();
        assert_eq!(err.code, "denied");
        assert_eq!(err.message, "outside");
    }

    #[test]
    fn op_receipts_are_bounded_at_256_isolated_by_scope_and_cleared_per_session() {
        let orch = SessionOrchestrator::new_multi();
        for i in 0..257 {
            orch.store_receipt(
                "primary",
                "create",
                Some(format!("req-{i}")),
                json!({ "n": i }),
                if i % 2 == 0 { "sid-a" } else { "sid-b" },
                &json!({ "saved": i }),
            );
        }
        assert_eq!(orch.receipts_len(), SESSION_OP_RECEIPT_CAPACITY);
        assert!(orch.lookup_receipt("primary", "create", Some("req-0"), &json!({ "n": 0 })).unwrap().is_none());
        assert_eq!(
            orch.lookup_receipt("primary", "create", Some("req-256"), &json!({ "n": 256 })).unwrap(),
            Some(("sid-a".to_owned(), json!({ "saved": 256 })))
        );
        // Different caller scope does not see primary's receipt.
        assert!(orch.lookup_receipt("peer:7", "create", Some("req-256"), &json!({ "n": 256 })).unwrap().is_none());
        // Mismatched fingerprint returns request_id_conflict.
        let conflict = orch.lookup_receipt("primary", "create", Some("req-256"), &json!({ "n": 999 })).unwrap_err();
        assert_eq!(conflict.code, "request_id_conflict");

        // Removing receipts for sid-a preserves sid-b receipts.
        orch.remove_receipts_for_session("sid-a");
        assert!(orch.lookup_receipt("primary", "create", Some("req-256"), &json!({ "n": 256 })).unwrap().is_none());
        assert_eq!(
            orch.lookup_receipt("primary", "create", Some("req-255"), &json!({ "n": 255 })).unwrap(),
            Some(("sid-b".to_owned(), json!({ "saved": 255 })))
        );
    }

    #[tokio::test]
    async fn replay_receipt_rejects_closed_session_and_evicts_stale_entry() {
        let orch = SessionOrchestrator::new_multi();
        let caller = OrchestratorCaller::host();
        let fp = json!({ "cwd": "/tmp" });
        orch.store_receipt(
            &caller.receipt_scope,
            "create",
            Some("req-dead".into()),
            fp.clone(),
            "sid-dead",
            &json!({ "session_id": "sid-dead", "session_epoch": "ep-1" }),
        );
        let replayed = orch.replay_receipt(&caller, "create", Some("req-dead"), &fp, None).await.unwrap();
        assert!(
            replayed.is_none(),
            "replay_receipt must not return a snapshot for an unloaded/closed session: {replayed:?}"
        );
        assert_eq!(orch.receipts_len(), 0, "stale receipt must be evicted when liveness check fails");
    }

    #[test]
    fn single_close_selector_covers_exact_path_prefix_and_ambiguity() {
        let active_id = Some("abcd1234-0000-0000-0000-000000000001");
        let active_path = Some("/tmp/s1.jsonl");

        // Exact id & exact path close active session.
        assert_eq!(
            match_single_close_selector("abcd1234-0000-0000-0000-000000000001", active_id, active_path, Ok(None))
                .unwrap(),
            SingleCloseMatch::CloseActive
        );
        assert_eq!(
            match_single_close_selector("/tmp/s1.jsonl", active_id, active_path, Ok(None)).unwrap(),
            SingleCloseMatch::CloseActive
        );
        // Unique >=4-char prefix matching active when resolver finds nothing closes active.
        assert_eq!(
            match_single_close_selector("abcd", active_id, active_path, Ok(None)).unwrap(),
            SingleCloseMatch::CloseActive
        );
        // Short (<4-char) prefix does not match active implicitly.
        assert_eq!(
            match_single_close_selector("abc", active_id, active_path, Ok(None)).unwrap_err().code,
            "unknown_session"
        );
        // Prefix matching active AND resolving to a different persisted session is ambiguous.
        assert_eq!(
            match_single_close_selector(
                "abcd",
                active_id,
                active_path,
                Ok(Some("abcd9999-0000-0000-0000-000000000002"))
            )
            .unwrap_err()
            .code,
            "invalid_arguments"
        );
        // Resolver ambiguity error propagates as invalid_arguments even if active prefix matches.
        assert_eq!(
            match_single_close_selector("abcd", active_id, active_path, Err(("invalid_arguments", "ambiguous".into())))
                .unwrap_err()
                .code,
            "invalid_arguments"
        );
        // Resolved target matching active id closes active.
        assert_eq!(
            match_single_close_selector(
                "my-session-name",
                active_id,
                active_path,
                Ok(Some("abcd1234-0000-0000-0000-000000000001"))
            )
            .unwrap(),
            SingleCloseMatch::CloseActive
        );
        // Resolved target pointing to a different unloaded session is a no-op.
        assert_eq!(
            match_single_close_selector(
                "other-session",
                active_id,
                active_path,
                Ok(Some("99999999-0000-0000-0000-000000000009"))
            )
            .unwrap(),
            SingleCloseMatch::NoopUnloaded
        );
    }

    #[tokio::test]
    async fn single_orchestrator_rejects_cross_cwd_session_create() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir_a = std::env::temp_dir().join(format!("mare-orch-n3-a-{ts}"));
                let dir_b = std::env::temp_dir().join(format!("mare-orch-n3-b-{ts}"));
                std::fs::create_dir_all(&dir_a).unwrap();
                std::fs::create_dir_all(&dir_b).unwrap();
                let host = Host::new(test_opts(dir_a.clone())).await.unwrap();
                let orch = host.orchestrator().expect("Host::new attaches Single orchestrator");
                assert!(!orch.is_multi());
                let caller = OrchestratorCaller::host();

                let err = orch
                    .create(&caller, &host, &json!({ "cwd": dir_b.to_string_lossy() }))
                    .await
                    .expect_err("single-host session.create with different cwd must be rejected");
                assert_eq!(
                    err.code, "unsupported",
                    "cross-cwd create in Capacity::Single must fail with unsupported: {err:?}"
                );

                // Same cwd (even unnormalized) must still be allowed.
                let same = orch.create(&caller, &host, &json!({ "cwd": dir_a.join(".").to_string_lossy() })).await;
                assert!(same.is_ok(), "same-cwd create in Capacity::Single must succeed: {same:?}");

                host.shutdown().await;
                let _ = std::fs::remove_dir_all(&dir_a);
                let _ = std::fs::remove_dir_all(&dir_b);
            })
            .await;
    }

    struct StubLifecycleExt {
        base_cwd: PathBuf,
        selectors: RefCell<HashMap<String, PathBuf>>,
        opened_ids: RefCell<Vec<String>>,
        rollback_ids: RefCell<Vec<String>>,
        closed_ids: RefCell<Vec<String>>,
        fail_create_get: Cell<bool>,
        fail_load_get: Cell<bool>,
        enable_project_plugins: Cell<bool>,
        pause_apply_defaults: RefCell<Option<(Rc<Notify>, Rc<Notify>)>>,
        pause_resolve_selector: RefCell<Option<(Rc<Notify>, Rc<Notify>)>>,
    }

    impl StubLifecycleExt {
        fn new(base_cwd: PathBuf) -> Self {
            Self {
                base_cwd,
                selectors: RefCell::new(HashMap::new()),
                opened_ids: RefCell::new(Vec::new()),
                rollback_ids: RefCell::new(Vec::new()),
                closed_ids: RefCell::new(Vec::new()),
                fail_create_get: Cell::new(false),
                fail_load_get: Cell::new(false),
                enable_project_plugins: Cell::new(false),
                pause_apply_defaults: RefCell::new(None),
                pause_resolve_selector: RefCell::new(None),
            }
        }
    }

    impl OrchestratorExt for StubLifecycleExt {
        fn clean_opts(
            &self,
            cwd_override: Option<&str>,
            no_session_override: Option<bool>,
        ) -> Result<HostOptions, OrchestratorError> {
            let cwd = cwd_override.map(PathBuf::from).unwrap_or_else(|| self.base_cwd.clone());
            let mut opts = test_opts(cwd);
            if no_session_override == Some(false) {
                opts.args = json!({});
            }
            if self.enable_project_plugins.get() {
                opts.no_user_plugins = false;
                opts.project_trusted = true;
            }
            Ok(opts)
        }

        fn before_ready(
            &self,
            _caller: &OrchestratorCaller,
            _pre_sid: Option<String>,
            host: Arc<Host>,
        ) -> Pin<Box<dyn Future<Output = ()>>> {
            let should_fail = self.fail_load_get.replace(false);
            Box::pin(async move {
                if should_fail {
                    let svc = host.service_table(SESSION_SERVICE).expect("session service table");
                    let fail_fn = host
                        .lua()
                        .create_function(|_, _: MultiValue| -> mlua::Result<Value> {
                            Err(mlua::Error::runtime("injected load get failure"))
                        })
                        .unwrap();
                    svc.set("get", fail_fn).unwrap();
                }
            })
        }

        fn rollback_before_ready(&self, _caller: &OrchestratorCaller, sid: &str, _host: &Arc<Host>) {
            self.rollback_ids.borrow_mut().push(sid.to_owned());
        }

        fn apply_defaults(&self, rt: &Rc<SessionRuntime>) -> Pin<Box<dyn Future<Output = ()>>> {
            let should_fail = self.fail_create_get.replace(false);
            let pause = self.pause_apply_defaults.borrow_mut().take();
            let host = Arc::clone(&rt.host);
            Box::pin(async move {
                if let Some((entered, release)) = pause {
                    entered.notify_waiters();
                    release.notified().await;
                }
                if should_fail {
                    let svc = host.service_table(SESSION_SERVICE).expect("session service table");
                    let fail_fn = host
                        .lua()
                        .create_function(|_, _: MultiValue| -> mlua::Result<Value> {
                            Err(mlua::Error::runtime("injected create get failure"))
                        })
                        .unwrap();
                    svc.set("get", fail_fn).unwrap();
                }
            })
        }

        fn opened(
            &self,
            _caller: &OrchestratorCaller,
            rt: &Rc<SessionRuntime>,
            _make_default: bool,
            _subscribe: Option<bool>,
        ) -> Pin<Box<dyn Future<Output = ()>>> {
            self.opened_ids.borrow_mut().push(rt.session_id.clone());
            Box::pin(async {})
        }

        fn closed(&self, session_id: &str) {
            self.closed_ids.borrow_mut().push(session_id.to_owned());
        }

        fn resolve_selector(
            &self,
            _anchor: &Arc<Host>,
            _caller: &OrchestratorCaller,
            selector: &str,
            _include_archived: bool,
        ) -> Pin<Box<dyn Future<Output = Result<String, OrchestratorError>>>> {
            let found = self.selectors.borrow().get(selector).map(|p| p.to_string_lossy().into_owned());
            let sel = selector.to_owned();
            let pause = self.pause_resolve_selector.borrow_mut().take();
            Box::pin(async move {
                if let Some((entered, release)) = pause {
                    entered.notify_waiters();
                    release.notified().await;
                }
                found.ok_or_else(|| OrchestratorError::unknown_session(format!("no session matches `{sel}`")))
            })
        }
    }

    #[tokio::test]
    async fn multi_orchestrator_close_unloaded_persisted_session_is_idempotent_and_enforces_scope() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir_a = std::env::temp_dir().join(format!("mare-orch-close-a-{ts}"));
                let dir_b = std::env::temp_dir().join(format!("mare-orch-close-b-{ts}"));
                std::fs::create_dir_all(&dir_a).unwrap();
                std::fs::create_dir_all(&dir_b).unwrap();

                let sid_a = "aaaa1111-0000-0000-0000-000000000001";
                let sid_b = "bbbb2222-0000-0000-0000-000000000002";
                let path_a = dir_a.join("a.jsonl");
                let path_b = dir_b.join("b.jsonl");
                std::fs::write(
                    &path_a,
                    format!("{}\n", json!({ "type": "session", "version": 4, "session_id": sid_a, "cwd": dir_a })),
                )
                .unwrap();
                std::fs::write(
                    &path_b,
                    format!("{}\n", json!({ "type": "session", "version": 4, "session_id": sid_b, "cwd": dir_b })),
                )
                .unwrap();

                let orch = SessionOrchestrator::new_multi();
                let ext = Rc::new(StubLifecycleExt::new(dir_a.clone()));
                *ext.selectors.borrow_mut() = HashMap::from([
                    (sid_a.to_owned(), path_a.clone()),
                    ("aaaa".to_owned(), path_a.clone()),
                    (sid_b.to_owned(), path_b.clone()),
                ]);
                orch.set_ext(ext.clone());

                let anchor = Host::new(test_opts(dir_a.clone())).await.unwrap();
                let owner = OrchestratorCaller::host();
                let restricted = OrchestratorCaller {
                    receipt_scope: "peer:9".into(),
                    ctx_id: Some(9),
                    conn_id: None,
                    unrestricted: false,
                    may_list: false,
                    allow_path: false,
                    is_admin: false,
                };

                // Unloaded persisted session closes idempotently with {} when expected_epoch is omitted
                // without emitting `ext.closed` (which could wipe a concurrently loaded runtime).
                let ok = orch.close(&owner, &anchor, &json!({ "session_id": sid_a })).await.unwrap();
                assert_eq!(ok, json!({}));
                assert!(
                    ext.closed_ids.borrow().is_empty(),
                    "idempotent close of an already-unloaded session must not invoke ext.closed: {:?}",
                    ext.closed_ids.borrow()
                );

                // Passing expected_epoch for an unloaded session returns stale_session (matching single-host mode).
                let stale = orch
                    .close(&owner, &anchor, &json!({ "session_id": sid_a, "expected_epoch": "old-epoch" }))
                    .await
                    .unwrap_err();
                assert_eq!(stale.code, "stale_session");

                // Prefix selector resolving to a different exact session_id does not silently succeed as no-op.
                let prefix_err = orch.close(&owner, &anchor, &json!({ "session_id": "aaaa" })).await.unwrap_err();
                assert_eq!(prefix_err.code, "unknown_session");

                // Truly unknown session fails with unknown_session.
                let missing = orch.close(&owner, &anchor, &json!({ "session_id": "missing-id" })).await.unwrap_err();
                assert_eq!(missing.code, "unknown_session");

                // Restricted caller closing an unloaded persisted session outside its project fails closed.
                let cross = orch.close(&restricted, &anchor, &json!({ "session_id": sid_b })).await.unwrap_err();
                assert_eq!(cross.code, "unknown_session");

                anchor.shutdown().await;
                let _ = orch.shutdown_all().await;
                let _ = std::fs::remove_dir_all(&dir_a);
                let _ = std::fs::remove_dir_all(&dir_b);
            })
            .await;
    }

    #[tokio::test]
    async fn multi_orchestrator_reclaims_runtime_when_post_create_or_fork_session_get_fails() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir = std::env::temp_dir().join(format!("mare-orch-reclaim-{ts}"));
                std::fs::create_dir_all(&dir).unwrap();

                let orch = SessionOrchestrator::new_multi();
                let ext = Rc::new(StubLifecycleExt::new(dir.clone()));
                ext.fail_create_get.set(true);
                orch.set_ext(ext.clone());

                let anchor = Host::new(test_opts(dir.clone())).await.unwrap();
                let caller = OrchestratorCaller::host();

                // 1. Multi-host session.create failure during post-create session.get must close & remove the runtime.
                let create_err = orch
                    .create(&caller, &anchor, &json!({ "request_id": "req-create-fail" }))
                    .await
                    .expect_err("injected session.get failure must fail session.create");
                assert!(create_err.message.contains("injected create get failure"), "{create_err:?}");
                assert!(
                    orch.sessions.borrow().is_empty(),
                    "failed session.create must not leak a live runtime: {:?}",
                    orch.sessions.borrow().keys().collect::<Vec<_>>()
                );
                assert_eq!(ext.closed_ids.borrow().len(), 1, "ext.closed must be called for reclaimed create runtime");
                assert_eq!(orch.receipts_len(), 0, "failed create must not store a receipt");

                // 2. Multi-host session.fork failure during post-fork session.get must close & remove the forked runtime.
                let mut src_opts = test_opts(dir.clone());
                src_opts.args = json!({});
                let src = orch.create_session(src_opts).await.unwrap();
                let src_sid = src.session_id.clone();
                let _ = host_call_service_json(
                    &src.host,
                    SESSION_SERVICE,
                    "persist_message",
                    vec![json!({
                        "message": { "role": "user", "content": [{ "type": "text", "text": "seed" }] },
                        "durable": true,
                        "session_id": src_sid,
                        "session_epoch": src.session_epoch,
                    })],
                )
                .await
                .unwrap();
                let _ = host_call_service_json(
                    &src.host,
                    SESSION_SERVICE,
                    "record_run_end",
                    vec![json!({
                        "run_id": "run-seed",
                        "session_id": src_sid,
                        "session_epoch": src.session_epoch,
                    })],
                )
                .await
                .unwrap();
                let src_path = host_session_info_field(&src.host, "path").await;
                // Inject a `session.get` failure on the forked Host during boot (before `ext.opened` runs).
                let plugin_dir = dir.join(".mare").join("plugins");
                std::fs::create_dir_all(&plugin_dir).unwrap();
                std::fs::write(
                    plugin_dir.join("fail-fork-get.lua"),
                    r#"return {
                        name = "fail-fork-get",
                        inject = { "session" },
                        apply = function(ctx)
                            if mare.args and mare.args.fork_session then
                                ctx:on("app/ready", function()
                                    local orig = mare.contracts.is_peer
                                    mare.contracts.is_peer = function(...)
                                        mare.contracts.is_peer = orig
                                        error("injected fork get failure")
                                    end
                                end)
                            end
                        end,
                    }"#,
                )
                .unwrap();
                ext.enable_project_plugins.set(true);
                ext.opened_ids.borrow_mut().clear();
                let fork_err = orch
                    .fork(&caller, &anchor, &json!({ "session_id": src_sid, "request_id": "req-fork-fail" }))
                    .await
                    .expect_err("injected session.get failure must fail session.fork");
                assert!(fork_err.message.contains("injected fork get failure"), "{fork_err:?}");
                assert!(
                    ext.opened_ids.borrow().is_empty(),
                    "failed session.fork must not call ext.opened before session.get succeeds: {:?}",
                    ext.opened_ids.borrow()
                );
                assert_eq!(
                    orch.sessions.borrow().keys().cloned().collect::<Vec<_>>(),
                    vec![src_sid.clone()],
                    "failed session.fork must leave only the source runtime"
                );
                assert_eq!(ext.closed_ids.borrow().len(), 2, "ext.closed must be called for reclaimed fork runtime");
                assert_eq!(orch.receipts_len(), 0, "failed fork must not store a receipt");

                anchor.shutdown().await;
                let _ = orch.shutdown_all().await;
                if let Some(p) = src_path {
                    let p = PathBuf::from(p);
                    let _ = std::fs::remove_file(&p);
                    let _ = std::fs::remove_file(mare_store::lock_path(&p));
                }
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test]
    async fn multi_orchestrator_load_fails_before_opened_and_reclaims_new_runtime_on_get_error() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir = std::env::temp_dir().join(format!("mare-orch-load-get-fail-{ts}"));
                std::fs::create_dir_all(&dir).unwrap();

                let sid = "cccc3333-0000-0000-0000-000000000003";
                let path = dir.join("c.jsonl");
                std::fs::write(
                    &path,
                    format!(
                        "{}\n{}\n",
                        json!({ "type": "session", "version": 3, "session_id": sid, "cwd": dir }),
                        json!({ "type": "message", "entry_id": "m1", "parent_entry_id": null, "message": { "role": "user", "content": "hi" } }),
                    ),
                )
                .unwrap();

                let orch = SessionOrchestrator::new_multi();
                let ext = Rc::new(StubLifecycleExt::new(dir.clone()));
                ext.selectors.borrow_mut().insert(sid.to_owned(), path.clone());
                ext.selectors.borrow_mut().insert(path.to_string_lossy().into_owned(), path.clone());
                ext.fail_load_get.set(true);
                orch.set_ext(ext.clone());

                let anchor = Host::new(test_opts(dir.clone())).await.unwrap();
                let caller = OrchestratorCaller::host();

                // 1. Newly loaded runtime: `session.get` fails -> runtime is reclaimed, before_ready rolled back,
                //    `ext.closed` called, and `ext.opened` is NEVER called.
                let err = orch
                    .load(&caller, &anchor, &json!({ "session_id": sid }))
                    .await
                    .expect_err("injected session.get failure must fail session.load");
                assert!(err.message.contains("injected load get failure"), "{err:?}");
                assert!(
                    orch.sessions.borrow().is_empty(),
                    "failed session.load must reclaim the newly created runtime: {:?}",
                    orch.sessions.borrow().keys().collect::<Vec<_>>()
                );
                assert!(
                    ext.opened_ids.borrow().is_empty(),
                    "failed session.load must not call ext.opened before session.get succeeds: {:?}",
                    ext.opened_ids.borrow()
                );
                assert_eq!(ext.rollback_ids.borrow().as_slice(), &[sid]);
                assert_eq!(ext.closed_ids.borrow().as_slice(), &[sid]);

                // 2. Reused already-loaded runtime: `session.get` fails -> `ext.opened` is NOT called and
                //    the existing runtime remains loaded (not closed).
                let ok_snap = orch.load(&caller, &anchor, &json!({ "session_id": sid })).await.unwrap();
                assert_eq!(ok_snap["session_id"], json!(sid));
                ext.opened_ids.borrow_mut().clear();
                ext.closed_ids.borrow_mut().clear();

                let rt = orch.session(sid).expect("loaded session runtime");
                let svc = rt.host.service_table(SESSION_SERVICE).expect("session service table");
                let fail_fn = rt
                    .host
                    .lua()
                    .create_function(|_, _: MultiValue| -> mlua::Result<Value> {
                        Err(mlua::Error::runtime("injected reuse get failure"))
                    })
                    .unwrap();
                svc.set("get", fail_fn).unwrap();

                let reuse_by_id = orch
                    .load(&caller, &anchor, &json!({ "session_id": sid }))
                    .await
                    .expect_err("reused runtime get failure by id must error");
                assert!(reuse_by_id.message.contains("injected reuse get failure"), "{reuse_by_id:?}");
                assert!(
                    ext.opened_ids.borrow().is_empty(),
                    "failed reuse load by id must not call ext.opened: {:?}",
                    ext.opened_ids.borrow()
                );

                let reuse_by_path = orch
                    .load(&caller, &anchor, &json!({ "path": path.to_string_lossy() }))
                    .await
                    .expect_err("reused runtime get failure by path must error");
                assert!(reuse_by_path.message.contains("injected reuse get failure"), "{reuse_by_path:?}");
                assert!(
                    ext.opened_ids.borrow().is_empty(),
                    "failed reuse load by path must not call ext.opened: {:?}",
                    ext.opened_ids.borrow()
                );
                assert!(orch.session(sid).is_ok(), "failed reuse load must leave existing runtime alive");
                assert!(ext.closed_ids.borrow().is_empty());

                anchor.shutdown().await;
                let _ = orch.shutdown_all().await;
                let _ = std::fs::remove_file(mare_store::lock_path(&path));
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test]
    async fn concurrent_create_and_fork_request_id_deduplicates_conflicts_and_releases_on_error() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir = std::env::temp_dir().join(format!("mare-orch-inflight-req-{ts}"));
                std::fs::create_dir_all(&dir).unwrap();

                let orch = SessionOrchestrator::new_multi();
                let ext = Rc::new(StubLifecycleExt::new(dir.clone()));
                orch.set_ext(ext.clone());

                let anchor = Host::new(test_opts(dir.clone())).await.unwrap();
                let caller = OrchestratorCaller::host();

                // 1. Overlapping `session.create` with same request_id + same params shares one creation,
                //    while a concurrent request with different params fails immediately with request_id_conflict.
                let entered = Rc::new(Notify::new());
                let release = Rc::new(Notify::new());
                *ext.pause_apply_defaults.borrow_mut() = Some((Rc::clone(&entered), Rc::clone(&release)));

                let orch1 = Rc::clone(&orch);
                let caller1 = caller.clone();
                let anchor1 = Arc::clone(&anchor);
                let first = tokio::task::spawn_local(async move {
                    orch1.create(&caller1, &anchor1, &json!({ "request_id": "req-dup-create", "name": "alpha" })).await
                });
                entered.notified().await;

                // Overlapping different fingerprint -> immediate conflict.
                let conflict = orch
                    .create(&caller, &anchor, &json!({ "request_id": "req-dup-create", "name": "beta" }))
                    .await
                    .expect_err("different params while in-flight must return request_id_conflict");
                assert_eq!(conflict.code, "request_id_conflict");

                // Overlapping same fingerprint -> waits for first and receives identical session snapshot.
                let orch2 = Rc::clone(&orch);
                let caller2 = caller.clone();
                let anchor2 = Arc::clone(&anchor);
                let second = tokio::task::spawn_local(async move {
                    orch2.create(&caller2, &anchor2, &json!({ "request_id": "req-dup-create", "name": "alpha" })).await
                });
                tokio::task::yield_now().await;

                release.notify_waiters();
                let snap1 = first.await.unwrap().expect("first create must succeed");
                let snap2 = second.await.unwrap().expect("second overlapping create must reuse first result");
                assert_eq!(snap1["session_id"], snap2["session_id"], "overlapping creates must return one session");
                assert_eq!(orch.len(), 1, "only one session runtime must be created: {:?}", orch.active_ids());

                // 2. Failure releases the reservation so a later retry with the same request_id succeeds.
                let err_entered = Rc::new(Notify::new());
                let err_release = Rc::new(Notify::new());
                *ext.pause_apply_defaults.borrow_mut() = Some((Rc::clone(&err_entered), Rc::clone(&err_release)));
                ext.fail_create_get.set(true);

                let orch_f1 = Rc::clone(&orch);
                let caller_f1 = caller.clone();
                let anchor_f1 = Arc::clone(&anchor);
                let fail_first = tokio::task::spawn_local(async move {
                    orch_f1.create(&caller_f1, &anchor_f1, &json!({ "request_id": "req-fail-release" })).await
                });
                err_entered.notified().await;

                let orch_f2 = Rc::clone(&orch);
                let caller_f2 = caller.clone();
                let anchor_f2 = Arc::clone(&anchor);
                let fail_second = tokio::task::spawn_local(async move {
                    orch_f2.create(&caller_f2, &anchor_f2, &json!({ "request_id": "req-fail-release" })).await
                });
                tokio::task::yield_now().await;

                err_release.notify_waiters();
                let e1 = fail_first.await.unwrap().expect_err("first create fails via injected get");
                let e2 = fail_second.await.unwrap().expect_err("waiting create observes failure");
                assert!(e1.message.contains("injected create get failure"), "{e1:?}");
                assert!(e2.message.contains("injected create get failure"), "{e2:?}");
                assert!(orch.in_flight_receipts.borrow().is_empty(), "failure must release in-flight reservation");

                let retry_ok = orch
                    .create(&caller, &anchor, &json!({ "request_id": "req-fail-release" }))
                    .await
                    .expect("subsequent retry after failure must succeed");
                assert!(retry_ok["session_id"].as_str().is_some_and(|s| !s.is_empty()));

                // 3. Cancellation (drop) of the first caller releases the reservation and wakes a
                //    waiting concurrent caller to take over cleanly.
                let cancel_entered = Rc::new(Notify::new());
                let cancel_release = Rc::new(Notify::new());
                *ext.pause_apply_defaults.borrow_mut() = Some((Rc::clone(&cancel_entered), Rc::clone(&cancel_release)));

                let orch_c1 = Rc::clone(&orch);
                let caller_c1 = caller.clone();
                let anchor_c1 = Arc::clone(&anchor);
                let cancel_first = tokio::task::spawn_local(async move {
                    orch_c1.create(&caller_c1, &anchor_c1, &json!({ "request_id": "req-cancel-takeover" })).await
                });
                cancel_entered.notified().await;

                let orch_c2 = Rc::clone(&orch);
                let caller_c2 = caller.clone();
                let anchor_c2 = Arc::clone(&anchor);
                let cancel_second = tokio::task::spawn_local(async move {
                    orch_c2.create(&caller_c2, &anchor_c2, &json!({ "request_id": "req-cancel-takeover" })).await
                });
                tokio::task::yield_now().await;

                cancel_first.abort();
                let _ = cancel_first.await;
                let takeover_snap = cancel_second
                    .await
                    .unwrap()
                    .expect("waiting caller must take over and succeed after first caller is cancelled");
                assert!(takeover_snap["session_id"].as_str().is_some_and(|s| !s.is_empty()));

                // 4. Overlapping `session.fork` (unloaded persisted session paused in resolve_selector)
                //    creates only one forked runtime and conflicts on different parameters.
                let mut src_opts = test_opts(dir.clone());
                src_opts.args = json!({});
                let src = orch.create_session(src_opts).await.unwrap();
                let src_sid = src.session_id.clone();
                let _ = host_call_service_json(
                    &src.host,
                    SESSION_SERVICE,
                    "persist_message",
                    vec![json!({
                        "message": { "role": "user", "content": [{ "type": "text", "text": "seed" }] },
                        "durable": true,
                        "session_id": src_sid,
                        "session_epoch": src.session_epoch,
                    })],
                )
                .await
                .unwrap();
                let _ = host_call_service_json(
                    &src.host,
                    SESSION_SERVICE,
                    "record_run_end",
                    vec![json!({
                        "run_id": "run-seed",
                        "session_id": src_sid,
                        "session_epoch": src.session_epoch,
                    })],
                )
                .await
                .unwrap();
                let src_path = PathBuf::from(host_session_info_field(&src.host, "path").await.expect("source path"));
                let _ = orch.close_session(&src_sid).await.unwrap();
                ext.selectors.borrow_mut().insert(src_sid.clone(), src_path.clone());
                let before_fork_count = orch.len();

                let fk_entered = Rc::new(Notify::new());
                let fk_release = Rc::new(Notify::new());
                *ext.pause_resolve_selector.borrow_mut() = Some((Rc::clone(&fk_entered), Rc::clone(&fk_release)));

                let orch_fk1 = Rc::clone(&orch);
                let caller_fk1 = caller.clone();
                let anchor_fk1 = Arc::clone(&anchor);
                let src_sid1 = src_sid.clone();
                let fork1 = tokio::task::spawn_local(async move {
                    orch_fk1
                        .fork(
                            &caller_fk1,
                            &anchor_fk1,
                            &json!({ "session_id": src_sid1, "request_id": "req-dup-fork", "name": "fork-a" }),
                        )
                        .await
                });
                fk_entered.notified().await;

                let fk_conflict = orch
                    .fork(
                        &caller,
                        &anchor,
                        &json!({ "session_id": src_sid, "request_id": "req-dup-fork", "name": "fork-b" }),
                    )
                    .await
                    .expect_err("different fork params while in-flight must return request_id_conflict");
                assert_eq!(fk_conflict.code, "request_id_conflict");

                // Differing only in `expected_epoch` must also return `request_id_conflict`.
                let fk_epoch_conflict = orch
                    .fork(
                        &caller,
                        &anchor,
                        &json!({
                            "session_id": src_sid,
                            "request_id": "req-dup-fork",
                            "name": "fork-a",
                            "expected_epoch": "epoch-other",
                        }),
                    )
                    .await
                    .expect_err("fork differing only in expected_epoch must return request_id_conflict");
                assert_eq!(fk_epoch_conflict.code, "request_id_conflict");

                let orch_fk2 = Rc::clone(&orch);
                let caller_fk2 = caller.clone();
                let anchor_fk2 = Arc::clone(&anchor);
                let src_sid2 = src_sid.clone();
                let fork2 = tokio::task::spawn_local(async move {
                    orch_fk2
                        .fork(
                            &caller_fk2,
                            &anchor_fk2,
                            &json!({ "session_id": src_sid2, "request_id": "req-dup-fork", "name": "fork-a" }),
                        )
                        .await
                });
                tokio::task::yield_now().await;

                fk_release.notify_waiters();
                let fsnap1 = fork1.await.unwrap().expect("first fork must succeed");
                let fsnap2 = fork2.await.unwrap().expect("second overlapping fork must reuse first result");
                assert_eq!(fsnap1["session_id"], fsnap2["session_id"], "overlapping forks must return one session");
                assert_eq!(orch.len(), before_fork_count + 1, "only one forked runtime must be created");

                anchor.shutdown().await;
                let _ = orch.shutdown_all().await;
                let _ = std::fs::remove_file(&src_path);
                let _ = std::fs::remove_file(mare_store::lock_path(&src_path));
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test]
    async fn idempotent_unloaded_close_does_not_clear_concurrently_loaded_session() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir = std::env::temp_dir().join(format!("mare-orch-unloaded-close-race-{ts}"));
                std::fs::create_dir_all(&dir).unwrap();

                let sid = "eeee5555-0000-0000-0000-000000000005";
                let path = dir.join("e.jsonl");
                std::fs::write(
                    &path,
                    format!(
                        "{}\n{}\n",
                        json!({ "type": "session", "version": 3, "session_id": sid, "cwd": dir }),
                        json!({ "type": "message", "entry_id": "m1", "parent_entry_id": null, "message": { "role": "user", "content": "hi" } }),
                    ),
                )
                .unwrap();

                let orch = SessionOrchestrator::new_multi();
                let ext = Rc::new(StubLifecycleExt::new(dir.clone()));
                ext.selectors.borrow_mut().insert(sid.to_owned(), path.clone());
                orch.set_ext(ext.clone());

                let anchor = Host::new(test_opts(dir.clone())).await.unwrap();
                let owner = OrchestratorCaller::host();

                let entered = Rc::new(Notify::new());
                let release = Rc::new(Notify::new());
                *ext.pause_resolve_selector.borrow_mut() = Some((Rc::clone(&entered), Rc::clone(&release)));

                // Start closing while `sid` is unloaded; pause inside `resolve_selector`.
                let orch_close = Rc::clone(&orch);
                let owner_close = owner.clone();
                let anchor_close = Arc::clone(&anchor);
                let close_task = tokio::task::spawn_local(async move {
                    orch_close.close(&owner_close, &anchor_close, &json!({ "session_id": sid })).await
                });
                entered.notified().await;

                // While `close` is paused resolving the unloaded session, another request loads `sid`.
                let loaded_snap = orch.load(&owner, &anchor, &json!({ "session_id": sid })).await.unwrap();
                assert_eq!(loaded_snap["session_id"], json!(sid));
                assert!(orch.session(sid).is_ok());

                // Resume the stale unloaded close; it returns {} idempotently WITHOUT calling `ext.closed`.
                release.notify_waiters();
                let close_res = close_task.await.unwrap().expect("unloaded close succeeds idempotently");
                assert_eq!(close_res, json!({}));
                assert!(
                    ext.closed_ids.borrow().is_empty(),
                    "idempotent unloaded close must not call ext.closed on concurrently loaded session: {:?}",
                    ext.closed_ids.borrow()
                );
                assert!(orch.session(sid).is_ok(), "concurrently loaded runtime must remain active");

                anchor.shutdown().await;
                let _ = orch.shutdown_all().await;
                let _ = std::fs::remove_file(mare_store::lock_path(&path));
                let _ = std::fs::remove_dir_all(&dir);
            })
            .await;
    }

    #[tokio::test]
    async fn load_and_fork_reject_missing_header_cwd_directory() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
                let dir_a = std::env::temp_dir().join(format!("mare-orch-missing-cwd-a-{ts}"));
                let dir_b = std::env::temp_dir().join(format!("mare-orch-missing-cwd-b-{ts}"));
                std::fs::create_dir_all(&dir_a).unwrap();
                std::fs::create_dir_all(&dir_b).unwrap();

                let sid = "ffff6666-0000-0000-0000-000000000006";
                let path = dir_a.join("missing-cwd.jsonl");
                std::fs::write(
                    &path,
                    format!(
                        "{}\n{}\n",
                        json!({ "type": "session", "version": 3, "session_id": sid, "cwd": dir_b }),
                        json!({ "type": "message", "entry_id": "m1", "parent_entry_id": null, "message": { "role": "user", "content": "hi" } }),
                    ),
                )
                .unwrap();
                // Delete project B's directory while the session journal remains on disk.
                std::fs::remove_dir_all(&dir_b).unwrap();

                let orch = SessionOrchestrator::new_multi();
                let ext = Rc::new(StubLifecycleExt::new(dir_a.clone()));
                ext.selectors.borrow_mut().insert(sid.to_owned(), path.clone());
                orch.set_ext(ext.clone());

                let anchor = Host::new(test_opts(dir_a.clone())).await.unwrap();
                let owner = OrchestratorCaller::host();

                let load_err = orch
                    .load(&owner, &anchor, &json!({ "session_id": sid }))
                    .await
                    .expect_err("session.load with deleted header cwd must fail instead of falling back to anchor cwd");
                assert_eq!(load_err.code, "invalid_arguments", "{load_err:?}");
                assert!(load_err.message.contains("no longer exists"), "{load_err:?}");
                assert!(orch.sessions.borrow().is_empty(), "failed load must not spawn a fallback Host");

                let fork_err = orch
                    .fork(&owner, &anchor, &json!({ "session_id": sid }))
                    .await
                    .expect_err("session.fork with deleted header cwd must fail instead of falling back to anchor cwd");
                assert_eq!(fork_err.code, "invalid_arguments", "{fork_err:?}");
                assert!(fork_err.message.contains("no longer exists"), "{fork_err:?}");
                assert!(orch.sessions.borrow().is_empty(), "failed fork must not spawn a fallback Host");

                anchor.shutdown().await;
                let _ = orch.shutdown_all().await;
                let _ = std::fs::remove_file(mare_store::lock_path(&path));
                let _ = std::fs::remove_dir_all(&dir_a);
            })
            .await;
    }
}
