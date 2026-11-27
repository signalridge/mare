//! Resuming: restored state, damaged or locked sessions, read-only views, id and name matching.

pub(super) use super::*;

mod cancellation_and_handover;
mod core_and_lookup;
mod directory_and_readonly;
mod poison_and_adoption;
mod settings_and_sandbox;
