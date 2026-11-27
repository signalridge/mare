//! Core Lua configuration engine loader (`config_api`, `config_packages`, `config_introspect`,
//! `config_txn`, `plugin_config_api`).
//!
//! Release builds embed precompiled Lua 5.5 bytecode produced by `build.rs`; debug builds load
//! source text directly from `mare_config`. Loaded chunks are cached on `core.config` so
//! `/reload config` never re-parses or re-loads bytecode.

use mlua::{Function, Lua, Table, Value};

#[cfg(not(debug_assertions))]
mod bytecode {
    pub const CONFIG_API: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/config_api.luac"));
    pub const CONFIG_API_UI_LAYOUT: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/config_api_ui_layout.luac"));
    pub const CONFIG_API_VALIDATE: &[u8] =
        include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/config_api_validate.luac"));
    pub const CONFIG_PACKAGES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/config_packages.luac"));
    pub const CONFIG_INTROSPECT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/config_introspect.luac"));
    pub const CONFIG_TXN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/config_txn.luac"));
    pub const PLUGIN_CONFIG_API: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/bytecode/plugin_config_api.luac"));
}

#[cfg(debug_assertions)]
mod bytecode {
    pub const CONFIG_API: &[u8] = mare_config::CONFIG_API.as_bytes();
    pub const CONFIG_API_UI_LAYOUT: &[u8] = mare_config::CONFIG_API_UI_LAYOUT.as_bytes();
    pub const CONFIG_API_VALIDATE: &[u8] = mare_config::CONFIG_API_VALIDATE.as_bytes();
    pub const CONFIG_PACKAGES: &[u8] = mare_config::CONFIG_PACKAGES.as_bytes();
    pub const CONFIG_INTROSPECT: &[u8] = mare_config::CONFIG_INTROSPECT.as_bytes();
    pub const CONFIG_TXN: &[u8] = mare_config::CONFIG_TXN.as_bytes();
    pub const PLUGIN_CONFIG_API: &[u8] = mare_config::PLUGIN_CONFIG_API.as_bytes();
}

fn cached_chunk(lua: &Lua, cfg: &Table, key: &str, name: &str, bytes: &[u8]) -> mlua::Result<Function> {
    if let Value::Function(f) = cfg.raw_get::<Value>(key)? {
        return Ok(f);
    }
    let f = lua.load(bytes).set_name(name).into_function()?;
    cfg.raw_set(key, f.clone())?;
    Ok(f)
}

/// Evaluate [`mare_config::CONFIG_API`] with `(mare, config, core, ui_layout_factory, validate_factory)`
/// without touching `package.preload`.
pub fn load_config_api(lua: &Lua, mare: &Table, config: &Table) -> mlua::Result<()> {
    let core = mare_shared::lua::core::ensure(lua)?;
    let cfg = mare_shared::lua::core::config::handle(lua)?;
    let ui_layout: Function = match cfg.raw_get::<Value>("ui_layout_factory")? {
        Value::Function(f) => f,
        _ => {
            let f: Function = lua.load(bytecode::CONFIG_API_UI_LAYOUT).set_name("@core/config_api_ui_layout").eval()?;
            cfg.raw_set("ui_layout_factory", f.clone())?;
            f
        }
    };
    let validate: Function = match cfg.raw_get::<Value>("validate_factory")? {
        Value::Function(f) => f,
        _ => {
            let f: Function = lua.load(bytecode::CONFIG_API_VALIDATE).set_name("@core/config_api_validate").eval()?;
            cfg.raw_set("validate_factory", f.clone())?;
            f
        }
    };
    let chunk = cached_chunk(lua, &cfg, "config_api_chunk", "@core/config_api", bytecode::CONFIG_API)?;
    chunk.call::<()>((mare.clone(), config.clone(), core, ui_layout, validate))
}

/// Evaluate [`mare_config::CONFIG_PACKAGES`] with `(mare, config, core)`.
pub fn load_config_packages(lua: &Lua, mare: &Table, config: &Table) -> mlua::Result<()> {
    let core = mare_shared::lua::core::ensure(lua)?;
    let cfg = mare_shared::lua::core::config::handle(lua)?;
    let chunk = cached_chunk(lua, &cfg, "config_packages_chunk", "@core/config_packages", bytecode::CONFIG_PACKAGES)?;
    chunk.call::<()>((mare.clone(), config.clone(), core))
}

/// Evaluate [`mare_config::CONFIG_INTROSPECT`] with `(mare, core)`.
pub fn load_config_introspect(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let core = mare_shared::lua::core::ensure(lua)?;
    lua.load(bytecode::CONFIG_INTROSPECT).set_name("@core/config_introspect").call::<()>((mare.clone(), core))
}

/// Evaluate [`mare_config::PLUGIN_CONFIG_API`] with `(mare, config, apis, core)`.
pub fn load_plugin_config_api(lua: &Lua, mare: &Table, config: &Table, apis: Table) -> mlua::Result<()> {
    let core = mare_shared::lua::core::ensure(lua)?;
    let cfg = mare_shared::lua::core::config::handle(lua)?;
    let chunk =
        cached_chunk(lua, &cfg, "plugin_config_api_chunk", "@core/plugin_config_api", bytecode::PLUGIN_CONFIG_API)?;
    chunk.call::<()>((mare.clone(), config.clone(), apis, core))
}

/// Evaluate [`mare_config::CONFIG_TXN`] with `(mare, core)`.
pub fn load_config_txn(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let core = mare_shared::lua::core::ensure(lua)?;
    let cfg = mare_shared::lua::core::config::handle(lua)?;
    let chunk = cached_chunk(lua, &cfg, "config_txn_chunk", "@core/config_txn", bytecode::CONFIG_TXN)?;
    chunk.call::<()>((mare.clone(), core))
}

/// Install the core configuration Lua chunks into `lua` during `mare_plugin::install`.
pub fn install(lua: &Lua, mare: &Table, config: &Table) -> mlua::Result<()> {
    load_config_api(lua, mare, config)?;
    load_config_packages(lua, mare, config)?;
    load_config_introspect(lua, mare)?;
    mare_shared::lua::core::config::set_fresh_install_config(lua, Some(config))?;
    Ok(())
}
