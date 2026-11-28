//! Approval dialogs, exit confirmation, notification cards and Lua-driven selects.

use super::*;

#[tokio::test]
async fn select_distinguishes_an_empty_list_from_a_filter_miss() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(&t.vt.host, r#"mare.task.spawn(function() mare.ui.select({ title = 'Empty list', items = {} }) end)"#).await;
        t.until("empty select", |s| s.contains("Empty list")).await;
        assert!(t.screen().contains("(empty)"), "{}", t.screen());
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("empty select closed", |s| !s.contains("Empty list")).await;
        exec(&t.vt.host, r#"mare.task.spawn(function() mare.ui.select({ title = 'Filter miss', items = { 'alpha' }, filter = 'zzz' }) end)"#).await;
        t.until("filtered select", |s| s.contains("Filter miss")).await;
        assert!(t.screen().contains("(no matches)"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn short_select_supports_digits_without_stealing_custom_actions() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        exec(&t.vt.host, r#"mare.task.spawn(function() R = { mare.ui.select({ title = 'Quick choice', allow_search = false, items = { 'one', 'two', 'three' } }) } end)"#).await;
        t.until("quick choice", |s| s.contains("Quick choice")).await;
        t.key(KeyCode::Char('2'), Modifiers::NONE);
        t.step().await;
        assert!(!t.screen().contains("Quick choice"), "2 should select the second item: {}", t.screen());
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return R and R[1]").await, Some(2));
        exec(&t.vt.host, r#"mare.task.spawn(function() R2 = { mare.ui.select({ title = 'Custom key', allow_search = false, items = { 'one', 'two' }, keys = { { key = '2', label = 'alternate' } } }) } end)"#).await;
        t.until("custom key", |s| s.contains("Custom key")).await;
        t.key(KeyCode::Char('2'), Modifiers::NONE);
        t.until("custom action closed", |s| !s.contains("Custom key")).await;
        assert_eq!(eval::<(i64, Option<String>)>(&t.vt.host, "return R2[1], R2[3]").await, (1, Some("2".into())));
    })
    .await
}

#[tokio::test]
async fn select_preview_can_be_hidden_and_shown_again() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        exec(&t.vt.host, r#"mare.task.spawn(function() mare.ui.select({ title = 'Preview choice', items = { { label = 'one', preview = mare.ui.Line('preview details') } } }) end)"#).await;
        t.until("the preview", |s| s.contains("Preview choice") && s.contains("preview details")).await;
        t.ctrl('p');
        t.step().await;
        assert!(!t.screen().contains("preview details"), "Ctrl+P should hide the preview: {}", t.screen());
        t.ctrl('p');
        t.until("the restored preview", |s| s.contains("preview details")).await;
    })
    .await
}

#[tokio::test]
async fn select_tabs_keep_the_active_tab_visible_at_narrow_widths() {
    local(async {
        let mut t = Term::new(json!([]), 42, 20).await;
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function() mare.ui.select({ title = 'Tabs', tab = 5, tabs = {
          { name = 'AlphaLong', items = { 'item' } }, { name = 'BetaLong', items = { 'item' } },
          { name = 'GammaLong', items = { 'item' } }, { name = 'DeltaLong', items = { 'item' } },
          { name = 'EpsilonLong', items = { 'item' } },
        } }) end)"#,
        )
        .await;
        t.until("the tabbed select", |s| s.contains("Tabs") && s.contains("item")).await;
        let screen = t.screen();
        assert!(screen.contains("EpsilonLong") && screen.contains('‹'), "active last tab and overflow: {screen}");
        t.key(KeyCode::Left, Modifiers::NONE);
        t.key(KeyCode::Left, Modifiers::NONE);
        t.step().await;
        let screen = t.screen();
        assert!(
            screen.contains("GammaLong") && screen.contains('‹') && screen.contains('›'),
            "middle tab and overflow: {screen}"
        );
    })
    .await
}

/// Assert the modal displays the full untruncated `Directory: <canonical_cwd>` across any wrapped
/// lines, then collapse those wrapped lines back to `<indent>│ Directory: <tmp> │` so 80-col
/// snapshots stay stable regardless of platform/CI `TMPDIR` length.
fn normalize_modal_directory_line(raw_lines: &mut Vec<String>, canonical_cwd: &str) {
    let Some(dir_idx) = raw_lines.iter().position(|l| l.contains("│ Directory:")) else {
        panic!("approval modal missing `Directory:` line:\n{}", raw_lines.join("\n"));
    };
    let indent = raw_lines[dir_idx].split("│ Directory:").next().unwrap_or("");
    let prefix = format!("{indent}│ ");
    let is_field_or_end = |l: &str| {
        let Some(inner) = l.trim_end().strip_prefix(&prefix).and_then(|s| s.strip_suffix(" │")) else {
            return true;
        };
        let t = inner.trim();
        t.is_empty()
            || [
                "Requester:",
                "Tool:",
                "Path:",
                "Command:",
                "URL:",
                "Read paths:",
                "Write paths:",
                "Network:",
                "Purpose:",
                "Reason:",
                "Sandbox:",
                "Scope:",
                "Maximum grant:",
            ]
            .iter()
            .any(|hdr| t.starts_with(hdr))
    };
    let mut end_idx = dir_idx + 1;
    while end_idx < raw_lines.len() && !is_field_or_end(&raw_lines[end_idx]) {
        end_idx += 1;
    }
    let joined: String = raw_lines[dir_idx..end_idx]
        .iter()
        .map(|l| {
            let s = l.trim_end();
            let s = s.strip_prefix(&prefix).unwrap_or(s);
            s.strip_suffix(" │").unwrap_or(s).trim_end()
        })
        .collect::<Vec<_>>()
        .join("");
    let expected = format!("Directory:{canonical_cwd}");
    let joined_no_space = joined.replace("Directory: ", "Directory:");
    assert!(
        joined_no_space == expected,
        "approval modal must display full untruncated Directory path:\nexpected: {expected}\ngot: {joined}\nscreen:\n{}",
        raw_lines.join("\n")
    );
    raw_lines.splice(dir_idx..end_idx, [format!("{indent}│ Directory: <tmp> │")]);
}

/// The approval dialog (read-only preset, a write needs approval): allowing runs the tool,
/// denying tells the model.
#[tokio::test]
async fn the_approval_dialog_allows_and_denies() {
    local(async {
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "A" } }] },
            { "text": "wrote a" },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "b.txt", "content": "B" } }] },
            { "text": "done b" }
        ]);
        let cfg =
            json!({ "reduce_motion": true, "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });
        // Use 28 rows so long CI/macOS/Windows TMPDIR paths wrapping `Directory:` onto 2-3 lines
        // still leave vertical room for the `Responding…` working row below the modal.
        let mut t = Term::with(script, cfg, 80, 28).await;
        let cwd = t._env.cwd.clone();
        t.submit("write a").await;
        t.until("the approval dialog over the running turn", |s| {
            s.contains("Allow once") && s.contains("Enter confirm")
        })
        .await;
        assert!(t.vt.busy(), "the turn is still running under the dialog");
        let canonical_cwd = dunce::canonicalize(&cwd).unwrap().to_string_lossy().into_owned();
        t.vt.dismiss_notices();
        t.step().await;
        let mut raw_lines: Vec<String> = t.screen().lines().map(str::to_string).collect();
        normalize_modal_directory_line(&mut raw_lines, &canonical_cwd);
        insta::assert_snapshot!("approval_dialog", trimmed(&raw_lines.join("\n")));
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).ok().as_deref(),
            Some("A"),
            "the allowed write didn't land in {}; screen:\n{}",
            cwd.display(),
            t.screen()
        );
        t.submit("write b").await;
        t.until("the second dialog", |s| s.contains("Allow once")).await;
        // `n` denies and asks why; the answer reaches the model in the tool result
        t.key(KeyCode::Char('n'), Modifiers::NONE);
        t.until("the reason prompt", |s| s.contains("Tell the model why")).await;
        t.vt.type_str("use the scratch dir");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        assert!(!cwd.join("b.txt").exists(), "denied write must not run");
        global(&t.vt.host, "AG", t.vt.host.service_table("agent").unwrap());
        let messages: String = eval(&t.vt.host, "return mare.json.encode(AG:list_messages().data)").await;
        assert!(messages.contains("denied by the user: use the scratch dir. Do not retry"), "{messages}");
    })
    .await
}

/// 80×24 coverage for the approval dialog: a short cwd must keep the full modal, preview,
/// `Responding…` row, input box and footer visible simultaneously; a long wrapped cwd (>= 90 chars)
/// must still display the full untruncated Directory path and keep all approval options reachable.
#[tokio::test]
async fn approval_dialog_80x24_layout_and_long_cwd_reachability() {
    local(async {
        let cfg =
            json!({ "reduce_motion": true, "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } });

        // 1) Stable short cwd at 80×24: single-line Directory fits all rows including `Responding…`.
        let short_dir = {
            #[cfg(unix)]
            {
                tempfile::Builder::new()
                    .prefix("m")
                    .tempdir_in("/tmp")
                    .unwrap_or_else(|_| tempfile::Builder::new().prefix("m").tempdir().unwrap())
            }
            #[cfg(not(unix))]
            {
                tempfile::Builder::new().prefix("m").tempdir().unwrap()
            }
        };
        let short_cwd = dunce::canonicalize(short_dir.path()).unwrap();
        let short_cwd_str = short_cwd.to_string_lossy().into_owned();
        assert!(
            short_cwd_str.len() <= 58,
            "expected short cwd <= 58 chars for single-line Directory at 80×24, got {} ({short_cwd_str})",
            short_cwd_str.len()
        );
        let mut env_short = TestEnv::new().await;
        env_short.cwd = short_cwd.clone();
        let script_short = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "A" } }] },
            { "text": "wrote a" }
        ]);
        let mut t = Term::in_env(env_short, script_short, cfg.clone(), json!({}), 80, 24).await;
        t.submit("write a").await;
        t.until("80x24 approval dialog", |s| s.contains("Allow once") && s.contains("Enter confirm")).await;
        assert!(t.vt.busy(), "the turn is still running under the 80x24 dialog");
        t.vt.dismiss_notices();
        t.step().await;
        let screen = t.screen();
        let mut raw_lines: Vec<String> = screen.lines().map(str::to_string).collect();
        normalize_modal_directory_line(&mut raw_lines, &short_cwd_str);
        for required in [
            "Allow write?",
            "Requester: Main session",
            "Tool: write",
            "Path: a.txt",
            "Allow once (y)",
            "Allow for this session (a)",
            "Allow and remember (A)",
            "Deny, and tell the model why (n)",
            "Deny and stop this turn (ctrl+c)",
            "Preview",
            "1 + A",
            "Responding…",
            "Queue a message…",
            "Enter confirm",
        ] {
            assert!(screen.contains(required), "80x24 short-cwd approval dialog missing {required:?}:\n{screen}");
        }
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.idle().await;
        assert_eq!(std::fs::read_to_string(short_cwd.join("a.txt")).ok().as_deref(), Some("A"));
        drop(t);
        drop(short_dir);

        // 2) Long wrapped cwd (>= 90 chars) at 80×24: full untruncated Directory path + all options reachable.
        let mut env_long = TestEnv::new().await;
        let nested = env_long
            .cwd
            .join("very-long-workspace-segment-alpha")
            .join("very-long-workspace-segment-beta")
            .join("deep-subdir");
        std::fs::create_dir_all(&nested).unwrap();
        let long_cwd = dunce::canonicalize(&nested).unwrap();
        let long_cwd_str = long_cwd.to_string_lossy().into_owned();
        assert!(
            long_cwd_str.len() >= 90,
            "expected long cwd >= 90 chars to force multi-line wrap, got {} ({long_cwd_str})",
            long_cwd_str.len()
        );
        env_long.cwd = long_cwd.clone();
        let script_long = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "b.txt", "content": "B" } }] },
            { "text": "wrote b" }
        ]);
        let mut t = Term::in_env(env_long, script_long, cfg, json!({}), 80, 24).await;
        t.submit("write b").await;
        t.until("80x24 long-cwd approval dialog", |s| s.contains("Allow once") && s.contains("1 of 5")).await;
        t.vt.dismiss_notices();
        t.step().await;
        let screen = t.screen();
        let mut raw_lines: Vec<String> = screen.lines().map(str::to_string).collect();
        normalize_modal_directory_line(&mut raw_lines, &long_cwd_str);
        for option in [
            "Allow once (y)",
            "Allow for this session (a)",
            "Allow and remember (A)",
            "Deny, and tell the model why (n)",
            "Deny and stop this turn (ctrl+c)",
        ] {
            assert!(screen.contains(option), "80x24 long-cwd approval dialog missing option {option:?}:\n{screen}");
        }
        // Navigate down through all 5 options to confirm the last option is reachable, then approve via `y`.
        for expected_pos in ["2 of 5", "3 of 5", "4 of 5", "5 of 5"] {
            t.key(KeyCode::Down, Modifiers::NONE);
            t.step().await;
            assert!(
                t.screen().contains(expected_pos),
                "expected selection to reach {expected_pos} in 80x24 long-cwd modal:\n{}",
                t.screen()
            );
        }
        t.key(KeyCode::Char('y'), Modifiers::NONE);
        t.idle().await;
        assert_eq!(std::fs::read_to_string(long_cwd.join("b.txt")).ok().as_deref(), Some("B"));
    })
    .await
}

#[path = "dialogs/approval_delay.rs"]
mod approval_delay;
#[path = "dialogs/approvals_and_select.rs"]
mod approvals_and_select;
#[path = "dialogs/questions_and_child.rs"]
mod questions_and_child;
#[path = "dialogs/ui_override.rs"]
mod ui_override;

/// Multiple choices remain in one picker and are returned in item order (Codex
/// `tui/src/bottom_pane/multi_select_picker.rs:1043`).
#[tokio::test]
async fn multi_select_toggles_in_place_and_returns_checked_indices() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R = { mare.ui.select({ title = "PickMany", multi = true, selected = 2,
                   items = { { label = "Group", header = true }, { label = "alpha" },
                             { label = "beta", checked = true }, { label = "disabled", disabled = true, checked = true } } }) }
               end)"#,
        )
        .await;
        t.until("multi select open", |s| s.contains("PickMany") && s.contains("beta")).await;
        let screen = t.screen();
        assert!(screen.contains("[ ] alpha") && screen.contains("[x] beta"), "{screen}");
        assert!(!screen.contains("[ ] Group") && !screen.contains("[ ] disabled"), "{screen}");
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        let screen = t.screen();
        assert!(screen.contains("PickMany") && screen.contains("[x] alpha"), "{screen}");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("multi select closed", |s| !s.contains("PickMany")).await;
        let result: (i64, i64, i64, Option<String>, String) =
            eval(&t.vt.host, "return R[1][1], R[1][2], R[2], R[3], R[4]").await;
        assert_eq!(result, (2, 3, 1, None, String::new()));
    })
    .await
}

#[tokio::test]
async fn multi_select_search_space_and_escape_preserve_modal_state() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R = { mare.ui.select({ title = 'FilteredChoices', multi = true, filter = 'red',
                   items = { 'red apple', 'red berry', 'blue' } }) }
               end)"#,
        )
        .await;
        t.until("filtered choices", |s| s.contains("FilteredChoices") && s.contains("red berry")).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("/ red ▏"), "space appends while searching: {}", t.screen());
        t.key(KeyCode::Backspace, Modifiers::NONE);
        t.key(KeyCode::PageDown, Modifiers::NONE);
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        let screen = t.screen();
        assert!(screen.contains("FilteredChoices") && screen.contains("/ red") && screen.contains("[x] red berry"), "{screen}");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("filtered choices closed", |s| !s.contains("FilteredChoices")).await;
        let result: (i64, String) = eval(&t.vt.host, "return R[1][1], R[4]").await;
        assert_eq!(result, (2, "red".into()));

        exec(&t.vt.host, r#"mare.task.spawn(function() R2 = { mare.ui.select({ title = 'EmptySearch', multi = true, items = { 'one' } }) } end)"#).await;
        t.until("new picker", |s| s.contains("EmptySearch")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("EmptySearch"), "first Esc exits search even when empty");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("new picker cancelled", |s| !s.contains("EmptySearch")).await;
        assert!(eval::<bool>(&t.vt.host, "return R2[1] == nil and R2[2] == 'cancelled'").await);
    })
    .await
}

#[tokio::test]
async fn multi_select_honors_space_as_a_key_sequence_continuation() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set('select', 'g <Space>', function() SEQUENCE_RAN = true end, { desc = 'sequence test' })
               mare.task.spawn(function() mare.ui.select({ title = 'Sequence', multi = true, items = { 'one' } }) end)"#,
        )
        .await;
        t.until("multi select", |s| s.contains("Sequence") && s.contains("[ ] one")).await;
        t.key(KeyCode::Char('g'), Modifiers::NONE);
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        assert!(eval::<bool>(&t.vt.host, "return SEQUENCE_RAN == true").await);
        assert!(t.screen().contains("[ ] one"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn multi_select_toggle_keeps_long_list_scroll_position() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"local items = {}
               for i = 1, 30 do items[i] = ('Item %02d'):format(i) end
               mare.task.spawn(function() mare.ui.select({ title = 'Long list', multi = true, selected = 20, items = items }) end)"#,
        )
        .await;
        t.until("long list", |s| s.contains("Long list") && s.contains("Item 20")).await;
        let visible = |screen: &str| {
            screen.lines().filter(|line| line.contains("Item ")).map(|line| line.replace("[x]", "[ ]")).collect::<Vec<_>>()
        };
        let before = visible(&t.screen());
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        let after = t.screen();
        assert!(after.contains("[x] Item 20"), "{after}");
        assert_eq!(visible(&after), before, "toggle must keep the visible list and scroll position");
    })
    .await
}

#[tokio::test]
async fn multi_select_toggle_takes_precedence_over_extra_space_action() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"
            mare.task.spawn(function()
              mare.ui.select({ title = 'Multi action', multi = true, items = { 'one' },
                keys = { { key = ' ', label = 'other action' } } })
            end)
        "#,
        )
        .await;
        t.until("multi action", |s| s.contains("Multi action") && s.contains("[ ] one")).await;
        t.key(KeyCode::Char(' '), Modifiers::NONE);
        t.step().await;
        assert!(t.screen().contains("Multi action") && t.screen().contains("[x] one"), "{}", t.screen());
    })
    .await
}

#[tokio::test]
async fn multi_select_help_lists_and_runs_space_toggle() {
    local(async {
        let mut t = Term::new(json!([]), 100, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function() mare.ui.select({ title = 'Choices', multi = true, items = { 'one' } }) end)"#,
        )
        .await;
        t.until("choices", |s| s.contains("Choices") && s.contains("[ ] one")).await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("key help", |s| s.contains("─ Keys ─")).await;
        t.vt.type_str("toggle");
        t.step().await;
        let screen = t.screen();
        assert!(screen.contains("Space") && screen.contains("toggle selection"), "{screen}");
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("checked after help action", |s| s.contains("Choices") && s.contains("[x] one")).await;
    })
    .await
}

#[tokio::test]
async fn multi_select_hints_and_actions_follow_rebinding() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        let registered: (bool, bool) = eval(
            &t.vt.host,
            r#"
            local names = {}
            for _, action in ipairs(mare.keymap.actions()) do names[action.name] = true end
            return names['select.toggle'] == true, names['select.toggle_preview'] == true
        "#,
        )
        .await;
        assert_eq!(registered, (true, true), "modal actions must be discoverable in the keymap API");
        exec(
            &t.vt.host,
            r#"
            mare.keymap.set('select', '<Space>', false)
            mare.keymap.set('select', '<C-x>', 'select.toggle')
            mare.keymap.set('select', '<C-p>', false)
            mare.keymap.set('select', '<C-k>', 'select.toggle_preview')
            mare.task.spawn(function()
              mare.ui.select({ title = 'Rebound choices', multi = true,
                items = { { label = 'one', preview = mare.ui.Line('PREVIEW CONTENT') } } })
            end)
        "#,
        )
        .await;
        t.until("rebound select", |s| s.contains("Rebound choices") && s.contains("PREVIEW CONTENT")).await;
        let screen = t.screen();
        assert!(screen.contains("Ctrl+X") && screen.contains("toggle") && screen.contains("Ctrl+K"), "{screen}");
        assert!(!screen.contains("Space toggle") && !screen.contains("Ctrl+P"), "{screen}");
        t.ctrl('x');
        t.until("rebound toggle", |s| s.contains("[x] one")).await;
        t.ctrl('k');
        t.until("rebound preview", |s| !s.contains("PREVIEW CONTENT")).await;
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("select help", |s| s.contains("─ Keys ─")).await;
        t.vt.type_str("toggle selection");
        t.until("rebound help", |s| s.contains("Ctrl+X") && s.contains("toggle selection")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("select restored", |s| s.contains("Rebound choices") && s.contains("[x] one")).await;
        exec(&t.vt.host, "mare.keymap.set('select', '<C-x>', false); mare.keymap.set('select', '<C-k>', false)").await;
        t.step().await;
        let screen = t.screen();
        assert!(
            !screen.contains("Ctrl+X toggle") && !screen.contains("Space toggle") && !screen.contains("Ctrl+K"),
            "{screen}"
        );
    })
    .await
}

#[tokio::test]
async fn plugin_panel_help_returns_to_the_selected_action() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        t.submit("/plugins").await;
        t.until("plugin panel", |s| s.contains("Discover") && s.contains("Installed")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("installed actions", |s| s.contains("Check updates") && s.contains("Clean")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Down, Modifiers::NONE);
        t.step().await;
        let before = t.screen();
        t.key(KeyCode::Char('?'), Modifiers::NONE);
        t.until("panel key help", |s| s.contains("─ Keys ─")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("panel restored", |s| s.contains("Installed") && !s.contains("─ Keys ─")).await;
        assert_eq!(t.screen(), before, "help must restore the same tab, selection, and viewport");
    })
    .await
}

/// Preview and explicit confirmation protect clean, ignored, and dirty checkouts alike.
#[tokio::test]
async fn plugin_panel_clean_requires_confirmation_and_previews_candidates() {
    local(async {
        let env = TestEnv::new().await;
        let packages = env.home.path().join("data/packages/github.com/example");
        for name in ["unused", "ignored", "dirty"] {
            let dir = packages.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(".gitignore"), "ignored-file\n").unwrap();
            for args in [
                vec!["init", "-q", "-b", "main"],
                vec!["add", ".gitignore"],
                vec![
                    "-c",
                    "user.name=test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-qm",
                    "fixture",
                ],
                vec!["update-ref", "refs/remotes/origin/main", "HEAD"],
            ] {
                let output = fixture_git(&dir).args(args).output().unwrap();
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            }
        }
        std::fs::write(packages.join("ignored/ignored-file"), "keep ignored").unwrap();
        std::fs::write(packages.join("dirty/local-file"), "keep dirty").unwrap();
        std::fs::create_dir_all(env.config_dir()).unwrap();
        let lock = env.config_dir().join("mare-lock.json");
        let lock_bytes =
            br#"{"version":1,"packages":{"stale-entry":{"url":"https://example.invalid/stale","commit":"0123456789abcdef0123456789abcdef01234567"}}}"#;
        std::fs::write(&lock, lock_bytes).unwrap();
        let mut t = Term::in_env(env, json!([]), json!({}), json!({}), 150, 30).await;
        t.submit("/plugins").await;
        t.until("plugin panel", |s| s.contains("Discover") && s.contains("Installed")).await;
        t.key(KeyCode::Tab, Modifiers::NONE);
        t.until("installed actions", |s| s.contains("Clean") && s.contains("Check updates")).await;
        for _ in 0..4 {
            t.key(KeyCode::Down, Modifiers::NONE);
        }
        t.key(KeyCode::Enter, Modifiers::NONE);
        let preview = tokio::time::timeout(
            Duration::from_secs(5),
            t.until("clean preview", |s| {
                s.contains("Clean unused") && s.contains("unused") && s.contains("stale-entry")
            }),
        )
        .await;
        assert!(preview.is_ok(), "clean must show candidates before deleting anything: {}", t.screen());
        assert!(packages.join("unused").exists());
        assert_eq!(std::fs::read(&lock).unwrap(), lock_bytes);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("clean cancelled", |s| s.contains("Installed") && !s.contains("Clean unused")).await;
        assert!(packages.join("unused").exists());
        assert_eq!(std::fs::read(&lock).unwrap(), lock_bytes);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("clean preview again", |s| s.contains("Clean unused")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("clean escaped", |s| s.contains("Installed") && !s.contains("Clean unused")).await;
        assert!(packages.join("unused").exists());
        assert_eq!(std::fs::read(&lock).unwrap(), lock_bytes);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("clean preview for explicit confirmation", |s| s.contains("Clean unused")).await;
        t.key(KeyCode::Left, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("clean finished", |s| s.contains("plugins clean — done")).await;
        assert!(!packages.join("unused").exists());
        assert!(packages.join("ignored/ignored-file").exists());
        assert!(packages.join("dirty/local-file").exists());
        assert!(!std::fs::read_to_string(&lock).unwrap().contains("stale-entry"));
    })
    .await
}

#[tokio::test]
async fn confirm_select_respects_custom_and_empty_accept_labels() {
    local(async {
        let mut t = Term::new(json!([]), 120, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 mare.ui.select({ title = 'Install changes?', kind = 'confirm', accept_label = 'install', items = { 'Yes', 'No' } })
                 mare.ui.select({ title = 'Continue?', kind = 'confirm', accept_label = '', items = { 'Yes', 'No' } })
               end)"#,
        )
        .await;
        t.until("install confirmation", |s| s.contains("Install changes?")).await;
        let screen = t.screen();
        assert!(screen.contains("Enter install") && !screen.contains("Enter confirm"), "{screen}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("hidden accept confirmation", |s| s.contains("Continue?")).await;
        let bar = t.screen().lines().last().unwrap_or("").to_owned();
        assert!(!bar.contains("Enter confirm") && !bar.contains("Enter select"), "{bar}");
        t.key(KeyCode::Escape, Modifiers::NONE);
    })
    .await
}

/// The search hint must describe the first Escape action, not also advertise cancellation.
#[tokio::test]
async fn searching_select_shows_only_clear_search_escape_hint() {
    local(async {
        let mut t = Term::new(json!([]), 160, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R = { mare.ui.select({ title = "SearchHints", items = { "alpha", "beta" }, filter = "a" }) }
               end)"#,
        )
        .await;
        t.until("search hint", |s| s.contains("SearchHints") && s.contains("clear search")).await;
        let screen = t.screen();
        assert!(!screen.contains("Esc cancel"), "{screen}");
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("search cleared", |s| s.contains("SearchHints") && s.contains("Esc cancel")).await;
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("select cancelled", |s| !s.contains("SearchHints")).await;
    })
    .await
}

/// Select modal keys route through `select` bindings and its hint bar follows rebinds/unbinds
/// (reference: Codex `list_selection_view.rs:3012,2617`).
#[tokio::test]
async fn select_keys_follow_rebinds() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set("select", "<C-j>", "select.down")
               mare.task.spawn(function()
                 R1 = { mare.ui.select({ title = "PickOne", items = { "one", "two", "three" } }) }
               end)"#,
        )
        .await;
        t.until("select open", |s| s.contains("PickOne")).await;
        t.key(KeyCode::Char('j'), Modifiers::CONTROL);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("select closed", |s| !s.contains("PickOne")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return R1 and R1[1]").await, Some(2));

        exec(
            &t.vt.host,
            r#"mare.keymap.set("select", "<Down>", false)
               mare.task.spawn(function()
                 R2 = { mare.ui.select({ title = "PickTwo", items = { "one", "two", "three" } }) }
               end)"#,
        )
        .await;
        t.until("second select with updated move hint", |s| s.contains("PickTwo") && s.contains("↑/Ctrl+J move")).await;
        t.key(KeyCode::Down, Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("second select closed", |s| !s.contains("PickTwo")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return R2 and R2[1]").await, Some(1));
    })
    .await
}

/// Printable keys while typing a search append to the filter instead of running `select` bindings
/// (reference: Codex `list_selection_view.rs:2529`).
#[tokio::test]
async fn printable_keys_search_before_select_bindings() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set("select", "x", function() _G.X = true end, { desc = "x test" })
               mare.task.spawn(function()
                 R = { mare.ui.select({ title = "FilterOrBind", items = { "alpha", "xray", "zebra" } }) }
               end)"#,
        )
        .await;
        t.until("select open", |s| s.contains("FilterOrBind") && s.contains("zebra")).await;
        t.key(KeyCode::Char('/'), Modifiers::NONE);
        t.key(KeyCode::Char('x'), Modifiers::NONE);
        t.until("search filtered by x", |s| s.contains("xray") && !s.contains("zebra")).await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return _G.X").await, None);
        // Clear search (`select.back`), then `x` runs the `select` binding.
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("search cleared", |s| s.contains("zebra")).await;
        t.key(KeyCode::Char('x'), Modifiers::NONE);
        t.step().await;
        assert_eq!(eval::<Option<bool>>(&t.vt.host, "return _G.X").await, Some(true));
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("select closed", |s| !s.contains("FilterOrBind")).await;
    })
    .await
}

/// `select.vim` subcontext activates only when `vim = true` and not searching, and obeys unbinds.
#[tokio::test]
async fn vim_select_context() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R1 = { mare.ui.select({ title = "VimPick", items = { "alpha", "bravo", "charlie", "delta" }, vim = true }) }
               end)"#,
        )
        .await;
        t.until("vim select open", |s| s.contains("VimPick")).await;
        t.key(KeyCode::Char('2'), Modifiers::NONE);
        t.key(KeyCode::Char('j'), Modifiers::NONE);
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("vim select closed", |s| !s.contains("VimPick")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return R1 and R1[1]").await, Some(3));

        exec(
            &t.vt.host,
            r#"mare.keymap.set("select.vim", "j", false)
               mare.task.spawn(function()
                 R2 = { mare.ui.select({ title = "VimSearch", items = { "alpha", "juliet" }, vim = true }) }
               end)"#,
        )
        .await;
        t.until("second vim select open", |s| s.contains("VimSearch") && s.contains("alpha")).await;
        t.key(KeyCode::Char('j'), Modifiers::NONE);
        t.until("j filtered to juliet", |s| s.contains("juliet") && !s.contains("alpha")).await;
        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("second vim select closed", |s| !s.contains("VimSearch")).await;
        assert_eq!(eval::<Option<i64>>(&t.vt.host, "return R2 and R2[1]").await, Some(2));
    })
    .await
}

/// `confirm` and `info` modal keys are keymap actions in their contexts.
#[tokio::test]
async fn confirm_and_info_keys_are_actions() {
    local(async {
        let mut t = Term::new(json!([]), 80, 14).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"mare.keymap.set("confirm", "o", "confirm.yes")
               mare.keymap.set("info", "<C-n>", "info.down")"#,
        )
        .await;
        let h = t.vt.host.clone();
        let ans =
            tokio::task::spawn_local(async move { eval::<bool>(&h, "return mare.ui.confirm('ConfirmRebind?')").await });
        t.until("confirm open", |s| s.contains("ConfirmRebind?")).await;
        t.key(KeyCode::Char('o'), Modifiers::NONE);
        t.until("confirm closed", |s| !s.contains("ConfirmRebind?")).await;
        assert!(ans.await.unwrap(), "o bound to confirm.yes should accept");

        exec(
            &t.vt.host,
            r#"local lines = {}
               for i = 1, 30 do lines[i] = string.format("info_row_%02d", i) end
               mare.ui.overlay("InfoScroll", table.concat(lines, "\n"))"#,
        )
        .await;
        t.until("info overlay open", |s| s.contains("InfoScroll") && s.contains("info_row_01")).await;
        t.key(KeyCode::Char('n'), Modifiers::CONTROL);
        t.until("info scrolled down without closing", |s| {
            s.contains("InfoScroll") && !s.contains("info_row_01") && s.contains("info_row_02")
        })
        .await;
        t.key(KeyCode::Char('q'), Modifiers::NONE);
        t.until("unbound key closes info overlay", |s| !s.contains("InfoScroll")).await;
    })
    .await
}

/// Plugin actions named `select.*` bound in `select` execute instead of being swallowed by
/// `modal_action` returning `Some(false)` on unknown ops.
#[tokio::test]
async fn plugin_select_action_runs_inside_select_modal() {
    local(async {
        let mut t = Term::new(json!([]), 80, 24).await;
        expose(&t.vt.host);
        exec(
            &t.vt.host,
            r#"PLUGIN_ACTION_RAN = false
               mare.keymap.action("select.plugin_custom", function()
                 PLUGIN_ACTION_RAN = true
                 mare.ui.notify("plugin select action ok", "info")
               end, { desc = "plugin custom", contexts = { "select" } })
               mare.keymap.set("select", "<C-x>", "select.plugin_custom")
               mare.task.spawn(function()
                 mare.ui.select({ title = "PluginSelectAction", items = { "alpha", "beta" } })
               end)"#,
        )
        .await;
        t.until("select open", |s| s.contains("PluginSelectAction")).await;
        t.key(KeyCode::Char('x'), Modifiers::CONTROL);
        t.key(KeyCode::Escape, Modifiers::NONE);
        t.until("select closed and plugin select action notice shown", |s| {
            !s.contains("PluginSelectAction") && s.contains("plugin select action ok")
        })
        .await;
        assert!(eval::<bool>(&t.vt.host, "return PLUGIN_ACTION_RAN == true").await);
    })
    .await
}

/// Disabling `keymap-default` diagnoses unbound accept/cancel actions in `/hotkeys` and still
/// allows `Ctrl+C` to dismiss modals.
#[tokio::test]
async fn disabled_keymap_default_reports_unbound_accept_cancel_and_ctrl_c_still_closes() {
    local(async {
        let mut t = Term::with(json!([]), json!({"plugins": {"keymap-default": {"enabled": false}}}), 120, 45).await;
        expose(&t.vt.host);
        t.until_vt("startup", |t| !t.vt.starting()).await;
        exec(&t.vt.host, r#"C.cordis:get("commands"):run("/hotkeys")"#).await;
        t.until("hotkeys reports unbound accept/cancel", |s| {
            s.contains("Key bindings")
                && s.contains("select.accept")
                && s.contains("select.back")
                && s.contains("confirm.accept")
                && s.contains("confirm.cancel")
                && s.contains("help.accept")
                && s.contains("help.cancel")
        })
        .await;
        t.key(KeyCode::Char('q'), Modifiers::NONE);
        t.until("hotkeys closed", |s| !s.contains("Key bindings")).await;

        exec(
            &t.vt.host,
            r#"mare.task.spawn(function()
                 R_DISMISSED = { mare.ui.select({ title = "NoDefaultModal", items = { "one", "two" } }) }
               end)"#,
        )
        .await;
        t.until("select open", |s| s.contains("NoDefaultModal")).await;
        t.key(KeyCode::Char('c'), Modifiers::CONTROL);
        t.until("Ctrl+C closes modal even with keymap-default disabled", |s| !s.contains("NoDefaultModal")).await;
        assert_eq!(
            eval::<Option<String>>(&t.vt.host, "return R_DISMISSED and R_DISMISSED[2]").await.as_deref(),
            Some("interrupted")
        );
    })
    .await
}

/// Faux TUI screen test: a connected MCP server that exposes 0 tools shows `connected`,
/// `0 tools`, and a visible hint in `/mcp` (both in the server picker and in the server detail
/// panel), never silently showing `0 tools` alone.
#[tokio::test]
async fn mcp_panel_shows_hint_for_connected_server_with_zero_tools() {
    local(async {
        let log_dir = tempfile::tempdir().unwrap();
        let log = log_dir.path().join("zero.log");
        let server_cfg = json!({
            "plugins": {
                "mcp": {
                    "servers": {
                        "empty_srv": {
                            "command": fake_mcp_bin(),
                            "args": [
                                "scripted",
                                json!({ "log": log, "capabilities": {}, "tools": [] }).to_string()
                            ],
                            "tier": "direct"
                        }
                    }
                }
            }
        });
        let mut t = Term::with(json!([]), server_cfg, 120, 30).await;
        t.until_vt("startup", |t| !t.vt.starting()).await;
        global(&t.vt.host, "M", t.vt.host.service_table("mcp").unwrap());
        let deadline = Instant::now() + WAIT;
        loop {
            t.step().await;
            if eval::<bool>(
                &t.vt.host,
                r#"local s = M:list_servers().data[1]
                   return s ~= nil and s.state == "connected""#,
            )
            .await
            {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for empty_srv connected");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        t.submit("/mcp").await;
        t.until("MCP servers picker", |s| s.contains("MCP servers") && s.contains("empty_srv")).await;
        let picker_screen = t.screen();
        assert!(
            picker_screen.contains("empty_srv")
                && picker_screen.contains("connected")
                && picker_screen.contains("0 tools")
                && picker_screen.contains("no tools exposed"),
            "expected connected + 0 tools + hint on /mcp picker screen:\n{picker_screen}"
        );

        t.key(KeyCode::Enter, Modifiers::NONE);
        t.until("MCP server details", |s| s.contains("MCP empty_srv")).await;
        let detail_screen = t.screen();
        assert!(
            detail_screen.contains("connected")
                && detail_screen.contains("0 tools")
                && detail_screen.contains("no tools exposed"),
            "expected connected + 0 tools + hint on /mcp detail screen:\n{detail_screen}"
        );
    })
    .await
}
