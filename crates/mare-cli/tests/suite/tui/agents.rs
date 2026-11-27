//! The agents screen: this
//! session's sub-agents, the live view of one, ← on an empty input, the Peers tab.

use super::*;

/// A sub-agent of this session: a sub-session file (`session:child`) with one message.
async fn child(t: &Term) {
    expose(&t.vt.host);
    exec(
        &t.vt.host,
        r#"CH = S:child({ run_id = "r1", seq = 1, label = "explorer" })
           function SAY(text)
             CH.append({ type = "message", message = { role = "assistant", content = { { type = "text", text = text } } } })
           end
           SAY("mapping the crate")"#,
    )
    .await;
}

/// /agents lists the sub-agent; Enter opens its conversation read-only and live (what it writes
/// next shows up); Esc in that view comes back to the list, not to the main screen.
#[tokio::test]
async fn agents_lists_a_sub_agent_and_enter_opens_its_live_view() {
    local(async {
        let config = json!({ "plugins": { "session": { "transcript_poll_ms": 20 } } });
        let mut t = Term::with(json!([]), config, 80, 24).await;
        child(&t).await;
        t.submit("/agents").await;
        t.until("the list", |s| s.contains("Agents") && s.contains("explorer") && s.contains("running")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("its conversation", |s| s.contains("mapping the crate")).await;
        // the header names the view's own keys from the keymap (`ui.transcript(…, { keys })`)
        t.until("the view's keys", |s| s.contains("Esc list · Alt+←/Alt+→ agent · Ctrl+T close")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("Escape returns to the agents list", |s| s.contains("sessions (/resume)") && s.contains("explorer"))
            .await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return mare.ui.transcript_title()").await, None);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the agent view reopens", |s| s.contains("mapping the crate") && s.contains("Esc list")).await;
        exec(&t.vt.host, r#"mare.keymap.set("transcript", "<Esc>", false); mare.keymap.set("transcript", "<C-b>", "agents.back")"#).await;
        t.until("a rebind follows", |s| s.contains("Ctrl+B list")).await;
        exec(&t.vt.host, r#"SAY("found the hint row")"#).await;
        t.until("followed live", |s| s.contains("found the hint row")).await;
        // Alt+→ steps to the next sub-agent of the list, Alt+← back
        exec(
            &t.vt.host,
            r#"local c = S:child({ run_id = "r1", seq = 2, label = "reviewer" })
               c.append({ type = "message", message = { role = "assistant", content = { { type = "text", text = "reviewing it" } } } })"#,
        )
        .await;
        t.key(KeyCode::Right, Modifiers::ALT);
        t.until("the next one", |s| s.contains("reviewing it") && !s.contains("mapping the crate")).await;
        t.key(KeyCode::Left, Modifiers::ALT);
        t.until("back to the first", |s| s.contains("mapping the crate")).await;
        t.ctrl('b');
        // the list is open again (its key hints), the view closed
        t.until("back on the list", |s| s.contains("sessions (/resume)") && !s.contains("Esc list")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return mare.ui.transcript_title()").await, None);
        // and Esc on the list closes it
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the main screen", |s| !s.contains("sessions (/resume)")).await;
    })
    .await
}

/// ← on an empty input opens /agents (the agents plugin's default binding, `when =
/// "editor_empty"`); with text in the input it stays the cursor movement.
#[tokio::test]
async fn left_on_an_empty_input_opens_the_agents_list() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        child(&t).await;
        t.step().await;
        t.key(KeyCode::Left, Modifiers::NONE);
        t.until("the agents list", |s| s.contains("sessions (/resume)") && s.contains("explorer")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the main screen", |s| !s.contains("sessions (/resume)")).await;
        t.vt.type_str("ab");
        t.key(KeyCode::Left, Modifiers::NONE);
        t.vt.type_str("X");
        assert_eq!(t.vt.editor_text(), "aXb", "with text: the cursor moved");
    })
    .await
}

/// The Peers tab lists the other mare sessions of the peer registry with what their plugins
/// published: the messages held there and the session open there.
#[tokio::test]
async fn the_peers_tab_shows_a_registered_session() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        let dir: String = eval(&t.vt.host, "return mare.peers.dir()").await;
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        let entry = json!({
            "name": "slipway-9c0d", "pid": pid, "cwd": "/work/slipway", "socket": "", "mode": "tui",
            "started": 0, "status": { "state": "idle", "message": "" },
            "plugins": { "status": { "held": 2 }, "session": { "session_id": "0199a3c2-aaaa", "name": "port the sandbox" } }
        });
        std::fs::write(std::path::Path::new(&dir).join(format!("{pid}.json")), entry.to_string()).unwrap();
        t.submit("/agents").await;
        t.until("the peer", |s| s.contains("slipway-9c0d") && s.contains("2 held")).await;
        t.until("its session in the preview", |s| s.contains("port the sandbox")).await;
    })
    .await
}

/// Typing a search in a plugin transcript view (agents sub-view) and pressing Esc clears the
/// search without leaving the view (Codex `transcript_view/input.rs:265-267`,
/// `pager_overlay/transcript_tests.rs:384`), and a subsequent Esc returns to the agents list.
#[tokio::test]
async fn esc_while_typing_search_in_agent_view_clears_search_without_closing_view() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        child(&t).await;
        t.submit("/agents").await;
        t.until("the list", |s| s.contains("Agents") && s.contains("explorer")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("its conversation", |s| s.contains("mapping the crate")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("map");
        t.until("search prompt visible", |s| s.contains("/map")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("search prompt gone", |s| !s.contains("/map")).await;
        assert!(
            t.screen().contains("mapping the crate") && t.screen().contains("Esc list"),
            "Esc while typing search must clear search without leaving the view:\n{}",
            t.screen()
        );
        let title = eval::<Option<String>>(&t.vt.host, "return mare.ui.transcript_title()").await;
        assert!(title.as_deref().is_some_and(|s| s.starts_with("explorer")), "unexpected title: {title:?}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back on agents list", |s| s.contains("sessions (/resume)")).await;
    })
    .await
}

/// `/agents` shows the side-by-side preview pane at >= 84 cols, omits it at 83 cols, and
/// `Ctrl+G` jumps from `/agents` to `/resume`.
#[tokio::test]
async fn agents_preview_threshold_and_ctrl_g_jump_to_sessions() {
    local(async {
        // 84 cols: preview pane shows the sub-agent's live transcript snippet.
        let mut wide = Term::new(json!([{ "text": "saved reply" }]), 84, 24).await;
        wide.submit("saved prompt").await;
        wide.until("saved reply", |s| s.contains("saved reply")).await;
        child(&wide).await;
        wide.submit("/agents").await;
        wide.until("wide agents list with preview", |s| {
            s.contains("explorer") && s.contains("mapping the crate") && s.contains("started")
        })
        .await;
        // Ctrl+G jumps to the sessions picker (/resume).
        wide.ctrl('g');
        wide.until("ctrl+g opened sessions", |s| {
            !s.contains("sessions (/resume)") && (s.contains("saved prompt") || s.contains("Resume"))
        })
        .await;
        drop(wide);

        // 83 cols (< preview_min_width 84): list stays visible without the preview pane.
        let mut narrow = Term::new(json!([]), 83, 24).await;
        child(&narrow).await;
        narrow.submit("/agents").await;
        narrow.until("narrow agents list", |s| s.contains("Agents") && s.contains("explorer")).await;
        assert!(
            !narrow.screen().contains("started"),
            "width 83 (<84 cols) should not render side-by-side preview pane:\n{}",
            narrow.screen()
        );
    })
    .await
}

/// `/agents` header right-hand counts distinguish waiting vs running members alongside total
/// agents, peer sessions, and held approvals (`N agents · M waiting · R running · K sessions · J held`).
#[tokio::test]
async fn agents_header_shows_spec_counts_format() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        child(&t).await;
        let dir: String = eval(&t.vt.host, "return mare.peers.dir()").await;
        std::fs::create_dir_all(&dir).unwrap();
        let pid = std::process::id();
        let entry = json!({
            "name": "peer-one", "pid": pid, "cwd": "/work/peer", "socket": "", "mode": "tui",
            "started": 0, "status": { "state": "idle", "message": "" },
            "plugins": { "status": { "held": 2 }, "session": { "session_id": "s-peer", "name": "peer work" } }
        });
        std::fs::write(std::path::Path::new(&dir).join(format!("{pid}.json")), entry.to_string()).unwrap();
        t.submit("/agents").await;
        t.until("the agents header", |s| s.contains("Agents") && s.contains("explorer") && s.contains("2 held")).await;
        let screen = t.screen();
        assert!(
            screen.contains("1 agent · 1 running · 1 session · 2 held"),
            "expected SPEC header counts `1 agent · 1 running · 1 session · 2 held`, got:\n{screen}"
        );
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("agents modal closed", |s| !s.contains("sessions (/resume)")).await;

        // Add a finished sub-agent (excluded from running/waiting) and one waiting for approval.
        exec(
            &t.vt.host,
            r#"local done_ch = S:child({ run_id = "r1", seq = 2, label = "finished-worker" })
               done_ch.append({ type = "state", state = "closed", ok = true })"#,
        )
        .await;
        t.vt.host.emit_json("agent/approval-queue", json!({ "pending": [{ "label": "blocked-worker" }] })).await;
        t.submit("/agents").await;
        t.until("waiting + running header", |s| {
            s.contains("Agents") && s.contains("blocked-worker") && s.contains("finished-worker")
        })
        .await;
        let screen = t.screen();
        assert!(
            screen.contains("3 agents · 1 waiting · 1 running"),
            "expected header counts distinguishing waiting vs running (`3 agents · 1 waiting · 1 running`), got:\n{screen}"
        );
    })
    .await
}

/// Reopening the same sub-agent transcript within one poll interval (`< transcript_poll_ms`)
/// and then switching to another sub-agent must cancel the superseded pollers so a late poll
/// tick for the first sub-agent cannot enqueue a stale `UiCmd::TranscriptView` behind the switch
/// and pull the screen back to the first sub-agent.
#[tokio::test]
async fn reopening_same_transcript_and_switching_does_not_revert_to_stale_poller() {
    local(async {
        let config = json!({ "plugins": { "session": { "transcript_poll_ms": 20 } } });
        let mut t = Term::with(json!([]), config, 80, 24).await;
        child(&t).await;
        t.submit("/agents").await;
        t.until("the list", |s| s.contains("Agents") && s.contains("explorer")).await;
        // Open explorer (poller #1), close and reopen same title within < 20ms (poller #2).
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("first open", |s| s.contains("mapping the crate")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("back to list", |s| s.contains("sessions (/resume)") && s.contains("explorer")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("second open", |s| s.contains("mapping the crate")).await;

        // Append to explorer, create reviewer, switch to reviewer, and let > 2 poll intervals
        // elapse before draining UiCmd so `ui.transcript_title()` still reports explorer while
        // any uncancelled explorer poller wakes up.
        exec(
            &t.vt.host,
            r#"SAY("stale explorer tail")
               local rev = S:child({ run_id = "r1", seq = 2, label = "reviewer" })
               rev.append({ type = "message", message = { role = "assistant", content = { { type = "text", text = "reviewing it" } } } })
               S:load_transcript(rev.path, "reviewer · r1 #2")
               -- sleep-ok: let any uncancelled 20ms explorer poller wake while UiCmd for reviewer is still queued
               mare.task.sleep(60)"#,
        )
        .await;
        t.step().await;
        let screen = t.screen();
        assert!(
            screen.contains("reviewing it")
                && !screen.contains("mapping the crate")
                && !screen.contains("stale explorer tail"),
            "stale explorer poller pulled view back after switch:\n{screen}"
        );
        assert_eq!(
            eval::<Option<String>>(&t.vt.host, "return mare.ui.transcript_title()").await.as_deref(),
            Some("reviewer · r1 #2")
        );
    })
    .await
}

/// Closing a sub-agent transcript view (Esc) while its background poller is mid-read inside
/// `mare.store.read` must not reopen the view once the read completes.
#[tokio::test]
async fn closing_transcript_while_poll_read_is_inflight_does_not_reopen_view() {
    local(async {
        let config = json!({ "plugins": { "session": { "transcript_poll_ms": 20 } } });
        let mut t = Term::with(json!([]), config, 80, 24).await;
        child(&t).await;
        t.submit("/agents").await;
        t.until("the list", |s| s.contains("Agents") && s.contains("explorer")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("transcript opened", |s| s.contains("mapping the crate")).await;

        // Pause the next background poll read of CH.path, then append to trigger the poller.
        exec(
            &t.vt.host,
            r#"
            local orig_read = mare.store.read
            READ_STARTED_TX, READ_STARTED_RX = mare.task.channel()
            READ_RELEASE_TX, READ_RELEASE_RX = mare.task.channel()
            READ_DONE_TX, READ_DONE_RX = mare.task.channel()
            local armed = true
            mare.store.read = function(p, o)
                if armed and p == CH.path then
                    armed = false
                    READ_STARTED_TX:send(true)
                    READ_RELEASE_RX:recv()
                    local res = { orig_read(p, o) }
                    mare.store.read = orig_read
                    READ_DONE_TX:send(true)
                    return table.unpack(res)
                end
                return orig_read(p, o)
            end
            SAY("late poll payload")
            "#,
        )
        .await;

        // Wait until the poller coroutine has entered `mare.store.read` inside `show(true)`.
        eval::<bool>(&t.vt.host, "return READ_STARTED_RX:recv() == true").await;

        // User closes the transcript view while the background read is still in flight.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("returned to agents list", |s| s.contains("sessions (/resume)") && s.contains("explorer")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return mare.ui.transcript_title()").await, None);

        // Release the in-flight read and process any resulting UiCmds.
        eval::<bool>(&t.vt.host, "READ_RELEASE_TX:send(true); return READ_DONE_RX:recv() == true").await;
        t.step().await;
        t.step().await;

        assert_eq!(
            eval::<Option<String>>(&t.vt.host, "return mare.ui.transcript_title()").await,
            None,
            "completed background poll read must not reopen a closed transcript view"
        );
        let screen = t.screen();
        assert!(
            screen.contains("sessions (/resume)") && !screen.contains("Esc list"),
            "closed transcript view must stay on agents list and not reopen transcript:\n{screen}"
        );
    })
    .await
}
