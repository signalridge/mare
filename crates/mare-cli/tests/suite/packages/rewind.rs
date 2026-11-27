//! Rewind and undo.

use super::*;

#[tokio::test]
async fn rewind_restores_files_to_before_a_turn_whatever_tool_wrote_them() {
    local(async {
        let env = TestEnv::new().await;
        let f = env.cwd.join("a.txt");
        std::fs::write(&f, "v1\n").unwrap();
        let h = env.host().builtins(&["changes", "subagent", "agents"]).no_session().build().await;
        expose(&h);
        global(&h, "C", h.service_table("changes").unwrap());
        let call = |name: &str, args: serde_json::Value| {
            mare_shared::lua::json_to_lua(
                h.lua(),
                &json!({ "id": format!("c-{name}"), "name": name, "arguments": args }),
            )
            .unwrap()
        };
        exec(&h, r#"mare.snapshot.begin_turn("t1")"#).await;
        global(&h, "R", call("read", json!({ "path": "a.txt" })));
        exec(&h, "T:execute(R, {})").await;
        global(&h, "W", call("write", json!({ "path": "a.txt", "content": "v2\n" })));
        exec(&h, "T:execute(W, {})").await;
        exec(&h, r#"mare.snapshot.begin_turn("t2")"#).await;
        global(&h, "W2", call("write", json!({ "path": "new.txt", "content": "made\n" })));
        exec(&h, "T:execute(W2, {})").await;
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v2\n");
        let turns: usize = eval(&h, "return #C:turns()").await;
        assert_eq!(turns, 2);
        // back to before t1: a.txt restored, the file created later removed
        let restored: usize = eval(&h, r#"return #C:rewind("t1", {}).restored"#).await;
        assert_eq!(restored, 2);
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1\n");
        assert!(!env.cwd.join("new.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn undo_forks_before_the_nth_last_prompt_and_puts_it_back_in_the_editor() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["changes", "subagent", "agents"])
            .faux(json!([{ "text": "a" }, { "text": "b" }]))
            .build()
            .await;
        h.prompt("first prompt", json!({})).await.unwrap();
        h.prompt("second prompt", json!({})).await.unwrap();
        exec(&h, "mare.ui.set_editor = function(t) mare.g.editor = t end").await;
        global(&h, "CMD", h.service_table("commands").unwrap());
        exec(&h, r#"CMD:run("/undo")"#).await;
        wait_until(WAIT_SECS, "/undo to put the prompt back", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return mare.g.editor").await.as_deref() == Some("second prompt") }
        })
        .await;
        // the conversation now ends before the second prompt (the old branch stays in the tree)
        global(&h, "A", h.service_table("agent").unwrap());
        let last_user: String = eval(
            &h,
            r#"local last
               for _, m in ipairs(A:list_messages().data) do
                 if m.role == "user" and not m.context_disclosure then last = m.content[1].text end
               end
               return last"#,
        )
        .await;
        assert_eq!(last_user, "first prompt");
        exec(&h, r#"CMD:run("/undo 1")"#).await;
        wait_until(WAIT_SECS, "/undo 1 to go back to the first prompt", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return mare.g.editor").await.as_deref() == Some("first prompt") }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn parent_and_child_subagent_snapshots_stay_isolated() {
    local(async {
        let env = TestEnv::new().await;
        let parent_file = env.cwd.join("parent.txt");
        let child_file = env.cwd.join("child.txt");
        std::fs::write(&parent_file, "parent-v1\n").unwrap();
        std::fs::write(&child_file, "child-v1\n").unwrap();

        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "child-new.txt", "content": "child-v2\n" } }] },
            { "text": "child done" }
        ]);
        let h = env.host().builtins(&["changes", "subagent", "agents"]).faux(script).no_session().build().await;
        expose(&h);
        global(&h, "C", h.service_table("changes").unwrap());
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let call = |name: &str, args: serde_json::Value| {
            mare_shared::lua::json_to_lua(
                h.lua(),
                &json!({ "id": format!("c-{name}"), "name": name, "arguments": args }),
            )
            .unwrap()
        };

        exec(&h, r#"mare.snapshot.begin_turn("parent-t1")"#).await;
        global(&h, "PR", call("read", json!({ "path": "parent.txt" })));
        exec(&h, "local r = T:execute(PR, {}); assert(not r.is_error, mare.json.encode(r))").await;
        global(&h, "PW", call("write", json!({ "path": "parent.txt", "content": "parent-v2\n" })));
        exec(&h, "local r = T:execute(PW, {}); assert(not r.is_error, mare.json.encode(r))").await;

        // Spawn a child subagent concurrently while the parent's snapshot turn is active.
        let child_ok: bool = eval(
            &h,
            r#"local a = mare.subagent.start({ cwd = CWD, prompt = "create child-new.txt", no_session = true })
               local ok = pcall(a.wait, a)
               return ok"#,
        )
        .await;
        assert!(child_ok);
        assert_eq!(std::fs::read_to_string(&parent_file).unwrap(), "parent-v2\n");
        assert_eq!(std::fs::read_to_string(env.cwd.join("child-new.txt")).unwrap(), "child-v2\n");

        // Parent's journal must only contain parent-t1 with parent.txt, and restore cleanly.
        let (turn_count, file_count): (usize, usize) = eval(
            &h,
            r#"local turns = C:turns()
               return #turns, #turns[1].files"#,
        )
        .await;
        assert_eq!(turn_count, 1);
        assert_eq!(file_count, 1);
        let restored: usize = eval(&h, r#"return #C:rewind("parent-t1", {}).restored"#).await;
        assert_eq!(restored, 1);
        assert_eq!(std::fs::read_to_string(&parent_file).unwrap(), "parent-v1\n");
        h.shutdown().await;
    })
    .await
}

/// If another turn edits files while /rewind's conflict confirmation dialog is open,
/// confirming the stale dialog must NOT overwrite the new edits or rewind the conversation, and
/// conflict preflight must not modify non-conflicting files before confirmation.
#[tokio::test]
async fn rewind_confirm_refuses_stale_selection_after_new_turn_edit() {
    local(async {
        let env = TestEnv::new().await;
        let fa = env.cwd.join("a.txt");
        let fb = env.cwd.join("b.txt");
        std::fs::write(&fa, "v1-a\n").unwrap();
        std::fs::write(&fb, "v1-b\n").unwrap();

        let script = json!([
            // Turn 1: read then overwrite a.txt and b.txt
            { "tool_calls": [
                { "name": "read", "arguments": { "path": "a.txt" } },
                { "name": "read", "arguments": { "path": "b.txt" } }
            ] },
            { "tool_calls": [
                { "name": "write", "arguments": { "path": "a.txt", "content": "turn1-a\n" } },
                { "name": "write", "arguments": { "path": "b.txt", "content": "turn1-b\n" } }
            ] },
            { "text": "turn1 done" },
            // Turn 2 (inserted while /rewind confirm is open): read then overwrite a.txt and b.txt
            { "tool_calls": [
                { "name": "read", "arguments": { "path": "a.txt" } },
                { "name": "read", "arguments": { "path": "b.txt" } }
            ] },
            { "tool_calls": [
                { "name": "write", "arguments": { "path": "a.txt", "content": "turn2-a\n" } },
                { "name": "write", "arguments": { "path": "b.txt", "content": "turn2-b\n" } }
            ] },
            { "text": "turn2 done" }
        ]);

        let h = env.host().builtins(&["changes", "subagent", "agents"]).faux(script).build().await;
        h.prompt("first edit", json!({})).await.unwrap();
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "turn1-a\n");
        assert_eq!(std::fs::read_to_string(&fb).unwrap(), "turn1-b\n");

        // External edit on a.txt triggers conflict confirmation; b.txt has no conflict.
        std::fs::write(&fa, "outside-a\n").unwrap();

        global(&h, "CMD", h.service_table("commands").unwrap());
        global(&h, "A", h.service_table("agent").unwrap());
        exec(
            &h,
            r#"
            NOTES = {}
            mare.g.select_calls = 0
            mare.g.confirm_done = false
            mare.g.b_during_confirm = nil
            mare.ui.notify = function(msg, level)
                NOTES[#NOTES + 1] = tostring(level or "info") .. ":" .. tostring(msg)
            end
            mare.ui.set_editor = function(t) mare.g.editor = t end
            mare.ui.select = function()
                mare.g.select_calls = mare.g.select_calls + 1
                if mare.g.select_calls == 1 then return 1 end -- pick turn 1
                return 2 -- "Restore files and the conversation" (both)
            end
            mare.ui.confirm = function()
                mare.g.b_during_confirm = mare.fs.read("b.txt")
                -- Intervening turn while the confirmation prompt is open: wait until it finishes
                -- editing a.txt and b.txt BEFORE returning `true` to confirm the stale dialog.
                local submitted = A:prompt("second edit", {})
                assert(submitted and submitted.turn_id, mare.json.encode(submitted))
                A:wait(submitted.turn_id)
                mare.g.confirm_done = true
                return true
            end
            CMD:run("/rewind")
            "#,
        )
        .await;

        wait_until(WAIT_SECS, "/rewind confirmation flow to finish", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return mare.g.confirm_done == true and #NOTES > 0").await }
        })
        .await;

        let b_during: String = eval(&h, "return mare.g.b_during_confirm or ''").await;
        assert_eq!(
            b_during, "turn1-b\n",
            "conflict preflight must not touch non-conflicting b.txt before confirmation"
        );

        assert_eq!(
            std::fs::read_to_string(&fa).unwrap(),
            "turn2-a\n",
            "stale /rewind confirmation must not overwrite turn 2 edits to a.txt"
        );
        assert_eq!(
            std::fs::read_to_string(&fb).unwrap(),
            "turn2-b\n",
            "stale /rewind confirmation must not overwrite turn 2 edits to b.txt"
        );

        let last_user: String = eval(
            &h,
            r#"local last = ""
               for _, m in ipairs(A:list_messages().data) do
                   if m.role == "user" and not m.context_disclosure then last = m.content[1].text end
               end
               return last"#,
        )
        .await;
        assert_eq!(last_user, "second edit", "stale /rewind confirmation must not rewind conversation");

        let notes: Vec<String> = eval(&h, "return NOTES").await;
        assert!(
            notes.iter().any(|n| n.contains("session changed while /rewind was open")),
            "expected stale-session warning notification, got {notes:?}"
        );
        h.shutdown().await;
    })
    .await
}

/// Declining conflict confirmation leaves all files and conversation untouched; confirming with
/// no intervening changes restores all files and rewinds the conversation cleanly.
#[tokio::test]
async fn rewind_confirm_decline_leaves_disk_untouched_and_fresh_confirm_restores() {
    local(async {
        let env = TestEnv::new().await;
        let fa = env.cwd.join("a.txt");
        let fb = env.cwd.join("b.txt");
        std::fs::write(&fa, "v1-a\n").unwrap();
        std::fs::write(&fb, "v1-b\n").unwrap();

        let script = json!([
            { "tool_calls": [
                { "name": "read", "arguments": { "path": "a.txt" } },
                { "name": "read", "arguments": { "path": "b.txt" } }
            ] },
            { "tool_calls": [
                { "name": "write", "arguments": { "path": "a.txt", "content": "turn1-a\n" } },
                { "name": "write", "arguments": { "path": "b.txt", "content": "turn1-b\n" } }
            ] },
            { "text": "turn1 done" }
        ]);

        let h = env.host().builtins(&["changes", "subagent", "agents"]).faux(script).build().await;
        h.prompt("first edit", json!({})).await.unwrap();
        std::fs::write(&fa, "outside-a\n").unwrap();

        global(&h, "CMD", h.service_table("commands").unwrap());
        global(&h, "A", h.service_table("agent").unwrap());

        // 1. Decline confirmation: neither a.txt nor b.txt changes, conversation stays.
        exec(
            &h,
            r#"
            NOTES = {}
            mare.g.select_calls = 0
            mare.g.confirm_calls = 0
            mare.ui.notify = function(msg, level)
                NOTES[#NOTES + 1] = tostring(level or "info") .. ":" .. tostring(msg)
            end
            mare.ui.set_editor = function(t) mare.g.editor = t end
            mare.ui.select = function()
                mare.g.select_calls = mare.g.select_calls + 1
                if mare.g.select_calls % 2 == 1 then return 1 end
                return 2 -- "both"
            end
            mare.ui.confirm = function()
                mare.g.confirm_calls = mare.g.confirm_calls + 1
                return false
            end
            CMD:run("/rewind")
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "declined /rewind to finish", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return mare.g.confirm_calls == 1 and #NOTES > 0").await }
        })
        .await;
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "outside-a\n");
        assert_eq!(std::fs::read_to_string(&fb).unwrap(), "turn1-b\n");
        let msg_count: usize = eval(&h, "return #A:list_messages().data").await;
        assert!(msg_count > 0, "declining confirm must not rewind conversation");

        // 2. Confirm cleanly without intervening changes: both files restored & conversation rewound.
        exec(
            &h,
            r#"
            NOTES = {}
            mare.ui.confirm = function()
                mare.g.confirm_calls = mare.g.confirm_calls + 1
                return true
            end
            CMD:run("/rewind")
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "confirmed /rewind to finish", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return mare.g.confirm_calls == 2 and mare.g.editor == 'first edit'").await }
        })
        .await;
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "v1-a\n");
        assert_eq!(std::fs::read_to_string(&fb).unwrap(), "v1-b\n");
        let msg_count_after: usize = eval(&h, "return #A:list_messages().data").await;
        assert_eq!(msg_count_after, 0, "confirmed both-mode /rewind must rewind conversation");
        h.shutdown().await;
    })
    .await
}

/// If the agent becomes busy (e.g. a turn starts) while /rewind's conflict confirmation is
/// open, confirming must be refused without touching disk.
#[tokio::test]
async fn rewind_confirm_refuses_when_agent_becomes_busy_during_confirm() {
    local(async {
        let env = TestEnv::new().await;
        let fa = env.cwd.join("a.txt");
        std::fs::write(&fa, "v1-a\n").unwrap();

        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "turn1-a\n" } }] },
            { "text": "turn1 done" },
            { "text": "slow turn 2", "delay_ms": 400 }
        ]);

        let h = env.host().builtins(&["changes", "subagent", "agents"]).faux(script).build().await;
        h.prompt("first edit", json!({})).await.unwrap();
        std::fs::write(&fa, "outside-a\n").unwrap();

        global(&h, "CMD", h.service_table("commands").unwrap());
        global(&h, "A", h.service_table("agent").unwrap());
        exec(
            &h,
            r#"
            NOTES = {}
            mare.g.select_calls = 0
            mare.g.confirm_done = false
            mare.ui.notify = function(msg, level)
                NOTES[#NOTES + 1] = tostring(level or "info") .. ":" .. tostring(msg)
            end
            mare.ui.select = function()
                mare.g.select_calls = mare.g.select_calls + 1
                if mare.g.select_calls == 1 then return 1 end
                return 1 -- "Restore files" (files-only mode)
            end
            mare.ui.confirm = function()
                -- Start a concurrent turn so agent:activity().busy == true at confirm return.
                mare.task.spawn(function() A:prompt("concurrent turn", {}) end)
                local deadline = mare.task.now() + 5000
                while not A:activity().busy and mare.task.now() < deadline do
                    mare.task.sleep(10)
                end
                mare.g.confirm_done = true
                return true
            end
            CMD:run("/rewind")
            "#,
        )
        .await;

        wait_until(WAIT_SECS, "busy /rewind rejection to finish", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return mare.g.confirm_done == true and #NOTES > 0").await }
        })
        .await;

        assert_eq!(
            std::fs::read_to_string(&fa).unwrap(),
            "outside-a\n",
            "confirming /rewind while a turn is busy must not overwrite files"
        );
        let notes: Vec<String> = eval(&h, "return NOTES").await;
        assert!(
            notes.iter().any(|n| n.contains("A turn is running") || n.contains("session changed")),
            "expected busy or stale-session rejection, got {notes:?}"
        );
        wait_until(WAIT_SECS, "concurrent turn to settle", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return not A:activity().busy").await }
        })
        .await;
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn rewind_shell_warning_persists_across_session_reopen() {
    local(async {
        let env = TestEnv::new().await;
        let file = env.cwd.join("shell-warn.txt");
        std::fs::write(&file, "v1\n").unwrap();

        let h = env
            .host()
            .builtins(&["changes"])
            .faux(json!([
                {
                    "tool_calls": [
                        { "name": "write", "arguments": { "path": "shell-warn-new.txt", "content": "v2\n" } },
                        { "name": "bash", "arguments": { "command": "echo ran-shell" } }
                    ]
                },
                { "text": "done" }
            ]))
            .build()
            .await;
        h.prompt("edit with shell", json!({})).await.unwrap();
        expose(&h);
        global(&h, "CH", h.service_table("changes").unwrap());
        let (session_path, shell_before): (String, bool) = eval(
            &h,
            r#"
            local turns = CH:turns()
            return S:info().path, (#turns == 1 and turns[1].shell == true)
            "#,
        )
        .await;
        assert!(shell_before, "shell flag should be set during original session");
        h.shutdown().await;

        let h2 = env.host().builtins(&["changes"]).faux(json!([])).build().await;
        expose(&h2);
        global(&h2, "CH", h2.service_table("changes").unwrap());
        global(&h2, "P", session_path);
        let (turn_count, shell_after): (usize, bool) = eval(
            &h2,
            r#"
            S:load({ path = P })
            local turns = CH:turns()
            return #turns, (#turns >= 1 and turns[1].shell == true)
            "#,
        )
        .await;
        assert_eq!(turn_count, 1);
        assert!(shell_after, "/rewind shell warning must persist across session reopen");
        h2.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn rewind_menu_offers_three_modes_warns_on_shell_edits_and_refreshes_read_records() {
    local(async {
        let env = TestEnv::new().await;
        let fa = env.cwd.join("a.txt");
        std::fs::write(&fa, "v1-a\n").unwrap();

        let script = json!([
            { "tool_calls": [
                { "name": "read", "arguments": { "path": "a.txt" } },
                { "name": "bash", "arguments": { "command": "echo shell-ran" } }
            ] },
            { "tool_calls": [
                { "name": "write", "arguments": { "path": "a.txt", "content": "v2-a\n" } }
            ] },
            { "text": "done" },
            { "tool_calls": [
                { "name": "edit", "arguments": { "path": "a.txt", "old_string": "v1-a", "new_string": "v3-a" } }
            ] },
            { "text": "edited after rewind" }
        ]);
        let h = env.host().builtins(&["changes", "subagent", "agents"]).faux(script).build().await;
        expose(&h);
        h.prompt("edit with shell", json!({})).await.unwrap();
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "v2-a\n");

        global(&h, "CMD", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"
            NOTES = {}
            SELECT_ITEMS = {}
            mare.g.select_calls = 0
            mare.g.rewind_done = false
            mare.ui.notify = function(msg, level)
                NOTES[#NOTES + 1] = tostring(level or "info") .. ":" .. tostring(msg)
                if tostring(msg):find("Rewound files", 1, true) then
                    mare.g.rewind_done = true
                end
            end
            mare.ui.select = function(opts)
                mare.g.select_calls = mare.g.select_calls + 1
                local labels = {}
                for _, it in ipairs(opts.items or {}) do
                    labels[#labels + 1] = (it.label or "") .. "|" .. (it.detail or "")
                end
                SELECT_ITEMS[mare.g.select_calls] = table.concat(labels, "\n")
                return 1 -- turn 1, then "Restore files" (files-only)
            end
            CMD:run("/rewind")
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "/rewind files-only flow to finish", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return mare.g.rewind_done == true").await }
        })
        .await;

        let (turn_menu, mode_menu, notes): (String, String, Vec<String>) =
            eval(&h, "return SELECT_ITEMS[1] or '', SELECT_ITEMS[2] or '', NOTES").await;
        assert!(turn_menu.contains("shell commands ran"), "turn menu must flag shell usage: {turn_menu}");
        assert!(
            mode_menu.contains("Restore files")
                && mode_menu.contains("Restore files and the conversation")
                && mode_menu.contains("Summarize from here"),
            "mode menu must offer all three choices:\n{mode_menu}"
        );
        assert!(
            notes.iter().any(|n| n.contains("This turn ran shell commands")),
            "expected shell warning notice: {notes:?}"
        );
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "v1-a\n");

        // Post-rewind read records must be refreshed so an edit against the restored content succeeds
        // without requiring a separate re-read.
        let r2 = h.prompt("edit after rewind", json!({})).await.unwrap();
        assert!(r2["error"].is_null(), "edit prompt after /rewind failed: {r2}");
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "v3-a\n");
        h.shutdown().await;
    })
    .await
}

/// `commands:run("/rewind")` in `both` mode must finish both the file restore and the session
/// leaf switch before returning so an immediate `S:create()` never races an active switch lease.
#[tokio::test]
async fn commands_run_rewind_completes_session_switch_before_returning() {
    local(async {
        let env = TestEnv::new().await;
        let fa = env.cwd.join("a.txt");
        std::fs::write(&fa, "v1-a\n").unwrap();

        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "a.txt" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "v2-a\n" } }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["changes", "commands"]).faux(script).build_exposed().await;
        h.prompt("edit a", json!({})).await.unwrap();
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "v2-a\n");

        global(&h, "CMD", h.service_table("commands").unwrap());
        let (switching_after, msg_count, created_id): (bool, usize, String) = eval(
            &h,
            r#"
            local step = 0
            mare.ui.connected = function() return true end
            mare.ui.notify = function() end
            mare.ui.set_editor = function() end
            mare.ui.select = function()
                step = step + 1
                if step == 1 then return 1 end
                return 2 -- Restore files and the conversation
            end
            CMD:run("/rewind")
            local sw = S:switching()
            local count = #A:list_messages().data
            local created = S:create()
            return sw, count, tostring(created and created.session_id or "")
            "#,
        )
        .await;
        assert!(!switching_after, "session switch must already be settled when commands.run(/rewind) returns");
        assert_eq!(msg_count, 0, "conversation must already be rewound before commands.run(/rewind) returns");
        assert!(!created_id.is_empty(), "immediate S:create() after /rewind must succeed without busy error");
        assert_eq!(std::fs::read_to_string(&fa).unwrap(), "v1-a\n");
        h.shutdown().await;
    })
    .await
}
