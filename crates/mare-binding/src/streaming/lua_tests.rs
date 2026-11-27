use super::*;

#[test]
fn buffers_are_byte_exact_independent_and_snapshots_are_immutable() {
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load(
        r#"
        local a, b = mare.stream.buffer(), mare.stream.buffer()
        assert(a:len() == 0 and a:snapshot() == "")
        a:append("a\0")
        local first = a:snapshot()
        a:append("\195")
        a:append("\169")
        b:append("other")
        assert(a:len() == 4 and a:snapshot() == "a\0\195\169")
        assert(first == "a\0" and b:snapshot() == "other")
        a:clear()
        assert(a:len() == 0 and a:snapshot() == "")
        assert(first == "a\0" and b:snapshot() == "other")
        a:append("again")
        assert(a:snapshot() == "again")
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn coalescer_preserves_event_order_and_samples_the_supplied_clock() {
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    lua.globals().set("mare", mare).unwrap();
    lua.load(
        r#"
        local c = mare.stream.coalescer()
        assert(not c:has_pending() and c:flush(17) == nil)
        assert(c:append("text_delta", nil, "a"))
        assert(not c:due(49) and c:due(50))
        local a = c:flush(50)
        assert(a.type == "text_delta" and a.tool_call_id == nil and a.delta == "a")
        assert(c:append("tool_call_delta", "id\0x", "{"))
        assert(c:append("tool_call_delta", "id\0x", "}"))
        assert(not c:append("tool_call_delta", "other", "ignored"))
        assert(not c:append("reasoning_delta", "id\0x", "ignored"))
        assert(not c:due(99) and c:due(100))
        local b = c:flush(100)
        assert(b.type == "tool_call_delta" and b.tool_call_id == "id\0x" and b.delta == "{}")
        assert(not c:has_pending())
        assert(c:append("text_delta", nil, "pending"))
        local pending = c:take_pending()
        assert(pending.type == "text_delta" and pending.delta == "pending")
        assert(c:append("reasoning_delta", "x", "nested"))
        c:append_after_flush("text_delta", nil, "+outer")
        assert(not c:due(149) and c:due(150))
        local nested = c:flush(150)
        assert(nested.type == "reasoning_delta" and nested.tool_call_id == "x" and nested.delta == "nested+outer")
        "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn activity_lua_4k_chunk_buffer_equivalence_and_utf8_byte_offsets() {
    let lua = mlua::Lua::new();
    let mare = lua.create_table().unwrap();
    install(&lua, &mare).unwrap();
    crate::codec::install(&lua, &mare).unwrap();
    crate::activity::install(&lua, &mare).unwrap();
    crate::text::install(&lua, &mare, std::env::temp_dir()).unwrap();
    lua.globals().set("mare", mare).unwrap();

    let activity_src = include_str!("../../../mare-plugin/preset/plugins/lua/agent/activity.lua");
    let loader: mlua::Function = lua.load(activity_src).set_name("@preset/agent/activity.lua").eval().unwrap();
    lua.globals().set("LOAD_ACTIVITY", loader).unwrap();

    lua.load(
        r#"
        local listeners = {}
        local ctx = {
            on = function(_, name, fn)
                listeners[name] = listeners[name] or {}
                table.insert(listeners[name], fn)
            end,
            bus = { emit = function() end },
            llm = { resolve_params = function() return { params = {} } end },
            logger = { warn = function() end },
        }
        local function fire(name, ev)
            for _, fn in ipairs(listeners[name] or {}) do
                fn(ev)
            end
        end
        local A = { busy = true, turn_id = "t1", messages = {}, steer = {}, follow = {}, token = nil }
        local act = LOAD_ACTIVITY({ ctx = ctx, A = A })

        -- Start an assistant message item for committed item/delta accumulation.
        fire("step/started", {})
        fire("item/started", { item_id = "msg-1", item = { type = "assistant_message", message = { role = "assistant" } } })

        -- Stream 4096 x 256B chunks (1 MiB) including multi-byte UTF-8 sequences and interleaved tool calls.
        local ascii_chunk = string.rep("a", 256)
        local utf8_chunk = string.rep("界", 85) .. "!" -- 85*3 + 1 = 256 bytes
        local expected_text_parts = {}
        local expected_think_parts = {}
        local expected_tool_a_parts = {}
        local expected_tool_b_parts = {}

        act.delta({ type = "tool_call_start", tool_call_id = "tc-a", name = "read" })
        act.delta({ type = "tool_call_start", tool_call_id = "tc-b", name = "bash" })

        for i = 1, 4096 do
            local chunk = (i % 17 == 0) and utf8_chunk or ascii_chunk
            expected_text_parts[#expected_text_parts + 1] = chunk
            act.delta({ type = "text_delta", delta = chunk })
            fire("item/delta", { kind = "text", delta = chunk })
            if i % 64 == 0 then
                local th = "思" .. tostring(i) .. ":"
                expected_think_parts[#expected_think_parts + 1] = th
                act.delta({ type = "reasoning_delta", delta = th })
                fire("item/delta", { kind = "reasoning", delta = th })
                local arg_a = '{"k":' .. tostring(i) .. ','
                local arg_b = '{"cmd":"echo ' .. tostring(i) .. '",'
                expected_tool_a_parts[#expected_tool_a_parts + 1] = arg_a
                expected_tool_b_parts[#expected_tool_b_parts + 1] = arg_b
                act.delta({ type = "tool_call_delta", tool_call_id = "tc-a", delta = arg_a })
                act.delta({ type = "tool_call_delta", tool_call_id = "tc-b", delta = arg_b })
                fire("item/delta", { kind = "tool_arguments", tool_call_id = "tc-a", delta = arg_a })
                fire("item/delta", { kind = "tool_arguments", tool_call_id = "tc-b", delta = arg_b })
            end
        end

        local expected_text = table.concat(expected_text_parts)
        local expected_think = table.concat(expected_think_parts)
        local expected_tool_a = table.concat(expected_tool_a_parts)
        local expected_tool_b = table.concat(expected_tool_b_parts)
        assert(#expected_text == 4096 * 256)

        -- baseline_items(false) returns exact byte offsets without materializing text.
        local skel = act.baseline_items(false)
        assert(#skel == 1 and skel[1].item_id == "msg-1")
        local total_text_bytes, total_think_bytes = 0, 0
        local tc_a_bytes, tc_b_bytes = 0, 0
        for _, b in ipairs(skel[1].content) do
            assert(b.text == nil, "baseline_items(false) must not materialize text")
            if b.kind == "text" then
                total_text_bytes = total_text_bytes + b.offset
            elseif b.kind == "reasoning" then
                total_think_bytes = total_think_bytes + b.offset
            elseif b.kind == "tool_call" and b.id == "tc-a" then
                tc_a_bytes = tc_a_bytes + b.offset
            elseif b.kind == "tool_call" and b.id == "tc-b" then
                tc_b_bytes = tc_b_bytes + b.offset
            end
        end
        assert(total_text_bytes == #expected_text, "text byte offset mismatch")
        assert(total_think_bytes == #expected_think, "thinking byte offset mismatch")
        assert(tc_a_bytes == #expected_tool_a and tc_b_bytes == #expected_tool_b, "tool arg byte offset mismatch")

        -- baseline_items(true) materializes exact concatenated text and offsets.
        local full = act.baseline_items(true)
        local full_text_parts, full_think_parts = {}, {}
        local full_a, full_b
        for _, b in ipairs(full[1].content) do
            assert(#b.text == b.offset)
            if b.kind == "text" then
                full_text_parts[#full_text_parts + 1] = b.text
            elseif b.kind == "reasoning" then
                full_think_parts[#full_think_parts + 1] = b.text
            elseif b.kind == "tool_call" and b.id == "tc-a" then
                full_a = b.text
            elseif b.kind == "tool_call" and b.id == "tc-b" then
                full_b = b.text
            end
        end
        assert(table.concat(full_text_parts) == expected_text)
        assert(table.concat(full_think_parts) == expected_think)
        assert(full_a == expected_tool_a and full_b == expected_tool_b)

        -- Raw snapshot().streaming matches exact concatenated text, reasoning, and tool raw_arguments.
        local snap = act.snapshot()
        local raw_text_parts, raw_think_parts = {}, {}
        local raw_a, raw_b
        for _, b in ipairs(snap.streaming) do
            if b.type == "text" then
                raw_text_parts[#raw_text_parts + 1] = b.text
            elseif b.type == "reasoning" then
                raw_think_parts[#raw_think_parts + 1] = b.text
            elseif b.type == "tool_call" and b.id == "tc-a" then
                raw_a = b.raw_arguments
            elseif b.type == "tool_call" and b.id == "tc-b" then
                raw_b = b.raw_arguments
            end
        end
        assert(table.concat(raw_text_parts) == expected_text)
        assert(table.concat(raw_think_parts) == expected_think)
        assert(raw_a == expected_tool_a and raw_b == expected_tool_b)

        -- Raw M.settled() clears streaming without clobbering committed baseline_items.
        act.settled()
        assert(#act.snapshot().streaming == 0)
        assert(#act.baseline_items(false)[1].content > 0)
        "#,
    )
    .exec()
    .unwrap();
}
