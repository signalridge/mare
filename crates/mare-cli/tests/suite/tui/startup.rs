//! Startup: input held back while the resume picker or project trust settles.

use super::*;

#[tokio::test]
async fn startup_input_stays_in_the_editor_until_the_resume_picker_settles() {
    local(async {
        let env = TestEnv::new().await;
        env.write_session(
            "2026-01-01T00-00-00_resume.jsonl",
            &[json!({ "type": "message", "entry_id": "old-1", "parent_entry_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": "earlier context" }] } })],
        );
        let mut opts = env.host().faux(json!([])).args(json!({ "resume": "" })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 80, 24).await;
        vt.type_str("typed early");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        let mut parser = vt100::Parser::new(nz(24), nz(80), 2000);
        pump_until(&mut vt, &mut parser, "resume picker did not open", |_, s| s.contains("Resume session")).await;
        assert_eq!(vt.turns_ended(), 0, "nothing is sent before the session is selected");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "startup did not finish", |vt, _| !vt.starting()).await;
        settle(&mut vt, &mut parser).await;
        assert_eq!(vt.turns_ended(), 0, "the refused prompt is not sent once startup is ready");
        assert_eq!(vt.editor_text(), "typed early", "the text stays in the input box");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "the prompt never ran", |vt, _| vt.turns_ended() > 0).await;
        global(&vt.host, "AG", vt.host.service_table("agent").unwrap());
        let messages: String = eval(&vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(messages.contains("earlier context") && messages.contains("typed early"), "{messages}");
        vt.host.shutdown().await;
    })
    .await
}

/// Enter while startup is still in progress (here: the trust question) sends nothing and holds
/// nothing (pi `handleStartupSubmit`): the text stays and a notice says why.
#[tokio::test]
async fn startup_input_is_not_sent_while_project_trust_is_undecided() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        std::fs::write(env.cwd.join(".mare/init.lua"), "mare.setup { notify = 'never' }").unwrap();
        let mut opts = env
            .host()
            .faux(json!([]))
            .config(json!({ "plugins": { "onboarding": { "plugins": false } } }))
            .args(json!({ "trust_pending": true }))
            .opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 80, 24).await;
        vt.type_str("typed while trusting");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        let mut parser = vt100::Parser::new(nz(24), nz(80), 2000);
        pump_until(&mut vt, &mut parser, "trust dialog did not open", |_, s| s.contains("Trust this folder?")).await;
        assert_eq!(vt.turns_ended(), 0, "prompt ran before trust was decided");
        vt.key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "startup did not finish", |vt, _| !vt.starting()).await;
        pump_until(&mut vt, &mut parser, "the notice", |_, s| s.contains("not sent: startup is still")).await;
        settle(&mut vt, &mut parser).await;
        assert_eq!(vt.turns_ended(), 0, "nothing is sent on its own once startup is ready");
        assert_eq!(vt.editor_text(), "typed while trusting");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "the prompt never ran", |vt, _| vt.turns_ended() > 0).await;
        global(&vt.host, "AG", vt.host.service_table("agent").unwrap());
        let messages: String = eval(&vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(messages.contains("typed while trusting"), "{messages}");
        vt.host.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn startup_input_is_not_sent_when_resume_fails() {
    local(async {
        let env = TestEnv::new().await;
        let mut opts = env.host().faux(json!([])).args(json!({ "continue": true })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 80, 24).await;
        vt.type_str("must not be sent");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        let mut parser = vt100::Parser::new(nz(24), nz(80), 2000);
        pump_until(&mut vt, &mut parser, "missing startup error", |_, s| s.contains("Startup blocked")).await;
        assert_eq!(vt.turns_ended(), 0, "prompt ran despite failed resume");
        assert_eq!(vt.editor_text(), "must not be sent", "the text stays in the input box");
        global(&vt.host, "AG", vt.host.service_table("agent").unwrap());
        let messages: String = eval(&vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(!messages.contains("must not be sent"), "{messages}");
        vt.host.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn startup_input_is_not_sent_when_trusted_project_config_fails() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        std::fs::write(env.cwd.join(".mare/init.lua"), "this is not valid lua!").unwrap();
        let mut opts = env.host().user_config(false).faux(json!([])).args(json!({ "trust_pending": true })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 80, 24).await;
        vt.type_str("must wait for trust");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        let mut parser = vt100::Parser::new(nz(24), nz(80), 2000);
        pump_until(&mut vt, &mut parser, "missing trust dialog", |_, s| s.contains("Trust this folder?")).await;
        vt.key(KeyEvent::new(KeyCode::Up, Modifiers::NONE));
        vt.key(KeyEvent::new(KeyCode::Up, Modifiers::NONE));
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "missing startup error", |_, s| s.contains("Startup blocked")).await;
        assert_eq!(vt.turns_ended(), 0, "prompt ran despite failed project config");
        assert_eq!(vt.editor_text(), "must wait for trust");
        vt.host.shutdown().await;
    })
    .await
}

/// A few more frames: anything that would start on its own has had the chance to.
async fn settle(vt: &mut Vt, parser: &mut vt100::Parser) {
    for _ in 0..10 {
        vt.frame().await;
        parser.process(&vt.output());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A read-only view (`-r` of a session another process holds) shows as a status-bar badge
/// plus one notice, and a turn in it raises no error card.
#[tokio::test]
async fn a_read_only_view_shows_a_badge_and_no_error_per_turn() {
    local(async {
        let mut lock = None;
        let script = json!([{ "text": "an answer" }]);
        let mut t = Term::build(script, json!({}), json!({ "resume": "s1" }), 160, 30, |env| {
            let p = env.write_session("2026-01-01T00-00-00_locked.jsonl", &[msg("m1", None, "user", "hello")]);
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(mare_store::lock_path(&p))
                .unwrap();
            f.try_lock().unwrap();
            lock = Some(f);
        })
        .await;
        t.until("the choice", |s| s.contains("View read-only")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the badge and the notice", |s| s.contains("read-only view") && s.contains("back up or repair")).await;
        assert!(t.screen().contains(" read-only "), "the status-bar badge: {}", t.screen());
        t.submit("go").await;
        t.idle().await;
        let screen = t.screen();
        assert!(screen.contains("an answer") && screen.contains(" read-only "), "{screen}");
        assert!(!screen.contains("Error") && !screen.contains("write failed"), "{screen}");
        drop(lock);
    })
    .await
}

/// `mare -r` with the picker cancelled starts a new session: nothing is blocked (a cancelled
/// picker is not a failed startup), and the text typed meanwhile is sent with Enter.
#[tokio::test]
async fn cancelled_resume_picker_starts_a_new_session() {
    local(async {
        let env = TestEnv::new().await;
        env.write_session(
            "2026-01-01T00-00-00_resume.jsonl",
            &[json!({ "type": "message", "entry_id": "old-1", "parent_entry_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": "earlier context" }] } })],
        );
        let mut opts = env.host().faux(json!([])).args(json!({ "resume": "" })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 80, 24).await;
        vt.type_str("after cancel");
        let mut parser = vt100::Parser::new(nz(24), nz(80), 2000);
        pump_until(&mut vt, &mut parser, "resume picker did not open", |_, s| s.contains("Resume session")).await;
        vt.key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "startup did not finish", |vt, _| !vt.starting()).await;
        assert!(!parser.screen().contents().contains("Startup blocked"), "{}", parser.screen().contents());
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "the prompt never ran", |vt, _| vt.turns_ended() > 0).await;
        global(&vt.host, "AG", vt.host.service_table("agent").unwrap());
        let messages: String = eval(&vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(messages.contains("after cancel") && !messages.contains("earlier context"), "{messages}");
        vt.host.shutdown().await;
    })
    .await
}

/// `mare -r` with no session to pick: a new session, input not blocked.
#[tokio::test]
async fn resume_picker_without_sessions_starts_a_new_session() {
    local(async {
        let env = TestEnv::new().await;
        let mut opts = env.host().faux(json!([])).args(json!({ "resume": "" })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 80, 24).await;
        let mut parser = vt100::Parser::new(nz(24), nz(80), 2000);
        pump_until(&mut vt, &mut parser, "startup did not finish", |vt, _| !vt.starting()).await;
        vt.type_str("nothing to resume");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "the prompt never ran", |vt, _| vt.turns_ended() > 0).await;
        assert!(!parser.screen().contents().contains("Startup blocked"), "{}", parser.screen().contents());
        assert!(crate_resume_error(&vt).await.is_none());
        vt.host.shutdown().await;
    })
    .await
}

/// A blocked startup never traps the input: slash commands still run, prompts are refused (the
/// text stays, the notice says what failed and how to fix it), and a `/new` that fixes what
/// blocked it (here `-c` with nothing to continue) sends nothing on its own (pi). Lifting the
/// block finishes startup: `ui/ready`, which the failure stopped before, is sent exactly once.
#[tokio::test]
async fn blocked_startup_runs_slash_commands_and_new_unblocks_it() {
    local(async {
        let env = TestEnv::new().await;
        let mut opts = env.host().faux(json!([])).args(json!({ "continue": true })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 80, 24).await;
        let mut parser = vt100::Parser::new(nz(24), nz(80), 2000);
        pump_until(&mut vt, &mut parser, "missing startup error", |_, s| s.contains("Startup blocked")).await;
        vt.type_str("refused prompt");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        settle(&mut vt, &mut parser).await;
        assert_eq!(vt.turns_ended(), 0, "prompt ran despite failed resume");
        assert_eq!(vt.editor_text(), "refused prompt", "the text stays in the input box");
        assert!(
            parser.screen().contents().contains("prompts are not sent until then"),
            "{}",
            parser.screen().contents()
        );
        assert!(crate_resume_error(&vt).await.is_some());
        expose(&vt.host);
        exec(&vt.host, "UI_READY = 0; C.cordis:on('ui/ready', function() UI_READY = UI_READY + 1 end)").await;
        vt.key(KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL));
        vt.type_str("/new");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "/new did not lift the block", |_, s| s.contains("Startup unblocked")).await;
        assert!(crate_resume_error(&vt).await.is_none());
        settle(&mut vt, &mut parser).await;
        assert_eq!(vt.turns_ended(), 0, "nothing is sent when the block lifts");
        vt.type_str("after new");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "the prompt never ran", |vt, _| vt.turns_ended() > 0).await;
        assert_eq!(eval::<i64>(&vt.host, "return UI_READY").await, 1, "ui/ready once the block lifted");
        global(&vt.host, "AG", vt.host.service_table("agent").unwrap());
        let messages: String = eval(&vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(messages.contains("after new") && !messages.contains("refused prompt"), "{messages}");
        vt.host.shutdown().await;
    })
    .await
}

/// The welcome banner is in the transcript exactly once and shows the model: it is drawn before
/// the provider plugins register, and `trust_default = "trust"` re-runs init.lua (reloading
/// plugins) before `app/ready`.
#[tokio::test]
async fn the_welcome_banner_is_shown_once_with_the_model() {
    local(async {
        let mut env = TestEnv::new().await;
        env.color_terminal();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.setup { model = 'faux/echo', trust_default = 'trust' }\nmare.plugin('provider-faux', {})",
        )
        .unwrap();
        let mut opts = env.host().user_config(false).args(json!({ "trust_pending": true })).opts;
        opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, opts, 100, 40).await;
        let mut t = Term { owned: vt, parser: vt100::Parser::new(nz(40), nz(100), 2000), ended: 0 };
        while t.vt.starting() {
            t.step().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let all = t.history();
        assert_eq!(all.matches("/model · /help · @ file · ! bash").count(), 1, "one banner:\n{all}");
        assert!(!all.contains("no model — /login"), "the banner shows the model:\n{all}");
        assert!(all.contains("faux/echo"), "the selected model is in the banner:\n{all}");
    })
    .await
}

/// `/new` clears the previous session's screen/scrollback and renders the same welcome banner
/// (pixel logo, version, cwd, model, key hints) as a fresh startup.
#[tokio::test]
async fn new_session_clears_screen_and_redraws_startup_welcome_banner() {
    local(async {
        let mut env = TestEnv::new().await;
        env.color_terminal();
        let script = json!([{ "text": "reply before new" }, { "text": "reply after new" }]);
        let mut opts = env.host().faux(script).opts;
        opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, opts, 100, 40).await;
        let mut t = Term { owned: vt, parser: vt100::Parser::new(nz(40), nz(100), 2000), ended: 0 };
        t.until("startup welcome banner", |s| {
            s.contains("/model · /help · @ file · ! bash") && s.contains("faux/echo") && s.contains('▀')
        })
        .await;
        let startup_banner = t.screen();
        assert!(startup_banner.contains(concat!("mare ", env!("CARGO_PKG_VERSION"))), "{startup_banner}");

        t.submit("hello before new").await;
        t.until("first turn reply", |s| s.contains("reply before new")).await;
        t.idle().await;

        t.submit("/new").await;
        t.until("fresh welcome banner after /new", |s| {
            !s.contains("reply before new")
                && s.contains("/model · /help · @ file · ! bash")
                && s.contains("faux/echo")
                && s.contains('▀')
        })
        .await;
        let after_new = t.history();
        assert!(!after_new.contains("hello before new"), "old prompt should be cleared:\n{after_new}");
        assert!(!after_new.contains("reply before new"), "old reply should be cleared:\n{after_new}");
        assert!(after_new.contains(concat!("mare ", env!("CARGO_PKG_VERSION"))), "{after_new}");
        assert_eq!(after_new.matches("/model · /help · @ file · ! bash").count(), 1, "one banner:\n{after_new}");

        t.submit("hello after new").await;
        t.until("reply in new session", |s| s.contains("reply after new")).await;
        t.idle().await;

        // `/archive` takes the same `session/new` path: clears prior turns, keeps the fresh
        // welcome banner, and shows the archive notice below it.
        t.submit("/archive").await;
        t.until("fresh welcome banner and archive notice after /archive", |s| {
            !s.contains("reply after new")
                && s.contains("Archived ")
                && s.contains("/model · /help · @ file · ! bash")
                && s.contains("faux/echo")
                && s.contains('▀')
        })
        .await;
        let after_archive = t.history();
        assert!(!after_archive.contains("hello after new"), "old prompt should be cleared:\n{after_archive}");
        assert_eq!(
            after_archive.matches("/model · /help · @ file · ! bash").count(),
            1,
            "one banner:\n{after_archive}"
        );
    })
    .await
}

/// `/new` from a resumed session (`-c`) switches to the full startup banner (not stuck in compact
/// resume mode), and `logo = false` still shows a minimal new-session divider without a logo.
#[tokio::test]
async fn new_session_respects_logo_config_and_lifts_resumed_compact_mode() {
    // 1. Resumed session (`continue = true`) starts with compact banner; `/new` renders full logo.
    local(async {
        let mut env = TestEnv::new().await;
        env.color_terminal();
        let h = env.host().faux(json!([{ "text": "saved reply" }])).build().await;
        h.prompt("saved prompt", json!({})).await.unwrap();
        h.shutdown().await;

        let mut opts = env.host().faux(json!([])).args(json!({ "continue": true })).opts;
        opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, opts, 100, 40).await;
        let mut t = Term { owned: vt, parser: vt100::Parser::new(nz(40), nz(100), 2000), ended: 0 };
        t.until("resumed session history", |s| s.contains("saved reply")).await;
        assert!(!t.history().contains('▀'), "resumed startup must use compact banner");

        t.submit("/new").await;
        t.until("full welcome banner after /new from resumed", |s| {
            !s.contains("saved reply")
                && s.contains('▀')
                && s.contains("/model · /help · @ file · ! bash")
                && s.contains("faux/echo")
        })
        .await;
        assert_eq!(t.history().matches("/model · /help · @ file · ! bash").count(), 1);
    })
    .await;

    // 2. With `logo = false`, `/new` clears the screen and shows only the minimal divider.
    local(async {
        let mut t2 = Term::with(json!([{ "text": "bye" }]), json!({ "logo": false }), 80, 24).await;
        t2.submit("hi").await;
        t2.until("turn reply", |s| s.contains("bye")).await;
        t2.idle().await;
        t2.submit("/new").await;
        t2.until("new session divider without logo", |s| !s.contains("bye") && s.contains("⎯ New session ⎯")).await;
        assert!(!t2.history().contains('▀') && !t2.history().contains("/model · /help"));
    })
    .await;
}

/// If `/new` runs before `app/ready` (for example, while startup is blocked on a failing trusted
/// `init.lua`), the freshly drawn banner marks `ui.welcomed` so a subsequent `/reload` (reloading
/// UI plugins) does not push a duplicate welcome banner.
#[tokio::test]
async fn new_session_before_app_ready_followed_by_reload_draws_single_banner() {
    local(async {
        let mut env = TestEnv::new().await;
        env.color_terminal();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let init_lua = env.config_dir().join("init.lua");
        std::fs::write(&init_lua, "error('BROKEN trust init')").unwrap();
        let mut opts = env
            .host()
            .user_config(false)
            .faux(json!([]))
            .config(json!({ "trust_default": "trust" }))
            .args(json!({ "trust_pending": true }))
            .opts;
        opts.mode = Mode::Tui;
        let vt = OwnedVt::new(env, opts, 100, 40).await;
        let mut t = Term { owned: vt, parser: vt100::Parser::new(nz(40), nz(100), 2000), ended: 0 };
        t.until("blocked startup before app/ready", |s| s.contains("Startup blocked")).await;
        expose(&t.vt.host);
        assert!(
            !eval::<bool>(&t.vt.host, "return mare.g['ui.welcomed'] == true").await,
            "app/ready must not have set ui.welcomed while trust init failed"
        );

        t.submit("/new").await;
        t.until("fresh welcome banner after /new while startup blocked", |s| {
            !s.contains("Startup blocked") && s.contains("/model · /help · @ file · ! bash") && s.contains('▀')
        })
        .await;
        assert!(
            eval::<bool>(&t.vt.host, "return mare.g['ui.welcomed'] == true").await,
            "/new must mark ui.welcomed so subsequent UI reloads do not duplicate the banner"
        );

        std::fs::write(&init_lua, "mare.setup { model = 'faux/echo' }\nmare.plugin('provider-faux', {})").unwrap();
        t.submit("/reload").await;
        t.until("reload completes after fixing init.lua", |s| {
            s.contains("Startup unblocked") || s.contains("Reloaded")
        })
        .await;
        for _ in 0..5 {
            t.step().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let all = t.history();
        assert_eq!(
            all.matches("/model · /help · @ file · ! bash").count(),
            1,
            "exactly one welcome banner after pre-ready /new + /reload:\n{all}"
        );
    })
    .await
}

/// A startup refusal can remain in scrollback while a retry Enter is already completing a
/// different, config-blocked request. A channel holds that request's real `turn/completed` boundary.
#[tokio::test]
async fn a_startup_refusal_is_not_the_retried_config_refusal() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), "error('BROKEN config line')").unwrap();
        let mut opts = env
            .host()
            .user_config(false)
            .faux(json!([]))
            .config(json!({ "logo": false, "plugins": { "onboarding": { "plugins": false } } }))
            .opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 100, 30).await;
        expose(&vt.host);
        exec(
            &vt.host,
            r#"
            END_ENTERED_TX, END_ENTERED_RX = mare.task.channel()
            END_RELEASE_TX, END_RELEASE_RX = mare.task.channel()
            C.cordis:on("turn/completed", function(res)
                if res.not_sent then
                    assert(END_ENTERED_TX:send(true))
                    assert(END_RELEASE_RX:recv())
                end
            end, { prepend = true })
        "#,
        )
        .await;
        vt.type_str("hello");
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        let mut parser = vt100::Parser::new(nz(30), nz(100), 2000);
        pump_until(&mut vt, &mut parser, "startup and its refusal", |vt, s| {
            !vt.starting() && s.contains("not sent: startup is still")
        })
        .await;
        assert_eq!(vt.editor_text(), "hello");
        // This is p.prompt's retry Enter after startup finishes, while its old notice is
        // still sufficient for the previous contains("not sent:") predicate.
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        assert!(
            tokio::time::timeout(WAIT, eval::<bool>(&vt.host, "return END_ENTERED_RX:recv()"))
                .await
                .expect("the retried request did not reach turn/completed")
        );
        vt.frame().await;
        parser.process(&vt.output());
        let stale = parser.screen().contents();
        assert!(stale.contains("not sent:"), "the previous observation was already satisfied");
        assert!(!crate::suite::tui_pty::config_refusal_restored(&stale));
        assert!(vt.busy(), "the new request has not published its end state");
        assert_eq!(vt.editor_text(), "");
        // The current config refusal is held by a channel, so the old startup notice cannot
        // let the fixture start its quit keys. Releasing it publishes the actual restored state.
        exec(&vt.host, "assert(END_RELEASE_TX:send(true))").await;
        pump_until(&mut vt, &mut parser, "the config refusal and restored draft", |_, s| {
            crate::suite::tui_pty::config_refusal_restored(s)
        })
        .await;
        assert!(!vt.busy());
        assert_eq!(vt.editor_text(), "hello");
        // The unchanged three-key contract: clear the restored draft, arm, then quit.
        vt.key(KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL));
        vt.key(KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL));
        vt.key(KeyEvent::new(KeyCode::Char('c'), Modifiers::CONTROL));
        assert!(vt.quit_requested(), "the original three presses did not quit");
    })
    .await;
}

/// A real TUI start, through `ui/ready`, leaves the built-ins' cold submodules unloaded: they
/// load on first use, not to draw the first frame.
#[tokio::test]
async fn tui_startup_leaves_cold_builtin_submodules_unloaded() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().faux(json!([])).no_session().config(json!({
            "logo": false,
            "plugins": {
                "permissions": { "enabled": true },
                "mcp": { "enabled": true },
                "onboarding": { "plugins": false }
            }
        }));
        b.opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, b.opts, 120, 16).await;
        let mut parser = vt100::Parser::new(nz(16), nz(120), 2000);
        vt.first_frame().await;
        parser.process(&vt.output());
        pump_until(&mut vt, &mut parser, "startup and ui/ready settled", |vt, _| vt.startup_finished()).await;
        let cold: &[(&str, &[&str])] = &[
            ("agent", &["agent.persist", "agent.tool_exec", "agent.inputs", "agent.step", "agent.loop"]),
            ("permissions", &["permissions.argv", "permissions.rules", "permissions.sensitive", "permissions.vote"]),
            ("commands", &["commands.availability"]),
            ("context", &["context.instructions", "context.report"]),
            ("mcp", &["mcp.results", "mcp.elicit", "mcp.prompts", "mcp.actions", "mcp.catalog"]),
        ];
        let mut loaded = Vec::new();
        for (owner, modules) in cold {
            for module in *modules {
                if vt.host.kernel.is_module_loaded(owner, module) {
                    loaded.push(*module);
                }
            }
        }
        assert!(loaded.is_empty(), "cold modules loaded by a TUI start: {loaded:?}");
        vt.shutdown().await;
    })
    .await
}

/// Interactive TUI startup defers restoring Lua's generational GC `minormajor` threshold until
/// after the first frame is drawn, whereas non-TUI hosts restore it immediately before returning.
#[tokio::test]
async fn tui_startup_defers_gc_threshold_restore_until_after_first_frame() {
    local(async {
        let gc_param = r#"
            local v = collectgarbage("param", "minormajor", 0)
            collectgarbage("param", "minormajor", v)
            return v
        "#;

        // Headless host restores default minormajor (68) before Host::new returns.
        {
            let env = TestEnv::new().await;
            let h = env.host().faux(json!([])).no_session().build().await;
            let val: i64 = eval(&h, gc_param).await;
            assert_eq!(val, 68, "non-TUI host must restore default GC minormajor before returning");
            h.shutdown().await;
        }

        // TUI host retains startup minormajor (1000) through the first frame and restores 68 once
        // post-first-frame startup runs.
        {
            let env = TestEnv::new().await;
            let mut b = env.host().faux(json!([])).no_session().config(json!({
                "logo": false,
                "plugins": { "onboarding": { "plugins": false } }
            }));
            b.opts.mode = Mode::Tui;
            let mut vt = OwnedVt::new(env, b.opts, 120, 16).await;
            let mut parser = vt100::Parser::new(nz(16), nz(120), 2000);
            vt.first_frame().await;
            parser.process(&vt.output());
            let before_ready: i64 = eval(&vt.host, gc_param).await;
            assert_eq!(before_ready, 1000, "TUI host must keep startup GC threshold across first frame");

            pump_until(&mut vt, &mut parser, "startup and ui/ready settled", |vt, _| vt.startup_finished()).await;
            let after_ready: i64 = eval(&vt.host, gc_param).await;
            assert_eq!(after_ready, 68, "TUI host must restore default GC minormajor after first frame");
            vt.shutdown().await;
        }
    })
    .await
}
