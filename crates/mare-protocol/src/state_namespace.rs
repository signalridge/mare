//! Isolated durable application-state namespaces; no plugin or provider-specific policy.
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Read isolated durable application state (distinct from `agent.get`).
/// Absent or deleted namespaces report `exists: false` and `null` value with tombstone revision retained.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StateSnapshotRequest {
    pub namespace: String,
}

fn revision<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let n = serde_json::Number::deserialize(d)?;
    if let Some(value) = n.as_u64().filter(|v| *v <= 9_007_199_254_740_991) {
        return Ok(value);
    }
    if let Some(value) = n.as_f64()
        && value.fract() == 0.0
        && (0.0..=9_007_199_254_740_991.0).contains(&value)
    {
        return Ok(value as u64);
    }
    Err(serde::de::Error::custom("expected_revision must be a nonnegative safe integer"))
}

/// Atomically replace durable namespace state at `expected_revision` (`ok`, `conflict` with the current snapshot, or `rejected`).
/// Strict JSON preserves null and empty containers; deletion requires `null` and retains a monotonic revision. Cancellation before publication prevents writes; after publication admission rollback is not guaranteed, and `unknown_state` requires reread/reconciliation.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StateCompareExchangeRequest {
    pub namespace: String,
    #[serde(deserialize_with = "revision")]
    pub expected_revision: u64,
    pub value: Value,
    #[serde(default)]
    pub delete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StateSnapshot {
    pub namespace: String,
    pub revision: u64,
    pub exists: bool,
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StateErrorCode {
    InvalidRequest,
    InvalidNamespace,
    InvalidValue,
    ReadOnly,
    Denied,
    Cancelled,
    LockTimeout,
    Corrupt,
    UnsupportedVersion,
    RevisionExhausted,
    Io,
    UnknownState,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StateFailure {
    pub code: StateErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateSnapshotResult {
    Ok { snapshot: StateSnapshot },
    Rejected { error: StateFailure },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateCompareExchangeResult {
    Ok { snapshot: StateSnapshot },
    Conflict { snapshot: StateSnapshot },
    Rejected { error: StateFailure },
}
