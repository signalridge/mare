//! State-transition regression tests for TUI overlays, caches, keymaps, handoffs, and flags.

use super::*;
use mare_tui::tty::Event;

/// Explicit wall-clock ceiling for state-transition waits so a stalled condition fails fast.
const TRANSITION_WAIT: Duration = Duration::from_secs(15);

async fn wait_screen(t: &mut Term, what: &str, f: impl Fn(&str) -> bool) {
    let deadline = Instant::now() + TRANSITION_WAIT;
    loop {
        t.step().await;
        if f(&t.screen()) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; screen:\n{}", t.screen());
        // sleep-ok: polling cadence for bounded virtual terminal condition wait
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

async fn wait_vt(t: &mut Term, what: &str, f: impl Fn(&mut Term) -> bool) {
    let deadline = Instant::now() + TRANSITION_WAIT;
    loop {
        t.step().await;
        if f(t) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}; screen:\n{}", t.screen());
        // sleep-ok: polling cadence for bounded virtual terminal condition wait
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// External editor handoff (`Ctrl+G`) must reset completion/history state and clear stale focus.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_resets_completion_and_focus_state() {
    local(async {
        let mut t = Term::build(
            json!([{ "text": "editor reply" }]),
            json!({ "editor": "sh -c 'printf \"edited prompt\\n\" > \"$1\"' _" }),
            json!({}),
            80,
            20,
            |env| {
                std::fs::write(env.work.path().join("alpha.txt"), "hello").unwrap();
            },
        )
        .await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        // Establish a known focused state before terminal handoff.
        t.vt.focus(true);
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await, Some(true));

        // Open `@` file completion popup.
        t.vt.type_str("@");
        wait_screen(&mut t, "file completion popup", |s| s.contains("alpha.txt")).await;

        // Hand off to external editor; it writes "edited prompt".
        t.ctrl('g');
        wait_vt(&mut t, "external editor applied text", |t| t.vt.editor_text() == "edited prompt").await;

        assert_eq!(t.vt.editor_text(), "edited prompt");
        assert_eq!(
            eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await,
            None,
            "terminal handoff disables focus reporting, so focus must become unknown until the next report"
        );
        assert!(!t.screen().contains("alpha.txt"), "stale completion popup should close after Ctrl+G:\n{}", t.screen());

        // Enter must submit "edited prompt" directly instead of accepting a stale completion entry.
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        assert!(t.history().contains("edited prompt"), "submitted text missing from history:\n{}", t.history());
        assert!(t.history().contains("editor reply"), "model reply missing from history:\n{}", t.history());
    })
    .await
}

/// `Ctrl+T` transcript search matches must stay fresh across appended output, resizes, and clears.
#[tokio::test]
async fn transcript_search_refreshes_on_append_resize_and_clear() {
    local(async {
        let mut t = Term::new(json!([]), 80, 14).await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        exec(
            &t.vt.host,
            r#"
            for i = 1, 20 do
                if i == 2 or i == 18 then
                    mare.ui.print("row " .. i .. " " .. string.rep("padding ", 6) .. "MARKER")
                else
                    mare.ui.print("row " .. i .. " " .. string.rep("filler ", 7))
                end
            end
            "#,
        )
        .await;
        t.step().await;

        t.ctrl('t');
        wait_screen(&mut t, "transcript opened", |s| s.contains("row 18")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("MARKER");
        t.key(KeyCode::Enter, Modifiers::NONE);
        wait_screen(&mut t, "initial two matches", |s| s.contains("/MARKER") && s.contains(" of 2")).await;

        // Appending a new matching line while transcript search is active must refresh match count.
        exec(&t.vt.host, r#"mare.ui.print("row 21 newly appended MARKER")"#).await;
        t.step().await;
        assert!(t.screen().contains(" of 3"), "match count must grow to 3 after append:\n{}", t.screen());

        // Narrowing the viewport re-wraps preceding lines and shifts line numbers; `n` must still jump to a match.
        t.resize(30, 14).await;
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.step().await;
        assert!(
            t.screen().contains("MARKER"),
            "jumping to next match after resize must land on a line containing MARKER:\n{}",
            t.screen()
        );

        // Clearing transcript must drop stale cached matches.
        exec(&t.vt.host, "mare.ui.clear()").await;
        t.step().await;
        assert!(t.screen().contains("/MARKER  no matches"), "clear must invalidate search matches:\n{}", t.screen());
    })
    .await
}

/// An attached TUI owns viewport dimensions and wakes when queue state changes from Lua.
#[tokio::test]
async fn attached_tui_guards_viewport_and_wakes_on_queue_state() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        t.vt.wait_startup_settled().await;
        let out = t.vt.output();
        t.parser.process(&out);

        // Attached TUI must reject Lua viewport overrides that would diverge from the renderer width.
        let changed: bool = eval(&t.vt.host, "return mare.ui.set_viewport(120, 40)").await;
        let width: u16 = eval(&t.vt.host, "return mare.ui.width()").await;
        assert!(!changed, "attached TUI must ignore set_viewport");
        assert_eq!(width, 80, "shared viewport width must remain synchronized with the attached TUI");

        // Changing queue state from Lua while idle must wake the production event loop and redraw.
        tokio::time::pause();
        t.vt.reset_loop_state();
        let _ = t.vt.output();
        exec(&t.vt.host, "mare.ui.set_queued(2); mare.ui.set_queue_mode(1)").await;
        let parser = &mut t.parser;
        let woke = t
            .owned
            .vt
            .run_production_loop_until(Duration::from_millis(200), |vt| {
                let bytes = vt.output();
                if !bytes.is_empty() {
                    parser.process(&bytes);
                }
                parser.screen().contents().contains("edit")
            })
            .await;
        assert!(
            woke,
            "set_queued / set_queue_mode must wake the idle event loop and render queue hints:\n{}",
            t.screen()
        );
    })
    .await
}

/// `UiCmd::Flag("show_reasoning", ...)` must reflow committed scrollback and never leak across instances.
#[tokio::test]
async fn show_reasoning_flag_reflows_scrollback_and_stays_instance_local() {
    local(async {
        let mut first = Term::new(json!([]), 80, 20).await;
        first.vt.wait_startup_settled().await;
        let out = first.vt.output();
        first.parser.process(&out);

        exec(
            &first.vt.host,
            r#"
            local id = mare.ui.push("reasoning", {
                node = "compact reasoning summary",
                expanded = "SECRET_REASONING_BODY",
            })
            mare.ui.finish(id)
            "#,
        )
        .await;
        wait_vt(&mut first, "reasoning block committed", |t| t.history().contains("compact reasoning summary")).await;
        assert!(
            !first.history().contains("SECRET_REASONING_BODY"),
            "reasoning body should start collapsed:\n{}",
            first.history()
        );

        // Toggling show_reasoning must schedule a reflow rebuild without needing a resize.
        tokio::time::pause();
        first.vt.reset_loop_state();
        exec(&first.vt.host, r#"mare.ui.flag("show_reasoning", true)"#).await;
        first.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
        let out = first.vt.output();
        first.parser.process(&out);
        assert!(
            first.history().contains("SECRET_REASONING_BODY"),
            "toggling show_reasoning must rebuild committed scrollback without a resize:\n{}",
            first.history()
        );
        tokio::time::resume();
    })
    .await;

    // A second independent TUI instance in the same process must start with show_reasoning = false.
    local(async {
        let mut second = Term::new(json!([]), 80, 20).await;
        wait_vt(&mut second, "second instance startup", |t| !t.vt.starting()).await;
        exec(
            &second.vt.host,
            r#"
            local id = mare.ui.push("reasoning", {
                node = "second compact summary",
                expanded = "LEAKED_REASONING_BODY",
            })
            mare.ui.finish(id)
            "#,
        )
        .await;
        wait_vt(&mut second, "second reasoning block committed", |t| t.history().contains("second compact summary"))
            .await;
        assert!(
            !second.history().contains("LEAKED_REASONING_BODY"),
            "show_reasoning must be per-App, not process-global:\n{}",
            second.history()
        );
    })
    .await
}

/// Runtime mutations to Lua UI component tables must invalidate component caches without a theme change.
#[tokio::test]
async fn component_caches_invalidate_on_runtime_lua_mutation() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        // Populate KeyHints cache via `?` help modal, then close it.
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        wait_screen(&mut t, "key menu opened", |s| s.contains("─ Keys ─")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        wait_screen(&mut t, "key menu closed", |s| !s.contains("─ Keys ─")).await;

        // Mutate KeyHints and EditorChrome in place with no theme/style change.
        exec(
            &t.vt.host,
            r#"
            mare.ui.KeyHints.sep = " :: "
            function mare.ui.EditorChrome:render(props)
                return { prompt = "CUSTOM> ", placeholder = props.placeholder, border = props.border }
            end
            "#,
        )
        .await;
        t.step().await;
        assert!(
            t.screen().contains("CUSTOM>"),
            "EditorChrome cache must invalidate on runtime Lua mutation:\n{}",
            t.screen()
        );

        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.step().await;
        assert!(
            t.screen().contains("─ Keys ─") && t.screen().contains(" :: "),
            "KeyHints cache must invalidate on runtime Lua mutation:\n{}",
            t.screen()
        );
    })
    .await
}

/// New keymap bindings set from Lua must take effect immediately inside modals and the transcript overlay.
#[tokio::test]
async fn keymap_mutation_applies_immediately_in_modal_and_transcript() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        // Modal context: open an info overlay with a custom subcontext.
        exec(
            &t.vt.host,
            r#"
            mare.keymap.context("info.custom", { parent = "info" })
            mare.ui.overlay("CustomInfo", "overlay body", { context = "info.custom" })
            "#,
        )
        .await;
        wait_screen(&mut t, "custom info overlay", |s| s.contains("CustomInfo")).await;

        // Mutate the keymap and send the key immediately BEFORE draining UiCmds.
        exec(
            &t.vt.host,
            r#"
            _G.__modal_fired = false
            mare.keymap.set("info.custom", "b", function() _G.__modal_fired = true end, { desc = "modal test" })
            "#,
        )
        .await;
        t.key(KeyCode::Char('b'), Modifiers::NONE);
        t.step().await;
        assert!(
            eval::<bool>(&t.vt.host, "return _G.__modal_fired == true").await,
            "keymap binding added while modal is open must fire on the next keypress without waiting for ui_rx"
        );

        // Close modal and test the same transition in the Ctrl+T transcript overlay.
        t.key(KeyCode::Escape, Modifiers::NONE);
        wait_screen(&mut t, "modal closed", |s| !s.contains("CustomInfo")).await;
        t.ctrl('t');
        wait_screen(&mut t, "transcript opened", |s| s.contains("Transcript")).await;

        exec(
            &t.vt.host,
            r#"
            _G.__transcript_fired = false
            mare.keymap.set("transcript", "x", function() _G.__transcript_fired = true end, { desc = "transcript test" })
            "#,
        )
        .await;
        t.key(KeyCode::Char('x'), Modifiers::NONE);
        t.step().await;
        assert!(
            eval::<bool>(&t.vt.host, "return _G.__transcript_fired == true").await,
            "keymap binding added while transcript is open must fire on the next keypress without waiting for ui_rx"
        );
    })
    .await
}

/// Armed quit hint must not reappear after a modal outlives the 2-second quit confirmation window.
#[tokio::test]
async fn quit_hint_clears_when_modal_outlives_quit_window() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        t.vt.wait_startup_settled().await;
        let out = t.vt.output();
        t.parser.process(&out);

        tokio::time::pause();
        t.vt.reset_loop_state();

        // Arm quit on empty input.
        t.ctrl('c');
        t.step().await;
        assert!(t.screen().contains("Ctrl+C again to exit"), "expected quit hint:\n{}", t.screen());

        // Open a modal, advance beyond the 2-second quit window, then close the modal.
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("─ Keys ─"), "expected key menu:\n{}", t.screen());

        tokio::time::advance(Duration::from_secs(3)).await;

        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        assert!(
            !t.screen().contains("Ctrl+C again to exit"),
            "expired quit hint must not stay visible after modal dismissal:\n{}",
            t.screen()
        );
    })
    .await
}

/// Recalling a prompt with `Ctrl+R` must close any open completion popup so `Enter` submits cleanly.
#[tokio::test]
async fn history_search_closes_stale_completion_popup() {
    local(async {
        let mut t = Term::build(
            json!([{ "text": "first reply" }, { "text": "recalled reply" }]),
            json!({}),
            json!({}),
            80,
            20,
            |env| {
                std::fs::write(env.work.path().join("alpha.txt"), "hello").unwrap();
            },
        )
        .await;

        // Seed prompt history with one prior submission.
        t.submit("first prompt").await;
        t.idle().await;

        // Open `@` file completion popup.
        t.vt.type_str("@");
        wait_screen(&mut t, "file completion popup", |s| s.contains("alpha.txt")).await;

        // Recall prior prompt via Ctrl+R history picker.
        t.ctrl('r');
        wait_screen(&mut t, "history picker opened", |s| s.contains("Search history") && s.contains("first prompt"))
            .await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        wait_vt(&mut t, "recalled text in editor", |t| t.vt.editor_text() == "first prompt").await;

        assert!(
            !t.screen().contains("alpha.txt"),
            "stale completion popup must close when Ctrl+R replaces editor text:\n{}",
            t.screen()
        );

        // Pressing Enter must submit "first prompt", not accept a stale completion item.
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        assert!(
            t.history().contains("recalled reply"),
            "Enter after Ctrl+R must submit the recalled prompt:\n{}",
            t.history()
        );
    })
    .await
}

/// External editor (`Ctrl+G`) must run asynchronously so Lua tasks and host events keep progressing.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_does_not_block_runtime_while_open() {
    local(async {
        let env = TestEnv::new().await;
        let started = env.work.path().join("editor-started");
        let progressed = env.work.path().join("lua-progressed");
        let script_path = env.work.path().join("fake-editor.sh");
        let script = format!(
            "#!/bin/sh\n: > '{}'\ni=0\nwhile [ ! -f '{}' ] && [ $i -lt 50 ]; do\n  sleep 0.03\n  i=$((i + 1))\ndone\nif [ -f '{}' ]; then\n  printf 'async-ok\\n' > \"$1\"\nelse\n  printf 'blocked-timeout\\n' > \"$1\"\nfi\n",
            started.display(),
            progressed.display(),
            progressed.display(),
        );
        std::fs::write(&script_path, script).unwrap();
        let editor_cmd = format!("sh '{}'", script_path.display());
        let mut t = Term::in_env(env, json!([]), json!({ "editor": editor_cmd }), json!({}), 80, 20).await;
        t.vt.wait_startup_settled().await;
        let out = t.vt.output();
        t.parser.process(&out);

        // Background Lua task signals `progressed` once it observes `started`.
        exec(
            &t.vt.host,
            &format!(
                r#"
                mare.task.spawn(function()
                    for _ = 1, 100 do
                        local f = io.open({:?}, "r")
                        if f then
                            f:close()
                            local out = assert(io.open({:?}, "w"))
                            out:write("ok")
                            out:close()
                            return
                        end
                        mare.task.sleep(20)
                    end
                end)
                "#,
                started.to_string_lossy(),
                progressed.to_string_lossy(),
            ),
        )
        .await;

        t.ctrl('g');
        let finished = t
            .owned
            .vt
            .run_production_loop_until(Duration::from_secs(3), |vt| !vt.editor_text().is_empty())
            .await;
        assert!(finished, "async external editor never completed");
        assert_eq!(
            t.vt.editor_text(),
            "async-ok",
            "Lua background task must keep running while external editor is open"
        );
    })
    .await
}

/// Height-only resize invalidates `Renderer.top`; an in-band CPR (`CSI 6n`) must re-anchor it.
#[tokio::test]
async fn renderer_top_reanchors_after_height_only_resize() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        t.vt.wait_startup_settled().await;
        // Settle any pending startup theme reflow at 80x20 before testing a pure height-only resize.
        t.vt.resize(80, 20);
        let out = t.vt.output();
        t.parser.process(&out);

        // Anchor the live area at 1-based row 15 (simulating completed startup CPR probe).
        t.vt.anchor(15);
        assert!(t.vt.scroll_insert_ready(), "anchored virtual terminal must have DECSTBM fast path enabled");

        // Height-only resize invalidates `top`.
        t.parser.screen_mut().set_size(nz(24), nz(80));
        t.vt.resize(80, 24);
        assert!(!t.vt.scroll_insert_ready(), "height-only resize clears Renderer.top");
        assert!(
            !eval::<bool>(&t.vt.host, "return mare.ui.caps().position_known == true").await,
            "caps().position_known must clear when Renderer.top is invalidated"
        );
        let _ = t.vt.output();

        // Drawing after invalidation must request an in-band CPR (`CSI 6n`).
        t.vt.frame().await;
        let bytes = t.vt.output();
        t.parser.process(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("\x1b[6n"), "expected in-band CPR query (CSI 6n) after top invalidation, got: {text:?}");

        // Delivering the CPR reply must re-anchor `Renderer.top` and update `caps().position_known`.
        t.vt.send_cpr(15, 1);
        assert!(t.vt.scroll_insert_ready(), "CPR reply must re-enable DECSTBM scroll-region fast path");
        assert!(
            eval::<bool>(&t.vt.host, "return mare.ui.caps().position_known == true").await,
            "caps().position_known must reflect the re-anchored state"
        );
    })
    .await
}

/// Reflow occurring while `ui/commit-before` holds a block must not commit that block twice.
#[tokio::test]
async fn reflow_during_held_commit_before_does_not_duplicate_block() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        exec(
            &t.vt.host,
            r#"
            _G.__commit_before_done = false
            mare.on("ui/commit-before", function(ev)
                mare.task.sleep(100)
                _G.__commit_before_done = true
                return ev
            end)
            local id = mare.ui.push("notice", { node = "UNIQUE_HELD_BLOCK_TOKEN" })
            mare.ui.finish(id)
            "#,
        )
        .await;
        // First frame moves the block into `held` awaiting `ui/commit-before`.
        t.step().await;

        // Trigger a reflow while the commit is still held in flight.
        t.resize(60, 20).await;

        // Wait until `ui/commit-before` resolves and the held commit drains into scrollback.
        let deadline = Instant::now() + TRANSITION_WAIT;
        while !eval::<bool>(&t.vt.host, "return _G.__commit_before_done == true").await {
            assert!(Instant::now() < deadline, "timed out waiting for ui/commit-before");
            t.step().await;
            tokio::task::yield_now().await;
        }
        t.step().await;
        t.step().await;

        let hist = t.history();
        let count = hist.matches("UNIQUE_HELD_BLOCK_TOKEN").count();
        assert_eq!(count, 1, "held commit must appear exactly once after reflow, found {count}:\n{hist}");
    })
    .await
}

/// A CPR reply queried before a commit frame shifts the live area down on a non-full screen
/// carries a stale cursor row and must be discarded rather than anchoring `Renderer.top` too high.
#[tokio::test]
async fn stale_cpr_after_commit_frame_is_rejected_on_non_full_screen() {
    local(async {
        let mut t = Term::new(json!([]), 80, 40).await;
        t.vt.wait_startup_settled().await;
        t.vt.resize(80, 40);
        let out = t.vt.output();
        t.parser.process(&out);

        // Seed committed lines so the live area sits part-way down a non-full screen.
        exec(
            &t.vt.host,
            r#"
            for i = 1, 4 do
                local id = mare.ui.push("notice", { node = "PRE_COMMIT_" .. i })
                mare.ui.finish(id)
            end
            "#,
        )
        .await;
        t.until("pre-commit lines appear", |s| s.contains("PRE_COMMIT_4")).await;

        // Height-only resize clears Renderer.top while keeping the screen non-full (50 rows).
        t.parser.screen_mut().set_size(nz(50), nz(80));
        t.vt.resize(80, 50);
        assert!(!t.vt.scroll_insert_ready(), "height-only resize clears Renderer.top");
        let _ = t.vt.output();

        // Drawing emits in-band CPR (`CSI 6n`); record the terminal cursor row at query time.
        t.vt.frame().await;
        let bytes = t.vt.output();
        t.parser.process(&bytes);
        assert!(
            String::from_utf8_lossy(&bytes).contains("\x1b[6n"),
            "expected in-band CPR query after height-only resize"
        );
        let stale_cpr_row = t.parser.screen().cursor_position().0 as usize + 1;

        // Before the CPR reply returns, commit 3 lines via the fallback path, shifting the live area down.
        exec(
            &t.vt.host,
            r#"
            for i = 1, 3 do
                local id = mare.ui.push("notice", { node = "MID_COMMIT_" .. i })
                mare.ui.finish(id)
            end
            "#,
        )
        .await;
        t.until("mid-commit lines appear", |s| s.contains("MID_COMMIT_3")).await;
        let actual_row_after_commit = t.parser.screen().cursor_position().0 as usize + 1;
        assert!(
            actual_row_after_commit > stale_cpr_row,
            "non-full-screen commit should shift cursor down (before={stale_cpr_row}, after={actual_row_after_commit})"
        );

        // Delivering the stale pre-commit CPR reply must NOT anchor Renderer.top, and should
        // trigger a fresh CPR query so the current row can anchor cleanly.
        let _ = t.vt.output();
        t.vt.send_cpr(stale_cpr_row as u16, 1);
        assert!(
            !t.vt.scroll_insert_ready(),
            "stale CPR reply from before commit frame must be discarded (stale_row={stale_cpr_row}, actual={actual_row_after_commit})"
        );
        let requery = t.vt.output();
        assert!(
            String::from_utf8_lossy(&requery).contains("\x1b[6n"),
            "discarding stale CPR reply should re-query current cursor position"
        );
        t.vt.send_cpr(actual_row_after_commit as u16, 1);
        assert!(
            t.vt.scroll_insert_ready(),
            "fresh CPR reply at current post-commit row should anchor Renderer.top"
        );

        // A subsequent commit must not overwrite the mid-commit lines.
        exec(
            &t.vt.host,
            r#"
            local id = mare.ui.push("notice", { node = "AFTER_STALE_CPR" })
            mare.ui.finish(id)
            "#,
        )
        .await;
        t.until("post-stale commit appears", |s| s.contains("AFTER_STALE_CPR")).await;
        let full = format!("{}\n{}", t.history(), t.screen());
        for tok in ["PRE_COMMIT_1", "MID_COMMIT_1", "MID_COMMIT_2", "MID_COMMIT_3", "AFTER_STALE_CPR"] {
            assert!(full.contains(tok), "missing {tok} after stale CPR:\n{full}");
        }
    })
    .await
}

/// Split in-band OSC 11 (ending in BEL / Ctrl+G) and split CPR (`ESC[1;5R`) arriving across
/// separate reads in the production event loop must route to terminal replies rather than leaking
/// into the editor, triggering Ctrl+G, or decoding as F3.
#[tokio::test]
async fn split_inband_osc11_and_cpr_replies_route_cleanly_in_event_loop() {
    local(async {
        let mut t = Term::new(json!([]), 80, 40).await;
        t.vt.wait_startup_settled().await;
        t.vt.resize(80, 40);
        let out = t.vt.output();
        t.parser.process(&out);

        // Seed committed lines so height-only resize clears Renderer.top and emits a CPR query.
        exec(
            &t.vt.host,
            r#"
            for i = 1, 4 do
                local id = mare.ui.push("notice", { node = "SPLIT_CPR_SEED_" .. i })
                mare.ui.finish(id)
            end
            "#,
        )
        .await;
        t.until("seed lines appear", |s| s.contains("SPLIT_CPR_SEED_4")).await;

        t.parser.screen_mut().set_size(nz(50), nz(80));
        t.vt.resize(80, 50);
        assert!(!t.vt.scroll_insert_ready());
        t.vt.frame().await;
        let _ = t.vt.output();

        tokio::time::pause();
        t.vt.reset_loop_state();
        let t0 = tokio::time::Instant::now().into_std();

        // Feed split OSC 11 ending with BEL (which termina reports as Ctrl+G).
        t.vt.send_tty_event_at(Event::Key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE)), t0);
        for (i, c) in "]11;rgb:1c1c/1c1c/1c1c".chars().enumerate() {
            t.vt.send_tty_event_at(
                Event::Key(KeyEvent::new(KeyCode::Char(c), Modifiers::NONE)),
                t0 + Duration::from_millis((i as u64) + 1),
            );
        }
        t.vt.send_tty_event_at(
            Event::Key(KeyEvent::new(KeyCode::Char('g'), Modifiers::CONTROL)),
            t0 + Duration::from_millis(25),
        );

        // Feed split CPR on row 1 (`ESC[1;5R`).
        t.vt.send_tty_event_at(
            Event::Key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE)),
            t0 + Duration::from_millis(40),
        );
        for (i, c) in "[1;5R".chars().enumerate() {
            t.vt.send_tty_event_at(
                Event::Key(KeyEvent::new(KeyCode::Char(c), Modifiers::NONE)),
                t0 + Duration::from_millis(41 + i as u64),
            );
        }

        t.vt.run_production_loop_for(Duration::from_millis(120)).await;
        assert!(
            t.vt.editor_text().is_empty(),
            "split OSC 11 / CPR must never leak characters into editor, got: {:?}",
            t.vt.editor_text()
        );
        assert!(t.vt.scroll_insert_ready(), "split CPR `ESC[1;5R` must anchor renderer.top instead of decoding as F3");
        tokio::time::resume();
    })
    .await
}

/// `Modal::Select.title_right` must resolve theme colors, and pressing Enter to close a
/// non-approval modal must record activity so a queued approval waits the 1s typing cooldown.
#[tokio::test]
async fn select_title_right_resolves_theme_and_modal_enter_delays_queued_approval() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        t.vt.wait_startup_settled().await;
        let _ = t.vt.output();

        // 1) `title_right` with a theme color token ("accent") must resolve to an ANSI color escape.
        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
                mare.ui.select({
                    title = "Sessions",
                    title_right = mare.ui.Line({ mare.ui.Span("Sort: Updated", { fg = "accent" }) }),
                    items = { { label = "One" } },
                })
            end)
            "#,
        )
        .await;
        let mut saw_styled_title_right = false;
        for _ in 0..30 {
            t.vt.drain_cmds();
            t.vt.frame().await;
            let bytes = t.vt.output();
            t.parser.process(&bytes);
            let raw = String::from_utf8_lossy(&bytes);
            if raw.contains("Sort: Updated") {
                // Resolved theme color emits an SGR foreground sequence (38;2;... / 38;5;... /
                // 30..37 / 90..97 under Ansi16) directly before `Sort: Updated` rather than
                // leaving `Color::Theme` unresolved.
                let prefix = raw.split("Sort: Updated").next().unwrap_or("");
                if prefix.rsplit('\x1b').next().is_some_and(|sgr| {
                    let Some(params) = sgr.strip_prefix('[').and_then(|s| s.split('m').next()) else {
                        return false;
                    };
                    params.split(';').any(|code| {
                        code == "38"
                            || code.parse::<u16>().is_ok_and(|n| (30..=37).contains(&n) || (90..=97).contains(&n))
                    })
                }) {
                    saw_styled_title_right = true;
                }
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            saw_styled_title_right,
            "select modal title_right did not resolve theme color; screen:\n{}",
            t.screen()
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        assert!(!t.vt.has_modal(), "select modal should close on Esc");

        // 2) Confirming a non-approval modal with Enter must record activity so a queued
        //    approval prompt waits the 1s cooldown before appearing.
        tokio::time::pause();
        t.vt.reset_loop_state();
        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
                mare.ui.input({ title = "PromptName" })
            end)
            "#,
        )
        .await;
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        assert!(t.vt.has_modal(), "input modal should be open");

        // Queue an approval modal behind the active input modal.
        exec(
            &t.vt.host,
            r#"
            local binding = { owner = "test", turn = "t1", request = "modal-cd-1", snapshot = "snap-modal-cd-1" }
            mare.approvals.register_snapshot(binding, {
                call = { name = "bash", arguments = { command = "echo queued" }, protected_arguments = false },
                info = { unconfined = false },
                verdict = { amendment_description = "allow echo queued" },
            })
            mare.task.spawn(function()
                mare.ui.approve({
                    approval_binding = binding,
                    approval_arguments = { command = "echo queued" },
                    tool = "bash",
                    choices = { "allow", "deny" },
                })
            end)
            "#,
        )
        .await;
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;

        // Press Enter to submit/close the input modal.
        let now = tokio::time::Instant::now().into_std();
        t.vt.send_tty_event_at(Event::Key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE)), now);
        t.vt.run_production_loop_for(Duration::from_millis(300)).await;
        assert!(!t.vt.has_modal(), "queued approval must wait the 1s cooldown after closing a modal with Enter");

        // Once the 1s cooldown passes, the queued approval must appear.
        t.vt.run_production_loop_for(Duration::from_millis(900)).await;
        assert!(t.vt.has_modal(), "queued approval must appear after the 1s cooldown");
        let out = t.vt.output();
        t.parser.process(&out);
        assert!(t.screen().contains("echo queued"), "expected queued approval on screen:\n{}", t.screen());
        tokio::time::resume();
    })
    .await
}

/// While the terminal is handed off to an external program, `UiCmd::Clear`, `UiCmd::Title`, and
/// `UiCmd::NotifyOs` (TTY OSC path) must not write to the tty; Clear and Title flush on reacquire.
#[tokio::test]
async fn handoff_defers_clear_and_title_and_suppresses_tty_notify_os() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        t.vt.wait_startup_settled().await;
        let _ = t.vt.output();

        mare_tui::modes::enter();
        mare_tui::modes::leave();
        let _ = t.vt.output();

        exec(
            &t.vt.host,
            r#"
            mare.ui.set_title("AfterEditor")
            mare.ui.notify_os("bg done", { protocol = "osc9" })
            mare.ui.clear()
            "#,
        )
        .await;
        t.vt.drain_cmds();
        let during_handoff = t.vt.output();
        assert!(
            during_handoff.is_empty(),
            "no tty bytes may be written during handoff, got: {:?}",
            String::from_utf8_lossy(&during_handoff)
        );

        mare_tui::modes::resume();
        let _ = t.vt.output();
        t.vt.reacquire_terminal();
        let after_reacquire = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(
            after_reacquire.contains("\x1b]2;AfterEditor\x07"),
            "deferred title must flush on reacquire: {after_reacquire:?}"
        );
        assert!(
            after_reacquire.contains("\x1b[H\x1b[J"),
            "deferred clear must flush on reacquire: {after_reacquire:?}"
        );
        mare_tui::modes::leave();
    })
    .await
}

/// Reordering `[Image #1]` and `[Image #2]` in an external editor must preserve both attachments
/// instead of dropping the second attachment when its placeholder moves earlier in the text.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_swapped_placeholders_preserve_both_attachments() {
    const PNG: [u8; 67] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00,
        0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d,
        0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    local(async {
        let mut t = Term::build(
            json!([{ "text": "saw both" }]),
            json!({ "editor": "sh -c 'printf \"[Image #2] before [Image #1]\\n\" > \"$1\"' _" }),
            json!({}),
            80,
            24,
            |env| {
                std::fs::write(env.work.path().join("first.png"), PNG).unwrap();
                std::fs::write(env.work.path().join("second.png"), PNG).unwrap();
            },
        )
        .await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        let cwd = t._env.cwd.clone();
        t.vt.paste(&format!("{} {} ", cwd.join("first.png").display(), cwd.join("second.png").display()));
        assert_eq!(t.vt.editor_text(), "[Image #1] [Image #2]");

        t.ctrl('g');
        wait_vt(&mut t, "swapped placeholders restored", |t| t.vt.editor_text() == "[Image #2] before [Image #1]").await;

        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        global(&t.vt.host, "AG", t.vt.host.service_table("agent").unwrap());
        let img_count: i64 = eval(
            &t.vt.host,
            "local n = 0; for _, m in ipairs(AG:list_messages().data) do for _, b in ipairs(type(m.content) == 'table' and m.content or {}) do if b.type == 'image' then n = n + 1 end end end; return n",
        )
        .await;
        assert_eq!(img_count, 2, "both swapped image attachments must be submitted");
    })
    .await
}

/// Draft mutations (such as restoring a rejected prompt with `prepend = true`) arriving while an
/// async external editor is open must be queued and applied after the editor result instead of
/// being overwritten when the editor exits.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_preserves_prepended_draft_arriving_while_open() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let release = dir.path().join("release");
        let script = dir.path().join("editor.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.02; done\nprintf 'edited draft\\n' > \"$1\"\n",
                ready.display(),
                release.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut t =
            Term::build(json!([]), json!({ "editor": script.to_string_lossy().as_ref() }), json!({}), 80, 20, |_| {})
                .await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        t.ctrl('g');
        wait_vt(&mut t, "editor process started", |_| ready.exists()).await;

        // While the external editor is open, prepend a rejected submission to the draft.
        exec(&t.vt.host, r#"mare.ui.set_editor("rejected submission", { prepend = true })"#).await;
        t.vt.drain_cmds();

        // Release the editor so it writes "edited draft" and exits.
        std::fs::write(&release, "go").unwrap();
        wait_vt(&mut t, "editor exited and replayed prepend", |t| {
            t.vt.editor_text() == "rejected submission\n\nedited draft"
        })
        .await;
    })
    .await
}

/// A pre-read `Enter` sitting in the same input batch right after `Ctrl+G` must be cleared when
/// pausing input for external-editor handoff so it cannot submit the edited draft on return.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_ctrl_g_plus_enter_batch_does_not_submit_edited_draft() {
    local(async {
        let mut t = Term::build(
            json!([{ "text": "UNEXPECTED_SUBMIT" }]),
            json!({ "editor": "sh -c 'printf \"edited-not-submitted\\n\" > \"$1\"' _" }),
            json!({}),
            80,
            20,
            |_| {},
        )
        .await;
        t.vt.wait_startup_settled().await;
        t.vt.reset_loop_state();

        let now = mare_tui::render::clock_now();
        t.vt.send_tty_event_at(Event::Key(KeyEvent::new(KeyCode::Char('g'), Modifiers::CONTROL)), now);
        t.vt.send_tty_event_at(Event::Key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE)), now);

        let edited =
            t.vt.run_production_loop_until(Duration::from_secs(5), |vt| vt.editor_text() == "edited-not-submitted")
                .await;
        assert!(edited, "external editor must finish and populate the draft");
        // Run a bit further to ensure no stale Enter replays and submits the draft.
        t.vt.run_production_loop_for(Duration::from_millis(150)).await;
        assert_eq!(t.vt.editor_text(), "edited-not-submitted", "stale Enter must not submit the edited draft");
        assert!(!t.vt.busy(), "no model turn should start from a pre-handoff Enter");
    })
    .await
}

/// An in-band `Event::cursor_position_report` arriving through the production `step_loop` must be
/// consumed once via `handle_cpr_reply`: a stale reply after a commit frame must NOT fall through
/// to `apply_inband_escape` and unconditionally re-anchor `Renderer.top`.
#[tokio::test]
async fn stale_inband_cpr_through_step_loop_is_not_reanchored_by_probe_fallback() {
    local(async {
        let mut t = Term::new(json!([]), 80, 40).await;
        t.vt.wait_startup_settled().await;
        t.vt.resize(80, 40);
        let out = t.vt.output();
        t.parser.process(&out);

        exec(
            &t.vt.host,
            r#"
            for i = 1, 4 do
                local id = mare.ui.push("notice", { node = "STEP_CPR_PRE_" .. i })
                mare.ui.finish(id)
            end
            "#,
        )
        .await;
        t.until("pre-commit lines appear", |s| s.contains("STEP_CPR_PRE_4")).await;

        t.parser.screen_mut().set_size(nz(50), nz(80));
        t.vt.resize(80, 50);
        assert!(!t.vt.scroll_insert_ready());
        let _ = t.vt.output();
        t.vt.frame().await;
        let bytes = t.vt.output();
        t.parser.process(&bytes);
        let stale_row = t.parser.screen().cursor_position().0 as u16 + 1;

        // Commit 3 lines before the stale CPR arrives, advancing position_epoch.
        exec(
            &t.vt.host,
            r#"
            for i = 1, 3 do
                local id = mare.ui.push("notice", { node = "STEP_CPR_MID_" .. i })
                mare.ui.finish(id)
            end
            "#,
        )
        .await;
        t.until("mid-commit lines appear", |s| s.contains("STEP_CPR_MID_3")).await;
        let actual_row = t.parser.screen().cursor_position().0 as u16 + 1;
        assert!(actual_row > stale_row);

        tokio::time::pause();
        t.vt.reset_loop_state();
        let _ = t.vt.output();
        let t0 = tokio::time::Instant::now().into_std();
        // Deliver stale CPR through production `step_loop` (Event::Termina Csi APR).
        t.vt.send_tty_event_at(Event::cursor_position_report(stale_row, 1), t0);
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        assert!(
            !t.vt.scroll_insert_ready(),
            "stale in-band CPR routed through step_loop must be rejected rather than anchored by probe fallback"
        );
        let requery = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(requery.contains("\x1b[6n"), "rejecting stale CPR in step_loop must emit fresh CSI 6n: {requery:?}");

        // Deliver fresh CPR at the current row; it must anchor cleanly.
        t.vt.send_tty_event_at(Event::cursor_position_report(actual_row, 1), t0 + Duration::from_millis(60));
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        assert!(t.vt.scroll_insert_ready(), "fresh in-band CPR must anchor renderer.top");
        tokio::time::resume();
    })
    .await
}

/// Opening the transcript overlay or removing a held image block must clear the active image-hold
/// deadline so the production event loop never busy-spins on an expired timer.
#[tokio::test]
async fn image_hold_clears_deadline_on_transcript_open_and_block_remove() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.dismiss_notices();
        mare_tui::image::set_protocol(mare_tui::image::Protocol::Kitty);
        mare_tui::image::set_cell_px(2, 2);

        let (resolver, mut requests) = mare_shared::image_resource::Resolver::register();
        let rid = resolver.id();
        exec(
            &t.vt.host,
            &format!(
                r#"
                HELD_ID = mare.ui.push("assistant", {{
                    node = {{
                        type = "image",
                        resource = {{ resolver_id = {rid}, key = "spin-pic", width = 16, height = 16 }},
                        max_cols = 40,
                        max_rows = 10,
                    }},
                }})
                mare.ui.finish(HELD_ID)
                "#
            ),
        )
        .await;

        tokio::time::pause();
        t.vt.reset_loop_state();
        t.vt.run_production_loop_for(Duration::from_millis(100)).await;
        assert!(t.vt.image_hold_deadline().is_some(), "held image block should arm image_hold deadline");

        // Opening the transcript overlay suppresses the hold deadline so the loop does not spin.
        t.ctrl('t');
        t.vt.run_production_loop_for(Duration::from_millis(100)).await;
        assert!(t.vt.image_hold_deadline().is_none(), "transcript open must suppress image_hold deadline");

        // Closing transcript restores visibility of the held block's deadline.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.vt.run_production_loop_for(Duration::from_millis(100)).await;
        assert!(t.vt.image_hold_deadline().is_some(), "closing transcript restores active hold deadline");

        // Removing the held block clears the hold deadline completely.
        exec(&t.vt.host, "mare.ui.remove(HELD_ID)").await;
        t.vt.run_production_loop_for(Duration::from_millis(100)).await;
        assert!(t.vt.image_hold_deadline().is_none(), "removing held block must clear image_hold deadline");

        resolver.close();
        while let Ok(req) = requests.try_recv() {
            req.complete(Err("test ended".into()));
        }
        mare_tui::image::set_protocol(mare_tui::image::Protocol::None);
        mare_tui::image::set_cell_px(9, 18);
        tokio::time::resume();
    })
    .await
}

/// Copying via OSC 52 while the terminal is handed off must return an error instead of reporting
/// that the selection was sent to the terminal clipboard.
#[tokio::test]
async fn osc52_copy_during_handoff_reports_error_instead_of_success() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        t.vt.wait_startup_settled().await;
        let _ = t.vt.output();

        mare_tui::modes::enter();
        mare_tui::modes::leave();
        let _ = t.vt.output();

        // Directly verify the TUI OSC 52 hook reports HandedOff and writes zero bytes.
        assert_eq!(t.vt.ui().osc52("selection text"), Some(mare_shared::ui::Osc52Status::HandedOff));
        assert!(t.vt.output().is_empty(), "osc52 during handoff must write no tty bytes");

        mare_tui::modes::resume();
        let _ = t.vt.output();
        assert_eq!(t.vt.ui().osc52("selection text"), Some(mare_shared::ui::Osc52Status::Written));
        let bytes = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(bytes.contains("\x1b]52;c;"), "osc52 after resume must write sequence: {bytes:?}");
        mare_tui::modes::leave();
    })
    .await
}

/// Cell pixel dimensions measured on resize must update `probe_state.cell_px` so a subsequent
/// in-band probe reply (e.g. DA1) does not overwrite the new geometry with a stale startup value.
#[tokio::test]
async fn resize_cell_px_survives_subsequent_inband_probe_update() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.wait_startup_settled().await;
        mare_tui::image::set_cell_px(9, 18);

        // Resize with new pixel dimensions: 80x24 at 960x576 px -> 12x24 px per cell.
        t.vt.resize_with_pixels(80, 24, Some(960), Some(576));
        let before: Option<Vec<u64>> = eval(&t.vt.host, "return mare.ui.caps().cell_px").await;
        assert_eq!(before, Some(vec![12, 24]), "resize must update published caps.cell_px");

        // Now deliver an unrelated in-band probe reply (DA1) through step_loop.
        tokio::time::pause();
        t.vt.reset_loop_state();
        let t0 = tokio::time::Instant::now().into_std();
        // Feed split DA1 (`ESC [ ? 62 ; 4 c`) which triggers `apply_probe_update` -> `publish_caps`.
        t.vt.send_tty_event_at(Event::Key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE)), t0);
        for (i, c) in "[?62;4c".chars().enumerate() {
            t.vt.send_tty_event_at(
                Event::Key(KeyEvent::new(KeyCode::Char(c), Modifiers::NONE)),
                t0 + Duration::from_millis(i as u64 + 1),
            );
        }
        t.vt.run_production_loop_for(Duration::from_millis(100)).await;
        let after: Option<Vec<u64>> = eval(&t.vt.host, "return mare.ui.caps().cell_px").await;
        assert_eq!(after, Some(vec![12, 24]), "in-band probe update must not overwrite resized cell_px");

        mare_tui::image::set_cell_px(9, 18);
        tokio::time::resume();
    })
    .await
}

/// An `AppMsg::SetEditor` arriving while an async external editor is open must defer into
/// `pending_editor_cmds` and apply after the editor exits instead of being clobbered, and
/// `reacquire_terminal` while the transcript overlay is open must clear the overlay frame rather
/// than issuing a live-area clear over it.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_defers_app_set_editor_and_reacquire_clears_open_transcript() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let release = dir.path().join("release");
        let script = dir.path().join("editor.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.02; done\nprintf 'from editor\\n' > \"$1\"\n",
                ready.display(),
                release.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut t =
            Term::build(json!([]), json!({ "editor": script.to_string_lossy().as_ref() }), json!({}), 80, 20, |_| {})
                .await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        t.ctrl('g');
        wait_vt(&mut t, "editor process started", |_| ready.exists()).await;

        // Non-typing `AppMsg::SetEditor` (e.g. history recall) arrives mid-handoff.
        t.vt.send_set_editor_msg("deferred-history-entry");
        t.vt.drain_cmds();
        assert_ne!(
            t.vt.editor_text(),
            "deferred-history-entry",
            "AppMsg::SetEditor must be deferred while external editor owns the draft"
        );

        std::fs::write(&release, "go").unwrap();
        wait_vt(&mut t, "deferred AppMsg::SetEditor applied after editor exit", |t| {
            t.vt.editor_text() == "deferred-history-entry"
        })
        .await;

        // Returning from handoff while transcript overlay is open must clear the overlay frame
        // and suppress live-area `clear_screen` until the overlay closes.
        t.ctrl('t');
        t.vt.frame().await;
        let _ = t.vt.output();
        t.vt.reacquire_terminal();
        let after_reacquire = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(
            !after_reacquire.contains("\x1b[H\x1b[J"),
            "reacquire while transcript is open must not clear live area directly: {after_reacquire:?}"
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.vt.frame().await;
    })
    .await
}

/// Activating a select modal while mouse select mode is active must leave select mode so keys are
/// delivered to the modal rather than trapped in selection mode.
#[tokio::test]
async fn select_modal_leaves_select_mode_on_activation() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        t.vt.action("select-mode");
        t.step().await;
        assert!(t.vt.selecting(), "select-mode action must enable selection mode");

        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
                mare.ui.select({ title = "Pick one", items = { "Alpha" } })
            end)
            "#,
        )
        .await;
        wait_vt(&mut t, "select modal opens", |t| t.vt.has_modal()).await;
        assert!(!t.vt.selecting(), "opening a select modal must leave select mode");

        t.key(KeyCode::Escape, Modifiers::NONE);
        wait_vt(&mut t, "select modal closes", |t| !t.vt.has_modal()).await;
    })
    .await
}

/// Editing a draft containing an image attachment in an external editor (`Ctrl+G`) must preserve
/// the element's original ID and keep undo-reachable attachments intact so Undo after external
/// editing (whether the placeholder was kept or deleted) still submits the image payload.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_undo_preserves_image_attachments() {
    const PNG: [u8; 67] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00,
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00,
        0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d,
        0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let mode_file = dir.path().join("mode");
        let script = dir.path().join("editor.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$(cat '{}')\" = \"keep\" ]; then\n  printf 'after [Image #1]\\n' > \"$1\"\nelse\n  printf 'deleted placeholder\\n' > \"$1\"\nfi\n",
                mode_file.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut t = Term::build(
            json!([{ "text": "seen-1" }, { "text": "seen-2" }]),
            json!({ "editor": script.to_string_lossy().as_ref() }),
            json!({}),
            80,
            24,
            |_| {},
        )
        .await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        let pic = t._env.cwd.join("shot.png");
        std::fs::write(&pic, PNG).unwrap();
        let pic_path = pic.display().to_string();

        // Case 1: External editor keeps `[Image #1]` with edited prose; Undo then submit must
        // still attach the image (same element ID across set_text + undo snapshot).
        std::fs::write(&mode_file, "keep").unwrap();
        t.vt.type_str("before ");
        t.vt.paste(&pic_path);
        assert_eq!(t.vt.editor_text(), "before [Image #1]");

        t.ctrl('g');
        wait_vt(&mut t, "external editor kept placeholder", |t| t.vt.editor_text() == "after [Image #1]").await;

        t.vt.action("text.undo");
        assert_eq!(t.vt.editor_text(), "before [Image #1]", "undo must restore pre-external-editor draft");
        // sleep-ok: past the stray-Enter paste guard before submitting
        tokio::time::sleep(Duration::from_millis(200)).await;
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;

        global(&t.vt.host, "AG", t.vt.host.service_table("agent").unwrap());
        let img_count_1: i64 = eval(
            &t.vt.host,
            "local n = 0; for _, m in ipairs(AG:list_messages().data) do for _, b in ipairs(type(m.content) == 'table' and m.content or {}) do if b.type == 'image' then n = n + 1 end end end; return n",
        )
        .await;
        assert_eq!(img_count_1, 1, "undo after external editor edit must preserve image attachment");

        // Case 2: External editor removes `[Image #1]`; Undo must recover both placeholder and
        // attachment because cleanup retains undo-reachable element IDs.
        std::fs::write(&mode_file, "delete").unwrap();
        t.vt.type_str("again ");
        t.vt.paste(&pic_path);
        assert_eq!(t.vt.editor_text(), "again [Image #2]");

        t.ctrl('g');
        wait_vt(&mut t, "external editor removed placeholder", |t| t.vt.editor_text() == "deleted placeholder").await;

        t.vt.action("text.undo");
        assert_eq!(t.vt.editor_text(), "again [Image #2]", "undo must restore deleted image placeholder");
        // sleep-ok: past the stray-Enter paste guard before submitting
        tokio::time::sleep(Duration::from_millis(200)).await;
        t.ended = t.vt.turns_ended();
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;

        let img_count_2: i64 = eval(
            &t.vt.host,
            "local n = 0; for _, m in ipairs(AG:list_messages().data) do for _, b in ipairs(type(m.content) == 'table' and m.content or {}) do if b.type == 'image' then n = n + 1 end end end; return n",
        )
        .await;
        assert_eq!(img_count_2, 2, "undo after external editor deleted placeholder must still preserve image attachment");
    })
    .await
}

/// Synchronous Lua editor mutations (`mare.ui.set_editor`, `editor_insert`, etc.) while an
/// external editor owns the draft must return an explicit error rather than mutating the live
/// buffer only to be silently overwritten when the external editor exits.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_rejects_sync_lua_mutations_while_open() {
    local(async {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let release = dir.path().join("release");
        let script = dir.path().join("editor.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.02; done\nprintf 'from editor\\n' > \"$1\"\n",
                ready.display(),
                release.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut t =
            Term::build(json!([]), json!({ "editor": script.to_string_lossy().as_ref() }), json!({}), 80, 20, |_| {})
                .await;
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        t.vt.type_str("initial");
        t.ctrl('g');
        wait_vt(&mut t, "editor process started", |_| ready.exists()).await;

        let (set_ok, set_err): (bool, String) =
            eval(&t.vt.host, r#"local ok, err = pcall(mare.ui.set_editor, "clobbered"); return ok, tostring(err)"#)
                .await;
        assert!(!set_ok, "sync set_editor while external editor is open must fail");
        assert!(set_err.contains("external editor"), "unexpected set_editor error: {set_err}");

        let (ins_ok, ins_err): (bool, String) =
            eval(&t.vt.host, r#"local ok, err = pcall(mare.ui.editor_insert, "x"); return ok, tostring(err)"#).await;
        assert!(!ins_ok, "sync editor_insert while external editor is open must fail");
        assert!(ins_err.contains("external editor"), "unexpected editor_insert error: {ins_err}");

        // Read-only snapshot remains available during handoff.
        let current: String = eval(&t.vt.host, "return mare.ui.get_editor()").await;
        assert_eq!(current, "initial");

        std::fs::write(&release, "go").unwrap();
        wait_vt(&mut t, "external editor exited", |t| t.vt.editor_text() == "from editor").await;

        // Synchronous mutations work again after the external editor returns.
        let after_ok: bool = eval(&t.vt.host, r#"return pcall(mare.ui.set_editor, "after")"#).await;
        assert!(after_ok, "sync set_editor must succeed once external editor exits");
        assert_eq!(t.vt.editor_text(), "after");
    })
    .await
}

/// A `/side` failure backfill arriving while an external editor owns the terminal must not be
/// dropped by the busy bridge or overwrite the edited text; once the editor closes, both the
/// backfill and the external editor text must be preserved in defined order and rendered on screen.
#[cfg(unix)]
#[tokio::test]
async fn external_editor_preserves_side_failure_backfill_while_open() {
    struct ReleaseGuard(std::path::PathBuf);
    impl Drop for ReleaseGuard {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "release");
        }
    }

    tokio::time::timeout(Duration::from_secs(30), local(async {
        let dir = tempfile::tempdir().unwrap();
        let ready = dir.path().join("ready");
        let release = dir.path().join("release");
        let _release_guard = ReleaseGuard(release.clone());
        let script = dir.path().join("editor.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ntouch '{}'\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 500 ]; do sleep 0.02; i=$((i + 1)); done\nprintf 'from external editor\\n' > \"$1\"\n",
                ready.display(),
                release.display(),
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Disable the faux provider (and onboarding login picker) so `/side` fails to start
        // ("No model available") and backfills `/side why failed` while the external editor owns
        // the terminal (empty sessions now allow `/side` after #62 / signalridge/mare#42).
        let mut t = Term::build(
            json!([]),
            json!({
                "editor": script.to_string_lossy().as_ref(),
                "model": "",
                "plugins": {
                    "onboarding": { "enabled": false },
                    "provider-faux": { "enabled": false }
                }
            }),
            json!({}),
            80,
            20,
            |_| {},
        )
        .await;
        expose(&t.vt.host);
        wait_vt(&mut t, "startup to finish", |t| !t.vt.starting()).await;

        t.ctrl('g');
        wait_vt(&mut t, "editor process started", |_| ready.exists()).await;

        // Missing model causes `/side` start to fail and backfill `/side why failed` into the editor.
        exec(&t.vt.host, r#"C.cordis:get("commands"):run("/side why failed")"#).await;
        t.vt.drain_cmds();
        assert_ne!(
            t.vt.editor_text(),
            "/side why failed",
            "backfill must not overwrite the live buffer while the external editor is open"
        );

        std::fs::write(&release, "go").unwrap();
        wait_vt(&mut t, "editor exited and preserved backfill + edited text", |t| {
            t.vt.editor_text() == "/side why failed\n\nfrom external editor"
        })
        .await;

        let screen = t.screen();
        let backfill_pos = screen.find("/side why failed").expect("screen must display /side backfill");
        let editor_pos = screen.find("from external editor").expect("screen must display external editor text");
        assert!(backfill_pos < editor_pos, "backfill must precede external editor text on screen:\n{screen}");
    }))
    .await
    .expect("external_editor_preserves_side_failure_backfill_while_open timed out")
}

/// Resizing while a `CSI 6n` CPR query is in flight must account for the stale reply rather than
/// letting that old reply anchor the post-resize renderer; only after the stale reply is drained
/// may a fresh `CSI 6n` be emitted and anchored.
#[tokio::test]
async fn resize_while_cpr_pending_discards_stale_reply_before_requerying() {
    local(async {
        let mut t = Term::new(json!([]), 80, 40).await;
        t.vt.wait_startup_settled().await;
        // Settle any pending startup theme reflow at 80x40 before testing height-only resizes.
        t.vt.resize(80, 40);
        let init_out = t.vt.output();
        t.parser.process(&init_out);
        assert!(t.vt.scroll_insert_ready(), "initial renderer should be anchored before testing resize CPR race");

        // First height-only resize clears `renderer.top`; drawing emits CSI 6n #1 (`cpr_pending = true`).
        t.parser.screen_mut().set_size(nz(45), nz(80));
        t.vt.resize(80, 45);
        let _ = t.vt.output();
        t.vt.frame().await;
        let first_out = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(first_out.contains("\x1b[6n"), "initial post-resize draw must emit CSI 6n: {first_out:?}");
        assert!(!t.vt.scroll_insert_ready());

        // Second resize arrives BEFORE CSI 6n #1's reply returns. Drawing must NOT emit a second
        // CSI 6n yet because the stale reply #1 is still in flight.
        t.parser.screen_mut().set_size(nz(50), nz(80));
        t.vt.resize(80, 50);
        let _ = t.vt.output();
        t.vt.frame().await;
        let after_second_resize = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(
            !after_second_resize.contains("\x1b[6n"),
            "must wait to drain stale in-flight CPR reply before issuing a new CSI 6n: {after_second_resize:?}"
        );
        t.parser.process(after_second_resize.as_bytes());
        let actual_row = t.parser.screen().cursor_position().0 as u16 + 1;
        let stale_row = if actual_row > 2 { actual_row - 1 } else { actual_row + 1 };

        tokio::time::pause();
        t.vt.reset_loop_state();
        let _ = t.vt.output();
        let t0 = tokio::time::Instant::now().into_std();

        // Deliver the stale pre-resize reply #1: it must be discarded (NOT anchor renderer.top)
        // and immediately trigger the fresh CSI 6n #2.
        t.vt.send_tty_event_at(Event::cursor_position_report(stale_row, 1), t0);
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        assert!(!t.vt.scroll_insert_ready(), "stale pre-resize CPR reply must not anchor renderer.top after a resize");
        let requery = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(requery.contains("\x1b[6n"), "draining stale CPR reply must emit fresh CSI 6n: {requery:?}");

        // Deliver the fresh post-resize reply #2: now renderer.top anchors cleanly.
        t.vt.send_tty_event_at(Event::cursor_position_report(actual_row, 1), t0 + Duration::from_millis(60));
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        assert!(t.vt.scroll_insert_ready(), "fresh post-resize CPR reply must anchor renderer.top");
        tokio::time::resume();
    })
    .await
}
