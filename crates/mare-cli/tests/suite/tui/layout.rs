//! layout tests: region table, `ui.layout` config, slot/align validation,
//! height-insufficient clipping order, and modal/overlay z-order & internal scrolling.

use super::*;

#[tokio::test]
async fn layout_config_reorders_hides_and_caps_regions() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        exec(
            &t.vt.host,
            r#"
            mare.o.ui = {
                layout = {
                    order = {
                        "status",
                        "live",
                        "above-editor",
                        "below-editor",
                        "editor",
                    },
                    regions = {
                        ["above-editor"] = { hidden = true },
                        ["below-editor"] = { max_height = 2 },
                    },
                },
            }
            mare.ui.set_widget("w_above", "above-editor", mare.ui.Line("HIDDEN_ABOVE_WIDGET"))
            mare.ui.set_widget("w_below", "below-editor", mare.ui.Lines({
                mare.ui.Line("BELOW_ROW_1"),
                mare.ui.Line("BELOW_ROW_2"),
                mare.ui.Line("BELOW_ROW_3"),
                mare.ui.Line("BELOW_ROW_4"),
            }))
            mare.ui.set_status("st_mark", "left", mare.ui.Line("TOP_STATUS_MARK"), 0, true)
            "#,
        )
        .await;
        t.until("reordered and capped layout", |s| {
            s.contains("TOP_STATUS_MARK") && s.contains("BELOW_ROW_1") && s.contains("BELOW_ROW_2")
        })
        .await;
        let screen = t.screen();
        assert!(!screen.contains("HIDDEN_ABOVE_WIDGET"), "hidden slot must not render:\n{screen}");
        assert!(!screen.contains("BELOW_ROW_3"), "max_height=2 must cap below-editor:\n{screen}");
        let status_row = screen.lines().position(|l| l.contains("TOP_STATUS_MARK")).unwrap();
        let below_row = screen.lines().position(|l| l.contains("BELOW_ROW_1")).unwrap();
        let editor_row = screen.lines().position(|l| l.contains("Ask mare")).unwrap();
        assert!(status_row < below_row, "status should be above below-editor:\n{screen}");
        assert!(below_row < editor_row, "below-editor should be above editor:\n{screen}");
    })
    .await;
}

#[tokio::test]
async fn layout_invalid_config_errors_and_keeps_previous_layout() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        // Establish a valid custom layout via `set_layout` (persists to `mare.o.ui`).
        exec(
            &t.vt.host,
            r#"
            mare.ui.set_layout({ "status", "live", "editor" })
            mare.ui.set_status("st_keep", "left", mare.ui.Line("CUSTOM_TOP_STATUS"), 0, true)
            "#,
        )
        .await;
        t.until("custom top status", |s| s.contains("CUSTOM_TOP_STATUS")).await;

        let errs: Vec<String> = eval(
            &t.vt.host,
            r#"
            local out = {}
            local cases = {
                { "live", "status" },                       -- missing editor
                { "live", "editor" },                       -- missing status
                { "live", "editor", "editor", "status" },   -- duplicate editor
                { "live", "editer", "status" },             -- unknown region (typo)
                { "live", "modal", "editor", "status" },    -- overlay not allowed in config
            }
            for _, c in ipairs(cases) do
                local ok, err = pcall(mare.ui.set_layout, c)
                out[#out + 1] = ok and "OK" or tostring(err)
            end
            -- Pre-commit rejection via `mare.o.ui` & `mare.o.ui.layout`
            local ok_false, e_false = pcall(function() mare.o.ui = { layout = false } end)
            out[#out + 1] = ok_false and "OK" or tostring(e_false)
            local ok_miss, e_miss = pcall(function() mare.o.ui.layout = { "live", "status" } end)
            out[#out + 1] = ok_miss and "OK" or tostring(e_miss)
            local ok_cmp, e_cmp = pcall(function()
                mare.o.ui = { layout = { regions = { completion = { max_height = 2 } } } }
            end)
            out[#out + 1] = ok_cmp and "OK" or tostring(e_cmp)
            -- Nested mutations on already-set tables/arrays must also go through pre-commit validation
            mare.o.ui = {
                layout = {
                    order = { "status", "live", "editor" },
                    regions = { editor = { max_height = 5 } },
                },
            }
            local ok_nested_reg, e_nested_reg = pcall(function()
                mare.o.ui.layout.regions.editor.max_height = 0
            end)
            out[#out + 1] = ok_nested_reg and "OK" or tostring(e_nested_reg)
            local ok_nested_ord, e_nested_ord = pcall(function()
                mare.o.ui.layout.order[2] = "broken"
            end)
            out[#out + 1] = ok_nested_ord and "OK" or tostring(e_nested_ord)
            -- Changing another shell option must not overwrite `set_layout`'s persisted `mare.o.ui`
            mare.o.border = "single"
            return out
            "#,
        )
        .await;
        for (i, e) in errs.iter().enumerate() {
            assert_ne!(e, "OK", "case {i} should have failed validation");
        }
        assert!(errs[3].contains("editor"), "typo 'editer' should suggest 'editor': {}", errs[3]);

        // Verify `mare.o.ui.layout.order` and `regions.editor.max_height` still hold the previous valid values.
        let (order, ed_max): (Vec<String>, i64) = eval(
            &t.vt.host,
            "local out = {}; for i = 1, #mare.o.ui.layout.order do out[i] = mare.o.ui.layout.order[i] end; return out, mare.o.ui.layout.regions.editor.max_height",
        )
        .await;
        assert_eq!(order, vec!["status", "live", "editor"]);
        assert_eq!(ed_max, 5);

        // Previous valid layout (status at top) must still be intact on screen (even after `border = "single"`).
        t.step().await;
        let screen = t.screen();
        let status_row = screen.lines().position(|l| l.contains("CUSTOM_TOP_STATUS")).unwrap();
        let editor_row = screen.lines().position(|l| l.contains("Ask mare")).unwrap();
        assert!(status_row < editor_row, "previous layout must be preserved:\n{screen}");
    })
    .await;
}

#[tokio::test]
async fn slot_and_align_reject_unknown_names_with_closest_match() {
    local(async {
        let t = Term::new(json!([]), 80, 20).await;
        let errs: Vec<String> = eval(
            &t.vt.host,
            r#"
            local out = {}
            local ok1, e1 = pcall(mare.ui.set_widget, "w1", "above_editor", mare.ui.Line("x"))
            out[1] = ok1 and "OK" or tostring(e1)
            local ok2, e2 = pcall(mare.ui.set_status, "s1", "rihgt", mare.ui.Line("x"))
            out[2] = ok2 and "OK" or tostring(e2)
            local ok3, e3 = pcall(function()
                local ui = mare.services and mare.services.ui
                if ui and ui.status then
                    ui:status({ id = "s2" }, "s2", { align = "center", render = function() return mare.ui.Line("x") end })
                else
                    mare.ui.set_status("s2", "center", mare.ui.Line("x"))
                end
            end)
            out[3] = ok3 and "OK" or tostring(e3)
            return out
            "#,
        )
        .await;
        assert!(errs[0].contains("above-editor"), "should suggest above-editor: {}", errs[0]);
        assert!(errs[1].contains("right"), "should suggest right: {}", errs[1]);
        assert_ne!(errs[2], "OK", "unknown align 'center' should error: {}", errs[2]);
    })
    .await;
}

#[tokio::test]
async fn height_clipping_order_and_modal_visibility() {
    local(async {
        // Small terminal: 80x9 -> max_h = 8.
        let mut t = Term::new(json!([{ "text": "line1\n\nline2\n\nline3\n\nline4", "delay_ms": 20 }]), 80, 9).await;
        exec(
            &t.vt.host,
            r#"
            mare.ui.set_widget("w1", "above-editor", mare.ui.Lines({
                mare.ui.Line("SLOT_LINE_1"),
                mare.ui.Line("SLOT_LINE_2"),
                mare.ui.Line("SLOT_LINE_3"),
                mare.ui.Line("SLOT_LINE_4"),
            }))
            mare.ui.working("Thinking", "work", {})
            mare.ui.set_status("left", "left", mare.ui.Line("KEEP_SEG"), 0, true)
            mare.ui.set_status("extra", "left", mare.ui.Line("DROP_SEG"), 1, false)
            "#,
        )
        .await;
        t.until("slot and working rendered", |s| s.contains("SLOT_LINE_4") && s.contains("KEEP_SEG")).await;
        let screen = t.screen();
        // Editor (3 lines) + status (1 line) + working (1 line) = 5 lines, leaving 3 lines for slot.
        // SLOT_LINE_1 must be clipped from the slot while working and status remain!
        assert!(!screen.contains("SLOT_LINE_1"), "SLOT_LINE_1 must be clipped when height is tight:\n{screen}");
        assert!(screen.contains("Thinking"), "working should survive when slot shrinks:\n{screen}");
        assert!(screen.contains("KEEP_SEG"), "status should survive:\n{screen}");

        // Now open key menu modal (`?`) on this short terminal: modal must scroll internally,
        // keep both top (`─ Keys ─`) and bottom (` of `) borders visible, and advance selection on Down.
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("key menu modal", |s| s.contains("─ Keys ─") && s.contains(" of ")).await;
        let before = t.screen();
        assert!(before.contains("1 of "), "expected footer '1 of ...':\n{before}");
        t.key(KeyCode::Down, Modifiers::NONE);
        t.until("key menu scrolled/moved", |s| s.contains("2 of ")).await;
        let after = t.screen();
        assert!(
            after.contains("─ Keys ─") && after.contains("╰") && after.contains("2 of "),
            "modal borders and scrolled footer must remain visible:\n{after}"
        );
    })
    .await;
}

#[tokio::test]
async fn omitted_live_region_still_renders_streaming_output() {
    local(async {
        let mut t = Term::new(json!([{ "text": "STREAMED_LIVE_OUTPUT" }]), 80, 16).await;
        exec(
            &t.vt.host,
            r#"
            mare.o.ui = { layout = { order = { "status", "editor" } } }
            "#,
        )
        .await;
        t.submit("hello").await;
        t.until("streamed output visible despite live omitted from order", |s| s.contains("STREAMED_LIVE_OUTPUT"))
            .await;
    })
    .await;
}

#[tokio::test]
async fn editor_max_height_bounds_entire_region_including_borders() {
    local(async {
        let mut t = Term::new(json!([]), 80, 16).await;
        exec(
            &t.vt.host,
            r#"
            mare.o.ui = { layout = { regions = { editor = { max_height = 4 } } } }
            mare.ui.set_editor("L1\nL2\nL3\nL4\nL5")
            "#,
        )
        .await;
        t.until("editor rendered with L5", |s| s.contains("L5")).await;
        let screen = t.screen();
        let top = screen.lines().position(|l| l.starts_with('╭')).unwrap();
        let bot = screen.lines().position(|l| l.starts_with('╰')).unwrap();
        assert!(bot - top < 4, "editor region including borders must be <= 4 rows:\n{screen}");

        // Graceful degradation when max_height < 3 (e.g. 1 row).
        exec(
            &t.vt.host,
            r#"
            mare.o.ui = { layout = { regions = { editor = { max_height = 1 } } } }
            "#,
        )
        .await;
        t.until("editor degraded to 1 row without borders", |s| !s.contains('╭') && s.contains("L5")).await;
    })
    .await;
}

#[tokio::test]
async fn oversized_modals_stay_within_terminal_height() {
    local(async {
        // Short narrow terminal: 76x9 -> max_h = 8, wide = false (< 84).
        let mut t = Term::new(json!([]), 76, 9).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
                mare.ui.select({
                    title = "Pick",
                    items = {
                        { label = "Alpha", preview = mare.ui.Lines({
                            mare.ui.Line("PV_1"), mare.ui.Line("PV_2"), mare.ui.Line("PV_3"),
                            mare.ui.Line("PV_4"), mare.ui.Line("PV_5"), mare.ui.Line("PV_6"),
                        }) },
                        { label = "Beta" },
                        { label = "Gamma" },
                    },
                })
                mare.ui.confirm(("Very long confirmation message "):rep(12))
                mare.ui.input({ title = "Notes", default = "R1\nR2\nR3\nR4\nR5\nR6\nR7\nR8" })
            end)
            "#,
        )
        .await;
        t.until("narrow select with preview fits in 8 rows", |s| s.contains("─ Pick ─") && s.contains("1 of 3")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);

        // Long confirm title on 76x9 must keep [Y]es [N]o buttons visible.
        t.until("confirm buttons visible", |s| s.contains("─ Confirm ─") && s.contains("[Y]es") && s.contains("[N]o"))
            .await;
        t.key(KeyCode::Escape, Modifiers::NONE);

        // Multiline input modal on 76x9 must keep borders & cursor line visible.
        t.until("multiline input modal fits with borders", |s| {
            s.contains("─ Notes ─") && s.contains('╭') && s.contains('╰')
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn oversized_modals_scroll_internally_to_tail() {
    local(async {
        // Short narrow terminal: 76x9 -> max_h = 8, wide = false (< 84).
        let mut t = Term::new(json!([]), 76, 9).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
                mare.ui.select({
                    title = "NotesPick",
                    body = {
                        "BODY_LINE_01", "BODY_LINE_02", "BODY_LINE_03", "BODY_LINE_04",
                        "BODY_LINE_05", "BODY_LINE_06", "BODY_LINE_07", "BODY_TAIL_MARKER",
                    },
                    items = { { label = "Item1" }, { label = "Item2" } },
                })
                mare.ui.select({
                    title = "PreviewPick",
                    items = {
                        { label = "Alpha", preview = mare.ui.Lines({
                            mare.ui.Line("PV_1"), mare.ui.Line("PV_2"), mare.ui.Line("PV_3"),
                            mare.ui.Line("PV_4"), mare.ui.Line("PV_5"), mare.ui.Line("PV_TAIL_MARKER"),
                        }) },
                        { label = "Beta" },
                    },
                })
                mare.ui.confirm(("Very long confirmation message "):rep(12) .. "CONFIRM_TAIL_MARKER")
            end)
            "#,
        )
        .await;

        t.until("select with body opened", |s| s.contains("─ NotesPick ─") && s.contains("BODY_LINE_01")).await;
        let body_init = t.screen();
        assert!(!body_init.contains("BODY_TAIL_MARKER"), "body tail should initially be off-screen:\n{body_init}");
        t.key(KeyCode::PageDown, Modifiers::CONTROL);
        t.until("select body scrolled to tail", |s| s.contains("BODY_TAIL_MARKER")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);

        t.until("select with preview opened", |s| s.contains("─ PreviewPick ─") && s.contains("PV_1")).await;
        let pv_init = t.screen();
        assert!(!pv_init.contains("PV_TAIL_MARKER"), "preview tail should initially be off-screen:\n{pv_init}");
        t.key(KeyCode::PageDown, Modifiers::SHIFT);
        t.until("select preview scrolled to tail", |s| s.contains("PV_TAIL_MARKER")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);

        // Long confirm message: initially clipped, scrolls to CONFIRM_TAIL_MARKER while keeping buttons visible.
        t.until("confirm opened", |s| s.contains("─ Confirm ─") && s.contains("[Y]es") && s.contains("[N]o")).await;
        let confirm_init = t.screen();
        assert!(
            !confirm_init.contains("CONFIRM_TAIL_MARKER"),
            "confirm tail should initially be off-screen:\n{confirm_init}"
        );
        t.key(KeyCode::End, Modifiers::NONE);
        t.until("confirm scrolled to tail", |s| {
            s.contains("CONFIRM_TAIL_MARKER") && s.contains("[Y]es") && s.contains("[N]o")
        })
        .await;
    })
    .await;
}
