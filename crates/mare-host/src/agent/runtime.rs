use std::path::PathBuf;

use mlua::{Function, Lua, Table};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::approvals::bridge_approvals;
use super::{ChildEvent, ParentHost};
use crate::agent_registry::{AgentRegistry, AgentStatus};
use crate::child_transcript::Transcript;
use crate::{Host, HostOptions};
use mare_kernel::TapEvent;
use mare_shared::lua::rt_err;

/// The session that owns this child at spawn time, even if the parent later switches.
#[derive(Clone)]
pub(super) struct ParentSession {
    pub(super) id: String,
    pub(super) session_epoch: String,
    pub(super) children_dir: Option<String>,
}

pub(super) fn current_session_scope(lua: &Lua) -> mlua::Result<Option<(String, String)>> {
    let Some(host) = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade()) else { return Ok(None) };
    let Some(info) = host.session_info()? else { return Ok(None) };
    let (_, session_epoch) = host.kernel.event_log().session_identity();
    Ok(Some((info.get("session_id")?, session_epoch)))
}

pub(super) fn parent_session_identity(lua: &Lua) -> mlua::Result<Option<ParentSession>> {
    let Some(host) = lua.app_data_ref::<ParentHost>().and_then(|p| p.0.upgrade()) else { return Ok(None) };
    let Some(svc) = host.service_table(mare_shared::SESSION_SERVICE_NAME) else { return Ok(None) };
    let Some(info) = host.session_info()? else { return Ok(None) };
    let disabled = info.get::<Option<bool>>("ephemeral")?.unwrap_or(false)
        || info.get::<Option<bool>>("readonly")?.unwrap_or(false);
    let children_dir = if disabled { None } else { info.get::<Option<String>>("children_dir")? };
    if children_dir.is_some() && info.get::<Option<String>>("path")?.is_none() {
        if mare_binding::exec_ctx::active(lua)?.is_some() {
            return Err(rt_err("parent session must be saved before starting a recorded child from a tool"));
        }
        let (ok, reason): (Option<bool>, Option<String>) =
            svc.get::<Function>("prepare_children")?
                .call(svc.clone())
                .map_err(|e| rt_err(format!("parent session must be saved before starting a recorded child: {e}")))?;
        if ok != Some(true) {
            return Err(rt_err(format!(
                "parent session must be saved before starting a recorded child: {}",
                reason.unwrap_or_else(|| "session writer unavailable".into())
            )));
        }
    }
    let (_, session_epoch) = host.kernel.event_log().session_identity();
    Ok(Some(ParentSession { id: info.get("session_id")?, session_epoch, children_dir }))
}

#[derive(Clone)]
pub(super) struct ChildEndMeta {
    pub(super) child_id: u64,
    pub(super) session_id: String,
    pub(super) agent_id: String,
    pub(super) name: Option<String>,
    pub(super) label: Option<String>,
    pub(super) run_id: Option<String>,
    pub(super) seq: Option<u64>,
    pub(super) parent_session: Option<(String, String)>,
}

pub(super) struct ChildRuntimeArgs {
    pub(super) prompt: String,
    pub(super) cwd: PathBuf,
    pub(super) spec: serde_json::Value,
    pub(super) config: serde_json::Value,
    pub(super) history_model: Option<String>,
    pub(super) parent_policy: Option<mare_binding::ossandbox::Policy>,
    pub(super) carried: mare_shared::carry::Carried,
    pub(super) scope: crate::agent_tree::Scope,
    pub(super) reservation: crate::agent_tree::Reservation,
    pub(super) parent_net_approver: Option<mare_binding::netauth::NetApprover>,
    pub(super) parent_snapshot: Option<mare_binding::snapshot::SnapshotJournal>,
    pub(super) transcript: Option<Transcript>,
    pub(super) origin: serde_json::Value,
    pub(super) shares_rules: bool,
    pub(super) cancel: CancellationToken,
    pub(super) ev_tx: mpsc::Sender<TapEvent>,
    pub(super) steer_rx: mpsc::Receiver<(String, Option<String>)>,
    pub(super) res_tx: watch::Sender<Option<Result<serde_json::Value, super::ChildError>>>,
    pub(super) lifecycle: Option<mpsc::UnboundedSender<ChildEvent>>,
    pub(super) registry: AgentRegistry,
    pub(super) bindings: Option<super::ChildBindings>,
    pub(super) meta: ChildEndMeta,
}

struct ChildEndOutcome {
    ok: bool,
    status: AgentStatus,
    status_str: &'static str,
    text: Option<String>,
    error: Option<mare_protocol::events::EventError>,
    error_kind: Option<String>,
    usage: Option<serde_json::Value>,
}

impl ChildEndOutcome {
    fn failed(msg: String) -> Self {
        Self {
            ok: false,
            status: AgentStatus::Failed,
            status_str: "failed",
            text: None,
            error: Some(mare_protocol::events::EventError { code: "error".into(), message: msg, data: None }),
            error_kind: Some("error".into()),
            usage: None,
        }
    }
}

fn emit_child_end(
    registry: &AgentRegistry,
    bindings: Option<&super::ChildBindings>,
    lifecycle: Option<&mpsc::UnboundedSender<ChildEvent>>,
    meta: ChildEndMeta,
    outcome: ChildEndOutcome,
) {
    registry.mark_finished(&meta.session_id, outcome.status, outcome.error.clone(), outcome.error_kind.clone());
    if let Some(bindings) = bindings {
        bindings.mark_finished(&meta.session_id, registry);
    }
    if let Some(tx) = lifecycle {
        let _ = tx.send(ChildEvent::End(Box::new(crate::approvals::child_events::ChildEnd {
            subagent_id: meta.session_id,
            agent_id: meta.agent_id,
            name: meta.name,
            label: meta.label,
            ok: outcome.ok,
            status: outcome.status_str.into(),
            run_id: meta.run_id,
            seq: meta.seq,
            parent_session: meta.parent_session,
            text: outcome.text,
            error: outcome.error,
            error_kind: outcome.error_kind,
            usage: outcome.usage,
        })));
    }
}

/// Drop guard guaranteeing `registry.mark_finished`, transcript closure, and `ChildEvent::End`
/// even if the child agent thread unwinds on a panic before reaching normal completion.
struct ChildEndGuard {
    registry: AgentRegistry,
    bindings: Option<super::ChildBindings>,
    lifecycle: Option<mpsc::UnboundedSender<ChildEvent>>,
    meta: Option<ChildEndMeta>,
    transcript: Option<std::rc::Rc<std::cell::RefCell<Transcript>>>,
}

impl ChildEndGuard {
    fn new(
        registry: AgentRegistry,
        bindings: Option<super::ChildBindings>,
        lifecycle: Option<mpsc::UnboundedSender<ChildEvent>>,
        meta: ChildEndMeta,
    ) -> Self {
        Self { registry, bindings, lifecycle, meta: Some(meta), transcript: None }
    }

    fn finish(mut self, outcome: ChildEndOutcome) {
        if let Some(meta) = self.meta.take() {
            self.transcript = None;
            emit_child_end(&self.registry, self.bindings.as_ref(), self.lifecycle.as_ref(), meta, outcome);
        }
    }
}

impl Drop for ChildEndGuard {
    fn drop(&mut self) {
        if let Some(meta) = self.meta.take() {
            if let Some(transcript) = self.transcript.take()
                && let Ok(mut guard) = transcript.try_borrow_mut()
            {
                let _ = guard.close(false);
            }
            emit_child_end(
                &self.registry,
                self.bindings.as_ref(),
                self.lifecycle.as_ref(),
                meta,
                ChildEndOutcome::failed("child agent thread panicked".into()),
            );
        }
    }
}

pub(super) fn spawn_child_thread(args: ChildRuntimeArgs) -> mlua::Result<()> {
    let on_spawn_error = args.lifecycle.clone();
    let failed_registry = args.registry.clone();
    let failed_bindings = args.bindings.clone();
    let failed_meta = args.meta.clone();
    std::thread::Builder::new().name("mare-agent".into()).spawn(move || run_child_thread(args)).map_err(|e| {
        emit_child_end(
            &failed_registry,
            failed_bindings.as_ref(),
            on_spawn_error.as_ref(),
            failed_meta,
            ChildEndOutcome::failed(e.to_string()),
        );
        rt_err(e)
    })?;
    Ok(())
}

fn run_child_thread(mut args: ChildRuntimeArgs) {
    let _reservation = args.reservation;
    let child_id = args.meta.child_id;
    let session_id = args.meta.session_id.clone();
    let parent_session = args.meta.parent_session.clone();
    let mut end_guard = ChildEndGuard::new(args.registry, args.bindings, args.lifecycle.clone(), args.meta);
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let msg = e.to_string();
            if let Some(mut transcript) = args.transcript.take() {
                let _ = transcript.close(false);
            }
            end_guard.finish(ChildEndOutcome::failed(msg.clone()));
            let _ = args.res_tx.send(Some(Err(super::ChildError::Failed(msg))));
            return;
        }
    };
    let local = tokio::task::LocalSet::new();
    let cleanup_scope = args.scope.clone();
    let transcript = args.transcript.map(|t| std::rc::Rc::new(std::cell::RefCell::new(t)));
    end_guard.transcript = transcript.clone();
    let cleanup_transcript = transcript.clone();
    let mut res = local.block_on(
        &rt,
        run_child_host(ChildHostArgs {
            prompt: args.prompt,
            cwd: args.cwd,
            spec: args.spec,
            config: args.config,
            history_model: args.history_model,
            parent_policy: args.parent_policy,
            carried: args.carried,
            scope: args.scope,
            parent_net_approver: args.parent_net_approver,
            parent_snapshot: args.parent_snapshot,
            transcript,
            origin: args.origin,
            shares_rules: args.shares_rules,
            cancel: args.cancel,
            ev_tx: args.ev_tx,
            steer_rx: args.steer_rx,
            approvals: args.lifecycle,
            child_id,
            session_id,
            parent_session,
        }),
    );
    let was_cancelled = matches!(res, Err(super::ChildError::Cancelled))
        || res.as_ref().ok().is_some_and(|value| {
            value.get("termination_reason").and_then(serde_json::Value::as_str) == Some("interrupted")
                || value.get("error_kind").and_then(serde_json::Value::as_str) == Some("aborted")
        });
    local.block_on(&rt, cleanup_scope.drain_as("parent"));
    if let Some(t) = cleanup_transcript {
        end_guard.transcript = None;
        let ok = res.as_ref().is_ok_and(|v| v.get("error").is_none_or(serde_json::Value::is_null));
        if let Err(e) = t.borrow_mut().close(ok)
            && res.is_ok()
        {
            res = Err(super::ChildError::Failed(format!("closing child transcript: {e:#}")));
        }
    }
    let ok = res.as_ref().is_ok_and(|v| v.get("error").is_none_or(serde_json::Value::is_null));
    let (error, error_kind) = match &res {
        Ok(v) => {
            let kind = v.get("error_kind").and_then(|e| e.as_str()).map(str::to_string);
            let err = crate::agent_registry::parse_event_error(v, kind.as_deref());
            (err, kind)
        }
        Err(e) => {
            let is_cancel = matches!(e, super::ChildError::Cancelled);
            let code = if is_cancel { "cancelled" } else { "error" };
            let kind = if is_cancel { "aborted" } else { "error" };
            (
                Some(mare_protocol::events::EventError { code: code.into(), message: e.to_string(), data: None }),
                Some(kind.into()),
            )
        }
    };
    let (status, status_str) = if ok {
        (AgentStatus::Completed, "completed")
    } else if was_cancelled || matches!(res, Err(super::ChildError::Cancelled)) {
        (AgentStatus::Cancelled, "cancelled")
    } else {
        (AgentStatus::Failed, "failed")
    };
    let text = res.as_ref().ok().and_then(|v| v.get("text").and_then(|t| t.as_str())).map(str::to_string);
    let usage = res.as_ref().ok().and_then(|v| v.get(mare_shared::USAGE_KEY).cloned());
    end_guard.finish(ChildEndOutcome { ok, status, status_str, text, error, error_kind, usage });
    let _ = args.res_tx.send(Some(res));
}

struct ChildHostArgs {
    prompt: String,
    cwd: PathBuf,
    spec: serde_json::Value,
    config: serde_json::Value,
    history_model: Option<String>,
    parent_policy: Option<mare_binding::ossandbox::Policy>,
    carried: mare_shared::carry::Carried,
    scope: crate::agent_tree::Scope,
    parent_net_approver: Option<mare_binding::netauth::NetApprover>,
    parent_snapshot: Option<mare_binding::snapshot::SnapshotJournal>,
    transcript: Option<std::rc::Rc<std::cell::RefCell<Transcript>>>,
    origin: serde_json::Value,
    shares_rules: bool,
    cancel: CancellationToken,
    ev_tx: mpsc::Sender<TapEvent>,
    steer_rx: mpsc::Receiver<(String, Option<String>)>,
    approvals: Option<mpsc::UnboundedSender<ChildEvent>>,
    child_id: u64,
    session_id: String,
    parent_session: Option<(String, String)>,
}

async fn run_child_host(mut args: ChildHostArgs) -> Result<serde_json::Value, super::ChildError> {
    if args.cancel.is_cancelled() {
        return Err(super::ChildError::Cancelled);
    }
    let mut child_args = serde_json::Map::new();
    for key in [
        "agent_label",
        "persona",
        "sections",
        "system",
        "params",
        "permission_ceiling",
        "approval_policy_ceilings",
        "approval_review_required",
        "required_services",
    ] {
        if let Some(value) = args.spec.get(key) {
            child_args.insert(key.into(), value.clone());
        }
    }
    if let Some(tools) = args.spec.get(crate::frontend::TOOLS).filter(|v| !v.is_null()) {
        child_args.insert("tool_ceiling".into(), tools.clone());
    }
    child_args.insert("no_session".into(), serde_json::Value::Bool(args.transcript.is_none()));
    if let Some(transcript) = &args.transcript {
        child_args
            .insert("recorded_session_path".into(), transcript.borrow().path().to_string_lossy().to_string().into());
    }
    let cancel_scope = args.scope.clone();
    let progress_id = args.session_id.clone();
    let child_snapshot = args.parent_snapshot.clone();
    let startup = Host::new_child(
        HostOptions {
            cwd: args.cwd,
            mode: mare_shared::ui::Mode::Child,
            config: args.config,
            load_init: false,
            project_trusted: false,
            args: serde_json::Value::Object(child_args),
            no_user_plugins: false,
        },
        args.parent_policy,
        args.carried,
        args.scope,
        args.parent_net_approver,
        args.parent_snapshot,
        args.session_id,
    );
    let host = tokio::select! {
        biased;
        _ = args.cancel.cancelled() => return Err(super::ChildError::Cancelled),
        host = startup => host.map_err(|e| super::ChildError::Failed(e.to_string()))?,
    };
    if let Some(t) = &args.transcript {
        crate::child_transcript::install(&host, t.clone()).map_err(|e| super::ChildError::Failed(e.to_string()))?;
    }
    // the child's identity (`mare.agent_id`): background jobs it starts belong to it,
    // not to the main agent
    if let Ok(m) = host.lua().globals().get::<Table>("mare") {
        let _ = m.raw_set("agent_id", format!("agent-{}", args.child_id));
    }
    let progress_tx = args.approvals.clone();
    let progress_parent = args.parent_session.clone();
    // approvals bubble up to the parent
    if let Some(tx) = args.approvals {
        // without the bridge nobody can answer: approvals stay `unavailable` (denied)
        if let Err(e) = bridge_approvals(&host, tx, args.origin, args.shares_rules) {
            tracing::warn!("sub-agent approvals: {e}");
        }
    }
    if let Some(snapshot) = child_snapshot
        && let (Ok(ctx), Ok(owner)) = (host.kernel.host_ctx(), host.kernel.host_owner())
        && let Ok(cb) = host.lua().create_function(move |_, ev: mlua::Value| {
            if let mlua::Value::Table(t) = ev
                && let Ok(call) = t.get::<Table>("call")
                && let Ok(name) = call.get::<String>("name")
                && matches!(name.as_str(), "bash" | "powershell" | "shell")
            {
                snapshot.mark_shell();
            }
            Ok(())
        })
    {
        let _ = host.kernel.host_on(&ctx, owner, "pipeline/tool-dispatching", cb, mare_kernel::HostOn::default());
    }
    let ev_cursor = host.kernel.open_cursor(std::sync::Arc::new(|_| Some(true)));
    let ev_tx = args.ev_tx;
    let ev_task =
        tokio::task::spawn_local(forward_child_events(ev_cursor, ev_tx, progress_tx, progress_id, progress_parent));
    let h2 = host.clone();
    let steer_task = tokio::task::spawn_local(async move {
        while let Some((s, source)) = args.steer_rx.recv().await {
            if let Some(src) = source {
                let _ = h2.steer_with(&s, serde_json::json!({ "source": src })).await;
            } else {
                let _ = h2.steer(&s).await;
            }
        }
    });
    let mut opts = serde_json::Map::new();
    for key in ["messages", "system", mare_shared::TOOLS_KEY, "output_schema", "source"] {
        if let Some(v) = args.spec.get(key).filter(|v| !v.is_null()) {
            opts.insert(key.to_string(), v.clone());
        }
    }
    if let Some(hm) = args.history_model {
        opts.insert("history_model".to_string(), serde_json::Value::String(hm));
    }
    if args.spec.get("_fork_origin").is_some()
        && let Some(effort) = args.spec.get("_history_effort").and_then(serde_json::Value::as_str)
    {
        opts.insert("history_effort".to_string(), serde_json::Value::String(effort.to_string()));
    }
    let opts = serde_json::Value::Object(opts);
    let prompt = host.prompt(&args.prompt, opts);
    tokio::pin!(prompt);
    let r = tokio::select! {
        biased;
        _ = args.cancel.cancelled() => {
            let _ = host.abort_with_cause(&cancel_scope.cancel_reason()).await;
            // Keep polling the in-flight prompt until it has persisted its interruption
            // and run completion listeners before closing the child host/transcript.
            let _ = tokio::time::timeout(host.shutdown_grace(), &mut prompt).await;
            Err(super::ChildError::Cancelled)
        },
        r = &mut prompt => r.map_err(|e| super::ChildError::Failed(e.to_string())),
    };
    steer_task.abort();
    let persist_error = host
        .service_table(mare_shared::AGENT_SERVICE_NAME)
        .and_then(|svc| svc.get::<Function>("persist_error").ok()?.call::<Option<String>>(svc.clone()).ok().flatten());
    host.shutdown().await;
    tokio::task::yield_now().await;
    ev_task.abort();
    if let Some(error) = persist_error {
        return Err(super::ChildError::Failed(format!("child transcript persistence failed: {error}")));
    }
    r
}

async fn forward_child_events(
    mut ev_cursor: mare_kernel::EventCursor,
    ev_tx: mpsc::Sender<TapEvent>,
    progress_tx: Option<mpsc::UnboundedSender<ChildEvent>>,
    progress_id: String,
    progress_parent: Option<(String, String)>,
) {
    let mut ev_open = true;
    while let Some(rec) = ev_cursor.recv().await {
        match rec {
            mare_kernel::CursorRecord::Event { event, .. } => {
                if event.name == "status/changed"
                    && let Some(tx) = &progress_tx
                {
                    let _ = tx.send(ChildEvent::Progress {
                        subagent_id: progress_id.clone(),
                        parent_session: progress_parent.clone(),
                        progress: event.payload.clone(),
                    });
                }
                if ev_open {
                    match ev_tx.try_send(event) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(dropped)) => {
                            tracing::warn!(
                                subagent_id = %progress_id,
                                event = %dropped.name,
                                "sub-agent handle event channel full; dropping event"
                            );
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            ev_open = false;
                        }
                    }
                }
                if !ev_open && progress_tx.as_ref().is_none_or(mpsc::UnboundedSender::is_closed) {
                    break;
                }
            }
            mare_kernel::CursorRecord::Lagged { from_seq, to_seq } => {
                tracing::warn!(from_seq, to_seq, "sub-agent event cursor lagged");
            }
            mare_kernel::CursorRecord::Dropped { count, .. } => {
                tracing::warn!(count, "sub-agent event cursor dropped events");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_end_outcome_failed_populates_error_fields() {
        let out = ChildEndOutcome::failed("child crashed".into());
        assert!(!out.ok);
        assert_eq!(out.status, AgentStatus::Failed);
        assert_eq!(out.status_str, "failed");
        assert_eq!(out.error.as_ref().map(|e| e.message.as_str()), Some("child crashed"));
        assert_eq!(out.error.as_ref().map(|e| e.code.as_str()), Some("error"));
        assert_eq!(out.error_kind.as_deref(), Some("error"));
    }

    #[test]
    fn child_end_guard_marks_registry_failed_and_emits_end_on_panic() {
        let registry = AgentRegistry::default();
        let (res_tx, res_rx) = watch::channel(None);
        let (steer_tx, _steer_rx) = mpsc::channel(1);
        let resv = registry.reserve_name(Some("worker-panic")).unwrap();
        registry.insert(
            resv,
            crate::agent_registry::AgentEntry {
                info: crate::agent_registry::AgentInfo {
                    subagent_id: "child-panic-1".into(),
                    name: Some("worker-panic".into()),
                    label: Some("worker".into()),
                    run_id: Some("run-1".into()),
                    seq: Some(1),
                    task: Some("test panic unwind".into()),
                    parent_session_id: Some("parent-1".into()),
                    status: AgentStatus::Running,
                    started_at_ms: 1,
                    ended_at_ms: None,
                    error: None,
                    error_kind: None,
                },
                parent_session_epoch: Some("ep-1".into()),
                result_rx: res_rx,
                cancel: CancellationToken::new(),
                cancel_scope: None,
                steer: steer_tx,
                collected: false,
            },
        );
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let meta = ChildEndMeta {
            child_id: 7,
            session_id: "child-panic-1".into(),
            agent_id: "agent-7".into(),
            name: Some("worker-panic".into()),
            label: Some("worker".into()),
            run_id: Some("run-1".into()),
            seq: Some(1),
            parent_session: Some(("parent-1".into(), "ep-1".into())),
        };
        let guard = ChildEndGuard::new(registry.clone(), None, Some(ev_tx), meta);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            let _tx = res_tx;
            panic!("simulated child thread panic");
        }));
        assert!(panicked.is_err());

        let entries = registry.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].status, AgentStatus::Failed, "registry must not stay Running after child panic");
        assert!(
            entries[0].error.as_ref().is_some_and(|e| e.message.contains("panicked")),
            "expected panic error recorded on registry entry: {:?}",
            entries[0].error
        );
        match ev_rx.try_recv().expect("expected ChildEvent::End emitted on panic unwind") {
            ChildEvent::End(end) => {
                assert_eq!(end.subagent_id, "child-panic-1");
                assert!(!end.ok);
                assert_eq!(end.status, "failed");
                assert!(end.error.as_ref().is_some_and(|e| e.message.contains("panicked")));
            }
            _ => panic!("expected ChildEvent::End"),
        }
        assert!(ev_rx.try_recv().is_err(), "panic unwind must emit ChildEvent::End exactly once");
    }

    #[test]
    fn child_end_guard_finish_emits_once_and_disarms_drop() {
        let registry = AgentRegistry::default();
        let (_res_tx, res_rx) = watch::channel(None);
        let (steer_tx, _steer_rx) = mpsc::channel(1);
        let resv = registry.reserve_name(None).unwrap();
        registry.insert(
            resv,
            crate::agent_registry::AgentEntry {
                info: crate::agent_registry::AgentInfo {
                    subagent_id: "child-ok-1".into(),
                    name: None,
                    label: None,
                    run_id: None,
                    seq: None,
                    task: None,
                    parent_session_id: None,
                    status: AgentStatus::Running,
                    started_at_ms: 1,
                    ended_at_ms: None,
                    error: None,
                    error_kind: None,
                },
                parent_session_epoch: None,
                result_rx: res_rx,
                cancel: CancellationToken::new(),
                cancel_scope: None,
                steer: steer_tx,
                collected: false,
            },
        );
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let meta = ChildEndMeta {
            child_id: 8,
            session_id: "child-ok-1".into(),
            agent_id: "agent-8".into(),
            name: None,
            label: None,
            run_id: None,
            seq: None,
            parent_session: None,
        };
        let guard = ChildEndGuard::new(registry.clone(), None, Some(ev_tx), meta);
        guard.finish(ChildEndOutcome {
            ok: true,
            status: AgentStatus::Completed,
            status_str: "completed",
            text: Some("done".into()),
            error: None,
            error_kind: None,
            usage: None,
        });

        let entries = registry.list();
        assert_eq!(entries[0].status, AgentStatus::Completed);
        assert!(matches!(ev_rx.try_recv().ok(), Some(ChildEvent::End(end)) if end.ok && end.status == "completed"));
        assert!(ev_rx.try_recv().is_err(), "disarmed guard must not emit a duplicate ChildEvent::End");
    }

    #[tokio::test]
    async fn forward_child_events_delivers_progress_when_handle_channel_is_full_or_closed() {
        tokio::task::LocalSet::new()
            .run_until(async {
                // Case 1: bounded ev_tx is full (capacity 1, never drained) — progress must still flow.
                let k1 = mare_kernel::Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let cursor1 = k1.open_cursor(std::sync::Arc::new(|_| Some(true)));
                let (ev_tx1, _ev_rx1) = mpsc::channel(1);
                let (prog_tx1, mut prog_rx1) = mpsc::unbounded_channel();
                let task1 = tokio::task::spawn_local(forward_child_events(
                    cursor1,
                    ev_tx1,
                    Some(prog_tx1),
                    "child-full".into(),
                    Some(("p1".into(), "ep1".into())),
                ));
                for i in 0..4 {
                    k1.emit_json("step/started", serde_json::json!({ "n": i })).await;
                    k1.emit_json("status/changed", serde_json::json!({ "phase": "running", "n": i })).await;
                }
                let mut seen_full = 0;
                for _ in 0..4 {
                    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), prog_rx1.recv())
                        .await
                        .expect("progress must not stall when ev_tx is full")
                        .expect("progress channel must stay open");
                    if matches!(ev, ChildEvent::Progress { .. }) {
                        seen_full += 1;
                    }
                }
                assert_eq!(seen_full, 4);
                task1.abort();

                // Case 2: ev_rx is dropped (Closed) — progress must still flow.
                let k2 = mare_kernel::Kernel::new(Lua::new(), serde_json::json!({})).unwrap();
                let cursor2 = k2.open_cursor(std::sync::Arc::new(|_| Some(true)));
                let (ev_tx2, ev_rx2) = mpsc::channel(1);
                drop(ev_rx2);
                let (prog_tx2, mut prog_rx2) = mpsc::unbounded_channel();
                let task2 = tokio::task::spawn_local(forward_child_events(
                    cursor2,
                    ev_tx2,
                    Some(prog_tx2),
                    "child-closed".into(),
                    Some(("p1".into(), "ep1".into())),
                ));
                k2.emit_json("step/started", serde_json::json!({ "n": 0 })).await;
                k2.emit_json("status/changed", serde_json::json!({ "phase": "running", "n": 1 })).await;
                k2.emit_json("status/changed", serde_json::json!({ "phase": "idle", "n": 2 })).await;
                let mut seen_closed = 0;
                for _ in 0..2 {
                    let ev = tokio::time::timeout(std::time::Duration::from_secs(2), prog_rx2.recv())
                        .await
                        .expect("progress must continue after ev_rx is dropped")
                        .expect("progress channel must stay open");
                    if matches!(ev, ChildEvent::Progress { .. }) {
                        seen_closed += 1;
                    }
                }
                assert_eq!(seen_closed, 2);
                task2.abort();
            })
            .await;
    }
}
