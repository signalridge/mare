//! Host: one Lua VM + kernel + primitives + loaded plugins. Frontends (TUI, print, RPC, ACP,
//! child agents) drive a Host through these async methods.

use std::path::PathBuf;
use std::sync::Arc;

use mlua::Table;

use crate::plugins::plugin_failures::PluginFailure;
use crate::ui_wait::UiWaits;
use mare_kernel::Kernel;
use mare_plugin::Mode;
use mare_shared::ui::UiShared;

pub mod agent;
pub mod agent_registry;
mod agent_tree;
mod approvals;
pub mod artifacts;
mod child_params;
mod child_transcript;
pub mod config;
mod frontend;
pub mod manager;
mod plugins;
pub mod reload;
pub mod session_orchestrator;
pub mod session_state;
mod startup;
pub mod ui_shell;
mod ui_wait;
pub mod web_registry;
pub mod workspace;

pub use frontend::PolicyImageError;
pub use manager::{
    CloseRetryOutcome, HostManager, ManagerShutdownReport, SessionError, SessionRuntime, SessionShutdownEntry,
};
pub use session_orchestrator::{
    CallerIdentity, Capacity, OrchestratorCaller, OrchestratorError, OrchestratorExt, SessionOrchestrator,
    SingleCloseMatch,
};
pub use session_state::{
    AnswererSlot, ConnId, DEFAULT_MAX_INBOUND_HELD, InboundHeld, InboundHeldGuard, PendingPrompt, PendingPromptOutcome,
    PromptRequestCache, SessionPromptReceipts, alloc_conn_id,
};
pub use startup::config_error::ConfigError;
pub use startup::flags::discover_cli_flags;
pub use startup::session_identity::{RuntimeSet, SessionCallGuard};
pub use startup::trust::TrustChoice;
pub use ui_shell::{UiAttachment, UiShellState};
pub use web_registry::{AssetResolve, WebListenerInfo, WebRegistry, WebRouteRequest, WebRouteResponse};
pub use workspace::{WorkspaceId, WorkspaceRegistry, WorkspaceSnapshot, WorkspaceState, resolve_workspace_trust};

/// Snapshot entry for one loaded session across a multi-host coordinator (`HostManager`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedSessionEntry {
    pub session_id: String,
    pub session_epoch: Option<String>,
}

/// What the interactive frontend gets when startup is about to install packages
/// ([`Host::with_ui_first_frame`]): answer `shown` once its input frame is on screen; `progress`
/// then carries the installation's progress lines for the activity area and closes
/// when installation is over.
pub struct InstallStart {
    pub shown: tokio::sync::oneshot::Sender<()>,
    pub progress: tokio::sync::mpsc::UnboundedReceiver<String>,
}

#[derive(Clone)]
pub struct HostOptions {
    pub cwd: PathBuf,
    pub mode: Mode,
    /// Base/override config. With `load_init`, init.lua runs first and this is merged on top
    /// (CLI flags win); otherwise it is the whole config (child agents, tests).
    pub config: serde_json::Value,
    /// Run `~/.config/mare/init.lua` (+ the trusted project's `.mare/init.lua`) in this VM.
    pub load_init: bool,
    pub project_trusted: bool,
    pub args: serde_json::Value,
    pub no_user_plugins: bool,
}

pub struct Host {
    pub kernel: Kernel,
    pub ui: Arc<UiShared>,
    pub mode: Mode,
    pub cwd: PathBuf,
    /// Workspace identity, trust state, and effective config snapshot.
    pub workspace: WorkspaceState,
    /// Exclusive UI / approval / elicitation answerer lease for this host.
    pub answerer: AnswererSlot,
    /// Per-host bounded inbound peer confirmation hold quota.
    pub inbound_held: InboundHeld,
    /// Per-host prompt idempotency receipt cache & in-flight map.
    pub prompt_receipts: SessionPromptReceipts,
    /// Per-host UI shell cache, attachments, takeover registry, and editor buffer.
    pub shell: UiShellState,
    /// Monotonic host instance id used for scoping host-level web registrations.
    pub instance_id: u64,
    /// Host-level HTTP/WebSocket route and asset registry.
    pub web: std::rc::Rc<WebRegistry>,
    /// Shared session identity, RuntimeSet attachment, switch handle, and bound-call state.
    pub(crate) identity: std::rc::Rc<startup::session_identity::IdentityState>,
    /// what built the config, for `/reload config`
    reload: ReloadInfo,
    /// Only one config/full reload may mutate the live Lua state at a time.
    reload_gate: Arc<tokio::sync::Mutex<()>>,
    reload_active: std::cell::Cell<bool>,
    /// The failures as they were when the running full reload started ([`Host::failures`]).
    reload_failures: std::cell::RefCell<Option<Vec<PluginFailure>>>,
    /// Plugins of a config-driven reload that is running or was cancelled (mixed state until
    /// retried); a reload that finished takes its plugins off, failed or not.
    reload_pending: std::cell::RefCell<std::collections::BTreeSet<String>>,
    /// Plugins whose last reload finished with a failure: the next config reload retries them
    /// even with unchanged options. Doesn't block requests by itself.
    reload_retry: std::cell::RefCell<std::collections::BTreeSet<String>>,
    reload_epoch: std::cell::Cell<u64>,
    /// A full reload can also fail before/after plugin activation (packages and hooks).
    reload_full_incomplete: std::cell::Cell<bool>,
    /// A slot sync (`plugins.preset` / `enabled` changed) started and has not finished: the next
    /// `/reload config` runs it again even with an unchanged config.
    slot_sync_pending: std::cell::Cell<bool>,
    /// Session-only trust was active when `session/changed` fired and awaits `reload_all()` once
    /// the session switch completes (`session/switch-completed`).
    pending_session_trust_revoke: std::cell::Cell<bool>,
    /// Revoking session-only trust modified or replaced the `session` plugin; only a full `/reload` can recover cleanly.
    pending_session_trust_full_reload: std::cell::Cell<bool>,
    /// What startup couldn't provide besides plugins (unavailable packages, package hooks,
    /// ): requests are refused until a full /reload completes (whose own package
    /// failures then block as `reload_full_incomplete`).
    startup_blocked: std::cell::RefCell<Vec<String>>,
    /// Config files and plugin definitions that failed (startup, then the last full /reload).
    /// Together with the defined plugins that aren't active ([`Self::failures`]) they decide what
    /// is blocked, and `mare.plugins.failures()` hands them to Lua (the llm plugin refuses
    /// requests for a model whose provider plugin is among them).
    plugin_failures: std::cell::RefCell<Vec<PluginFailure>>,
    /// a full /reload has completed (the block reason then no longer says "startup")
    reloaded: std::cell::Cell<bool>,
    /// Problems from startup, first trust, or package reload: a successful full /reload drops
    /// them (and the counted handler errors), while retaining failures that still exist.
    startup_problems: std::cell::RefCell<Vec<String>>,
    /// cancelled on shutdown: package operations (git) started by the host stop with it
    cancel: tokio_util::sync::CancellationToken,
    /// cancelled when the active session is switched, closed, or the host shuts down
    session_cancel: std::rc::Rc<std::cell::RefCell<tokio_util::sync::CancellationToken>>,
    /// open `mare.ui.select/input/confirm` waits, so a sub-agent's approval dialog never replaces
    /// a dialog that is already open (the TUI shows one modal at a time)
    waits: std::rc::Rc<UiWaits>,
    /// Session orchestrator attached to this host (`Single` by default, switched to `Multi` when
    /// registered in a multi-runtime [`HostManager`]).
    orchestrator: std::cell::RefCell<Option<std::rc::Rc<SessionOrchestrator>>>,
    /// Deferred startup GC guard held by TUI hosts until after the first frame is drawn.
    startup_gc: std::cell::RefCell<Option<startup::StartupGcGuard>>,
}

#[derive(Clone)]
struct ReloadInfo {
    load_init: bool,
    no_user_plugins: bool,
    cli: serde_json::Value,
    cli_flag_paths: Vec<String>,
    startup_project_trusted: bool,
    startup_cli_trust: Option<bool>,
}

impl Host {
    /// Synchronize the Lua `NetOriginSource` with the kernel's current session identity so
    /// subsequent `approver(lua)` captures freeze the origin.
    pub fn sync_net_origin(&self) {
        let (session_id, session_epoch) = self.kernel.event_log().session_identity();
        mare_binding::netauth::set_origin_source(self.lua(), session_id, session_epoch);
    }
    pub fn lua(&self) -> &mlua::Lua {
        self.kernel.lua()
    }

    /// Session orchestrator currently managing this host (`Single` by default, `Multi` once
    /// registered in a multi-session [`HostManager`]).
    pub fn orchestrator(&self) -> Option<std::rc::Rc<SessionOrchestrator>> {
        self.orchestrator.borrow().clone()
    }

    /// Attach or replace the [`SessionOrchestrator`] managing this host.
    pub fn set_orchestrator(&self, orchestrator: std::rc::Rc<SessionOrchestrator>) {
        *self.orchestrator.borrow_mut() = Some(orchestrator);
    }

    /// Attach a default [`Capacity::Single`] orchestrator holding only `Weak<Host>`.
    pub(crate) fn attach_single_orchestrator(self: &Arc<Self>) {
        self.set_orchestrator(SessionOrchestrator::new_single(self));
    }

    /// Working directory of this host.
    pub fn cwd(&self) -> &std::path::Path {
        &self.cwd
    }

    /// Canonical project resource root for this host's workspace.
    pub fn workspace_root(&self) -> PathBuf {
        let rr = mare_shared::project::resource_root(&self.cwd);
        let subject = mare_shared::project::trust_subject(&rr);
        dunce::canonicalize(subject).unwrap_or_else(|_| subject.to_path_buf())
    }

    /// Whether project-level resources in this workspace are trusted for this host.
    pub fn is_project_trusted(&self) -> bool {
        self.workspace.is_trusted()
    }

    /// Directories in this host's workspace (`mare.workspace.dirs()`).
    pub fn workspace_dirs(&self) -> mlua::Result<Vec<String>> {
        let mare: Table = self.lua().globals().get("mare")?;
        let ws: Table = mare.get(mare_shared::WORKSPACE_SERVICE_NAME)?;
        let dirs: mlua::Function = ws.get("dirs")?;
        dirs.call(())
    }

    /// Current effective host config JSON.
    pub fn config(&self) -> serde_json::Value {
        self.kernel.config()
    }

    /// Original startup/CLI config JSON prior to evaluating `.mare/init.lua`.
    pub fn startup_config(&self) -> serde_json::Value {
        self.reload.cli.clone()
    }

    /// Whether this host started with project trust enabled (`--trust` or persisted decision).
    pub fn startup_project_trusted(&self) -> bool {
        self.reload.startup_project_trusted
    }

    /// Explicit CLI trust override (`--trust` -> `Some(true)`, `--no-trust` -> `Some(false)`),
    /// or `None` when trust came from saved decisions / default / undecided.
    pub fn startup_cli_trust(&self) -> Option<bool> {
        self.reload.startup_cli_trust
    }

    /// Whether session-only trust revocation is pending or failed.
    pub fn is_pending_session_trust_revoke(&self) -> bool {
        self.pending_session_trust_revoke.get()
    }

    /// Whether the host started with `--no-user-plugins` / `--clean`.
    pub fn no_user_plugins(&self) -> bool {
        self.reload.no_user_plugins
    }

    /// Shared extension CLI flag store.
    pub fn flags(&self) -> mare_shared::flags::FlagStore {
        mare_shared::flags::FlagStore::of(self.lua())
    }

    pub fn service_table(&self, name: &str) -> Option<Table> {
        ui_wait::service_table_for_kernel(&self.kernel, name)
    }

    /// The `session:info()` table of the currently loaded session, if any.
    pub fn session_info(&self) -> mlua::Result<Option<Table>> {
        startup::session_info_for_kernel(&self.kernel)
    }

    /// Asynchronously emit a `ui/print` observation event.
    pub fn spawn_ui_print(&self, text: &str) {
        self.spawn_ui_print_node(text, None);
    }

    /// Asynchronously emit a `ui/print` observation event with an optional wire `Node`.
    pub fn spawn_ui_print_node(&self, text: &str, node: Option<serde_json::Value>) {
        let payload = ui_wait::ui_print_payload_with_node(text, node);
        let kernel = self.kernel.clone();
        tokio::task::spawn_local(async move {
            kernel.emit_json("ui/print", payload).await;
        });
    }

    /// Synchronize declarative `web = { dir }` assets from active plugins and packages.
    pub fn sync_web_assets(&self) {
        self.web.sync_declarative_assets(
            self.instance_id,
            self.identity.web_session_id.clone(),
            &self.kernel,
            &self.kernel.config(),
            !self.reload.no_user_plugins,
        );
    }

    /// Bind an in-flight call to this host's current session; fails if a session switch is already
    /// in progress, and while any guard is held `kernel.begin_session_switch` refuses new switches.
    pub fn bind_session_call(&self) -> anyhow::Result<SessionCallGuard> {
        self.identity.bind_session_call()
    }

    /// Number of currently active bound session calls holding this host's session.
    pub fn bound_session_calls(&self) -> usize {
        self.identity.bound_session_calls()
    }

    /// Whether a `kernel.begin_session_switch` transaction is currently open on this host.
    pub fn is_session_switching(&self) -> bool {
        self.identity.has_active_switch()
    }

    /// Whether a session switch is in progress either at the host transaction gate or in the
    /// active session provider (`session:switching()`).
    pub fn is_session_switching_or_provider(&self, poisoned: bool) -> bool {
        if self.is_session_switching() {
            return true;
        }
        if poisoned {
            return false;
        }
        let Some(session) = self.service_table(crate::frontend::SESSION_SERVICE) else {
            return false;
        };
        let Ok(switching) = session.get::<mlua::Function>("switching") else {
            return false;
        };
        switching.call::<bool>(session).unwrap_or(false)
    }

    /// Whether the active agent provider declares and implements the `cancel` contract capability.
    pub fn supports_turn_cancellation(&self) -> bool {
        self.kernel.service_has_capability(crate::frontend::AGENT_SERVICE, "cancel")
    }

    /// Attach this host to a multi-session [`RuntimeSet`] (e.g. [`HostManager`]).
    pub(crate) fn attach_runtime_set(&self, set: std::rc::Weak<dyn RuntimeSet>) {
        self.identity.attach_runtime_set(set);
    }

    /// Update this host's session identity across all sinks via the single writer in
    /// [`startup::session_identity::write_session_identity`].
    pub(crate) fn apply_session_identity(
        self: &Arc<Self>,
        new_id: Option<String>,
        bump_epoch: bool,
    ) -> Result<String, SessionError> {
        startup::session_identity::write_session_identity(&self.kernel, &self.identity, Some(self), new_id, bump_epoch)
    }

    /// Root host cancellation token, cancelled when the host shuts down.
    pub fn cancel_token(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancel
    }

    /// Cancellation token for the currently active session (cancelled on switch, close, or shutdown).
    pub fn session_cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.session_cancel.borrow().clone()
    }

    /// Cancel the current session token and install a fresh child token for the next session.
    pub fn rotate_session_cancel(&self) {
        let next = self.cancel.child_token();
        let prev = std::mem::replace(&mut *self.session_cancel.borrow_mut(), next);
        prev.cancel();
    }

    /// Cooperative shutdown grace period (`shutdown_grace_ms`, default 4000 ms).
    pub fn shutdown_grace(&self) -> std::time::Duration {
        let ms = self.config().get("shutdown_grace_ms").and_then(|v| v.as_u64()).unwrap_or(4000);
        std::time::Duration::from_millis(ms)
    }

    pub fn load_init(&self) -> bool {
        self.reload.load_init
    }

    pub fn is_reload_full_incomplete(&self) -> bool {
        self.reload_full_incomplete.get()
    }
}

/// Outcome of [`Host::shutdown_report`]: any cooperative tasks or process trees whose stop
/// could not be confirmed are reported rather than claimed cleaned up.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    pub unconfirmed_tasks: Vec<&'static str>,
    pub unconfirmed_trees: Vec<u32>,
}

impl ShutdownReport {
    pub fn stop_unconfirmed(&self) -> bool {
        !self.unconfirmed_tasks.is_empty() || !self.unconfirmed_trees.is_empty()
    }
}
