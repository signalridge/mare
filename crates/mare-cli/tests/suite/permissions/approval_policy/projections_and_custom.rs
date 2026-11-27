use super::*;

#[tokio::test]
async fn queued_follow_up_with_registered_value_stays_projected_after_steer_consume() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local secret = "QUEUED_FOLLOWUP_SECRET_99"
            KEEP = mare.projection.register(secret)
            local queues = {}
            C.cordis:on("queue/changed", function(q)
                queues[#queues + 1] = mare.json.encode(q)
            end)
            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get("policy"):voter("request/ready", function()
                if hold_tx then
                    local tx = hold_tx
                    hold_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = "deny", reason = "gate" }
                end
            end)
            local agent = C.cordis:get("agent")
            local held_turn = agent:prompt("hold")
            hold_rx:recv()
            agent:steer("steer message")
            agent:prompt("follow with " .. secret, { when_busy = "queue" })
            agent:cancel({ turn_id = held_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            agent:wait(held_turn.turn_id)
            -- Consume only steer via prompt/consume or check all emitted queue snapshots
            assert(#queues >= 2)
            for _, encoded in ipairs(queues) do
                assert(not encoded:find(secret, 1, true), "raw secret leaked in queue/changed: " .. encoded)
            end
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn prompt_content_and_busy_custom_follow_up_project_registered_values_in_public_events() {
    local(async {
        let env = TestEnv::new().await;
        // The model's reply and the tool's result both carry the registered value, so
        // item/started, item/completed (tool_call result + assistant_message) really have it as input.
        let script = json!([
            { "tool_calls": [{ "name": "pause_probe", "arguments": {} }] },
            { "text": "first done STRUCTURED_CUSTOM_SECRET_73" },
            { "text": "followup done" }
        ]);
        let h = env
            .host()
            .no_session()
            .config(json!({
                "model": "faux/echo",
                "plugins": { "provider-faux": { "script_json": script.to_string() } }
            }))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            local secret = "STRUCTURED_CUSTOM_SECRET_73"
            KEEP = mare.projection.register(secret)
            PUBLIC_PAYLOADS = {}
            CAPTURED_EVENTS = {}
            for _, ev_name in ipairs({ "turn/started", "queue/changed", "item/started", "item/completed", "turn/completed", "step/completed" }) do
                C.cordis:on(ev_name, function(payload)
                    CAPTURED_EVENTS[ev_name] = (CAPTURED_EVENTS[ev_name] or 0) + 1
                    PUBLIC_PAYLOADS[#PUBLIC_PAYLOADS + 1] = ev_name .. ":" .. mare.json.encode(payload)
                end)
            end
            local tool_entered_tx, tool_entered_rx = mare.task.channel()
            local tool_release_tx, tool_release_rx = mare.task.channel()
            C.cordis:get("tools"):register("pause_probe", {
                description = "pause probe",
                side_effect = "read",
                parameters = { type = "object" },
                execute = function()
                    tool_entered_tx:send(true)
                    tool_release_rx:recv()
                    return { content = "ok " .. secret }
                end,
            })
            local agent = C.cordis:get("agent")
            local turn = agent:prompt("ignored", {
                content = mare.json.array({ { type = "text", text = "structured " .. secret } }),
            })
            tool_entered_rx:recv()
            agent:prompt("follow", {
                when_busy = "queue",
                custom_message = {
                    role = "custom",
                    custom_type = "test/followup",
                    content = "custom " .. secret,
                    display = true,
                },
            })
            tool_release_tx:send(true)
            local res = agent:wait(turn.turn_id)
            assert(not res.error, "prompt failed: " .. tostring(res.error))
            for _, entry in ipairs(PUBLIC_PAYLOADS) do
                assert(not entry:find(secret, 1, true), "raw secret leaked in public event: " .. entry)
            end
            -- each event that carried the value shows its reference instead (not just "an event came")
            local function carried(prefix)
                for _, entry in ipairs(PUBLIC_PAYLOADS) do
                    if entry:sub(1, #prefix) == prefix and entry:find(KEEP.reference, 1, true) then
                        return true
                    end
                end
                return false
            end
            local tool_result_ref, assistant_msg_ref = false, false
            for _, entry in ipairs(PUBLIC_PAYLOADS) do
                if entry:sub(1, 15) == "item/completed:" then
                    local ev = mare.json.decode(entry:sub(16))
                    local item = ev and ev.item or {}
                    if item.type == "tool_call" and mare.json.encode(item.message or {}):find(KEEP.reference, 1, true) then tool_result_ref = true end
                    if item.type == "assistant_message" and mare.json.encode(item.message or {}):find(KEEP.reference, 1, true) then assistant_msg_ref = true end
                end
            end
            assert(tool_result_ref, "item/completed tool_call result must carry the reference")
            assert(assistant_msg_ref, "item/completed assistant_message must carry the reference")
            assert(carried("item/started:"), "item/started must carry the reference")
            assert(carried("item/completed:"), "item/completed must carry the reference")
        "#,
        )
        .await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_query_projects_registered_values_before_chunking() {
    local(async {
        let env = TestEnv::new().await;
        let secret = "C7_HISTORY_SECRET_42";
        let canonical = dunce::canonicalize(&env.cwd).unwrap_or_else(|_| env.cwd.clone());
        let sdir = mare_shared::paths::sessions_dir().join(mare_store::cwd_slug(&canonical));
        std::fs::create_dir_all(&sdir).unwrap();
        let spath = sdir.join("2026-01-01T00-00-00Z_proj-test.jsonl");
        let lines = [
            json!({"type":"session","version":mare_store::CURRENT_VERSION,"session_id":"proj-test","cwd":env.cwd,"created_at_ms":1000}),
            json!({"type":"message","entry_id":"m1","created_at_ms":1010,"message":{"role":"user","content":format!("remember {secret}")}}),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        std::fs::write(&spath, lines).unwrap();
        let h = env.host().build().await;
        expose(&h);
        exec(&h, r#"
            local secret = "C7_HISTORY_SECRET_42"
            KEEP = mare.projection.register(secret)
            C.cordis:get("tools"):register("query_probe", {
                description = "query probe",
                side_effect = "read",
                parameters = { type = "object" },
                execute = function()
                    local hits = mare.store.search_events({ session_id = "proj-test", query = "remember" })
                    assert(#hits.data >= 1, "expected search hit")
                    local event_id = hits.data[1].event_id
                    local snippet = hits.data[1].snippet or ""
                    assert(not snippet:find(secret, 1, true), "secret leaked in search_events snippet: " .. snippet)
                    local chunked = mare.store.read_event({ event_id = event_id, offset = 0, max_bytes = 256 })
                    assert(not chunked.chunk:find(secret, 1, true), "secret leaked in read_event chunk: " .. chunked.chunk)
                    local traced = mare.store.trace_event({ event_id = event_id })
                    local traced_json = mare.json.encode(traced)
                    assert(not traced_json:find(secret, 1, true), "secret leaked in trace_event: " .. traced_json)
                    return { content = "ok" }
                end
            })
            local res = C.cordis:get("tools"):execute({ id = "q-1", name = "query_probe", arguments = {} }, { on_state = function() return true end })
            assert(not res.is_error and res.content == "ok", mare.json.encode(res))
        "#).await;
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn custom_opts_approve_allow_still_runs_tool_approval_voters_and_denies() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"full-access"}), json!({})).await;
        exec(&h, SETUP).await;
        let (refused, is_error, exec_count, custom_count, post_voter_count, result_decision, audit_decision): (
            bool,
            bool,
            i64,
            i64,
            i64,
            String,
            String,
        ) = eval(
            &h,
            r#"
            local custom_ran = false
            local post_votes = 0
            local result_decision = ""
            C.cordis:on("pipeline/approval-decided", function(ev)
                if ev.call and ev.call.id == "custom-deny-1" then
                    result_decision = ev.decision
                end
            end)
            -- Voter 1 allows, Voter 2 denies once custom opts.approve has answered.
            C.cordis:get("policy"):voter("tool/approval", function()
                return { decision = "allow", source = "allow-voter" }
            end)
            C.cordis:get("policy"):voter("tool/approval", function()
                if custom_ran then
                    post_votes = post_votes + 1
                    return { decision = "deny", reason = "denied by post-approve voter", source = "deny-voter" }
                end
                return { decision = "ask" }
            end)
            local res = EXECUTE({ id = "custom-deny-1", name = "effect", arguments = {} }, {
                approve = function()
                    answered = answered + 1
                    custom_ran = true
                    return { decision = "allow", source = "user" }
                end,
            })
            local decided = S:list_records("approval/resolved").data
            local last_decided = decided[#decided]
            return res.refused == true,
                res.is_error == true,
                ran,
                answered,
                post_votes,
                result_decision,
                last_decided and last_decided.result and last_decided.result.decision or ""
            "#,
        )
        .await;
        assert!(refused, "tool call must be refused");
        assert!(is_error, "tool call must return error");
        assert_eq!(exec_count, 0, "tool must NOT execute when tool/approval voter denies");
        assert_eq!(custom_count, 1, "custom opts.approve must run once");
        assert_eq!(post_voter_count, 1, "tool/approval voter must run after custom opts.approve");
        assert_eq!(result_decision, "deny");
        assert_eq!(audit_decision, "deny");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn custom_approve_contradictory_allow_and_abort_is_rejected() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        exec(
            &h,
            r#"
            RAN = 0
            C.cordis:get("policy"):voter("tool/before", function() return { decision = "ask", reason = "approval required" } end)
            C.cordis:get("tools"):register("effect_contradict", {
                description = "test",
                side_effect = "external",
                parameters = { type = "object", properties = {} },
                execute = function() RAN = RAN + 1; return { content = "ran" } end,
            })
            "#,
        )
        .await;
        let (refused, is_error, ran): (bool, bool, i64) = eval(
            &h,
            r#"
            local res = C.cordis:get("tools"):execute({ id = "contra-1", name = "effect_contradict", arguments = {} }, {
                approve = function()
                    return { decision = "allow", abort = true, source = "user" }
                end,
            })
            return res.refused == true, res.is_error == true, RAN
            "#,
        )
        .await;
        assert!(refused && is_error, "contradictory allow+abort must be refused");
        assert_eq!(ran, 0, "tool must not execute on contradictory allow+abort");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn duplicate_call_id_never_cross_caches_session_approval() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (a_err, b_err, third_ran): (bool, bool, i64) = eval(
            &h,
            r#"
            RAN_B = 0
            local entered_a_tx, entered_a_rx = mare.task.channel()
            local release_a_tx, release_a_rx = mare.task.channel()
            local started_b_tx, started_b_rx = mare.task.channel()
            local entered_b_tx, entered_b_rx = mare.task.channel()
            local release_b_tx, release_b_rx = mare.task.channel()
            local res_a, res_b
            local tools = C.cordis:get("tools")
            C.cordis:get("policy"):voter("tool/before", function(call)
                if call.arguments and call.arguments.path == "dup-b.txt" then
                    started_b_tx:send(true)
                end
            end)
            local f1 = mare.task.spawn(function()
                res_a = tools:execute({ id = "dup-1", name = "write", arguments = { path = "dup-a.txt", content = "a" } }, {
                    approve = function()
                        entered_a_tx:send(true)
                        release_a_rx:recv()
                        return { decision = "allow_session", source = "user" }
                    end,
                })
            end)
            entered_a_rx:recv()
            local f2 = mare.task.spawn(function()
                res_b = tools:execute({ id = "dup-1", name = "write", arguments = { path = "dup-b.txt", content = "b" } }, {
                    approve = function()
                        entered_b_tx:send(true)
                        release_b_rx:recv()
                        return { decision = "deny", reason = "denied B", source = "user" }
                    end,
                })
            end)
            started_b_rx:recv()
            release_a_tx:send(true)
            entered_b_rx:recv()
            release_b_tx:send(true)
            f1:wait()
            f2:wait()
            local res_b2 = tools:execute({ id = "dup-2", name = "write", arguments = { path = "dup-b.txt", content = "b2" } })
            return res_a.is_error == true, res_b.is_error == true, res_b2.is_error and 0 or 1
            "#,
        )
        .await;
        assert!(!a_err, "A should be allowed");
        assert!(b_err, "B should be denied");
        assert_eq!(third_ran, 0, "B must not be session-cached by A sharing call.id");
        assert!(env.cwd.join("dup-a.txt").exists());
        assert!(!env.cwd.join("dup-b.txt").exists());
        h.shutdown().await;
    })
    .await
}
