use super::*;

#[tokio::test]
async fn completion_notification_failure_still_delivers_commit_events() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "completion-notification-error.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"agent/queue", "provenance":"session-host", "data":{"follow_up":[{"input_id":"queued-id","text":"queued"}]}}),
            ],
        );
        let h = env.host().build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, active, switching, events): (bool, String, String, bool, String) = eval(
            &h,
            r#"
            EVENTS = {}
            C.cordis:on('agent/messages-replaced', function() EVENTS[#EVENTS + 1] = 'messages' end)
            C.cordis:on('session/changed', function(e) if e.reason == 'resume' then EVENTS[#EVENTS + 1] = 'changed' end end)
            C.cordis:on('session/loaded', function() EVENTS[#EVENTS + 1] = 'loaded' end)
            local notify = mare.ui.notify
            mare.ui.notify = function() error('injected notification failure') end
            local ok, err = pcall(S.load, S, { path = TARGET })
            mare.ui.notify = notify
            return ok, tostring(err), S:info().path, S:switching(), table.concat(EVENTS, ',')
        "#,
        )
        .await;
        assert!(!ok && err.contains("session committed") && err.contains("injected notification failure"), "{err}");
        assert_eq!(active, path.to_string_lossy());
        assert!(!switching);
        assert_eq!(events, "messages,changed,loaded");
        assert_eq!(eval::<String>(&h, "return A:list_queue().data[1].text").await, "queued");
        h.shutdown().await;
    })
    .await
}

async fn assert_partial_adoption_poisoned(mode: &str) {
    let env = TestEnv::new().await;
    let path = env.write_session("agent-partial-adoption.jsonl", &[msg("m1", None, "user", "TARGET")]);
    let probe = env.write_session("agent-write-probe.jsonl", &[msg("p1", None, "user", "PROBE")]);
    let target_before = std::fs::read(&path).unwrap();
    let probe_before = std::fs::read(&probe).unwrap();
    let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
    expose(&h);
    exec(
        &h,
        r#"
        TURN_STARTED = false
        C.cordis:on('step/started', function() TURN_STARTED = true end, {prepend=true})
    "#,
    )
    .await;
    h.prompt("BEFORE", json!({})).await.unwrap();
    assert!(
        eval::<bool>(&h, "return TURN_STARTED").await,
        "positive control: step/started must fire on a healthy turn"
    );
    let old_path: String = eval(&h, "return S:info().path").await;
    let old_before = std::fs::read(&old_path).unwrap();
    global(&h, "TARGET", path.to_string_lossy().to_string());
    global(&h, "PROBE", probe.to_string_lossy().to_string());
    global(&h, "ADOPTION_FAILURE", mode);
    let (ok, err, session_before, session_after, agent_before, agent_after, messages_before, messages_after): (
        bool,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = eval(
        &h,
        r#"
        J = mare.store.open(PROBE, {existing=true, auxiliary=true})
        local session_before, agent_before = S:info().session_id, A:get().session_id
        local messages_before = mare.json.encode(A:list_messages().data)
        local original = A.adopt_session
        A.adopt_session = function(self, snapshot)
            A:set_session_id(snapshot.session_id)
            A:set_messages(snapshot.messages)
            if ADOPTION_FAILURE == 'yield' then mare.task.sleep(100) end
            if ADOPTION_FAILURE == 'nil' then return nil, 'injected adoption failure' end
            if ADOPTION_FAILURE == 'false' then return false, 'injected adoption failure' end
            error('injected adoption failure')
        end
        local ok, err = pcall(S.load, S, { path = TARGET })
        A.adopt_session = original
        return ok, tostring(err), session_before, S:info().session_id,
            agent_before, A:get().session_id, messages_before, mare.json.encode(A:list_messages().data)
    "#,
    )
    .await;
    assert!(!ok, "{err}");
    assert_eq!(session_after, session_before, "{err}");
    assert_ne!(agent_after, agent_before, "the defective provider really changed agent identity");
    assert_ne!(messages_after, messages_before, "the defective provider really changed context");
    let (entries_before, records_before): (String, String) =
        eval(&h, "return mare.json.encode(S:entries()), mare.json.encode(S:list_records('bad').data)").await;
    let (switching, prompt_err, lease_err, exec_ok, append_ok, open_ok, http_ok, record_ok, proxy_ok, event_error):
        (bool, String, String, bool, bool, bool, bool, bool, bool, String) = eval(&h, r#"
        local request_lease = function(...) return C.cordis:get('kernel'):request_lease(...) end
        INPUT_ESCAPED = false
        TURN_STARTED = false
        C.cordis:on('input/submit', function() INPUT_ESCAPED = true end, {prepend=true})
        -- the property: the poisoned session's provider is not executed (whoever looks the method
        -- up, a proxied call is refused with the barrier's error before the provider runs)
        local proxy = C.cordis:get('agent')
        local before = mare.json.encode(A:list_messages().data)
        local proxy_ok, proxy_err = pcall(function() return proxy:prompt('must not run') end)
        assert(tostring(proxy_err):find('restart', 1, true), 'not the barrier: ' .. tostring(proxy_err))
        assert(mare.json.encode(A:list_messages().data) == before, 'the provider ran: the agent context changed')
        local event = { error = proxy_err }
        -- the published agent refuses at its call boundary (raised, the barrier's error)
        local prompt_ok, prompt_raised = pcall(A.prompt, A, 'must not run')
        local prompt = prompt_ok and prompt_raised or { error = prompt_raised }
        local lease, lease_err = request_lease()
        local exec_ok = pcall(mare.exec.run, {tool_call_id='bad'}, function() error('executed') end)
        local append_ok = pcall(J.append, J, {type='custom', kind='bad'})
        local open_ok = pcall(mare.store.open, PROBE, {existing=true, auxiliary=true})
        local http_ok = pcall(mare.http.request, {url='http://127.0.0.1:1'})
        local record_called, record_result = pcall(S.add_record, S, 'bad', {})
        local record_ok = record_called and record_result ~= nil
        -- ending an uncommitted operation claim stays possible (the barrier allows it; the
        -- implementation checks the claim: an unknown one is false, not a refusal)
        local released_ok, released = pcall(S.release_operation, S, { claim_id = 'unknown' }, 'probe')
        assert(released_ok and released == false, 'release_operation refused while poisoned: ' .. tostring(released))
        return S:switching(), tostring(prompt.error), tostring(lease_err), exec_ok, append_ok, open_ok, http_ok, record_ok == true, proxy_ok, tostring(event and event.error)
    "#).await;
    assert!(switching && prompt_err.contains("restart") && lease_err.contains("restart"), "{prompt_err}; {lease_err}");
    assert!(!exec_ok && !append_ok && !open_ok && !http_ok && !record_ok && !proxy_ok);
    assert!(event_error.contains("restart"), "{event_error}");
    let (checkpoint_ok, diagnostics_ok): (bool, bool) = eval(
        &h,
        r#"
        local checkpoint_ok = pcall(J.checkpoint, J)
        local rows, total = mare.store.index_read({scope='all', kind='any', archived='any'})
        return checkpoint_ok, type(rows) == 'table' and type(total) == 'number'
    "#,
    )
    .await;
    assert!(!checkpoint_ok && diagnostics_ok);
    let (result, reason, unchanged): (bool, String, bool) = eval(
        &h,
        r#"
        local before = mare.json.encode(mare.proc.sandbox.current())
        -- refused at the published session's call boundary (raised) or by the method (nil, reason)
        local called, result, reason = pcall(S.set_sandbox_mode, S, 'read-only')
        if not called then
            result, reason = nil, result
        end
        return result ~= nil, tostring(reason), mare.json.encode(mare.proc.sandbox.current()) == before
    "#,
    )
    .await;
    assert!(!result && reason.contains("restart") && unchanged, "{reason}");
    h.submit("MUST-NOT-RUN").await;
    assert!(!eval::<bool>(&h, "return INPUT_ESCAPED").await);
    assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
    assert!(h.continue_turn().await.is_err());
    assert_eq!(eval::<String>(&h, "return mare.json.encode(A:list_messages().data)").await, messages_after);
    assert!(!eval::<bool>(&h, "return A:get().busy").await);
    assert!(!eval::<bool>(&h, "return TURN_STARTED").await);
    let (entries_after, records_after): (String, String) =
        eval(&h, "return mare.json.encode(S:entries()), mare.json.encode(S:list_records('bad').data)").await;
    assert_eq!(entries_after, entries_before);
    assert_eq!(records_after, records_before);
    assert_eq!(std::fs::read(&old_path).unwrap(), old_before);
    assert_eq!(std::fs::read(&path).unwrap(), target_before);
    assert_eq!(std::fs::read(&probe).unwrap(), probe_before);
    exec(&h, "J:close()").await;
    h.shutdown().await;
    let fresh = env.host().faux(json!([{ "text": "RECOVERED" }])).build().await;
    assert_eq!(fresh.prompt("AFTER-RESTART", json!({})).await.unwrap()["text"], "RECOVERED");
    fresh.shutdown().await;
}

#[tokio::test]
async fn failing_agent_adoption_poison_barrier_contains_mixed_context() {
    local(assert_partial_adoption_poisoned("throw")).await
}

#[tokio::test]
async fn yielding_agent_adoption_poison_barrier_contains_mixed_context() {
    local(assert_partial_adoption_poisoned("yield")).await
}

#[tokio::test]
async fn reported_agent_adoption_failure_poison_barrier_contains_mixed_context() {
    local(async {
        assert_partial_adoption_poisoned("nil").await;
        assert_partial_adoption_poisoned("false").await;
    })
    .await
}

#[tokio::test]
async fn reported_agent_revision_errors_poison_before_publication() {
    local(async {
        for method in ["session_revision", "check_session_revision"] {
            for mode in ["nil", "false"] {
                let env = TestEnv::new().await;
                let path = env.write_session("revision-error.jsonl", &[msg("m1", None, "user", "TARGET")]);
                let before = std::fs::read(&path).unwrap();
                let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
                expose(&h);
                h.prompt("BEFORE", json!({})).await.unwrap();
                global(&h, "TARGET", path.to_string_lossy().to_string());
                global(&h, "METHOD", method);
                global(&h, "MODE", mode);
                let (ok, error, switching): (bool, String, bool) = eval(
                    &h,
                    r#"
                    local original = A[METHOD]
                    A[METHOD] = function()
                        if MODE == 'nil' then return nil, 'revision refused' end
                        return false, 'revision refused'
                    end
                    local ok, err = pcall(S.load, S, { path = TARGET })
                    A[METHOD] = original
                    return ok, tostring(err), S:switching()
                "#,
                )
                .await;
                assert!(!ok && error.contains("revision refused") && switching, "{method}/{mode}: {error}");
                assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
                h.shutdown().await;
                assert_eq!(std::fs::read(&path).unwrap(), before);
            }
        }
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn completion_spawn_failure_poison_barrier_blocks_new_work() {
    local(async {
        for after_registration in [false, true] {
            let env = TestEnv::new().await;
            let path = env.write_session("completion-spawn-failure.jsonl", &[msg("m1", None, "user", "TARGET")]);
            let original = std::fs::read(&path).unwrap();
            let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
            expose(&h);
            h.prompt("BEFORE", json!({})).await.unwrap();
            global(&h, "TARGET", path.to_string_lossy().to_string());
            exec(
                &h,
                r#"
            EVENTS = {}
            C.cordis:on('agent/messages-replaced', function() EVENTS[#EVENTS + 1] = 'messages' end)
            C.cordis:on('session/changed', function(e) if e.reason == 'resume' then EVENTS[#EVENTS + 1] = 'changed' end end)
            C.cordis:on('session/loaded', function() EVENTS[#EVENTS + 1] = 'loaded' end)
        "#,
            )
            .await;
            if after_registration {
                mare_binding::exec_gate::fail_next_session_completion_wrap(h.lua());
            } else {
                mare_binding::exec_gate::fail_next_session_completion(h.lua());
            }
            let (ok, err, active, switching, events, lease_ok, exec_ok): (
                bool,
                String,
                String,
                bool,
                String,
                bool,
                bool,
            ) = eval(
                &h,
                r#"
            local request_lease = function(...) return C.cordis:get('kernel'):request_lease(...) end
            local ok, err = pcall(S.load, S, { path = TARGET })
            local lease = request_lease()
            local exec_ok = pcall(mare.exec.run, {tool_call_id='bad'}, function() error('executed') end)
            return ok, tostring(err), S:info().path, S:switching(), table.concat(EVENTS, ','), lease ~= nil, exec_ok
        "#,
            )
            .await;
            let expected = if after_registration { "task handle allocation failure" } else { "spawn failure" };
            assert!(!ok && err.contains("session committed") && err.contains(expected), "{err}");
            assert_eq!(active, path.to_string_lossy());
            assert!(switching && !lease_ok && !exec_ok);
            tokio::task::yield_now().await;
            assert!(events.is_empty(), "completion did not start: {events}");
            assert!(eval::<String>(&h, "return table.concat(EVENTS, ',')").await.is_empty());
            assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
            assert_eq!(std::fs::read(&path).unwrap(), original);
            h.shutdown().await;
            assert_eq!(std::fs::read(&path).unwrap(), original);
        }
    })
    .await
}

#[tokio::test]
async fn disposing_session_during_completion_poison_barrier_survives_reload() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("completion-disposed.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
        expose(&h);
        h.prompt("BEFORE", json!({})).await.unwrap();
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let child_path: String = eval(&h, "local c = S:child({run_id='held', seq=1}); return c.path").await;
        let child_before = std::fs::read(&child_path).unwrap();
        exec(
            &h,
            r#"
            COMPLETION_WAITING = false
            C.cordis:on('agent/messages-replaced', function()
                COMPLETION_WAITING = true
                mare.task.sleep(10000)
            end, {prepend=true})
        "#,
        )
        .await;
        let loader = h.clone();
        let loading = tokio::task::spawn_local(async move {
            eval::<(bool, String)>(&loader, "local ok, e = pcall(S.load, S, { path = TARGET }); return ok, tostring(e)")
                .await
        });
        wait_until(5, "completion listener", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return COMPLETION_WAITING").await }
        })
        .await;
        h.kernel.reload(Some("session")).await.unwrap();
        let (ok, error) = tokio::time::timeout(std::time::Duration::from_secs(5), loading).await.unwrap().unwrap();
        assert!(!ok && error.contains("interrupted"), "{error}");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        assert_eq!(std::fs::read(child_path).unwrap(), child_before, "child disposal must not write a clean marker");
        let after = std::fs::read(&path).unwrap();
        h.shutdown().await;
        assert_eq!(std::fs::read(&path).unwrap(), after);
    })
    .await
}

#[tokio::test]
async fn pre_mutation_sandbox_restore_failure_falls_back_and_keeps_the_session_usable() {
    local(async {
        for mode in ["throw", "nil", "false"] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "pre-mutation-sandbox-error.jsonl",
                &[
                    msg("m1", None, "user", "TARGET"),
                    json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write"}}),
                ],
            );
            let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "FAIL_MODE", mode);
            let (ok, err, active, switching, events, level): (bool, String, String, bool, String, String) = eval(&h, r#"
                EVENTS = {}
                C.cordis:on('agent/messages-replaced', function() EVENTS[#EVENTS + 1] = 'messages' end)
                C.cordis:on('session/changed', function(e) if e.reason == 'resume' then EVENTS[#EVENTS + 1] = 'changed' end end)
                C.cordis:on('session/loaded', function() EVENTS[#EVENTS + 1] = 'loaded' end)
                mare.proc.sandbox.set_current({level='read-only'})
                local sandbox = mare.proc.sandbox.set_current
                local function fail()
                    local message = 'injected sandbox failure'
                    if FAIL_MODE == 'nil' then return nil, message end
                    if FAIL_MODE == 'false' then return false, message end
                    error(message)
                end
                mare.proc.sandbox.set_current = function(spec)
                    if spec and spec.level == 'workspace-write' then return fail() end
                    return sandbox(spec)
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                mare.proc.sandbox.set_current = sandbox
                return ok, tostring(err), S:info().path, S:switching(), table.concat(EVENTS, ','), mare.proc.sandbox.current().level
            "#).await;
            assert!(ok && !switching, "{mode}: {err}");
            assert_eq!(active, path.to_string_lossy());
            assert_eq!(events, "messages,changed,loaded");
            assert_eq!(level, "read-only");
            assert!(notices(&h).await.contains("injected sandbox failure"));
            assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn double_pre_mutation_sandbox_failure_reestablishes_the_known_default() {
    local(async {
        for (default_is_current, mode) in
            [(true, "throw"), (true, "nil"), (true, "false"), (false, "throw"), (false, "nil"), (false, "false")]
        {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "double-pre-mutation-sandbox-error.jsonl",
                &[
                    msg("m1", None, "user", "TARGET"),
                    json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write"}}),
                ],
            );
            let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "FAIL_MODE", mode);
            global(&h, "DEFAULT_IS_CURRENT", default_is_current);
            let (ok, err, switching, level): (bool, String, bool, String) = eval(
                &h,
                r#"
                mare.proc.sandbox.set_current({level='read-only'})
                if not DEFAULT_IS_CURRENT then S:set_sandbox_mode('workspace-write') end
                local setter = mare.proc.sandbox.set_current
                local function fail()
                    if FAIL_MODE == 'nil' then return nil, 'injected failure before mutation' end
                    if FAIL_MODE == 'false' then return false, 'injected failure before mutation' end
                    error('injected failure before mutation')
                end
                mare.proc.sandbox.set_current = function(spec)
                    if spec then return fail() end
                    return setter(spec)
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                mare.proc.sandbox.set_current = setter
                local p = mare.proc.sandbox.current()
                return ok, tostring(err), S:switching(), p and p.level or ''
            "#,
            )
            .await;
            assert!(ok && !switching, "default_current={default_is_current}, {mode}: {err}");
            assert_eq!(level, "read-only", "the safe baseline was reasserted");
            let notices = notices(&h).await;
            assert!(
                notices.contains("captured host PolicyUd snapshot"),
                "the host snapshot revalidates the baseline even when it equals the current policy: {notices}"
            );
            assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn fallback_setter_errors_after_policy_mutation_keep_the_poison_barrier() {
    local(async {
        for mode in ["throw", "nil", "false"] {
            let env = TestEnv::new().await;
            let path = env.write_session(
                "fallback-sandbox-after-mutation.jsonl",
                &[
                    msg("m1", None, "user", "TARGET"),
                    json!({"type":"custom", "kind":"sandbox/mode", "provenance":"session-host", "data":{"level":"workspace-write"}}),
                ],
            );
            let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "FAIL_MODE", mode);
            let (ok, err, switching, before, after): (bool, String, bool, String, String) = eval(
                &h,
                r#"
                mare.proc.sandbox.set_current({level='read-only'})
                S:set_sandbox_mode('workspace-write')
                local before = mare.proc.sandbox.current().level
                local setter, calls = mare.proc.sandbox.set_current, 0
                mare.proc.sandbox.set_current = function(spec)
                    calls = calls + 1
                    if calls == 1 then error('target failed before mutation') end
                    local result = table.pack(setter(spec))
                    if FAIL_MODE == 'nil' then return nil, 'fallback failed after mutation' end
                    if FAIL_MODE == 'false' then return false, 'fallback failed after mutation' end
                    error('fallback failed after mutation')
                    return table.unpack(result, 1, result.n)
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                mare.proc.sandbox.set_current = setter
                return ok, tostring(err), S:switching(), before, mare.proc.sandbox.current().level
            "#,
            )
            .await;
            assert!(
                !ok && switching && err.contains("default policy restore failed after changing policy"),
                "{mode}: {err}"
            );
            assert_eq!(before, "workspace-write");
            assert_eq!(after, "read-only", "fallback actually changed the policy before failing");
            assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
            h.shutdown().await;
        }
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn failed_running_marker_poison_barrier_blocks_turns() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session("running-marker-failure.jsonl", &[msg("m1", None, "user", "TARGET")]);
        let before = std::fs::read(&path).unwrap();
        let h = env.host().build().await;
        expose(&h);
        global(&h, "TARGET", path.to_string_lossy().to_string());
        mare_binding::exec_gate::fail_session_journal_ops(h.lua(), 0, true, false);
        let (ok, error, switching): (bool, String, bool) = eval(
            &h,
            r#"
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:switching()
        "#,
        )
        .await;
        assert!(!ok && error.contains("injected journal append failure") && switching, "{error}");
        assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
        h.shutdown().await;
        assert_eq!(std::fs::read(&path).unwrap(), before);
    })
    .await
}

#[cfg(debug_assertions)]
#[tokio::test]
async fn journal_transition_failures_poison_and_refuse_further_work() {
    local(async {
        for (activations, append, close) in [(1, false, false), (2, false, false), (0, true, false), (0, false, true)] {
            let env = TestEnv::new().await;
            let path = env.write_session("journal-failure.jsonl", &[msg("m1", None, "user", "TARGET")]);
            let h = env.host().faux(json!([{ "text": "OLD" }])).build().await;
            expose(&h);
            h.prompt("BEFORE", json!({})).await.unwrap();
            global(&h, "TARGET", path.to_string_lossy().to_string());
            mare_binding::exec_gate::fail_session_journal_ops(h.lua(), activations, append, close);
            let (ok, err, switching): (bool, String, bool) = eval(
                &h,
                r#"
                local ok, err = pcall(S.load, S, { path = TARGET })
                return ok, tostring(err), S:switching()
            "#,
            )
            .await;
            let expected = if activations > 0 {
                "activation"
            } else if append {
                "close marker"
            } else {
                "close failed"
            };
            assert!(!ok && err.contains(expected), "{err}");
            if activations == 2 {
                assert!(err.contains("previous journal restore failed"), "{err}");
            }
            assert!(switching);
            assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
            let after = std::fs::read(&path).unwrap();
            h.shutdown().await;
            assert_eq!(std::fs::read(&path).unwrap(), after, "shutdown must not append to the target");
        }
    })
    .await
}

#[tokio::test]
async fn reported_request_recovery_failure_blocks_turns_and_preserves_events() {
    local(async {
        for mode in ["nil", "false"] {
            let env = TestEnv::new().await;
            let path = env.write_session("reported-recovery-failure.jsonl", &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"custom", "kind":"agent/request", "data":{"request_id":"r1", "attempt":0}}),
            ]);
            let h = env.host().build().await;
            expose(&h);
            global(&h, "TARGET", path.to_string_lossy().to_string());
            global(&h, "MODE", mode);
            let (ok, err, switching, events): (bool, String, bool, String) = eval(&h, r#"
                EVENTS = {}
                C.cordis:on('agent/messages-replaced', function() EVENTS[#EVENTS + 1] = 'messages' end)
                C.cordis:on('session/changed', function(e) if e.reason == 'resume' then EVENTS[#EVENTS + 1] = 'changed' end end)
                C.cordis:on('session/loaded', function() EVENTS[#EVENTS + 1] = 'loaded' end)
                local original = A.set_pending_request
                A.set_pending_request = function()
                    if MODE == 'nil' then return nil, 'recovery refused' end
                    return false, 'recovery refused'
                end
                local ok, err = pcall(S.load, S, { path = TARGET })
                A.set_pending_request = original
                return ok, tostring(err), S:switching(), table.concat(EVENTS, ',')
            "#).await;
            assert!(!ok && err.contains("recovery refused"), "{mode}: {err}");
            assert!(switching);
            assert_eq!(events, "messages,changed,loaded");
            assert!(h.prompt("MUST-NOT-RUN", json!({})).await.is_err());
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn unavailable_resume_settings_leave_a_usable_session() {
    local(async {
        for record in [
            json!({"type":"model", "model":"missing-provider/model", "effort":"invalid", "at_entry_id":"m1"}),
            json!({"type":"model", "model":"faux/echo", "effort":"invalid", "at_entry_id":"m1"}),
        ] {
            let env = TestEnv::new().await;
            let path = env.write_session("unavailable-settings.jsonl", &[msg("m1", None, "user", "TARGET"), record]);
            let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
            expose(&h);
            fake_ui(&h).await;
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "TARGET", path.to_string_lossy().to_string());
            let default_effort: Option<String> = eval(&h, "return L:raw_effort()").await;
            exec(&h, r#"L:set_effort("high", {save=false}); L:set_model("faux/echo2", {save=false})"#).await;
            let (ok, err, switching): (bool, String, bool) = eval(
                &h,
                r#"
                local ok, err = pcall(S.load, S, { path = TARGET })
                return ok, tostring(err), S:switching()
            "#,
            )
            .await;
            assert!(ok && !switching, "{err}");
            assert_eq!(eval::<String>(&h, "return L:current().ref").await, "faux/echo");
            assert!(notices(&h).await.contains("not restored"));
            assert_eq!(eval::<Option<String>>(&h, "return L:raw_effort()").await, default_effort);
            let entries = std::fs::read_to_string(&path).unwrap();
            assert_eq!(entries.lines().filter(|l| l.contains("\"type\":\"model\"")).count(), 1);
            assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn failed_restore_warning_notification_does_not_poison_the_session() {
    local(async {
        let env = TestEnv::new().await;
        let path = env.write_session(
            "notification-failure.jsonl",
            &[
                msg("m1", None, "user", "TARGET"),
                json!({"type":"model", "model":"missing-provider/model", "at_entry_id":"m1"}),
            ],
        );
        let h = env.host().faux(json!([{ "text": "OK" }])).build().await;
        expose(&h);
        fake_ui(&h).await;
        global(&h, "TARGET", path.to_string_lossy().to_string());
        let (ok, err, switching): (bool, String, bool) = eval(
            &h,
            r#"
            local notify = ui.notify
            ui.notify = function(note, ...)
                if tostring(note):find('session model not restored', 1, true) then
                    error('warning notification unavailable')
                end
                return notify(note, ...)
            end
            local ok, err = pcall(S.load, S, { path = TARGET })
            return ok, tostring(err), S:switching()
        "#,
        )
        .await;
        assert!(ok && !switching, "{err}");
        assert_eq!(h.prompt("NEXT", json!({})).await.unwrap()["text"], "OK");
        h.shutdown().await;
    })
    .await
}

/// Auto-resume timer spawned by `agent:set_pending_request` must be bound to the session and
/// request identity: switching from interrupted session X (2s delay) to interrupted session Y
/// (8s delay) within 2s must NOT let X's timer prematurely continue Y.
#[tokio::test]
async fn auto_resume_timer_does_not_fire_after_switching_to_another_interrupted_session() {
    local(async {
        let env = TestEnv::new().await;
        let path_x = env.write_session(
            "sess-x.jsonl",
            &[
                msg("mx1", None, "user", "PROMPT-X"),
                json!({"type":"custom", "kind":"agent/request", "data":{"request_id":"req-x", "attempt":0}}),
            ],
        );
        let path_y = env.write_session(
            "sess-y.jsonl",
            &[
                msg("my1", None, "user", "PROMPT-Y"),
                json!({"type":"custom", "kind":"agent/request", "data":{"request_id":"req-y", "attempt":2}}),
            ],
        );
        let h = env
            .host()
            .faux(json!([{ "text": "RESUMED" }]))
            .config(json!({ "plugins": { "agent": { "request_retry_base_ms": 250, "request_retry_max_ms": 5000 } } }))
            .build()
            .await;
        expose(&h);
        global(&h, "PATH_X", path_x.to_string_lossy().to_string());
        global(&h, "PATH_Y", path_y.to_string_lossy().to_string());

        let (premature_turns, y_pending_req): (i64, String) = eval(
            &h,
            r#"local orig_mode = mare.mode
               mare.mode = "tui"
               local started = 0
               C.cordis:on("turn/started", function() started = started + 1 end)
               local ok, err = pcall(function()
                 -- Load X (attempt 0 -> clean=false -> attempt 1 -> 250ms auto-resume delay),
                 -- then quickly switch to Y (attempt 2 -> attempt 3 -> 1000ms delay).
                 S:load({ path = PATH_X })
                 mare.task.sleep(50)
                 S:load({ path = PATH_Y })
                 -- Wait past X's 250ms timer, well before Y's 1000ms timer.
                 mare.task.sleep(400)
               end)
               mare.mode = orig_mode
               assert(ok, tostring(err))
               local pr = A:pending_request()
               return started, pr and pr.request_id or "none""#,
        )
        .await;
        assert_eq!(premature_turns, 0, "session X's 250ms timer must not prematurely continue session Y");
        assert_eq!(y_pending_req, "req-y", "session Y's pending request should still be waiting for its own backoff");
        h.shutdown().await;
    })
    .await
}
