//! Synchronous JSON-only state provider boundary for extensible activity queries.
use std::cell::Cell;

use mare_shared::lua::{copy_json_metadata, json_to_lua_lossless, strict_lua_to_json};
use mlua::{Function, Lua, Table, Value};

thread_local! {
    static SYNC_CAPTURE_DEPTH: Cell<usize> = const { Cell::new(0) };
}

struct SyncCaptureGuard;

impl SyncCaptureGuard {
    fn enter() -> Self {
        SYNC_CAPTURE_DEPTH.with(|depth| depth.set(depth.get() + 1));
        Self
    }
}

impl Drop for SyncCaptureGuard {
    fn drop(&mut self) {
        SYNC_CAPTURE_DEPTH.with(|depth| depth.set(depth.get() - 1));
    }
}

/// Whether the current thread is executing inside a synchronous `mare.activity.capture` callback.
pub(crate) fn in_sync_capture() -> bool {
    SYNC_CAPTURE_DEPTH.with(Cell::get) > 0
}

pub fn install(lua: &Lua, mare: &Table) -> mlua::Result<()> {
    let activity = lua.create_table()?;
    activity.set(
        "capture",
        lua.create_function(|lua, provider: Function| {
            // call, not call_async: yielding providers fail closed rather than mixing generations.
            let value: Value = {
                let _guard = SyncCaptureGuard::enter();
                provider.call(())?
            };
            if value.is_nil() {
                return Ok(Value::Nil);
            }
            let json = strict_lua_to_json(lua, value.clone())?;
            let cloned = json_to_lua_lossless(lua, &json)?;
            copy_json_metadata(lua, &value, &cloned)?;
            Ok(cloned)
        })?,
    )?;
    mare.set("activity", activity)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn providers_cannot_yield_or_return_cycles_and_copies_are_detached() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare).unwrap();
        crate::codec::install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        lua.load(
            r#"
            assert(not pcall(mare.activity.capture, function() coroutine.yield() end))
            local cycle={}; cycle.self=cycle
            assert(not pcall(mare.activity.capture,function() return cycle end))
            local original={nested={value=1}}
            local detached=mare.activity.capture(function() return original end)
            detached.nested.value=2
            assert(original.nested.value==1)
            assert(mare.activity.capture(function() return nil end)==nil)
            assert(mare.activity.capture(function() return mare.json.null end)==mare.json.null)
            local fixture=assert(mare.json.decode_lossless('{"n":null,"a":[],"o":{}}'))
            local cloned=mare.activity.capture(function() return fixture end)
            assert(mare.json.encode_strict(cloned)=='{"a":[],"n":null,"o":{}}')
            assert(not pcall(mare.activity.capture,function() return 0/0 end))
            assert(not pcall(mare.activity.capture,function() return math.huge end))
        "#,
        )
        .exec()
        .unwrap();
    }
    #[test]
    fn capture_preserves_nested_codec_schema_serialization_order() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        crate::codec::install(&lua, &mare).unwrap();
        install(&lua, &mare).unwrap();
        lua.globals().set("mare", mare).unwrap();
        let schema = json_to_lua_lossless(
            &lua,
            &serde_json::json!({
                "type":"object","x-mare-order":["z","a"],"properties":{"z":{"type":"string"},"a":{"const":{"x-mare-order":["user-data"]}}},"required":["z"]
            }),
        )
        .unwrap();
        mare_shared::lua::mark_tool_schema(&lua, &schema).unwrap();
        lua.globals().set("SOURCE_SCHEMA", schema).unwrap();
        lua.load(
            r#"
            local source={nested={parameters=SOURCE_SCHEMA}}
            local copy=mare.activity.capture(function() return source end)
            assert(mare.json.encode(source)==mare.json.encode(copy))
            assert(copy.nested.parameters.properties.a.const["x-mare-order"][1]=="user-data")
            copy.nested.parameters.properties.z.description="detached"
            assert(SOURCE_SCHEMA.properties.z.description==nil)
        "#,
        )
        .exec()
        .unwrap();
    }

    #[test]
    fn sync_capture_flag_is_scoped_and_restored_on_error() {
        let lua = Lua::new();
        let mare = lua.create_table().unwrap();
        install(&lua, &mare).unwrap();
        mare.set("check_capture", lua.create_function(|_, ()| Ok(in_sync_capture())).unwrap()).unwrap();
        lua.globals().set("mare", mare).unwrap();
        assert!(!in_sync_capture());
        lua.load(
            r#"
            assert(mare.check_capture() == false)
            local inside = mare.activity.capture(function()
                return mare.check_capture()
            end)
            assert(inside == true)
            assert(mare.check_capture() == false)
            local ok = pcall(mare.activity.capture, function()
                assert(mare.check_capture() == true)
                error("boom")
            end)
            assert(not ok)
            assert(mare.check_capture() == false)
        "#,
        )
        .exec()
        .unwrap();
        assert!(!in_sync_capture());
    }
}
