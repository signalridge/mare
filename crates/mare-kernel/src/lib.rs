//! Plugin kernel (Cordis-style).
//!
//! * every plugin gets its own `ctx`; everything registered through it is reclaimed on dispose
//! * services: `mare.Service` (a name a definition declares is provided through it only; an
//!   undeclared one may be a raw `ctx:provide`) / `inject = {...}`; one provider per name;
//! * events: `emit` (notify all), `bail` (first non-nil wins), `waterfall` (sequential rewrite)
//! * static dependency resolution at startup (topological sort, cycles are an error)
//! * `/reload [plugin]` disposes and re-applies (plus dependents)

use std::collections::HashMap;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, Weak};

use mlua::{Function, Lua, LuaSerdeExt, Value};

use crate::lua_env::modules::Modules;

mod bytecode;
mod dispatch;
mod lua_env;
mod plugins;
mod problems;
mod resources;
mod source;

pub use bytecode::{load_cached_chunk, loader_prune, loader_prune_if_due, loader_prune_if_due_at, loader_reset};
pub use dispatch::baseline::{BASELINE_IDLE_TIMEOUT, BASELINE_PAGE_MAX_BYTES, BaselineAccess};
pub use dispatch::bus::{HostOn, HostVoter};
pub use dispatch::contracts;
pub use dispatch::events::recent_events;
pub use dispatch::policy::PolicyOptions;
pub use dispatch::remote::RemoteChannel;
pub use dispatch::runtime_fields::{ProjectedEventArgs, answer_to_lua, project_event_args, restore_event_result};
pub use dispatch::service_rpc::{
    CallbackChannel, CallerAccess, CallerAuthorizer, CallerPathAuthorizer, ErrorValue, ProviderReloadGuard,
    SERVICE_SCHEMA_MAX_BYTES, ServiceCallError, allow_provider_reload, current_export_caller, internal_error,
    is_direct_host_export_call, mark_host_service_call_committed, mark_service_call_committed, release_disposer,
};
pub use dispatch::tap::{
    CursorRecord, EVENT_MAX_BYTES, EventCursor, EventLog, EventLogLimits, HeadlessCursor, HeadlessEvent,
    HeadlessFilter, HeadlessRecord, HeadlessSelection, HeadlessTapHandle, ReliableFilter, SESSION_LOG_CAPACITY,
    SESSION_LOG_MAX_BYTES, SubscribeCursorOutcome, TAP_BLOCK_LIMIT, TapEvent, strip_event_envelope,
};
pub use dispatch::timing::{SLOW_CALLBACK, timed};
pub use lua_env::cordis_host::test_ctx;
pub use lua_env::modules::set_builtin_modules;
pub use lua_env::public_modules::{set_builtin_lua_dir, set_public_modules};
pub use lua_env::watchdog::install_watchdog;
pub use plugins::overrides::OverrideRefusal;
pub use plugins::prims::{
    DEFAULT_POLICY_TIMEOUT, active_caller_thread, active_owner_id, as_host, attach_owner_disposer, current_owner_info,
};
pub use plugins::reload::KeymapReloadGuard;
pub use plugins::reload::ReloadHold;
pub use plugins::reload::ReloadReport;
pub use plugins::stalls::StalledPlugin;
pub use plugins::tree::PluginTreeRow;
pub use source::{Declared, Origin, PluginDef, PluginSource, ServiceContract, WebPluginDef};

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum PolicyFold {
    #[default]
    Restrictive,
    Approval,
    Affirmative,
}
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct PolicyMetadata {
    pub fold: PolicyFold,
    pub permissions: bool,
    pub sandbox_grant: bool,
    pub join_ask_reasons: bool,
    /// Service name declared by the policy point contract whose active provider's explicit
    /// `"allow"` vote sets `verdict.authority_approved`.
    pub authority_service: Option<String>,
}

struct ServiceEntry {
    ctx: u64,
    /// Unique per `provide` call, so withdrawal removes only that registration.
    seq: u64,
    value: Value,
    contract: Arc<ServiceContract>,
    lifetime: tokio_util::sync::CancellationToken,
    exports: Option<std::rc::Rc<dispatch::service_rpc::ServiceExports>>,
    /// backend runs outside local confinement (a remote provider)
    unconfined: bool,
    /// contract methods the provider implements, read once at registration with the same
    /// `Table::get` the validation uses (so `__index` providers count), never under the lock
    implemented: std::collections::BTreeSet<String>,
}

/// A held disposer; `None` once released.
type Registration = std::rc::Rc<tokio::sync::Mutex<Option<Function>>>;

struct CtxState {
    plugin: String,
    origin: String,
    disposed: bool,
    cancel: tokio_util::sync::CancellationToken,
    /// disposers kept for JSON callers of registering service methods, by registration id; the
    /// lock serializes releases of one registration (release, retry, ctx disposal)
    registrations: std::collections::BTreeMap<String, Registration>,
    /// Synchronous disposers registered by `attach_owner_disposer` (e.g. direct plugin `mare.keymap.set`).
    owner_disposers: std::collections::BTreeMap<u64, Function>,
    next_disposer_id: u64,
}

#[derive(Default)]
struct State {
    next_id: u64,
    /// service name -> the contract its exports were last derived for and that derivation: a
    /// provider of the same contract (every reload re-provides every service) shares it instead
    /// of compiling its schemas and adapters again (the guards are stateless)
    derived: HashMap<String, (Arc<ServiceContract>, std::rc::Rc<dispatch::service_rpc::ServiceExports>)>,
    defs: Vec<PluginDef>,
    /// `/reload config`: the definitions [`Kernel::prepare_changed_defs`] replaced, until the
    /// rows are applied — one whose new definition fails runs on its previous one again
    /// (`restored`), and its definition goes back with it
    rows_prior: HashMap<String, PluginDef>,
    /// what every definition read for the current config declares, the ones left out
    /// (`enabled = false`, `plugins.preset`) included ([`Kernel::set_declared`])
    declared: Vec<Declared>,
    /// plugin name -> the `cfg` table its fiber received (the loader's mark_active): a runtime
    /// write of an option declared `apply = "live"`/`"next_op"` updates it in place
    cfgs: HashMap<String, mlua::Table>,
    services: HashMap<String, ServiceEntry>,
    /// Built-in service contracts loaded independently of provider implementations.
    contracts: Arc<std::collections::BTreeMap<String, Arc<ServiceContract>>>,
    /// plugin name -> the services it called `ctx:provide` for since its latest apply, declared
    /// or not (refused): kept after a failure, so the host can tell a failed replacement of a
    /// core service
    provided: HashMap<String, Vec<String>>,
    ctxs: HashMap<u64, CtxState>,
    policy_metadata: HashMap<String, PolicyMetadata>,
    policy_points: std::collections::BTreeSet<String>,
    rewrite_points: std::collections::BTreeSet<String>,
    /// failures / skips for diagnostics
    problems: Vec<String>,
    /// (plugin, what failed) with a problem line: every handler error is notified (
    /// part five); the line keeps the latest one
    handler_errors: std::collections::HashSet<(String, String)>,
    /// JSON callers' registrations whose disposer failed when their owner was disposed:
    /// retried on the next disposal and at shutdown until released
    pending_releases: Vec<(String, Registration)>,
    /// how a JSON caller's ctx answers contract callbacks (`callback.call`), set by its frontend
    callback_channels: HashMap<u64, std::rc::Rc<dyn crate::dispatch::service_rpc::CallbackChannel>>,
    /// connection-level access rules registered for a JSON caller's ctx, set by its frontend
    caller_access: HashMap<u64, crate::dispatch::service_rpc::CallerAccess>,
    /// the lifetime of each registration whose callbacks cross the wire: cancelled on release
    callback_lifetimes: HashMap<String, tokio_util::sync::CancellationToken>,
}

pub struct KernelInner {
    id: u64,
    /// Image-resolver namespace scope (`0` for primary host, unique non-zero for isolated test hosts).
    resource_scope: u64,
    lua: Lua,
    /// plugin implementation modules: `<plugin>/lua/` dirs
    /// searched by `require`, and which plugin owns each loaded module (cleared on dispose)
    modules: Mutex<Modules>,
    state: Mutex<State>,
    config: Arc<Mutex<serde_json::Value>>,
    event_log: EventLog,
    baselines: Mutex<dispatch::baseline::BaselineRegistry>,
    /// number of reloads in progress: tool calls wait until it drops to 0
    reloading: tokio::sync::watch::Sender<u32>,
    reload_gate: Arc<tokio::sync::Mutex<()>>,
    reload_generation: AtomicU64,
    policy_epoch: AtomicU64,
    measure_apply: std::sync::atomic::AtomicBool,
    reload_incomplete: Mutex<std::collections::BTreeSet<String>>,
    disposals: Mutex<HashMap<u64, tokio::sync::watch::Receiver<Option<Vec<tokio_util::sync::CancellationToken>>>>>,
}

#[derive(Clone)]
pub struct Kernel(pub Arc<KernelInner>);

impl Kernel {
    pub fn new(lua: Lua, config: serde_json::Value) -> mlua::Result<Self> {
        static NEXT_KERNEL_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_KERNEL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let log = EventLog::new();
        log.set_wire_projector_slot(mare_shared::wire_projection::hook_slot(&lua));
        // !Send by design (actor model: one thread owns the VM); Arc for Weak back-references.
        #[allow(clippy::arc_with_non_send_sync)]
        let k = Self(Arc::new(KernelInner {
            id,
            resource_scope: 0,
            lua,
            modules: Mutex::new(Modules::default()),
            state: Mutex::new(State { next_id: 1, ..Default::default() }),
            config: Arc::new(Mutex::new(config.clone())),
            event_log: log,
            baselines: Mutex::new(Default::default()),
            reloading: tokio::sync::watch::channel(0).0,
            reload_gate: Arc::new(tokio::sync::Mutex::new(())),
            reload_generation: AtomicU64::new(0),
            policy_epoch: AtomicU64::new(0),
            measure_apply: std::sync::atomic::AtomicBool::new(mare_shared::profile::startup_profile_enabled()),
            reload_incomplete: Mutex::new(std::collections::BTreeSet::new()),
            disposals: Mutex::new(HashMap::new()),
        }));
        mare_shared::exec_ctx::init(k.lua());
        k.lua().set_app_data(k.weak());
        k.install_searcher()?;
        k.install_globals()?;
        Ok(k)
    }

    /// Create an isolated child [`Kernel`] sharing the same Lua VM and cloned built-in contracts,
    /// without touching VM-global `app_data`, `require`, or `mare.*` globals. Used by `mare.test.host()`.
    pub fn new_isolated_from(&self) -> mlua::Result<Self> {
        static NEXT_KERNEL_ID: AtomicU64 = AtomicU64::new(1 << 32);
        let id = NEXT_KERNEL_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let lua = self.lua().clone();
        let log = EventLog::new();
        log.set_wire_projector_slot(mare_shared::wire_projection::hook_slot(&lua));
        let contracts = self.st().contracts.clone();
        let mut state = State { next_id: 1, contracts, ..Default::default() };
        Self::rebuild_event_catalog_locked(&mut state);
        #[allow(clippy::arc_with_non_send_sync)]
        let k = Self(Arc::new(KernelInner {
            id,
            resource_scope: id,
            lua,
            modules: Mutex::new(Modules::default()),
            state: Mutex::new(state),
            config: Arc::new(Mutex::new(serde_json::json!({}))),
            event_log: log,
            baselines: Mutex::new(Default::default()),
            reloading: tokio::sync::watch::channel(0).0,
            reload_gate: Arc::new(tokio::sync::Mutex::new(())),
            reload_generation: AtomicU64::new(0),
            policy_epoch: AtomicU64::new(0),
            measure_apply: std::sync::atomic::AtomicBool::new(false),
            reload_incomplete: Mutex::new(std::collections::BTreeSet::new()),
            disposals: Mutex::new(HashMap::new()),
        }));
        Ok(k)
    }

    /// Process-unique identifier for this [`Kernel`] instance.
    pub fn id(&self) -> u64 {
        self.0.id
    }

    pub(crate) fn resource_scope(&self) -> u64 {
        self.0.resource_scope
    }

    pub fn lua(&self) -> &Lua {
        &self.0.lua
    }

    fn st(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn weak(&self) -> Weak<KernelInner> {
        Arc::downgrade(&self.0)
    }

    pub fn from_weak(w: &Weak<KernelInner>) -> Option<Self> {
        w.upgrade().map(Kernel)
    }

    pub fn set_config(&self, t: serde_json::Value) {
        *self.0.config.lock().unwrap() = t.clone();
        self.release_unauthorized_baselines(&t);
        self.0.event_log.clear_all_cursor_decisions();
        self.taps_changed();
    }

    pub fn config(&self) -> serde_json::Value {
        self.0.config.lock().unwrap().clone()
    }

    pub fn shared_config(&self) -> Arc<Mutex<serde_json::Value>> {
        self.0.config.clone()
    }

    pub(crate) fn measure_apply_enabled(&self) -> bool {
        self.0.measure_apply.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn set_measure_apply(&self, enabled: bool) {
        self.0.measure_apply.store(enabled, std::sync::atomic::Ordering::Relaxed);
    }
}

pub fn lua_serde_check(lua: &Lua) -> mlua::Result<()> {
    let _ = lua.to_value(&serde_json::json!({}))?;
    Ok(())
}

/// `mare-kernel`-owned persistence artifacts.
pub const ARTIFACTS: &[mare_shared::artifact::Artifact] = &[mare_shared::artifact::Artifact {
    id: mare_shared::artifact::ArtifactId::LuacCache,
    root: mare_shared::artifact::Root::Cache,
    pattern: "luac/*.luac",
    sidecars: &["luac/.pruned"],
    owner: "mare-kernel::bytecode",
    format: mare_shared::artifact::Format::Binary,
    version: None,
    write: mare_shared::artifact::WriteKind::Atomic,
    lock: mare_shared::artifact::LockKind::None,
    corrupt: mare_shared::artifact::CorruptPolicy::Discard,
    mode: mare_shared::paths::WriteMode::Private,
    bound: mare_shared::artifact::Bound::WithParent,
    content: mare_shared::artifact::ContentClass::Plain,
}];

#[cfg(test)]
mod kernel_lib_tests {
    use super::*;

    #[test]
    fn lua_serde_check_succeeds_on_kernel_lua() {
        let kernel = Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
        lua_serde_check(kernel.lua()).unwrap();
        assert_ne!(kernel.id(), 0);
    }
}
