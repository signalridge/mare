//! Command × session-state TUI screen matrix (faux provider, real in-process TUI).
//!
//! Runs every built-in slash command and key entry point across 5 session states
//! (`Empty`, `AfterTurn`, `MidStream`, `AfterCompaction`, `AfterResume`), asserting user-visible
//! screen properties and global screen invariants.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionState {
    Empty,
    AfterTurn,
    MidStream,
    AfterCompaction,
    AfterResume,
}

impl SessionState {
    const ALL: [Self; 5] = [Self::Empty, Self::AfterTurn, Self::MidStream, Self::AfterCompaction, Self::AfterResume];

    fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::AfterTurn => "after_turn",
            Self::MidStream => "mid_stream",
            Self::AfterCompaction => "after_compaction",
            Self::AfterResume => "after_resume",
        }
    }
}

const SESSION_RESET_COMMANDS: &[&str] = &["/new"];
const SIDE_COMMANDS: &[&str] = &["/side", "/btw"];
const SESSION_CORE_COMMANDS: &[&str] = &[
    "/resume",
    "/model",
    "/compact",
    "/rewind",
    "/diff",
    "/undo",
    "/retry",
    "/tree",
    "/fork",
    "/clone",
    "/name matrix-run",
    "/label matrix-tag",
    "/delete",
    "/continue",
    "/session",
    "/init",
    "/export",
    "/reload",
];
const INFO_AND_PICKER_COMMANDS: &[&str] = &[
    "/help",
    "/status",
    "/config",
    "/commands",
    "/skills",
    "/show-reasoning",
    "/effort",
    "/doctor",
    "/theme",
    "/terminal-setup",
    "/memory",
    "/hotkeys",
    "/login",
    "/logout",
    "/add-dir",
    "/debug",
    "/context",
    "/plugins",
    "/news",
    "/mcp",
    "/web",
    "/usage",
    "/statusline",
    "/profile",
    "/lua 6 * 7",
    "/jobs",
    "/review",
    "/permission",
    "/yolo",
    "/approval",
    "/notifications",
    "/copy",
];
const AGENTS_COMMAND: &str = "/agents";
const ARCHIVE_COMMAND: &str = "/archive";
const EXIT_COMMAND: &str = "/exit";

fn state_script(state: SessionState, extra_turns: &[Value]) -> Value {
    let mut turns = match state {
        SessionState::Empty => vec![],
        SessionState::AfterTurn => vec![json!({ "text": "matrix reply" })],
        SessionState::MidStream => vec![json!({
            "text": "stream chunk alpha\n\nstream chunk beta\n\nstream chunk gamma\n\nstream chunk delta\n\nSTREAM_DONE",
            "delay_ms": 80
        })],
        SessionState::AfterCompaction => {
            let a1 = format!("compacted turn one {}", "work ".repeat(800));
            vec![
                json!({ "text": a1 }),
                json!({ "text": "compacted turn two" }),
                json!({ "text": "## Summary\nCompacted summary text." }),
            ]
        }
        SessionState::AfterResume => vec![],
    };
    turns.extend(extra_turns.iter().cloned());
    Value::Array(turns)
}

async fn setup_state(state: SessionState, extra_turns: &[Value], extra_config: Value) -> Term {
    let mut cfg = json!({
        "logo": true,
        "plugins": {
            "permissions": { "enabled": true },
            "onboarding": { "plugins": false, "extras": false },
            "compact": { "keep_recent_tokens": 100, "recent_user_tokens": 0 }
        }
    });
    merge(&mut cfg, extra_config);
    let script = state_script(state, extra_turns);
    let args = if state == SessionState::AfterResume { json!({ "resume": "s1" }) } else { json!({}) };
    let mut t = Term::with_all_builtins(script, cfg, args, 100, 30, |env| {
        env.set_var("MARE_TEST_CLIPBOARD", "osc52");
        if state == SessionState::AfterResume {
            env.write_session(
                "2026-01-01T00-00-00_matrix.jsonl",
                &[msg("m1", None, "user", "resumed prompt"), msg("m2", Some("m1"), "assistant", "resumed reply")],
            );
        }
    })
    .await;

    t.until_vt("startup settled", |t| !t.vt.starting()).await;

    match state {
        SessionState::Empty => {
            t.until("welcome banner", invariants::has_welcome_banner).await;
        }
        SessionState::AfterTurn => {
            t.submit("matrix prompt").await;
            t.idle().await;
            t.until("matrix reply visible", |s| s.contains("matrix reply")).await;
        }
        SessionState::MidStream => {
            t.submit("stream prompt").await;
            t.until_vt("stream started", |t| t.vt.busy() && t.vt.live_source().contains("stream chunk alpha")).await;
        }
        SessionState::AfterCompaction => {
            t.submit("first long turn").await;
            t.idle().await;
            let keep_turn_two = format!("second long turn {}", "keep ".repeat(200));
            t.submit(&keep_turn_two).await;
            t.idle().await;
            t.submit("/compact").await;
            t.until_vt("compaction settled", |t| !t.vt.busy() && t.history().contains("Compacted")).await;
        }
        SessionState::AfterResume => {
            t.until("resumed reply visible", |s| s.contains("resumed reply")).await;
        }
    }
    t.vt.dismiss_notices();
    t.step().await;
    t
}

fn check_no_lua_or_error_chrome(screen: &str) -> Result<(), String> {
    let normalized: String = screen.replace('│', " ").split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.contains("traceback") {
        return Err(format!("Lua traceback visible on screen:\n{screen}"));
    }
    Ok(())
}

async fn assert_input_usable(t: &mut Term, wait_idle: bool) -> Result<(), String> {
    if t.vt.quit_requested() {
        return Err(format!("unexpected quit requested before input probe:\n{}", t.screen()));
    }
    if wait_idle && t.vt.busy() {
        let end = Instant::now() + WAIT;
        while t.vt.busy() {
            if Instant::now() >= end {
                return Err(format!("busy turn never finished before input probe:\n{}", t.screen()));
            }
            t.step().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
    // Dismiss any open modal/overlay first.
    for _ in 0..3 {
        if !t.vt.has_modal() && !t.vt.transcript_open() {
            break;
        }
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
    }
    if t.vt.has_modal() || t.vt.transcript_open() {
        return Err(format!("could not close overlay/modal to reach editor:\n{}", t.screen()));
    }
    let existing = t.vt.editor_text();
    if !existing.is_empty() {
        t.ctrl('u');
        t.step().await;
    }
    t.vt.type_str("ok");
    t.step().await;
    let got = t.vt.editor_text();
    if got != "ok" {
        return Err(format!("editor input box unusable (got {got:?}):\n{}", t.screen()));
    }
    t.ctrl('u');
    t.step().await;
    Ok(())
}

fn assert_matrix_cell(cell_name: &str, state: SessionState, result: Result<(), String>) {
    if let Err(err) = result {
        panic!("matrix cell [{cell_name} @ {}] failed:\n{err}", state.label());
    }
}

async fn run_slash_and_settle(t: &mut Term, cmd: &str) {
    t.vt.type_str(cmd);
    t.key(KeyCode::Enter, Modifiers::NONE);
    t.step().await;
    // Allow async command tasks (like /doctor, /diff, /resume, /compact, /agents) a brief settle.
    for _ in 0..8 {
        t.step().await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn check_standard_cell(
    t: &mut Term,
    wait_idle: bool,
    screen_check: impl FnOnce(&mut Term) -> Result<(), String>,
) -> Result<(), String> {
    let screen = t.screen();
    check_no_lua_or_error_chrome(&screen)?;
    screen_check(t)?;
    t.check_invariants()?;
    assert_input_usable(t, wait_idle).await?;
    t.check_invariants()?;
    Ok(())
}

#[tokio::test]
async fn matrix_covers_every_builtin_slash_command() {
    local(async {
        let t = setup_state(SessionState::Empty, &[], json!({})).await;
        expose(&t.vt.host);
        let registered: Vec<String> = eval(
            &t.vt.host,
            r#"
            local out = {}
            local cmds = C.cordis:get("commands")
            for _, d in ipairs(cmds:list().data) do
                if not d.alias_of and string.sub(d.name, 1, 6) ~= "skill:" then
                    out[#out + 1] = d.name
                end
            end
            table.sort(out)
            return out
            "#,
        )
        .await;
        let expected: BTreeSet<&str> = SESSION_RESET_COMMANDS
            .iter()
            .chain(SIDE_COMMANDS.iter())
            .chain(SESSION_CORE_COMMANDS.iter())
            .chain(INFO_AND_PICKER_COMMANDS.iter())
            .chain([AGENTS_COMMAND, ARCHIVE_COMMAND, EXIT_COMMAND].iter())
            .map(|cmd| cmd.trim_start_matches('/').split_whitespace().next().unwrap())
            .collect();
        let missing: Vec<&str> =
            registered.iter().map(String::as_str).filter(|name| !expected.contains(name)).collect();
        let extra: Vec<&str> = expected.iter().copied().filter(|name| !registered.iter().any(|r| r == name)).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "built-in slash command mismatch: missing={missing:?} extra={extra:?} (registered: {registered:?})"
        );
    })
    .await
}

#[tokio::test]
async fn matrix_new_and_clear_redraw_welcome_banner_across_states() {
    local(async {
        for cmd in SESSION_RESET_COMMANDS.iter().copied().chain(["/clear"]) {
            for state in SessionState::ALL {
                let mut t = setup_state(state, &[], json!({})).await;
                run_slash_and_settle(&mut t, cmd).await;
                // Let pending mid-stream turn finish stopping before checking cleared banner.
                t.until_vt("new session settled", |t| !t.vt.busy()).await;
                let res = check_standard_cell(&mut t, true, |t| {
                    let hist = t.history();
                    if !invariants::has_welcome_banner(&hist) {
                        return Err(format!("{cmd} did not redraw welcome banner:\n{hist}"));
                    }
                    Ok(())
                })
                .await;
                assert_matrix_cell(cmd, state, res);
            }
        }
    })
    .await
}

#[tokio::test]
async fn matrix_side_and_btw_across_states() {
    local(async {
        for cmd in SIDE_COMMANDS {
            for state in SessionState::ALL {
                let mut t = setup_state(state, &[json!({ "text": "SIDE_MATRIX_REPLY_91" })], json!({})).await;
                expose(&t.vt.host);
                exec(
                    &t.vt.host,
                    "SIDE_SETTLED = false; C.cordis:on('conversation/settled', function() SIDE_SETTLED = true end)",
                )
                .await;
                let line = format!("{cmd} What is 2+2?");
                t.vt.type_str(&line);
                t.key(KeyCode::Enter, Modifiers::NONE);
                t.step().await;

                // Wait until either the side turn settles after showing its reply or an error notice shows up.
                let mut warning_screen: Option<String> = None;
                let end = Instant::now() + Duration::from_secs(10);
                while Instant::now() < end {
                    t.step().await;
                    let s = t.screen();
                    if warning_screen.is_none() && s.contains("⚠ Warning") {
                        warning_screen = Some(s.clone());
                    }
                    if s.contains("session.no_history")
                        || s.contains("no history")
                        || s.contains("Send a main conversation message")
                    {
                        break;
                    }
                    if s.contains("SIDE_MATRIX_REPLY_91")
                        && eval::<bool>(&t.vt.host, "return SIDE_SETTLED == true").await
                    {
                        t.step().await;
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }

                let res = (|| {
                    let s = t.screen();
                    if s.contains("session.no_history")
                        || s.contains("no history")
                        || s.contains("Send a main conversation message")
                    {
                        return Err(format!("{cmd} failed on {} state:\n{s}", state.label()));
                    }
                    if !s.contains("SIDE_MATRIX_REPLY_91") {
                        let shown = warning_screen.as_deref().unwrap_or(&s);
                        return Err(format!("{cmd} never rendered side answer:\n{shown}"));
                    }
                    if s.matches("SIDE_MATRIX_REPLY_91").count() != 1 {
                        return Err(format!(
                            "{cmd} rendered side answer {} times (expected 1):\n{s}",
                            s.matches("SIDE_MATRIX_REPLY_91").count()
                        ));
                    }
                    t.check_invariants()?;
                    Ok(())
                })();

                assert_matrix_cell(cmd, state, res);

                // Cleanly close side conversation and verify the main editor input remains usable.
                t.key(KeyCode::Escape, Modifiers::NONE);
                t.step().await;
                let input_res = assert_input_usable(&mut t, false).await;
                assert_matrix_cell(cmd, state, input_res);
            }
        }
    })
    .await
}

#[tokio::test]
async fn matrix_ctrl_t_transcript_toggle_across_states() {
    local(async {
        for state in SessionState::ALL {
            let mut t = setup_state(state, &[], json!({})).await;
            t.ctrl('t');
            t.until_vt("transcript overlay open", |t| t.vt.transcript_open()).await;
            t.key(KeyCode::Escape, Modifiers::NONE);
            t.until_vt("returned to main screen", |t| !t.vt.transcript_open()).await;
            let res = check_standard_cell(&mut t, false, |t| {
                let obs = invariants::observe_term(t, true);
                invariants::check_no_duplicated_prompt_box(&obs)
            })
            .await;
            assert_matrix_cell("ctrl+t", state, res);
        }
    })
    .await
}

#[tokio::test]
async fn matrix_ctrl_o_expand_toggle_across_states() {
    local(async {
        for state in SessionState::ALL {
            let mut t = setup_state(state, &[], json!({})).await;
            if state != SessionState::Empty {
                // Ensure a committed tool/reasoning block exists so Ctrl+O exercises committed toggle.
                exec(
                    &t.vt.host,
                    r#"
                    local id = mare.ui.push("reasoning", {
                        node = "compact thought summary",
                        expanded = "EXPANDED_THOUGHT_BODY",
                    })
                    mare.ui.finish(id)
                    "#,
                )
                .await;
                t.until("reasoning committed", |s| s.contains("compact thought summary")).await;
            }
            // First Ctrl+O: expand
            t.ctrl('o');
            t.step().await;
            // Second Ctrl+O: return to compact view
            t.ctrl('o');
            t.step().await;

            let res = check_standard_cell(&mut t, false, |t| {
                if state != SessionState::Empty {
                    if t.vt.transcript_open() {
                        return Err(format!("second Ctrl+O left transcript overlay open:\n{}", t.screen()));
                    }
                    if t.vt.expand_all() {
                        return Err(format!("second Ctrl+O left expand_all=true:\n{}", t.screen()));
                    }
                    if t.history().contains("EXPANDED_THOUGHT_BODY") {
                        return Err(format!("second Ctrl+O did not re-collapse committed block:\n{}", t.history()));
                    }
                }
                Ok(())
            })
            .await;
            assert_matrix_cell("ctrl+o", state, res);
        }
    })
    .await
}

#[tokio::test]
async fn matrix_edit_write_patch_diffs_collapse_in_compact_view() {
    local(async {
        let big_body: String = (1..=120).map(|i| format!("line_{i:03} = true\n")).collect();
        let overwritten_body: String = (1..=120).map(|i| format!("rewritten_{i:03} = false\n")).collect();
        let old_block: String = (1..=80).map(|i| format!("rewritten_{i:03} = false\n")).collect();
        let new_block: String = (1..=80).map(|i| format!("edited_{i:03} = false\n")).collect();
        let mut patch = String::from("*** Begin Patch\n*** Update File: big.txt\n@@\n");
        for i in 1..=80 {
            let _ = writeln!(patch, "-line_{i:03} = true");
        }
        for i in 1..=80 {
            let _ = writeln!(patch, "+patched_{i:03} = 1");
        }
        patch.push_str("*** End Patch\n");

        let mk_cfg = |edit_mode: &str| {
            json!({
                "logo": false,
                "plugins": {
                    "tools": { "edit_mode": edit_mode },
                    "permissions": { "enabled": true, "preset": "full-access", "untrusted_preset": "full-access" },
                    "onboarding": { "plugins": false, "extras": false }
                }
            })
        };

        // 1. Replace mode: write (new + overwrite) and edit diffs collapse in compact view.
        {
            let mut t = Term::with_all_builtins(
                json!([
                    { "tool_calls": [{ "name": "write", "arguments": { "path": "big.txt", "content": big_body } }] },
                    { "text": "wrote big file" },
                    { "tool_calls": [{ "name": "write", "arguments": { "path": "big.txt", "content": overwritten_body } }] },
                    { "text": "rewrote big file" },
                    { "tool_calls": [{ "name": "edit", "arguments": { "path": "big.txt", "old_string": old_block, "new_string": new_block } }] },
                    { "text": "edited big file" }
                ]),
                mk_cfg("replace"),
                json!({}),
                100,
                30,
                |_| {},
            )
            .await;

            t.submit("write big.txt").await;
            t.idle().await;
            let res = check_standard_cell(&mut t, true, |t| {
                let hist = t.history();
                if !hist.contains("wrote big file") || !hist.contains("Wrote big.txt") {
                    return Err(format!("new-file write did not render summary:\n{hist}"));
                }
                if hist.contains("line_070 = true") {
                    return Err(format!("new-file write dumped full content:\n{hist}"));
                }
                Ok(())
            })
            .await;
            assert_matrix_cell("diff_collapse_write_new", SessionState::AfterTurn, res);

            for (prompt, done_marker, needle) in [
                ("rewrite big.txt", "rewrote big file", "rewritten_070 = false"),
                ("edit big.txt", "edited big file", "edited_070 = false"),
            ] {
                t.submit(prompt).await;
                t.idle().await;
                let res = check_standard_cell(&mut t, true, |t| {
                    let hist = t.history();
                    if !hist.contains(done_marker) || !hist.contains("big.txt") {
                        return Err(format!("{prompt} did not render tool header + reply:\n{hist}"));
                    }
                    if hist.contains(needle) || !hist.contains("more line") {
                        return Err(format!("diff ({prompt}) did not collapse in compact view:\n{hist}"));
                    }
                    Ok(())
                })
                .await;
                assert_matrix_cell("diff_collapse_replace", SessionState::AfterTurn, res);
            }
        }

        // 2. apply_patch mode: apply_patch diff in compact view.
        {
            let mut t = Term::with_all_builtins(
                json!([
                    { "tool_calls": [{ "name": "write", "arguments": { "path": "big.txt", "content": big_body } }] },
                    { "text": "wrote big file" },
                    { "tool_calls": [{ "name": "apply_patch", "arguments": { "input": patch } }] },
                    { "text": "patched big file" }
                ]),
                mk_cfg("apply_patch"),
                json!({}),
                100,
                30,
                |_| {},
            )
            .await;
            t.submit("write big.txt").await;
            t.idle().await;
            t.submit("patch big.txt").await;
            t.idle().await;
            let res = check_standard_cell(&mut t, true, |t| {
                let hist = t.history();
                if !hist.contains("patched big file") || !hist.contains("big.txt") {
                    return Err(format!("apply_patch did not render tool header + reply:\n{hist}"));
                }
                if hist.contains("patched_070 = 1") || !hist.contains("more line") {
                    return Err(format!("apply_patch diff did not collapse in compact view:\n{hist}"));
                }
                Ok(())
            })
            .await;
            assert_matrix_cell("diff_collapse_apply_patch", SessionState::AfterTurn, res);
        }
    })
    .await
}

#[tokio::test]
async fn matrix_retry_notice_delay_and_border() {
    local(async {
        let mut t = Term::with_all_builtins(
            json!([
                { "error": "stream ended before response.completed from upstream provider edge", "retryable": true },
                { "text": "recovered answer" }
            ]),
            json!({ "logo": false, "plugins": { "llm": { "retry_base_ms": 440 }, "onboarding": { "plugins": false, "extras": false } } }),
            json!({}),
            110,
            24,
            |_| {},
        )
        .await;
        t.submit("hello").await;
        t.until("retry notice", |s| s.contains("Retrying")).await;
        let saw_zero_sec = t.screen().contains("in 0s");
        let border_err = t.check_invariants().err();
        t.idle().await;
        let res = (|| {
            if saw_zero_sec {
                return Err("retry notice displayed `in 0s`".to_string());
            }
            if let Some(e) = border_err {
                return Err(e);
            }
            if t.history().matches("recovered answer").count() != 1 {
                return Err(format!("expected recovered answer once:\n{}", t.history()));
            }
            Ok(())
        })();
        assert_matrix_cell("retry_notice", SessionState::MidStream, res);
    })
    .await
}

#[tokio::test]
async fn matrix_shift_tab_across_states() {
    local(async {
        for state in SessionState::ALL {
            let mut t = setup_state(state, &[], json!({})).await;
            t.key(KeyCode::BackTab, Modifiers::SHIFT);
            t.step().await;
            let res = check_standard_cell(&mut t, false, |_| Ok(())).await;
            assert_matrix_cell("shift+tab", state, res);
        }
    })
    .await
}

#[tokio::test]
async fn matrix_subagent_spawn_across_states() {
    local(async {
        let perms = json!({
            "agents": { "max_depth": 1 },
            "plugins": {
                "permissions": { "enabled": true, "preset": "full-access", "untrusted_preset": "full-access" }
            }
        });
        for state in SessionState::ALL {
            let mut t = if state == SessionState::MidStream {
                let mut t = Term::with_all_builtins(
                    json!([
                        { "tool_calls": [{ "name": "subagent", "arguments": { "prompt": "child work", "name": "matrix-worker", "wait": false } }] },
                        { "text": "stream chunk alpha\n\nstream chunk beta\n\nstream chunk gamma\n\nSTREAM_DONE", "delay_ms": 80 }
                    ]),
                    json!({
                        "logo": true,
                        "agents": { "max_depth": 1 },
                        "plugins": {
                            "permissions": { "enabled": true, "preset": "full-access", "untrusted_preset": "full-access" },
                            "onboarding": { "plugins": false, "extras": false }
                        }
                    }),
                    json!({}),
                    100,
                    30,
                    |_| {},
                )
                .await;
                t.until_vt("startup settled", |t| !t.vt.starting()).await;
                t.vt.dismiss_notices();
                t.step().await;
                t.submit("spawn while streaming").await;
                t.until_vt("stream started", |t| {
                    t.vt.busy() && t.vt.live_source().contains("stream chunk alpha")
                })
                .await;
                t
            } else {
                let mut t = setup_state(
                    state,
                    &[
                        json!({ "tool_calls": [{ "name": "subagent", "arguments": { "prompt": "child work", "name": "matrix-worker", "wait": true } }] }),
                        json!({ "text": "parent done" }),
                    ],
                    perms.clone(),
                )
                .await;
                t.submit("spawn worker").await;
                t.idle().await;
                t
            };
            run_slash_and_settle(&mut t, AGENTS_COMMAND).await;
            let res = check_standard_cell(&mut t, state != SessionState::MidStream, |t| {
                if !t.screen().contains("matrix-worker") {
                    return Err(format!("expected spawned subagent in /agents:\n{}", t.screen()));
                }
                Ok(())
            })
            .await;
            assert_matrix_cell("subagent_spawn", state, res);
        }
    })
    .await
}

async fn run_session_core_commands_for_state(state: SessionState) {
    for cmd in SESSION_CORE_COMMANDS {
        let extra =
            if *cmd == "/init" || *cmd == "/retry" { vec![json!({ "text": "followup reply" })] } else { vec![] };
        let mut t = setup_state(state, &extra, json!({})).await;
        run_slash_and_settle(&mut t, cmd).await;
        let wait_idle = state != SessionState::MidStream;
        let res = check_standard_cell(&mut t, wait_idle, |_| Ok(())).await;
        assert_matrix_cell(cmd, state, res);
    }
}

#[tokio::test]
async fn matrix_session_commands_empty() {
    local(run_session_core_commands_for_state(SessionState::Empty)).await;
}

#[tokio::test]
async fn matrix_session_commands_after_turn() {
    local(run_session_core_commands_for_state(SessionState::AfterTurn)).await;
}

#[tokio::test]
async fn matrix_session_commands_mid_stream() {
    local(run_session_core_commands_for_state(SessionState::MidStream)).await;
}

#[tokio::test]
async fn matrix_session_commands_after_compaction() {
    local(run_session_core_commands_for_state(SessionState::AfterCompaction)).await;
}

#[tokio::test]
async fn matrix_session_commands_after_resume() {
    local(run_session_core_commands_for_state(SessionState::AfterResume)).await;
}

#[tokio::test]
async fn matrix_archive_across_states() {
    local(async {
        for state in SessionState::ALL {
            let mut t = setup_state(state, &[], json!({})).await;
            run_slash_and_settle(&mut t, ARCHIVE_COMMAND).await;
            t.until_vt("archive settled", |t| !t.vt.busy()).await;
            let res = check_standard_cell(&mut t, true, |t| {
                // Only saved sessions (AfterTurn / AfterCompaction / AfterResume) clear & start new.
                if matches!(state, SessionState::AfterTurn | SessionState::AfterCompaction | SessionState::AfterResume)
                    && !invariants::has_welcome_banner(&t.history())
                {
                    return Err(format!("{ARCHIVE_COMMAND} did not redraw welcome banner:\n{}", t.history()));
                }
                Ok(())
            })
            .await;
            assert_matrix_cell(ARCHIVE_COMMAND, state, res);
        }
    })
    .await
}

#[tokio::test]
async fn matrix_exit_across_states() {
    local(async {
        for state in SessionState::ALL {
            let mut t = setup_state(state, &[], json!({})).await;
            run_slash_and_settle(&mut t, EXIT_COMMAND).await;
            let res = (|| {
                if !t.vt.quit_requested() {
                    return Err(format!(
                        "{EXIT_COMMAND} did not request quit in {} state:\n{}",
                        state.label(),
                        t.screen()
                    ));
                }
                t.check_invariants()?;
                Ok(())
            })();
            assert_matrix_cell(EXIT_COMMAND, state, res);
        }
    })
    .await
}

#[tokio::test]
async fn matrix_info_and_picker_slash_commands_across_states() {
    local(async {
        for state in SessionState::ALL {
            let mut t = setup_state(state, &[], json!({})).await;
            for cmd in INFO_AND_PICKER_COMMANDS {
                if state == SessionState::MidStream && !t.vt.busy() {
                    // Restart a MidStream turn if an earlier command ran long enough for the stream to end.
                    drop(t);
                    t = setup_state(state, &[], json!({})).await;
                }
                run_slash_and_settle(&mut t, cmd).await;
                let res = check_standard_cell(&mut t, false, |_| Ok(())).await;
                assert_matrix_cell(cmd, state, res);
                t.vt.dismiss_notices();
                t.step().await;
            }
        }
    })
    .await
}
