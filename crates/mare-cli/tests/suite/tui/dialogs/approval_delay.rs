//! Approval modal input-idle delay, pre-draw decision guard, digit exclusion, and paste-burst protection.

use super::*;

fn spawn_approve_lua(id: &str, with_cancel: bool) -> String {
    let cancel_setup = if with_cancel { "APPROVAL_TOK = mare.task.token()" } else { "APPROVAL_TOK = nil" };
    format!(
        r#"
        APPROVAL_DONE = false
        APPROVAL_DECISION = nil
        {cancel_setup}
        local binding = {{ owner = "test", turn = "t1", request = "{id}", snapshot = "snap-{id}" }}
        mare.approvals.register_snapshot(binding, {{
            call = {{ name = "bash", arguments = {{ command = "echo hi" }}, protected_arguments = false }},
            info = {{ unconfined = false }},
            verdict = {{ amendment_description = "allow echo hi" }},
        }})
        mare.task.spawn(function()
            local decision = mare.ui.approve({{
                approval_binding = binding,
                approval_arguments = {{ command = "echo hi" }},
                tool = "bash",
                choices = {{ "allow", "allow_session", "allow_amend", "deny", "abort" }},
                cancel = APPROVAL_TOK,
            }})
            APPROVAL_DECISION = decision
            APPROVAL_DONE = true
        end)
        "#
    )
}

#[tokio::test]
async fn approval_modal_delays_while_typing_and_resets_idle_deadline() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;
        tokio::time::pause();
        t.vt.reset_loop_state();

        // User is actively typing in the editor.
        t.key(KeyCode::Char('h'), Modifiers::NONE);
        exec(&t.vt.host, &spawn_approve_lua("delay-1", false)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        // Approval must stay queued while typing; pressing 'y' must stay in the editor.
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(!t.vt.has_modal(), "approval modal must not steal focus immediately while typing");
        assert_eq!(t.vt.editor_text(), "hy", "keystrokes during idle delay must remain in the editor");
        assert!(
            !eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await,
            "approval must not be answered by typing 'y'"
        );

        // Before 1s idle has elapsed, the approval stays queued.
        t.vt.run_production_loop_for(Duration::from_millis(900)).await;
        assert!(!t.vt.has_modal(), "approval modal opened before 1s typing idle delay elapsed");

        // Continued typing resets the 1s idle deadline.
        t.key(KeyCode::Char('!'), Modifiers::NONE);
        assert_eq!(t.vt.editor_text(), "hy!");
        t.vt.run_production_loop_for(Duration::from_millis(900)).await;
        assert!(!t.vt.has_modal(), "continued typing must reset the 1s idle delay");

        // Once 1s passes after the last keystroke, the production loop wakes and opens the modal.
        let opened = t.vt.run_production_loop_until(Duration::from_millis(300), |vt| vt.has_modal()).await;
        assert!(opened, "approval modal should open after 1s of typing idle");
        let out = t.vt.output();
        t.parser.process(&out);
        t.step().await;
        assert!(t.screen().contains("Allow once (y)"), "modal should be drawn:\n{}", t.screen());

        // Now a deliberate 'y' answers the approval.
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return APPROVAL_DECISION").await.as_deref(), Some("allow"));
    })
    .await
}

#[tokio::test]
async fn approval_modal_ignores_decision_keys_before_first_draw_and_excludes_digit_shortcuts() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;

        // Idle: approval opens into self.modal, but NO frame has been rendered yet.
        exec(&t.vt.host, &spawn_approve_lua("predraw-1", false)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(t.vt.has_modal(), "idle approval should become active modal");

        // Decision keys arriving before the modal has been drawn must not answer it.
        for code in [
            KeyCode::Char('y'),
            KeyCode::Char('a'),
            KeyCode::Char('A'),
            KeyCode::Char('n'),
            KeyCode::Enter,
            KeyCode::Char('1'),
        ] {
            t.key(code, Modifiers::NONE);
        }
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(t.vt.has_modal(), "undrawn approval modal must stay open on early decision keys");
        assert!(
            !eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await,
            "undrawn approval modal must not resolve before first draw"
        );

        // Draw the modal at least once.
        t.step().await;
        assert!(t.screen().contains("Allow once (y)"), "modal should be drawn:\n{}", t.screen());

        // Digit shortcuts (1-9) are disabled for approval modals even after drawing.
        t.key(KeyCode::Char('1'), Modifiers::NONE);
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(t.vt.has_modal(), "digit shortcut '1' must not decide an approval modal");
        assert!(!eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await);

        // Deliberate 'y' after draw resolves the approval.
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return APPROVAL_DECISION").await.as_deref(), Some("allow"));
    })
    .await
}

#[tokio::test]
async fn approval_modal_ignores_unbracketed_paste_burst_keys() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;
        t.vt.set_paste_burst(true);

        exec(&t.vt.host, &spawn_approve_lua("burst-1", false)).await;
        t.until("approval modal visible", |s| s.contains("Allow once (y)")).await;

        tokio::time::pause();
        // Simulate unbracketed paste arriving as a rapid key stream: m, a, y, Enter.
        for code in [KeyCode::Char('m'), KeyCode::Char('a'), KeyCode::Char('y'), KeyCode::Enter] {
            t.key(code, Modifiers::NONE);
        }
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(t.vt.has_modal(), "unbracketed paste burst must not decide an open approval modal");
        assert!(
            !eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await,
            "paste burst keys must not produce an approval decision"
        );

        // After the burst window expires, a deliberate 'y' still works.
        tokio::time::advance(Duration::from_millis(250)).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return APPROVAL_DECISION").await.as_deref(), Some("allow"));
    })
    .await
}

#[tokio::test]
async fn cancel_token_dismisses_delayed_approval_before_it_opens() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;
        tokio::time::pause();
        t.vt.reset_loop_state();

        t.key(KeyCode::Char('h'), Modifiers::NONE);
        exec(&t.vt.host, &spawn_approve_lua("cancel-1", true)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(!t.vt.has_modal(), "approval should be waiting in delayed queue");

        exec(&t.vt.host, "APPROVAL_TOK:cancel()").await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }

        t.vt.run_production_loop_for(Duration::from_millis(1500)).await;
        assert!(!t.vt.has_modal(), "cancelled delayed approval must never open");
        let (done, decision): (bool, Option<String>) =
            eval(&t.vt.host, "return APPROVAL_DONE == true, APPROVAL_DECISION").await;
        assert!(done, "cancelled approval coroutine should complete");
        assert_eq!(decision, None);
    })
    .await
}

#[tokio::test]
async fn approval_modal_idle_delay_blocks_enter_from_approving_default_choice() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;
        tokio::time::pause();
        t.vt.reset_loop_state();

        // User finishes typing a draft and hits Enter just as an approval arrives.
        t.vt.type_str("hi");
        exec(&t.vt.host, &spawn_approve_lua("enter-1", false)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(!t.vt.has_modal(), "approval must wait behind active typing");

        // Pressing Enter submits the draft (not default approval choice 0), and an immediate
        // follow-up Enter still cannot auto-approve while the idle delay is active.
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(!t.vt.has_modal(), "Enter while approval is queued must not pop and confirm it");
        assert!(
            !eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await,
            "Enter must not silently approve default choice 0"
        );

        // After 1s idle, the approval opens and Enter works once drawn.
        let opened = t.vt.run_production_loop_until(Duration::from_millis(1200), |vt| vt.has_modal()).await;
        assert!(opened, "approval modal should open after 1s idle");
        let out = t.vt.output();
        t.parser.process(&out);
        t.step().await;
        assert!(t.screen().contains("Allow once (y)"), "modal should be drawn:\n{}", t.screen());

        t.key(KeyCode::Enter, Modifiers::NONE);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return APPROVAL_DECISION").await.as_deref(), Some("allow"));
    })
    .await
}

#[tokio::test]
async fn approval_modal_after_submit_keeps_idle_delay_so_next_typed_key_cannot_approve() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;
        tokio::time::pause();
        t.vt.reset_loop_state();

        // User types a prompt and hits Enter to submit while no approval is queued yet.
        t.vt.type_str("hi");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.vt.run_production_loop_for(Duration::from_millis(150)).await;

        // An approval arrives shortly after submit (within the 1s typing idle delay).
        exec(&t.vt.host, &spawn_approve_lua("after-submit-1", false)).await;
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;

        // Typing 'a' for the next message must stay in the editor and never grant allow_session.
        t.key(KeyCode::Char('a'), Modifiers::NONE);
        for _ in 0..4 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert!(!t.vt.has_modal(), "approval arriving right after Enter submit must still wait for the 1s idle delay");
        assert_eq!(
            t.vt.editor_text(),
            "a",
            "keystroke shortly after submit must go to the editor instead of answering approval"
        );
        assert!(
            !eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await,
            "typing 'a' right after submit must not grant allow_session"
        );

        // Once 1s of idle elapses after the last keystroke, the approval opens and can be answered.
        let opened = t.vt.run_production_loop_until(Duration::from_millis(1200), |vt| vt.has_modal()).await;
        assert!(opened, "approval modal should open after 1s of typing idle");
        let out = t.vt.output();
        t.parser.process(&out);
        t.step().await;
        assert!(t.screen().contains("Allow once (y)"), "modal should be drawn:\n{}", t.screen());

        t.key(KeyCode::Char('a'), Modifiers::NONE);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert_eq!(
            eval::<Option<String>>(&t.vt.host, "return APPROVAL_DECISION").await.as_deref(),
            Some("allow_session")
        );
    })
    .await
}

/// Alt- or Super-modified letter keys (e.g. `Alt+Y`, `Alt+A`, `Super+Y`) must never trigger
/// single-character approval decision shortcuts.
#[tokio::test]
async fn approval_modal_ignores_alt_and_super_modified_decision_keys() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;
        exec(&t.vt.host, &spawn_approve_lua("alt-guard-1", false)).await;
        t.until("approval modal drawn", |s| s.contains("Allow once (y)")).await;

        for (ch, mods) in [
            ('y', Modifiers::ALT),
            ('a', Modifiers::ALT),
            ('y', Modifiers::SUPER),
            ('a', Modifiers::SUPER),
            ('y', Modifiers::HYPER),
            ('a', Modifiers::HYPER),
            ('y', Modifiers::META),
            ('a', Modifiers::META),
        ] {
            t.key(KeyCode::Char(ch), mods);
            for _ in 0..4 {
                tokio::task::yield_now().await;
                t.vt.drain_pending();
            }
            assert!(t.vt.has_modal(), "{mods:?}+{ch} must not dismiss or answer the approval modal");
            assert!(
                !eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await,
                "{mods:?}+{ch} must not trigger approval decision"
            );
        }

        // Plain `y` still approves normally.
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return APPROVAL_DECISION").await.as_deref(), Some("allow"));
    })
    .await
}

/// When an upper modal (e.g. `mare.ui.input` askpass prompt) closes over a stacked approval modal,
/// the restored approval must reset `drawn = false` and enforce the typing idle cooldown so a
/// double-tapped `Enter Enter` cannot approve the underlying tool.
#[tokio::test]
async fn stacked_approval_under_input_modal_requires_cooldown_and_redraw_after_enter() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.wait_startup_settled().await;
        exec(&t.vt.host, &spawn_approve_lua("stacked-1", false)).await;
        t.until("approval modal drawn first", |s| s.contains("Allow once (y)")).await;

        // Open an input modal on top of the drawn approval (pushes approval onto modal_stack).
        tokio::time::pause();
        t.vt.reset_loop_state();
        exec(
            &t.vt.host,
            r#"
            INPUT_RESULT = nil
            mare.task.spawn(function()
                INPUT_RESULT = mare.ui.input({ title = "Passphrase", default = "secret" })
            end)
            "#,
        )
        .await;
        t.vt.run_production_loop_for(Duration::from_millis(50)).await;
        let out = t.vt.output();
        t.parser.process(&out);
        assert!(t.screen().contains("Passphrase"), "input modal must sit on top of approval:\n{}", t.screen());

        // Send two Enters back-to-back in the same burst: the first submits the input modal,
        // and the second must NOT approve the stacked approval underneath.
        let t0 = tokio::time::Instant::now().into_std();
        t.vt.send_tty_event_at(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE)), t0);
        t.vt.send_tty_event_at(
            mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE)),
            t0 + Duration::from_millis(5),
        );
        t.vt.run_production_loop_for(Duration::from_millis(250)).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return INPUT_RESULT").await.as_deref(), Some("secret"));
        assert!(
            !eval::<bool>(&t.vt.host, "return APPROVAL_DONE == true").await,
            "second Enter after closing upper input modal must not approve stacked approval"
        );
        assert!(!t.vt.has_modal(), "restored stacked approval must wait the 1s typing cooldown");

        // After the 1s cooldown elapses, the approval reopens cleanly and can be answered.
        let reopened = t.vt.run_production_loop_until(Duration::from_millis(1200), |vt| vt.has_modal()).await;
        assert!(reopened, "stacked approval must reopen after the 1s cooldown");
        t.vt.frame().await;
        let out = t.vt.output();
        t.parser.process(&out);
        assert!(t.screen().contains("Allow once (y)"), "reopened approval must be drawn:\n{}", t.screen());

        t.key(KeyCode::Char('y'), Modifiers::NONE);
        for _ in 0..8 {
            tokio::task::yield_now().await;
            t.vt.drain_pending();
        }
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return APPROVAL_DECISION").await.as_deref(), Some("allow"));
        tokio::time::resume();
    })
    .await
}
