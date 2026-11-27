//! Animation scheduling (widgets & containers), 3 generic fallback structures,
//! component failure notices, and component render fixtures.
//!
//! Sibling test references:
//! - Codex `codex-rs/tui/src/status_indicator_widget/effects_tests.rs:9-57` (shimmer/progress +
//!   master animations switch snapshot) and `codex-rs/tui/src/screen_reader_tests.rs:130`
//!   (no-animation frame request assertion)
//! - Kimi Code `packages/pi-tui/test/tui-render.test.ts:178` (controllable clock & timer
//!   start/stop), `apps/reference implementation/test/tui/components/editor/wrapping-select-list.test.ts:79`
//!   (selected item style), and `apps/vis/web/test/renderers.test.ts:1-60` (fixture-driven
//!   component renderers)
//! - reference implementation (slot fallback ->
//!   live registration -> unload -> fallback)
//! - mini.nvim `tests/test_test.lua:965-1000` (reference screenshot & double-width char testing)

use super::*;

#[tokio::test]
async fn which_key_packs_columns_and_aligns_unicode_keys_in_cells() {
    local(async {
        let t = Term::new(json!([]), 100, 24).await;
        let render = |width: usize| {
            format!(
                r#"
            return mare.ui.render_text(mare.ui.WhichKey:render({{
              prefix = 'Ctrl+X', width = {width}, bindings = {{
                {{ key = 'a', action = 'first' }}, {{ key = '界', action = 'second' }},
                {{ key = '→', action = 'third' }}, {{ key = 'b', action = 'fourth' }},
              }}
            }}), {width})"#
            )
        };
        let wide: String = eval(&t.vt.host, &render(80)).await;
        assert!(wide.contains("+Ctrl+X"), "the group prefix is visible:\n{wide}");
        assert!(
            wide.lines().any(|line| {
                ["first", "second", "third", "fourth"].iter().filter(|word| line.contains(**word)).count() >= 2
            }),
            "wide terminals should use multiple columns:\n{wide}"
        );
        let narrow: String = eval(&t.vt.host, &render(24)).await;
        let columns: Vec<usize> = ["first", "second", "third", "fourth"]
            .iter()
            .map(|word| {
                let line = narrow.lines().find(|line| line.contains(word)).unwrap();
                mare_shared::style::str_width(&line[..line.find(word).unwrap()])
            })
            .collect();
        assert!(columns.iter().all(|column| *column == columns[0]), "{columns:?}:\n{narrow}");
    })
    .await
}

#[tokio::test]
async fn notices_replace_by_id_and_keep_identical_text_with_distinct_ids() {
    local(async {
        let mut t = Term::new(json!([]), 120, 30).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            mare.ui.notify('old progress', 'info', { id = 'build' })
            mare.ui.notify('new progress', 'info', { id = 'build' })
            mare.ui.notify('same text', 'info', { id = 'one' })
            mare.ui.notify('same text', 'info', { id = 'two' })
        "#,
        )
        .await;
        t.until("the latest notices", |s| s.contains("new progress") && s.contains("same text")).await;
        let screen = t.screen();
        assert!(!screen.contains("old progress"), "the same id must replace in place:\n{screen}");
        assert_eq!(screen.matches("same text").count(), 2, "different ids must remain separate:\n{screen}");
    })
    .await
}

#[tokio::test]
async fn clipped_notice_explains_how_many_lines_are_hidden() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"mare.ui.notify('first line\nsecond line\nthird line\nfourth line\nfifth line\nsixth line', 'warning')"#,
        )
        .await;
        t.until("notice shown", |s| s.contains("first line")).await;
        assert!(t.screen().contains("↓ 2 more lines"), "the card must explain clipped content: {}", t.screen());
    })
    .await
}

#[tokio::test]
async fn notifications_command_shows_recent_notice_text_in_full() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"NOTICE_EVENT_SEEN = false
               mare.on('ui/notification', function() NOTICE_EVENT_SEEN = true end)
               mare.ui.notify('first line\nsecond line\nthird line\nfourth line\nfifth line\nsixth line', 'warning')"#,
        )
        .await;
        t.until("notice shown", |s| s.contains("first line")).await;
        t.vt.dismiss_notices();
        t.submit("/notifications").await;
        let deadline = Instant::now() + Duration::from_secs(3);
        while !t.screen().contains("sixth line") && Instant::now() < deadline {
            t.step().await;
            tokio::task::yield_now().await;
        }
        let screen = t.screen();
        assert!(
            screen.contains("Notifications") && screen.contains("sixth line"),
            "full notification history: {screen}"
        );
        assert!(
            !eval::<bool>(&t.vt.host, "return NOTICE_EVENT_SEEN").await,
            "notice history must stay out of the event stream"
        );
    })
    .await
}

#[tokio::test]
async fn select_frame_recomputes_after_a_terminal_resize() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        exec(
            &t.vt.host,
            r#"
            FRAME_WIDTHS = {}
            local original = mare.ui.Select.frame
            function mare.ui.Select:frame(state)
                FRAME_WIDTHS[#FRAME_WIDTHS + 1] = state.width or -1
                return original(self, state)
            end
            mare.task.spawn(function() mare.ui.select({ title = 'Resizable', items = { 'one', 'two' } }) end)
        "#,
        )
        .await;
        t.until("select open", |s| s.contains("Resizable")).await;
        let before: usize = eval(&t.vt.host, "return #FRAME_WIDTHS").await;
        t.resize(60, 24).await;
        let (after, first, last): (usize, i64, i64) =
            eval(&t.vt.host, "return #FRAME_WIDTHS, FRAME_WIDTHS[1], FRAME_WIDTHS[#FRAME_WIDTHS]").await;
        assert!(
            after > before && first != last,
            "frame widths must follow resizing: {before} -> {after}, {first} -> {last}"
        );
    })
    .await
}

#[tokio::test]
async fn widget_and_container_animation_scheduling_and_reduce_motion() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();
        assert!(!t.vt.animating(), "idle app should not be animating");

        // Place an animated node (Frames inside Block -> Split -> Indent -> Clip) in a widget.
        exec(
            &t.vt.host,
            r#"
            local ui = mare.ui
            local anim = ui.Frames({
                ui.Line({ ui.Span("ANIM-A") }),
                ui.Line({ ui.Span("ANIM-B") }),
            }, 80, ui.Line({ ui.Span("STILL-X") }))
            local nested = ui.Block(
                ui.Split(ui.Indent("> ", ui.Clip(anim, 1)), ui.Line({ ui.Span("right") })),
                { title = "Widget" }
            )
            ui.set_widget("anim_widget", "above-editor", nested)
            "#,
        )
        .await;
        t.step().await;

        assert!(t.vt.animating(), "widget with animation inside Block/Split/Indent/Clip must drive redraws");
        assert!(
            t.screen().contains("ANIM-A") || t.screen().contains("ANIM-B"),
            "animated widget should render active frame; screen:\n{}",
            t.screen()
        );

        // Enable reduce_motion: must render the still frame and stop scheduling widget ticks.
        exec(&t.vt.host, "mare.o.reduce_motion = true").await;
        t.step().await;
        assert!(!t.vt.animating(), "reduce_motion without active working turn should not schedule animation ticks");
        assert!(t.screen().contains("STILL-X"), "reduce_motion must render the still frame; screen:\n{}", t.screen());
    })
    .await;
}

#[tokio::test]
async fn confirm_fallback_notifies_once_uses_choice_marker_and_remains_operable() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            function mare.ui.Confirm:render(_c)
                error("broken confirm")
            end
            "#,
        )
        .await;

        let h = t.vt.host.clone();
        let answer =
            tokio::task::spawn_local(async move { eval::<bool>(&h, "return mare.ui.confirm('Delete item?')").await });

        t.until("confirm fallback dialog", |s| {
            s.contains("Confirm") && s.contains("Delete item?") && s.contains("[Y]es") && s.contains("[N]o")
        })
        .await;

        let screen = t.screen();
        assert!(
            screen.contains("▸ [Y]es") || screen.contains("❯ [Y]es"),
            "choice fallback must mark selected option with a prefix marker; screen:\n{screen}"
        );

        // Still operable: Tab/Right moves selection to No, Enter confirms false.
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("selection moves to No", |s| s.contains("▸ [N]o") || s.contains("❯ [N]o")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.step().await;
        assert!(!answer.await.unwrap(), "Enter on No must return false");

        // Notice is visible once the modal closes (not deferred behind pop_top).
        t.until("component failure notice", |s| s.contains("mare.ui.Confirm:render")).await;

        // Second failure of the same component does not emit a duplicate notice.
        t.vt.dismiss_notices();
        let h2 = t.vt.host.clone();
        let answer2 =
            tokio::task::spawn_local(async move { eval::<bool>(&h2, "return mare.ui.confirm('Second?')").await });
        t.until("second confirm fallback", |s| s.contains("Second?")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(answer2.await.unwrap(), "y must still confirm yes");
        assert!(
            !t.screen().contains("mare.ui.Confirm:render"),
            "same component failure must only notify once; screen:\n{}",
            t.screen()
        );
    })
    .await;
}

#[tokio::test]
async fn select_fallback_notifies_once_uses_frame_and_remains_operable() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            function mare.ui.Select:frame(_s)
                error("broken select frame")
            end
            "#,
        )
        .await;

        let h = t.vt.host.clone();
        let picked = tokio::task::spawn_local(async move {
            eval::<Option<i64>>(&h, "return mare.ui.select({ title = 'Pick one', items = { 'alpha', 'beta' } })").await
        });

        t.until("select fallback frame", |s| s.contains("Pick one") && s.contains("alpha") && s.contains("beta")).await;

        // Still operable: Down + Enter picks beta (index 2).
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.step().await;
        assert_eq!(picked.await.unwrap(), Some(2));
        t.until("Select:frame failure notice", |s| s.contains("mare.ui.Select:frame")).await;
    })
    .await;
}

#[tokio::test]
async fn keyhints_fallback_notifies_once_and_renders_lines() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            function mare.ui.KeyHints:render(_hints)
                error("broken keyhints")
            end
            "#,
        )
        .await;

        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("key menu with fallback keyhints", |s| s.contains("Keys") && s.contains("Esc back")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("key menu closes and failure notice shows", |s| {
            !s.contains("Esc back") && s.contains("mare.ui.KeyHints:render")
        })
        .await;
    })
    .await;
}

#[tokio::test]
async fn stable_frames_do_not_reinvoke_lua_components() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            _G.__kh_calls = 0
            local orig_kh = mare.ui.KeyHints.render
            function mare.ui.KeyHints:render(hints)
                _G.__kh_calls = _G.__kh_calls + 1
                return orig_kh(self, hints)
            end
            "#,
        )
        .await;

        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("key menu open", |s| s.contains("Keys") && s.contains("Esc back")).await;
        let first_calls: i64 = eval(&t.vt.host, "return _G.__kh_calls").await;
        assert_eq!(first_calls, 1, "KeyHints:render should be called once on open");

        // Render 5 more stable frames without state changes.
        for _ in 0..5 {
            t.step().await;
        }
        let after_calls: i64 = eval(&t.vt.host, "return _G.__kh_calls").await;
        assert_eq!(after_calls, 1, "stable frames must not re-invoke Lua components");

        t.key(KeyCode::Escape, Modifiers::NONE);
    })
    .await;
}

#[tokio::test]
async fn component_render_fixtures() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        t.vt.dismiss_notices();

        // 1. Confirm fallback fixture (Frame + Choice)
        exec(
            &t.vt.host,
            r#"
            function mare.ui.Confirm:render(_c)
                return nil
            end
            "#,
        )
        .await;
        let h = t.vt.host.clone();
        let confirm =
            tokio::task::spawn_local(
                async move { eval::<bool>(&h, "return mare.ui.confirm('Deploy release?')").await },
            );
        t.until("confirm fixture", |s| s.contains("Deploy release?")).await;
        insta::assert_snapshot!("confirm_fallback_fixture", t.snap().await);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        let _ = confirm.await;

        // 2. Select default + KeyHints fallback fixture (Lines)
        exec(
            &t.vt.host,
            r#"
            function mare.ui.KeyHints:render(_hints)
                return nil
            end
            "#,
        )
        .await;
        let h2 = t.vt.host.clone();
        let sel = tokio::task::spawn_local(async move {
            eval::<Option<i64>>(
                &h2,
                "return mare.ui.select({ title = 'Choose target', items = { 'staging', 'production' } })",
            )
            .await
        });
        t.until("select fixture", |s| s.contains("Choose target") && s.contains("staging")).await;
        insta::assert_snapshot!("select_with_keyhints_fallback_fixture", t.snap().await);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        let _ = sel.await;

        // 3. Animated widget under reduce_motion (still frame)
        exec(
            &t.vt.host,
            r#"
            local ui = mare.ui
            mare.o.reduce_motion = true
            ui.set_widget("fixture_w", "above-editor", ui.Block(
                ui.Frames({
                    ui.Line({ ui.Span("RUNNING-1") }),
                }, 80, ui.Line({ ui.Span("STATIC-READY") })),
                { title = "Status" }
            ))
            "#,
        )
        .await;
        t.until("static widget fixture", |s| s.contains("STATIC-READY")).await;
        insta::assert_snapshot!(
            if cfg!(windows) { "widget_reduce_motion_fixture_windows" } else { "widget_reduce_motion_fixture" },
            t.snap().await
        );
    })
    .await;
}

#[tokio::test]
async fn empty_frames_node_and_covered_widget_do_not_schedule_animation() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();

        // 1. Empty frames list always draws `still` and must NOT schedule animation ticks.
        exec(
            &t.vt.host,
            r#"
            local ui = mare.ui
            ui.set_widget("empty_frames", "above-editor", ui.Frames({}, 80, ui.Line({ ui.Span("ONLY-STILL") })))
            "#,
        )
        .await;
        t.until("only-still widget", |s| s.contains("ONLY-STILL")).await;
        assert!(!t.vt.animating(), "empty Frames node must not schedule animation redraws");

        // 2. Animated widget covered by a static transcript must stop animating.
        exec(
            &t.vt.host,
            r#"
            mare.ui.set_widget("empty_frames", "above-editor", mare.ui.Spinner("spinning"))
            "#,
        )
        .await;
        t.step().await;
        assert!(t.vt.animating(), "visible spinner widget should animate");

        t.key(KeyCode::Char('t'), Modifiers::CONTROL);
        t.until("transcript opens", |s| s.contains("Transcript")).await;
        assert!(!t.vt.animating(), "animated widget covered by static transcript must not animate");
    })
    .await;
}

#[tokio::test]
async fn keyhints_fallback_narrow_width_preserves_ellipsis() {
    local(async {
        let mut t = Term::new(json!([]), 24, 16).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            function mare.ui.KeyHints:render(_hints)
                return nil
            end
            "#,
        )
        .await;

        let h = t.vt.host.clone();
        let sel = tokio::task::spawn_local(async move {
            eval::<Option<i64>>(&h, "return mare.ui.select({ title = 'Pick', items = { 'a', 'b' } })").await
        });
        t.until("narrow select with fallback keyhints", |s| s.contains("Pick") && s.contains("Enter")).await;
        let screen = t.screen();
        let last_line = screen.lines().last().unwrap_or_default();
        assert!(
            last_line.contains('…'),
            "narrow KeyHints fallback must preserve truncation ellipsis '…'; last line: {last_line:?}\nscreen:\n{screen}"
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        let _ = sel.await;
    })
    .await;
}

#[tokio::test]
async fn component_failure_while_transcript_open_still_notifies() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();

        // Open transcript first.
        t.key(KeyCode::Char('t'), Modifiers::CONTROL);
        t.until("transcript opens", |s| s.contains("Transcript")).await;

        exec(
            &t.vt.host,
            r#"
            function mare.ui.Confirm:render(_c)
                error("transcript confirm boom")
            end
            "#,
        )
        .await;

        let h = t.vt.host.clone();
        let answer =
            tokio::task::spawn_local(async move { eval::<bool>(&h, "return mare.ui.confirm('On transcript?')").await });
        t.until("confirm on transcript", |s| s.contains("On transcript?")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(answer.await.unwrap());

        // While STILL in transcript, the failure notice must be shown!
        t.until("failure notice visible in transcript", |s| s.contains("mare.ui.Confirm:render")).await;
    })
    .await;
}

#[tokio::test]
async fn modal_info_and_confirm_node_animate_and_empty_confirm_falls_back() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        t.vt.dismiss_notices();

        // 1. Modal::Info with Spinner drives animation
        let h = t.vt.host.clone();
        let overlay = tokio::task::spawn_local(async move {
            exec(&h, r#"mare.ui.overlay("InfoTitle", mare.ui.Spinner("info-spin"))"#).await;
        });
        t.until("info overlay", |s| s.contains("InfoTitle") && s.contains("info-spin")).await;
        assert!(t.vt.animating(), "Modal::Info containing Spinner must drive animation");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        let _ = overlay.await;

        // 2. Confirm:render returning {} (empty non-nil) triggers fallback + notice + remains operable
        exec(
            &t.vt.host,
            r#"
            function mare.ui.Confirm:render(_c)
                return {}
            end
            "#,
        )
        .await;
        let h2 = t.vt.host.clone();
        let ans =
            tokio::task::spawn_local(async move { eval::<bool>(&h2, "return mare.ui.confirm('Empty table?')").await });
        t.until("confirm fallback on empty table", |s| {
            s.contains("Confirm") && s.contains("Empty table?") && s.contains("▸ [Y]es")
        })
        .await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(ans.await.unwrap());
        t.until("empty confirm failure notice", |s| s.contains("mare.ui.Confirm:render")).await;

        // 3. Confirm:render returning 2-frame Frames (with empty still!) animates when reduce_motion=false
        exec(
            &t.vt.host,
            r#"
            function mare.ui.Confirm:render(c)
                return mare.ui.Block(
                    mare.ui.Frames({
                        mare.ui.Line({ mare.ui.Span("C-ANIM-1: " .. c.prompt) }),
                        mare.ui.Line({ mare.ui.Span("C-ANIM-2: " .. c.prompt) }),
                    }, 80, mare.ui.Line({})),
                    { title = "Confirm" }
                )
            end
            "#,
        )
        .await;
        let h3 = t.vt.host.clone();
        let ans2 =
            tokio::task::spawn_local(async move { eval::<bool>(&h3, "return mare.ui.confirm('Animated?')").await });
        t.until("animated confirm", |s| s.contains("C-ANIM-1: Animated?") || s.contains("C-ANIM-2: Animated?")).await;
        assert!(t.vt.animating(), "2-frame animated Confirm must drive animation ticks when idle");
        assert!(
            t.screen().contains("C-ANIM-"),
            "Confirm with empty still must not falsely fall back when reduce_motion is false"
        );
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.step().await;
        assert!(!ans2.await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn select_fallback_snapshot_and_selected_style() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            function mare.ui.Select:frame(_s)
                error("broken select")
            end
            "#,
        )
        .await;

        let h = t.vt.host.clone();
        let picked = tokio::task::spawn_local(async move {
            eval::<Option<i64>>(&h, "return mare.ui.select({ title = 'Fallback Select', items = { 'alpha', 'beta' } })")
                .await
        });
        t.until("select fallback", |s| s.contains("Fallback Select") && s.contains("alpha")).await;

        // Verify selected row ('alpha') has highlight background styling in terminal cells.
        let screen_obj = t.parser.screen();
        let mut found_styled_row = false;
        for row in 0..24 {
            let line = screen_obj.contents_between(row, 0, row, 69);
            if line.contains("alpha") {
                if let Some(col) = line.find("alpha")
                    && let Some(cell) = screen_obj.cell(row, col as u16)
                {
                    found_styled_row = cell.bgcolor() != vt100::Color::Default || cell.bold() || cell.inverse();
                }
                break;
            }
        }
        assert!(found_styled_row, "selected row in Select fallback must have highlight styling");
        insta::assert_snapshot!("select_fallback_fixture", t.snap().await);

        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        let _ = picked.await;

        // Multi-line Select item: every line of the selected item must keep highlight styling in fallback.
        t.resize(40, 24).await;
        let h2 = t.vt.host.clone();
        let picked2 = tokio::task::spawn_local(async move {
            eval::<Option<i64>>(
                &h2,
                "return mare.ui.select({ title = 'Wrap Select', items = { { label = 'alpha', lines = { 'wrapped row one', 'wrapped row two' } }, 'beta' } })",
            )
            .await
        });
        t.until("wrapped select fallback", |s| s.contains("wrapped row one") && s.contains("wrapped row two")).await;
        let screen_obj = t.parser.screen();
        let mut styled_rows = 0usize;
        for row in 0..24 {
            let line = screen_obj.contents_between(row, 0, row, 39);
            for needle in ["wrapped row one", "wrapped row two"] {
                if let Some(col) = line.find(needle)
                    && let Some(cell) = screen_obj.cell(row, col as u16)
                    && (cell.bgcolor() != vt100::Color::Default || cell.bold() || cell.inverse())
                {
                    styled_rows += 1;
                }
            }
        }
        assert_eq!(styled_rows, 2, "every row of a multi-line selected item must stay highlighted:\n{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        let _ = picked2.await;
    })
    .await;
}

#[tokio::test]
async fn component_failure_recovery_and_refailure_notifies_again() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        t.vt.dismiss_notices();

        // 1. Fail -> notifies once, and repeated toggle / reopen while still returning {} does not re-notify.
        exec(&t.vt.host, "function mare.ui.Confirm:render() return {} end").await;
        let h = t.vt.host.clone();
        let a1 = tokio::task::spawn_local(async move { eval::<bool>(&h, "return mare.ui.confirm('Step 1')").await });
        t.until("step 1", |s| s.contains("Step 1")).await;
        t.key(KeyCode::Right, Modifiers::NONE);
        t.step().await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(a1.await.unwrap());
        t.until("first empty failure notice", |s| s.contains("returned empty node")).await;
        t.vt.dismiss_notices();

        let h1b = t.vt.host.clone();
        let a1b =
            tokio::task::spawn_local(async move { eval::<bool>(&h1b, "return mare.ui.confirm('Step 1b')").await });
        t.until("step 1b", |s| s.contains("Step 1b")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(a1b.await.unwrap());
        assert!(
            !t.screen().contains("returned empty node"),
            "still-broken Confirm must not re-emit failure notice on subsequent render:\n{}",
            t.screen()
        );

        // 2. Recover -> succeeds (clears failed state)
        exec(
            &t.vt.host,
            r#"
            function mare.ui.Confirm:render(c)
                return mare.ui.Block(mare.ui.Line({ mare.ui.Span("OK: " .. c.prompt) }), { title = "Confirm" })
            end
            "#,
        )
        .await;
        let h2 = t.vt.host.clone();
        let a2 = tokio::task::spawn_local(async move { eval::<bool>(&h2, "return mare.ui.confirm('Step 2')").await });
        t.until("step 2", |s| s.contains("OK: Step 2")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(a2.await.unwrap());

        // 3. Fail again -> notifies again
        exec(&t.vt.host, "function mare.ui.Confirm:render() error('fail 2') end").await;
        let h3 = t.vt.host.clone();
        let a3 = tokio::task::spawn_local(async move { eval::<bool>(&h3, "return mare.ui.confirm('Step 3')").await });
        t.until("step 3", |s| s.contains("Step 3")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(a3.await.unwrap());
        t.until("second failure notice after recovery", |s| s.contains("fail 2")).await;
    })
    .await;
}

/// Ref: Kimi `packages/pi-tui/test/tui-render.test.ts:178-235` & Codex `screen_reader_tests.rs:130-150`:
/// drives the REAL production `event_loop::step_loop` under `tokio::time::pause()`. Removing the
/// `if animating` gate in `event_loop.rs` must fail the idle zero-redraw assertion.
#[tokio::test]
async fn production_animation_timer_advances_frames_and_stops_when_removed() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        // Wait for background startup (`app/ready`, `StartupReady`, `ui/ready`) to settle before
        // pausing tokio time so late startup UI commands cannot race with the idle redraw check.
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        // Install an animated Spinner widget via Lua -> delivered through production `step_loop`'s `ui_rx`.
        // Note: `Spinner` keys off `app.frame` (advanced by `step_loop`'s `anim_at` timer), so it responds
        // deterministically under `tokio::time::pause()` without depending on wall-clock `Instant::now()`.
        exec(
            &t.vt.host,
            r#"
            mare.ui.set_widget("timer_w", "above-editor", mare.ui.Spinner("TICK-SPIN"))
            "#,
        )
        .await;
        let appeared = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(Duration::from_secs(2), |vt| {
                    let out = vt.output();
                    if !out.is_empty() {
                        parser.process(&out);
                    }
                    vt.animating() && parser.screen().contents().contains("TICK-SPIN")
                })
                .await
        };
        assert!(appeared, "initial animated widget should appear:\n{}", t.screen());
        assert!(t.vt.animating());
        let first_line = t
            .screen()
            .lines()
            .find(|l| l.contains("TICK-SPIN"))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("initial animated widget should appear:\n{}", t.screen()));

        let f0 = t.vt.anim_frame_count();
        let mut tick_bytes = Vec::new();
        let advanced = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(mare_tui::schedule::IDLE + Duration::from_millis(50), |vt| {
                    let bytes = vt.output();
                    if !bytes.is_empty() {
                        parser.process(&bytes);
                        tick_bytes.extend_from_slice(&bytes);
                    }
                    vt.anim_frame_count() > f0
                        && !tick_bytes.is_empty()
                        && parser
                            .screen()
                            .contents()
                            .lines()
                            .find(|l| l.contains("TICK-SPIN"))
                            .is_some_and(|l| l != first_line)
                })
                .await
        };
        assert!(!tick_bytes.is_empty(), "production animation tick must redraw terminal output");
        assert!(t.vt.anim_frame_count() > f0, "production animation timer must advance frame counter");
        let second_line = t
            .screen()
            .lines()
            .find(|l| l.contains("TICK-SPIN"))
            .map(str::to_string)
            .unwrap_or_else(|| panic!("animated widget missing after tick:\n{}", t.screen()));
        assert!(
            advanced && second_line != first_line,
            "production timer tick must advance spinner frame:\n{}",
            t.screen()
        );

        // Remove widget -> wait for removal frame to render & stop animation, then advance 5s in
        // production loop: zero frames & zero bytes.
        exec(&t.vt.host, r#"mare.ui.set_widget("timer_w", "above-editor", nil)"#).await;
        let removed = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(Duration::from_secs(2), |vt| {
                    let out = vt.output();
                    if !out.is_empty() {
                        parser.process(&out);
                    }
                    !vt.animating() && !parser.screen().contents().contains("TICK-SPIN")
                })
                .await
        };
        assert!(removed && !t.vt.animating(), "removing widget must stop animation:\n{}", t.screen());
        let _ = t.vt.output();
        let f1 = t.vt.anim_frame_count();
        t.vt.run_production_loop_for(Duration::from_secs(5)).await;
        let idle_bytes = t.vt.output();
        assert_eq!(t.vt.anim_frame_count(), f1, "idle production loop must not advance frame counter");
        assert!(idle_bytes.is_empty(), "idle production loop must emit zero redraw bytes, got {}", idle_bytes.len());
    })
    .await;
}

/// While working is active, the animation timer must NEVER drop to DEEP_IDLE (5s) even after
/// >30s of silence with zero `ui_rx` traffic (e.g. a silent build or slow tool call).
#[tokio::test]
async fn working_animation_never_enters_deep_idle_after_30s_silent() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        exec(&t.vt.host, r#"mare.ui.working("Silent build")"#).await;
        let appeared = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(Duration::from_secs(2), |vt| {
                    let out = vt.output();
                    if !out.is_empty() {
                        parser.process(&out);
                    }
                    vt.busy() && parser.screen().contents().contains("Silent build")
                })
                .await
        };
        assert!(appeared, "working indicator must appear:\n{}", t.screen());

        // Advance past DEEP_AFTER (30s) with zero ui_rx activity while working stays active.
        t.vt.run_production_loop_for(Duration::from_secs(31)).await;
        let _ = t.vt.output();
        let f_at_31s = t.vt.anim_frame_count();

        // Over the next 4 seconds (31s..35s), working ticks must stay at active cadence (~80ms -> ~50 ticks),
        // never dropping to DEEP_IDLE (5s -> 0-1 ticks).
        t.vt.run_production_loop_for(Duration::from_secs(4)).await;
        let ticks = t.vt.anim_frame_count() - f_at_31s;
        assert!(ticks >= 30, "working animation dropped to deep idle after 30s silent: got only {ticks} ticks in 4s");
    })
    .await;
}

/// While working is active and the terminal is unfocused, animation slowdown must be capped
/// at ~2x normal cadence (~160ms), never dropping to DEEP_IDLE (5s).
#[tokio::test]
async fn working_animation_unfocused_caps_slowdown_to_2x() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        exec(&t.vt.host, r#"mare.ui.working("Background compile")"#).await;
        let appeared = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(Duration::from_secs(2), |vt| {
                    let out = vt.output();
                    if !out.is_empty() {
                        parser.process(&out);
                    }
                    vt.busy() && parser.screen().contents().contains("Background compile")
                })
                .await
        };
        assert!(appeared, "working indicator must appear:\n{}", t.screen());

        t.vt.focus(false);
        let _ = t.vt.output();
        let f0 = t.vt.anim_frame_count();
        t.vt.run_production_loop_for(Duration::from_secs(2)).await;
        let ticks = t.vt.anim_frame_count() - f0;
        assert!(
            (8..=20).contains(&ticks),
            "unfocused working animation should slow to ~2x (~12 ticks in 2s), got {ticks}"
        );
    })
    .await;
}

/// Priority #1 regression: switching away from a terminal tab (`FocusOut`) and switching back
/// (`FocusIn`) while working must immediately redraw (dirty + urgent) and advance the next
/// animation tick within `<= ANIMATE` (never freezing up to 5s).
#[tokio::test]
async fn focus_in_after_focus_out_resumes_next_frame_within_animate() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        exec(&t.vt.host, r#"mare.ui.working("Tab switch work")"#).await;
        let appeared = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(Duration::from_secs(2), |vt| {
                    let out = vt.output();
                    if !out.is_empty() {
                        parser.process(&out);
                    }
                    vt.busy() && parser.screen().contents().contains("Tab switch work")
                })
                .await
        };
        assert!(appeared, "working indicator must appear:\n{}", t.screen());

        // Lose focus and let unfocused cadence run for 1.5s.
        t.vt.focus(false);
        t.vt.run_production_loop_for(Duration::from_millis(1500)).await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await, Some(false));

        // Regain focus: must immediately redraw and tick again within `<= ANIMATE`.
        let _ = t.vt.output();
        let f_before = t.vt.anim_frame_count();
        t.vt.focus(true);
        let resumed = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(mare_tui::schedule::ANIMATE + Duration::from_millis(5), |vt| {
                    let out = vt.output();
                    if !out.is_empty() {
                        parser.process(&out);
                    }
                    vt.anim_frame_count() > f_before
                })
                .await
        };
        assert!(resumed, "FocusIn must advance next animation tick within <= ANIMATE (80ms)");
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await, Some(true));
    })
    .await;
}

/// Every cadence-affecting state transition (focus, key/mouse/paste/resize activity, `working`
/// start/end, `reduce_motion` toggle) must take effect immediately because `step_loop` computes
/// `next_anim_deadline` fresh on every iteration instead of storing a stale `anim_at`.
#[tokio::test]
async fn state_transitions_recompute_anim_cadence_immediately() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        // Install a non-working animated widget so DEEP_IDLE (30s -> 5s) applies while idle.
        exec(&t.vt.host, r#"mare.ui.set_widget("trans_w", "above-editor", mare.ui.Spinner("TRANS"))"#).await;
        assert!(t.vt.run_production_loop_until(Duration::from_secs(2), |vt| vt.animating()).await);

        // 1. After 31s silent idle, widget enters DEEP_IDLE (5s). Verify Mouse, Paste, Resize,
        //    and Key each immediately wake cadence back to IDLE (250ms) and Key/Mouse/Paste
        //    restore focus when unfocused.
        let wake_events = [
            (
                "mouse",
                mare_tui::tty::Event::Mouse(mare_tui::tty::MouseEvent {
                    kind: mare_tui::tty::MouseEventKind::Moved,
                    column: 1,
                    row: 1,
                    modifiers: Modifiers::NONE,
                }),
                true,
            ),
            ("paste", mare_tui::tty::Event::Paste("x".into()), true),
            (
                "resize",
                mare_tui::tty::Event::WindowResized(mare_tui::tty::WindowSize {
                    cols: 82,
                    rows: 24,
                    pixel_width: None,
                    pixel_height: None,
                }),
                false,
            ),
            ("key", mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('a'), Modifiers::NONE)), true),
        ];
        for (name, ev, proves_focus) in wake_events {
            t.vt.focus(false);
            t.vt.run_production_loop_for(Duration::from_secs(31)).await;
            let f0 = t.vt.anim_frame_count();
            t.vt.send_tty_event(ev);
            if !proves_focus {
                t.vt.focus(true);
            }
            let woke =
                t.vt.run_production_loop_until(mare_tui::schedule::IDLE + Duration::from_millis(20), |vt| {
                    vt.anim_frame_count() > f0
                })
                .await;
            assert!(woke, "{name} event must wake animation from DEEP_IDLE within <= IDLE");
            if proves_focus {
                assert_eq!(
                    eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await,
                    Some(true),
                    "{name} event must restore ui.focused() == true"
                );
            }
        }

        // 2. working start (false -> true) accelerates from IDLE (250ms) to ANIMATE (80ms);
        //    working end (true -> false) decelerates back to IDLE (250ms).
        t.vt.run_production_loop_for(Duration::from_secs(31)).await; // back into DEEP_IDLE
        let f_start = t.vt.anim_frame_count();
        exec(&t.vt.host, r#"mare.ui.working("Starting")"#).await;
        let fast =
            t.vt.run_production_loop_until(mare_tui::schedule::ANIMATE + Duration::from_millis(20), |vt| {
                vt.busy() && vt.anim_frame_count() > f_start
            })
            .await;
        assert!(fast, "working start must accelerate cadence to <= ANIMATE");

        exec(&t.vt.host, r#"mare.ui.working(nil)"#).await;
        assert!(t.vt.run_production_loop_until(Duration::from_secs(1), |vt| !vt.busy()).await);
        let f_idle = t.vt.anim_frame_count();
        t.vt.run_production_loop_for(Duration::from_secs(1)).await;
        let idle_ticks = t.vt.anim_frame_count() - f_idle;
        assert!((2..=6).contains(&idle_ticks), "working end must decelerate to IDLE (~4 ticks/s), got {idle_ticks}");

        // 3. reduce_motion toggle: false -> true slows to ~1s; true -> false immediately resumes
        //    within <= ANIMATE instead of waiting out the remaining 1s timer.
        exec(&t.vt.host, r#"mare.ui.working("Motion test")"#).await;
        assert!(t.vt.run_production_loop_until(Duration::from_secs(1), |vt| vt.busy()).await);
        exec(&t.vt.host, r#"mare.o.reduce_motion = true"#).await;
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        let f_rm = t.vt.anim_frame_count();
        t.vt.run_production_loop_for(Duration::from_millis(2500)).await;
        let rm_ticks = t.vt.anim_frame_count() - f_rm;
        assert!((1..=4).contains(&rm_ticks), "reduce_motion=true must tick ~1/s, got {rm_ticks} in 2.5s");

        // Turn reduce_motion off mid-second -> must wake within <= ANIMATE.
        t.vt.run_production_loop_for(Duration::from_millis(200)).await;
        let f_back = t.vt.anim_frame_count();
        exec(&t.vt.host, r#"mare.o.reduce_motion = false"#).await;
        let resumed =
            t.vt.run_production_loop_until(mare_tui::schedule::ANIMATE + Duration::from_millis(20), |vt| {
                vt.anim_frame_count() > f_back
            })
            .await;
        assert!(resumed, "reduce_motion=false must immediately resume <= ANIMATE cadence");
    })
    .await;
}

/// Carbon #5c (calibrated per xval-tui): `StreamCommit` catches up in one batch once a line ages
/// past `ENTER_OLDEST_AGE` (120ms), but without self-driving `dirty = true` that catch-up still
/// waits for the next animation tick (1s under `reduce_motion`, or indefinitely when no `working`
/// indicator is active). Setting `dirty = app.stream_commit_pending` drains queued rows smoothly
/// at 16ms/frame right away even under `reduce_motion = true` and `FocusOut` without `working`.
#[tokio::test]
async fn stream_commit_self_drains_under_reduce_motion_and_unfocused() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        exec(&t.vt.host, r#"mare.o.reduce_motion = true"#).await;
        t.vt.focus(false);
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;

        // Push a streaming assistant block with 5 completed lines (below the 8-line catch-up
        // threshold -> Smooth DrainPlan::Single, 1 line/frame) plus a trailing live paragraph.
        exec(
            &t.vt.host,
            r#"
            local id = mare.ui.push("assistant", { markdown = "line1\n\nline2\n\nline3\n\ntrailing" })
            _G.__sc_id = id
            "#,
        )
        .await;
        let drained = {
            let Term { owned, parser, .. } = &mut t;
            owned
                .run_production_loop_until(Duration::from_millis(300), |vt| {
                    let out = vt.output();
                    if !out.is_empty() {
                        parser.process(&out);
                    }
                    let screen = parser.screen().contents();
                    screen.contains("line1")
                        && screen.contains("line2")
                        && screen.contains("line3")
                        && screen.contains("trailing")
                })
                .await
        };
        assert!(drained, "StreamCommit must self-drain queued lines within 300ms under reduce_motion+unfocused");
        // Verify all 5 stable lines drained smoothly within 300ms (without `working` or waiting
        // for a 1s `reduce_motion` tick).
        t.vt.run_production_loop_for(Duration::from_millis(200)).await;
        let _ = t.vt.output();
        exec(&t.vt.host, r#"mare.ui.finish(_G.__sc_id)"#).await;
        assert!(t.vt.run_production_loop_until(Duration::from_millis(100), |vt| vt.live_blocks() == 0).await);
    })
    .await;
}

/// `PasteGuard` and `EscJoin` must use event arrival timestamps, not processing time,
/// so a UI stall that drains `[Paste(t0), Enter(t0+300ms)]` or `[Esc(t0), 'j'(t0+300ms)]` in one
/// batch does NOT swallow Enter or merge Esc+'j'.
#[tokio::test]
async fn stalled_input_batch_uses_arrival_timestamps_for_paste_guard_and_esc_join() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        let t0 = mare_tui::render::clock_now();
        // 1. Stall batch: Esc at t0 followed by 'j' arriving 300ms later (well past 10ms ESC_TIMEOUT).
        //    Must NOT join into a CSI/Alt sequence; 'j' must land in the editor as a normal char.
        t.vt.send_tty_event_at(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE)), t0);
        t.vt.send_tty_event_at(
            mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('j'), Modifiers::NONE)),
            t0 + Duration::from_millis(300),
        );
        assert!(
            t.vt.run_production_loop_until(Duration::from_millis(50), |vt| vt.editor_text() == "j").await,
            "stalled Esc + 'j' (300ms arrival gap) must keep 'j' as plain text, got {:?}",
            t.vt.editor_text()
        );

        // 2. Stall batch: Paste at t0 followed by Enter arriving 300ms later (past 150ms GUARD_WINDOW).
        //    Enter must NOT be swallowed by PasteGuard — it must submit (clearing the editor).
        t.vt.send_tty_event_at(mare_tui::tty::Event::Paste("hello".into()), t0);
        t.vt.send_tty_event_at(
            mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE)),
            t0 + Duration::from_millis(300),
        );
        let submitted =
            t.vt.run_production_loop_until(Duration::from_millis(100), |vt| vt.editor_text().is_empty()).await;
        assert!(
            submitted,
            "stalled Paste + Enter (300ms arrival gap) must NOT swallow Enter, editor={:?}",
            t.vt.editor_text()
        );
    })
    .await;
}

/// Leading + trailing edge throttle for live tool output — when a tool emits a burst
/// of deltas within the 100ms throttle window and then goes silent, the final lines must flush
/// within <= 150ms instead of staying hidden until the tool finishes.
#[tokio::test]
async fn tool_output_throttle_flushes_trailing_edge_after_burst() {
    local(async {
        // This test exercises live tool output throttling, not permissions: disable permissions
        // so Windows' default tier (`windows-acl`, where untrusted `read-only` intentionally gates
        // shell commands behind approval and rejects Git Bash under a restricted token) runs the
        // burst command directly like other platforms.
        let mut t = Term::with(
            json!([
                {
                    "tool_calls": [{
                        "name": "bash",
                        "arguments": {
                            "command": "printf 'FIRST_LINE\\n'; sleep 0.01; printf 'SECOND_LINE\\n'; sleep 0.01; printf 'PROCEED_TRAILING_PROMPT\\n'; sleep 8",
                            "description": "burst then silent"
                        }
                    }]
                },
                { "text": "BURST_DONE" }
            ]),
            json!({ "plugins": { "permissions": { "enabled": false } } }),
            90,
            24,
        )
        .await;
        t.vt.type_str("run burst");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.step().await;
        // Must see the final line of the burst well before the 8s sleep completes!
        t.until("trailing line flushed during silence", |s| s.contains("PROCEED_TRAILING_PROMPT")).await;
        assert!(
            t.vt.busy(),
            "trailing line must appear while bash is still sleeping, not only after completion"
        );
        // Interrupt the sleeping bash command so the test finishes quickly.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        t.until_vt("turn stops after interrupt", |t| !t.vt.busy()).await;
    })
    .await;
}

/// Step 5: `ui.working.text_throttle_ms()` caches its resolved value on the hot path while
/// still reacting immediately when `mare.o.ui.working` is mutated at runtime.
#[tokio::test]
async fn working_text_throttle_cache_stays_reactive_on_runtime_mutation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let (default_ms, after_set, after_reset): (i64, i64, i64) = eval(
            &h,
            r#"
            local W = require("ui.working")
            local d = W.text_throttle_ms()
            -- Hot-path repeat hits cache cleanly
            assert(W.text_throttle_ms() == d)
            mare.o.ui.working = { text_throttle_ms = 42 }
            local after = W.text_throttle_ms()
            mare.o.ui.working = { text_throttle_ms = 250 }
            local reset = W.text_throttle_ms()
            return d, after, reset
            "#,
        )
        .await;
        assert_eq!(default_ms, 250);
        assert_eq!(after_set, 42);
        assert_eq!(after_reset, 250);
        h.shutdown().await;
    })
    .await;
}

/// Regression: opening regular Ctrl+T transcript must keep visible active blocks animating.
#[tokio::test]
async fn regular_transcript_keeps_visible_live_block_animating() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();

        tokio::time::pause();
        t.vt.reset_loop_state();

        exec(
            &t.vt.host,
            r#"
            mare.ui.push("tool", {
                node = mare.ui.Spinner("BLK-SPIN")
            })
            "#,
        )
        .await;
        t.step().await;

        // Open regular transcript (`Ctrl+T`) on the paused clock so `first_line` is rendered at
        // `anim_millis() == 0` on the same clock epoch that `run_production_loop_for` advances.
        t.ctrl('t');
        t.step().await;
        assert!(
            t.screen().contains("Transcript") && t.screen().contains("BLK-SPIN"),
            "regular transcript must show live animated block:\n{}",
            t.screen()
        );
        assert!(t.vt.animating(), "visible animated block inside regular transcript must keep animating()");
        let first_line = t.screen().lines().find(|l| l.contains("BLK-SPIN")).map(str::to_string).unwrap();
        let f0 = t.vt.anim_frame_count();

        t.vt.run_production_loop_for(mare_tui::schedule::IDLE + Duration::from_millis(50)).await;
        let out = t.vt.output();
        t.parser.process(&out);
        let second_line = t.screen().lines().find(|l| l.contains("BLK-SPIN")).map(str::to_string).unwrap();
        assert!(t.vt.anim_frame_count() > f0, "regular transcript visible live block must tick animation frames");
        assert_ne!(
            second_line,
            first_line,
            "regular transcript visible live block must advance frames:\n{}",
            t.screen()
        );
    })
    .await;
}

/// Regression: narrow (width = 2) transcript with notice must not underflow `cw - 4` or panic.
#[tokio::test]
async fn narrow_transcript_notice_renders_without_panic_snapshot() {
    local(async {
        let mut t = Term::new(json!([]), 2, 14).await;
        t.ctrl('t');
        t.step().await;
        exec(&t.vt.host, r#"mare.ui.notify("hi", "warn")"#).await;
        t.step().await;
        let screen = t.screen();
        let trimmed_lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
        let end = trimmed_lines.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(0);
        insta::assert_snapshot!("narrow_transcript_notice", trimmed_lines[..end].join("\n"));
    })
    .await;
}

/// A short transcript that cannot fit a notice defers its expiry until space returns.
#[tokio::test]
async fn short_transcript_defers_overflowing_notice_until_height_restored() {
    local(async {
        let mut t = Term::new(json!([]), 80, 4).await;
        t.vt.dismiss_notices();
        t.ctrl('t');
        t.step().await;
        t.vt.push_aged_notice("deferred-short-notice", Duration::from_secs(5));
        t.step().await;
        // The card is deferred without resetting the time already spent visible.
        let age = t.vt.oldest_notice_age().expect("notice must stay deferred rather than dropped");
        assert!(age >= Duration::from_secs(5) && age < Duration::from_secs(6), "deferred notice age: {age:?}");
        t.resize(80, 24).await;
        assert!(t.screen().contains("deferred-short-notice"), "notice must appear once height allows:\n{}", t.screen());
    })
    .await;
}

/// `NoticeCard`, `CompletionPopup`, and `WhichKey` render via `mare.ui.<Comp>:render`
/// and fall back cleanly via `Fallback::{Frame, Choice, Lines}` on error/nil while staying operable.
#[tokio::test]
async fn overlay_widgets_use_lua_and_fallback_cleanly() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();

        // 1. NoticeCard customization -> error fallback + notify once.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.NoticeCard:render(p)
                return mare.ui.Block(
                    mare.ui.Lines({ mare.ui.Line("[NC-LUA] " .. p.text) }),
                    { title = p.icon .. " " .. p.title, border_style = p.style }
                )
            end
            mare.ui.notify("custom notice", "warning")
            "#,
        )
        .await;
        t.until("NoticeCard custom Lua render", |s| s.contains("[NC-LUA] custom notice")).await;
        t.vt.dismiss_notices();

        exec(
            &t.vt.host,
            r#"
            function mare.ui.NoticeCard:render(_p) error("boom notice") end
            mare.ui.notify("fallback notice", "info")
            "#,
        )
        .await;
        t.until("NoticeCard fallback renders text and failure notice", |s| {
            s.contains("fallback notice") && s.contains("NoticeCard:render failed")
        })
        .await;
        t.vt.dismiss_notices();

        // 2. CompletionPopup customization -> error fallback still completes on Tab.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.CompletionPopup:render(p)
                local rows = {}
                for _, it in ipairs(p.items) do
                    rows[#rows + 1] = mare.ui.Line("[CMP-LUA] " .. it.label)
                end
                return mare.ui.Block(
                    mare.ui.List(rows, { selected = p.selected, focused = true, height = p.height }),
                    { title = p.title, footer = tostring(p.selected) .. " of " .. tostring(p.total), border_style = "MareBorder" }
                )
            end
            "#,
        )
        .await;
        t.vt.type_str("/");
        t.until("CompletionPopup custom Lua render", |s| s.contains("[CMP-LUA] /")).await;

        exec(&t.vt.host, r#"function mare.ui.CompletionPopup:render(_p) error("boom cmp") end"#).await;
        t.vt.type_str("he");
        t.until("CompletionPopup fallback still lists /help and notifies once", |s| {
            s.contains("/help") && s.contains("CompletionPopup:render") && s.contains("failed")
        })
        .await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("CompletionPopup fallback remains operable on Tab", |s| s.contains("/help ")).await;
        t.key(KeyCode::Char('u'), Modifiers::CONTROL);
        t.vt.dismiss_notices();

        // 3. WhichKey customization -> error fallback still dispatches prefix continuation.
        exec(
            &t.vt.host,
            r#"
            WK_HIT = false
            mare.keymap.set("editor", "<C-x>a", function() WK_HIT = true end, { desc = "wk probe" })
            function mare.ui.WhichKey:render(p)
                local rows = { mare.ui.Line("[WK-LUA] " .. p.prefix) }
                for _, b in ipairs(p.bindings) do
                    rows[#rows + 1] = mare.ui.Line(b.key .. " -> " .. b.action)
                end
                return mare.ui.Block(mare.ui.Lines(rows), { title = p.prefix, border_style = "MareBorderActive" })
            end
            "#,
        )
        .await;
        t.step().await;
        t.ctrl('x');
        t.until("WhichKey custom Lua render", |s| s.contains("[WK-LUA]")).await;
        t.ctrl('c');
        t.step().await;

        exec(&t.vt.host, r#"function mare.ui.WhichKey:render(_p) error("boom wk") end"#).await;
        t.ctrl('x');
        t.until("WhichKey fallback renders bindings", |s| s.contains("wk probe")).await;
        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.until("WhichKey failure notice shown after prefix", |s| {
            s.contains("WhichKey:render") && s.contains("failed")
        })
        .await;
        assert!(eval::<bool>(&t.vt.host, "return WK_HIT == true").await, "WhichKey prefix sequence stays operable");
    })
    .await;
}

/// `HelpModal`, `InputModal`, `InfoModal`, `EditorChrome`, and `SelectRow` open to Lua
/// and fall back cleanly while preserving editing, modal dismissal, and list selection.
#[tokio::test]
async fn chrome_modal_and_row_widgets_use_lua_and_fallback_cleanly() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();

        // 1. HelpModal:frame customization & fallback.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.HelpModal:frame(s)
                local f = mare.ui.Select:frame(s)
                f.title = mare.ui.Line("[HELP-LUA]")
                return f
            end
            "#,
        )
        .await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("HelpModal custom Lua frame", |s| s.contains("[HELP-LUA]")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("help closed", |s| !s.contains("[HELP-LUA]")).await;

        exec(&t.vt.host, r#"function mare.ui.HelpModal:frame(_s) error("boom help") end"#).await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("HelpModal fallback operable", |s| s.contains("Keys")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("help fallback closed and deferred notice shown", |s| {
            !s.contains("ctrl+d") && s.contains("HelpModal:frame") && s.contains("failed")
        })
        .await;
        t.vt.dismiss_notices();

        // 2. InfoModal:frame customization & fallback.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.InfoModal:frame(_s)
                return { title = mare.ui.Line("[INFO-LUA]"), border = "MareBorderActive", footer_style = "MareDim" }
            end
            mare.ui.overlay("Doc", "info body line")
            "#,
        )
        .await;
        t.until("InfoModal custom Lua frame", |s| s.contains("[INFO-LUA]") && s.contains("info body line")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("InfoModal closed", |s| !s.contains("[INFO-LUA]")).await;

        exec(
            &t.vt.host,
            r#"
            function mare.ui.InfoModal:frame(_s) error("boom info") end
            mare.ui.overlay("DocFb", "fallback info body")
            "#,
        )
        .await;
        t.until("InfoModal fallback operable", |s| s.contains("DocFb") && s.contains("fallback info body")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("InfoModal fallback closed and notice shown", |s| {
            !s.contains("fallback info body") && s.contains("InfoModal:frame") && s.contains("failed")
        })
        .await;
        t.vt.dismiss_notices();

        // 3. InputModal:frame customization & fallback still returns entered text.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.InputModal:frame(_s)
                return { title = mare.ui.Line("[INP-LUA]"), border = "MareBorderActive" }
            end
            mare.task.spawn(function() INP_R1 = mare.ui.input({ title = "Prompt1", default = "one" }) end)
            "#,
        )
        .await;
        t.until("InputModal custom Lua frame", |s| s.contains("[INP-LUA]")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("InputModal closed", |s| !s.contains("[INP-LUA]")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return INP_R1").await.as_deref(), Some("one"));

        exec(
            &t.vt.host,
            r#"
            function mare.ui.InputModal:frame(_s) error("boom inp") end
            mare.task.spawn(function() INP_R2 = mare.ui.input({ title = "PromptFb", default = "two" }) end)
            "#,
        )
        .await;
        t.until("InputModal fallback operable", |s| s.contains("PromptFb") && s.contains("two")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("InputModal fallback closed and notice shown", |s| {
            !s.contains("PromptFb") && s.contains("InputModal:frame") && s.contains("failed")
        })
        .await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return INP_R2").await.as_deref(), Some("two"));
        t.vt.dismiss_notices();

        // 4. EditorChrome:render customization & fallback preserves typing.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.EditorChrome:render(s)
                return { prompt = "[ED-LUA] ", placeholder = s.placeholder, border = s.border }
            end
            "#,
        )
        .await;
        t.vt.type_str("x");
        t.until("EditorChrome custom Lua prompt", |s| s.contains("[ED-LUA] x")).await;
        exec(&t.vt.host, r#"function mare.ui.EditorChrome:render(_s) error("boom ed") end"#).await;
        t.key(KeyCode::Char('u'), Modifiers::CONTROL);
        t.step().await;
        t.vt.type_str("typed-fb");
        t.until("EditorChrome fallback keeps typing and notifies once", |s| {
            s.contains("typed-fb") && s.contains("EditorChrome:render") && s.contains("failed")
        })
        .await;
        t.key(KeyCode::Char('u'), Modifiers::CONTROL);
        t.vt.dismiss_notices();

        // 5. SelectRow:render customization & fallback preserves Down/Enter item selection.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.SelectRow:render(item)
                return mare.ui.Line("[ROW-LUA] " .. item.label)
            end
            mare.task.spawn(function() SEL_ROW_R1 = mare.ui.select({ title = "PickRow", items = { "alpha", "beta" } }) end)
            "#,
        )
        .await;
        t.until("SelectRow custom Lua render", |s| s.contains("[ROW-LUA] alpha")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("SelectRow select closed", |s| !s.contains("PickRow")).await;

        // Default SelectRow renders headers + detail rows via Lua without returning nil for lines items.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.SelectRow:render(item)
                if item.header then
                    return mare.ui.Line({ mare.ui.Span(item.label or ""):hl("MareDim") })
                end
                local spans = { mare.ui.Span(item.label or "") }
                if item.detail and item.detail ~= "" then
                    spans[#spans + 1] = mare.ui.Span("  ")
                    spans[#spans + 1] = mare.ui.Span(item.detail):hl("MareMuted")
                end
                return mare.ui.Line(spans)
            end
            mare.task.spawn(function()
                SEL_ROW_PRE = mare.ui.select({
                    title = "PickPre",
                    items = {
                        { label = "Group", header = true },
                        { label = "needle-row", detail = "extra-match-tail" },
                        { label = "card-item", lines = { mare.ui.Line("PreRenderedCard") } },
                    },
                })
            end)
            "#,
        )
        .await;
        t.until("SelectRow default + pre-rendered card", |s| {
            s.contains("Group") && s.contains("needle-row") && s.contains("PreRenderedCard")
        })
        .await;
        t.vt.type_str("tail");
        t.until("SelectRow filter keeps matching detail row", |s| {
            s.contains("needle-row") && !s.contains("PreRenderedCard")
        })
        .await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("SelectRow PickPre closed", |s| !s.contains("PickPre")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return SEL_ROW_PRE").await, Some(2));

        exec(
            &t.vt.host,
            r#"
            function mare.ui.SelectRow:render(_item) error("boom row") end
            mare.task.spawn(function() SEL_ROW_R2 = mare.ui.select({ title = "PickRowFb", items = { "alpha", "beta" } }) end)
            "#,
        )
        .await;
        t.until("SelectRow fallback renders rows", |s| s.contains("PickRowFb") && s.contains("alpha")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("SelectRow fallback closed and notice shown", |s| {
            !s.contains("PickRowFb") && s.contains("SelectRow:render") && s.contains("failed")
        })
        .await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return SEL_ROW_R2").await, Some(2));
    })
    .await;
}

#[tokio::test]
async fn optional_frame_titles_keep_the_original_title() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(&t.vt.host, r#"function mare.ui.InfoModal:frame() return {border = "MareBorder"} end
            mare.ui.overlay("Info original title", "info body")"#).await;
        t.until("info body", |s| s.contains("info body")).await;
        assert!(t.screen().contains("Info original title"), "missing optional title must preserve the info title: {}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("info closed", |s| !s.contains("info body")).await;

        exec(&t.vt.host, r#"function mare.ui.InputModal:frame() return {} end
            mare.task.spawn(function() _G.__optional_input = mare.ui.input({title = "Input original title", default = "input body"}) end)"#).await;
        t.until("input body", |s| s.contains("input body")).await;
        assert!(t.screen().contains("Input original title"), "empty frame must preserve the input title: {}", t.screen());
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("input closed", |s| !s.contains("input body")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return _G.__optional_input").await.as_deref(), Some("input body"));
    }).await
}

#[tokio::test]
async fn secret_input_is_masked_and_ignores_lua_frame_overrides() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(&t.vt.host, r#"_G.__original_input_frame = mare.ui.InputModal.frame
            _G.__secret_frame_calls = 0
            function mare.ui.InputModal:frame()
              _G.__secret_frame_calls = _G.__secret_frame_calls + 1
              return {title = "UNSAFE OVERRIDE", border = "MareNotifyError"}
            end"#).await;
        for (title, secret, mode) in [
            ("Secret custom", "smoke-password", "custom"),
            ("Secret failing override", "páss密", "error"),
            ("Secret default", "default-secret", "default"),
        ] {
            if mode == "error" {
                exec(&t.vt.host, r#"function mare.ui.InputModal:frame()
                    _G.__secret_frame_calls = _G.__secret_frame_calls + 1
                    error("secret frame must never execute")
                end"#).await;
            } else if mode == "default" {
                exec(&t.vt.host, "mare.ui.InputModal.frame = _G.__original_input_frame").await;
            }
            exec(&t.vt.host, &format!(r#"mare.task.spawn(function() _G.__secret_result = mare.ui.input({{title = "{title}", secret = true}}) end)"#)).await;
            t.until("secret input opened", |s| s.contains(title) || s.contains("UNSAFE OVERRIDE")).await;
            for (index, character) in secret.chars().enumerate() {
                t.vt.type_str(&character.to_string());
                t.until("secret input masked", |s| s.matches('•').count() == index + 1).await;
                assert!(!t.screen().contains(secret), "secret leaked while typing: {}", t.screen());
            }
            let screen = t.screen();
            assert!(screen.contains(title), "the core must retain the protected title: {screen}");
            assert!(!screen.contains("UNSAFE OVERRIDE") && !screen.contains(secret), "secret leaked or Lua changed its frame: {screen}");
            assert_eq!(screen.matches('•').count(), secret.chars().count(), "mask count: {screen}");
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("secret input closed", |s| !s.contains(title) && !s.contains("UNSAFE OVERRIDE")).await;
            assert_eq!(eval::<Option<String>>(&t.vt.host, "return _G.__secret_result").await.as_deref(), Some(secret));
        }
        assert_eq!(eval::<i64>(&t.vt.host, "return _G.__secret_frame_calls").await, 0, "secret frames must not call Lua");
    }).await
}

#[tokio::test]
async fn select_row_cache_keeps_indices_and_survives_large_lists() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(
            &t.vt.host,
            r#"_G.__row_calls = 0
            function mare.ui.SelectRow:render(item)
              _G.__row_calls = _G.__row_calls + 1
              return mare.ui.Line(tostring(item.index) .. ":duplicate")
            end
            local items = {}
            for i = 1, 520 do items[i] = "duplicate" end
            mare.task.spawn(function() mare.ui.select({title = "Large list", items = items}) end)"#,
        )
        .await;
        t.until("indexed large list", |s| s.contains("1:duplicate") && s.contains("2:duplicate")).await;
        assert!(t.screen().contains("3:duplicate"), "identical labels must retain their own index: {}", t.screen());
        let first: i64 = eval(&t.vt.host, "return _G.__row_calls").await;
        assert!(first > 0, "visible rows must have been rendered");
        t.step().await;
        let stable: i64 = eval(&t.vt.host, "return _G.__row_calls").await;
        assert!(
            stable - first < 256,
            "a large list must retain cache hits instead of clearing every frame: {first} -> {stable}"
        );

        t.resize(40, 16).await;
        let resized: i64 = eval(&t.vt.host, "return _G.__row_calls").await;
        assert!(resized > stable, "width changes must invalidate row rendering: {stable} -> {resized}");
        assert!(t.screen().contains("1:duplicate"));
        exec(&t.vt.host, r##"mare.ui.style("MareDim", {fg = "#21ab43"})"##).await;
        t.step().await;
        let themed: i64 = eval(&t.vt.host, "return _G.__row_calls").await;
        assert!(themed > resized, "theme changes must invalidate row rendering: {resized} -> {themed}");
        t.key(KeyCode::Escape, Modifiers::NONE);
    })
    .await
}

#[tokio::test]
async fn completion_cache_hits_and_invalidates_for_inputs_and_display() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(
            &t.vt.host,
            r#"_G.__completion_calls = 0
            local render = mare.ui.CompletionPopup.render
            function mare.ui.CompletionPopup:render(props)
              _G.__completion_calls = _G.__completion_calls + 1
              return render(self, props)
            end"#,
        )
        .await;
        t.vt.type_str("/");
        t.until("completion open", |s| s.contains("Commands") && s.contains("/add-dir")).await;
        let first: i64 = eval(&t.vt.host, "return _G.__completion_calls").await;
        assert!(first > 0);
        for _ in 0..3 {
            t.step().await;
        }
        let stable: i64 = eval(&t.vt.host, "return _G.__completion_calls").await;
        assert_eq!(stable, first, "unchanged completion must hit the cache");
        t.key(KeyCode::Down, Modifiers::NONE);
        t.step().await;
        let selected: i64 = eval(&t.vt.host, "return _G.__completion_calls").await;
        assert!(selected > stable, "selection change must invalidate completion");
        t.resize(40, 16).await;
        let resized: i64 = eval(&t.vt.host, "return _G.__completion_calls").await;
        assert!(resized > selected, "resize must invalidate completion");
        exec(&t.vt.host, r##"mare.ui.style("MareDim", {fg = "#21ab43"})"##).await;
        t.step().await;
        let themed: i64 = eval(&t.vt.host, "return _G.__completion_calls").await;
        assert!(themed > resized, "theme change must invalidate completion");
        t.vt.type_str("hel");
        t.until("updated completion", |s| s.contains("/hel") && s.contains("/help")).await;
        let changed: i64 = eval(&t.vt.host, "return _G.__completion_calls").await;
        assert!(changed > themed, "changed items must invalidate completion");
    })
    .await
}

#[tokio::test]
async fn select_row_nil_empty_and_yield_fall_back_without_blocking() {
    local(async {
        for (title, body) in [("Nil row", "return nil"), ("Empty row", "return {}"), ("Yield row", "coroutine.yield()") ] {
            let mut t = Term::new(json!([]), 80, 24).await;
            t.until_vt("startup", |t| !t.vt.starting()).await;
            t.vt.dismiss_notices();
            exec(&t.vt.host, &format!(r#"function mare.ui.SelectRow:render(_) {body} end
              mare.task.spawn(function() _G.__fallback_choice = mare.ui.select({{title = "{title}", items = {{"alpha", "beta"}}}}) end)"#)).await;
            t.until("fallback rows", |s| s.contains(title) && s.contains("alpha") && s.contains("beta")).await;
            t.key(KeyCode::Down, Modifiers::NONE);
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("fallback closed", |s| !s.contains(title)).await;
            assert_eq!(eval::<Option<i64>>(&t.vt.host, "return _G.__fallback_choice").await, Some(2), "{title}");
            for _ in 0..3 { t.step().await; }
            let screen = t.screen();
            assert_eq!(screen.matches("SelectRow:render").count(), 1, "{title}: expected one failure notice: {screen}");
            t.vt.dismiss_notices();

            exec(&t.vt.host, &format!(r#"mare.task.spawn(function() _G.__fallback_choice = mare.ui.select({{title = "{title}", items = {{"alpha", "beta"}}}}) end)"#)).await;
            t.until("reopened fallback rows", |s| s.contains(title) && s.contains("beta")).await;
            t.key(KeyCode::Down, Modifiers::NONE);
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("reopened fallback closed", |s| !s.contains(title)).await;
            assert_eq!(eval::<Option<i64>>(&t.vt.host, "return _G.__fallback_choice").await, Some(2), "{title}");
            for _ in 0..3 { t.step().await; }
            assert!(!t.screen().contains("SelectRow:render"), "{title}: duplicate notice: {}", t.screen());
        }
    }).await
}

#[tokio::test]
async fn custom_multiline_select_row_scrolls_and_recovers_after_failure() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        // 1. Custom multi-line SelectRow with offscreen 3-line item accurately scrolls & sizes.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.SelectRow:render(item)
                if item.label == "item-20" then
                    return mare.ui.Lines({
                        mare.ui.Line("TALL-TOP"),
                        mare.ui.Line("TALL-MID"),
                        mare.ui.Line("TALL-BOT"),
                    })
                end
                return mare.ui.Line(item.label)
            end
            local items = {}
            for i = 1, 22 do items[i] = string.format("item-%02d", i) end
            mare.task.spawn(function() _G.__multi_pick = mare.ui.select({ title = "MultiRows", items = items }) end)
            "#,
        )
        .await;
        t.until("MultiRows opened", |s| s.contains("MultiRows") && s.contains("item-01")).await;
        for _ in 0..19 {
            t.key(KeyCode::Down, Modifiers::NONE);
        }
        t.until("offscreen tall item scrolled into view with all 3 rows", |s| {
            s.contains("20 of 22") && s.contains("TALL-TOP") && s.contains("TALL-MID") && s.contains("TALL-BOT")
        })
        .await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("MultiRows closed", |s| !s.contains("MultiRows")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return _G.__multi_pick").await, Some(20));

        // 2. Failing SelectRow in modal 1 must not permanently disable valid Lua SelectRow in later modals.
        exec(
            &t.vt.host,
            r#"
            _G.__allow_row = false
            function mare.ui.SelectRow:render(item)
                if not _G.__allow_row then error("temporary row failure") end
                return mare.ui.Line("[RECOVERED] " .. item.label)
            end
            mare.task.spawn(function() _G.__r1 = mare.ui.select({ title = "BrokenModal", items = { "alpha", "beta" } }) end)
            "#,
        )
        .await;
        t.until("BrokenModal fallback shown", |s| s.contains("BrokenModal") && s.contains("alpha")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("BrokenModal closed", |s| !s.contains("BrokenModal")).await;
        t.vt.dismiss_notices();

        // Same Lua function recovers when condition fixes.
        exec(
            &t.vt.host,
            r#"
            _G.__allow_row = true
            mare.task.spawn(function() _G.__r2 = mare.ui.select({ title = "SameFnOk", items = { "alpha", "beta" } }) end)
            "#,
        )
        .await;
        t.until("SameFnOk uses recovered Lua SelectRow", |s| s.contains("SameFnOk") && s.contains("[RECOVERED] alpha"))
            .await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("SameFnOk closed", |s| !s.contains("SameFnOk")).await;

        // Also recovers when replaced with a brand-new Lua SelectRow function after another failure.
        exec(
            &t.vt.host,
            r#"
            function mare.ui.SelectRow:render(_) error("hard boom") end
            mare.task.spawn(function() mare.ui.select({ title = "HardFail", items = { "one" } }) end)
            "#,
        )
        .await;
        t.until("HardFail fallback", |s| s.contains("HardFail") && s.contains("one")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("HardFail closed", |s| !s.contains("HardFail")).await;

        exec(
            &t.vt.host,
            r#"
            function mare.ui.SelectRow:render(item) return mare.ui.Line("[NEWFN] " .. item.label) end
            mare.task.spawn(function() mare.ui.select({ title = "NewFnOk", items = { "gamma" } }) end)
            "#,
        )
        .await;
        t.until("NewFnOk uses replaced Lua SelectRow", |s| s.contains("NewFnOk") && s.contains("[NEWFN] gamma")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
    })
    .await
}
