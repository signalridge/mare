//! Resumed and replayed sessions, and the resume picker.

use super::*;

/// Day headings in the /resume picker are shown but never selected: the cursor starts on the
/// first session and moving skips them; Enter resumes.
#[tokio::test]
async fn resume_picker_headings_are_never_selected() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        write_two_sessions(&t._env);
        t.submit("/resume").await;
        // The picker may open on an index the startup reconcile hasn't finished (only `sa`, the
        // cursor on it) and then refresh in place, keeping the cursor on `sa` ("3 of 3"). Wait for
        // both sessions, then start from the first one: Down from the last wraps past the heading.
        t.until("both sessions listed", |s| s.contains("fix the flaky test") && s.contains("refactor the parser"))
            .await;
        // The in-place refresh can land after this check and move the cursor back to `sa`, so keep
        // nudging Down (at most once per 300ms, so an unprocessed press isn't doubled) until the
        // cursor rests on the first session.
        let last = std::cell::Cell::new(None::<Instant>);
        t.until_vt("the picker on the first session", |t| {
            let s = t.screen();
            if s.contains("2 of 3") {
                return true;
            }
            if s.contains("3 of 3") && last.get().is_none_or(|at| at.elapsed() > Duration::from_millis(300)) {
                t.key(KeyCode::Down, Modifiers::NONE);
                last.set(Some(Instant::now()));
            }
            false
        })
        .await;
        t.key(KeyCode::Up, Modifiers::NONE);
        t.until("wrapped past the heading", |s| s.contains("3 of 3")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.until("over the heading again", |s| s.contains("2 of 3")).await;
        t.key(KeyCode::Char('k'), Modifiers::NONE);
        t.until("k wraps past the heading", |s| s.contains("3 of 3")).await;
        t.key(KeyCode::Char('j'), Modifiers::NONE);
        t.until("j wraps back over the heading", |s| s.contains("2 of 3")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("resumed", |s| !s.contains(" of 3") && s.contains("Ask mare")).await;
    })
    .await
}

/// An unsupported version remains visible in the real /resume picker, with its path and delete
/// hint; filtering to it cannot select it or close the picker (Codex's action path is session-only,
/// `codex-rs/tui/src/resume_picker.rs:1139-1165`). Supported sessions still resume as usual.
#[tokio::test]
async fn resume_picker_keeps_unsupported_sessions_visible_but_unselectable() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({}), 160, 30, |env| {
            write_two_sessions(env);
            let bad = env
                .write_session("2026-01-03T00-00-00_future.jsonl", &[msg("future-1", None, "user", "future session")]);
            let body = std::fs::read_to_string(&bad).unwrap().replacen(
                &format!("\"version\":{}", mare_store::CURRENT_VERSION),
                "\"version\":99",
                1,
            );
            std::fs::write(bad, body).unwrap();
        })
        .await;
        let bad = t
            ._env
            .home
            .path()
            .join("state/sessions")
            .join(mare_store::cwd_slug(&t._env.cwd))
            .join("2026-01-03T00-00-00_future.jsonl");
        let bad_path = bad.to_string_lossy().into_owned();
        let normalized = |path: &str| path.replace('\\', "/");
        let path_is_visible = |screen: &str| {
            let left = screen.lines().filter_map(|line| line.split('│').nth(1)).map(str::trim).collect::<String>();
            normalized(&left).contains(&normalized(&bad_path))
        };

        t.submit("/resume").await;
        t.until("valid sessions and the unsupported diagnostic", |s| {
            s.contains("refactor the parser")
                && s.contains("fix the flaky test")
                && path_is_visible(s)
                && s.contains("Version unsupported")
                && s.contains("/delete <path>")
        })
        .await;

        t.vt.type_str("future.jsonl");
        t.until("only the unsupported row", |s| s.contains("future.jsonl") && !s.contains("refactor the parser")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("Enter leaves the disabled row visible", |s| {
            s.contains("Resume session") && s.contains("future.jsonl")
        })
        .await;

        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the supported sessions return", |s| {
            s.contains("refactor the parser") && s.contains("fix the flaky test") && path_is_visible(s)
        })
        .await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("a supported session resumes", |s| !s.contains(" of 4") && s.contains("fix the flaky test")).await;
        assert!(bad.exists(), "opening or selecting the diagnostic never modifies its file");
    })
    .await
}

#[tokio::test]
async fn resume_lists_sessions_and_filters_by_text() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        write_two_sessions(&t._env);
        t.submit("/resume").await;
        t.until("the resume picker", |s| s.contains("refactor the parser") && s.contains("fix the flaky test")).await;
        t.vt.type_str("flaky");
        t.until("the filtered list", |s| s.contains("fix the flaky test") && !s.contains("refactor the parser")).await;
        insta::assert_snapshot!("resume_filtered", t.snap().await);
    })
    .await
}

/// Pasting a search query into `/resume` while highlighted on the last row must clamp `selected`
/// to the filtered items via `finish_select_key` so Enter immediately switches to the match.
#[tokio::test]
async fn resume_picker_paste_filter_clamps_selection_and_enter_switches_session() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        write_two_sessions(&t._env);
        t.submit("/resume").await;
        t.until("both sessions listed", |s| s.contains("fix the flaky test") && s.contains("refactor the parser"))
            .await;
        let last = std::cell::Cell::new(None::<Instant>);
        t.until_vt("picker moved to last row (3 of 3)", |t| {
            let s = t.screen();
            if s.contains("3 of 3") {
                return true;
            }
            if s.contains("2 of 3") && last.get().is_none_or(|at| at.elapsed() > Duration::from_millis(300)) {
                t.key(KeyCode::Down, Modifiers::NONE);
                last.set(Some(Instant::now()));
            }
            false
        })
        .await;
        t.vt.paste("flaky");
        t.until("pasted filter shows only matching session", |s| {
            s.contains("fix the flaky test") && !s.contains("refactor the parser") && s.contains("1 of 1")
        })
        .await;
        // sleep-ok: wait for PasteGuard's 150ms post-paste Enter suppression window before single Enter
        tokio::time::sleep(Duration::from_millis(200)).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("switched to pasted filtered session", |s| !s.contains(" of 1") && s.contains("fix the flaky test"))
            .await;
    })
    .await
}

/// "界面上不显示这条内部标记": replaying a session (resume, /tree) shows `!cmd`
/// messages as commands, notifications as one-line notices, and never `<turn_aborted>` — an
/// interrupted turn reads "⎿ Interrupted" as it did live, once, whether it was cut off mid-answer
/// or mid-tool (no aborted answer in front of the marker).
#[tokio::test]
async fn replayed_sessions_show_no_internal_markers() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        let msgs = json!([
            { "role": "user", "content": [{ "type": "text", "text": "a real prompt" }] },
            { "role": "assistant", "content": [{ "type": "text", "text": "half an answer" }], "stop_reason": "aborted" },
            { "role": "user", "notification": true, "turn_aborted": true, "content": [{ "type": "text",
              "text": "<turn_aborted>The user interrupted the previous turn.</turn_aborted>" }] },
            { "role": "user", "content": [{ "type": "text", "text": "run a tool" }] },
            { "role": "assistant", "content": [{ "type": "tool_call", "id": "c1", "name": "nap", "arguments": {} }],
              "stop_reason": "tool_use" },
            { "role": "tool_result", "tool_call_id": "c1", "tool_name": "nap", "is_error": true,
              "content": [{ "type": "text", "text": "Aborted" }] },
            { "role": "user", "notification": true, "turn_aborted": true, "content": [{ "type": "text",
              "text": "<turn_aborted>The user interrupted the previous turn.</turn_aborted>" }] },
            { "role": "user", "shell": true, "content": [{ "type": "text",
              "text": "<bash command=\"echo hi\" exit_code=\"0\">\nhi\n</bash>" }] },
            { "role": "user", "notification": true, "content": [{ "type": "text",
              "text": "[background job bash-1 finished: completed — nap]\nmore detail" }] }
        ]);
        t.vt.host.emit_json("session/loaded", json!({ "messages": msgs })).await;
        t.until("the replay", |s| s.contains("background job bash-1")).await;
        let h = t.history();
        assert!(!h.contains("turn_aborted") && !h.contains("<bash"), "{h}");
        assert!(h.contains("! echo hi") && !h.contains("more detail"), "{h}");
        let marks: Vec<_> = h.lines().filter(|l| l.contains("Interrupted")).collect();
        assert_eq!(marks.len(), 2, "one per interrupted turn:\n{h}");
        let (answer, tool) = (h.find("half an answer").unwrap(), h.find("run a tool").unwrap());
        let first = h.find("Interrupted").unwrap();
        assert!(answer < first && first < tool && h.rfind("Interrupted").unwrap() > tool, "{h}");
    })
    .await
}

/// The resumed view is the live view: `Ran cmd` (not `Bash(cmd)`), exploration
/// grouped under a count header (`Read 1 file`); a failed call has the error style and its exit code (Kimi, Codex); a
/// long output shows its last lines (Codex).
#[tokio::test]
async fn replayed_tool_calls_look_like_the_live_ones() {
    local(async {
        let mut t = Term::colored(json!([]), 80, 30).await;
        let long: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let call = |id: &str, cmd: &str| json!({ "type": "tool_call", "id": id, "name": "bash", "arguments": { "command": cmd } });
        let result = |id: &str, text: &str, code: i64| json!({ "role": "tool_result", "tool_call_id": id, "tool_name": "bash",
            "content": [{ "type": "text", "text": text }], "is_error": code != 0, "details": { "code": code, "output": text } });
        let msgs = json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            { "role": "assistant", "content": [call("c1", "seq 20"), call("c2", "cat missing.txt")] },
            result("c1", &long, 0),
            result("c2", "cat: missing.txt: No such file or directory", 1),
            { "role": "assistant", "content": [call("c3", "make build")] },
            result("c3", "error: no rule\n", 2),
            { "role": "assistant", "content": [{ "type": "text", "text": "all done" }] }
        ]);
        t.vt.host.emit_json("session/loaded", json!({ "messages": msgs })).await;
        t.until("the replay", |s| s.contains("all done")).await;
        let h = t.history();
        assert!(h.contains("Ran seq 20") && !h.contains("Bash("), "the live form:\n{h}");
        assert!(h.contains("line 20") && !h.contains("line 1\n"), "the last lines, not the first:\n{h}");
        assert!(h.contains("Read 1 file") && h.contains("Read missing.txt (exit 1)"), "a failed read:\n{h}");
        assert!(h.contains("Ran make build (exit 2)"), "a failed command:\n{h}");
        // the exit code is in the error color
        let screen = t.parser.screen();
        let (rows, cols) = screen.size();
        let red = (0..rows.get()).find_map(|r| {
            let row: String = (0..cols.get()).filter_map(|c| screen.cell(r, c).map(|x| x.contents())).collect();
            let c = row.find("(exit 2)")?;
            screen.cell(r, row[..c].chars().count() as u16 + 1).map(|x| x.fgcolor())
        });
        assert!(matches!(red, Some(c) if c != vt100::Color::Default), "exit code uncolored: {red:?}");
    })
    .await
}

/// On exit the TUI prints how to come back: only once the session
/// is on disk.
#[tokio::test]
async fn exit_names_the_command_that_resumes_the_session() {
    local(async {
        let mut t = Term::new(json!([{ "text": "hello back" }]), 80, 20).await;
        assert_eq!(t.vt.resume_hint().await, None, "an empty session has nothing to resume");
        t.submit("hello").await;
        t.idle().await;
        let hint = t.vt.resume_hint().await.expect("a hint after a turn");
        global(&t.vt.host, "S", t.vt.host.service_table("session").unwrap());
        let id: String = eval(&t.vt.host, "return S:info().session_id").await;
        assert_eq!(hint, format!("Resume this session with: mare -r {id}"));
    })
    .await
}

/// Two earlier sessions of the workspace (ids `sa`, `sb`), written as files.
fn write_two_sessions(env: &TestEnv) {
    let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
    std::fs::create_dir_all(&dir).unwrap();
    // The index orders sessions by their last update, which for these (timestamp-less) files is
    // the file's mtime in ms: two files written back to back can share one, and then their order
    // is arbitrary. Pin it: `sb` is newer than `sa`, both older than a file written afterwards.
    let now = std::time::SystemTime::now();
    for (file, id, text, age) in [
        ("2026-01-01T00-00-00_aaa.jsonl", "sa", "refactor the parser", 20),
        ("2026-01-02T00-00-00_bbb.jsonl", "sb", "fix the flaky test", 10),
    ] {
        let lines = [
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": id, "cwd": env.cwd }),
            json!({ "type": "message", "entry_id": format!("{id}-1"), "parent_entry_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": text }] } }),
        ];
        let path = dir.join(file);
        std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(now - Duration::from_secs(age)).unwrap();
    }
}

/// `mare -r <text>` that is not a full session id opens the picker with the text as the search
/// `-r <text>` filtering: nothing fitting shows "(no matches)", the first
/// Esc clears the search and shows the whole list, the second leaves for a new session. Not a
/// startup error.
#[tokio::test]
async fn resume_with_text_opens_the_picker_searching_it() {
    local(async {
        let env = TestEnv::new().await;
        write_two_sessions(&env);
        let mut opts = env.host().faux(json!([])).args(json!({ "resume": "nosuchtext" })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 100, 24).await;
        let mut parser = vt100::Parser::new(nz(24), nz(100), 2000);
        pump_until(&mut vt, &mut parser, "the empty search", |_, s| s.contains("(no matches)")).await;
        let screen = parser.screen().contents();
        assert!(screen.contains("Resume session") && screen.contains("nosuchtext"), "{screen}");
        vt.key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "the whole list", |_, s| {
            s.contains("refactor the parser") && s.contains("fix the flaky test") && !s.contains("(no matches)")
        })
        .await;
        vt.key(KeyEvent::new(KeyCode::Escape, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "startup did not finish", |vt, _| !vt.starting()).await;
        let screen = parser.screen().contents();
        assert!(!screen.contains("Startup blocked") && !screen.contains("no session matches"), "{screen}");
        assert!(crate_resume_error(&vt).await.is_none());
        drop(Term::from_vt(vt, parser));
    })
    .await
}

/// The search text can match: only those sessions are listed, and Enter resumes the one on.
#[tokio::test]
async fn resume_with_text_lists_the_matches_and_resumes_one() {
    local(async {
        let env = TestEnv::new().await;
        write_two_sessions(&env);
        let mut opts = env.host().faux(json!([])).args(json!({ "resume": "flaky" })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 100, 24).await;
        let mut parser = vt100::Parser::new(nz(24), nz(100), 2000);
        pump_until(&mut vt, &mut parser, "the matching list", |_, s| {
            s.contains("fix the flaky test") && !s.contains("refactor the parser")
        })
        .await;
        vt.key(KeyEvent::new(KeyCode::Enter, Modifiers::NONE));
        pump_until(&mut vt, &mut parser, "startup did not finish", |vt, _| !vt.starting()).await;
        global(&vt.host, "SS", vt.host.service_table("session").unwrap());
        let id: String = eval(&vt.host, "return SS:info(nil).session_id").await;
        assert_eq!(id, "sb");
        drop(Term::from_vt(vt, parser));
    })
    .await
}

/// A full session id resumes at once, with no picker.
#[tokio::test]
async fn resume_with_a_full_id_skips_the_picker() {
    local(async {
        let env = TestEnv::new().await;
        write_two_sessions(&env);
        let mut opts = env.host().faux(json!([])).args(json!({ "resume": "sa" })).opts;
        opts.mode = Mode::Tui;
        let mut vt = OwnedVt::new(env, opts, 100, 24).await;
        let mut parser = vt100::Parser::new(nz(24), nz(100), 2000);
        pump_until(&mut vt, &mut parser, "startup did not finish", |vt, _| !vt.starting()).await;
        assert!(!parser.screen().contents().contains("Resume session"), "{}", parser.screen().contents());
        global(&vt.host, "SS", vt.host.service_table("session").unwrap());
        let id: String = eval(&vt.host, "return SS:info(nil).session_id").await;
        assert_eq!(id, "sa");
        drop(Term::from_vt(vt, parser));
    })
    .await
}

/// Whether a reconcile of the session index has finished (it stamps `reconciled_at`).
fn reconciled(index: &std::path::Path) -> bool {
    let Ok(c) = rusqlite::Connection::open_with_flags(index, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else {
        return false;
    };
    c.query_row("SELECT v FROM kv WHERE k = 'reconciled_at'", [], |r| r.get::<_, i64>(0)).is_ok()
}

/// The picker retries a transient in-flight rollback and catches up behind it: sessions another
/// process wrote since startup show up in the open picker, which stays open with its selection.
#[tokio::test]
async fn the_resume_picker_refreshes_when_the_background_reconcile_finds_sessions() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({}), 100, 24, write_two_sessions).await;
        // Startup is over and the startup reconcile has run to its end (it stamps `reconciled_at`
        // when it finishes): the third session is written only after that. While the reconcile
        // is still going, a file written now can be picked up by the first read of the open
        // picker (no refresh to test) or be missed by a reconcile the picker joins.
        t.until_vt("startup to finish", |t| !t.vt.starting()).await;
        let index = t._env.home.path().join("state/sessions/index.sqlite");
        let end = Instant::now() + WAIT;
        while !reconciled(&index) {
            assert!(Instant::now() < end, "the startup reconcile never finished");
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        assert_eq!(eval::<i64>(&t.vt.host, "return select(2, mare.store.index_read({ scope = 'all' }))").await, 2);
        // the plugin takes the finished reconcile's result a few frames later
        for _ in 0..20 {
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let dir = t._env.home.path().join("state/sessions").join(mare_store::cwd_slug(&t._env.cwd));
        let lines = [
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "sc", "cwd": t._env.cwd }),
            json!({ "type": "message", "entry_id": "sc-1", "parent_entry_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": "write the changelog" }] } }),
        ];
        std::fs::write(
            dir.join("2026-01-03T00-00-00_ccc.jsonl"),
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        exec(
            &t.vt.host,
            r#"INDEX_RECONCILE_CALLS = 0
               local reconcile = mare.store.index_reconcile
               mare.store.index_reconcile = function()
                 INDEX_RECONCILE_CALLS = INDEX_RECONCILE_CALLS + 1
                 if INDEX_RECONCILE_CALLS == 1 then
                   return { files = 3, updated = 0, removed = 0, failed = 0, pending = 1, unsupported = {} }
                 end
                 return reconcile()
               end"#,
        )
        .await;
        t.submit("/resume").await;
        t.until("all three sessions in the open picker", |s| {
            s.contains("Resume session")
                && s.contains("write the changelog")
                && s.contains("refactor the parser")
                && s.contains("fix the flaky test")
        })
        .await;
        // the refresh keeps the session the cursor was on (the newest then, "fix the flaky test"):
        // the new session lands above it, so the cursor is on the third row of four
        assert!(t.screen().contains("3 of 4"), "{}", t.screen());
        assert!(eval::<i64>(&t.vt.host, "return INDEX_RECONCILE_CALLS").await >= 2);
    })
    .await
}

/// The refresh replaces the items of the open picker (`sel:update`) instead of opening another:
/// what the user types the moment it opens is neither lost nor reset when the background
/// reconcile lands, and once the search is cleared the new session is in the list.
#[tokio::test]
async fn typing_into_the_resume_picker_survives_the_background_refresh() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({}), 100, 24, write_two_sessions).await;
        let end = Instant::now() + Duration::from_secs(10);
        while eval::<i64>(&t.vt.host, "return select(2, mare.store.index_read({ scope = 'all' }))").await < 2 {
            assert!(Instant::now() < end, "the startup reconcile never indexed the sessions");
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let dir = t._env.home.path().join("state/sessions").join(mare_store::cwd_slug(&t._env.cwd));
        let lines = [
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "sc", "cwd": t._env.cwd }),
            json!({ "type": "message", "entry_id": "sc-1", "parent_entry_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": "write the changelog" }] } }),
        ];
        std::fs::write(
            dir.join("2026-01-03T00-00-00_ccc.jsonl"),
            lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
        )
        .unwrap();
        t.submit("/resume").await;
        // typed the moment the picker is open: the refresh lands around these keys
        t.until("the picker", |s| s.contains("Resume session")).await;
        t.vt.type_str("flaky");
        t.until("the picker narrowed to the typed search", |s| {
            s.contains("fix the flaky test") && !s.contains("refactor the parser")
        })
        .await;
        // let the refresh land, then clear the search: the session found meanwhile is listed
        let end = Instant::now() + Duration::from_secs(10);
        while eval::<i64>(&t.vt.host, "return select(2, mare.store.index_read({ scope = 'all' }))").await < 3 {
            assert!(Instant::now() < end, "the reconcile never indexed the new session");
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        t.step().await;
        assert!(t.screen().contains("flaky") && !t.screen().contains("refactor the parser"), "{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the whole list with the new session", |s| {
            s.contains("write the changelog") && s.contains("refactor the parser")
        })
        .await;
    })
    .await
}

/// An index of an older schema is set aside and rebuilt from the session files; the user sees
/// one notice at startup, and the rows are back.
#[tokio::test]
async fn a_schema_change_rebuilds_the_index_and_says_so_once() {
    local(async {
        let mut t = Term::build(json!([]), json!({}), json!({}), 100, 24, |env| {
            write_two_sessions(env);
            let c = rusqlite::Connection::open(env.home.path().join("state/sessions/index.sqlite")).unwrap();
            c.execute_batch("CREATE TABLE sessions (id, cost_usd, branch_count); PRAGMA user_version = 1;").unwrap();
        })
        .await;
        t.until("the notice", |s| s.contains("session index rebuilt (schema version 1")).await;
        // handed out once: the picker's own reconcile has nothing more to say
        t.submit("/resume").await;
        t.until("the picker", |s| s.contains("refactor the parser") && s.contains("fix the flaky test")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return mare.store.index_notice()").await, None);
    })
    .await
}

/// The row of the screen holding `text`, and the one under it.
fn row_and_next(s: &str, text: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = s.lines().collect();
    let i = lines.iter().position(|l| l.contains(text))?;
    Some((lines[i].to_string(), lines.get(i + 1).copied().unwrap_or("").to_string()))
}

/// `/resume` rows use `SelectItem.lines`: the default is the
/// title over a dim row of time · short id · directory; Ctrl+L steps to
/// the one-row dense look, then the three-row card (title · time / id · dir / › last input);
/// Ctrl+E opens the highlighted session up in place (Codex). The search text is the same.
#[tokio::test]
async fn resume_looks_switch_with_ctrl_l_and_ctrl_e_expands_a_row() {
    local(async {
        let mut t = Term::new(json!([]), 80, 30).await;
        write_two_sessions(&t._env);
        t.submit("/resume").await;
        let body = |l: &str| l.trim_start_matches(['│', ' ']).to_string();
        // "This project": the session ran in the project's own directory, so no directory
        let two_rows = |next: &str| {
            let b = body(next);
            b.trim_end_matches(['│', ' ']).ends_with("· sa") && !b.contains('⌁')
        };
        t.until("two rows per session", |s| {
            row_and_next(s, "refactor the parser").is_some_and(|(_, next)| two_rows(&next))
        })
        .await;
        t.ctrl('l');
        t.until("one row per session", |s| {
            row_and_next(s, "refactor the parser").is_some_and(|(row, next)| {
                row.contains("just now") && row.trim_end_matches(['│', ' ']).ends_with("sa") && !two_rows(&next)
            })
        })
        .await;
        t.ctrl('l');
        t.until("the cards", |s| {
            row_and_next(s, "refactor the parser").is_some_and(|(_, next)| body(&next).starts_with("sa "))
        })
        .await;
        t.ctrl('l');
        t.until("back to two rows", |s| {
            row_and_next(s, "refactor the parser").is_some_and(|(_, next)| two_rows(&next))
        })
        .await;
        // "All": where each session ran
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("the directory in All", |s| {
            row_and_next(s, "refactor the parser").is_some_and(|(_, next)| body(&next).contains("· sa · ⌁ "))
        })
        .await;
        t.ctrl('e');
        t.until("a session opened up", |s| s.contains("│ Session:") && s.contains("└ Last input:")).await;
    })
    .await
}

/// Ctrl+X in /resume archives the highlighted session (not this one) after asking: an archive
/// entry in its file, and it leaves the list.
#[tokio::test]
async fn resume_archives_another_session_and_it_leaves_the_list() {
    local(async {
        let mut t = Term::new(json!([]), 80, 30).await;
        write_two_sessions(&t._env);
        t.submit("/resume").await;
        t.until("both sessions", |s| s.contains("refactor the parser") && s.contains("fix the flaky test")).await;
        t.vt.type_str("flaky");
        t.until("only the one to archive", |s| s.contains("fix the flaky test") && !s.contains("refactor the parser"))
            .await;
        t.ctrl('x');
        t.until("the question", |s| s.contains("Archive session")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.until("the list again, without it", |s| {
            s.contains("Resume session") && !s.contains("Archive session") && !s.contains("fix the flaky test")
        })
        .await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the rest of the list", |s| s.contains("refactor the parser") && !s.contains("fix the flaky test"))
            .await;
        let archived: bool = eval(
            &t.vt.host,
            "for _, r in ipairs(mare.store.index_read({ scope = 'all', archived = true })) do \
               if r.session_id == 'sb' then return true end end return false",
        )
        .await;
        assert!(archived, "the session is archived");
    })
    .await
}

#[tokio::test]
async fn deleting_a_session_requires_an_explicit_confirmation() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        write_two_sessions(&t._env);
        let session = t
            ._env
            .home
            .path()
            .join("state/sessions")
            .join(mare_store::cwd_slug(&t._env.cwd))
            .join("2026-01-02T00-00-00_bbb.jsonl");
        t.submit("/resume").await;
        t.until("both sessions", |s| s.contains("refactor the parser") && s.contains("fix the flaky test")).await;
        t.vt.type_str("flaky");
        t.until("the selected session", |s| s.contains("fix the flaky test") && !s.contains("refactor the parser"))
            .await;
        t.ctrl('d');
        t.until("the deletion confirmation", |s| s.contains("Delete session") && s.contains("undone.")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("Delete session"), "a bare y must not confirm deletion: {}", t.screen());
        assert!(session.exists(), "the session file must survive an accidental y");
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until_vt("the explicitly confirmed deletion", |_| !session.exists()).await;
    })
    .await
}

#[tokio::test]
async fn durable_ui_card_rewraps_on_resize_and_replays_in_causal_order_on_resume() {
    local(async {
        let mut t = Term::build(
            json!([{ "text": "alpha" }, { "text": "omega" }]),
            json!({}),
            json!({}),
            90,
            24,
            |env| {
                let state = env.home.path().join("state");
                std::fs::create_dir_all(&state).unwrap();
                std::fs::write(state.join("state.json"), r#"{"version":1,"sandbox_setup_hint_shown":true}"#).unwrap();
            },
        )
        .await;

        global(&t.vt.host, "UI", t.vt.host.service_table("ui").unwrap());
        global(&t.vt.host, "S", t.vt.host.service_table("session").unwrap());

        t.submit("first").await;
        t.idle().await;

        exec(
            &t.vt.host,
            r#"UI:card({
                title = "Proposed Plan",
                persist = true,
                markdown = "bravo charlie delta echo foxtrot golf hotel india juliett kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu"
            })"#,
        )
        .await;
        t.until("card rendered", |s| s.contains("Proposed Plan") && s.contains("zulu")).await;

        t.resize(40, 24).await;
        let screen = t.screen();
        assert!(
            screen.contains("Proposed Plan")
                && screen.contains("zulu")
                && screen.lines().all(|l| l.trim_end().chars().count() <= 40),
            "card must rewrap to 40 cols:\n{screen}"
        );

        t.submit("second").await;
        t.idle().await;

        let path: String = eval(&t.vt.host, "return S:info().path").await;
        let VtResources { mut vt, _env } = t.owned.0.take().unwrap();
        vt.shutdown().await;
        drop(vt);

        let mut rt = Term::in_env(_env, json!([]), json!({}), json!({}), 90, 24).await;
        global(&rt.vt.host, "S", rt.vt.host.service_table("session").unwrap());
        exec(&rt.vt.host, &format!("S:load({{ path = {path:?} }})")).await;
        rt.until("resumed", |s| s.contains("omega")).await;

        let hist = rt.history();
        let pos_alpha = hist.find("alpha").expect("alpha in resumed history");
        let pos_card = hist.find("Proposed Plan").expect("Proposed Plan card in resumed history");
        let pos_omega = hist.find("omega").expect("omega in resumed history");
        assert!(
            pos_alpha < pos_card && pos_card < pos_omega,
            "card must replay in causal order between alpha and omega:\n{hist}"
        );
    })
    .await
}

#[tokio::test]
async fn persistent_ui_card_record_failure_does_not_publish_or_succeed() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        global(&t.vt.host, "UI", t.vt.host.service_table("ui").unwrap());
        global(&t.vt.host, "S", t.vt.host.service_table("session").unwrap());
        let ok: bool = eval(
            &t.vt.host,
            r#"
            CARD_EVENTS = 0
            mare.on("ui/card", function() CARD_EVENTS = CARD_EVENTS + 1 end)
            local orig_record = S.add_record
            S.add_record = function() error({ code = "persist_failed", message = "ENOSPC" }, 0) end
            local res, err = UI:card({
                title = "Should Not Appear",
                persist = true,
                markdown = "secret failure body",
            })
            S.add_record = orig_record
            return res == nil and err == "ENOSPC" and CARD_EVENTS == 0
            "#,
        )
        .await;
        assert!(ok, "ui.card must return nil, err and emit no ui/card event when session:record fails");
        let screen = t.screen();
        let hist = t.history();
        assert!(
            !screen.contains("Should Not Appear") && !hist.contains("Should Not Appear"),
            "failed card must not render"
        );
    })
    .await
}
