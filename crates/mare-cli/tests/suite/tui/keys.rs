//! Esc, Ctrl+C and Ctrl+D in every state.

use super::*;

#[tokio::test]
async fn idle_interrupt_offers_a_transient_undo_hint_with_the_bound_key() {
    local(async {
        let mut t = Term::new(json!([]), 120, 20).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(
            &t.vt.host,
            "INTERRUPT_SEEN = 0; mare.on('key/interrupt', function() INTERRUPT_SEEN = INTERRUPT_SEEN + 1 end)",
        )
        .await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !t.screen().contains("Esc again to undo") && Instant::now() < deadline {
            t.step().await;
            tokio::task::yield_now().await;
        }
        let keys: String = eval(&t.vt.host, "return mare.json.encode(mare.keymap.hint('interrupt', 'editor'))").await;
        let seen: i64 = eval(&t.vt.host, "return INTERRUPT_SEEN").await;
        assert!(
            t.screen().contains("Esc again to undo"),
            "keys: {keys}, events: {seen}, problems: {:?}, screen: {}",
            t.vt.host.kernel.problems(),
            t.screen()
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("undo hint cleared", |s| !s.contains("Esc again to undo")).await;
        if t.screen().contains("Undo") {
            t.key(KeyCode::Escape, Modifiers::NONE);
            t.step().await;
        }
        exec(
            &t.vt.host,
            r#"mare.keymap.set('editor', '<Esc>', false); mare.keymap.set('editor', '<C-j>', 'interrupt')"#,
        )
        .await;
        t.step().await;
        t.ctrl('j');
        t.until("rebound undo hint", |s| s.contains("Ctrl+J again to undo")).await;
    })
    .await
}

#[tokio::test]
async fn queued_input_navigation_and_hints_follow_rebound_keys() {
    local(async {
        let mut t = Term::new(json!([{ "text": "z".repeat(4000), "delay_ms": 20 }]), 140, 24).await;
        t.submit("start").await;
        t.until("working", |s| s.contains("Responding") || s.contains("Thinking")).await;
        t.vt.type_str("first queued");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("first queued input", |s| s.contains("↳ first queued")).await;
        t.until_vt("empty input after the first queue", |t| t.vt.editor_text().is_empty()).await;
        t.vt.type_str("second queued");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("second queued input", |s| s.contains("↳ second queued")).await;
        t.until_vt("empty input after queuing", |t| t.vt.editor_text().is_empty()).await;
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('text', '<Up>', false)
            mare.keymap.set('text', '<C-p>', false)
            mare.keymap.set('text', '<C-b>', 'text.up')
            mare.keymap.set('editor', '<CR>', false)
            mare.keymap.set('editor', '<C-j>', 'editor.submit')
        "#,
        )
        .await;
        t.step().await;
        t.key(KeyCode::Up, Modifiers::NONE);
        t.step().await;
        assert!(!t.screen().contains("▸ second queued"), "unbound Up must not select the queue");
        assert_eq!(t.vt.editor_text(), "", "unbound Up must not recall history");
        let keys: String = eval(&t.vt.host, "return mare.json.encode(mare.keymap.hint('text.up', 'editor'))").await;
        assert!(keys.contains("ctrl+b"), "the new key must be registered: {keys}");
        t.ctrl('b');
        t.until("the rebound key selects the queue", |s| s.contains("▸ second queued")).await;
        assert!(
            t.screen().contains("Ctrl+B") && t.screen().contains("Ctrl+J"),
            "queue hints must use the live keys: {}",
            t.screen()
        );
        t.ctrl('b');
        t.until("the rebound key moves within the queue", |s| s.contains("▸ first queued")).await;
        t.ctrl('j');
        t.until_vt("queued input for editing", |t| t.vt.editor_text() == "first queued").await;
    })
    .await
}

#[tokio::test]
async fn keymap_default_off_leaves_core_actions_unbound() {
    local(async {
        let config = json!({"plugins": {"keymap-default": {"enabled": false}}});
        let mut t = Term::with(json!([]), config, 80, 24).await;
        t.ctrl('t');
        t.step().await;
        assert!(t.screen().contains("Ask mare"), "Ctrl+T must leave the input visible: {}", t.screen());
        assert!(eval::<bool>(&t.vt.host, r#"for _, b in ipairs(mare.keymap.list()) do if b.action == "transcript" then return false end end return true"#).await);
        assert!(eval::<bool>(&t.vt.host, r#"for _, b in ipairs(mare.keymap.list()) do if b.action == "agents" then return true end end return false"#).await, "other plugins keep their keys");
    }).await;
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.ctrl('t');
        t.step().await;
        assert!(t.screen().contains("Transcript (expanded)"), "{}", t.screen());
    })
    .await;
}

#[tokio::test]
async fn init_lua_binding_beats_a_later_plugin_binding() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({}), 80, 24, |env| {
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(
                env.config_dir().join("init.lua"),
                r#"mare.keymap.set("editor", "<C-l>", function() mare.g.HIT = "user" end, { desc = "user redraw" })"#,
            )
            .unwrap();
        })
        .await;
        exec(
            &t.vt.host,
            r#"mare.keymap.set("editor", "<C-l>", function() mare.g.HIT = "plugin" end, { desc = "plugin redraw" })"#,
        )
        .await;
        t.step().await;
        t.ctrl('l');
        assert_eq!(eval::<String>(&t.vt.host, "return mare.g.HIT").await, "user");
        t.vt.host.reload_config().await.unwrap();
        exec(&t.vt.host, "mare.g.HIT = nil").await;
        t.step().await;
        t.ctrl('l');
        assert_eq!(eval::<String>(&t.vt.host, "return mare.g.HIT").await, "user");
        exec(&t.vt.host, r#"mare.keymap.del("editor", "<C-l>"); mare.keymap.del("editor", "<C-l>"); mare.g.HIT = nil"#)
            .await;
        t.step().await;
        t.ctrl('l');
        assert_eq!(
            eval::<String>(&t.vt.host, "return mare.g.HIT").await,
            "user",
            "runtime del cannot remove the user binding"
        );
    })
    .await;
}

#[tokio::test]
async fn false_unbinds_until_removed() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({}), 80, 24, |env| {
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(env.config_dir().join("init.lua"), r#"mare.keymap.set("editor", "<C-t>", false)"#).unwrap();
        }).await;
        t.ctrl('t');
        t.step().await;
        assert!(t.screen().contains("Ask mare"), "Ctrl+T must leave the input visible: {}", t.screen());
        assert!(eval::<bool>(&t.vt.host, r#"if #mare.keymap.hint("transcript", "editor") ~= 0 then return false end for _, b in ipairs(mare.keymap.list()) do if b.context == "editor" and b.key == "ctrl+t" then return false end end return true"#).await);
        exec(&t.vt.host, r#"H = mare.keymap.set("editor", "<C-o>", false, { unique = true })"#).await;
        t.step().await;
        t.vt.dismiss_notices();
        t.ctrl('o');
        t.step().await;
        assert!(!t.screen().contains("Tool output:"), "{}", t.screen());
        exec(&t.vt.host, "H:remove()").await;
        t.step().await;
        t.ctrl('o');
        t.step().await;
        assert!(t.screen().contains("Tool output:"), "{}", t.screen());
        assert!(eval::<bool>(&t.vt.host, r#"local ok, err = pcall(mare.keymap.set, "editor", "<C-o>", false, { when = "working" }); return not ok and err:find("mare.keymap.set: an unbinding %(false%) takes no `when`") ~= nil"#).await);
    }).await;
}

#[tokio::test]
async fn del_removes_only_its_own_layer() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({}), 80, 24, |env| {
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(env.config_dir().join("init.lua"), r#"mare.keymap.del("editor", "<C-t>")"#).unwrap();
        }).await;
        t.ctrl('t');
        t.step().await;
        assert!(t.screen().contains("Transcript (expanded)"), "{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back on input", |s| s.contains("Ask mare")).await;
        // Reload while keymap-default is live: user-layer del still cannot remove runtime defaults.
        t.vt.host.reload_config().await.unwrap();
        t.ctrl('t');
        t.step().await;
        assert!(t.screen().contains("Transcript (expanded)"), "reload del must keep default: {}", t.screen());
        exec(&t.vt.host, r#"mare.keymap.set("editor", "<C-y>", "redraw"); mare.keymap.del("editor", "<C-y>")"#).await;
        assert!(eval::<bool>(&t.vt.host, r#"for _, b in ipairs(mare.keymap.list()) do if b.context == "editor" and b.key == "ctrl+y" then return false end end return true"#).await);
    }).await;
}

/// SE bug 2: Esc Esc rewinds only with an empty input (Codex app_backtrack.rs:150); with a
/// draft it does nothing, and a rewind never puts the old prompt over a draft.
/// it opens the /undo picker (newest prompt first) instead of rewinding one prompt at once.
#[tokio::test]
async fn double_esc_never_overwrites_a_draft() {
    local(async {
        let mut t = Term::new(json!([{ "text": "first answer" }, { "text": "second answer" }]), 80, 24).await;
        t.submit("question one").await;
        t.idle().await;
        t.submit("question two").await;
        t.idle().await;
        t.vt.type_str("my draft");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        for _ in 0..5 {
            t.step().await;
        }
        assert_eq!(t.vt.editor_text(), "my draft", "a draft is never replaced");
        // emptied, Esc Esc rewinds: the last prompt comes back to the input
        t.key(KeyCode::Char('c'), Modifiers::CONTROL);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the rewind picker", |s| s.contains("Rewind the conversation") && s.contains("question one")).await;
        assert_eq!(t.vt.editor_text(), "", "the picker rewinds nothing by itself");
        t.key(KeyCode::Enter, Modifiers::NONE);
        let end = Instant::now() + WAIT;
        while t.vt.editor_text() != "question two" {
            assert!(Instant::now() < end, "no rewind; input: {:?}", t.vt.editor_text());
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
}

// ---------------------------------------------------------------------------------------------
// Esc, Ctrl+C and Ctrl+D in every state (one test per row group of the table)
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn ctrl_c_closes_a_transcript_with_a_committed_search() {
    local(async {
        let mut t = Term::new(json!([{ "text": "needle in the haystack" }]), 80, 24).await;
        t.submit("find it").await;
        t.idle().await;
        let in_transcript = |s: &str| s.contains("Transcript (expanded)");
        t.ctrl('t');
        t.until("transcript open", in_transcript).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("needle");
        t.until("search being typed", |s| s.contains("/needle")).await;
        t.ctrl('c');
        t.until("typing search cancelled", |s| in_transcript(s) && !s.contains("/needle")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("needle");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("search committed", |s| s.contains("/needle  1 of")).await;
        t.ctrl('c');
        t.step().await;
        assert!(!in_transcript(&t.screen()), "Ctrl+C must close the transcript after a committed search");
    })
    .await
}

#[tokio::test]
async fn ctrl_c_cancels_a_pending_prefix_and_keeps_the_draft() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(&t.vt.host, "mare.keymap.set('editor', '<C-x>g', 'redraw')").await;
        t.step().await; // deliver the new binding before testing the prefix
        t.vt.type_str("abc");
        t.ctrl('x');
        t.ctrl('c');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "abc");
        assert!(!t.screen().contains("again to exit"));
        t.ctrl('c');
        assert_eq!(t.vt.editor_text(), "");
    })
    .await
}

/// turning keymap-default off drops the core actions' default keys, not Ctrl+C.
#[tokio::test]
async fn ctrl_c_works_without_keymap_default() {
    local(async {
        let config = json!({"plugins": {"keymap-default": {"enabled": false}}});
        let mut t = Term::with(json!([]), config, 80, 24).await;
        t.vt.type_str("abc");
        t.ctrl('c');
        assert_eq!(t.vt.editor_text(), "", "Ctrl+C clears the draft");
        t.ctrl('c');
        t.until("the exit hint", |s| s.contains("Press Ctrl+C again to exit")).await;
        assert!(!t.vt.quit_requested());
        t.ctrl('c');
        assert!(t.vt.quit_requested(), "two presses on an empty input exit");
    })
    .await
}

#[tokio::test]
async fn setup_keys_reserved_ctrl_c_reports_error_and_keeps_submit() {
    local(async {
        let mut t = Term::build(json!([{ "text": "ok" }]), json!({}), json!({}), 80, 24, |env| {
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(env.config_dir().join("init.lua"), r#"mare.setup { keys = { ["<C-c>"] = "redraw" } }"#)
                .unwrap();
        })
        .await;
        t.until("reserved key notice", |s| s.contains("reserved")).await;
        t.submit("hello").await;
        t.idle().await;
        assert!(t.history().contains("ok"), "submit routing must still work:\n{}", t.history());
    })
    .await
}

/// Rows 1, 2 and 11 (idle): Esc never clears a draft, single or multi-line; Ctrl+D deletes
/// forward on a draft and exits on an empty input; Ctrl+C clears a draft into the history
/// (the editor's undo, Ctrl+_, restores it) without arming the exit, and on an empty input exits on the second press.
#[tokio::test]
async fn idle_keys_keep_drafts_and_exit_on_two_ctrl_c() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.type_str("my draft");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "my draft", "Esc never clears a draft");
        t.key(KeyCode::Home, Modifiers::NONE);
        t.ctrl('d');
        assert_eq!(t.vt.editor_text(), "y draft", "Ctrl+D deletes the character after the cursor");
        t.ctrl('c');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "");
        assert!(!t.screen().contains("again to exit"), "clearing a draft doesn't arm the exit");
        t.ctrl('_');
        assert_eq!(t.vt.editor_text(), "y draft", "undo brings the cleared draft back");
        t.ctrl('c');
        t.key(KeyCode::Up, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "y draft", "the cleared draft went into the history");
        t.ctrl('c');
        // multi-line (row 11): same as a one-line draft
        t.vt.type_str("line one");
        t.key(KeyCode::Enter, Modifiers::SHIFT);
        t.vt.type_str("line two");
        t.key(KeyCode::Escape, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "line one\nline two");
        t.ctrl('c');
        assert_eq!(t.vt.editor_text(), "");
        // empty: the first Ctrl+C only says how to exit, the second exits
        t.ctrl('c');
        t.until("the exit hint", |s| s.contains("Press Ctrl+C again to exit")).await;
        assert!(!t.vt.quit_requested());
        t.ctrl('c');
        assert!(t.vt.quit_requested(), "two presses within 2 s exit");
    })
    .await
}

/// Row 3 (an answer streaming): Ctrl+D never exits (it says how to stop); Ctrl+C clears a draft
/// before it interrupts; Esc interrupts and gives the queued messages back above the draft; the
/// interrupt marker for the model never shows; only after the interrupt does Ctrl+C arm the exit.
#[tokio::test]
async fn streaming_keys_interrupt_and_give_queued_messages_back() {
    local(async {
        let slow = "z".repeat(4000);
        let mut t = Term::new(json!([{ "text": slow, "delay_ms": 20 }]), 80, 24).await;
        t.submit("go slowly").await;
        t.until("streaming", |s| s.contains("zzzzzzzz")).await;
        t.ctrl('d');
        t.until("the stop hint", |s| s.contains("Ctrl+C to stop")).await;
        assert!(!t.vt.quit_requested() && t.vt.busy(), "Ctrl+D doesn't exit or stop while working");
        t.vt.type_str("queued message");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the queued message", |s| s.contains("↳ queued message")).await;
        t.vt.type_str("scratch");
        t.ctrl('c');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "", "a draft is cleared first");
        assert!(t.vt.busy(), "…without interrupting");
        t.vt.type_str("my draft");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("the turn to end", |t| !t.vt.busy()).await;
        t.until_vt("the queue back in the input", |t| t.vt.editor_text() == "queued message\n\nmy draft").await;
        t.until("the interrupted answer", |s| s.contains("Interrupted")).await;
        let h = t.history();
        assert!(!h.contains("turn_aborted"), "the model-only marker never shows:\n{h}");
        t.ctrl('c');
        assert_eq!(t.vt.editor_text(), "");
        t.ctrl('c');
        t.until("the exit hint", |s| s.contains("Press Ctrl+C again to exit")).await;
        assert!(!t.vt.quit_requested(), "the interrupt didn't count toward the exit");
    })
    .await
}

/// with messages queued, ↑ selects one (↑/↓ move), Enter loads it into the input,
/// Enter saves it back at the same place, Esc puts it back unchanged and never interrupts the
/// turn, Delete removes the selected one; the queue handed back by an interrupt is in order.
#[tokio::test]
async fn a_queued_message_is_chosen_and_edited_in_place() {
    local(async {
        let slow = "z".repeat(4000);
        let mut t = Term::new(json!([{ "text": slow, "delay_ms": 20 }]), 80, 24).await;
        t.submit("go slowly").await;
        t.until("streaming", |s| s.contains("zzzzzzzz")).await;
        for m in ["first", "second", "third"] {
            t.vt.type_str(m);
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("the message queued", |s| s.contains(&format!("↳ {m}"))).await;
        }
        // ↑ selects the newest, ↑ again the one before it
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("the newest selected", |s| s.contains("▸ third")).await;
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("the one before selected", |s| s.contains("▸ second") && s.contains("↳ third")).await;
        // Enter loads it; Esc puts it back unchanged and the turn goes on
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("the message in the input", |t| t.vt.editor_text() == "second").await;
        t.until("marked as being edited", |s| s.contains("✎ second") && s.contains("(editing)")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("the input empty again", |t| t.vt.editor_text().is_empty()).await;
        t.until("the queue unchanged", |s| s.contains("↳ second") && !s.contains("(editing)")).await;
        assert!(t.vt.busy(), "Esc on a queue edit never interrupts the turn");
        // edit "second" and save it: same place, new text, nothing new queued
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("the message in the input", |t| t.vt.editor_text() == "second").await;
        t.vt.type_str(" edited");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the edit saved", |s| s.contains("↳ second edited") && !s.contains("(editing)")).await;
        t.until_vt("the input empty", |t| t.vt.editor_text().is_empty()).await;
        // Delete removes the selected one
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("the oldest selected", |s| s.contains("▸ first")).await;
        t.key(KeyCode::Delete, Modifiers::NONE);
        t.until("it removed", |s| !s.contains("first") && s.contains("▸ second edited")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        assert!(t.vt.busy(), "Esc leaves the selection without interrupting");
        // the interrupt hands back what is left, in order
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("the turn to end", |t| !t.vt.busy()).await;
        t.until_vt("the queue back in the input", |t| t.vt.editor_text() == "second edited\n\nthird").await;
    })
    .await
}

#[tokio::test]
async fn queue_edit_ctrl_c_cancels_editing_without_overwriting_deleting_or_interrupting() {
    local(async {
        let slow = "z".repeat(4000);
        let mut t = Term::new(json!([{ "text": slow, "delay_ms": 20 }]), 80, 24).await;
        t.submit("go slowly").await;
        t.until("streaming", |s| s.contains("zzzzzzzz")).await;
        for m in ["first", "second"] {
            t.vt.type_str(m);
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("the message queued", |s| s.contains(&format!("↳ {m}"))).await;
        }

        // 1. EDITING (non-empty) -> Ctrl+C -> type "third" + Enter: must queue "third" without overwriting "second".
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("second selected", |s| s.contains("▸ second")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("second loaded", |t| t.vt.editor_text() == "second").await;
        t.until("editing indicator", |s| s.contains("(editing)")).await;
        t.ctrl('c');
        t.until_vt("draft cleared", |t| t.vt.editor_text().is_empty()).await;
        t.until("editing cancelled", |s| !s.contains("(editing)")).await;
        assert!(t.vt.busy(), "Ctrl+C while editing a queued message must not interrupt the turn");
        t.vt.type_str("third");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("third queued alongside first and second", |s| {
            s.contains("↳ first") && s.contains("↳ second") && s.contains("↳ third")
        })
        .await;

        // 2. EDITING -> Ctrl+C -> Enter on empty box must NOT delete "second".
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("second selected again", |s| s.contains("▸ second")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("second loaded again", |t| t.vt.editor_text() == "second").await;
        t.ctrl('c');
        t.until_vt("cleared again", |t| t.vt.editor_text().is_empty()).await;
        t.until("editing cancelled again", |s| !s.contains("(editing)")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("queue menu opens instead of deleting", |s| s.contains("Queued messages")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("queue menu closed and second intact", |s| !s.contains("Queued messages") && s.contains("↳ second"))
            .await;

        // 3. EDITING -> backspace to empty -> Ctrl+C must cancel editing without interrupting the turn.
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("second selected third time", |s| s.contains("▸ second")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("second loaded third time", |t| t.vt.editor_text() == "second").await;
        for _ in 0..6 {
            t.key(KeyCode::Backspace, Modifiers::NONE);
        }
        t.until_vt("editor emptied manually", |t| t.vt.editor_text().is_empty()).await;
        t.ctrl('c');
        t.until("editing cancelled on empty editor", |s| !s.contains("(editing)")).await;
        assert!(t.vt.busy(), "Ctrl+C on empty queue-edit buffer must cancel editing, not interrupt the turn");
        assert!(t.screen().contains("↳ second"), "second must remain queued:\n{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn queue_edit_is_cancelled_when_editor_is_replaced_by_history_or_set_editor() {
    local(async {
        let slow = "z".repeat(4000);
        let mut t = Term::new(json!([{ "text": slow, "delay_ms": 20 }]), 80, 24).await;
        t.submit("go slowly").await;
        t.until("streaming", |s| s.contains("zzzzzzzz")).await;
        for m in ["first", "second"] {
            t.vt.type_str(m);
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("the message queued", |s| s.contains(&format!("↳ {m}"))).await;
        }

        // 1. Plugin mare.ui.set_editor replaces editor content -> leaves EDITING so Enter does not overwrite.
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("second selected", |s| s.contains("▸ second")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("second loaded", |t| t.vt.editor_text() == "second").await;
        t.until("editing indicator", |s| s.contains("(editing)")).await;
        exec(&t.vt.host, r#"mare.ui.set_editor("replaced")"#).await;
        t.until_vt("editor replaced", |t| t.vt.editor_text() == "replaced").await;
        t.until("editing cancelled after set_editor", |s| !s.contains("(editing)")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("replaced queued without overwriting second", |s| {
            s.contains("↳ first") && s.contains("↳ second") && s.contains("↳ replaced")
        })
        .await;

        // 2. Ctrl+R history search replaces editor via AppMsg::SetEditor -> leaves EDITING.
        t.key(KeyCode::Up, Modifiers::NONE);
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("second selected for history search", |s| s.contains("▸ second")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("second loaded for history search", |t| t.vt.editor_text() == "second").await;
        t.ctrl('r');
        t.until("history search open", |s| s.contains("Search history")).await;
        t.vt.type_str("go slowly");
        t.step().await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("history entry recalled", |t| t.vt.editor_text() == "go slowly").await;
        t.until("editing cancelled after history recall", |s| !s.contains("(editing)")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("history entry queued without overwriting second", |s| {
            s.contains("↳ second") && s.contains("↳ go slowly")
        })
        .await;
    })
    .await
}

#[tokio::test]
async fn queue_edit_is_cancelled_on_steer_and_slash_commands() {
    local(async {
        let slow = "z".repeat(4000);
        let mut t = Term::new(json!([{ "text": slow, "delay_ms": 20 }]), 80, 24).await;
        t.submit("go slowly").await;
        t.until("streaming", |s| s.contains("zzzzzzzz")).await;
        for m in ["first", "second"] {
            t.vt.type_str(m);
            t.key(KeyCode::Enter, Modifiers::NONE);
            t.until("the message queued", |s| s.contains(&format!("↳ {m}"))).await;
        }

        // 1. Ctrl+S (steer) while EDITING must abandon ghost editing state.
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("second selected", |s| s.contains("▸ second")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("second loaded", |t| t.vt.editor_text() == "second").await;
        t.until("editing indicator", |s| s.contains("(editing)")).await;
        t.ctrl('s');
        t.until("editing cleared after steer", |s| !s.contains("(editing)") && s.contains("(steering)")).await;

        // 2. Slash command while EDITING must abandon ghost editing state.
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("item selected", |s| s.contains("▸ ")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("editing started", |s| s.contains("(editing)")).await;
        // Replace with a slash command via typing (select all + type)
        t.ctrl('a');
        t.ctrl('k');
        t.vt.type_str("/notifications");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("notifications overlay opened", |s| s.contains("Notifications")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("overlay closed and editing cleared", |s| !s.contains("Notifications") && !s.contains("(editing)"))
            .await;
    })
    .await
}

/// Whether a process runs with exactly this command line (`ps`, macOS and Linux).
#[cfg(unix)]
fn process_running(command_line: &str) -> bool {
    let out = std::process::Command::new("ps").args(["-axo", "args="]).output().expect("ps");
    String::from_utf8_lossy(&out.stdout).lines().any(|l| l.trim() == command_line)
}

/// Row 4 (a command running): Esc interrupts and the working line says how to force the stop;
/// Ctrl+C then forces it (the real SIGKILL is covered in tui_pty.rs).
#[cfg(unix)]
#[tokio::test]
async fn a_running_command_stops_on_esc_and_ctrl_c_forces_it() {
    local(async {
        // The shell runs `trap` before it starts the sleep, so a process whose whole command line
        // is this (odd) `sleep` says the traps are in place: a stop sent earlier kills the shell
        // at once and never shows "stopping…" (a race under load). The workspace is untrusted
        // here, so the permissions plugin runs read-only whatever the config says (permissions.lua
        // clamps an untrusted project to read-only; the status line shows READ): the tool's sandbox
        // lets the command write no marker file, and its output is shown only when it ends.
        const NAP: &str = "sleep 30.25";
        let script = json!([
            // the traps run in the persistent shell itself, which shares its process group with
            // the sleep: both ignore the stop's SIGINT (1 s) and SIGTERM (3 s grace), so the stop
            // stays in "stopping…" for seconds and only the second Ctrl+C's SIGKILL ends it; a
            // command that exits on the first signal rightly shows "interrupted" at once
            { "tool_calls": [{ "name": "bash", "arguments": { "command": format!("trap '' INT TERM HUP; {NAP}"), "description": "nap" } }] },
            { "text": "unreachable" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "preset": "read-only" } } });
        let mut t = Term::with(script, cfg, 80, 20).await;
        t.submit("nap").await;
        t.until("the command running", |s| s.contains("Esc to interrupt") && s.contains("nap")).await;
        t.until_vt("the command to have set its traps", |_| process_running(NAP)).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the stopping hint", |s| s.contains("stopping… (Ctrl+C again to force)")).await;
        // a forced stop reaches only trees already registered (or registering within 3 s). The
        // window lasts the whole polite stop. (A past CI failure, "interrupted after 0s" with the
        // stop never seen active, was not a too-short grace: under bubblewrap the process group
        // was led by bwrap's monitor, so the polite SIGINT killed the whole sandbox at once; fixed
        // in platform/process.rs, e860379.)
        t.until_vt("the cancellation to reach the command's process tree", |_| mare_binding::stopping::active()).await;
        t.ctrl('c');
        t.until_vt("the turn to end", |t| !t.vt.busy()).await;
        assert!(!t.vt.quit_requested());
    })
    .await
}

/// Row 5 (the approval dialog): Esc denies this call and the turn goes on; Ctrl+C denies and
/// ends the turn.
#[tokio::test]
async fn approval_esc_denies_the_call_and_ctrl_c_also_stops_the_turn() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "A" } }] },
            { "text": "went on after the denial" },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "b.txt", "content": "B" } }] },
            { "text": "never said" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });
        let mut t = Term::with(script, cfg, 80, 24).await;
        let cwd = t._env.cwd.clone();
        t.submit("write a").await;
        t.until("the approval dialog", |s| s.contains("Allow once") && s.contains("Deny and stop")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.idle().await;
        assert!(!cwd.join("a.txt").exists());
        assert!(t.history().contains("went on after the denial"), "Esc denies the call, the turn goes on");
        t.submit("write b").await;
        t.until("the second dialog", |s| s.contains("Allow once")).await;
        t.ctrl('c');
        t.idle().await;
        assert!(!cwd.join("b.txt").exists());
        let h = t.history();
        assert!(!h.contains("never said"), "Ctrl+C ends the turn:\n{h}");
        assert!(!t.vt.quit_requested());
    })
    .await
}

/// Rows 6 and 7: an overlay closes on Esc or Ctrl+C and ignores Ctrl+D; the completion menu
/// closes on Esc or Ctrl+C (never an interrupt, never clearing the input) and ignores Ctrl+D.
#[tokio::test]
async fn overlays_and_the_completion_menu_take_esc_and_ctrl_c_first() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.dismiss_notices();
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("the key menu", |s| s.contains("─ Keys ─")).await;
        t.ctrl('d');
        t.step().await;
        assert!(t.screen().contains("─ Keys ─") && !t.vt.quit_requested(), "Ctrl+D does nothing over an overlay");
        t.ctrl('c');
        t.until("the menu to close", |s| s.contains("Ask mare")).await;
        assert!(!t.screen().contains("again to exit"), "closing the overlay isn't half an exit");
        // the Ctrl+T transcript closes on Ctrl+C too
        t.ctrl('t');
        t.step().await;
        t.ctrl('c');
        t.until("back from the transcript", |s| s.contains("Ask mare")).await;
        for key in ['\x1b', 'c'] {
            t.vt.type_str("/");
            t.until("the completion menu", |s| s.contains("/compact")).await;
            t.ctrl('d');
            t.step().await;
            assert_eq!(t.vt.editor_text(), "/", "Ctrl+D does nothing with the menu open");
            if key == '\x1b' {
                t.key(KeyCode::Escape, Modifiers::NONE);
            } else {
                t.ctrl('c');
            }
            t.until("the menu to close", |s| !s.contains("/compact")).await;
            assert_eq!(t.vt.editor_text(), "/", "only the menu closes");
            t.key(KeyCode::Backspace, Modifiers::NONE);
        }
        assert!(!t.vt.quit_requested());
    })
    .await
}

/// Row 8 (`!` shell mode): Esc on a bare `!` leaves the mode; Esc aborts a running `!cmd` (and
/// isn't half a double-Esc); Ctrl+C aborts one too.
#[cfg(unix)]
#[tokio::test]
async fn shell_mode_esc_leaves_it_and_esc_or_ctrl_c_abort_the_command() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.type_str("!");
        t.key(KeyCode::Escape, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "", "Esc on an empty shell line leaves shell mode");
        t.vt.type_str("!ls");
        t.key(KeyCode::Escape, Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "!ls", "with a command typed, Esc keeps it");
        t.ctrl('c');
        for stop in ['\x1b', 'c'] {
            let t0 = Instant::now();
            t.submit("!sleep 30").await;
            t.until("the command running", |s| s.contains("Running sleep 30")).await;
            if stop == '\x1b' {
                t.key(KeyCode::Escape, Modifiers::NONE);
            } else {
                t.ctrl('c');
            }
            t.until_vt("the command to stop", |t| !t.vt.busy()).await;
            assert!(t0.elapsed() < Duration::from_secs(25), "the stop waited for the 30 s sleep");
            t.until("the abort note", |s| s.contains("[command aborted]")).await;
            if stop == '\x1b' {
                // had the aborting Esc counted, this one would complete a double-Esc (/undo)
                t.vt.dismiss_notices();
                t.key(KeyCode::Escape, Modifiers::NONE);
                for _ in 0..5 {
                    t.step().await;
                }
                assert!(!t.screen().contains("Nothing to undo"), "the abort wasn't half a double-Esc");
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
        }
    })
    .await
}

/// Row 9 (compaction running): Esc cancels it quietly — no error, the conversation as it was.
#[tokio::test]
async fn esc_cancels_a_compaction_without_an_error() {
    local(async {
        let slow = format!("## Summary\n{}", "s".repeat(4000));
        // Keep this history large enough that a summary can shrink it; otherwise
        // the feasibility precheck correctly finishes before there is anything to cancel.
        let a1 = format!("a1 {}", "work ".repeat(1000));
        let a2 = format!("a2 {}", "work ".repeat(1000));
        let script = json!([{ "text": a1 }, { "text": a2 }, { "text": slow, "delay_ms": 20 }]);
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let mut t = Term::with(script, cfg, 80, 24).await;
        t.submit("one").await;
        t.idle().await;
        t.submit("two").await;
        t.idle().await;
        t.submit("/compact").await;
        t.until("the compaction running", |s| s.contains("Compacting conversation")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the quiet cancel", |s| s.contains("Compaction cancelled")).await;
        t.until_vt("the working line to go", |t| !t.vt.busy()).await;
        let s = t.screen();
        assert!(!s.contains("failed") && !s.contains("skipped") && !s.contains("incomplete"), "{s}");
        let h = t.history();
        assert!(h.contains("a1") && h.contains("a2") && !h.contains("Compacted conversation"), "{h}");
    })
    .await
}

/// "另外": with a selection in the live area, Ctrl+C copies it — no clear, no interrupt.
#[tokio::test]
async fn ctrl_c_with_a_selection_copies_it() {
    local(async {
        use mare_tui::tty::{MouseButton, MouseEventKind};
        unsafe { std::env::set_var("MARE_TEST_CLIPBOARD", "osc52") };
        // a full screen puts the live area at the bottom, where select mode maps mouse rows
        let body: String = (1..=40).map(|i| format!("line {i}\n\n")).collect();
        let mut t = Term::new(json!([{ "text": body }]), 70, 16).await;
        t.submit("fill the screen").await;
        t.idle().await;
        t.vt.type_str("keep me");
        t.vt.action("select-mode");
        t.until("the select-mode hints", |s| s.contains("drag select text")).await;
        let screen = t.screen();
        let (row, line) = screen.lines().enumerate().find(|(_, l)| l.contains("keep me")).expect("the input");
        let col = line.find("keep me").map(|b| line[..b].chars().count()).unwrap() as u16;
        t.vt.mouse(MouseEventKind::Down(MouseButton::Left), col, row as u16);
        t.vt.mouse(MouseEventKind::Drag(MouseButton::Left), col + 3, row as u16);
        t.step().await;
        let _ = t.vt.output();
        t.ctrl('c');
        assert_eq!(t.vt.editor_text(), "keep me", "the draft stays");
        assert!(!t.vt.selecting());
        let want = mare_binding::clipboard::osc52_sequence("keep", false).unwrap();
        let mut out = String::from_utf8_lossy(&t.vt.output()).into_owned();
        let end = Instant::now() + WAIT;
        while !out.contains(&want) {
            assert!(Instant::now() < end, "timed out waiting for OSC 52 copy: {out:?}");
            t.vt.frame().await;
            out.push_str(&String::from_utf8_lossy(&t.vt.output()));
            tokio::time::sleep(Duration::from_millis(15)).await; // sleep-ok: bounded poll waiting for background copy
        }
        unsafe { std::env::remove_var("MARE_TEST_CLIPBOARD") };
    })
    .await
}

/// Row 4b: under a running shell command the working line names the key that moves it to the
/// background; Ctrl+B does it and the turn goes on. Inside tmux (Ctrl+B is its prefix: tmux
/// passes it on after two presses) the hint asks for two presses and names the conflict.
#[cfg(unix)]
#[tokio::test]
async fn ctrl_b_moves_a_running_command_to_the_background() {
    local(async {
        let cfg = json!({ "plugins": { "permissions": { "preset": "full-access" } } });
        for tmux in [false, true] {
            let script = json!([
                { "tool_calls": [{ "name": "bash", "arguments": { "command": "sleep 30", "description": "long nap" } }] },
                { "text": "carried on" }
            ]);
            if tmux {
                unsafe { std::env::set_var("TMUX", "/tmp/tmux-test,1,0") };
            }
            let mut t = Term::with(script, cfg.clone(), 120, 20).await;
            if tmux {
                unsafe { std::env::remove_var("TMUX") };
            }
            t.submit("nap").await;
            let want = if tmux { "Ctrl+B Ctrl+B to run in background (tmux prefix)" } else { "· Ctrl+B to run in background" };
            t.until("the background hint", |s| s.contains(want)).await;
            t.ctrl('b');
            t.idle().await;
            let h = t.history();
            assert!(h.contains("carried on") && h.contains("moved to background job"), "{h}");
        }
    })
    .await
}

/// `mare.ui.working(label, nil, {background = true})` is what earns the background-key hint;
/// the core does not look at the label.
#[tokio::test]
async fn the_background_hint_follows_the_flag_not_the_label() {
    local(async {
        let mut t = Term::new(json!([]), 100, 20).await;
        exec(&t.vt.host, r#"mare.ui.working("Sleeping on it", nil, { background = true })"#).await;
        t.until("the hint", |s| s.contains("Sleeping on it") && s.contains("to run in background")).await;
        exec(&t.vt.host, r#"mare.ui.working("Running bash")"#).await;
        t.until("no hint", |s| s.contains("Running bash") && !s.contains("to run in background")).await;
    })
    .await
}

/// `set_status(.., keep)`: on a narrow terminal a kept left segment stays whole while the rest
/// of the bar gives way; without the flag nothing is privileged.
#[tokio::test]
async fn a_kept_status_segment_stays_whole_on_a_narrow_bar() {
    local(async {
        let mut t = Term::new(json!([]), 40, 20).await;
        let set = |keep: bool| {
            format!(
                r#"local ui = mare.ui
                   ui.set_status("left", "left", nil)
                   ui.set_status("right", "right", nil)
                   ui.set_status("a", "left", ui.Line({{ ui.Span(("A"):rep(20)) }}), 0, {keep})
                   ui.set_status("b", "left", ui.Line({{ ui.Span(("B"):rep(10)) }}), 1)
                   ui.set_status("r", "right", ui.Line({{ ui.Span(("R"):rep(30)) }}), 0)"#
            )
        };
        let (a, r) = ("A".repeat(20), "R".repeat(30));
        exec(&t.vt.host, &set(false)).await;
        t.until("the plain bar", |s| s.contains(&r) && !s.contains(&a)).await;
        exec(&t.vt.host, &set(true)).await;
        t.until("the kept segment", |s| s.contains(&a) && !s.contains(&r)).await;
    })
    .await
}

/// The Lua action contract: an action that returns `false` declines and
/// the key goes on to the editor; one that yields first has taken the key and still finishes; an
/// error takes the key and shows a notice.
#[tokio::test]
async fn lua_actions_decline_yield_or_fail_and_the_key_follows() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(
            &t.vt.host,
            r#"mare.keymap.action("t.decline", function(ev) mare.g.ev = ev.context .. " " .. ev.key; return false end)
               mare.keymap.action("t.later", function() mare.task.sleep(1); mare.g.later = true end)
               mare.keymap.action("t.boom", function() error("boom") end)
               mare.keymap.set("editor", "x", "t.decline")
               mare.keymap.set("editor", "y", "t.later")
               mare.keymap.set("editor", "z", "t.boom")"#,
        )
        .await;
        t.step().await;
        t.vt.type_str("xyz");
        assert_eq!(t.vt.editor_text(), "x", "a declined key reaches the editor, a taken one doesn't");
        t.until("the error notice", |s| s.contains("keymap:") && s.contains("boom")).await;
        let end = Instant::now() + WAIT;
        while eval::<Option<bool>>(&t.vt.host, "return mare.g.later").await != Some(true) {
            assert!(Instant::now() < end, "the action's part after its yield never ran");
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(eval::<String>(&t.vt.host, "return mare.g.ev").await, "editor x");
    })
    .await
}

/// The agents UI's `←` action: a Lua action bound to ← with
/// `when = "editor_empty"` runs on an empty input, and with text in the input the condition
/// fails, the key passes on and ← is the editor's cursor-left — no core action needed.
#[tokio::test]
async fn left_runs_its_binding_on_an_empty_input_else_moves_the_cursor() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(
            &t.vt.host,
            r#"mare.g.left = 0
               mare.keymap.action("t.left", function() mare.g.left = mare.g.left + 1 end)
               mare.keymap.set("editor", "<Left>", "t.left", { when = "editor_empty" })"#,
        )
        .await;
        t.step().await;
        t.key(KeyCode::Left, Modifiers::NONE);
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.left").await, 1, "empty input: the binding");
        t.vt.type_str("ab");
        t.key(KeyCode::Left, Modifiers::NONE);
        t.vt.type_str("X");
        assert_eq!(t.vt.editor_text(), "aXb", "with text: the cursor moved left");
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.left").await, 1, "and the binding didn't run");
    })
    .await
}

#[tokio::test]
async fn queue_menu_moves_and_sends_original_identities_through_the_durable_inbox() {
    local(async {
        for selection in [1,2] {
            let mut t=Term::new(json!([{"text":"sent original inputs"}]),100,32).await;
            expose(&t.vt.host);
            exec(&t.vt.host,r#"
                local moved_tx,moved_rx=mare.task.channel()
                MOVED=moved_rx
                C.cordis:on('queue/changed',function() if MENU_PENDING then MENU_PENDING=false;moved_tx:send(true) end end,{prepend = true})
                local ready_tx,ready_rx=mare.task.channel()
                local release_tx,release_rx=mare.task.channel()
                READY,RELEASE=ready_rx,release_tx
                local first=true
                C.cordis:get("policy"):voter('request/ready',function()
                    if first then first=false;ready_tx:send(true);release_rx:recv() end
                end)
                local consume_tx,consume_rx=mare.task.channel()
                local accept_tx,accept_rx=mare.task.channel()
                CONSUME,ACCEPT=consume_rx,accept_tx
                C.cordis:get("policy"):voter('session/commit-ready',function(batch)
                    if #batch.messages>0 then
                        CONSUMING=batch.messages
                        consume_tx:send(true)
                        accept_rx:recv()
                    end
                end)
            "#).await;
            t.submit("held initial turn").await;
            exec(&t.vt.host,"READY:recv()").await;
            for text in ["first menu input","second menu input"] {
                t.vt.type_str(text);
                t.key(KeyCode::Enter,Modifiers::NONE);
                t.until("queued input",|screen|screen.contains(text)).await;
            }
            let before:String=eval(&t.vt.host,"local q=A:list_queue().data;return q[1].input_id..','..q[2].input_id").await;
            exec(&t.vt.host,"MENU_PENDING=true").await;
            t.key(KeyCode::Enter,Modifiers::NONE);
            t.until("queue menu",|screen|screen.contains("Steer now") && screen.contains("Interrupt and send")).await;
            if selection==2 {t.key(KeyCode::Down,Modifiers::NONE);}
            t.key(KeyCode::Enter,Modifiers::NONE);
            exec(&t.vt.host,"MOVED:recv()").await;
            if selection==1 {
                t.until("both steering entries",|screen|screen.matches("(steering)").count()==2).await;
                let after:String=eval(&t.vt.host,"local q=A:list_queue().data;assert(#q==2 and q[1].lane=='steer' and q[2].lane=='steer');return q[1].input_id..','..q[2].input_id").await;
                assert_eq!(after,before,"Steer now must move the whole batch without replacing identities");
                exec(&t.vt.host,"A:cancel({clear_queue=false});RELEASE:send(true)").await;
                t.until_vt("cancelled held turn",|t|!t.vt.busy()).await;
            } else {
                exec(&t.vt.host,"RELEASE:send(true);CONSUME:recv()").await;
                let after:String=eval(&t.vt.host,"assert(#CONSUMING==2);return CONSUMING[1]._input_id..','..CONSUMING[2]._input_id").await;
                assert_eq!(after,before,"Interrupt and send consumes the original identified messages");
                exec(&t.vt.host,"ACCEPT:send(true)").await;
                t.until_vt("queued turn completed",|t|!t.vt.busy()).await;
            }
            t.vt.host.shutdown().await;
            let h=t._env.host().builtins(&["jobs", "compact", "shell-mode"]).resume().build().await;
            expose(&h);
            if selection==1 {
                let after:String=eval(&h,"local q=A:list_queue().data;assert(#q==2);return q[1].input_id..','..q[2].input_id").await;
                assert_eq!(after,before,"every moved item survives restart");
            } else {
                let (queued,delivered):(i64,i64)=eval(&h,"local n=0;for _,m in ipairs(A:list_messages().data) do if m._input_id then n=n+1 end end;return #A:list_queue().data,n").await;
                assert_eq!((queued,delivered),(0,2),"sent menu inputs never replay after restart");
            }
            h.shutdown().await;
        }
    }).await
}

/// Ordered handler chains: `ui:handler(kind, name, {order}, fn)` runs the handlers of `submit` /
/// `interrupt` in ascending order (equal orders in registration order), stops at the first that
/// returns true, and a disposed handler leaves the chain; with none handling, ui's own routing runs.
#[tokio::test]
async fn ui_handlers_run_in_order_and_a_disposed_one_leaves_the_chain() {
    local(async {
        let mut t = Term::new(json!([{ "text": "routed by ui" }]), 60, 16).await;
        global(&t.vt.host, "UI", t.vt.host.service_table("ui").unwrap());
        exec(
            &t.vt.host,
            r#"ORDER = {}
            local function probe(label, handles)
              return function(ev)
                ORDER[#ORDER + 1] = label .. ":" .. tostring(ev.text or "")
                return handles
              end
            end
            UI:handler("interrupt", "a", { order = 20 }, probe("a", false))
            UI:handler("interrupt", "b", { order = 10 }, probe("b", false))
            STOP = UI:handler("interrupt", "c", { order = 20 }, probe("c", true))
            UI:handler("interrupt", "d", { order = 30 }, probe("d", false))
            UI:handler("submit", "claim", { order = 10 }, function(ev)
              ORDER[#ORDER + 1] = "claim:" .. ev.text
              return ev.text == "mine"
            end)"#,
        )
        .await;
        let host = t.vt.host.clone();
        let recorded = async |n: i64| {
            wait_until(WAIT_SECS, "handlers ran", async || eval::<i64>(&host, "return #ORDER").await >= n).await;
            eval::<Vec<String>>(&host, "return ORDER").await
        };
        t.key(KeyCode::Escape, Modifiers::NONE);
        let order = recorded(4).await;
        // interrupt: every participant runs (one Esc stops everything running, as on the old
        // engine), ascending order, ties in registration order
        assert_eq!(order, vec!["b:", "a:", "c:", "d:"], "ascending order, ties in registration order, all run");
        exec(&t.vt.host, "ORDER = {}; STOP()").await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        let order = recorded(3).await;
        assert_eq!(order, vec!["b:", "a:", "d:"], "a disposed handler leaves the chain");
        // submit: the claimed line stops at the handler (ui's routing never sends it)
        exec(&t.vt.host, "ORDER = {}").await;
        t.vt.type_str("mine");
        t.key(KeyCode::Enter, Modifiers::NONE);
        let order = recorded(1).await;
        assert_eq!(order, vec!["claim:mine"]);
        assert!(!t.screen().contains("routed by ui"), "a claimed line reached the model");
    })
    .await
}

/// The ui:handler chains run without an attached UI too (RPC `command.run`, headless): the host
/// emits input/submit and reads `ev.handled` (commands moved to ui:handler answered
/// "unhandled command" over RPC when only the attached-UI routing ran the chain).
#[tokio::test]
async fn ui_handlers_run_without_an_attached_ui() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "UI", h.service_table("ui").unwrap());
        exec(
            &h,
            r#"SEEN = {}
            local seen = SEEN
            UI:handler("submit", "probe", { order = 10 }, function(ev) seen[#seen + 1] = ev.text; return true end)"#,
        )
        .await;
        // the RPC `command.run` path: the host emits input/submit and reads ev.handled
        let reply = h.submit_command("probe line").await;
        assert!(reply.get("message").and_then(|m| m.as_str()).is_none_or(|m| !m.contains("unhandled")), "{reply}");
        assert_eq!(eval::<Vec<String>>(&h, "return SEEN").await, vec!["probe line"]);
        h.shutdown().await;
    })
    .await
}

/// One interrupt reaches every participant: with two things running (a review and a `!cmd`, say),
/// a single Esc stops both and is handled if any took it; a submit still stops at the first
/// handler that takes the line (coordinator's ruling on the ordered chains).
#[tokio::test]
async fn one_interrupt_reaches_every_running_participant() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "UI", h.service_table("ui").unwrap());
        exec(
            &h,
            r#"STOPPED, CLAIMED, ESC, LINE = {}, {}, {}, { text = "plain" }
            UI:handler("interrupt", "review", { order = 10 }, function() STOPPED[#STOPPED + 1] = "review"; return true end)
            UI:handler("interrupt", "shell", { order = 10 }, function() STOPPED[#STOPPED + 1] = "shell"; return true end)
            UI:handler("interrupt", "idle", { order = 20 }, function() STOPPED[#STOPPED + 1] = "idle"; return false end)
            UI:handler("submit", "first", { order = 10 }, function() CLAIMED[#CLAIMED + 1] = "first"; return true end)
            UI:handler("submit", "second", { order = 20 }, function() CLAIMED[#CLAIMED + 1] = "second"; return true end)"#,
        )
        .await;
        let esc_val: mlua::Value = h.lua().globals().get("ESC").unwrap();
        h.kernel.emit("key/interrupt", mlua::MultiValue::from_vec(vec![esc_val])).await;
        let handled: (Vec<String>, bool, Vec<String>, bool) = eval(
            &h,
            r#"C.cordis:get("bus"):emit("input/submit", LINE)
            return STOPPED, ESC.handled == true, CLAIMED, LINE.handled == true"#,
        )
        .await;
        assert_eq!(handled.0, vec!["review", "shell", "idle"], "every interrupt participant runs");
        assert!(handled.1, "the interrupt is handled when any participant took it");
        assert_eq!(handled.2, vec!["first"], "a submit stops at the first handler that takes it");
        assert!(handled.3);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn global_bindings_reach_editor_and_transcript() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set("global", "<F5>", function() mare.g.N = (mare.g.N or 0) + 1 end, { desc = "g f5" })
               mare.keymap.set("global", "q", function() mare.g.Q = true end, { desc = "g q" })"#,
        )
        .await;
        t.step().await;
        t.key(KeyCode::Function(5), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.N").await, 1);
        t.vt.type_str("q");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "q", "printable key in editor bypasses global");
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return mare.g.Q").await, None);
        t.ctrl('t');
        t.until("transcript open", |s| s.contains("Transcript (expanded)")).await;
        t.key(KeyCode::Function(5), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.N").await, 2);
    })
    .await
}

#[tokio::test]
async fn transcript_sequences_and_typed_search() {
    local(async {
        let body: String = (1..=40).map(|i| format!("row {i}\n\n")).collect();
        let mut t = Term::new(json!([{ "text": body }]), 80, 14).await;
        t.submit("fill").await;
        t.idle().await;
        exec(
            &t.vt.host,
            r#"mare.keymap.set("transcript", "g", false)
               mare.keymap.set("transcript", "g g", "view.top")
               mare.keymap.set("transcript.search", "<F6>", false)
               mare.keymap.set("global", "<F6>", function() _G.F6 = true end, { desc = "f6 test" })"#,
        )
        .await;
        t.ctrl('t');
        t.until("transcript at end", |s| s.contains("row 40") && !s.contains("Transcript (expanded)")).await;
        // First `g` enters the sequence prefix and describes the continuation.
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.until("which-key popup shows pending continuation", |s| s.contains("row 40") && s.contains("go to the top"))
            .await;
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.until("g g jumps to top", |s| s.contains("Transcript (expanded)") && !s.contains("go to the top")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        // Unbind (`false`) in `transcript.search` stops the whole chain before `global`.
        t.key(KeyCode::Function(6), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return _G.F6").await, None);
        t.vt.type_str("q");
        t.until("typing q goes into search query", |s| s.contains("/q") && s.contains("Transcript (expanded)")).await;
    })
    .await
}

/// `global` bindings cannot steal editor kernel keys (`Up`, `Backspace`, etc.) or show
/// printable `global` keys in `?` / `mare.ui.keymap()`, while non-printable `global` bindings
/// show in `/hotkeys` and run in the editor.
#[tokio::test]
async fn global_keys_do_not_steal_editor_kernel_or_list_printable_in_help() {
    local(async {
        let mut t = Term::new(json!([]), 100, 60).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set("global", "<Up>", function() _G.G_UP = true end, { desc = "g up" })
               mare.keymap.set("global", "q", function() _G.G_Q = true end, { desc = "g q" })
               mare.keymap.set("global", "<F5>", function() _G.G_F5 = true end, { desc = "g f5" })"#,
        )
        .await;
        t.vt.type_str("q");
        assert_eq!(t.vt.editor_text(), "q");
        t.key(KeyCode::Up, Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<(Option<bool>, Option<bool>)>(&t.vt.host, "return _G.G_Q, _G.G_UP").await, (None, None));
        t.key(KeyCode::Function(5), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return _G.G_F5").await, Some(true));
        let has_q: bool = eval(&t.vt.host, "return mare.ui.keymap()['q'] ~= nil").await;
        assert!(!has_q, "printable global key must be excluded from mare.ui.keymap()");

        t.ctrl('c');
        t.step().await;
        t.submit("/hotkeys").await;
        t.until("hotkeys open", |s| s.contains("Input editor") && s.contains("g f5")).await;
        assert!(!t.screen().contains("g q"), "printable global key must not show in /hotkeys:\n{}", t.screen());
    })
    .await
}

/// runtime `text` rebinds and masks apply identically in the main input and `ui.input`.
/// Reference: Kimi Code `packages/pi-tui/test/keybindings.test.ts:58`; Codex `codex-rs/tui/src/keymap.rs:4498`.
#[tokio::test]
async fn text_keys_follow_rebinds_in_the_input_and_the_dialog() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set("text", "<M-l>", "text.line_end")
               mare.keymap.set("text", "<C-l>", "text.line_end")"#,
        )
        .await;
        t.step().await;

        // Alt+L (unbound in `editor`) reaches `text` in the main input.
        t.vt.type_str("abc");
        t.key(KeyCode::Home, Modifiers::NONE);
        t.key(KeyCode::Char('l'), Modifiers::ALT);
        t.vt.type_str("d");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "abcd", "Alt+L rebound to text.line_end in main input");

        // In ui.input (`input -> text`, without `editor`), Ctrl+L is not shadowed by `editor`'s redraw.
        exec(
            &t.vt.host,
            r#"INPUT_RES = 'pending'; mare.task.spawn(function() local v, err = mare.ui.input({ title = 'TextPrompt' }); INPUT_RES = { v = v, err = err } end)"#,
        )
        .await;
        t.until("input dialog open", |s| s.contains("TextPrompt")).await;
        t.vt.type_str("abc");
        t.key(KeyCode::Home, Modifiers::NONE);
        t.ctrl('l');
        t.vt.type_str("d");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("input dialog closed", |s| !s.contains("TextPrompt")).await;
        assert_eq!(
            eval::<String>(&t.vt.host, "return type(INPUT_RES) == 'table' and INPUT_RES.v or ''").await,
            "abcd",
            "Ctrl+L rebound to text.line_end in ui.input"
        );

        exec(&t.vt.host, r#"mare.keymap.set("text", "<C-w>", false)"#).await;
        t.step().await;
        t.ctrl('w');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "abcd", "Ctrl+W masked to false deletes nothing in main input");

        exec(
            &t.vt.host,
            r#"INPUT_RES2 = 'pending'; mare.task.spawn(function() local v, err = mare.ui.input({ title = 'MaskPrompt' }); INPUT_RES2 = { v = v, err = err } end)"#,
        )
        .await;
        t.until("second input dialog open", |s| s.contains("MaskPrompt")).await;
        t.vt.type_str("abcd");
        t.ctrl('w');
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("second input dialog closed", |s| !s.contains("MaskPrompt")).await;
        assert_eq!(
            eval::<String>(&t.vt.host, "return type(INPUT_RES2) == 'table' and INPUT_RES2.v or ''").await,
            "abcd",
            "Ctrl+W masked to false deletes nothing in ui.input"
        );
    })
    .await
}

/// rows 2/11: `editor` context bindings beat `text` bindings (Ctrl+D, Ctrl+B, Alt+Enter).
#[tokio::test]
async fn editor_keys_beat_text_keys() {
    local(async {
        let mut t = Term::new(json!([{ "text": "z".repeat(4000), "delay_ms": 20 }]), 100, 30).await;
        expose(&t.vt.host);

        t.vt.type_str("ab");
        t.key(KeyCode::Home, Modifiers::NONE);
        t.ctrl('d');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "b", "Ctrl+D deletes forward on a draft");

        t.ctrl('b');
        t.vt.type_str("x");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "xb", "Ctrl+B moves left when nothing is running");
        t.ctrl('c');
        t.step().await;

        exec(
            &t.vt.host,
            r#"INPUT_NL = 'pending'; mare.task.spawn(function() local v, err = mare.ui.input({ title = 'AltEnterPrompt' }); INPUT_NL = { v = v, err = err } end)"#,
        )
        .await;
        t.until("input dialog open", |s| s.contains("AltEnterPrompt")).await;
        t.vt.type_str("a");
        t.key(KeyCode::Enter, Modifiers::ALT);
        t.vt.type_str("b");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("input dialog closed", |s| !s.contains("AltEnterPrompt")).await;
        assert_eq!(
            eval::<String>(&t.vt.host, "return type(INPUT_NL) == 'table' and INPUT_NL.v or ''").await,
            "a\nb",
            "Alt+Enter in ui.input inserts a newline"
        );

        t.submit("go slowly").await;
        t.until("streaming", |s| s.contains("zzzzzzzz")).await;
        t.vt.type_str("follow up item");
        t.ctrl('q');
        t.until("follow-up queued via Ctrl+Q", |s| s.contains("↳ follow up item")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("turn settled", |t| !t.vt.busy()).await;
        t.ctrl('c');
        t.step().await;

        assert!(t.vt.editor_text().is_empty());
        t.ctrl('d');
        t.step().await;
        assert!(t.vt.quit_requested(), "Ctrl+D on empty idle input triggers quit-if-empty");
    })
    .await
}

/// with keymap-default off, typing still works, Enter does not submit, and /keys reports
/// `editor.submit` under "No key".
#[tokio::test]
async fn without_keymap_default_text_still_types_and_doctor_reports_submit() {
    local(async {
        let config = json!({"plugins": {"keymap-default": {"enabled": false}}});
        let mut t = Term::with(json!([{ "text": "should not run" }]), config, 100, 40).await;
        expose(&t.vt.host);
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.type_str("hello");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "hello", "Enter must not submit when keymap-default is off");
        assert_eq!(t.vt.turns_ended(), 0, "no turn started");

        t.ctrl('c');
        t.step().await;
        exec(&t.vt.host, r#"C.cordis:get("commands"):run("/hotkeys")"#).await;
        t.until("hotkeys overlay", |s| s.contains("No key") && s.contains("editor.submit")).await;
        assert!(
            eval::<bool>(
                &t.vt.host,
                r#"for _, a in ipairs(mare.keymap.actions()) do
                    if a.name == "editor.submit" and a.essential == true and #mare.keymap.hint(a.name, "editor") == 0 then
                        return true
                    end
                end
                return false"#
            )
            .await,
            "editor.submit must be essential with no hint key"
        );
    })
    .await
}

#[allow(non_snake_case)]
#[tokio::test]
async fn keymap_default_text_bindings_equal_DEFAULT_TEXT_KEYS() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().user_config(false).no_session().build().await;
        let mut actual: Vec<[String; 2]> = eval(
            &h,
            r#"local out = {}
            for _, b in ipairs(mare.keymap.list()) do
                if b.context == "text" then
                    out[#out + 1] = { b.key, b.action }
                end
            end
            return out"#,
        )
        .await;
        actual.sort();
        let mut expected: Vec<[String; 2]> = mare_tui::editor::DEFAULT_TEXT_KEYS
            .iter()
            .map(|&(k, op)| [mare_tui::keys::normalize_key(k), op.name().to_string()])
            .collect();
        expected.sort();
        assert_eq!(actual, expected);
        h.shutdown().await;
    })
    .await
}

/// Kimi Code parity (`packages/pi-tui/test/keybindings.test.ts:6`): Ctrl+J inserts a newline.
#[tokio::test]
async fn ctrl_j_inserts_newline_in_main_input() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.vt.type_str("a");
        t.ctrl('j');
        t.vt.type_str("b");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "a\nb");
    })
    .await
}

#[tokio::test]
async fn backslash_enter_continues_line_in_main_input() {
    local(async {
        let mut t = Term::new(json!([{ "text": "ok" }]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.type_str("hello\\");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "hello\n", "trailing backslash + Enter inserts newline");
        assert_eq!(t.vt.turns_ended(), 0, "did not submit on backslash continuation");
        t.vt.type_str("world");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "hello\nworld");
        assert!(!t.vt.busy());
        assert_eq!(t.vt.turns_ended(), 0, "still not submitted before final Enter");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        let h = t.history();
        assert!(h.contains("hello") && h.contains("world") && h.contains("ok"), "{h}");
    })
    .await
}

/// Codex `codex-rs/tui/src/bottom_pane/textarea.rs:3560-3573`: Shift+Backspace/Delete still delete,
/// and Shift+İ still inserts cleanly in the main input.
#[tokio::test]
async fn shift_backspace_and_shift_delete_edit_in_main_input() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.type_str("abc");
        t.key(KeyCode::Backspace, Modifiers::SHIFT);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "ab", "Shift+Backspace deletes backward");
        t.key(KeyCode::Left, Modifiers::SHIFT);
        t.key(KeyCode::Delete, Modifiers::SHIFT);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "a", "Shift+Left + Shift+Delete deletes forward");
        t.key(KeyCode::Char('İ'), Modifiers::SHIFT);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "aİ", "Turkish capital İ inserts without being dropped");
    })
    .await
}

/// Ref: Kimi `packages/pi-tui/src/keybindings.ts:7-90`, Codex `textarea/editing.rs:25-62`,
/// for `text.select_*`, `text.redo`, and selection replacement in TUI.
#[tokio::test]
async fn text_selection_and_redo_work_end_to_end_in_main_input() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        t.vt.type_str("hello brave world");
        t.step().await;
        // Select "world" via Ctrl+Shift+Left (`text.select_word_left`) and replace by typing "mare".
        t.key(KeyCode::Left, Modifiers::CONTROL | Modifiers::SHIFT);
        t.vt.type_str("mare");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "hello brave mare");
        // Undo (`Ctrl+_` -> `text.undo`) restores "hello brave world".
        t.ctrl('_');
        t.step().await;
        assert_eq!(t.vt.editor_text(), "hello brave world");
        // Redo (`Ctrl+Shift+Z` -> `text.redo`) restores "hello brave mare".
        t.key(KeyCode::Char('z'), Modifiers::CONTROL | Modifiers::SHIFT);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "hello brave mare");
        // Cross-line selection via Shift+Enter + Shift+Up + Shift+End, then Backspace deletes "\nline2".
        t.key(KeyCode::Enter, Modifiers::SHIFT);
        t.vt.type_str("line2");
        t.key(KeyCode::Up, Modifiers::SHIFT);
        t.key(KeyCode::End, Modifiers::SHIFT);
        t.key(KeyCode::Backspace, Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "hello brave mare");
    })
    .await
}

/// Ref: `paste.rs:111`, Codex `textarea.rs:480-496`, and `render.rs` `editor.selection`.
#[tokio::test]
async fn modal_input_multiline_paste_over_selection_and_left_does_not_panic() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        exec(
            &t.vt.host,
            r#"mare.task.spawn(function() MODAL_RES = mare.ui.input({ title = "Edit", default = "hello world" }) end)"#,
        )
        .await;
        t.until("input modal opened", |s| s.contains("hello world")).await;

        // Reverse-select "hello world", paste multi-line text, press Left -> must replace & not panic.
        t.key(KeyCode::Home, Modifiers::SHIFT);
        t.vt.paste("a\nb");
        t.step().await;
        t.key(KeyCode::Left, Modifiers::NONE);
        t.step().await;
        // Undo restores "hello world"; Redo (<M-S-z>) restores "a\nb".
        t.ctrl('_');
        t.step().await;
        assert!(t.screen().contains("hello world"), "undo in input modal:\n{}", t.screen());
        t.key(KeyCode::Char('z'), Modifiers::ALT | Modifiers::SHIFT);
        t.step().await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("modal answered", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return MODAL_RES == 'a\\nb'"))
                .unwrap_or(false)
        })
        .await;
    })
    .await
}

/// plugin actions declare default keys (`opts.keys`) owned by the plugin,
/// `ui.key` is removed, unknown action names do not fall back to `key/<name>` events, and
/// disposing/unloading the plugin removes both its action and default bindings while restoring
/// lower bindings.
#[tokio::test]
async fn plugin_action_default_keys_no_ui_key_and_no_key_event_fallback() {
    local(async {
        let lua = mlua::Lua::new();
        let value: mlua::Value = lua.load(mare_plugin::service_contracts_lua().as_ref()).eval().unwrap();
        let contracts = mare_kernel::contracts::parse_contracts(&lua, value).unwrap();
        assert!(!contracts["ui"].methods.contains_key("key"), "ui.key contract must be removed");

        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin_path = plugins_dir.join("u7_probe.lua");
        std::fs::write(
            &plugin_path,
            r#"return {
                name = "u7-probe",
                inject = { "ui" },
                apply = function(ctx)
                    assert(ctx.ui.key == nil, "ui.key must be removed")
                    ctx.ui:action("u7.ping", function()
                        mare.g.u7_hits = (mare.g.u7_hits or 0) + 1
                    end, {
                        desc = "u7 sample action",
                        contexts = { "editor" },
                        keys = { editor = { "<M-9>" } },
                    })
                end,
            }"#,
        )
        .unwrap();
        let mut b = env
            .host()
            .builtins(&["review", "jobs"])
            .user_plugins()
            .faux(json!([]))
            .no_session()
            .config(json!({ "logo": false, "plugins": { "onboarding": { "plugins": false } } }));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 80, 24).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(24), nz(80), 2000));
        t.step().await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        // 1) UI contract no longer exposes `key`; review & tools own named actions.
        let (has_review_action, has_bg_action): (bool, bool) = eval(
            &t.vt.host,
            r#"local has_review, has_bg = false, false
            for _, a in ipairs(mare.keymap.actions()) do
                if a.name == "review.viewer" then has_review = true end
                if a.name == "background" then has_bg = true end
            end
            return has_review, has_bg"#,
        )
        .await;
        assert!(has_review_action, "review must register review.viewer action");
        assert!(has_bg_action, "tools must register background action");

        // 2) Plugin action's default key works and shows up in hints.
        exec(&t.vt.host, "mare.g.u7_hits = 0").await;
        let hints: Vec<String> = eval(&t.vt.host, r#"return mare.keymap.hint("u7.ping", "editor")"#).await;
        assert_eq!(hints, vec!["alt+9".to_string()]);
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_hits").await, 1);

        // 3) User rebind + unbind (`false`) + `del` restores plugin default.
        exec(
            &t.vt.host,
            r#"USER_H = mare.keymap.set("editor", "<M-9>", function() mare.g.user_hits = (mare.g.user_hits or 0) + 1 end, { desc = "user override", priority = 10 })"#,
        )
        .await;
        t.step().await;
        exec(&t.vt.host, "mare.g.user_hits = 0").await;
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.user_hits").await, 1);
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_hits").await, 1);

        exec(&t.vt.host, "USER_H:remove()").await;
        t.step().await;
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_hits").await, 2, "default returns after override");
        exec(&t.vt.host, r#"MASK_H = mare.keymap.set("editor", "<M-9>", false)"#).await;
        t.step().await;
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_hits").await, 2, "false masks the default");
        exec(&t.vt.host, "MASK_H:remove()").await;
        t.step().await;
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_hits").await, 3, "default returns after unmasking");
        exec(
            &t.vt.host,
            r#"USER_H = mare.keymap.set("editor", "<M-9>", function() mare.g.user_hits = mare.g.user_hits + 1 end, { desc = "user override", priority = 10 })"#,
        )
        .await;

        // Unload plugin -> removes its action & default key, user override still intact; remove user override -> nothing handles Alt+9.
        std::fs::remove_file(&plugin_path).unwrap();
        t.vt.host.reload_all().await.unwrap();
        t.step().await;
        let post_hints: Vec<String> = eval(&t.vt.host, r#"return mare.keymap.hint("u7.ping", "editor")"#).await;
        assert!(post_hints.is_empty(), "plugin action hint must be gone after dispose: {post_hints:?}");
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.user_hits").await, 2, "user override survives unload");
        exec(&t.vt.host, "USER_H:remove()").await;
        t.step().await;
        assert!(
            eval::<bool>(&t.vt.host, r#"return mare.ui.keymap()["alt+9"] == nil"#).await,
            "plugin default key must be removed on dispose"
        );

        // 4) Unregistered action name is rejected and must NOT emit `key/<name>` fallback event.
        exec(
            &t.vt.host,
            r#"mare.g.phantom = 0
            mare.g.interrupt_seen = false
            mare.on("key/phantom_u7", function() mare.g.phantom = mare.g.phantom + 1 end)
            mare.on("key/interrupt", function() mare.g.interrupt_seen = true end)
            local ok, err = pcall(mare.keymap.set, "editor", "<M-8>", "phantom_u7")
            assert(not ok and tostring(err):find("unknown action"), "unknown action must be rejected: " .. tostring(err))"#,
        )
        .await;
        t.step().await;
        t.key(KeyCode::Char('8'), Modifiers::ALT);
        // The following interrupt is queued after the key and gives the event loop a visible barrier.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until_vt("interrupt event after unknown action", |t| {
            futures_util::FutureExt::now_or_never(eval::<bool>(&t.vt.host, "return mare.g.interrupt_seen"))
                .unwrap_or(false)
        })
        .await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.phantom").await, 0, "must not emit key/<name>");
    })
    .await
}

/// Existing user and runtime bindings survive plugin defaults and are restored after unload.
#[tokio::test]
async fn existing_bindings_survive_plugin_load_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.keymap.action("u7.user", function() mare.g.u7_user = (mare.g.u7_user or 0) + 1 end)
               mare.keymap.set("editor", "<M-9>", "u7.user")"#,
        )
        .unwrap();
        let mut b = env
            .host()
            .user_config(false)
            .faux(json!([]))
            .no_session()
            .config(json!({ "logo": false, "plugins": { "onboarding": { "plugins": false } } }));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 80, 24).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(24), nz(80), 2000));
        t.step().await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(
            &t.vt.host,
            r#"mare.g.u7_user, mare.g.u7_earlier, mare.g.u7_plugin = 0, 0, 0
               mare.keymap.action("u7.earlier", function() mare.g.u7_earlier = (mare.g.u7_earlier or 0) + 1 end)
               mare.keymap.set("editor", "<M-8>", "u7.earlier")"#,
        )
        .await;
        t.step().await;
        assert!(eval::<bool>(&t.vt.host, r#"for _, e in ipairs(mare.keymap.list()) do if e.key == "alt+9" and e.action == "u7.user" and e.layer == "user" then return true end end return false"#).await, "init.lua binding must have user rank");
        assert_eq!(eval::<Vec<String>>(&t.vt.host, r#"return mare.keymap.hint("u7.user", "editor")"#).await, vec!["alt+9"]);
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        t.key(KeyCode::Char('8'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_user").await, 1);
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_earlier").await, 1);

        let plugin_path = plugins.join("u7_layers.lua");
        std::fs::write(&plugin_path, r#"return {name = "u7-layers", inject = {"ui"}, apply = function(ctx)
            ctx.ui:action("u7.plugin", function() mare.g.u7_plugin = (mare.g.u7_plugin or 0) + 1 end,
                {keys = {editor = {{key = "<M-8>", priority = 1}, {key = "<M-9>", priority = 1}}}})
        end}"#).unwrap();
        t.vt.host.reload_all().await.unwrap();
        t.step().await;
        assert_eq!(eval::<Vec<String>>(&t.vt.host, r#"return mare.keymap.hint("u7.user", "editor")"#).await, vec!["alt+9"]);
        let plugin_hints: Vec<String> = eval(&t.vt.host, r#"return mare.keymap.hint("u7.plugin", "editor")"#).await;
        assert!(plugin_hints.iter().any(|key| key == "alt+8"), "plugin default hint: {plugin_hints:?}");
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        t.key(KeyCode::Char('8'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_user").await, 2, "user rank survives plugin load");
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_plugin").await, 1, "newer runtime default wins over the older runtime binding");
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_earlier").await, 1);

        std::fs::remove_file(&plugin_path).unwrap();
        t.vt.host.reload_all().await.unwrap();
        t.step().await;
        assert!(eval::<Vec<String>>(&t.vt.host, r#"return mare.keymap.hint("u7.plugin", "editor")"#).await.is_empty());
        assert_eq!(eval::<Vec<String>>(&t.vt.host, r#"return mare.keymap.hint("u7.user", "editor")"#).await, vec!["alt+9"]);
        assert_eq!(eval::<Vec<String>>(&t.vt.host, r#"return mare.keymap.hint("u7.earlier", "editor")"#).await, vec!["alt+8"]);
        t.key(KeyCode::Char('9'), Modifiers::ALT);
        t.step().await;
        t.key(KeyCode::Char('8'), Modifiers::ALT);
        t.step().await;
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_user").await, 3);
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_earlier").await, 2, "unload restores the pre-existing lower binding");
        assert_eq!(eval::<i64>(&t.vt.host, "return mare.g.u7_plugin").await, 1);
    }).await
}

#[tokio::test]
async fn ctrl_b_declines_on_empty_input_and_reaches_the_editor() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        let km_core = mare_shared::lua::core::keymap::handle(t.vt.host.lua()).unwrap();
        t.vt.host
            .lua()
            .load(
                r#"local km = ...
                local invoke = km.invoke
                km.invoke = function(name, ev)
                  local result = invoke(name, ev)
                  if name == "background" then mare.g.bg_seen, mare.g.bg_result = true, result end
                  return result
                end"#,
            )
            .call::<()>(km_core)
            .unwrap();
        assert_eq!(t.vt.editor_text(), "");
        t.ctrl('b');
        t.step().await;
        assert!(eval::<bool>(&t.vt.host, "return mare.g.bg_seen == true and mare.g.bg_result == false").await);
        assert_eq!(t.vt.editor_text(), "");
        t.vt.type_str("ab");
        t.ctrl('b');
        t.vt.type_str("X");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "aXb", "declined background action must leave Ctrl+B to the editor");
    })
    .await
}

#[tokio::test]
async fn action_rebinds_each_updated_key_once() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        let km_core = mare_shared::lua::core::keymap::handle(t.vt.host.lua()).unwrap();
        t.vt.host
            .lua()
            .load(
                r#"local km = ...
                local h0 = mare.keymap.action("u7.count", function() end)
                local a = mare.keymap.set("editor", "<M-9>", "u7.count")
                local b = mare.keymap.set("editor", "<M-8>", "u7.count")
                h0:remove()
                local old, calls = km.set_bindings, {}
                km.set_bindings = function(ctx, key, list)
                  if ctx == "editor" then calls[key] = (calls[key] or 0) + 1 end
                  return old(ctx, key, list)
                end
                local ok, err = pcall(function()
                  local h = mare.keymap.action("u7.count", function() end, {keys = {editor = "<M-9>"}})
                  assert(calls["alt+9"] == 1 and calls["alt+8"] == 1, "registration: " .. mare.json.encode(calls))
                  calls = {}
                  h:remove()
                  assert(calls["alt+9"] == 1 and calls["alt+8"] == 1, "removal: " .. mare.json.encode(calls))
                  calls = {}
                  local registered = pcall(mare.keymap.action, "u7.count", function() end,
                    {keys = {editor = {"<M-9>", "<C-c>"}}})
                  assert(not registered, "reserved key must roll back the action")
                  assert(calls["alt+9"] == 2 and calls["alt+8"] == 1, "rollback: " .. mare.json.encode(calls))
                end)
                km.set_bindings = old
                a:remove(); b:remove()
                assert(ok, err)"#,
            )
            .call::<()>(km_core)
            .unwrap();
    })
    .await
}

#[tokio::test]
async fn keymap_four_tier_precedence_priority_and_same_owner_replacement() {
    local(async {
        {
            let env2 = TestEnv::new().await;
            std::fs::create_dir_all(env2.config_dir().join("plugins/p-low")).unwrap();
            std::fs::write(
                env2.config_dir().join("plugins/p-low/init.lua"),
                r#"return {
                    name = "p-low",
                    inject = { "ui" },
                    apply = function(ctx)
                        ctx.ui:action("tier.act", function() return true end, { desc = "tier act" })
                        mare.keymap.set("editor", "<F7>", "tier.act", { priority = 50 })
                    end,
                }"#,
            )
            .unwrap();
            std::fs::create_dir_all(env2.config_dir().join("plugins/p-high")).unwrap();
            std::fs::write(
                env2.config_dir().join("plugins/p-high/init.lua"),
                r#"return {
                    name = "p-high",
                    inject = { "ui" },
                    apply = function(ctx)
                        mare.keymap.set("editor", "<F6>", "tier.act", { priority = 100 })
                    end,
                }"#,
            )
            .unwrap();
            std::fs::write(
                env2.config_dir().join("init.lua"),
                r#"
                mare.keymap.set("editor", "<F8>", "tier.act", { priority = -5 })
                mare.keymap.set("editor", "<F8>", "tier.act", { priority = 1 })
                mare.keymap.set("editor", "<F8>", "tier.act", { when = "editor_empty" })
                mare.keymap.set("editor", "<F8>", "tier.act", { when = "!editor_empty" })
                "#,
            )
            .unwrap();
            std::fs::create_dir_all(env2.cwd.join(".mare")).unwrap();
            std::fs::write(
                env2.cwd.join(".mare/init.lua"),
                r#"mare.keymap.set("editor", "<F9>", "tier.act", { priority = -999 })"#,
            )
            .unwrap();
            let h = env2.host().user_config(true).no_session().build().await;
            let (hints, f8_count, f8_layers): (Vec<String>, i64, Vec<String>) = eval(
                &h,
                r#"
                -- Project > User > Plugin(prio 100) > Plugin(prio 50), plus default-layer binding below all.
                mare.keymap.set("editor", "<F5>", "tier.act", { priority = 500, owner = "plugin:keymap-default", layer = "default" })
                local hints = mare.keymap.hint("tier.act", "editor")
                local f8 = {}
                for _, e in ipairs(mare.keymap.list({ context = "editor" })) do
                    if e.key == "f8" and e.owner == "user" then f8[#f8 + 1] = e.layer or "" end
                end
                return hints, #f8, f8
                "#,
            )
            .await;
            // Project(-999) > User(1) > Plugin(100) > Plugin(50) > Default(500)
            assert_eq!(hints, vec!["f9", "f8", "f6", "f7", "f5"], "hint order must respect 4-tier rank then priority: {hints:?}");
            // Same-owner overlapping-when replacement: the earlier unguarded F8 entries in `user` were replaced
            // cleanly while disjoint `when` ("editor_empty" & "!editor_empty") coexist (total 2 user entries).
            assert_eq!(f8_count, 2, "same-owner rebinds with overlapping when must replace cleanly, disjoint coexist: {f8_layers:?}");

            let list_len: i64 = eval(
                &h,
                r#"local seen = {}
                   for _, b in ipairs(mare.keymap.list()) do
                       if b.action == "tier.act" and b.context == "editor" then seen[b.key] = b.layer end
                   end
                   assert(seen["f9"] == "project" and seen["f8"] == "user" and seen["f6"] == "plugin" and seen["f5"] == "default")
                   return 4"#,
            )
            .await;
            assert_eq!(list_len, 4);
            h.shutdown().await;
        }

        let mut t = Term::build(json!([]), json!({}), json!({}), 100, 24, |env| {
            std::fs::create_dir_all(env.config_dir().join("plugins/tier-probe")).unwrap();
            std::fs::write(
                env.config_dir().join("plugins/tier-probe/init.lua"),
                r#"return {
                    name = "tier-probe",
                    inject = { "ui" },
                    apply = function(ctx)
                        ctx.ui:action("tier.probe", function(ev)
                            mare.g.TIER_HIT = (mare.g.TIER_HIT or "") .. ":" .. ev.key
                        end, {
                            desc = "tier probe",
                            keys = { editor = { key = "<M-7>", priority = 5 } },
                        })
                        mare.keymap.set("editor", "<F8>", "tier.probe", { priority = 999 })
                    end,
                }"#,
            )
            .unwrap();
            std::fs::write(
                env.config_dir().join("init.lua"),
                r#"
                mare.keymap.set("editor", "<M-7>", function() mare.g.TIER_HIT = "user-alt7" end, { desc = "user alt7", priority = -5 })
                mare.keymap.set("editor", "<F6>", "tier.probe", { priority = 10 })
                mare.keymap.set("editor", "<F8>", "tier.probe", { priority = -5 })
                "#,
            )
            .unwrap();
        })
        .await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        // Verify `Term` dispatch also uses user priority/tier above plugin.
        t.key(KeyCode::Char('7'), Modifiers::ALT);
        t.step().await;
        assert!(eval::<bool>(&t.vt.host, r#"return mare.g.TIER_HIT == "user-alt7""#).await);
    })
    .await
}

#[tokio::test]
async fn keymap_ambiguity_sequence_conflicts_and_hot_reload_rollback() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir().join("plugins/p-a")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/p-a/init.lua"),
            r#"return {
                name = "p-a",
                inject = { "ui" },
                apply = function(ctx)
                    ctx.ui:action("pa.act", function() return true end, { desc = "a" })
                    mare.keymap.set("editor", "<F8>", "pa.act")
                    mare.keymap.set("editor", "<F9>", "pa.act", { when = "working" })
                    mare.keymap.set("editor", "<C-x>", "pa.act")
                end,
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(env.config_dir().join("plugins/p-b")).unwrap();
        std::fs::write(
            env.config_dir().join("plugins/p-b/init.lua"),
            r#"return {
                name = "p-b",
                inject = { "ui" },
                apply = function(ctx)
                    ctx.ui:action("pb.act", function() return true end, { desc = "b" })
                    mare.keymap.set("editor", "<F8>", "pb.act")
                    mare.keymap.set("editor", "<F9>", "pb.act", { when = "!working" })
                    mare.keymap.set("editor", "<C-x> a", "pb.act")
                end,
            }"#,
        )
        .unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.keymap.set("editor", "<F6>", function() return "preserved" end, { desc = "good key" })
            mare.keymap.set("editor", "<F4>", "missing.action")
            "#,
        )
        .unwrap();

        let mut hb = env.host().user_config(true).no_session();
        hb.opts.load_init = true;
        let h = mare_host::Host::new(hb.opts).await.unwrap();
        let problems = h.kernel.problems();
        // 1. Ambiguity diagnostic names key, context, and both owners; neither F8 binding survives.
        assert!(
            problems.iter().any(|p| p.contains("f8")
                && p.contains("editor")
                && p.contains("plugin:p-a")
                && p.contains("plugin:p-b")),
            "missing ambiguity diagnostic naming key/context/both owners: {problems:?}"
        );
        // 2. Sequence-prefix collision diagnostic recorded; conflicting bindings removed.
        assert!(
            problems.iter().any(|p| p.contains("ctrl+x") && p.contains("ctrl+x a")),
            "missing sequence collision diagnostic: {problems:?}"
        );
        // 3. Unknown action in init.lua caught at _settle().
        assert!(
            problems.iter().any(|p| p.contains("missing.action")),
            "missing unknown-action settle diagnostic: {problems:?}"
        );
        let (f8_ok, f9_count, seq_ok, f6_val): (bool, i64, bool, String) = eval(
            &h,
            r#"
            local function count_key(k, ctx)
                local n = 0
                for _, e in ipairs(mare.keymap.list()) do
                    if e.key == k and (not ctx or e.context == ctx) then n = n + 1 end
                end
                return n
            end
            local f8 = count_key("f8") == 0
                and not table.find(mare.keymap.hint("pa.act", "editor"), "f8")
                and not table.find(mare.keymap.hint("pb.act", "editor"), "f8")
            local f9_n = count_key("f9", "editor")
            local seq = count_key("ctrl+x") == 0 and count_key("ctrl+x a") == 0
            local c = mare.keymap.resolve("editor", "<F6>").candidates
            return f8, f9_n, seq, mare.keymap.callback(c[1].action)()
            "#,
        )
        .await;
        assert!(f8_ok, "conflicting F8 bindings must neither take effect");
        assert_eq!(f9_count, 2, "mutually-exclusive `working` / `!working` F9 bindings must both coexist");
        assert!(seq_ok, "conflicting sequence prefix/chord must neither take effect");
        assert_eq!(f6_val, "preserved");

        // 4. Anonymous function without non-empty desc rejected; unguarded default printable rejected while guarded `?` works; runtime unknown action/wrong context rejected.
        let checks: (bool, bool, bool, bool, bool, bool) = eval(
            &h,
            r#"
            mare.keymap.action("tr.only", function() end, { contexts = { "transcript" } })
            local no_desc = pcall(mare.keymap.set, "editor", "<F2>", function() end)
            local blank_desc = pcall(mare.keymap.set, "editor", "<F2>", function() end, { desc = "   " })
            local wrong_ctx = pcall(mare.keymap.set, "editor", "<F2>", "tr.only")
            local unknown_rt = pcall(mare.keymap.set, "editor", "<F2>", "never.registered")
            local def_print = pcall(mare.keymap.set, "editor", "x", "pa.act", { owner = "plugin:def", layer = "default" })
            local def_guarded = pcall(mare.keymap.set, "editor", "!", "pa.act", { when = { "editor_empty", "!completion_open" }, owner = "plugin:def", layer = "default" })
            return not no_desc, not blank_desc, not wrong_ctx, not unknown_rt, not def_print, def_guarded
            "#,
        )
        .await;
        assert_eq!(checks, (true, true, true, true, true, true), "validation checks failed: {checks:?}");

        // 5. Hot-reload introducing sequence conflict in init.lua rolls back and preserves previous F6 callback.
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.keymap.set("editor", "<F6>", function() return "new" end, { desc = "new key" })
            mare.keymap.set("editor", "<C-x>", "pa.act")
            mare.keymap.set("editor", "<C-x> b", "pa.act")
            "#,
        )
        .unwrap();
        let err = h.reload_config().await.unwrap_err().to_string();
        assert!(err.contains("ctrl+x"), "expected sequence conflict on reload: {err}");
        let f6_after: String =
            eval(&h, "local c = mare.keymap.resolve('editor', '<F6>').candidates; return mare.keymap.callback(c[1].action)()").await;
        assert_eq!(f6_after, "preserved", "failed hot-reload must roll back and preserve previous bindings");

        // 6. <C-r> collision between keymap-default (history-search) and review (review.viewer) resolves cleanly:
        //    `review` wins in `editor` with priority 10, `commands` & `review` expose no `*_key` options.
        let (cr_winner, legacy_opts): (String, bool) = eval(
            &h,
            r#"
            local cr = ""
            for _, b in ipairs(mare.keymap.list()) do
                if b.key == "ctrl+r" and b.context == "editor" then cr = b.action; break end
            end
            local cs = mare._plugin_schemas and mare._plugin_schemas.commands or {}
            local rs = mare._plugin_schemas and mare._plugin_schemas.review or {}
            local any_legacy = cs.model_picker_key ~= nil or cs.model_cycle_key ~= nil
                or cs.effort_down_key ~= nil or cs.effort_up_key ~= nil or rs.key ~= nil
            return cr, any_legacy
            "#,
        )
        .await;
        assert_eq!(cr_winner, "review.viewer", "review.viewer priority 10 must cleanly resolve default <C-r>");
        assert!(!legacy_opts, "commands/review must not expose retired *_key options");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn reload_rollback_preserves_plugin_and_setup_keys_and_enforces_priority_when_prefix_disposal() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins");
        let init_path = env.config_dir().join("init.lua");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("pa.lua"),
            r#"
            return {
                name = "pa",
                apply = function(ctx)
                    PA_F8 = 0
                    mare.keymap.action("pa.f8", function() PA_F8 = PA_F8 + 1; mare.ui.notify("pa_f8_ran") end, { contexts = { "editor" }, desc = "pa f8" })
                    mare.keymap.action("pa.work", function() mare.ui.notify("f6_work") end, { contexts = { "editor" }, desc = "work f6" })
                    mare.keymap.action("pa.chord", function() mare.ui.notify("f6_chord_g") end, { contexts = { "editor" }, desc = "chord" })
                    mare.keymap.action("pa.c1", function() end, { contexts = { "editor" }, desc = "c1" })
                    mare.keymap.action("pa.c2", function() end, { contexts = { "editor" }, desc = "c2" })
                    mare.keymap.action("pa.f9w", function() end, { contexts = { "editor" }, desc = "f9w" })
                    mare.keymap.action("pa.f9i", function() end, { contexts = { "editor" }, desc = "f9i" })
                    mare.keymap.set("editor", "<F8>", "pa.f8")
                    mare.keymap.set("editor", "<F9>", "pa.f9w", { when = "working" })
                    mare.keymap.set("editor", "<F9>", "pa.f9i", { when = "!working" })
                    mare.keymap.set("editor", "<F6>", "pa.work", { when = "working" })
                    mare.keymap.set("editor", "<F6> g", "pa.chord", { when = "!working" })
                end,
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            plugins_dir.join("pb.lua"),
            r#"
            return {
                name = "pb",
                apply = function()
                    mare.keymap.action("pb.other", function() mare.ui.notify("pb_f10_old") end, { contexts = { "editor" }, desc = "pb old" })
                    mare.keymap.set("editor", "<F10>", "pb.other")
                end,
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            &init_path,
            r#"
            mare.keymap.action("cfg.act", function() mare.ui.notify("cfg_old_fn") end, { contexts = { "editor" }, desc = "cfg" })
            mare.keymap.set("editor", "<F5>", "cfg.act")
            mare.setup { keys = { ["<F7>"] = "redraw" } }
            "#,
        )
        .unwrap();

        let mut b = env
            .host()
            .builtins(&[
                "agents", "subagent", "jobs", "shell-mode", "compact", "lua-console", "mcp", "copy", "usage",
                "tools-apply-patch", "user-config", "onboarding", "plugins", "images", "file-ref",
            ])
            .faux(json!([]))
            .config(json!({ "logo": false, "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } } }));
        b.opts.mode = Mode::Tui;
        b.opts.load_init = true;
        b.opts.no_user_plugins = false;
        let owned = OwnedVt::new(env, b.opts, 120, 24).await;
        let mut t = Term::from_vt(owned, vt100::Parser::new(nz(24), nz(120), 2000));
        t.step().await;
        t.until_vt("startup", |t| !t.vt.starting()).await;

        // 1. Disjoint F6 (`working`) vs `F6 g` (`!working`): while idle, pressing `F6` waits for `g` and runs `pa.chord`.
        t.key(KeyCode::Function(6), Modifiers::NONE);
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.until("f6 chord dispatched when !working", |s| s.contains("f6_chord_g")).await;

        // 2a. Failed named `/reload pb` (colliding on A's `<F8>`, dropping `F10`, adding unrelated `F11`)
        //     must fail cleanly, keep A's `F8`, restore B's old `F10` callback, and drop B's `F11`.
        std::fs::write(
            plugins_dir.join("pb.lua"),
            r#"
            return {
                name = "pb",
                apply = function()
                    mare.keymap.action("pb.collide", function() end, { contexts = { "editor" }, desc = "collide" })
                    mare.keymap.set("editor", "<F8>", "pb.collide")
                    mare.keymap.action("pb.new11", function() end, { contexts = { "editor" }, desc = "new11" })
                    mare.keymap.set("editor", "<F11>", "pb.new11")
                end,
            }
            "#,
        )
        .unwrap();
        let named_err = t.vt.host.kernel.reload(Some("pb")).await.unwrap_err().to_string();
        assert!(named_err.contains("f8"), "named reload pb must fail on F8 ambiguity: {named_err}");
        t.key(KeyCode::Function(8), Modifiers::NONE);
        t.until("plugin A F8 preserved after failed named reload", |s| s.contains("pa_f8_ran")).await;
        t.key(KeyCode::Function(10), Modifiers::NONE);
        t.until("plugin B old F10 callback preserved after failed named reload", |s| s.contains("pb_f10_old")).await;
        let f11_after_named: bool = eval(&t.vt.host, "for _, b in ipairs(mare.keymap.list()) do if b.key == 'f11' then return false end end return true").await;
        assert!(f11_after_named, "rejected pb reload must not leave unrelated F11 behind");

        // 2b. Failed full `reload_all()` with colliding pb.lua keeps A's F8, restores B's old F10 callback, drops B's F11.
        let rep = t.vt.host.reload_all().await.unwrap();
        let notices = rep["notices"].as_array().unwrap();
        assert!(
            notices.iter().any(|n| n.as_str().is_some_and(|s| s.contains("f8"))),
            "reload_all must report F8 ambiguity notice: {notices:?}"
        );
        t.key(KeyCode::Function(8), Modifiers::NONE);
        t.until("plugin A F8 preserved after failed reload_all", |s| s.contains("pa_f8_ran")).await;
        t.key(KeyCode::Function(10), Modifiers::NONE);
        t.until("plugin B old F10 callback preserved after failed reload_all", |s| s.contains("pb_f10_old")).await;
        let f11_after_full: bool = eval(&t.vt.host, "for _, b in ipairs(mare.keymap.list()) do if b.key == 'f11' then return false end end return true").await;
        assert!(f11_after_full, "rejected full reload must not leave pb F11 behind");

        // Restore clean pb.lua (`F10`), then verify:
        // - P0.3 shorthand conflict against another owner's `<F3> g` keeps BOTH `F7` AND `<F3> g` untouched;
        // - P0.3 removing `F7` across UI service reload cleans `F7`;
        // - P0.4 failed `init.lua` replacing `cfg.act` + sequence conflict preserves `cfg_old_fn` on `<F5>`.
        std::fs::write(plugins_dir.join("pb.lua"), r#"return { name = "pb", apply = function() mare.keymap.set("editor", "<F3> g", "pb.other") end }"#).unwrap();
        t.vt.host.reload_all().await.unwrap();
        std::fs::write(
            &init_path,
            r#"
            mare.keymap.action("cfg.act", function() mare.ui.notify("cfg_new_fn") end, { contexts = { "editor" }, desc = "cfg" })
            mare.keymap.set("editor", "<F5>", "cfg.act")
            mare.keymap.set("editor", "<C-x>", "redraw")
            mare.keymap.set("editor", "<C-x> a", "redraw")
            "#,
        )
        .unwrap();
        assert!(t.vt.host.reload_config().await.is_err());
        t.key(KeyCode::Function(5), Modifiers::NONE);
        t.until("cfg.act old callback preserved after failed staged reload", |s| s.contains("cfg_old_fn")).await;

        std::fs::write(&init_path, r#"mare.setup { keys = { ["<F3>"] = "redraw" } }"#).unwrap();
        t.vt.host.reload_config().await.unwrap();
        t.until("f3 sequence conflict notice shown", |s| s.contains("f3")).await;
        let (f7_kept, f3g_kept): (bool, bool) = eval(
            &t.vt.host,
            r#"local f7, f3g = false, false
            for _, b in ipairs(mare.keymap.list()) do
                if b.key == "f7" then f7 = true end
                if b.key == "f3 g" then f3g = true end
            end
            return f7, f3g"#,
        )
        .await;
        assert!(f7_kept && f3g_kept, "failed setup.keys shorthand must preserve both F7 and pb's F3 g");

        // Removing F7 from init.lua and reloading `ui` service must drop F7.
        // Also bind `<F12>` as a user-layer key in init.lua for P1.7 precedence check below.
        std::fs::write(&init_path, r#"mare.setup { keys = {} }; mare.keymap.set("editor", "<F12>", "pa.c2", { priority = 0 })"#).unwrap();
        t.vt.host.reload_config().await.unwrap();
        t.vt.host.kernel.reload(Some("ui")).await.unwrap();
        let f7_removed: bool = eval(&t.vt.host, "for _, b in ipairs(mare.keymap.list()) do if b.key == 'f7' then return false end end return true").await;
        assert!(f7_removed, "removing F7 from setup.keys + ui reload must clear F7");

        // 3. Post-settle sequence collision BOTH orders, equal/lower-priority mask vs higher-priority action
        //    in same layer, child context sequence conflict + mask, user > plugin in list(), weak GC check.
        let km_core = mare_shared::lua::core::keymap::handle(t.vt.host.lua()).unwrap();
        let checks: (bool, bool, bool, bool, bool, bool, bool, bool, bool, bool) = t
            .vt
            .host
            .lua()
            .load(
                r#"
            local km = ...
            local function has_key(k)
                for _, b in ipairs(mare.keymap.list()) do
                    if b.key == k then return true end
                end
                return false
            end
            -- sequence collision order 1 & 2 (rejected without destroying existing binding)
            mare.keymap.set("editor", "<C-x>", "pa.c1")
            local ok1 = pcall(mare.keymap.set, "editor", "<C-x> a", "pa.c2")
            local kept1 = has_key("ctrl+x")
            mare.keymap.del("editor", "<C-x>")
            mare.keymap.set("editor", "<C-x> a", "pa.c2")
            local ok2 = pcall(mare.keymap.set, "editor", "<C-x>", "pa.c1")
            local kept2 = has_key("ctrl+x a")
            mare.keymap.del("editor", "<C-x> a")

            -- P1.4: priority-0 mask beats priority-10 action from another owner in SAME layer (both orders)
            local function check_mask_prio(mask_first)
                local h1, h2
                if mask_first then
                    h1 = mare.keymap.set("editor", "<F11>", false, { priority = 0, owner = "plugin:m1", layer = "plugin" })
                    h2 = mare.keymap.set("editor", "<F11>", "pa.c1", { priority = 10, owner = "plugin:b1", layer = "plugin" })
                else
                    h2 = mare.keymap.set("editor", "<F11>", "pa.c1", { priority = 10, owner = "plugin:b1", layer = "plugin" })
                    h1 = mare.keymap.set("editor", "<F11>", false, { priority = 0, owner = "plugin:m1", layer = "plugin" })
                end
                local listed = false
                for _, b in ipairs(mare.keymap.list()) do
                    if b.context == "editor" and b.key == "f11" then listed = true end
                end
                h1:remove(); h2:remove()
                return not listed
            end
            local m_fwd = check_mask_prio(true)
            local m_rev = check_mask_prio(false)

            -- P1.2: active child subcontext `editor.sub/F4` collides with `editor/F4 g`
            local sub = mare.keymap.context("editor.sub", { parent = "editor", active = true })
            local h_ed = mare.keymap.set("editor", "<F4> g", "pa.c1")
            local ok_sub_col = pcall(mare.keymap.set, "editor.sub", "<F4>", "pa.c2")
            h_ed:remove(); sub:remove()

            -- P1.7: user priority-0 (bound in init.lua above) sorts before plugin priority-100 in list(),
            -- and Ctrl+D text.delete_forward stays reachable
            local hp = mare.keymap.set("editor", "<F12>", "pa.c1", { priority = 100, owner = "plugin:hi", layer = "plugin" })
            local f12_acts = {}
            for _, b in ipairs(mare.keymap.list()) do
                if b.key == "f12" and b.context == "editor" then f12_acts[#f12_acts + 1] = b.action end
            end
            hp:remove()

            -- P1.5: weak-ref GC probe confirming manual handle:remove() detaches owner_disposers
            local weak = setmetatable({}, { __mode = "v" })
            local hs = {}
            for i = 1, 12 do
                local fn = function() return i end
                weak[i] = fn
                hs[i] = mare.keymap.set("editor", "<M-F" .. i .. ">", fn, { desc = "gc test", owner = "plugin:pb", layer = "plugin" })
            end
            for i = 1, 12 do hs[i]:remove() end
            hs = nil
            collectgarbage(); collectgarbage()
            local gc_freed = true
            for i = 1, 12 do if weak[i] ~= nil then gc_freed = false end end

            local tr_esc = {}
            for _, b in ipairs(mare.keymap.list()) do
                if b.context == "transcript" and b.key == "esc" then tr_esc[b.action] = true end
            end
            local settled = km.is_settled() == true
            return not ok1 and kept1, not ok2 and kept2, m_fwd, m_rev, not ok_sub_col,
                   (f12_acts[1] == "pa.c2" and f12_acts[2] == "pa.c1"), gc_freed,
                   (tr_esc["agents.back"] and tr_esc["view.back"]) == true, settled, true
            "#,
            )
            .call(km_core)
            .unwrap();
        assert_eq!(
            checks,
            (true, true, true, true, true, true, true, true, true, true),
            "checks failed: {checks:?}"
        );

        // 4. Direct plugin `mare.keymap.set` disposes cleanly via owner disposer when plugin unloads.
        std::fs::remove_file(plugins_dir.join("pa.lua")).unwrap();
        t.vt.host.reload_all().await.unwrap();
        let f9_after: i64 =
            eval(&t.vt.host, "local n = 0; for _, b in ipairs(mare.keymap.list()) do if b.key == 'f9' then n = n + 1 end end; return n").await;
        assert_eq!(f9_after, 0, "direct plugin mare.keymap.set must dispose on plugin unload");
    })
    .await
}

#[tokio::test]
async fn keymap_public_surface_user_plugin_and_runtime_keys_hot_reload() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.config_dir().join("plugins/user-key-probe");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("init.lua"),
            r#"
            return {
                name = "user-key-probe",
                apply = function()
                    -- 1. Zero underscore fields on mare.keymap, removed keymap fields absent on mare.ui, no _setup_key_handles.
                    local km_priv = {}
                    for k in next, mare.keymap do
                        if type(k) == "string" and k:sub(1, 1) == "_" then
                            km_priv[#km_priv + 1] = k
                        end
                    end
                    table.sort(km_priv)
                    assert(#km_priv == 0, "mare.keymap must expose no underscore fields: " .. table.concat(km_priv, ", "))
                    local forbidden_ui = {
                        "_keymap_check", "_keymap_context", "_keymap_contexts", "_core_actions",
                        "_set_bindings", "_context_info", "_hint", "_is_printable_key",
                        "_is_reserved_key", "_key_label", "_is_core_action", "_normalize_seq",
                        "_keymap_resolve", "_keymap_invoke", "_keymap_del", "_keymap_snapshot",
                        "_with_keymap_source", "_begin_keymap_tx", "_commit_keymap_tx",
                        "_rollback_keymap_tx", "_config_snapshot", "_config_restore", "_key_source",
                    }
                    local ui_priv = {}
                    for _, name in ipairs(forbidden_ui) do
                        if rawget(mare.ui, name) ~= nil then
                            ui_priv[#ui_priv + 1] = name
                        end
                    end
                    assert(#ui_priv == 0, "mare.ui must not expose removed keymap privates: " .. table.concat(ui_priv, ", "))
                    assert(rawget(mare, "_setup_key_handles") == nil, "mare._setup_key_handles must be removed")

                    -- 2. Public normalize_key (single + sequence + <Space>) and is_reserved.
                    assert(mare.ui.normalize_key("<C-t>") == "ctrl+t", "single key normalization")
                    assert(mare.ui.normalize_key("<C-x>g") == "ctrl+x g", "compact sequence normalization")
                    assert(mare.ui.normalize_key("<C-x> g") == "ctrl+x g", "spaced sequence normalization")
                    assert(mare.ui.normalize_key("<Space>") == "space", "space key normalization")
                    assert(mare.keymap.is_reserved("<C-c>") == true, "<C-c> must be reserved")
                    assert(mare.keymap.is_reserved("ctrl+c") == true, "ctrl+c must be reserved")
                    assert(mare.keymap.is_reserved("<C-c> a") == true, "sequence with ctrl+c must be reserved")
                    assert(mare.keymap.is_reserved("<F6>") == false, "<F6> must not be reserved")

                    -- 3. Public resolve / invoke / list({snapshot=true}) / del from a user-origin plugin.
                    local seen = {}
                    local h_ok = mare.keymap.action("user_probe.ok", function(ev)
                        seen[#seen + 1] = (ev.context or "") .. ":" .. (ev.key or "")
                        return true
                    end, { desc = "user probe ok" })
                    local h_dec = mare.keymap.action("user_probe.decline", function()
                        return false
                    end, { desc = "user probe decline" })
                    local h_set = mare.keymap.set("editor", "<F6>", "user_probe.ok")

                    local res = mare.keymap.resolve("editor", "<F6>", { editor_empty = true, working = false })
                    assert(type(res) == "table" and res.key == "f6" and res.masked == false, "resolve result shape")
                    assert(#res.candidates >= 1 and res.candidates[1].action == "user_probe.ok" and res.candidates[1].context == "editor",
                        "resolve must find user_probe.ok")

                    local invoked = mare.keymap.invoke("user_probe.ok", { key = "<F6>", context = "editor" })
                    assert(invoked == true, "invoke must return true for handled action")
                    assert(seen[1] == "editor:f6", "invoke must normalize ev.key: " .. tostring(seen[1]))
                    assert(mare.keymap.invoke("user_probe.decline", { key = "<F6>", context = "editor" }) == false,
                        "invoke must return false for declining action")
                    assert(mare.keymap.invoke("user_probe.missing") == nil,
                        "invoke must return nil for unknown Lua action")

                    local snap = mare.keymap.list({ snapshot = true, context = "editor" })
                    assert(type(snap) == "table" and type(snap.contexts) == "table" and type(snap.actions) == "table"
                        and type(snap.bindings) == "table" and type(snap.conflicts) == "table",
                        "list({snapshot = true}) must return snapshot structure")

                    local removed1, norm1 = mare.keymap.del("editor", "<F6>")
                    assert(removed1 == true and norm1 == "f6", "del must return (true, norm_key)")
                    local removed2, norm2 = mare.keymap.del("editor", "<F6>")
                    assert(removed2 == false and norm2 == "f6", "second del must return (false, norm_key)")

                    -- Rank impersonation must be rejected.
                    local ok_user = pcall(mare.keymap.set, "editor", "<F6>", "redraw", { layer = "user" })
                    local ok_proj = pcall(mare.keymap.set, "editor", "<F6>", "redraw", { layer = "project" })
                    assert(not ok_user and not ok_proj, "layer='user'/'project' must be rejected outside config")

                    h_set:remove()
                    h_dec:remove()
                    h_ok:remove()
                    mare.g.USER_KEY_PROBE_OK = true
                end,
            }
            "#,
        )
        .unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.setup { keys = { ["<F7>"] = "redraw" } }"#,
        )
        .unwrap();

        let mut hb = env.host().user_config(true).no_session();
        hb.opts.load_init = true;
        hb.opts.no_user_plugins = false;
        let h = mare_host::Host::new(hb.opts).await.unwrap();
        let problems = h.kernel.problems();
        assert!(
            eval::<bool>(&h, "return mare.g.USER_KEY_PROBE_OK == true").await,
            "user-key-probe plugin must pass all public API assertions; problems={problems:?}"
        );

        // 4. Initial setup.keys bound <F7> -> redraw at user layer.
        let init_f7: String = eval(
            &h,
            r#"for _, b in ipairs(mare.keymap.list()) do
                if b.context == "editor" and b.key == "f7" then return b.action .. ":" .. (b.layer or "") end
            end
            return "missing""#,
        )
        .await;
        assert_eq!(init_f7, "redraw:user", "init.lua setup.keys must bind at user layer");

        // 5. Runtime option/changed("keys") hot-reload updates bindings without reloading ui plugin,
        //    rolls back on error (e.g. reserved <C-c>), and clears when emptied.
        exec(&h, r#"mare.o.keys = { ["<F8>"] = "transcript" }"#).await;
        // Let the async option/changed notifier run.
        exec(&h, "mare.task.sleep(50)").await;
        let (has_f7_after, f8_act): (bool, String) = eval(
            &h,
            r#"local f7, f8 = false, "missing"
            for _, b in ipairs(mare.keymap.list()) do
                if b.context == "editor" and b.key == "f7" then f7 = true end
                if b.context == "editor" and b.key == "f8" then f8 = b.action end
            end
            return f7, f8"#,
        )
        .await;
        assert!(!has_f7_after, "runtime keys update must remove dropped <F7>");
        assert_eq!(f8_act, "transcript", "runtime keys update must bind new <F8>");

        exec(&h, r#"mare.o.keys = { ["<C-c>"] = "redraw" }; mare.task.sleep(50)"#).await;
        let f8_kept: String = eval(
            &h,
            r#"for _, b in ipairs(mare.keymap.list()) do
                if b.context == "editor" and b.key == "f8" then return b.action end
            end
            return "missing""#,
        )
        .await;
        assert_eq!(f8_kept, "transcript", "failed runtime keys update must roll back and keep <F8>");

        exec(&h, r#"mare.o.keys = {}; mare.task.sleep(50)"#).await;
        let f8_cleared: bool = eval(
            &h,
            r#"for _, b in ipairs(mare.keymap.list()) do
                if b.context == "editor" and b.key == "f8" then return false end
            end
            return true"#,
        )
        .await;
        assert!(f8_cleared, "clearing runtime keys must drop <F8>");

        h.shutdown().await;
    })
    .await
}

/// Ctrl+L (`redraw`) replays committed history via the same replay path as resize
/// (`\x1b[0m\x1b[H\x1b[J\x1b[3J`), never emitting `CSI 2J`, preserving multi-page scrollback;
/// while the `Ctrl+T` alt-screen overlay is open, `redraw` and `mare.ui.clear()` only repaint
/// the overlay without emitting `CSI 3J` or dumping main history into the alternate screen.
#[tokio::test]
async fn ctrl_l_redraws_via_replay_without_csi_2j() {
    local(async {
        let mut body = String::from("TOP-SCROLLBACK-MARKER\n\n");
        for i in 1..=30 {
            use std::fmt::Write as _;
            let _ = write!(body, "scroll-row-{i}\n\n");
        }
        body.push_str("committed-redraw-marker");
        let mut t = Term::new(json!([{ "text": body }]), 80, 24).await;
        t.submit("hello").await;
        t.idle().await;
        t.until("committed answer on screen", |s| s.contains("committed-redraw-marker")).await;
        let _ = t.vt.output();
        t.ctrl('l');
        t.vt.frame().await;
        let bytes = t.vt.output();
        t.parser.process(&bytes);
        let out = String::from_utf8_lossy(&bytes).into_owned();
        assert!(!out.contains("\x1b[2J"), "Ctrl+L must not emit CSI 2J: {out:?}");
        assert!(out.contains("\x1b[0m\x1b[H\x1b[J\x1b[3J"), "Ctrl+L must use replay clear sequence: {out:?}");
        let hist = t.history();
        assert!(
            hist.contains("TOP-SCROLLBACK-MARKER") && hist.contains("committed-redraw-marker"),
            "Ctrl+L must replay full multi-page committed history:\n{hist}"
        );

        // Bind `<C-l>` in `global` and open the `Ctrl+T` alt-screen transcript overlay, then jump to top (`g`).
        exec(&t.vt.host, r#"mare.keymap.set("global", "<C-l>", "redraw")"#).await;
        t.ctrl('t');
        t.step().await;
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.until("transcript overlay open at top", |s| s.contains("Transcript (expanded)")).await;
        let _ = t.vt.output();

        // Trigger redraw during alt-screen: must only repaint the overlay, never emit CSI 3J.
        t.ctrl('l');
        t.vt.frame().await;
        let alt_bytes = t.vt.output();
        t.parser.process(&alt_bytes);
        let alt_out = String::from_utf8_lossy(&alt_bytes).into_owned();
        assert!(!alt_out.contains("\x1b[3J"), "alt-screen redraw must not emit CSI 3J: {alt_out:?}");
        assert!(
            alt_out.contains("Transcript (expanded)") && t.screen().contains("Transcript (expanded)"),
            "alt-screen redraw must repaint overlay cleanly:\n{}",
            t.screen()
        );

        // Closing the overlay restores the main screen with full scrollback intact.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back on main screen", |s| !s.contains("Transcript (expanded)") && s.contains("Ask mare anything"))
            .await;
        let hist_after = t.history();
        assert!(
            hist_after.contains("TOP-SCROLLBACK-MARKER") && hist_after.contains("committed-redraw-marker"),
            "main screen history must remain intact after alt-screen redraw:\n{hist_after}"
        );

        // `mare.ui.clear()` while alt-screen is open must also not emit CSI 3J into the alt screen;
        // once closed, the deferred rebuild clears the main screen cleanly.
        t.ctrl('t');
        t.step().await;
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.until("transcript overlay reopened", |s| s.contains("Transcript (expanded)")).await;
        let _ = t.vt.output();
        exec(&t.vt.host, "mare.ui.clear()").await;
        t.vt.frame().await;
        let clear_alt = String::from_utf8_lossy(&t.vt.output()).into_owned();
        assert!(!clear_alt.contains("\x1b[3J"), "UiCmd::Clear during alt-screen must not emit CSI 3J: {clear_alt:?}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("returned after deferred clear", |s| !s.contains("Transcript (expanded)")).await;
    })
    .await
}

/// Compaction running: while compaction is running, Ctrl+D is ignored (never exits) and Ctrl+C
/// cancels compaction quietly without arming exit or leaving an error.
#[tokio::test]
async fn ctrl_c_cancels_compaction_quietly_and_ctrl_d_is_ignored() {
    local(async {
        let slow = format!("## Summary\n{}", "s".repeat(4000));
        let a1 = format!("a1 {}", "work ".repeat(1000));
        let a2 = format!("a2 {}", "work ".repeat(1000));
        let script = json!([{ "text": a1 }, { "text": a2 }, { "text": slow, "delay_ms": 20 }]);
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let mut t = Term::with(script, cfg, 80, 24).await;
        t.submit("one").await;
        t.idle().await;
        t.submit("two").await;
        t.idle().await;
        t.submit("/compact").await;
        t.until("the compaction running", |s| s.contains("Compacting conversation")).await;

        // Ctrl+D on empty input during compaction: ignored, does not quit, compaction keeps running.
        t.ctrl('d');
        t.step().await;
        assert!(!t.vt.quit_requested(), "Ctrl+D must not quit during compaction");
        assert!(t.vt.busy(), "Ctrl+D must not cancel compaction");

        // Ctrl+C during compaction: cancels quietly, does not arm exit, conversation unchanged.
        t.ctrl('c');
        t.until("the quiet cancel", |s| s.contains("Compaction cancelled")).await;
        t.until_vt("the working line to go", |t| !t.vt.busy()).await;
        assert!(!t.vt.quit_requested(), "Ctrl+C during compaction must not quit");
        let s = t.screen();
        assert!(!s.contains("Press Ctrl+C again to exit"), "Ctrl+C cancelling compaction must not arm exit:\n{s}");
        assert!(!s.contains("failed") && !s.contains("skipped") && !s.contains("incomplete"), "{s}");
        let h = t.history();
        assert!(h.contains("a1") && h.contains("a2") && !h.contains("Compacted conversation"), "{h}");
    })
    .await
}

/// Running background job: Esc does not stop it and double Ctrl+C
/// opens the exit confirmation listing the job; answering no stays and keeps the job running,
/// answering yes confirms exit.
#[cfg(unix)]
#[tokio::test]
async fn double_ctrl_c_with_running_background_job_asks_before_exiting() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        let jobs = t.vt.host.service_table("jobs").unwrap();
        t.vt.host.lua().globals().set("J", jobs).unwrap();
        t.vt.host
            .lua()
            .load(r#"JOB = J:start({ kind = "bash", description = "sleep 30", cmd = "sleep", args = { "30" }, background = true })"#)
            .exec_async()
            .await
            .unwrap();
        t.step().await;
        t.vt.dismiss_notices();

        // Esc while idle with a background job must not cancel the job.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<String>(&t.vt.host, "return JOB.status").await, "running");

        // First Ctrl+C arms exit; second Ctrl+C invokes exit guard confirmation.
        t.ctrl('c');
        t.until("exit armed", |s| s.contains("again to exit")).await;
        assert!(!t.vt.quit_requested());
        t.ctrl('c');
        t.until("background job exit confirmation", |s| {
            s.contains("1 background job is still") && s.contains("sleep 30")
        })
        .await;
        assert!(!t.vt.quit_requested());

        // Answering no keeps mare open and the background job running.
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("dialog closed", |s| !s.contains("still running")).await;
        assert!(!t.vt.quit_requested());
        assert_eq!(eval::<String>(&t.vt.host, "return JOB.status").await, "running");

        // Second double Ctrl+C and answering yes confirms exit.
        t.ctrl('c');
        t.until("exit armed again", |s| s.contains("again to exit")).await;
        t.ctrl('c');
        t.until("background job exit confirmation again", |s| s.contains("still running")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.until_vt("confirmed exit", |t| t.vt.quit_requested()).await;
        t.vt.host.shutdown().await;
    })
    .await
}

/// `/quit` and `Ctrl+D` on an idle empty editor both trigger a clean exit.
#[tokio::test]
async fn quit_command_and_ctrl_d_both_trigger_clean_exit() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        t.submit("/quit").await;
        t.until_vt("/quit sets exit flag", |t| t.vt.quit_requested()).await;
        drop(t);

        let mut t2 = Term::new(json!([]), 80, 24).await;
        t2.key(KeyCode::Char('d'), Modifiers::CONTROL);
        t2.step().await;
        assert!(t2.vt.quit_requested(), "Ctrl+D on an idle empty editor must exit immediately");
    })
    .await
}

/// `mare.keymap.set_many` registers multiple bindings in one batch (flushing once via
/// `set_bindings_batch`), rolls back atomically on error, and removes all batch bindings together.
#[tokio::test]
async fn keymap_set_many_registers_atomically_flushes_in_batch_and_removes_cleanly() {
    local(async {
        let t = Term::new(json!([]), 80, 24).await;
        let km_core = mare_shared::lua::core::keymap::handle(t.vt.host.lua()).unwrap();
        t.vt.host
            .lua()
            .load(
                r#"local km = ...
                assert(type(mare.keymap.set_many) == "function", "mare.keymap.set_many must exist")
                assert(type(km.set_bindings_batch) == "function", "core.keymap.set_bindings_batch must exist")
                local batch_calls, single_calls = 0, 0
                local orig_batch, orig_single = km.set_bindings_batch, km.set_bindings
                km.set_bindings_batch = function(updates)
                  batch_calls = batch_calls + 1
                  return orig_batch(updates)
                end
                km.set_bindings = function(...)
                  single_calls = single_calls + 1
                  return orig_single(...)
                end
                local ok, err = pcall(function()
                  local h = mare.keymap.set_many({
                    { "editor", "<F7>", "redraw" },
                    { "editor", "<F8>", "suspend", { when = { "editor_empty" } } },
                    { "text", { "<F7>", "<F8>" }, "text.left" },
                    { "select", "<F7>", "select.up" },
                  })
                  assert(batch_calls == 1, "expected 1 batch flush, got " .. tostring(batch_calls))
                  assert(single_calls == 0, "expected 0 single set_bindings calls, got " .. tostring(single_calls))
                  local r1 = mare.keymap.resolve("editor", "<F7>", {})
                  assert(r1.candidates[1] and r1.candidates[1].action == "redraw", "editor <F7> bound")
                  local r2 = mare.keymap.resolve("text", "<F8>", {})
                  assert(r2.candidates[1] and r2.candidates[1].action == "text.left", "text <F8> bound")
                  local r3 = mare.keymap.resolve("select", "<F7>", {})
                  assert(r3.candidates[1] and r3.candidates[1].action == "select.up", "select <F7> bound")

                  -- Atomic rollback: invalid second entry must not leave first entry bound.
                  local bad_ok = pcall(mare.keymap.set_many, {
                    { "editor", "<F9>", "redraw" },
                    { "editor", "<C-c>", "redraw" },
                  })
                  assert(not bad_ok, "reserved key in batch must error")
                  local r9 = mare.keymap.resolve("editor", "<F9>", {})
                  assert(#r9.candidates == 0, "failed batch must roll back earlier entries")

                  -- Removing via both `h:remove()` and `mare.keymap.remove(h)` removes every binding in one batch flush.
                  batch_calls = 0
                  mare.keymap.remove(h)
                  assert(batch_calls == 1, "expected 1 batch flush on mare.keymap.remove(h), got " .. tostring(batch_calls))
                  assert(#mare.keymap.resolve("editor", "<F7>", {}).candidates == 0, "editor <F7> removed")
                  assert(#mare.keymap.resolve("text", "<F8>", {}).candidates == 0, "text <F8> removed")
                  assert(#mare.keymap.resolve("select", "<F7>", {}).candidates == 0, "select <F7> removed")
                  -- Idempotent second removal does nothing.
                  batch_calls = 0
                  h:remove()
                  assert(batch_calls == 0, "second remove must be a no-op")
                end)
                km.set_bindings_batch = orig_batch
                km.set_bindings = orig_single
                assert(ok, err)"#,
            )
            .call::<()>(km_core)
            .unwrap();
    })
    .await
}

/// `mare.keymap.set_many` matches `mare.keymap.set` semantics across `unique`, owner ambiguity,
/// sequence conflicts, unified handle removal (`mare.keymap.remove(h)`), atomic host batch writes,
/// `init.lua` config reload, plugin unload, and `mare.setup { keys = ... }` hot updates.
#[tokio::test]
async fn keymap_set_many_matches_set_semantics_across_runtime_config_reload_and_unload() {
    local(async {
        // 1. Runtime equivalence: unique, ambiguity, sequence conflicts, unified handles, and
        //    atomic host `set_bindings_batch` validation before writes.
        {
            let t = Term::new(json!([]), 80, 24).await;
            let km_core = mare_shared::lua::core::keymap::handle(t.vt.host.lua()).unwrap();
            t.vt.host
                .lua()
                .load(
                    r#"local km = ...
                    local h_single = mare.keymap.set("editor", "<F6>", "redraw", { owner = "owner-a", layer = "plugin" })
                    assert(type(h_single.records) == "table" and #h_single.records == 1 and type(h_single.remove) == "function")

                    -- unique = true rejects conflict with existing binding without leaving earlier items bound.
                    local ok_u1, err_u1 = pcall(mare.keymap.set_many, {
                      { "editor", "<F7>", "redraw" },
                      { "editor", "<F6>", "suspend", { unique = true } },
                    }, { owner = "owner-b", layer = "plugin" })
                    assert(not ok_u1 and tostring(err_u1):find("unique = true"), tostring(err_u1))
                    assert(#mare.keymap.resolve("editor", "<F7>", {}).candidates == 0, "unique failure must roll back batch")

                    -- unique = true rejects intra-batch duplicates.
                    local ok_u2, err_u2 = pcall(mare.keymap.set_many, {
                      { "editor", "<F7>", "redraw" },
                      { "editor", "<F7>", "suspend", { unique = true } },
                    }, { owner = "owner-b", layer = "plugin" })
                    assert(not ok_u2 and tostring(err_u2):find("unique = true"), tostring(err_u2))
                    assert(#mare.keymap.resolve("editor", "<F7>", {}).candidates == 0, "intra-batch unique failure must roll back")

                    -- Ambiguity across different owners at equal layer/priority fails atomically.
                    local ok_amb, err_amb = pcall(mare.keymap.set_many, {
                      { "editor", "<F7>", "redraw" },
                      { "editor", "<F6>", "suspend" },
                    }, { owner = "owner-b", layer = "plugin" })
                    assert(not ok_amb and tostring(err_amb):find("ambiguous"), tostring(err_amb))
                    assert(#mare.keymap.resolve("editor", "<F7>", {}).candidates == 0, "ambiguity must roll back batch")

                    -- Sequence prefix conflict fails atomically.
                    local ok_seq, err_seq = pcall(mare.keymap.set_many, {
                      { "editor", "<F7>", "redraw" },
                      { "editor", "<F6> a", "suspend" },
                    }, { owner = "owner-a", layer = "plugin" })
                    assert(not ok_seq and tostring(err_seq):find("is also the start of", 1, true), tostring(err_seq))
                    assert(#mare.keymap.resolve("editor", "<F7>", {}).candidates == 0, "sequence conflict must roll back batch")

                    -- Same-owner replacement works via set_many and restores cleanly via mare.keymap.remove(h).
                    local h_batch = mare.keymap.set_many({
                      { "editor", "<F6>", "suspend" },
                      { "editor", "<F7>", "redraw" },
                    }, { owner = "owner-a", layer = "plugin" })
                    assert(type(h_batch.records) == "table" and #h_batch.records == 2 and type(h_batch.remove) == "function")
                    assert(mare.keymap.resolve("editor", "<F6>", {}).candidates[1].action == "suspend")
                    assert(mare.keymap.resolve("editor", "<F7>", {}).candidates[1].action == "redraw")
                    mare.keymap.remove(h_batch)
                    assert(#mare.keymap.resolve("editor", "<F6>", {}).candidates == 0, "mare.keymap.remove(batch) must clear F6")
                    assert(#mare.keymap.resolve("editor", "<F7>", {}).candidates == 0, "mare.keymap.remove(batch) must clear F7")
                    mare.keymap.remove(h_single)

                    -- Rust set_bindings_batch validates all rows before mutating any row.
                    local ok_host = pcall(km.set_bindings_batch, {
                      { "editor", "f9", { { action = "redraw", unbound = false, rank = 2, priority = 0, seq = 1, all = 0, none = 0, lua = false } } },
                      { "not-a-context", "f10", {} },
                    })
                    assert(not ok_host, "invalid context in second batch row must fail")
                    assert(#mare.keymap.resolve("editor", "<F9>", {}).candidates == 0, "first row must not persist after batch validation error")"#,
                )
                .call::<()>(km_core)
                .unwrap();
        }

        // 2. Config evaluation (`init.lua`), `mare.keymap.remove(h)` on staged batch handle,
        //    `/reload config`, `mare.setup { keys = ... }` hot updates, and plugin unload.
        {
            let env = TestEnv::new().await;
            let plugins = env.config_dir().join("plugins");
            std::fs::create_dir_all(&plugins).unwrap();
            let user_plugin = plugins.join("batch-plug.lua");
            std::fs::write(
                &user_plugin,
                r#"return {
                    name = "batch-plug",
                    apply = function()
                        mare.keymap.set_many({
                            { "editor", "<F7>", "redraw" },
                            { "editor", "<F8>", "suspend" },
                        })
                    end,
                }"#,
            )
            .unwrap();
            let init_lua = env.config_dir().join("init.lua");
            std::fs::write(
                &init_lua,
                r#"
                mare.setup { keys = { ["<F6>"] = "redraw" } }
                local drop_me = mare.keymap.set_many({
                    { "editor", "<F9>", "redraw" },
                })
                mare.keymap.remove(drop_me)
                mare.keymap.set_many({
                    { "editor", "<F10>", "redraw" },
                    { "select", "<F10>", "select.up" },
                })
                "#,
            )
            .unwrap();

            let mut b = env.host().user_config(false).faux(json!([])).no_session().config(json!({
                "logo": false,
                "plugins": { "onboarding": { "plugins": false } }
            }));
            b.opts.mode = Mode::Tui;
            let owned = OwnedVt::new(env, b.opts, 80, 24).await;
            let mut t = Term::from_vt(owned, vt100::Parser::new(nz(24), nz(80), 2000));
            t.step().await;
            t.until_vt("startup", |t| !t.vt.starting()).await;

            // Verify initial config set_many, immediate remove, setup.keys, and plugin set_many.
            exec(
                &t.vt.host,
                r#"
                assert(#mare.keymap.resolve("editor", "<F9>", {}).candidates == 0, "removed init batch must not stay bound")
                assert(mare.keymap.resolve("editor", "<F10>", {}).candidates[1].action == "redraw")
                assert(mare.keymap.resolve("select", "<F10>", {}).candidates[1].action == "select.up")
                assert(mare.keymap.resolve("editor", "<F6>", {}).candidates[1].action == "redraw")
                assert(mare.keymap.resolve("editor", "<F7>", {}).candidates[1].action == "redraw")
                assert(mare.keymap.resolve("editor", "<F8>", {}).candidates[1].action == "suspend")
                "#,
            )
            .await;

            // Reload config (staged path): test staged cancellation, committed staged batch handle
            // removal via `mare.keymap.remove(h)`, `setup.keys` hot-update, and plugin unload.
            std::fs::remove_file(&user_plugin).unwrap();
            std::fs::write(
                &init_lua,
                r#"
                mare.setup { keys = { ["<F6>"] = "suspend" } }
                local drop_staged = mare.keymap.set_many({
                    { "editor", "<F9>", "redraw" },
                })
                mare.keymap.remove(drop_staged)
                local staged_batch = mare.keymap.set_many({
                    { "editor", "<F11>", "redraw" },
                    { "select", "<F11>", "select.up" },
                })
                mare.keymap.action("test.drop_staged_batch", function()
                    mare.keymap.remove(staged_batch)
                end, { desc = "drop staged batch" })
                "#,
            )
            .unwrap();
            t.vt.host.reload_all().await.unwrap();
            t.step().await;

            exec(
                &t.vt.host,
                r#"
                assert(#mare.keymap.resolve("editor", "<F7>", {}).candidates == 0, "unloaded plugin batch F7 must be removed")
                assert(#mare.keymap.resolve("editor", "<F8>", {}).candidates == 0, "unloaded plugin batch F8 must be removed")
                assert(#mare.keymap.resolve("editor", "<F9>", {}).candidates == 0, "cancelled staged batch F9 must not commit")
                assert(#mare.keymap.resolve("editor", "<F10>", {}).candidates == 0, "old config batch F10 must be dropped on reload")
                assert(#mare.keymap.resolve("select", "<F10>", {}).candidates == 0, "old config batch select F10 must be dropped on reload")
                assert(mare.keymap.resolve("editor", "<F11>", {}).candidates[1].action == "redraw", "new staged config batch F11 must apply")
                assert(mare.keymap.resolve("select", "<F11>", {}).candidates[1].action == "select.up", "new staged config batch select F11 must apply")
                assert(mare.keymap.resolve("editor", "<F6>", {}).candidates[1].action == "suspend", "setup.keys must hot-update")
                -- Unified handle from staged config set_many works with mare.keymap.remove(h) after commit.
                assert(mare.keymap.invoke("test.drop_staged_batch") == true)
                assert(#mare.keymap.resolve("editor", "<F11>", {}).candidates == 0, "mare.keymap.remove(staged_batch) must clear editor F11")
                assert(#mare.keymap.resolve("select", "<F11>", {}).candidates == 0, "mare.keymap.remove(staged_batch) must clear select F11")
                "#,
            )
            .await;
        }
    })
    .await
}
