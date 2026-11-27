//! Session switching, generation barriers, and provider reload isolation.

use super::*;

#[tokio::test]
async fn switching_rejects_an_inflight_agent_persistence_write() {
    local(async {
        let env = TestEnv::new().await;
        let target = env.write_session("inflight-target.jsonl", &[msg("t1", None, "user", "TARGET")]);
        let target_before = std::fs::read(&target).unwrap();
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("OPEN-OLD", json!({})).await.unwrap();
        let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        global(&h, "TARGET", target.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            PERSIST_WAITING, RELEASE_PERSIST = false, false
            local orig_persist = S.persist_message
            S.persist_message = function(self, e)
                if e.message and e.message.content[1].text == 'OLD-LATE' then
                    PERSIST_WAITING = true
                    while not RELEASE_PERSIST do mare.task.sleep(10) end
                end
                return orig_persist(self, e)
            end
        "#,
        )
        .await;
        let worker = h.clone();
        let writing = tokio::task::spawn_local(async move {
            exec(&worker, "A:append({role='user', content={{type='text',text='OLD-LATE'}}})").await;
        });
        wait_until(5, "persistence dispatch", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return PERSIST_WAITING").await }
        })
        .await;
        let (ok, error): (bool, String) =
            eval(&h, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)").await;
        exec(&h, "RELEASE_PERSIST = true").await;
        writing.await.unwrap();
        assert!(!ok && error.contains("persist"), "{error}");
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, old_id);
        assert!(std::fs::read_to_string(&old_path).unwrap().contains("OLD-LATE"));
        assert_eq!(std::fs::read(&target).unwrap(), target_before);
        assert!(eval::<String>(&h, "return mare.json.encode(A:list_messages().data)").await.contains("OLD-LATE"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_reload_during_persistence_never_moves_an_old_message() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("OPEN-OLD", json!({})).await.unwrap();
        let old_path: String = eval(&h, "return S:info().path").await;
        exec(
            &h,
            r#"
            PERSIST_WAITING, RELEASE_PERSIST = false, false
            local orig_persist = S.persist_message
            S.persist_message = function(self, e)
                if e.message and e.message.content[1].text == 'OLD-LATE' then
                    PERSIST_WAITING = true
                    while not RELEASE_PERSIST do mare.task.sleep(10) end
                end
                return orig_persist(self, e)
            end
        "#,
        )
        .await;
        let worker = h.clone();
        let writing = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(
                &worker,
                r#"
                local ok, err = A:append({role='user', content={{type='text',text='OLD-LATE'}}})
                return ok ~= nil, tostring(err)
            "#,
            )
            .await
        });
        wait_until(5, "persistence dispatch", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return PERSIST_WAITING").await }
        })
        .await;
        h.kernel.reload(Some("session")).await.unwrap();
        global(&h, "NEW_S", h.service_table("session").unwrap());
        exec(&h, "assert(NEW_S:add_record('probe', {}, {persist=true})); RELEASE_PERSIST = true").await;
        let new_path: String = eval(&h, "return NEW_S:info().path").await;
        let (ok, error) = writing.await.unwrap();
        assert!(!ok && error.contains("session changed"), "{error}");
        assert!(!std::fs::read_to_string(&old_path).unwrap().contains("OLD-LATE"));
        assert!(!std::fs::read_to_string(&new_path).unwrap().contains("OLD-LATE"));
        assert!(!eval::<String>(&h, "return mare.json.encode(A:list_messages().data)").await.contains("OLD-LATE"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_recorder_refuses_records_from_another_generation() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("OPEN-OLD", json!({})).await.unwrap();
        let old_path: String = eval(&h, "return S:info().path").await;
        let before = std::fs::read(&old_path).unwrap();
        let errors: String = eval(&h, r#"
            local info = S:info()
            local errors = {}
            local calls = {
                { 'persist_message', { session_id=info.session_id, session_epoch='other-epoch', message={role='user',content={}} } },
                { 'record_tool_state', { session_id=info.session_id, session_epoch='other-epoch', tool_call_id='x', name='probe', state='started' } },
                { 'record_request', { session_id=info.session_id, session_epoch='other-epoch', request_id='r', attempt=1 } },
                { 'record_request_end', { session_id=info.session_id, session_epoch='other-epoch', request_id='r' } },
                { 'record_request_aborted', { session_id=info.session_id, session_epoch='other-epoch', request_id='r', attempt=1 } },
            }
            for _, spec in ipairs(calls) do
                local ok, err = pcall(S[spec[1]], S, spec[2])
                assert(not ok and type(err) == 'table' and err.code == 'stale_session', tostring(err))
                errors[#errors + 1] = tostring(err.message)
            end
            return table.concat(errors, '\n')
        "#).await;
        assert_eq!(errors.matches("session changed before").count(), 5, "{errors}");
        assert_eq!(std::fs::read(&old_path).unwrap(), before);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn cancelled_persistence_releases_the_switch_barrier() {
    local(async {
        let env = TestEnv::new().await;
        let target = env.write_session("cancelled-persist-target.jsonl", &[msg("t1", None, "user", "TARGET")]);
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("OPEN-OLD", json!({})).await.unwrap();
        global(&h, "TARGET", target.to_string_lossy().to_string());
        let (finished, waiting, loaded, error): (bool, bool, bool, String) = eval(
            &h,
            r#"
            local waiting = false
            local orig_persist = S.persist_message
            S.persist_message = function(self, e)
                if e.message and e.message.content[1].text == 'CANCEL' then
                    waiting = true
                    mare.task.sleep(1000)
                end
                return orig_persist(self, e)
            end
            local finished = mare.task.timeout(40, function()
                A:append({role='user', content={{type='text',text='CANCEL'}}})
            end)
            local ok, err = pcall(S.load, S, { path = TARGET })
            return finished, waiting, ok, tostring(err)
        "#,
        )
        .await;
        assert!(!finished && waiting && loaded, "{error}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_failed_old_session_record_cannot_be_replayed_into_a_new_session() {
    local(async {
        let env = TestEnv::new().await;
        let target = env.write_session("pending-target.jsonl", &[msg("t1", None, "user", "TARGET")]);
        let target_before = std::fs::read(&target).unwrap();
        let h = env.host().faux(json!([{ "text": "OLD" }, { "text": "RECOVERED" }])).build().await;
        expose(&h);
        h.prompt("OPEN-OLD", json!({})).await.unwrap();
        let (old_id, old_path): (String, String) = eval(&h, "return S:info().session_id, S:info().path").await;
        global(&h, "TARGET", target.to_string_lossy().to_string());
        exec(
            &h,
            r#"
            FAIL_OLD = true
            local orig_persist = S.persist_message
            S.persist_message = function(...)
                if FAIL_OLD then require('mare_lib').fail('persist_failed', 'old disk failure') end
                return orig_persist(...)
            end
        "#,
        )
        .await;
        h.prompt("OLD-QUEUED", json!({})).await.unwrap();
        let (ok, err): (bool, String) =
            eval(&h, "local ok, err = pcall(S.load, S, { path = TARGET }); return ok, tostring(err)").await;
        assert!(!ok && err.contains("persist"), "{err}");
        assert_eq!(eval::<String>(&h, "return S:info().session_id").await, old_id);
        assert_eq!(std::fs::read(&target).unwrap(), target_before);
        exec(&h, "FAIL_OLD = false").await;
        h.prompt("RECOVER-OLD", json!({})).await.unwrap();
        let old = std::fs::read_to_string(&old_path).unwrap();
        assert!(old.contains("OLD-QUEUED"), "{old}");
        assert!(!std::fs::read_to_string(&target).unwrap().contains("OLD-QUEUED"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_context_cannot_begin_or_publish_a_session_switch() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (ok, err, adopting, adopt_err, sync_ok, sync_err, unchanged): (
            bool,
            String,
            bool,
            String,
            bool,
            String,
            bool,
        ) = eval(
            &h,
            r#"
            local id = S:info().session_id
            local ok, err, adopting, adopt_err, sync_ok, sync_err = mare.exec.run({tool="probe"}, function()
                local ok, err = pcall(S.create, S)
                local aok, aerr = pcall(A.adopt_session, A, {id="forged", messages={}})
                local sok, serr = pcall(mare.task.call_sync, function() return S:create() end)
                return ok, tostring(err), aok, tostring(aerr), sok, tostring(serr)
            end)
            return ok, err, adopting, adopt_err, sync_ok, sync_err, S:info().session_id == id
        "#,
        )
        .await;
        assert!(!ok && err.contains("inside a tool"), "{err}");
        assert!(!adopting && adopt_err.contains("inside a tool"), "{adopt_err}");
        assert!(!sync_ok && sync_err.contains("inside a tool"), "{sync_err}");
        assert!(unchanged);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_record_cannot_cross_a_reload_of_the_same_session() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("OPEN-OLD", json!({})).await.unwrap();
        let (same_id, generation_changed, accepted, error, count): (bool, bool, bool, String, i64) = eval(
            &h,
            r#"
          local before = S:info()
          local ready_tx, ready_rx = mare.task.channel()
          local release_tx, release_rx = mare.task.channel()
          local done_tx, done_rx = mare.task.channel()
          assert(type(S:record_tool_state({tool_call_id='probe', name='probe', state='started', durable=true})) == 'string')
          local channel = mare.task.channel
          mare.task.channel = function(...)
            if mare.exec.current() then
              mare.task.channel = channel
              ready_tx:send(true)
              release_rx:recv()
            end
            return channel(...)
          end
          mare.task.spawn(function()
            local ok, value = pcall(mare.exec.run, {tool='probe', tool_call_id='probe'}, function()
              return S:add_record('generation-probe', {value='old'}, {persist=true})
            end)
            done_tx:send({ok=ok, value=value})
          end)
          ready_rx:recv()
          S:load({ path = before.path })
          local after = S:info()
          release_tx:send(true)
          local result = done_rx:recv()
          local err_text = type(result.value) == 'table' and tostring(result.value.message or result.value) or tostring(result.value)
          return before.session_id == after.session_id, before.session_epoch ~= after.session_epoch,
            result.ok, err_text, #S:list_records('generation-probe').data
        "#,
        )
        .await;
        assert!(same_id && generation_changed);
        assert!(!accepted && error.contains("originating session changed"), "{error}");
        assert_eq!(count, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn ordinary_writes_inside_finish_switch_window_are_rejected_without_completion_token() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("finish-window.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let h = env.host().faux(json!([{ "text": "ok" }])).build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (switching_in_window, rec_ok, rec_err, pm_ok, pm_err, ts_ok, ts_err, hook_ok): (
            bool,
            bool,
            String,
            bool,
            String,
            bool,
            String,
            bool,
        ) = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local hook_ok = false
            local switching_in_window = false
            local new_id, new_gen
            local off
            off = C.cordis:on("session/loaded", function()
                off()
                switching_in_window = S:switching()
                local info = S:info()
                new_id, new_gen = info.session_id, info.session_epoch
                -- Synchronous completion-task write owns the lease token and succeeds
                hook_ok = pcall(S.add_record, S, "completion/probe", { ok = true }, { persist = true, session_id = new_id, session_epoch = new_gen })
                entered_tx:send(true)
                release_rx:recv()
            end)
            local done_tx, done_rx = mare.task.channel()
            mare.task.spawn(function()
                entered_rx:recv()
                local r_ok, r_err = pcall(S.add_record, S, "ordinary/probe", { x = 1 }, { persist = true, session_id = new_id, session_epoch = new_gen })
                local p_ok, p_err = pcall(S.persist_message, S, {
                    message = { role = "user", content = { { type = "text", text = "intruder" } } },
                    session_id = new_id,
                    session_epoch = new_gen,
                })
                local t_ok, t_err = pcall(S.record_tool_state, S, {
                    tool_call_id = "intruder-ts",
                    name = "probe",
                    state = "prepared",
                    session_id = new_id,
                    session_epoch = new_gen,
                })
                release_tx:send(true)
                done_tx:send({
                    r_ok = r_ok == true,
                    r_err = type(r_err) == "table" and (tostring(r_err.code) .. ":" .. tostring(r_err.message)) or tostring(r_err),
                    p_ok = p_ok == true,
                    p_err = type(p_err) == "table" and (tostring(p_err.code) .. ":" .. tostring(p_err.message)) or tostring(p_err),
                    t_ok = t_ok == true,
                    t_err = type(t_err) == "table" and (tostring(t_err.code) .. ":" .. tostring(t_err.message)) or tostring(t_err),
                })
            end)
            S:load({ path = TARGET })
            local out = done_rx:recv()
            return switching_in_window, out.r_ok, out.r_err, out.p_ok, out.p_err, out.t_ok, out.t_err, hook_ok
        "#,
        )
        .await;
        assert!(switching_in_window);
        assert!(hook_ok, "completion task's own write inside finish_switch must succeed");
        assert!(!rec_ok && rec_err.contains("originating session changed"), "record rejected: {rec_err}");
        assert!(
            !pm_ok && pm_err.contains("stale_session") && pm_err.contains("originating session changed"),
            "persist_message rejected: {pm_err}"
        );
        assert!(
            !ts_ok && ts_err.contains("stale_session") && ts_err.contains("originating session changed"),
            "record_tool_state rejected: {ts_err}"
        );
        h.shutdown().await;
    })
    .await
}
