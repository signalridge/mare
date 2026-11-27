//! Branching: set_leaf, forks, branch summaries, retry and undo.

use super::*;

#[tokio::test]
async fn set_leaf_branches_in_place_and_fork_copies_the_path() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        h.prompt("one", json!({})).await.unwrap();
        h.prompt("two", json!({})).await.unwrap();
        expose(&h);
        // branch from before "two": move the leaf to the first answer
        let n: i64 =
            eval(&h, "local path = S:path_to(); S:set_leaf(path[2].entry_id); return #A:list_messages().data").await;
        assert_eq!(n, 2, "context rebuilt from the chosen branch point");
        h.prompt("three", json!({})).await.unwrap();
        let (users, path_len, file): (i64, i64, String) = eval(
            &h,
            r#"local users = 0
               for _, e in ipairs(S:entries()) do
                 if e.type == "message" and e.message.role == "user" then users = users + 1 end
               end
               return users, #S:path_to(), S:info().path"#,
        )
        .await;
        assert_eq!(users, 3, "both branches are kept in the tree");
        assert_eq!(path_len, 4, "current path: one, answer, three, answer");
        let (forked, msgs): (String, i64) =
            eval(&h, "local path = S:path_to(); S:fork({ session_id = S:info().session_id, before_item_id = path[3].entry_id }); return S:info().path, #A:list_messages().data").await;
        assert_ne!(forked, file);
        assert_eq!(msgs, 2, "the fork holds only the path before the fork point");
        assert!(std::path::Path::new(&forked).exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_keeps_branch_ancestry_across_another_branchs_compaction() {
    local(async {
        let env = TestEnv::new().await;
        // branch A: u1 a1 u2 a2 + compaction (keeps u2); back to a1, new branch u3 a3
        env.write_session(
            "2026-01-01T00-00-00_test.jsonl",
            &[
                msg("u1", None, "user", "original task"),
                msg("a1", Some("u1"), "assistant", "ack"),
                msg("u2", Some("a1"), "user", "branch A"),
                msg("a2", Some("u2"), "assistant", "on A"),
                json!({ "type": "compaction", "entry_id": "c1", "parent_entry_id": "a2", "summary": "A summary", "first_kept_entry_id": "u2" }),
                json!({ "type": "leaf", "target_entry_id": "a1" }),
                msg("u3", Some("a1"), "user", "branch B"),
                msg("a3", Some("u3"), "assistant", "on B"),
            ],
        );
        let h = env.host().resume().build().await;
        let st = h.state().await;
        assert_eq!(st["message_count"], 4, "u1 a1 u3 a3 must survive: {st}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fork_header_records_the_parent_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "a" }, { "text": "b" }])).build().await;
        expose(&h);
        h.prompt("one", json!({})).await.unwrap();
        h.prompt("two", json!({})).await.unwrap();
        let (parent, child_path): (String, String) = eval(
            &h,
            "local id = S:info().session_id S:fork({ session_id = S:info().session_id }) return id, S:info().path",
        )
        .await;
        h.shutdown().await;
        let header: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&child_path).unwrap().lines().next().unwrap()).unwrap();
        assert_eq!(header["parent_session_id"], parent.as_str(), "{header}");
    })
    .await
}

#[tokio::test]
async fn branch_summary_entry_is_part_of_the_resumed_context() {
    local(async {
        let env = TestEnv::new().await;
        env.write_session(
            "2026-01-01T00-00-00_bs.jsonl",
            &[
                msg("u1", None, "user", "start"),
                msg("a1", Some("u1"), "assistant", "ok"),
                json!({ "type": "branch_summary", "entry_id": "bs1", "parent_entry_id": "a1", "from_entry_id": "x9",
                        "summary": "BRANCH-MARKER tried X, failed", "files": ["src/x.rs"] }),
                msg("u2", Some("bs1"), "user", "go on"),
            ],
        );
        let h = env.host().faux(json!([{ "text": "fine" }])).resume().build().await;
        expose(&h);
        exec(&h, r#"C.cordis:on("request/build", function(req, next) LAST = mare.json.encode(req.messages); return next() end)"#).await;
        h.continue_turn().await.unwrap();
        let last: String = eval(&h, "return LAST").await;
        assert!(last.contains("BRANCH-MARKER"), "{last}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn branch_summary_preserves_earlier_files_and_reports_dropped_history() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "2026-01-01T00-00-00_branch_files.jsonl",
            &[
                msg("u1", None, "user", "start"),
                msg("a1", Some("u1"), "assistant", "ok"),
                json!({ "type": "branch_summary", "entry_id": "bs1", "parent_entry_id": "a1", "from_entry_id": "x9",
                        "summary": "earlier work", "files": ["src/previous.rs"] }),
                msg("u2", Some("bs1"), "user", "new work"),
            ],
        );
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let h =
            env.host().builtins(&["compact"]).faux(json!([overflow, { "text": "new summary" }])).resume().build().await;
        expose(&h);
        fake_ui(&h).await;
        let ok: bool = eval(&h, r#"local r = S:rewind({ session_id = S:info().session_id, before_item_id = "a1", summarize = true }); return type(r) == "table" and type(r.session_id) == "string""#).await;
        assert!(ok);
        let entries: Vec<serde_json::Value> =
            std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let summary = entries.iter().rev().find(|entry| entry["type"] == "branch_summary").unwrap();
        assert_eq!(summary["parent_entry_id"], "u1");
        assert_eq!(summary["files"], json!(["src/previous.rs"]));
        let text = summary["summary"].as_str().unwrap();
        assert!(text.contains("new summary") && text.contains("oldest 1") && text.contains("u2"), "{text}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tree_branch_summary_carries_files_from_an_abandoned_summary() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "2026-01-01T00-00-00_tree_files.jsonl",
            &[
                msg("u1", None, "user", "start"),
                msg("a1", Some("u1"), "assistant", "return here"),
                json!({ "type": "branch_summary", "entry_id": "bs1", "parent_entry_id": "a1", "from_entry_id": "x9",
                        "summary": "earlier work", "files": ["src/previous.rs"] }),
                msg("u2", Some("bs1"), "user", "new work"),
            ],
        );
        let h = env.host().faux(json!([{ "text": "new summary" }])).resume().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(
            &h,
            r#"mare.ui.select = function(o)
                 if o.title:find("Session tree", 1, true) then
                   for i, item in ipairs(o.items) do if item.label:find("return here", 1, true) then return i end end
                 end
                 return 2
               end
               CM:run("/tree")"#,
        )
        .await;
        wait_until(WAIT_SECS, "the branch summary", || {
            let h = h.clone();
            async move {
                eval::<bool>(
                    &h,
                    r#"local p = S:path_to() return p[#p] and p[#p].type == "branch_summary" and p[#p].entry_id ~= "bs1""#,
                )
                .await
            }
        })
        .await;
        let entries: Vec<Value> =
            std::fs::read_to_string(path).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        let summary = entries.iter().rev().find(|entry| entry["type"] == "branch_summary").unwrap();
        assert_eq!(summary["parent_entry_id"], "a1");
        assert_eq!(summary["files"], json!(["src/previous.rs"]));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn forking_a_session_that_is_still_being_written_is_refused() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_durable_session("2026-01-01T00-00-00_busy.jsonl", &[msg("m1", None, "user", "hello")]);
        // another writer holds the session and keeps appending (a real process)
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        // the file changes during every read (deterministic: a real background writer races the
        // read and can look stable on a loaded machine)
        // SAFETY: env-dependent tests are serialized by TestEnv's global lock
        unsafe { std::env::set_var("MARE_TEST_UNSTABLE_READ", "2026-01-01T00-00-00_busy.jsonl") };
        let h = env.host().build().await;
        expose(&h);
        global(&h, "PATH", path.to_string_lossy().to_string());
        // the user picks "Fork into a new session"
        let (ok, err): (bool, String) = eval(
            &h,
            r#"mare.ui.connected = function() return true end
               mare.ui.select = function() return 1 end
               local ok, e = pcall(S.load, S, { path = PATH }); return ok, tostring(e)"#,
        )
        .await;
        assert!(!ok && err.contains("no recorded completed turn"), "{err}");
        // A read-only view whose source keeps changing is refused without switching.
        let (ok, err, readonly): (bool, String, bool) = eval(
            &h,
            r#"mare.ui.select = function() return 2 end
               local ok, e = pcall(S.load, S, { path = PATH })
               local info = S:info()
               return ok, tostring(e), info.readonly == true"#,
        )
        .await;
        unsafe { std::env::remove_var("MARE_TEST_UNSTABLE_READ") };
        assert!(!ok && err.contains("changed") && !readonly, "{err}");
        drop(lock);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn retry_branches_at_the_last_user_message_and_sends_it_again() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "first answer" }, { "text": "second answer" }])).build().await;
        h.prompt("hello", json!({})).await.unwrap();
        expose(&h);
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/retry")"#).await;
        wait_until(WAIT_SECS, "the retried answer", || {
            let h = h.clone();
            async move {
                let n: i64 = eval(
                    &h,
                    r#"local n = 0
                       for _, e in ipairs(S:entries()) do
                         if e.type == "message" and e.message.role == "assistant" then n = n + 1 end
                       end
                       return n"#,
                )
                .await;
                n == 2 && !h.is_busy().await
            }
        })
        .await;
        let (path_users, last, tree_users): (i64, String, i64) = eval(
            &h,
            r#"local path, users = S:path_to(), 0
               for _, e in ipairs(path) do
                 if e.type == "message" and e.message.role == "user" then users = users + 1 end
               end
               local tree = 0
               for _, e in ipairs(S:entries()) do
                 if e.type == "message" and e.message.role == "user" then tree = tree + 1 end
               end
               local m = path[#path].message
               return users, m.content[1].text, tree"#,
        )
        .await;
        assert_eq!(path_users, 1, "the current path holds the message once");
        assert_eq!(last, "second answer");
        assert_eq!(tree_users, 2, "the old branch (with the first answer) is kept in the tree");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_fork_keeps_labels_prunes_the_model_and_the_message_count() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "2026-01-01T00-00-00_fk.jsonl",
            &[
                msg("m1", None, "user", "first prompt"),
                msg("m2", Some("m1"), "tool_result", "a very long tool output"),
                msg("m3", Some("m2"), "assistant", "done"),
                json!({"type":"custom", "kind":"agent/run-end", "data":{"session_id":"s1", "run_id":"r1", "leaf_entry_id":"m3"}}),
                json!({ "type": "label", "target_entry_id": "m3", "label": "good point" }),
                json!({ "type": "prune", "target_entry_ids": { "m2": [{ "type": "text", "text": "[pruned]" }] } }),
                json!({ "type": "model", "model": "faux/echo", "effort": "high", "at_entry_id": "m3" }),
                json!({ "type": "custom", "kind": "sandbox/mode", "provenance": "session-host", "data": { "level": "read-only" } }),
            ],
        );
        let h = env.host().build().await;
        expose(&h);
        global(&h, "P", path.to_string_lossy().to_string());
        let fork: String = eval(&h, "S:load({ path = P }) S:fork({ session_id = S:info().session_id }) return S:info().path").await;
        h.shutdown().await;
        let entries: Vec<Value> =
            std::fs::read_to_string(&fork).unwrap().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let has = |f: &dyn Fn(&Value) -> bool| entries.iter().any(f);
        assert!(
            has(&|e| e["type"] == "label" && e["target_entry_id"] == "m3" && e["label"] == "good point"),
            "{entries:?}"
        );
        assert!(
            has(&|e| e["type"] == "prune" && e["target_entry_ids"]["m2"][0]["text"] == "[pruned]"),
            "pruned stays pruned"
        );
        assert!(has(&|e| e["type"] == "model" && e["effort"] == "high"));
        assert!(has(&|e| e["kind"] == "sandbox/mode"));
        let fork = mare_store::index::norm(std::path::Path::new(&fork));
        let line = index_rows().into_iter().find(|l| l["path"] == fork.as_str()).expect("the fork is indexed");
        assert_eq!((line["messages"].as_i64(), line["preview"].as_str()), (Some(3), Some("first prompt")), "{line}");
        assert_eq!(line["parent_session_id"], "s1");
    })
    .await
}

#[tokio::test]
async fn set_leaf_checks_its_target_and_ids_resist_collisions_and_cycles() {
    local(async {
        let env = TestEnv::new().await;
        // two entries that name each other as parents (an old 8-char id collision)
        let path = env.write_session(
            "2026-01-01T00-00-00_cy.jsonl",
            &[msg("aa", Some("bb"), "user", "one"), msg("bb", Some("aa"), "assistant", "two")],
        );
        let h = env.host().faux(json!([{ "text": "x" }])).build().await;
        expose(&h);
        global(&h, "P", path.to_string_lossy().to_string());
        let n: i64 = eval(&h, "S:load({ path = P }) return #S:path_to()").await;
        assert!(n <= 2, "a parent cycle ends the walk");
        let (ok, err, leaf): (Option<bool>, String, String) = eval(
            &h,
            r#"local before = S:leaf()
               local ok, err = S:set_leaf("no-such-entry")
               return ok, tostring(err), tostring(S:leaf() == before)"#,
        )
        .await;
        assert_eq!((ok, leaf.as_str()), (None, "true"), "{err}");
        assert!(err.contains("no such session entry"));
        h.prompt("next", json!({})).await.unwrap();
        let id: String = eval(&h, "return S:leaf()").await;
        assert_eq!(id.len(), 16, "new ids are 16 hex chars: {id}");
        h.shutdown().await;
    })
    .await
}

/// `/fork` without an argument forks from your latest message (agents-ui lua-batch, user
/// decision 2026-11-12): a new session holding what came before it, the message back in the
/// input (without overwriting a draft).
#[tokio::test]
async fn fork_without_an_argument_forks_from_the_latest_message() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "a" }, { "text": "b" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        for p in ["one", "two"] {
            h.prompt(p, json!({})).await.unwrap();
        }
        exec(&h, "OLD = S:info().session_id; CM:run('/fork')").await;
        let (ed, keep, same, users): (String, bool, bool, String) = eval(
            &h,
            r#"local users = {}
               for _, e in ipairs(S:path_to()) do
                 if e.type == "message" and e.message.role == "user" then
                   users[#users + 1] = e.message.content[1].text
                 end
               end
               return ED, ED_KEEP == true, S:info().session_id == OLD, table.concat(users, ",")"#,
        )
        .await;
        assert_eq!((ed.as_str(), keep, same, users.as_str()), ("two", true, false, "one"));
        h.shutdown().await;
    })
    .await
}

/// SE bug 2 (Lua side): /undo, /tree and /fork never put text over a draft — they ask for
/// `keep_draft`, which the TUI honours (suite::tui::keys::double_esc_never_overwrites_a_draft).
#[tokio::test]
async fn undo_asks_to_keep_a_draft_and_without_a_count_opens_a_picker() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "a" }, { "text": "b" }, { "text": "c" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        for p in ["one", "two", "three"] {
            h.prompt(p, json!({})).await.unwrap();
        }
        // the picker lists your prompts newest first; pick the second → back to before "two"
        exec(&h, r#"mare.ui.select = function(o) ITEMS = o.items return 2 end CM:run("/undo")"#).await;
        wait_until(WAIT_SECS, "the rewind", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return ED").await.is_some() }
        })
        .await;
        let (first, ed, keep, users): (String, String, bool, i64) = eval(
            &h,
            r#"local users = 0
               for _, e in ipairs(S:path_to()) do
                 if e.type == "message" and e.message.role == "user" then users = users + 1 end
               end
               return ITEMS[1].label, ED, ED_KEEP == true, users"#,
        )
        .await;
        assert_eq!((first.as_str(), ed.as_str(), keep, users), ("three", "two", true, 1));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn locked_fork_copies_last_recorded_completed_turn() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_durable_session("locked-fork.jsonl", &[
            msg("u1", None, "user", "finished question"),
            msg("a1", Some("u1"), "assistant", "finished answer"),
            json!({"type":"custom", "kind":"agent/run-end", "data":{"session_id":"s1", "run_id":"run1", "leaf_entry_id":"a1"}}),
            msg("u2", Some("a1"), "user", "unfinished question"),
        ]);
        let source = std::fs::read(&path).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().build().await;
        expose(&h);
        global(&h, "PATH", path.to_string_lossy().to_string());
        let fork: String = eval(
            &h,
            r#"mare.ui.connected = function() return true end
            mare.ui.select = function() return 1 end
            S:load({ path = PATH }); return S:info().path"#,
        )
        .await;
        let contents = std::fs::read_to_string(fork).unwrap();
        assert!(contents.contains("finished answer"));
        assert!(!contents.contains("unfinished question"));
        assert_eq!(std::fs::read(&path).unwrap(), source);
        drop(lock);
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fork_rejects_corruption_added_after_open_and_does_not_skip_later_lines() {
    use std::io::Write;
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "done" }])).build().await;
        h.prompt("original", json!({})).await.unwrap();
        expose(&h);
        let path: String = eval(&h, "return S:info().path").await;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "invalid json").unwrap();
        writeln!(file, "{{\"type\":\"name\",\"name\":\"after bad line\"}}").unwrap();
        let (ok, err): (bool, String) =
            eval(&h, "local ok, e = pcall(S.fork, S, { session_id = S:info().session_id }); return ok, tostring(e)")
                .await;
        assert!(!ok && err.contains("session file is corrupt") && err.contains(&path), "{err}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fork_rejects_a_completion_marker_owned_by_another_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("wrong-owner.jsonl", &[
            msg("u", None, "user", "hello"),
            msg("a", Some("u"), "assistant", "done"),
            json!({"type":"custom", "kind":"agent/run-end", "data":{"session_id":"other", "run_id":"r", "leaf_entry_id":"a"}}),
        ]);
        let h = env.host().build().await;
        expose(&h);
        global(&h, "PATH", path.to_string_lossy().to_string());
        let (ok, err): (bool, String) =
            eval(&h, "S:load({ path = PATH }); local ok, e = pcall(S.fork, S, { session_id = S:info().session_id }); return ok, tostring(e)").await;
        assert!(!ok && err.contains("invalid completed turn") && err.contains("wrong-owner.jsonl"), "{err}");
        h.shutdown().await;
    })
    .await;
}

// Codex session/handlers.rs:522 and session/tests.rs:3793: reconstruction must
// affect the next request, including after multiple compactions and a restart.
#[tokio::test]
async fn undo_across_compactions_restores_branch_settings_for_requests_forks_and_reload() {
    local(async {
        let env = TestEnv::new().await;
        let branch_dir = env.cwd.join("branch-directory");
        std::fs::create_dir(&branch_dir).unwrap();
        let path = env.write_session("undo-settings.jsonl", &[
            json!({"type":"model", "model":"faux/echo", "effort":"low", "at_entry_id":false}),
            json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "at_entry_id":false, "data":{"level":"workspace-write"}}),
            msg("u1", None, "user", "first"),
            msg("a1", Some("u1"), "assistant", "first answer"),
            json!({"type":"custom", "kind":"agent/run-end", "data":{"session_id":"s1", "run_id":"r1", "leaf_entry_id":"a1"}}),
            json!({"type":"model", "model":"faux/echo", "effort":"high", "at_entry_id":"a1"}),
            // Legacy unanchored events must inherit a1, including after copying into a fork.
            json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"read-only"}}),
            json!({"type":"custom", "kind":"working-directory/changed", "data":{"to":branch_dir}}),
            msg("u2", Some("a1"), "user", "second"),
            msg("a2", Some("u2"), "assistant", "second answer"),
            json!({"type":"model", "model":"faux/echo2", "at_entry_id":"a2"}),
            json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "at_entry_id":"a2", "data":{"level":"workspace-write"}}),
            json!({"type":"custom", "kind":"working-directory/changed", "at_entry_id":"a2", "data":{"to":env.cwd}}),
            json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"a2", "summary":"first summary"}),
            msg("u3", Some("c1"), "user", "third"),
            msg("a3", Some("u3"), "assistant", "third answer"),
            json!({"type":"compaction", "entry_id":"c2", "parent_entry_id":"a3", "summary":"second summary"}),
        ]);
        let cfg = json!({"plugins":{"provider-faux":{"script_json":"[]"},"permissions":{"enabled":false}}});
        let mut host = env.host().config(cfg.clone());
        host.opts.config.as_object_mut().unwrap().remove("model");
        let h = host.build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "T", h.service_table("tools").unwrap());
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "P", path.to_string_lossy().to_string());
        exec(&h, "S:load({ path = P })").await;
        assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo2");
        exec(&h, "CM:run('/undo 2')").await;
        wait_until(WAIT_SECS, "undo across both compactions", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return ED").await.as_deref() == Some("second") }
        }).await;
        let state: (String, String, String, String, i64) = eval(&h,
            "return L:current().ref, L:raw_effort(), mare.proc.sandbox.current().level, T:cwd(), #A:list_messages().data").await;
        assert_eq!(state, ("faux/echo".into(), "high".into(), "read-only".into(), branch_dir.to_string_lossy().into_owned(), 2));
        // Fork the completed selected point, then prove its older root settings survived.
        let fork: String = eval(&h, "S:fork({ session_id = S:info().session_id }); return S:info().path").await;
        let root: (String, String, String, i64) = eval(&h,
            "S:set_leaf(); return L:current().ref, L:raw_effort(), mare.proc.sandbox.current().level, #A:list_messages().data").await;
        assert_eq!(root, ("faux/echo".into(), "low".into(), "workspace-write".into(), 0));
        exec(&h, "S:set_leaf('a1')").await;
        h.shutdown().await;
        let mut host = env.host().config(cfg);
        host.opts.config.as_object_mut().unwrap().remove("model");
        let h = host.build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "T", h.service_table("tools").unwrap());
        global(&h, "P", fork);
        exec(&h, r#"S:load({ path = P })
            local p = L:provider('faux')
            p.stream = function(req)
                OBSERVED = { req.model.ref, req.effort, mare.proc.sandbox.current().level, T:cwd(), #req.messages }
                return {role='assistant', content={{type='text', text='verified'}}, stop_reason='stop', usage={input=1, output=1}}
            end"#).await;
        let response = h.prompt("after restart", json!({})).await.unwrap();
        assert_eq!(response["text"], "verified", "{response}");
        let actual: (String, String, String, String, i64) = eval(&h, "return table.unpack(OBSERVED)").await;
        assert_eq!(actual, ("faux/echo".into(), "high".into(), "read-only".into(), branch_dir.to_string_lossy().into_owned(), 3));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn navigating_to_an_invalid_or_unpreparable_leaf_keeps_live_state_and_journal() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{"text":"one"}, {"text":"two"}])).build().await;
        expose(&h);
        h.prompt("first", json!({})).await.unwrap();
        h.prompt("second", json!({})).await.unwrap();
        let unchanged: (bool, bool, bool) = eval(
            &h,
            r#"
            local before = S:info().leaf_entry_id
            local messages = A:messages()
            local count = #S:entries()
            local ok = S:set_leaf('missing')
            return ok == nil, S:info().leaf_entry_id == before and A:messages() == messages, #S:entries() == count
        "#,
        )
        .await;
        assert_eq!(unchanged, (true, true, true));
        let path: String = eval(&h, "return S:info().path").await;
        let journal = std::fs::read(&path).unwrap();
        let preparation: (bool, String, bool) = eval(
            &h,
            r#"
            local before, messages = S:info().leaf_entry_id, A:messages()
            local copy = table.copy
            table.copy = function(value)
                if value.type == 'model' then error('injected settings projection failure') end
                return copy(value)
            end
            local ok, err = pcall(S.set_leaf, S, S:path_to()[2].entry_id)
            table.copy = copy
            return ok, tostring(err), S:info().leaf_entry_id == before and A:messages() == messages
        "#,
        )
        .await;
        assert!(!preparation.0 && preparation.1.contains("projection failure") && preparation.2, "{preparation:?}");
        assert_eq!(std::fs::read(&path).unwrap(), journal, "failed preparation must not append a leaf marker");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelled_leaf_navigation_finishes_settings_before_admitting_another_request() {
    local(async {
        let env = TestEnv::new().await;
        let mut host = env.host().faux(json!([{"text":"one"}, {"text":"two"}, {"text":"after"}]));
        host.opts.config.as_object_mut().unwrap().remove("model");
        let h = host.build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("first", json!({})).await.unwrap();
        exec(&h, "L:set_effort('high', {save=false}); TARGET = S:info().leaf_entry_id").await;
        h.prompt("second", json!({})).await.unwrap();
        exec(&h, "L:set_effort('low', {save=false})").await;
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            C.cordis:on('llm/model-changed', function()
                if not PAUSED then
                    PAUSED = true
                    entered_tx:send(true)
                    RELEASE_RX:recv()
                end
            end)
            local caller = mare.task.spawn(function() S:set_leaf(TARGET) end)
            assert(mare.task.timeout(10000, function() entered_rx:recv() end), "navigation did not reach model-changed")
            caller:abort()
            local ok, err = pcall(caller.wait, caller)
            return ok, tostring(err)
        "#,
        )
        .await;
        assert!(
            !ok && err.contains("cancel"),
            "caller must cancel while the service owns committed restoration: {err}"
        );
        let blocked = h.prompt("during restoration", json!({})).await.unwrap_err().to_string();
        assert!(blocked.contains("switch"), "{blocked}");
        exec(&h, "RELEASE_TX:send(true)").await;
        wait_until(WAIT_SECS, "owned navigation restoration", || {
            let h = h.clone();
            async move { !eval::<bool>(&h, "return S:switching()").await }
        })
        .await;
        assert_eq!(eval::<String>(&h, "return L:raw_effort()").await, "high");
        let after = h.prompt("after restoration", json!({})).await.unwrap();
        assert_eq!(after["text"], "after", "{after}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn leaf_navigation_failure_after_model_mutation_blocks_future_requests() {
    local(async {
        let env = TestEnv::new().await;
        let mut host = env.host().faux(json!([{"text":"one"}, {"text":"two"}]));
        host.opts.config.as_object_mut().unwrap().remove("model");
        let h = host.build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("first", json!({})).await.unwrap();
        exec(&h, "L:set_effort('high', {save=false}); TARGET = S:info().leaf_entry_id").await;
        h.prompt("second", json!({})).await.unwrap();
        exec(&h, "L:set_effort('low', {save=false})").await;
        let (ok, error, blocked): (bool, String, bool) = eval(
            &h,
            r#"
            local setter = L.restore_selection
            L.restore_selection = function(self, sel)
                local result = table.pack(setter(self, sel))
                if sel and sel.effort then
                    error('injected error after effort changed')
                end
                return table.unpack(result, 1, result.n)
            end
            local ok, err = pcall(S.set_leaf, S, TARGET)
            L.restore_selection = setter
            return ok, tostring(err), S:switching()
        "#,
        )
        .await;
        assert!(!ok && blocked && error.contains("after effort changed"), "{error}");
        assert!(h.prompt("must not run", json!({})).await.is_err());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn navigation_retains_cli_model_precedence_and_filters_metadata_to_the_selected_path() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "metadata-cli.jsonl",
            &[
                json!({"type":"custom", "kind":"permission/preset", "provenance":"session-host", "at_entry_id":false, "data":{"preset":"read-only"}}),
                msg("u1", None, "user", "first"),
                msg("a1", Some("u1"), "assistant", "first answer"),
                json!({"type":"model", "model":"faux/echo2", "at_entry_id":"a1"}),
                msg("u2", Some("a1"), "user", "second"),
                msg("a2", Some("u2"), "assistant", "second answer"),
                json!({"type":"custom", "kind":"permission/preset", "provenance":"session-host", "at_entry_id":"a2", "data":{"preset":"full-access"}}),
            ],
        );
        let h = env
            .host()
            .user_config(false)
            .config(json!({"plugins":{"permissions":{"enabled":false}}}))
            .faux(json!([{"text":"explicit CLI model"}]))
            .build()
            .await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "P", path.to_string_lossy().to_string());
        let state: (String, String, i64) = eval(
            &h,
            r#"
            S:load({ path = P })
            S:set_leaf('a1')
            local selected = S:list_records('permission/preset', { branch = "current" }).data
            return L:current().ref, selected[#selected].preset, #S:list_records('permission/preset').data
        "#,
        )
        .await;
        assert_eq!(state, ("faux/echo".into(), "read-only".into(), 2));
        assert_eq!(h.prompt("next", json!({})).await.unwrap()["text"], "explicit CLI model");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn first_input_persists_initial_model_and_effort_on_root_for_undo_and_branch_replay() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "model": "fast",
            "model_aliases": { "fast": "faux/echo" },
            "model_alias_params": { "fast": { "effort": "low" } },
            "plugins": {
                "provider-faux": { "script_json": json!([{"text":"first answer"}, {"text":"second answer"}]).to_string() },
                "permissions": { "enabled": false }
            }
        });
        let h = env.host().config(cfg).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "CM", h.service_table("commands").unwrap());
        assert_eq!(eval::<i64>(&h, "return #S:entries()").await, 0);

        assert_eq!(h.prompt("first", json!({})).await.unwrap()["text"], "first answer");

        let (session_path, model_at_is_false, model_at_str, model_ref, model_effort, first_user_id): (String, bool, String, String, String, String) = eval(
            &h,
            r#"
            local model_entry, user_id
            for _, e in ipairs(S:entries()) do
                if e.type == 'model' and not model_entry then
                    model_entry = e
                elseif e.type == 'message' and e.message.role == 'user' and not user_id then
                    user_id = e.entry_id
                end
            end
            return S:info().path, model_entry.at_entry_id == false, tostring(model_entry.at_entry_id), model_entry.model, model_entry.effort, user_id
        "#,
        )
        .await;
        assert!(model_at_is_false);
        assert_ne!(model_at_str, first_user_id);
        assert_eq!(model_ref, "faux/echo");
        assert_eq!(model_effort, "medium");

        let disk_lines: Vec<serde_json::Value> = std::fs::read_to_string(&session_path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert!(
            disk_lines.iter().any(|e| e["type"] == "model" && e["model"] == "faux/echo" && e["effort"] == "medium" && e["at_entry_id"] == false),
            "expected root model entry on disk: {disk_lines:?}"
        );

        exec(&h, "L:set_effort('high'); L:set_model('faux/echo2')").await;
        assert_eq!(h.prompt("second", json!({})).await.unwrap()["text"], "second answer");
        let second_leaf: String = eval(&h, "return S:info().leaf_entry_id").await;

        exec(&h, "CM:run('/undo 2')").await;
        wait_until(WAIT_SECS, "undo all back to root", || {
            let h = h.clone();
            async move { eval::<Option<String>>(&h, "return ED").await.as_deref() == Some("first") }
        })
        .await;
        let root_state: (Option<String>, String, String, i64) = eval(
            &h,
            "return S:info().leaf_entry_id, L:current().ref, L:raw_effort(), #A:list_messages().data",
        )
        .await;
        assert_eq!(root_state, (None, "faux/echo".into(), "medium".into(), 0));

        exec(&h, &format!("S:set_leaf('{second_leaf}')")).await;
        let branch_model: String = eval(&h, "return L:current().ref").await;
        assert_eq!(branch_model, "faux/echo2");

        exec(&h, "S:set_leaf()").await;
        let replay_root: (String, String) = eval(&h, "return L:current().ref, L:raw_effort()").await;
        assert_eq!(replay_root, ("faux/echo".into(), "medium".into()));
        h.shutdown().await;

        // Reload in a fresh host with a different default model and verify root replay from disk.
        let mut restart = env.host().config(json!({
            "plugins": {
                "provider-faux": { "script_json": "[]" },
                "permissions": { "enabled": false }
            }
        }));
        restart.opts.config.as_object_mut().unwrap().remove("model");
        let h2 = restart.build().await;
        expose(&h2);
        global(&h2, "L", h2.service_table("llm").unwrap());
        global(&h2, "P", session_path);
        exec(&h2, "S:load({ path = P }); S:set_leaf()").await;
        let restarted_root: (String, String) = eval(&h2, "return L:current().ref, L:raw_effort()").await;
        assert_eq!(restarted_root, ("faux/echo".into(), "medium".into()));
        h2.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn cancelled_tool_call_persists_fork_synthesizes_orphans_tree_does_not_and_resume_cwd_current() {
    local(async {
        // 1. Cancelled tool call is recorded in the session journal on disk.
        let env = TestEnv::new().await;
        let started = env.cwd.join("cancel-started");
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": format!("touch {:?}; sleep 30", started) } }] },
            { "text": "done" }
        ]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        exec(&h, r#"mare.task.spawn(function() A:prompt("cancel me", {}) end)"#).await;
        wait_until(WAIT_SECS, "bash call to start before cancel", || {
            let p = started.clone();
            async move { p.exists() }
        })
        .await;
        exec(&h, "A:cancel()").await;
        wait_until(WAIT_SECS, "cancelled turn to settle", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return not A:activity().busy").await }
        })
        .await;
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("\"role\":\"tool_result\"") && raw.contains("[command aborted]"),
            "cancelled tool call result must be persisted in journal:\n{raw}"
        );
        drop(env);

        // 2. Fork synthesizes orphan results in the fork while inspecting /tree does not
        //    mutate the source session file.
        let env = TestEnv::new().await;
        let call = |id: &str, name: &str| json!({ "type": "tool_call", "id": id, "name": name, "arguments": {} });
        let orphan_path = env.write_session(
            "2026-01-01T00-00-00_orphan.jsonl",
            &[
                msg("m1", None, "user", "first"),
                msg("m2", Some("m1"), "assistant", "ok"),
                run_end("m2"),
                msg("m3", Some("m2"), "user", "second"),
                json!({ "type": "message", "entry_id": "m4", "parent_entry_id": "m3",
                        "message": { "role": "assistant", "content": [call("c_orp", "bash")] } }),
                json!({ "type": "tool_state", "tool_call_id": "c_orp", "name": "bash", "state": "started",
                        "side_effect": "external" }),
            ],
        );
        let before_bytes = std::fs::read(&orphan_path).unwrap();
        let h = env.host().build().await;
        expose(&h);
        global(&h, "OP", orphan_path.to_string_lossy().to_string());
        // Reading entries (as /tree does) without loading writable leaves source untouched.
        let entry_count: usize = eval(&h, "return #mare.store.read(OP)").await;
        assert_eq!(entry_count, 7);
        assert_eq!(std::fs::read(&orphan_path).unwrap(), before_bytes, "/tree read must not write orphan results");
        // Forking/snapshotting from the source synthesizes the orphan result in the forked session.
        let (fork_path, fork_has_orphan): (String, bool) = eval(
            &h,
            r#"
            local src_entries = mare.store.read(OP)
            local sid = src_entries[1].session_id
            mare.store.index_append({ session_id = sid, path = OP, cwd = mare.cwd })
            local r = S:snapshot({ session_id = sid })
            local entries = mare.store.read(r.path)
            local found = false
            for _, e in ipairs(entries) do
                if e.type == "message" and e.message and e.message.role == "tool_result"
                   and e.message.tool_call_id == "c_orp" then
                    found = tostring(e.message.content[1].text or ""):find("outcome_unknown:", 1, true) ~= nil
                end
            end
            return r.path, found
            "#,
        )
        .await;
        assert!(fork_has_orphan, "fork must synthesize orphan tool_result in {fork_path}");
        assert_eq!(std::fs::read(&orphan_path).unwrap(), before_bytes, "fork must not modify source file");
        h.shutdown().await;
        drop(env);

        // 3. `resume_cwd = "current"` keeps shell in the launch directory rather than the
        //    session's recorded directory.
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session(
            "resume-cwd-current.jsonl",
            &[
                msg("m1", None, "user", "hi"),
                msg("m2", Some("m1"), "assistant", "there"),
                run_end("m2"),
            ],
        );
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, body).unwrap();
        let h = env
            .host()
            .faux(json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": "pwd -P" } }] }, { "text": "ok" }]))
            .config(json!({ "plugins": { "session": { "resume_cwd": "current" } } }))
            .build()
            .await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "S:load({ path = TARGET })").await;
        h.prompt("check cwd", json!({})).await.unwrap();
        let out: String = eval(
            &h,
            r#"for _, m in ipairs(A:list_messages().data) do
                   if m.role == "tool_result" then return tostring(m.details and m.details.output or "") end
               end
               return "" "#,
        )
        .await;
        assert_eq!(
            dunce::canonicalize(out.trim()).unwrap(),
            dunce::canonicalize(&env.cwd).unwrap(),
            "resume_cwd=current must keep shell in launch cwd"
        );
        h.shutdown().await;
    })
    .await
}
