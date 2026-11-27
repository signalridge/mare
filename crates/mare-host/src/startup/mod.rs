//! Building a host and what it reports: startup order, health and the request gate, trust, and
//! config errors.

pub(crate) mod config_error;
pub(crate) mod flags;
mod health;
mod lifecycle;
pub(crate) use lifecycle::{StartupGcGuard, session_info_for_kernel};
pub(crate) mod request_gate;
pub(crate) mod session_identity;
pub(crate) mod trust;
