use mlua::{Function, Lua, Table};

#[test]
fn tool_constraints_intersect_fail_closed_and_follow_owner_lifetime() -> mlua::Result<()> {
    let lua = Lua::new();
    let install: Function = lua.load(include_str!("../preset/plugins/lua/llm/tool_constraints.lua")).eval()?;
    let d = lua.create_table()?;
    d.set("llm", lua.create_table()?)?;
    let apply: Function = install.call(d.clone())?;
    lua.globals().set("llm", d.get::<Table>("llm")?)?;
    lua.globals().set("apply", apply)?;
    lua.load(
        r#"
        local disposers = {}
        -- the caller's cordis effect (af6484ed): run now, its cleanup kept for disposal
        local owner = { effect = function(_, fn) disposers[#disposers+1] = fn() end }
        local req = {model={ref="test/model"},purpose="agent",tools={{name="read"},{name="write"}}}
        local off = llm:constrain_tools(owner, function(info)
            assert(info.model_ref == "test/model" and info.tools == nil)
            info.purpose = "changed"
            return {"read", "never-declared"}
        end)
        local result = apply(req)
        assert(#result.tools == 1 and result.tools[1].name == "read")
        assert(#req.tools == 2 and req.purpose == "agent")
        local empty = llm:constrain_tools(owner, function() return {} end)
        assert(#apply(req).tools == 0)
        empty(); empty()
        assert(#apply(req).tools == 1)
        off(); assert(#apply(req).tools == 2)
        local bad = llm:constrain_tools(owner, function() error("broken") end)
        assert(not pcall(apply, req)); bad()
        bad = llm:constrain_tools(owner, function() return {[2]="read"} end)
        assert(not pcall(apply, req)); bad()
        llm:constrain_tools(owner, function() return {} end)
        for _, dispose in ipairs(disposers) do dispose() end
        assert(#apply(req).tools == 2)
    "#,
    )
    .exec()
}
