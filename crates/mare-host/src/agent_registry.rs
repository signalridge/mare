//! Host-owned registry of spawned child subagents, shared by Lua (`mare.subagent.*`) and the `subagent` contract service.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub const STEER_QUEUE_CAPACITY: usize = 64;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentInfo {
    pub subagent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    pub status: AgentStatus,
    pub started_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<mare_protocol::events::EventError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentWaitResult {
    pub subagent_id: String,
    pub status: AgentStatus,
    pub timed_out: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<Vec<serde_json::Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<mare_protocol::events::EventError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

pub(crate) fn parse_event_error(
    value: &serde_json::Value,
    fallback_code: Option<&str>,
) -> Option<mare_protocol::events::EventError> {
    let raw = value.get("error")?;
    if raw.is_null() {
        return None;
    }
    if let Ok(err) = serde_json::from_value::<mare_protocol::events::EventError>(raw.clone()) {
        return Some(err);
    }
    if let Some(msg) = raw.as_str() {
        let code = fallback_code
            .filter(|c| !c.is_empty())
            .or_else(|| value.get("error_kind").and_then(|v| v.as_str()).filter(|c| !c.is_empty()))
            .unwrap_or("error")
            .to_string();
        return Some(mare_protocol::events::EventError { code, message: msg.to_string(), data: None });
    }
    Some(mare_protocol::events::EventError {
        code: fallback_code.filter(|c| !c.is_empty()).unwrap_or("error").to_string(),
        message: raw.to_string(),
        data: None,
    })
}

pub(crate) struct AgentEntry {
    pub info: AgentInfo,
    pub parent_session_epoch: Option<String>,
    pub result_rx: watch::Receiver<Option<Result<serde_json::Value, crate::agent::ChildError>>>,
    pub cancel: CancellationToken,
    pub cancel_scope: Option<crate::agent_tree::Scope>,
    pub steer: mpsc::Sender<(String, Option<String>)>,
    pub collected: bool,
}

#[derive(Default)]
struct RegistryInner {
    order: Vec<String>,
    entries: HashMap<String, AgentEntry>,
    reserved_names: HashSet<(Option<(String, String)>, String)>,
}

#[derive(Clone, Default)]
pub struct AgentRegistry {
    inner: Arc<Mutex<RegistryInner>>,
    scope: Option<(String, String)>,
    scoped: bool,
}

pub(crate) struct NameReservation {
    registry: AgentRegistry,
    name: Option<(Option<(String, String)>, String)>,
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        if let Some(name) = self.name.take()
            && let Ok(mut inner) = self.registry.inner.lock()
        {
            inner.reserved_names.remove(&name);
        }
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Maximum number of finished agent entries retained in memory; finished entries beyond this cap
/// are pruned (collected first, then oldest-first) while running entries are always kept.
const MAX_ENTRIES: usize = 64;

pub(crate) fn try_send_steer(
    tx: &mpsc::Sender<(String, Option<String>)>,
    text: String,
    source: Option<String>,
) -> Result<bool, String> {
    match tx.try_send((text, source)) {
        Ok(()) => Ok(true),
        Err(mpsc::error::TrySendError::Closed(_)) => Ok(false),
        Err(mpsc::error::TrySendError::Full(_)) => {
            Err(format!("steer queue is full (capacity {STEER_QUEUE_CAPACITY})"))
        }
    }
}

impl AgentRegistry {
    pub(crate) fn for_session(&self, scope: Option<(String, String)>) -> Self {
        Self { inner: self.inner.clone(), scope, scoped: true }
    }

    fn accepts(&self, entry: &AgentEntry) -> bool {
        if !self.scoped {
            return true;
        }
        match &self.scope {
            Some((session, session_epoch)) => {
                entry.info.parent_session_id.as_deref() == Some(session)
                    && entry.parent_session_epoch.as_deref() == Some(session_epoch)
            }
            None => entry.info.parent_session_id.is_none(),
        }
    }

    fn prune_locked(inner: &mut RegistryInner) {
        const MAX_UNCOLLECTED_FINISHED: usize = 256;
        let finished_count = inner.entries.values().filter(|e| e.info.status != AgentStatus::Running).count();
        if finished_count <= MAX_ENTRIES {
            return;
        }
        let mut dropped = HashSet::new();
        let mut collected: Vec<(String, i64, usize)> = inner
            .order
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| {
                let e = inner.entries.get(id)?;
                if e.info.status == AgentStatus::Running || !e.collected {
                    return None;
                }
                Some((id.clone(), e.info.ended_at_ms.unwrap_or(e.info.started_at_ms), idx))
            })
            .collect();
        let mut remaining_finished = finished_count;
        if !collected.is_empty() {
            let excess = (remaining_finished - MAX_ENTRIES).min(collected.len());
            collected.sort_by_key(|(_, ts, idx)| (*ts, *idx));
            for (id, _, _) in collected.into_iter().take(excess) {
                inner.entries.remove(&id);
                dropped.insert(id);
                remaining_finished -= 1;
            }
        }
        // High-water cap for uncollected finished entries (e.g. remote `subagent.start` callers
        // that never call `wait`): evict oldest finished entries so host memory stays bounded.
        if remaining_finished > MAX_UNCOLLECTED_FINISHED {
            let mut uncollected: Vec<(String, i64, usize)> = inner
                .order
                .iter()
                .enumerate()
                .filter_map(|(idx, id)| {
                    let e = inner.entries.get(id)?;
                    if e.info.status == AgentStatus::Running {
                        return None;
                    }
                    Some((id.clone(), e.info.ended_at_ms.unwrap_or(e.info.started_at_ms), idx))
                })
                .collect();
            let excess = (remaining_finished - MAX_UNCOLLECTED_FINISHED).min(uncollected.len());
            uncollected.sort_by_key(|(_, ts, idx)| (*ts, *idx));
            for (id, _, _) in uncollected.into_iter().take(excess) {
                inner.entries.remove(&id);
                dropped.insert(id);
            }
        }
        if !dropped.is_empty() {
            inner.order.retain(|id| !dropped.contains(id));
        }
    }

    pub(crate) fn reserve_name(&self, name: Option<&str>) -> Result<NameReservation, String> {
        let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
            return Ok(NameReservation { registry: self.clone(), name: None });
        };
        let mut inner = self.inner.lock().unwrap();
        if inner
            .reserved_names
            .iter()
            .any(|(scope, reserved)| reserved == name && (!self.scoped || scope == &self.scope))
            || inner.entries.values().any(|e| self.accepts(e) && e.info.name.as_deref() == Some(name))
        {
            return Err(format!("duplicate subagent name: {name}"));
        }
        let key = (self.scope.clone(), name.to_string());
        inner.reserved_names.insert(key.clone());
        Ok(NameReservation { registry: self.clone(), name: Some(key) })
    }

    pub(crate) fn insert(&self, mut reservation: NameReservation, entry: AgentEntry) {
        let session_id = entry.info.subagent_id.clone();
        let mut inner = self.inner.lock().unwrap();
        if let Some(name) = reservation.name.take() {
            inner.reserved_names.remove(&name);
        }
        if !inner.entries.contains_key(&session_id) {
            inner.order.push(session_id.clone());
        }
        inner.entries.insert(session_id, entry);
        Self::prune_locked(&mut inner);
    }

    pub(crate) fn mark_finished(
        &self,
        session_id: &str,
        status: AgentStatus,
        error: Option<mare_protocol::events::EventError>,
        error_kind: Option<String>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.entries.get_mut(session_id) {
            entry.info.status = status;
            entry.info.ended_at_ms = Some(now_ms());
            entry.info.error = error;
            entry.info.error_kind = error_kind;
        }
        Self::prune_locked(&mut inner);
    }

    fn resolve_id_locked<'a>(&self, inner: &'a RegistryInner, session_id_or_name: &str) -> Option<&'a str> {
        if inner.entries.get(session_id_or_name).is_some_and(|entry| self.accepts(entry)) {
            return inner.entries.get_key_value(session_id_or_name).map(|(k, _)| k.as_str());
        }
        inner
            .entries
            .iter()
            .find(|(_, e)| self.accepts(e) && e.info.name.as_deref() == Some(session_id_or_name))
            .map(|(k, _)| k.as_str())
    }

    pub fn list(&self) -> Vec<AgentInfo> {
        let inner = self.inner.lock().unwrap();
        inner
            .order
            .iter()
            .filter_map(|id| inner.entries.get(id).filter(|e| self.accepts(e)).map(|e| e.info.clone()))
            .collect()
    }

    pub fn get(&self, id_or_name: &str) -> Option<AgentInfo> {
        let inner = self.inner.lock().unwrap();
        let id = self.resolve_id_locked(&inner, id_or_name)?;
        inner.entries.get(id).map(|e| e.info.clone())
    }

    pub fn steer(&self, id_or_name: &str, text: String) -> Result<bool, String> {
        self.steer_with(id_or_name, text, None)
    }

    pub fn steer_with(&self, id_or_name: &str, text: String, source: Option<String>) -> Result<bool, String> {
        let sender = {
            let inner = self.inner.lock().unwrap();
            let id =
                self.resolve_id_locked(&inner, id_or_name).ok_or_else(|| format!("unknown subagent: {id_or_name}"))?;
            let entry = &inner.entries[id];
            if entry.info.status != AgentStatus::Running {
                return Ok(false);
            }
            entry.steer.clone()
        };
        try_send_steer(&sender, text, source)
    }

    pub fn cancel(&self, id_or_name: &str) -> Result<bool, String> {
        let inner = self.inner.lock().unwrap();
        let id = self.resolve_id_locked(&inner, id_or_name).ok_or_else(|| format!("unknown subagent: {id_or_name}"))?;
        let entry = &inner.entries[id];
        let was_running = entry.info.status == AgentStatus::Running && !entry.cancel.is_cancelled();
        if let Some(scope) = &entry.cancel_scope {
            scope.cancel_as("parent");
        } else {
            entry.cancel.cancel();
        }
        Ok(was_running)
    }

    pub async fn wait(
        &self,
        id_or_name: &str,
        timeout_ms: Option<u64>,
        cancel: Option<CancellationToken>,
    ) -> Result<AgentWaitResult, String> {
        let (id, mut rx, agent_cancel) = {
            let inner = self.inner.lock().unwrap();
            let id = self
                .resolve_id_locked(&inner, id_or_name)
                .ok_or_else(|| format!("unknown subagent: {id_or_name}"))?
                .to_string();
            let entry = &inner.entries[&id];
            (id, entry.result_rx.clone(), entry.cancel.clone())
        };

        if let Some(0) = timeout_ms {
            if let Some(done) = rx.borrow().clone() {
                let status = self.record_collected_locked(&id);
                return Ok(Self::build_wait_result(id, done, status, &agent_cancel));
            }
            if cancel.as_ref().is_some_and(CancellationToken::is_cancelled) {
                return Ok(self.cancelled_result(&id));
            }
            return Ok(self.timed_out_result(&id));
        }

        let wait_fut = async {
            rx.wait_for(Option::is_some)
                .await
                .map(|value| value.clone().unwrap())
                .unwrap_or_else(|_| Err(crate::agent::ChildError::Failed("agent task dropped".into())))
        };

        let outcome = match (timeout_ms, cancel) {
            (Some(ms), Some(tok)) => {
                tokio::select! {
                    biased;
                    _ = tok.cancelled() => return Ok(self.cancelled_result(&id)),
                    res = tokio::time::timeout(Duration::from_millis(ms), wait_fut) => match res {
                        Ok(r) => r,
                        Err(_) => return Ok(self.timed_out_result(&id)),
                    },
                }
            }
            (Some(ms), None) => match tokio::time::timeout(Duration::from_millis(ms), wait_fut).await {
                Ok(r) => r,
                Err(_) => return Ok(self.timed_out_result(&id)),
            },
            (None, Some(tok)) => {
                tokio::select! {
                    biased;
                    _ = tok.cancelled() => return Ok(self.cancelled_result(&id)),
                    res = wait_fut => res,
                }
            }
            (None, None) => wait_fut.await,
        };

        let info_status = self.record_collected_locked(&id);
        Ok(Self::build_wait_result(id, outcome, info_status, &agent_cancel))
    }

    fn record_collected_locked(&self, id: &str) -> Option<AgentStatus> {
        let mut inner = self.inner.lock().unwrap();
        let status = if let Some(entry) = inner.entries.get_mut(id) {
            entry.collected = true;
            Some(entry.info.status.clone())
        } else {
            None
        };
        Self::prune_locked(&mut inner);
        status
    }

    fn build_wait_result(
        id: String,
        outcome: Result<serde_json::Value, crate::agent::ChildError>,
        info_status: Option<AgentStatus>,
        agent_cancel: &CancellationToken,
    ) -> AgentWaitResult {
        match outcome {
            Ok(value) => {
                let error_kind = value.get("error_kind").and_then(|v| v.as_str()).map(str::to_string);
                let err = parse_event_error(&value, error_kind.as_deref());
                let status = info_status.filter(|s| *s != AgentStatus::Running).unwrap_or_else(|| {
                    if agent_cancel.is_cancelled() {
                        AgentStatus::Cancelled
                    } else if err.is_some() {
                        AgentStatus::Failed
                    } else {
                        AgentStatus::Completed
                    }
                });
                AgentWaitResult {
                    subagent_id: id,
                    status,
                    timed_out: false,
                    cancelled: false,
                    text: value.get("text").and_then(|v| v.as_str()).map(str::to_string),
                    messages: value.get("messages").and_then(|v| v.as_array()).cloned(),
                    usage: value.get(mare_shared::USAGE_KEY).filter(|v| v.is_object()).cloned(),
                    error: err,
                    error_kind,
                }
            }
            Err(err) => {
                let cancelled = agent_cancel.is_cancelled() || matches!(err, crate::agent::ChildError::Cancelled);
                let status = info_status.filter(|s| *s != AgentStatus::Running).unwrap_or(if cancelled {
                    AgentStatus::Cancelled
                } else {
                    AgentStatus::Failed
                });
                let code = if cancelled { "cancelled" } else { "error" };
                let kind = if cancelled { "aborted" } else { "error" };
                AgentWaitResult {
                    subagent_id: id,
                    status,
                    timed_out: false,
                    cancelled: false,
                    text: None,
                    messages: None,
                    usage: None,
                    error: Some(mare_protocol::events::EventError {
                        code: code.into(),
                        message: err.to_string(),
                        data: None,
                    }),
                    error_kind: Some(kind.into()),
                }
            }
        }
    }

    fn timed_out_result(&self, id: &str) -> AgentWaitResult {
        let info = self.get(id);
        AgentWaitResult {
            subagent_id: id.to_string(),
            status: info.as_ref().map(|i| i.status.clone()).unwrap_or(AgentStatus::Running),
            timed_out: true,
            cancelled: false,
            text: None,
            messages: None,
            usage: None,
            error: None,
            error_kind: None,
        }
    }

    fn cancelled_result(&self, id: &str) -> AgentWaitResult {
        let info = self.get(id);
        AgentWaitResult {
            subagent_id: id.to_string(),
            status: info.as_ref().map(|i| i.status.clone()).unwrap_or(AgentStatus::Running),
            timed_out: false,
            cancelled: true,
            text: None,
            messages: None,
            usage: None,
            error: None,
            error_kind: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type DummyEntry = (
        AgentEntry,
        watch::Sender<Option<Result<serde_json::Value, crate::agent::ChildError>>>,
        mpsc::Receiver<(String, Option<String>)>,
    );

    fn dummy_entry(id_num: u64, name: Option<&str>) -> DummyEntry {
        let (res_tx, res_rx) = watch::channel(None);
        let (steer_tx, steer_rx) = mpsc::channel(STEER_QUEUE_CAPACITY);
        let session_id = format!("child-{id_num}");
        let entry = AgentEntry {
            info: AgentInfo {
                subagent_id: session_id,
                name: name.map(str::to_string),
                label: name.map(str::to_string),
                run_id: Some("run-1".into()),
                seq: Some(1),
                task: Some("test task".into()),
                parent_session_id: Some("sess-p".into()),
                status: AgentStatus::Running,
                started_at_ms: now_ms(),
                ended_at_ms: None,
                error: None,
                error_kind: None,
            },
            parent_session_epoch: Some("gen-p".into()),
            result_rx: res_rx,
            cancel: CancellationToken::new(),
            cancel_scope: None,
            steer: steer_tx,
            collected: false,
        };
        (entry, res_tx, steer_rx)
    }

    #[test]
    fn duplicate_name_is_rejected_and_reservation_releases_on_drop() {
        let reg = AgentRegistry::default();
        let res1 = reg.reserve_name(Some("worker")).unwrap();
        assert!(reg.reserve_name(Some("worker")).is_err());
        drop(res1);
        let res2 = reg.reserve_name(Some("worker")).unwrap();
        let (entry, _, _) = dummy_entry(1, Some("worker"));
        reg.insert(res2, entry);
        assert!(reg.reserve_name(Some("worker")).is_err());
        assert_eq!(reg.get("worker").unwrap().subagent_id, "child-1");
        assert_eq!(reg.get("child-1").unwrap().subagent_id, "child-1");
        assert!(reg.get("1").is_none(), "numeric aliases must be removed");
        assert!(reg.get("agent-1").is_none(), "synthetic agent-N aliases must be removed");
    }

    #[tokio::test]
    async fn wait_timeout_and_cancel_do_not_cancel_child_and_steer_delivers() {
        let reg = AgentRegistry::default();
        let res = reg.reserve_name(Some("alpha")).unwrap();
        let (entry, res_tx, mut steer_rx) = dummy_entry(7, Some("alpha"));
        let child_cancel = entry.cancel.clone();
        reg.insert(res, entry);

        // Timed out wait does not cancel child
        let timed = reg.wait("alpha", Some(5), None).await.unwrap();
        assert!(timed.timed_out);
        assert_eq!(timed.status, AgentStatus::Running);
        assert!(!child_cancel.is_cancelled());

        // Cancelled wait token does not cancel child and reports cancelled (not timed_out)
        let wait_tok = CancellationToken::new();
        wait_tok.cancel();
        let cancelled_wait = reg.wait("child-7", None, Some(wait_tok.clone())).await.unwrap();
        assert!(cancelled_wait.cancelled && !cancelled_wait.timed_out, "{cancelled_wait:?}");
        assert_eq!(cancelled_wait.status, AgentStatus::Running);
        assert!(!child_cancel.is_cancelled());

        // Simultaneous timeout_ms = 0 and cancelled wait token still reports cancelled
        let cancelled_zero = reg.wait("child-7", Some(0), Some(wait_tok)).await.unwrap();
        assert!(cancelled_zero.cancelled && !cancelled_zero.timed_out, "{cancelled_zero:?}");
        assert!(!child_cancel.is_cancelled());

        // Steer delivers
        assert!(reg.steer("alpha", "focus on tests".into()).unwrap());
        assert_eq!(steer_rx.recv().await, Some(("focus on tests".into(), None)));

        // Completion delivers to wait
        res_tx.send(Some(Ok(serde_json::json!({"text": "done!", "usage": {"input": 10}})))).unwrap();
        reg.mark_finished("child-7", AgentStatus::Completed, None, None);
        let done = reg.wait("child-7", None, None).await.unwrap();
        assert!(!done.timed_out);
        assert_eq!(done.status, AgentStatus::Completed);
        assert_eq!(done.text.as_deref(), Some("done!"));
    }

    #[tokio::test]
    async fn finished_entries_are_pruned_beyond_cap_while_running_stay() {
        let reg = AgentRegistry::default();
        // Insert one running agent that must never be pruned.
        let r0 = reg.reserve_name(Some("keeper")).unwrap();
        let (e0, _, _) = dummy_entry(1000, Some("keeper"));
        reg.insert(r0, e0);

        for i in 1..=(MAX_ENTRIES + 5) {
            let name = format!("w-{i}");
            let res = reg.reserve_name(Some(&name)).unwrap();
            let (entry, tx, _) = dummy_entry(i as u64, Some(&name));
            reg.insert(res, entry);
            let _ = tx.send(Some(Ok(serde_json::json!({ "text": format!("done-{i}") }))));
            reg.mark_finished(&format!("child-{i}"), AgentStatus::Completed, None, None);
            if i <= 5 {
                reg.wait(&format!("child-{i}"), Some(100), None).await.unwrap();
            }
        }

        let listed = reg.list();
        assert_eq!(listed.len(), MAX_ENTRIES + 1);
        assert!(reg.get("keeper").is_some(), "running entry must not be pruned");
        assert!(reg.get("w-1").is_none(), "oldest collected finished entry should be pruned");
        assert!(reg.reserve_name(Some("w-1")).is_ok(), "pruned name can be reused");
    }

    /// Item 5 (self-written): with >64 concurrent agents running, all 65 finishing before any
    /// `subagent.wait` must still preserve the first (and last) uncollected result for `subagent.wait`.
    #[tokio::test]
    async fn concurrent_agents_above_cap_preserve_first_finisher_for_wait() {
        let reg = AgentRegistry::default();
        let total = MAX_ENTRIES + 1; // 65 concurrent agents
        let mut senders = Vec::with_capacity(total);
        for i in 1..=total {
            let name = format!("conc-{i}");
            let res = reg.reserve_name(Some(&name)).unwrap();
            let (entry, res_tx, _steer_rx) = dummy_entry(i as u64, Some(&name));
            reg.insert(res, entry);
            senders.push(res_tx);
        }
        assert_eq!(reg.list().len(), total);

        // All 65 agents finish before any `subagent.wait` is called.
        for (idx, tx) in senders.into_iter().enumerate() {
            let i = idx + 1;
            let id = format!("child-{i}");
            reg.mark_finished(&id, AgentStatus::Completed, None, None);
            tx.send(Some(Ok(serde_json::json!({ "text": format!("done-{i}") })))).unwrap();
        }

        assert!(reg.get("child-1").is_some(), "first uncollected finisher must not be pruned");
        let waited = reg.wait("child-1", Some(1000), None).await.unwrap();
        assert!(!waited.timed_out);
        assert_eq!(waited.status, AgentStatus::Completed);
        assert_eq!(waited.text.as_deref(), Some("done-1"));

        let waited_last = reg.wait(&format!("child-{total}"), Some(1000), None).await.unwrap();
        assert!(!waited_last.timed_out);
        assert_eq!(waited_last.status, AgentStatus::Completed);
        assert_eq!(waited_last.text.as_deref(), Some("done-65"));
    }

    #[tokio::test]
    async fn steer_queue_enforces_capacity_rejects_full_recovers_on_drain_and_stops_on_cancel() {
        let reg = AgentRegistry::default();
        let res = reg.reserve_name(Some("worker")).unwrap();
        let (entry, _res_tx, mut steer_rx) = dummy_entry(10, Some("worker"));
        reg.insert(res, entry);

        for i in 0..64 {
            assert!(reg.steer("worker", format!("msg-{i}")).unwrap(), "expected steer {i} to succeed");
        }
        let full_err = reg.steer("worker", "overflow".into()).unwrap_err();
        assert!(full_err.contains("full"), "expected full-queue error, got: {full_err}");

        // Draining one message frees capacity for a subsequent steer.
        assert_eq!(steer_rx.recv().await, Some(("msg-0".into(), None)));
        assert!(reg.steer("worker", "recovered".into()).unwrap());

        // Dropping receiver (or cancelling/finishing) stops delivery without panicking or blocking.
        drop(steer_rx);
        assert!(!reg.steer("worker", "after-close".into()).unwrap());
    }

    #[tokio::test]
    async fn zero_timeout_returns_immediately_when_running_and_filters_null_usage() {
        let reg = AgentRegistry::default();
        let res = reg.reserve_name(Some("z-worker")).unwrap();
        let (entry, res_tx, _steer_rx) = dummy_entry(11, Some("z-worker"));
        reg.insert(res, entry);

        let running = reg.wait("z-worker", Some(0), None).await.unwrap();
        assert!(running.timed_out);
        assert_eq!(running.status, AgentStatus::Running);

        reg.mark_finished("child-11", AgentStatus::Completed, None, None);
        res_tx.send(Some(Ok(serde_json::json!({ "text": "ok", "usage": null })))).unwrap();
        let done = reg.wait("z-worker", Some(0), None).await.unwrap();
        assert!(!done.timed_out);
        assert_eq!(done.status, AgentStatus::Completed);
        assert_eq!(done.usage, None, "null usage must be filtered to None");
    }

    #[tokio::test]
    async fn uncollected_finished_entries_are_bounded_at_high_water_mark() {
        let reg = AgentRegistry::default();
        let total = 300usize;
        for i in 1..=total {
            let (entry, res_tx, _steer_rx) = dummy_entry(i as u64, None);
            let res = reg.reserve_name(None).unwrap();
            reg.insert(res, entry);
            let id = format!("child-{i}");
            reg.mark_finished(&id, AgentStatus::Completed, None, None);
            res_tx.send(Some(Ok(serde_json::json!({ "text": format!("payload-{i}") })))).unwrap();
        }
        let count = reg.list().len();
        assert!(count <= 256, "uncollected finished entries must be bounded by high-water cap (<= 256), got {count}");
        // Most recent uncollected entries remain retrievable.
        assert!(reg.get(&format!("child-{total}")).is_some());
    }
}
