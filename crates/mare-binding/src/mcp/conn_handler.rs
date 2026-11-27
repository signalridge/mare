//! MCP client handler (roots, elicitation, list-changed, progress, and logging notifications)
//! and `mcp.log` writer.

// roots and logging are SEP-2577-deprecated in rmcp but still what servers speak today
#![allow(deprecated)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use rmcp::ClientHandler;
use rmcp::ErrorData as McpError;
use rmcp::model::{
    ClientCapabilities, ClientConfig, ElicitRequestParams, ElicitResult, Implementation, ListRootsResult,
    LoggingMessageNotificationParam, ProgressNotificationParam, Root,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient, RunningService};
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use mare_shared::redact::redact_str;

/// list_changed notifications are coalesced over this window.
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(300);
/// `mcp.log` moves to `mcp.log.1` past this size.
const LOG_MAX: u64 = 5 * 1024 * 1024;
/// How long an elicitation may wait for the user.
const ELICIT_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) type Service = RunningService<RoleClient, Handler>;

/// Events for the Lua plugin (`client:next_event()`).
#[derive(Debug)]
pub(crate) enum Event {
    ToolsChanged,
    PromptsChanged,
    ResourcesChanged,
    Reconnected,
    Elicit { id: u64, params: serde_json::Value, cancel: CancellationToken },
}

// ---- mcp.log ------------------------------------------------------------------------------

static LOG_LOCK: StdMutex<()> = StdMutex::new(());

pub(crate) fn log_path() -> PathBuf {
    mare_shared::paths::state_dir().join("mcp.log")
}

/// Append `<time> [<server>] <level>: <message>` (redacted) to `<state>/mcp.log`.
pub(crate) fn mcp_log(server: &str, level: &str, msg: &str) {
    use std::io::Write;
    let _g = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = log_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if std::fs::metadata(&path).map(|m| m.len() > LOG_MAX).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut o = std::fs::OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    if let Ok(mut f) = o.open(&path) {
        let _ = writeln!(f, "{secs} [{server}] {level}: {}", redact_str(msg));
    }
}

// ---- the client handler -------------------------------------------------------------------

/// RAII lease attached to a `ProgressNotification`'s extensions at transport receive time.
/// Ensures `ProgressTracker::end` runs even if `rmcp` drops the notification before
/// `Handler::on_progress` (such as during the `initialize` handshake or teardown).
#[derive(Clone)]
struct ProgressLease {
    _inner: Arc<ProgressLeaseInner>,
}

struct ProgressLeaseInner {
    tracker: Arc<ProgressTracker>,
    token: rmcp::model::ProgressToken,
    seq: u64,
}

impl Drop for ProgressLeaseInner {
    fn drop(&mut self) {
        self.tracker.end(&self.token, self.seq);
    }
}

/// Tracks in-flight `notifications/progress` tasks from transport receive through `on_progress`
/// completion so `call_tool` does not drain and return before a spawned handler finishes.
///
/// This tracker is per-connection (held by `Inner` / `Handler` / `ProgressTrackingTransport`)
/// and drops all state when the connection closes; completed token entries are removed
/// immediately when their in-flight set becomes empty.
#[derive(Default)]
pub(crate) struct ProgressTracker {
    next_seq: AtomicU64,
    tokens: StdMutex<HashMap<rmcp::model::ProgressToken, HashSet<u64>>>,
    notify: tokio::sync::Notify,
}

impl ProgressTracker {
    pub(crate) fn begin(&self, token: &rmcp::model::ProgressToken) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let mut map = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(token.clone()).or_default().insert(seq);
        seq
    }

    pub(crate) fn end(&self, token: &rmcp::model::ProgressToken, seq: u64) {
        let mut map = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(in_flight) = map.get_mut(token) {
            in_flight.remove(&seq);
            if in_flight.is_empty() {
                map.remove(token);
            }
        }
        drop(map);
        self.notify.notify_waiters();
    }

    pub(crate) fn clear(&self) {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.notify.notify_waiters();
    }

    pub(crate) fn barrier(&self, token: &rmcp::model::ProgressToken) -> Vec<u64> {
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(token)
            .map_or_else(Vec::new, |in_flight| in_flight.iter().copied().collect())
    }

    pub(crate) async fn wait_before(&self, token: &rmcp::model::ProgressToken, target: &[u64]) {
        if target.is_empty() {
            return;
        }
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            // Register before checking `in_flight` so a concurrent `end()` cannot be missed.
            notified.as_mut().enable();
            let done = {
                let map = self.tokens.lock().unwrap_or_else(|e| e.into_inner());
                map.get(token).is_none_or(|in_flight| target.iter().all(|seq| !in_flight.contains(seq)))
            };
            if done {
                return;
            }
            notified.await;
        }
    }

    #[cfg(test)]
    pub(crate) fn token_count(&self) -> usize {
        self.tokens.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Transport wrapper that records each `notifications/progress` before `rmcp` spawns its
/// notification handler, allowing `call_tool` to await handler completion deterministically.
pub(crate) struct ProgressTrackingTransport<T> {
    inner: T,
    tracker: Arc<ProgressTracker>,
    abort_rx: Option<CancellationToken>,
    #[cfg(test)]
    on_progress_received: Option<mpsc::UnboundedSender<rmcp::model::ProgressToken>>,
}

impl<T> ProgressTrackingTransport<T> {
    pub(crate) fn new(inner: T, tracker: Arc<ProgressTracker>) -> Self {
        Self {
            inner,
            tracker,
            abort_rx: None,
            #[cfg(test)]
            on_progress_received: None,
        }
    }

    pub(crate) fn with_abort(mut self, token: CancellationToken) -> Self {
        self.abort_rx = Some(token);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_progress_received_tx(
        inner: T,
        tracker: Arc<ProgressTracker>,
        tx: mpsc::UnboundedSender<rmcp::model::ProgressToken>,
    ) -> Self {
        Self { inner, tracker, abort_rx: None, on_progress_received: Some(tx) }
    }
}

impl<T: rmcp::transport::Transport<RoleClient>> rmcp::transport::Transport<RoleClient>
    for ProgressTrackingTransport<T>
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<rmcp::service::RxJsonRpcMessage<RoleClient>> {
        let mut msg = match &self.abort_rx {
            Some(tok) => tokio::select! {
                biased;
                _ = tok.cancelled() => return None,
                msg = self.inner.receive() => msg,
            },
            None => self.inner.receive().await,
        };
        if let Some(rmcp::model::JsonRpcMessage::Notification(ref mut n)) = msg
            && let rmcp::model::ServerNotification::ProgressNotification(ref mut p) = n.notification
        {
            let seq = self.tracker.begin(&p.params.progress_token);
            p.extensions.insert(ProgressLease {
                _inner: Arc::new(ProgressLeaseInner {
                    tracker: self.tracker.clone(),
                    token: p.params.progress_token.clone(),
                    seq,
                }),
            });
            #[cfg(test)]
            if let Some(ref tx) = self.on_progress_received {
                let _ = tx.send(p.params.progress_token.clone());
            }
        }
        msg
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

#[derive(Clone)]
pub(crate) struct Handler {
    pub(super) server: Arc<str>,
    pub(super) tx: mpsc::Sender<Event>,
    pub(super) closed: CancellationToken,
    pub(super) pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
    pub(super) next_id: Arc<AtomicU64>,
    pub(super) roots: Arc<Vec<Root>>,
    /// per kind (tools, prompts, resources): bumped on every notification
    pub(super) gens: Arc<[AtomicU64; 3]>,
    pub(super) debounce_slots: Arc<[DebounceSlot; 3]>,
    /// coalesced pending list_changed flags (tools, prompts, resources), never dropped on full queue
    pub(super) pending_changed: Arc<[AtomicBool; 3]>,
    pub(super) changed_notify: Arc<Notify>,
    pub(super) dispatch_gate: Arc<Mutex<()>>,
    pub(super) progress: broadcast::Sender<super::conn::ProgressUpdate>,
    pub(super) progress_tracker: Arc<ProgressTracker>,
    pub(super) human_waits: crate::human_wait::HumanWaits,
    #[cfg(test)]
    pub(super) active_debounce_tasks: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    pub(super) notification_seen: Option<mpsc::UnboundedSender<()>>,
    #[cfg(test)]
    pub(super) progress_seen: Option<mpsc::UnboundedSender<rmcp::model::ProgressToken>>,
    #[cfg(test)]
    pub(super) progress_hold: Option<(mpsc::UnboundedSender<()>, tokio::sync::watch::Receiver<bool>)>,
    #[cfg(test)]
    pub(super) progress_hold_per_notif: Option<mpsc::UnboundedSender<(Option<String>, oneshot::Sender<()>)>>,
}

struct DebounceState {
    deadline: tokio::time::Instant,
    running: bool,
}

pub(super) struct DebounceSlot {
    state: StdMutex<DebounceState>,
    notify: Notify,
}

impl Default for DebounceSlot {
    fn default() -> Self {
        Self {
            state: StdMutex::new(DebounceState { deadline: tokio::time::Instant::now(), running: false }),
            notify: Notify::new(),
        }
    }
}

fn client_config() -> ClientConfig {
    // roots (the workspace) and form elicitation; sampling is out of scope
    let caps: ClientCapabilities =
        serde_json::from_value(serde_json::json!({ "roots": {}, "elicitation": { "form": {} } })).unwrap_or_default();
    ClientConfig::new(caps, Implementation::new("mare", env!("CARGO_PKG_VERSION")))
}

impl Handler {
    pub(super) fn debounced(&self, kind: usize) {
        self.gens[kind].fetch_add(1, Ordering::SeqCst);
        let slot = &self.debounce_slots[kind];
        let should_spawn = {
            let mut st = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            st.deadline = tokio::time::Instant::now() + DEBOUNCE;
            if st.running {
                false
            } else {
                st.running = true;
                true
            }
        };
        if !should_spawn {
            slot.notify.notify_one();
            return;
        }
        let slots = self.debounce_slots.clone();
        let pending_changed = self.pending_changed.clone();
        let changed_notify = self.changed_notify.clone();
        let closed = self.closed.clone();
        #[cfg(test)]
        let active = self.active_debounce_tasks.clone();
        #[cfg(test)]
        active.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            #[cfg(test)]
            struct Guard(Arc<std::sync::atomic::AtomicUsize>);
            #[cfg(test)]
            impl Drop for Guard {
                fn drop(&mut self) {
                    self.0.fetch_sub(1, Ordering::SeqCst);
                }
            }
            #[cfg(test)]
            let _guard = Guard(active);

            let slot = &slots[kind];
            loop {
                let target = {
                    let mut st = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                    if tokio::time::Instant::now() >= st.deadline {
                        st.running = false;
                        break;
                    }
                    st.deadline
                };
                tokio::select! {
                    biased;
                    _ = closed.cancelled() => {
                        let mut st = slot.state.lock().unwrap_or_else(|e| e.into_inner());
                        st.running = false;
                        return;
                    }
                    _ = tokio::time::sleep_until(target) => {}
                    _ = slot.notify.notified() => {}
                }
            }
            if !closed.is_cancelled() {
                pending_changed[kind].store(true, Ordering::SeqCst);
                changed_notify.notify_one();
            }
        });
    }
}

// Remove the pending answer even when rmcp drops the request handler during teardown.
struct PendingElicitation {
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
    id: u64,
}

impl Drop for PendingElicitation {
    fn drop(&mut self) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.id);
    }
}

impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        client_config()
    }

    async fn list_roots(&self, _context: RequestContext<RoleClient>) -> Result<ListRootsResult, McpError> {
        Ok(ListRootsResult::new(self.roots.as_ref().clone()))
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, McpError> {
        let _wait = self.human_waits.enter();
        let cancelled = context.ct.child_token();
        let _cancel = cancelled.clone().drop_guard();
        let cancel = || serde_json::from_value::<ElicitResult>(serde_json::json!({ "action": "cancel" }));
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, tx);
        let _pending = PendingElicitation { pending: self.pending.clone(), id };
        let params = serde_json::to_value(&request).unwrap_or_default();
        let deadline = tokio::time::Instant::now() + ELICIT_TIMEOUT;
        let send_res = tokio::select! {
            res = tokio::time::timeout_at(deadline, self.tx.send(Event::Elicit { id, params, cancel: cancelled.clone() })) => res,
            _ = cancelled.cancelled() => return cancel().map_err(|e| McpError::internal_error(e.to_string(), None)),
            _ = self.closed.cancelled() => return cancel().map_err(|e| McpError::internal_error(e.to_string(), None)),
        };
        let Ok(Ok(())) = send_res else {
            return cancel().map_err(|e| McpError::internal_error(e.to_string(), None));
        };
        let answer = tokio::select! {
            result = tokio::time::timeout_at(deadline, rx) => result,
            _ = cancelled.cancelled() => return cancel().map_err(|e| McpError::internal_error(e.to_string(), None)),
            _ = self.closed.cancelled() => return cancel().map_err(|e| McpError::internal_error(e.to_string(), None)),
        };
        match answer {
            Ok(Ok(v)) => serde_json::from_value::<ElicitResult>(v)
                .or_else(|_| cancel())
                .map_err(|e| McpError::internal_error(e.to_string(), None)),
            _ => cancel().map_err(|e| McpError::internal_error(e.to_string(), None)),
        }
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        #[cfg(test)]
        if let Some(seen) = &self.notification_seen {
            let _ = seen.send(());
        }
        // Serialize notification invalidation with the final generation check and enqueue.
        let _gate = self.dispatch_gate.lock().await;
        self.debounced(0);
    }
    async fn on_progress(&self, params: ProgressNotificationParam, context: NotificationContext<RoleClient>) {
        let _lease = context.extensions.get::<ProgressLease>().cloned();
        let token = params.progress_token;
        #[cfg(test)]
        if let Some((entered, hold)) = &self.progress_hold {
            let _ = entered.send(());
            let mut hold = hold.clone();
            let _ = hold.wait_for(|ready| *ready).await;
        }
        #[cfg(test)]
        if let Some(gate_tx) = &self.progress_hold_per_notif {
            let (release_tx, release_rx) = oneshot::channel();
            let _ = gate_tx.send((params.message.clone(), release_tx));
            let _ = release_rx.await;
        }
        let _ = self.progress.send(super::conn::ProgressUpdate {
            token: token.clone(),
            progress: params.progress,
            total: params.total,
            message: params.message,
        });
        #[cfg(test)]
        if let Some(seen) = &self.progress_seen {
            let _ = seen.send(token.clone());
        }
    }
    async fn on_prompt_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.debounced(1);
    }
    async fn on_resource_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.debounced(2);
    }
    async fn on_logging_message(
        &self,
        params: LoggingMessageNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        let v = serde_json::to_value(&params).unwrap_or_default();
        let level = v.get("level").and_then(|l| l.as_str()).unwrap_or("info").to_string();
        let logger = v.get("logger").and_then(|l| l.as_str()).map(|l| format!("{l}: ")).unwrap_or_default();
        let data = match v.get("data") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(d) => d.to_string(),
            None => String::new(),
        };
        mcp_log(&self.server, &level, &format!("{logger}{data}"));
    }
}
