mod config;
pub mod core;
mod cov;
mod json;
mod shape;
mod tasks;

#[cfg(test)]
mod tests;

pub(crate) use config::{FN_SENTINEL, config_upvalue, fn_registry};
pub use config::{
    begin_config_eval, config_to_json, config_to_lua, restore_config_fn_registry, restore_config_fns,
    retire_config_fns, snapshot_config_fns,
};
pub use cov::{
    COV_TARGET_REGISTRY, CovLineCallback, attach_cov_hook, call_async, dispatch_cov_line_hook, has_cov_hook,
    has_cov_hook_raw, install_cov_hook,
};
pub use json::{
    HISTORY_LOSSLESS_MAX_NODES, STRICT_JSON_MAX_NODES, json_to_lua, json_to_lua_lossless, json_to_lua_lossless_bounded,
    json_to_lua_with_null, lua_to_json, lua_to_json_lossy, lua_to_json_lossy_bounded, strict_lua_to_json,
    strict_lua_to_json_bounded,
};
pub use shape::{
    ShapeMarks, apply_shape, apply_shape_with, copy_json_metadata, copy_tool_schema_marker, deep_clone_value,
    mark_tool_schema, order_schema,
};
#[cfg(debug_assertions)]
pub use tasks::fail_next_task_wrap;
pub use tasks::{TaskHandle, Token};

use mlua::{Lua, Table, Value};

pub fn table_or_empty(lua: &Lua, t: Option<Table>) -> mlua::Result<Table> {
    match t {
        Some(t) => Ok(t),
        None => lua.create_table(),
    }
}

pub fn rt_err(msg: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(msg.to_string())
}

/// Return `(nil, err)` pair convention helper.
pub fn nil_err(lua: &Lua, e: impl std::fmt::Display) -> mlua::Result<mlua::MultiValue> {
    let _ = lua;
    Ok(mlua::MultiValue::from_vec(vec![Value::Nil, Value::String(lua.create_string(e.to_string())?)]))
}

/// A fresh environment for a plugin chunk or init.lua: globals written there stay there
/// (`_G` points at the env itself); reads fall back to the shared globals. The standard library
/// tables are frozen (see `freeze_stdlib`), so plugins cannot patch them for everyone.
pub fn isolated_env(lua: &Lua) -> mlua::Result<Table> {
    let env = lua.create_table()?;
    let meta = lua.create_table()?;
    meta.set("__index", lua.globals())?;
    env.set_metatable(Some(meta))?;
    env.set("_G", env.clone())?;
    Ok(env)
}
