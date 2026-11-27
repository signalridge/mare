//! Rust accessors and interface boundary for the core Lua keymap engine (`core.keymap`).
//!
//! Defines the stable boundary between:
//! - Rust host / UI bindings (`chrome.rs`, `reload.rs`, `lifecycle.rs`, `app/input/*`)
//! - Config evaluation / reload (`eval.rs`, `reevaluate.rs`) via [`apply_config_keys`] and the
//!   config snapshot/commit/restore hooks
//! - The keymap engine in `preset/keymap.lua`
//!
//! Host-provided callbacks (`check`, `context`, `contexts`, `core_actions`, `set_bindings`,
//! `context_info`, `hint`, `is_printable`, `is_reserved`, `is_core_action`, `resolve`, `snapshot`,
//! `current_owner`, `attach_disposer`) and keymap-engine-provided hooks (`apply_config_keys`,
//! `begin_reload`, `finish_reload`, `rollback_reload`, `settle`, `invoke`, `maps`,
//! `current_source`, `config_*`) all live strictly on the registry-held `core.keymap` table.

use mlua::{Function, Lua, Table, Value};

/// Return the `core.keymap` namespace table on the VM's core handle.
pub fn handle(lua: &Lua) -> mlua::Result<Table> {
    super::namespace(lua, "keymap")
}

fn core_hook(lua: &Lua, name: &str) -> Option<Function> {
    super::get(lua)
        .ok()
        .flatten()
        .and_then(|core| core.get::<Table>("keymap").ok())
        .and_then(|km| km.get::<Function>(name).ok())
}

fn require_hook(lua: &Lua, name: &str) -> mlua::Result<Function> {
    core_hook(lua, name).ok_or_else(|| mlua::Error::runtime(format!("core.keymap.{name} is unavailable")))
}

// ---------------------------------------------------------------------------
// CFG <-> KEY interface boundary: config-evaluated `mare.setup{keys}`
// ---------------------------------------------------------------------------

/// Notify the keymap engine of the config-evaluated or runtime-updated `keys` table.
///
/// Interface boundary between CFG (config evaluation / runtime `option/changed`) and KEY:
/// - `keys`: the resolved `mare.config.keys` table (or `Value::Nil` when unset)
/// - `sources`: the config sources table (maps `"keys.<k>"` / `"keys"` -> `{ layer = ... }`)
/// - `staged`: `true` while running inside a staged `/reload config` evaluation
pub fn apply_config_keys(lua: &Lua, keys: Value, sources: Value, staged: bool) -> mlua::Result<()> {
    if let Some(f) = core_hook(lua, "apply_config_keys") {
        f.call::<()>((keys, sources, staged))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reload lifecycle and settle hooks
// ---------------------------------------------------------------------------

pub fn begin_reload(lua: &Lua) {
    if let Some(f) = core_hook(lua, "begin_reload") {
        let _ = f.call::<()>(());
    }
}

pub fn finish_reload(lua: &Lua, strict: bool) -> Vec<String> {
    if let Some(f) = core_hook(lua, "finish_reload")
        && let Ok(diags) = f.call::<Vec<String>>(strict)
    {
        return diags;
    }
    Vec::new()
}

pub fn rollback_reload(lua: &Lua) {
    if let Some(f) = core_hook(lua, "rollback_reload") {
        let _ = f.call::<()>(());
    }
}

pub fn settle(lua: &Lua, _mare: &Table) -> Option<Vec<String>> {
    let f = core_hook(lua, "settle")?;
    f.call::<Vec<String>>(()).ok()
}

// ---------------------------------------------------------------------------
// Action dispatch, binding maps, and current binding source
// ---------------------------------------------------------------------------

pub fn invoke_fn(lua: &Lua) -> mlua::Result<Function> {
    require_hook(lua, "invoke")
}

pub fn maps(lua: &Lua) -> mlua::Result<Option<Table>> {
    if let Some(f) = core_hook(lua, "maps") {
        return f.call(());
    }
    Ok(None)
}

pub fn current_source(lua: &Lua) -> mlua::Result<Option<(u8, String, String)>> {
    if let Some(f) = core_hook(lua, "current_source") {
        let triple: (u8, String, String) = f.call(())?;
        return Ok(Some(triple));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Config re-evaluation snapshot / commit / rollback hooks
// ---------------------------------------------------------------------------

pub fn config_snapshot(lua: &Lua, _mare: &Table, previous_keymaps: Value) -> mlua::Result<Value> {
    require_hook(lua, "config_snapshot")?.call(previous_keymaps)
}

pub fn config_restore_fn(lua: &Lua, _mare: &Table) -> mlua::Result<Function> {
    require_hook(lua, "config_restore")
}

pub fn config_actions_snapshot(lua: &Lua, _mare: &Table) -> mlua::Result<Value> {
    require_hook(lua, "config_actions_snapshot")?.call(())
}

pub fn config_actions_restore_fn(lua: &Lua, _mare: &Table) -> mlua::Result<Function> {
    require_hook(lua, "config_actions_restore")
}

pub fn config_actions_commit(lua: &Lua, _mare: &Table, snapshot: Value) -> mlua::Result<()> {
    if let Some(f) = core_hook(lua, "config_actions_commit") {
        f.call::<()>(snapshot)?;
    }
    Ok(())
}

pub fn config_rollback_contexts_fn(lua: &Lua, _mare: &Table) -> mlua::Result<Function> {
    require_hook(lua, "config_rollback_contexts")
}

pub fn config_commit_contexts(lua: &Lua, _mare: &Table, previous: Value, current: Value) -> mlua::Result<()> {
    require_hook(lua, "config_commit_contexts")?.call::<()>((previous, current))
}

pub fn config_validate_stage(lua: &Lua, _mare: &Table, stage: Table, previous_keymaps: Value) -> mlua::Result<()> {
    if let Some(f) = core_hook(lua, "config_validate_stage") {
        f.call::<()>((stage, previous_keymaps))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_keymap_hooks_handle_absent_and_installed_callbacks() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();

        // When no core.keymap hooks are installed yet, optional hooks degrade cleanly
        // and required hooks return a clear error.
        begin_reload(&lua);
        assert!(finish_reload(&lua, true).is_empty());
        rollback_reload(&lua);
        assert!(settle(&lua, &mare).is_none());
        assert!(maps(&lua).unwrap().is_none());
        assert!(current_source(&lua).unwrap().is_none());
        assert!(invoke_fn(&lua).is_err());
        apply_config_keys(&lua, Value::Nil, Value::Nil, false).unwrap();
        config_actions_commit(&lua, &mare, Value::Nil).unwrap();
        config_validate_stage(&lua, &mare, lua.create_table().unwrap(), Value::Nil).unwrap();

        // Once core.keymap hooks are installed, every accessor forwards arguments and return values.
        let km = handle(&lua).unwrap();
        km.set(
            "current_source",
            lua.create_function(|_, ()| Ok((2u8, "plugin".to_string(), "demo".to_string()))).unwrap(),
        )
        .unwrap();
        km.set("maps", lua.create_function(|lua, ()| lua.create_table()).unwrap()).unwrap();
        km.set("finish_reload", lua.create_function(|_, _strict: bool| Ok(vec!["diag".to_string()])).unwrap()).unwrap();
        km.set("begin_reload", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        km.set("rollback_reload", lua.create_function(|_, ()| Ok(())).unwrap()).unwrap();
        km.set("config_actions_commit", lua.create_function(|_, _snap: Value| Ok(())).unwrap()).unwrap();
        km.set("config_validate_stage", lua.create_function(|_, (_s, _p): (Table, Value)| Ok(())).unwrap()).unwrap();

        assert_eq!(current_source(&lua).unwrap(), Some((2, "plugin".to_string(), "demo".to_string())));
        assert!(maps(&lua).unwrap().is_some());
        assert_eq!(finish_reload(&lua, false), vec!["diag".to_string()]);
        begin_reload(&lua);
        rollback_reload(&lua);
        config_actions_commit(&lua, &mare, Value::Nil).unwrap();
        config_validate_stage(&lua, &mare, lua.create_table().unwrap(), Value::Nil).unwrap();
    }
}
