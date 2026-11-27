//! User-facing keyboard and dialog hints.

use super::*;

#[tokio::test]
async fn completion_footer_and_dispatch_follow_rebound_keys() {
    local(async {
        let mut t = Term::new(json!([]), 140, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('editor', '<Tab>', false)
            mare.keymap.set('editor', '<C-e>', 'complete')
            mare.keymap.set('editor', '<Esc>', false)
            mare.keymap.set('editor', '<M-i>', 'interrupt')
        "#,
        )
        .await;
        t.vt.type_str("/");
        t.until("completion popup", |s| s.contains("Commands") && s.contains("/compact")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+E complete") && screen.contains("Alt+I close"), "{screen}");
        assert!(!screen.contains("Tab complete") && !screen.contains("Esc close"), "{screen}");
        exec(&t.vt.host, "mare.keymap.set('editor', '<C-e>', false); mare.keymap.set('editor', '<M-i>', false)").await;
        t.step().await;
        let screen = t.screen();
        assert!(
            !screen.contains("Ctrl+E complete")
                && !screen.contains("Alt+I close")
                && !screen.contains("Tab complete")
                && !screen.contains("Esc close"),
            "{screen}"
        );
        exec(&t.vt.host, "mare.keymap.set('editor', '<M-i>', 'interrupt')").await;
        t.step().await;
        t.key(KeyCode::Char('i'), Modifiers::ALT);
        t.until("completion closed by rebound interrupt", |s| !s.contains("Commands")).await;
    })
    .await
}

#[tokio::test]
async fn fallback_completion_footer_follows_rebound_keys() {
    local(async {
        let mut t = Term::new(json!([]), 140, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.ui.CompletionPopup.render = function() return nil end
            mare.keymap.set('editor', '<Tab>', false)
            mare.keymap.set('editor', '<C-e>', 'complete')
            mare.keymap.set('editor', '<Esc>', false)
            mare.keymap.set('editor', '<M-i>', 'interrupt')
        "#,
        )
        .await;
        t.vt.type_str("/");
        t.until("fallback completion", |s| s.contains("Commands") && s.contains("/compact")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+E complete") && screen.contains("Alt+I close"), "{screen}");
        assert!(!screen.contains("Tab complete") && !screen.contains("Esc close"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn completion_navigation_respects_keymap_conditions() {
    local(async {
        let mut t = Term::new(json!([]), 140, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('text', '<C-n>', false)
            mare.keymap.set('editor.completion', '<C-n>', 'text.down', { when = { 'working' } })
        "#,
        )
        .await;
        t.vt.type_str("/");
        t.until("completion first item", |s| s.contains("Commands") && s.contains("1/")).await;
        t.ctrl('n');
        t.step().await;
        assert!(t.screen().contains("1/"), "inactive when condition must not move the selection: {}", t.screen());
    })
    .await
}

#[tokio::test]
async fn which_key_cancel_hint_tracks_interrupt_binding() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('editor', '<Esc>', false)
            mare.keymap.set('editor', '<M-i>', 'interrupt')
            mare.keymap.set('editor', '<C-x>g', 'redraw')
        "#,
        )
        .await;
        t.step().await;
        t.ctrl('x');
        t.until("which-key", |s| s.contains("redraw") && s.contains("cancel")).await;
        let screen = t.screen();
        assert!(screen.contains("Alt+I cancel") && !screen.contains("Esc cancel"), "{screen}");
        t.key(KeyCode::Char('i'), Modifiers::ALT);
        t.until("which-key cancelled", |s| !s.contains("Alt+I cancel")).await;
    })
    .await
}

#[tokio::test]
async fn text_selection_cancel_hint_follows_interrupt_binding() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('editor', '<Esc>', false)
            mare.keymap.set('editor', '<M-i>', 'interrupt')
        "#,
        )
        .await;
        t.key(KeyCode::Char('s'), Modifiers::ALT);
        t.until("selection mode", |s| s.contains("select text")).await;
        let screen = t.screen();
        assert!(screen.contains("Alt+I cancel") && !screen.contains("Esc cancel"), "{screen}");
        t.key(KeyCode::Char('i'), Modifiers::ALT);
        t.until("selection cancelled", |s| !s.contains("select text")).await;
    })
    .await
}

#[tokio::test]
async fn conditional_interrupt_does_not_steal_prefix_or_selection_keys() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.g.prefix_hit = false
            mare.keymap.action('prefix.probe', function() mare.g.prefix_hit = true end, { contexts = { 'editor' } })
            mare.keymap.set('editor', '<C-x>g', 'prefix.probe')
            mare.keymap.set('editor', 'g', 'interrupt', { when = { 'working' } })
            mare.keymap.set('editor', 'a', 'interrupt', { when = { 'working' } })
        "#,
        )
        .await;
        t.step().await;
        t.ctrl('x');
        t.until("prefix open", |s| s.contains("prefix.probe") || s.contains("Ctrl+X")).await;
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.step().await;
        assert!(
            eval::<bool>(&t.vt.host, "return mare.g.prefix_hit").await,
            "inactive interrupt must not cancel a valid sequence"
        );
        t.key(KeyCode::Char('s'), Modifiers::ALT);
        t.until("selection mode", |s| s.contains("select text")).await;
        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.step().await;
        assert_eq!(t.vt.editor_text(), "a", "inactive interrupt must not consume the key");
    })
    .await
}

#[tokio::test]
async fn fallback_working_indicator_uses_the_current_interrupt_binding() {
    local(async {
        let mut t = Term::new(json!([]), 140, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.ui.Working.render = function() return nil end
            mare.keymap.set('editor', '<Esc>', false)
            mare.keymap.set('editor', '<M-i>', 'interrupt')
            mare.ui.working('Working probe')
        "#,
        )
        .await;
        t.until("working indicator", |s| s.contains("Working probe")).await;
        let screen = t.screen();
        assert!(screen.contains("Alt+I to interrupt") && !screen.contains("Esc to interrupt"), "{screen}");
        exec(&t.vt.host, "mare.keymap.set('editor', '<M-i>', false)").await;
        t.step().await;
        let screen = t.screen();
        assert!(!screen.contains("Alt+I to interrupt") && !screen.contains("Esc to interrupt"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn folded_thinking_uses_the_current_expand_binding() {
    local(async {
        let mut t = Term::new(json!([]), 140, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('editor', '<C-o>', false)
            mare.keymap.set('editor', '<C-x>', 'toggle-expand')
            local id = mare.ui.push('reasoning', { markdown = 'private reasoning' })
            mare.ui.finish(id)
        "#,
        )
        .await;
        t.until("folded thinking", |s| s.contains("Thought for")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+X to expand") && !screen.contains("Ctrl+O to expand"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn long_tool_command_uses_the_current_expand_binding() {
    local(async {
        let mut t = Term::new(json!([]), 140, 24).await;
        expose(&t.vt.host);
        global(&t.vt.host, "UI_SERVICE", t.vt.host.service_table("ui").unwrap());
        exec(&t.vt.host, r#"
            mare.keymap.set('editor', '<C-o>', false)
            mare.keymap.set('editor', '<C-x>', 'toggle-expand')
            local D = { ctx = C, cfg = { preview_lines = 1 }, U = mare.ui,
              service = { service = { get_keymap = function(_, _, context) return UI_SERVICE:get_keymap(context) end } } }
            local nodes = require('ui.tool_nodes')(D)
            function MAKE_TOOL_HINT()
              local node = nodes.bash_node({ arguments = { command = 'echo first\necho second' } }, nil, false)
              mare.ui.push('tool', { node = node })
            end
            MAKE_TOOL_HINT()
        "#).await;
        t.until("long tool command", |s| s.contains("more line")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+X to expand") && !screen.contains("Ctrl+O to expand"), "{screen}");
        exec(&t.vt.host, "mare.keymap.set('editor', '<C-x>', false); mare.keymap.set('editor', '<M-z>', 'toggle-expand'); MAKE_TOOL_HINT()").await;
        t.until("new tool hint", |s| s.contains("Alt+Z to expand")).await;
        exec(&t.vt.host, "mare.keymap.set('editor', '<M-z>', false); MAKE_TOOL_HINT()").await;
        t.until("unbound tool hint", |s| s.matches("more line").count() >= 3).await;
        let screen = t.screen();
        let newest = screen.lines().rfind(|line| line.contains("more line")).unwrap();
        assert!(!newest.contains("to expand"), "unbound action should have no key hint: {screen}");
    }).await
}

#[tokio::test]
async fn review_picker_uses_rebound_viewer_key() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["review"]).faux(json!([])).config(json!({"logo": false}));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 120, 24).await;
        let mut t = Term { owned, parser: vt100::Parser::new(nz(24), nz(120), 2000), ended: 0 };
        t.step().await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('editor', '<C-r>', false)
            mare.keymap.set('editor', '<C-j>', 'review.viewer')
        "#,
        )
        .await;
        t.submit("/review").await;
        t.until("review picker", |s| s.contains("Browse changes")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+J") && !screen.contains("Ctrl+R"), "{screen}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("review picker closed", |s| !s.contains("Browse changes")).await;
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('editor', '<C-j>', false)
        "#,
        )
        .await;
        t.submit("/review").await;
        t.until("review picker reopened", |s| s.contains("Browse changes")).await;
        assert!(!t.screen().contains("Ctrl+J") && !t.screen().contains("Ctrl+R"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn review_working_hint_tracks_rebinding_and_unbinding() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["review"]).faux(json!([])).config(json!({"logo": false}));
        b.opts.mode = Mode::Tui;
        let owned = OwnedVt::new(env, b.opts, 140, 24).await;
        let mut t = Term { owned, parser: vt100::Parser::new(nz(24), nz(140), 2000), ended: 0 };
        t.step().await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            REVIEW_HOLD = mare.task.token()
            mare.subagent.start = function()
              return {
                wait = function() REVIEW_HOLD:wait(); return { text = 'done' } end,
                cancel = function() REVIEW_HOLD:cancel() end,
              }
            end
            mare.keymap.set('editor', '<Esc>', false)
            mare.keymap.set('editor', '<M-i>', 'interrupt')
        "#,
        )
        .await;
        t.submit("/review inspect the current code").await;
        t.until("review working", |s| s.contains("Reviewing") && s.contains("Alt+I")).await;
        exec(&t.vt.host, "mare.keymap.set('editor', '<M-i>', false); mare.keymap.set('editor', '<C-k>', 'interrupt')")
            .await;
        t.step().await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+K") && !screen.contains("Alt+I"), "review must follow the new binding: {screen}");
        exec(&t.vt.host, "mare.keymap.set('editor', '<C-k>', false)").await;
        t.step().await;
        let screen = t.screen();
        assert!(!screen.contains("Alt+I") && !screen.contains("Ctrl+K") && !screen.contains("Esc stops"), "{screen}");
        exec(&t.vt.host, "REVIEW_HOLD:cancel()").await;
    })
    .await
}

#[tokio::test]
async fn refreshed_plugin_transcript_preserves_scroll_and_narrow_diff_fits() {
    local(async {
        let mut t = Term::new(json!([]), 40, 12).await;
        expose(&t.vt.host);
        exec(&t.vt.host, r#"
            function REFRESH_TRANSCRIPT(extra)
              local rows = {}
              for i = 1, 40 do rows[#rows + 1] = mare.ui.Line(('row %02d'):format(i)) end
              if extra then rows[#rows + 1] = mare.ui.Line('NEW TAIL') end
              mare.ui.transcript('Live list', mare.ui.Lines(rows))
            end
            REFRESH_TRANSCRIPT(false)
        "#).await;
        t.until("transcript end", |s| s.contains("row 40")).await;
        t.key(KeyCode::Home, Modifiers::NONE);
        t.until("transcript top", |s| s.contains("row 01") && !s.contains("row 40")).await;
        let before = t.screen().lines().filter(|line| line.contains("row ")).map(str::to_owned).collect::<Vec<_>>();
        exec(&t.vt.host, "REFRESH_TRANSCRIPT(true)").await;
        t.step().await;
        let after = t.screen().lines().filter(|line| line.contains("row ")).map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(after, before, "refresh must preserve a scrolled-up viewport");
        t.key(KeyCode::End, Modifiers::NONE);
        t.until("new tail", |s| s.contains("NEW TAIL")).await;
        exec(&t.vt.host, r#"
            mare.ui.transcript('Narrow diff', mare.ui.Diff({ old = ('old_'):rep(100), new = ('new_'):rep(100), path = 'long.txt' }))
        "#).await;
        t.until("narrow diff", |s| s.contains("old_") || s.contains("new_")).await;
        t.key(KeyCode::PageDown, Modifiers::NONE);
        t.step().await;
        assert!(t.screen().lines().all(|line| line.chars().count() <= 40), "{}", t.screen());
    }).await
}

#[tokio::test]
async fn working_placeholder_tracks_rebound_and_unbound_keys() {
    local(async {
        let mut t = Term::new(json!([{ "text": "working ".repeat(100), "delay_ms": 100 }]), 140, 24).await;
        t.submit("begin").await;
        t.until("working placeholder", |s| s.contains("Queue a message")).await;
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('editor', '<C-s>', false)
            mare.keymap.set('editor', '<C-k>', 'steer')
            mare.keymap.set('editor', '<Esc>', false)
            mare.keymap.set('editor', '<M-i>', 'interrupt')
        "#,
        )
        .await;
        t.step().await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+K steer now") && screen.contains("Alt+I interrupt"), "{screen}");
        assert!(!screen.contains("Ctrl+S steer now") && !screen.contains("Esc interrupt"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn idle_alt_enter_inserts_a_newline() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.type_str("alpha");
        t.key(KeyCode::Enter, Modifiers::ALT);
        t.vt.type_str("beta");
        t.step().await;
        assert_eq!(t.vt.editor_text(), "alpha\nbeta");
    })
    .await
}

#[tokio::test]
async fn shift_tab_cycles_permissions_but_respects_untrusted_folder() {
    local(async {
        let mut t = Term::with(json!([]), json!({ "plugins": { "permissions": { "preset": "read-only" } } }), 120, 24).await;
        expose(&t.vt.host);
        global(&t.vt.host, "P", t.vt.host.service_table("permissions").unwrap());
        exec(&t.vt.host, "mare.trusted = true; P:set('read-only')").await;
        let binding: String = eval(&t.vt.host, "for _, b in ipairs(mare.keymap.list()) do if b.key == 'shift+tab' and b.context == 'editor' then return tostring(b.action) end end return 'missing'").await;
        assert_eq!(binding, "permissions.cycle", "the permission action must be registered");
        t.step().await;
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        let auto_badge = if cfg!(windows) { "PARTIAL" } else { "AUTO" };
        let activated = tokio::time::timeout(Duration::from_secs(5), t.until("auto preset", |s| s.contains(auto_badge))).await;
        let actual: String = eval(&t.vt.host, "return P:preset()").await;
        assert!(activated.is_ok(), "permission cycle did not activate: preset={actual}; screen: {}", t.screen());
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "auto");
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        tokio::time::timeout(Duration::from_secs(5), t.until("full access confirmation", |s| s.contains("Full access:")))
            .await
            .expect("full access must be confirmed");
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        tokio::time::timeout(Duration::from_secs(5), t.until("full preset", |s| s.lines().last().is_some_and(|bar| bar.contains("FULL"))))
            .await
            .expect("full access preset did not activate");
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "full-access");
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        let read_badge = if cfg!(windows) { "PARTIAL" } else { "READ" };
        tokio::time::timeout(Duration::from_secs(5), t.until("read-only preset", |s| s.lines().last().is_some_and(|bar| bar.contains(read_badge))))
            .await
            .expect("cycle did not return to read-only");
        t.vt.type_str("/");
        t.until("completion menu", |s| s.contains("Commands") && s.contains("/compact")).await;
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        t.step().await;
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "read-only", "completion owns Shift+Tab");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.ctrl('c');
        t.step().await;
        exec(&t.vt.host, "mare.trusted = false; P:set('full-access')").await;
        // Untrusted directory: stepping down from FULL to READ still cycles directly.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted downgrade to read-only", |s| s.lines().last().is_some_and(|bar| bar.contains(read_badge))),
        )
        .await
        .expect("untrusted Shift+Tab from full-access must step down to read-only");
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "read-only");
        assert!(!eval::<bool>(&t.vt.host, "return mare.trusted").await, "trust state must stay false");

        // Next Shift+Tab from READ would raise to AUTO: opens the /permission picker with READ selected.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        let opened = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted permission picker", |s| {
                s.contains("Permissions (untrusted folder)") && s.contains("READ  read-only")
            }),
        )
        .await;
        assert!(opened.is_ok(), "untrusted Shift+Tab did not open permission picker; screen:\n{}", t.screen());
        // Pressing Enter immediately keeps the default-selected current mode (READ).
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "read-only");
        assert!(!eval::<bool>(&t.vt.host, "return mare.trusted").await);

        // Opening again and pressing Esc leaves READ unchanged.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        let reopened = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted picker before esc", |s| s.contains("Permissions (untrusted folder)")),
        )
        .await;
        assert!(reopened.is_ok(), "picker did not reopen; screen:\n{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "read-only");
        assert!(!eval::<bool>(&t.vt.host, "return mare.trusted").await);

        // Explicitly selecting AUTO in the picker switches to AUTO without changing trust.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        let for_auto = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted picker for auto", |s| s.contains("Permissions (untrusted folder)")),
        )
        .await;
        assert!(for_auto.is_ok(), "picker did not open for auto; screen:\n{}", t.screen());
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        let auto_ok = tokio::time::timeout(Duration::from_secs(5), t.until("auto preset from picker", |s| s.contains(auto_badge))).await;
        assert!(auto_ok.is_ok(), "explicit AUTO selection in untrusted picker did not activate; screen:\n{}", t.screen());
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "auto");
        assert!(!eval::<bool>(&t.vt.host, "return mare.trusted").await, "explicit AUTO must not trust folder");

        // Next Shift+Tab from AUTO would raise to FULL: opens picker with AUTO selected; picking FULL still asks for confirmation.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        let from_auto = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted picker from auto", |s| s.contains("Permissions (untrusted folder)")),
        )
        .await;
        assert!(from_auto.is_ok(), "picker did not open from auto; screen:\n{}", t.screen());
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        let confirm_full = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted full access confirmation", |s| s.contains("Full access")),
        )
        .await;
        assert!(confirm_full.is_ok(), "selecting FULL in untrusted folder must still ask for confirmation; screen:\n{}", t.screen());
        // Cancel first -> stays AUTO.
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "auto");
        assert!(!eval::<bool>(&t.vt.host, "return mare.trusted").await);

        // Confirm FULL -> switches to FULL, trust still false.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        let for_full = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted picker for full confirm", |s| s.contains("Permissions (untrusted folder)")),
        )
        .await;
        assert!(for_full.is_ok(), "picker did not reopen for full; screen:\n{}", t.screen());
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        let confirm_yes = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("untrusted full confirm yes", |s| s.contains("Full access")),
        )
        .await;
        assert!(confirm_yes.is_ok(), "full confirmation did not reappear; screen:\n{}", t.screen());
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        let full_ok = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("full preset from untrusted picker", |s| s.lines().last().is_some_and(|bar| bar.contains("FULL"))),
        )
        .await;
        assert!(full_ok.is_ok(), "confirmed FULL did not activate; screen:\n{}", t.screen());
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "full-access");
        assert!(!eval::<bool>(&t.vt.host, "return mare.trusted").await, "confirmed FULL must not trust folder");
    })
    .await
}

#[tokio::test]
async fn key_help_shows_pretty_keys_and_action_descriptions() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.action('example.action', function() end,
                   { desc = 'perform example action', keys = { editor = '<C-k>' } })"#,
        )
        .await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("key help", |s| s.contains("─ Keys ─")).await;
        t.vt.type_str("toggle");
        t.until("toggle entry", |s| s.contains("toggle")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+O") && screen.contains("toggle compact"), "{screen}");
        assert!(!screen.contains("toggle-expand") && !screen.contains("ctrl+o"), "{screen}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("key help again", |s| s.contains("─ Keys ─")).await;
        t.vt.type_str("perform example");
        t.until("custom action", |s| s.contains("perform example action")).await;
        assert!(t.screen().contains("Ctrl+K"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn trust_dialog_names_resources_and_read_only_mode() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({ "trust_pending": true }), 110, 30, |env| {
            let skill = env.cwd.join(".mare/skills/example");
            std::fs::create_dir_all(&skill).unwrap();
            std::fs::write(skill.join("SKILL.md"), "---\nname: example\ndescription: test\n---\nbody\n").unwrap();
            std::fs::write(
                env.cwd.join(".mcp.json"),
                r#"{"mcpServers":{"example-server":{"command":"/usr/local/bin/example-cli","args":["--token","private-value"],"env":{"SECRET":"private-value"}}}}"#,
            )
            .unwrap();
        })
        .await;
        t.until("trust dialog", |s| s.contains("Trust this folder?") && s.contains("Trust and load")).await;
        let screen = t.screen();
        assert!(
            screen.contains(".mare/skills")
                && screen.contains("read-only")
                && screen.contains("example-server (example-cli)"),
            "{screen}"
        );
        assert!(!screen.contains("private-value"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn model_argument_completion_is_not_labeled_files() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        t.vt.dismiss_notices();
        exec(
            &t.vt.host,
            r#"mare.ui.completion.register({ name = 'model-example', triggers = {}, min_prefix = 0,
                   enabled = function(ev) return (ev.text or ''):match('^/model ') ~= nil end,
                   complete = function() return { { label = 'openai/gpt-example', insert = 'openai/gpt-example' } } end })"#,
        )
        .await;
        t.vt.type_str("/model openai/gpt");
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("model argument completion", |s| s.contains("openai/gpt-example")).await;
        let screen = t.screen();
        assert!(screen.contains("Completions") && !screen.contains("─ Files ─"), "{screen}");
        assert!(screen.contains("Tab complete") && screen.contains("Esc close"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn model_arguments_open_completion_and_show_a_non_submitted_hint() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        t.vt.dismiss_notices();
        t.vt.type_str("/model ");
        t.until("model choices and argument hint", |s| {
            s.contains("Completions") && s.contains("faux/echo") && s.contains("[provider/id]")
        })
        .await;
        assert_eq!(t.vt.editor_text(), "/model ", "the hint must not become prompt text");
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.step().await;
        let text = t.vt.editor_text();
        assert!(
            text.starts_with("/model ") && text.len() > "/model ".len() && !text.contains("[provider/id]"),
            "{text}"
        );
    })
    .await
}

#[tokio::test]
async fn approval_hints_show_real_keys_without_search() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "A" } }] },
            { "text": "done" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });
        let mut t = Term::with(script, cfg, 160, 30).await;
        t.submit("write a").await;
        t.until("approval", |s| s.contains("Allow once")).await;
        let screen = t.screen();
        assert!(!screen.contains("Enter select") && !screen.contains("/ search"), "{screen}");
        assert!(
            screen.contains("Enter confirm")
                && screen.contains("y allow once")
                && screen.contains("Esc cancel")
                && screen.contains("Ctrl+C interrupt"),
            "{screen}"
        );
        t.resize(80, 30).await;
        t.step().await;
        let screen = t.screen();
        assert!(
            screen.contains("Enter confirm")
                && screen.contains("y allow once")
                && screen.contains("Esc cancel")
                && screen.contains("Ctrl+C interrupt"),
            "{screen}"
        );
        t.vt.paste("unmatched pasted text");
        t.step().await;
        let screen = t.screen();
        assert!(
            screen.contains("Allow once") && !screen.contains("(no matches)"),
            "paste must not filter approval: {screen}"
        );
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.idle().await;
    })
    .await
}

#[tokio::test]
async fn which_key_displays_descriptions_and_cancel_key() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        exec(&t.vt.host, r#"mare.keymap.set('editor', '<C-x> d', 'toggle-expand')"#).await;
        t.step().await;
        t.ctrl('x');
        t.until("key continuation", |s| s.contains("toggle compact")).await;
        let screen = t.screen();
        assert!(
            screen.contains("Ctrl+X") && screen.contains("Esc cancel") && !screen.contains("toggle-expand"),
            "{screen}"
        );
    })
    .await
}

#[tokio::test]
async fn idle_hints_hide_shortcuts_while_typing() {
    local(async {
        let mut t = Term::new(json!([]), 160, 24).await;
        t.until("status line", |s| s.contains("READ")).await;
        assert!(t.screen().contains("? shortcuts") && t.screen().contains("Shift+Tab mode"), "{}", t.screen());
        t.vt.type_str("a draft");
        t.step().await;
        let bar = t.screen().lines().last().unwrap_or("").to_owned();
        assert!(!bar.contains("? shortcuts") && bar.contains("Shift+Tab mode"), "{bar}");
    })
    .await
}

#[tokio::test]
async fn compact_welcome_has_actionable_hints() {
    local(async {
        let mut t = Term::with(json!([]), json!({ "logo": "compact" }), 100, 24).await;
        t.until("compact welcome", |s| s.contains("mare") && s.contains("faux/echo")).await;
        let screen = t.screen();
        assert!(
            screen.contains("/model") && screen.contains("Shift+Tab mode") && screen.contains("? keys"),
            "{screen}"
        );
    })
    .await
}

#[tokio::test]
async fn background_hint_remains_visible_with_compact_working_look() {
    local(async {
        let mut t = Term::with(json!([]), json!({ "ui": { "working": { "preset": "kimi" } } }), 140, 24).await;
        exec(&t.vt.host, r#"mare.ui.working('Working', nil, { background = true })"#).await;
        t.until("working indicator", |s| s.contains("Working")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+B") && screen.contains("to run in background"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn plugin_transcript_does_not_advertise_unavailable_copy() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        exec(&t.vt.host, r#"mare.ui.transcript('Plugin view', mare.ui.Line('plugin content'))"#).await;
        t.until("plugin transcript", |s| s.contains("Plugin view") && s.contains("plugin content")).await;
        let screen = t.screen();
        assert!(!screen.lines().next().unwrap_or("").contains("y copy"), "{screen}");
    })
    .await
}

#[tokio::test]
async fn mcp_unknown_server_reports_an_error_notification() {
    local(async {
        let t = Term::new(json!([]), 100, 24).await;
        global(&t.vt.host, "COMMANDS", t.vt.host.service_table("commands").unwrap());
        let (ok, text, level): (bool, String, String) = eval(
            &t.vt.host,
            r#"local old = mare.ui.notify
               local text, level
               mare.ui.notify = function(t, l) text, level = t, l end
               local ok = pcall(function() COMMANDS:run('/mcp login absent-server') end)
               mare.ui.notify = old
               return ok, text or '', level or ''"#,
        )
        .await;
        assert!(ok && text == "no MCP server named absent-server" && level == "error", "{ok} {text:?} {level:?}");
    })
    .await
}

#[tokio::test]
async fn copy_picker_typing_filters_without_changing_preference() {
    local(async {
        let response = "Here are examples:\n```lua\nprint(1)\n```\n```sh\necho a\n```";
        let mut t = Term::new(json!([{ "text": response }]), 120, 30).await;
        t.submit("show examples").await;
        t.idle().await;
        t.submit("/copy").await;
        t.until("copy picker", |s| s.contains("Copy which part?")).await;
        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.step().await;
        let screen = t.screen();
        assert!(screen.contains("Copy which part?") && screen.contains("/ a"), "{screen}");
        assert!(!eval::<bool>(&t.vt.host, "return (mare.state.read() or {}).copy_always_full == true").await);
    })
    .await
}
