//! Approvals that cross a boundary: a sub-agent's approvals answered by the parent, network
//! approvals for sandboxed processes, and the rules shared with or proposed by sub-agents.

mod approval_queue;
mod child;
pub(crate) mod child_events;
mod net;
pub(crate) mod permission_grants;
pub(crate) mod proposed_rule;
pub(crate) mod shared_rule;

use serde_json::Value;

use crate::Host;

impl Host {
    /// Emit `approval/requested` for host-managed (network or delegated child) approval lifecycles.
    pub(crate) async fn emit_approval_requested(
        &self,
        approval_id: &str,
        category: &str,
        call_id: Option<&str>,
        snapshot: Option<Value>,
        child_approval_id: Option<u64>,
    ) {
        let mut payload = serde_json::json!({
            "approval_id": approval_id,
            "category": category,
        });
        if let Some(call_id) = call_id {
            payload["tool_call_id"] = call_id.into();
        }
        if let Some(snapshot) = snapshot {
            payload["snapshot"] = snapshot;
        }
        if let Some(child_approval_id) = child_approval_id {
            payload["child_approval_id"] = child_approval_id.into();
        }
        self.kernel.emit_json("approval/requested", payload).await;
    }

    /// Emit `approval/resolved` with the sanitized public audit payload for a finished approval.
    pub(crate) async fn emit_approval_resolved(
        &self,
        approval_id: &str,
        category: &str,
        call_id: Option<&str>,
        raw_result: &Value,
    ) {
        let mut payload = serde_json::json!({
            "approval_id": approval_id,
            "category": category,
            "result": mare_binding::approval::public_result(raw_result),
        });
        if let Some(call_id) = call_id {
            payload["tool_call_id"] = call_id.into();
        }
        self.kernel.emit_json("approval/resolved", payload).await;
    }

    /// Emit `approval/requested` (unless Lua already did) followed by `approval/resolved`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn emit_approval_pair(
        &self,
        approval_id: &str,
        category: &str,
        call_id: Option<&str>,
        snapshot: Option<Value>,
        child_approval_id: Option<u64>,
        requested_emitted: bool,
        raw_result: &Value,
    ) {
        if !requested_emitted {
            self.emit_approval_requested(approval_id, category, call_id, snapshot, child_approval_id).await;
        }
        self.emit_approval_resolved(approval_id, category, call_id, raw_result).await;
    }
}
