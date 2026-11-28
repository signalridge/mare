//! Per-session and per-turn plumbing shared by the ACP protocol thread and the session actors:
//! turn cancellation, persistent session update forwarding, queued-prompt waiting, and the
//! close-until-confirmed state.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::Client;
use agent_client_protocol::v1::ConnectionTo;
use mare_host::Host;
use mare_shared::wire_projection::WireProjector;
use serde_json::{Value as J, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::rpc::call_service;

use super::AcpErr;
use super::helpers::{config_options, has_truncated_image, hydrate_tool_result_update, to_update};

/// How long a finished turn may take to hand its queued updates to the client.
pub(super) const PROMPT_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const FORWARDER_CMD_CAPACITY: usize = 64;

enum ForwarderCmd {
    BeginTurn(oneshot::Sender<()>),
    Flush(oneshot::Sender<Result<(), String>>),
}

pub(super) enum SinkOutcome {
    Sent,
    Failed(String),
    #[cfg(test)]
    Disconnected,
}

enum RecordStep {
    Continue,
    #[cfg(test)]
    Disconnected,
}

trait ConfigWatcher {
    fn is_active(&self) -> bool;
    async fn changed(&mut self) -> bool;
    async fn flush(&mut self, force: bool) -> Option<J>;
    async fn translate_event(&mut self, name: &str, payload: &J) -> Option<J>;
}

#[cfg(test)]
struct NoConfigWatcher;

#[cfg(test)]
impl ConfigWatcher for NoConfigWatcher {
    fn is_active(&self) -> bool {
        false
    }

    async fn changed(&mut self) -> bool {
        std::future::pending().await
    }

    async fn flush(&mut self, _force: bool) -> Option<J> {
        None
    }

    async fn translate_event(&mut self, name: &str, payload: &J) -> Option<J> {
        to_update(name, payload)
    }
}

/// Host-backed watcher for live session `configOptions` updates (`llm/model-changed`) and
/// hydration of wire-truncated tool-result image payloads (`agent.messages`).
pub(super) struct ConfigSync {
    host: std::sync::Weak<Host>,
    rx: Option<tokio::sync::watch::Receiver<u64>>,
    last: J,
}

impl ConfigSync {
    pub(super) fn install(host: &Arc<Host>, initial: J) -> Self {
        let rx = (|| -> mlua::Result<tokio::sync::watch::Receiver<u64>> {
            let ctx = host.kernel.host_ctx()?;
            let owner = host.kernel.host_owner()?;
            let (tx, rx) = tokio::sync::watch::channel(0u64);
            let cb = host.lua().create_function(move |_, _: mlua::MultiValue| {
                tx.send_modify(|n| *n = n.wrapping_add(1));
                Ok(())
            })?;
            let _ = host.kernel.host_on(&ctx, owner, "llm/model-changed", cb, mare_kernel::HostOn::default())?;
            Ok(rx)
        })()
        .map_err(|e| tracing::warn!("installing ACP llm/model-changed watcher failed: {e}"))
        .ok();
        Self { host: Arc::downgrade(host), rx, last: initial }
    }
}

impl ConfigWatcher for ConfigSync {
    fn is_active(&self) -> bool {
        self.rx.is_some()
    }

    async fn changed(&mut self) -> bool {
        match self.rx.as_mut() {
            Some(rx) => rx.changed().await.is_ok(),
            None => std::future::pending().await,
        }
    }

    async fn flush(&mut self, force: bool) -> Option<J> {
        let rx_changed = self.rx.as_ref().and_then(|rx| rx.has_changed().ok()).unwrap_or(false);
        if rx_changed && let Some(rx) = self.rx.as_mut() {
            rx.mark_unchanged();
        }
        if !force && !rx_changed {
            return None;
        }
        let host = self.host.upgrade()?;
        let opts = config_options(&host).await;
        let opts_json = serde_json::to_value(&opts).ok()?;
        if opts_json == self.last {
            return None;
        }
        self.last = opts_json;
        Some(json!({
            "sessionUpdate": "config_option_update",
            "configOptions": opts,
        }))
    }

    async fn translate_event(&mut self, name: &str, payload: &J) -> Option<J> {
        if name == "item/completed"
            && payload.pointer("/item/message/content").is_some_and(has_truncated_image)
            && let Some(host) = self.host.upgrade()
        {
            return hydrate_tool_result_update(&host, payload).await;
        }
        to_update(name, payload)
    }
}

async fn translate_record_with<W: ConfigWatcher>(
    rec: mare_kernel::CursorRecord,
    watch: &mut W,
) -> Result<Option<J>, String> {
    match rec {
        mare_kernel::CursorRecord::Event { event, selected } if selected => {
            Ok(watch.translate_event(&event.name, &event.payload).await)
        }
        mare_kernel::CursorRecord::Event { .. } => Ok(None),
        mare_kernel::CursorRecord::Lagged { from_seq, to_seq } => {
            tracing::warn!(from_seq, to_seq, "acp cursor lagged behind event log");
            Err(format!("event stream lagged (missed sequences {from_seq}..={to_seq}); turn output is incomplete"))
        }
        mare_kernel::CursorRecord::Dropped { count, .. } => {
            tracing::warn!(count, "acp cursor dropped events under backpressure");
            Err(format!("event stream dropped {count} events under backpressure; turn output is incomplete"))
        }
    }
}

async fn send_update<F, Fut>(
    session_id: &str,
    update: J,
    send: &mut F,
    stream_err: &mut Option<String>,
    record_err: bool,
) -> RecordStep
where
    F: FnMut(String, J) -> Fut,
    Fut: Future<Output = SinkOutcome>,
{
    match send(session_id.to_string(), update).await {
        SinkOutcome::Sent => RecordStep::Continue,
        SinkOutcome::Failed(e) => {
            if record_err {
                stream_err.get_or_insert(e);
            }
            RecordStep::Continue
        }
        #[cfg(test)]
        SinkOutcome::Disconnected => RecordStep::Disconnected,
    }
}

async fn flush_config<F, Fut, W: ConfigWatcher>(
    watch: &mut W,
    session_id: &str,
    send: &mut F,
    stream_err: &mut Option<String>,
    record_err: bool,
    force: bool,
) -> RecordStep
where
    F: FnMut(String, J) -> Fut,
    Fut: Future<Output = SinkOutcome>,
{
    let Some(update) = watch.flush(force).await else {
        return RecordStep::Continue;
    };
    send_update(session_id, update, send, stream_err, record_err).await
}

async fn forward_record<F, Fut, W: ConfigWatcher>(
    rec: mare_kernel::CursorRecord,
    watch: &mut W,
    session_id: &str,
    send: &mut F,
    stream_err: &mut Option<String>,
    record_err: bool,
) -> RecordStep
where
    F: FnMut(String, J) -> Fut,
    Fut: Future<Output = SinkOutcome>,
{
    match translate_record_with(rec, watch).await {
        Ok(Some(u)) => send_update(session_id, u, send, stream_err, record_err).await,
        Ok(None) => RecordStep::Continue,
        Err(e) => {
            if record_err {
                stream_err.get_or_insert(e);
            }
            RecordStep::Continue
        }
    }
}

async fn drain_cursor<F, Fut, W: ConfigWatcher>(
    cursor: &mut mare_kernel::EventCursor,
    watch: &mut W,
    session_id: &str,
    send: &mut F,
    stream_err: &mut Option<String>,
    record_err: bool,
) -> RecordStep
where
    F: FnMut(String, J) -> Fut,
    Fut: Future<Output = SinkOutcome>,
{
    while let Ok(Some(rec)) = cursor.try_recv() {
        match forward_record(rec, watch, session_id, send, stream_err, record_err).await {
            RecordStep::Continue => {}
            #[cfg(test)]
            RecordStep::Disconnected => return RecordStep::Disconnected,
        }
    }
    flush_config(watch, session_id, send, stream_err, record_err, false).await
}

fn ack_pending_cmds(cmd_rx: &mut mpsc::Receiver<ForwarderCmd>, stream_err: &mut Option<String>) {
    while let Ok(cmd) = cmd_rx.try_recv() {
        match cmd {
            ForwarderCmd::BeginTurn(ack) => {
                let _ = ack.send(());
            }
            ForwarderCmd::Flush(ack) => {
                let _ = ack.send(stream_err.take().map_or(Ok(()), Err));
            }
        }
    }
}

async fn forward_session_loop<F, Fut, W: ConfigWatcher>(
    session_id: String,
    mut cursor: mare_kernel::EventCursor,
    mut cmd_rx: mpsc::Receiver<ForwarderCmd>,
    stop_task: CancellationToken,
    start_after: Option<oneshot::Receiver<()>>,
    mut watch: W,
    mut send: F,
) where
    F: FnMut(String, J) -> Fut,
    Fut: Future<Output = SinkOutcome>,
{
    if let Some(rx) = start_after {
        tokio::select! {
            biased;
            _ = stop_task.cancelled() => return,
            _ = rx => {}
        }
    }
    let mut stream_err: Option<String> = None;
    let mut config_rx_open = watch.is_active();
    loop {
        tokio::select! {
            biased;
            _ = stop_task.cancelled() => {
                let _ = drain_cursor(&mut cursor, &mut watch, &session_id, &mut send, &mut stream_err, true).await;
                ack_pending_cmds(&mut cmd_rx, &mut stream_err);
                return;
            }
            changed = watch.changed(), if config_rx_open => {
                if !changed {
                    config_rx_open = false;
                } else {
                    match flush_config(&mut watch, &session_id, &mut send, &mut stream_err, true, true).await {
                        RecordStep::Continue => {}
                        #[cfg(test)]
                        RecordStep::Disconnected => return,
                    }
                }
            }
            rec = cursor.recv() => {
                let Some(rec) = rec else {
                    ack_pending_cmds(&mut cmd_rx, &mut stream_err);
                    return;
                };
                match forward_record(rec, &mut watch, &session_id, &mut send, &mut stream_err, true).await {
                    RecordStep::Continue => {}
                    #[cfg(test)]
                    RecordStep::Disconnected => return,
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    ForwarderCmd::BeginTurn(ack) => {
                        // Drain pre-turn buffered updates and clear any stale lag/drop
                        // error from an earlier idle/background window so it cannot
                        // poison an unrelated subsequent `session/prompt`.
                        let step = drain_cursor(&mut cursor, &mut watch, &session_id, &mut send, &mut stream_err, false).await;
                        stream_err = None;
                        let _ = ack.send(());
                        #[cfg(test)]
                        if matches!(step, RecordStep::Disconnected) {
                            return;
                        }
                        #[cfg(not(test))]
                        let _ = step;
                    }
                    ForwarderCmd::Flush(ack) => {
                        let step = drain_cursor(&mut cursor, &mut watch, &session_id, &mut send, &mut stream_err, true).await;
                        let _ = ack.send(stream_err.take().map_or(Ok(()), Err));
                        #[cfg(test)]
                        if matches!(step, RecordStep::Disconnected) {
                            return;
                        }
                        #[cfg(not(test))]
                        let _ = step;
                    }
                }
            }
        }
    }
}

struct SessionForwarder {
    cmd_tx: mpsc::Sender<ForwarderCmd>,
    stop: CancellationToken,
}

/// Stable client-facing ACP session ID ↔ current `HostManager` internal session ID mapping.
///
/// Matches kimi-code (`packages/acp-server/src/session.ts:168,586-610`): the ACP wire protocol
/// has no session-rekey notification, so the client-visible `sessionId` stays fixed for the
/// lifetime of the ACP session even when a plugin rekeys the underlying host session in place.
#[derive(Default)]
pub(super) struct AcpSessionAliases {
    external_to_internal: HashMap<String, String>,
    internal_to_external: HashMap<String, String>,
}

impl AcpSessionAliases {
    /// Resolve a client-facing ACP session ID to the current `HostManager` session ID.
    pub(super) fn resolve_internal(&self, external_id: &str) -> String {
        self.external_to_internal.get(external_id).cloned().unwrap_or_else(|| external_id.to_owned())
    }

    /// Record an in-place `HostManager` rekey (`old_internal` → `new_internal`) while keeping
    /// the client-facing external ACP session ID unchanged.
    pub(super) fn rekey(&mut self, old_internal: &str, new_internal: &str) {
        if old_internal == new_internal {
            return;
        }
        let external = self.internal_to_external.remove(old_internal).unwrap_or_else(|| old_internal.to_owned());
        self.external_to_internal.insert(external.clone(), new_internal.to_owned());
        self.internal_to_external.insert(new_internal.to_owned(), external);
    }

    /// Remove any alias entry for a client-facing ACP session ID, returning its current internal ID.
    pub(super) fn remove_by_external(&mut self, external_id: &str) -> String {
        if let Some(internal) = self.external_to_internal.remove(external_id) {
            self.internal_to_external.remove(&internal);
            internal
        } else {
            external_id.to_owned()
        }
    }

    /// Remove any alias entry for a `HostManager` internal session ID, returning its external ACP ID.
    pub(super) fn remove_by_internal(&mut self, internal_id: &str) -> String {
        if let Some(external) = self.internal_to_external.remove(internal_id) {
            self.external_to_internal.remove(&external);
            external
        } else {
            internal_id.to_owned()
        }
    }
}

/// Persistent session-scoped event forwarders: one cursor per live ACP session so background
/// wakeup turns stream `session/update` notifications even when no `session/prompt` is in flight,
/// while prompt completion flushes the same cursor before replying with `PromptResponse`.
#[derive(Clone, Default)]
pub(super) struct SessionForwarders(Arc<Mutex<HashMap<String, SessionForwarder>>>);

pub(super) struct ForwarderTarget {
    pub(super) cursor: mare_kernel::EventCursor,
    pub(super) conn: ConnectionTo<Client>,
    pub(super) projector: Option<WireProjector>,
    pub(super) stop: CancellationToken,
    pub(super) start_after: Option<oneshot::Receiver<()>>,
    pub(super) config: ConfigSync,
}

impl SessionForwarders {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, SessionForwarder>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn open(&self, session: &str, target: ForwarderTarget) {
        let ForwarderTarget { cursor, conn, projector, stop, start_after, config } = target;
        let session_id = session.to_string();
        let (cmd_tx, cmd_rx) = mpsc::channel::<ForwarderCmd>(FORWARDER_CMD_CAPACITY);
        let stop_task = stop.clone();

        tokio::task::spawn_local(forward_session_loop(
            session_id,
            cursor,
            cmd_rx,
            stop_task,
            start_after,
            config,
            move |sid, u| {
                let res = super::wire::send_update(&conn, &sid, u, projector.as_slice());
                async move {
                    match res {
                        Ok(()) => SinkOutcome::Sent,
                        Err(e) => SinkOutcome::Failed(e),
                    }
                }
            },
        ));

        if let Some(prev) = self.map().insert(session.to_string(), SessionForwarder { cmd_tx, stop }) {
            prev.stop.cancel();
        }
    }

    #[cfg(test)]
    pub(super) fn open_with_sink<F, Fut>(
        &self,
        session: &str,
        cursor: mare_kernel::EventCursor,
        stop: CancellationToken,
        start_after: Option<oneshot::Receiver<()>>,
        send: F,
    ) where
        F: FnMut(String, J) -> Fut + Send + 'static,
        Fut: Future<Output = SinkOutcome> + Send + 'static,
    {
        let session_id = session.to_string();
        let (cmd_tx, cmd_rx) = mpsc::channel::<ForwarderCmd>(FORWARDER_CMD_CAPACITY);
        let stop_task = stop.clone();

        tokio::spawn(forward_session_loop(session_id, cursor, cmd_rx, stop_task, start_after, NoConfigWatcher, send));

        if let Some(prev) = self.map().insert(session.to_string(), SessionForwarder { cmd_tx, stop }) {
            prev.stop.cancel();
        }
    }

    pub(super) async fn begin_turn(&self, session: &str) {
        let tx = { self.map().get(session).map(|f| f.cmd_tx.clone()) };
        if let Some(tx) = tx {
            let (ack_tx, ack_rx) = oneshot::channel();
            if tx.send(ForwarderCmd::BeginTurn(ack_tx)).await.is_ok() {
                let _ = ack_rx.await;
            }
        }
    }

    pub(super) async fn flush(&self, session: &str, limit: Duration) -> Result<(), String> {
        let tx = { self.map().get(session).map(|f| f.cmd_tx.clone()) };
        let Some(tx) = tx else {
            return Ok(());
        };
        let (ack_tx, ack_rx) = oneshot::channel();
        if tx.send(ForwarderCmd::Flush(ack_tx)).await.is_err() {
            return Ok(());
        }
        match tokio::time::timeout(limit, ack_rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Ok(()),
            Err(_) => Err(format!(
                "updates were not delivered within {} ms; the turn's output is incomplete",
                limit.as_millis()
            )),
        }
    }

    pub(super) fn close(&self, session: &str) {
        if let Some(fwd) = self.map().remove(session) {
            fwd.stop.cancel();
        }
    }

    pub(super) fn close_all(&self) {
        for (_, fwd) in self.map().drain() {
            fwd.stop.cancel();
        }
    }
}

struct CancelSlot {
    token: CancellationToken,
    generation: u64,
}

/// `session/cancel` state, shared with the protocol thread. One token per *live* session:
/// a prompt takes the session's current token when the protocol thread accepts it, and a
/// cancel fires that token and installs a fresh one. So a cancel covers every prompt accepted
/// before it — including one still queued, not yet started — and never a later one; cancels
/// for unknown sessions are ignored and repeated cancels cost one swap each (no queue, no task).
#[derive(Clone, Default)]
pub(super) struct TurnCancels(Arc<Mutex<HashMap<String, CancelSlot>>>);

impl TurnCancels {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancelSlot>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A session became live (new / resumed).
    pub(super) fn open(&self, session: &str) {
        self.map().insert(session.to_string(), CancelSlot { token: CancellationToken::new(), generation: 0 });
    }

    /// A session is closing: its prompts end, later cancels for it are ignored.
    pub(super) fn close(&self, session: &str) {
        if let Some(slot) = self.map().remove(session) {
            slot.token.cancel();
        }
    }

    /// The token a prompt accepted now belongs to (`None`: the session is not live).
    pub(super) fn current(&self, session: &str) -> Option<CancellationToken> {
        self.map().get(session).map(|slot| slot.token.clone())
    }

    /// The token and generation a prompt accepted now belongs to (`None`: the session is not live).
    pub(super) fn current_with_gen(&self, session: &str) -> Option<(CancellationToken, u64)> {
        self.map().get(session).map(|slot| (slot.token.clone(), slot.generation))
    }

    /// `session/cancel`: ends every prompt accepted so far. Returns the new cancel generation
    /// when the session is live, or `None` for unknown/closed sessions.
    pub(super) fn cancel_with_gen(&self, session: &str) -> Option<u64> {
        match self.map().get_mut(session) {
            Some(slot) => {
                slot.generation = slot.generation.wrapping_add(1);
                let generation = slot.generation;
                std::mem::replace(&mut slot.token, CancellationToken::new()).cancel();
                Some(generation)
            }
            None => None,
        }
    }

    /// `session/cancel`: ends every prompt accepted so far. Returns whether the session is live.
    #[cfg(test)]
    pub(super) fn cancel(&self, session: &str) -> bool {
        self.cancel_with_gen(session).is_some()
    }
}

/// Wait for an already-accepted turn (`turn_id`) to settle, aborting that exact turn if
/// `turn_cancel` fired during submission or arrives while waiting.
pub(super) async fn wait_for_started_turn(
    host: &Host,
    turn_id: &str,
    turn_cancel: &CancellationToken,
) -> anyhow::Result<J> {
    if turn_cancel.is_cancelled() {
        let _ = host.abort_turn(turn_id).await;
        return call_service(host, "agent", "wait", vec![json!(turn_id)]).await;
    }
    let wait = call_service(host, "agent", "wait", vec![json!(turn_id)]);
    tokio::pin!(wait);
    tokio::select! {
        biased;
        _ = turn_cancel.cancelled() => {
            let _ = host.abort_turn(turn_id).await;
            wait.await
        }
        res = &mut wait => res,
    }
}

/// Cancel only the running turn (if any) without clearing unrelated durable queue items.
pub(super) async fn cancel_running_turn(host: &Host) {
    let _ = call_service(host, "agent", "cancel", vec![json!({ "clear_queue": false })]).await;
}

/// Wait for a prompt accepted into the durable queue (`status == "queued"`) while another turn
/// (such as a background wakeup) was already running. Watches lifecycle events opened *before*
/// `host.prompt` so we correlate `input_id` to the exact `turn_id` that consumes it, never
/// confuse stale queue updates or external removals with consumption, and never hang when a
/// preceding turn fails and parks queued inputs.
async fn remove_queued_input(host: &Host, input_id: &str) -> bool {
    call_service(host, "agent", "delete_queued", vec![json!(input_id)]).await.is_ok()
}

fn drain_for_consumed_turn(watch: &mut mare_kernel::EventCursor, input_id: &str) -> anyhow::Result<Option<String>> {
    while let Ok(Some(rec)) = watch.try_recv() {
        if let mare_kernel::CursorRecord::Event { event, selected: true } = rec
            && event.name == "item/completed"
            && event.payload.pointer("/item/message/input_id").and_then(J::as_str) == Some(input_id)
        {
            if let Some(turn_id) = event.payload["turn_id"].as_str() {
                return Ok(Some(turn_id.to_string()));
            }
            anyhow::bail!("queued prompt was consumed without a turn_id");
        }
    }
    Ok(None)
}

async fn cancel_queued_input(host: &Host, watch: &mut mare_kernel::EventCursor, input_id: &str) {
    if !remove_queued_input(host, input_id).await
        && let Ok(Some(turn_id)) = drain_for_consumed_turn(watch, input_id)
    {
        let _ = host.abort_turn(&turn_id).await;
    }
}

enum QueuedProbe {
    KeepWaiting,
    Consumed(String),
    Aborted,
}

async fn check_settled_idle_queued_input(
    host: &Host,
    watch: &mut mare_kernel::EventCursor,
    input_id: &str,
    seen_in_queue: &mut bool,
    last_turn_cancel_msg: &Option<String>,
) -> anyhow::Result<QueuedProbe> {
    // Let any `wakeup.wake_pending` task spawned immediately after `publish_end` claim `busy = true`.
    tokio::task::yield_now().await;
    while let Ok(Some(rec)) = watch.try_recv() {
        match rec {
            mare_kernel::CursorRecord::Event { event, selected } if selected => match event.name.as_str() {
                "item/completed" => {
                    if event.payload.pointer("/item/message/input_id").and_then(J::as_str) == Some(input_id) {
                        if let Some(turn_id) = event.payload["turn_id"].as_str() {
                            return Ok(QueuedProbe::Consumed(turn_id.to_string()));
                        }
                        remove_queued_input(host, input_id).await;
                        anyhow::bail!("queued prompt was consumed without a turn_id");
                    }
                }
                "queue/changed" => {
                    let present = event.payload["items"]
                        .as_array()
                        .is_some_and(|items| items.iter().any(|it| it["input_id"].as_str() == Some(input_id)));
                    if present {
                        *seen_in_queue = true;
                    } else if *seen_in_queue && event.payload["action"].as_str() != Some("consumed") {
                        return Ok(QueuedProbe::Aborted);
                    }
                }
                _ => {}
            },
            mare_kernel::CursorRecord::Event { .. } => {}
            mare_kernel::CursorRecord::Lagged { from_seq, to_seq } => {
                remove_queued_input(host, input_id).await;
                anyhow::bail!(
                    "event stream lagged (missed sequences {from_seq}..={to_seq}) while waiting for queued prompt"
                );
            }
            mare_kernel::CursorRecord::Dropped { count, .. } => {
                remove_queued_input(host, input_id).await;
                anyhow::bail!("event stream dropped {count} events under backpressure while waiting for queued prompt");
            }
        }
    }
    let busy =
        call_service(host, "agent", "activity", vec![]).await.ok().and_then(|v| v["busy"].as_bool()).unwrap_or(false);
    if busy {
        return Ok(QueuedProbe::KeepWaiting);
    }
    let compact_busy =
        call_service(host, "compact", "busy", vec![]).await.ok().and_then(|v| v.as_bool()).unwrap_or(false);
    if compact_busy {
        return Ok(QueuedProbe::KeepWaiting);
    }
    let still_queued = call_service(host, "agent", "list_queue", vec![])
        .await
        .ok()
        .and_then(|v| v["data"].as_array().cloned())
        .is_some_and(|items| items.iter().any(|it| it["input_id"].as_str() == Some(input_id)));
    if still_queued {
        if !remove_queued_input(host, input_id).await
            && let Some(turn_id) = drain_for_consumed_turn(watch, input_id)?
        {
            return Ok(QueuedProbe::Consumed(turn_id));
        }
        let msg = last_turn_cancel_msg.as_deref().unwrap_or("previous turn settled without waking queued prompt");
        anyhow::bail!("queued prompt was parked: {msg}");
    }
    Ok(QueuedProbe::KeepWaiting)
}

pub(super) async fn wait_for_queued_input(
    host: &Host,
    watch: &mut mare_kernel::EventCursor,
    input_id: &str,
    turn_cancel: &CancellationToken,
) -> anyhow::Result<J> {
    if turn_cancel.is_cancelled() {
        cancel_queued_input(host, watch, input_id).await;
        return Ok(json!({ "error_kind": "aborted" }));
    }
    let mut seen_in_queue = false;
    let owned_turn_id = loop {
        tokio::select! {
            biased;
            _ = turn_cancel.cancelled() => {
                cancel_queued_input(host, watch, input_id).await;
                return Ok(json!({ "error_kind": "aborted" }));
            }
            rec = watch.recv() => {
                let Some(rec) = rec else {
                    let _ = remove_queued_input(host, input_id).await;
                    anyhow::bail!("session event stream closed while waiting for queued prompt");
                };
                match rec {
                    mare_kernel::CursorRecord::Event { event, selected } if selected => {
                        match event.name.as_str() {
                            "item/completed" => {
                                if event.payload.pointer("/item/message/input_id").and_then(J::as_str) == Some(input_id) {
                                    if let Some(turn_id) = event.payload["turn_id"].as_str() {
                                        break turn_id.to_string();
                                    }
                                    let _ = remove_queued_input(host, input_id).await;
                                    anyhow::bail!("queued prompt was consumed without a turn_id");
                                }
                            }
                            "queue/changed" => {
                                let present = event.payload["items"]
                                    .as_array()
                                    .is_some_and(|items| items.iter().any(|it| it["input_id"].as_str() == Some(input_id)));
                                if present {
                                    seen_in_queue = true;
                                } else if seen_in_queue && event.payload["action"].as_str() != Some("consumed") {
                                    return Ok(json!({ "error_kind": "aborted" }));
                                }
                            }
                            "turn/completed" => {
                                let status = event.payload["status"].as_str();
                                let not_sent = event.payload["not_sent"].as_bool() == Some(true);
                                let msg = event.payload["error"]["message"]
                                    .as_str()
                                    .or_else(|| event.payload["error"].as_str());
                                // `wakeup.park_failed_inputs` runs before `turn/completed` is emitted, so a
                                // failing turn parks any unconsumed queued input instead of auto-waking it.
                                // Delete our entry from the durable queue before bailing so a later prompt
                                // cannot unexpectedly execute a request we already reported as failed.
                                if seen_in_queue && (status == Some("failed") || not_sent) {
                                    if !remove_queued_input(host, input_id).await
                                        && let Some(turn_id) = drain_for_consumed_turn(watch, input_id)?
                                    {
                                        break turn_id;
                                    }
                                    let msg = msg.unwrap_or("previous turn failed");
                                    anyhow::bail!("queued prompt was parked after previous turn failed: {msg}");
                                }
                                // A preceding turn cancelled with `halt` also parks queued inputs without
                                // waking them (`wakeup.lua:34`), while a normal cancel without `halt`
                                // spawns `wake_pending`. Probe once the turn settles to avoid hanging.
                                if seen_in_queue && status == Some("cancelled") {
                                    let cancel_msg =
                                        Some(msg.unwrap_or("previous turn was cancelled").to_string());
                                    match check_settled_idle_queued_input(
                                        host,
                                        watch,
                                        input_id,
                                        &mut seen_in_queue,
                                        &cancel_msg,
                                    )
                                    .await?
                                    {
                                        QueuedProbe::KeepWaiting => {}
                                        QueuedProbe::Consumed(turn_id) => break turn_id,
                                        QueuedProbe::Aborted => return Ok(json!({ "error_kind": "aborted" })),
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    mare_kernel::CursorRecord::Event { .. } => {}
                    mare_kernel::CursorRecord::Lagged { from_seq, to_seq } => {
                        let _ = remove_queued_input(host, input_id).await;
                        anyhow::bail!(
                            "event stream lagged (missed sequences {from_seq}..={to_seq}) while waiting for queued prompt"
                        );
                    }
                    mare_kernel::CursorRecord::Dropped { count, .. } => {
                        let _ = remove_queued_input(host, input_id).await;
                        anyhow::bail!(
                            "event stream dropped {count} events under backpressure while waiting for queued prompt"
                        );
                    }
                }
            }
        }
    };

    wait_for_started_turn(host, &owned_turn_id, turn_cancel).await
}

fn is_refusal_error(v: &J) -> bool {
    if v["error_kind"] == "refusal" {
        return true;
    }
    v["error"]["code"].as_str().is_some_and(|code| {
        code == "refusal" || code.ends_with(".refusal") || code == "provider.filtered" || code == "content_filter"
    })
}

fn last_message_stop_reason(v: &J) -> Option<&str> {
    v["messages"].as_array()?.last()?.get("stop_reason")?.as_str()
}

/// Map a completed `agent.prompt` / `agent.wait` JSON record into an ACP `StopReason` tag.
pub(super) fn map_turn_result(res: &anyhow::Result<J>) -> Result<String, AcpErr> {
    match res {
        Err(e) => Err(AcpErr::Internal(e.to_string())),
        Ok(v)
            if v["error_kind"] == "aborted"
                || v["error"]["code"] == "aborted"
                || v["error"]["code"] == "cancelled"
                || v["status"] == "cancelled"
                || v["termination_reason"] == "interrupted" =>
        {
            Ok("cancelled".into())
        }
        Ok(v)
            if v["termination_reason"] == "turn_limit"
                || v["error_kind"] == "max_turns"
                || v["error"]["code"] == "agent.max_turns" =>
        {
            Ok("max_turn_requests".into())
        }
        Ok(v) if is_refusal_error(v) => Ok("refusal".into()),
        Ok(v) if !v["error"].is_null() => {
            let msg = v["error"]["message"].as_str().or_else(|| v["error"].as_str()).unwrap_or("error");
            Err(AcpErr::Internal(msg.to_string()))
        }
        Ok(v)
            if matches!(
                v["stop_reason"].as_str().or_else(|| last_message_stop_reason(v)),
                Some("filtered" | "refusal")
            ) =>
        {
            Ok("refusal".into())
        }
        Ok(v) if v["truncated"].as_bool().unwrap_or_else(|| last_message_stop_reason(v) == Some("length")) => {
            Ok("max_tokens".into())
        }
        _ => Ok("end_turn".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mare_host::manager::close_until_confirmed;
    use serde_json::json;

    async fn push_text_event(log: &mare_kernel::EventLog, i: usize) {
        log.admit_json(
            "item/delta",
            json!({ "turn_id": "t-1", "item_id": "it-1", "kind": "text", "delta": format!("chunk-{i}"), "offset": i }),
        )
        .await
        .unwrap();
    }

    fn outcome(ok: bool) -> SinkOutcome {
        if ok { SinkOutcome::Sent } else { SinkOutcome::Disconnected }
    }

    #[tokio::test]
    async fn session_forwarder_drains_all_updates_when_update_queue_is_full() {
        let log = mare_kernel::EventLog::new();
        let cursor = log.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let (utx, mut urx) = mpsc::channel::<(String, J)>(2);
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("s-1", cursor, CancellationToken::new(), None, move |sid, u| {
            let utx = utx.clone();
            async move { outcome(utx.send((sid, u)).await.is_ok()) }
        });
        let total = 12usize;
        for i in 0..total {
            push_text_event(&log, i).await;
        }
        let fwd_flush = forwarders.clone();
        let flush_task = tokio::spawn(async move { fwd_flush.flush("s-1", PROMPT_DRAIN_TIMEOUT).await });
        let mut received = Vec::new();
        while received.len() < total {
            let (sid, u) = urx.recv().await.unwrap();
            assert_eq!(sid, "s-1");
            received.push(u["content"]["text"].as_str().unwrap().to_string());
        }
        assert_eq!(flush_task.await.unwrap(), Ok(()));
        for (i, text) in received.iter().enumerate() {
            assert_eq!(text, &format!("chunk-{i}"));
        }
        forwarders.close("s-1");
    }

    #[tokio::test(start_paused = true)]
    async fn flush_reports_updates_still_blocked_at_the_deadline() {
        let log = mare_kernel::EventLog::new();
        let cursor = log.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let (utx, _urx_never_read) = mpsc::channel::<(String, J)>(1);
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("s-1", cursor, CancellationToken::new(), None, move |sid, u| {
            let utx = utx.clone();
            async move { outcome(utx.send((sid, u)).await.is_ok()) }
        });
        for i in 0..3 {
            push_text_event(&log, i).await;
        }
        let res = forwarders.flush("s-1", Duration::from_millis(50)).await;
        let err = res.expect_err("a flush that timed out must not report success");
        assert!(err.contains("not delivered"), "{err}");
        forwarders.close("s-1");
    }

    #[tokio::test]
    async fn session_forwarder_reports_lagged_cursor_and_begin_turn_clears_stale_error() {
        let log = mare_kernel::EventLog::new();
        let mut limits = log.limits();
        limits.capacity = 2;
        log.set_limits(limits);
        // Optional cursor (selected = None) so admit_json never blocks, allowing eviction/lag.
        let cursor = log.open_cursor(std::sync::Arc::new(|_| None));
        let (utx, _urx) = mpsc::channel::<(String, J)>(8);
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("s-1", cursor, CancellationToken::new(), None, move |sid, u| {
            let utx = utx.clone();
            async move { outcome(utx.send((sid, u)).await.is_ok()) }
        });
        for i in 0..8 {
            push_text_event(&log, i).await;
        }
        let res = forwarders.flush("s-1", PROMPT_DRAIN_TIMEOUT).await;
        assert!(res.is_err(), "lagged ACP cursor must error instead of claiming end_turn: {res:?}");
        assert!(res.unwrap_err().contains("lagged"));

        // A lag during an idle/background period before the next prompt starts must be cleared by
        // `begin_turn` so it does not poison an unrelated subsequent prompt.
        for i in 8..16 {
            push_text_event(&log, i).await;
        }
        forwarders.begin_turn("s-1").await;
        let after_begin = forwarders.flush("s-1", PROMPT_DRAIN_TIMEOUT).await;
        assert_eq!(after_begin, Ok(()), "begin_turn must clear stale background lag error");
        forwarders.close("s-1");
    }

    #[tokio::test]
    async fn begin_turn_awaits_when_cmd_queue_is_full_and_clears_stale_lag_error() {
        let log = mare_kernel::EventLog::new();
        let mut limits = log.limits();
        limits.capacity = 2;
        log.set_limits(limits);
        let cursor = log.open_cursor(Arc::new(|_| Some(true)));
        let (start_tx, start_rx) = oneshot::channel();
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("s-full", cursor, CancellationToken::new(), Some(start_rx), |_, _| async {
            SinkOutcome::Sent
        });

        // Overflow the event log while the forwarder is still gated on `start_after`, then fill
        // every slot of the bounded command channel so `try_send` would fail with Full.
        for i in 0..8 {
            push_text_event(&log, i).await;
        }
        let tx = forwarders.map().get("s-full").unwrap().cmd_tx.clone();
        let mut ack_rxs = Vec::with_capacity(FORWARDER_CMD_CAPACITY);
        for _ in 0..FORWARDER_CMD_CAPACITY {
            let (ack_tx, ack_rx) = oneshot::channel();
            tx.try_send(ForwarderCmd::Flush(ack_tx)).expect("channel must accept up to capacity");
            ack_rxs.push(ack_rx);
        }
        let (probe_tx, _probe_rx) = oneshot::channel();
        assert!(tx.try_send(ForwarderCmd::BeginTurn(probe_tx)).is_err(), "command queue must now be full");

        // `begin_turn` must wait for capacity rather than dropping the command.
        let fwd_for_begin = forwarders.clone();
        let begin_task = tokio::spawn(async move {
            fwd_for_begin.begin_turn("s-full").await;
        });
        tokio::task::yield_now().await;
        assert!(!begin_task.is_finished(), "begin_turn must await space while command queue is full");

        // Unblock the forwarder; the pre-queued flushes drain, then `begin_turn` delivers and
        // clears the stale lag error so a subsequent flush returns Ok(()).
        let _ = start_tx.send(());
        tokio::time::timeout(PROMPT_DRAIN_TIMEOUT, begin_task)
            .await
            .expect("begin_turn timed out waiting for queue space")
            .unwrap();
        let first_flush = ack_rxs.remove(0).await.unwrap();
        assert!(first_flush.is_err(), "earlier flush must still observe the pre-turn lag error: {first_flush:?}");
        let after_begin = forwarders.flush("s-full", PROMPT_DRAIN_TIMEOUT).await;
        assert_eq!(after_begin, Ok(()), "begin_turn queued behind full channel must clear stale lag error");
        forwarders.close("s-full");
    }

    #[tokio::test]
    async fn begin_turn_waits_for_forwarder_ack_so_new_turn_lag_is_preserved() {
        let log = mare_kernel::EventLog::new();
        let mut limits = log.limits();
        limits.capacity = 2;
        log.set_limits(limits);
        let cursor = log.open_cursor(Arc::new(|_| None));
        let (start_tx, start_rx) = oneshot::channel();
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("s-sync", cursor, CancellationToken::new(), Some(start_rx), |_, _| async {
            SinkOutcome::Sent
        });

        // Pre-turn lag while forwarder is gated; `begin_turn` must not return until processed.
        for i in 0..6 {
            push_text_event(&log, i).await;
        }
        let fwd_begin = forwarders.clone();
        let begin_task = tokio::spawn(async move {
            fwd_begin.begin_turn("s-sync").await;
        });
        tokio::task::yield_now().await;
        assert!(!begin_task.is_finished(), "begin_turn must wait for forwarder ack before starting a turn");

        let _ = start_tx.send(());
        tokio::time::timeout(PROMPT_DRAIN_TIMEOUT, begin_task).await.expect("begin_turn ack timed out").unwrap();

        // Any lag occurring after `begin_turn` returned belongs to the new turn and must be reported.
        for i in 6..14 {
            push_text_event(&log, i).await;
        }
        let res = forwarders.flush("s-sync", PROMPT_DRAIN_TIMEOUT).await;
        assert!(res.is_err(), "new-turn lag after begin_turn ack must not be wiped: {res:?}");
        assert!(res.unwrap_err().contains("lagged"));
        forwarders.close("s-sync");
    }

    #[tokio::test]
    async fn session_forwarder_reports_sink_delivery_failure_on_flush() {
        let log = mare_kernel::EventLog::new();
        let cursor = log.open_cursor(Arc::new(|_| Some(true)));
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("s-err", cursor, CancellationToken::new(), None, |_, _| async {
            SinkOutcome::Failed("failed to send projected ACP session update".into())
        });
        push_text_event(&log, 0).await;
        let err = forwarders
            .flush("s-err", PROMPT_DRAIN_TIMEOUT)
            .await
            .expect_err("sink delivery failure must surface on flush");
        assert!(err.contains("failed to send projected ACP session update"), "{err}");
        forwarders.close("s-err");
    }

    #[tokio::test]
    async fn session_forwarder_exits_promptly_on_disconnect() {
        let log = mare_kernel::EventLog::new();
        let cursor = log.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let (utx, urx) = mpsc::channel::<(String, J)>(1);
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("s-1", cursor, CancellationToken::new(), None, move |sid, u| {
            let utx = utx.clone();
            async move { outcome(utx.send((sid, u)).await.is_ok()) }
        });
        push_text_event(&log, 0).await;
        drop(urx);
        let res = tokio::time::timeout(Duration::from_secs(1), forwarders.flush("s-1", PROMPT_DRAIN_TIMEOUT))
            .await
            .expect("forwarder did not exit on disconnect");
        assert_eq!(res, Ok(()));
    }

    #[tokio::test]
    async fn session_forwarder_streams_out_of_band_rekeys_and_respects_start_after_and_parent_cancel() {
        let log = mare_kernel::EventLog::new();
        let cursor = log.open_cursor(std::sync::Arc::new(|_| Some(true)));
        let (utx, mut urx) = mpsc::channel::<(String, J)>(8);
        let parent_cancel = CancellationToken::new();
        let (replay_tx, replay_rx) = oneshot::channel();
        let forwarders = SessionForwarders::default();
        forwarders.open_with_sink("old-sid", cursor, parent_cancel.child_token(), Some(replay_rx), move |sid, u| {
            let utx = utx.clone();
            async move { outcome(utx.send((sid, u)).await.is_ok()) }
        });

        // Live event emitted before replay completes is buffered, not delivered prematurely.
        push_text_event(&log, 0).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), urx.recv()).await.is_err(),
            "live update must wait for replay gate"
        );
        let _ = replay_tx.send(());
        let (sid0, u0) = urx.recv().await.unwrap();
        assert_eq!(sid0, "old-sid");
        assert_eq!(u0["content"]["text"], "chunk-0");

        // Subsequent updates keep emitting under the fixed client-facing ACP sessionId.
        push_text_event(&log, 1).await;
        assert_eq!(forwarders.flush("old-sid", PROMPT_DRAIN_TIMEOUT).await, Ok(()));
        let (sid1, u1) = urx.recv().await.unwrap();
        assert_eq!(sid1, "old-sid");
        assert_eq!(u1["content"]["text"], "chunk-1");

        // Cancelling the parent session token stops the forwarder task cleanly.
        parent_cancel.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), urx.recv()).await.unwrap().is_none(),
            "forwarder must exit when parent session token cancels"
        );
    }

    #[test]
    fn map_turn_result_recognizes_all_cancel_and_stop_reason_shapes() {
        assert_eq!(map_turn_result(&Ok(json!({ "error_kind": "aborted" }))).ok().as_deref(), Some("cancelled"));
        assert_eq!(map_turn_result(&Ok(json!({ "error": { "code": "aborted" } }))).ok().as_deref(), Some("cancelled"));
        assert_eq!(
            map_turn_result(&Ok(json!({ "error": { "code": "cancelled", "message": "cancelled" } }))).ok().as_deref(),
            Some("cancelled")
        );
        assert_eq!(
            map_turn_result(&Ok(json!({ "status": "cancelled", "error": { "code": "error", "message": "stopped" } })))
                .ok()
                .as_deref(),
            Some("cancelled")
        );
        assert_eq!(
            map_turn_result(&Ok(json!({ "termination_reason": "interrupted" }))).ok().as_deref(),
            Some("cancelled")
        );
        assert_eq!(
            map_turn_result(&Ok(json!({ "termination_reason": "turn_limit", "error": { "code": "agent.max_turns", "message": "max turns reached" } })))
                .ok()
                .as_deref(),
            Some("max_turn_requests")
        );
        assert_eq!(
            map_turn_result(&Ok(
                json!({ "error_kind": "max_turns", "error": { "code": "max_turns", "message": "limit" } })
            ))
            .ok()
            .as_deref(),
            Some("max_turn_requests")
        );
        assert_eq!(
            map_turn_result(&Ok(
                json!({ "error_kind": "refusal", "error": { "code": "llm.refusal", "message": "refused" } })
            ))
            .ok()
            .as_deref(),
            Some("refusal")
        );
        assert_eq!(
            map_turn_result(&Ok(json!({ "error": { "code": "provider.filtered", "message": "blocked by filter" } })))
                .ok()
                .as_deref(),
            Some("refusal")
        );
        assert_eq!(
            map_turn_result(&Ok(json!({ "messages": [{ "stop_reason": "filtered" }] }))).ok().as_deref(),
            Some("refusal")
        );
        assert_eq!(
            map_turn_result(&Ok(json!({ "messages": [{ "stop_reason": "refusal" }] }))).ok().as_deref(),
            Some("refusal")
        );
        assert_eq!(map_turn_result(&Ok(json!({ "truncated": true }))).ok().as_deref(), Some("max_tokens"));
        assert_eq!(
            map_turn_result(&Ok(json!({ "truncated": false, "stop_reason": "length" }))).ok().as_deref(),
            Some("end_turn")
        );
        assert_eq!(
            map_turn_result(&Ok(json!({ "messages": [{ "stop_reason": "length" }] }))).ok().as_deref(),
            Some("max_tokens")
        );
        assert_eq!(map_turn_result(&Ok(json!({ "status": "completed" }))).ok().as_deref(), Some("end_turn"));
        assert!(map_turn_result(&Ok(json!({ "error": { "code": "internal", "message": "boom" } }))).is_err());
        assert!(
            map_turn_result(&Ok(json!({
                "messages": [{ "stop_reason": "filtered" }],
                "error": { "code": "save_failed", "message": "disk full" }
            })))
            .is_err(),
            "persistence error alongside filtered stop_reason must return Internal error, not refusal"
        );
        assert!(
            map_turn_result(&Ok(json!({
                "messages": [{ "stop_reason": "refusal" }],
                "error": { "code": "save_failed", "message": "disk full" }
            })))
            .is_err(),
            "persistence error alongside refusal stop_reason must return Internal error, not refusal"
        );
    }

    #[tokio::test]
    async fn wait_for_queued_input_deletes_queued_item_on_cursor_lag() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let script = json!([{ "text": "x".repeat(400), "delay_ms": 30 }, { "text": "queued-should-not-run" }]);
                let host = Host::new(mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": {
                            "permissions": { "enabled": false },
                            "provider-faux": { "script_json": script.to_string() }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                // Start a slow turn so the next prompt is queued.
                let first = tokio::time::timeout(
                    Duration::from_secs(5),
                    call_service(&host, "agent", "prompt", vec![json!("first"), json!({ "when_busy": "queue" })]),
                )
                .await
                .expect("first prompt timed out")
                .unwrap();
                assert_eq!(first["status"], "started", "{first}");

                let queued = tokio::time::timeout(
                    Duration::from_secs(5),
                    call_service(
                        &host,
                        "agent",
                        "prompt",
                        vec![json!("second queued"), json!({ "when_busy": "queue" })],
                    ),
                )
                .await
                .expect("second prompt timed out")
                .unwrap();
                assert_eq!(queued["status"], "queued", "{queued}");
                let input_id = queued["input_id"].as_str().expect("queued input_id").to_string();

                let before_queue = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                assert!(
                    before_queue["data"].as_array().is_some_and(|arr| arr.iter().any(|x| x["input_id"] == input_id)),
                    "queued input must be in queue before failure: {before_queue}"
                );

                // Trigger Lagged on a bounded EventLog cursor passed to wait_for_queued_input.
                let log = mare_kernel::EventLog::new();
                let mut limits = log.limits();
                limits.capacity = 2;
                log.set_limits(limits);
                let mut cursor = log.open_cursor(Arc::new(|_| None));
                for i in 0..8 {
                    push_text_event(&log, i).await;
                }

                let res = tokio::time::timeout(
                    Duration::from_secs(5),
                    wait_for_queued_input(&host, &mut cursor, &input_id, &CancellationToken::new()),
                )
                .await
                .expect("wait_for_queued_input timed out");
                assert!(res.is_err(), "lagged cursor must fail wait_for_queued_input: {res:?}");

                // The queued item must have been removed so it will not execute later.
                let after_queue = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                assert!(
                    !after_queue["data"].as_array().is_some_and(|arr| arr.iter().any(|x| x["input_id"] == input_id)),
                    "failed wait_for_queued_input must delete queued item from queue: {after_queue}"
                );

                let _ = tokio::time::timeout(Duration::from_secs(5), host.abort()).await;
                let _ = tokio::time::timeout(Duration::from_secs(5), host.shutdown()).await;
            })
            .await;
    }

    #[test]
    fn cancel_covers_prompts_accepted_before_it_and_not_after() {
        let cancels = TurnCancels::default();
        cancels.open("s");
        let queued = cancels.current("s").unwrap();
        assert!(cancels.cancel("s"));
        assert!(queued.is_cancelled(), "the prompt accepted before the cancel must see it");
        let later = cancels.current("s").unwrap();
        assert!(!later.is_cancelled(), "a prompt accepted after the cancel must run");
    }

    #[test]
    fn cancel_floods_and_unknown_sessions_stay_bounded() {
        let cancels = TurnCancels::default();
        for i in 0..10_000 {
            assert!(!cancels.cancel(&format!("unknown-{i}")));
        }
        cancels.open("s");
        for _ in 0..10_000 {
            assert!(cancels.cancel("s"));
        }
        assert_eq!(cancels.map().len(), 1, "only live sessions hold state");
        cancels.close("s");
        assert!(cancels.map().is_empty());
        assert!(!cancels.cancel("s"), "a closed session ignores cancels");
    }

    #[tokio::test]
    async fn unconfirmed_close_is_released_once_a_later_attempt_confirms() {
        let attempts = std::rc::Rc::new(std::cell::Cell::new(0));
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let released = std::rc::Rc::new(std::cell::Cell::new(false));
        let (a, r) = (attempts.clone(), released.clone());
        close_until_confirmed(
            move || {
                let n = a.get() + 1;
                a.set(n);
                async move { if n < 3 { Err("stop_unconfirmed".to_string()) } else { Ok(()) } }
            },
            move |res| {
                let _ = reply_tx.send(res);
            },
            move || r.set(true),
        )
        .await;
        assert_eq!(reply_rx.await.unwrap(), Err("stop_unconfirmed".to_string()), "the client hears the first result");
        assert_eq!(attempts.get(), 3, "retried until confirmed");
        assert!(released.get(), "released after the confirming attempt");
    }

    #[tokio::test]
    async fn confirmed_close_is_released_after_one_attempt() {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let released = std::rc::Rc::new(std::cell::Cell::new(false));
        let r = released.clone();
        close_until_confirmed(
            || async { Ok(()) },
            move |res| {
                let _ = reply_tx.send(res);
            },
            move || r.set(true),
        )
        .await;
        assert_eq!(reply_rx.await.unwrap(), Ok(()));
        assert!(released.get());
    }

    #[test]
    fn aliases_keep_external_acp_id_stable_across_chained_rekeys_and_close() {
        let mut aliases = AcpSessionAliases::default();
        assert_eq!(aliases.resolve_internal("s0"), "s0");

        aliases.rekey("s0", "s1");
        assert_eq!(aliases.resolve_internal("s0"), "s1");
        assert_eq!(aliases.resolve_internal("s1"), "s1");

        aliases.rekey("s1", "s2");
        assert_eq!(aliases.resolve_internal("s0"), "s2");
        assert_eq!(aliases.remove_by_internal("s2"), "s0");
        assert_eq!(aliases.resolve_internal("s0"), "s0");

        aliases.rekey("s3", "s4");
        assert_eq!(aliases.remove_by_external("s3"), "s4");
        assert_eq!(aliases.remove_by_internal("s4"), "s4");
    }

    #[tokio::test]
    async fn wait_for_queued_input_bails_when_preceding_turn_halts_and_parks_input() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let script =
                    json!([{ "text": "x".repeat(400), "delay_ms": 500 }, { "text": "queued-should-stay-parked" }]);
                let host = Host::new(mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": {
                            "permissions": { "enabled": false },
                            "provider-faux": { "script_json": script.to_string() }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();

                let mut cursor = host.kernel.open_cursor(Arc::new(|name| {
                    matches!(name, "item/completed" | "queue/changed" | "turn/completed").then_some(true)
                }));

                // Start a slow preceding turn, then queue an ACP prompt behind it.
                let first = tokio::time::timeout(
                    Duration::from_secs(5),
                    call_service(&host, "agent", "prompt", vec![json!("first"), json!({ "when_busy": "queue" })]),
                )
                .await
                .expect("first prompt timed out")
                .unwrap();
                assert_eq!(first["status"], "started", "{first}");

                let queued = tokio::time::timeout(
                    Duration::from_secs(5),
                    call_service(
                        &host,
                        "agent",
                        "prompt",
                        vec![json!("second queued"), json!({ "when_busy": "queue" })],
                    ),
                )
                .await
                .expect("second prompt timed out")
                .unwrap();
                assert_eq!(queued["status"], "queued", "{queued}");
                let input_id = queued["input_id"].as_str().expect("queued input_id").to_string();

                // Halt the preceding turn without clearing the queue: `wakeup.park_failed_inputs`
                // parks our queued input and emits `turn/completed(cancelled)` + `agent/settled(busy=false)`.
                let halted = call_service(
                    &host,
                    "agent",
                    "cancel",
                    vec![json!({ "mode": "halt", "clear_queue": false, "reason": "halted by abort" })],
                )
                .await
                .unwrap();
                assert!(halted.is_object(), "{halted}");
                let cancelled = call_service(
                    &host,
                    "agent",
                    "cancel",
                    vec![json!({ "mode": "cancel", "clear_queue": false, "reason": "halted by abort" })],
                )
                .await
                .unwrap();
                assert!(cancelled.is_object(), "{cancelled}");

                let res = tokio::time::timeout(
                    Duration::from_secs(5),
                    wait_for_queued_input(&host, &mut cursor, &input_id, &CancellationToken::new()),
                )
                .await
                .expect("wait_for_queued_input must not hang when preceding turn halts and parks input");
                let err = res.expect_err("parked input after halted turn must return error");
                assert!(err.to_string().contains("parked"), "unexpected error: {err}");

                // The parked prompt must be removed from the durable queue before bailing so a
                // later prompt cannot execute it unexpectedly.
                let after_queue = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                assert!(
                    !after_queue["data"].as_array().is_some_and(|arr| arr.iter().any(|x| x["input_id"] == input_id)),
                    "parked queued prompt must be deleted before bailing: {after_queue}"
                );

                host.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn cancel_queued_input_and_running_turn_preserve_unrelated_durable_queue_entries() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let script = json!([
                    { "text": "x".repeat(400), "delay_ms": 500 },
                    { "text": "later" }
                ]);
                let host = Host::new(mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": {
                            "permissions": { "enabled": false },
                            "provider-faux": { "script_json": script.to_string() }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();

                let mut cursor = host.kernel.open_cursor(Arc::new(|name| {
                    matches!(name, "item/completed" | "queue/changed" | "turn/completed").then_some(true)
                }));

                let first =
                    call_service(&host, "agent", "prompt", vec![json!("running"), json!({ "when_busy": "queue" })])
                        .await
                        .unwrap();
                assert_eq!(first["status"], "started", "{first}");

                // Pre-existing durable queue entry that belongs to another request/restored queue.
                let kept =
                    call_service(&host, "agent", "prompt", vec![json!("keep-me"), json!({ "when_busy": "queue" })])
                        .await
                        .unwrap();
                assert_eq!(kept["status"], "queued", "{kept}");
                let keep_id = kept["input_id"].as_str().unwrap().to_string();

                // Second queued prompt owned by our ACP waiter.
                let owned =
                    call_service(&host, "agent", "prompt", vec![json!("cancel-me"), json!({ "when_busy": "queue" })])
                        .await
                        .unwrap();
                assert_eq!(owned["status"], "queued", "{owned}");
                let owned_id = owned["input_id"].as_str().unwrap().to_string();

                let cancel_tok = CancellationToken::new();
                cancel_tok.cancel();
                let res = wait_for_queued_input(&host, &mut cursor, &owned_id, &cancel_tok).await.unwrap();
                assert_eq!(res["error_kind"], "aborted", "{res}");

                let q1 = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                let items1 = q1["data"].as_array().expect("queue array");
                assert!(
                    !items1.iter().any(|x| x["input_id"] == owned_id),
                    "cancelled ACP prompt must be removed: {q1}"
                );
                assert!(items1.iter().any(|x| x["input_id"] == keep_id), "unrelated queued prompt must stay: {q1}");

                // Cancelling the running turn via ACP must also preserve `keep_id`.
                cancel_running_turn(&host).await;
                let q2 = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                assert!(
                    q2["data"].as_array().is_some_and(|arr| arr.iter().any(|x| x["input_id"] == keep_id)),
                    "cancel_running_turn must not clear unrelated durable queue items: {q2}"
                );

                let _ = host.abort().await;
                host.shutdown().await;
            })
            .await;
    }

    #[tokio::test]
    async fn drain_for_consumed_turn_handles_consumed_missing_turn_id_and_empty() {
        let log = mare_kernel::EventLog::new();
        let mut watch = log.open_cursor(Arc::new(|name| (name == "item/completed").then_some(true)));
        assert_eq!(drain_for_consumed_turn(&mut watch, "inp-1").unwrap(), None);

        log.admit_json(
            "item/completed",
            json!({ "turn_id": "turn-42", "item": { "message": { "input_id": "inp-1" } } }),
        )
        .await
        .unwrap();
        assert_eq!(drain_for_consumed_turn(&mut watch, "inp-1").unwrap().as_deref(), Some("turn-42"));

        log.admit_json("item/completed", json!({ "item": { "message": { "input_id": "inp-2" } } })).await.unwrap();
        let err = drain_for_consumed_turn(&mut watch, "inp-2").expect_err("missing turn_id must bail");
        assert!(err.to_string().contains("without a turn_id"), "{err}");
    }

    #[tokio::test]
    async fn wait_for_queued_input_handles_external_removal_stream_close_and_failed_preceding_turn() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let script = json!([
                    { "text": "x".repeat(400), "delay_ms": 2000 },
                    { "text": "second-turn-body", "delay_ms": 2000 }
                ]);
                let host = Host::new(mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": {
                            "permissions": { "enabled": false },
                            "provider-faux": { "script_json": script.to_string() }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({ "no_session": true }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();

                let first =
                    call_service(&host, "agent", "prompt", vec![json!("running"), json!({ "when_busy": "queue" })])
                        .await
                        .unwrap();
                assert_eq!(first["status"], "started", "{first}");

                // 1. External queue deletion (`action != "consumed"`) ends wait cleanly as `aborted`.
                let log1 = mare_kernel::EventLog::new();
                let mut watch1 = log1.open_cursor(Arc::new(|_| Some(true)));
                log1.admit_json("queue/changed", json!({ "action": "queued", "items": [{ "input_id": "ext-1" }] }))
                    .await
                    .unwrap();
                log1.admit_json("queue/changed", json!({ "action": "deleted", "items": [] })).await.unwrap();
                let ext_res = wait_for_queued_input(&host, &mut watch1, "ext-1", &CancellationToken::new())
                    .await
                    .expect("external removal must return aborted");
                assert_eq!(ext_res["error_kind"], "aborted", "{ext_res}");

                // 2. Closed event stream removes the queued entry from the durable queue and errors.
                let q_close =
                    call_service(&host, "agent", "prompt", vec![json!("will-close"), json!({ "when_busy": "queue" })])
                        .await
                        .unwrap();
                let close_id = q_close["input_id"].as_str().unwrap().to_string();
                let log2 = mare_kernel::EventLog::new();
                let mut watch2 = log2.open_cursor(Arc::new(|_| Some(true)));
                watch2.close();
                let close_err = wait_for_queued_input(&host, &mut watch2, &close_id, &CancellationToken::new())
                    .await
                    .expect_err("closed event stream must fail");
                assert!(close_err.to_string().contains("event stream closed"), "{close_err}");
                let q_after_close = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                assert!(
                    !q_after_close["data"].as_array().is_some_and(|a| a.iter().any(|x| x["input_id"] == close_id)),
                    "closed stream must delete queued input: {q_after_close}"
                );

                // 3. Preceding turn failure deletes the parked queued entry and surfaces the error.
                let q_fail =
                    call_service(&host, "agent", "prompt", vec![json!("will-park"), json!({ "when_busy": "queue" })])
                        .await
                        .unwrap();
                let fail_id = q_fail["input_id"].as_str().unwrap().to_string();
                let log3 = mare_kernel::EventLog::new();
                let mut watch3 = log3.open_cursor(Arc::new(|_| Some(true)));
                log3.admit_json("queue/changed", json!({ "action": "queued", "items": [{ "input_id": fail_id }] }))
                    .await
                    .unwrap();
                log3.admit_json(
                    "turn/completed",
                    json!({ "turn_id": "t-prev", "status": "failed", "error": { "message": "upstream boom" } }),
                )
                .await
                .unwrap();
                let fail_err = wait_for_queued_input(&host, &mut watch3, &fail_id, &CancellationToken::new())
                    .await
                    .expect_err("failed preceding turn must bail");
                assert!(fail_err.to_string().contains("upstream boom"), "{fail_err}");
                let q_after_fail = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                assert!(
                    !q_after_fail["data"].as_array().is_some_and(|a| a.iter().any(|x| x["input_id"] == fail_id)),
                    "failed preceding turn must delete parked input: {q_after_fail}"
                );

                // 4. Consumed queued prompt cancelled during `agent.wait` aborts ONLY its own turn
                //    while preserving an unrelated durable queue entry.
                let keep =
                    call_service(&host, "agent", "prompt", vec![json!("keep-other"), json!({ "when_busy": "queue" })])
                        .await
                        .unwrap();
                let keep_id = keep["input_id"].as_str().unwrap().to_string();
                let running_turn_id = first["turn_id"].as_str().unwrap().to_string();
                let log4 = mare_kernel::EventLog::new();
                let mut watch4 = log4.open_cursor(Arc::new(|_| Some(true)));
                log4.admit_json(
                    "item/completed",
                    json!({ "turn_id": running_turn_id, "item": { "message": { "input_id": "owned-consumed" } } }),
                )
                .await
                .unwrap();
                let cancel_tok = CancellationToken::new();
                let cancel_fire = cancel_tok.clone();
                let waiter = tokio::task::spawn_local(async move {
                    let _ = wait_for_queued_input(&host, &mut watch4, "owned-consumed", &cancel_tok).await;
                    let q_left = call_service(&host, "agent", "list_queue", vec![]).await.unwrap();
                    let kept = q_left["data"].as_array().is_some_and(|a| a.iter().any(|x| x["input_id"] == keep_id));
                    let _ = host.abort().await;
                    host.shutdown().await;
                    kept
                });
                tokio::task::yield_now().await;
                cancel_fire.cancel();
                let kept = tokio::time::timeout(Duration::from_secs(5), waiter)
                    .await
                    .expect("consumed queued turn cancel timed out")
                    .unwrap();
                assert!(kept, "cancelling consumed queued turn must preserve unrelated durable queue entries");
            })
            .await;
    }

    #[tokio::test]
    async fn wait_for_started_turn_aborts_immediately_when_token_already_cancelled() {
        tokio::task::LocalSet::new()
            .run_until(async {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join("should-not-exist.txt");
                let host = Host::new(mare_host::HostOptions {
                    cwd: dir.path().to_owned(),
                    mode: mare_plugin::Mode::Acp,
                    config: json!({
                        "model": "faux/echo",
                        "plugins": { "permissions": { "enabled": false } }
                    }),
                    load_init: false,
                    project_trusted: true,
                    args: json!({
                        "no_session": true,
                        "faux_script": [
                            { "tool_calls": [{ "name": "write", "arguments": { "path": "should-not-exist.txt", "content": "nope" } }] },
                            { "text": "completed" }
                        ]
                    }),
                    no_user_plugins: true,
                })
                .await
                .unwrap();

                let accepted =
                    host.prompt("start then cancel immediately", json!({ "source": "acp", "wait": false })).await.unwrap();
                assert_eq!(accepted["status"], "started", "{accepted}");
                let turn_id = accepted["turn_id"].as_str().unwrap().to_string();

                let already_cancelled = CancellationToken::new();
                already_cancelled.cancel();
                let res = wait_for_started_turn(&host, &turn_id, &already_cancelled).await;
                let stop = map_turn_result(&res).ok();
                assert_eq!(stop.as_deref(), Some("cancelled"), "pre-cancelled token must abort the started turn: {res:?}");
                assert!(!marker.exists(), "aborted turn must not execute tools");

                host.shutdown().await;
            })
            .await;
    }
}
