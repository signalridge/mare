//! Fork cancellation, partial history expansion, and readonly/locked source snapshots.

use super::*;

#[tokio::test]
async fn cancelled_fork_during_attachment_preparation_keeps_old_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "SEED" }, { "text": "NEXT" }])).build().await;
        expose(&h);
        let img = json!([{ "type": "image", "mime": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGNgYGAAAAAEAAH2FzhVAAAAAElFTkSuQmCC" }]);
        h.prompt("IMAGE", json!({"images":img})).await.unwrap();
        let (id, path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        let dir = std::path::Path::new(&path).parent().unwrap();
        let count = || {
            std::fs::read_dir(dir)
                .unwrap()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
                .count()
        };
        let before = count();
        let (finished, entered, switching, after): (bool, bool, bool, String) = eval(
            &h,
            r#"
            WRITE_WAITING = false
            local entered_tx, entered_rx = mare.task.channel()
            local read_tx, read_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local blocked = mare.task.token()
            local read = mare.store.read_fork_snapshot
            local reserve = mare.store.reserve_staging_attachments
            -- Hold an earlier phase behind a barrier: cancellation must wait for _write.
            mare.store.read_fork_snapshot = function(...)
                assert(read_tx:send(true))
                assert(release_rx:recv())
                return read(...)
            end
            mare.store.reserve_staging_attachments = function(...)
                local stage = reserve(...)
                return { write = function(_, ...)
                             WRITE_WAITING = true
                             assert(entered_tx:send(true))
                             blocked:wait()
                             return stage:write(...)
                         end,
                         close = function() return stage:close() end }
            end
            local winner = mare.task.select(
                function() S:fork({ session_id = S:info().session_id }) end,
                function()
                    assert(read_rx:recv())
                    assert(not WRITE_WAITING and S:switching())
                    assert(entered_rx:try_recv() == nil)
                    assert(release_tx:send(true))
                    assert(entered_rx:recv())
                    return false
                end
            )
            -- select aborts and joins the fork before we inspect its lease and artifacts.
            local finished = winner == 1
            mare.store.read_fork_snapshot = read
            mare.store.reserve_staging_attachments = reserve
            return finished, WRITE_WAITING, S:switching(), S:info().session_id
        "#,
        )
        .await;
        assert!(!finished && entered && !switching, "finished={finished}, entered={entered}, switching={switching}");
        assert_eq!(after, id);
        assert_eq!(count(), before, "no fork JSONL is created before attachment preparation finishes");
        assert!(std::fs::read_dir(dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".u1-")), "cancelled staging directory remains");
        assert_eq!(h.prompt("FOLLOW", json!({})).await.unwrap()["text"], "NEXT");
        h.shutdown().await;
        assert!(std::fs::read_to_string(path).unwrap().contains("FOLLOW"));
    })
    .await
}

#[tokio::test]
async fn cancelled_fork_during_full_history_read_preserves_loaded_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "partial-source.jsonl",
            &[
                msg("m1", None, "user", "FIRST"),
                msg("m2", Some("m1"), "assistant", "SECOND"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let h = env
            .host()
            .faux(json!([{ "text": "AFTER" }]))
            .config(json!({"plugins":{"usage":{"enabled":false}}}))
            .build()
            .await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "S:load({ path = TARGET })").await;
        let (id, old_messages): (String, String) =
            eval(&h, "return S:info().session_id, mare.json.encode(A:list_messages().data)").await;
        let (finished, entered, switching): (bool, bool, bool) = eval(
            &h,
            r#"
            READ_WAITING = false
            local entered_tx, entered_rx = mare.task.channel()
            local blocked = mare.task.token()
            local read = mare.store.read_fork_snapshot
            mare.store.read_fork_snapshot = function(...)
                READ_WAITING = true
                assert(entered_tx:send(true))
                blocked:wait()
                return read(...)
            end
            local winner = mare.task.select(
                function() S:fork({ session_id = S:info().session_id }) end,
                function()
                    assert(entered_rx:recv())
                    return false
                end
            )
            local finished = winner == 1
            mare.store.read_fork_snapshot = read
            return finished, READ_WAITING, S:switching()
        "#,
        )
        .await;
        assert!(!finished && entered && !switching, "finished={finished}, entered={entered}, switching={switching}");
        let (after_id, messages): (String, String) =
            eval(&h, "return S:info().session_id, mare.json.encode(A:list_messages().data)").await;
        assert_eq!((after_id, messages), (id, old_messages));
        assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "AFTER");
        h.shutdown().await;
        assert!(std::fs::read_to_string(path).unwrap().contains("NEXT"));
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn fork_uses_its_observed_snapshot_after_source_path_replacement() {
    local(async {
        for stage in ["before", "after_observation"] {
            let env = TestEnv::new().await;
            let path = env.write_durable_session(
                "partial-source-race.jsonl",
                &[
                    msg("m1", None, "user", "ORIGINAL"),
                    msg("m2", Some("m1"), "assistant", "ANSWER"),
                    run_end("m2"),
                    json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                    msg("m3", Some("c1"), "user", "LATEST"),
                ],
            );
            let replaced_contents = std::fs::read_to_string(&path).unwrap().replace("ORIGINAL", "REPLACED");
            let replacement = env.durable_replacement(&path, &replaced_contents);
            let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "REPLACEMENT", replacement.to_string_lossy().to_string());
            exec(&h, "S:load({ path = TARGET })").await;
            if stage == "before" {
                std::fs::rename(&path, path.with_extension("moved")).unwrap();
                std::fs::rename(&replacement, &path).unwrap();
                std::fs::rename(
                    format!("{}.commit", mare_store::lock_path(&replacement).display()),
                    format!("{}.commit", mare_store::lock_path(&path).display()),
                )
                .unwrap();
            }
            global(&h, "READ_STAGE", stage);
            exec(
                &h,
                r#"
                local read = mare.store.read_fork_snapshot
                mare.store.read_fork_snapshot = function(...)
                    local result = table.pack(read(...))
                    if READ_STAGE == 'after_observation' then
                        mare.fs.rename(TARGET, TARGET .. '.moved')
                        mare.fs.rename(REPLACEMENT, TARGET)
                    end
                    return table.unpack(result, 1, result.n)
                end
            "#,
            )
            .await;
            let fork_path: String = eval(&h, "S:fork({ session_id = S:info().session_id }); return S:info().path").await;
            let forked = std::fs::read_to_string(fork_path).unwrap();
            if stage == "before" {
                assert!(forked.contains("REPLACED") && !forked.contains("ORIGINAL"), "{forked}");
            } else {
                assert!(forked.contains("ORIGINAL") && !forked.contains("REPLACED"), "{forked}");
            }
            assert!(!forked.contains("LATEST"), "the incomplete tail must not be copied: {forked}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), replaced_contents);
            h.shutdown().await;
        }
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn partial_fork_rechecks_source_at_publication() {
    local(async {
        for replace_lock in [false, true] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "partial-publish-race.jsonl",
                &[
                    msg("m1", None, "user", "ORIGINAL"),
                    msg("m2", Some("m1"), "assistant", "ANSWER"),
                    run_end("m2"),
                    json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                    msg("m3", Some("c1"), "user", "LATEST"),
                ],
            );
            let replacement = env.cwd.join("replacement-file");
            std::fs::write(&replacement, b"replacement").unwrap();
            let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(
                &h,
                "VICTIM",
                if replace_lock { mare_store::lock_path(&path) } else { path.clone() }.to_string_lossy().to_string(),
            );
            global(&h, "REPLACEMENT", replacement.to_string_lossy().to_string());
            exec(&h, "S:load({ path = TARGET })").await;
            let (ok, error, fork_path): (bool, String, String) = eval(
                &h,
                r#"
                local check = A.check_session_revision
                local changed = false
                A.check_session_revision = function(...)
                    if not changed then
                        changed = true
                        mare.fs.rename(VICTIM, VICTIM .. '.moved')
                        mare.fs.rename(REPLACEMENT, VICTIM)
                    end
                    return check(...)
                end
                local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
                A.check_session_revision = check
                return ok, tostring(err), S:info().path
            "#,
            )
            .await;
            assert!(ok, "lock={replace_lock}: {error}");
            assert_ne!(fork_path, path.to_string_lossy());
            let forked = std::fs::read_to_string(fork_path).unwrap();
            assert!(forked.contains("ORIGINAL") && forked.contains("ANSWER"), "{forked}");
            assert!(!forked.contains("LATEST"), "the incomplete tail must not be copied: {forked}");
            if !replace_lock {
                assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
            }
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn expanding_partial_history_cannot_overwrite_a_new_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "expand-race.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            S:load({ path = TARGET })
            WAITING = false
            local read = mare.store.read_snapshot
            mare.store.read_snapshot = function(...)
                local result = table.pack(read(...))
                WAITING = true
                mare.task.sleep(200)
                return table.unpack(result, 1, result.n)
            end
        "#,
        )
        .await;
        let worker = h.clone();
        let expanding = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(&worker, "local ok, e = pcall(S.entries, S); return ok, tostring(e)").await
        });
        wait_until(5, "partial history read", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return WAITING").await }
        })
        .await;
        let (switched, new_id): (bool, String) =
            eval(&h, "local ok = pcall(S.create, S); return ok, S:info().session_id").await;
        let (expanded, _) = expanding.await.unwrap();
        let after: String = eval(&h, "return S:info().session_id").await;
        assert!(!switched || !expanded || after == new_id, "history expansion overwrote the new session");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn partial_fork_uses_the_snapshot_if_source_changes_after_read() {
    local(async {
        use std::io::Write;
        let env = TestEnv::new().await;
        let path = env.write_session(
            "fork-revision-race.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            S:load({ path = TARGET })
            WAITING = false
            local release, wait = mare.task.channel()
            FORK_RELEASE = release
            local read = mare.store.read_fork_snapshot
            mare.store.read_fork_snapshot = function(...)
                local result = table.pack(read(...))
                WAITING = true
                wait:recv()
                return table.unpack(result, 1, result.n)
            end
        "#,
        )
        .await;
        let id: String = eval(&h, "return S:info().session_id").await;
        let worker = h.clone();
        let forking = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(
                &worker,
                "local ok, e = pcall(S.fork, S, { session_id = S:info().session_id }); return ok, tostring(e)",
            )
            .await
        });
        wait_until(5, "fork source read", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return WAITING").await }
        })
        .await;
        let mut external = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(external, "{{\"type\":\"name\",\"name\":\"external\"}}").unwrap();
        exec(&h, "FORK_RELEASE:send(true)").await;
        let (ok, error) = forking.await.unwrap();
        assert!(ok, "{error}");
        let (fork_id, fork_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        assert_ne!(fork_id, id);
        let forked = std::fs::read_to_string(fork_path).unwrap();
        assert!(forked.contains("ORIGINAL") && forked.contains("ANSWER"), "{forked}");
        assert!(!forked.contains("external"), "changes after the observed snapshot are not copied: {forked}");
        h.shutdown().await;
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn partial_fork_retires_source_and_contains_retirement_failures() {
    local(async {
        for mode in ["success", "append", "close", "both"] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "fork-retire.jsonl",
                &[
                    msg("m1", None, "user", "ORIGINAL"),
                    msg("m2", Some("m1"), "assistant", "ANSWER"),
                    run_end("m2"),
                    json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                    msg("m3", Some("c1"), "user", "LATEST"),
                ],
            );
            let h = env
                .host()
                .faux(json!([{ "text": "FORK ANSWER" }]))
                .config(json!({"plugins":{"usage":{"enabled":false}}}))
                .build()
                .await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            exec(&h, "S:load({ path = TARGET })").await;
            let append = mode == "append";
            let close = mode == "close" || mode == "both";
            let fail_discard = mode == "both";
            let fail = h
                .lua()
                .create_function(move |lua, ()| {
                    mare_binding::exec_gate::fail_session_journal_ops(lua, 0, append, close);
                    if fail_discard {
                        mare_binding::exec_gate::fail_next_discard_before_release(lua);
                    }
                    Ok(())
                })
                .unwrap();
            global(&h, "FAIL_RETIRE", fail);
            let (ok, err, switching, fork_path, barrier): (bool, String, bool, String, String) = eval(
                &h,
                r#"
                local check = A.check_session_revision
                A.check_session_revision = function(...) FAIL_RETIRE(); return check(...) end
                local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
                A.check_session_revision = check
                return ok, tostring(err), S:switching(), S:info().path or '', tostring(mare.task.session_error())
            "#,
            )
            .await;
            if mode == "success" {
                assert!(ok && !switching, "{err}");
                assert!(std::fs::read_to_string(&path).unwrap().contains("\"state\":\"closed\""));
                assert_eq!(h.prompt("FORK CONTINUES", json!({})).await.unwrap()["text"], "FORK ANSWER");
                assert!(std::fs::read_to_string(fork_path).unwrap().contains("FORK CONTINUES"));
            } else {
                assert!(!ok && switching && err.contains("old session close failed"), "{mode}: {err}");
                assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
                if fail_discard {
                    assert!(err.contains("discard failure before release"), "{err}");
                    assert!(
                        barrier.contains("fallback discard failed") && barrier.contains("before release"),
                        "{barrier}"
                    );
                }
            }
            if !fail_discard {
                assert!(
                    mare_store::SessionFile::open_existing(&path, None).is_ok(),
                    "{mode}: source lock remains held"
                );
            }
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn cancelling_partial_history_expansion_keeps_the_live_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "cancel-expand.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "S:load({ path = TARGET })").await;
        let (id, messages): (String, String) =
            eval(&h, "return S:info().session_id, mare.json.encode(A:list_messages().data)").await;
        let (finished, entered, count): (bool, bool, i64) = eval(
            &h,
            r#"
            READ_WAITING = false
            local entered_tx, entered_rx = mare.task.channel()
            local blocked = mare.task.token()
            local read = mare.store.read_snapshot
            mare.store.read_snapshot = function(...)
                local result = table.pack(read(...))
                READ_WAITING = true
                assert(entered_tx:send(true))
                blocked:wait()
                return table.unpack(result, 1, result.n)
            end
            local winner = mare.task.select(
                function() S:entries() end,
                function()
                    assert(entered_rx:recv())
                    return false
                end
            )
            local finished = winner == 1
            mare.store.read_snapshot = read
            return finished, READ_WAITING, #S:entries()
        "#,
        )
        .await;
        assert!(!finished && entered && count >= 4, "finished={finished}, entered={entered}, count={count}");
        let after: (String, String) = eval(&h, "return S:info().session_id, mare.json.encode(A:list_messages().data)").await;
        assert_eq!(after, (id, messages));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn partial_readonly_fork_expands_history_but_copies_only_its_completed_snapshot() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_durable_session(
            "readonly-expand.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "mare.ui.select = function() return 2 end; S:load({ path = TARGET })").await;
        let (before, expanded, has_m1, forked, fork_path): (String, String, bool, String, String) = eval(
            &h,
            r#"
            local before = mare.json.encode(A:list_messages().data)
            local entries = S:entries()
            local has_m1 = false
            for _, entry in ipairs(entries) do if entry.entry_id == 'm1' then has_m1 = true end end
            local expanded = mare.json.encode(A:list_messages().data)
            local ok, err = pcall(S.fork, S, { session_id = S:info().session_id })
            assert(ok, tostring(err))
            return before, expanded, has_m1, mare.json.encode(A:list_messages().data), S:info().path
        "#,
        )
        .await;
        assert!(before.contains("LATEST"), "the initially partial view keeps its tail: {before}");
        assert!(has_m1, "full history expansion must include the earlier entry");
        assert!(expanded.contains("sum") && expanded.contains("ANSWER") && expanded.contains("LATEST"), "{expanded}");
        assert!(forked.contains("ORIGINAL") && forked.contains("ANSWER"), "{forked}");
        assert!(!forked.contains("LATEST"), "the incomplete tail must not be copied: {forked}");
        let child = std::fs::read_to_string(fork_path).unwrap();
        assert!(child.contains("ORIGINAL") && child.contains("ANSWER"), "{child}");
        assert!(!child.contains("LATEST"), "the incomplete tail must not be copied: {child}");
        h.shutdown().await;
        drop(lock);
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn readonly_history_uses_bound_snapshot_after_source_replacement() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("readonly-replaced.jsonl", &[
            msg("m1", None, "user", "ORIGINAL"),
            msg("m2", Some("m1"), "assistant", "ANSWER"),
            json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
            msg("m3", Some("c1"), "user", "LATEST"),
        ]);
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true)
            .open(mare_store::lock_path(&path)).unwrap();
        lock.try_lock().unwrap();
        let h = env.host().faux(json!([{ "text": "MEMORY ANSWER" }])).config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "mare.ui.select = function() return 2 end; S:load({ path = TARGET })").await;
        h.prompt("MEMORY PROMPT", json!({})).await.unwrap();
        let before: (String, String, String) = eval(&h, "return S:info().session_id, S:leaf(), mare.json.encode(A:list_messages().data)").await;
        let replacement = std::fs::read_to_string(&path).unwrap().replace("ORIGINAL", "FORGED").replace("LATEST", "FORGED LATEST");
        std::fs::rename(&path, path.with_extension("moved")).unwrap();
        std::fs::write(&path, replacement).unwrap();
        let (ok, history, id, leaf, messages): (bool, String, String, String, String) = eval(&h, r#"
            local ok, err = pcall(S.entries, S)
            return ok, ok and mare.json.encode(err) or tostring(err), S:info().session_id, S:leaf(), mare.json.encode(A:list_messages().data)
        "#).await;
        assert!(ok && history.contains("ORIGINAL") && !history.contains("FORGED"), "{history}");
        assert_eq!((id, leaf, messages), before);
        h.shutdown().await;
    }).await
}

#[cfg(unix)]
#[tokio::test]
async fn readonly_partial_fork_uses_a_fresh_snapshot_after_source_replacement() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_durable_session(
            "partial-readonly-race.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let replacement = std::fs::read_to_string(&path).unwrap().replace("ORIGINAL", "REPLACED");
        let prepared = env.durable_replacement(&path, &replacement);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "mare.ui.select = function() return 2 end; S:load({ path = TARGET })").await;
        assert!(eval::<bool>(&h, "return S:info().readonly == true").await);
        // Expanding the view does not replace the source observation used by the fork.
        exec(&h, "S:entries()").await;
        exec(
            &h,
            r#"
            FORK_READS = 0
            local read = mare.store.read_fork_snapshot
            mare.store.read_fork_snapshot = function(...)
                FORK_READS = FORK_READS + 1
                return read(...)
            end
        "#,
        )
        .await;
        std::fs::rename(&path, path.with_extension("moved")).unwrap();
        std::fs::rename(&prepared, &path).unwrap();
        std::fs::rename(
            format!("{}.commit", mare_store::lock_path(&prepared).display()),
            format!("{}.commit", mare_store::lock_path(&path).display()),
        )
        .unwrap();
        let (ok, error, reads, fork_path): (bool, String, i64, String) = eval(
            &h,
            r#"
            local ok, result = pcall(S.fork, S, { session_id = S:info().session_id })
            return ok, tostring(result), FORK_READS, S:info().path
        "#,
        )
        .await;
        assert!(ok, "{error}");
        assert_eq!(reads, 1, "fork rereads a partial view exactly once");
        let forked = std::fs::read_to_string(fork_path).unwrap();
        assert!(forked.contains("REPLACED") && forked.contains("ANSWER"), "{forked}");
        assert!(!forked.contains("LATEST"), "the incomplete tail must not be copied: {forked}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        h.shutdown().await;
        drop(lock);
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn initial_locked_fork_reads_a_fresh_snapshot_without_the_writer_lock() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_durable_session(
            "initial-locked-race.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let replacement = std::fs::read_to_string(&path).unwrap().replace("ANSWER", "FORGED");
        let prepared = env.durable_replacement(&path, &replacement);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(mare_store::lock_path(&path))
            .unwrap();
        lock.try_lock().unwrap();
        let h = env.host().config(json!({"plugins":{"usage":{"enabled":false}}})).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        exec(&h, "mare.ui.select = function() return 1 end").await;
        std::fs::rename(&path, path.with_extension("moved")).unwrap();
        std::fs::rename(&prepared, &path).unwrap();
        std::fs::rename(
            format!("{}.commit", mare_store::lock_path(&prepared).display()),
            format!("{}.commit", mare_store::lock_path(&path).display()),
        )
        .unwrap();
        let fork_path: String = eval(&h, "S:load({ path = TARGET }); return S:info().path").await;
        let forked = std::fs::read_to_string(fork_path).unwrap();
        assert!(forked.contains("FORGED") && forked.contains("m2"), "{forked}");
        assert!(!forked.contains("LATEST"), "the incomplete tail must not be copied: {forked}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), replacement);
        drop(lock);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn partial_fork_does_not_carry_shell_directory_from_the_incomplete_tail() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let shell = other.join("sub");
        std::fs::create_dir(&shell).unwrap();
        let path = env.write_session(
            "partial-cwd.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                run_end("m2"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
                json!({"type":"custom", "kind":"working-directory/changed", "data":{"to":shell}}),
            ],
        );
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, body).unwrap();
        let pwd = if cfg!(windows) { "pwd -W" } else { "pwd -P" };
        let h = env
            .host()
            .faux(json!([{ "tool_calls": [{ "name": "bash", "arguments": { "command": pwd } }] }, { "text": "DONE" }]))
            .config(json!({"plugins":{"usage":{"enabled":false}, "session":{"resume_cwd":"session"}}}))
            .build()
            .await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (load_cwd, fork_cwd): (String, String) = eval(
            &h,
            r#"
            local last = ''
            C.cordis:on('session/loaded', function(e) last = e.shell_cwd or '' end)
            S:load({ path = TARGET })
            local loaded = last
            S:fork({ session_id = S:info().session_id })
            return loaded, last
        "#,
        )
        .await;
        assert_eq!(load_cwd, shell.to_string_lossy());
        assert_eq!(fork_cwd, other.to_string_lossy());
        h.prompt("check the shell directory", json!({})).await.unwrap();
        let messages: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        let messages: serde_json::Value = serde_json::from_str(&messages).unwrap();
        let result = messages.as_array().unwrap().iter().find(|m| m["role"] == "tool_result").unwrap();
        assert_eq!(result["is_error"], false, "{result}");
        let output = result["details"]["output"].as_str().unwrap().trim();
        assert_eq!(dunce::canonicalize(output).unwrap(), other, "{result}");
        h.shutdown().await;
    })
    .await
}
