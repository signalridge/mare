use super::*;

#[tokio::test]
async fn resume_restores_the_sessions_model_effort_and_queued_inputs() {
    local(async {
        let env = TestEnv::new().await;
        // no `model` in the host config: a config model would count as --model (which wins)
        let cfg = json!({ "plugins": { "provider-faux": { "script_json": json!([{ "text": "a" }, { "text": "b" }]).to_string() } } });
        let h = env.host().config(cfg.clone()).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("hi", json!({})).await.unwrap();
        exec(
            &h,
            r#"
            L:set_model("faux/echo2") L:set_model("faux/echo")
            L:set_effort("high")
            local held_tx, held_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get("policy"):voter("request/ready", function()
                if held_tx then
                    local tx = held_tx; held_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = "deny", reason = "gate" }
                end
            end)
            local held_turn = A:prompt("hold")
            held_rx:recv()
            A:prompt("queued while busy", { when_busy = "queue" })
            A:cancel({ turn_id = held_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(held_turn.turn_id)
            "#,
        )
        .await;
        h.shutdown().await;
        // the saved default moves on; the session keeps what it used
        let h = env.host().config(cfg.clone()).build().await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"L:set_model("faux/echo") L:set_effort("low") L:set_model("faux/echo2")"#).await;
        h.shutdown().await;

        let h = env.host().config(cfg).resume().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        let (model, effort, queued): (String, String, String) =
            eval(&h, "return L:current().ref, L:raw_effort(), (A:list_queue().data[1] or {}).text or ''").await;
        assert_eq!((model.as_str(), effort.as_str()), ("faux/echo", "high"));
        assert_eq!(queued, "queued while busy", "queued input restored after the restart");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn damaged_session_is_not_resumed_writable_and_left_untouched() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "2026-01-01T00-00-00_damaged.jsonl",
            &[msg("m1", None, "user", "hello"), msg("m2", Some("m1"), "assistant", "hi")],
        );
        // a bad line in the middle (not a torn tail): never skipped, migrated or rewritten
        let mut body = std::fs::read_to_string(&path).unwrap();
        let second = body.find('\n').unwrap() + 1;
        body.insert_str(second, "{\"type\":\"message\",\"id\":\"half\n");
        std::fs::write(&path, &body).unwrap();
        let h = env.host().build().await;
        expose(&h);
        global(&h, "PATH", path.to_string_lossy().to_string());
        let (ok, err): (bool, String) =
            eval(&h, "local ok, e = pcall(S.load, S, { path = PATH }); return ok, tostring(e)").await;
        assert!(!ok);
        assert!(err.contains("damaged") && err.contains("read-only") && err.contains("back up or repair"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body, "file bytes unchanged");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_locked_by_another_holder_is_not_resumed_writable() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("2026-01-01T00-00-00_locked.jsonl", &[msg("m1", None, "user", "hello")]);
        let body = std::fs::read_to_string(&path).unwrap();
        // stands in for another mare process holding the session (the lock is <session>.lock)
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
        let (ok, err): (bool, String) =
            eval(&h, "local ok, e = pcall(S.load, S, { path = PATH }); return ok, tostring(e)").await;
        assert!(!ok && err.contains("another mare process"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        drop(lock);
        h.shutdown().await;
    })
    .await
}

/// A read-only view (the session is locked by another process) is a calm state: one notice
/// when it opens, then turns run in memory with no write-failed error per turn; a tool with
/// side effects is refused with a result explaining that a completed snapshot can be forked, read-only tools still run, and
/// nothing reaches the file or the index.
#[tokio::test]
async fn a_read_only_view_runs_turns_in_memory_and_refuses_side_effects() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("2026-01-01T00-00-00_locked.jsonl", &[msg("m1", None, "user", "hello")]);
        let before = std::fs::read(&path).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let script = json!([
            { "tool_calls": [
                { "name": "write", "arguments": { "path": "a.txt", "content": "A" } },
                { "name": "ls", "arguments": { "path": "." } }
            ] },
            { "text": "done" },
            { "text": "second answer" }
        ]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        fake_ui(&h).await;
        exec(&h, r#"NOTICES = {} C.cordis:on("host/notice", function(ev) NOTICES[#NOTICES + 1] = ev.text end)"#).await;
        global(&h, "PATH", path.to_string_lossy().to_string());
        let (ok, err): (bool, String) = eval(
            &h,
            r#"mare.ui.select = function() return 2 end
               local ok, e = pcall(S.load, S, { path = PATH }) return ok, tostring(e)"#,
        )
        .await;
        assert!(ok, "{err}");
        let n = notices(&h).await;
        assert_eq!(n.matches("read-only view — nothing is saved").count(), 1, "{n}");
        assert!(
            n.contains("fork a completed snapshot")
                && n.contains("back up or repair")
                && !n.contains("close any other writer")
                && !n.contains("/fork"),
            "{n}"
        );
        let rows_before: String =
            eval(&h, r#"return mare.json.encode((mare.store.index_read({ scope = "all", archived = "any" })))"#).await;
        for text in ["go", "and again"] {
            let res = h.prompt(text, json!({})).await.unwrap();
            assert!(res["error"].is_null(), "{res}");
        }
        assert!(!env.cwd.join("a.txt").exists(), "the side effect never ran");
        let results: Vec<(String, String)> = serde_json::from_str(
            &eval::<String>(
                &h,
                r#"local out = {}
                   for _, m in ipairs(A:list_messages().data) do
                     if m.role == "tool_result" then
                       out[#out + 1] = { m.tool_name, m.content[1] and m.content[1].text or "" }
                     end
                   end
                   return mare.json.encode(out)"#,
            )
            .await,
        )
        .unwrap();
        let write = &results.iter().find(|(name, _)| name == "write").unwrap().1;
        assert!(
            write.contains("read-only view")
                && write.contains("fork a completed snapshot")
                && write.contains("back up or repair")
                && !write.contains("/fork"),
            "{write}"
        );
        let ls = &results.iter().find(|(name, _)| name == "ls").unwrap().1;
        assert!(!ls.contains("blocked"), "read-only tools still run: {ls}");
        let quiet: String = eval(&h, "return table.concat(NOTICES, '\\n')").await;
        assert!(!quiet.contains("write failed") && !quiet.contains("can't be saved"), "{quiet}");
        let n = notices(&h).await;
        assert!(!n.contains("write failed") && !n.contains("can't be saved"), "{n}");
        assert!(eval::<bool>(&h, "return S:info().readonly == true").await);
        assert_eq!(std::fs::read(&path).unwrap(), before, "nothing reached the file");
        let rows_after: String =
            eval(&h, r#"return mare.json.encode((mare.store.index_read({ scope = "all", archived = "any" })))"#).await;
        assert_eq!(rows_after, rows_before, "nor the index");
        drop(lock);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_session_is_open_at_most_once_per_process() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        h.prompt("hello", json!({})).await.unwrap();
        expose(&h);
        let (second, reload, writes): (String, String, bool) = eval(
            &h,
            r#"local path = S:info().path
               -- a second open of the session this process holds is refused (in-process registry)
               local ok, e = pcall(mare.store.open, path)
               local second = ok and "opened" or tostring(e)
               -- resuming the session that is open right now releases it first, then reopens
               local rok, re = pcall(S.load, S, { path = path })
               return second, rok and "ok" or tostring(re), S:info().readonly ~= true"#,
        )
        .await;
        assert!(second.contains("already open in this process"), "{second}");
        assert_eq!(reload, "ok");
        assert!(writes, "the session is writable again after the reload");
        h.prompt("again", json!({})).await.unwrap();
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resuming_a_missing_file_or_a_stale_index_line_creates_nothing() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        let gone = env.home.path().join("state/sessions/nowhere/gone.jsonl");
        global(&h, "GONE", gone.to_string_lossy().to_string());
        let (ok, err): (bool, String) =
            eval(&h, "local ok, e = pcall(S.load, S, { path = GONE }) return ok, tostring(e)").await;
        assert!(!ok && err.contains("not found"), "{err}");
        assert!(!gone.exists() && !gone.with_extension("jsonl.lock").exists());
        // an index row whose file is gone (deleted behind the index's back): never listed,
        // never resumed
        let dead = env.home.path().join("state/sessions/nowhere/dead.jsonl");
        std::fs::create_dir_all(dead.parent().unwrap()).unwrap();
        std::fs::write(
            &dead,
            format!(
                "{}\n{}\n",
                json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"dead0000-1111","cwd":"/w"}),
                msg("m1", None, "user", "old")
            ),
        )
        .unwrap();
        global(&h, "DEAD", dead.to_string_lossy().to_string());
        exec(&h, "mare.store.index_append({ path = DEAD })").await;
        assert!(index_rows().iter().any(|r| r["session_id"] == "dead0000-1111"));
        std::fs::remove_file(&dead).unwrap();
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/resume dead0000")"#).await;
        wait_until(WAIT_SECS, "the notice", || {
            let h = h.clone();
            async move { !notices(&h).await.is_empty() }
        })
        .await;
        assert!(notices(&h).await.contains("no session matches"), "{}", notices(&h).await);
        assert!(!gone.exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_matches_an_id_prefix_or_a_name_and_refuses_ambiguity() {
    local(async {
        let env = TestEnv::new().await;
        let dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&env.cwd));
        std::fs::create_dir_all(&dir).unwrap();
        for (file, id, name) in [
            ("2026-01-01T00-00-00_a.jsonl", "abc11111-x", "alpha"),
            ("2026-01-02T00-00-00_b.jsonl", "abc22222-y", "beta"),
        ] {
            let lines = [
                json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": id, "cwd": env.cwd }),
                msg(&format!("{id}m"), None, "user", &format!("prompt of {name}")),
                json!({ "type": "name", "name": name }),
            ];
            std::fs::write(dir.join(file), lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        }
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "CM", h.service_table("commands").unwrap());
        exec(&h, r#"CM:run("/resume abc")"#).await;
        wait_until(WAIT_SECS, "the ambiguity", || {
            let h = h.clone();
            async move { notices(&h).await.contains("matches 2 sessions") }
        })
        .await;
        for (arg, want) in [("ALPHA", "abc11111-x"), ("abc2", "abc22222-y")] {
            exec(&h, &format!(r#"CM:run("/resume {arg}")"#)).await;
            let want = want.to_string();
            // The new id is published before the switch finishes (store_switch.lua `publish`
            // vs `finish_switch`); a /resume issued inside that window is refused ("another
            // switch is in progress; retry"). Wait for the switch itself to be over.
            wait_until(WAIT_SECS, "the resume", || {
                let (h, want) = (h.clone(), want.clone());
                async move { eval::<(String, bool)>(&h, "return S:info().session_id, S:switching()").await == (want, false) }
            })
            .await;
        }
        h.shutdown().await;
    })
    .await
}

/// The session's directory is gone: resuming headless continues in the launch directory (and
/// says so on stderr / in the log) instead of failing or switching silently.
#[tokio::test]
async fn resuming_a_session_whose_directory_is_gone_runs_in_the_current_one() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("2026-01-01T00-00-00_gd.jsonl", &[msg("m1", None, "user", "hello")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            "\"/definitely/not/here\"",
            1,
        );
        std::fs::write(&path, body).unwrap();
        let h = env.host().build().await;
        expose(&h);
        global(&h, "P", path.to_string_lossy().to_string());
        let cwd: String = eval(&h, "S:load({ path = P }) return T:cwd()").await;
        // dunce: `canonicalize` gives `\\?\C:\…` on Windows, `env.cwd` the plain form
        assert_eq!(dunce::canonicalize(&cwd).unwrap(), env.cwd);
        h.shutdown().await;
    })
    .await
}

/// SE bug 4: headless `--resume <unknown>` and `-c` with nothing to continue exit 1 instead of
/// quietly starting a new session.
#[test]
fn headless_resume_of_nothing_exits_1() {
    let home = tempfile::tempdir().unwrap();
    for args in [vec!["--resume", "nope"], vec!["-c"], vec!["--resume", "-1"]] {
        let out = mare_cmd()
            .args(["-p", "--model", "faux/echo"])
            .args(&args)
            .arg("hi")
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .stdin(std::process::Stdio::null())
            .run_output();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {err}");
        assert!(err.contains("session"), "{args:?}: {err}");
    }
    let sessions = home.path().join("state/sessions");
    let made: Vec<_> = walk(&sessions).into_iter().filter(|p| p.extension().is_some_and(|e| e == "jsonl")).collect();
    assert!(made.is_empty(), "no session was started: {made:?}");
}

fn walk(d: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![];
    for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

#[tokio::test]
async fn unsupported_session_is_rejected_without_switching_or_modifying_it() {
    local(async {
        for ui in [false, true] {
            let env = TestEnv::new().await;
            let path = env.write_session("2026-01-01T00-00-00_future.jsonl", &[msg("m1", None, "user", "hello")]);
            let original = std::fs::read_to_string(&path).unwrap().replacen(
                &format!("\"version\":{}", mare_store::CURRENT_VERSION),
                "\"version\":99",
                1,
            );
            assert!(original.contains("\"version\":99"));
            std::fs::write(&path, &original).unwrap();
            let mut backup = path.as_os_str().to_owned();
            backup.push(".v1.bak");
            std::fs::write(&backup, "keep backup").unwrap();
            let h = env.host().build().await;
            expose(&h);
            if ui {
                fake_ui(&h).await;
            }
            global(&h, "PATH", path.to_string_lossy().to_string());
            let (ok, err, before, after): (bool, String, String, String) = eval(
                &h,
                r#"
                local before = S:info().session_id
                local ok, e = pcall(S.load, S, { path = PATH })
                return ok, tostring(e), before, S:info().session_id
            "#,
            )
            .await;
            assert!(!ok && err.contains("unsupported session format version"), "{err}");
            assert_eq!(before, after);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), "keep backup");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn rebuilding_index_skips_and_reports_unsupported_session_version() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        let path = env.write_session("2026-01-01T00-00-00_future.jsonl", &[msg("m1", None, "user", "hello")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &format!("\"version\":{}", mare_store::CURRENT_VERSION),
            "\"version\":99",
            1,
        );
        assert!(body.contains("\"version\":99"));
        std::fs::write(&path, &body).unwrap();
        let (ok, warnings): (bool, String) = eval(
            &h,
            r#"
            local ok = pcall(mare.store.index_rebuild)
            local _, _, warnings = mare.store.index_read({scope = "all", kind = "any", archived = "any"})
            return ok, table.concat(warnings or {}, "\n")
        "#,
        )
        .await;
        assert!(ok && warnings.contains("future.jsonl"), "{warnings}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unsupported_session_warns_in_resume_and_can_be_deleted_by_path() {
    local(async {
        let env = TestEnv::new().await;
        let good = env.write_session("2026-01-01T00-00-00_good.jsonl", &[msg("m1", None, "user", "valid")]);

        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        // the unsupported session appears only now: the startup index reconcile (session/startup.lua
        // on app/ready, which Host::new emits before it returns) would otherwise give its
        // once-per-path warning to the real, unconnected UI before this test listens; the plain
        // listing below has to be the one that warns
        let bad = env.write_session("2026-01-02T00-00-00_future.jsonl", &[msg("m2", None, "user", "future")]);
        let body = std::fs::read_to_string(&bad).unwrap().replacen(
            &format!("\"version\":{}", mare_store::CURRENT_VERSION),
            "\"version\":99",
            1,
        );
        std::fs::write(&bad, &body).unwrap();
        global(&h, "CM", h.service_table("commands").unwrap());
        global(&h, "BAD", bad.to_string_lossy().to_string());
        let listed: String = eval(&h, r#"return mare.json.encode(S:list({}))"#).await;
        assert!(listed.contains("good.jsonl") && !listed.contains("future.jsonl"), "{listed}");
        let listed_notices = notices(&h).await;
        assert!(listed_notices.contains("future.jsonl"), "the plain list must warn");
        assert_eq!(listed_notices.matches("future.jsonl").count(), 1, "{listed_notices}");
        eval::<String>(&h, "return mare.json.encode(S:list({}))").await;
        let repeated_list_notices = notices(&h).await;
        assert_eq!(repeated_list_notices.matches("future.jsonl").count(), 1, "{repeated_list_notices}");
        // pickers open on the index as it is; it was reconciled at startup, before the file existed
        exec(&h, "mare.store.index_reconcile()").await;
        exec(
            &h,
            r#"RESUME_OPEN = false
               RESUME_CALLS = 0
               mare.ui.select = function(o)
                 local items = o.items or (o.tabs and o.tabs[o.tab or 1] and o.tabs[o.tab or 1].items) or {}
                 local labels = {}
                 local unsupported
                 for i, item in ipairs(items) do
                   if type(item) == 'table' then
                     labels[#labels + 1] = item.label .. ' ' .. (item.detail or '')
                     if item.disabled then unsupported = i end
                   else
                     labels[#labels + 1] = item
                   end
                 end
                 RESUME_ITEMS = table.concat(labels, '\n')
                 RESUME_DISABLED = unsupported ~= nil
                 RESUME_CALLS = RESUME_CALLS + 1
                 RESUME_OPEN = true
                 -- Even a host that returns a disabled row cannot resume it; it reopens the picker.
                 return RESUME_CALLS == 1 and unsupported or nil
               end
               CM:run("/resume")"#,
        )
        .await;
        wait_until(WAIT_SECS, "the /resume picker", || {
            let h = h.clone();
            async move { eval::<i64>(&h, "return RESUME_CALLS").await >= 2 }
        })
        .await;
        let (items, message, disabled, calls): (String, String, bool, i64) = eval(
            &h,
            "return RESUME_ITEMS, table.concat(N, '\\n'), RESUME_DISABLED, RESUME_CALLS",
        )
        .await;
        let bad_path = bad.to_string_lossy();
        let normalized = |path: &str| path.replace('\\', "/");
        assert!(items.contains("valid"), "the supported session remains listed: {items}");
        assert!(
            normalized(&items).contains(&normalized(bad_path.as_ref())),
            "the full unsupported path stays visible: {items} (expected {bad_path})"
        );
        assert!(items.contains("Version unsupported") && items.contains("/delete <path>"), "{items}");
        assert!(disabled, "the picker marked the unsupported path unselectable");
        assert!(calls >= 2, "returning the disabled item must reopen the picker: {calls}");
        assert!(message.contains("unsupported version") && message.contains("/delete <path>"), "{message}");
        assert_eq!(message.matches("future.jsonl").count(), 1, "the warning stays deduplicated in this Host: {message}");
        assert!(good.exists() && bad.exists(), "listing must leave session files intact");
        exec(&h, r#"CONFIRM_SEEN = false; mare.ui.confirm = function() CONFIRM_SEEN = true; return false end; CM:run("/delete " .. BAD)"#).await;
        wait_until(WAIT_SECS, "the delete confirmation", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return CONFIRM_SEEN").await }
        }).await;
        assert!(bad.exists(), "declining deletion must preserve the file");
        exec(&h, "mare.ui.confirm = function() return true end").await;
        exec(&h, r#"CM:run("/delete " .. BAD)"#).await;
        wait_until(WAIT_SECS, "delete unsupported session", || {
            let bad = bad.clone();
            async move { !bad.exists() }
        })
        .await;
        assert!(good.exists(), "the supported session remains");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn continue_skips_a_newer_unsupported_session() {
    local(async {
        let env = TestEnv::new().await;
        let good = env.write_session("2026-01-01T00-00-00_good.jsonl", &[msg("m1", None, "user", "valid")]);
        let bad = env.write_session("2026-01-02T00-00-00_future.jsonl", &[msg("m2", None, "user", "future")]);
        let body = std::fs::read_to_string(&bad).unwrap().replacen(
            &format!("\"version\":{}", mare_store::CURRENT_VERSION),
            "\"version\":99",
            1,
        );
        std::fs::write(&bad, &body).unwrap();
        let h = env.host().resume().build().await;
        expose(&h);
        let resumed: String = eval(&h, "return S:info().path").await;
        assert_eq!(std::path::Path::new(&resumed), good);
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), body);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn partial_compacted_resume_preserves_name_revision_and_pre_compaction_known_ids() {
    local(async {
        let env = TestEnv::new().await;
        // Two renames + an explicit custom record entry_id precede the compaction boundary;
        // no `name` entry exists in the tail so partial load only sees `c1`..`m3`.
        let path = env.write_session(
            "2026-01-03T00-00-00_partial-rev-ids.jsonl",
            &[
                msg("m1", None, "user", "early prompt"),
                json!({ "type": "name", "name": "first-title" }),
                json!({ "type": "name", "name": "second-title" }),
                json!({
                    "type": "custom",
                    "entry_id": "pre-compact-rec-1",
                    "kind": "probe/id",
                    "at_entry_id": "m1",
                    "data": { "v": 1 }
                }),
                msg("m2", Some("m1"), "assistant", "early answer"),
                json!({
                    "type": "compaction",
                    "entry_id": "c1",
                    "parent_entry_id": "m2",
                    "first_kept_entry_id": "m2",
                    "summary": "compacted"
                }),
                msg("m3", Some("c1"), "user", "after compaction"),
            ],
        );

        let h = env.host().resume().build().await;
        expose(&h);
        let report_json: String = eval(
            &h,
            r#"
            local info = S:info()
            local snap = S:get(info.session_id)
            local ok_stale, err_stale = pcall(S.update, S, "stale-overwrite", { expected_revision = 0 })
            local stale_code = type(err_stale) == "table" and err_stale.code or tostring(err_stale)
            local ok_stale1, err_stale1 = pcall(S.update, S, "stale-overwrite-1", { expected_revision = 1 })
            local stale1_code = type(err_stale1) == "table" and err_stale1.code or tostring(err_stale1)
            local ok_dup, err_dup = pcall(S.add_record, S, "probe/id", { v = 2 }, { entry_id = "pre-compact-rec-1" })
            local ok_fresh, updated = pcall(S.update, S, "third-title", { expected_revision = 2 })
            return mare.json.encode({
                name_before = snap.name,
                rev_before = snap.name_revision,
                ok_stale = ok_stale,
                stale_code = stale_code,
                ok_stale1 = ok_stale1,
                stale1_code = stale1_code,
                ok_dup = ok_dup,
                err_dup = tostring(err_dup),
                ok_fresh = ok_fresh,
                name_after = ok_fresh and updated.name or nil,
                rev_after = ok_fresh and updated.name_revision or nil,
                loaded_entries = #(S:entries()),
            })
            "#,
        )
        .await;
        let report: serde_json::Value = serde_json::from_str(&report_json).unwrap();
        assert_eq!(report["name_before"], "second-title", "{report}");
        assert_eq!(report["rev_before"], 2, "partial resume must preserve full name_revision: {report}");
        assert_eq!(report["ok_stale"], false, "expected_revision=0 must be rejected: {report}");
        assert_eq!(report["stale_code"], "session.revision_conflict", "{report}");
        assert_eq!(report["ok_stale1"], false, "expected_revision=1 must be rejected: {report}");
        assert_eq!(report["stale1_code"], "session.revision_conflict", "{report}");
        assert_eq!(report["ok_dup"], false, "replaying pre-compaction entry_id must be rejected: {report}");
        assert!(
            report["err_dup"].as_str().unwrap_or("").contains("duplicate entry id"),
            "expected duplicate entry id error: {report}"
        );
        assert_eq!(report["ok_fresh"], true, "expected_revision=2 must succeed: {report}");
        assert_eq!(report["name_after"], "third-title", "{report}");
        assert_eq!(report["rev_after"], 3, "{report}");
        let disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            disk.matches("\"pre-compact-rec-1\"").count(),
            1,
            "duplicate pre-compaction entry_id must not be appended to JSONL"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn partial_compacted_resume_preserves_file_order_for_branch_records_anchored_before_compaction() {
    local(async {
        let env = TestEnv::new().await;
        // Record 1 is written on the post-compaction leaf `m3`; then the user switches to the
        // pre-compaction ancestor `m1`, writes record 2 anchored on `m1`, and switches back to `m3`.
        // Both records belong to `m3`'s branch and must appear in file order `[1, 2]` after resume.
        let _path = env.write_session(
            "2026-01-04T00-00-00_partial-branch-order.jsonl",
            &[
                msg("m1", None, "user", "root prompt"),
                msg("m2", Some("m1"), "assistant", "root answer"),
                json!({
                    "type": "compaction",
                    "entry_id": "c1",
                    "parent_entry_id": "m2",
                    "first_kept_entry_id": "m2",
                    "summary": "compacted"
                }),
                msg("m3", Some("c1"), "user", "after compaction"),
                json!({
                    "type": "custom",
                    "entry_id": "rec-1",
                    "kind": "probe/order",
                    "at_entry_id": "m3",
                    "data": { "seq": 1 }
                }),
                json!({ "type": "leaf", "target_entry_id": "m1" }),
                json!({
                    "type": "custom",
                    "entry_id": "rec-2",
                    "kind": "probe/order",
                    "at_entry_id": "m1",
                    "data": { "seq": 2 }
                }),
                json!({ "type": "leaf", "target_entry_id": "m3" }),
            ],
        );

        let h = env.host().resume().build().await;
        expose(&h);
        let seqs_json: String = eval(
            &h,
            r#"
            local rows = S:list_records("probe/order", { branch = "current" }).data
            local seqs = {}
            for _, row in ipairs(rows or {}) do
                seqs[#seqs + 1] = row.seq
            end
            return mare.json.encode(seqs)
            "#,
        )
        .await;
        let seqs: serde_json::Value = serde_json::from_str(&seqs_json).unwrap();
        assert_eq!(seqs, json!([1, 2]), "branch=current records must preserve file order across compaction: {seqs}");
        h.shutdown().await;
    })
    .await
}
