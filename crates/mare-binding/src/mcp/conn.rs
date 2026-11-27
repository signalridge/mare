//! One MCP server connection: lifecycle (`Inner`), reconnection, and `tools/call` dispatch
//! with progress-resetting idle timeouts and cancellation.
//!
//! HTTP initialize retries on transient statuses with delays (250 ms, 1 s) (Apache-2.0),
//! and rmcp's response cache is disabled so a refreshed list is really fetched again.
//! Page cap and list budget (MIT).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CancelledNotificationParam, ClientRequest, ProgressToken, ServerResult,
};
use rmcp::service::{ClientCacheConfig, Peer, PeerRequestOptions, RoleClient};
use tokio::sync::{Mutex, Notify, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::conn_handler::{DebounceSlot, Handler, ProgressTracker, Service};
use super::conn_transport::{
    AbandonedHandshakeGuard, ChildProc, Listings, connect_http_with_guard, prefetch, roots_of, spawn_stdio,
};
#[cfg(test)]
use super::conn_transport::{DEFAULT_STARTUP, MAX_STARTUP, env_value, is_retryable_http};
use mare_shared::redact::redact_str;

pub(crate) use super::conn_handler::{Event, log_path, mcp_log};
pub(crate) use super::conn_transport::Spec;

/// Bounded per-connection progress fanout; lagging calls fail rather than missing progress.
pub(crate) const PROGRESS_CAPACITY: usize = 4096;
/// Cancel notifications are best effort; a stalled transport must not retain a task forever.
const CANCEL_SEND_TIMEOUT: Duration = Duration::from_secs(5);
/// Transport close grace period before killing the MCP stdio child.
const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

struct Conn {
    svc: Option<Service>,
    proc: Option<ChildProc>,
    info: serde_json::Value,
    listings: Listings,
    connects: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct ProgressUpdate {
    pub token: ProgressToken,
    pub progress: f64,
    pub total: Option<f64>,
    pub message: Option<String>,
}

const EVENT_CAPACITY: usize = 256;

pub(crate) struct Inner {
    pub spec: Spec,
    conn: Mutex<Conn>,
    tx: mpsc::Sender<Event>,
    rx: Mutex<mpsc::Receiver<Event>>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
    next_id: Arc<AtomicU64>,
    gens: Arc<[AtomicU64; 3]>,
    debounce_slots: Arc<[DebounceSlot; 3]>,
    pending_changed: Arc<[AtomicBool; 3]>,
    changed_notify: Arc<Notify>,
    poll_cursor: AtomicUsize,
    dispatch_gate: Arc<Mutex<()>>,
    progress: broadcast::Sender<ProgressUpdate>,
    progress_tracker: Arc<ProgressTracker>,
    human_waits: crate::human_wait::HumanWaits,
    pub stderr: Arc<StdMutex<VecDeque<String>>>,
    closed: CancellationToken,
    reconnected: AtomicBool,
    reconnected_notify: Notify,
    restart_changed_gen: AtomicU64,
    /// the host whose shutdown ends a stdio server's tree
    owner: Option<crate::trees::Owner>,
    #[cfg(test)]
    pub(crate) response_received: StdMutex<Option<mpsc::UnboundedSender<()>>>,
    #[cfg(test)]
    active_debounce_tasks: Arc<AtomicUsize>,
}

impl Inner {
    pub(crate) fn new(spec: Spec, owner: Option<crate::trees::Owner>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(EVENT_CAPACITY);
        let (progress, _) = broadcast::channel(PROGRESS_CAPACITY);
        Arc::new(Self {
            spec,
            conn: Mutex::new(Conn {
                svc: None,
                proc: None,
                info: serde_json::Value::Null,
                listings: Listings::default(),
                connects: 0,
            }),
            tx,
            rx: Mutex::new(rx),
            pending: Default::default(),
            next_id: Arc::new(AtomicU64::new(1)),
            gens: Arc::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]),
            debounce_slots: Arc::new([DebounceSlot::default(), DebounceSlot::default(), DebounceSlot::default()]),
            pending_changed: Arc::new([AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false)]),
            changed_notify: Arc::new(Notify::new()),
            poll_cursor: AtomicUsize::new(0),
            dispatch_gate: Arc::new(Mutex::new(())),
            progress,
            progress_tracker: Arc::new(ProgressTracker::default()),
            human_waits: Default::default(),
            stderr: Default::default(),
            closed: CancellationToken::new(),
            reconnected: AtomicBool::new(false),
            reconnected_notify: Notify::new(),
            restart_changed_gen: AtomicU64::new(0),
            owner,
            #[cfg(test)]
            response_received: StdMutex::new(None),
            #[cfg(test)]
            active_debounce_tasks: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn handler(&self) -> Handler {
        Handler {
            server: Arc::from(self.spec.name.as_str()),
            tx: self.tx.clone(),
            closed: self.closed.clone(),
            pending: self.pending.clone(),
            next_id: self.next_id.clone(),
            roots: Arc::new(roots_of(&self.spec)),
            gens: self.gens.clone(),
            debounce_slots: self.debounce_slots.clone(),
            pending_changed: self.pending_changed.clone(),
            changed_notify: self.changed_notify.clone(),
            dispatch_gate: self.dispatch_gate.clone(),
            progress: self.progress.clone(),
            progress_tracker: self.progress_tracker.clone(),
            human_waits: self.human_waits.clone(),
            #[cfg(test)]
            active_debounce_tasks: self.active_debounce_tasks.clone(),
            #[cfg(test)]
            notification_seen: None,
            #[cfg(test)]
            progress_seen: None,
            #[cfg(test)]
            progress_hold: None,
            #[cfg(test)]
            progress_hold_per_notif: None,
        }
    }

    async fn wait_cancel(&self, cancel: Option<&CancellationToken>) {
        match cancel {
            Some(c) => {
                tokio::select! {
                    _ = self.closed.cancelled() => {}
                    _ = c.cancelled() => {}
                }
            }
            None => self.closed.cancelled().await,
        }
    }

    /// initialize + prefetch, all within the startup timeout
    async fn open(&self, conn: &mut Conn, cancel: Option<&CancellationToken>) -> Result<(), String> {
        if self.closed.is_cancelled() || cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err("MCP connection cancelled".into());
        }
        let deadline = Instant::now() + self.spec.startup;
        let timeout_msg = || format!("MCP server did not initialize within {}ms", self.spec.startup.as_millis());
        let handler = self.handler();
        let fut = async {
            let handshake_guard = AbandonedHandshakeGuard::new();
            let (mut svc, mut proc) = if self.spec.str("command").is_some() {
                let (s, p) = spawn_stdio(&self.spec, handler, &self.stderr, self.owner.clone(), cancel).await?;
                (s, Some(p))
            } else {
                let s = tokio::select! {
                    biased;
                    _ = self.wait_cancel(cancel) => return Err("MCP connection cancelled".into()),
                    res = connect_http_with_guard(&self.spec, handler, deadline, &handshake_guard) => res?,
                };
                (s, None)
            };
            // rmcp 3 caches list responses and serves stale ones on errors: a refresh must
            // really ask the server
            let prefetch_res: Result<Listings, String> = tokio::select! {
                biased;
                _ = self.wait_cancel(cancel) => Err("MCP connection cancelled".into()),
                res = async {
                    svc.peer().set_response_cache_config(ClientCacheConfig::disabled()).await;
                    let generation = self.tools_generation();
                    let mut listings = prefetch(&svc, &self.spec.name).await?;
                    if self.tools_generation() != generation {
                        return Err("MCP tools changed while listing; retry the connection".to_string());
                    }
                    listings.tools_generation = generation;
                    Ok(listings)
                } => res,
            };
            let listings = match prefetch_res {
                Ok(l) => l,
                Err(e) => {
                    let _ = svc.close_with_timeout(CLOSE_TIMEOUT).await;
                    handshake_guard.cleanup_now().await;
                    if let Some(p) = proc.take() {
                        p.stop().await;
                    }
                    return Err(e);
                }
            };
            handshake_guard.disarm();
            Ok::<_, String>((svc, proc, listings))
        };
        let (svc, proc, listings) = match tokio::time::timeout(self.spec.startup, fut).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                self.progress_tracker.clear();
                return Err(e);
            }
            Err(_) => {
                self.progress_tracker.clear();
                return Err(timeout_msg());
            }
        };
        conn.info = svc.peer_info().map(|i| serde_json::to_value(i.as_ref()).unwrap_or_default()).unwrap_or_default();
        conn.svc = Some(svc);
        conn.proc = proc;
        conn.listings = listings;
        conn.connects += 1;
        Ok(())
    }

    async fn teardown(&self, conn: &mut Conn) {
        if let Some(mut svc) = conn.svc.take() {
            let _ = svc.close_with_timeout(CLOSE_TIMEOUT).await;
        }
        if let Some(p) = conn.proc.take() {
            p.stop().await;
        }
        self.progress_tracker.clear();
    }

    fn alive(conn: &Conn) -> bool {
        conn.svc.as_ref().is_some_and(|s| !s.is_closed() && !s.peer().is_transport_closed())
    }

    /// First connect (errors are the caller's to report).
    #[cfg(test)]
    pub(crate) async fn start(&self) -> Result<(), String> {
        self.start_with_cancel(None).await
    }

    /// First connect (errors are the caller's to report), optionally aborting on `cancel`.
    pub(crate) async fn start_with_cancel(&self, cancel: Option<&CancellationToken>) -> Result<(), String> {
        let mut conn = self.conn.lock().await;
        self.open(&mut conn, cancel).await.map_err(|e| redact_str(&e))
    }

    /// The peer of a live connection, reconnecting first when the server went away (stdio
    /// process exited, HTTP session gone and not recoverable).
    pub(crate) async fn peer(&self, allow_reconnect: bool) -> Result<Peer<RoleClient>, String> {
        if self.closed.is_cancelled() {
            return Err("MCP client is closed".into());
        }
        let mut conn = self.conn.lock().await;
        if Self::alive(&conn) {
            return Ok(conn.svc.as_ref().map(|s| s.peer().clone()).expect("alive"));
        }
        if !allow_reconnect {
            return Err("sandbox: MCP reconnect cannot be safely confined with deny_read".into());
        }
        let had = conn.connects > 0;
        let old_tools = had.then(|| conn.listings.tools.clone());
        self.teardown(&mut conn).await;
        if had {
            mcp_log(&self.spec.name, "info", "connection lost; reconnecting");
        }
        self.open(&mut conn, None).await.map_err(|e| redact_str(&e))?;
        if let Some(prev_tools) = old_tools {
            if conn.listings.tools != prev_tools {
                let _gate = self.dispatch_gate.lock().await;
                let new_gen = self.gens[0].fetch_add(1, Ordering::SeqCst) + 1;
                conn.listings.tools_generation = new_gen;
                self.restart_changed_gen.store(new_gen, Ordering::SeqCst);
            }
            self.reconnected.store(true, Ordering::SeqCst);
            self.reconnected_notify.notify_one();
        }
        Ok(conn.svc.as_ref().map(|s| s.peer().clone()).expect("connected"))
    }

    /// `/mcp reconnect`: drop the connection and open a new one.
    pub(crate) async fn reconnect(&self) -> Result<(), String> {
        let mut conn = self.conn.lock().await;
        {
            let _gate = self.dispatch_gate.lock().await;
            self.gens[0].fetch_add(1, Ordering::SeqCst);
        }
        self.teardown(&mut conn).await;
        self.open(&mut conn, None).await.map_err(|e| redact_str(&e))?;
        self.reconnected.store(true, Ordering::SeqCst);
        self.reconnected_notify.notify_one();
        Ok(())
    }

    pub(crate) async fn close(&self) {
        self.closed.cancel();
        let mut conn = self.conn.lock().await;
        self.teardown(&mut conn).await;
        for (_, tx) in self.pending.lock().unwrap_or_else(|e| e.into_inner()).drain() {
            let _ = tx.send(serde_json::json!({ "action": "cancel" }));
        }
    }

    pub(crate) async fn status(&self) -> &'static str {
        if self.closed.is_cancelled() {
            return "closed";
        }
        let conn = self.conn.lock().await;
        if Self::alive(&conn) { "connected" } else { "disconnected" }
    }

    pub(crate) async fn info(&self) -> serde_json::Value {
        self.conn.lock().await.info.clone()
    }

    pub(crate) async fn listings(&self) -> Listings {
        self.conn.lock().await.listings.clone()
    }

    /// Changes immediately when a tools/list_changed notification is received, before debounce.
    pub(crate) fn tools_generation(&self) -> u64 {
        self.gens[0].load(Ordering::SeqCst)
    }

    /// List everything again on the live connection (reconnecting first when needed).
    pub(crate) async fn refresh(&self) -> Result<(), String> {
        self.peer(true).await?;
        let mut conn = self.conn.lock().await;
        let Some(svc) = conn.svc.as_ref() else { return Err("MCP server is not connected".into()) };
        let generation = self.tools_generation();
        let mut listings = tokio::time::timeout(self.spec.startup, prefetch(svc, &self.spec.name))
            .await
            .map_err(|_| format!("listing tools did not finish within {}ms", self.spec.startup.as_millis()))??;
        if self.tools_generation() != generation {
            return Err("MCP tools changed while listing; retry the refresh".into());
        }
        listings.tools_generation = generation;
        conn.listings = listings;
        Ok(())
    }

    pub(crate) fn stderr_tail(&self) -> Vec<String> {
        self.stderr.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect()
    }

    fn take_next_fair(&self, rx: &mut mpsc::Receiver<Event>) -> Option<Event> {
        const CHANGED: [fn() -> Event; 3] =
            [|| Event::ToolsChanged, || Event::PromptsChanged, || Event::ResourcesChanged];
        let start = self.poll_cursor.fetch_add(1, Ordering::Relaxed) % 4;
        for offset in 0..4 {
            let slot = (start + offset) % 4;
            if slot == 0 {
                if let Ok(ev) = rx.try_recv() {
                    return Some(ev);
                }
            } else {
                let idx = slot - 1;
                if self.pending_changed[idx].swap(false, Ordering::SeqCst) {
                    return Some(CHANGED[idx]());
                }
            }
        }
        None
    }

    pub(crate) async fn next_event(&self) -> Option<Event> {
        if self.closed.is_cancelled() {
            return None;
        }
        let mut rx = self.rx.lock().await;
        loop {
            if self.closed.is_cancelled() {
                return None;
            }
            if self.reconnected.swap(false, Ordering::SeqCst) {
                return Some(Event::Reconnected);
            }
            if let Some(ev) = self.take_next_fair(&mut rx) {
                return Some(ev);
            }
            tokio::select! {
                _ = self.closed.cancelled() => return None,
                _ = self.reconnected_notify.notified() => {
                    if self.reconnected.swap(false, Ordering::SeqCst) {
                        return Some(Event::Reconnected);
                    }
                }
                _ = self.changed_notify.notified() => {
                    if let Some(ev) = self.take_next_fair(&mut rx) {
                        return Some(ev);
                    }
                }
                ev = rx.recv() => return ev,
            }
        }
    }

    pub(crate) fn respond(&self, id: u64, v: serde_json::Value) -> bool {
        match self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id) {
            Some(tx) => tx.send(v).is_ok(),
            None => false,
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // ChildProc's tree ends the process group
        let conn = self.conn.get_mut();
        conn.svc.take();
        conn.proc.take();
    }
}

/// Sends `notifications/cancelled` when a call is abandoned (the user interrupted, the tool
/// call's task was dropped). Timeouts are cancelled by rmcp itself.
struct CancelGuard {
    peer: Option<Peer<RoleClient>>,
    id: rmcp::model::RequestId,
    reason: &'static str,
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        let Some(peer) = self.peer.take() else { return };
        let param = CancelledNotificationParam::new(Some(self.id.clone()), Some(self.reason.into()));
        if let Ok(h) = tokio::runtime::Handle::try_current() {
            h.spawn(async move {
                let _ = tokio::time::timeout(CANCEL_SEND_TIMEOUT, peer.notify_cancelled(param)).await;
            });
        }
    }
}

/// tools/call with the call timeout; a progress notification resets it, capped
/// at ten times the timeout in active time. Human callback waits pause both budgets.
pub(crate) async fn call_tool(
    peer: Peer<RoleClient>,
    name: String,
    args: serde_json::Map<String, serde_json::Value>,
    timeout: Duration,
    inner: &Inner,
    expected_generation: Option<u64>,
) -> Result<serde_json::Value, String> {
    call_tool_with_progress(peer, name, args, timeout, inner, expected_generation, None).await
}

pub(crate) async fn call_tool_with_progress(
    peer: Peer<RoleClient>,
    name: String,
    args: serde_json::Map<String, serde_json::Value>,
    timeout: Duration,
    inner: &Inner,
    expected_generation: Option<u64>,
    on_progress: Option<mpsc::Sender<ProgressUpdate>>,
) -> Result<serde_json::Value, String> {
    call_tool_with_before_submit(
        peer,
        name,
        args,
        timeout,
        inner,
        expected_generation,
        on_progress,
        std::future::ready(()),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn call_tool_with_before_submit<F: std::future::Future<Output = ()>>(
    peer: Peer<RoleClient>,
    name: String,
    args: serde_json::Map<String, serde_json::Value>,
    timeout: Duration,
    inner: &Inner,
    expected_generation: Option<u64>,
    on_progress: Option<mpsc::Sender<ProgressUpdate>>,
    before_submit: F,
) -> Result<serde_json::Value, String> {
    let params = CallToolRequestParams::new(name.clone()).with_arguments(args);
    // rmcp registers a private progress watcher before awaiting queue capacity and does not
    // clean it if that future is dropped. Track progress ourselves with a bounded receiver.
    let opts = PeerRequestOptions::no_options();
    let gate = inner.dispatch_gate.lock().await;
    let current_gen = inner.tools_generation();
    if expected_generation.is_some_and(|g| current_gen != g) {
        if inner.restart_changed_gen.load(Ordering::SeqCst) == current_gen {
            return Err(
                "MCP server restarted and tool definition changed before dispatch; tool list refreshed, please retry"
                    .into(),
            );
        }
        return Err("MCP tool definition changed before dispatch".into());
    }
    let mut progress = inner.progress.subscribe();
    before_submit.await;
    // `send_request_with_option` returns once rmcp enqueues the request, before the response.
    // Its preparation and send may await; hold the same gate as notification invalidation until
    // this submission point, then release it while the tool runs.
    let handle = tokio::time::timeout(
        timeout,
        peer.send_request_with_option(ClientRequest::CallToolRequest(CallToolRequest::new(params)), opts),
    )
    .await
    .map_err(|_| format!("tools/call {name} could not be submitted within {}ms", timeout.as_millis()))?
    .map_err(|e| redact_str(&e.to_string()))?;
    drop(gate);
    let mut guard = CancelGuard { peer: Some(peer.clone()), id: handle.id.clone(), reason: "cancelled by the user" };
    let token = handle.progress_token.clone();
    let response = handle.await_response();
    tokio::pin!(response);
    let clock = &inner.human_waits;
    let mut idle_deadline = clock.active_time() + timeout;
    let total_deadline = clock.active_time() + timeout.saturating_mul(10);
    let res = loop {
        tokio::select! {
            biased;
            result = &mut response => break result.map_err(|e| redact_str(&e.to_string())),
            _ = clock.until(idle_deadline) => {
                guard.reason = "tool request timed out";
                return Err(format!("tools/call {name} timed out after {}ms", timeout.as_millis()));
            }
            _ = clock.until(total_deadline) => {
                guard.reason = "tool request exceeded its total timeout";
                return Err(format!("tools/call {name} exceeded its total timeout after {}ms", timeout.saturating_mul(10).as_millis()));
            }
            event = progress.recv() => match event {
                Ok(update) if update.token == token => {
                    idle_deadline = clock.active_time() + timeout;
                    if let Some(tx) = &on_progress {
                        let _ = tx.send(update).await;
                    }
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    guard.reason = "tool progress notifications overrun";
                    return Err("MCP tool progress notifications overrun; call cancelled".into());
                }
                Err(broadcast::error::RecvError::Closed) => {
                    guard.reason = "tool progress channel closed";
                    return Err("MCP tool progress channel closed; call cancelled".into());
                }
            }
        }
    };
    #[cfg(test)]
    if let Some(tx) = inner.response_received.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        let _ = tx.send(());
    }
    if let Some(tx) = &on_progress {
        // Wait until every `on_progress` task spawned by rmcp before the response has
        // pushed its update into `inner.progress`, bounded by total timeout and shutdown.
        let barrier = inner.progress_tracker.barrier(&token);
        tokio::select! {
            biased;
            _ = inner.progress_tracker.wait_before(&token, &barrier) => {}
            _ = clock.until(total_deadline) => {
                guard.reason = "tool request exceeded its total timeout";
                return Err(format!(
                    "tools/call {name} exceeded its total timeout after {}ms",
                    timeout.saturating_mul(10).as_millis()
                ));
            }
            _ = inner.closed.cancelled() => {
                guard.reason = "MCP client closed";
                return Err("MCP client closed".into());
            }
        }
        loop {
            match progress.try_recv() {
                Ok(update) => {
                    if update.token == token {
                        tokio::select! {
                            biased;
                            sent = tx.send(update) => {
                                if sent.is_err() {
                                    break;
                                }
                            }
                            _ = clock.until(total_deadline) => {
                                guard.reason = "tool request exceeded its total timeout";
                                return Err(format!(
                                    "tools/call {name} exceeded its total timeout after {}ms",
                                    timeout.saturating_mul(10).as_millis()
                                ));
                            }
                            _ = inner.closed.cancelled() => {
                                guard.reason = "MCP client closed";
                                return Err("MCP client closed".into());
                            }
                        }
                    }
                }
                Err(broadcast::error::TryRecvError::Empty) | Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => {
                    guard.reason = "tool progress notifications overrun";
                    return Err("MCP tool progress notifications overrun; call cancelled".into());
                }
            }
        }
    }
    guard.peer = None;
    match res {
        Ok(ServerResult::CallToolResult(r)) => serde_json::to_value(r).map_err(|e| e.to_string()),
        Ok(_) => Err("unexpected response to tools/call".into()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[path = "conn_tests/mod.rs"]
mod tests;
