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
async fn subagent_shares_parent_turn_without_clobbering_or_clearing_journal() {
    local(async {
        let env = TestEnv::new().await;
        let parent_file = env.cwd.join("parent.txt");
        let child_new = env.cwd.join("child-new.txt");
        std::fs::write(&parent_file, "parent-v1\n").unwrap();

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

        // Spawn a child subagent while the parent's snapshot turn is active.
        let child_ok: bool = eval(
            &h,
            r#"local a = mare.subagent.start({ cwd = CWD, prompt = "create child-new.txt", no_session = true })
               local ok = pcall(a.wait, a)
               return ok"#,
        )
        .await;
        assert!(child_ok);
        assert_eq!(std::fs::read_to_string(&parent_file).unwrap(), "parent-v2\n");
        assert_eq!(std::fs::read_to_string(&child_new).unwrap(), "child-v2\n");

        // Parent's journal must still have only parent-t1 (not overwritten or cleared by child exit),
        // covering both parent.txt and child-new.txt, and restore both cleanly.
        let (turn_count, file_count): (usize, usize) = eval(
            &h,
            r#"local turns = C:turns()
               return #turns, #turns[1].files"#,
        )
        .await;
        assert_eq!(turn_count, 1);
        assert_eq!(file_count, 2);
        let restored: usize = eval(&h, r#"return #C:rewind("parent-t1", {}).restored"#).await;
        assert_eq!(restored, 2);
        assert_eq!(std::fs::read_to_string(&parent_file).unwrap(), "parent-v1\n");
        assert!(!child_new.exists(), "child-created file must be removed on parent rewind");
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

fn install_splitfaux_plugin(env: &TestEnv) {
    let plugins = env.config_dir().join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("splitfaux.lua"),
        r#"return {
    name = "splitfaux",
    inject = { "llm" },
    config_schema = {
        parent_json = { type = "string", desc = "Parent scripted turns as JSON." },
        child_json = { type = "string", desc = "Child scripted turns as JSON." },
    },
    apply = function(ctx, cfg)
        local raw = mare.mode == "child" and cfg.child_json or cfg.parent_json
        local script = raw and mare.json.decode(raw) or {}
        local turn = 0
        ctx.llm:register_provider({
            provider = "splitfaux",
            default_model = "echo",
            is_configured = function() return true end,
            models = {
                { model_id = "echo", name = "Split Faux", context_window = 100000, max_output = 4096 },
            },
            stream = function(_req, sink)
                turn = turn + 1
                local step = script[turn] or { text = "done" }
                local content = {}
                if step.text then
                    sink({ type = "text_delta", delta = step.text })
                    content[#content + 1] = { type = "text", text = step.text }
                end
                for n, tc in ipairs(step.tool_calls or {}) do
                    local id = "call_" .. turn .. "_" .. n
                    sink({ type = "tool_call_start", tool_call_id = id, name = tc.name })
                    content[#content + 1] = { type = "tool_call", id = id, name = tc.name, arguments = tc.arguments }
                end
                return {
                    role = "assistant",
                    content = content,
                    stop_reason = (#(step.tool_calls or {}) > 0) and "tool_use" or "stop",
                    usage = { input = 1, output = 1, cache_read = 0, cache_write = 0 },
                }
            end,
        })
    end,
}"#,
    )
    .unwrap();
}

#[tokio::test]
async fn subagent_edits_appear_in_diff_and_rewind_restores_disk() {
    local(async {
        let env = TestEnv::new().await;
        install_splitfaux_plugin(&env);

        let existing = env.cwd.join("existing.txt");
        let shared = env.cwd.join("shared.txt");
        let created = env.cwd.join("subagent-new.txt");
        std::fs::write(&existing, "original existing\n").unwrap();
        std::fs::write(&shared, "original shared\n").unwrap();

        let parent_script = json!([
            {
                "tool_calls": [{
                    "name": "subagent",
                    "arguments": {
                        "prompt": "edit existing.txt, shared.txt, and create subagent-new.txt",
                        "name": "editor-worker",
                        "wait": true
                    }
                }]
            },
            { "tool_calls": [{ "name": "read", "arguments": { "path": "shared.txt" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "shared.txt", "content": "parent after subagent\n" } }] },
            { "text": "parent turn 1 done" },
            { "tool_calls": [{ "name": "write", "arguments": { "path": "after-child.txt", "content": "turn 2 file\n" } }] },
            { "text": "parent turn 2 done" }
        ]);
        let child_script = json!([
            {
                "tool_calls": [
                    { "name": "read", "arguments": { "path": "existing.txt" } },
                    { "name": "read", "arguments": { "path": "shared.txt" } }
                ]
            },
            {
                "tool_calls": [
                    { "name": "edit", "arguments": { "path": "existing.txt", "old_string": "original existing", "new_string": "edited by subagent" } },
                    { "name": "write", "arguments": { "path": "shared.txt", "content": "subagent middle\n" } },
                    { "name": "write", "arguments": { "path": "subagent-new.txt", "content": "created by subagent\n" } }
                ]
            },
            { "text": "child done" }
        ]);

        let h = env
            .host()
            .builtins(&["changes", "subagent", "agents", "commands"])
            .user_plugins()
            .config(json!({
                "model": "splitfaux/echo",
                "plugins": {
                    "splitfaux": {
                        "parent_json": parent_script.to_string(),
                        "child_json": child_script.to_string(),
                    }
                }
            }))
            .build_exposed()
            .await;

        let res = h.prompt("delegate edits to subagent", json!({})).await.unwrap();
        assert_eq!(res["text"], "parent turn 1 done", "parent prompt failed: {res}");
        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "edited by subagent\n");
        assert_eq!(std::fs::read_to_string(&created).unwrap(), "created by subagent\n");
        assert_eq!(std::fs::read_to_string(&shared).unwrap(), "parent after subagent\n");

        global(&h, "CH", h.service_table("changes").unwrap());
        global(&h, "CMD", h.service_table("commands").unwrap());

        // 1. changes:diffs() and /diff overlay must list all three files under the parent turn.
        let diff_summary: String = eval(
            &h,
            r#"
            local diffs = CH:diffs()
            assert(#diffs == 1, "expected 1 turn in diffs, got " .. #diffs)
            local by_name = {}
            for _, f in ipairs(diffs[1].files) do
                local name = f.path:match("([^/\\]+)$")
                by_name[name] = (f.old or "") .. "=>" .. (f.new or "")
            end
            return table.concat({
                "existing:" .. tostring(by_name["existing.txt"]),
                "created:" .. tostring(by_name["subagent-new.txt"]),
                "shared:" .. tostring(by_name["shared.txt"]),
            }, "|")
            "#,
        )
        .await;
        assert_eq!(
            diff_summary,
            "existing:original existing\n=>edited by subagent\n|created:=>created by subagent\n|shared:original shared\n=>parent after subagent\n"
        );

        exec(
            &h,
            r#"
            mare.g.overlay_text = nil
            mare.ui.overlay = function(title, node)
                mare.g.overlay_text = tostring(title) .. "\n" .. mare.json.encode(node)
            end
            CMD:run("/diff")
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "/diff overlay to render", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return mare.g.overlay_text").await.is_some() }
        })
        .await;
        let overlay_text: String = eval(&h, "return mare.g.overlay_text or ''").await;
        assert!(overlay_text.contains("existing.txt"), "/diff overlay missing existing.txt: {overlay_text}");
        assert!(overlay_text.contains("subagent-new.txt"), "/diff overlay missing subagent-new.txt: {overlay_text}");
        assert!(overlay_text.contains("shared.txt"), "/diff overlay missing shared.txt: {overlay_text}");

        // 2. /rewind in the parent must restore modified files and delete subagent-created files on disk.
        exec(
            &h,
            r#"
            mare.g.rewind_done = false
            mare.ui.notify = function(msg)
                if tostring(msg):find("Rewound files", 1, true) then
                    mare.g.rewind_done = true
                end
            end
            mare.ui.select = function() return 1 end -- turn 1, then "Restore files"
            CMD:run("/rewind")
            "#,
        )
        .await;
        wait_until(WAIT_SECS, "/rewind to restore subagent files", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return mare.g.rewind_done == true").await }
        })
        .await;

        assert_eq!(std::fs::read_to_string(&existing).unwrap(), "original existing\n");
        assert!(!created.exists(), "subagent-created file must be removed by /rewind");
        assert_eq!(std::fs::read_to_string(&shared).unwrap(), "original shared\n");

        // 3. Parent snapshot journal must remain active after child shutdown.
        let res2 = h.prompt("write after child shutdown", json!({})).await.unwrap();
        assert_eq!(res2["text"], "parent turn 2 done");
        let after_child = env.cwd.join("after-child.txt");
        assert_eq!(std::fs::read_to_string(&after_child).unwrap(), "turn 2 file\n");
        let turn_count: usize = eval(&h, "return #CH:turns()").await;
        assert_eq!(turn_count, 2, "parent journal must continue recording turns after child exits");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn subagent_shell_command_marks_parent_turn_and_persists_across_reopen() {
    local(async {
        let env = TestEnv::new().await;
        install_splitfaux_plugin(&env);

        let parent_script = json!([
            {
                "tool_calls": [{
                    "name": "subagent",
                    "arguments": {
                        "prompt": "write a file and run bash",
                        "name": "shell-worker",
                        "wait": true
                    }
                }]
            },
            { "text": "parent done" }
        ]);
        let child_script = json!([
            {
                "tool_calls": [
                    { "name": "write", "arguments": { "path": "sub-shell.txt", "content": "from subagent\n" } },
                    { "name": "bash", "arguments": { "command": "echo subagent-shell" } }
                ]
            },
            { "text": "child shell done" }
        ]);

        let h = env
            .host()
            .builtins(&["changes", "subagent", "agents"])
            .user_plugins()
            .config(json!({
                "model": "splitfaux/echo",
                "plugins": {
                    "splitfaux": {
                        "parent_json": parent_script.to_string(),
                        "child_json": child_script.to_string(),
                    }
                }
            }))
            .build_exposed()
            .await;

        let res = h.prompt("delegate shell work", json!({})).await.unwrap();
        assert_eq!(res["text"], "parent done", "parent prompt failed: {res}");
        let sub_file = env.cwd.join("sub-shell.txt");
        assert_eq!(std::fs::read_to_string(&sub_file).unwrap(), "from subagent\n");

        global(&h, "CH", h.service_table("changes").unwrap());
        let (session_path, turn_id, shell_before): (String, String, bool) = eval(
            &h,
            r#"
            local turns = CH:turns()
            assert(#turns == 1, "expected 1 turn, got " .. #turns)
            return S:info().path, turns[1].turn, turns[1].shell == true
            "#,
        )
        .await;
        assert!(shell_before, "parent turn must inherit shell=true when a subagent runs bash");
        h.shutdown().await;

        let h2 = env.host().builtins(&["changes"]).faux(json!([])).build_exposed().await;
        global(&h2, "CH", h2.service_table("changes").unwrap());
        global(&h2, "P", session_path);
        global(&h2, "TID", turn_id);
        let shell_after: bool = eval(
            &h2,
            r#"
            S:load({ path = P })
            local turns = CH:turns()
            assert(#turns == 1, "expected 1 turn after reopen, got " .. #turns)
            local r = CH:rewind(TID, {})
            assert(#r.restored == 1, "expected 1 restored file after reopen, got " .. #r.restored)
            return turns[1].shell == true
            "#,
        )
        .await;
        assert!(shell_after, "subagent shell warning must persist across session reopen");
        assert!(!sub_file.exists(), "subagent file must be removed by rewind after session reopen");
        h2.shutdown().await;
    })
    .await
}
