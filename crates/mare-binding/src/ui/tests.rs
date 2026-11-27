use std::sync::atomic::Ordering;

use mare_shared::ui::{UiCmd, UiShared};
use mlua::Lua;

fn setup_ui() -> (Lua, std::sync::Arc<UiShared>, tokio::sync::mpsc::UnboundedReceiver<UiCmd>) {
    let lua = Lua::new();
    let shared = UiShared::new();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    *shared.tx.write().unwrap() = Some(tx);
    let mare = lua.create_table().unwrap();
    let o = lua.create_table().unwrap();
    mare.set("o", o).unwrap();
    let ui = lua.create_table().unwrap();
    super::install(&lua, &mare, ui, shared.clone()).unwrap();
    lua.globals().set("mare", mare).unwrap();
    (lua, shared, rx)
}

#[test]
fn oversized_gap_is_rejected_by_ui_node_entry_points() {
    let (lua, _shared, mut rx) = setup_ui();
    lua.load(
        r#"
        local ui = mare.ui
        assert(ui.validate_node({ type = "gap" }))
        assert(ui.validate_node({ type = "gap", n = 0 }))
        assert(ui.validate_node({ type = "gap", n = 1000 }))
        local gap = { type = "gap", n = 1001 }
        local function rejected(fn)
            local ok, err = pcall(fn)
            assert(not ok and tostring(err):find("gap", 1, true), tostring(err))
        end
        rejected(function() ui.validate_node(gap) end)
        for _, n in ipairs({ -1, 0.5, math.huge, math.maxinteger }) do
            assert(not pcall(ui.validate_node, { type = "gap", n = n }))
        end
        rejected(function() ui.validate_node({ type = "lines", items = { gap } }) end)
        rejected(function() ui.push("assistant", { node = gap }) end)
        rejected(function() ui.update(1, { expanded = gap }) end)
        rejected(function() ui.set_widget("gap", nil, gap) end)
        rejected(function() ui.set_status("gap", nil, gap) end)
        rejected(function() ui.print(gap) end)
        "#,
    )
    .exec()
    .unwrap();
    assert!(rx.try_recv().is_err(), "rejected nodes must not enqueue UI commands");
}

#[test]
fn surface_queries_blocks_widgets_status_overlay_and_transcript() {
    let (lua, shared, mut rx) = setup_ui();
    let km_core = mare_shared::lua::core::keymap::handle(&lua).unwrap();
    lua.load(
        r##"
        local km = ...
        local ui = mare.ui
        assert(type(ui.image_id()) == "number")
        assert(type(ui.layout_revision()) == "number")
        local blocks = ui.markdown_blocks("```rust\nfn f() {}\n```\n")
        assert(#blocks == 1 and blocks[1].language == "rust" and blocks[1].closed == true)
        local ok, info = ui.validate_node("hello")
        assert(ok == true and info.images == false)
        assert(ui.connected() == true and ui.can_respond() == true)
        assert(ui.width() == 100 and ui.height() == 40)
        assert(type(ui.display_path("/tmp/some/long/path/file.rs", 12)) == "string")

        local id = ui.push("assistant", { markdown = "hi" })
        assert(type(id) == "number")
        ui.push("assistant")
        ui.update(id, { title = "t", kind = "tool" })
        ui.append(id, "!")
        ui.finish(id)
        ui.remove(id)
        ui.print("printed")

        ui.set_widget("w1", nil, "above", 1)
        ui.set_widget("w1", "below-editor", nil, 0)
        local ok_w = pcall(ui.set_widget, "w2", "invalid-slot", "x")
        assert(not ok_w)

        ui.set_status("s1", nil, "left", 1, true)
        ui.set_status("s1", "right", nil, 0, false)
        local ok_s = pcall(ui.set_status, "s2", "middle", "x")
        assert(not ok_s)
        assert(ui._validate_status_align == nil)

        ui.working("thinking", "step 1", { background = true })
        ui.set_queued(3)
        ui.set_queue_mode(2)
        assert(ui.focused() == nil)
        assert(type(ui.caps()) == "table")
        assert(type(ui.slow_frames()) == "table")
        ui.notify("hello")
        ui.notify("warn", "warning")
        ui.overlay("Help", "body")
        km.context("info.panel", "info")
        ui.overlay("Panel", "line0\nline1", { context = "info.panel", cursor = { row = 1, col = 2 } })
        assert(not pcall(ui.overlay, "Bad", "body", { context = "info.unregistered" }))
        assert(not pcall(ui.overlay, "Bad", "body", { context = "editor" }))
        assert(not pcall(ui.overlay, "Bad", "body", { context = "global" }))
        assert(not pcall(ui.overlay, "Bad", "body", { cursor = "bad" }))
        assert(not pcall(ui.overlay, "Bad", "body", { cursor = { row = -1, col = 0 } }))
        assert(not pcall(ui.overlay, "Bad", "body", { cursor = { row = 0 } }))
        ui.transcript("Log", "content", {
            keys = {
                { action = "close", label = "Close" },
                { action = { "up", "down" }, label = "Move" },
            },
        })
        assert(not pcall(ui.transcript, "Log", "x", { keys = { { action = 123 } } }))
        assert(ui.transcript_title() == nil)
        ui.transcript_close("Log")
        ui.clear()
        ui.exit_guard(true)
        ui.exit()
        ui.exit({ force = true })
        assert(type(ui.render_text("plain", 40)) == "string")
    "##,
    )
    .call::<()>(km_core)
    .unwrap();

    assert_eq!(shared.queued.load(Ordering::Relaxed), 3);
    assert_eq!(shared.queue_mode.load(Ordering::Relaxed), 2);
    assert!(rx.try_recv().is_ok());

    // Focused + notify_os only_unfocused + headless fallback branches
    shared.focus_known.store(true, Ordering::Relaxed);
    shared.focused.store(true, Ordering::Relaxed);
    lua.load(
        r#"
        assert(mare.ui.focused() == true)
        assert(mare.ui.notify_os("ping", { only_unfocused = true }) == false)
        assert(mare.ui.notify_os("ping", { only_unfocused = false, protocol = "osc9", bell = true }) == true)
    "#,
    )
    .exec()
    .unwrap();

    *shared.tx.write().unwrap() = None;
    lua.load(
        r#"
        assert(mare.ui.connected() == false)
        mare.ui.print("headless")
        mare.ui.notify("headless", "info")
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn surface_layout_parsing_normalization_and_validation_errors() {
    let (lua, shared, _rx) = setup_ui();
    lua.load(
        r##"
        local ui = mare.ui
        ui.set_layout(nil)
        assert(type(mare.o.ui.layout.order) == "table")
        ui.set_layout({ "status", "live", "editor" })
        assert(mare.o.ui.layout.order[1] == "status")
        ui.set_layout({
            order = { "status", "live", "editor" },
            regions = { ["below-editor"] = { max_height = 5.0, hidden = false } },
        })
        assert(mare.o.ui.layout.regions["below-editor"].max_height == 5)
        ui.set_layout(nil, { persist = false })
        assert(mare.o.ui.layout.regions["below-editor"].max_height == 5, "persist=false must keep mare.o.ui.layout")

        local bad_specs = {
            "not-a-table",
            { 123 },
            { "live", order = { "live" } },
            { order = "bad" },
            { order = { 123 } },
            { regions = "bad" },
            { regions = { [1] = {} } },
            { regions = { [string.rep("a", 80)] = {} } },
            { regions = { live = "bad" } },
            { regions = { live = { [1] = true } } },
            { regions = { live = { max_height = -1 } } },
            { regions = { live = { max_height = 1.5 } } },
            { regions = { live = { hidden = "yes" } } },
            { regions = { live = { unknown_key = 1 } } },
            { unknown_top = 1 },
            { [true] = 1 },
        }
        for i, spec in ipairs(bad_specs) do
            local ok = pcall(ui.set_layout, spec)
            assert(not ok, "expected layout error for case " .. i)
        end
    "##,
    )
    .exec()
    .unwrap();
    assert!(!shared.layout.read().unwrap().order.is_empty());
}

#[test]
fn chrome_theme_styles_spinner_editor_keymap_and_syntax_theme() {
    let (lua, shared, _rx) = setup_ui();
    let valid_tmtheme = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>name</key><string>Minimal</string>
  <key>settings</key><array><dict><key>settings</key><dict>
    <key>background</key><string>#111111</string>
    <key>foreground</key><string>#eeeeee</string>
  </dict></dict></array>
</dict></plist>"#;
    lua.globals().set("VALID_TMTHEME", valid_tmtheme).unwrap();

    let km_core = mare_shared::lua::core::keymap::handle(&lua).unwrap();
    lua.load(
        r##"
        local km = ...
        local ui = mare.ui
        assert(ui.set_border == nil and ui.set_spinner == nil and ui.set_icons == nil and ui.set_markdown == nil)
        assert(ui._keymap_check == nil and ui._keymap_context == nil and ui._set_bindings == nil and ui._key_label == nil and ui._normalize_seq == nil)
        assert(ui.set_theme_data ~= nil, "set_theme_data must exist")
        ui.set_theme_data({ border = "double", icons = "ascii", glyphs = { frames = { "|", "/" }, still = "o" }, markdown = { glyphs = { h1 = "#h1" }, code_line_numbers = true } })
        assert(ui.icon_set() == "ascii")
        -- Independent 4-field update preserves untouched fields.
        ui.set_theme_data({ border = "single" })
        assert(ui.icon_set() == "ascii")
        ui.set_theme_data({ glyphs = false })
        ui.set_theme_data(nil)
        ui.set_ambiwidth("double")
        ui.set_ambiwidth("single")
        ui.set_title("Mare")

        ui.set_editor("draft", {
            keep_draft = true,
            prepend = true,
            images = {},
            image_markers = {},
        })
        ui.flag("show_reasoning", true)
        ui.editor_hint("Enter to send")
        ui.editor_config({
            prompt = "> ",
            placeholder = "Ask",
            busy_placeholder = "Wait",
            border = "single",
            shell = { placeholder = "$ ", private_placeholder = "priv", border = "rounded" },
        })
        ui.editor_config({ shell = false })
        ui.editor_config({ shell = nil })

        ui.style("accent", { fg = "#ff0000", bg = 4, bold = true, italic = true })
        local st = ui.style("accent")
        assert(st.fg == "#ff0000" and st.bg == "4" and st.bold == true and st.italic == true)
        local reset_st = ui.style("nonexistent_reset_style")
        assert(type(reset_st) == "table")
        assert(type(ui.styles()) == "table")
        assert(type(ui.normalize_key("C-c")) == "string")
        assert(type(ui.normalize_key("<C-x>g")) == "string")

        local a1, n1, e1 = km.check("editor", "g g", nil)
        assert(a1 == 0 and n1 == 0 and e1 == nil)
        local _, _, e2 = km.check("modal", "g g", "has_selection")
        assert(type(e2) == "string")
        local _, _, e3 = km.check("editor", "x", { "not_a_real_predicate" })
        assert(type(e3) == "string")
        local _, _, e4 = km.check("editor", "x", 123)
        assert(type(e4) == "string")
        local _, _, e5 = km.check("transcript", "x", nil, "steer")
        assert(type(e5) == "string" and e5:find("not available", 1, true) ~= nil)
        local _, _, e6 = km.check("global", "g g", nil)
        assert(type(e6) == "string")

        km.context("editor.custom", "editor")
        local ok_ctx = false
        for _, c in ipairs(km.contexts()) do
            if c == "editor.custom" then ok_ctx = true end
        end
        assert(ok_ctx)
        assert(#km.core_actions() >= 29)
        km.context("editor.custom", nil)

        km.set_bindings("editor", "C-k", {
            { action = "delete_line", rank = 1, seq = 1, all = 0, none = 0, lua = false },
        })
        assert(#km.hint("delete_line", "editor") >= 1)
        assert(ui.keymap()["C-k"] == "delete_line")

        ui.theme(nil)
        ui.theme({ accent = "#112233", ignored = "not-a-color" })

        -- syntax_theme: validate_only must not mutate active syntax; >1MiB and malformed XML fail
        ui.syntax_theme(VALID_TMTHEME, true)
        assert(not pcall(ui.syntax_theme, "<not-valid-plist", false))
        assert(not pcall(ui.syntax_theme, string.rep("x", 1024 * 1024 + 1), false))
        ui.syntax_theme(VALID_TMTHEME, false)
        ui.syntax_theme(nil, false)
    "##,
    )
    .call::<()>(km_core)
    .unwrap();

    assert!(shared.theme.read().unwrap().syntax.is_none());
}

#[test]
fn editor_state_and_completion_registry_apis_validate_and_track_state() {
    let (lua, shared, mut rx) = setup_ui();
    let km_core = mare_shared::lua::core::keymap::handle(&lua).unwrap();
    lua.load(
        r##"
        local km = ...
        local ui = mare.ui
        -- editor state accessors + mutators (headless/UiShared fallback)
        assert(type(ui.get_editor) == "function")
        assert(type(ui.editor_state) == "function")
        assert(type(ui.editor_cursor) == "function")
        assert(type(ui.set_editor_cursor) == "function")
        assert(type(ui.editor_selection) == "function")
        assert(type(ui.set_editor_selection) == "function")
        assert(type(ui.editor_insert) == "function")
        assert(type(ui.editor_replace) == "function")

        assert(ui.get_editor() == "")
        local st = ui.editor_state()
        assert(st.text == "" and st.cursor.row == 0 and st.cursor.col == 0 and st.selection == nil)
        ui.set_editor_cursor(0, 0)
        ui.set_editor_selection({ row = 0, col = 0 }, { row = 0, col = 0 })
        ui.set_editor_selection(nil)
        ui.editor_insert("abc")
        assert(ui.get_editor() == "abc", "expected 'abc', got " .. tostring(ui.get_editor()))
        ui.editor_replace({ start = { row = 0, col = 0 }, ["end"] = { row = 0, col = 1 } }, "A")
        assert(ui.get_editor() == "Abc", "expected 'Abc', got " .. tostring(ui.get_editor()))
        ui.set_editor_selection({ row = 0, col = 1 }, { row = 0, col = 3 })
        assert(ui.editor_selection().text == "bc")
        ui.editor_insert("YZ")
        assert(ui.get_editor() == "AYZ", "expected 'AYZ', got " .. tostring(ui.get_editor()))
        assert(not pcall(ui.set_editor_selection, { start = { row = 0, col = 0 }, ["end"] = { row = 0, col = 1 } }))
        assert(not pcall(ui.set_editor_cursor, -1, 0))

        -- swallow_printable context option & activation
        km.context("editor.vim_normal", "editor", { swallow_printable = true, active = false })
        km.context("editor.vim_normal", { active = true })
        assert(km.context("editor.vim_normal", {}) == true)
        km.context("editor.vim_normal", { active = false })
        assert(km.context("editor.vim_normal", {}) == false)
        km.context("editor.vim_normal", nil)

        -- completion registry validation and release
        assert(type(ui.completion) == "table" and type(ui.completion.register) == "function")
        assert(not pcall(ui.completion.register, {}))
        assert(not pcall(ui.completion.register, { name = "x", complete = 123 }))
        local rel = ui.completion.register({
            name = "custom",
            triggers = { "#" },
            min_prefix = 1,
            score_offset = 25,
            timeout_ms = 200,
            complete = function() return { { label = "#one", insert = "#one " } } end,
        })
        assert(type(rel) == "function")
        rel()
        rel() -- idempotent
    "##,
    )
    .call::<()>(km_core)
    .unwrap();
    drop(shared);
    assert!(rx.try_recv().is_ok());
}

#[tokio::test]
async fn presentations_lua_skips_markdown_scan_without_presenters_and_caches_with_presenters() {
    let lua = Lua::new();
    let shared = UiShared::new();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    *shared.tx.write().unwrap() = Some(tx);
    let mare = lua.create_table().unwrap();
    mare.set("o", lua.create_table().unwrap()).unwrap();
    crate::codec::install(&lua, &mare).unwrap();
    crate::streaming::install(&lua, &mare).unwrap();
    crate::task::install(&lua, &mare).unwrap();
    let prelude_src = include_str!("../../../mare-plugin/preset/ui_prelude.lua");
    let ui_table: mlua::Table = lua.load(prelude_src).set_name("@preset/ui_prelude.lua").eval().unwrap();
    super::install(&lua, &mare, ui_table, shared).unwrap();
    lua.globals().set("mare", mare).unwrap();

    let pres_src = include_str!("../../../mare-plugin/preset/plugins/lua/ui/presentations.lua");
    let pres_loader: mlua::Function = lua.load(pres_src).set_name("@preset/ui/presentations.lua").eval().unwrap();
    lua.globals().set("LOAD_PRESENTATIONS", pres_loader).unwrap();

    tokio::task::LocalSet::new()
        .run_until(
            lua.load(
                r##"
                local loaded = package.loaded
                loaded.mare_lib = {
                    caller_id = function(c) return c and c.id or nil end,
                    caller_name = function(c) return c and c.name or nil end,
                    fail = function(code, msg) error({ code = code, message = msg }, 0) end,
                }

                local U = mare.ui
                local md_calls = 0
                local orig_md_blocks = U.markdown_blocks
                U.markdown_blocks = function(src)
                    md_calls = md_calls + 1
                    return orig_md_blocks(src)
                end

                local ctx = {
                    id = "owner-1",
                    name = "ui-test",
                    on = function() end,
                    effect = function() end,
                    get = function() return nil end,
                    spawn = function(_, fn) mare.task.spawn(fn) end,
                    bus = { emit = function() end },
                    logger = { warn = function() end },
                }
                local D = { ctx = ctx, U = U }
                local service = {}
                LOAD_PRESENTATIONS(D, service)

                -- 1. Without presenters: 0 calls to markdown_blocks across streaming updates.
                for i = 1, 10 do
                    local snap = D.native_markdown_document(ctx, "doc-plain", "```lua\nlocal x = " .. i .. "\n```", {})
                    assert(not snap.enhanced and #snap.node.blocks == 0 and #snap.expanded_node.blocks == 0)
                end
                assert(md_calls == 0, "expected 0 markdown_blocks calls without presenters, got " .. md_calls)

                -- 2. With a presenter registered: <= 1 markdown_blocks call per (source, revision).
                local presenter_caller = {
                    id = "pres-1",
                    name = "pres",
                    effect = function() end,
                    spawn = function(_, fn) mare.task.spawn(fn) end,
                }
                local release_tx, release_rx = mare.task.channel()
                local entered_tx, entered_rx = mare.task.channel()
                local done_tx, done_rx = mare.task.channel()
                local gate = false
                local dispose = service.markdown_presenter(service, presenter_caller, "diagram", {
                    preview = true,
                    render = function(req)
                        if gate then
                            entered_tx:send(true)
                            release_rx:recv()
                            done_tx:send(true)
                        end
                        return U.Code("rendered:" .. req.phase .. ":" .. req.source, "text")
                    end,
                })

                md_calls = 0
                local open_src = "```diagram\nnode A\n"
                local open_snap = service.render_markdown(service, ctx, "doc-pres", open_src, {})
                -- Yield so spawned render completes.
                mare.task.sleep(10)
                local settled_open = service.get_markdown(service, ctx, "doc-pres")
                assert(settled_open.enhanced and settled_open.node.blocks[1].node.code == "rendered:open:node A\n")
                assert(settled_open.expanded_node.blocks[1].node.type == "lines")
                assert(md_calls == 1, "expected 1 markdown_blocks call for open revision, got " .. md_calls)

                -- Identical fence content at two offsets, closed fence transition.
                md_calls = 0
                local two_src = "```diagram\nsame\n```\n\n```diagram\nsame\n```"
                service.render_markdown(service, ctx, "doc-pres", two_src, {})
                mare.task.sleep(10)
                local settled_two = service.get_markdown(service, ctx, "doc-pres")
                assert(#settled_two.node.blocks == 2)
                assert(settled_two.node.blocks[1].start ~= settled_two.node.blocks[2].start)
                assert(md_calls == 1, "expected 1 markdown_blocks call for closed revision, got " .. md_calls)

                -- Stale async completion must not overwrite newer revision.
                gate = true
                service.render_markdown(service, ctx, "doc-pres", "```diagram\nstale\n```", {})
                assert(entered_rx:recv(), "old render must actually enter before installing fresh revision")
                gate = false
                service.render_markdown(service, ctx, "doc-pres", "```diagram\nfresh\n```", {})
                mare.task.sleep(10)
                release_tx:send(true)
                assert(done_rx:recv(), "old render must finish late completion path")
                mare.task.sleep(10)
                local final_snap = service.get_markdown(service, ctx, "doc-pres")
                assert(final_snap.node.blocks[1].node.code == "rendered:closed:fresh\n")

                -- Removing presenter restores 0 markdown_blocks calls.
                dispose()
                md_calls = 0
                D.native_markdown_document(ctx, "doc-after", "```diagram\nx\n```", {})
                assert(md_calls == 0, "expected 0 markdown_blocks calls after presenter removed, got " .. md_calls)
                "##,
            )
            .exec_async(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn transcript_lua_updates_projected_view_after_last_presenter_removed() {
    let lua = Lua::new();
    let shared = UiShared::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    *shared.tx.write().unwrap() = Some(tx);
    let mare = lua.create_table().unwrap();
    mare.set("o", lua.create_table().unwrap()).unwrap();
    crate::codec::install(&lua, &mare).unwrap();
    crate::streaming::install(&lua, &mare).unwrap();
    crate::task::install(&lua, &mare).unwrap();
    crate::text::install(&lua, &mare, std::env::temp_dir()).unwrap();
    let prelude_src = include_str!("../../../mare-plugin/preset/ui_prelude.lua");
    let ui_table: mlua::Table = lua.load(prelude_src).set_name("@preset/ui_prelude.lua").eval().unwrap();
    super::install(&lua, &mare, ui_table, shared.clone()).unwrap();
    lua.globals().set("mare", mare).unwrap();

    let pres_src = include_str!("../../../mare-plugin/preset/plugins/lua/ui/presentations.lua");
    let pres_loader: mlua::Function = lua.load(pres_src).set_name("@preset/ui/presentations.lua").eval().unwrap();
    lua.globals().set("LOAD_PRESENTATIONS", pres_loader).unwrap();

    let tr_src = include_str!("../../../mare-plugin/preset/plugins/lua/ui/transcript.lua");
    let tr_loader: mlua::Function = lua.load(tr_src).set_name("@preset/ui/transcript.lua").eval().unwrap();
    lua.globals().set("LOAD_TRANSCRIPT", tr_loader).unwrap();

    tokio::task::LocalSet::new()
        .run_until(
            lua.load(
                r##"
                local loaded = package.loaded
                loaded.mare_lib = {
                    caller_id = function(c) return c and c.id or nil end,
                    caller_name = function(c) return c and c.name or nil end,
                    fail = function(code, msg) error({ code = code, message = msg }, 0) end,
                }
                loaded["ui.working"] = {
                    verb = function(_, fallback) return fallback end,
                    text_throttle_ms = function() return 250 end,
                }
                loaded["ui.handler_errors"] = function()
                    return { turn_start = function() end, turn_end = function() end, on_error = function() end }
                end

                local U = mare.ui
                U.AssistantMessage = U.AssistantMessage or { marker = function() return "● " end }
                U.has_markdown_transforms = function() return false end
                U.transform_markdown = function(s) return s end
                U.working = function() end

                local listeners = {}
                local bus = {
                    emit = function(_, name, ev)
                        for _, fn in ipairs(listeners[name] or {}) do
                            fn(ev)
                        end
                    end,
                }
                local ctx = {
                    id = "ui-owner",
                    name = "ui",
                    on = function(_, name, fn)
                        listeners[name] = listeners[name] or {}
                        table.insert(listeners[name], fn)
                    end,
                    effect = function() end,
                    get = function() return nil end,
                    spawn = function(_, fn) mare.task.spawn(fn) end,
                    bus = bus,
                    logger = { warn = function() end },
                }
                local service = {}
                local D = {
                    ctx = ctx,
                    cfg = { preview_lines = 2, working_interval_ms = 250, tool_update_interval_ms = 100 },
                    U = U,
                    classify = { classify = function() return "Unknown" end },
                    tool_nodes = {
                        group_node = function() return U.Line("") end,
                        bash_node = function() return U.Line("") end,
                        tool_node = function() return U.Line("") end,
                    },
                    service = { service = service },
                }
                LOAD_PRESENTATIONS(D, service)
                LOAD_TRANSCRIPT(D)

                local dispose = service.markdown_presenter(service, ctx, "diagram", {
                    preview = true,
                    render = function(req)
                        return U.Code("rendered:" .. req.source, "text")
                    end,
                })

                bus:emit("turn/started", {})
                bus:emit("item/delta", { kind = "text", delta = "```diagram\nfirst\n" })
                mare.task.sleep(10)

                -- Remove the last presenter mid-stream; subsequent text delta must update the projected node.
                dispose()
                bus:emit("item/delta", { kind = "text", delta = " second" })
                "##,
            )
            .exec_async(),
        )
        .await
        .unwrap();

    let mut saw_updated_projected = false;
    while let Ok(cmd) = rx.try_recv() {
        if let UiCmd::BlockUpdate { patch, .. } = cmd
            && let Some(mare_shared::ui::Node::Indent { child, .. }) = patch.node
            && let mare_shared::ui::Node::PresentedMarkdown { text, blocks } = *child
            && text == "```diagram\nfirst\n second"
            && blocks.is_empty()
        {
            saw_updated_projected = true;
        }
    }
    assert!(saw_updated_projected, "projected block must continue updating after last presenter is removed");
}

#[test]
fn ui_public_surface_exposes_no_private_fields_and_supports_persist_viewport_ambiwidth() {
    let (lua, shared, _rx) = setup_ui();
    lua.load(
        r##"
        local ui = mare.ui
        local priv = {}
        for k in next, ui do
            if type(k) == "string" and k:sub(1, 1) == "_" then
                priv[#priv + 1] = k
            end
        end
        table.sort(priv)
        assert(#priv == 0, "mare.ui must not expose underscore fields: " .. table.concat(priv, ", "))

        assert(type(ui.set_viewport) == "function", "ui.set_viewport must be public")
        assert(ui.set_viewport(120, 48) == true)
        assert(ui.set_viewport(120, 48) == false)
        assert(ui.width() == 120 and ui.height() == 48)
        assert(not pcall(ui.set_viewport, 0, 40))

        assert(type(ui.set_ambiwidth) == "function", "ui.set_ambiwidth must be public")
        ui.set_ambiwidth("double")
        ui.set_ambiwidth("single")
        assert(not pcall(ui.set_ambiwidth, "triple"))

        ui.set_layout({ "status", "live", "editor" })
        assert(mare.o.ui.layout.order[1] == "status")
        -- persist = false applies layout without overwriting mare.o.ui.layout
        ui.set_layout({ "live", "editor", "status" }, { persist = false })
        assert(mare.o.ui.layout.order[1] == "status", "persist = false must not overwrite mare.o.ui.layout")
        assert(not pcall(ui.set_layout, nil, { persist = "yes" }))
    "##,
    )
    .exec()
    .unwrap();
    assert_eq!(shared.layout.read().unwrap().order[0].as_str(), "live");

    // render_line_text & non_neg_int float-integer coercion.
    let rendered = super::render_line_text(&shared, mlua::Value::String(lua.create_string("hello").unwrap())).unwrap();
    assert_eq!(rendered, "hello");
    assert_eq!(super::non_neg_int(mlua::Value::Number(3.0), "test").unwrap(), 3);
}
