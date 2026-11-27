//! The session browser and the tree, fork and resume pickers.

use super::*;

#[tokio::test]
async fn session_browser_loads_on_use_and_is_cleared_on_reload() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        global(&h, "CM", h.service_table("commands").unwrap());
        assert!(!h.kernel.is_module_loaded("session", "session.browser"));
        assert!(eval::<bool>(&h, "return CM:get('resume') ~= nil").await);
        exec(&h, "CM:run('/resume')").await;
        wait_until(WAIT_SECS, "the session browser to load", || {
            let h = h.clone();
            async move { h.kernel.is_module_loaded("session", "session.browser") }
        })
        .await;
        h.kernel.reload(Some("session")).await.unwrap();
        assert!(!h.kernel.is_module_loaded("session", "session.browser"));
        h.shutdown().await;
    })
    .await
}

/// The first tab lists the sessions of the project root (a session from a subdirectory of the
/// same repository is there, one from elsewhere is not), and each row shows its git branch.
#[tokio::test]
async fn the_picker_lists_the_project_and_shows_the_git_branch() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".git/refs")).unwrap();
        std::fs::write(env.cwd.join(".git/HEAD"), "ref: refs/heads/feature-x\n").unwrap();
        let sub = env.cwd.join("crates/sub");
        let dir = env.home.path().join("state/sessions");
        for (file, id, cwd, text) in [
            ("--a--/2026-01-01T00-00-00_a.jsonl", "sa", sub.to_string_lossy().into_owned(), "from the subdirectory"),
            ("--b--/2026-01-02T00-00-00_b.jsonl", "sb", "/somewhere/else".to_string(), "from another project"),
        ] {
            write_lines(
                &dir.join(file),
                &[
                    json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": id, "cwd": cwd }),
                    msg(&format!("{id}1"), None, "user", text),
                ],
            );
        }
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        // the branch is recorded when a live writer appends (the directory it runs in)
        global(&h, "P", dir.join("--a--/2026-01-01T00-00-00_a.jsonl").to_string_lossy().to_string());
        global(&h, "SUB", sub.to_string_lossy().to_string());
        exec(&h, "mare.store.index_append({ path = P, cwd = SUB })").await;
        exec(
            &h,
            r#"mare.ui.select = function(o)
                 local t = o.tabs[1]
                 local rows = {}
                 for _, it in ipairs(t.items) do rows[#rows + 1] = (it.label or "") .. " | " .. (it.detail or "") end
                 SEEN, TAB = table.concat(rows, "\n"), t.name
               end
               CM:run("/resume")"#,
        )
        .await;
        wait_until(WAIT_SECS, "the picker", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return SEEN").await.is_some() }
        })
        .await;
        let (seen, tab): (String, String) = eval(&h, "return SEEN, TAB").await;
        assert_eq!(tab, "This project");
        assert!(seen.contains("from the subdirectory") && !seen.contains("from another project"), "{seen}");
        assert!(seen.contains("feature-x"), "the row shows the git branch:\n{seen}");
        h.shutdown().await;
    })
    .await
}

/// An ID collision removes the old row but returns no row from its file update. The open
/// picker's reconcile watcher still has to observe that removal and refresh its items.
#[cfg(unix)]
#[tokio::test]
async fn open_picker_refreshes_when_reconcile_removes_a_duplicate_id_row() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        let changed = dir.join("2026-01-01T00-00-00_old.jsonl");
        let canonical = dir.join("2026-01-02T00-00-00_canonical.jsonl");
        write_lines(
            &changed,
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "old-id", "cwd": env.cwd }),
                msg("old-msg", None, "user", "old preview"),
            ],
        );
        write_lines(
            &canonical,
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "new-id", "cwd": env.cwd }),
                msg("new-msg", None, "user", "canonical preview"),
            ],
        );

        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, "mare.store.index_reconcile()").await;
        exec(
            &h,
            r#"
            RECONCILE_TX, RECONCILE_RX = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            local index_reconcile = mare.store.index_reconcile
            local first_reconcile = true
            mare.store.index_reconcile = function(...)
              if first_reconcile then
                first_reconcile = false
                RECONCILE_STARTED = true
                RECONCILE_TX:send(true)
                RELEASE_RX:recv()
              end
              return index_reconcile(...)
            end
            mare.ui.connected = function() return true end
            mare.ui.select = function(o)
              local function ids(items)
                local out = {}
                for _, item in ipairs(items or {}) do
                  if item.item_id then out[#out + 1] = item.item_id end
                end
                return table.concat(out, "\n")
              end
              INITIAL_IDS = ids(o.tabs[1].items)
              local update_tx, update_rx = mare.task.channel()
              local handle = {
                update = function(_, spec)
                  local items = spec.tabs and spec.tabs[1].items or spec.items
                  UPDATED_IDS = ids(items)
                  update_tx:send(true)
                end,
              }
              o.on_open(handle)
              SELECT_WAITING = true
              update_rx:recv()
              return nil
            end
            "#,
        )
        .await;

        let picker_host = h.clone();
        let picker = tokio::task::spawn_local(async move { exec(&picker_host, "CM:run('/resume')").await });
        // Wait for the refresher's actual call into the store before replacing the source, so the
        // watcher must account for the new row removal in its returned updated/removed counts.
        wait_until(5, "the picker reconcile to start", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return RECONCILE_STARTED and SELECT_WAITING").await }
        })
        .await;

        let replacement = changed.with_extension("replacement");
        write_lines(
            &replacement,
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "new-id", "cwd": env.cwd }),
                msg("new-msg", None, "user", "duplicate preview"),
            ],
        );
        std::fs::rename(&replacement, &changed).unwrap();
        exec(&h, "RELEASE_TX:send(true)").await;

        wait_until(5, "the open picker to drop the replaced ID", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return UPDATED_IDS").await.is_some() }
        })
        .await;
        let (initial, updated): (String, String) = eval(&h, "return INITIAL_IDS, UPDATED_IDS").await;
        assert!(initial.lines().any(|id| id == "old-id"), "initial list: {initial}");
        assert!(!updated.lines().any(|id| id == "old-id"), "refreshed list: {updated}");
        assert!(updated.lines().any(|id| id == "new-id"), "refreshed list: {updated}");

        picker.await.unwrap();
        h.shutdown().await;
    })
    .await
}

/// SE bugs 3, 8, 9: /tree shows every node (answers too, so a label on one is visible) but not
/// `!` shell entries or notifications; its filters cycle; /fork lists other branches too.
#[tokio::test]
async fn the_tree_shows_all_nodes_with_filters_and_fork_lists_every_branch() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "2026-01-01T00-00-00_tr.jsonl",
            &[
                msg("u1", None, "user", "question one"),
                msg("a1", Some("u1"), "assistant", "answer one"),
                json!({ "type": "message", "entry_id": "sh", "parent_entry_id": "a1", "message": { "role": "user",
                        "content": [{ "type": "text", "text": "<bash command=\"ls\">out</bash>" }] } }),
                json!({ "type": "message", "entry_id": "nt", "parent_entry_id": "sh", "message": { "role": "user", "notification": true,
                        "content": [{ "type": "text", "text": "background task finished" }] } }),
                msg("u2", Some("nt"), "user", "question two"),
                msg("a2", Some("u2"), "assistant", "answer two"),
                msg("u3", Some("a1"), "user", "another branch"),
                msg("a3", Some("u3"), "assistant", "answer three"),
                json!({ "type": "label", "target_entry_id": "a1", "label": "LBL" }),
                json!({ "type": "leaf", "target_entry_id": "a2" }),
            ],
        );
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "P", path.to_string_lossy().to_string());
        exec(&h, "S:load({ path = P })").await;
        // capture each view: default, then Ctrl+F through the filters, then Esc
        exec(
            &h,
            r#"VIEWS = {}
               mare.ui.select = function(o)
                 local labels = {}
                 for _, it in ipairs(o.items) do labels[#labels + 1] = it.label end
                 VIEWS[#VIEWS + 1] = table.concat(labels, "\n")
                 if #VIEWS < 5 then return 1, 1, "ctrl+f", "" end
               end
               CM:run("/tree")"#,
        )
        .await;
        wait_until(WAIT_SECS, "five views", || {
            let h = h.clone();
            async move { eval::<i64>(&h, "return #VIEWS").await == 5 }
        })
        .await;
        let views: Vec<String> = eval::<mlua::Table>(&h, "return VIEWS").await.sequence_values().map(|v| v.unwrap()).collect();
        let default = &views[0];
        assert!(default.contains("answer one") && default.contains("[LBL]"), "answers and their labels:\n{default}");
        assert!(default.contains("├─") && default.contains("└─"), "branches are drawn:\n{default}");
        assert!(!default.contains("bash command") && !default.contains("background task"), "{default}");
        assert!(!views[2].contains("answer"), "your messages only:\n{}", views[2]);
        assert!(views[3].contains("LBL") && !views[3].contains("question"), "labeled only:\n{}", views[3]);
        assert!(views[4].contains("bash command"), "everything:\n{}", views[4]);
        // /fork pick: every message of yours, on this branch or not
        exec(&h, r#"mare.ui.select = function(o) FORK = o.items return nil end CM:run("/fork pick")"#).await;
        wait_until(WAIT_SECS, "the fork list", || {
            let h = h.clone();
            async move { eval::<Option<i64>>(&h, "return FORK and #FORK").await.is_some() }
        })
        .await;
        let n: i64 = eval(&h, "return #FORK").await;
        assert_eq!(n, 3, "question one, question two and the other branch");
        h.shutdown().await;
    })
    .await
}

/// The /resume picker: day headings that can't be chosen, forks under their parent, only
/// named sessions, renaming, and the conversation tail loaded only for the highlighted row.
#[tokio::test]
async fn the_resume_picker_nests_forks_filters_names_and_renames() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |file: &str, header: Value, text: &str, name: Option<&str>| {
            let mut lines = vec![
                header,
                msg("m1", None, "user", text),
                msg("m2", Some("m1"), "assistant", &format!("reply to {text}")),
            ];
            if let Some(n) = name {
                lines.push(json!({ "type": "name", "name": n }));
            }
            std::fs::write(dir.join(file), lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        };
        write(
            "2026-01-01T00-00-00_p.jsonl",
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "parent-1", "cwd": env.cwd }),
            "parent work",
            Some("Parent"),
        );
        write(
            "2026-01-02T00-00-00_c.jsonl",
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "child-1", "cwd": env.cwd, "parent_session_id": "parent-1" }),
            "forked work",
            None,
        );
        // an empty session is never listed
        std::fs::write(
            dir.join("2026-01-03T00-00-00_e.jsonl"),
            format!("{}\n", json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "empty-1", "cwd": env.cwd })),
        )
        .unwrap();
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"SEEN = {}
               mare.ui.input = function() return "Renamed" end
               mare.ui.select = function(o)
                 local items = o.tabs and o.tabs[o.tab or 1].items or o.items
                 local labels = {}
                 for _, it in ipairs(items) do labels[#labels + 1] = (it.header and "H:" or "") .. it.label end
                 SEEN[#SEEN + 1] = table.concat(labels, "\n")
                 if #SEEN == 1 then
                   PREVIEW = o.on_highlight(3, 1)
                   return 3, 1, "ctrl+n", ""         -- named only
                 elseif #SEEN == 2 then
                   return 2, 1, "ctrl+r", ""         -- rename the parent
                 end
               end
               CM:run("/resume")"#,
        )
        .await;
        wait_until(WAIT_SECS, "three picker views", || {
            let h = h.clone();
            async move { eval::<i64>(&h, "return #SEEN").await >= 3 }
        })
        .await;
        let seen: Vec<String> =
            eval::<mlua::Table>(&h, "return SEEN").await.sequence_values().map(|v| v.unwrap()).collect();
        let first = &seen[0];
        assert!(first.lines().next().unwrap().starts_with("H:── "), "a heading first:\n{first}");
        assert!(first.contains("↳ forked work"), "the fork sits under its parent:\n{first}");
        assert!(!first.contains("empty"), "empty sessions aren't listed");
        assert!(!seen[1].contains("forked work") && seen[1].contains("Parent"), "named only:\n{}", seen[1]);
        assert!(seen[2].contains("Renamed"), "renamed in the index:\n{}", seen[2]);
        let has_preview: bool = eval(&h, "return PREVIEW ~= nil").await;
        assert!(has_preview, "the highlighted row's preview is built on demand");
        h.shutdown().await;
        let parent = std::fs::read_to_string(dir.join("2026-01-01T00-00-00_p.jsonl")).unwrap();
        assert!(parent.contains("\"name\":\"Renamed\""), "and in the session file, so a resume keeps it");
    })
    .await
}

/// Short ids are git-style (user decision 2026-11-12): the shortest prefix, 8 characters at
/// least, that no other session in the index shares — UUIDv7s started minutes apart share their
/// first 8 digits — so what a row shows is what `-r <id>` takes back.
#[tokio::test]
async fn short_ids_are_the_shortest_unique_prefix_of_eight_or_more() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        for (file, id) in [
            ("2026-01-01T00-00-00_a.jsonl", "01a1cc17-abf6-74ce"),
            ("2026-01-01T00-00-01_b.jsonl", "01a1cc17-b03f-76f5"),
            ("2026-01-01T00-00-02_c.jsonl", "77aa0000-0000-0000"),
        ] {
            write_lines(
                &dir.join(file),
                &[json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": id, "cwd": env.cwd }), msg(&format!("{id}-1"), None, "user", "hi")],
            );
        }
        let h = env.host().build().await;
        expose(&h);
        exec(&h, "mare.store.index_reconcile()").await;
        let got: (String, String, String) = eval(
            &h,
            "return S:short_id('01a1cc17-abf6-74ce'), S:short_id('01a1cc17-b03f-76f5'), S:short_id('77aa0000-0000-0000')",
        )
        .await;
        assert_eq!(got, ("01a1cc17-a".into(), "01a1cc17-b".into(), "77aa0000".into()));
        h.shutdown().await;
    })
    .await
}

/// Item 4 (self-written): `/resume` picker archive (`ctrl+x`) and archived-list unarchive (`ctrl+a`)
/// must reuse `session.archive`/`session.unarchive` so child agent sessions cascade too.
#[tokio::test]
async fn picker_archive_and_unarchive_cascade_to_child_sessions() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        write_lines(
            &dir.join("2026-01-01T00-00-00_p.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "p-pick", "cwd": env.cwd }),
                msg("m1", None, "user", "parent task"),
                json!({ "type": "name", "name": "PickParent" }),
            ],
        );
        write_lines(
            &dir.join("2026-01-02T00-00-00_o.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "p-other", "cwd": env.cwd }),
                msg("m2", None, "user", "other task"),
                json!({ "type": "name", "name": "OtherSession" }),
            ],
        );
        let child_dir = dir.join("agents/run-1");
        std::fs::create_dir_all(&child_dir).unwrap();
        write_lines(
            &child_dir.join("001-c-pick.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "c-pick", "parent_session_id": "p-pick", "run_id": "run-1", "label": "worker", "cwd": env.cwd }),
                msg("cm1", None, "user", "child work"),
            ],
        );

        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        exec(&h, "mare.store.index_reconcile()").await;
        let (child_arch_after_picker_arch, child_unarch_after_picker_unarch, diag): (bool, bool, String) = eval(
            &h,
            r#"
            local logs = {}
            local step = 0
            local last_idx = "none"
            mare.ui.confirm = function() logs[#logs + 1] = "confirm"; return true end
            mare.ui.notify = function(msg) logs[#logs + 1] = "notify:" .. tostring(msg) end
            local done = false
            mare.ui.select = function(o)
                step = step + 1
                local items = o.tabs and o.tabs[o.tab or 1].items or o.items or {}
                if step == 1 then
                    -- Archive the highlighted parent session via ctrl+x
                    for idx, item in ipairs(items) do
                        if tostring(item.label):find("PickParent", 1, true) then
                            last_idx = tostring(idx)
                            return idx, 1, "ctrl+x", ""
                        end
                    end
                    error("PickParent not found in picker")
                elseif step == 2 then
                    done = true
                    return nil
                end
                done = true
                return nil
            end
            local pre = mare.store.index_read({ scope = "all", kind = "agent", parent_session_id = "p-pick", archived = "any" })
            local ok_cmd, cmd_err = pcall(function() return C.cordis:get("commands"):run("/resume") end)
            local dl = mare.task.clock() + 5000
            while not done and mare.task.clock() < dl do mare.task.sleep(10) end
            local arch_rows = mare.store.index_read({ scope = "all", kind = "agent", parent_session_id = "p-pick", archived = true })
            -- Now unarchive via ctrl+a -> Enter
            step = 0
            done = false
            mare.ui.select = function(o)
                step = step + 1
                local items = o.tabs and o.tabs[o.tab or 1].items or o.items or {}
                if step == 1 then
                    return 2, 1, "ctrl+a", ""
                elseif step == 2 then
                    for idx, item in ipairs(items) do
                        if tostring(item.label):find("PickParent", 1, true) then
                            return idx
                        end
                    end
                    error("PickParent not found in archived picker")
                end
                done = true
                return nil
            end
            C.cordis:get("commands"):run("/resume")
            dl = mare.task.clock() + 5000
            while not done and mare.task.clock() < dl do mare.task.sleep(10) end
            local unarch_rows = mare.store.index_read({ scope = "all", kind = "agent", parent_session_id = "p-pick", archived = false })
            return #arch_rows == 1, #unarch_rows == 1, ("pre=%d idx=%s ok=%s err=%s logs=%s"):format(#pre, last_idx, tostring(ok_cmd), tostring(cmd_err), table.concat(logs, " | "))
            "#,
        )
        .await;
        assert!(child_arch_after_picker_arch, "picker ctrl+x archive must cascade to child session: {diag}");
        assert!(child_unarch_after_picker_unarch, "picker ctrl+a unarchive must cascade to child session: {diag}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn picker_archive_and_unarchive_display_child_failure_details() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        write_lines(
            &dir.join("2026-01-01T00-00-00_p.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "p-det", "cwd": env.cwd }),
                msg("m1", None, "user", "parent task"),
                json!({ "type": "name", "name": "DetailParent" }),
            ],
        );
        write_lines(
            &dir.join("2026-01-02T00-00-00_o.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "p-det-other", "cwd": env.cwd }),
                msg("m2", None, "user", "other task"),
                json!({ "type": "name", "name": "OtherSession" }),
            ],
        );
        let child_dir = dir.join("agents/run-det");
        std::fs::create_dir_all(&child_dir).unwrap();
        let child_path = child_dir.join("001-c-det.jsonl");
        write_lines(
            &child_path,
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "c-det", "parent_session_id": "p-det", "run_id": "run-det", "label": "worker", "cwd": env.cwd }),
                msg("cm1", None, "user", "child work"),
            ],
        );

        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        exec(&h, "mare.store.index_reconcile()").await;
        global(&h, "CHILD_PATH", child_path.to_string_lossy().to_string());

        let (arch_notify, unarch_notify): (String, String) = eval(
            &h,
            r#"
            -- Hold a lock on the child session so archiving/unarchiving the child fails with a specific error.
            local locked_child = mare.store.open(CHILD_PATH, { existing = true })

            local notifications = {}
            mare.ui.confirm = function() return true end
            mare.ui.notify = function(msg) notifications[#notifications + 1] = tostring(msg) end

            local step = 0
            local done = false
            mare.ui.select = function(o)
                step = step + 1
                local items = o.tabs and o.tabs[o.tab or 1].items or o.items or {}
                if step == 1 then
                    for idx, item in ipairs(items) do
                        if tostring(item.label):find("DetailParent", 1, true) then
                            return idx, 1, "ctrl+x", ""
                        end
                    end
                    error("DetailParent not found in picker")
                end
                done = true
                return nil
            end
            C.cordis:get("commands"):run("/resume")
            local dl = mare.task.clock() + 5000
            while not done and mare.task.clock() < dl do mare.task.sleep(10) end
            local arch_notify = table.concat(notifications, " | ")

            notifications = {}
            step = 0
            done = false
            mare.ui.select = function(o)
                step = step + 1
                local items = o.tabs and o.tabs[o.tab or 1].items or o.items or {}
                if step == 1 then
                    return 2, 1, "ctrl+a", ""
                elseif step == 2 then
                    for idx, item in ipairs(items) do
                        if tostring(item.label):find("DetailParent", 1, true) then
                            return idx
                        end
                    end
                    error("DetailParent not found in archived picker")
                end
                done = true
                return nil
            end
            C.cordis:get("commands"):run("/resume")
            dl = mare.task.clock() + 5000
            while not done and mare.task.clock() < dl do mare.task.sleep(10) end
            local unarch_notify = table.concat(notifications, " | ")

            pcall(locked_child.close, locked_child)
            return arch_notify, unarch_notify
            "#,
        )
        .await;

        assert!(
            arch_notify.contains("c-det") && !arch_notify.contains("(nil)"),
            "picker archive notification must include child failure details, got: {arch_notify}"
        );
        assert!(
            unarch_notify.contains("c-det") && !unarch_notify.contains("(nil)"),
            "picker unarchive notification must include child failure details, got: {unarch_notify}"
        );
        h.shutdown().await;
    })
    .await
}
