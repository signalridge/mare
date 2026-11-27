use super::*;

#[tokio::test]
async fn initial_handoff_settled_and_failed_require_sync_and_recover_after_restart() {
    local(async {
        // Part A: `settled` write fails -> not published to memory/disk -> restart recovers without re-prompting.
        let env = TestEnv::new().await;
        let h1 = env.host().faux(json!([{"text": "initial-ok"}])).build().await;
        expose(&h1);
        let target_path_settled: String = eval(
            &h1,
            r#"
            -- Verify sync=true barrier is wired in session:add_record.
            assert(C.cordis:get("session"):add_record('sync-probe', { ok = true }, { sync = true }))
            assert(#C.cordis:get("session"):list_records('sync-probe').data == 1)

            local orig_record = S.add_record
            S.add_record = function(self, a, b, c, d)
                local kind, data, opts = a, b, c
                if type(a) ~= 'string' then
                    kind, data, opts = b, c, d
                end
                if kind == 'session/transition-initial' and type(data) == 'table' and data.status == 'settled' then
                    assert(type(opts) == 'table' and opts.sync == true, 'settled must request sync=true')
                    error('injected settled sync failure')
                end
                return orig_record(self, a, b, c, d)
            end

            local info = C.cordis:get("session"):info()
            local res = C.cordis:get("session"):transition({
                mode = 'new',
                initial = { kind = 'input', content = 'implement plan', source = 'handoff' },
            }, { session_id = info.session_id, session_epoch = info.session_epoch })
            S.add_record = orig_record

            assert(res.status == 'rejected', tostring(res.status))
            assert(tostring(res.error):find('injected settled sync failure', 1, true), tostring(res.error))
            local recs = C.cordis:get("session"):list_records('session/transition-initial').data
            assert(#recs == 1 and recs[1].status == 'pending', 'settled failure must leave only pending')
            return C.cordis:get("session"):info().path
        "#,
        )
        .await;
        let disk_before = std::fs::read_to_string(&target_path_settled).unwrap();
        assert!(disk_before.contains("\"status\":\"pending\""), "{disk_before}");
        assert!(!disk_before.contains("\"status\":\"settled\""), "{disk_before}");
        h1.shutdown().await;

        let h2 = env.host().faux(json!([{"text": "must-not-run-duplicate"}])).build().await;
        expose(&h2);
        let recovered = std::sync::Arc::new(tokio::sync::Notify::new());
        let recovered_signal = recovered.clone();
        global(
            &h2,
            "SIGNAL_RECOVERED",
            h2.lua()
                .create_function(move |_, ()| {
                    recovered_signal.notify_one();
                    Ok(())
                })
                .unwrap(),
        );
        global(&h2, "TARGET_PATH", target_path_settled.clone());
        exec(
            &h2,
            r#"
            RECOVERED_EV = nil
            C.cordis:on('session/transition-completed', function(ev)
                RECOVERED_EV = ev
                SIGNAL_RECOVERED()
            end)
            assert(C.cordis:get("session"):load({ path = TARGET_PATH }))
        "#,
        )
        .await;
        if eval::<bool>(&h2, "return RECOVERED_EV == nil").await {
            tokio::time::timeout(std::time::Duration::from_secs(5), recovered.notified())
                .await
                .expect("timed out waiting for settled recovery");
        }
        let ok_a: bool = eval(
            &h2,
            r#"
            local recs = C.cordis:get("session"):list_records('session/transition-initial').data
            local msgs = C.cordis:get("agent"):list_messages().data
            return RECOVERED_EV ~= nil
                and RECOVERED_EV.recovered == true
                and RECOVERED_EV.initial_status == 'settled'
                and #recs == 2 and recs[1].status == 'pending' and recs[2].status == 'settled'
                and #msgs == 2 and msgs[2].content[1].text == 'initial-ok'
        "#,
        )
        .await;
        assert!(ok_a);
        let disk_after = std::fs::read_to_string(&target_path_settled).unwrap();
        assert!(disk_after.contains("\"status\":\"settled\""), "{disk_after}");
        h2.shutdown().await;
        drop(env);

        // Part B: `failed` write fails -> not published -> restart retries and settles.
        let env_b = TestEnv::new().await;
        let h3 = env_b
            .host()
            .faux(json!([{"error": "provider down", "kind": "rate_limit"}]))
            .config(json!({ "plugins": { "llm": { "retries": 0 } } }))
            .build()
            .await;
        expose(&h3);
        let target_path_failed: String = eval(
            &h3,
            r#"
            local orig_record = S.add_record
            S.add_record = function(self, a, b, c, d)
                local kind, data, opts = a, b, c
                if type(a) ~= 'string' then
                    kind, data, opts = b, c, d
                end
                if kind == 'session/transition-initial' and type(data) == 'table' and data.status == 'failed' then
                    assert(type(opts) == 'table' and opts.sync == true, 'failed must request sync=true')
                    error('injected failed sync failure')
                end
                return orig_record(self, a, b, c, d)
            end

            local info = C.cordis:get("session"):info()
            local res = C.cordis:get("session"):transition({
                mode = 'new',
                initial = { kind = 'input', content = 'retryable plan', source = 'handoff' },
            }, { session_id = info.session_id, session_epoch = info.session_epoch })
            S.add_record = orig_record

            assert(res.status == 'rejected', tostring(res.status))
            local recs = C.cordis:get("session"):list_records('session/transition-initial').data
            assert(#recs == 1 and recs[1].status == 'pending', 'failed sync failure must leave only pending')
            return C.cordis:get("session"):info().path
        "#,
        )
        .await;
        let disk_failed_before = std::fs::read_to_string(&target_path_failed).unwrap();
        assert!(disk_failed_before.contains("\"status\":\"pending\""), "{disk_failed_before}");
        assert!(!disk_failed_before.contains("\"status\":\"failed\""), "{disk_failed_before}");
        h3.shutdown().await;

        let h4 = env_b.host().faux(json!([{"text": "recovered-after-failed-persist"}])).build().await;
        expose(&h4);
        let recovered_b = std::sync::Arc::new(tokio::sync::Notify::new());
        let recovered_b_signal = recovered_b.clone();
        global(
            &h4,
            "SIGNAL_RECOVERED_B",
            h4.lua()
                .create_function(move |_, ()| {
                    recovered_b_signal.notify_one();
                    Ok(())
                })
                .unwrap(),
        );
        global(&h4, "TARGET_PATH_B", target_path_failed.clone());
        exec(
            &h4,
            r#"
            RECOVERED_B_EV = nil
            C.cordis:on('session/transition-completed', function(ev)
                RECOVERED_B_EV = ev
                SIGNAL_RECOVERED_B()
            end)
            assert(C.cordis:get("session"):load({ path = TARGET_PATH_B }))
        "#,
        )
        .await;
        if eval::<bool>(&h4, "return RECOVERED_B_EV == nil").await {
            tokio::time::timeout(std::time::Duration::from_secs(5), recovered_b.notified())
                .await
                .expect("timed out waiting for failed-persist recovery");
        }
        let ok_b: bool = eval(
            &h4,
            r#"
            local recs = C.cordis:get("session"):list_records('session/transition-initial').data
            local msgs = C.cordis:get("agent"):list_messages().data
            return RECOVERED_B_EV ~= nil
                and RECOVERED_B_EV.recovered == true
                and RECOVERED_B_EV.initial_status == 'settled'
                and #recs == 2 and recs[1].status == 'pending' and recs[2].status == 'settled'
                and #msgs >= 2 and msgs[#msgs].content[1].text == 'recovered-after-failed-persist'
        "#,
        )
        .await;
        assert!(ok_b);
        h4.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn recovery_does_not_skip_initial_handoff_when_unrelated_message_is_present() {
    local(async {
        // Part A: unrelated message injected during `session/transition-committed` window.
        let env = TestEnv::new().await;
        let h1 = env
            .host()
            .faux(json!([
                {"text": "unrelated-reply"},
                {"text": "handoff-reply"}
            ]))
            .build()
            .await;
        expose(&h1);
        let ok_committed: bool = eval(
            &h1,
            r#"
            local injected = false
            C.cordis:on('session/transition-committed', function()
                if not injected then
                    injected = true
                    local ag = C.cordis:get("agent")
                    local r = ag:wait(ag:prompt('unrelated message', {}).turn_id)
                    assert(not r.error, tostring(r.error))
                end
            end)
            local info = C.cordis:get("session"):info()
            local res = C.cordis:get("session"):transition({
                mode = 'new',
                initial = { kind = 'input', content = 'run handoff', source = 'handoff' },
            }, { session_id = info.session_id, session_epoch = info.session_epoch })
            assert(res.status == 'applied' and res.initial_status == 'settled', mare.json.encode(res))
            local msgs = C.cordis:get("agent"):list_messages().data
            return #msgs == 4
                and msgs[1].content[1].text == 'unrelated message'
                and msgs[2].content[1].text == 'unrelated-reply'
                and msgs[3].content[1].text == 'run handoff'
                and msgs[4].content[1].text == 'handoff-reply'
        "#,
        )
        .await;
        assert!(ok_committed);
        h1.shutdown().await;
        drop(env);

        // Part B: unrelated message + unrelated agent/run-end present on disk before restart recovery.
        let env_b = TestEnv::new().await;
        let h2 = env_b.host().faux(json!([{"text": "unrelated-before-restart"}])).build().await;
        expose(&h2);
        let (sid, sgen, saved_path): (String, String, String) = eval(
            &h2,
            r#"
            C.cordis:get("session"):create()
            local ag = C.cordis:get("agent")
            local r = ag:wait(ag:prompt('unrelated before restart', {}).turn_id)
            assert(not r.error, tostring(r.error))
            local info = C.cordis:get("session"):info()
            return info.session_id, info.session_epoch, info.path
        "#,
        )
        .await;
        h2.shutdown().await;
        let pending_line = json!({
            "type": "custom",
            "kind": "session/transition-initial",
            "provenance": "session-host",
            "data": {
                "transition_id": "tr-lz11",
                "turn_id": "transition:tr-lz11",
                "mode": "new",
                "status": "pending",
                "baseline_messages": 0,
                "target": { "session_id": sid, "session_epoch": sgen },
                "initial": { "kind": "input", "content": "run handoff after restart", "source": "handoff" }
            },
            "timestamp": 1000
        });
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(&saved_path).unwrap();
        writeln!(f, "{pending_line}").unwrap();

        let h3 = env_b.host().faux(json!([{"text": "handoff-after-unrelated"}])).build().await;
        expose(&h3);
        let recovered = std::sync::Arc::new(tokio::sync::Notify::new());
        let recovered_signal = recovered.clone();
        global(
            &h3,
            "SIGNAL_RECOVERY",
            h3.lua()
                .create_function(move |_, ()| {
                    recovered_signal.notify_one();
                    Ok(())
                })
                .unwrap(),
        );
        global(&h3, "RECOVERY_PATH", saved_path);
        exec(
            &h3,
            r#"
            RECOVERY_EV = nil
            C.cordis:on('session/transition-completed', function(ev)
                RECOVERY_EV = ev
                SIGNAL_RECOVERY()
            end)
            assert(C.cordis:get("session"):load({ path = RECOVERY_PATH }))
        "#,
        )
        .await;
        if eval::<bool>(&h3, "return RECOVERY_EV == nil").await {
            tokio::time::timeout(std::time::Duration::from_secs(5), recovered.notified())
                .await
                .expect("timed out waiting for transition recovery");
        }
        let ok_recovery: bool = eval(
            &h3,
            r#"
            local msgs = C.cordis:get("agent"):list_messages().data
            local recs = C.cordis:get("session"):list_records('session/transition-initial').data
            return RECOVERY_EV ~= nil
                and RECOVERY_EV.recovered == true
                and RECOVERY_EV.initial_status == 'settled'
                and #recs == 2 and recs[2].status == 'settled'
                and #msgs == 4
                and msgs[3].content[1].text == 'run handoff after restart'
                and msgs[4].content[1].text == 'handoff-after-unrelated'
        "#,
        )
        .await;
        assert!(ok_recovery);
        h3.shutdown().await;
    })
    .await
}
