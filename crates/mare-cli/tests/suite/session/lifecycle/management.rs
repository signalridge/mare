//! Session delete, archive, rename, snapshot journal, and provider/disposal notifications.

use mlua::LuaSerdeExt;

use super::*;

#[tokio::test]
async fn deleting_a_session_takes_its_lock_and_writes_a_tombstone() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["changes"]).faux(json!([{ "text": "a" }, { "text": "b" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        h.prompt("keep me", json!({})).await.unwrap();
        let (keep_path, keep_id): (String, String) = eval(&h, "return S:info().path, S:info().session_id").await;
        exec(&h, r#"CM:run("/new")"#).await;
        h.prompt("delete me", json!({})).await.unwrap();
        let (path, id): (String, String) = eval(&h, "return S:info().path, S:info().session_id").await;
        // things that belong to it: attachments, the snapshot journal, a sub-agent's session
        let base = path.trim_end_matches(".jsonl").to_string();
        std::fs::create_dir_all(format!("{base}.attachments")).unwrap();
        std::fs::create_dir_all(format!("{path}.snapshots")).unwrap();
        let run = std::path::Path::new(&path).parent().unwrap().join("agents/r1");
        std::fs::create_dir_all(&run).unwrap();
        std::fs::write(
            run.join("000-a.jsonl"),
            format!("{{\"type\":\"session\",\"version\":3,\"session_id\":\"c\",\"parent_session_id\":\"{id}\"}}\n"),
        )
        .unwrap();
        // the kept session is open in "another process": the picker's delete is refused
        exec(&h, r#"CM:run("/new")"#).await;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(format!("{keep_path}.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        global(&h, "KEEP", keep_id.clone());
        exec(
            &h,
            r#"local calls = 0
               mare.ui.select = function(o)
                 calls = calls + 1
                 if calls == 2 and o.title == "Delete session?" then return 2 end
                 if calls > 1 then return nil end
                 for i, it in ipairs(o.tabs[1].items) do
                   if (it.label or ""):find("keep me", 1, true) then return i, 1, "ctrl+d", "" end
                 end
               end
               CM:run("/resume")"#,
        )
        .await;
        wait_until(WAIT_SECS, "the refusal", || {
            let h = h.clone();
            async move { notices(&h).await.contains("not deleted") }
        })
        .await;
        assert!(std::path::Path::new(&keep_path).exists(), "a locked session stays");
        drop(lock);
        // /delete of the current session: everything goes, and its index row
        global(&h, "P", path.clone());
        exec(&h, "S:load({ path = P })").await;
        exec(&h, r#"CM:run("/delete")"#).await;
        wait_until(WAIT_SECS, "the delete", || {
            let p = path.clone();
            async move { !std::path::Path::new(&p).exists() }
        })
        .await;
        wait_until(WAIT_SECS, "the row removed", || {
            let id = id.clone();
            async move { index_rows().iter().all(|l| l["session_id"] != id.as_str()) }
        })
        .await;
        for gone in [format!("{base}.attachments"), format!("{path}.snapshots"), format!("{path}.lock")] {
            assert!(!std::path::Path::new(&gone).exists(), "{gone} left behind");
        }
        assert!(!run.exists(), "the sub-agent's session went with it");
        // the other session's row (with its message count) is untouched
        let live = index_rows();
        let kept = live.iter().find(|e| e["session_id"] == keep_id.as_str()).expect("kept session still indexed");
        assert_eq!(kept["messages"], 2, "{kept}");
        h.shutdown().await;
    })
    .await
}

/// Archive, unarchive and rename are entries in the session file first (the index is derived
/// from it): /archive leaves the file where it is, Ctrl+A lists it and unarchives it, and a
/// rename of a session another process holds open is refused instead of editing only the index.
#[tokio::test]
async fn archive_unarchive_and_rename_go_through_the_session_file() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["changes"]).faux(json!([{ "text": "a" }, { "text": "b" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        h.prompt("archive me", json!({})).await.unwrap();
        let (apath, aid): (String, String) = eval(&h, "return S:info().path, S:info().session_id").await;
        exec(&h, r#"CM:run("/archive")"#).await;
        let row = |id: String| async move { index_rows().into_iter().find(|r| r["session_id"] == id.as_str()) };
        wait_until(WAIT_SECS, "the archived row", || {
            let aid = aid.clone();
            async move { row(aid).await.is_some_and(|r| r["archived"] == true) }
        })
        .await;
        let file = |p: &str| std::fs::read_to_string(p).unwrap();
        assert!(file(&apath).contains(r#""type":"archive""#), "the archive is an entry in the session");
        h.prompt("keep me", json!({})).await.unwrap();
        let (bpath, bid): (String, String) = eval(&h, "return S:info().path, S:info().session_id").await;
        exec(&h, r#"CM:run("/new")"#).await;
        // the picker: the archived session isn't listed; B is open in "another process" → its
        // rename is refused; then Ctrl+A lists the archive and Enter unarchives
        let lock =
            std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(format!("{bpath}.lock")).unwrap();
        lock.try_lock().unwrap();
        exec(
            &h,
            r#"SEEN, calls = {}, 0
               mare.ui.input = function() return "Renamed" end
               mare.ui.select = function(o)
                 calls = calls + 1
                 if o.title and o.title:find("Archived", 1, true) then
                   ARCHIVED = o.items[1] and o.items[1].label
                   return 1
                 end
                 local items = o.tabs and o.tabs[o.tab or 1].items or o.items
                 local labels = {}
                 for _, it in ipairs(items) do labels[#labels + 1] = it.label end
                 SEEN[#SEEN + 1] = table.concat(labels, "\n")
                 for i, it in ipairs(items) do
                   if (it.label or ""):find("keep me", 1, true) then
                     if calls == 1 then return i, 1, "ctrl+r", "" end
                     if calls == 2 then return i, 1, "ctrl+a", "" end
                   end
                 end
               end
               CM:run("/resume")"#,
        )
        .await;
        wait_until(WAIT_SECS, "the unarchive", || {
            let aid = aid.clone();
            async move { row(aid).await.is_some_and(|r| r["archived"] == false) }
        })
        .await;
        let seen: String = eval(&h, "return SEEN[1]").await;
        assert!(seen.contains("keep me") && !seen.contains("archive me"), "archived sessions aren't listed:\n{seen}");
        assert_eq!(eval::<String>(&h, "return ARCHIVED").await, "archive me");
        assert!(notices(&h).await.contains("open in another window"), "{}", notices(&h).await);
        assert!(!file(&bpath).contains("Renamed"), "nothing written to a session open elsewhere");
        assert!(row(bid.clone()).await.unwrap().get("title").is_none(), "nor to its row");
        assert!(file(&apath).contains(r#""archived":false"#));
        drop(lock);
        h.shutdown().await;
    })
    .await
}

/// Resuming an archived session by id or name is refused with the way back (Codex
/// `read_thread.rs` points to `codex unarchive`); nothing is loaded.
#[tokio::test]
async fn resuming_an_archived_session_by_id_is_refused_with_a_hint() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        write_lines(
            &dir.join("2026-01-01T00-00-00_a.jsonl"),
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "arch1111-x", "cwd": env.cwd }),
                msg("m1", None, "user", "old work"),
                json!({ "type": "name", "name": "oldwork" }),
                json!({ "type": "archive", "archived": true, "created_at_ms": 5 }),
            ],
        );
        let h = env.host().builtins(&["changes"]).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        let before: String = eval(&h, "return S:info().session_id").await;
        for arg in ["arch1111-x", "oldwork", "arch1"] {
            exec(&h, "N = {}").await;
            exec(&h, &format!(r#"CM:run("/resume {arg}")"#)).await;
            wait_until(WAIT_SECS, "the refusal", || {
                let h = h.clone();
                async move { notices(&h).await.contains("is archived") }
            })
            .await;
            let n = notices(&h).await;
            assert!(n.contains("unarchive it first") && n.contains("Ctrl+A"), "{arg}: {n}");
        }
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, before, "nothing was resumed");
        h.shutdown().await;
    })
    .await
}

/// SE bug 1: while a turn runs, nothing may move the session position — the turn would write
/// its messages under the moved leaf.
#[tokio::test]
async fn commands_that_move_the_session_refuse_while_a_turn_runs() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "one" }, { "text": "x".repeat(40_000), "delay_ms": 10 }]);
        let h = env.host().builtins(&["changes"]).faux(script).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        h.prompt("first", json!({})).await.unwrap();
        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("long one", json!({})).await });
        wait_until(WAIT_SECS, "the turn", || {
            let h = h.clone();
            async move { h.is_busy().await }
        })
        .await;
        let leaf: String = eval(&h, "return S:leaf()").await;
        for cmd in ["/tree", "/fork", "/clone", "/label here", "/resume", "/undo", "/retry"] {
            exec(&h, "N = {}").await;
            exec(&h, &format!(r#"pcall(CM.run, CM, "{cmd}")"#)).await;
            let h3 = h.clone();
            wait_until(WAIT_SECS, cmd, move || {
                let h = h3.clone();
                // `availability = "idle"` (commands) or the session's own check (/tree's actions)
                async move {
                    let n = notices(&h).await;
                    n.contains("interrupt it first") || n.contains("isn't available while a turn is running")
                }
            })
            .await;
        }
        let path: String = eval(&h, "return S:info().path").await;
        h.abort().await.unwrap();
        let _ = run.await;
        // nothing moved: the turn's messages hang under the leaf it started from
        global(&h, "LEAF", leaf);
        let on_path: bool =
            eval(&h, "for _, e in ipairs(S:path_to()) do if e.entry_id == LEAF then return true end end return false")
                .await;
        assert!(on_path, "the leaf moved during the turn");
        assert!(!std::fs::read_to_string(&path).unwrap().contains("\"label\":\"here\""));
        h.shutdown().await;
    })
    .await
}

/// While a running turn's prompt persistence is still in flight (`persist_inflight > 0`), `/tree`
/// must report that a turn is running rather than falsely claiming session writes are failing.
#[tokio::test]
async fn tree_command_while_turn_persistence_is_inflight_reports_busy_not_write_failure() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "one" }, { "text": "two" }]);
        let h = env.host().builtins(&["changes"]).faux(script).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        h.prompt("first", json!({})).await.unwrap();
        exec(
            &h,
            r#"
            PERSIST_WAITING, RELEASE_PERSIST = false, false
            local orig_persist = S.persist_message
            S.persist_message = function(self, e)
                if e.message and e.message.role == "user" and e.message.content[1].text == "in-flight-prompt" then
                    PERSIST_WAITING = true
                    while not RELEASE_PERSIST do mare.task.sleep(10) end
                end
                return orig_persist(self, e)
            end
            "#,
        )
        .await;
        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("in-flight-prompt", json!({})).await });
        wait_until(WAIT_SECS, "turn persistence in flight", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return PERSIST_WAITING").await }
        })
        .await;

        exec(&h, r#"N = {}; pcall(CM.run, CM, "/tree")"#).await;
        let note = notices(&h).await;
        exec(&h, "RELEASE_PERSIST = true").await;
        let _ = run.await;
        assert!(
            note.contains("interrupt it first"),
            "expected busy refusal for /tree while turn persistence is in flight, got: {note:?}"
        );
        assert!(
            !note.contains("writes are failing"),
            "in-flight persistence must not be reported as failing writes: {note:?}"
        );
        h.shutdown().await;
    })
    .await
}

/// A new session's snapshot journal sits next to its file from the first turn on (not in a
/// per-process directory that nothing cleans up).
#[tokio::test]
async fn a_new_sessions_snapshot_journal_is_kept_next_to_it() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "a.txt", "content": "A" } }] },
            { "text": "wrote it" }
        ]);
        let h = env.host().builtins(&["changes"]).faux(script).build().await;
        expose(&h);
        h.prompt("write a file", json!({})).await.unwrap();
        let path: String = eval(&h, "return S:info().path").await;
        h.shutdown().await;
        let idx =
            std::fs::read_to_string(format!("{path}.snapshots/index.jsonl")).expect("journal next to the session");
        assert!(idx.contains("a.txt"), "{idx}");
        let runs = env.home.path().join("state/snapshots");
        assert!(!runs.exists() || std::fs::read_dir(&runs).unwrap().next().is_none(), "no run-<pid> journal");
    })
    .await
}

#[tokio::test]
async fn disposed_session_cannot_publish_a_previously_prepared_load() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let target = env.write_session("dispose-target.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let body = std::fs::read_to_string(&target).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&target, &body).unwrap();
        let h = env.host().builtins(&["changes"]).build().await;
        expose(&h);
        global(&h, "TARGET", target.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            SELECT_WAITING = false
            mare.ui.connected = function() return true end
            mare.ui.select = function() SELECT_WAITING = true; mare.task.sleep(350); return 1 end
        "#,
        )
        .await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(&loader, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)")
                .await
        });
        wait_until(5, "cwd selection", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SELECT_WAITING").await }
        })
        .await;
        h.kernel.reload(Some("session")).await.unwrap();
        let new_session = h.service_table("session").unwrap();
        global(&h, "NEW_S", new_session);
        let (new_id, new_agent): (String, String) =
            eval(&h, "return NEW_S:info(nil).session_id, A:get().session_id").await;
        let (ok, err) = loading.await.unwrap();
        assert!(!ok && err.contains("disposed"), "{err}");
        let (after_id, after_agent): (String, String) =
            eval(&h, "return NEW_S:info(nil).session_id, A:get().session_id").await;
        assert_eq!((after_id, after_agent), (new_id, new_agent));
        assert_eq!(std::fs::read_to_string(target).unwrap(), body);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn deferred_notification_wakes_after_session_provider_is_replaced() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let target = env.write_session("wake-dispose.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let body = std::fs::read_to_string(&target).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&target, &body).unwrap();
        let h = env.host().builtins(&["changes"]).faux(json!([{ "text": "WAKE-AFTER-RELOAD" }])).build().await;
        expose(&h);
        global(&h, "TARGET", target.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            SELECT_WAITING = false
            SELECT_TX, SELECT_RX = mare.task.channel()
            mare.ui.connected = function() return true end
            mare.ui.select = function() SELECT_WAITING = true; SELECT_RX:recv(); return 1 end
        "#,
        )
        .await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<bool>(&loader, "local ok = pcall(S.load, S, { path = TARGET }); return ok").await
        });
        wait_until(5, "cwd selection", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SELECT_WAITING").await }
        })
        .await;
        exec(&h, "A:notify('WAKE-ON-RELOAD', {wake=true})").await;
        assert!(!eval::<bool>(&h, "return A:get().busy").await);
        h.kernel.reload(Some("session")).await.unwrap();
        wait_until(5, "wake on replacement", || {
            let h = h.clone();
            async move {
                eval::<String>(&h, "return mare.json.encode(A:list_messages().data)")
                    .await
                    .contains("WAKE-AFTER-RELOAD")
            }
        })
        .await;
        exec(&h, "SELECT_TX:send(true)").await;
        assert!(!loading.await.unwrap());
        h.shutdown().await;
    })
    .await
}

/// The session's slash commands belong to the `commands` follower: reloading `commands` withdraws
/// the previous registrations and their `turn/completed` listener instead of piling them up on the
/// session plugin until it unloads (each /reload of `commands` used to leave one more).
#[tokio::test]
async fn reloading_commands_does_not_pile_up_the_session_command_listeners() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(serde_json::json!([])).build().await;
        let before = h.kernel.listener_count("turn/completed");
        for _ in 0..3 {
            h.kernel.reload(Some("commands")).await.unwrap();
        }
        let after = h.kernel.listener_count("turn/completed");
        expose(&h);
        let new_registered: bool = eval(&h, "return C.cordis:get('commands'):get('new') ~= nil").await;
        h.shutdown().await;
        assert_eq!(after, before, "turn/completed listeners grew across 3 reloads of commands");
        assert!(new_registered, "the session commands are registered again after the reloads");
    })
    .await
}

/// In a single-runtime host, `ctx.session:create()` from Lua and host/wire `session.create`
/// must produce the same snapshot shape, share `request_id` idempotency, and update the bound
/// session identically.
#[tokio::test]
async fn lua_create_matches_wire_create_in_a_single_runtime_host() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);

        let (lua_id, lua_name, replay_id, conflict_code, active_id): (String, String, String, String, String) = eval(
            &h,
            r#"
            local first = S:create({ name = "lua-one", request_id = "req-single-1" })
            assert(type(first.session_id) == "string" and first.session_id ~= "", "missing session_id")
            assert(type(first.session_epoch) == "string" and first.session_epoch ~= "", "missing session_epoch")
            assert(type(first.cwd) == "string" and first.cwd ~= "", "missing cwd")
            local replay = S:create({ name = "lua-one", request_id = "req-single-1" })
            local ok, err = pcall(S.create, S, { name = "different", request_id = "req-single-1" })
            assert(not ok, "reused request_id with different params must fail")
            return first.session_id, tostring(first.name), replay.session_id,
                tostring(type(err) == "table" and err.code or err), S:info().session_id
            "#,
        )
        .await;
        assert_eq!(replay_id, lua_id, "request_id replay must return the original session");
        assert_eq!(lua_name, "lua-one");
        assert_eq!(active_id, lua_id, "single-runtime create must bind the created session");
        assert_eq!(conflict_code, "request_id_conflict");

        // Call via Host::call_service (the host/wire entry point) and verify Lua shares the receipt.
        let arg = h.lua().to_value(&json!({ "name": "wire-two", "request_id": "req-single-2" })).unwrap();
        let mut args = mlua::MultiValue::new();
        args.push_back(arg);
        let values = h.call_service("session", "create", args).await.unwrap();
        let wire_snap: Value = h.lua().from_value(values.into_iter().next().unwrap()).unwrap();
        let wire_id = wire_snap["session_id"].as_str().expect("wire session_id").to_owned();
        assert_ne!(wire_id, lua_id);
        assert_eq!(wire_snap["name"], json!("wire-two"));

        let (lua_replay_wire_id, bound_id): (String, String) = eval(
            &h,
            r#"
            local r = S:create({ name = "wire-two", request_id = "req-single-2" })
            return r.session_id, S:info().session_id
            "#,
        )
        .await;
        assert_eq!(lua_replay_wire_id, wire_id, "Lua must share request_id receipts with host/wire create");
        assert_eq!(bound_id, wire_id);
        h.shutdown().await;
    })
    .await
}

/// In a single-runtime host, `session.close` yields a fresh session when targeting the bound
/// session (by exact id, path, or unique >=4-char prefix) and enforces ambiguity, unknown-id,
/// stale-epoch, and busy rejections without disturbing the bound session.
#[tokio::test]
async fn closing_the_bound_session_yields_a_fresh_one() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "r1" },
                { "text": "r2" },
                { "text": format!("BUSY-{}", "slow ".repeat(400)), "delay_ms": 10 }
            ]))
            .build()
            .await;
        expose(&h);

        // 1. Unknown id and <4-char prefix on unpersisted bound session -> unknown_session;
        //    exact id -> yields a fresh session.
        let (unk_code, short_code, id1, id2): (String, String, String, String) = eval(
            &h,
            r#"
            local before = S:info().session_id
            local ok1, e1 = pcall(S.close, S, "zzzz-does-not-exist")
            assert(not ok1, "unknown id must fail")
            local ok2, e2 = pcall(S.close, S, before:sub(1, 3))
            assert(not ok2, "<4-char prefix of unpersisted active session must fail")
            assert(S:info().session_id == before, "failed close must keep bound session")
            local res = S:close(before)
            assert(type(res) == "table", "close must return an object")
            return tostring(type(e1) == "table" and e1.code or e1),
                tostring(type(e2) == "table" and e2.code or e2),
                before,
                S:info().session_id
            "#,
        )
        .await;
        assert_eq!(unk_code, "unknown_session");
        assert_eq!(short_code, "unknown_session");
        assert_ne!(id2, id1, "closing by exact id must yield a fresh session");

        // 2. Path -> fresh session
        h.prompt("persist for path close", json!({})).await.unwrap();
        let (before_path_id, after_path_id): (String, String) = eval(
            &h,
            r#"
            local info = S:info()
            assert(type(info.path) == "string" and info.path ~= "", "persisted path required")
            S:close(info.path)
            return info.session_id, S:info().session_id
            "#,
        )
        .await;
        assert_ne!(after_path_id, before_path_id, "closing by path must yield a fresh session");

        // 3. Unique >=4-char prefix -> fresh session (UUIDv7 shares leading timestamp hex across
        // rapid creates, so take a 16-char prefix that is unique while still shorter than full UUID).
        let (before_prefix_id, after_prefix_id): (String, String) = eval(
            &h,
            r#"
            local before = S:info().session_id
            S:close(before:sub(1, 16))
            return before, S:info().session_id
            "#,
        )
        .await;
        assert_ne!(after_prefix_id, before_prefix_id, "closing by unique >=4-char prefix must yield a fresh session");

        // 4. Stale expected_epoch -> stale_session; active session unchanged
        let (stale_code, stayed_id, expected_id): (String, String, String) = eval(
            &h,
            r#"
            local cur = S:info().session_id
            local ok, err = pcall(S.close, S, cur, { expected_epoch = "stale-epoch-value" })
            assert(not ok, "stale expected_epoch must fail")
            return tostring(type(err) == "table" and err.code or err), S:info().session_id, cur
            "#,
        )
        .await;
        assert_eq!(stale_code, "stale_session");
        assert_eq!(stayed_id, expected_id);

        // 6. Ambiguous >=4-char prefix -> invalid_arguments; active session unchanged
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        let p_a = dir.join("2026-01-01T00-00-01_ambig-a.jsonl");
        let p_b = dir.join("2026-01-01T00-00-02_ambig-b.jsonl");
        write_lines(
            &p_a,
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "ab12cd34-1111-4111-8111-111111111111", "cwd": env.cwd }),
                msg("m1", None, "user", "ambig a"),
            ],
        );
        write_lines(
            &p_b,
            &[
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "ab12cd34-2222-4222-8222-222222222222", "cwd": env.cwd }),
                msg("m1", None, "user", "ambig b"),
            ],
        );
        global(&h, "AMBIG_A", p_a.to_string_lossy().to_string());
        global(&h, "AMBIG_B", p_b.to_string_lossy().to_string());
        let (ambig_code, ambig_msg, after_ambig_id): (String, String, String) = eval(
            &h,
            r#"
            S:load({ path = AMBIG_B })
            S:load({ path = AMBIG_A })
            local ok, err = pcall(S.close, S, "ab12cd34")
            assert(not ok, "ambiguous prefix close must fail")
            return tostring(type(err) == "table" and err.code or err),
                tostring(type(err) == "table" and err.message or err),
                S:info().session_id
            "#,
        )
        .await;
        assert_eq!(ambig_code, "invalid_arguments", "{ambig_msg}");
        assert!(ambig_msg.contains("ambiguous"), "{ambig_msg}");
        assert_eq!(after_ambig_id, "ab12cd34-1111-4111-8111-111111111111");

        // 7. Busy turn active -> busy; active session unchanged
        h.prompt("warmup", json!({})).await.unwrap();
        exec(
            &h,
            r#"CLOSE_STREAM = false
            C.cordis:on("item/delta", function(e) if e.kind == "text" then CLOSE_STREAM = true end end)"#,
        )
        .await;
        let running = h.clone();
        let turn = tokio::task::spawn_local(async move { running.prompt("busy close test", json!({})).await.unwrap() });
        wait_until(5, "stream to start for busy close", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return CLOSE_STREAM").await }
        })
        .await;
        let (busy_code, after_busy_id): (String, String) = eval(
            &h,
            r#"
            local cur = S:info().session_id
            local ok, err = pcall(S.close, S, cur)
            assert(not ok)
            return tostring(type(err) == "table" and err.code or err), S:info().session_id
            "#,
        )
        .await;
        turn.await.unwrap();
        assert_eq!(busy_code, "busy");
        assert_eq!(after_busy_id, "ab12cd34-1111-4111-8111-111111111111");

        h.shutdown().await;
    })
    .await
}

/// The built-in `session` provider exposes Lua-only `session.switch` (`kind = "new" | "load" | "fork"`)
/// for single-runtime host orchestration and keeps it off the remote/wire export surface.
#[tokio::test]
async fn provider_session_switch_handles_new_load_and_fork_in_place() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "turn-1" }])).build().await;
        expose(&h);

        // 1. switch(new) with name and explicit id
        let (id1, name1, custom_id): (String, String, String) = eval(
            &h,
            r#"
            assert(type(S.switch) == "function", "session provider must implement session.switch")
            local s1 = S:switch({ kind = "new", name = "switched-new" })
            assert(s1.session_id == S:info().session_id, "switch(new) must bind new session")
            local s2 = S:switch({ kind = "new", new_session_id = "11111111-2222-4333-8444-555555555555", name = "custom-id" })
            assert(s2.session_id == S:info().session_id, "switch(new, id) must adopt requested id")
            return s1.session_id, tostring(s1.name), s2.session_id
            "#,
        )
        .await;
        assert_eq!(name1, "switched-new");
        assert_eq!(custom_id, "11111111-2222-4333-8444-555555555555");
        assert_ne!(id1, custom_id);

        // Persist one turn on custom_id, switch away, then switch(load) back and switch(fork).
        h.prompt("persist for switch test", json!({})).await.unwrap();
        let saved_path: String = eval(&h, "return S:info().path").await;
        global(&h, "SAVED_PATH", saved_path);
        let (reloaded_id, forked_id, forked_name): (String, String, String) = eval(
            &h,
            r#"
            S:switch({ kind = "new" })
            assert(S:info().session_id ~= "11111111-2222-4333-8444-555555555555")
            local loaded = S:switch({ kind = "load", path = SAVED_PATH })
            assert(loaded.session_id == S:info().session_id)
            local forked = S:switch({ kind = "fork", session_id = loaded.session_id, name = "switched-fork" })
            assert(forked.session_id == S:info().session_id)
            return loaded.session_id, forked.session_id, tostring(forked.name)
            "#,
        )
        .await;
        assert_eq!(reloaded_id, "11111111-2222-4333-8444-555555555555");
        assert_ne!(forked_id, reloaded_id);
        assert_eq!(forked_name, "switched-fork");

        // 2. session.switch is Lua-only (not exported to remote/JSON callers).
        let owner = h.kernel.host_owner().unwrap();
        let export_err = h
            .kernel
            .call_service_export(owner, None, "session", "switch", json!({ "kind": "new" }), Default::default())
            .await
            .expect_err("session.switch must remain expose = 'lua' only");
        assert_eq!(export_err.code(), "method_not_found", "{export_err}");

        h.shutdown().await;
    })
    .await
}
