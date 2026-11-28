//! `mare.subagent`: run an independent agent loop in a fresh Lua VM + task.
//! Sub-agents, workflows and teams are built on top of this.

mod approvals;
pub(crate) mod inheritance;
mod runtime;

use std::path::PathBuf;

use mlua::{Lua, Table, UserData, UserDataMethods, Value};
use tokio::sync::{Mutex as AsyncMutex, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::Host;
use crate::agent_registry::{AgentEntry, AgentInfo, AgentRegistry, AgentStatus, SteerMessage, now_ms, try_send_steer};
use mare_kernel::TapEvent;
use mare_shared::lua::{HISTORY_LOSSLESS_MAX_NODES, json_to_lua, json_to_lua_lossless_bounded, lua_to_json, rt_err};

use approvals::origin_of;
use inheritance::{
    apply_inheritance, inherits_parent_preset, parent_permission_sync, validate_and_resolve_history_model,
};
use runtime::{ChildEndMeta, ChildRuntimeArgs, current_session_scope, parent_session_identity, spawn_child_thread};

pub use crate::approvals::child_events::{ChildApproval, ChildEvent, ChildEventsRx};

/// The host that owns this VM, so `mare.subagent.start` can read the parent's permission preset.
pub struct ParentHost(pub std::sync::Weak<Host>);
pub struct ChildEvents(pub mpsc::UnboundedSender<ChildEvent>);
#[derive(Clone, Default)]
struct ToolCeiling {
    names: Option<std::collections::HashSet<String>>,
    prefixes: Vec<String>,
    denied: std::collections::HashSet<String>,
}

impl ToolCeiling {
    fn allows(&self, name: &str) -> bool {
        let Some(names) = &self.names else {
            return true;
        };
        if names.contains(name) {
            return true;
        }
        if self.denied.contains(name) {
            return false;
        }
        self.prefixes.iter().any(|prefix| !prefix.is_empty() && name.starts_with(prefix))
    }

    fn allows_prefix(&self, prefix: &str) -> bool {
        if prefix.is_empty() {
            return false;
        }
        if self.names.is_none() {
            return true;
        }
        self.prefixes.iter().any(|allowed| !allowed.is_empty() && prefix.starts_with(allowed))
    }
}
struct SessionScopeState {
    active: crate::agent_tree::Scope,
    pending_drains: Vec<crate::agent_tree::Scope>,
    draining: bool,
}
struct SessionScope(std::sync::Arc<std::sync::Mutex<SessionScopeState>>);

struct SessionDrainGuard {
    state: std::sync::Arc<std::sync::Mutex<SessionScopeState>>,
    root: crate::agent_tree::Scope,
}
impl Drop for SessionDrainGuard {
    fn drop(&mut self) {
        let mut st = self.state.lock().unwrap();
        if st.active.cancel.is_cancelled() {
            let old = std::mem::replace(&mut st.active, self.root.session_scope());
            if old.active_count() > 0 {
                st.pending_drains.push(old);
            }
        }
        st.pending_drains.retain(|s| s.active_count() > 0);
        st.draining = false;
    }
}
const MAX_CHILD_BINDINGS: usize = 256;

#[derive(Default)]
struct ChildBindingMap {
    by_subagent: std::collections::HashMap<String, serde_json::Value>,
    running: std::collections::HashSet<String>,
    finished_order: std::collections::VecDeque<String>,
}

impl ChildBindingMap {
    fn prune_finished(&mut self, mut is_retained: impl FnMut(&str) -> bool) {
        self.finished_order.retain(|id| {
            if !self.by_subagent.contains_key(id) {
                return false;
            }
            if is_retained(id) {
                true
            } else {
                self.by_subagent.remove(id);
                false
            }
        });
        // Fallback safety cap: only evict finished bindings, never live running children.
        while self.by_subagent.len() > MAX_CHILD_BINDINGS {
            let Some(oldest) = self.finished_order.pop_front() else {
                break;
            };
            self.by_subagent.remove(&oldest);
        }
    }

    fn insert_for_parent(
        &mut self,
        subagent_id: String,
        parent_id: &str,
        parent_epoch: &str,
        is_retained: impl FnMut(&str) -> bool,
    ) {
        self.by_subagent.retain(|_, v| {
            v.get("parent_session_id").and_then(serde_json::Value::as_str) == Some(parent_id)
                && v.get("parent_session_epoch").and_then(serde_json::Value::as_str) == Some(parent_epoch)
        });
        self.running.retain(|id| self.by_subagent.contains_key(id));
        self.finished_order.retain(|id| self.by_subagent.contains_key(id));
        let binding = serde_json::json!({
            "subagent_id": subagent_id,
            "parent_session_id": parent_id,
            "parent_session_epoch": parent_epoch,
        });
        self.by_subagent.insert(subagent_id.clone(), binding);
        self.running.insert(subagent_id.clone());
        self.finished_order.retain(|id| id != &subagent_id);
        self.prune_finished(is_retained);
    }

    fn mark_finished(&mut self, subagent_id: &str, is_retained: impl FnMut(&str) -> bool) {
        self.running.remove(subagent_id);
        if self.by_subagent.contains_key(subagent_id) && !self.finished_order.iter().any(|id| id == subagent_id) {
            self.finished_order.push_back(subagent_id.to_owned());
        }
        self.prune_finished(is_retained);
    }
}

#[derive(Clone, Default)]
pub(super) struct ChildBindings(std::sync::Arc<std::sync::Mutex<ChildBindingMap>>);

impl ChildBindings {
    pub(super) fn mark_finished(&self, subagent_id: &str, registry: &AgentRegistry) {
        let retained: std::collections::HashSet<String> =
            registry.list().into_iter().map(|info| info.subagent_id).collect();
        if let Ok(mut guard) = self.0.lock() {
            guard.mark_finished(subagent_id, |id| retained.contains(id));
        }
    }
}
static NEXT_CHILD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
const DEFAULT_DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ChildError {
    Cancelled,
    Failed(String),
}

impl std::fmt::Display for ChildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed(s) => f.write_str(s),
        }
    }
}

pub struct AgentHandle {
    id: String,
    info: AgentInfo,
    events: AsyncMutex<mpsc::Receiver<TapEvent>>,
    result: watch::Receiver<Option<Result<serde_json::Value, ChildError>>>,
    cancel: CancellationToken,
    cancel_scope: Option<crate::agent_tree::Scope>,
    inherits_parent_preset: bool,
    steer: mpsc::Sender<SteerMessage>,
}

impl UserData for AgentHandle {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("subagent_id", |_, this, ()| Ok(this.id.clone()));
        m.add_method("info", |lua, this, ()| {
            json_to_lua_lossless_bounded(
                lua,
                &serde_json::to_value(&this.info).map_err(rt_err)?,
                HISTORY_LOSSLESS_MAX_NODES,
            )
        });
        // next_event() -> {name, payload} | nil when finished
        m.add_async_method("next_event", |lua, this, ()| async move {
            match this.events.lock().await.recv().await {
                Some(ev) => {
                    let t = lua.create_table()?;
                    t.set("name", ev.name)?;
                    t.set("payload", json_to_lua(&lua, &ev.payload)?)?;
                    Ok(Value::Table(t))
                }
                None => Ok(Value::Nil),
            }
        });
        // wait() -> result {text, messages, usage} (raises on failure)
        m.add_async_method("wait", |lua, this, ()| async move {
            // Completion belongs to the handle, never to an individual waiter. Each call
            // owns its subscription, so cancellation cannot consume a receiver or retain
            // an exclusive lock needed by a later Lua coroutine.
            let mut receiver = this.result.clone();
            let result = receiver
                .wait_for(Option::is_some)
                .await
                .map(|value| value.clone().unwrap())
                .unwrap_or_else(|_| Err(ChildError::Failed("agent task dropped".into())));
            match result {
                Ok(v) => json_to_lua(&lua, &v),
                Err(e) => Err(rt_err(e.to_string())),
            }
        });
        m.add_method("cancel", |_, this, ()| {
            if let Some(scope) = &this.cancel_scope {
                scope.cancel_as("parent");
            } else {
                this.cancel.cancel();
            }
            Ok(())
        });
        m.add_method("steer", |lua, this, text: String| {
            let sync = match (this.inherits_parent_preset, this.cancel_scope.as_ref()) {
                (true, Some(scope)) => Some(parent_permission_sync(lua, scope)?),
                _ => None,
            };
            try_send_steer(&this.steer, text, peer_caller_source(lua), sync).map_err(rt_err)
        });
    }
}

fn peer_caller_source(lua: &Lua) -> Option<String> {
    let caller = mare_kernel::current_export_caller()?;
    let is_peer = lua
        .app_data_ref::<std::sync::Weak<mare_kernel::KernelInner>>()
        .and_then(|w| mare_kernel::Kernel::from_weak(&w))
        .is_some_and(|k| k.is_peer_caller(Some(caller)));
    is_peer.then(|| "peer".to_string())
}

fn resolve_cwd(lua: &Lua, spec: &serde_json::Value) -> PathBuf {
    spec.get("cwd").and_then(|v| v.as_str()).map(PathBuf::from).unwrap_or_else(|| {
        lua.globals()
            .get::<Table>("mare")
            .and_then(|m| m.get::<String>("cwd"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default())
    })
}

pub(super) struct ChildTrust {
    pub(super) project_trusted: bool,
    pub(super) trust_arg: Option<String>,
    pub(super) trust_source: Option<String>,
    pub(super) approve_mcps: bool,
}

fn resolve_child_trust(lua: &Lua, child_cwd: &std::path::Path) -> ChildTrust {
    let Some(host) = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade()) else {
        return ChildTrust { project_trusted: false, trust_arg: None, trust_source: None, approve_mcps: false };
    };
    let parent_args = lua.globals().get::<Table>("mare").and_then(|m| m.get::<Table>("args")).ok();
    let parent_trust_arg = parent_args.as_ref().and_then(|a| a.get::<Option<String>>("trust").ok().flatten());
    let parent_trust_source = parent_args.as_ref().and_then(|a| a.get::<Option<String>>("trust_source").ok().flatten());
    let parent_approve_mcps =
        parent_args.as_ref().and_then(|a| a.get::<Option<bool>>("approve_mcps").ok().flatten()).unwrap_or(false);

    let rr = mare_shared::project::resource_root(child_cwd);
    let subject = mare_shared::project::trust_subject(&rr);
    let child_root = dunce::canonicalize(subject).unwrap_or_else(|_| subject.to_path_buf());
    let same_ws = child_root == host.workspace_root();
    let cli_trust = host.startup_cli_trust();
    let project_trusted = if same_ws {
        cli_trust != Some(false) && host.is_project_trusted()
    } else {
        crate::resolve_workspace_trust(child_cwd, false, host.is_project_trusted(), cli_trust, host.no_user_plugins())
    };
    if cli_trust == Some(false) {
        return ChildTrust {
            project_trusted,
            trust_arg: Some("untrusted".into()),
            trust_source: Some("cli".into()),
            approve_mcps: false,
        };
    }
    if same_ws {
        let trust_arg = if project_trusted {
            parent_trust_arg.as_ref().map(|_| "trusted".to_owned())
        } else {
            parent_trust_arg.as_deref().map(|v| match v {
                "untrusted" | "undecided" | "none" => v.to_owned(),
                _ => "untrusted".to_owned(),
            })
        };
        ChildTrust {
            project_trusted,
            trust_arg,
            trust_source: parent_trust_source,
            approve_mcps: project_trusted && parent_approve_mcps,
        }
    } else {
        ChildTrust {
            project_trusted,
            trust_arg: Some(if project_trusted { "trusted" } else { "untrusted" }.into()),
            trust_source: project_trusted.then(|| "saved".into()),
            approve_mcps: false,
        }
    }
}

enum StartError {
    Coded(&'static str, String),
    Other(mlua::Error),
}

impl StartError {
    fn coded(code: &'static str, message: impl Into<String>) -> Self {
        Self::Coded(code, message.into())
    }
}

impl From<mlua::Error> for StartError {
    fn from(err: mlua::Error) -> Self {
        Self::Other(err)
    }
}

fn coded_result(lua: &Lua, code: &str, message: &str) -> mlua::Result<(bool, Value)> {
    let err = mare_shared::lua_error::error_value(lua, code, message, Value::Nil)?;
    Ok((false, Value::Table(err)))
}

/// start{prompt, model, effort, system, cwd, config = {plugins = {...}}, tools = {"read",...},
///       messages = {...} (fork history), plugins_only = bool}
fn start_raw(lua: &Lua, opts: Value) -> mlua::Result<(bool, Value)> {
    let Value::Table(opts) = opts else {
        return coded_result(lua, "invalid_arguments", "start requires an options table");
    };
    let spec = lua_to_json(lua, Value::Table(opts))
        .map_err(|e| StartError::coded("invalid_arguments", format!("invalid start options: {e}")));
    let res = spec.and_then(|spec| start_with_spec(lua, spec));
    match res {
        Ok((handle, _)) => Ok((true, Value::UserData(lua.create_userdata(handle)?))),
        Err(StartError::Coded(code, msg)) => coded_result(lua, code, &msg),
        Err(StartError::Other(err)) => Err(err),
    }
}

fn start_with_spec(lua: &Lua, mut spec: serde_json::Value) -> Result<(AgentHandle, AgentInfo), StartError> {
    // Validate arguments first so refusal precedence is deterministic:
    // arguments -> tree state -> duplicate name -> capacity/depth.
    if !spec.is_object() {
        return Err(StartError::coded("invalid_arguments", "start requires an options table"));
    }
    if let Some(src) = peer_caller_source(lua) {
        if let Some(obj) = spec.as_object_mut() {
            obj.insert("source".into(), serde_json::Value::String(src));
        } else {
            return Err(StartError::coded("invalid_arguments", "start requires an options table"));
        }
    }
    let inherited_ceiling = lua.app_data_ref::<ToolCeiling>().map(|ceiling| ceiling.clone());
    let has_inherited_ceiling = inherited_ceiling.as_ref().is_some_and(|c| c.names.is_some());
    match spec.get(crate::frontend::TOOLS) {
        None | Some(serde_json::Value::Null) => {
            if let Some(ceiling) = &inherited_ceiling
                && let Some(inherited) = &ceiling.names
            {
                let mut names: Vec<_> = inherited.iter().cloned().collect();
                names.sort();
                spec[crate::frontend::TOOLS] = serde_json::json!(names);
            }
        }
        Some(serde_json::Value::Array(values)) => {
            let mut names = values
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| StartError::coded("invalid_arguments", "tools must contain strings"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(ceiling) = &inherited_ceiling
                && ceiling.names.is_some()
            {
                names.retain(|name| ceiling.allows(name));
                names.sort();
                names.dedup();
                spec[crate::frontend::TOOLS] = serde_json::json!(names);
            }
        }
        Some(_) => return Err(StartError::coded("invalid_arguments", "tools must be an array")),
    }
    match spec.get("tool_discovery_prefixes") {
        None | Some(serde_json::Value::Null) => {
            if has_inherited_ceiling
                && spec.get(crate::frontend::TOOLS).is_some()
                && let Some(ceiling) = &inherited_ceiling
                && !ceiling.prefixes.is_empty()
            {
                // When the caller didn't narrow `tools` explicitly and omitted prefixes, only
                // propagate prefixes if `tool_discovery_prefixes` was not intentionally omitted;
                // callers with an explicit allowlist leave `tool_discovery_prefixes` unset.
            }
        }
        Some(serde_json::Value::Array(values)) => {
            let mut prefixes = values
                .iter()
                .map(|value| {
                    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| {
                        StartError::coded("invalid_arguments", "tool_discovery_prefixes must contain nonempty strings")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(ceiling) = &inherited_ceiling {
                prefixes.retain(|prefix| ceiling.allows_prefix(prefix));
            }
            prefixes.sort();
            prefixes.dedup();
            spec["tool_discovery_prefixes"] = serde_json::json!(prefixes);
        }
        Some(_) => {
            return Err(StartError::coded("invalid_arguments", "tool_discovery_prefixes must be an array"));
        }
    }
    match spec.get("tool_ceiling_denied") {
        None | Some(serde_json::Value::Null) => {
            if let Some(ceiling) = &inherited_ceiling
                && !ceiling.denied.is_empty()
            {
                let mut denied: Vec<_> = ceiling.denied.iter().cloned().collect();
                denied.sort();
                spec["tool_ceiling_denied"] = serde_json::json!(denied);
            }
        }
        Some(serde_json::Value::Array(values)) => {
            let mut denied = values
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_owned).ok_or_else(|| {
                        StartError::coded("invalid_arguments", "tool_ceiling_denied must contain strings")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            if let Some(ceiling) = &inherited_ceiling {
                denied.extend(ceiling.denied.iter().cloned());
            }
            denied.sort();
            denied.dedup();
            spec["tool_ceiling_denied"] = serde_json::json!(denied);
        }
        Some(_) => return Err(StartError::coded("invalid_arguments", "tool_ceiling_denied must be an array")),
    }
    let prompt = spec
        .get("prompt")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| StartError::coded("invalid_arguments", "start requires a string prompt"))?;
    let requested_name = match spec.get("name") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(StartError::coded("invalid_arguments", "name must be a string")),
    };
    if let Some(schema) = spec.get("output_schema").filter(|v| !v.is_null()) {
        mare_shared::schema::OutputSchema::compile(schema.clone())
            .map_err(|e| StartError::coded("invalid_arguments", format!("invalid output_schema: {e}")))?;
    }
    if spec.get("_fork_origin").is_some()
        && let Some(effort) = spec.get("_history_effort").filter(|value| !value.is_null())
        && effort.as_str().is_none_or(str::is_empty)
    {
        return Err(StartError::coded("invalid_arguments", "snapshot source effort must be a non-empty string"));
    }

    if let Some(host) = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade())
        && let Some(session) = host.service_table(crate::frontend::SESSION_SERVICE)
        && session
            .get::<Option<mlua::Function>>("switching")?
            .map(|f| f.call::<bool>(session.clone()))
            .transpose()?
            .unwrap_or(false)
    {
        return Err(StartError::coded(
            "subagent.tree_closed",
            "cannot start a subagent while the session is switching",
        ));
    }
    let session_scope = lua.app_data_ref::<SessionScope>().ok_or_else(|| rt_err("agent tree unavailable"))?;
    let scope = {
        let state = session_scope.0.lock().unwrap();
        if state.draining || state.active.cancel.is_cancelled() {
            return Err(StartError::coded("subagent.tree_closed", "agent_tree_closed"));
        }
        state.active.clone()
    };
    drop(session_scope);
    let registry = registry(lua)?;
    let name_reservation = registry
        .reserve_name(requested_name.as_deref())
        .map_err(|msg| StartError::coded("subagent.duplicate_name", msg))?;
    let (scope, reservation) = scope.reserve().map_err(|reason| match reason {
        "agent_tree_closed" => StartError::coded("subagent.tree_closed", reason),
        "agent_depth_limit" | "agent_capacity_limit" => StartError::coded("subagent.limit_reached", reason),
        other => StartError::Other(rt_err(other)),
    })?;
    let cwd = resolve_cwd(lua, &spec);
    let inherits_preset = inherits_parent_preset(&spec);
    let trust = resolve_child_trust(lua, &cwd);
    let (parent_policy, config, carried) = apply_inheritance(lua, &mut spec)?;
    let history_model = validate_and_resolve_history_model(lua, &spec)?;

    let (ev_tx, ev_rx) = mpsc::channel(mare_kernel::SESSION_LOG_CAPACITY);
    let (res_tx, res_rx) = watch::channel(None);
    let child_id = NEXT_CHILD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let lifecycle = lua.app_data_ref::<ChildEvents>().map(|c| c.0.clone());
    let parent_session = parent_session_identity(lua)?;
    let transcript = match &parent_session {
        Some(parent) if parent.children_dir.is_some() => Some(crate::child_transcript::Transcript::create(
            parent.children_dir.as_deref().unwrap(),
            &parent.id,
            &cwd,
            &spec,
        )?),
        _ => None,
    };
    let session_id = transcript.as_ref().map(|t| t.id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let child_bindings = lua.app_data_ref::<ChildBindings>().map(|b| b.clone());
    if let (Some(parent), Some(bindings)) = (&parent_session, &child_bindings) {
        let retained: std::collections::HashSet<String> =
            registry.list().into_iter().map(|info| info.subagent_id).collect();
        let mut guard = bindings.0.lock().unwrap();
        guard.insert_for_parent(session_id.clone(), &parent.id, &parent.session_epoch, |id| retained.contains(id));
    }

    let agent_id = format!("agent-{child_id}");
    let label = spec.get("label").or_else(|| spec.get("name")).and_then(|v| v.as_str()).map(str::to_string);
    let run_id = spec.get("run_id").and_then(|v| v.as_str()).map(str::to_string);
    let seq = spec.get("seq").and_then(|v| v.as_u64());
    let mut origin = origin_of(child_id, &session_id, &spec);
    let task = origin.get("task").and_then(|v| v.as_str()).map(str::to_string);
    let parent_identity = parent_session.as_ref().map(|p| (p.id.clone(), p.session_epoch.clone()));
    if let Some(tx) = &lifecycle {
        let _ = tx.send(ChildEvent::Start {
            subagent_id: session_id.clone(),
            agent_id: agent_id.clone(),
            name: requested_name.clone(),
            label: label.clone(),
            run_id: run_id.clone(),
            seq,
            task: task.clone(),
            parent_session: parent_identity.clone(),
        });
    }
    if let Some(parent) = &parent_session {
        origin["parent_session_id"] = parent.id.clone().into();
        origin["parent_session_epoch"] = parent.session_epoch.clone().into();
    }
    let shares_rules = crate::approvals::shared_rule::shares_parent_rules(&spec);
    let parent_net_approver = mare_binding::netauth::approver(lua);
    let parent_snapshot = mare_binding::snapshot::handle(lua);
    if spec.get("agent_label").is_none()
        && let Some(l) = origin.get("label").cloned()
    {
        spec["agent_label"] = l;
    }

    let (steer_tx, steer_rx) = mpsc::channel::<SteerMessage>(crate::agent_registry::STEER_QUEUE_CAPACITY);
    let cancel = scope.cancel.clone();
    let entry_info = AgentInfo {
        subagent_id: session_id.clone(),
        name: requested_name.clone(),
        label: label.clone(),
        run_id: run_id.clone(),
        seq,
        task,
        parent_session_id: parent_session.as_ref().map(|p| p.id.clone()),
        status: AgentStatus::Running,
        started_at_ms: now_ms(),
        ended_at_ms: None,
        error: None,
        error_kind: None,
    };
    registry.insert(
        name_reservation,
        AgentEntry {
            info: entry_info.clone(),
            parent_session_epoch: parent_session.as_ref().map(|p| p.session_epoch.clone()),
            result_rx: res_rx.clone(),
            cancel: cancel.clone(),
            cancel_scope: Some(scope.clone()),
            inherits_parent_preset: inherits_preset,
            steer: steer_tx.clone(),
            collected: false,
        },
    );
    // A child agent gets its own VM, owned by its own thread + single-threaded runtime (the
    // actor model again); it talks to the parent only through Send channels.
    spawn_child_thread(ChildRuntimeArgs {
        prompt,
        cwd,
        trust,
        spec,
        config,
        history_model,
        parent_policy,
        carried,
        scope: scope.clone(),
        reservation,
        parent_net_approver,
        parent_snapshot: Some(parent_snapshot),
        transcript,
        origin,
        shares_rules,
        inherits_parent_preset: inherits_preset,
        cancel: cancel.clone(),
        ev_tx,
        steer_rx,
        res_tx,
        lifecycle,
        registry,
        bindings: child_bindings,
        meta: ChildEndMeta {
            child_id,
            session_id: session_id.clone(),
            agent_id,
            name: requested_name,
            label,
            run_id,
            seq,
            parent_session: parent_identity,
        },
    })?;
    Ok((
        AgentHandle {
            id: session_id,
            info: entry_info.clone(),
            events: AsyncMutex::new(ev_rx),
            result: res_rx,
            cancel,
            cancel_scope: Some(scope),
            inherits_parent_preset: inherits_preset,
            steer: steer_tx,
        },
        entry_info,
    ))
}

fn registry(lua: &Lua) -> mlua::Result<AgentRegistry> {
    let registry = lua
        .app_data_ref::<AgentRegistry>()
        .map(|r| r.clone())
        .ok_or_else(|| rt_err("subagent registry unavailable"))?;
    Ok(registry.for_session(current_session_scope(lua)?))
}

const SUBAGENT_TABLE_KEY: &str = "mare.subagent_table";

pub(crate) fn subagent_table(lua: &Lua) -> mlua::Result<Table> {
    lua.named_registry_value(SUBAGENT_TABLE_KEY)
}

pub(crate) async fn drain_session_as(
    lua: &Lua,
    reason: &'static str,
    timeout: std::time::Duration,
) -> mlua::Result<()> {
    drain_session_inner(
        lua,
        reason,
        timeout,
        "subagents did not stop in time",
        "child notifications did not drain in time",
    )
    .await
}

async fn join_all_units<F: std::future::Future<Output = ()>>(futs: impl IntoIterator<Item = F>) {
    let mut pending: Vec<Option<std::pin::Pin<Box<F>>>> = futs.into_iter().map(|f| Some(Box::pin(f))).collect();
    std::future::poll_fn(|cx| {
        let mut all_ready = true;
        for slot in &mut pending {
            if let Some(fut) = slot.as_mut() {
                if fut.as_mut().poll(cx).is_ready() {
                    *slot = None;
                } else {
                    all_ready = false;
                }
            }
        }
        if all_ready { std::task::Poll::Ready(()) } else { std::task::Poll::Pending }
    })
    .await;
}

async fn drain_session_inner(
    lua: &Lua,
    reason: &'static str,
    timeout: std::time::Duration,
    stop_timeout_msg: &'static str,
    notify_timeout_msg: &'static str,
) -> mlua::Result<()> {
    let Some(shared) = lua.app_data_ref::<SessionScope>().map(|s| s.0.clone()) else {
        return Ok(());
    };
    let root = lua.app_data_ref::<crate::agent_tree::Scope>().ok_or_else(|| rt_err("agent tree unavailable"))?.clone();
    let (state, scopes) = {
        let mut state = shared.lock().unwrap();
        if state.draining {
            return Err(rt_err("subagent session drain already in progress"));
        }
        state.draining = true;
        state.pending_drains.retain(|s| s.active_count() > 0);
        let mut scopes = state.pending_drains.clone();
        scopes.push(state.active.clone());
        drop(state);
        (shared, scopes)
    };
    let _guard = SessionDrainGuard { state: state.clone(), root: root.clone() };
    let deadline = tokio::time::Instant::now() + timeout;
    tokio::time::timeout_at(deadline, join_all_units(scopes.iter().map(|scope| scope.drain_as(reason))))
        .await
        .map_err(|_| rt_err(stop_timeout_msg))?;
    let forwarding = {
        let rx = lua.app_data_ref::<ChildEventsRx>().ok_or_else(|| rt_err("child event queue unavailable"))?;
        let rx = rx.0.lock().unwrap();
        match rx.as_ref() {
            None => true,
            Some(pending) if pending.is_empty() => false,
            Some(_) => return Err(rt_err("child event forwarding has not started")),
        }
    };
    if forwarding {
        let tx = lua.app_data_ref::<ChildEvents>().ok_or_else(|| rt_err("child event queue unavailable"))?.0.clone();
        let (done, wait) = tokio::sync::oneshot::channel();
        tx.send(ChildEvent::Barrier(done)).map_err(|_| rt_err("child event forwarding stopped"))?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::timeout(remaining, wait)
            .await
            .map_err(|_| rt_err(notify_timeout_msg))?
            .map_err(|_| rt_err("child event forwarding stopped"))?;
    }
    let mut st = state.lock().unwrap();
    st.pending_drains.retain(|s| s.active_count() > 0);
    st.active = root.session_scope();
    Ok(())
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    lua.set_app_data(AgentRegistry::default());
    lua.set_app_data(ChildEvents(tx));
    lua.set_app_data(ChildBindings::default());
    lua.set_app_data(ChildEventsRx(std::sync::Mutex::new(Some(rx))));
    let args_table = mare.get::<Option<Table>>("args")?;
    let tool_ceiling =
        args_table.as_ref().map(|args| args.get::<Option<Vec<String>>>("tool_ceiling")).transpose()?.flatten();
    let tool_discovery_prefixes = args_table
        .as_ref()
        .map(|args| args.get::<Option<Vec<String>>>("tool_discovery_prefixes"))
        .transpose()?
        .flatten()
        .unwrap_or_default()
        .into_iter()
        .filter(|prefix| !prefix.is_empty())
        .collect();
    let tool_ceiling_denied = args_table
        .as_ref()
        .map(|args| args.get::<Option<Vec<String>>>("tool_ceiling_denied"))
        .transpose()?
        .flatten()
        .unwrap_or_default()
        .into_iter()
        .collect();
    lua.set_app_data(ToolCeiling {
        names: tool_ceiling.map(|names| names.into_iter().collect()),
        prefixes: tool_discovery_prefixes,
        denied: tool_ceiling_denied,
    });
    let root = lua.app_data_ref::<crate::agent_tree::Scope>().ok_or_else(|| rt_err("agent tree unavailable"))?.clone();
    lua.set_app_data(SessionScope(std::sync::Arc::new(std::sync::Mutex::new(SessionScopeState {
        active: root.session_scope(),
        pending_drains: Vec::new(),
        draining: false,
    }))));
    let t = lua.create_table()?;
    t.set("start", mare_shared::lua_error::wrap_coded(lua, "@subagent/wrap", lua.create_function(start_raw)?)?)?;
    t.set(
        "tool_allowed",
        lua.create_function(|lua, name: String| {
            let ceiling = lua.app_data_ref::<ToolCeiling>().ok_or_else(|| rt_err("tool ceiling unavailable"))?;
            Ok(ceiling.allows(&name))
        })?,
    )?;
    t.set(
        "drain_session",
        lua.create_async_function(|lua, ()| async move {
            let timeout = lua
                .app_data_ref::<ParentHost>()
                .and_then(|p| p.0.upgrade())
                .map(|host| host.shutdown_grace())
                .unwrap_or(DEFAULT_DRAIN_TIMEOUT);
            drain_session_inner(
                &lua,
                "session_switch",
                timeout,
                "subagents did not stop before the session switch",
                "child notifications did not drain before the session switch",
            )
            .await
        })?,
    )?;
    t.set(
        "list",
        lua.create_function(|lua, ()| {
            let list = registry(lua)?.list();
            json_to_lua_lossless_bounded(lua, &serde_json::to_value(list).map_err(rt_err)?, HISTORY_LOSSLESS_MAX_NODES)
        })?,
    )?;
    t.set(
        "get",
        lua.create_function(|lua, id: Value| {
            let Value::String(id) = id else { return Err(rt_err("subagent_id must be a string")) };
            let id = id.to_str()?.to_owned();
            let item = registry(lua)?.get(&id);
            match item {
                Some(info) => json_to_lua_lossless_bounded(
                    lua,
                    &serde_json::to_value(info).map_err(rt_err)?,
                    HISTORY_LOSSLESS_MAX_NODES,
                ),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    t.set(
        "wait",
        lua.create_async_function(|lua, (id, opts): (String, Option<Table>)| async move {
            let reg = registry(&lua)?;
            let timeout_ms = match opts.as_ref().map(|o| o.get::<Value>("timeout_ms")).transpose()? {
                None | Some(Value::Nil) => None,
                Some(Value::Integer(ms)) if ms >= 0 => Some(ms as u64),
                Some(Value::Integer(_)) => return Err(rt_err("invalid timeout_ms: must be >= 0")),
                Some(_) => return Err(rt_err("invalid timeout_ms: expected integer")),
            };
            let cancel =
                opts.as_ref().and_then(|o| o.get::<Option<Value>>("cancel").ok().flatten()).and_then(|v| match v {
                    Value::UserData(ud) => ud.borrow::<mare_binding::task::Token>().ok().map(|t| t.0.clone()),
                    _ => None,
                });
            let res = reg.wait(&id, timeout_ms, cancel).await.map_err(rt_err)?;
            json_to_lua_lossless_bounded(&lua, &serde_json::to_value(res).map_err(rt_err)?, HISTORY_LOSSLESS_MAX_NODES)
        })?,
    )?;
    t.set(
        "steer",
        lua.create_function(|lua, (id, text): (String, String)| {
            registry(lua)?
                .steer_with_sync(&id, text, peer_caller_source(lua), |scope| {
                    parent_permission_sync(lua, scope).map(Some).map_err(|e| e.to_string())
                })
                .map_err(rt_err)
        })?,
    )?;
    t.set("cancel", lua.create_function(|lua, id: String| registry(lua)?.cancel(&id).map_err(rt_err))?)?;
    // Only bindings minted by this host are eligible to join a live row to a transcript.
    t.set(
        "identity",
        lua.create_function(|lua, id: Value| {
            let Value::String(id) = id else {
                return Err(rt_err("subagent identity id must be a string"));
            };
            let id = id.to_str()?.to_owned();
            let scope = current_session_scope(lua)?;
            let binding = lua.app_data_ref::<ChildBindings>().and_then(|b| {
                let guard = b.0.lock().ok()?;
                let binding = guard.by_subagent.get(&id).cloned()?;
                let (parent, session_epoch) = scope.as_ref()?;
                (binding.get("parent_session_id").and_then(serde_json::Value::as_str) == Some(parent.as_str())
                    && binding.get("parent_session_epoch").and_then(serde_json::Value::as_str)
                        == Some(session_epoch.as_str()))
                .then_some(binding)
            });
            match binding {
                Some(binding) => json_to_lua(lua, &binding),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    // what of a config value would not reach a sub-agent's VM (functions it can't carry)
    t.set("uncarried", lua.create_function(|lua, v: Value| mare_shared::carry::uncarried(lua, v))?)?;
    crate::approvals::proposed_rule::install(lua, &t)?;
    lua.set_named_registry_value(SUBAGENT_TABLE_KEY, t.clone())?;
    mare.set(mare_plugin::SUBAGENT_GLOBAL_NAME, t)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::ObjectLike;
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    fn assert_pending(future: std::pin::Pin<&mut impl Future>) {
        assert!(matches!(future.poll(&mut Context::from_waker(Waker::noop())), Poll::Pending));
    }

    async fn cancelled_wait_preserves_result(outcome: Option<Result<serde_json::Value, String>>) {
        let lua = Lua::new();
        let (tx, rx) = watch::channel(None);
        let (_events_tx, events) = mpsc::channel(1);
        let (steer, _steer_rx) = mpsc::channel(crate::agent_registry::STEER_QUEUE_CAPACITY);
        let cancel = CancellationToken::new();
        let handle = lua
            .create_userdata(AgentHandle {
                id: "child-1".into(),
                info: AgentInfo {
                    subagent_id: "child-1".into(),
                    name: None,
                    label: None,
                    run_id: None,
                    seq: None,
                    task: None,
                    parent_session_id: None,
                    status: AgentStatus::Running,
                    started_at_ms: 0,
                    ended_at_ms: None,
                    error: None,
                    error_kind: None,
                },
                events: AsyncMutex::new(events),
                result: rx,
                cancel: cancel.clone(),
                cancel_scope: None,
                inherits_parent_preset: false,
                steer,
            })
            .unwrap();
        let mut first = Box::pin(handle.call_async_method::<Value>("wait", ()));
        let mut second = Box::pin(handle.call_async_method::<Value>("wait", ()));
        assert_pending(first.as_mut());
        assert_pending(second.as_mut());
        drop(first);
        // Cancellation only drops this wait; the child and the second waiter remain live.
        assert!(!cancel.is_cancelled());
        assert_pending(second.as_mut());
        let expected = outcome.clone().unwrap_or_else(|| Err("agent task dropped".into()));
        match outcome {
            Some(result) => {
                tx.send(Some(result.map_err(ChildError::Failed))).unwrap();
            }
            None => drop(tx),
        }
        let result = second.await;
        let repeated = handle.call_async_method::<Value>("wait", ()).await;
        for actual in [result, repeated] {
            match &expected {
                Ok(value) => assert_eq!(lua_to_json(&lua, actual.unwrap()).unwrap(), *value),
                Err(error) => assert!(actual.unwrap_err().to_string().contains(error)),
            }
        }
    }

    #[tokio::test]
    async fn cancelled_agent_wait_preserves_completion() {
        cancelled_wait_preserves_result(Some(Ok(serde_json::json!({"text": "finished"})))).await;
    }

    #[tokio::test]
    async fn cancelled_agent_wait_preserves_error() {
        cancelled_wait_preserves_result(Some(Err("child failed".into()))).await;
    }

    #[tokio::test]
    async fn cancelled_agent_wait_preserves_sender_drop() {
        cancelled_wait_preserves_result(None).await;
    }

    #[tokio::test]
    async fn mare_subagent_large_history_identity_and_timeout_validation() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        lua.set_app_data(crate::agent_tree::Scope::root(&serde_json::json!({})).unwrap());
        install(&lua, &mare).unwrap();

        let reg = lua.app_data_ref::<AgentRegistry>().unwrap().clone();
        // Big child (> 10,000 JSON nodes across messages)
        let res = reg.reserve_name(Some("big-child")).unwrap();
        let (res_tx, res_rx) = watch::channel(None);
        let (steer, _steer_rx) = mpsc::channel(crate::agent_registry::STEER_QUEUE_CAPACITY);
        reg.insert(
            res,
            AgentEntry {
                info: AgentInfo {
                    subagent_id: "big-session-id".into(),
                    name: Some("big-child".into()),
                    label: None,
                    run_id: None,
                    seq: None,
                    task: None,
                    parent_session_id: None,
                    status: AgentStatus::Running,
                    started_at_ms: 0,
                    ended_at_ms: None,
                    error: None,
                    error_kind: None,
                },
                parent_session_epoch: None,
                result_rx: res_rx,
                cancel: CancellationToken::new(),
                cancel_scope: None,
                inherits_parent_preset: false,
                steer,
                collected: false,
            },
        );
        let big_msgs: Vec<serde_json::Value> = (0..2_500)
            .map(|i| serde_json::json!({ "role": "user", "content": [{ "type": "text", "text": i }] }))
            .collect();
        reg.mark_finished("big-session-id", AgentStatus::Completed, None, None);
        res_tx.send(Some(Ok(serde_json::json!({ "text": "big-ok", "messages": big_msgs, "usage": null })))).unwrap();

        // Bindings are only visible to their owning session, never by a numeric alias.
        {
            let bindings = lua.app_data_ref::<ChildBindings>().unwrap();
            let mut g = bindings.0.lock().unwrap();
            let b = serde_json::json!({
                "subagent_id": "big-session-id",
                "parent_session_id": "parent-1",
                "parent_session_epoch": "gen-1"
            });
            g.by_subagent.insert("big-session-id".into(), b);
        }

        // Running child for zero-timeout check
        let run_res = reg.reserve_name(Some("slow-child")).unwrap();
        let (_run_tx, run_rx) = watch::channel(None);
        let (run_steer, _run_steer_rx) = mpsc::channel(crate::agent_registry::STEER_QUEUE_CAPACITY);
        reg.insert(
            run_res,
            AgentEntry {
                info: AgentInfo {
                    subagent_id: "slow-session-id".into(),
                    name: Some("slow-child".into()),
                    label: None,
                    run_id: None,
                    seq: None,
                    task: None,
                    parent_session_id: None,
                    status: AgentStatus::Running,
                    started_at_ms: 0,
                    ended_at_ms: None,
                    error: None,
                    error_kind: None,
                },
                parent_session_epoch: None,
                result_rx: run_rx,
                cancel: CancellationToken::new(),
                cancel_scope: None,
                inherits_parent_preset: false,
                steer: run_steer,
                collected: false,
            },
        );

        let (msg_len, usage_nil, id_by_str, id_by_num, zero_to, neg_err): (usize, bool, bool, bool, bool, String) = lua
            .load(
                r#"
                local w = mare.subagent.wait("big-child")
                assert(w.text == "big-ok")
                local by_s = mare.subagent.identity("big-session-id")
                local numeric_ok = pcall(mare.subagent.identity, 7)
                local z = mare.subagent.wait("slow-child", { timeout_ms = 0 })
                local ok_neg, err_neg = pcall(mare.subagent.wait, "big-child", { timeout_ms = -1 })
                assert(not ok_neg)
                return #w.messages, w.usage == nil, by_s == nil, not numeric_ok, z.timed_out, tostring(err_neg)
                "#,
            )
            .eval_async()
            .await
            .unwrap();
        assert_eq!(msg_len, 2_500);
        assert!(usage_nil);
        assert!(id_by_str && id_by_num, "bindings cannot be inspected without an owning session");
        assert!(zero_to);
        assert!(neg_err.contains("timeout_ms"), "{neg_err}");
    }

    #[test]
    fn mare_subagent_start_emits_structured_error_codes_in_deterministic_order() {
        let setup = |config: serde_json::Value| {
            let lua = Lua::new();
            let mare = lua.create_table().unwrap();
            lua.globals().set("mare", mare.clone()).unwrap();
            lua.set_app_data(crate::agent_tree::Scope::root(&config).unwrap());
            install(&lua, &mare).unwrap();
            lua
        };
        let call_code = |lua: &Lua, expr: &str| -> (String, String) {
            lua.load(format!(
                r#"
                local ok, err = pcall(function() return {expr} end)
                assert(not ok, "expected refusal for: {expr}")
                assert(type(err) == "table", "expected structured mare.Error table, got: " .. type(err) .. " (" .. tostring(err) .. ")")
                return err.code, tostring(err)
                "#
            ))
            .eval()
            .unwrap()
        };

        // Surface exposes `start` and not `spawn`.
        let lua = setup(serde_json::json!({ "agents": { "max_active": 0 } }));
        let (has_start, has_spawn): (bool, bool) =
            lua.load("return type(mare.subagent.start) == 'function', mare.subagent.spawn ~= nil").eval().unwrap();
        assert!(has_start && !has_spawn);

        // Argument validation runs before capacity/tree checks and returns `invalid_arguments`.
        for expr in [
            "mare.subagent.start()",
            "mare.subagent.start('bad')",
            "mare.subagent.start({})",
            "mare.subagent.start({ prompt = 123 })",
            "mare.subagent.start({ prompt = 'hi', tools = 'read' })",
            "mare.subagent.start({ prompt = 'hi', tools = { 1 } })",
            "mare.subagent.start({ prompt = 'hi', name = 42 })",
            "mare.subagent.start({ prompt = 'hi', output_schema = { type = 123 } })",
        ] {
            let (code, text) = call_code(&lua, expr);
            assert_eq!(code, "invalid_arguments", "{expr}: {text}");
        }

        // Capacity limit returns `subagent.limit_reached`.
        let (cap_code, cap_text) = call_code(&lua, "mare.subagent.start({ prompt = 'over-cap' })");
        assert_eq!(cap_code, "subagent.limit_reached", "{cap_text}");

        // Depth limit returns `subagent.limit_reached`.
        let depth_lua = setup(serde_json::json!({ "agents": { "max_active": 4, "max_depth": 0 } }));
        let (depth_code, depth_text) = call_code(&depth_lua, "mare.subagent.start({ prompt = 'too-deep' })");
        assert_eq!(depth_code, "subagent.limit_reached", "{depth_text}");

        // Duplicate name returns `subagent.duplicate_name` and takes precedence over capacity limits.
        let dup_lua = setup(serde_json::json!({ "agents": { "max_active": 4 } }));
        let reg = dup_lua.app_data_ref::<AgentRegistry>().unwrap().clone();
        let _held = reg.reserve_name(Some("worker")).unwrap();
        let (dup_code, dup_text) = call_code(&dup_lua, "mare.subagent.start({ prompt = 'hi', name = 'worker' })");
        assert_eq!(dup_code, "subagent.duplicate_name", "{dup_text}");

        let dup_over_cap = setup(serde_json::json!({ "agents": { "max_active": 0 } }));
        let cap_reg = dup_over_cap.app_data_ref::<AgentRegistry>().unwrap().clone();
        let _held_cap = cap_reg.reserve_name(Some("worker")).unwrap();
        let (dup_cap_code, dup_cap_text) =
            call_code(&dup_over_cap, "mare.subagent.start({ prompt = 'hi', name = 'worker' })");
        assert_eq!(dup_cap_code, "subagent.duplicate_name", "{dup_cap_text}");

        // Closed/draining tree returns `subagent.tree_closed`, while invalid arguments still win first.
        let closed_lua = setup(serde_json::json!({ "agents": { "max_active": 4 } }));
        {
            let scope = closed_lua.app_data_ref::<SessionScope>().unwrap();
            scope.0.lock().unwrap().draining = true;
        }
        let (drain_code, drain_text) = call_code(&closed_lua, "mare.subagent.start({ prompt = 'while-draining' })");
        assert_eq!(drain_code, "subagent.tree_closed", "{drain_text}");
        let (inv_first, inv_text) = call_code(&closed_lua, "mare.subagent.start({})");
        assert_eq!(inv_first, "invalid_arguments", "{inv_text}");

        let cancelled_lua = setup(serde_json::json!({ "agents": { "max_active": 4 } }));
        {
            let scope = cancelled_lua.app_data_ref::<SessionScope>().unwrap();
            scope.0.lock().unwrap().active.cancel_as("host");
        }
        let (cancel_code, cancel_text) = call_code(&cancelled_lua, "mare.subagent.start({ prompt = 'after-cancel' })");
        assert_eq!(cancel_code, "subagent.tree_closed", "{cancel_text}");
    }

    #[test]
    fn child_binding_map_preserves_running_entries_and_evicts_finished_bindings() {
        let mut map = ChildBindingMap::default();

        // A long-running child must survive even when >MAX_CHILD_BINDINGS short-lived children finish.
        map.insert_for_parent("running-0".into(), "session-1", "epoch-1", |_| true);
        for i in 0..(MAX_CHILD_BINDINGS + 12) {
            let id = format!("finished-{i}");
            map.insert_for_parent(id.clone(), "session-1", "epoch-1", |_| true);
            map.mark_finished(&id, |_| true);
        }
        assert!(map.by_subagent.contains_key("running-0"), "running child binding must never be evicted by FIFO cap");
        assert_eq!(map.by_subagent.len(), MAX_CHILD_BINDINGS);
        assert!(!map.by_subagent.contains_key("finished-0"), "oldest finished binding must be evicted by fallback cap");
        assert!(map.by_subagent.contains_key(&format!("finished-{}", MAX_CHILD_BINDINGS + 11)));

        // Finished bindings pruned from AgentRegistry are cleaned up immediately on finish.
        let latest = format!("finished-{}", MAX_CHILD_BINDINGS + 11);
        map.insert_for_parent("short-child".into(), "session-1", "epoch-1", |id| id == latest);
        map.mark_finished("short-child", |id| id == "short-child");
        assert!(map.by_subagent.contains_key("running-0"), "running child still preserved");
        assert!(map.by_subagent.contains_key("short-child"), "retained finished child still queryable");
        assert!(!map.by_subagent.contains_key(&latest), "unretained finished child cleaned up on completion");

        // Switching to a new parent session/epoch drops prior session bindings immediately.
        map.insert_for_parent("child-new".into(), "session-2", "epoch-2", |_| true);
        assert_eq!(map.by_subagent.len(), 1);
        assert!(map.by_subagent.contains_key("child-new"));
    }

    #[tokio::test]
    async fn drain_session_tracks_timed_out_scope_until_reservation_drops() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        lua.globals().set("mare", mare.clone()).unwrap();
        lua.set_app_data(
            crate::agent_tree::Scope::root(&serde_json::json!({ "agents": { "max_active": 4 } })).unwrap(),
        );
        install(&lua, &mare).unwrap();

        let shared = lua.app_data_ref::<SessionScope>().unwrap().0.clone();
        let (_child_scope, stubborn_res) = {
            let st = shared.lock().unwrap();
            st.active.reserve().unwrap()
        };

        // First drain cancels the scope and times out, rotating `active` while retaining the old scope in `pending_drains`.
        let err1 = drain_session_as(&lua, "session_switch", std::time::Duration::ZERO)
            .await
            .expect_err("first drain must time out while stubborn reservation is held");
        assert!(err1.to_string().contains("did not stop"), "{err1}");

        // Current session can still spawn into the fresh rotated active scope.
        let (fresh_child, fresh_res) = {
            let st = shared.lock().unwrap();
            assert_eq!(st.pending_drains.len(), 1, "timed-out scope must be retained in pending_drains");
            st.active.reserve().unwrap()
        };

        // Keep `fresh_res` alive while draining with zero timeout: even though the older stubborn
        // scope in `pending_drains` times out, concurrent drain must STILL cancel the fresh active scope.
        let err2 = drain_session_as(&lua, "session_switch", std::time::Duration::ZERO)
            .await
            .expect_err("second drain must still fail while timed-out scope has a live reservation");
        assert!(err2.to_string().contains("did not stop"), "{err2}");
        assert!(
            fresh_child.cancel.is_cancelled(),
            "concurrent drain must cancel newer active scope even when an older pending scope times out"
        );
        drop(fresh_res);

        // Once the stubborn reservation drops, draining succeeds and prunes `pending_drains`.
        drop(stubborn_res);
        drain_session_as(&lua, "session_switch", DEFAULT_DRAIN_TIMEOUT)
            .await
            .expect("drain must succeed after stubborn reservation drops");
        assert!(shared.lock().unwrap().pending_drains.is_empty(), "drained scopes must be pruned");
    }
}
