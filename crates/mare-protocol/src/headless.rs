//! `mare --json` (headless) event stream: one `{type, data}` object per line.
//! Only declared public events (core protocol events + `expose = "both"`
//! contract events) are written — internal plugin events never leak:
//!
//! 1. first line: [`SessionRecord`] (`type = "session"`) — the id/path a caller resumes with;
//! 2. the external events of the run, in order, with `lagged`/`dropped` records if selected
//!    events were omitted under pressure;
//! 3. on failure: [`TurnFailed`] (`type = "turn.failed"`);
//! 4. last line: [`Settled`] (`type = "settled"`) — the run's final text, usage and error.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One line of the stream.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Line {
    /// `"session"`, a declared public event name, `"turn.failed"` or `"settled"`
    #[serde(rename = "type")]
    pub kind: String,
    pub data: Value,
}

/// First line (`type = "session"`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionRecord {
    pub protocol_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// session file (absent with `--no-session`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `--no-session`: nothing is persisted, the run can't be resumed and has no crash recovery
    /// (a side-effecting call interrupted by a crash has an unknown, unverifiable outcome)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub ephemeral: bool,
}

impl Default for SessionRecord {
    fn default() -> Self {
        Self {
            protocol_version: crate::PROTOCOL_VERSION.into(),
            session_id: None,
            path: None,
            cwd: String::new(),
            model: None,
            ephemeral: false,
        }
    }
}

/// Structured failure (`type = "turn.failed"`), written before `settled`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TurnFailed {
    pub error: crate::events::EventError,
}

/// Sequence range omitted when an ordinary consumer cursor fell behind the bounded event log.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Lagged {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_epoch: Option<String>,
    pub from_seq: u64,
    pub to_seq: u64,
}

/// Drop count for unretained `lossy` or refused events omitted from external stream consumers.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Dropped {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_epoch: Option<String>,
    pub count: u64,
}

/// Last line (`type = "settled"`): nothing else follows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct Settled {
    /// final assistant text
    #[serde(default)]
    pub text: String,
    /// parsed JSON result when `--output-schema` was given and validation succeeded
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<crate::events::EventError>,
    /// the process exit code: 0 completed, 1 error, 2 arguments/configuration,
    /// 3 an approval was needed and nobody could answer, 4 `--max-turns` / `--timeout` reached,
    /// 130 interrupted (SIGINT)
    #[serde(default)]
    pub exit_code: i32,
    /// why the run ended: `completed` | `error` | `usage` | `approval_unavailable` |
    /// `max_turns` | `timeout` | `interrupted`
    #[serde(default)]
    pub reason: String,
    /// structured agent outcome (`completed` | `usage_limit` | `interrupted` | `turn_limit` | `error`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination_reason: Option<crate::AgentTerminationReason>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn protocol_events_classification() {
        assert!(crate::event_registry::is_protocol_event("turn/completed"));
        assert!(!crate::event_registry::is_protocol_event("compact/stalled"));
        assert!(!crate::event_registry::is_protocol_event("kernel/reloaded"));
    }
}
