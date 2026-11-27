use super::*;

#[tokio::test]
async fn resuming_a_session_from_another_directory_asks_and_never_switches_silently() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("2026-01-01T00-00-00_moved.jsonl", &[msg("m1", None, "user", "hello")]);
        // the header says the session was recorded in `other`
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, body).unwrap();
        let h = env.host().build().await;
        expose(&h);
        global(&h, "PATH", path.to_string_lossy().to_string());
        // the user picks "Use the session's directory": the shell starts there; the file keeps it
        let (title, cwd): (String, String) = eval(
            &h,
            r#"mare.ui.connected = function() return true end
               local seen
               mare.ui.select = function(o) seen = o.title; return 1 end
               S:load({ path = PATH })
               return seen, T:cwd()"#,
        )
        .await;
        assert!(title.contains("another directory"), "{title}");
        assert_eq!(dunce::canonicalize(&cwd).unwrap(), other);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn readonly_choice_for_other_directory_keeps_complete_bound_history() {
    local(async {
        for covers_leaf in [true, false] {
            let env = TestEnv::new().await;
            let elsewhere = tempfile::tempdir().unwrap();
            let other = dunce::canonicalize(elsewhere.path()).unwrap();
            let last = if covers_leaf { Some("c1") } else { Some("m1") };
            let path = env.write_session(
                "other-readonly.jsonl",
                &[
                    msg("m1", None, "user", "ORIGINAL"),
                    msg("m2", Some("m1"), "assistant", "ANSWER"),
                    json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                    msg("m3", last, "user", "LATEST"),
                ],
            );
            let body = std::fs::read_to_string(&path).unwrap().replacen(
                &serde_json::to_string(&env.cwd).unwrap(),
                &serde_json::to_string(&other).unwrap(),
                1,
            );
            std::fs::write(&path, body).unwrap();
            let h = env.host().build().await;
            expose(&h);
            fake_ui(&h).await;
            let rows_before = index_rows();
            let ix = mare_store::index::ready(&mare_shared::paths::sessions_dir()).unwrap();
            let checkpoint_before = ix.lock().unwrap().checkpoint(&path).unwrap();
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let (ok, entries): (bool, String) = eval(
                &h,
                r#"
                mare.ui.select = function() return 3 end
                S:load({ path = TARGET })
                local ok, e = pcall(S.entries, S)
                return ok, ok and mare.json.encode(e) or tostring(e)
            "#,
            )
            .await;
            assert!(
                ok && entries.contains("ORIGINAL") && entries.contains("LATEST"),
                "covers={covers_leaf}: {entries}"
            );
            assert!(eval::<bool>(&h, "return S:info().readonly == true").await);
            assert_eq!(index_rows(), rows_before, "read-only conversion must not write the index");
            assert_eq!(ix.lock().unwrap().checkpoint(&path).unwrap(), checkpoint_before);
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn cancelling_directory_choice_discards_unactivated_target_without_checkpoint() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("cancel-other.jsonl", &[msg("m1", None, "user", "ORIGINAL")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, &body).unwrap();
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let rows_before = index_rows();
        let ix = mare_store::index::ready(&mare_shared::paths::sessions_dir()).unwrap();
        let checkpoint_before = ix.lock().unwrap().checkpoint(&path).unwrap();
        let before: String = eval(&h, "return S:info().session_id").await;
        let finished: bool = eval(
            &h,
            r#"
            mare.ui.select = function() mare.task.sleep(200); return 3 end
            return mare.task.timeout(40, function() S:load({ path = TARGET }) end)
        "#,
        )
        .await;
        assert!(!finished);
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, before);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        assert_eq!(index_rows(), rows_before);
        assert_eq!(ix.lock().unwrap().checkpoint(&path).unwrap(), checkpoint_before);
        assert!(mare_store::SessionFile::open_existing(&path, None).is_ok());
        let (ok, err): (bool, String) = eval(
            &h,
            r#"
            local read = mare.store.read_tail_snapshot
            mare.store.read_tail_snapshot = function() error('injected tail failure') end
            local ok, err = pcall(S.load, S, { path = TARGET })
            mare.store.read_tail_snapshot = read
            return ok, tostring(err)
        "#,
        )
        .await;
        assert!(!ok && err.contains("injected tail failure"), "{err}");
        assert_eq!(index_rows(), rows_before);
        assert_eq!(ix.lock().unwrap().checkpoint(&path).unwrap(), checkpoint_before);
        assert!(mare_store::SessionFile::open_existing(&path, None).is_ok());
        h.shutdown().await;
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn readonly_target_is_discarded_before_completion_can_fail() {
    local(async {
        for after_registration in [false, true] {
            let env = TestEnv::new().await;
            let elsewhere = tempfile::tempdir().unwrap();
            let other = dunce::canonicalize(elsewhere.path()).unwrap();
            let path = env.write_session("readonly-completion.jsonl", &[msg("m1", None, "user", "ORIGINAL")]);
            let body = std::fs::read_to_string(&path).unwrap().replacen(
                &serde_json::to_string(&env.cwd).unwrap(),
                &serde_json::to_string(&other).unwrap(),
                1,
            );
            std::fs::write(&path, &body).unwrap();
            let h = env.host().build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let rows_before = index_rows();
            let ix = mare_store::index::ready(&mare_shared::paths::sessions_dir()).unwrap();
            let checkpoint_before = ix.lock().unwrap().checkpoint(&path).unwrap();
            if after_registration {
                mare_binding::exec_gate::fail_next_session_completion_wrap(h.lua());
            } else {
                mare_binding::exec_gate::fail_next_session_completion(h.lua());
            }
            let (ok, err): (bool, String) = eval(
                &h,
                r#"
                mare.ui.select = function() return 3 end
                local ok, err = pcall(S.load, S, { path = TARGET })
                collectgarbage('collect')
                return ok, tostring(err)
            "#,
            )
            .await;
            assert!(!ok && err.contains("completion"), "{err}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
            assert_eq!(index_rows(), rows_before);
            assert_eq!(ix.lock().unwrap().checkpoint(&path).unwrap(), checkpoint_before);
            assert!(mare_store::SessionFile::open_existing(&path, None).is_ok(), "target lock remains held");
            h.shutdown().await;
        }
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn readonly_target_discard_failure_poison_barrier_preserves_index_and_unlocks() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("readonly-discard-error.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, &body).unwrap();
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let rows_before = index_rows();
        let ix = mare_store::index::ready(&mare_shared::paths::sessions_dir()).unwrap();
        let checkpoint_before = ix.lock().unwrap().checkpoint(&path).unwrap();
        mare_binding::exec_gate::fail_session_journal_ops(h.lua(), 0, false, true);
        let (ok, error, switching): (bool, String, bool) = eval(
            &h,
            r#"
            mare.ui.select = function() return 3 end
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:switching()
        "#,
        )
        .await;
        assert!(!ok && error.contains("discard failed"), "{error}");
        assert!(switching, "discard failure after agent adoption must poison the barrier");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        assert_eq!(index_rows(), rows_before);
        assert_eq!(ix.lock().unwrap().checkpoint(&path).unwrap(), checkpoint_before);
        assert!(mare_store::SessionFile::open_existing(&path, None).is_ok());
        h.shutdown().await;
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn readonly_source_lock_survives_old_session_close_until_publication() {
    local(async {
        use std::sync::mpsc;
        use std::time::Duration;
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("readonly-publish-lock.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, &body).unwrap();
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        h.prompt("OLD", json!({})).await.unwrap();
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        mare_binding::store::before_close_marker(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let target = path.clone();
        let observer = std::thread::spawn(move || {
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let blocked = mare_store::SessionFile::open_existing(&target, None).is_err();
            release_tx.send(()).unwrap();
            blocked
        });
        exec(&h, "mare.ui.select = function() return 3 end; S:load({ path = TARGET })").await;
        assert!(observer.join().unwrap(), "the source lock was released before state installation");
        assert!(eval::<bool>(&h, "return S:info().readonly == true").await);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
        h.shutdown().await;
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn readonly_source_change_during_old_close_poison_barrier() {
    local(async {
        use std::io::Write;
        use std::sync::mpsc;
        use std::time::Duration;
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session("readonly-publish-change.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, &body).unwrap();
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        h.prompt("OLD", json!({})).await.unwrap();
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        mare_binding::store::before_close_marker(move || {
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        });
        let target = path.clone();
        let external = std::thread::spawn(move || {
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            writeln!(
                std::fs::OpenOptions::new().append(true).open(target).unwrap(),
                "{{\"type\":\"name\",\"name\":\"external\"}}"
            )
            .unwrap();
            release_tx.send(()).unwrap();
        });
        let (ok, err, switching): (bool, String, bool) = eval(
            &h,
            r#"
            mare.ui.select = function() return 3 end
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:switching()
        "#,
        )
        .await;
        external.join().unwrap();
        assert!(!ok && err.contains("changed") && switching, "{err}; switching={switching}");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn readonly_choice_can_retire_reused_current_writer() {
    local(async {
        let env = TestEnv::new().await;
        let elsewhere = tempfile::tempdir().unwrap();
        let other = dunce::canonicalize(elsewhere.path()).unwrap();
        let path = env.write_session(
            "readonly-reuse.jsonl",
            &[
                msg("m1", None, "user", "ORIGINAL"),
                msg("m2", Some("m1"), "assistant", "ANSWER"),
                json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                msg("m3", Some("c1"), "user", "LATEST"),
            ],
        );
        let body = std::fs::read_to_string(&path).unwrap().replacen(
            &serde_json::to_string(&env.cwd).unwrap(),
            &serde_json::to_string(&other).unwrap(),
            1,
        );
        std::fs::write(&path, body).unwrap();
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, readonly, entries): (bool, String, bool, String) = eval(
            &h,
            r#"
            mare.ui.select = function() return 2 end
            S:load({ path = TARGET })
            mare.ui.select = function() return 3 end
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:info().readonly == true, mare.json.encode(S:entries())
        "#,
        )
        .await;
        assert!(ok && readonly && entries.contains("ORIGINAL"), "{err}: {entries}");
        assert!(mare_store::SessionFile::open_existing(&path, None).is_ok());
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn readonly_choice_rejects_source_replacement_during_full_read() {
    local(async {
        for covers_leaf in [true, false] {
            let env = TestEnv::new().await;
            let elsewhere = tempfile::tempdir().unwrap();
            let other = dunce::canonicalize(elsewhere.path()).unwrap();
            let last = if covers_leaf { Some("c1") } else { Some("m1") };
            let path = env.write_session(
                "other-readonly-race.jsonl",
                &[
                    msg("m1", None, "user", "ORIGINAL"),
                    msg("m2", Some("m1"), "assistant", "ANSWER"),
                    json!({"type":"compaction", "entry_id":"c1", "parent_entry_id":"m2", "first_kept_entry_id":"m2", "summary":"sum"}),
                    msg("m3", last, "user", "LATEST"),
                ],
            );
            let ix = mare_store::index::ready(&mare_shared::paths::sessions_dir()).unwrap();
            mare_store::index::reconcile(&ix).unwrap();
            let mut body = std::fs::read_to_string(&path).unwrap().replacen(
                &serde_json::to_string(&env.cwd).unwrap(),
                &serde_json::to_string(&other).unwrap(),
                1,
            );
            body.push_str(&json!({ "type": "name", "name": "index-ready" }).to_string());
            body.push('\n');
            std::fs::write(&path, &body).unwrap();
            let replacement = path.with_extension("replacement");
            std::fs::write(&replacement, body.replace("ORIGINAL", "FORGED")).unwrap();
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(
                env.config_dir().join("init.lua"),
                concat!(
                    "mare._test_index_reconcile_done = false\n",
                    "local reconcile = mare.store.index_reconcile\n",
                    "mare.store.index_reconcile = function(...)\n",
                    "  local result = table.pack(reconcile(...))\n",
                    "  mare._test_index_reconcile_done = true\n",
                    "  return table.unpack(result, 1, result.n)\n",
                    "end\n"
                ),
            )
            .unwrap();
            let mut host = env.host();
            host.opts.load_init = true;
            host.opts.no_user_plugins = false;
            let h = host.build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "REPLACEMENT", replacement.to_string_lossy().to_string());
            wait_until(5, "startup index reconcile", || async {
                eval::<bool>(&h, "return mare._test_index_reconcile_done == true").await
            })
            .await;
            let rows_before = index_rows();
            let ix = mare_store::index::ready(&mare_shared::paths::sessions_dir()).unwrap();
            let checkpoint_before = ix.lock().unwrap().checkpoint(&path).unwrap();
            let before: String = eval(&h, "return S:info().session_id").await;
            let (ok, error, after, index_appends): (bool, String, String, i64) = eval(
                &h,
                r#"
                mare.ui.select = function() return 3 end
                INDEX_APPENDS = 0
                local append = mare.store.index_append
                mare.store.index_append = function(...)
                    INDEX_APPENDS = INDEX_APPENDS + 1
                    return append(...)
                end
                local read = mare.store.read_snapshot
                mare.store.read_snapshot = function(...)
                    mare.fs.rename(TARGET, TARGET .. '.moved')
                    mare.fs.rename(REPLACEMENT, TARGET)
                    return read(...)
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                mare.store.read_snapshot = read
                mare.store.index_append = append
                return ok, tostring(err), S:info().session_id, INDEX_APPENDS
            "#,
            )
            .await;
            assert!(!ok && error.contains("changed"), "covers={covers_leaf}: {error}");
            assert_eq!(after, before);
            assert_eq!(index_appends, 0, "discarding the unactivated target must not append an index row");
            assert_eq!(index_rows(), rows_before, "discarding the unactivated target must not write the index");
            assert_eq!(ix.lock().unwrap().checkpoint(&path).unwrap(), checkpoint_before);
            h.shutdown().await;
        }
    })
    .await
}
