//! Core Lua engine loaders (`config`, `keymap`, `ui`).
//!
//! Each engine receives the VM's private core handle (`mare_shared::lua::core::ensure(lua)`)
//! alongside `mare`, keeping engine loading and internal wiring out of `mare-plugin/src/lib.rs`.

pub mod config;
pub mod keymap;
pub mod ui;
