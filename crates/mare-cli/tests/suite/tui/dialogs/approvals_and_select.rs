//! Approval keys, notification cards, exit prompts, and `mare.ui.select`/`input`/`editor` dialogs.

use super::*;

/// The approval keys (Codex approval_overlay.rs): `y` allows once, `a` for the session —
/// whichever item is highlighted.
#[tokio::test]
async fn approval_keys_answer_without_moving_the_selection() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "A" } }] },
            { "text": "wrote a" },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "b.txt", "content": "B" } }] },
            { "text": "wrote b" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });
        let mut t = Term::with(script, cfg, 80, 24).await;
        let cwd = t._env.cwd.clone();
        t.submit("write a").await;
        t.until("the dialog", |s| s.contains("Allow once")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE); // on "Deny": the key still decides
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.idle().await;
        assert!(cwd.join("a.txt").exists(), "y allowed it");
        t.submit("write b").await;
        t.until("the second dialog", |s| s.contains("Allow once")).await;
        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.idle().await;
        assert!(cwd.join("b.txt").exists(), "a allowed it (for the session)");
    })
    .await
}

/// A notification card never covers a popup (the startup "Permissions" toast hid the approval
/// dialog's preview pane) and never grows the live area for it: with a dialog open it shows only
/// in rows already above the dialog, else it waits and shows once the dialog closes.
#[tokio::test]
async fn notification_cards_stay_clear_of_an_open_dialog() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "PREVIEW-LINE" } }] },
            { "text": "wrote a" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });
        let mut t = Term::with(script, cfg, 100, 30).await;
        t.submit("write a").await;
        t.until("the dialog", |s| s.contains("Allow once") && s.contains("PREVIEW-LINE")).await;
        let before = t.screen();
        exec(&t.vt.host, "mare.ui.notify('TOAST-MARK a notice while the dialog is open', 'info')").await;
        for _ in 0..5 {
            t.step().await;
        }
        let screen = t.screen();
        let rows: Vec<&str> = screen.lines().collect();
        let allow = rows.iter().position(|l| l.contains("Allow once")).unwrap();
        let top =
            (0..allow).rev().find(|&i| rows[i].trim_start().starts_with('╭') && !rows[i].starts_with("    ")).unwrap();
        assert!(rows[top..].iter().all(|l| !l.contains("TOAST-MARK")), "no card over the dialog:\n{screen}");
        assert!(screen.contains("PREVIEW-LINE") && screen.contains("Deny and stop"), "{screen}");
        let at = |s: &str| s.lines().position(|l| l.contains("Allow once"));
        assert_eq!(at(&screen), at(&before), "the dialog didn't move:\n{before}\n---\n{screen}");
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.idle().await;
        t.until("the card, once the dialog is gone", |s| s.contains("TOAST-MARK")).await;
    })
    .await
}

/// Exiting with a background job still running asks first and lists it; "no" stays, "yes"
/// leaves; without jobs Ctrl+D leaves at once.
#[cfg(unix)]
#[tokio::test]
async fn exit_with_running_background_jobs_asks_first() {
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
        // the startup notice card would cover the dialog
        t.vt.dismiss_notices();
        t.key(KeyCode::Char('d'), Modifiers::CONTROL);
        t.until("the exit confirmation", |s| s.contains("1 background job is still") && s.contains("sleep 30")).await;
        assert!(!t.vt.quit_requested());
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("the dialog to close", |s| !s.contains("still running")).await;
        assert!(!t.vt.quit_requested(), "answering no stays");
        t.key(KeyCode::Char('d'), Modifiers::CONTROL);
        t.until("the exit confirmation again", |s| s.contains("still running")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        let end = Instant::now() + WAIT;
        while !t.vt.quit_requested() {
            assert!(Instant::now() < end, "confirmed exit never quit; screen:\n{}", t.screen());
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await; // sleep-ok: bounded poll waiting for confirmed quit
        }
        t.vt.host.shutdown().await;
    })
    .await
}

/// Sub-agent approvals waiting behind the open one show as one line at the bottom
/// (`agent/approval-queue`; Codex `pending_thread_approvals.rs`): the oldest and how many more;
/// gone once nobody waits.
#[tokio::test]
async fn waiting_child_approvals_show_one_line_at_the_bottom() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        let pending = json!({ "pending": [{ "label": "explorer", "run_id": "2" }, { "agent_id": "agent-5" }] });
        t.vt.host.emit_json("agent/approval-queue", pending).await;
        t.until("the queue line", |s| s.contains("! approval needed: explorer · run 2 (+1)")).await;
        t.vt.host.emit_json("agent/approval-queue", json!({ "pending": [] })).await;
        t.until("the line to go", |s| !s.contains("approval needed")).await;
    })
    .await
}

/// With the `agents` action on a key the line ends with where to see them (`· ← for agents`,
/// the key from the live keymap — the agents plugin binds ← by default); without one it is the
/// plain line.
#[tokio::test]
async fn the_waiting_line_names_the_agents_key_when_one_is_bound() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        let pending = json!({ "pending": [{ "label": "explorer" }] });
        t.vt.host.emit_json("agent/approval-queue", pending.clone()).await;
        t.until("the hint", |s| s.contains("approval needed: explorer  · answered in order · ← for agents")).await;
        exec(&t.vt.host, r#"mare.keymap.set("editor", "<Left>", false)"#).await;
        t.vt.host.emit_json("agent/approval-queue", pending).await;
        t.until("the plain line", |s| s.contains("approval needed: explorer  · answered in order\n")).await;
    })
    .await
}

/// A key binding written as a coroutine of its own (`coroutine.wrap(function() … end)()`) can
/// wait on a dialog: the async wait passes through the inner coroutine (it used to end there —
/// the dialog opened, but its answer went nowhere).
#[tokio::test]
async fn a_key_binding_can_wait_on_a_dialog_from_a_nested_coroutine() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(
            &t.vt.host,
            r#"mare.keymap.set("editor", "<C-x>", function()
                 coroutine.wrap(function()
                   local ok = mare.ui.confirm("Nested?")
                   mare.task.sleep(1)
                   mare.g.nested = ok and "yes" or "no"
                 end)()
               end, { desc = "nested dialog test" })"#,
        )
        .await;
        t.step().await;
        t.ctrl('x');
        t.until("the dialog", |s| s.contains("Nested?")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.until("the dialog to close", |s| !s.contains("Nested?")).await;
        let end = Instant::now() + WAIT;
        loop {
            t.step().await;
            if eval::<Option<String>>(&t.vt.host, "return mare.g.nested").await.as_deref() == Some("yes") {
                break;
            }
            assert!(Instant::now() < end, "the answer never reached the binding");
            tokio::time::sleep(Duration::from_millis(15)).await; // sleep-ok: bounded poll waiting for Lua binding result
        }
    })
    .await
}

/// `mare.ui.select{on_open=…}` (W4): `sel:update(items)` replaces the open select's items in
/// place — the search text, the searching state and the highlighted item (by id) stay — and the
/// answer is an index into the latest items. A search given up front opens in searching mode:
/// the first Esc clears it, the second closes the select.
#[tokio::test]
async fn an_open_select_updates_its_items_in_place() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R = { mare.ui.select({
                   title = "Pick one",
                   items = { { item_id = "a", label = "alpha" }, { item_id = "b", label = "beta" }, { item_id = "c", label = "gamma" } },
                   on_open = function(sel) H = sel end,
                 }) }
               end)"#,
        )
        .await;
        t.until("the select", |s| s.contains("Pick one") && s.contains("gamma")).await;
        // search "a" (every item matches), then move to beta
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("a");
        t.key(KeyCode::Down, Modifiers::NONE);
        t.step().await;
        exec(
            &t.vt.host,
            r#"OPEN = H:update({ { item_id = "n", label = "a new one" }, { item_id = "c", label = "gamma" }, { item_id = "b", label = "beta (renamed)" } })"#,
        )
        .await;
        t.until("the new items", |s| s.contains("a new one") && s.contains("beta (renamed)")).await;
        assert!(!t.screen().contains("alpha"), "{}", t.screen());
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the select closed", |s| !s.contains("Pick one")).await;
        let answer: (Option<i64>, bool, bool) = eval(&t.vt.host, "return R and R[1], OPEN, H:update({ 'x' })").await;
        assert_eq!(answer, (Some(3), true, false), "beta (renamed) is the 3rd new item; closed: no update");

        // a search given up front: Esc clears it first
        exec(
            &t.vt.host,
            r#"R2 = nil
               mare.task.spawn(function() R2 = { mare.ui.select({ title = "Prefilled", items = { "one", "two" }, filter = "tw" }) } end)"#,
        )
        .await;
        t.until("the prefilled select", |s| s.contains("Prefilled") && s.contains("two") && !s.contains("one")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the cleared search", |s| s.contains("Prefilled") && s.contains("one")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("closed", |s| !s.contains("Prefilled")).await;
        assert!(eval::<bool>(&t.vt.host, "return R2 ~= nil and R2[1] == nil").await);
    })
    .await
}

#[tokio::test]
async fn multi_select_keeps_checked_items_by_id_across_updates_and_tabs() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R = { mare.ui.select({ title = 'Choose items', multi = true,
                   tabs = {
                     { name = 'First', items = { { item_id = 'a', label = 'alpha' }, { item_id = 'b', label = 'beta' } } },
                     { name = 'Second', items = { { item_id = 'c', label = 'charlie' }, { item_id = 'd', label = 'delta' } } },
                   },
                   on_open = function(handle) H = handle end,
                 }) }
               end)"#,
        )
        .await;
        t.until("the first tab", |s| s.contains("Choose items") && s.contains("alpha")).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("alpha checked", |s| s.contains("[x] alpha")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("the second tab", |s| s.contains("charlie")).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.until("charlie checked", |s| s.contains("[x] charlie")).await;
        exec(
            &t.vt.host,
            r#"H:update({ tabs = {
                 { name = 'First', items = { { item_id = 'b', label = 'beta' }, { item_id = 'a', label = 'alpha renamed' } } },
                 { name = 'Second', items = { { item_id = 'new', label = 'new item' }, { item_id = 'c', label = 'charlie renamed' } } },
               } })"#,
        )
        .await;
        t.until("updated second tab", |s| s.contains("charlie renamed") && s.contains("new item")).await;
        let screen = t.screen();
        assert!(screen.contains("[x] charlie renamed") && screen.contains("[ ] new item"), "{screen}");
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        t.until("updated first tab", |s| s.contains("alpha renamed") && s.contains("beta")).await;
        let screen = t.screen();
        assert!(screen.contains("[x] alpha renamed") && screen.contains("[ ] beta"), "{screen}");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the picker closed", |s| !s.contains("Choose items")).await;
        assert_eq!(eval::<(i64, i64)>(&t.vt.host, "return R[1][1], R[2]").await, (2, 1));
    })
    .await
}

/// An approval that arrives while the Ctrl+T transcript is open is drawn over the overlay and
/// answered there; the overlay stays open afterwards. Esc on the approval is the approval's Esc
/// (it doesn't close the overlay). User decision 2026-11-12, after Kimi's approval
/// panel taking focus (kimi-tui.ts:4025-4054); Codex instead defers approvals while its overlay
/// is open (codex-rs/tui/src/app/input.rs:102/120/194).
#[tokio::test]
async fn an_approval_shows_and_answers_over_the_open_transcript() {
    local(async {
        let write = |p: &str| json!([{ "name": "write", "arguments": { "path": p, "content": "X" } }]);
        let filler: String = (0..12)
            .map(|i| format!("UNDERLYING_TRANSCRIPT_ROW_{i:02} {}", "Z".repeat(60)))
            .collect::<Vec<_>>()
            .join("\n");
        let script = json!([
            { "text": filler, "delay_ms": 60, "tool_calls": write("a.txt") },
            { "text": "wrote a" },
            { "text": "about to write b.txt", "delay_ms": 60, "tool_calls": write("b.txt") },
            { "text": "stopped" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });
        let mut t = Term::with(script, cfg, 100, 30).await;
        let cwd = t._env.cwd.clone();
        let in_transcript =
            |s: &str| s.contains("Transcript (expanded) — ↑/↓ PgUp/PgDn g/G, / search, y copy, Esc to close");

        t.submit("write a").await;
        t.ctrl('t');
        t.until("the transcript before the approval", |s| in_transcript(s) && !s.contains("Allow once")).await;
        t.until("the approval over the transcript", |s| in_transcript(s) && s.contains("Allow once")).await;
        let modal_screen = t.screen();
        let rows: Vec<&str> = modal_screen.lines().collect();
        let top = rows.iter().position(|r| r.contains("╭─ Allow write?")).expect("modal top border");
        let bot = rows[top..].iter().rposition(|r| r.contains('╰')).map(|i| top + i).expect("modal bottom border");
        for row in &rows[top..=bot] {
            let left_end = row.find(['╭', '│', '╰']).expect("left border");
            let right_start = row.rfind(['╮', '│', '╯']).expect("right border");
            let right_after = right_start + row[right_start..].chars().next().unwrap().len_utf8();
            assert!(
                row[..left_end].trim().is_empty() && row[right_after..].trim().is_empty(),
                "full-screen transcript rows behind modal must be blank outside borders:\n{row}\nfull:\n{modal_screen}"
            );
        }
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.idle().await;
        assert!(cwd.join("a.txt").exists(), "y over the transcript allowed it; screen:\n{}", t.screen());
        t.until("still in the transcript, the dialog gone", |s| in_transcript(s) && !s.contains("Allow once")).await;

        // back to the editor for the next prompt, then the same with Esc
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("the main screen", |s| !in_transcript(s)).await;
        t.submit("write b").await;
        t.ctrl('t');
        t.until("the second approval over the transcript", |s| in_transcript(s) && s.contains("Allow once")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("Esc answered the approval, not the overlay", |s| in_transcript(s) && !s.contains("Allow once")).await;
        t.idle().await;
        assert!(!cwd.join("b.txt").exists(), "Esc on the approval must not allow the write");
        assert!(in_transcript(&t.screen()), "the overlay stays open:\n{}", t.screen());
    })
    .await
}

/// The same layering for the ask dialog and while a transcript search is being typed: the
/// approval comes over the search prompt, `n` and the reason go to the dialogs (not into the
/// query), and afterwards the transcript and its half-typed search are as they were. Decision
/// (2026-11-12): any modal is drawn over any overlay and takes the keys first.
#[tokio::test]
async fn the_ask_dialog_answers_over_a_transcript_search() {
    local(async {
        let write = json!([{ "name": "write", "arguments": { "path": "c.txt", "content": "X" } }]);
        let script = json!([
            { "text": "about to write c.txt", "delay_ms": 400, "tool_calls": write },
            { "text": "stopped" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });
        let mut t = Term::with(script, cfg, 100, 30).await;
        let cwd = t._env.cwd.clone();
        let in_transcript =
            |s: &str| s.contains("Transcript (expanded) — ↑/↓ PgUp/PgDn g/G, / search, y copy, Esc to close");

        t.submit("write c").await;
        t.ctrl('t');
        t.until("the transcript", in_transcript).await;
        t.vt.type_str("/abo");
        t.until("the search prompt", |s| s.contains("/abo") && !s.contains("Allow once")).await;
        t.until("the approval over the search", |s| in_transcript(s) && s.contains("Allow once")).await;
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("the reason prompt over the transcript", |s| in_transcript(s) && s.contains("Tell the model why"))
            .await;
        t.vt.type_str("use scratch");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        assert!(!cwd.join("c.txt").exists(), "denied write must not run");
        global(&t.vt.host, "AG", t.vt.host.service_table("agent").unwrap());
        let messages: String = eval(&t.vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(messages.contains("denied by the user: use scratch. Do not retry"), "{messages}");
        // the overlay and its search are untouched: no key leaked into the query
        t.until("the search prompt again, the dialogs gone", |s| {
            in_transcript(s) && s.contains("/abo") && !s.contains("Tell the model why")
        })
        .await;
        assert!(!t.screen().contains("/abon"), "a dialog key reached the search:\n{}", t.screen());
        t.vt.type_str("ut");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("the search still works", |s| s.contains("/about") && s.contains(" of ")).await;
    })
    .await
}

/// a select item's `lines` are drawn instead of its label (a card), the list scrolls by whole
/// cards keeping the highlighted one in full view, and the search still matches the label. Separately,
/// `title_right` ends the top border.
#[tokio::test]
async fn select_items_of_several_lines_scroll_and_search() {
    local(async {
        let mut t = Term::new(json!([]), 60, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"local items = {}
               for i = 1, 12 do
                 items[i] = { label = "session " .. i, lines = {
                   mare.ui.Line({ mare.ui.Span("card " .. i):hl("MareTitle"), mare.ui.Span("  2h ago") }),
                   "  ~/repo  main",
                   "  › first prompt " .. i,
                 } }
               end
               mare.task.spawn(function() R = { mare.ui.select({ title = "Cards", title_right = "Sort: Updated", items = items }) } end)"#,
        )
        .await;
        t.until("the cards", |s| s.contains("card 1  2h ago") && s.contains("› first prompt 1")).await;
        assert!(!t.screen().contains("session 1"), "the label is not drawn: {}", t.screen());
        assert!(t.screen().contains("Sort: Updated ─╮"), "title_right on the top border: {}", t.screen());
        for _ in 0..6 {
            t.key(KeyCode::Down, Modifiers::NONE);
        }
        t.until("card 7 in full view", |s| s.contains("card 7") && s.contains("› first prompt 7")).await;
        assert!(!t.screen().contains("card 1 "), "scrolled by whole cards: {}", t.screen());
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.vt.type_str("session 12");
        t.until("only the matching card", |s| s.contains("› first prompt 12") && !s.contains("card 7")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("closed", |s| !s.contains("Cards")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return R and R[1]").await, Some(12));
    })
    .await
}

/// Ctrl+C dismisses the top dialog with a reason, as in Codex bottom_pane/mod.rs:3153 and
/// bottom_pane/mcp_server_elicitation.rs:2443. Esc remains an ordinary cancellation.
#[tokio::test]
async fn ctrl_c_interrupts_every_modal_and_esc_cancels() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        let cases = [
            ("select empty", "local v, err = mare.ui.select({ title = 'ReservedModal', items = {} }); R = { v = v, err = err }", false, false),
            ("select unselectable", "local v, err = mare.ui.select({ title = 'ReservedModal', items = { { label = 'h', header = true }, { label = 'x', disabled = true } } }); R = { v = v, err = err }", false, false),
            ("select searching", "local v, err = mare.ui.select({ title = 'ReservedModal', items = { 'one' }, filter = 'zz' }); R = { v = v, err = err }", true, false),
            ("select keys", "local v, err = mare.ui.select({ title = 'ReservedModal', items = { 'one' }, keys = { 'x' } }); R = { v = v, err = err }", false, false),
            ("input", "local v, err = mare.ui.input({ title = 'ReservedModal' }); R = { v = v, err = err }", false, false),
            ("editor", "local v, err = mare.ui.editor({ title = 'ReservedModal' }); R = { v = v, err = err }", false, false),
            ("confirm", "local v, err = mare.ui.confirm('ReservedModal', true); R = { v = v, err = err }", false, false),
            ("questions", "R = mare.ui.questions({ questions = { { question_id = 'q', question = 'ReservedModal', options = { { label = 'A' } } } } })", false, true),
        ];
        for (name, body, searching, questions) in cases {
            for interrupted in [true, false] {
                exec(&t.vt.host, &format!("R = 'pending'; mare.task.spawn(function() {body} end)")).await;
                t.until("modal open", |s| s.contains("ReservedModal")).await;
                if interrupted {
                    t.ctrl('c');
                } else {
                    t.key(KeyCode::Escape, Modifiers::NONE);
                    if searching {
                        t.step().await;
                        t.key(KeyCode::Escape, Modifiers::NONE);
                    }
                }
                t.until("modal closed", |s| !s.contains("ReservedModal")).await;
                let reason = if interrupted { "interrupted" } else { "cancelled" };
                let check = if questions {
                    format!("return type(R) == 'table' and R.dismissed == true and R.reason == '{reason}' and next(R.answers) == nil")
                } else {
                    format!("return type(R) == 'table' and R.v == nil and R.err == '{reason}'")
                };
                assert!(eval::<bool>(&t.vt.host, &check).await, "{name}: expected {reason}");
            }
        }

        exec(&t.vt.host, "R = 'pending'; mare.task.spawn(function() local v, err = mare.ui.select({ title = 'ReservedModal', items = { 'one' } }); R = { v = v, err = err } end)").await;
        t.until("select open", |s| s.contains("ReservedModal")).await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("help open", |s| s.contains("─ Keys ─")).await;
        t.ctrl('c');
        t.until("select restored", |s| s.contains("ReservedModal") && !s.contains("─ Keys ─")).await;
        assert!(eval::<bool>(&t.vt.host, "return R == 'pending'").await);
        t.ctrl('c');
        t.until("select closed", |s| !s.contains("ReservedModal")).await;
        assert!(eval::<bool>(&t.vt.host, "return type(R) == 'table' and R.v == nil and R.err == 'interrupted'").await);
    })
    .await
}

#[tokio::test]
async fn ctrl_c_closes_the_modal_then_the_transcript() {
    local(async {
        let mut t = Term::new(json!([]), 100, 30).await;
        expose(&t.vt.host);
        let in_transcript = |s: &str| s.contains("Transcript (expanded)");
        t.ctrl('t');
        t.until("transcript open", in_transcript).await;
        exec(&t.vt.host, "R = 'pending'; mare.task.spawn(function() local v, err = mare.ui.select({ title = 'ReservedModal', items = { 'one' } }); R = { v = v, err = err } end)").await;
        t.until("modal over transcript", |s| in_transcript(s) && s.contains("ReservedModal")).await;
        t.ctrl('c');
        t.until("modal closed", |s| in_transcript(s) && !s.contains("ReservedModal")).await;
        assert!(eval::<bool>(&t.vt.host, "return type(R) == 'table' and R.v == nil and R.err == 'interrupted'").await);
        t.ctrl('c');
        t.until("transcript closed", |s| !in_transcript(s) && s.contains("Ask mare")).await;
        assert!(!t.screen().contains("again to exit"));
    })
    .await
}

/// UI-CANCEL: `cancel` (`mare.Token`) closes open `ui.select`, `ui.input`, and `ui.confirm`
/// dialogs (ref: Kimi `question-dialog.ts:665`, Codex `async_questions/input.rs:274-284`).
#[tokio::test]
async fn ui_cancel_closes_select_input_and_confirm_dialogs() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);

        // 1. ui.select cancelled while open -> nil, "cancelled"
        exec(
            &t.vt.host,
            r#"TOK = mare.task.token()
               R = "pending"
               mare.task.spawn(function()
                 local i, tab_or_err, key, flt = mare.ui.select({ title = "CancelSelect", items = { "a", "b" }, cancel = TOK })
                 R = { i = i, err = tab_or_err, key = key, flt = flt }
               end)"#,
        )
        .await;
        t.until("select open", |s| s.contains("CancelSelect")).await;
        exec(&t.vt.host, "TOK:cancel()").await;
        t.until("select closed by cancel", |s| !s.contains("CancelSelect")).await;
        assert!(eval::<bool>(&t.vt.host, "return type(R) == 'table' and R.i == nil and R.err == 'cancelled'").await);

        // 2. ui.input cancelled while open -> nil, "cancelled"
        exec(
            &t.vt.host,
            r#"TOK2 = mare.task.token()
               R2 = "pending"
               mare.task.spawn(function()
                 local v, err = mare.ui.input({ title = "CancelInput", cancel = TOK2 })
                 R2 = { v = v, err = err }
               end)"#,
        )
        .await;
        t.until("input open", |s| s.contains("CancelInput")).await;
        exec(&t.vt.host, "TOK2:cancel()").await;
        t.until("input closed by cancel", |s| !s.contains("CancelInput")).await;
        assert!(eval::<bool>(&t.vt.host, "return type(R2) == 'table' and R2.v == nil and R2.err == 'cancelled'").await);

        // 3. ui.confirm cancelled while open -> nil, "cancelled" (distinct from user choosing No -> false)
        exec(
            &t.vt.host,
            r#"TOK3 = mare.task.token()
               R3 = "pending"
               mare.task.spawn(function()
                 local ok, err = mare.ui.confirm("CancelConfirm?", true, { cancel = TOK3 })
                 R3 = { ok = ok, err = err }
               end)"#,
        )
        .await;
        t.until("confirm open", |s| s.contains("CancelConfirm?")).await;
        exec(&t.vt.host, "TOK3:cancel()").await;
        t.until("confirm closed by cancel", |s| !s.contains("CancelConfirm?")).await;
        assert!(eval::<bool>(&t.vt.host, "return type(R3) == 'table' and R3.ok == nil and R3.err == 'cancelled'").await);

        // 4. ui.select cancelled while on_open hangs -> modal closes and handle:is_open() == false
        exec(
            &t.vt.host,
            r#"TOK4 = mare.task.token()
               R4 = "pending"
               H_HANG = nil
               mare.task.spawn(function()
                 local i = mare.ui.select({
                   title = "HangOnOpen",
                   items = { "a", "b" },
                   cancel = TOK4,
                   on_open = function(sel)
                     H_HANG = sel
                     mare.task.sleep(60000)
                   end,
                 })
                 R4 = { i = i }
               end)"#,
        )
        .await;
        t.until("HangOnOpen open", |s| s.contains("HangOnOpen")).await;
        exec(&t.vt.host, "TOK4:cancel()").await;
        t.until("HangOnOpen closed by cancel", |s| !s.contains("HangOnOpen")).await;
        assert!(
            eval::<bool>(
                &t.vt.host,
                "return type(R4) == 'table' and R4.i == nil and H_HANG ~= nil and not H_HANG:is_open()"
            )
            .await
        );

        // 5. ui.select cancelled while on_highlight hangs -> modal closes cleanly
        exec(
            &t.vt.host,
            r#"TOK5 = mare.task.token()
               R5 = "pending"
               mare.task.spawn(function()
                 local i = mare.ui.select({
                   title = "HangHighlight",
                   items = { "a", "b" },
                   cancel = TOK5,
                   on_highlight = function()
                     mare.task.sleep(60000)
                   end,
                 })
                 R5 = { i = i }
               end)"#,
        )
        .await;
        t.until("HangHighlight open", |s| s.contains("HangHighlight")).await;
        exec(&t.vt.host, "TOK5:cancel()").await;
        t.until("HangHighlight closed by cancel", |s| !s.contains("HangHighlight")).await;
        assert!(eval::<bool>(&t.vt.host, "return type(R5) == 'table' and R5.i == nil").await);
    })
    .await
}

/// UI-ARGS: `ui.editor{prefill=...}` and `ui.input{default=...}` set initial editable text,
/// not `placeholder` (ref: `types/mare/ui.lua:387`, `ui.rs:540-555`).
#[tokio::test]
async fn ui_editor_prefill_and_input_default_set_initial_text() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);

        // 1. ui.editor prefill is returned on immediate Enter (not placeholder)
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 E1 = mare.ui.editor({ title = "EditPrefill", prefill = "hello world" })
               end)"#,
        )
        .await;
        t.until("editor open with prefill", |s| s.contains("EditPrefill") && s.contains("hello world")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("editor closed", |s| !s.contains("EditPrefill")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return E1").await.as_deref(), Some("hello world"));

        // 2. ui.editor prefill can be appended to
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 E2 = mare.ui.editor({ title = "EditAppend", prefill = "hello" })
               end)"#,
        )
        .await;
        t.until("editor 2 open", |s| s.contains("EditAppend") && s.contains("hello")).await;
        t.vt.type_str("!");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("editor 2 closed", |s| !s.contains("EditAppend")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return E2").await.as_deref(), Some("hello!"));

        // 3. ui.input default sets initial text
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 I1 = mare.ui.input({ title = "InputDefault", default = "abc", placeholder = "empty" })
               end)"#,
        )
        .await;
        t.until("input open with default", |s| s.contains("InputDefault") && s.contains("abc")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("input closed", |s| !s.contains("InputDefault")).await;
        assert_eq!(eval::<Option<String>>(&t.vt.host, "return I1").await.as_deref(), Some("abc"));
    })
    .await
}

/// After `suggest_after` consecutive similar approvals, permissions offers to widen the
/// preset with a "Not now, don't ask for 7 days" choice persisted to state.json.
#[tokio::test]
async fn permission_streak_offer_supports_seven_day_snooze() {
    local(async {
        let cfg = json!({
            "plugins": {
                "permissions": {
                    "preset": "read-only",
                    "suggest_after": 2
                }
            }
        });
        let mut t = Term::with(json!([]), cfg, 90, 26).await;
        expose(&t.vt.host);
        exec(&t.vt.host, "mare.trusted = true").await;
        t.vt.dismiss_notices();

        let emit_two = r#"
            local bus = C.cordis:get("bus")
            for i = 1, 2 do
                bus:emit("pipeline/approval-decided", {
                    call = { id = "call-" .. tostring(i) .. "-" .. tostring(os.time()), name = "write", arguments = { path = "a.txt" } },
                    decision = "allow",
                    scope = "once",
                })
            end
        "#;
        exec(&t.vt.host, emit_two).await;
        t.until("wider preset suggestion dialog", |s| {
            s.contains("similar requests") && s.contains("7 days")
        })
        .await;

        // Move down to the "Not now — don't ask for 7 days" item and select it.
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("suggestion dialog closed", |s| !s.contains("similar requests")).await;

        let snooze_until: i64 = eval(
            &t.vt.host,
            "local st = mare.state.read() or {}; return tonumber(st.permission_suggest_snooze_until) or 0",
        )
        .await;
        let now: i64 = eval(&t.vt.host, "return os.time()").await;
        let delta = snooze_until - now;
        assert!(
            (6 * 86400..=8 * 86400).contains(&delta),
            "expected ~7-day snooze persisted in state.json, got delta={delta}s"
        );

        // Another streak of 2 approvals while snoozed must not reopen the dialog.
        exec(&t.vt.host, emit_two).await;
        for _ in 0..10 {
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await; // sleep-ok: negative-assertion ticks confirming snoozed approval suggestion stays hidden
        }
        assert!(
            !t.screen().contains("similar requests"),
            "snoozed suggestion must not prompt again within 7 days:\n{}",
            t.screen()
        );
    })
    .await
}

#[tokio::test]
async fn permission_streak_offer_switches_to_wider_preset_when_accepted() {
    local(async {
        let cfg = json!({
            "plugins": {
                "permissions": {
                    "preset": "read-only",
                    "suggest_after": 2
                }
            }
        });
        let mut t = Term::with(json!([]), cfg, 90, 26).await;
        expose(&t.vt.host);
        t.vt.dismiss_notices();

        // 1. Untrusted workspace suppresses the wider-preset offer, then live TUI acceptance switches to auto.
        exec(
            &t.vt.host,
            r#"
            local bus = C.cordis:get("bus")
            local function emit_streak(prefix)
                for i = 1, 2 do
                    bus:emit("pipeline/approval-decided", {
                        call = { id = prefix .. "-" .. tostring(i), name = "write", arguments = { path = "a.txt" } },
                        decision = "allow",
                        scope = "once",
                    })
                end
            end

            mare.trusted = false
            emit_streak("untrusted")

            mare.trusted = true
            local orig_select = mare.ui.select
            local err_tx, err_rx = mare.task.channel()
            mare.ui.select = function()
                err_tx:send(true)
                error("select unavailable")
            end
            emit_streak("select-err")
            assert(err_rx:recv() == true)
            mare.task.sleep(20)
            mare.ui.select = orig_select

            emit_streak("accept-call")
            "#,
        )
        .await;
        t.until("wider preset suggestion dialog", |s| s.contains("similar requests") && s.contains("Switch to auto"))
            .await;

        // Accept choice 1 ("Switch to auto") and confirm any follow-up confirmation modal.
        // On Windows default tier `read-only` displays `READ` while `auto` displays `PARTIAL`;
        // on Unix with full sandbox `auto` displays `AUTO`.
        let expected_badge = if cfg!(windows) { "PARTIAL" } else { "AUTO" };
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("suggestion accepted or confirmation shown", |s| !s.contains("similar requests")).await;
        t.until("confirmation shown or auto preset active", |s| {
            s.contains(expected_badge)
                || s.contains("Allow")
                || s.contains("Yes")
                || s.contains("Confirm")
                || s.contains("Switch")
        })
        .await;
        if !t.screen().contains(expected_badge) {
            t.key(KeyCode::Enter, Modifiers::NONE);
        }
        t.until("statusline reflects auto preset", |s| s.contains(expected_badge)).await;
        let cur: String = eval(&t.vt.host, "return C.cordis:get('permissions'):preset()").await;
        assert_eq!(cur, "auto", "preset did not switch to auto; screen:\n{}", t.screen());

        // 2. Failed preset switch from auto -> full-access surfaces a warning notification.
        exec(
            &t.vt.host,
            r#"
            local bus = C.cordis:get("bus")
            local warned_tx, warned_rx = mare.task.channel()
            local orig_select = mare.ui.select
            local orig_notify = mare.ui.notify
            local orig_set_cur = mare.proc.sandbox.set_current
            mare.ui.select = function() return 1 end
            mare.proc.sandbox.set_current = function() error("simulated sandbox switch failure") end
            mare.ui.notify = function(msg, lvl)
                if tostring(msg):find("permission:", 1, true) and lvl == "warning" then
                    warned_tx:send(true)
                end
                return orig_notify(msg, lvl)
            end
            for i = 1, 2 do
                bus:emit("pipeline/approval-decided", {
                    call = { id = "switch-fail-" .. tostring(i), name = "write", arguments = { path = "a.txt" } },
                    decision = "allow",
                    scope = "once",
                })
            end
            assert(warned_rx:recv() == true)
            mare.proc.sandbox.set_current = orig_set_cur
            mare.ui.notify = orig_notify
            mare.ui.select = orig_select
            "#,
        )
        .await;
    })
    .await
}

#[tokio::test]
async fn permission_approval_dialog_shows_grant_scope_and_expiry() {
    local(async {
        let ext = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let ext_root = dunce::canonicalize(ext.path()).unwrap();
        let main_repo = ext_root.join("main");
        let linked_wt = ext_root.join("wt");
        let run_git = |dir: &std::path::Path, args: &[&str]| {
            let out = fixture_git(dir).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::create_dir_all(&main_repo).unwrap();
        run_git(&main_repo, &["init", "-q", "-b", "main"]);
        std::fs::write(main_repo.join("f.txt"), "v1\n").unwrap();
        run_git(&main_repo, &["add", "f.txt"]);
        run_git(&main_repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
        run_git(&main_repo, &["worktree", "add", "-q", linked_wt.to_str().unwrap(), "-b", "wt-branch"]);
        let wt_gitdir = dunce::canonicalize(main_repo.join(".git/worktrees/wt")).unwrap();
        let main_git = dunce::canonicalize(main_repo.join(".git")).unwrap();
        let file_parent = ext_root.join("out-dir");
        std::fs::create_dir_all(&file_parent).unwrap();
        let file_target = file_parent.join("target.txt");
        let long_cmd = format!("echo {} && true", "x".repeat(140));
        let script = json!([
            { "tool_calls": [{ "name": "request_permissions", "arguments": {
                "additional_permissions": { "write": [linked_wt.join(".git")] },
                "scope": "session",
                "justification": "fetch linked worktree"
            } }] },
            { "tool_calls": [{ "name": "bash", "arguments": {
                "command": long_cmd,
                "sandbox_permissions": format!("write:{}", file_target.display()),
                "justification": "write file in external dir"
            } }] },
            { "text": "done" }
        ]);
        let cfg = json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } });
        let mut t = Term::with(script, cfg, 120, 32).await;
        t.submit("fetch").await;
        // Structured body shows resolved physical worktree roots and grant scope/expiry without raw argument echo.
        let wt_gd_tail = wt_gitdir.file_name().unwrap().to_string_lossy().to_string();
        let main_git_s = main_git.display().to_string();
        let file_parent_s = format!("write:{}", file_parent.display());
        t.until("scoped permission dialog shows scope, expiry, and resolved physical worktree roots in body", |s| {
            let trimmed: Vec<&str> = s.lines().map(|l| l.trim().trim_matches('│').trim()).collect();
            let flat_paths = trimmed.join("");
            let flat_words = trimmed.join(" ");
            flat_words.contains("this session (a, until session ends or until the permission preset changes)")
                && flat_words.contains("reused by later commands until this turn ends")
                && flat_words.matches("until session ends or until the permission preset changes").count() == 1
                && s.contains("Allow requested access for this turn (y)")
                && s.contains("Allow requested access for this session (a)")
                && !s.contains("Grant scope:")
                && !s.contains("Requested write:")
                && flat_paths.contains(&wt_gd_tail)
                && flat_paths.contains(&main_git_s)
        })
        .await;
        t.key(KeyCode::Char('a'), Modifiers::NONE);
        t.until(
            "sandbox_permissions dialog shows resolved physical parent dir in body even with a long command",
            |s| {
                let trimmed: Vec<&str> = s.lines().map(|l| l.trim().trim_matches('│').trim()).collect();
                let flat_paths = trimmed.join("");
                let flat_words = trimmed.join(" ");
                !s.contains("Grant scope:")
                    && !s.contains("later commands return to baseline")
                    && flat_paths.contains(&file_parent_s)
                    && flat_words.contains("Allow once = this call only")
                    && flat_words.matches("Allow once = this call only").count() == 1
                    && s.contains("Allow once (y)")
                    && s.contains("Allow for this session (a)")
            },
        )
        .await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.idle().await;
    })
    .await
}

/// Approval dialogs show the full 300-character command (including its tail marker) and working
/// directory before approval, show only the first line in the title, clarify that the session
/// option applies to this exact command/call, and display argument summaries for tools without
/// `command`/`path`.
#[tokio::test]
async fn long_bash_approval_shows_tail_marker_cwd_exact_scope_and_generic_tool_args() {
    local(async {
        let padding = "segment_value_".repeat(20);
        let long_cmd = format!(
            "echo first_line_head && echo {padding} && echo TAIL_MARKER_300_END\necho second_line_hidden_in_title"
        );
        assert!(long_cmd.len() > 300);
        let script = json!([
            {
                "tool_calls": [{
                    "name": "bash",
                    "arguments": {
                        "command": long_cmd,
                        "workdir": "work-sub"
                    }
                }]
            },
            { "text": "bash denied" },
            {
                "tool_calls": [{
                    "name": "mcp__demo__query",
                    "arguments": {
                        "table": "orders",
                        "limit": 7
                    }
                }]
            },
            { "text": "mcp denied" }
        ]);
        let cfg = json!({
            "reduce_motion": true,
            "plugins": {
                "permissions": { "enabled": true, "preset": "read-only" }
            }
        });
        let mut t = Term::with(script, cfg, 110, 30).await;
        std::fs::create_dir_all(t._env.cwd.join("work-sub")).unwrap();
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            C.cordis:get("tools"):register("mcp__demo__query", {
                description = "demo mcp query",
                side_effect = "external",
                unconfined = true,
                parameters = { type = "object" },
                execute = function() return { content = "ok" } end,
            })
            "#,
        )
        .await;

        t.submit("run long bash").await;
        t.until("long bash approval dialog with preview", |screen| {
            screen.contains("Allow bash: echo first_line_head")
                && screen.contains("Allow this exact command for this session (a)")
        })
        .await;

        let screen = t.screen();
        let title_line = screen.lines().find(|l| l.contains("Allow bash:")).unwrap_or("");
        assert!(
            !title_line.contains("second_line_hidden_in_title") && !title_line.contains("TAIL_MARKER_300_END"),
            "dialog title must show only the first line:\n{title_line}"
        );
        let compact: String = screen
            .lines()
            .filter(|l| l.contains('│'))
            .map(|l| l.trim_matches(|c: char| c == ' ' || c == '│').trim())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            compact.contains("TAIL_MARKER_300_END"),
            "300-char command tail marker must be visible before approval:\n{screen}"
        );
        assert!(compact.contains("work-sub"), "working directory must be visible before approval:\n{screen}");

        // Deny bash, then verify non-command/path MCP tool shows its arguments.
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("why prompt", |s| s.contains("Tell the model why")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;

        t.submit("run mcp tool").await;
        t.until("mcp tool approval dialog with arguments", |screen| {
            screen.contains("Allow mcp__demo__query?")
                && screen.contains("Allow this exact call for this session (a)")
                && screen.contains("Arguments:")
        })
        .await;
        let mcp_screen = t.screen();
        assert!(
            mcp_screen.contains("\"table\":\"orders\"") && mcp_screen.contains("\"limit\":7"),
            "non-command/path tool must display argument summary:\n{mcp_screen}"
        );
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("why prompt 2", |s| s.contains("Tell the model why")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
    })
    .await
}
