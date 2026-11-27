//! Structured run termination outcomes.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Machine-readable final outcome; provider error text never controls this classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentTerminationReason {
    Completed,
    UsageLimit,
    Interrupted,
    TurnLimit,
    Error,
}

/// Machine-readable origin of an interrupted turn (`termination_reason == "interrupted"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CancellationCause {
    User,
    Parent,
    Hook,
    Disposed,
}
