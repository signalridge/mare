//! What a child agent sends its parent host: lifecycle events, approvals to answer and rule
//! checks, and the receiving end the parent drains.

use tokio::sync::{mpsc, oneshot};

/// Lifecycle of child agents, sent to the parent host as `subagent/started` and
/// `subagent/completed`.
pub enum ChildEvent {
    /// Confirms that all earlier child lifecycle notifications have reached the parent bus.
    Barrier(oneshot::Sender<()>),
    Start {
        subagent_id: String,
        agent_id: String,
        name: Option<String>,
        label: Option<String>,
        run_id: Option<String>,
        seq: Option<u64>,
        task: Option<String>,
        parent_session: Option<(String, String)>,
    },
    /// A replaceable progress snapshot for one child and parent session.
    Progress {
        subagent_id: String,
        parent_session: Option<(String, String)>,
        progress: serde_json::Value,
    },
    End(Box<ChildEnd>),
    /// An approval the child needs: it bubbles up to the parent, which
    /// answers it through its own approval path (see [`ChildApproval`]).
    Approval(Box<ChildApproval>),
    /// A call the child's own vote allowed with a per-call sandbox: do the parent's rules allow
    /// it too (a shared rule escalates every child, see [`crate::approvals::shared_rule`])?
    RuleCheck(Box<crate::approvals::shared_rule::RuleCheck>),
}

pub struct ChildEnd {
    pub subagent_id: String,
    pub agent_id: String,
    pub name: Option<String>,
    pub label: Option<String>,
    pub ok: bool,
    pub status: String,
    pub run_id: Option<String>,
    pub seq: Option<u64>,
    pub parent_session: Option<(String, String)>,
    pub text: Option<String>,
    pub error: Option<mare_protocol::events::EventError>,
    pub error_kind: Option<String>,
    pub usage: Option<serde_json::Value>,
    pub truncated: bool,
    pub stop_reason: Option<String>,
}

/// A child's `tool/approval` request, forwarded to the parent host. A child has no UI of its own;
/// before this, everything it needed approved was `unavailable` (= denied). The parent answers
/// with the same rules as its own calls: its `tool/approval` listeners (RPC host, plugins), else
/// its interactive dialog, else `unavailable` (headless). Requests are answered one at a time, in
/// arrival order (queued, never dropped) and routed to the root session's approval runtime, tagged
/// with the originating agent.
pub struct ChildApprovalReply {
    pub vote: serde_json::Value,
    pub human_approved: bool,
}

pub struct ChildApproval {
    /// where it comes from: `{agent_id = "agent-3", id = 3, label?, task?, run_id?, phase?}`;
    /// added to the call and info as `origin`
    pub origin: serde_json::Value,
    pub call: serde_json::Value,
    pub info: serde_json::Value,
    pub verdict: serde_json::Value,
    /// the child runs with the parent's permission configuration (no spawn-time
    /// `config.plugins.permissions`): the parent's rules may resolve the request
    pub shares_rules: bool,
    /// the child inherits the parent's live preset (no explicit spawn-time `permission` /
    /// `permission_ceiling` / `config.plugins.permissions` override): the parent's current preset
    /// may auto-allow sandbox requests it already permits
    pub inherits_parent_preset: bool,
    /// whether the child's approval policy requires per-call review (`review_all`)
    pub review_required: bool,
    /// the policy vote the child's `tool/approval` dispatch receives:
    /// `{decision = "allow", grant, amendment?}` | `{decision = "deny", abort?, unavailable?, reason?}`
    pub reply: oneshot::Sender<ChildApprovalReply>,
}

pub struct ChildEventsRx(pub std::sync::Mutex<Option<mpsc::UnboundedReceiver<ChildEvent>>>);
