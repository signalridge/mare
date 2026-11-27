//! Which plugins and packages load, and what failed: the wanted definitions, packages and their
//! hooks, plugin failures and explicit disables.

pub(crate) mod defs;
pub(crate) mod introspect;
pub(crate) mod packages;
pub(crate) mod plugin_disabled;
pub(crate) mod plugin_failures;
