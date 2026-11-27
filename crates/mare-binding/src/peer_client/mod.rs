//! A bounded peer RPC client. Receipts describe remote delivery, never agent completion.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::net_unix::UnixConn;

mod lua;
#[cfg(all(test, unix))]
mod tests;
pub use lua::install;

const MAX_PENDING: usize = 32;
const MAX_RECEIPTS: usize = 128;
const MAX_FRAME: usize = 1 << 20;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientError {
    pub code: String,
    pub message: String,
}
impl ClientError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into() }
    }
}
type Result<T> = std::result::Result<T, ClientError>;

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Target {
    Name(String),
    Instance(Instance),
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instance {
    pub pid: u32,
    pub started_at_ms: u64,
}

/// All waits are bounded by default; None is explicit unbounded waiting.
#[derive(Clone)]
pub struct WaitOptions {
    pub timeout: Option<Duration>,
    pub cancel: CancellationToken,
}
impl Default for WaitOptions {
    fn default() -> Self {
        Self { timeout: Some(Duration::from_secs(5)), cancel: CancellationToken::new() }
    }
}

struct State {
    next: u64,
    pending: HashMap<u64, oneshot::Sender<Result<Value>>>,
    receipts: HashMap<String, String>,
    receipt_details: HashMap<String, Value>,
    issued: HashSet<String>,
    subscribed_session: Option<Option<String>>,
    idle_gen: u64,
    latest_idle: Option<(Option<String>, Option<u64>, u64, Value)>,
    error: Option<ClientError>,
}
struct Inner {
    conn: UnixConn,
    path: String,
    state: Mutex<State>,
    subscribe_gate: tokio::sync::Mutex<()>,
    changed: watch::Sender<u64>,
    finished: watch::Sender<bool>,
    closed: CancellationToken,
    reader: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}
struct Lease(CancellationToken);
impl Drop for Lease {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
#[derive(Clone)]
pub struct PeerClient {
    inner: Arc<Inner>,
    _lease: Arc<Lease>,
    /// The session cell of the VM this client was created for, read at send time.
    session: crate::peers::SessionCell,
    pub info: Value,
}

fn resolve(target: &Target) -> Result<Value> {
    let entries: Vec<_> = crate::peers::list()
        .into_iter()
        .filter(|entry| match target {
            Target::Name(name) => entry["name"].as_str() == Some(name.as_str()),
            Target::Instance(i) => {
                entry["pid"].as_u64() == Some(i.pid.into()) && entry["started_at_ms"].as_u64() == Some(i.started_at_ms)
            }
        })
        .collect();
    match entries.as_slice() {
        [] => Err(ClientError::new("invalid_target", "No live registered peer matches the target")),
        [entry] if entry["pid"].as_u64() == Some(std::process::id().into()) => {
            Err(ClientError::new("self", "Cannot connect to this process as a peer"))
        }
        [entry] => Ok(entry.clone()),
        _ => Err(ClientError::new(
            "ambiguous_target",
            "Use a pid and started_at_ms instance to disambiguate the registered name",
        )),
    }
}

impl PeerClient {
    pub fn pin_target(target: &Target) -> Result<(Target, String)> {
        let entry = resolve(target)?;
        let path = entry["socket"].as_str().ok_or_else(|| ClientError::new("invalid_target", "Peer has no socket"))?;
        let pid = entry["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .ok_or_else(|| ClientError::new("invalid_target", "Peer has no process identity"))?;
        let started_at_ms = entry["started_at_ms"]
            .as_u64()
            .ok_or_else(|| ClientError::new("invalid_target", "Peer has no instance identity"))?;
        Ok((Target::Instance(Instance { pid, started_at_ms }), path.to_string()))
    }

    pub async fn connect(
        target: Target,
        expected_path: String,
        opts: WaitOptions,
        session: crate::peers::SessionCell,
    ) -> Result<Self> {
        let entry = resolve(&target)?;
        let path = entry["socket"].as_str().ok_or_else(|| ClientError::new("invalid_target", "Peer has no socket"))?;
        if path != expected_path {
            return Err(ClientError::new("target_changed", "Peer endpoint changed after authorization"));
        }
        let conn = wait(&opts, crate::net_unix::connect(path, opts.timeout.unwrap_or(Duration::from_secs(5))))
            .await?
            .map_err(|e| ClientError::new("unavailable", e))?;
        let mut client = Self::from_connection(conn, path.to_string(), session);
        let handshake = async {
            let _init = client
                .request(
                    "initialize",
                    json!({"protocol_version": mare_protocol::PROTOCOL_VERSION, "capabilities": {"ui": false}}),
                    &opts,
                )
                .await?;
            let info = client.request("peer.info", json!({}), &opts).await?;
            if info["pid"] != entry["pid"] || info["name"] != entry["name"] {
                return Err(ClientError::new(
                    "target_changed",
                    "Peer identity changed during connection; rediscover the target",
                ));
            }
            Ok(info)
        }
        .await;
        match handshake {
            Ok(info) => {
                client.info = info;
                Ok(client)
            }
            Err(error) => {
                client.close().await;
                Err(error)
            }
        }
    }

    fn from_connection(conn: UnixConn, path: String, session: crate::peers::SessionCell) -> Self {
        let (changed, _) = watch::channel(0);
        let (finished, _) = watch::channel(false);
        let closed = CancellationToken::new();
        let inner = Arc::new(Inner {
            conn,
            path,
            changed,
            finished,
            closed: closed.clone(),
            state: Mutex::new(State {
                next: 1,
                pending: HashMap::new(),
                receipts: HashMap::new(),
                receipt_details: HashMap::new(),
                issued: HashSet::new(),
                subscribed_session: None,
                idle_gen: 0,
                latest_idle: None,
                error: None,
            }),
            subscribe_gate: tokio::sync::Mutex::new(()),
            reader: tokio::sync::Mutex::new(None),
        });
        let reader = inner.clone();
        let task = mare_shared::txn_scope::spawn_local(async move { reader.run().await });
        *inner.reader.try_lock().expect("new client reader lock") = Some(task);
        Self { inner, _lease: Arc::new(Lease(closed)), session, info: Value::Null }
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_cancelled()
    }

    pub fn path(&self) -> &str {
        &self.inner.path
    }

    pub async fn close(&self) {
        let mut finished = self.inner.finished.subscribe();
        self.inner.closed.cancel();
        let task = self.inner.reader.lock().await.take();
        if let Some(task) = task
            && task.await.is_err()
        {
            self.inner.conn.close().await;
            let mut state = self.inner.state.lock().unwrap_or_else(|error| error.into_inner());
            let error = ClientError::new("closed", "Peer reader terminated");
            state.error = Some(error.clone());
            for (_, reply) in state.pending.drain() {
                let _ = reply.send(Err(error.clone()));
            }
            self.inner.changed.send_modify(|n| *n += 1);
            self.inner.finished.send_replace(true);
        }
        while !*finished.borrow_and_update() {
            if finished.changed().await.is_err() {
                break;
            }
        }
    }

    pub async fn status(&self, opts: WaitOptions) -> Result<Value> {
        self.request("status.get", json!({}), &opts).await
    }

    pub async fn send(&self, text: String, queue_only: bool, opts: WaitOptions) -> Result<Value> {
        if text.trim().is_empty() {
            return Err(ClientError::new("invalid_params", "Message text must not be empty"));
        }
        if queue_only
            && !self.info["delivery_modes"]
                .as_array()
                .is_some_and(|modes| modes.iter().any(|mode| mode == "queue_only"))
        {
            return Err(ClientError::new("unsupported", "Peer does not advertise queue_only delivery"));
        }
        if self.inner.state.lock().unwrap().receipts.len() >= MAX_RECEIPTS {
            return Err(ClientError::new("capacity", "Close this client and rediscover before sending more messages"));
        }
        let me = crate::peers::self_entry().unwrap_or(json!({}));
        let mut params = json!({"text": text, "from": {"name": me.get("name"), "pid": std::process::id(), "session_id": self.session.get(), "cwd": std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string())}});
        if queue_only {
            params["queue_only"] = json!(true);
        }
        let result = self.request("peer.message", params, &opts).await?;
        let id = result["message_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ClientError::new("outcome_unknown", "Peer returned an invalid message receipt"))?;
        let status = result["status"]
            .as_str()
            .filter(|s| matches!(*s, "held" | "handled"))
            .ok_or_else(|| ClientError::new("outcome_unknown", "Peer returned an invalid initial delivery status"))?;
        let mut state = self.inner.state.lock().unwrap();
        if state.issued.contains(id) {
            self.inner.closed.cancel();
            return Err(ClientError::new("outcome_unknown", "Peer reused a message receipt id; do not retry"));
        }
        if !state.receipts.contains_key(id) && state.receipts.len() >= MAX_RECEIPTS {
            self.inner.closed.cancel();
            return Err(ClientError::new("outcome_unknown", "Receipt capacity reached after sending; do not retry"));
        }
        state.issued.insert(id.to_string());
        state.receipts.entry(id.to_string()).or_insert_with(|| status.to_string());
        Ok(result)
    }

    pub async fn wait_message(&self, id: &str, opts: WaitOptions) -> Result<Value> {
        let mut changed = self.inner.changed.subscribe();
        wait(&opts, async {
            loop {
                {
                    let state = self.inner.state.lock().unwrap();
                    let status = state
                        .receipts
                        .get(id)
                        .ok_or_else(|| ClientError::new("invalid_message", "Message does not belong to this client"))?;
                    if status != "held" {
                        return Ok(state
                            .receipt_details
                            .get(id)
                            .cloned()
                            .unwrap_or_else(|| json!({"message_id": id, "status": status})));
                    }
                    if let Some(error) = &state.error {
                        return Err(ClientError::new("outcome_unknown", &error.message));
                    }
                }
                changed.changed().await.map_err(|_| ClientError::new("closed", "Client closed"))?;
            }
        })
        .await?
    }

    async fn ensure_status_subscribed(&self, target_sid: Option<&str>, opts: &WaitOptions) -> Result<()> {
        let desired = target_sid.map(str::to_owned);
        if self.inner.state.lock().unwrap().subscribed_session.as_ref() == Some(&desired) {
            return Ok(());
        }
        let _guard = wait(opts, self.inner.subscribe_gate.lock()).await?;
        if self.inner.state.lock().unwrap().subscribed_session.as_ref() == Some(&desired) {
            return Ok(());
        }
        let mut params = json!({ "include": ["status/changed"] });
        if let Some(sid) = target_sid {
            params["session_id"] = Value::String(sid.to_owned());
        }
        self.request("events.subscribe", params, opts).await?;
        let mut state = self.inner.state.lock().unwrap();
        if state.subscribed_session.as_ref() != Some(&desired) {
            state.latest_idle = None;
            state.subscribed_session = Some(desired);
        }
        Ok(())
    }

    /// Wait for the peer to report `idle` or `exited` via `events.subscribe` (`status/changed`)
    /// and `status.get`, ordered by monotonic `seq` within the remote's active session.
    pub async fn wait_idle(&self, current_or_next: bool, opts: WaitOptions) -> Result<Value> {
        if let Some(methods) = self.info["allowed"].as_array()
            && !methods.iter().any(|m| m == "events.subscribe")
        {
            return Err(ClientError::new("unsupported", "Peer does not advertise events.subscribe"));
        }
        if opts.cancel.is_cancelled() {
            return Err(ClientError::new("cancelled", "Cancelled before waiting for idle"));
        }
        let start_gen = self.inner.state.lock().unwrap().idle_gen;
        let mut changed = self.inner.changed.subscribe();
        let operation = async {
            let mut snap = self.status(opts.clone()).await?;
            let mut snap_sid = status_session_id(&snap);
            let subscribed_before = self.inner.state.lock().unwrap().subscribed_session.clone();
            if subscribed_before.as_ref() != Some(&snap_sid) {
                self.ensure_status_subscribed(snap_sid.as_deref(), &opts).await?;
                snap = self.status(opts.clone()).await?;
                let after_sid = status_session_id(&snap);
                if after_sid != snap_sid {
                    snap_sid = after_sid;
                    self.ensure_status_subscribed(snap_sid.as_deref(), &opts).await?;
                    snap = self.status(opts.clone()).await?;
                    snap_sid = status_session_id(&snap);
                }
            }
            if current_or_next && matches!(snap["state"].as_str(), Some("idle" | "exited")) {
                return Ok(snap);
            }
            let snap_seq = snap.get("seq").and_then(Value::as_u64);
            loop {
                {
                    let state = self.inner.state.lock().unwrap();
                    if let Some((notif_sid, notif_seq, idle_gen, payload)) = &state.latest_idle {
                        let same_session = snap_sid.is_none() || notif_sid == &snap_sid;
                        let newer = same_session
                            && match (snap_seq, notif_seq) {
                                (Some(s), Some(n)) => n > &s,
                                _ => *idle_gen > start_gen,
                            };
                        if newer {
                            return Ok(payload.clone());
                        }
                    }
                    if let Some(error) = &state.error {
                        return Err(error.clone());
                    }
                }
                changed.changed().await.map_err(|_| ClientError::new("closed", "Client closed"))?;
            }
        };
        wait(&opts, operation).await.and_then(|result| result)
    }

    async fn request(&self, method: &str, params: Value, opts: &WaitOptions) -> Result<Value> {
        if opts.cancel.is_cancelled() {
            return Err(ClientError::new("cancelled", "Cancelled before sending"));
        }
        let (reply, receive) = oneshot::channel();
        let id = {
            let mut state = self.inner.state.lock().unwrap();
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            if self.inner.closed.is_cancelled() {
                return Err(ClientError::new("closed", "Client closed"));
            }
            if state.pending.len() >= MAX_PENDING {
                return Err(ClientError::new("capacity", "Too many pending peer calls"));
            }
            let id = state.next;
            state.next += 1;
            state.pending.insert(id, reply);
            id
        };
        let _pending = Pending { inner: self.inner.clone(), id };
        let frame = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}).to_string();
        if frame.len() + 1 > MAX_FRAME {
            return Err(ClientError::new("invalid_params", "Encoded peer request exceeds the frame limit"));
        }
        let written = std::cell::Cell::new(false);
        let exchange = async {
            self.inner
                .conn
                .write_line(&frame, opts.timeout.unwrap_or(Duration::from_secs(5)))
                .await
                .map_err(|e| ClientError::new("transport_error", e))?;
            written.set(true);
            receive.await.map_err(|_| ClientError::new("closed", "Peer disconnected before its response"))?
        };
        let result = wait(opts, exchange).await.and_then(|r| r);
        if result.is_err() && !written.get() {
            self.inner.closed.cancel();
        }
        if method == "peer.message" {
            result.map_err(|e| {
                if e.code == "remote_error" {
                    e
                } else {
                    ClientError::new(
                        "outcome_unknown",
                        format!("Message may have been accepted: {}. Do not retry automatically", e.message),
                    )
                }
            })
        } else {
            result
        }
    }
}

struct Pending {
    inner: Arc<Inner>,
    id: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.inner.state.lock().unwrap().pending.remove(&self.id);
    }
}

impl Inner {
    async fn run(self: Arc<Self>) {
        let error = loop {
            let line = tokio::select! {
                biased;
                _ = self.closed.cancelled() => break ClientError::new("closed", "Client closed"),
                line = self.conn.read_line_with_timeout(None) => line,
            };
            let value: Value = match line.and_then(|line| serde_json::from_str(&line).map_err(|e| e.to_string())) {
                Ok(value) => value,
                Err(e) => break ClientError::new("protocol_error", e),
            };
            if let Err(error) = self.dispatch(value) {
                break error;
            }
        };
        self.closed.cancel();
        self.conn.close().await;
        let mut state = self.state.lock().unwrap();
        state.error = Some(error.clone());
        for (_, reply) in state.pending.drain() {
            let _ = reply.send(Err(error.clone()));
        }
        self.changed.send_modify(|n| *n += 1);
        self.finished.send_replace(true);
    }

    fn dispatch(&self, value: Value) -> Result<()> {
        if value["jsonrpc"] != "2.0" {
            return Err(ClientError::new("protocol_error", "Invalid JSON-RPC envelope"));
        }
        let mut state = self.state.lock().unwrap();
        if let Some(id) = value["id"].as_u64() {
            let result = if let Some(error) = value.get("error") {
                Err(ClientError::new("remote_error", error.to_string()))
            } else if let Some(result) = value.get("result") {
                Ok(result.clone())
            } else {
                return Err(ClientError::new("protocol_error", "Response has neither result nor error"));
            };
            // Late responses to locally cancelled waits are ignored, never reassigned.
            if let Some(reply) = state.pending.remove(&id) {
                let _ = reply.send(result);
            }
            return Ok(());
        }
        match value["method"].as_str() {
            Some("peer.message_status") => {
                let params = &value["params"];
                let id = params["message_id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| ClientError::new("protocol_error", "Missing receipt id"))?;
                let status = params["status"]
                    .as_str()
                    .filter(|s| matches!(*s, "delivered" | "rejected"))
                    .ok_or_else(|| ClientError::new("protocol_error", "Invalid receipt status"))?;
                if !state.receipts.contains_key(id) && state.receipts.len() >= MAX_RECEIPTS {
                    return Err(ClientError::new("capacity", "Peer receipt buffer overflow"));
                }
                if let Some(previous) = state.receipts.get(id)
                    && previous != "held"
                    && previous != status
                {
                    return Err(ClientError::new("protocol_error", "Peer changed a terminal delivery receipt"));
                }
                #[derive(Deserialize, Serialize)]
                struct MessageStatusReceipt {
                    message_id: String,
                    status: String,
                    #[serde(default, skip_serializing_if = "Option::is_none")]
                    reason: Option<String>,
                }
                let receipt: MessageStatusReceipt = serde_json::from_value(params.clone())
                    .map_err(|error| ClientError::new("protocol_error", error.to_string()))?;
                let details = serde_json::to_value(receipt).unwrap();
                if details.to_string().len() > 8192 {
                    return Err(ClientError::new("protocol_error", "Peer receipt metadata exceeds 8192 bytes"));
                }
                state.receipt_details.entry(id.to_string()).or_insert(details);
                state.receipts.insert(id.into(), status.into());
            }
            Some("status/changed") => {
                let sid = status_session_id(&value["params"]);
                record_status_payload(&mut state, sid, &value["params"]);
            }
            Some("event") if value["params"]["type"].as_str() == Some("status/changed") => {
                let sid = value["params"]
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .or_else(|| status_session_id(&value["params"]["data"]));
                record_status_payload(&mut state, sid, &value["params"]["data"]);
            }
            Some(_) if value.get("id").is_none() => return Ok(()),
            _ => return Err(ClientError::new("protocol_error", "Unexpected peer envelope")),
        }
        self.changed.send_modify(|n| *n += 1);
        Ok(())
    }
}

fn status_session_id(val: &Value) -> Option<String> {
    val.get("session")
        .and_then(|s| s.get("session_id"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn record_status_payload(state: &mut State, sid: Option<String>, payload: &Value) -> bool {
    if !matches!(payload.get("state").and_then(Value::as_str), Some("idle" | "exited")) {
        return false;
    }
    if let Some(Some(ref active_sid)) = state.subscribed_session
        && sid.as_deref() != Some(active_sid.as_str())
    {
        return false;
    }
    let seq = payload.get("seq").and_then(Value::as_u64);
    let duplicate = matches!(
        (&state.latest_idle, seq),
        (Some((prev_sid, Some(prev), _, _)), Some(cur)) if *prev_sid == sid && cur <= *prev
    );
    if duplicate {
        return false;
    }
    state.idle_gen += 1;
    state.latest_idle = Some((sid, seq, state.idle_gen, payload.clone()));
    true
}

async fn wait<T>(opts: &WaitOptions, operation: impl std::future::Future<Output = T>) -> Result<T> {
    let deadline = async {
        match opts.timeout {
            Some(d) => tokio::time::sleep(d).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        biased;
        _ = opts.cancel.cancelled() => Err(ClientError::new("cancelled", "Local wait cancelled")),
        result = operation => Ok(result),
        _ = deadline => Err(ClientError::new("timeout", "Peer wait timed out")),
    }
}
