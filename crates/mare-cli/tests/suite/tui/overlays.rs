//! Overlays and pickers: transcript, keys, tree, trust, info, model picker and Lua shells.

use super::*;

/// `/lua` must work in the real plugin environment, where the debug library is not exposed.
#[tokio::test]
async fn lua_console_evaluates_and_reports_errors_without_the_debug_library() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.submit("/lua 6 * 7").await;
        t.until_vt("the Lua expression result", |t| t.history().lines().any(|line| line.trim() == "42")).await;
        t.submit("/lua error('console-' .. 'fault')").await;
        t.until_vt("the Lua error", |t| t.history().contains("console-fault")).await;
        assert!(!t.history().contains("attempt to index a nil value (global 'debug')"));
    })
    .await
}

#[tokio::test]
async fn the_transcript_overlay_opens_searches_and_closes() {
    local(async {
        let mut t = Term::new(json!([{ "text": "needle in the haystack" }]), 60, 14).await;
        t.submit("find it").await;
        t.idle().await;
        t.key(KeyCode::Char('t'), Modifiers::CONTROL);
        t.until("the transcript", |s| s.contains("needle in the haystack") && s.contains("find it")).await;
        // `/` search inside the overlay
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("needle");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("a match count", |s| s.contains("/needle  1 of")).await;
        insta::assert_snapshot!("transcript_search", t.snap().await);
        // Esc clears the search, a second Esc closes the overlay
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the main screen", |s| s.contains('╰')).await;
    })
    .await
}

#[tokio::test]
async fn the_key_menu_lists_keys() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("the key menu", |s| s.contains("─ Keys ─") && s.contains(" of ")).await;
        insta::assert_snapshot!(if cfg!(windows) { "key_menu_windows" } else { "key_menu" }, t.snap().await);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the menu to close", |s| !s.contains("─ Keys ─")).await;
    })
    .await
}

#[tokio::test]
async fn the_tree_overlay_lists_the_conversation() {
    local(async {
        let mut t = Term::new(json!([{ "text": "first answer" }, { "text": "second answer" }]), 80, 24).await;
        t.submit("question one").await;
        t.idle().await;
        t.submit("question two").await;
        t.idle().await;
        t.submit("/tree").await;
        t.until("the tree overlay", |s| s.contains("question one") && s.contains("question two") && s.contains('╭'))
            .await;
        insta::assert_snapshot!("tree_overlay", t.snap().await);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the overlay to close", |s| s.contains("Ask mare")).await;
    })
    .await
}

/// Model-only `context_disclosure` messages (such as `subagent.models`'s `<system-reminder>`)
/// must reach the agent message history without appearing in the TUI transcript or overlays.
#[tokio::test]
async fn context_disclosure_is_hidden_from_transcript_and_overlays() {
    local(async {
        let mut t = Term::new(json!([{ "text": "answer after disclosure" }]), 80, 24).await;
        expose(&t.vt.host);
        t.submit("hello disclosure").await;
        t.idle().await;
        t.until("the assistant answer", |s| s.contains("answer after disclosure")).await;
        let disclosed: bool = eval(
            &t.vt.host,
            r#"
            for _, m in ipairs(A:list_messages().data) do
                if m.context_disclosure and m.context_disclosure.namespace == "subagent.models" then
                    return true
                end
            end
            return false
            "#,
        )
        .await;
        assert!(disclosed, "subagent.models disclosure must still be emitted for the model");
        let history = t.history();
        assert!(
            !history.contains("system-reminder") && !history.contains("Available subagent models"),
            "live transcript must hide context_disclosure: {history}"
        );
        // Ctrl+T transcript overlay must also omit model-only context disclosures.
        t.key(KeyCode::Char('t'), Modifiers::CONTROL);
        t.until("the transcript overlay", |s| s.contains("answer after disclosure") && s.contains("hello disclosure"))
            .await;
        let overlay = t.screen();
        assert!(
            !overlay.contains("system-reminder") && !overlay.contains("Available subagent models"),
            "transcript overlay must hide context_disclosure: {overlay}"
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the main screen", |s| s.contains("Ask mare")).await;
        // Session replay (`session/loaded`) must keep context_disclosure hidden as well.
        let sid: String = eval(&t.vt.host, r#"return S:info().session_id"#).await;
        t.submit(&format!("/resume {sid}")).await;
        t.until("resumed transcript", |s| s.contains("Resumed") && s.contains("answer after disclosure")).await;
        let replayed = t.history();
        assert!(
            !replayed.contains("system-reminder") && !replayed.contains("Available subagent models"),
            "replayed transcript must hide context_disclosure: {replayed}"
        );
    })
    .await
}

/// The trust overlay: a workspace with project resources and no saved
/// decision asks after the first frame; Esc means "Not now".
#[tokio::test]
async fn the_trust_overlay_asks_about_a_folder_with_project_resources() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({ "trust_pending": true }), 80, 24, |env| {
            let skill = env.cwd.join(".mare/skills/demo");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "---\nname: demo\ndescription: d\n---\nbody\n").unwrap();
        })
        .await;
        t.until("the trust overlay", |s| s.contains("Trust this folder?") && s.contains("Trust and load")).await;
        insta::assert_snapshot!("trust_overlay", t.snap().await);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the overlay to close", |s| !s.contains("Trust this folder?")).await;
        drop(t);
        // a git repository without project resources is asked too (Codex onboarding): the
        // question is about the read-only start
        let mut t = Term::build(json!([]), json!({}), json!({ "trust_pending": true }), 80, 24, |env| {
            std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
            std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
            std::fs::write(env.cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        })
        .await;
        t.until("the trust overlay", |s| s.contains("Trust this folder?") && s.contains("start each session")).await;
        assert!(!t.screen().contains("project resources"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn entering_the_trust_overlay_default_does_not_persist_trust() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({ "trust_pending": true }), 80, 24, |env| {
            let skill = env.cwd.join(".mare/skills/demo");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "---\nname: demo\ndescription: d\n---\nbody\n").unwrap();
        })
        .await;
        t.until("the trust overlay", |s| s.contains("Trust this folder?") && s.contains("Not now")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("startup after the default answer", |t| !t.vt.starting()).await;
        let trusted: bool = eval(&t.vt.host, "return mare.trusted").await;
        assert!(!trusted, "bare Enter must not trust the folder");
        assert!(!t._env.home.path().join("state/trust.json").exists(), "Not now must not persist a decision");
    })
    .await
}

#[tokio::test]
async fn trust_overlay_esc_from_search_and_expanded_always_dismisses_as_not_now() {
    local(async {
        // 1. Typing '/' must NOT trap Esc in search mode where Enter could select a different item.
        let mut t = Term::build(json!([]), json!({}), json!({ "trust_pending": true }), 80, 24, |env| {
            std::fs::create_dir_all(env.cwd.join(".mare/plugins")).unwrap();
            for i in 1..=8 {
                std::fs::write(env.cwd.join(format!(".mare/plugins/p{i}.lua")), "return {}\n").unwrap();
            }
        })
        .await;
        t.until("the trust overlay", |s| s.contains("Trust this folder?") && s.contains("Trust and load")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the overlay to close on Esc after '/'", |s| !s.contains("Trust this folder?")).await;
        t.until_vt("startup settled", |t| !t.vt.starting()).await;
        let trusted: bool = eval(&t.vt.host, "return mare.trusted").await;
        assert!(!trusted, "search -> Esc must dismiss safely as NotNow");
        assert!(!t._env.home.path().join("state/trust.json").exists());
        drop(t);

        // 2. Pressing 'e' to expand and then Esc also dismisses safely as NotNow.
        let mut t = Term::build(json!([]), json!({}), json!({ "trust_pending": true }), 80, 24, |env| {
            std::fs::create_dir_all(env.cwd.join(".mare/plugins")).unwrap();
            for i in 1..=8 {
                std::fs::write(env.cwd.join(format!(".mare/plugins/p{i}.lua")), "return {}\n").unwrap();
            }
        })
        .await;
        t.until("the trust overlay with +2 more", |s| s.contains("Trust this folder?") && s.contains("+2 more")).await;
        t.key(KeyCode::Char('e'), Modifiers::NONE);
        t.until("expanded list", |s| s.contains("p8.lua") && !s.contains("+2 more")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("expanded trust overlay to close on Esc", |s| !s.contains("Trust this folder?")).await;
        t.until_vt("startup settled", |t| !t.vt.starting()).await;
        let trusted: bool = eval(&t.vt.host, "return mare.trusted").await;
        assert!(!trusted, "e -> Esc must dismiss safely as NotNow");
        assert!(!t._env.home.path().join("state/trust.json").exists());
    })
    .await
}

/// Ctrl+O toggles committed tool output and reasoning in place between compact and expanded views
/// across repeated toggles, pre-turn activation, and `mare.ui.flag("expand_all", ...)`.
#[tokio::test]
async fn ctrl_o_toggles_committed_tool_and_reasoning_between_compact_and_expanded() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "notes.txt" } }] },
            { "text": "read it" },
            { "tool_calls": [{ "name": "read", "arguments": { "path": "notes.txt" } }] },
            { "text": "read again" }
        ]);
        let mut t = Term::new(script, 80, 24).await;
        t.vt.dismiss_notices();
        let body: String = (1..=30).map(|i| format!("note line {i}\n")).collect();
        std::fs::write(t._env.cwd.join("notes.txt"), body).unwrap();
        t.submit("read the notes").await;
        t.idle().await;
        t.until("the answer", |s| s.contains("read it")).await;
        // Also commit a markdown reasoning block so both tool and reasoning compact/expanded
        // paths (including the `(Ctrl+O to expand)` hint on reflow) are covered.
        exec(
            &t.vt.host,
            r#"
            local id = mare.ui.push("reasoning", { markdown = "REASONING_DETAIL_BODY" })
            mare.ui.finish(id)
            "#,
        )
        .await;
        t.until_vt("reasoning committed", |t| t.history().contains("Thought for")).await;
        assert!(
            t.history().contains("Read 1 file") && !t.history().contains("Read 30 lines"),
            "tool starts compact:\n{}",
            t.history()
        );
        assert!(
            t.history().contains("Ctrl+O to expand") && !t.history().contains("REASONING_DETAIL_BODY"),
            "reasoning starts compact:\n{}",
            t.history()
        );

        // 1. Toggle Ctrl+O repeatedly (compact -> expanded -> compact -> expanded -> compact).
        tokio::time::pause();
        t.vt.reset_loop_state();
        for cycle in 0..2 {
            // Expand
            t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('o'), Modifiers::CONTROL)));
            t.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
            let out = t.vt.output();
            t.parser.process(&out);
            let s = t.screen();
            let h = t.history();
            assert!(!s.contains("Transcript"), "cycle {cycle} expand must stay on main screen:\n{s}");
            assert!(
                h.contains("Read 30 lines") && !h.contains("Read 1 file"),
                "cycle {cycle} expand must show expanded tool:\n{h}"
            );
            assert!(h.contains("REASONING_DETAIL_BODY"), "cycle {cycle} expand must show reasoning:\n{h}");

            // Collapse back to compact
            t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('o'), Modifiers::CONTROL)));
            t.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
            let out = t.vt.output();
            t.parser.process(&out);
            let s = t.screen();
            let h = t.history();
            assert!(!s.contains("Transcript"), "cycle {cycle} collapse must stay on main screen:\n{s}");
            assert!(
                h.contains("Read 1 file") && !h.contains("Read 30 lines"),
                "cycle {cycle} collapse must restore compact tool:\n{h}"
            );
            assert!(!h.contains("REASONING_DETAIL_BODY"), "cycle {cycle} collapse must hide reasoning body:\n{h}");
            assert!(h.contains("Ctrl+O to expand"), "cycle {cycle} collapse must keep expand hint:\n{h}");
        }

        // 2. `mare.ui.flag("expand_all", true/false)` also reflows committed scrollback in place.
        exec(&t.vt.host, r#"mare.ui.flag("expand_all", true)"#).await;
        t.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
        let out = t.vt.output();
        t.parser.process(&out);
        assert!(t.history().contains("Read 30 lines"), "expand_all=true must reflow scrollback:\n{}", t.history());

        exec(&t.vt.host, r#"mare.ui.flag("expand_all", false)"#).await;
        t.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
        let out = t.vt.output();
        t.parser.process(&out);
        assert!(
            t.history().contains("Read 1 file") && !t.history().contains("Read 30 lines"),
            "expand_all=false must restore compact:\n{}",
            t.history()
        );
        tokio::time::resume();

        // 3. Turning Ctrl+O ON before a turn and pressing Ctrl+O again after the turn commits
        // must return to compact view instead of trapping expand_all = true.
        t.key(KeyCode::Char('o'), Modifiers::CONTROL);
        t.until("expanded notice before turn", |s| s.contains("Tool output: expanded")).await;
        t.submit("read the notes again").await;
        t.idle().await;
        t.until("second answer", |s| s.contains("read again")).await;
        assert!(t.history().contains("Read 30 lines"), "turn committed while expanded:\n{}", t.history());

        tokio::time::pause();
        t.vt.reset_loop_state();
        t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('o'), Modifiers::CONTROL)));
        t.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
        let out = t.vt.output();
        t.parser.process(&out);
        assert!(!t.screen().contains("Transcript"), "second Ctrl+O must not open transcript:\n{}", t.screen());
        assert!(
            t.history().contains("Read 1 file") && !t.history().contains("Read 30 lines"),
            "second Ctrl+O must collapse back to compact:\n{}",
            t.history()
        );
        tokio::time::resume();
    })
    .await
}

/// Scrolling an info overlay past its end doesn't pile up: one Up moves back at once.
#[tokio::test]
async fn info_overlay_scroll_stops_at_the_end() {
    local(async {
        let mut t = Term::new(json!([]), 70, 14).await;
        t.submit("/help").await;
        t.until("the help overlay", |s| s.contains(" of ")).await;
        let footer = |s: &str| {
            let l = s.lines().find(|l| l.contains(" of ") && l.contains('╯')).unwrap_or_default().to_string();
            let words: Vec<&str> = l.split_whitespace().collect();
            let at = words.iter().position(|w| *w == "of").unwrap();
            (words[at - 1].trim_start_matches('─').parse::<usize>().unwrap(), words[at + 1].parse::<usize>().unwrap())
        };
        for _ in 0..200 {
            t.key(KeyCode::Down, Modifiers::NONE);
        }
        t.step().await;
        let (shown, total) = footer(&t.screen());
        assert_eq!(shown, total, "at the end");
        t.key(KeyCode::Up, Modifiers::NONE);
        t.step().await;
        let (shown, _) = footer(&t.screen());
        assert_eq!(shown, total - 1, "one Up moves back one line");
    })
    .await
}

/// "都是 Lua 组件": the key hint line, the select frame (search line, empty text) and
/// the confirm dialog come from `mare.ui.KeyHints` / `Select` / `Confirm`; overriding them in Lua
/// changes what the core draws.
#[tokio::test]
async fn overlay_shell_pieces_are_overridable_lua_components() {
    local(async {
        let mut t = Term::new(json!([]), 70, 24).await;
        exec(
            &t.vt.host,
            r#"
            mare.ui.KeyHints.sep = " ~ "
            mare.ui.Select.empty = "(nothing here)"
            function mare.ui.Confirm:render(c)
                return mare.ui.Block(mare.ui.Line({ mare.ui.Span("custom " .. c.prompt) }), { title = "Sure?" })
            end
            "#,
        )
        .await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("the key menu", |s| s.contains("─ Keys ─") && s.contains(" of ")).await;
        t.until("the hint line from KeyHints", |s| s.contains("run ~ type search ~ Esc back")).await;
        t.vt.type_str("zzzzqq");
        t.until("the empty text from Select", |s| s.contains("(nothing here)")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the menu to close", |s| !s.contains("(nothing here)")).await;
        // a confirm dialog drawn by the replaced Confirm:render
        let h = t.vt.host.clone();
        let answer =
            tokio::task::spawn_local(async move { eval::<bool>(&h, "return mare.ui.confirm('Delete it?')").await });
        t.until("the custom dialog", |s| s.contains("custom Delete it?") && s.contains("Sure?")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(answer.await.unwrap(), "y answers yes");
    })
    .await
}

/// in the model picker Tab edits the highlighted model's reasoning effort; Alt+M
/// cycles the favorite models without the picker.
#[tokio::test]
async fn model_picker_tab_edits_effort_and_alt_m_cycles_favorites() {
    local(async {
        let cfg = json!({ "favorite_models": ["faux/echo", "faux/echo2"], "effort": "low" });
        let mut t = Term::with(json!([]), cfg, 100, 24).await;
        t.key(KeyCode::Char('p'), Modifiers::ALT);
        t.until("the model picker", |s| s.contains("Select model") && s.contains("Tab parameters")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("the effort picker", |s| s.contains("Reasoning effort · faux/echo")).await;
        t.key(KeyCode::End, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("effort high", |s| s.contains("Faux Echo thinking: high")).await;
        t.key(KeyCode::Char('m'), Modifiers::ALT);
        t.until("the next favorite", |s| s.contains("Model: faux/echo2 (2/2)")).await;
        t.key(KeyCode::Char('m'), Modifiers::ALT);
        t.until("back to the first", |s| s.contains("Model: faux/echo (1/2)")).await;
    })
    .await
}

// --- command overlays (the commands half; Lua-level tests are in suite::commands) ---

/// /context is an overlay (it stayed 6 seconds as a toast before): the summary, free-space and
/// total rows stay on screen until a key closes it.
#[tokio::test]
async fn context_is_an_overlay_with_free_space_and_a_total() {
    local(async {
        let mut t = Term::new(json!([]), 90, 30).await;
        t.submit("/context").await;
        t.until("the /context overlay", |s| s.contains("Free space") && s.contains("Compaction summary")).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        t.step().await;
        assert!(t.screen().contains("Free space"), "still open: {}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the overlay to close", |s| !s.contains("Free space")).await;
    })
    .await
}

/// /hotkeys lists each binding's description and the editor's own keys (Ctrl+W …), not action ids.
#[tokio::test]
async fn hotkeys_list_descriptions_and_the_editor_keys() {
    local(async {
        let mut t = Term::new(json!([]), 100, 60).await;
        t.submit("/hotkeys").await;
        t.until("the key list", |s| s.contains("Input editor")).await;
        let s = t.screen();
        assert!(s.contains("Ctrl+T") && s.contains("expanded transcript"), "{s}");
        assert!(s.contains("Ctrl+W") && s.contains("delete the word before the cursor"), "{s}");
        assert!(!s.contains("<C-t>"), "keys are written out: {s}");
    })
    .await
}

/// The agents UI's child view: a sub-session file
/// (`session:child`) opened with `session:open_transcript` follows what is appended while the
/// view is at its end, and stays put once scrolled up. `ui.transcript_close(title)` closes only
/// the view showing that title.
#[tokio::test]
async fn a_child_session_view_follows_appends_and_closes_by_title() {
    local(async {
        let config = json!({ "plugins": { "session": { "transcript_poll_ms": 20 } } });
        let mut t = Term::with(json!([]), config, 60, 12).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"CH = S:child({ run_id = "r1", seq = 1, label = "explorer" })
               function SAY(text)
                 CH.append({ type = "message", message = { role = "assistant", content = { { type = "text", text = text } } } })
               end
               for i = 1, 8 do SAY("line " .. i) end
               mare.task.spawn(function() S:load_transcript(CH.path, "explorer") end)"#,
        )
        .await;
        t.until("the child's view at its end", |s| s.contains("line 8") && !s.contains("line 2")).await;
        exec(&t.vt.host, r#"SAY("line 9")"#).await;
        t.until("the appended message, followed", |s| s.contains("line 9")).await;
        // scrolled to the top, an append doesn't move the view
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.until("the top", |s| s.contains("line 1") && !s.contains("line 9")).await;
        exec(&t.vt.host, r#"SAY("line 10")"#).await;
        for _ in 0..10 {
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert!(t.screen().contains("line 1") && !t.screen().contains("line 10"), "{}", t.screen());
        // another title leaves it open; its own title closes it
        const TITLE: &str = "return mare.ui.transcript_title()";
        exec(&t.vt.host, r#"mare.ui.transcript_close("someone else")"#).await;
        t.step().await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, TITLE).await.as_deref(), Some("explorer"));
        exec(&t.vt.host, r#"mare.ui.transcript_close("explorer")"#).await;
        t.step().await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, TITLE).await, None);
        t.until("the main screen", |s| s.contains('╰') && !s.contains("line 1")).await;
    })
    .await
}

/// Modal hint bar and `?` help rows follow the live keymap bindings.
#[tokio::test]
async fn modal_hint_bar_and_help_follow_bindings() {
    local(async {
        let mut t = Term::new(json!([]), 90, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set("select", "/", false)
               mare.task.spawn(function()
                 mare.ui.select({ title = "HintSelect", items = { "one", "two" } })
               end)"#,
        )
        .await;
        t.until("select open without search hint", |s| {
            s.contains("HintSelect") && s.contains("Enter select") && !s.contains("/ search")
        })
        .await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("modal help open", |s| s.contains("choose the selected item")).await;
        let help_screen = t.screen();
        assert!(!help_screen.contains("search (fuzzy)"), "unbound select.search omitted from ?: {help_screen}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back to select", |s| s.contains("HintSelect") && !s.contains("choose the selected item")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("select closed", |s| !s.contains("HintSelect")).await;

        exec(
            &t.vt.host,
            r#"mare.keymap.set("select", "<CR>", false)
               mare.keymap.set("select", "<C-o>", "select.accept")
               mare.task.spawn(function()
                 mare.ui.select({ title = "HintAccept", items = { "one", "two" } })
               end)"#,
        )
        .await;
        t.until("hint shows Ctrl+O select", |s| s.contains("HintAccept") && s.contains("Ctrl+O select")).await;
    })
    .await
}

/// Focused custom overlay resolves `opts.context -> info` without falling through to `global`,
/// positions the terminal cursor at `opts.cursor`, and restores focus + cursor to the editor.
#[tokio::test]
async fn custom_overlay_resolves_context_chain_without_global_and_places_cursor() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        t.vt.type_str("hi");
        t.until("editor text", |s| s.contains("❯ hi")).await;
        let ed_cur = t.parser.screen().cursor_position();

        exec(
            &t.vt.host,
            r#"PANEL_ACT = 0
               GLOBAL_ACT = 0
               mare.keymap.context("info.custom", { parent = "info" })
               mare.keymap.action("info.custom_act", function() PANEL_ACT = PANEL_ACT + 1 end, {
                 desc = "panel action",
                 contexts = { "info.custom" },
               })
               mare.keymap.set("info.custom", "a", "info.custom_act")
               mare.keymap.action("global.never", function() GLOBAL_ACT = GLOBAL_ACT + 1 end, {
                 desc = "global action",
                 contexts = { "global" },
               })
               mare.keymap.set("global", "<C-g>", "global.never")
               mare.ui.overlay("FocusedPanel", "zero_line\none_line\ntwo_line", {
                 context = "info.custom",
                 cursor = { row = 1, col = 3 },
               })"#,
        )
        .await;
        t.until("focused overlay open", |s| s.contains("FocusedPanel") && s.contains("one_line")).await;

        let sc = t.screen();
        let (exp_r, exp_c) = sc
            .lines()
            .enumerate()
            .find_map(|(r, line)| {
                line.find("one_line").map(|c| (r as u16, (mare_shared::style::str_width(&line[..c]) + 3) as u16))
            })
            .expect("one_line visible");
        assert!(!t.parser.screen().hide_cursor(), "overlay cursor must be visible");
        assert_eq!(t.parser.screen().cursor_position(), (exp_r, exp_c));

        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return PANEL_ACT").await, 1);

        // Unbound non-Esc key and a `global` binding must stay absorbed without closing or hitting global.
        t.key(KeyCode::Char('z'), Modifiers::NONE);
        t.key(KeyCode::Char('g'), Modifiers::CONTROL);
        t.step().await;
        assert!(t.screen().contains("FocusedPanel"), "focused overlay stays open on unbound/global keys");
        assert_eq!(eval::<i64>(&t.vt.host, "return GLOBAL_ACT").await, 0, "must not fall through to global");

        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("overlay closed", |s| !s.contains("FocusedPanel")).await;
        assert_eq!(t.parser.screen().cursor_position(), ed_cur, "editor cursor restored");
        t.key(KeyCode::Char('!'), Modifiers::NONE);
        t.until("editor has focus again", |s| s.contains("❯ hi!")).await;
    })
    .await
}

/// Real modals always stack above custom overlays and take keys first; closing the modal returns
/// focus + cursor to the overlay, and closing the overlay returns to prior `transcript` or `editor`.
#[tokio::test]
async fn modal_stacks_above_custom_overlay_and_restores_focus_and_cursor() {
    local(async {
        let mut t = Term::new(json!([{ "text": "hello transcript" }]), 90, 26).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"UNDER_ACT = 0
               PICKED = nil
               mare.keymap.context("info.under", { parent = "info" })
               mare.keymap.action("info.under_act", function() UNDER_ACT = UNDER_ACT + 1 end, {
                 desc = "under action",
                 contexts = { "info.under" },
               })
               mare.keymap.set("info.under", "a", "info.under_act")
               mare.ui.overlay("PanelUnder", "u_row0\nu_row1", {
                 context = "info.under",
                 cursor = { row = 1, col = 2 },
               })"#,
        )
        .await;
        t.until("custom overlay open", |s| s.contains("PanelUnder") && s.contains("u_row1")).await;
        let ov_cur = t.parser.screen().cursor_position();

        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 PICKED = mare.ui.select({ title = "TopSelect", items = { "choice_alpha", "choice_beta" } })
               end)"#,
        )
        .await;
        t.until("select stacked over overlay", |s| s.contains("TopSelect") && s.contains("choice_alpha")).await;

        // Calling overlay() while a real modal is open must keep the modal on top taking keys.
        exec(
            &t.vt.host,
            r#"mare.ui.overlay("PanelUnderUpdated", "n_row0\nn_row1", {
                 context = "info.under",
                 cursor = { row = 1, col = 4 },
               })"#,
        )
        .await;
        t.step().await;
        assert!(t.screen().contains("TopSelect"), "modal stays on top of updated overlay");

        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("modal closed, updated overlay restored", |s| {
            !s.contains("TopSelect") && s.contains("PanelUnderUpdated") && s.contains("n_row1")
        })
        .await;
        assert_eq!(eval::<i64>(&t.vt.host, "return UNDER_ACT").await, 0, "modal took keys first");

        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return UNDER_ACT").await, 1, "overlay regained key focus");
        assert!(!t.parser.screen().hide_cursor());
        assert_eq!(t.parser.screen().cursor_position().1, ov_cur.1 + 2, "updated overlay cursor active");

        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("overlay closed", |s| !s.contains("PanelUnderUpdated")).await;

        // Also verify return to Ctrl+T transcript overlay and cursor hiding.
        t.submit("ping").await;
        t.idle().await;
        t.ctrl('t');
        t.until("transcript open", |s| s.contains("hello transcript")).await;
        exec(
            &t.vt.host,
            r#"mare.ui.overlay("OverTranscript", "tr_row0\ntr_row1", {
                 context = "info.under",
                 cursor = { row = 0, col = 2 },
               })"#,
        )
        .await;
        t.until("custom overlay over transcript", |s| s.contains("OverTranscript") && s.contains("tr_row0")).await;
        assert!(!t.parser.screen().hide_cursor(), "overlay cursor shown over transcript");

        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back to transcript", |s| !s.contains("OverTranscript") && s.contains("hello transcript")).await;
        assert!(t.parser.screen().hide_cursor(), "transcript restores hidden cursor");

        // CJK wide-char cursor placement + hiding cursor when scrolled out of viewport + Esc when
        // custom subcontext is removed while overlay is open.
        exec(
            &t.vt.host,
            r#"OV_CTX_H = mare.keymap.context("info.ephemeral", { parent = "info" })
               local lines = "中文x"
               for i = 1, 30 do lines = lines .. "\nline_" .. i end
               mare.ui.overlay("WideAndScroll", lines, {
                 context = "info.ephemeral",
                 cursor = { row = 0, col = 4 },
               })"#,
        )
        .await;
        t.until("wide overlay open", |s| s.contains("WideAndScroll") && s.contains("中文x")).await;
        let (exp_r, exp_c) = t
            .screen()
            .lines()
            .enumerate()
            .find_map(|(r, line)| {
                line.find("中文x").map(|c| (r as u16, (mare_shared::style::str_width(&line[..c]) + 4) as u16))
            })
            .expect("中文x visible");
        assert!(!t.parser.screen().hide_cursor());
        assert_eq!(t.parser.screen().cursor_position(), (exp_r, exp_c), "cursor after CJK chars");

        t.key(KeyCode::PageDown, Modifiers::NONE);
        t.step().await;
        assert!(t.parser.screen().hide_cursor(), "scrolled-away cursor must be hidden, not clamped");

        exec(&t.vt.host, "OV_CTX_H.remove()").await;
        t.step().await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("Esc closes overlay even after subcontext removed", |s| !s.contains("WideAndScroll")).await;
    })
    .await
}

#[tokio::test]
async fn transcript_viewport_search_and_large_select_modal_navigation_keep_parity() {
    local(async {
        let mut turns = Vec::new();
        for i in 0..25 {
            let tag = if i == 4 || i == 20 { "MARKER" } else { "filler" };
            turns.push(json!({ "text": format!("block {i:02} {tag} content line") }));
        }
        let mut t = Term::new(json!(turns), 80, 18).await;
        for i in 0..25 {
            t.submit(&format!("prompt {i:02}")).await;
            t.idle().await;
        }
        t.ctrl('t');
        t.until("transcript open at bottom", |s| s.contains("block 24")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("MARKER");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("search hits both MARKER blocks", |s| s.contains("/MARKER  1 of 2") && s.contains("block 04")).await;
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("next match jumps to block 20", |s| s.contains("/MARKER  2 of 2") && s.contains("block 20")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("transcript closed", |s| s.contains('╰')).await;

        // Large 300-item Select modal: filter + window scroll + Enter selection.
        exec(
            &t.vt.host,
            r#"
            LARGE_PICK = nil
            local items = {}
            for i = 1, 300 do
                items[i] = { id = "id_" .. i, label = string.format("item-%03d", i), detail = "detail-" .. (i % 7) }
            end
            mare.task.spawn(function()
                LARGE_PICK = mare.ui.select({ title = "BigSelect", items = items, filter = "" })
            end)
            "#,
        )
        .await;
        t.until("BigSelect open", |s| s.contains("BigSelect") && s.contains("1 of 300")).await;
        t.vt.type_str("item-24");
        t.until("filtered down to 24x items", |s| s.contains("item-240") && s.contains("1 of ")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.step().await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("BigSelect closed", |s| !s.contains("BigSelect")).await;
        let picked: i64 = eval(&t.vt.host, "return LARGE_PICK").await;
        assert_eq!(picked, 241);
    })
    .await
}

#[tokio::test]
async fn transcript_cache_rebuilds_on_width_rule_or_theme_change_while_open() {
    local(async {
        let bullet_line = "•".repeat(18) + " WRAP_END_TOKEN";
        let mut t = Term::new(json!([{ "text": bullet_line }]), 40, 16).await;
        t.submit("trigger").await;
        t.idle().await;
        t.ctrl('t');
        t.until("transcript open", |s| s.contains("WRAP_END_TOKEN")).await;
        // Find the row where WRAP_END_TOKEN sits under single-width bullets (width 18 -> single line at 40 cols).
        let single_row = t.screen().lines().position(|l| l.contains("WRAP_END_TOKEN")).unwrap();

        // Switch ambiwidth to double while Ctrl+T stays open: 18*2=36 cols + padding exceeds inner width 38 -> wraps!
        exec(&t.vt.host, r#"mare.o.terminal.ambiwidth = "double""#).await;
        t.until("transcript reflows double-width bullets while open", |s| {
            s.lines().position(|l| l.contains("WRAP_END_TOKEN")) != Some(single_row)
        })
        .await;
        let double_row = t.screen().lines().position(|l| l.contains("WRAP_END_TOKEN")).unwrap();
        assert!(
            double_row > single_row,
            "double width should wrap bullets onto next row: {single_row} vs {double_row}"
        );

        exec(&t.vt.host, r#"mare.o.terminal.ambiwidth = "single""#).await;
        t.until("transcript restores single-width layout while open", |s| {
            s.lines().position(|l| l.contains("WRAP_END_TOKEN")) == Some(single_row)
        })
        .await;
    })
    .await
}

#[tokio::test]
async fn tabbed_select_shrink_modal_stack_and_preview_revision_work_cleanly() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;

        // 1. Shrinking tabs via `sel:update({ tabs = ... })` clamps active tab cleanly.
        exec(
            &t.vt.host,
            r#"
            SHRINK_RES = nil
            local sel_handle
            mare.task.spawn(function()
                local idx, tab = mare.ui.select({
                    title = "Tabs",
                    tabs = {
                        { name = "One", items = { "a1", "a2" } },
                        { name = "Two", items = { "b1" } },
                        { name = "Three", items = { "c1" } },
                    },
                    tab = 3,
                    on_open = function(h) sel_handle = h end,
                })
                SHRINK_RES = { idx, tab }
            end)
            _SHRINK_H = function()
                sel_handle:update({ tabs = { { name = "Only", items = { "only-item" } } } })
            end
            "#,
        )
        .await;
        t.until("third tab shown", |s| s.contains("c1") && s.contains("Three")).await;
        exec(&t.vt.host, "_SHRINK_H()").await;
        t.until("shrunk to single tab", |s| s.contains("only-item") && !s.contains("Three")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("shrunk select closed", |s| !s.contains("only-item")).await;
        let (idx, tab): (i64, i64) = eval(&t.vt.host, "return SHRINK_RES[1], SHRINK_RES[2]").await;
        assert_eq!((idx, tab), (1, 1));

        // 2. Stacked select + input preserves displaced modal's reply without dropping it.
        exec(
            &t.vt.host,
            r#"
            STACK_A, STACK_B = nil, nil
            mare.task.spawn(function()
                STACK_A = mare.ui.select({ title = "FirstModal", items = { "keep-me" } })
            end)
            "#,
        )
        .await;
        t.until("first modal open", |s| s.contains("FirstModal")).await;
        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
                STACK_B = mare.ui.input({ title = "TopSecret", secret = true })
            end)
            "#,
        )
        .await;
        t.until("secret input modal open on top", |s| s.contains("TopSecret")).await;
        t.vt.type_str("pw123");
        t.until("secret masked with bullets", |s| s.contains("•••••") && !s.contains("pw123")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("displaced select restored", |s| s.contains("FirstModal") && !s.contains("TopSecret")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("both modals resolved", |s| !s.contains("FirstModal")).await;
        let (a, b): (i64, String) = eval(&t.vt.host, "return STACK_A, STACK_B").await;
        assert_eq!(a, 1);
        assert_eq!(b, "pw123");

        // 3. Stale delayed on_highlight preview after `sel:update` is discarded.
        exec(
            &t.vt.host,
            r#"
            REV_PICK = nil
            local h_ref
            mare.task.spawn(function()
                REV_PICK = mare.ui.select({
                    title = "PreviewRev",
                    items = { { item_id = "old", label = "old-row" } },
                    on_open = function(h) h_ref = h end,
                    on_highlight = function(i)
                        if i == 1 then
                            mare.task.sleep(60)
                            return mare.ui.Line("PREVIEW-" .. (_UPDATED and "NEW" or "OLD"))
                        end
                    end,
                })
            end)
            _DO_REV_UPDATE = function()
                _UPDATED = true
                h_ref:update({ { item_id = "new", label = "new-row" } })
            end
            "#,
        )
        .await;
        t.until("initial preview item visible", |s| s.contains("old-row")).await;
        exec(&t.vt.host, "_DO_REV_UPDATE()").await;
        t.until("updated item and fresh preview visible", |s| s.contains("new-row") && s.contains("PREVIEW-NEW")).await;
        assert!(!t.screen().contains("PREVIEW-OLD"));
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("preview modal closed", |s| !s.contains("PreviewRev")).await;
    })
    .await
}

#[tokio::test]
async fn select_which_key_box_and_narrow_terminal_do_not_panic() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set("select", "g g", "select.top", { desc = "top item" })
            mare.keymap.set("select", "g d", "select.bottom", { desc = "bottom item" })
            mare.task.spawn(function()
                mare.ui.select({ title = "WhichSelect", items = { "first", "second", "third" } })
            end)
            "#,
        )
        .await;
        t.until("select open", |s| s.contains("WhichSelect")).await;
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.until("which-key popup over select", |s| s.contains("+G") && s.contains("top item")).await;
        t.key(KeyCode::Char('d'), Modifiers::NONE);
        t.until("which-key closes and selects last", |s| !s.contains("+G") && s.contains("3 of 3")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("select closes", |s| !s.contains("WhichSelect")).await;

        // Narrow terminal resize down to 12x10 with modal open must not panic or underflow.
        t.resize(12, 10).await;
        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
                mare.ui.select({ title = "Narrow", items = { "item1" } })
            end)
            "#,
        )
        .await;
        t.until("narrow modal renders", |s| s.contains("Nar") || s.contains("ite")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
    })
    .await
}

#[tokio::test]
async fn diff_command_and_ctrl_r_open_and_close_change_viewers() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "demo.txt", "content": "hello diff\n" } }] },
            { "text": "wrote demo.txt" }
        ]);
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .builtins(&["changes", "review"])
            .faux(script)
            .config(json!({ "logo": false, "plugins": { "permissions": { "enabled": false } } }));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 90, 24).await;
        let mut t = Term { owned, parser: vt100::Parser::new(nz(24), nz(90), 2000), ended: 0 };
        t.step().await;
        t.submit("create demo.txt").await;
        t.idle().await;
        t.until("turn settled", |s| s.contains("wrote demo.txt")).await;

        t.submit("/diff").await;
        t.until("the /diff overlay", |s| s.contains("Changes by turn") && s.contains("demo.txt")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("/diff closed", |s| !s.contains("Changes by turn")).await;

        t.key(KeyCode::Char('r'), Modifiers::CONTROL);
        t.until("the Ctrl+R change viewer", |s| {
            s.contains("demo.txt") && (s.contains("Review") || s.contains("Changes"))
        })
        .await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("Ctrl+R viewer closed", |s| s.contains("Ask mare")).await;
    })
    .await
}

/// Searchable list modals and the `?` help modal park the real terminal cursor at the end of the
/// filter text (for IME candidate placement), and single-key modal shortcuts fold fullwidth ASCII.
#[tokio::test]
async fn list_and_help_modals_park_cursor_for_ime_and_accept_fullwidth_shortcuts() {
    local(async {
        let mut t = Term::new(json!([]), 80, 20).await;
        t.until_vt("startup settled", |t| t.vt.startup_finished()).await;
        expose(&t.vt.host);

        // 1. Select modal with body lines in search mode parks the visible terminal cursor on the
        //    search row (below `body`), right after the filter text.
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 mare._sel_res = { mare.ui.select({
                   title = "Pick item",
                   body = { "Header line 1", "Header line 2" },
                   allow_search = true,
                   items = { { label = "alpha" }, { label = "beta" } },
                 }) }
               end)"#,
        )
        .await;
        t.until("select modal open", |s| s.contains("Pick item")).await;
        t.vt.type_str("/ab");
        t.step().await;
        assert!(
            !t.parser.screen().hide_cursor(),
            "select search must show the real terminal cursor for IME positioning"
        );
        let (cr, cc) = t.parser.screen().cursor_position();
        let search_line = t.screen().lines().nth(cr as usize).unwrap_or("").to_string();
        assert!(
            search_line.contains("ab"),
            "terminal cursor row {cr} must sit on the search input line (below body); got {search_line:?}\n{}",
            t.screen()
        );
        let after_ab = search_line.find("ab").map(|b| search_line[..b].chars().count() + 2).unwrap() as u16;
        assert_eq!(cc, after_ab, "terminal cursor col must sit right after filter in {search_line:?}");

        // 2. Help modal (`?`) also parks the real terminal cursor on its filter row.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        t.vt.type_str("?mo");
        t.until("help modal open", |s| s.contains("Keys")).await;
        assert!(
            !t.parser.screen().hide_cursor(),
            "help modal search must show the real terminal cursor for IME positioning"
        );
        let (hr, hc) = t.parser.screen().cursor_position();
        let help_line = t.screen().lines().nth(hr as usize).unwrap_or("").to_string();
        assert!(
            help_line.contains("mo"),
            "help cursor row {hr} must sit on the search line; got {help_line:?}\n{}",
            t.screen()
        );
        let after_mo = help_line.find("mo").map(|b| help_line[..b].chars().count() + 2).unwrap() as u16;
        assert_eq!(hc, after_mo, "help cursor col must sit right after filter in {help_line:?}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("modals closed", |s| !s.contains("Pick item") && !s.contains("Keys")).await;

        // 3. Fullwidth ASCII shortcut key (`ａ` -> `a`) triggers extra select action key.
        exec(
            &t.vt.host,
            r#"mare._fw_key = nil
               mare.task.spawn(function()
                 local res = { mare.ui.select({
                   title = "Action keys",
                   allow_search = false,
                   keys = { { key = "a", label = "always" } },
                   items = { { label = "Allow once" }, { label = "Deny" } },
                 }) }
                 mare._fw_key = res[3] or tostring(res[1])
               end)"#,
        )
        .await;
        t.until("action keys modal", |s| s.contains("Action keys")).await;
        t.key(KeyCode::Char('\u{FF41}'), Modifiers::NONE); // fullwidth 'ａ'
        t.until("action keys modal closed", |s| !s.contains("Action keys")).await;
        let got_key: String = eval(&t.vt.host, "return tostring(mare._fw_key)").await;
        assert_eq!(got_key, "a", "fullwidth 'ａ' must trigger extra key 'a'");

        // 4. Fullwidth digit (`２` -> `2`) selects item 2 when allow_search = false.
        exec(
            &t.vt.host,
            r#"mare._fw_digit = nil
               mare.task.spawn(function()
                 local idx = mare.ui.select({
                   title = "Numbered pick",
                   allow_search = false,
                   items = { { label = "First" }, { label = "Second" } },
                 })
                 mare._fw_digit = idx
               end)"#,
        )
        .await;
        t.until("numbered modal", |s| s.contains("Numbered pick")).await;
        t.key(KeyCode::Char('\u{FF12}'), Modifiers::NONE); // fullwidth '２'
        t.until("numbered modal closed", |s| !s.contains("Numbered pick")).await;
        let got_digit: i64 = eval(&t.vt.host, "return mare._fw_digit or 0").await;
        assert_eq!(got_digit, 2, "fullwidth '２' must pick the second item");

        // 5. Fullwidth `ｙ` confirms a confirm dialog.
        exec(
            &t.vt.host,
            r#"mare._fw_confirm = nil
               mare.task.spawn(function()
                 mare._fw_confirm = mare.ui.confirm("Proceed with action?")
               end)"#,
        )
        .await;
        t.until("confirm dialog", |s| s.contains("Proceed with action?")).await;
        t.key(KeyCode::Char('\u{FF59}'), Modifiers::NONE); // fullwidth 'ｙ'
        t.until("confirm dialog closed", |s| !s.contains("Proceed with action?")).await;
        let confirmed: bool = eval(&t.vt.host, "return mare._fw_confirm == true").await;
        assert!(confirmed, "fullwidth 'ｙ' must confirm the dialog");
    })
    .await
}

/// Modal scroll offsets clamp back into modal state on render/resize, and pressing "bottom"
/// before the first frame renders never traps scroll at `usize::MAX`.
#[tokio::test]
async fn info_modal_scroll_clamps_on_resize_and_bottom_before_first_render() {
    local(async {
        let mut t = Term::new(json!([]), 80, 14).await;
        t.until_vt("startup settled", |t| t.vt.startup_finished()).await;
        let lines_lua = r#"
            local lines = {}
            for i = 1, 30 do lines[i] = string.format("ROW-%02d", i) end
            mare.ui.overlay("ScrollTest", table.concat(lines, "\n"), { context = "info" })
        "#;

        // 1. Scroll to bottom, grow terminal height, press Up once -> top visible row moves up by 1 immediately.
        exec(&t.vt.host, lines_lua).await;
        t.until("info overlay shown", |s| s.contains("ROW-01")).await;
        t.key(KeyCode::End, Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("ROW-30"), "expected bottom row after End:\n{}", t.screen());

        t.resize(80, 24).await;
        let top_before = first_row_num(&t.screen()).expect("visible ROW-NN after resize");
        assert!(t.screen().contains("ROW-30"), "bottom row stays visible after growing height:\n{}", t.screen());
        t.key(KeyCode::Char('k'), Modifiers::NONE);
        t.step().await;
        let top_after = first_row_num(&t.screen()).expect("visible ROW-NN after pressing k");
        assert_eq!(
            top_after,
            top_before - 1,
            "single Up after growing terminal height must scroll up by 1 immediately:\n{}",
            t.screen()
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("first overlay closed", |s| !s.contains("ScrollTest")).await;

        // 2. Press "bottom" BEFORE the first render of a fresh overlay -> render -> Up moves off bottom immediately.
        exec(&t.vt.host, lines_lua).await;
        t.vt.drain_cmds();
        t.key(KeyCode::End, Modifiers::NONE);
        t.step().await;
        let bottom_screen = t.screen();
        assert!(bottom_screen.contains("ROW-30"), "bottom before first render should show ROW-30:\n{bottom_screen}");
        let top_at_bottom = first_row_num(&bottom_screen).expect("visible ROW-NN at bottom");

        t.key(KeyCode::Char('k'), Modifiers::NONE);
        t.step().await;
        let after_up = first_row_num(&t.screen()).expect("visible ROW-NN after single k");
        assert_eq!(
            after_up,
            top_at_bottom - 1,
            "Up after pre-render bottom must not be trapped at usize::MAX:\n{}",
            t.screen()
        );
    })
    .await
}

fn first_row_num(screen: &str) -> Option<usize> {
    for line in screen.lines() {
        if let Some(pos) = line.find("ROW-") {
            let digits: String = line[pos + 4..].chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(n) = digits.parse::<usize>() {
                return Some(n);
            }
        }
    }
    None
}
