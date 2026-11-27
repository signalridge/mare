//! Lua-only contract parameters (`lua = {name = LuaLS type}`): methods exposed in process only
//! can take Lua values with no wire form (functions, tables holding them).

use super::{Origin, kernel, src_at};

const LUA_PARAMS: &str = r#"return { name = "lua-svc", provide = {"sections"}, apply = function(ctx)
  mare._sections = {}
  local Sections = require("cordis").class("Sections", mare.Service)
  function Sections:constructor(c)
    Sections.super.constructor(self, c, "sections", { contract = { methods = { section = { doc = 'add a section', expose = 'lua', registration = true,
      params = { type = 'object', properties = { name = { type = 'string' } }, required = { 'name' },
        additionalProperties = false },
      order = { 'name', 'render', 'def' },
      -- Lua values with no wire form: a function, and a table holding one
      lua = { render = 'fun(): string', def = '{ suffix: fun(): string }?' } } } } })
  end
  function Sections:section(name, render, def)
    mare._sections[name] = render() .. (def and def.suffix() or "")
    return function() mare._sections[name] = nil end
  end
  ctx:plugin(Sections)
end }"#;

const LUA_USER: &str = r#"return { name = "lua-user", inject = {"sections"}, apply = function(ctx)
  ctx.sections:section("intro", function() return "hi" end, { suffix = function() return "!" end })
  local ok, e = pcall(ctx.sections.section, ctx.sections, 42, function() return "x" end)
  mare._bad_name = ok and "accepted" or e.code
end }"#;

/// Lua-only parameters (`lua = {name = LuaLS type}`): a registration method can take functions and
/// tables holding functions; they skip the JSON admission, the other parameters are still checked.
#[tokio::test]
async fn lua_only_parameters_reach_the_provider_and_the_rest_is_still_checked() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let k = kernel();
            k.set_defs(vec![
                k.load_def(&src_at("lua-svc", Origin::Builtin, LUA_PARAMS)).unwrap(),
                k.load_def(&src_at("lua-user", Origin::User, LUA_USER)).unwrap(),
            ]);
            k.start().await.unwrap();
            let lua = k.lua().clone();
            let state: String =
                lua.load("return tostring(mare._sections.intro) .. '|' .. tostring(mare._bad_name)").eval().unwrap();
            assert_eq!(state, "hi!|invalid_arguments", "{:?}", k.problems());
        })
        .await
}

/// A Lua-only parameter has no wire form, so a method exported to JSON callers cannot declare one.
#[test]
fn a_remote_method_cannot_declare_lua_only_parameters() {
    let lua = mlua::Lua::new();
    let contracts = lua
        .load(
            "return { svc = { methods = { m = { doc = 'm', expose = 'both', order = { 'f' }, \
             result = { type = 'object' }, lua = { f = 'fun()' } } } } }",
        )
        .eval()
        .unwrap();
    let error = mare_kernel::contracts::parse_contracts(&lua, contracts).unwrap_err().to_string();
    assert!(error.contains("Lua-only"), "{error}");
}
