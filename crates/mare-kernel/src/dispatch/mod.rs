//! Event dispatch: listeners and emit / bail / waterfall, fail-closed policy votes, services,
//! frontend taps and slow-callback timing.

pub(crate) mod baseline;
pub(crate) mod bus;
pub(crate) mod contract_events;
pub(crate) mod contract_prims;
pub mod contracts;
pub(crate) mod events;
pub(crate) mod luals;
pub(crate) mod policy;
pub(crate) mod policy_prims;
pub(crate) mod remote;
pub(crate) mod runtime_fields;
pub(crate) mod service_error;
pub(crate) mod service_rpc;
pub(crate) mod services;
pub(crate) mod tap;
pub(crate) mod timing;

/// Convert a positive finite millisecond value into a clamped [`std::time::Duration`].
pub(crate) fn duration_from_positive_ms(ms: f64) -> std::time::Duration {
    let floored = ms.floor();
    let millis = if floored >= u64::MAX as f64 { u64::MAX } else { (floored as u64).max(1) };
    std::time::Duration::from_millis(millis)
}
