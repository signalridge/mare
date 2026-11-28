//! What the screen shows: scrollback, resize, markdown, colour and motion settings.

use super::*;

#[tokio::test]
async fn editor_border_dims_when_terminal_focus_is_lost() {
    local(async {
        use std::sync::atomic::Ordering;
        let mut t = Term::colored(json!([]), 80, 20).await;
        t.until("the editor", |s| s.contains("Ask mare anything")).await;
        let border_color = |t: &Term| {
            let screen = t.parser.screen();
            let row = (0..screen.size().0.get())
                .find(|&r| screen.cell(r, 0).is_some_and(|cell| cell.contents() == "╭"))
                .unwrap();
            screen.cell(row, 0).unwrap().fgcolor()
        };
        let focused = border_color(&t);
        t.vt.host.ui.focus_known.store(true, Ordering::Relaxed);
        t.vt.host.ui.focused.store(false, Ordering::Relaxed);
        t.step().await;
        assert_ne!(border_color(&t), focused, "unfocused input should use a quieter border");
        t.vt.host.ui.focused.store(true, Ordering::Relaxed);
        t.step().await;
        assert_eq!(border_color(&t), focused, "focus restores the original border");
    })
    .await
}

// Windows defaults to PARTIAL enforcement; retain its exact status bar and layout in snapshots.

#[tokio::test]
async fn an_overflowing_answer_commits_to_scrollback_once_and_keeps_the_input_box() {
    local(async {
        let body: String = (1..=40).map(|i| format!("line {i:02}\n\n")).collect();
        let mut t = Term::new(json!([{ "text": body }]), 50, 12).await;
        t.submit("long please").await;
        t.idle().await;
        let hist = t.history();
        for i in [1, 20, 40] {
            let needle = format!("line {i:02}");
            assert_eq!(hist.matches(&needle).count(), 1, "{needle} committed exactly once:\n{hist}");
        }
        // the earliest lines left the screen; the input box is still drawn at the bottom
        let screen = t.screen();
        assert!(!screen.contains("line 01"), "{screen}");
        assert!(screen.contains('╰'), "input box at the bottom:\n{screen}");
        #[cfg(windows)]
        insta::assert_snapshot!("overflow_screen_windows", t.snap().await);
        #[cfg(not(windows))]
        insta::assert_snapshot!("overflow_screen", t.snap().await);
    })
    .await
}

#[tokio::test]
async fn resizing_rewraps_history_and_a_resize_mid_stream_loses_nothing() {
    local(async {
        let para = "alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo lima mike \
                    november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu";
        let slow = format!("{para} END-OF-STREAM");
        // Keep the same permissions-introduction card on all platforms. Windows otherwise adds
        // a one-time network caveat and (outside modern terminals) a legacy-console terminal hint,
        // widening the first-launch card and changing the number of visible rows before this snapshot.
        let mut t = Term::build(
            json!([{ "text": para }, { "text": slow, "delay_ms": 5 }]),
            json!({}),
            json!({}),
            90,
            20,
            |env| {
                let state = env.home.path().join("state");
                std::fs::create_dir_all(&state).unwrap();
                std::fs::write(state.join("state.json"), r#"{"version":1,"sandbox_setup_hint_shown":true}"#).unwrap();
                std::fs::write(state.join("terminal-hint-shown"), "").unwrap();
            },
        )
        .await;
        t.submit("first").await;
        t.idle().await;
        t.resize(40, 20).await;
        let screen = t.screen();
        assert!(screen.lines().all(|l| l.trim_end().chars().count() <= 40), "rewrapped to 40:\n{screen}");
        assert!(screen.contains("zulu"), "{screen}");
        // resize while the second answer streams: it ends once, complete
        t.submit("second").await;
        // the answer has started and not finished, read from the app rather than the screen: the
        // screen can't tell: the first answer may still be on it. The stream needs ~20 timed
        // chunks, far more than one frame: the resize lands mid-stream
        t.until_vt("streaming to start", |t| {
            let src = t.vt.live_source();
            t.vt.busy() && src.contains("alpha") && !src.contains("END-OF-STREAM")
        })
        .await;
        t.resize(70, 20).await;
        t.idle().await;
        let screen = t.snap().await;
        let hist = t.history();
        assert_eq!(hist.matches("END-OF-STREAM").count(), 1, "{hist}");
        let words = hist.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(words.matches(para).count(), 2, "both answers must survive the resize:\n{hist}");
        assert_eq!(hist.matches("❯ second").count(), 1, "the second prompt must survive the resize:\n{hist}");
        #[cfg(windows)]
        insta::assert_snapshot!("after_resize_mid_stream_windows", screen);
        #[cfg(not(windows))]
        insta::assert_snapshot!("after_resize_mid_stream", screen);
    })
    .await
}

/// A long notice can push rows into scrollback during resize; neither answer may be lost.
#[tokio::test]
async fn a_long_warning_during_midstream_resize_keeps_the_full_conversation() {
    local(async {
        let para = "alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo lima mike \
                    november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu";
        let slow = format!("{para} END-OF-STREAM");
        let mut t = Term::new(json!([{ "text": para }, { "text": slow, "delay_ms": 5 }]), 90, 20).await;
        t.submit("first").await;
        t.idle().await;
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"mare.ui.notify("Permissions: READ (read anywhere; writing files or going online needs your approval) — /permission to change\nNetwork isn't sandboxed on this tier (PARTIAL): `mare sandbox setup`", "warning")"#,
        )
        .await;
        t.until("the warning card", |s| s.contains("PARTIAL")).await;
        t.resize(40, 20).await;
        t.submit("second").await;
        t.until_vt("streaming to start", |t| {
            let src = t.vt.live_source();
            t.vt.busy() && src.contains("alpha") && !src.contains("END-OF-STREAM")
        })
        .await;
        t.resize(70, 20).await;
        t.idle().await;
        let screen = t.snap().await;
        assert!(screen.contains("END-OF-STREAM") && screen.contains("Ask mare anything"), "{screen}");
        let hist = t.history();
        let words = hist.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(words.matches(para).count(), 2, "both answers survive:\n{hist}");
        assert_eq!(hist.matches("❯ second").count(), 1, "second prompt survives:\n{hist}");
        assert_eq!(hist.matches("END-OF-STREAM").count(), 1, "{hist}");
    })
    .await
}

#[tokio::test]
async fn wide_characters_and_emoji_take_two_columns() {
    local(async {
        // multi-byte characters stream across chunk boundaries (faux cuts ~8-byte chunks)
        let mut t = Term::new(json!([{ "text": "中文😀ok 中文😀ok" }]), 50, 24).await;
        t.submit("w").await;
        t.idle().await;
        t.until("the answer", |s| s.contains("中文")).await;
        let screen = t.parser.screen();
        let (rows, cols) = (screen.size().0.get(), screen.size().1.get());
        let mut found = false;
        for r in 0..rows {
            for c in 0..cols {
                if screen.cell(r, c).is_some_and(|x| x.contents() == "中") {
                    found = true;
                    let at = |dc: u16| screen.cell(r, c + dc).map(|x| x.contents().to_string()).unwrap_or_default();
                    assert!(screen.cell(r, c).unwrap().is_wide(), "中 is wide");
                    assert_eq!(at(2), "文", "文 starts two columns later");
                    assert_eq!(at(4), "😀", "the emoji starts after two wide characters");
                    assert_eq!(at(6), "o", "the emoji takes two columns");
                }
            }
        }
        assert!(found, "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn markdown_answers_render_with_the_styled_elements() {
    local(async {
        let md = "# Title\n\nSome `code` here.\n\n- one\n  - nested\n\n```rust\nfn main() {}\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n> quoted";
        let mut t = Term::new(json!([{ "text": md }]), 60, 30).await;
        t.submit("md").await;
        t.idle().await;
        let screen = t.screen();
        assert!(screen.contains("Title") && !screen.contains("# Title"), "headings without #:\n{screen}");
        assert!(screen.contains("rust"), "code card language label:\n{screen}");
        assert!(screen.contains('▎'), "quote bar:\n{screen}");
        #[cfg(windows)]
        insta::assert_snapshot!("markdown_answer_windows", t.snap().await);
        #[cfg(not(windows))]
        insta::assert_snapshot!("markdown_answer", t.snap().await);
    })
    .await
}

/// A user message is a full-width bar in the selection color, with the ❯ prompt.
#[tokio::test]
async fn user_messages_are_a_full_width_bar() {
    local(async {
        // This test checks the colored presentation even when the invoking shell uses NO_COLOR.
        // Nextest runs each test in its own process, as required by this suite's environment setup.
        let no_color = std::env::var_os("NO_COLOR");
        unsafe { std::env::remove_var("NO_COLOR") };
        let mut t = Term::new(json!([{ "text": "ok" }]), 60, 16).await;
        if let Some(value) = no_color {
            unsafe { std::env::set_var("NO_COLOR", value) };
        }
        t.submit("hello bar").await;
        t.idle().await;
        let screen = t.parser.screen();
        let row = (0..16u16)
            .find(|&r| screen.contents_between(r, 0, r, 60).contains("❯ hello bar"))
            .expect("the user message row");
        let bg = |c: u16| screen.cell(row, c).map(|c| c.bgcolor()).unwrap_or_default();
        assert_ne!(bg(0), vt100::Color::Default, "the bar starts at the left edge");
        assert_eq!(bg(0), bg(4), "under the text too");
        assert_eq!(bg(0), bg(59), "and reaches the right edge");
    })
    .await
}

/// NO_COLOR keeps every color out of the frames (bold and reverse stay).
#[tokio::test]
async fn no_color_writes_no_color_sequences() {
    local(async {
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let mut t = Term::new(json!([{ "text": "plain **bold** `code`" }]), 60, 16).await;
        t.submit("hi").await;
        t.idle().await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        // the menu can take more than one frame to appear (Windows CI): wait for it, then move the
        // selection so the next frame redraws the selected row (waiting consumed earlier output)
        t.until("the key menu", |s| s.contains("─ Keys ─")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.vt.frame().await;
        let out = String::from_utf8_lossy(&t.vt.output()).into_owned();
        unsafe { std::env::remove_var("NO_COLOR") };
        for bad in ["[38;", "[48;", "[3m", "[2m"] {
            assert!(!out.contains(bad), "{bad:?} in a NO_COLOR frame: {out:?}");
        }
        assert!(out.contains("\x1b[7m"), "the selected row is reverse video");
    })
    .await
}

/// `reduce_motion` draws a static marker instead of the spinner.
#[tokio::test]
async fn reduce_motion_holds_the_spinner_still() {
    local(async {
        let slow: String = "word ".repeat(60);
        let mut t =
            Term::with(json!([{ "text": slow, "delay_ms": 30 }]), json!({ "reduce_motion": true }), 80, 16).await;
        t.submit("go").await;
        t.until("the working line", |s| s.lines().any(|l| l.contains("▂▃▅▃") && l.contains("Esc to interrupt"))).await;
        let spinning = |s: &str| s.chars().any(|c| mare_shared::style::SPINNER_FRAMES.iter().any(|g| g.starts_with(c)));
        assert!(!spinning(&t.screen()), "no braille spinner:\n{}", t.screen());
        t.idle().await;
    })
    .await
}

/// The working line is the ui plugin's (`mare.ui.Working`, drawn with ui.Frames): its glyph
/// frames come from the option's data, not from the core.
#[tokio::test]
async fn the_working_line_draws_glyph_frames_from_data() {
    local(async {
        let slow: String = "word ".repeat(60);
        let script = json!([{ "text": slow, "delay_ms": 30 }]);
        let look = json!({ "ui": { "working": { "frames": { "frames": ["<>"], "ms": 100 } } } });
        let mut t = Term::with(script, look, 80, 16).await;
        t.submit("go").await;
        t.until("the data-driven glyph", |s| s.lines().any(|l| l.starts_with("<> Responding…"))).await;
        t.idle().await;
    })
    .await
}

/// A notification card never covers the working line (it used to overlay the right of the live
/// area, cutting "… esc to interrupt)" off): the live area grows above the line instead. The card
/// is posted while the answer streams, taller than the rows above the working line (never the
/// startup card: on Windows the first-launch terminal hint makes that one a "Warning" card, and
/// it may expire before the turn starts), and the answer streams until Esc ends it.
#[tokio::test]
async fn a_notification_card_never_covers_the_working_line() {
    local(async {
        let slow: String = "word ".repeat(4000);
        let mut t = Term::with(json!([{ "text": slow, "delay_ms": 20 }]), json!({}), 80, 24).await;
        t.submit("go").await;
        t.until("the working line", |s| s.contains("Responding…")).await;
        t.vt.dismiss_notices();
        exec(&t.vt.host, "mare.ui.notify('CARD-MARK a tall notice\\ntwo\\nthree\\nfour', 'info')").await;
        t.until("the card above the working line", |s| s.contains("CARD-MARK") && s.contains("Responding…")).await;
        // every frame while the card shows: the working line is whole
        let end = Instant::now() + Duration::from_millis(400);
        while Instant::now() < end {
            let s = t.screen();
            if let Some(l) = s.lines().find(|l| l.contains("Responding…")) {
                assert!(l.contains("Esc to interrupt)"), "the card covers the working line:\n{s}");
            }
            t.step().await;
        }
        assert!(t.screen().contains("CARD-MARK"), "the card still shows:\n{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("the turn to end", |t| !t.vt.busy()).await;
    })
    .await
}

/// On a narrow terminal a card covers most of each row it is drawn on (the Windows first-run
/// card is widened to the full width): it never overlays the live stream. While the live area
/// fits on screen it sits in rows added above the answer; every word streamed so far stays on
/// screen in every frame.
#[tokio::test]
async fn a_card_on_a_narrow_terminal_never_covers_the_streaming_answer() {
    local(async {
        let words: String = (1..=60).map(|i| format!("w{i:02} ")).collect();
        let mut t = Term::with(json!([{ "text": words, "delay_ms": 30 }]), json!({}), 40, 24).await;
        t.submit("go").await;
        t.until_vt("a few streamed rows", |t| t.vt.live_source().contains("w20")).await;
        t.vt.dismiss_notices();
        exec(&t.vt.host, "mare.ui.notify('CARD-MARK narrow', 'info')").await;
        let mut card_seen = false;
        let end = Instant::now() + WAIT;
        while t.vt.busy() {
            t.step().await;
            let (screen, src) = (t.screen(), t.vt.live_source());
            card_seen |= screen.contains("CARD-MARK");
            // the last word may still be partial
            let streamed: Vec<&str> = src.split_whitespace().filter(|w| w.len() == 3 && w.starts_with('w')).collect();
            for w in streamed.iter().take(streamed.len().saturating_sub(1)) {
                assert!(screen.contains(w), "{w} is hidden (by the card?):\n{screen}");
            }
            assert!(Instant::now() < end, "turn never ended:\n{screen}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(card_seen, "the card shows above the answer while it fits:\n{}", t.screen());
    })
    .await
}

/// TERM=dumb runs a plain line-by-line session — no escape sequences at all, the
/// answer printed as text.
#[tokio::test]
async fn the_dumb_terminal_session_is_plain_text() {
    local(async {
        let env = TestEnv::new().await;
        // the first-open extras offer would take the first input line (the plain frontend asks it too)
        let mut b = env
            .host()
            .builtins(&["copy"])
            .faux(json!([{ "text": "Hello **there**\n\n- one\n- two" }]))
            .config(json!({ "plugins": { "onboarding": { "plugins": false } } }));
        b.opts.mode = Mode::Tui;
        let out = mare_cli::testkit::plain_session(b.opts, 60, "hi\n").await.unwrap();
        assert!(!out.contains('\x1b'), "no escape sequences: {out:?}");
        assert!(out.contains("Hello there") && out.contains("one") && out.contains("two"), "{out}");
    })
    .await
}

/// "窗口缩放" (Codex): every resize clears the scrollback (CSI 3J) and replays the
/// whole conversation at the new width — one copy of it in the terminal, however often the
/// window changes (the old "visible" replay left a screenful behind per resize).
#[tokio::test]
async fn resizing_keeps_one_copy_of_the_conversation() {
    local(async {
        let para = "alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo lima mike \
                    november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu";
        let mut t = Term::new(json!([{ "text": para }, { "text": "second answer" }]), 90, 12).await;
        t.submit("first").await;
        t.idle().await;
        t.submit("second").await;
        t.idle().await;
        for (w, h) in [(40, 12), (90, 12), (60, 10), (90, 12)] {
            t.resize(w, h).await;
            let hist = t.history();
            assert_eq!(hist.matches("zulu").count(), 1, "{w}x{h}:\n{hist}");
            assert_eq!(hist.matches("second answer").count(), 1, "{w}x{h}:\n{hist}");
        }
        t.vt.dismiss_notices();
        t.step().await;
        #[cfg(windows)]
        insta::assert_snapshot!("resize_full_replay_windows", trimmed(&t.history()));
        #[cfg(not(windows))]
        insta::assert_snapshot!("resize_full_replay", trimmed(&t.history()));
    })
    .await
}

/// A partial sandbox must remain visible even when read results are collapsed.
#[tokio::test]
async fn collapsed_read_group_displays_partial_deny_read() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        t.vt.host.emit_json("session/loaded", json!({ "messages": [
            {"role": "user", "content": [{"type": "text", "text": "go"}]},
            {"role": "assistant", "content": [{"type": "tool_call", "id": "p", "name": "read", "arguments": {"path": "a.txt"}}]},
            {"role": "tool_result", "tool_call_id": "p", "tool_name": "read", "content": [
                {"type": "text", "text": "visible\n[sandbox: PARTIAL; unavailable: fs_deny_read]"}], "is_error": false},
            {"role": "assistant", "content": [{"type": "text", "text": "done"}]}
        ]})).await;
        t.until("partial read group", |s| s.contains("PARTIAL: fs_deny_read")).await;
        assert!(t.history().contains("PARTIAL: fs_deny_read"));
    }).await
}

/// Consecutive reads, searches and listings are one group whose header counts them.
#[tokio::test]
async fn an_exploration_group_header_counts_what_was_read_and_searched() {
    local(async {
        let mut t = Term::new(json!([]), 80, 30).await;
        let call = |id: &str, cmd: &str| json!({ "type": "tool_call", "id": id, "name": "bash", "arguments": { "command": cmd } });
        let result = |id: &str| json!({ "role": "tool_result", "tool_call_id": id, "tool_name": "bash",
            "content": [{ "type": "text", "text": "ok" }], "is_error": false, "details": { "code": 0, "output": "ok" } });
        let cmds = ["cat a.txt", "cat b.txt", "cat a.txt", "rg foo src", "ls docs"];
        let calls: Vec<_> = cmds.iter().enumerate().map(|(i, c)| call(&format!("c{i}"), c)).collect();
        let mut msgs = vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "go" }] }),
            json!({ "role": "assistant", "content": calls }),
        ];
        msgs.extend((0..cmds.len()).map(|i| result(&format!("c{i}"))));
        msgs.push(json!({ "role": "assistant", "content": [{ "type": "text", "text": "all done" }] }));
        t.vt.host.emit_json("session/loaded", json!({ "messages": msgs })).await;
        t.until("the replay", |s| s.contains("all done")).await;
        let h = t.history();
        assert!(h.contains("Read 2 files, searched 1 pattern, listed 1 directory"), "the counts:\n{h}");
        assert!(!h.contains("Explored"), "no bare header:\n{h}");
    })
    .await
}

/// Hook error display: a handler that fails during a turn is shown
/// under that turn's prompt as one short line, every turn, with no notice card and no traceback;
/// outside a turn it is the one-line notice.
#[tokio::test]
async fn a_failing_hook_is_shown_under_each_prompt() {
    local(async {
        let mut t = Term::new(json!([{ "text": "one" }, { "text": "two" }]), 90, 30).await;
        expose(&t.vt.host);
        exec(&t.vt.host, r#"C.cordis:on("turn/started", function() error("hook boom") end)"#).await;
        for prompt in ["first", "second"] {
            t.submit(prompt).await;
            t.idle().await;
        }
        t.until("two inline lines", |s| s.matches("hook `turn/started` failed: ").count() == 2).await;
        let screen = t.screen();
        assert!(screen.contains("⎿  plugin `") && screen.contains("hook boom"), "{screen}");
        assert!(!screen.contains("see /plugins") && !screen.contains("traceback"), "no card, no traceback:\n{screen}");
        exec(&t.vt.host, r#"C.cordis:on("probe/idle", function() error("idle boom") end)"#).await;
        t.vt.host.kernel.emit("probe/idle", mlua::MultiValue::new()).await;
        t.until("the notice outside a turn", |s| s.contains("idle boom") && s.contains("mare doctor")).await;
        assert!(t.screen().contains("mare doctor"), "plugin error needs a CLI recovery path: {}", t.screen());
    })
    .await
}

/// Without a custom `AssistantMessage.render`, both live streaming (`finish_text`) and
/// `session/loaded` replay must display the `Sources:` list when an assistant message has citations.
#[tokio::test]
async fn live_and_replayed_assistant_answers_display_sources_without_custom_renderer() {
    local(async {
        let mut t = Term::new(json!([]), 90, 30).await;
        let citations = json!([
            { "url": "https://example.com/docs", "title": "Example Docs" },
            { "url": "https://example.org/guide", "title": "Example Guide" }
        ]);

        // 1. Live streaming path: item/delta text chunks followed by assistant_message item/completed + turn/completed.
        t.vt.host.emit_json("turn/started", json!({ "turn_id": "t1", "trigger": "prompt" })).await;
        t.vt.host
            .emit_json(
                "item/started",
                json!({
                    "turn_id": "t1",
                    "item_id": "m1",
                    "item": { "id": "m1", "type": "assistant_message", "message": { "role": "assistant" } }
                }),
            )
            .await;
        t.vt.host
            .emit_json(
                "item/delta",
                json!({ "turn_id": "t1", "item_id": "m1", "kind": "text", "delta": "Answer ", "offset": 0 }),
            )
            .await;
        t.vt.host
            .emit_json(
                "item/delta",
                json!({ "turn_id": "t1", "item_id": "m1", "kind": "text", "delta": "with citation.", "offset": 7 }),
            )
            .await;
        t.vt.host
            .emit_json(
                "item/completed",
                json!({
                    "turn_id": "t1",
                    "item_id": "m1",
                    "status": "completed",
                    "item": {
                        "id": "m1",
                        "type": "assistant_message",
                        "message": {
                            "role": "assistant",
                            "content": [{
                                "type": "text",
                                "text": "Answer with citation.",
                                "citations": citations,
                            }]
                        }
                    }
                }),
            )
            .await;
        t.vt.host.emit_json("turn/completed", json!({ "turn_id": "t1", "status": "completed" })).await;

        t.until("live sources list", |s| {
            s.contains("Answer with citation.")
                && s.contains("Sources:")
                && s.contains("Example Docs")
                && s.contains("Example Guide")
        })
        .await;
        let live_hist = t.history();
        assert!(live_hist.contains("Sources:"), "live history missing Sources:\n{live_hist}");
        assert!(live_hist.contains("Example Docs"), "live history missing Example Docs:\n{live_hist}");
        assert!(live_hist.contains("Example Guide"), "live history missing Example Guide:\n{live_hist}");

        // 2. Replay path: session/loaded clears and replays the same assistant message.
        t.vt.host
            .emit_json(
                "session/loaded",
                json!({
                    "messages": [
                        { "role": "user", "content": [{ "type": "text", "text": "search" }] },
                        {
                            "role": "assistant",
                            "content": [{
                                "type": "text",
                                "text": "Answer with citation.",
                                "citations": citations,
                            }]
                        }
                    ]
                }),
            )
            .await;

        t.until("replayed sources list", |s| {
            s.contains("Resumed session")
                && s.contains("Answer with citation.")
                && s.contains("Sources:")
                && s.contains("Example Docs")
                && s.contains("Example Guide")
        })
        .await;
        let replay_hist = t.history();
        assert!(replay_hist.contains("Sources:"), "replay history missing Sources:\n{replay_hist}");
        assert!(replay_hist.contains("Example Docs"), "replay history missing Example Docs:\n{replay_hist}");
        assert!(replay_hist.contains("Example Guide"), "replay history missing Example Guide:\n{replay_hist}");
    })
    .await
}

/// color depth and icon set share one resolution path between Rust and Lua
/// (`option > env > probe > default`), and `terminal.{color,probe.{enabled,background}}` are valid options.
#[tokio::test]
async fn color_depth_and_icons_follow_single_resolution_path_between_rust_and_lua() {
    local(async {
        {
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(
                env.config_dir().join("init.lua"),
                r#"mare.setup {
                    terminal = {
                        color = "16",
                        probe = { enabled = true, background = false },
                    },
                    icons = "ascii",
                }"#,
            )
            .unwrap();
            let mut hb = env.host();
            hb.opts.load_init = true;
            let h = hb.no_session().build().await;
            let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
            assert!(warnings.is_empty(), "terminal option schema should accept terminal.{{color,probe}}: {warnings:?}");
            h.shutdown().await;
        }

        let mut t = Term::with(
            json!([{ "text": "**bold** text" }]),
            json!({
                "terminal": { "color": "16", "probe": { "enabled": true, "background": false } },
                "icons": "ascii",
                "border": "single",
            }),
            60,
            16,
        )
        .await;
        t.submit("hi").await;
        t.idle().await;
        let out = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(!out.contains("\x1b[38;2;") && !out.contains("\x1b[38;5;"), "16-color mode must not emit 256/RGB SGR: {out:?}");

        let (depth, icon_set, detected_icons, caps_depth, caps_icons): (String, String, String, String, String) = eval(
            &t.vt.host,
            r#"local c = mare.ui.caps()
               return mare.ui.color_depth(), mare.ui.icon_set(), mare.ui.detect_icons(), tostring(c.color_depth), tostring(c.icons)"#,
        )
        .await;
        assert_eq!(depth, "16", "terminal.color = '16' must win over env");
        assert_eq!(caps_depth, depth, "Rust caps().color_depth and Lua ui.color_depth() must match");
        assert_eq!(icon_set, "ascii");
        assert_eq!(detected_icons, "ascii", "detect_icons() must honor explicit option > caps > default");
        assert_eq!(caps_icons, "ascii", "caps().icons must report resolved icon set");
    })
    .await
}

/// degraded ASCII mode keeps ASCII glyphs in UI icons and notices without emoji/nerd chrome.
#[tokio::test]
async fn degraded_ascii_mode_uses_ascii_icons_without_emoji_or_nerd_chrome() {
    local(async {
        let mut t =
            Term::with(json!([{ "text": "ascii ok" }]), json!({ "icons": "ascii", "border": "single" }), 60, 16).await;
        let (icon_set, info_g, warn_g, caps_icons): (String, String, String, String) = eval(
            &t.vt.host,
            r#"return mare.ui.icon_set(), mare.ui.icon("info"), mare.ui.icon("warn"), tostring(mare.ui.caps().icons)"#,
        )
        .await;
        assert_eq!(icon_set, "ascii");
        assert_eq!(caps_icons, "ascii");
        assert_eq!((info_g.as_str(), warn_g.as_str()), ("i", "!"));
        t.vt.host.emit_json("app/ready", json!({})).await;
        let _: () = eval(&t.vt.host, r#"mare.ui.notify("ascii notice", "warn")"#).await;
        t.until("ascii notice card", |s| s.contains("ascii notice")).await;
        let scr = t.screen();
        assert!(!scr.contains('ℹ') && !scr.contains('⚠') && !scr.contains('✖'), "non-ASCII notice glyph found:\n{scr}");
    })
    .await
}

/// narrow terminal width keeps the editor prompt/cursor and `keep` status segment usable.
#[tokio::test]
async fn degraded_narrow_width_preserves_editor_and_keep_status() {
    local(async {
        let mut t = Term::with(json!([{ "text": "narrow done" }]), json!({ "border": "single" }), 24, 10).await;
        t.until("permissions notice", |s| s.contains("Permissions:")).await;
        t.vt.dismiss_notices();
        t.step().await;
        t.key(KeyCode::Char('h'), Modifiers::NONE);
        t.key(KeyCode::Char('i'), Modifiers::NONE);
        t.step().await;
        let scr = t.screen();
        assert!(scr.contains("❯") && scr.contains("hi"), "editor content missing at narrow width:\n{scr}");
        assert!(
            scr.contains("AUTO") || scr.contains("READ") || scr.contains("FULL") || scr.contains("PARTIAL"),
            "keep status badge clipped at narrow width:\n{scr}"
        );
    })
    .await
}

/// default working line renders through Lua (`combined`: `swell` + `glint`), `stopping` routes
/// through `mare.ui.Working:render(info)`, unknown preset/glyph emits `did you mean` warnings,
/// and `nil`/error falls back through the generic fallback with a single warning notice.
#[tokio::test]
async fn working_line_defaults_to_combined_swell_stopping_via_lua_and_fallback() {
    local(async {
        // 1. Default working line renders Lua `swell` + `glint` (not Rust braille) and `ui.working.*` works.
        let slow: String = "word ".repeat(80);
        {
            let mut t = Term::with(json!([{ "text": slow.clone(), "delay_ms": 30 }]), json!({}), 80, 16).await;
            t.submit("go").await;
            t.until("default swell glyph in working line", |s| {
                s.contains("Responding…")
                    && s.lines()
                        .any(|l| l.contains("Responding…") && l.chars().any(|c| matches!(c, '▁' | '▂' | '▃' | '▅')))
            })
            .await;
            let stop_rendered: String = eval(
                &t.vt.host,
                r#"
                local line = mare.ui.Working:render({
                    label = "Thinking",
                    stopping = true,
                    still = true,
                    color = "truecolor",
                })
                return mare.json.encode(line)
                "#,
            )
            .await;
            assert!(
                stop_rendered.contains("stopping…") && stop_rendered.contains("Ctrl+C again to force"),
                "Working:render must format info.stopping cleanly, got: {stop_rendered:?}"
            );
            t.idle().await;
            t.vt.shutdown().await;
        }

        // 2. Stopping state flows through `Working:render(info)` with `info.stopping = true` live.
        #[cfg(unix)]
        {
            let script = json!([
                { "tool_calls": [{ "name": "bash", "arguments": { "command": "trap '' INT TERM HUP; echo TRAP_READY; sleep 30", "description": "nap" } }] },
                { "text": "done" }
            ]);
            let cfg = json!({ "plugins": { "permissions": { "preset": "read-only" } } });
            let mut t = Term::with(script, cfg, 80, 20).await;
            let _: () = eval(
                &t.vt.host,
                r#"
                _G.SEEN_STOPPING = false
                local orig = mare.ui.Working.render
                mare.ui.Working.render = function(self, info)
                    if info and info.stopping then
                        _G.SEEN_STOPPING = true
                        return mare.ui.Line({ mare.ui.Span("CUSTOM-STOP " .. tostring(info.label)):hl("MareDim") })
                    end
                    return orig(self, info)
                end
                "#,
            )
            .await;
            t.submit("nap").await;
            t.until("traps installed", |s| s.contains("TRAP_READY")).await;
            t.key(KeyCode::Escape, Modifiers::NONE);
            t.until("custom stopping via Working:render", |s| s.contains("CUSTOM-STOP")).await;
            let seen_stopping: bool = eval(&t.vt.host, "return _G.SEEN_STOPPING == true").await;
            assert!(seen_stopping, "stopping indicator must route through mare.ui.Working:render with info.stopping=true");
            t.ctrl('c');
            t.idle().await;
            t.vt.shutdown().await;
        }

        // 2b. Throwing in Working:render triggers generic text fallback and logs at most one notice.
        {
            let mut t_fb = Term::with(json!([{ "text": slow, "delay_ms": 30 }]), json!({}), 80, 16).await;
            let _: () = eval(&t_fb.vt.host, r#"mare.ui.Working.render = function() error("boom-working") end"#).await;
            t_fb.submit("go").await;
            t_fb.until("U3 fallback working line", |s| {
                s.contains("Responding…") && s.contains("mare.ui.Working:render failed:")
            })
            .await;
            let notice_count = t_fb.screen().matches("mare.ui.Working:render failed:").count();
            assert_eq!(notice_count, 1, "Working:render error notice must be deduplicated:\n{}", t_fb.screen());
            t_fb.key(KeyCode::Escape, Modifiers::NONE);
            t_fb.idle().await;
            t_fb.vt.shutdown().await;
        }

        // 3. Unknown preset and glyph produce config warnings with `did you mean` suggestions.
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.setup { ui = { working = { preset = "combnd", glyph = "embr" } } }"#,
        )
        .unwrap();
        let mut hb = env.host();
        hb.opts.load_init = true;
        let h = hb.no_session().build().await;
        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        assert!(
            warnings.iter().any(|w| w.contains("ui.working.preset") && w.contains("did you mean") && w.contains("combined")),
            "expected did-you-mean working.preset warning, got: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("did you mean") && w.contains("ember")),
            "expected did-you-mean glyph warning, got: {warnings:?}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn diff_fold_lines_defaults_to_8_folds_expands_and_respects_reduce_motion() {
    local(async {
        let mut t = Term::with(json!([]), json!({ "reduce_motion": true }), 80, 24).await;
        expose(&t.vt.host);
        let (default_fold, rendered_compact, rendered_expanded, rendered_unfolded): (i64, String, String, String) =
            eval(
                &t.vt.host,
                r#"
            local lines_old, lines_new = {}, {}
            for i = 1, 14 do
                lines_old[i] = ("line_%02d_before"):format(i)
                lines_new[i] = ("line_%02d_after"):format(i)
            end
            local old_s = table.concat(lines_old, "\n")
            local new_s = table.concat(lines_new, "\n")
            local diff_node = mare.ui.Diff({ path = "demo.txt", old = old_s, new = new_s })
            local call = { id = "e1", name = "edit", arguments = { path = "demo.txt" } }
            local result = { content = { { type = "text", text = "ok" } } }
            local info = {
                summary = "Edit(demo.txt)",
                text = "ok",
                tool = { render = function() return diff_node end },
            }
            local compact = mare.ui.render_text(mare.ui.ToolCall:render(call, result, false, info), 80)
            local expanded = mare.ui.render_text(mare.ui.ToolCall:render(call, result, true, info), 80)
            local prev = mare.ui.ToolCall.diff_fold
            mare.ui.ToolCall.diff_fold = false
            local unfolded = mare.ui.render_text(mare.ui.ToolCall:render(call, result, false, info), 80)
            mare.ui.ToolCall.diff_fold = prev
            return mare.ui.ToolCall.diff_fold, compact, expanded, unfolded
            "#,
            )
            .await;
        assert_eq!(default_fold, 8, "plugins.ui.diff_fold_lines must default to 8");
        assert!(rendered_compact.contains("more line"), "compact 14-line diff must fold at 8: {rendered_compact}");
        assert!(!rendered_compact.contains("line_14_after"), "compact fold must elide later lines: {rendered_compact}");
        assert!(
            rendered_expanded.contains("line_14_after") && !rendered_expanded.contains("more line"),
            "expanded diff must show every line: {rendered_expanded}"
        );
        assert!(
            rendered_unfolded.contains("line_14_after") && !rendered_unfolded.contains("more line"),
            "diff_fold = false must show full diff even when compact: {rendered_unfolded}"
        );
        // reduce_motion still shows static working glyph without animated spinner cycling.
        exec(&t.vt.host, "mare.ui.working('Steady diff work')").await;
        t.until("static working indicator under reduce_motion", |s| s.contains("Steady diff work")).await;
    })
    .await
}

/// Large `edit`, `write`, and multi-file `apply_patch` diffs fold in compact view (with
/// per-file headers and stats on multi-file patches), show full diffs when expanded,
/// and emit no fold hint for small diffs that fit within the limit.
#[tokio::test]
async fn diff_fold_covers_edit_write_and_multifile_apply_patch() {
    local(async {
        let old_big: String = (1..=120).map(|i| format!("old_row_{i:03}\n")).collect();
        let new_big: String = (1..=120).map(|i| format!("new_row_{i:03}\n")).collect();
        let write_big: String = (1..=120).map(|i| format!("written_row_{i:03}\n")).collect();

        // Session 1: default edit/write mode (120-line edit, 120-line write overwrite, 1-line edit).
        let edit_script = json!([
            { "tool_calls": [
                { "name": "read", "arguments": { "path": "big.txt" } },
                { "name": "read", "arguments": { "path": "over.txt" } },
                { "name": "read", "arguments": { "path": "tiny.txt" } }
            ] },
            { "tool_calls": [{ "name": "edit", "arguments": { "path": "big.txt", "old_string": old_big, "new_string": new_big } }] },
            { "text": "edit done" },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "over.txt", "content": write_big } }] },
            { "text": "write done" },
            { "tool_calls": [{ "name": "edit", "arguments": { "path": "tiny.txt", "old_string": "a\n", "new_string": "b\n" } }] },
            { "text": "tiny edit done" }
        ]);
        let mut t = Term::build(
            edit_script,
            json!({ "plugins": { "permissions": { "enabled": false } } }),
            json!({}),
            90,
            30,
            |env| {
                std::fs::write(env.cwd.join("big.txt"), &old_big).unwrap();
                std::fs::write(env.cwd.join("over.txt"), &old_big).unwrap();
                std::fs::write(env.cwd.join("tiny.txt"), "a\n").unwrap();
            },
        )
        .await;
        t.vt.dismiss_notices();

        t.submit("edit big").await;
        t.idle().await;
        t.until("edit done", |s| s.contains("edit done")).await;
        let h1 = t.history();
        assert!(h1.contains("more line"), "120-line edit must fold in compact view:\n{h1}");
        assert!(!h1.contains("new_row_120"), "120-line edit must elide tail in compact view:\n{h1}");
        t.key(KeyCode::Char('t'), Modifiers::CONTROL);
        t.until("expanded transcript shows edit tail", |s| s.contains("new_row_120")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back to main after edit", |s| !s.contains("Transcript") && s.contains("Ask mare")).await;

        t.submit("write big").await;
        t.idle().await;
        t.until("write done", |s| s.contains("write done")).await;
        let h2 = t.history();
        assert!(!h2.contains("written_row_120"), "120-line write must fold in compact view:\n{h2}");
        assert_eq!(h2.matches("more line").count(), 2, "edit and write must each emit a fold hint:\n{h2}");
        t.key(KeyCode::Char('t'), Modifiers::CONTROL);
        t.until("expanded transcript shows write tail", |s| s.contains("written_row_120")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back to main after write", |s| !s.contains("Transcript") && s.contains("Ask mare")).await;

        t.submit("tiny edit").await;
        t.idle().await;
        t.until("tiny edit done", |s| s.contains("tiny edit done")).await;
        assert_eq!(
            t.history().matches("more line").count(),
            2,
            "small edit must not emit fold hint:\n{}",
            t.history()
        );
    })
    .await;

    local(async {
        let patch_body = |file: &str, prefix: &str, count: usize| -> String {
            use std::fmt::Write;
            let mut s = format!("*** Add File: {file}\n");
            for i in 1..=count {
                let _ = writeln!(s, "+{prefix}_{i:02}");
            }
            s
        };
        let big_patch = format!(
            "*** Begin Patch\n{}{}{}*** End Patch\n",
            patch_body("p1.txt", "alpha", 20),
            patch_body("p2.txt", "beta", 20),
            patch_body("p3.txt", "gamma", 20),
        );
        // Uneven 2-file small patch: 6 rendered rows (stats + 5 lines) + 2 rendered rows
        // (stats + 1 line) = 8 total rows, which fits within the default 8-row shared budget.
        let uneven_small_patch = format!(
            "*** Begin Patch\n{}{}*** End Patch\n",
            patch_body("s1.txt", "s1", 5),
            patch_body("s2.txt", "s2", 1),
        );

        // Session 2: apply_patch mode (3-file large patch + uneven 2-file small patch).
        let patch_script = json!([
            { "tool_calls": [{ "name": "apply_patch", "arguments": { "input": big_patch } }] },
            { "text": "patch done" },
            { "tool_calls": [{ "name": "apply_patch", "arguments": { "input": uneven_small_patch } }] },
            { "text": "small patch done" }
        ]);
        let mut t = Term::build(
            patch_script,
            json!({ "plugins": {
                "permissions": { "enabled": false },
                "tools": { "edit_mode": "apply_patch" }
            } }),
            json!({}),
            90,
            30,
            |_| {},
        )
        .await;
        t.vt.dismiss_notices();

        t.submit("patch big").await;
        t.idle().await;
        t.until("patch done", |s| s.contains("patch done")).await;
        let h1 = t.history();
        let patch_lines: Vec<&str> = h1.lines().collect();
        let head_idx = patch_lines
            .iter()
            .position(|l| l.contains("Patched p1.txt, p2.txt, p3.txt"))
            .unwrap_or_else(|| panic!("missing top-level Patched header:\n{h1}"));
        let end_idx = patch_lines[head_idx + 1..]
            .iter()
            .position(|l| l.contains("patch done"))
            .map(|rel| head_idx + 1 + rel)
            .unwrap_or_else(|| panic!("missing assistant reply after patch:\n{h1}"));
        let body_lines: Vec<&str> =
            patch_lines[head_idx + 1..end_idx].iter().copied().filter(|l| !l.trim().is_empty()).collect();
        let big_hints = body_lines.iter().filter(|l| l.contains("more line")).count();
        assert!(big_hints >= 1, "3-file patch compact view must fold:\n{h1}");
        assert!(
            body_lines.len() <= 8 + 3 + big_hints,
            "3-file patch compact body ({} lines) must stay within limit(8) + 3 headers + {big_hints} hints:\n{h1}",
            body_lines.len()
        );
        for header in ["p1.txt", "p2.txt", "p3.txt"] {
            assert_eq!(
                h1.matches(header).count(),
                2,
                "3-file patch must show {header} in both summary and per-file header:\n{h1}"
            );
            assert!(
                body_lines.iter().any(|l| l.trim().ends_with(header)),
                "3-file patch result body missing per-file header {header}:\n{h1}"
            );
        }
        assert_eq!(h1.matches("+20").count(), 3, "each of the 3 files must show its +20 stats row once:\n{h1}");
        for tail in ["alpha_20", "beta_20", "gamma_20"] {
            assert!(!h1.contains(tail), "3-file patch compact view must elide {tail}:\n{h1}");
        }
        t.key(KeyCode::Char('t'), Modifiers::CONTROL);
        t.until("expanded transcript shows patch tails", |s| s.contains("gamma_20") && s.contains("p3.txt")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back to main after patch", |s| !s.contains("Transcript") && s.contains("Ask mare")).await;

        let hints_before_small = t.history().matches("more line").count();
        t.submit("small patch").await;
        t.idle().await;
        t.until("small patch done", |s| s.contains("small patch done")).await;
        let h2 = t.history();
        for header in ["s1.txt", "s2.txt"] {
            assert_eq!(
                h2.matches(header).count(),
                2,
                "uneven small patch must show {header} in both summary and per-file header:\n{h2}"
            );
        }
        assert!(
            h2.contains("s1_05") && h2.contains("s2_01"),
            "uneven 6+2 small patch must show every diff line without clipping:\n{h2}"
        );
        assert_eq!(
            h2.matches("more line").count(),
            hints_before_small,
            "uneven 6+2 small multi-file patch must not emit fold hint:\n{h2}"
        );
    })
    .await;
}

#[tokio::test]
async fn ui_working_presets_glyphs_tail_leaves_and_spinners() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup {
                ui = {
                    working = {
                        glyph = "phase",
                        label = "foam",
                        elapsed = "seconds",
                        tokens = true,
                        interrupt_hint = false,
                        sep = "bullet",
                        text_throttle_ms = 120,
                        glyphs = { tide = { ms = 100, frames = { "~", "^" }, still = "=", flat = true } },
                    },
                    tool_call = { spinner = "moon" },
                    stopping = { spinner = "wave" },
                },
            }
            "#,
        )
        .unwrap();
        let mut hb = env.host().user_config(true).no_session();
        hb.opts.load_init = true;
        let h = hb.build_exposed().await;
        let check = mare_shared::lua::core::config::check_config_fn(h.lua()).unwrap();
        let _: mlua::Value = check
            .call((mare_shared::lua::json_to_lua(h.lua(), &json!({ "ui": { "working": { "tail": true } } })).unwrap(), "user"))
            .unwrap();
        let warnings: Vec<String> = eval(&h, "return mare.config_warnings()").await;
        assert!(
            warnings.iter().any(|w| w.contains("tail")),
            "unknown ui.working.tail should emit a config warning: {warnings:?}"
        );
        let (default_look, phase_still, wave_ascii, suppressed_tail, leaf_tail, stop_wave, throttle, readonly_err, has_tool_spinner): (
            String,
            String,
            String,
            String,
            String,
            String,
            i64,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
            local W = require("ui.working")
            local WD = require("ui.working_draw")
            local WL = require("ui.working_looks")
            local default_look = W.resolve({})
            local phase_s = mare.ui.render_text(WD.glyph({ glyph = "phase" }, WL, { still = true, color = "truecolor" }), 40)
            local orig_icons = mare.ui.icon_set
            mare.ui.icon_set = function() return "ascii" end
            local wave_a = mare.ui.render_text(WD.glyph({ glyph = "wave" }, WL, { still = true, color = "truecolor" }), 40)
            mare.ui.icon_set = orig_icons
            local off_cfg = W.resolve({ ui = { working = { elapsed = "off", tokens = false, interrupt_hint = false } } })
            local off_t = mare.ui.render_text(WD.tail(off_cfg, { secs = 12, detail = "~1.2k", interrupt_key = "Esc" }) or mare.ui.Line("NONE"), 60)
            local leaf_t = mare.ui.render_text(WD.tail({ elapsed = "seconds", tokens = false, interrupt_hint = true, sep = "bullet" }, { secs = 9, detail = "~1.2k", interrupt_key = "Esc" }), 60)
            local stop_line = mare.ui.render_text(mare.ui.Working:render({ label = "Work", stopping = true, still = true, color = "truecolor" }), 80)
            local ok_ro = pcall(function() mare.ui.Working.frames.ember = {} end)
            return default_look.glyph .. "/" .. default_look.label,
                   phase_s,
                   wave_a,
                   off_t,
                   leaf_t,
                   stop_line,
                   W.text_throttle_ms(),
                   not ok_ro,
                   mare.ui.ToolCall.spinner ~= nil
            "#,
        )
        .await;
        assert_eq!(default_look, "swell/glint");
        assert!(phase_still.contains('◐'), "phase still frame mismatch: {phase_still}");
        assert!(wave_ascii.contains(".-^-"), "wave ascii still frame mismatch: {wave_ascii}");
        assert_eq!(suppressed_tail.trim(), "NONE", "off leaves must suppress elapsed/tokens/interrupt_hint: {suppressed_tail}");
        assert!(leaf_tail.contains("9s • Esc to interrupt") && !leaf_tail.contains("~1.2k"), "per-leaf tail toggle failed: {leaf_tail}");
        assert!(stop_wave.contains("▂▃▅▃") && stop_wave.contains("stopping…"), "ui.stopping.spinner mismatch: {stop_wave}");
        assert_eq!(throttle, 120);
        assert!(readonly_err, "mare.ui.Working.frames must stay read-only");
        assert!(has_tool_spinner, "ui.tool_call.spinner = 'moon' must install spinner");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn ui_working_setup_presets_validation_read_only_caps_and_retired_hints() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup {
                permission = "full-access",
                ui = {
                    working = {
                        glyph = "phase",
                        label = "foam",
                        text_throttle_ms = 17,
                    },
                    working_interval_ms = 55,
                    tool_call = { spinner = "moon" },
                },
                plugins = {
                    ui = { diff_fold_lines = true },
                    commands = { model_picker_key = "<C-m>" },
                    agents = { keys = { next = "<M-]>" } },
                    review = { key = "<C-r>" },
                },
            }
            "#,
        )
        .unwrap();
        let mut hb = env.host().user_config(true).no_session();
        hb.opts.load_init = true;
        let h = hb.build_exposed().await;

        let check = mare_shared::lua::core::config::check_config_fn(h.lua()).unwrap();
        for case in [
            json!({ "ui": { "working": { "preset": "oops", "glyph": "phas" } } }),
            json!({ "ui": { "working": { "frames": 1 } } }),
            json!({ "ui": { "working": { "glyphs": { "bada": { "frames": ["X"], "ascii": true } } } } }),
            json!({ "ui": { "working": { "verbs": { "reasoning": 123 } } } }),
            json!({ "ui": { "working_interval_ms": 80 } }),
        ] {
            let _: mlua::Value =
                check.call((mare_shared::lua::json_to_lua(h.lua(), &case).unwrap(), "user")).unwrap();
        }
        exec(&h, "pcall(function() mare.o.ui.working_interval_ms = 55 end)").await;
        let (perm_nil, custom_g, throttle, tool_node_tag, unset_still, clip_ok, ro_pairs_nil, working_pairs_keys, ro_fn_ok, icons_after_caps, dotted_err, retired_warns): (
            bool,
            String,
            i64,
            String,
            bool,
            bool,
            bool,
            String,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local W = require("ui.working")
            local res = W.resolve()
            -- Default tools = "still" keeps ToolCall.still == true when unset, and explicit
            -- ui.tool_call.spinner = "moon" keeps ToolCall spinning even when working.tools = "still".
            W.apply({ ui = {} })
            local unset_still = (mare.ui.ToolCall.still == true)
            W.apply({ ui = { working = { tools = "still" }, tool_call = { spinner = "moon" } } })
            local tc = mare.ui.ToolCall:render({ name = "bash" }, nil, false, { summary = "ls" })
            local tag = tc and tc.items and tc.items[1] and tc.items[1].type or ""
            W.apply(mare.o)

            -- diff_fold_lines = true folds cleanly at 8 lines
            local ok_diff, diff_node = pcall(function()
                return mare.ui.ToolCall:render(
                    { name = "edit" },
                    { is_error = false },
                    false,
                    {
                        summary = "a.rs",
                        tool = {
                            render = function()
                                return mare.ui.Diff({
                                    path = "a.rs",
                                    hunks = {
                                        {
                                            header = "@@ -1,1 +1,10 @@",
                                            lines = {
                                                { kind = "context", text = "1" },
                                                { kind = "add", text = "2" },
                                                { kind = "add", text = "3" },
                                                { kind = "add", text = "4" },
                                                { kind = "add", text = "5" },
                                                { kind = "add", text = "6" },
                                                { kind = "add", text = "7" },
                                                { kind = "add", text = "8" },
                                                { kind = "add", text = "9" },
                                                { kind = "add", text = "10" },
                                            },
                                        },
                                    },
                                })
                            end,
                        },
                    }
                )
            end)
            local ok_render_diff, rendered_diff = pcall(mare.ui.render_text, diff_node, 80)
            local diff_json = ok_diff and diff_node and mare.json.encode(diff_node) or ""
            local clip_ok = ok_diff and ok_render_diff and diff_json:find('"type":"clip"', 1, true) ~= nil
                and diff_json:find('"max":8', 1, true) ~= nil and type(rendered_diff) == "string"

            -- pairs() state is nil, Working pairs() enumerates render + frames/sweeps, render replaceable
            local next_fn, state = pairs(mare.ui.Working.frames)
            local _, first_k = next_fn(state, nil)
            local wkeys = {}
            for k in pairs(mare.ui.Working) do
                if type(k) == "string" then wkeys[k] = true end
            end
            local wlist = table.concat({
                wkeys.render and "render" or "",
                wkeys.frames and "frames" or "",
                wkeys.sweeps and "sweeps" or "",
            }, ",")
            local ok_render = pcall(function()
                local orig = mare.ui.Working.render
                mare.ui.Working.render = orig
            end)

            -- ui/caps switches icon_set to ascii when emoji_wide = false
            C.cordis:get("bus"):waterfall("ui/caps", { emoji_wide = false })
            local icons = mare.ui.icon_set()

            -- dotted write rejects unknown leaf without storing it
            local ok_dot = pcall(function() mare.o.ui.working = { elapsed = "off", tail = false } end)

            -- fine-grained leaves override defaults cleanly
            local custom_res = W.resolve({ ui = { working = { elapsed = "off", interrupt_hint = true, text_throttle_ms = 75 } } })
            assert(custom_res.elapsed == "off" and custom_res.interrupt_hint == true and custom_res.text_throttle_ms == 75)

            return mare.o.permission == nil,
                   res.glyph,
                   W.text_throttle_ms(),
                   tostring(tag),
                   unset_still,
                   clip_ok,
                   state == nil and first_k ~= nil,
                   wlist,
                   ok_render,
                   icons,
                   not ok_dot and mare.o.ui.working.tail == nil,
                   table.concat(mare.config_warnings(), "\n")
            "#,
        )
        .await;
        assert!(perm_nil, "retired permission must be dropped after warning");
        assert_eq!(custom_g, "phase");
        assert_eq!(throttle, 17);
        assert!(unset_still, "ToolCall.still must be true when user has not overridden ui");
        assert_eq!(tool_node_tag, "frames", "explicit ui.tool_call.spinner must emit frames node: {tool_node_tag}");
        assert!(clip_ok, "diff_fold_lines = true must render folded diff without crashing");
        assert!(ro_pairs_nil, "read_only_view pairs() state must be nil");
        assert_eq!(working_pairs_keys, "render,frames,sweeps");
        assert!(ro_fn_ok, "mare.ui.Working.render replacement must still succeed");
        assert_eq!(icons_after_caps, "ascii");
        assert!(dotted_err, "dotted write with unknown tail must fail without mutating mare.o.ui.working");
        assert!(
            retired_warns.contains("mare.keymap.set")
                && retired_warns.contains("agents.next")
                && retired_warns.contains("<M-Right>")
                && retired_warns.contains("ui.working.text_throttle_ms")
                && retired_warns.contains("plugins.permissions.preset")
                && retired_warns.contains("ui.working.preset")
                && retired_warns.contains("did you mean `phase`")
                && retired_warns.contains("ui.working.frames")
                && retired_warns.contains("bada.ascii")
                && retired_warns.contains("ui.working.verbs"),
            "expected actionable warnings for retired/invalid options: {retired_warns}"
        );

        h.shutdown().await;
    })
    .await
}

/// Working line occasionally displays a usage tip next to the spinner/tail when waiting,
/// keeping `(elapsed · Esc to interrupt)` intact, and honors `ui.working.tips = false` and custom lists.
#[tokio::test]
async fn working_line_shows_tips_when_waiting_and_can_be_disabled() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).no_session().build().await;
        let (early, waiting, disabled, custom): (String, String, String, String) = eval(
            &h,
            r#"
            local W = require("ui.working")
            local WD = require("ui.working_draw")
            local WL = require("ui.working_looks")
            local def_look = W.resolve({})
            local early = mare.ui.render_text(
                WD.row(def_look, WL, { label = "Thinking", secs = 0, interrupt_key = "Esc", still = true, color = "truecolor" }),
                120
            )
            local waiting = mare.ui.render_text(
                WD.row(def_look, WL, { label = "Thinking", secs = 5, interrupt_key = "Esc", still = true, color = "truecolor" }),
                120
            )
            local off_look = W.resolve({ ui = { working = { tips = false } } })
            local disabled = mare.ui.render_text(
                WD.row(off_look, WL, { label = "Thinking", secs = 5, interrupt_key = "Esc", still = true, color = "truecolor" }),
                120
            )
            local custom_look = W.resolve({ ui = { working = { tips = { "Use /undo to rewind" } } } })
            local custom = mare.ui.render_text(
                WD.row(custom_look, WL, { label = "Thinking", secs = 5, interrupt_key = "Esc", still = true, color = "truecolor" }),
                120
            )
            return early, waiting, disabled, custom
            "#,
        )
        .await;
        assert!(early.contains("Esc to interrupt)") && !early.contains("Tip:"), "0s turn should stay concise: {early}");
        assert!(
            waiting.contains("Esc to interrupt)") && waiting.contains("Tip:"),
            "waiting >=3s should show a tip alongside the working line: {waiting}"
        );
        assert!(
            disabled.contains("Esc to interrupt)") && !disabled.contains("Tip:"),
            "tips = false must hide working tips: {disabled}"
        );
        assert!(
            custom.contains("Esc to interrupt)") && custom.contains("Tip: Use /undo to rewind"),
            "custom tips list must render: {custom}"
        );
        h.shutdown().await;
    })
    .await
}

/// Nodes built under the initial dark theme (with osc8 off) must recolor and enable OSC 8 links
/// when an async OSC 11 light-background event / theme change arrives later.
#[tokio::test]
async fn theme_and_osc8_changes_recolor_and_relink_existing_node_blocks() {
    local(async {
        let mut t = Term::colored(json!([{ "text": "ok" }]), 80, 24).await;
        t.vt.host.ui.theme.write().unwrap().md.osc8 = false;
        t.submit("hello theme").await;
        t.idle().await;
        // Push a styled Node::Line with a named hl + OSC 8 link and a Node::Block with a string title.
        exec(
            &t.vt.host,
            r#"
            local id = mare.ui.push("info", {
                node = {
                    type = "lines",
                    items = {
                        {
                            type = "line",
                            spans = {
                                { text = "tool-label", style = "accent" },
                                { text = " " },
                                { text = "docs-link", style = { hl = "accent", link = "https://example.com" } },
                            },
                        },
                        {
                            type = "block",
                            title = "BoxTitle",
                            child = {
                                type = "line",
                                spans = { { text = "inside", style = "MareDim" } },
                            },
                        },
                    },
                },
            })
            mare.ui.finish(id)
            "#,
        )
        .await;
        t.until("styled node block rendered", |s| s.contains("tool-label") && s.contains("BoxTitle")).await;
        let find_cell_colors = |t: &Term, needle: &str| -> (vt100::Color, vt100::Color) {
            let screen = t.parser.screen();
            let (rows, cols) = (screen.size().0.get(), screen.size().1.get());
            for r in 0..rows {
                let row_text = screen.contents_between(r, 0, r, cols);
                if let Some(byte_idx) = row_text.find(needle) {
                    let col = mare_shared::style::str_width(&row_text[..byte_idx]) as u16;
                    let cell = screen.cell(r, col).unwrap();
                    return (cell.fgcolor(), cell.bgcolor());
                }
            }
            panic!("missing {needle:?} on screen:\n{}", t.screen());
        };
        let (_dark_user_fg, dark_user_bg) = find_cell_colors(&t, "hello theme");
        let (dark_tool_fg, _) = find_cell_colors(&t, "tool-label");
        let (dark_title_fg, _) = find_cell_colors(&t, "BoxTitle");

        // Now simulate async OSC 11 light-terminal reply + OSC 8 probe enablement, and replay history.
        t.vt.host.ui.theme.write().unwrap().md.osc8 = true;
        t.vt.host.emit_json("ui/background", json!({ "dark": false, "r": 255, "g": 255, "b": 255 })).await;
        t.step().await;
        // Trigger debounced reflow replay at the same width via resize(80, 24) + capture raw bytes.
        let _ = t.vt.output();
        t.resize(80, 24).await;
        // Capture replayed output containing the OSC 8 hyperlink sequence.
        let replayed = {
            t.vt.resize(79, 24);
            t.vt.frame().await;
            let bytes = t.vt.output();
            t.parser.screen_mut().set_size(nz(24), nz(79));
            t.parser.process(&bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        };
        let (_light_user_fg, light_user_bg) = find_cell_colors(&t, "hello theme");
        let (light_tool_fg, _) = find_cell_colors(&t, "tool-label");
        let (light_title_fg, _) = find_cell_colors(&t, "BoxTitle");

        assert_ne!(
            light_user_bg, dark_user_bg,
            "user message bar bg must update to light theme (was {dark_user_bg:?})"
        );
        assert_ne!(light_tool_fg, dark_tool_fg, "named hl span fg must update to light theme (was {dark_tool_fg:?})");
        assert_ne!(
            light_title_fg, dark_title_fg,
            "Node::Block string title fg must update to light theme (was {dark_title_fg:?})"
        );
        assert!(
            replayed.contains("\x1b]8;;https://example.com\x1b\\"),
            "OSC 8 link must appear once theme.md.osc8 becomes true; replayed={replayed:?}"
        );
    })
    .await
}

/// Issue #43: retries appear on the transient working line with `attempt/max` and a non-zero
/// wait delay (never `0s`), never push per-attempt error toasts, cancel cleanly on `Esc`, and
/// only surface the raw error in the transcript when the final attempt fails.
#[tokio::test]
async fn retry_status_line_shows_attempt_and_nonzero_delay_cancels_on_esc_and_only_errors_on_final_failure() {
    let drop_err = json!({
        "error": "stream ended before response.completed",
        "retryable": true,
        "kind": "stream_closed",
    });

    // 1. Stream drops early -> transient working indicator with attempt/max + sub-second delay -> succeeds once.
    local(async {
        let mut t = Term::with(
            json!([drop_err.clone(), { "text": "RECOVERED-ONCE" }]),
            json!({ "plugins": { "llm": { "retries": 3, "retry_base_ms": 800 } } }),
            110,
            24,
        )
        .await;
        t.vt.dismiss_notices();
        t.submit("go").await;
        t.until("transient retry status line", |s| s.contains("Retrying (1/3) in 0.")).await;
        let retry_screen = t.screen();
        assert!(!retry_screen.contains("in 0s"), "sub-second backoff must never print as 0s:\n{retry_screen}");
        assert!(
            !retry_screen.contains("stream ended before response.completed"),
            "per-attempt retry must not show raw error toast:\n{retry_screen}"
        );
        t.idle().await;
        let hist = t.history();
        assert_eq!(hist.matches("RECOVERED-ONCE").count(), 1, "final answer must appear once:\n{hist}");
        assert!(
            !hist.contains("stream ended before response.completed"),
            "successful retry must leave no error in transcript:\n{hist}"
        );
        assert!(!t.screen().contains("Retrying"), "retry indicator clears after recovery:\n{}", t.screen());
    })
    .await;

    // 2. Esc cancels an in-flight retry sleep without running the next attempt or showing an error.
    local(async {
        let mut t_esc = Term::with(
            json!([drop_err.clone(), { "text": "SHOULD-NOT-RUN" }]),
            json!({ "plugins": { "llm": { "retries": 2, "retry_base_ms": 5000 } } }),
            110,
            24,
        )
        .await;
        t_esc.vt.dismiss_notices();
        t_esc.submit("cancel me").await;
        t_esc.until("retry wait before Esc", |s| s.contains("Retrying (1/2) in ")).await;
        assert!(!t_esc.screen().contains("in 0s"), "retry wait must not show 0s:\n{}", t_esc.screen());
        t_esc.key(KeyCode::Escape, Modifiers::NONE);
        t_esc.until_vt("retry aborted by Esc", |t| !t.vt.busy()).await;
        let esc_hist = t_esc.history();
        assert!(!esc_hist.contains("SHOULD-NOT-RUN"), "Esc must cancel before next attempt:\n{esc_hist}");
        assert!(
            !esc_hist.contains("stream ended before response.completed"),
            "Esc cancel must not show provider error:\n{esc_hist}"
        );
    })
    .await;

    // 3. Exhausting all retries surfaces the raw error once in the transcript.
    local(async {
        let mut t_fail = Term::with(
            json!([drop_err.clone(), drop_err]),
            json!({ "plugins": { "llm": { "retries": 1, "retry_base_ms": 50 } } }),
            110,
            24,
        )
        .await;
        t_fail.vt.dismiss_notices();
        t_fail.submit("fail me").await;
        t_fail.idle().await;
        let fail_hist = t_fail.history();
        assert_eq!(
            fail_hist.matches("stream ended before response.completed").count(),
            1,
            "final failure must show raw error exactly once:\n{fail_hist}"
        );
    })
    .await;
}

/// Every toast (`mare.ui.notify`) stays strictly inside its box borders and terminal width,
/// including long messages, URLs, wide CJK text, ANSI control bytes, and over-wide live rows.
#[tokio::test]
async fn notify_toast_never_wraps_or_overflows_past_its_border() {
    let assert_toast_box = |t: &Term, label: &str| {
        let screen = t.parser.screen();
        let (rows, cols) = (screen.size().0.get(), screen.size().1.get());
        let mut top_row = None;
        let mut left_col = 0u16;
        let mut right_col = 0u16;
        for r in 0..rows {
            let mut found_tl = None;
            let mut found_tr = None;
            for c in 0..cols {
                let ch = screen.cell(r, c).map(|cell| cell.contents()).unwrap_or("");
                if ch == "╭" {
                    found_tl = Some(c);
                } else if ch == "╮" {
                    found_tr = Some(c);
                }
            }
            if let (Some(tl), Some(tr)) = (found_tl, found_tr)
                && tl > 0
            {
                top_row = Some(r);
                left_col = tl;
                right_col = tr;
                break;
            }
        }
        let top = top_row.unwrap_or_else(|| panic!("{label}: missing toast top border:\n{}", t.screen()));
        let mut closed = false;
        for r in (top + 1)..rows {
            let lch = screen.cell(r, left_col).map(|c| c.contents()).unwrap_or("");
            let rch = screen.cell(r, right_col).map(|c| c.contents()).unwrap_or("");
            if lch == "╰" {
                assert_eq!(rch, "╯", "{label}: bottom border misaligned at row {r}:\n{}", t.screen());
                closed = true;
                break;
            }
            assert_eq!(lch, "│", "{label}: missing left border at row {r}:\n{}", t.screen());
            assert_eq!(rch, "│", "{label}: missing right border at row {r}:\n{}", t.screen());
            for c in (right_col + 1)..cols {
                let tail = screen.cell(r, c).map(|cell| cell.contents()).unwrap_or("");
                assert!(
                    tail.trim().is_empty(),
                    "{label}: text {tail:?} spilled past right border at ({r},{c}):\n{}",
                    t.screen()
                );
            }
        }
        assert!(closed, "{label}: toast box never closed:\n{}", t.screen());
    };

    for width in [110u16, 40u16] {
        local(async {
            let mut t = Term::new(json!([]), width, 24).await;
            t.vt.dismiss_notices();
            // Add an over-wide live block so overlay_notices also composites over a row > w.
            exec(
                &t.vt.host,
                r#"mare.ui.push("text", { node = mare.ui.Paragraph("https://example.com/" .. string.rep("x", 160)) })"#,
            )
            .await;
            for (tag, lua_msg) in [
                ("retry-like", r#""Retrying (1) in 0.5s: stream ended before response.completed""#),
                (
                    "long-url",
                    r#""Failed fetching https://api.example.com/v1/responses/very-long-unbreakable-endpoint-path""#,
                ),
                ("cjk-ansi", r#""\x1b[31m警告\x1b[0m 中文宽字符长提示测试确保每一行都在边框内部""#),
            ] {
                t.vt.dismiss_notices();
                exec(&t.vt.host, &format!("mare.ui.notify({lua_msg}, 'warning')")).await;
                t.until(&format!("{tag} toast at w={width}"), |s| s.contains("Warning")).await;
                assert_toast_box(&t, &format!("{tag}@w={width}"));
            }
        })
        .await;
    }
}
