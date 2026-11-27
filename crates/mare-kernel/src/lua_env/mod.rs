//! The Lua environment the kernel sets up around plugins: `require` over plugin modules,
//! `mare.g` / `mare.loader`, and the runaway-callback watchdog.

pub(crate) mod cordis_host;
mod globals;
pub(crate) mod modules;
pub(crate) mod public_modules;
pub(crate) mod watchdog;
