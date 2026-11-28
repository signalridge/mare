//! The `mare.Error` value in Lua: a table `{code, message, data?}` that prints as
//! `code: message`. A Lua table raised as an error keeps no structure once it crosses a Rust
//! boundary (mlua keeps its print, `__tostring`, plus a traceback). Printing also records the
//! value under its print, so a bridge that gets the text back (mare.exec.run's thread) takes the
//! value itself again. (Catching it inside the thread instead would change how the thread ends:
//! a closer's error would replace the original.)
use mlua::{Lua, Table, Value};

const META: &str = "mare.error.meta";
const RECENT: &str = "mare.error.recent";
const ORDER: &str = "mare.error.order";
/// Printed values kept for recall; older ones are dropped (an error is recalled right after it
/// crossed a boundary, so a short history is enough, and it is held strongly so a collection in
/// between cannot lose it).
const KEPT: i64 = 128;

fn recent(lua: &Lua) -> mlua::Result<Table> {
    if let Ok(table) = lua.named_registry_value::<Table>(RECENT) {
        return Ok(table);
    }
    let table = lua.create_table()?;
    lua.set_named_registry_value(RECENT, &table)?;
    lua.set_named_registry_value(ORDER, lua.create_table()?)?;
    Ok(table)
}

fn remember(lua: &Lua, text: &str, error: Table) -> mlua::Result<()> {
    let table = recent(lua)?;
    let order: Table = lua.named_registry_value(ORDER)?;
    if table.raw_get::<Option<Table>>(text)?.is_none() {
        order.raw_push(text)?;
        if order.raw_len() as i64 > KEPT {
            let oldest: String = order.raw_get(1)?;
            lua.load("table.remove(...,1)").call::<()>(order.clone())?;
            table.raw_set(oldest, Value::Nil)?;
        }
    }
    table.raw_set(text, error)
}

/// The one `mare.Error` metatable.
pub fn error_metatable(lua: &Lua) -> mlua::Result<Table> {
    if let Ok(meta) = lua.named_registry_value::<Table>(META) {
        return Ok(meta);
    }
    let meta = lua.create_table()?;
    meta.set(
        "__tostring",
        lua.create_function(|lua, error: Table| {
            let text = format!("{}: {}", error.get::<String>("code")?, error.get::<String>("message")?);
            remember(lua, &text, error)?;
            Ok(text)
        })?,
    )?;
    meta.set("__name", "mare.Error")?;
    lua.set_named_registry_value(META, &meta)?;
    Ok(meta)
}

/// A new `mare.Error` value.
pub fn error_value(lua: &Lua, code: &str, message: &str, data: Value) -> mlua::Result<Table> {
    let table = lua.create_table()?;
    table.set("code", code)?;
    table.set("message", message)?;
    if !data.is_nil() {
        table.set("data", data)?;
    }
    table.set_metatable(Some(error_metatable(lua)?))?;
    Ok(table)
}

fn runtime_text(error: &mlua::Error) -> Option<&str> {
    match error {
        mlua::Error::RuntimeError(text) => Some(text),
        mlua::Error::CallbackError { cause, .. } => runtime_text(cause),
        _ => None,
    }
}

/// The `mare.Error` value an error crossing a Rust boundary carried: `text` as mlua reports it
/// (maybe with `runtime error: ` before and a traceback after) is that value's print, which may
/// span lines.
pub fn recall_error(lua: &Lua, text: &str) -> Option<Table> {
    let mut text = text;
    while let Some(rest) = text.strip_prefix("runtime error: ") {
        text = rest;
    }
    let printed = text.split("\nstack traceback:").next().unwrap_or(text);
    recent(lua).ok()?.get::<Option<Table>>(printed).ok().flatten()
}

/// Recall a structured `mare.Error` carried through an `mlua::Error`.
pub fn recall_mlua_error(lua: &Lua, error: &mlua::Error) -> Option<Table> {
    runtime_text(error).and_then(|text| recall_error(lua, text))
}

/// Format a structured `mare.Error` carried through an `mlua::Error` as `code: message`.
pub fn recall_code_message(lua: &Lua, error: &mlua::Error) -> Option<String> {
    let table = recall_mlua_error(lua, error).or_else(|| recall_error(lua, &error.to_string()))?;
    let (Ok(code), Ok(message)) = (table.get::<String>("code"), table.get::<String>("message")) else {
        return None;
    };
    Some(format!("{code}: {message}"))
}

/// Wrap a Rust callback returning `(ok, value_or_error)` so failures raise `error(value, 0)` on
/// the Lua side and keep structured `mare.Error` tables intact across the boundary.
pub fn wrap_coded(lua: &Lua, chunk_name: &str, inner: mlua::Function) -> mlua::Result<mlua::Function> {
    lua.load("local f = ... return function(...) local ok, v = f(...) if not ok then error(v, 0) end return v end")
        .set_name(chunk_name)
        .call(inner)
}
