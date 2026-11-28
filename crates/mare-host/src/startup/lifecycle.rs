//! Building a host in startup order, the reload policy it installs, and shutdown.

use std::sync::Arc;

use anyhow::{Context, Result};
use mlua::{MultiValue, Table};

use crate::config::eval::evaluate_config;
use crate::plugins::defs::wanted_defs;
use crate::plugins::packages::{install_packages, package_hooks};
use crate::plugins::plugin_failures::PluginFailure;
use crate::plugins::{introspect, plugin_failures};
use crate::startup::request_gate;
use crate::ui_wait::UiWaits;
use crate::{Host, HostOptions, InstallStart, ReloadInfo, agent};
use mare_kernel::Kernel;
use mare_plugin::{self as lua, Env, Mode};
use mare_shared::ui::UiShared;

/// Lua 5.5's default `minormajor` generational GC threshold (`LUAI_MINORMAJOR` = 68%).
const DEFAULT_GC_MINOR_TO_MAJOR: i32 = 68;
/// Deferred `minormajor` threshold while host startup constructs the live plugin graph (~0.7 -> ~12 MB),
/// avoiding repeated premature minor->major shifts while keeping minor collections and `lua_memory_mb`
/// emergency GC active. Restored on drop before non-TUI hosts return, or after the TUI's first
/// frame via [`Host::restore_startup_gc`].
const STARTUP_GC_MINOR_TO_MAJOR: i32 = 1000;

pub(crate) struct StartupGcGuard(mlua::Lua);

impl StartupGcGuard {
    fn enter(lua: &mlua::Lua) -> Self {
        lua.gc_set_mode(mlua::state::GcMode::Generational(
            mlua::state::GcGenParams::default()
                .minor_multiplier(mare_plugin::GC_MINOR_MULTIPLIER)
                .minor_to_major(STARTUP_GC_MINOR_TO_MAJOR),
        ));
        Self(lua.clone())
    }
}

impl Drop for StartupGcGuard {
    fn drop(&mut self) {
        self.0.gc_set_mode(mlua::state::GcMode::Generational(
            mlua::state::GcGenParams::default()
                .minor_multiplier(mare_plugin::GC_MINOR_MULTIPLIER)
                .minor_to_major(DEFAULT_GC_MINOR_TO_MAJOR),
        ));
    }
}

struct StatusBatchGuard(Option<mlua::Lua>);

impl StatusBatchGuard {
    fn enter(lua: &mlua::Lua) -> Self {
        mare_shared::lua::core::ui::begin_status_batch(lua);
        Self(Some(lua.clone()))
    }

    fn finish(mut self) {
        if let Some(lua) = self.0.take() {
            mare_shared::lua::core::ui::end_status_batch(&lua);
        }
    }
}

impl Drop for StatusBatchGuard {
    fn drop(&mut self) {
        if let Some(lua) = self.0.take() {
            mare_shared::lua::core::ui::end_status_batch(&lua);
        }
    }
}

impl Host {
    pub async fn new(opts: HostOptions) -> Result<Arc<Self>> {
        let ui = UiShared::new();
        Self::with_ui(opts, ui).await
    }

    /// Build a [`Host`] with a pre-bound [`crate::SessionOrchestrator`] attached before
    /// `app/ready` fires (used by multi-runtime frontends such as ACP).
    pub async fn new_with_orchestrator(
        opts: HostOptions,
        orchestrator: std::rc::Rc<crate::SessionOrchestrator>,
    ) -> Result<Arc<Self>> {
        Self::with_ui_inherited_ready(
            opts,
            UiShared::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(orchestrator),
            true,
        )
        .await
    }

    pub async fn new_with_web(opts: HostOptions, web: std::rc::Rc<crate::WebRegistry>) -> Result<Arc<Self>> {
        Self::with_ui_inherited_ready(
            opts,
            UiShared::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(web),
            None,
            true,
        )
        .await
    }

    /// Build a [`Host`] sharing `web` with a pre-bound [`crate::SessionOrchestrator`] attached
    /// before `app/ready` fires (used for RPC fallback anchor hosts).
    pub async fn new_with_web_and_orchestrator(
        opts: HostOptions,
        web: std::rc::Rc<crate::WebRegistry>,
        orchestrator: std::rc::Rc<crate::SessionOrchestrator>,
    ) -> Result<Arc<Self>> {
        Self::with_ui_inherited_ready(
            opts,
            UiShared::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(web),
            Some(orchestrator),
            true,
        )
        .await
    }

    /// Build a [`Host`] like [`Self::new_with_web`] without emitting `app/ready` yet, so the caller
    /// can attach UI/capability bridges before plugins run startup/resume hooks.
    pub async fn new_with_web_deferred(opts: HostOptions, web: std::rc::Rc<crate::WebRegistry>) -> Result<Arc<Self>> {
        Self::new_with_web_deferred_and_orchestrator(opts, web, None).await
    }

    pub(crate) async fn new_with_web_deferred_and_orchestrator(
        opts: HostOptions,
        web: std::rc::Rc<crate::WebRegistry>,
        orchestrator: Option<std::rc::Rc<crate::SessionOrchestrator>>,
    ) -> Result<Arc<Self>> {
        Self::with_ui_inherited_ready(
            opts,
            UiShared::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(web),
            orchestrator,
            false,
        )
        .await
    }

    pub(crate) async fn new_with_fork_images_and_web(
        opts: HostOptions,
        fork_images: Option<std::collections::HashMap<String, String>>,
        web: std::rc::Rc<crate::WebRegistry>,
        orchestrator: Option<std::rc::Rc<crate::SessionOrchestrator>>,
    ) -> Result<Arc<Self>> {
        Self::with_ui_inherited_ready(
            opts,
            UiShared::new(),
            None,
            None,
            None,
            None,
            None,
            None,
            fork_images,
            Some(web),
            orchestrator,
            true,
        )
        .await
    }

    pub async fn with_ui(opts: HostOptions, ui: Arc<UiShared>) -> Result<Arc<Self>> {
        Self::with_ui_inherited(opts, ui, None, None, None, None, None, None, None, None).await
    }

    /// For the interactive frontend: signal when package installation would start, then wait for
    /// the frontend to show its initial input frame before beginning any installation network I/O.
    pub async fn with_ui_first_frame(
        opts: HostOptions,
        ui: Arc<UiShared>,
        before_install: tokio::sync::oneshot::Sender<InstallStart>,
    ) -> Result<Arc<Self>> {
        Self::with_ui_inherited(opts, ui, None, Some(before_install), None, None, None, None, None, None).await
    }

    pub(crate) async fn new_child(
        mut opts: HostOptions,
        ceiling: Option<mare_binding::ossandbox::Policy>,
        carried: mare_shared::carry::Carried,
        scope: crate::agent_tree::Scope,
        net_approver: Option<mare_binding::netauth::NetApprover>,
        snapshot_journal: Option<mare_binding::snapshot::SnapshotJournal>,
        child_session_id: String,
    ) -> Result<Arc<Self>> {
        let mut args = opts.args.as_object().cloned().unwrap_or_default();
        args.insert("session_id".into(), serde_json::Value::String(child_session_id));
        opts.args = serde_json::Value::Object(args);
        Self::with_ui_inherited(
            opts,
            UiShared::new(),
            Some(ceiling),
            None,
            Some(carried),
            Some(scope),
            net_approver,
            snapshot_journal,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn with_ui_inherited(
        opts: HostOptions,
        ui: Arc<UiShared>,
        ceiling: Option<Option<mare_binding::ossandbox::Policy>>,
        before_install: Option<tokio::sync::oneshot::Sender<InstallStart>>,
        carried: Option<mare_shared::carry::Carried>,
        scope: Option<crate::agent_tree::Scope>,
        net_approver: Option<mare_binding::netauth::NetApprover>,
        snapshot_journal: Option<mare_binding::snapshot::SnapshotJournal>,
        fork_images: Option<std::collections::HashMap<String, String>>,
        web: Option<std::rc::Rc<crate::WebRegistry>>,
    ) -> Result<Arc<Self>> {
        Self::with_ui_inherited_ready(
            opts,
            ui,
            ceiling,
            before_install,
            carried,
            scope,
            net_approver,
            snapshot_journal,
            fork_images,
            web,
            None,
            true,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn with_ui_inherited_ready(
        opts: HostOptions,
        ui: Arc<UiShared>,
        ceiling: Option<Option<mare_binding::ossandbox::Policy>>,
        before_install: Option<tokio::sync::oneshot::Sender<InstallStart>>,
        carried: Option<mare_shared::carry::Carried>,
        scope: Option<crate::agent_tree::Scope>,
        net_approver: Option<mare_binding::netauth::NetApprover>,
        snapshot_journal: Option<mare_binding::snapshot::SnapshotJournal>,
        fork_images: Option<std::collections::HashMap<String, String>>,
        web: Option<std::rc::Rc<crate::WebRegistry>>,
        orchestrator: Option<std::rc::Rc<crate::SessionOrchestrator>>,
        emit_ready: bool,
    ) -> Result<Arc<Self>> {
        static NEXT_HOST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let instance_id = NEXT_HOST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let web = web.unwrap_or_else(|| std::rc::Rc::new(crate::WebRegistry::new()));
        let web_session_id = std::rc::Rc::new(std::cell::RefCell::new(None));
        let identity = std::rc::Rc::new(super::session_identity::IdentityState::new(web_session_id.clone()));
        let lua = lua::new_lua()?;
        let mut startup_gc = Some(StartupGcGuard::enter(&lua));
        let mut lua_args = opts.args.clone();
        let cli_flag_paths: Vec<String> = lua_args
            .as_object_mut()
            .and_then(|obj| obj.remove("cli_flag_paths"))
            .and_then(|v| v.as_array().cloned())
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .filter(|s| !s.is_empty())
            .collect();
        let env =
            Env { cwd: opts.cwd.clone(), mode: opts.mode, ui: ui.clone(), config: opts.config.clone(), args: lua_args };
        // Share the parent session's snapshot journal before `lua::install` installs `mare.snapshot`
        // so file edits made by subagents (and nested grandchildren) are recorded under the parent turn.
        if let Some(journal) = snapshot_journal {
            lua.set_app_data(journal);
        }
        mare_shared::profile::mark("host: before lua install");
        let mare = lua::install(&lua, &env)?;
        // Overlap capability probes with host initialization while retaining their temp/process
        // ownership through the same blocking scope drained by shutdown.
        mare_binding::blocking::spawn(&lua, mare_binding::ossandbox::warm)?;
        if let Some(ceiling) = ceiling {
            mare_binding::ossandbox::set_inherited_ceiling(&lua, ceiling);
        }
        mare_shared::profile::mark("host: lua install");
        // a sub-agent's parameter rules arrive with their functions (child_params.rs): back in
        // `mare.config` before any plugin reads it
        if let Some(carried) = carried.filter(|c| !c.is_empty()) {
            mare_shared::lua::core::config::set_fresh_install_config(&lua, None)?;
            for p in carried.install(&lua)? {
                tracing::warn!("sub-agent: parameter mapping: {p}");
            }
            mare_shared::lua::restore_config_fns(&lua, mare.get::<mlua::Value>("config")?)?;
        }
        let host_scope = match scope {
            Some(scope) => scope,
            None => crate::agent_tree::Scope::root(&opts.config).map_err(anyhow::Error::msg)?,
        };
        if let Some(handle) = mare_binding::ossandbox::policy_handle(&lua) {
            host_scope.attach_policy(handle);
        }
        let scope_for_policy = host_scope.clone();
        mare_binding::ossandbox::set_policy_change_hook(
            &lua,
            std::sync::Arc::new(move |policy| {
                if let Some(policy) = policy {
                    scope_for_policy.tighten_descendants(policy);
                }
            }),
        );
        lua.set_app_data(host_scope);
        agent::install(&lua, &mare)?;
        mare.set("trusted", opts.project_trusted)?;
        // --no-user-plugins: built-in plugins only; packages' skills, prompts, themes and types
        // stay out too (`mare.plugins.dirs` returns nothing)
        mare.set("no_user_plugins", opts.no_user_plugins)?;
        // per-callback limit for Lua that never yields: `watchdog_ms` (default 10s)
        let watchdog_ms = opts
            .config
            .get("watchdog_ms")
            .and_then(|v| v.as_u64())
            .or_else(|| std::env::var("MARE_WATCHDOG_MS").ok().and_then(|v| v.parse().ok()))
            .unwrap_or(10_000);
        let _watchdog = mare_kernel::install_watchdog(&lua, std::time::Duration::from_millis(watchdog_ms))?;
        if let Some(mb) = opts
            .config
            .get("lua_memory_mb")
            .and_then(|v| v.as_u64())
            .or_else(|| std::env::var("MARE_LUA_MEMORY_MB").ok().and_then(|v| v.parse().ok()))
        {
            mare_plugin::set_memory_limit_mb(&lua, mb as usize)?;
        }
        // init.lua runs in the host VM (like Neovim) so closures — keymaps, component overrides,
        // renderers — are live; data calls (setup/plugin/packages/tool.command) fill mare.config.
        mare_shared::profile::mark("rust init + primitives");
        let t_init = std::time::Instant::now();
        // init.lua errors don't stop startup (WezTerm, Neovim): reported, defaults used
        let mut config_errors = vec![];
        // the decision points init.lua's `mare.policy.voter` may vote on, checked as it is called
        // (an error is the evaluation's, rolled back like any other) before the kernel exists.
        // Reuse the build-time pre-evaluated and process-cached built-in service contracts
        // (parsed through the same `parse_contracts_json_owned` validator as third-party contracts)
        // by shared `Arc` without cloning schema trees.
        let parsed_contracts = mare_plugin::builtin_service_contracts()?;
        let points = lua.create_table()?;
        for spec in parsed_contracts.values().flat_map(|contract| contract.events.values()) {
            if spec.mode == mare_protocol::EventMode::Policy {
                points.set(spec.name.as_str(), true)?;
            }
        }
        mare_shared::lua::core::config::set_policy_points(&lua, points)?;
        let config = if opts.load_init {
            evaluate_config(
                &lua,
                &mare,
                &opts.cwd,
                opts.project_trusted,
                &opts.config,
                &cli_flag_paths,
                Some(&mut config_errors),
            )
            .await?
        } else {
            // --clean / --clean still use the option registry for invocation overrides.
            crate::config::eval::apply_clean_cli(&lua, &mare, &opts.config, &cli_flag_paths)?
        };
        // Expose `mare.config_sources()` and `mare.config_warnings()` before plugins start so
        // plugins such as `permissions` and `approvals` can inspect whether an option came from
        // the `cli` layer during `apply()`.
        mare_shared::lua::core::config::install_introspection_fns(&lua, &mare)?;
        mare_shared::profile::record("init.lua", t_init.elapsed());
        if let Some(scope) = lua.app_data_ref::<crate::agent_tree::Scope>() {
            scope.configure(&config).map_err(anyhow::Error::msg)?;
        }
        // Install missing packages before loading plugins, retaining individual failures.
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut pkg_notices: Vec<String> = vec![];
        let interactive_startup = before_install.is_some();
        let mut blocked: Vec<String> = vec![];
        if opts.load_init && !opts.no_user_plugins {
            let mut progress = None;
            if interactive_startup && mare_plugin_manager::needs_install(&config) {
                let (shown, wait) = tokio::sync::oneshot::channel();
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                if let Some(notify) = before_install {
                    notify
                        .send(InstallStart { shown, progress: rx })
                        .map_err(|_| anyhow::anyhow!("startup UI closed before package installation"))?;
                    wait.await.map_err(|_| anyhow::anyhow!("startup UI closed before first frame"))?;
                    progress = Some(tx);
                }
            }
            pkg_notices = install_packages(&lua, &config, &cancel, progress).await;
            mare_shared::profile::mark("packages");
        }
        let kernel = Kernel::new(lua, config.clone())?;
        if opts.mode == Mode::Child {
            let id = opts.args.get("session_id").and_then(serde_json::Value::as_str).map(str::to_owned);
            let _ = super::session_identity::write_session_identity(&kernel, &identity, None, id, false);
        }
        kernel.install_parsed_service_contracts(parsed_contracts)?;
        // Essential and explicitly required failures block requests; other failures warn.
        let mut failures: Vec<PluginFailure> = vec![];
        for (file, e) in config_errors {
            // Neovim's wording; the whole error stays in the problems (/plugins, doctor)
            kernel.add_problem(format!("Error detected while processing {}: {e}", file.display()));
            failures.push(PluginFailure::config_file(file, e));
        }
        let mut unread = vec![];
        let (defs, refusals) = wanted_defs(
            &kernel,
            &config,
            &opts.cwd,
            opts.project_trusted,
            opts.no_user_plugins,
            &mut |src, e, slot| {
                // never printed here: the frontend shows it once (a TUI's screen must not get
                // stray stderr lines), the log and the problems keep it whole
                tracing::warn!("{e}");
                unread.push((src.clone(), e, slot.to_vec()));
            },
        );
        let essential = kernel.essential_services();
        for (src, e, slot) in unread {
            // a definition that can't be read can't declare `required` or `provide` either: it
            // is responsible for what the same-name definition it would replace provides (a
            // left-out built-in included: it holds that built-in's slot)
            failures.push(PluginFailure::plugin(
                &src.name_hint,
                "definition",
                e.clone(),
                Some(&src),
                &slot,
                &essential,
                plugin_failures::source_package_required(&config, &src),
            ));
            kernel.add_problem(e);
        }
        // same-source duplicates and the reserved name: refused while reading
        for r in &refusals {
            kernel.add_problem(r.message());
        }
        failures.extend(refusals.iter().map(PluginFailure::override_refused));
        // a same-name definition without `override = true` is refused (the kernel recorded the
        // problem); the earlier one stays, so it only warns
        failures.extend(kernel.set_defs(defs).iter().map(PluginFailure::override_refused));
        if opts.load_init && !opts.no_user_plugins {
            failures.extend(plugin_failures::unavailable_packages(&kernel, &config, &pkg_notices));
        }
        let mare: mlua::Table = kernel.lua().globals().get("mare")?;
        mare_plugin::config_load::sync_plugin_slots(kernel.lua(), &kernel.effective_declared())?;
        crate::plugins::introspect::install_early(&kernel, &mare)?;
        // packages' `init(pkg)` (lazy.nvim `init`): before their plugins load
        let init_problems = if opts.load_init { package_hooks(&kernel, "init", &config).await } else { vec![] };
        // Stage verified fork images into `mare.args.fork_images` before plugins start; the
        // session plugin reads and clears this table on load.
        if let Some(ref images) = fork_images
            && !images.is_empty()
            && let Ok(args_tbl) = mare.get::<mlua::Table>("args")
        {
            let tbl = kernel.lua().create_table()?;
            for (k, v) in images {
                tbl.set(k.as_str(), v.as_str())?;
            }
            args_tbl.set("fork_images", tbl)?;
        }
        let mare: mlua::Table = kernel.lua().globals().get("mare")?;
        crate::web_registry::install_lua(
            kernel.lua(),
            &mare,
            web.clone(),
            instance_id,
            web_session_id.clone(),
            opts.cwd.clone(),
        )?;
        crate::config::options::install_service(&kernel)?;
        let reload_gate = kernel.reload_gate();
        let session_cancel = std::rc::Rc::new(std::cell::RefCell::new(cancel.child_token()));
        let host_slot: request_gate::HostSlot = std::rc::Rc::new(std::cell::OnceCell::new());
        request_gate::register_lease_methods(&kernel, reload_gate.clone(), session_cancel.clone(), host_slot.clone())?;
        super::session_identity::register_identity_methods(&kernel, identity.clone(), host_slot.clone())?;
        crate::session_orchestrator::register_session_host_methods(&kernel, host_slot.clone())?;
        crate::session_orchestrator::register_workspace_host_methods(&kernel, host_slot.clone())?;
        // Coalesce startup status-bar renders across all plugin waves and keymap settle so the
        // status bar lays out once right before the first frame (preserving first frame == ready frame).
        let status_batch = StatusBatchGuard::enter(kernel.lua());
        let t_plugins = std::time::Instant::now();
        let start_tree = kernel.start_with_tree().await.context("starting plugins")?;
        kernel.bind_declaration_problem_targets();
        // Runtime service registrations are known only after the package's plugins have started.
        failures.extend(plugin_failures::package_hook_failures(&kernel, &config, "init", &init_problems));
        if let Some(error) = pkg_notices.iter().find(|n| n.starts_with("package installation task failed:"))
            && plugin_failures::package_operation_blocks(&kernel, &config)
        {
            blocked.push(error.clone());
        }
        mare_shared::profile::record("plugins (all)", t_plugins.elapsed());
        for f in failures.iter().chain(plugin_failures::start_failures(&kernel, &start_tree).iter()) {
            tracing::warn!("{}", f.notice());
        }
        for n in pkg_notices {
            if !failures.iter().any(|failure| failure.owns_package_problem(&n)) {
                kernel.add_problem(n);
            }
        }
        // packages' `config = function(pkg, opts)` (lazy.nvim `config`): after their plugins load,
        // only for packages that are installed and loaded
        if opts.load_init {
            let problems = package_hooks(&kernel, "config", &config).await;
            failures.extend(plugin_failures::package_hook_failures(&kernel, &config, "config", &problems));
        }
        for problem in failures.iter().filter_map(PluginFailure::package_problem) {
            kernel.add_problem(problem);
        }
        if let Some(problems) = mare_shared::lua::core::keymap::settle(kernel.lua(), &mare) {
            for problem in problems {
                tracing::warn!("{problem}");
                kernel.add_problem(problem);
            }
        }
        // the bytecode cache drops entries that can't be used again (at most daily, off-thread)
        let cache = mare_shared::paths::cache_dir().join("luac");
        mare_binding::blocking::spawn(kernel.lua(), move || mare_kernel::loader_prune_if_due_at(&cache))?;
        // Host lives on the single-threaded actor runtime; Arc (not Rc) so frontends can share it
        // the same way across modes. It is intentionally !Send.
        let startup_cli_trust =
            (opts.args.get("trust_source").and_then(|v| v.as_str()) == Some("cli")).then_some(opts.project_trusted);
        let reload = ReloadInfo {
            load_init: opts.load_init,
            no_user_plugins: opts.no_user_plugins,
            cli: opts.config.clone(),
            cli_flag_paths,
            startup_project_trusted: opts.project_trusted,
            startup_cli_trust,
        };
        let workspace = crate::WorkspaceState::new(opts.cwd.clone(), opts.project_trusted, kernel.config())?;
        let waits = std::rc::Rc::new(UiWaits::default());
        #[allow(clippy::arc_with_non_send_sync)]
        let host = Arc::new(Self {
            kernel,
            ui,
            mode: opts.mode,
            cwd: opts.cwd,
            workspace,
            answerer: crate::AnswererSlot::new(),
            inbound_held: crate::InboundHeld::default(),
            prompt_receipts: crate::SessionPromptReceipts::new(),
            shell: crate::UiShellState::new(),
            instance_id,
            web,
            identity,
            reload,
            reload_gate,
            reload_active: std::cell::Cell::new(false),
            reload_failures: std::cell::RefCell::new(None),
            reload_pending: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            reload_retry: std::cell::RefCell::new(std::collections::BTreeSet::new()),
            reload_epoch: std::cell::Cell::new(0),
            reload_full_incomplete: std::cell::Cell::new(false),
            slot_sync_pending: std::cell::Cell::new(false),
            pending_session_trust_revoke: std::cell::Cell::new(false),
            pending_session_trust_full_reload: std::cell::Cell::new(false),
            startup_blocked: std::cell::RefCell::new(blocked),
            plugin_failures: std::cell::RefCell::new(failures),
            reloaded: std::cell::Cell::new(false),
            startup_problems: std::cell::RefCell::new(vec![]),
            session_cancel,
            cancel,
            waits,
            orchestrator: std::cell::RefCell::new(None),
            startup_gc: std::cell::RefCell::new(if opts.mode == Mode::Tui { startup_gc.take() } else { None }),
        });
        let _ = host_slot.set(Arc::downgrade(&host));
        if let Some(reason) = host.blocked_reason() {
            tracing::warn!("{reason}");
            host.kernel.add_problem(reason);
        }
        host.startup_problems.replace(host.kernel.problems());
        host.lua().set_app_data(agent::ParentHost(Arc::downgrade(&host)));
        // the host's readiness vote first: it is the first voter of the host segment, ahead of
        // init.lua's voters (registered in their declaration order, after it, on every reload too)
        host.install_reload_policy()?;
        host.install_config_runtime()?;
        // Network approval is owned per-session; a child agent does not install its own approver
        // and instead receives an explicit delegation from its parent session.
        if opts.mode != Mode::Child {
            host.install_net_approver();
        } else if let Some(approver) = net_approver {
            mare_binding::netauth::set_approver(host.lua(), Some(approver));
        }
        host.install_wait_hook()?;
        host.install_session_cancel_hook()?;
        host.install_keymap_option_hook()?;
        let id = session_info_for_kernel(&host.kernel)
            .ok()
            .flatten()
            .and_then(|info| info.get::<Option<String>>("session_id").ok().flatten())
            .or_else(|| {
                if opts.mode == Mode::Child {
                    opts.args.get("session_id").and_then(serde_json::Value::as_str).map(str::to_owned)
                } else {
                    None
                }
            });
        let _ = host.apply_session_identity(id, false);
        host.sync_web_assets();
        host.forward_child_events();
        // Attach the target orchestrator (Multi for sibling hosts spawned by a HostManager, or
        // Single for standalone hosts) BEFORE `app/ready` fires so plugin startup listeners
        // calling `ctx.session:create/load/fork/close` dispatch through the final orchestrator.
        if let Some(orch) = orchestrator {
            host.set_orchestrator(orch);
        } else {
            host.attach_single_orchestrator();
        }
        status_batch.finish();
        // The interactive TUI (and deferred RPC session loaders) emit app/ready themselves after
        // UI/capability hooks are ready, so session restore (-c / -r / ui.select / session/loaded)
        // sees live UI connectivity without delaying the input box.
        if emit_ready && opts.mode != Mode::Tui {
            host.kernel.emit("app/ready", MultiValue::new()).await;
            clear_fork_images_arg(host.lua());
        }
        Ok(host)
    }

    /// Emit deferred `app/ready` once caller-side UI/capability bridges are installed.
    pub async fn emit_app_ready(&self) {
        self.kernel.emit("app/ready", MultiValue::new()).await;
        clear_fork_images_arg(self.lua());
    }

    /// Restore Lua's generational GC `minormajor` threshold after the interactive TUI has drawn
    /// its first frame. Idempotent; non-TUI hosts restore immediately before returning.
    pub fn restore_startup_gc(&self) {
        let _ = self.startup_gc.borrow_mut().take();
    }
}

/// Clear any staged `mare.args.fork_images` table after startup completes (whether the fork path
/// consumed it, failed, or was skipped).
fn clear_fork_images_arg(lua: &mlua::Lua) {
    if let Ok(mare) = lua.globals().get::<Table>("mare")
        && let Ok(args_tbl) = mare.get::<Table>("args")
    {
        let _ = args_tbl.set("fork_images", mlua::Value::Nil);
    }
}

/// Read the active session's `info()` table via the kernel service registry, if loaded.
pub(crate) fn session_info_for_kernel(kernel: &mare_kernel::Kernel) -> mlua::Result<Option<Table>> {
    let Some(svc) = crate::ui_wait::service_table_for_kernel(kernel, mare_shared::SESSION_SERVICE_NAME) else {
        return Ok(None);
    };
    let info: Table = svc.get::<mlua::Function>("info")?.call(svc)?;
    Ok(Some(info))
}

impl Host {
    fn install_reload_policy(self: &Arc<Self>) -> Result<()> {
        let lua = self.lua().clone();
        let mare: Table = lua.globals().get("mare")?;
        request_gate::install(self, &mare)?;
        introspect::install(self, &mare)?;
        Ok(())
    }

    fn install_session_cancel_hook(self: &Arc<Self>) -> Result<()> {
        let weak = Arc::downgrade(self);
        let cb = self.lua().create_function(move |_, _: MultiValue| {
            if let Some(host) = weak.upgrade() {
                host.rotate_session_cancel();
            }
            Ok(())
        })?;
        // a listener of the host's fiber
        let (ctx, owner) = (self.kernel.host_ctx()?, self.kernel.host_owner()?);
        self.kernel.host_on(&ctx, owner, "session/changed", cb, mare_kernel::HostOn::default())?;
        Ok(())
    }

    fn install_keymap_option_hook(self: &Arc<Self>) -> Result<()> {
        let cb = self.lua().create_function(move |lua, event: Table| {
            if event.get::<Option<String>>("name")?.as_deref() == Some("keys") {
                let mare: Table = lua.globals().get("mare")?;
                let keys_val = match mare.get::<mlua::Value>("config")? {
                    mlua::Value::Table(c) => c.get::<mlua::Value>("keys")?,
                    _ => mlua::Value::Nil,
                };
                let sources = mare_shared::lua::core::config::opt_sources(lua)
                    .map(mlua::Value::Table)
                    .unwrap_or(mlua::Value::Nil);
                mare_shared::lua::core::keymap::apply_config_keys(lua, keys_val, sources, false)?;
            }
            Ok(())
        })?;
        // Apply runtime `keys` changes before user/plugin `option/changed` listeners run.
        let (ctx, owner) = (self.kernel.host_ctx()?, self.kernel.host_owner()?);
        self.kernel.host_on(&ctx, owner, "option/changed", cb, mare_kernel::HostOn { prepend: true })?;
        Ok(())
    }

    pub async fn shutdown(&self) {
        let _ = self.shutdown_report().await;
    }

    /// Run the shutdown sequence: cancel the host root token, give the cooperative
    /// stages (agent tree, `app/shutdown`, plugin release, blocking scope) one shared
    /// `shutdown_grace_ms` budget — every stage is still started after the budget
    /// runs out, so an uncooperative stage never skips later cleanup — then run process-tree
    /// termination to completion without truncating it. Unconfirmed tasks or process trees are
    /// logged and reported as `stop_unconfirmed`.
    pub async fn shutdown_report(&self) -> crate::ShutdownReport {
        // Cancel the host root token (and child tokens: session_cancel, package ops) first.
        self.web.remove_host(self.instance_id);
        self.cancel.cancel();
        let scope = self.lua().app_data_ref::<crate::agent_tree::Scope>().map(|s| s.clone());
        let grace = self.shutdown_grace();
        let deadline = tokio::time::Instant::now() + grace;
        let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
        let mut unconfirmed_tasks = Vec::new();

        if let Some(scope) = &scope
            && tokio::time::timeout(left(), scope.drain()).await.is_err()
            && scope.active_count() > 0
        {
            unconfirmed_tasks.push("agent_tree");
        }

        if tokio::time::timeout(left(), self.kernel.emit("app/shutdown", MultiValue::new())).await.is_err() {
            unconfirmed_tasks.push("app_shutdown");
        }

        let unreleased = self.kernel.shutdown_until(deadline).await;
        if !unreleased.is_empty() {
            tracing::warn!(plugins = ?unreleased, "stop_unconfirmed: plugin release did not finish within shutdown_grace_ms");
            unconfirmed_tasks.push("kernel_shutdown");
        }

        if tokio::time::timeout(left(), mare_binding::blocking::close_and_wait(self.lua())).await.is_err() {
            if mare_binding::blocking::close(self.lua()) {
                unconfirmed_tasks.push("blocking_scope");
            } else if unconfirmed_tasks.is_empty() {
                unconfirmed_tasks.push("blocking_scope");
            }
        }

        if !unconfirmed_tasks.is_empty() {
            unconfirmed_tasks.push("cooperative_shutdown");
            tracing::warn!(
                grace_ms = grace.as_millis() as u64,
                tasks = ?unconfirmed_tasks,
                "stop_unconfirmed: cooperative shutdown exceeded shutdown_grace_ms"
            );
        }

        // Process-tree termination is never truncated by `shutdown_grace_ms`.
        let unconfirmed_trees = mare_binding::trees::end_all_graceful(self.lua()).await;
        if !unconfirmed_trees.is_empty() {
            tracing::warn!(
                pids = ?unconfirmed_trees,
                "stop_unconfirmed: process trees did not confirm exit"
            );
        }
        mare_plugin::flush_lua_cov(self.lua());
        mare_binding::peers::session_cell(self.lua()).set(None);
        crate::ShutdownReport { unconfirmed_tasks, unconfirmed_trees }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_gc_guard_raises_and_restores_default_minor_to_major() {
        let lua = mare_plugin::new_lua().unwrap();
        let read = |l: &mlua::Lua| -> i32 { l.load(r#"return collectgarbage("param", "minormajor")"#).eval().unwrap() };
        assert_eq!(read(&lua), DEFAULT_GC_MINOR_TO_MAJOR);
        {
            let _guard = StartupGcGuard::enter(&lua);
            assert_eq!(read(&lua), STARTUP_GC_MINOR_TO_MAJOR);
        }
        assert_eq!(read(&lua), DEFAULT_GC_MINOR_TO_MAJOR);
    }
}
