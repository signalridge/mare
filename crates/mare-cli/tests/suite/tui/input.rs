//! The input box: typing, paste, history, shell mode and refused prompts.

use super::*;

/// Codex textarea.rs:3560-3573: shifted delete keys retain normal character deletion.
#[tokio::test]
async fn shifted_delete_keys_reach_the_editor() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        t.vt.type_str("abc");
        t.key(KeyCode::Left, Modifiers::NONE);
        t.key(KeyCode::Backspace, Modifiers::SHIFT);
        assert_eq!(t.vt.editor_text(), "ac", "Shift+Backspace deletes before the cursor");
        t.vt.type_str("b");
        t.key(KeyCode::Left, Modifiers::NONE);
        t.key(KeyCode::Delete, Modifiers::SHIFT);
        assert_eq!(t.vt.editor_text(), "ac", "Shift+Delete deletes after the cursor");
    })
    .await
}

#[tokio::test]
async fn double_bang_backspace_leaves_each_shell_level() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        t.vt.type_str("!!");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "!!", "!! enters shell mode level 2");
        let s = t.screen();
        assert!(
            s.contains("!!") && s.contains("Shell command, output kept out of the context"),
            "shell level 2 prompt shown:\n{s}"
        );
        t.key(KeyCode::Backspace, Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "!", "first Backspace steps back to level 1");
        let s = t.screen();
        assert!(!s.contains("!!") && s.contains("! Shell command"), "single-bang shell prompt:\n{s}");
        t.key(KeyCode::Backspace, Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "", "second Backspace exits shell mode");
        assert!(t.screen().contains("Ask mare"), "second Backspace exits shell mode:\n{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn select_vim_count_is_consumed_after_movement_and_paging() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                local items = {}
                for i = 1, 20 do items[i] = ("item%02d"):format(i) end
                COUNT_RESULT = { mare.ui.select({ title = "CountSelect", items = items, vim = true }) }
            end)"#,
        )
        .await;
        t.until("counted select", |s| s.contains("CountSelect")).await;
        t.key(KeyCode::Char('2'), Modifiers::NONE);
        t.key(KeyCode::Char('j'), Modifiers::NONE);
        t.key(KeyCode::Char('j'), Modifiers::NONE);
        t.key(KeyCode::Char('3'), Modifiers::NONE);
        t.key(KeyCode::PageDown, Modifiers::NONE);
        t.key(KeyCode::Char('j'), Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("counted select closes", |s| !s.contains("CountSelect")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return COUNT_RESULT and COUNT_RESULT[1]").await, Some(15));
    })
    .await
}

#[tokio::test]
async fn configured_freeform_description_is_isolated_from_returned_tool_definitions() {
    local(async {
        let config = json!({ "tool_descriptions": { "apply_patch": "user patch description" } });
        let t = Term::with(json!([]), config, 80, 24).await;
        expose(&t.vt.host);
        let (first, again): (String, String) = eval(
            &t.vt.host,
            r#"local definition = assert(T:get("apply_patch"))
               local first = definition.freeform.description
               definition.freeform.description = "changed by caller"
               return first, T:get("apply_patch").freeform.description"#,
        )
        .await;
        assert_eq!(first, "user patch description");
        assert_eq!(again, first, "mutating a returned freeform table must not alter the registered tool");
    })
    .await
}

#[tokio::test]
async fn typing_shows_in_the_input_box_and_submit_streams_the_answer() {
    local(async {
        let mut t = Term::new(json!([{ "text": "Hello from the model." }]), 60, 16).await;
        t.vt.type_str("hi there");
        t.until("the typed text", |s| s.contains("hi there")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        t.until("the answer", |s| s.contains("Hello from the model.")).await;
        assert_eq!(t.vt.editor_text(), "", "the input box is cleared after submit");
        // the prompt and the answer are in the transcript, the input box stays at the bottom
        let screen = t.screen();
        assert!(screen.contains("hi there"), "{screen}");
        let last = trimmed(&screen).lines().count();
        assert!(last >= 3, "{screen}");
    })
    .await
}

#[tokio::test]
async fn bracketed_and_large_pastes_land_in_the_input_box() {
    local(async {
        let mut t = Term::new(json!([]), 60, 14).await;
        t.vt.paste("short paste");
        t.until("the small paste", |s| s.contains("short paste")).await;
        assert_eq!(t.vt.editor_text(), "short paste");
        let big: String = (0..30).map(|i| format!("row {i}\n")).collect();
        t.vt.paste(&big);
        t.until("the folded paste", |s| s.contains("[Pasted text #1 +")).await;
        assert!(!t.screen().contains("row 29"), "a large paste is folded, not drawn:\n{}", t.screen());
    })
    .await
}

/// Up/Down: a multi-line draft is never replaced by history; a recalled entry keeps browsing.
#[tokio::test]
async fn history_needs_an_empty_input_or_the_untouched_recalled_entry() {
    local(async {
        let mut t = Term::new(json!([{ "text": "a" }, { "text": "b" }]), 70, 16).await;
        t.submit("first prompt").await;
        t.idle().await;
        t.submit("second prompt").await;
        t.idle().await;
        // a draft: Up stays in it
        t.vt.type_str("my draft");
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Up, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "my draft");
        // empty input: Up recalls, and the next Up keeps going back
        t.key(KeyCode::End, Modifiers::NONE);
        t.key(KeyCode::Char('u'), Modifiers::CONTROL);
        assert_eq!(t.vt.editor_text(), "");
        t.key(KeyCode::Up, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "second prompt");
        t.key(KeyCode::Up, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "first prompt");
        t.key(KeyCode::Down, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "second prompt");
        // while browsing, Ctrl+R searches the history (outside it, Ctrl+R is the review page):
        // type to filter, Enter puts the entry in the input
        t.ctrl('r');
        t.until("the history search", |s| s.contains("Search history")).await;
        t.vt.type_str("firs");
        t.step().await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("the picked entry", |t| t.vt.editor_text() == "first prompt").await;
        assert!(!t.screen().contains("Search history"));
    })
    .await
}

#[tokio::test]
async fn history_search_uses_the_live_keymap_in_idle_and_browsing_states() {
    local(async {
        let mut t = Term::new(json!([{ "text": "answer" }]), 90, 20).await;
        t.ctrl('r');
        t.until("the empty history hint", |s| s.contains("No history yet")).await;
        t.vt.dismiss_notices();
        t.submit("remember this").await;
        t.idle().await;
        t.ctrl('r');
        t.until("history search from idle", |s| s.contains("Search history") && s.contains("remember this")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("history search closed", |s| !s.contains("Search history")).await;
        exec(
            &t.vt.host,
            r#"mare.keymap.set('editor', '<C-r>', false); mare.keymap.set('editor', '<C-x>', 'history-search')"#,
        )
        .await;
        t.step().await;
        t.key(KeyCode::Up, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "remember this");
        t.ctrl('r');
        t.step().await;
        assert!(!t.screen().contains("Search history"), "the removed binding must not search while browsing");
        t.ctrl('x');
        t.until("history search on the rebound key", |s| s.contains("Search history")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("history search closed again", |s| !s.contains("Search history")).await;
        t.key(KeyCode::End, Modifiers::NONE);
        t.ctrl('u');
        assert_eq!(t.vt.editor_text(), "");
        t.vt.type_str("draft text");
        t.ctrl('x');
        t.until("history search with a draft", |s| s.contains("Search history")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("draft restored", |s| !s.contains("Search history")).await;
        assert_eq!(t.vt.editor_text(), "draft text");
    })
    .await
}

/// Several large pastes: each placeholder expands to its own content, once.
#[tokio::test]
async fn several_paste_placeholders_submit_their_own_content() {
    local(async {
        let mut t = Term::new(json!([{ "text": "got it" }]), 80, 16).await;
        let a: String = (0..15).map(|i| format!("alpha {i}\n")).collect();
        let b: String = (0..15).map(|i| format!("beta {i}\n")).collect();
        t.vt.paste(&a);
        t.vt.type_str(" and ");
        t.vt.paste(&b);
        let text = t.vt.editor_text();
        assert!(text.contains("[Pasted text #1") && text.contains("[Pasted text #2"), "{text}");
        // past the window in which an Enter right after a paste is ignored (Windows Terminal)
        tokio::time::sleep(mare_tui::paste::GUARD_WINDOW + Duration::from_millis(50)).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        let h = t.history();
        assert!(h.contains("alpha 14") && h.contains("beta 14"), "{h}");
        assert!(!h.contains("[Pasted text"), "{h}");
    })
    .await
}

/// select mode turns mouse reporting on; a drag in the live area selects
/// text (reverse video) and releasing copies it through the clipboard primitive (OSC 52 here),
/// then the mouse goes back to the terminal.
#[tokio::test]
async fn select_mode_copies_a_drag_in_the_live_area() {
    local(async {
        use mare_tui::tty::{MouseButton, MouseEventKind};
        // the terminal path of the clipboard primitive, on any machine (debug-build seam)
        unsafe { std::env::set_var("MARE_TEST_CLIPBOARD", "osc52") };
        let body: String = (1..=40).map(|i| format!("line {i}\n\n")).collect();
        let mut t = Term::new(json!([{ "text": body }]), 70, 16).await;
        t.submit("fill the screen").await;
        t.idle().await;
        t.vt.action("select-mode");
        t.step().await;
        assert!(t.vt.selecting());
        t.until("the select-mode hints", |s| s.contains("drag select text")).await;
        let screen = t.screen();
        let (row, line) = screen.lines().enumerate().find(|(_, l)| l.contains("Ask mare")).expect("the input box");
        let col = line.find("Ask mare").map(|b| line[..b].chars().count()).unwrap() as u16;
        t.vt.mouse(MouseEventKind::Down(MouseButton::Left), col, row as u16);
        t.vt.mouse(MouseEventKind::Drag(MouseButton::Left), col + 7, row as u16);
        t.step().await;
        assert!(t.parser.screen().cell(row as u16, col).unwrap().inverse(), "the selection shows reversed");
        let _ = t.vt.output();
        t.vt.mouse(MouseEventKind::Up(MouseButton::Left), col + 7, row as u16);
        let immediate = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(immediate.contains("\x1b[?1002l"), "the mouse is the terminal's again");
        assert!(!t.vt.selecting());
        let want = mare_binding::clipboard::osc52_sequence("Ask mare", false).unwrap();
        let mut saw_osc52 = immediate.contains(&want);
        let end = Instant::now() + WAIT;
        loop {
            t.vt.frame().await;
            let raw = t.vt.output();
            if String::from_utf8_lossy(&raw).contains(&want) {
                saw_osc52 = true;
            }
            t.parser.process(&raw);
            if saw_osc52 && t.screen().contains("Selection sent to the") {
                break;
            }
            assert!(Instant::now() < end, "timed out waiting for OSC 52 + report:\n{}", t.screen());
            tokio::time::sleep(Duration::from_millis(15)).await; // sleep-ok: bounded poll waiting for background copy
        }
        unsafe { std::env::remove_var("MARE_TEST_CLIPBOARD") };
    })
    .await
}

#[tokio::test]
async fn select_mode_copy_does_not_block_event_loop() {
    local(async {
        use mare_tui::tty::{MouseButton, MouseEventKind};
        unsafe { std::env::set_var("MARE_TEST_CLIPBOARD", "slow:400") };
        let body: String = (1..=40).map(|i| format!("line {i}\n\n")).collect();
        let mut t = Term::new(json!([{ "text": body }]), 70, 16).await;
        t.submit("fill the screen").await;
        t.idle().await;
        t.vt.action("select-mode");
        t.step().await;
        t.until("the select-mode hints", |s| s.contains("drag select text")).await;
        let screen = t.screen();
        let (row, line) = screen.lines().enumerate().find(|(_, l)| l.contains("Ask mare")).expect("the input box");
        let col = line.find("Ask mare").map(|b| line[..b].chars().count()).unwrap() as u16;
        t.vt.mouse(MouseEventKind::Down(MouseButton::Left), col, row as u16);
        t.vt.mouse(MouseEventKind::Drag(MouseButton::Left), col + 7, row as u16);
        t.step().await;
        let _ = t.vt.output();

        // Releasing the mouse must return immediately without blocking the current-thread loop.
        let started = Instant::now();
        t.vt.mouse(MouseEventKind::Up(MouseButton::Left), col + 7, row as u16);
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_millis(150), "mouse-release copy blocked the event loop for {elapsed:?}");
        assert!(!t.vt.selecting());

        // While the slow backend runs off-thread, the UI event loop stays responsive and later
        // delivers the OSC 52 fallback + completion notice.
        let want = mare_binding::clipboard::osc52_sequence("Ask mare", false).unwrap();
        let mut saw_osc52 = false;
        let end = Instant::now() + WAIT;
        loop {
            t.vt.frame().await;
            let raw = t.vt.output();
            if String::from_utf8_lossy(&raw).contains(&want) {
                saw_osc52 = true;
            }
            t.parser.process(&raw);
            if saw_osc52 && t.screen().contains("Selection sent to the") {
                break;
            }
            assert!(Instant::now() < end, "timed out waiting for async selection copy:\n{}", t.screen());
            tokio::time::sleep(Duration::from_millis(15)).await; // sleep-ok: bounded poll waiting for background copy
        }
        unsafe { std::env::remove_var("MARE_TEST_CLIPBOARD") };
    })
    .await
}

/// Paste-burst detection (on by default on Windows, whose console may not bracket pastes) with
/// keys arriving at paste speed: the Enters inside the burst are newlines, not submits; an Enter
/// typed after a pause submits the whole text.
#[tokio::test]
async fn an_unbracketed_paste_keeps_its_newlines() {
    local(async {
        let mut t = Term::new(json!([{ "text": "got it" }]), 80, 20).await;
        t.vt.set_paste_burst(true);
        t.ended = t.vt.turns_ended();
        // one key event after another, as fast as a terminal delivers a paste
        t.vt.type_str("first line\nsecond line\nthird");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "first line\nsecond line\nthird", "a pasted Enter submitted");
        assert!(!t.vt.busy());
        // a person's Enter, after the burst window (120 ms) closed
        tokio::time::sleep(Duration::from_millis(250)).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        t.until("the answer", |s| s.contains("got it")).await;
        assert!(t.history().contains("third"), "{}", t.history());
    })
    .await
}

/// Ctrl+S runs a shell line while idle; Alt+Enter inserts a newline before a slash command runs.
#[cfg(unix)]
#[tokio::test]
async fn ctrl_s_runs_shell_and_alt_enter_adds_an_idle_newline() {
    local(async {
        let mut t = Term::new(json!([]), 100, 40).await;
        t.vt.type_str("!echo steered-$((40+2))");
        t.ctrl('s');
        t.until("the shell output", |s| s.contains("steered-42")).await;
        t.vt.type_str("/hotkeys");
        t.key(KeyCode::Enter, Modifiers::ALT);
        assert_eq!(t.vt.editor_text(), "/hotkeys\n");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the /hotkeys overlay", |s| s.contains("Input editor")).await;
        assert_eq!(t.vt.turns_ended(), 0, "nothing went to the model");
        global(&t.vt.host, "AG", t.vt.host.service_table("agent").unwrap());
        // `shell-mode.lua` renders `!cmd` output to the TUI before `agent:append` finishes
        // persisting it to the session journal; wait for the append to commit.
        wait_until(WAIT_SECS, "shell output appended to context", || {
            let h = t.vt.host.clone();
            async move {
                eval::<String>(&h, "return mare.json.encode(AG:list_messages().data)").await.contains("steered-42")
            }
        })
        .await;
        let messages: String = eval(&t.vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(!messages.contains("/hotkeys") && !messages.contains("!echo"), "{messages}");
        assert!(messages.contains("steered-42"), "the !cmd output is context: {messages}");
    })
    .await
}

/// the host's durable-start gate covers tool calls only. With the session journal
/// open, a user's `!` command and a plugin's own process (statusline / review style
/// `mare.proc.run`, outside any tool call) still run.
#[cfg(unix)]
#[tokio::test]
async fn shell_lines_and_plugin_processes_run_while_the_session_is_saved() {
    local(async {
        let mut t = Term::new(json!([{ "text": "noted" }]), 100, 40).await;
        t.submit("hello").await;
        t.idle().await;
        global(&t.vt.host, "SS", t.vt.host.service_table("session").unwrap());
        let journal: bool = eval(&t.vt.host, "return SS:info(nil).path ~= nil").await;
        assert!(journal, "the session journal is open");
        t.vt.type_str("!echo bang-$((40+2))");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the shell output", |s| s.contains("bang-42")).await;
        let out: String =
            eval(&t.vt.host, r#"return mare.proc.run({ cmd = "sh", args = { "-c", "echo plugin-ok" } }).stdout"#).await;
        assert_eq!(out.trim(), "plugin-ok");
    })
    .await
}

/// Repro #217: a completed `!` command must not duplicate its full captured output across both
/// `node` (collapsed) and `expanded` and retain unbounded copies across committed blocks.
#[cfg(unix)]
#[tokio::test]
async fn repro_217_shell_mode_duplicates_and_retains_unbounded_output() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        exec(
            &t.vt.host,
            r#"
            REPRO_217_PUSHES = {}
            local orig_push = mare.ui.push
            mare.ui.push = function(kind, opts)
                if kind == "tool" and type(opts) == "table" and opts.node and opts.expanded then
                    REPRO_217_PUSHES[#REPRO_217_PUSHES + 1] = {
                        node_bytes = #mare.json.encode(opts.node),
                        expanded_bytes = #mare.json.encode(opts.expanded),
                    }
                end
                return orig_push(kind, opts)
            end
            "#,
        )
        .await;

        // Run a `!` shell command producing 400 lines (~40 KB), well above preview_lines (default 6).
        t.vt.type_str("!awk 'BEGIN{for(i=1;i<=400;i++) printf \"line-%04d %090d\\n\", i, i}'");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("shell output committed", |s| s.contains("line-0001")).await;

        let (count, node_bytes, expanded_bytes): (i64, i64, i64) = eval(
            &t.vt.host,
            r#"
            local last = REPRO_217_PUSHES[#REPRO_217_PUSHES]
            return #REPRO_217_PUSHES, last and last.node_bytes or 0, last and last.expanded_bytes or 0
            "#,
        )
        .await;
        assert_eq!(count, 1, "expected one shell-mode tool block push");
        assert!(
            node_bytes < expanded_bytes / 2,
            "collapsed `node` duplicates full shell output alongside `expanded` (node_bytes={node_bytes}, expanded_bytes={expanded_bytes})"
        );
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn shell_mode_spills_older_large_outputs_and_keeps_recent_expandable() {
    local(async {
        let mut t = Term::with(
            json!([]),
            json!({
                "plugins": {
                    "shell-mode": {
                        "retained_blocks": 2,
                        "preview_lines": 5
                    }
                }
            }),
            100,
            30,
        )
        .await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        exec(
            &t.vt.host,
            r#"
            REPRO_217_UPDATES = {}
            local orig_update = mare.ui.update
            mare.ui.update = function(id, opts)
                if type(opts) == "table" and opts.expanded then
                    REPRO_217_UPDATES[#REPRO_217_UPDATES + 1] = mare.json.encode(opts.expanded)
                end
                return orig_update(id, opts)
            end
            "#,
        )
        .await;

        for run in 1..=3 {
            let cmd = format!("!awk 'BEGIN{{for(i=1;i<=120;i++) printf \"run{run}-line-%03d %080d\\n\", i, i}}'");
            t.vt.type_str(&cmd);
            t.key(KeyCode::Enter, Modifiers::NONE);
            let marker = format!("run{run}-line-001");
            t.until("shell output committed", |s| s.contains(&marker)).await;
        }

        let (evicted_count, spill_path): (i64, String) = eval(
            &t.vt.host,
            r#"
            local first = REPRO_217_UPDATES[1] or ""
            local p = first:match("([^%s%[%]\"']+spill/shell%-[^%s%[%]\"']+%.log)") or ""
            return #REPRO_217_UPDATES, p
            "#,
        )
        .await;
        assert_eq!(evicted_count, 1, "oldest large shell block must be evicted once retained_blocks=2 is exceeded");
        assert!(!spill_path.is_empty(), "evicted shell block must record a retrievable spill file path");
        let spilled = std::fs::read_to_string(&spill_path).expect("spill file must exist on disk");
        assert!(
            spilled.contains("run1-line-001") && spilled.contains("run1-line-120"),
            "spill file must preserve full output of evicted shell command"
        );
    })
    .await
}

/// Oversized (> expanded_max_bytes) spilled shell outputs must still participate in retention
/// eviction so their 64 KiB expanded preview prefixes cannot leak across repeated commands;
/// single-line outputs without newlines must still clip by visual rows on narrow screens; and
/// the expand hint must follow `mare.keymap.hint("toggle-expand", "editor")`.
#[cfg(unix)]
#[tokio::test]
async fn shell_mode_evicts_oversized_spills_clips_wrapped_lines_and_uses_keymap_hint() {
    local(async {
        let mut t = Term::with(
            json!([]),
            json!({
                "plugins": {
                    "shell-mode": {
                        "expanded_max_bytes": 4096,
                        "retained_blocks": 1,
                        "preview_lines": 3
                    }
                }
            }),
            50,
            24,
        )
        .await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();

        exec(
            &t.vt.host,
            r#"
            OVERSIZED_UPDATES = {}
            local orig_update = mare.ui.update
            mare.ui.update = function(id, opts)
                if type(opts) == "table" and opts.expanded then
                    OVERSIZED_UPDATES[#OVERSIZED_UPDATES + 1] = mare.json.encode(opts.expanded)
                end
                return orig_update(id, opts)
            end
            -- Rebind toggle-expand to verify the collapsed footer uses the live keymap hint.
            mare.keymap.set("editor", "<C-o>", false)
            mare.keymap.set("editor", "<M-e>", "toggle-expand")
            "#,
        )
        .await;

        // 1) Two oversized (> 4096 B) commands with retained_blocks=1: the first oversized block's
        //    expanded prefix must be evicted to a short preview + spill reference (no memory leak).
        for run in 1..=2 {
            let cmd = format!("!awk 'BEGIN{{for(i=1;i<=180;i++) printf \"big{run}-%03d %020d\\n\", i, i}}'");
            t.vt.type_str(&cmd);
            t.key(KeyCode::Enter, Modifiers::NONE);
            let marker = format!("big{run}-001");
            t.until("oversized shell output committed", |s| s.contains(&marker)).await;
        }
        let (evicted, evicted_len, has_spill): (i64, i64, bool) = eval(
            &t.vt.host,
            r#"
            local u = OVERSIZED_UPDATES[1] or ""
            return #OVERSIZED_UPDATES, #u, u:find("spill/shell%-", 1) ~= nil
            "#,
        )
        .await;
        assert_eq!(evicted, 1, "oversized spilled block must enter retention queue and be evicted");
        assert!(has_spill, "evicted oversized block must keep spill file reference");
        assert!(
            evicted_len < 2048,
            "evicted oversized expanded node must shrink to short preview, got {evicted_len} B"
        );

        // 2) Collapsed footer reflects the rebound toggle-expand key rather than hardcoded ctrl+o.
        let hist = t.history();
        assert!(
            hist.contains("(Alt+E to expand)")
                && !hist.contains("(ctrl+o to expand)")
                && !hist.contains("(Ctrl+O to expand)"),
            "collapsed footer must reflect live toggle-expand keymap hint:\n{hist}"
        );

        // 3) Single-line 3500-char output (0 newlines) must still collapse via visual-row ui.Clip
        //    on a 50-column terminal rather than flooding ~70 visual rows.
        t.vt.type_str("!awk 'BEGIN{for(i=1;i<=350;i++) printf \"0123456789\"; printf \"\\n\"}'");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("wrapped single-line clipped", |s| s.contains("0123456789") && s.contains("more line")).await;
        let screen = t.screen();
        let digit_rows = screen.lines().filter(|l| l.contains("0123456789")).count();
        assert!(
            digit_rows <= 4,
            "3500-char single-line output must clip to <= preview_lines (3) visual rows, got {digit_rows}:\n{screen}"
        );
    })
    .await
}

/// `!` shell mode: `!` on an empty input becomes the prompt (never shown twice), a
/// `/path` after it opens no command menu, Backspace on the empty line leaves the mode.
#[tokio::test]
async fn entering_shell_with_unchanged_raw_text_invalidates_pending_completion() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            mare.ui.editor_config({shell=false})
            SAME_GATE, SAME_WAIT = mare.task.channel()
            SAME_STARTED, SAME_FINISHED = false, false
            mare.ui.completion.register({name='same-text', triggers={'!'}, min_prefix=2, timeout_ms=10000,
                complete=function()
                    SAME_STARTED=true; SAME_WAIT:recv(); SAME_FINISHED=true
                    return {{label='STALE-SAME-TEXT',insert='STALE'}}
                end})
        "#,
        )
        .await;
        t.step().await;
        t.vt.type_str("!@x");
        t.until_vt("completion started", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return SAME_STARTED")).unwrap_or(false)
        })
        .await;
        exec(&t.vt.host, "mare.ui.editor_config({shell={placeholder='shell'}})").await;
        t.step().await;
        exec(&t.vt.host, "mare.ui.set_editor('!@x')").await;
        tokio::time::timeout(Duration::from_secs(5), t.until("same text in shell mode", |s| s.contains("! @x")))
            .await
            .expect("shell mode was not entered");
        exec(&t.vt.host, "SAME_GATE:send(true)").await;
        t.until_vt("completion settled", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return SAME_FINISHED")).unwrap_or(false)
        })
        .await;
        t.step().await;
        assert!(!t.screen().contains("STALE-SAME-TEXT"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn shell_entry_invalidates_inflight_completion_and_tab_reports_mode() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        exec(&t.vt.host, r#"
            GATE, WAIT = mare.task.channel()
            STARTED, FINISHED, FORCE_CTX = false, false, nil
            mare.ui.completion.register({name='shell-probe', triggers={'@'}, min_prefix=0, timeout_ms=3000,
                enabled=function(ev) return not ev.shell_mode or ev.force end,
                complete=function(ev)
                    if ev.shell_mode then FORCE_CTX={ev.force,ev.shell_level}; return {{label='@forced',insert='@forced'}} end
                    STARTED=true; WAIT:recv(); FINISHED=true
                    return {{label='@STALE',insert='@stale'}}
                end})
        "#).await;
        t.vt.type_str("@a");
        t.until_vt("completion started", |t| futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return STARTED")).unwrap_or(false)).await;
        exec(&t.vt.host, "mare.ui.set_editor('!@x')").await;
        t.until("shell mode", |s| s.contains("! @x")).await;
        exec(&t.vt.host, "GATE:send(true)").await;
        t.until_vt("stale completion finished", |t| futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return FINISHED")).unwrap_or(false)).await;
        assert!(!t.screen().contains("@STALE"));
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("explicit completion", |s| s.contains("@forced")).await;
        let (force, level): (bool, i64) = eval(&t.vt.host, "return FORCE_CTX[1], FORCE_CTX[2]").await;
        assert!(force);
        assert_eq!(level, 1);
    }).await
}

#[tokio::test]
async fn whitespace_paste_preserves_text() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        t.vt.type_str("a");
        t.vt.paste(" ");
        assert_eq!(t.vt.editor_text(), "a ");
    })
    .await
}

#[tokio::test]
async fn shell_at_argument_does_not_steal_enter_for_completion() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        exec(&t.vt.host, r#"mare.ui.completion.register({name='at-test', triggers={'@'}, min_prefix=0, enabled=function(ev) return not ev.shell_mode or ev.force end, complete=function() return {{label='@data.json', insert='@wrong'}} end})"#).await;
        t.vt.type_str("!curl -d @data.json");
        for _ in 0..20 { t.step().await; }
        t.key(KeyCode::Enter, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "", "Enter must submit the shell command");
    }).await
}

#[tokio::test]
async fn shell_aware_completion_can_automatically_offer_candidates() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"
            mare.ui.completion.register({name='shell-only', triggers={'@'}, min_prefix=0,
                enabled=function(ev) return ev.shell_mode and not ev.force end,
                complete=function() return {{label='@shell-candidate',insert='@shell-candidate'}} end})
        "#,
        )
        .await;
        t.vt.type_str("!@x");
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !t.screen().contains("@shell-candidate") && std::time::Instant::now() < deadline {
            t.step().await;
            tokio::task::yield_now().await;
        }
        assert!(t.screen().contains("@shell-candidate"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn bang_on_an_empty_input_turns_the_prompt_into_shell_mode() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.type_str("!");
        t.until("the shell prompt", |s| s.contains("! Shell command")).await;
        assert!(t.screen().contains("Esc to leave shell mode"), "{}", t.screen());
        t.vt.type_str("/bin/ls");
        t.step().await;
        let s = t.screen();
        assert!(s.contains("! /bin/ls") && !s.contains("!/bin") && !s.contains("/compact"), "{s}");
        assert_eq!(t.vt.editor_text(), "!/bin/ls");
        while t.vt.editor_text().len() > 1 {
            t.key(KeyCode::Backspace, Modifiers::NONE);
        }
        t.key(KeyCode::Backspace, Modifiers::NONE);
        t.until("the normal prompt", |s| s.contains("Ask mare")).await;
        assert_eq!(t.vt.editor_text(), "");
    })
    .await
}

/// Shell mode belongs to whoever sets `editor_config{ shell }` (the shell-mode plugin): once it
/// is switched off, `!` is an ordinary character.
#[tokio::test]
async fn shell_mode_exists_only_while_a_plugin_enables_it() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(&t.vt.host, "mare.ui.editor_config({ shell = false })").await;
        t.step().await;
        t.vt.type_str("!");
        t.step().await;
        let s = t.screen();
        assert!(!s.contains("Shell command"), "{s}");
        t.vt.type_str("x");
        assert_eq!(t.vt.editor_text(), "!x");
    })
    .await
}

/// A prompt a hook refuses is "not sent:
/// <reason>" in the transcript, is not added to the conversation, and its text goes back into the
/// input box — in front of what was typed since.
#[tokio::test]
async fn a_refused_prompt_goes_back_into_the_input_box() {
    local(async {
        let mut t = Term::new(json!([{ "text": "answered" }]), 90, 30).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare._veto = true
               C.cordis:get("policy"):voter("request/ready", function()
                 if mare._veto then return { decision = "deny", reason = "held by the test hook" } end
               end)"#,
        )
        .await;
        t.submit("please refactor").await;
        t.idle().await;
        t.until("the refusal", |s| s.contains("not sent: held by the test hook")).await;
        assert_eq!(t.vt.editor_text(), "please refactor");
        let messages: String = eval(&t.vt.host, "return mare.json.encode(A:list_messages().data)").await;
        assert!(!messages.contains("please refactor"), "a refused prompt is not in the conversation: {messages}");
        // typed meanwhile: the refused text goes in front of it
        t.key(KeyCode::Char('c'), Modifiers::CONTROL);
        t.ended = t.vt.turns_ended();
        exec(&t.vt.host, r#"A:prompt("from a plugin")"#).await;
        t.vt.type_str("new draft");
        t.idle().await;
        assert_eq!(t.vt.editor_text(), "from a plugin\n\nnew draft");
        exec(&t.vt.host, "mare._veto = false").await;
        t.key(KeyCode::Char('c'), Modifiers::CONTROL);
        t.submit("please refactor").await;
        t.idle().await;
        t.until("the answer", |s| s.contains("answered")).await;
    })
    .await
}

/// Select mode freezes the live snapshot at drag start so streaming or notice shifts during the
/// drag do not copy wrong rows; releasing above the live area still finishes selection and turns
/// mouse reporting off; opening a transcript or modal overlay exits select mode.
#[tokio::test]
async fn select_mode_freezes_drag_snapshot_clamps_up_and_exits_on_overlay() {
    local(async {
        use mare_tui::tty::{MouseButton, MouseEventKind};
        unsafe { std::env::set_var("MARE_TEST_CLIPBOARD", "osc52") };

        let body: String = (1..=40).map(|i| format!("line {i}\n\n")).collect();
        let mut t = Term::new(json!([{ "text": body }]), 70, 16).await;
        t.until_vt("startup settled", |t| t.vt.startup_finished()).await;
        t.vt.dismiss_notices();
        t.step().await;
        t.submit("fill the screen").await;
        t.idle().await;

        // 1. Releasing the mouse above the live area's top row still ends selection and disables mouse mode.
        t.vt.action("select-mode");
        t.step().await;
        assert!(t.vt.selecting());
        let screen = t.screen();
        let (row, line) = screen.lines().enumerate().find(|(_, l)| l.contains("Ask mare")).expect("the input box");
        assert!(row > 0, "expected live area below row 0 after scrollback fill:\n{screen}");
        let col = line.find("Ask mare").map(|b| line[..b].chars().count()).unwrap() as u16;
        t.vt.mouse(MouseEventKind::Down(MouseButton::Left), col, row as u16);
        let _ = t.vt.output();
        t.vt.mouse(MouseEventKind::Up(MouseButton::Left), col, 0);
        let out = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(!t.vt.selecting(), "releasing above live area must exit select mode");
        assert!(out.contains("\x1b[?1002l"), "mouse reporting must disable on out-of-bounds release: {out:?}");
        t.vt.dismiss_notices();
        t.step().await;

        // 2. Live-area shift during an active drag must still copy what was under the pointer at press.
        t.vt.action("select-mode");
        t.step().await;
        assert!(t.vt.selecting());
        let screen = t.screen();
        let (row, line) = screen.lines().enumerate().find(|(_, l)| l.contains("Ask mare")).expect("the input box");
        let col = line.find("Ask mare").map(|b| line[..b].chars().count()).unwrap() as u16;
        t.vt.mouse(MouseEventKind::Down(MouseButton::Left), col, row as u16);
        t.vt.mouse(MouseEventKind::Drag(MouseButton::Left), col + 7, row as u16);
        t.step().await;
        // Shift live area while left button is still held (working spinner + multi-line notice).
        exec(
            &t.vt.host,
            r#"mare.ui.working("Streaming", "busy detail")
               mare.ui.notify("Notice row one\nNotice row two\nNotice row three")"#,
        )
        .await;
        t.step().await;
        let _ = t.vt.output();
        t.vt.mouse(MouseEventKind::Up(MouseButton::Left), col + 7, row as u16);
        let immediate = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(immediate.contains("\x1b[?1002l"), "mouse reporting must turn off after release");
        assert!(!t.vt.selecting());
        let want = mare_binding::clipboard::osc52_sequence("Ask mare", false).unwrap();
        let mut saw_osc52 = immediate.contains(&want);
        let end = Instant::now() + WAIT;
        loop {
            t.vt.frame().await;
            let raw = t.vt.output();
            if String::from_utf8_lossy(&raw).contains(&want) {
                saw_osc52 = true;
            }
            t.parser.process(&raw);
            if saw_osc52 && t.screen().contains("Selection sent to the") {
                break;
            }
            assert!(
                Instant::now() < end,
                "copied text drifted or timed out after live-area shift (row={row} col={col} selecting={}): saw_osc52={saw_osc52}\nscreen:\n{}",
                t.vt.selecting(),
                t.screen()
            );
            tokio::time::sleep(Duration::from_millis(15)).await; // sleep-ok: bounded poll waiting for background copy
        }
        exec(&t.vt.host, "mare.ui.working(nil)").await;
        t.vt.dismiss_notices();
        t.step().await;

        // 3. Opening a plugin transcript view or modal while in select mode exits select mode.
        t.vt.action("select-mode");
        t.step().await;
        assert!(t.vt.selecting());
        let _ = t.vt.output();
        exec(&t.vt.host, r#"mare.ui.transcript("View", mare.ui.Line("transcript line"))"#).await;
        t.vt.frame().await;
        let bytes = t.vt.output();
        t.parser.process(&bytes);
        let out = String::from_utf8_lossy(&bytes).into_owned();
        assert!(!t.vt.selecting(), "TranscriptView must exit select mode");
        assert!(out.contains("\x1b[?1002l"), "mouse reporting must disable when TranscriptView opens: {out:?}");
        exec(&t.vt.host, r#"mare.ui.transcript_close("View")"#).await;
        t.step().await;

        t.vt.action("select-mode");
        t.step().await;
        assert!(t.vt.selecting());
        exec(&t.vt.host, r#"mare.ui.overlay("Info", "modal body")"#).await;
        t.step().await;
        assert!(!t.vt.selecting(), "opening an overlay modal must exit select mode");

        unsafe { std::env::remove_var("MARE_TEST_CLIPBOARD") };
    })
    .await
}

/// A notification suppressed by `only_unfocused` while the terminal is focused must not poison the
/// `dedupe_ms` window, so a subsequent blocked event with the same text after the terminal loses
/// focus still emits the OS notification.
#[tokio::test]
async fn notify_suppressed_while_focused_does_not_poison_dedupe_window() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["status", "notify"]).faux(json!([])).config(json!({
            "logo": false,
            "plugins": {
                "permissions": { "enabled": true },
                "onboarding": { "plugins": false },
                "notify": {
                    "enabled": true,
                    "only_unfocused": true,
                    "protocol": "osc9",
                    "dedupe_ms": 60000
                }
            }
        }));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 80, 16).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(16), nz(80), 2000));
        t.step().await;
        t.until_vt("startup settled", |t| t.vt.startup_finished()).await;
        expose(&t.vt.host);

        // Terminal is focused: blocked notification is suppressed and must NOT consume dedupe_ms.
        t.vt.focus(true);
        t.step().await;
        let _ = t.vt.output();
        exec(&t.vt.host, r#"C.cordis:emit("status/changed", { state = "blocked", message = "Approve tool call" })"#)
            .await;
        t.vt.frame().await;
        let focused_out = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(!focused_out.contains("\x1b]9;"), "focused terminal must not emit OS notification: {focused_out:?}");

        // Return to idle, then lose focus, then enter blocked with the same text within dedupe_ms.
        exec(&t.vt.host, r#"C.cordis:emit("status/changed", { state = "idle", message = "" })"#).await;
        t.vt.focus(false);
        t.vt.frame().await;
        let _ = t.vt.output();
        exec(&t.vt.host, r#"C.cordis:emit("status/changed", { state = "blocked", message = "Approve tool call" })"#)
            .await;
        for _ in 0..4 {
            t.vt.frame().await;
            let out = String::from_utf8_lossy(&t.vt.output()).into_owned();
            if out.contains("\x1b]9;") {
                assert!(out.contains("Approve tool call"), "unexpected notification payload: {out:?}");
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("expected unfocused blocked event with same text not to be swallowed by dedupe_ms");
    })
    .await
}

/// Typing past a cached completion prefix and pressing Enter before the new async completion
/// reply returns must discard the stale popup and submit the current editor text instead of
/// accepting and auto-executing the stale candidate.
#[tokio::test]
async fn enter_does_not_accept_stale_completion_popup_after_typing() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.dismiss_notices();
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            GATE, WAIT = mare.task.channel()
            EXPORT_STARTED = false
            SUBMITTED = {}
            C.cordis:on("input/submit", function(ev)
                SUBMITTED[#SUBMITTED + 1] = ev.text
                ev.handled = true
            end)
            mare.ui.completion.register({
                name = "stale-enter-test",
                triggers = { "/" },
                min_prefix = 1,
                timeout_ms = 5000,
                complete = function(ev)
                    if ev.token == "/ex" then
                        return { { label = "/exit", insert = "/exit", auto_execute = true } }
                    end
                    if ev.token == "/export" then
                        EXPORT_STARTED = true
                        WAIT:recv()
                        return { { label = "/export", insert = "/export", auto_execute = true } }
                    end
                    return {}
                end,
            })
            "#,
        )
        .await;

        // Open the completion popup for `/ex` (selecting auto-execute `/exit`).
        t.vt.type_str("/ex");
        t.until("stale completion popup opens", |s| s.contains("/exit")).await;

        // Keep typing `port` so the editor holds `/export` while `/export`'s completion is still
        // in flight, then press Enter immediately.
        t.vt.type_str("port");
        t.until_vt("export completion started", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return EXPORT_STARTED")).unwrap_or(false)
        })
        .await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("submit recorded", |t| {
            futures_util::FutureExt::now_or_never(eval::<i64>(&t.vt.host, "return #SUBMITTED")).unwrap_or(0) >= 1
        })
        .await;
        let submitted: String = eval(&t.vt.host, "return SUBMITTED[1]").await;
        assert_eq!(submitted, "/export", "Enter must submit /export rather than accepting stale /exit");

        // Unblock the suspended completion task so shutdown is clean.
        exec(&t.vt.host, "GATE:send(true)").await;
        t.step().await;

        // Positive case: when the popup matches the current token, Enter still accepts and runs it.
        t.vt.type_str("/ex");
        t.until("fresh completion popup opens", |s| s.contains("/exit")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("second submit recorded", |t| {
            futures_util::FutureExt::now_or_never(eval::<i64>(&t.vt.host, "return #SUBMITTED")).unwrap_or(0) >= 2
        })
        .await;
        let accepted: String = eval(&t.vt.host, "return SUBMITTED[2]").await;
        assert_eq!(accepted, "/exit", "Enter on a fresh matching popup must still accept and execute /exit");
    })
    .await
}

#[tokio::test]
async fn notify_delivers_when_focus_reporting_never_arrived_repro_280() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["status", "notify"]).faux(json!([])).config(json!({
            "logo": false,
            "plugins": {
                "permissions": { "enabled": true },
                "onboarding": { "plugins": false },
                "notify": {
                    "enabled": true,
                    "only_unfocused": true,
                    "protocol": "osc9",
                    "dedupe_ms": 0
                }
            }
        }));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 80, 16).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(16), nz(80), 2000));
        t.step().await;
        t.until_vt("startup settled", |t| t.vt.startup_finished()).await;
        t.vt.wait_startup_settled().await;
        t.vt.reset_loop_state();
        expose(&t.vt.host);

        // Simulate a terminal that never sends FocusIn/FocusOut: user presses a key only.
        t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('a'), Modifiers::NONE)));
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        let _ = t.vt.output();

        // Trigger an eligible notification with only_unfocused=true; focus reporting never arrived.
        let sent: bool = eval(
            &t.vt.host,
            r#"return mare.ui.notify_os("Turn finished", { only_unfocused = true, protocol = "osc9" })"#,
        )
        .await;
        assert!(
            sent,
            "notify_os with only_unfocused=true must still deliver when terminal never reported FocusIn/FocusOut (focus_known={})",
            t.vt.ui().focus_known.load(std::sync::atomic::Ordering::Relaxed)
        );
    })
    .await
}

#[tokio::test]
async fn notify_delivers_turn_completion_after_input_without_focus_reports_and_suppresses_after_focus_in() {
    local(async {
        let env = TestEnv::new().await;
        let mut b =
            env.host().builtins(&["status", "notify"]).faux(json!([{ "text": "done without focus" }])).config(json!({
                "logo": false,
                "plugins": {
                    "permissions": { "enabled": true },
                    "onboarding": { "plugins": false },
                    "notify": {
                        "enabled": true,
                        "only_unfocused": true,
                        "min_turn_ms": 0,
                        "protocol": "osc9",
                        "dedupe_ms": 0
                    }
                }
            }));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 80, 16).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(16), nz(80), 2000));
        t.step().await;
        t.until_vt("startup settled", |t| t.vt.startup_finished()).await;
        t.vt.wait_startup_settled().await;
        t.vt.reset_loop_state();
        expose(&t.vt.host);

        // Input events alone (Key, Paste, Mouse) must not claim focus-report availability.
        t.vt.send_tty_event(mare_tui::tty::Event::Paste("hi".into()));
        t.vt.send_tty_event(mare_tui::tty::Event::Mouse(mare_tui::tty::MouseEvent {
            kind: mare_tui::tty::MouseEventKind::Moved,
            column: 1,
            row: 1,
            modifiers: Modifiers::NONE,
        }));
        t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('!'), Modifiers::NONE)));
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        assert!(!t.vt.ui().focus_known.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await, None);
        let _ = t.vt.output();

        // Submit the editor via Enter (still no FocusIn/FocusOut) and wait for turn completion.
        t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE)));
        let mut saw_osc9 = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            t.vt.run_production_loop_for(Duration::from_millis(30)).await;
            let out = String::from_utf8_lossy(&t.vt.output()).into_owned();
            if out.contains("\x1b]9;") {
                saw_osc9 = true;
                break;
            }
        }
        assert!(saw_osc9, "expected OSC 9 turn-completion notification when focus reporting never arrived");
        assert!(!t.vt.ui().focus_known.load(std::sync::atomic::Ordering::Relaxed));

        // Once the terminal reports FocusIn, focus is known and only_unfocused suppresses notifications.
        t.vt.send_tty_event(mare_tui::tty::Event::FocusIn);
        t.vt.run_production_loop_for(Duration::from_millis(30)).await;
        assert!(t.vt.ui().focus_known.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await, Some(true));
        let sent_while_focused: bool =
            eval(&t.vt.host, r#"return mare.ui.notify_os("Suppressed", { only_unfocused = true, protocol = "osc9" })"#)
                .await;
        assert!(!sent_while_focused, "notify_os with only_unfocused=true must suppress once FocusIn arrived");

        // After FocusOut, a subsequent keypress still restores known focused=true.
        t.vt.send_tty_event(mare_tui::tty::Event::FocusOut);
        t.vt.run_production_loop_for(Duration::from_millis(30)).await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await, Some(false));
        t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('x'), Modifiers::NONE)));
        t.vt.run_production_loop_for(Duration::from_millis(30)).await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.ui.focused()").await, Some(true));
    })
    .await
}
