//! Daily scenarios replaying every user-reported bug since v1.0.0.
//!
//! Each test replays the user-facing steps from the linked GitHub issue against the real
//! in-process TUI (`Term` + `vt100`), real PTY (`MareCmd::pty`), or real sandboxed `Host` so
//! every post-release regression stays covered by default offline `cargo nextest` runs.
//!
//! Covered issues: #13, #14, #18, #19, #20, #21, #39, #41, #42, #43, #44, #45, #47, #48, #57.

use std::time::{Duration, Instant};

use mare_tui::tty::{KeyCode, KeyEvent, Modifiers};
use serde_json::json;

use crate::common::*;
use crate::suite::tui::{Term, invariants, local};

async fn until_side_settled(t: &mut Term, what: &str, needle: &str) {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        t.step().await;
        let settled: bool = eval(
            &t.vt.host,
            r#"
            local c = C.cordis:get("conversations")
            local info = c and c:info()
            return info ~= nil and info.selected == true and info.busy == false
            "#,
        )
        .await;
        if settled && t.screen().contains(needle) {
            t.step().await;
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}; screen:\n{}", t.screen());
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

/// Issue #13: toggling `Ctrl+T` transcript overlay open and closed repeatedly must never leave
/// ghost prompt boxes stacked above the active input box.
#[tokio::test]
async fn scenario_issue_13_ctrl_t_transcript_toggle_never_duplicates_prompt_box() {
    local(async {
        let mut t = Term::new(json!([{ "text": "completed turn reply for issue 13" }]), 90, 24).await;
        t.submit("hello").await;
        t.idle().await;
        t.until("turn settled", |s| s.contains("completed turn reply for issue 13")).await;

        for (i, close_key) in [
            (KeyCode::Char('t'), Modifiers::CONTROL),
            (KeyCode::Escape, Modifiers::NONE),
            (KeyCode::Char('q'), Modifiers::NONE),
        ]
        .into_iter()
        .enumerate()
        {
            t.ctrl('t');
            t.until(&format!("transcript overlay opened (cycle {i})"), |s| s.contains("Transcript")).await;
            t.key(close_key.0, close_key.1);
            t.until(&format!("returned to main screen (cycle {i})"), |s| !s.contains("Transcript")).await;

            let saw = t.saw_welcome_banner;
            let obs = invariants::observe_term(&mut t, saw);
            invariants::check_no_duplicated_prompt_box(&obs)
                .unwrap_or_else(|e| panic!("cycle {i}: {e}\n{}", t.screen()));
        }
    })
    .await;
}

/// Issue #14: pressing `Ctrl+O` twice after a turn with tool output and reasoning commits to
/// scrollback expands on press 1 and returns to compact view on press 2.
#[tokio::test]
async fn scenario_issue_14_ctrl_o_second_press_returns_to_compact_view_across_scrollback() {
    local(async {
        let script = json!([
            {
                "reasoning": "REASONING_BODY_14",
                "tool_calls": [{ "name": "read", "arguments": { "path": "notes.txt" } }]
            },
            { "text": "read finished" }
        ]);
        let mut t = Term::build(script, json!({}), json!({}), 100, 28, |env| {
            let body: String = (1..=30).map(|i| format!("note line {i}\n")).collect();
            std::fs::write(env.cwd.join("notes.txt"), body).unwrap();
        })
        .await;

        t.submit("read notes.txt").await;
        t.idle().await;
        t.until("turn reply", |s| s.contains("read finished")).await;
        assert!(t.history().contains("Read 1 file"), "expected compact tool summary:\n{}", t.history());
        assert!(!t.history().contains("REASONING_BODY_14"), "reasoning should start collapsed:\n{}", t.history());

        tokio::time::pause();
        t.vt.reset_loop_state();

        // First Ctrl+O -> expanded across committed scrollback.
        t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('o'), Modifiers::CONTROL)));
        t.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
        let out = t.vt.output();
        t.parser.process(&out);
        let h_expanded = t.history();
        assert!(
            h_expanded.contains("Read 30 lines") && h_expanded.contains("REASONING_BODY_14"),
            "first Ctrl+O must expand committed scrollback:\n{h_expanded}"
        );

        // Second Ctrl+O -> returns to compact view cleanly.
        t.vt.send_tty_event(mare_tui::tty::Event::Key(KeyEvent::new(KeyCode::Char('o'), Modifiers::CONTROL)));
        t.owned.vt.run_production_loop_for(Duration::from_millis(300)).await;
        let out = t.vt.output();
        t.parser.process(&out);
        let s_compact = t.screen();
        let h_compact = t.history();
        assert!(
            s_compact.contains("Tool output: compact")
                && h_compact.contains("Read 1 file")
                && !h_compact.contains("Read 30 lines")
                && !h_compact.contains("REASONING_BODY_14"),
            "second Ctrl+O must collapse committed scrollback back to compact:\n{h_compact}"
        );
        tokio::time::resume();
    })
    .await;
}

/// Issue #18: after elevating the main session from an initial non-full preset to `full-access`,
/// a `general` subagent writing inside the repository must not prompt for write permission.
#[tokio::test]
async fn scenario_issue_18_subagent_inherits_full_access_without_prompting() {
    local(async {
        let parent_script = json!([
            {
                "tool_calls": [{
                    "name": "subagent",
                    "arguments": {
                        "type": "general",
                        "description": "write marker file",
                        "prompt": "Write subagent-18.txt",
                        "wait": true
                    }
                }]
            },
            { "text": "SUBAGENT_18_DONE" }
        ]);
        let child_turns = json!([
            {
                "tool_calls": [{
                    "name": "write",
                    "arguments": { "path": "subagent-18.txt", "content": "written-under-full-access\n" }
                }]
            },
            { "text": "child wrote file" }
        ]);

        let cfg = json!({
            "plugins": {
                "permissions": { "enabled": true, "preset": "read-only" }
            }
        });
        let mut t = Term::with(parent_script, cfg, 100, 28).await;
        let cwd = t._env.cwd.clone();
        expose(&t.vt.host);

        // Start untrusted / read-only, then elevate main session via `/permission full-access`.
        exec(&t.vt.host, "mare.trusted = false").await;
        t.submit("/permission full-access").await;
        t.until("full-access confirmation modal", |s| s.contains("Full access")).await;
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.until("FULL status active", |s| s.lines().last().is_some_and(|bar| bar.contains("FULL"))).await;

        exec(
            &t.vt.host,
            &format!(
                r#"
                local orig_start = mare.subagent.start
                local child_script = {child_script}
                mare.subagent.start = function(opts)
                    opts = opts or {{}}
                    opts.no_session = true
                    opts.config = {{
                        model = "faux/echo",
                        plugins = {{ ["provider-faux"] = {{ script_json = child_script }} }},
                    }}
                    return orig_start(opts)
                end
                "#,
                child_script = serde_json::to_string(&child_turns.to_string()).unwrap(),
            ),
        )
        .await;

        t.submit("spawn general subagent to write a file").await;
        t.idle().await;
        let hist = t.history();
        assert!(hist.contains("SUBAGENT_18_DONE"), "parent turn should finish cleanly:\n{hist}");
        assert!(
            !hist.contains("Allow write?") && !hist.contains("Allow request_permissions?"),
            "subagent must inherit full-access without prompting:\n{hist}"
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("subagent-18.txt")).unwrap_or_default(),
            "written-under-full-access\n"
        );
    })
    .await;
}

/// Issue #19: a subagent approval dialog shows the requester once (never `<name> · <name>`),
/// the full wrapped target path, purpose, scope/expiration, waiting status, and subagent-only deny.
#[tokio::test]
async fn scenario_issue_19_subagent_approval_dialog_shows_requester_path_and_waiting_status() {
    local(async {
        let long_rel_dir = "deeply/nested/issue19/segment_alpha/segment_beta/segment_gamma/artifacts_out";
        let parent_script = json!([
            {
                "tool_calls": [{
                    "name": "subagent",
                    "arguments": {
                        "description": "check the permission picker",
                        "prompt": "request write permission",
                        "wait": true
                    }
                }]
            },
            { "text": "parent completed issue 19" }
        ]);
        let child_turns = json!([
            {
                "tool_calls": [{
                    "name": "request_permissions",
                    "arguments": {
                        "additional_permissions": { "write": [long_rel_dir] },
                        "justification": "persist picker check output",
                        "scope": "turn"
                    }
                }]
            },
            { "text": "subagent finished" }
        ]);
        let cfg = json!({
            "reduce_motion": true,
            "plugins": {
                "permissions": { "enabled": true, "preset": "read-only" },
                "status": { "enabled": true }
            }
        });
        let mut t = Term::with(parent_script, cfg, 100, 30).await;
        std::fs::create_dir_all(t._env.cwd.join(long_rel_dir)).unwrap();
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            &format!(
                r#"
                local orig_start = mare.subagent.start
                local child_script = {child_script}
                mare.subagent.start = function(opts)
                    opts = opts or {{}}
                    opts.no_session = true
                    opts.permission = "read-only"
                    opts.config = {{
                        model = "faux/echo",
                        plugins = {{ ["provider-faux"] = {{ script_json = child_script }} }},
                    }}
                    return orig_start(opts)
                end
                "#,
                child_script = serde_json::to_string(&child_turns.to_string()).unwrap(),
            ),
        )
        .await;

        t.submit("start subagent to check permissions").await;
        t.until("subagent approval dialog open", |s| {
            s.contains("Subagent · check the permission picker — Allow request_permissions?")
                && s.contains("check the permission picker · Waiting for approval")
        })
        .await;

        let screen = t.screen();
        assert!(
            !screen.contains("check the permission picker · check the permission picker"),
            "requester name must not be duplicated:\n{screen}"
        );
        assert!(screen.contains("Write paths:"), "missing Write paths section:\n{screen}");
        assert!(screen.contains("artifacts_out"), "full target path must wrap without truncation:\n{screen}");
        assert!(screen.contains("Purpose: persist picker check output"), "missing purpose:\n{screen}");
        assert!(screen.contains("Scope: this turn (y, expires on turn end)"), "missing scope expiry:\n{screen}");
        assert!(
            screen.contains("affects only this subagent") && screen.contains("stops only this subagent"),
            "deny option must clarify subagent-only scope:\n{screen}"
        );

        // Deny cleanly (skip optional reason prompt with Enter) and verify the parent turn completes.
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("optional deny reason prompt", |s| s.contains("Tell the sub-agent why")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        assert!(t.history().contains("parent completed issue 19"), "parent turn should finish:\n{}", t.history());
    })
    .await;
}

/// Issue #20: in an untrusted folder, Shift+Tab steps `FULL -> READ` and pressing Shift+Tab again
/// opens the preset picker (allowing `AUTO`) instead of getting stuck with repeated warnings.
#[tokio::test]
async fn scenario_issue_20_shift_tab_opens_preset_picker_in_untrusted_folder() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        exec(&t.vt.host, "mare.trusted = false; P = C.cordis:get('permissions'); P:set('full-access')").await;
        t.step().await;

        // Step down FULL -> READ directly.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        t.until("stepped down to READ", |s| s.lines().last().is_some_and(|bar| bar.contains("READ"))).await;
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "read-only");

        // Pressing Shift+Tab from READ in an untrusted folder opens the preset picker.
        t.key(KeyCode::BackTab, Modifiers::SHIFT);
        t.until("untrusted preset picker opened", |s| {
            s.contains("Permissions (untrusted folder)") && s.contains("READ  read-only")
        })
        .await;

        // Selecting AUTO switches to auto without marking the folder trusted.
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("switched to AUTO via picker", |s| s.lines().last().is_some_and(|bar| bar.contains("AUTO"))).await;
        assert_eq!(eval::<String>(&t.vt.host, "return P:preset()").await, "auto");
        assert!(!eval::<bool>(&t.vt.host, "return mare.trusted").await, "folder must remain untrusted");
    })
    .await;
}

/// Issue #21: approving write access to an external repository's `.git` directory persists for
/// subsequent `git fetch` invocations within the grant scope.
#[cfg(unix)]
#[tokio::test]
async fn scenario_issue_21_approved_git_write_grant_persists_for_later_git_fetch() {
    local(async {
        let env = TestEnv::new().await;
        // Place external repos outside TMPDIR so Linux bwrap's private /tmp mount does not hide them.
        let ext_root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let ext_base = dunce::canonicalize(ext_root.path()).unwrap();
        let remote = ext_base.join("remote.git");
        let ext_repo = ext_base.join("ext-repo");

        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).output().expect("git available");
            assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        };
        run_git(&["init", "--bare", remote.to_str().unwrap()]);
        run_git(&["clone", remote.to_str().unwrap(), ext_repo.to_str().unwrap()]);

        let mut b = env
            .host()
            .builtins(&["tools-bash", "permissions"])
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "EXT_REPO", ext_repo.to_string_lossy().to_string());
        global(&h, "EXT_GIT", ext_repo.join(".git").to_string_lossy().to_string());

        let (grant_ok, fetch1_ok, fetch2_ok, second_prompted): (bool, bool, bool, bool) = eval(
            &h,
            r#"
            local P = C.cordis:get("permissions")
            local grant = P:request({ write = { EXT_GIT } }, {
                justification = "git fetch in external repo",
                scope = "session",
                approve = function() return { decision = "allow", scope = "session" } end,
            })
            local r1 = T:execute({
                id = "fetch-1",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch", EXT_REPO) },
            }, {})
            local prompted = false
            local r2 = T:execute({
                id = "fetch-2",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch", EXT_REPO) },
            }, {
                approve = function()
                    prompted = true
                    return { decision = "deny" }
                end,
            })
            return grant.refused ~= true, r1.is_error ~= true, r2.is_error ~= true, prompted
            "#,
        )
        .await;

        assert!(grant_ok, "write:.git grant must succeed");
        assert!(fetch1_ok, "first git fetch must succeed after approving write:.git");
        assert!(fetch2_ok, "subsequent git fetch must succeed under the same session grant");
        assert!(!second_prompted, "subsequent git fetch must not re-prompt during the active session grant");
        assert!(ext_repo.join(".git/FETCH_HEAD").exists(), "git fetch should have updated FETCH_HEAD");
        h.shutdown().await;
    })
    .await;
}

/// Issue #39: `/new` (and `/clear`) clears the screen and restores the welcome banner.
#[tokio::test]
async fn scenario_issue_39_new_clears_screen_and_restores_welcome_banner() {
    local(async {
        let mut t = Term::with(json!([{ "text": "OLD_TURN_REPLY_39" }]), json!({ "logo": true }), 100, 28).await;
        t.until("initial welcome banner", invariants::has_welcome_banner).await;

        t.submit("first message").await;
        t.idle().await;
        t.until("old reply rendered", |s| s.contains("OLD_TURN_REPLY_39")).await;

        let clears_before = t.vt.clear_count();
        t.submit("/new").await;
        t.until_vt("screen cleared on /new", |t| {
            t.vt.clear_count() > clears_before && invariants::has_welcome_banner(&t.screen())
        })
        .await;

        let screen = t.screen();
        assert!(!screen.contains("OLD_TURN_REPLY_39"), "old conversation must be cleared:\n{screen}");
        assert!(invariants::has_welcome_banner(&screen), "welcome banner must return after /new:\n{screen}");
    })
    .await;
}

/// Issue #41: `/side` and `/btw` render the assistant answer once, never duplicated.
#[tokio::test]
async fn scenario_issue_41_side_and_btw_render_answer_once() {
    local(async {
        let mut t = Term::new(
            json!([
                { "text": "main baseline" },
                { "text": "UNIQUE_SIDE_41", "delay_ms": 5 },
                { "text": "UNIQUE_BTW_41", "delay_ms": 5 }
            ]),
            90,
            24,
        )
        .await;
        expose(&t.vt.host);

        t.submit("hello").await;
        t.idle().await;

        t.submit("/side who are you").await;
        until_side_settled(&mut t, "streaming /side answer", "UNIQUE_SIDE_41").await;
        assert_eq!(t.screen().matches("UNIQUE_SIDE_41").count(), 1, "/side answer duplicated:\n{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("side closed", |s| !s.contains("SIDE · Esc")).await;

        t.submit("/btw quick check").await;
        until_side_settled(&mut t, "streaming /btw answer", "UNIQUE_BTW_41").await;
        assert_eq!(t.screen().matches("UNIQUE_BTW_41").count(), 1, "/btw answer duplicated:\n{}", t.screen());
    })
    .await;
}

/// Issue #42: `/btw` and `/side` answer cleanly on a brand-new empty session without failing with
/// `source session has no history`.
#[tokio::test]
async fn scenario_issue_42_side_and_btw_work_on_empty_session() {
    local(async {
        let mut t =
            Term::new(json!([{ "text": "EMPTY_SESSION_BTW_42" }, { "text": "EMPTY_SESSION_SIDE_42" }]), 90, 24).await;

        t.submit("/btw ask before any main turn").await;
        t.until("btw answer on empty session", |s| {
            s.contains("SIDE · Esc / /side back") && s.contains("EMPTY_SESSION_BTW_42")
        })
        .await;
        assert!(!t.history().contains("no history"), "unexpected no-history error:\n{}", t.history());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("side closed", |s| !s.contains("SIDE · Esc")).await;

        t.submit("/side second question on empty session").await;
        t.until("side answer on empty session", |s| {
            s.contains("SIDE · Esc / /side back") && s.contains("EMPTY_SESSION_SIDE_42")
        })
        .await;
        assert!(!t.history().contains("no history"), "unexpected no-history error:\n{}", t.history());
    })
    .await;
}

/// Issue #43: sub-second retry backoff displays a non-zero delay (`0.Xs`, never `0s`) on the
/// working line, and long warning toasts never wrap past their right border.
#[tokio::test]
async fn scenario_issue_43_retry_status_shows_nonzero_delay_and_toast_stays_in_border() {
    local(async {
        let drop_err = json!({
            "error": "stream ended before response.completed",
            "retryable": true,
            "kind": "stream_closed",
        });
        let mut t = Term::with(
            json!([drop_err, { "text": "RECOVERED_43" }]),
            json!({ "plugins": { "llm": { "retries": 3, "retry_base_ms": 800 } } }),
            100,
            24,
        )
        .await;
        t.vt.dismiss_notices();

        t.submit("trigger retry").await;
        t.until("retry indicator", |s| s.contains("Retrying (1/3) in 0.")).await;
        let retry_screen = t.screen();
        assert!(!retry_screen.contains("in 0s"), "sub-second delay must not show as 0s:\n{retry_screen}");
        t.idle().await;
        assert!(t.history().contains("RECOVERED_43"), "retried turn must succeed:\n{}", t.history());

        // A long warning toast must stay strictly within its box border.
        exec(
            &t.vt.host,
            r#"mare.ui.notify("Retrying (1) in 0.5s: stream ended before response.completed", "warning")"#,
        )
        .await;
        t.until("warning toast visible", |s| s.contains("Warning")).await;
        let saw = t.saw_welcome_banner;
        let obs = invariants::observe_term(&mut t, saw);
        invariants::check_global_invariants(&obs).unwrap_or_else(|e| panic!("toast border violation: {e}"));
    })
    .await;
}

/// Issue #44: large `edit`, `write` overwrite, and multi-file `apply_patch` diffs collapse cleanly
/// in compact view with `Ctrl+O to expand`, while `Ctrl+T` / `Ctrl+O` reveals the full diff.
#[tokio::test]
async fn scenario_issue_44_edit_write_and_apply_patch_diffs_collapse_in_compact_view() {
    local(async {
        let old_edit: String = (1..=40).map(|i| format!("old_line_{i}\n")).collect();
        let new_edit: String = (1..=40).map(|i| format!("new_edit_line_{i}\n")).collect();
        let new_write: String = (1..=40).map(|i| format!("rewritten_line_{i}\n")).collect();

        // 1. Default edit + write mode.
        let edit_write_script = json!([
            {
                "tool_calls": [
                    { "name": "read", "arguments": { "path": "big.txt" } },
                    { "name": "read", "arguments": { "path": "over.txt" } }
                ]
            },
            {
                "tool_calls": [
                    {
                        "name": "edit",
                        "arguments": { "path": "big.txt", "old_string": old_edit, "new_string": new_edit }
                    },
                    {
                        "name": "write",
                        "arguments": { "path": "over.txt", "content": new_write }
                    }
                ]
            },
            { "text": "DIFF_EDIT_WRITE_44_DONE" }
        ]);
        let mut t1 = Term::build(
            edit_write_script,
            json!({ "plugins": { "permissions": { "enabled": false } } }),
            json!({}),
            100,
            30,
            |env| {
                std::fs::write(env.cwd.join("big.txt"), &old_edit).unwrap();
                std::fs::write(env.cwd.join("over.txt"), &old_edit).unwrap();
            },
        )
        .await;

        t1.submit("apply large edit and write").await;
        t1.idle().await;
        t1.until("edit/write finished", |s| s.contains("DIFF_EDIT_WRITE_44_DONE")).await;

        let compact1 = t1.history();
        assert!(
            compact1.to_lowercase().contains("ctrl+o to expand"),
            "compact edit/write diffs must fold:\n{compact1}"
        );
        assert!(!compact1.contains("new_edit_line_40"), "tail of large edit diff should be folded:\n{compact1}");
        assert!(!compact1.contains("rewritten_line_40"), "tail of large write diff should be folded:\n{compact1}");
    })
    .await;

    // 2. Multi-file apply_patch mode.
    local(async {
        let patch_a: String = (1..=20).map(|i| format!("+patch_a_{i}\n")).collect();
        let patch_b: String = (1..=20).map(|i| format!("+patch_b_{i}\n")).collect();
        let patch =
            format!("*** Begin Patch\n*** Add File: pa.txt\n{patch_a}*** Add File: pb.txt\n{patch_b}*** End Patch\n");
        let patch_script = json!([
            { "tool_calls": [{ "name": "apply_patch", "arguments": { "input": patch } }] },
            { "text": "DIFF_PATCH_44_DONE" }
        ]);
        let mut t2 = Term::build(
            patch_script,
            json!({
                "plugins": {
                    "permissions": { "enabled": false },
                    "tools": { "edit_mode": "apply_patch" }
                }
            }),
            json!({}),
            100,
            30,
            |_| {},
        )
        .await;

        t2.submit("apply multi-file patch").await;
        t2.idle().await;
        t2.until("patch finished", |s| s.contains("DIFF_PATCH_44_DONE")).await;

        let compact2 = t2.history();
        assert!(
            compact2.to_lowercase().contains("ctrl+o to expand"),
            "compact apply_patch diffs must fold:\n{compact2}"
        );
        assert!(
            compact2.contains("pa.txt") && compact2.contains("pb.txt"),
            "multi-file patch headers missing:\n{compact2}"
        );
        assert!(!compact2.contains("patch_b_20"), "tail of multi-file patch should be folded:\n{compact2}");

        // Opening Ctrl+T transcript shows the full multi-file diff tail.
        t2.ctrl('t');
        t2.until("expanded transcript shows full diff tail", |s| s.contains("patch_b_20")).await;
        t2.key(KeyCode::Escape, Modifiers::NONE);
        t2.until("transcript closed", |s| !s.contains("patch_b_20") && s.contains("Ask mare")).await;
    })
    .await;
}

/// Issue #45: rendering a Markdown fenced code block with nested fences must never panic inside
/// `syntect` regex compilation (`TargetNotRepeatable`).
#[tokio::test]
async fn scenario_issue_45_markdown_fenced_code_block_highlights_without_panic() {
    local(async {
        let md_reply = "Here is a nested Markdown example:\n\n````markdown\n```rust\nfn issue_45() -> u32 {\n    45\n}\n```\n~~~\ntilde fence\n~~~\n````\n\nHIGHLIGHT_OK_45";
        let mut t = Term::new(json!([{ "text": md_reply }]), 100, 28).await;

        t.submit("show nested markdown code block").await;
        t.idle().await;
        t.until("highlighted markdown rendered", |s| s.contains("HIGHLIGHT_OK_45")).await;
        let hist = t.history();
        assert!(hist.contains("fn issue_45()"), "code block content missing:\n{hist}");
    })
    .await;
}

/// Issue #47: when a read boundary splits bracketed-paste start (`ESC [200~`) immediately after
/// the leading `ESC` byte, the PTY input decoder must treat it as a paste rather than `Esc` plus
/// literal `[200~` typed characters.
#[cfg(unix)]
#[test]
fn scenario_issue_47_bracketed_paste_split_after_esc_handled_as_paste() {
    let mut p = MareCmd::new()
        .faux(json!([]))
        .setup(r#"mare.plugin("onboarding", { plugins = false })"#)
        .no_term_probe()
        .pty(100, 30);

    p.wait_text("input box ready", |s| s.contains('╰'));

    // Split ESC [200~ across two separate PTY writes right after the lone ESC byte.
    p.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(5));
    p.send(b"[200~SPLIT_PASTE_OK_47\x1b[201~");

    p.wait_text("pasted text in input box", |s| s.contains("SPLIT_PASTE_OK_47"));
    let out = p.text();
    assert!(
        !out.contains("[200~SPLIT_PASTE_OK_47"),
        "split ESC before bracketed paste leaked raw [200~ into editor:\n{out}"
    );

    // Clear input and quit cleanly.
    p.send(b"\x15");
    p.send(b"\x04");
    let _ = p.wait_exit(15);
}

/// Issue #48: an async `/side` failure backfill arriving while `$EDITOR` owns the terminal must
/// not be dropped or overwrite the text written in the external editor.
#[cfg(unix)]
#[tokio::test]
async fn scenario_issue_48_external_editor_preserves_async_side_backfill() {
    struct ReleaseGuard(std::path::PathBuf);
    impl Drop for ReleaseGuard {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, "release");
        }
    }

    local(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            let dir = tempfile::tempdir().unwrap();
            let ready = dir.path().join("ready");
            let release = dir.path().join("release");
            let _guard = ReleaseGuard(release.clone());
            let script = dir.path().join("editor.sh");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\ntouch '{}'\ni=0\nwhile [ ! -f '{}' ] && [ \"$i\" -lt 500 ]; do sleep 0.02; i=$((i + 1)); done\nprintf 'edited in external editor\\n' > \"$1\"\n",
                    ready.display(),
                    release.display(),
                ),
            )
            .unwrap();
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            // No provider is configured so `/side` fails to start ("No model available") and
            // backfills `/side failing backfill 48` via `{ prepend = true }` while `$EDITOR` is open.
            let mut t =
                Term::without_model(json!({ "editor": script.to_string_lossy().as_ref() }), 90, 24).await;
            expose(&t.vt.host);
            t.until_vt("startup settled", |t| !t.vt.starting()).await;
            assert!(!t.vt.has_modal(), "unexpected startup modal before Ctrl+G:\n{}", t.screen());

            // Open external editor via Ctrl+G and wait until it owns the terminal.
            t.ctrl('g');
            t.until_vt("external editor started", |_| ready.exists()).await;

            exec(&t.vt.host, r#"C.cordis:get("commands"):run("/side failing backfill 48")"#).await;
            t.vt.drain_cmds();
            assert_ne!(
                t.vt.editor_text(),
                "/side failing backfill 48",
                "backfill must wait until external editor exits"
            );

            // Release the external editor and verify both texts are preserved.
            std::fs::write(&release, "go").unwrap();
            let end = Instant::now() + Duration::from_secs(15);
            while t.vt.editor_text() != "/side failing backfill 48\n\nedited in external editor" {
                assert!(
                    Instant::now() < end,
                    "backfill + editor text never merged; editor_text={:?}\nscreen:\n{}",
                    t.vt.editor_text(),
                    t.screen()
                );
                t.step().await;
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        })
        .await
        .expect("scenario_issue_48 timed out");
    })
    .await;
}

/// Issue #57: running `/reload` while a model response is mid-stream must display a clean
/// user-facing busy notice without surfacing a raw Lua `stack traceback:`.
#[tokio::test]
async fn scenario_issue_57_reload_mid_stream_shows_clean_notice_without_traceback() {
    local(async {
        let script = json!([{
            "text": "stream alpha\n\nstream beta\n\nstream gamma\n\nstream delta\n\nSTREAM_57_DONE",
            "delay_ms": 150
        }]);
        let mut t = Term::with_all_builtins(script, json!({}), json!({}), 100, 28, |_| {}).await;
        t.until_vt("startup settled", |t| !t.vt.starting()).await;

        t.submit("start slow stream").await;
        t.until_vt("stream active", |t| t.vt.busy() && t.vt.live_source().contains("stream alpha")).await;

        let flat =
            |s: &str| s.replace(['╭', '╮', '╰', '╯', '│', '─'], " ").split_whitespace().collect::<Vec<_>>().join(" ");
        let expected = "/reload isn't available while a model request is active (Esc stops it)";
        t.submit("/reload").await;
        t.until("reload busy notice", |s| {
            let norm = flat(s);
            norm.contains(expected) || norm.contains("stack traceback") || norm.contains("Reload failed")
        })
        .await;

        let screen = t.screen();
        let norm = flat(&screen);
        assert!(
            norm.contains(expected),
            "/reload mid-stream did not show expected busy notice `{expected}`:\n{screen}"
        );
        assert!(
            !norm.contains("stack traceback") && !norm.contains("traceback:") && !norm.contains("runtime error:"),
            "/reload mid-stream surfaced a raw Lua traceback:\n{screen}"
        );
        t.idle().await;
        t.until_vt("stream completed", |t| t.history().contains("STREAM_57_DONE")).await;
    })
    .await;
}
