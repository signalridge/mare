//! Item 17b (`agent:capabilities().input_admission = 1`, `agent/input-ready`) and
//! Item 16 (`agent:enqueue`, `agent:resume_input`) behavioral & crash-recovery tests.

use crate::common::*;
use serde_json::json;

async fn eval_json(h: &mare_host::Host, body: &str) -> serde_json::Value {
    let encoded: String = eval(h, &format!("return mare.json.encode((function() {body} end)())")).await;
    serde_json::from_str(&encoded).unwrap()
}

#[tokio::test]
async fn agent_input_ready_direct_denial_retry_abort_and_step_correlation() {
    local(async {
        let env = TestEnv::new().await;
        let plugins_dir = env.home.path().join("config/plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::write(
            plugins_dir.join("hooks_probe.lua"),
            r#"
            return {
                name = "hooks-probe",
                inject = { "agent", "session", "policy" },
                apply = function(ctx)
                    mare._agent_caps = ctx.agent:capabilities()
                    mare._input_events = {}
                    mare._prompt_disclosures = {}
                    mare._mode = "deny"
                    local pending = {}
                    ctx.policy:voter("agent/input-ready", function(ev)
                        assert(ev.cancel and ev.input_id and ev.attempt_id and ev.session)
                        assert(type(ev.turn_id) == "string" and ev.turn_id ~= "")
                        assert(type(ev.revision) == "number" and ev.revision >= 1)
                        assert(type(ev.session.session_id) == "string" and type(ev.session.session_epoch) == "string")
                        mare._input_events[#mare._input_events + 1] = {
                            input_id = ev.input_id,
                            attempt_id = ev.attempt_id,
                            revision = ev.revision,
                            turn_id = ev.turn_id,
                            session_id = ev.session.session_id,
                            generation = ev.session.session_epoch,
                            delivery = ev.delivery,
                        }
                        if mare._mode == "deny" then
                            return { decision = "deny", reason = "blocked by UserPromptSubmit" }
                        elseif mare._mode == "abort" then
                            return { decision = "deny", reason = "stopped by UserPromptSubmit", abort = true }
                        end
                        pending[ev.attempt_id] = {
                            input_id = ev.input_id,
                            turn_id = ev.turn_id,
                            session_id = ev.session.session_id,
                            generation = ev.session.session_epoch,
                            text = "Hook context for " .. ev.input_id,
                        }
                        return { decision = "allow" }
                    end)
                    ctx.agent:step_participant("hooks.prompts", function(previous, step)
                        local seen, contexts, ids = {}, {}, {}
                        for _, id in ipairs(previous and previous.attempts or {}) do
                            seen[id] = true
                        end
                        local info = ctx.session:info()
                        for _, message in ipairs(step.messages) do
                            local attempt = message.input_attempt_id
                            local item = attempt and pending[attempt]
                            if item
                                and message.input_id == item.input_id
                                and item.turn_id == step.turn_id
                                and item.session_id == info.session_id
                                and item.generation == info.session_epoch
                            then
                                ids[#ids + 1] = attempt
                                if not seen[attempt] then
                                    contexts[#contexts + 1] = item.text
                                    mare._prompt_disclosures[#mare._prompt_disclosures + 1] = item.text
                                end
                            end
                        end
                        if #contexts == 0 then
                            return { state = { attempts = ids } }
                        end
                        local disclosures = {}
                        for i, text in ipairs(contexts) do
                            disclosures[i] = { source = "UserPromptSubmit", text = text }
                        end
                        return { state = { attempts = ids }, disclosures = disclosures }
                    end, { output = "transaction" })
                end,
            }
            "#,
        )
        .unwrap();

        let script = json!([{ "text": "allowed answer" }]);
        let h = env.host().user_plugins().faux(script).build().await;
        expose(&h);

        assert_eq!(eval::<i64>(&h, "return mare._agent_caps.input_admission").await, 1);

        // 1. Direct denial stops admission without writing user message or consuming faux turn.
        let denied = h.prompt("first try", json!({})).await.unwrap();
        assert_eq!(denied["not_sent"], true, "{denied}");
        assert!(denied["error"]["message"].as_str().unwrap_or("").contains("blocked by UserPromptSubmit"), "{denied}");
        assert_eq!(eval::<i64>(&h, "return #A:list_messages().data").await, 0);

        // 2. Abort denial (`abort = true` / `continue: false`) halts cleanly with not_sent + aborted.
        exec(&h, "mare._mode = 'abort'").await;
        let aborted = h.prompt("abort try", json!({})).await.unwrap();
        assert_eq!(aborted["not_sent"], true, "{aborted}");
        assert_eq!(aborted["error_kind"], "aborted", "{aborted}");
        assert_eq!(aborted["termination_reason"], "error", "{aborted}");
        assert_eq!(eval::<i64>(&h, "return #A:list_messages().data").await, 0);

        // 3. Retry with allow passes admission, writes user message with input_id/attempt_id, and triggers hooks.prompts.
        exec(&h, "mare._mode = 'allow'").await;
        let allowed = h.prompt("allowed try", json!({})).await.unwrap();
        assert!(allowed["error"].is_null(), "{allowed}");
        assert_eq!(allowed["text"], "allowed answer");
        let (ev_count, disc_count, msg_input_id, msg_attempt_id, ev_input_id, ev_attempt_id): (
            i64,
            i64,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
            local msgs = A:messages()
            local user_msg = msgs[1]
            local last_ev = mare._input_events[#mare._input_events]
            return #mare._input_events, #mare._prompt_disclosures,
                tostring(user_msg.input_id), tostring(user_msg.input_attempt_id),
                tostring(last_ev.input_id), tostring(last_ev.attempt_id)
        "#,
        )
        .await;
        assert_eq!(ev_count, 3);
        assert_eq!(disc_count, 1);
        assert_eq!(msg_input_id, ev_input_id);
        assert_eq!(msg_attempt_id, ev_attempt_id);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_input_ready_queued_denial_retry_revision_and_abort_parks_queue() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([
            { "tool_calls": [{ "name": "probe_hold", "arguments": {} }] },
            { "text": "after steer" },
            { "text": "after follow retry" }
        ]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            HOLD_STARTED_TX, HOLD_STARTED_RX = mare.task.channel()
            HOLD_RELEASE_TX, HOLD_RELEASE_RX = mare.task.channel()
            T:register("probe_hold", {
                description = "hold",
                parameters = { type = "object", properties = {} },
                execute = function()
                    HOLD_STARTED_TX:send(true)
                    HOLD_RELEASE_RX:recv()
                    return { content = "held done" }
                end,
            })
            ADMIT_EVENTS = {}
            ADMIT_MODE = "allow"
            C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                ADMIT_EVENTS[#ADMIT_EVENTS + 1] = {
                    input_id = ev.input_id,
                    attempt_id = ev.attempt_id,
                    revision = ev.revision,
                    delivery = ev.delivery,
                }
                if ev.delivery ~= "prompt" then
                    if ADMIT_MODE == "deny" then
                        return { decision = "deny", reason = "queued blocked" }
                    elseif ADMIT_MODE == "abort" then
                        return { decision = "deny", reason = "queued aborted", abort = true }
                    end
                end
                return { decision = "allow" }
            end)
        "#,
        )
        .await;

        let started = eval_json(&h, "return A:prompt('start')").await;
        let turn_id = started["turn_id"].as_str().unwrap().to_string();
        global(&h, "T1", turn_id);
        let held_ok: bool = eval(
            &h,
            "return mare.task.timeout(5000, function() return HOLD_STARTED_RX:recv() end) == true",
        )
        .await;
        assert!(held_ok, "timed out waiting for probe_hold execution to start");

        // 1. Mid-turn steer denial: keeps queued steer intact without sending it to model.
        exec(&h, "ADMIT_MODE = 'deny'").await;
        let q_res = eval_json(&h, "return A:steer('queued steer v1')").await;
        let qid = q_res["input_id"].as_str().unwrap().to_string();
        global(&h, "QID", qid.clone());
        exec(&h, "HOLD_RELEASE_TX:send(true)").await;
        let t1_res = eval_json(&h, "return A:wait(T1)").await;
        assert!(
            t1_res["error"]["message"].as_str().unwrap_or("").contains("queued blocked"),
            "steer denial must fail turn without consuming queued item: {t1_res}"
        );
        assert_eq!(eval::<i64>(&h, "return #A:list_queue().data").await, 1);

        // 2. Edit queued item (`update_queued` increments revision -> 2) and allow retry to settle T1's tool_result + steer.
        exec(
            &h,
            r#"
            A:update_queued(QID, { input = "queued steer v2" })
            ADMIT_MODE = "allow"
        "#,
        )
        .await;
        let t2_start = eval_json(&h, "return A:continue()").await;
        global(&h, "T2", t2_start["turn_id"].as_str().unwrap().to_string());
        let t2_res = eval_json(&h, "return A:wait(T2)").await;
        assert!(t2_res["error"].is_null(), "{t2_res}");
        assert_eq!(eval::<i64>(&h, "return #A:list_queue().data").await, 0);

        // 3. Queue a follow-up item on idle agent, deny with abort = true (`not_sent = true`), then retry with allow.
        let fq_res = eval_json(&h, "return A:prompt('queued follow v1', { queue_only = true })").await;
        let fqid = fq_res["input_id"].as_str().unwrap().to_string();
        global(&h, "FQID", fqid);
        exec(
            &h,
            r#"
            A:update_queued(FQID, { input = "queued follow v2" })
            ADMIT_MODE = "abort"
        "#,
        )
        .await;
        let t3_start = eval_json(&h, "return A:continue()").await;
        global(&h, "T3", t3_start["turn_id"].as_str().unwrap().to_string());
        let t3_res = eval_json(&h, "return A:wait(T3)").await;
        assert_eq!(t3_res["not_sent"], true, "{t3_res}");
        assert_eq!(t3_res["error_kind"], "aborted", "{t3_res}");
        assert_eq!(t3_res["termination_reason"], "error", "{t3_res}");
        assert_eq!(eval::<i64>(&h, "return #A:list_queue().data").await, 1);
        assert!(!eval::<bool>(&h, "return A:get().busy").await);

        exec(&h, "ADMIT_MODE = 'allow'").await;
        let t4_start = eval_json(&h, "return A:continue()").await;
        global(&h, "T4", t4_start["turn_id"].as_str().unwrap().to_string());
        let t4_res = eval_json(&h, "return A:wait(T4)").await;
        assert!(t4_res["error"].is_null(), "{t4_res}");
        assert_eq!(eval::<i64>(&h, "return #A:list_queue().data").await, 0);

        let (rev_s1, rev_s2, rev_f1, rev_f2, att_s1, att_s2, att_f1, att_f2): (
            i64,
            i64,
            i64,
            i64,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
            -- events: 1=prompt, 2=steer deny (rev 1), 3=steer allow (rev 2), 4=follow abort (rev 2), 5=follow allow (rev 2)
            local e2, e3, e4, e5 = ADMIT_EVENTS[2], ADMIT_EVENTS[3], ADMIT_EVENTS[4], ADMIT_EVENTS[5]
            return e2.revision, e3.revision, e4.revision, e5.revision,
                e2.attempt_id, e3.attempt_id, e4.attempt_id, e5.attempt_id
        "#,
        )
        .await;
        assert_eq!((rev_s1, rev_s2, rev_f1, rev_f2), (1, 2, 2, 2));
        assert_ne!(att_s1, att_s2);
        assert_ne!(att_f1, att_f2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_enqueue_durable_recovery_idempotency_attachments_and_resume_input() {
    local(async {
        let env = TestEnv::new().await;
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let script = json!([
            { "tool_calls": [{ "name": "resume_probe_tool", "arguments": {} }] },
            { "text": "scheduled goal answer after steer" },
            { "text": "unrelated fg answer" }
        ]);
        let h = env.host().faux(script.clone()).build().await;
        expose(&h);
        global(&h, "PNG_B64", png_b64.to_string());

        // 1. Stale owner and disallowed internal classification rejected cleanly.
        let stale = eval_json(
            &h,
            r#"
            local info = S:info()
            return A:enqueue("stale", { session_id = info.session_id, generation = "wrong-gen" })
        "#,
        )
        .await;
        assert_eq!(stale["status"], "stale", "{stale}");
        assert_eq!(stale["durable"], false, "{stale}");
        let bad_custom: bool = eval(
            &h,
            r#"
            local info = S:info()
            local ok, err = pcall(A.enqueue, A, "bypass", { session_id = info.session_id, session_epoch = info.session_epoch }, {
                custom_message = { role = "user", notification = true, content = { { type = "text", text = "hi" } } },
            })
            return not ok and tostring(err):find("invalid_arguments", 1, true) ~= nil
        "#,
        )
        .await;
        assert!(bad_custom, "enqueue must reject internal custom_message/notification fields");

        // 2. Durable queue-only enqueue with image attachment, attachments array, input_context=false,
        // and request_id does not start turn.
        let receipt = eval_json(
            &h,
            r#"
            local info = S:info()
            return A:enqueue({
                { type = "text", text = "background goal work" },
                { type = "image", mime = "image/png", data = PNG_B64 },
            }, { session_id = info.session_id, session_epoch = info.session_epoch }, {
                delivery = "follow_up",
                source = "schedule",
                request_id = "sched-req-1",
                input_context = { owner_kind = "schedule", task_id = "task-42" },
            })
        "#,
        )
        .await;
        assert_eq!(receipt["status"], "queued", "{receipt}");
        assert_eq!(receipt["durable"], true, "{receipt}");
        let input_id = receipt["input_id"].as_str().unwrap().to_string();
        assert!(!eval::<bool>(&h, "return A:get().busy").await);
        assert_eq!(eval::<i64>(&h, "return #A:list_queue().data").await, 1);

        // 3. Reusing request_id with different image conflicts (`request_id_conflict`); exact payload replays.
        let img_conflict: bool = eval(
            &h,
            r#"
            local info = S:info()
            local ok, err = pcall(A.enqueue, A, {
                { type = "text", text = "background goal work" },
                { type = "image", mime = "image/jpeg", data = PNG_B64 },
            }, { session_id = info.session_id, session_epoch = info.session_epoch }, {
                delivery = "follow_up",
                source = "schedule",
                request_id = "sched-req-1",
                input_context = { owner_kind = "schedule", task_id = "task-42" },
            })
            return not ok and tostring(err):find("request_id", 1, true) ~= nil
        "#,
        )
        .await;
        assert!(img_conflict, "changing image on same request_id must trigger request_id_conflict");

        let dup = eval_json(
            &h,
            r#"
            local info = S:info()
            return A:enqueue({
                { type = "text", text = "background goal work" },
                { type = "image", mime = "image/png", data = PNG_B64 },
            }, { session_id = info.session_id, session_epoch = info.session_epoch }, {
                delivery = "follow_up",
                source = "schedule",
                request_id = "sched-req-1",
                input_context = { owner_kind = "schedule", task_id = "task-42" },
            })
        "#,
        )
        .await;
        assert_eq!(dup["duplicate"], true, "{dup}");
        assert_eq!(dup["input_id"], input_id);
        assert_eq!(eval::<i64>(&h, "return #A:list_queue().data").await, 1);

        // Graceful shutdown + resume keeps parked queue + idempotency receipt intact.
        h.shutdown().await;

        let h2 = env.host().faux(script).resume().build().await;
        expose(&h2);
        global(&h2, "PNG_B64", png_b64.to_string());
        global(&h2, "EXPECTED_ID", input_id.clone());

        // Restored queue stays parked with preserved input_context and image metadata.
        assert!(!eval::<bool>(&h2, "return A:get().busy").await);
        let q_item = eval_json(&h2, "return A:list_queue().data[1]").await;
        assert_eq!(q_item["input_id"], input_id);
        assert_eq!(q_item["input_context"], json!({ "owner_kind": "schedule", "task_id": "task-42" }));
        assert_eq!(q_item["images"].as_array().unwrap().len(), 1);

        // Persisted duplicate retry after restart still replays exact input_id!
        let restart_dup = eval_json(
            &h2,
            r#"
            local info = S:info()
            return A:enqueue({
                { type = "text", text = "background goal work" },
                { type = "image", mime = "image/png", data = PNG_B64 },
            }, { session_id = info.session_id, session_epoch = info.session_epoch }, {
                delivery = "follow_up",
                source = "schedule",
                request_id = "sched-req-1",
                input_context = { owner_kind = "schedule", task_id = "task-42" },
            })
        "#,
        )
        .await;
        assert_eq!(restart_dup["duplicate"], true, "{restart_dup}");
        assert_eq!(restart_dup["input_id"], input_id);
        assert_eq!(eval::<i64>(&h2, "return #A:list_queue().data").await, 1);

        // Missing input_id resume check returns "empty".
        let missing = eval_json(
            &h2,
            r#"
            local info = S:info()
            return A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = "missing-id" })
        "#,
        )
        .await;
        assert_eq!(missing["status"], "empty", "{missing}");

        // Queue an unrelated parked foreground item, resume ONLY EXPECTED_ID across a multi-step tool turn,
        // and inject live steering mid-turn: live steering runs, while unrelated parked `fg_id` stays queued!
        let resumed = eval_json(
            &h2,
            r#"
            T:register("resume_probe_tool", {
                description = "probe",
                parameters = { type = "object", properties = {} },
                execute = function()
                    A:steer("live steer during resume")
                    return { content = "probe ok" }
                end,
            })
            C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                if ev.input_id == EXPECTED_ID then
                    mare._resumed_ev_context = ev.input_context
                    mare._resumed_ev_id = ev.input_id
                end
                return { decision = "allow" }
            end)
            local info = S:info()
            local fg = A:prompt("unrelated fg item", { queue_only = true })
            local res = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = EXPECTED_ID })
            return { fg_id = fg.input_id, started = res }
        "#,
        )
        .await;
        let fg_id = resumed["fg_id"].as_str().unwrap().to_string();
        let started = &resumed["started"];
        assert_eq!(started["status"], "started", "{started}");
        assert_eq!(started["input_id"], input_id);
        let turn_id = started["turn_id"].as_str().unwrap().to_string();
        global(&h2, "RTURN", turn_id);
        let res = eval_json(&h2, "return A:wait(RTURN)").await;
        assert!(res["error"].is_null(), "{res}");

        let (seen_ev_id, seen_ctx_str, msg_id, msg_ctx_task, has_fg_msg, has_live_steer, remaining_id): (
            String,
            String,
            String,
            String,
            bool,
            bool,
            String,
        ) = eval(
            &h2,
            r#"
            local msgs = A:messages()
            local first_user = msgs[1]
            local found_fg, found_steer = false, false
            for _, m in ipairs(msgs) do
                if m.input_id == nil and m.content and m.content[1] and m.content[1].text == "unrelated fg item" then
                    found_fg = true
                end
                if m.content and m.content[1] and tostring(m.content[1].text):find("unrelated fg item", 1, true) then
                    found_fg = true
                end
                if m.content and m.content[1] and m.content[1].text == "live steer during resume" then
                    found_steer = true
                end
            end
            local q = A:list_queue().data
            return tostring(mare._resumed_ev_id), mare.json.encode(mare._resumed_ev_context),
                tostring(first_user.input_id), tostring(first_user.input_context and first_user.input_context.task_id),
                found_fg, found_steer, tostring(#q == 1 and q[1].input_id or "")
        "#,
        )
        .await;
        let seen_ctx: serde_json::Value = serde_json::from_str(&seen_ctx_str).unwrap();
        assert_eq!(seen_ev_id, input_id);
        assert_ne!(seen_ev_id, fg_id);
        assert_eq!(seen_ctx, json!({ "owner_kind": "schedule", "task_id": "task-42" }));
        assert_eq!(msg_id, input_id);
        assert_eq!(msg_ctx_task, "task-42");
        assert!(!has_fg_msg, "unrelated parked item must never enter the resumed turn's messages");
        assert!(has_live_steer, "live steering arriving during resumed turn must still be delivered");
        assert_eq!(remaining_id, fg_id, "unrelated parked item must remain queued after multi-step resume");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_input_ready_cancel_transform_attachments_and_edge_outcomes() {
    local(async {
        let env = TestEnv::new().await;
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
        let script = json!([
            { "text": "only after allowed prompt" },
            { "text": "transformed queued answer" },
            { "text": "superseded edit answer" },
            { "text": "v2 answer" },
            { "text": "mut-1 ans" },
            { "text": "mut-2 ans" },
            { "text": "mixed ans" },
            { "text": "empty att ans" },
            { "text": "obs ans" }
        ]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        global(&h, "PNG_B64", png_b64.to_string());

        // 1. Cancelling while direct input-ready voter is pending keeps <turn_aborted> & not_sent=true
        // separately without appending unadmitted prompt or sending it to provider on next turn.
        exec(
            &h,
            r#"
            ADMIT_HOLD_TX, ADMIT_HOLD_RX = mare.task.channel()
            ADMIT_WAIT_TX, ADMIT_WAIT_RX = mare.task.channel()
            ADMIT_HOLD = true
            SEEN_MODEL_INPUTS = {}
            C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                if ADMIT_HOLD then
                    ADMIT_HOLD = false
                    ADMIT_HOLD_TX:send(true)
                    mare.task.select(function() ADMIT_WAIT_RX:recv() end, function() ev.cancel:wait() end)
                end
                return { decision = "allow" }
            end)
            A:step_participant("inspect.req", function(_, step)
                SEEN_MODEL_INPUTS[#SEEN_MODEL_INPUTS + 1] = step.messages
                return { state = {} }
            end)
        "#,
        )
        .await;
        let started = eval_json(&h, "return A:prompt('secret cancelled prompt')").await;
        global(&h, "CTURN", started["turn_id"].as_str().unwrap().to_string());
        assert!(
            eval::<bool>(&h, "return mare.task.timeout(5000, function() return ADMIT_HOLD_RX:recv() end) == true")
                .await
        );
        exec(&h, "A:cancel({ turn_id = CTURN })").await;
        let cancelled = eval_json(&h, "return A:wait(CTURN)").await;
        assert_eq!(cancelled["termination_reason"], "interrupted", "{cancelled}");
        assert_eq!(cancelled["not_sent"], true, "{cancelled}");
        let msgs_after_cancel: i64 = eval(&h, "return #A:messages()").await;
        assert_eq!(msgs_after_cancel, 1, "only <turn_aborted> marker must be recorded");
        assert!(eval::<bool>(&h, "return A:messages()[1].turn_aborted == true").await);
        assert!(
            eval::<bool>(
                &h,
                "return tostring(A:messages()[1].content[1].text):find('secret cancelled prompt', 1, true) == nil"
            )
            .await
        );
        let next_res = h.prompt("clean second prompt", json!({})).await.unwrap();
        assert_eq!(next_res["text"], "only after allowed prompt", "{next_res}");
        let leaked_to_provider: bool = eval(
            &h,
            r#"
            for _, msgs in ipairs(SEEN_MODEL_INPUTS) do
                for _, m in ipairs(msgs) do
                    for _, b in ipairs(m.content or {}) do
                        if tostring(b.text):find("secret cancelled prompt", 1, true) then
                            return true
                        end
                    end
                end
            end
            return false
        "#,
        )
        .await;
        assert!(!leaked_to_provider, "cancelled unadmitted prompt must never reach next provider request");

        // 2. Queued admission, transform isolation, attachments, and queue observer invariants:
        // - Queued admission receives exact post-input/transform content (and transform runs once).
        // - A:queue_custom strips notification=true so it cannot bypass admission.
        // - input_context=false & valid attachments survive; invalid attachments reject pre-ack.
        // - Slow denial on superseded queue revision retries cleanly without aborting turn.
        // - Denial with reason='aborted' (abort=false) yields status='failed' and parks queue.
        let (transform_count, seen_transformed, custom_admitted, false_ctx_ok, attach_count, bad_attach_err): (
            i64,
            String,
            bool,
            bool,
            i64,
            bool,
        ) = eval(
            &h,
            r#"
            local xform_calls = 0
            local last_admitted_text = ""
            local saw_custom = false
            local saw_false_ctx = false
            C.cordis:on("input/transform", function(msg, ev)
                xform_calls = xform_calls + 1
                if msg.content and msg.content[1] and msg.content[1].text == "raw queued" then
                    msg.content[1].text = "raw queued + TRANSFORMED"
                end
                return ev.next(msg)
            end)
            C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                if ev.delivery == "follow_up" and ev.content and ev.content[1] then
                    local txt = tostring(ev.content[1].text)
                    if txt == "spoof custom" then
                        saw_custom = true
                    else
                        last_admitted_text = txt
                    end
                    if ev.input_context == false then
                        saw_false_ctx = true
                    end
                end
                return { decision = "allow" }
            end)
            local info = S:info()
            local ok_bad = pcall(A.enqueue, A, "bad", { session_id = info.session_id, session_epoch = info.session_epoch }, {
                attachments = { { kind = "image", mime = "image/png" } },
            })
            local r1 = A:enqueue("raw queued", { session_id = info.session_id, session_epoch = info.session_epoch }, {
                input_context = false,
                attachments = { { kind = "image", mime = "image/png", data = PNG_B64 } },
            })
            assert(r1.durable == true and r1.input_id)
            local q1 = A:list_queue().data[1]
            local false_preserved = (q1.input_context == false)
            local n_att = #(q1.images or {})
            local r_turn = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = r1.input_id })
            assert(r_turn.status == "started")
            local w = A:wait(r_turn.turn_id)
            assert(w.error == nil, tostring(w.error))

            -- queue_custom cannot bypass admission by setting notification=true
            A:queue_custom({ role = "user", notification = true, content = { { type = "text", text = "spoof custom" } } }, "follow_up")
            local c_turn = A:continue()
            A:wait(c_turn.turn_id)
            return xform_calls, last_admitted_text, saw_custom, (false_preserved and saw_false_ctx), n_att, not ok_bad
        "#,
        )
        .await;
        assert_eq!(transform_count, 1, "input/transform must run once before queued admission, not again after");
        assert_eq!(seen_transformed, "raw queued + TRANSFORMED");
        assert!(custom_admitted, "queue_custom with notification=true must still pass agent/input-ready");
        assert!(false_ctx_ok, "falsy input_context=false must be preserved end-to-end");
        assert_eq!(attach_count, 1, "supported attachments must normalize into queued entry");
        assert!(bad_attach_err, "invalid attachment must reject before ack");

        // Slow denial on superseded revision + reason="aborted" denial status="failed":
        let (retry_after_supersede, aborted_reason_status, aborted_reason_parked): (bool, String, i64) = eval(
            &h,
            r#"
            local info = S:info()
            local r = A:enqueue("v1", { session_id = info.session_id, session_epoch = info.session_epoch })
            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            local mode = "supersede"
            local dispose_v = C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                if ev.input_id == r.input_id then
                    if mode == "supersede" and ev.revision == 1 then
                        hold_tx:send(true)
                        rel_rx:recv()
                        return { decision = "deny", reason = "stale deny", abort = true }
                    elseif mode == "aborted_str" then
                        return { decision = "deny", reason = "aborted" }
                    end
                end
                return { decision = "allow" }
            end)
            local started = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = r.input_id })
            assert(mare.task.timeout(5000, function() return hold_rx:recv() end))
            A:update_queued(r.input_id, { input = "v2" })
            mode = "allow"
            rel_tx:send(true)
            local done = A:wait(started.turn_id)
            dispose_v()
            local superseded_ok = (done.error == nil)

            local r2 = A:enqueue("fail-msg", { session_id = info.session_id, session_epoch = info.session_epoch })
            mode = "aborted_str"
            local dispose_v2 = C.cordis:get("policy"):voter("agent/input-ready", function()
                return { decision = "deny", reason = "aborted" }
            end)
            local s2 = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = r2.input_id })
            local w2 = A:wait(s2.turn_id)
            dispose_v2()
            return superseded_ok, tostring(w2.status), #A:list_queue().data
        "#,
        )
        .await;
        assert!(retry_after_supersede, "slow denial for superseded revision must retry instead of failing turn");
        assert_eq!(aborted_reason_status, "failed", "reason='aborted' without abort=true must stay status='failed'");
        assert_eq!(aborted_reason_parked, 1, "failed denial with reason='aborted' must still park queue");

        // Mutable transform reuse isolation, submit & transform notification bypass blocked,
        // mixed images + attachments + empty attachments without mutating caller opts,
        // and throwing queue/changed observer does not make committed enqueue return rejected, durable=false:
        let (
            reuse_mut_ok,
            submit_blocked,
            xform_notif_blocked,
            opts_unmutated,
            mixed_img_count,
            empty_att_img_count,
            obs_throw_queued,
        ): (bool, bool, bool, bool, i64, i64, bool) = eval(
            &h,
            r#"
            local info = S:info()
            -- Shared mutable table returned from input/transform cannot mutate item 1 in SAME batch,
            -- and secret registered for projection is redacted BEFORE both voter payload and committed message:
            mare.projection.register("TEST_SECRET_VALUE_999")
            local shared_tbl = { role = "user", content = { { type = "text", text = "" } } }
            local seen_by_voter = {}
            local dx = C.cordis:on("input/transform", function(msg, ev)
                local txt = msg.content and msg.content[1] and msg.content[1].text or ""
                if txt == "mut-1" or txt == "mut-2" then
                    shared_tbl.content[1].text = txt .. "-SNAP-TEST_SECRET_VALUE_999"
                    return shared_tbl
                end
                return ev.next(msg)
            end)
            local dv_proj = C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                if ev and ev.input_id and ev.content and ev.content[1] then
                    seen_by_voter[ev.input_id] = ev.content[1].text
                end
                return { decision = "allow" }
            end)
            local m1 = A:enqueue("mut-1", { session_id = info.session_id, session_epoch = info.session_epoch })
            local m2 = A:enqueue("mut-2", { session_id = info.session_id, session_epoch = info.session_epoch })
            local r_mut_all = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch })
            A:wait(r_mut_all.turn_id)
            dv_proj()
            dx()
            local mut1_text, mut2_text
            for _, m in ipairs(A:messages()) do
                if m.input_id == m1.input_id and m.content and m.content[1] then
                    mut1_text = m.content[1].text
                elseif m.input_id == m2.input_id and m.content and m.content[1] then
                    mut2_text = m.content[1].text
                end
            end
            local mut1_ok = (mut1_text ~= nil and mut2_text ~= nil
                and mut1_text == seen_by_voter[m1.input_id]
                and mut2_text == seen_by_voter[m2.input_id]
                and mut1_text:find("mut-1-SNAP-", 1, true) ~= nil
                and mut2_text:find("mut-2-SNAP-", 1, true) ~= nil
                and not mut1_text:find("TEST_SECRET_VALUE_999", 1, true)
                and not mut2_text:find("TEST_SECRET_VALUE_999", 1, true))

            -- queue_custom rejects role="user" in next_turn/pending; submit/transform notification blocked
            local ok_nt = pcall(A.queue_custom, A, { role = "user", content = { { type = "text", text = "nt" } } }, "next_turn")
            local ok_pd = pcall(A.queue_custom, A, { role = "user", content = { { type = "text", text = "pd" } } }, "pending")
            assert(not ok_nt and not ok_pd, "queue_custom must disallow user-role next_turn/pending bypass")
            -- New P2: queued custom message with string content preserves string content cleanly
            local custom_eid = A:queue_custom({ role = "custom", customType = "note", content = "string custom body" }, "follow_up")
            local r_cust = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = custom_eid })
            A:wait(r_cust.turn_id)
            local custom_str_ok = false
            for _, m in ipairs(A:messages()) do
                if m.input_id == custom_eid and m.content == "string custom body" then
                    custom_str_ok = true
                end
            end
            assert(custom_str_ok, "queued custom message must preserve string content")

            local deny_all = C.cordis:get("policy"):voter("agent/input-ready", function()
                return { decision = "deny", reason = "blocked submit spoof" }
            end)
            local ok_sub, err_sub = pcall(function()
                return A:prompt("spoof submit", { internal_notification = true })
            end)
            local spoof_rejected = (not ok_sub and type(err_sub) == "table" and err_sub.code == "invalid_arguments")
            local dx2 = C.cordis:on("input/transform", function(msg, ev)
                msg.notification = true
                return msg
            end)
            local s_xf = A:prompt("spoof xform notif")
            local w_xf = A:wait(s_xf.turn_id)
            dx2()
            deny_all()

            -- Mixed images + attachments + text-only ContentPart + empty attachments without mutating caller opts
            local caller_opts = {
                request_id = "mixed-media-1",
                images = { { type = "image", mime = "image/png", data = PNG_B64 } },
                attachments = { { kind = "image", mime = "image/png", data = PNG_B64 } },
            }
            local e_mix = A:enqueue({ { type = "text", text = "text-only part" } }, { session_id = info.session_id, session_epoch = info.session_epoch }, caller_opts)
            local e_mix_dup = A:enqueue({ { type = "text", text = "text-only part" } }, { session_id = info.session_id, session_epoch = info.session_epoch }, caller_opts)
            local opts_clean = (#caller_opts.images == 1 and #caller_opts.attachments == 1 and e_mix_dup.duplicate == true)
            local r_mix = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = e_mix.input_id })
            A:wait(r_mix.turn_id)
            local mix_imgs = 0
            for _, m in ipairs(A:messages()) do
                if m.input_id == e_mix.input_id then
                    for _, b in ipairs(m.content or {}) do
                        if b.type == "image" then mix_imgs = mix_imgs + 1 end
                    end
                end
            end
            local e_empty_att = A:enqueue("empty att", { session_id = info.session_id, session_epoch = info.session_epoch }, {
                images = { { type = "image", mime = "image/png", data = PNG_B64 } },
                attachments = {},
            })
            local r_ea = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = e_empty_att.input_id })
            A:wait(r_ea.turn_id)
            local ea_imgs = 0
            for _, m in ipairs(A:messages()) do
                if m.input_id == e_empty_att.input_id then
                    for _, b in ipairs(m.content or {}) do
                        if b.type == "image" then ea_imgs = ea_imgs + 1 end
                    end
                end
            end

            -- Throwing observer on queue/changed must not report rejected, durable=false
            local obs_dispose = C.cordis:on("queue/changed", function()
                error("queue observer boom")
            end)
            local e_obs = A:enqueue("after-obs", { session_id = info.session_id, session_epoch = info.session_epoch })
            obs_dispose()
            local r_obs = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = e_obs.input_id })
            A:wait(r_obs.turn_id)
            return mut1_ok, spoof_rejected,
                (w_xf.status == "failed" and w_xf.not_sent == true), opts_clean, mix_imgs, ea_imgs,
                (e_obs.status == "queued" and e_obs.durable == true)
        "#,
        )
        .await;
        assert!(reuse_mut_ok, "reused mutable table from input/transform must not mutate voted candidate");
        assert!(submit_blocked, "submit with _internal_notification/notification must still be vetted");
        assert!(xform_notif_blocked, "input/transform setting notification=true must still be vetted");
        assert!(opts_unmutated, "normalize_input must never mutate caller opts or break retry fingerprint");
        assert_eq!(mixed_img_count, 2, "mixed opts.images + opts.attachments must preserve both images");
        assert_eq!(empty_att_img_count, 1, "empty attachments={{}} must not drop opts.images");
        assert!(obs_throw_queued, "throwing observer after store commit must not mask queued/durable=true");

        h.shutdown().await;
        drop(h);
        drop(env);

        // Missing claim_operation + request_id rejected; post-commit publication failure returns indeterminate;
        // info.resume_error rejects enqueue against an active replacement session service:
        let env2 = TestEnv::new().await;
        let plugins2 = env2.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins2).unwrap();
        std::fs::write(
            plugins2.join("session.lua"),
            r##"
            return {
                name = "session",
                override = true,
                provide = { "session" },
                contracts = {
                    test = {
                        owner = "session",
                        events = {
                            ["set-edge"] = { doc = "test edge toggle", mode = "emit", expose = "process" },
                        },
                    },
                },
                apply = function(ctx)
                    local resume_err = nil
                    local post_commit_fail = false
                    local with_claim = false
                    local omit_commit_op = false
                    local receipt_replayed = false
                    local claim_calls = 0
                    local receipt_calls = 0
                    local last_op_type = nil
                    local sw_hold_rx = nil
                    ctx:on("test/set-edge", function(ev)
                        resume_err = ev and ev.resume_err or nil
                        post_commit_fail = ev and ev.post_commit_fail == true or false
                        with_claim = ev and ev.with_claim == true or false
                        omit_commit_op = ev and ev.omit_commit_op == true or false
                        receipt_replayed = ev and ev.receipt_replayed == true or false
                        sw_hold_rx = ev and ev.sw_hold_rx or nil
                    end)
                    local cur = { id = "stub-1", path = "/stub/stub-1.jsonl" }
                    local function desc()
                        return {
                            session_id = cur.id,
                            session_epoch = cur.id .. "#0:0",
                            cwd = mare.cwd,
                            path = cur.path,
                            loaded = true,
                            created_at_ms = 0,
                            updated_at_ms = 0,
                        }
                    end
                    local impl = {
                        commit = function(_, batch)
                            last_op_type = type(batch and batch.operation)
                            if post_commit_fail then
                                return nil, "post publish boom", "post_commit_failed", "committed"
                            end
                            local agent_svc = ctx:get("agent")
                            local publication, _, messages
                            if agent_svc and batch and batch.inbox and batch.messages then
                                publication, _, messages = agent_svc:prepare_commit(batch.inbox, batch.messages)
                                if publication then publication() end
                            end
                            local res = {
                                messages = messages or (batch and batch.messages) or mare.json.array(),
                                ids = mare.json.array(),
                                inbox = batch and batch.inbox or { steer = mare.json.array(), follow_up = mare.json.array() },
                            }
                            if batch and batch.operation and not omit_commit_op then
                                res.operation = {
                                    request_id = batch.operation.request_id,
                                    kind = batch.operation.kind,
                                    fingerprint = batch.operation.fingerprint,
                                    result = batch.operation.result,
                                }
                            end
                            return res
                        end,
                        claim_operation = function(_, id, _)
                            if not with_claim then
                                return nil, "not supported", "rejected"
                            end
                            claim_calls = claim_calls + 1
                            return { status = "claimed", claim_id = "c-" .. claim_calls, session_id = cur.id, session_epoch = cur.id .. "#0:0" }
                        end,
                        operation_receipt = function(_, id)
                            receipt_calls = receipt_calls + 1
                            if receipt_replayed then
                                return {
                                    status = "replayed",
                                    receipt = {
                                        version = 1,
                                        session_id = cur.id,
                                        request_id = id.request_id,
                                        kind = id.kind,
                                        fingerprint = id.fingerprint,
                                        result = { status = "queued", durable = true, input_id = "reconciled-1" },
                                    },
                                }
                            end
                            return { status = "missing" }
                        end,
                        release_operation = function() return true end,
                        append_compaction = function() return true end,
                        append_prune = function() return true end,
                        set_sandbox_mode = function(_, spec) return mare.proc.sandbox.set_current(spec) end,
                        info = function()
                            return {
                                session_id = cur.id,
                                session_epoch = cur.id .. "#0:0",
                                path = cur.path,
                                count = 0,
                                resume_error = resume_err,
                                last_op_type = last_op_type,
                                claim_calls = claim_calls,
                                receipt_calls = receipt_calls,
                            }
                        end,
                        available = function() return true end,
                        switching = function()
                            if sw_hold_rx then
                                local rx = sw_hold_rx
                                rx:recv()
                            end
                            return false
                        end,
                        child = function() return nil end,
                        children = function() return {} end,
                        short_id = function(_, id) return id end,
                        load_transcript = function() end,
                        get = function() return desc() end,
                        list = function() return { data = { desc() } } end,
                        entries = function() return {} end,
                        list_items = function() return { data = {} } end,
                        add_record = function() return { record_id = "r1", durable = false } end,
                        list_records = function() return { data = {} } end,
                        leaf = function() return nil end,
                        entry_id = function() return nil end,
                        file_changes = function() return {} end,
                        path_to = function() return {} end,
                        set_leaf = function() return true end,
                        update = function() return desc() end,
                        label_item = function() return { item_id = "e1" } end,
                        archive = function() return {} end,
                        unarchive = function() return {} end,
                        delete = function() return {} end,
                        rewind = function() return desc() end,
                        transition = function() return { status = "rejected", error = "unsupported" } end,
                        persist_message = function() return "mock-msg" end,
                        record_tool_state = function() return "mock-ts" end,
                        record_request = function() return "mock-req" end,
                        record_request_end = function() return "mock-req-end" end,
                        record_request_aborted = function() return "mock-req-abort" end,
                        record_run_end = function() return true end,
                        baseline_cut = function() return { parts = {} } end,
                        snapshot = function() return { session_id = cur.id, path = cur.path } end,
                        snapshot_context = function() return { entries = {}, message_count = 0 } end,
                        search = function() return { data = {}, corpus_revision = "g0", incomplete = false } end,
                        search_items = function() return { data = {}, corpus_revision = "g0", incomplete = false } end,
                        get_item = function() return {} end,
                        trace = function() return {} end,
                        trace_item = function() return {} end,
                    }
                    local Session = require("cordis").class("Session", mare.Service)
                    function Session:constructor(c)
                        Session.super.constructor(self, c, "session")
                    end
                    for name, fn in pairs(impl) do
                        Session[name] = fn
                    end
                    ctx:plugin(Session)
                end,
            }
            "##,
        )
        .unwrap();
        let h2 = env2.host().user_plugins().faux(json!([{ "text": "ok" }])).build().await;
        expose(&h2);
        assert!(h2.kernel.problems().is_empty(), "{:?}", h2.kernel.problems());
        let (
            no_claim_status,
            plain_op_type,
            missing_op_status,
            missing_op_durable,
            ind_status,
            ind_durable_nil,
            ind_claim_delta,
            ind_receipt_delta,
            reconciled_status,
            reconciled_durable,
            resume_err_status,
            concurrent_resumes,
        ): (
            String,
            String,
            String,
            bool,
            String,
            bool,
            i64,
            i64,
            String,
            bool,
            String,
            String,
        ) = eval(
            &h2,
            r#"
            local info = S:info()
            -- Without claim_operation support (explicitly absent or rejecting), request_id is rejected:
            S.claim_operation = nil
            local no_claim = A:enqueue("x", { session_id = info.session_id, session_epoch = info.session_epoch }, { request_id = "req-nc" })

            -- New P2 A: plain enqueue without request_id must pass nil operation (never builder function):
            local plain_ok = A:enqueue("plain-no-rid", { session_id = info.session_id, session_epoch = info.session_epoch })
            assert(plain_ok.status == "queued")
            local plain_type = tostring(S:info().last_op_type)

            -- Commit returning without receipt after publication() reconciles via operation_receipt
            -- and yields indeterminate (durable=nil) when unconfirmed, never false-negative rejected/durable=false:
            local before_omit_receipts = S:info().receipt_calls
            C.cordis:emit("test/set-edge", { with_claim = true, omit_commit_op = true })
            local missing_op = A:enqueue("op-miss", { session_id = info.session_id, session_epoch = info.session_epoch }, { request_id = "req-miss" })
            local omit_receipt_delta = S:info().receipt_calls - before_omit_receipts
            assert(missing_op.status == "indeterminate" and missing_op.durable == nil and omit_receipt_delta == 1)

            -- Indeterminate with request_id reconciles via read-only operation_receipt (never extra claim_operation):
            local before_claims = S:info().claim_calls
            local before_receipts = S:info().receipt_calls
            C.cordis:emit("test/set-edge", { with_claim = true, post_commit_fail = true, receipt_replayed = false })
            local ind = A:enqueue("ind-msg", { session_id = info.session_id, session_epoch = info.session_epoch }, { request_id = "ind-req-1" })
            local after_claims = S:info().claim_calls
            local after_receipts = S:info().receipt_calls

            C.cordis:emit("test/set-edge", { with_claim = true, post_commit_fail = true, receipt_replayed = true })
            local rec = A:enqueue("ind-msg-rec", { session_id = info.session_id, session_epoch = info.session_epoch }, { request_id = "ind-req-2" })
            C.cordis:emit("test/set-edge", {})

            C.cordis:emit("test/set-edge", { resume_err = "corrupt tail" })
            local re = A:enqueue("re-msg", { session_id = info.session_id, session_epoch = info.session_epoch })
            C.cordis:emit("test/set-edge", {})

            -- Deterministic concurrent overlap in resume_input across yielding sess:switching():
            local entered_tx, entered_rx = mare.task.channel()
            local sw_tx1, sw_rx1 = mare.task.channel()
            local sw_tx2, sw_rx2 = mare.task.channel()
            local n_sw = 0
            local hold_sw_rx = {
                recv = function()
                    n_sw = n_sw + 1
                    if n_sw == 1 then
                        entered_tx:send(1)
                        sw_rx1:recv()
                    elseif n_sw == 2 then
                        entered_tx:send(2)
                        sw_rx2:recv()
                    end
                end
            }
            C.cordis:emit("test/set-edge", { sw_hold_rx = hold_sw_rx })
            local r1_tx, r1_rx = mare.task.channel()
            local r2_tx, r2_rx = mare.task.channel()
            C.cordis:spawn(function()
                r1_tx:send(A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = plain_ok.input_id }))
            end)
            C.cordis:spawn(function()
                r2_tx:send(A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = plain_ok.input_id }))
            end)
            assert(entered_rx:recv() ~= nil and entered_rx:recv() ~= nil)
            sw_tx1:send(true)
            sw_tx2:send(true)
            local cr1 = r1_rx:recv()
            local cr2 = r2_rx:recv()
            C.cordis:emit("test/set-edge", {})
            if cr1 and cr1.turn_id then A:wait(cr1.turn_id) end
            if cr2 and cr2.turn_id then A:wait(cr2.turn_id) end

            return tostring(no_claim.status), plain_type, tostring(missing_op.status), missing_op.durable == nil,
                tostring(ind.status), ind.durable == nil, after_claims - before_claims, after_receipts - before_receipts,
                tostring(rec.status), rec.durable == true, tostring(re.status),
                tostring(cr1.status) .. ":" .. tostring(cr2.status)
        "#,
        )
        .await;
        assert_eq!(no_claim_status, "rejected");
        assert_eq!(plain_op_type, "nil", "plain enqueue without request_id must pass nil operation, not function");
        assert_eq!(missing_op_status, "indeterminate");
        assert!(missing_op_durable, "post-commit missing operation receipt must return indeterminate with durable=nil");
        assert_eq!(ind_status, "indeterminate");
        assert!(ind_durable_nil, "indeterminate must not claim durable=false");
        assert_eq!(ind_claim_delta, 1, "reconciliation must NOT call mutating claim_operation a second time");
        assert_eq!(ind_receipt_delta, 1, "reconciliation must call read-only operation_receipt once");
        assert_eq!(reconciled_status, "queued");
        assert!(reconciled_durable);
        assert_eq!(resume_err_status, "rejected");
        assert!(
            concurrent_resumes == "started:busy" || concurrent_resumes == "busy:started",
            "two overlapping resume_input calls across yielding switching() must return started:busy or busy:started: {concurrent_resumes}"
        );

        h2.shutdown().await;
        drop(h2);
        drop(env2);

        // Voter withdraws mid-wait, and verify actual next HTTP request body sent to provider:
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let mut env3 = TestEnv::new().await;
        env3.set_var("ANTHROPIC_API_KEY", "test-anthropic");
        let h3 = env3
            .host()
            .config(json!({
                "model": "anthropic/claude-sonnet-5-5",
                "plugins": {
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }))
            .build()
            .await;
        expose(&h3);
        exec(
            &h3,
            r#"
            W_HOLD_TX, W_HOLD_RX = mare.task.channel()
            W_REL_TX, W_REL_RX = mare.task.channel()
            W_DISPOSE = C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                W_HOLD_TX:send(true)
                mare.task.select(function() W_REL_RX:recv() end, function() ev.cancel:wait() end)
                return { decision = "allow" }
            end)
        "#,
        )
        .await;
        let w_started = eval_json(&h3, "return A:prompt('secret withdrawn prompt')").await;
        global(&h3, "WTURN", w_started["turn_id"].as_str().unwrap().to_string());
        assert!(
            eval::<bool>(&h3, "return mare.task.timeout(5000, function() return W_HOLD_RX:recv() end) == true").await
        );
        // Withdraw the only voter while admission is suspended, THEN cancel the turn!
        exec(&h3, "W_DISPOSE(); W_REL_TX:send(true); A:cancel({ turn_id = WTURN })").await;
        let w_cancelled = eval_json(&h3, "return A:wait(WTURN)").await;
        assert_eq!(w_cancelled["termination_reason"], "interrupted", "{w_cancelled}");
        assert_eq!(w_cancelled["not_sent"], true, "{w_cancelled}");
        assert!(
            !eval::<bool>(&h3, "return mare.json.encode(A:messages()):find('secret withdrawn prompt', 1, true) ~= nil")
                .await,
            "withdrawn-voter cancelled prompt must never enter conversation history"
        );
        // Earlier window: cancel + last-voter withdrawal during yielding input/transform before new_state():
        exec(
            &h3,
            r#"
            XF_HOLD_TX, XF_HOLD_RX = mare.task.channel()
            XF_REL_TX, XF_REL_RX = mare.task.channel()
            XF_DISPOSE_V = C.cordis:get("policy"):voter("agent/input-ready", function()
                return { decision = "allow" }
            end)
            XF_DISPOSE_H = C.cordis:on("input/transform", function(msg, ev)
                XF_HOLD_TX:send(true)
                XF_REL_RX:recv()
                return ev.next(msg)
            end)
        "#,
        )
        .await;
        let xf_started = eval_json(&h3, "return A:prompt('secret transform cancelled prompt')").await;
        global(&h3, "XFTURN", xf_started["turn_id"].as_str().unwrap().to_string());
        assert!(
            eval::<bool>(&h3, "return mare.task.timeout(5000, function() return XF_HOLD_RX:recv() end) == true").await
        );
        exec(&h3, "XF_DISPOSE_V(); A:cancel({ turn_id = XFTURN }); XF_REL_TX:send(true); XF_DISPOSE_H()").await;
        let xf_cancelled = eval_json(&h3, "return A:wait(XFTURN)").await;
        assert_eq!(xf_cancelled["termination_reason"], "interrupted", "{xf_cancelled}");
        assert!(
            !eval::<bool>(&h3, "return mare.json.encode(A:messages()):find('secret transform cancelled prompt', 1, true) ~= nil")
                .await,
            "transform-cancelled prompt after voter withdrawal must never enter conversation history"
        );
        let http_res = h3.prompt("clean http prompt", json!({})).await.unwrap();
        assert_eq!(http_res["text"], "All done.", "{http_res}");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1, "expected exactly 1 provider HTTP request");
        assert!(reqs[0].body.contains("clean http prompt"), "{}", reqs[0].body);
        assert!(
            !reqs[0].body.contains("secret withdrawn prompt"),
            "cancelled unadmitted prompt leaked into HTTP request body: {}",
            reqs[0].body
        );
        h3.shutdown().await;
    })
    .await
}

#[test]
fn agent_enqueue_abrupt_exit_crash_recovery_replays_receipt() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let cwd = dunce::canonicalize(work.path()).unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let ready_marker = home.path().join("enqueue-committed.marker");
    let replay_marker = home.path().join("enqueue-replay.json");
    std::fs::write(
        plugins.join("abrupt_enqueue_probe.lua"),
        format!(
            r#"
            return {{
                name = "abrupt-enqueue-probe",
                inject = {{ "agent", "session", "commands" }},
                apply = function(ctx)
                    ctx.commands:register("trigger-abrupt-enqueue", {{
                        description = "commit durable enqueue and write marker before RPC ACK",
                        run = function()
                            local info = ctx.session:info()
                            local res = ctx.agent:enqueue("crash boundary work", {{
                                session_id = info.session_id,
                                session_epoch = info.session_epoch,
                            }}, {{
                                request_id = "abrupt-req-1",
                                input_context = {{ owner_kind = "goal", task_id = "crash-1" }},
                            }})
                            assert(res.durable == true and res.input_id)
                            mare.fs.write({:?}, res.input_id, {{ atomic = true }})
                            -- Hold before returning so the test process kills us BEFORE any RPC ACK arrives.
                            local _, never_rx = mare.task.channel()
                            never_rx:recv()
                        end,
                    }})
                    ctx.commands:register("replay-abrupt-enqueue", {{
                        description = "replay duplicate enqueue after restart",
                        run = function()
                            local info = ctx.session:info()
                            local res = ctx.agent:enqueue("crash boundary work", {{
                                session_id = info.session_id,
                                session_epoch = info.session_epoch,
                            }}, {{
                                request_id = "abrupt-req-1",
                                input_context = {{ owner_kind = "goal", task_id = "crash-1" }},
                            }})
                            mare.fs.write({:?}, mare.json.encode(res))
                        end,
                    }})
                end,
            }}
            "#,
            ready_marker.to_string_lossy(),
            replay_marker.to_string_lossy()
        ),
    )
    .unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "SEED" }, { "text": "RECOVERED" }]).to_string()).unwrap();

    // Process 1: trigger durable enqueue commit then abruptly terminate before returning/shutting down gracefully.
    let mut cmd1 = mare_cmd();
    cmd1.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .env("MARE_ABRUPT_ENQUEUE", "1")
        .current_dir(&cwd)
        .stderr(Stdio::null());
    let mut rpc1 = RpcClient::spawn_cmd(&mut cmd1);
    let init1 = rpc1.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let sid = init1["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let p_start = rpc1.call_raw("agent.prompt", json!({ "session_id": sid, "input": "seed" }), |_| json!(null));
    rpc1.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": p_start["result"]["turn_id"] }), |_| json!(null));
    let s_path = rpc1.call_raw("session.list", json!({ "loaded": true }), |_| json!(null))["result"]["data"][0]["path"]
        .as_str()
        .unwrap()
        .to_string();
    // Send command asynchronously (without waiting for RPC ACK), wait for durable commit marker on disk,
    // then kill the process BEFORE RPC ACK arrives.
    let _req_id = rpc1.request("commands.run", json!({ "line": "/trigger-abrupt-enqueue" }));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready_marker.exists() {
        assert!(std::time::Instant::now() < deadline, "durable enqueue commit marker missing before timeout");
        std::thread::yield_now();
    }
    let committed_id = std::fs::read_to_string(&ready_marker).unwrap();
    rpc1.kill_and_wait();

    // Verify journal has NO clean close marker (`"state":"closed"`), confirming real abrupt exit.
    let journal = std::fs::read_to_string(&s_path).unwrap();
    assert!(!journal.contains("\"state\":\"closed\""), "abrupt kill must not write clean close marker");
    assert!(journal.contains("abrupt-req-1"), "operation receipt must be durable on disk");

    // Process 2: resume after abrupt kill and verify parked queue + persisted receipt.
    let mut cmd2 = mare_cmd();
    cmd2.args(["--rpc", "--yolo", "--model", "faux/echo", "--resume", &s_path])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(&cwd)
        .stderr(Stdio::null());
    let mut rpc2 = RpcClient::spawn_cmd(&mut cmd2);
    rpc2.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let q = rpc2.call_raw("agent.list_queue", json!({ "session_id": sid }), |_| json!(null));
    let items = q["result"]["data"].as_array().unwrap();
    assert_eq!(items.len(), 1, "restored queue after crash: {q}");
    assert_eq!(items[0]["input_id"], committed_id);
    assert_eq!(items[0]["input_context"], json!({ "owner_kind": "goal", "task_id": "crash-1" }));
    let replay_cmd = rpc2.call_raw("commands.run", json!({ "line": "/replay-abrupt-enqueue" }), |_| json!(null));
    assert!(replay_cmd.get("error").is_none(), "{replay_cmd}");
    let replay_val: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&replay_marker).unwrap()).unwrap();
    assert_eq!(replay_val["duplicate"], true, "{replay_val}");
    assert_eq!(replay_val["input_id"], committed_id, "{replay_val}");
    rpc2.shutdown_and_wait();
}

#[tokio::test]
async fn agent_prompt_validates_attachment_shapes_and_supports_file_audio_video() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "ack" }])).build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local info = S:info()
            local owner = { session_id = info.session_id, session_epoch = info.session_epoch }
            local function must_fail(opts)
                local ok_p, err = pcall(function()
                    return A:enqueue("check", owner, opts)
                end)
                assert(not ok_p, "expected enqueue error")
                assert(tostring(err):find("invalid_arguments", 1, true), "expected invalid_arguments in " .. tostring(err))
            end
            must_fail({ attachments = { [2] = { kind = "file", path = "a.txt" } } })
            must_fail({ attachments = { "bad" } })
            must_fail({ attachments = { { kind = "file", type = "image", path = "a.txt" } } })
            must_fail({ attachments = { {} } })
            must_fail({ attachments = { { kind = "file", path = "" } } })
            must_fail({ attachments = { { kind = "spreadsheet" } } })

            -- Valid file attachment is accepted and turned into a mention block.
            local enq = A:enqueue("with attachments", owner, {
                attachments = {
                    { kind = "file", path = "README.md", mime = "text/markdown", name = "README.md" },
                },
            })
            assert(enq.status == "queued", tostring(enq.status))
            local started = A:continue()
            local res = A:wait(started.turn_id)
            assert(not res.error, tostring(res.error))

            -- Valid audio and video attachments parse cleanly then fail with capability `unsupported`.
            for _, att in ipairs({
                { kind = "audio", mime = "audio/wav", data = "AAAA", duration_secs = 1.5, transcription = "hi" },
                { kind = "video", mime = "video/mp4", data = "BBBB", duration_secs = 2.0 },
            }) do
                local ok_m, err_m = pcall(function()
                    return A:enqueue("media check", owner, { attachments = { att } })
                end)
                assert(not ok_m and tostring(err_m):find("unsupported", 1, true), tostring(err_m))
            end
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_persist_failure_admit_deny_request_id_conflict_and_subagent_validation() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("loop_file.txt"), "ok\n").unwrap();
        let h = env
            .host()
            .builtins(&["subagent", "compact"])
            .faux(json!([
                { "text": "first" },
                { "tool_calls": [{ "id": "tc_loop_1", "name": "read", "arguments": { "path": "loop_file.txt" } }] },
                { "text": "after tool" },
                { "text": "persist fault reply" },
                { "text": "extra" }
            ]))
            .build()
            .await;
        expose(&h);
        global(&h, "SUB", h.service_table("subagent").unwrap());
        let ok: bool = eval(
            &h,
            r#"
            -- 1. subagent rejects unknown type, invalid fork, conflicting fork+messages, and subagent tools handle missing ids.
            local ok_fork, err_fork = pcall(function()
                return SUB:start({ prompt = "hi", fork = "all", messages = { { role = "user", content = { { type = "text", text = "m" } } } } })
            end)
            assert(not ok_fork and tostring(err_fork):find("cannot be combined", 1, true), tostring(err_fork))
            local ok_badfork, err_badfork = pcall(function()
                return SUB:start({ prompt = "hi", fork = "bogus_fork_val" })
            end)
            assert(not ok_badfork and tostring(err_badfork):find("fork", 1, true), tostring(err_badfork))
            local ok_type, err_type = pcall(function()
                return SUB:start({ prompt = "hi", type = "no_such_subagent_type_xyz" })
            end)
            assert(not ok_type and tostring(err_type):find("unknown subagent type", 1, true), tostring(err_type))
            local sl = T:execute({ id = "sub_l", name = "subagent_list", arguments = {} }, { on_state = function() return true end })
            assert(tostring(sl.content):find("No subagents", 1, true), tostring(sl.content))
            local ss = T:execute({ id = "sub_s", name = "subagent_steer", arguments = { id = "no-such-sub", message = "hi" } }, { on_state = function() return true end })
            assert(tostring(ss.content):find("not running", 1, true) or ss.is_error, tostring(ss.content))
            local sc = T:execute({ id = "sub_c", name = "subagent_cancel", arguments = { id = "no-such-sub" } }, { on_state = function() return true end })
            assert(sc ~= nil)

            -- 1b. agent.service input validation arms.
            assert(not pcall(function() A:prompt("x", { when_busy = "bogus" }) end))
            assert(not pcall(function() A:prompt("x", { tools = "not-an-array" }) end))
            assert(not pcall(function() A:steer("") end))
            assert(not pcall(function() A:queue_custom({}, "bogus_mode") end))
            assert(not pcall(function() A:cancel({ mode = "bogus" }) end))
            assert(not pcall(function() A:wait("unknown-turn-id-xyz") end))
            assert(not pcall(function() A:enqueue("hi", nil, "bad-opts") end))
            assert(not pcall(function() A:enqueue("hi", nil, { delivery = "bogus" }) end))
            assert(not pcall(function() A:enqueue("", nil, {}) end))

            -- 1c. session.lifecycle & session.queries argument validation and idempotent session.create receipts.
            assert(not pcall(function() S:list("bad") end))
            assert(not pcall(function() S:list({ cwd = "" }) end))
            assert(not pcall(function() S:list({ parent_session_id = "" }) end))
            assert(not pcall(function() S:list({ loaded = "yes" }) end))
            assert(not pcall(function() S:load("bad") end))
            assert(not pcall(function() S:load({}) end))
            assert(not pcall(function() S:load({ path = "" }) end))
            assert(not pcall(function() S:load({ session_id = "" }) end))
            assert(not pcall(function() S:load({ path = "/tmp/x", subscribe = "no" }) end))
            assert(not pcall(function() S:create("bad") end))
            assert(not pcall(function() S:create({ cwd = "" }) end))
            assert(not pcall(function() S:create({ cwd = "/nonexistent_mare_cov_dir_xyz" }) end))
            assert(not pcall(function() S:create({ name = 123 }) end))
            assert(not pcall(function() S:create({ subscribe = "no" }) end))
            assert(not pcall(function() S:create({ request_id = "" }) end))
            local cr1 = S:create({ request_id = "cov-cr-1", name = "cr-alpha" })
            local cr1_dup = S:create({ request_id = "cov-cr-1", name = "cr-alpha" })
            assert(cr1.session_id == cr1_dup.session_id)
            assert(not pcall(function() S:create({ request_id = "cov-cr-1", name = "cr-beta" }) end))
            assert(not pcall(function() S:close("") end))
            assert(not pcall(function() S:close(cr1.session_id, "bad") end))
            assert(not pcall(function() S:add_workspace_dirs("bad") end))
            assert(not pcall(function() S:add_workspace_dirs({ "" }) end))

            -- 2. agent/input-ready denial with abort=true on a queued follow-up halts the loop cleanly.
            local deny_follow = false
            local off_admit = C.cordis:get("policy"):voter("agent/input-ready", function()
                if deny_follow then
                    deny_follow = false
                    return { decision = "deny", reason = "denied by test", abort = true }
                end
            end)
            local info = S:info()
            local t1 = A:prompt("alpha payload", { request_id = "prompt-rid-1" })
            A:wait(t1.turn_id)

            -- 3. prompt with conflicting request_id raises request_id_conflict.
            local ok_c, err_c = pcall(function()
                return A:prompt("beta payload", { request_id = "prompt-rid-1" })
            end)
            assert(not ok_c and tostring(err_c):find("request_id", 1, true), tostring(err_c))

            -- 3b. Enqueue a follow-up and deny it with abort=true on resume to cover consume_or_fail deny_abort path.
            local enq = A:enqueue("blocked follow", { session_id = info.session_id, session_epoch = info.session_epoch })
            deny_follow = true
            local r_res = A:resume_input({ session_id = info.session_id, session_epoch = info.session_epoch }, { input_id = enq.input_id })
            local r_den = A:wait(r_res.turn_id)
            off_admit()
            assert(r_den.error ~= nil, "expected denied turn error")

            -- 3c. store_switch refuses session switch when side requests or compaction cannot be cancelled.
            local llm_svc = C.cordis:get("llm")
            local orig_pending = llm_svc.side_requests_pending
            local orig_cancel = llm_svc.cancel_side_requests
            llm_svc.side_requests_pending = function() return true end
            llm_svc.cancel_side_requests = false
            local ok_sw, err_sw = pcall(function() return S:create() end)
            llm_svc.side_requests_pending = orig_pending
            llm_svc.cancel_side_requests = orig_cancel
            assert(not ok_sw and tostring(err_sw):find("side requests", 1, true), tostring(err_sw))

            local cmp_svc = C.cordis:get("compact")
            if cmp_svc then
                local orig_busy, orig_cc = cmp_svc.busy, cmp_svc.cancel
                cmp_svc.busy = function() return true end
                cmp_svc.cancel = false
                local ok_cs, err_cs = pcall(function() return S:create() end)
                cmp_svc.busy, cmp_svc.cancel = orig_busy, orig_cc
                assert(not ok_cs and tostring(err_cs):find("compaction", 1, true), tostring(err_cs))
            end
            local orig_drain = SUB.drain
            SUB.drain = function() error("subagent drain busy") end
            local ok_dr, err_dr = pcall(function() return S:create() end)
            SUB.drain = orig_drain
            assert(not ok_dr and tostring(err_dr):find("subagent drain busy", 1, true), tostring(err_dr))

            -- 3d. Multi-turn tool call where turn-2 request_lease is refused covers agent/loop.lua lease denial.
            local orig_req_lease = C.cordis.kernel and C.cordis.kernel.request_lease
            if C.cordis.kernel and orig_req_lease then
                local lease_calls = 0
                C.cordis.kernel.request_lease = function(kself, tok)
                    lease_calls = lease_calls + 1
                    if lease_calls >= 2 then
                        return nil, "turn-2 lease refused by test"
                    end
                    return orig_req_lease(kself, tok)
                end
                local t_lr = A:prompt("lease deny multi-turn")
                local r_lr = A:wait(t_lr.turn_id)
                C.cordis.kernel.request_lease = orig_req_lease
                assert(r_lr.error ~= nil, "expected turn-2 lease refusal error")
            end

            -- 3e. Queued steer denied with abort=true inside consume_or_fail halts the loop.
            local queued_steer = false
            local off_ss = C.cordis:on("step/started", function()
                if not queued_steer then
                    queued_steer = true
                    A:steer("queued steer denied with abort")
                end
            end)
            local off_fv = C.cordis:get("policy"):voter("agent/input-ready", function(ev)
                if type(ev) == "table" and ev.delivery == "steer" then
                    return { decision = "deny", reason = "steer denied with abort", abort = true }
                end
                return { decision = "allow" }
            end)
            local t_ca = A:prompt("steer deny turn")
            local r_ca = A:wait(t_ca.turn_id)
            off_ss()
            off_fv()
            assert(r_ca.error ~= nil, "expected aborted error when queued steer is denied with abort")

            -- 4. session.persist_message error table triggers quarantine recording in agent/persist.
            local orig_pm = S.persist_message
            S.persist_message = function()
                return { error = "simulated disk full" }
            end
            local t_q = A:prompt("persist fault prompt")
            A:wait(t_q.turn_id)
            S.persist_message = orig_pm
            assert(A:persist_error() ~= nil, "expected persist_error after persist_message failure")
            -- Switching sessions while persist_error is pending fails cleanly in store_switch.
            local ok_pe, err_pe = pcall(function() return S:create() end)
            assert(not ok_pe and tostring(err_pe):find("persistence records are pending", 1, true), tostring(err_pe))

            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
        drop(env);

        // 5. Unknown CLI --agent type surfaces subagent unknown_type error on prompt.
        let env2 = TestEnv::new().await;
        let h2 = env2
            .host()
            .builtins(&["subagent"])
            .faux(json!([{ "text": "unused" }]))
            .args(json!({ "agent": "no_such_cli_agent_type" }))
            .build()
            .await;
        expose(&h2);
        let err_agent: bool = eval(
            &h2,
            r#"
            local ok_p, res = pcall(function()
                local t = A:prompt("hello")
                return t and t.turn_id and A:wait(t.turn_id) or t
            end)
            if not ok_p then
                return tostring(res):find("unknown subagent type", 1, true) ~= nil
            end
            return type(res) == "table" and tostring(type(res.error) == "table" and res.error.message or res.error or ""):find("unknown subagent type", 1, true) ~= nil
            "#,
        )
        .await;
        assert!(err_agent, "expected unknown subagent type error");
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn session_resume_warns_on_missing_model_effort_cwd_sandbox_dir_and_renders_tree_entries() {
    local(async {
        let env = TestEnv::new().await;
        let canon_cwd = dunce::canonicalize(&env.cwd).unwrap();
        let sess_dir = env.home.path().join("state/sessions").join(mare_store::cwd_slug(&canon_cwd));
        std::fs::create_dir_all(&sess_dir).unwrap();
        let cwd_str = canon_cwd.display().to_string();
        let missing_cwd = canon_cwd.join("deleted_shell_dir").display().to_string();
        let missing_sb = canon_cwd.join("deleted_sandbox_dir").display().to_string();

        // Write two sessions sharing prefix `ambig-cov-` to test ambiguous session lookup.
        let s1_path = sess_dir.join("2026-01-02T00-00-00Z_ambig-cov-one.jsonl");
        let s2_path = sess_dir.join("2026-01-01T00-00-00Z_ambig-cov-two.jsonl");
        let now_ms = 1760000000000i64;
        let s1_lines = [
            json!({
                "type": "session",
                "version": mare_store::CURRENT_VERSION,
                "session_id": "ambig-cov-one",
                "cwd": cwd_str,
                "created_at_ms": now_ms,
                "shell_cwd": missing_cwd
            }),
            json!({
                "type": "model",
                "at_entry_id": false,
                "model": "faux/nonexistent_model_xyz",
                "effort": "nonexistent_effort_xyz"
            }),
            json!({
                "type": "custom",
                "at_entry_id": false,
                "kind": "working-directory/change",
                "data": { "to": missing_cwd }
            }),
            json!({
                "type": "custom",
                "at_entry_id": false,
                "kind": "workspace/dirs",
                "provenance": "session-host",
                "data": { "dirs": [missing_sb] }
            }),
            json!({
                "type": "custom",
                "at_entry_id": false,
                "kind": "sandbox/mode",
                "provenance": "session-host",
                "data": { "level": "workspace-write", "writable": [missing_sb] }
            }),
            json!({
                "type": "message",
                "entry_id": "m1",
                "parent_entry_id": null,
                "created_at_ms": now_ms + 1,
                "message": { "role": "user", "content": [{ "type": "text", "text": "hello tree" }] }
            }),
            json!({
                "type": "message",
                "entry_id": "m2",
                "parent_entry_id": "m1",
                "created_at_ms": now_ms + 2,
                "message": { "role": "assistant", "content": [{ "type": "text", "text": "calling" }, { "type": "tool_call", "id": "tc1", "name": "read", "arguments": {} }] }
            }),
            json!({
                "type": "message",
                "entry_id": "m3",
                "parent_entry_id": "m2",
                "created_at_ms": now_ms + 3,
                "message": { "role": "tool_result", "tool_call_id": "tc1", "is_error": true, "content": [{ "type": "text", "text": "boom" }] }
            }),
            json!({
                "type": "message",
                "entry_id": "m4",
                "parent_entry_id": "m3",
                "created_at_ms": now_ms + 4,
                "message": { "role": "custom", "custom_type": "note", "display": true, "content": [{ "type": "text", "text": "custom entry" }] }
            }),
            json!({
                "type": "message",
                "entry_id": "m4b",
                "parent_entry_id": "m4",
                "created_at_ms": now_ms + 5,
                "message": { "role": "user", "turn_aborted": true, "content": [{ "type": "text", "text": "<turn_aborted>" }] }
            }),
            json!({
                "type": "message",
                "entry_id": "m4c",
                "parent_entry_id": "m4b",
                "created_at_ms": now_ms + 6,
                "message": { "role": "user", "context_disclosure": { "namespace": "skills" }, "content": [{ "type": "text", "text": "disclosed" }] }
            }),
            json!({
                "type": "branch_summary",
                "entry_id": "m4d",
                "parent_entry_id": "m4c",
                "created_at_ms": now_ms + 7,
                "summary": "branch note"
            }),
            json!({
                "type": "compaction",
                "entry_id": "m5",
                "parent_entry_id": "m4d",
                "created_at_ms": now_ms + 8,
                "summary": "summary text",
                "tokens_before": 1234,
                "first_kept_entry_id": "m4"
            }),
            json!({
                "type": "label",
                "target_entry_id": "m1",
                "label": "LBL"
            }),
        ];
        std::fs::write(&s1_path, s1_lines.iter().map(|v| v.to_string()).collect::<Vec<_>>().join("\n") + "\n").unwrap();
        std::fs::write(
            &s2_path,
            json!({ "type": "session", "version": mare_store::CURRENT_VERSION, "session_id": "ambig-cov-two", "cwd": cwd_str, "created_at_ms": now_ms - 10 }).to_string() + "\n",
        )
        .unwrap();

        let h = env.host().args(json!({ "resume": "ambig-cov-one" })).build().await;
        expose(&h);
        global(&h, "CMD", h.service_table("commands").unwrap());
        let ok: bool = eval(
            &h,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.notify = function() end
            -- Ambiguous prefix lookup and invalid query arguments fail cleanly.
            local ok_amb, err_amb = pcall(function() return S:get("ambig-cov-") end)
            assert(not ok_amb and tostring(err_amb):find("ambiguous", 1, true), tostring(err_amb))
            local ok_lim = pcall(function() return S:list({ limit = 0 }) end)
            assert(not ok_lim, "limit=0 must fail")
            local ok_opts = pcall(function() return S:list_items("bad") end)
            assert(not ok_opts, "non-table list_items opts must fail")
            local ok_br = pcall(function() return S:list_items({ branch = "invalid" }) end)
            assert(not ok_br, "invalid branch must fail")
            local ok_dir = pcall(function() return S:list_items({ direction = "sideways" }) end)
            assert(not ok_dir, "invalid direction must fail")
            local ok_ty = pcall(function() return S:list_items({ types = "message" }) end)
            assert(not ok_ty, "non-array types must fail")

            -- Current resumed session snapshot is retrievable by exact id.
            local got = S:get("ambig-cov-one")
            assert(got and got.session_id == "ambig-cov-one", tostring(got))

            -- Drive /tree through every filter mode and fold toggle via stubbed mare.ui.select.
            TREE_DONE = false
            TREE_ALL_ITEMS = 0
            TREE_FOLDED_ITEMS = 0
            local calls = 0
            mare.ui.select = function(opts)
                calls = calls + 1
                if opts and opts.title and opts.title:find("everything", 1, true) then
                    local count = #(opts.items or {})
                    if calls == 5 then
                        TREE_ALL_ITEMS = count
                        -- Fold the first entry to exercise descendant counting.
                        return 1, 1, "ctrl+e", ""
                    end
                    TREE_FOLDED_ITEMS = count
                    TREE_DONE = true
                    return nil
                end
                if calls <= 4 then
                    return 1, 1, "ctrl+f", ""
                end
                TREE_DONE = true
                return nil
            end
            CMD:run("/tree")
            return true
            "#,
        )
        .await;
        assert!(ok);
        wait_until(WAIT_SECS, "/tree selector completed", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return TREE_DONE == true").await }
        })
        .await;
        let (saw_all_items, saw_folded_items): (i64, i64) = eval(&h, "return TREE_ALL_ITEMS, TREE_FOLDED_ITEMS").await;
        assert!(saw_all_items >= 6, "expected /tree 'everything' filter to render all entry kinds, got {saw_all_items}");
        assert_eq!(saw_folded_items, 1, "folding the root entry must collapse its descendants into 1 visible row");

        let lh_ok: bool = eval(
            &h,
            r#"
            local lh_path = mare.cwd .. "/lh_missing_shell.jsonl"
            local lh_missing = mare.cwd .. "/no_such_shell_subdir_xyz"
            local lh_lines = table.concat({
                mare.json.encode({ type = "session", version = mare.store.format_version, session_id = "lh-cov-1", cwd = mare.cwd, created_at_ms = 1760000000000 }),
                mare.json.encode({ type = "custom", kind = "working-directory/change", data = { to = lh_missing } }),
            }, "\n") .. "\n"
            assert(mare.fs.write(lh_path, lh_lines))
            local lh_snap = S:load({ path = lh_path })
            return lh_snap ~= nil and lh_snap.session_id == "lh-cov-1"
            "#,
        )
        .await;
        assert!(lh_ok);

        // Cover Host frontend helpers for continue (completes pending context, then reports no_pending_work),
        // request-id abort, and follow-up variants.
        let cont = h.continue_turn().await.unwrap();
        assert_eq!(cont["status"], "no_pending_work");
        let _ = h.abort_with_inputs_for(Some("cov-abort-req")).await;
        let _ = h.follow_up_with("follow-up cov", json!({})).await;
        let _ = h.follow_up_with_cancel("follow-up cancel cov", json!({}), tokio_util::sync::CancellationToken::new()).await;
        // Also cover submit_with image payload conversion and non-typed Lua error propagation in Host frontend.
        h.submit_with("image submit cov", vec![json!({ "type": "image", "mime": "image/png", "data": "AAAA" })]).await;
        exec(&h, "A.steer = function() error('plain lua steer error') end").await;
        assert!(h.steer("trigger plain err").await.is_err());

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn repro_304_cancel_during_step_ready_before_admission_returns_not_sent() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "should never be reached" }]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            STEP_STARTED_TX, STEP_STARTED_RX = mare.task.channel()
            STEP_RELEASE_TX, STEP_RELEASE_RX = mare.task.channel()
            C.cordis:get("policy"):voter("agent/step-ready", function(ev)
                STEP_STARTED_TX:send(true)
                STEP_RELEASE_RX:recv()
                if ev.cancel and ev.cancel:is_cancelled() then
                    return { decision = "deny", reason = "cancelled before admit" }
                end
                return { decision = "allow" }
            end)
            "#,
        )
        .await;

        let started = eval_json(&h, "return A:prompt('unsent draft text')").await;
        let turn_id = started["turn_id"].as_str().unwrap().to_string();
        global(&h, "T_REPRO", turn_id);
        let entered: bool =
            eval(&h, "return mare.task.timeout(5000, function() return STEP_STARTED_RX:recv() end) == true").await;
        assert!(entered, "timed out waiting for agent/step-ready voter");

        // Cancel while parked in agent/step-ready BEFORE on_admit runs.
        exec(&h, "A:cancel(); STEP_RELEASE_TX:send(true)").await;
        let res = eval_json(&h, "return A:wait(T_REPRO)").await;
        let msg_count: i64 = eval(&h, "return #A:list_messages().data").await;
        h.shutdown().await;

        assert_eq!(msg_count, 0, "cancelled pre-admission prompt must stay out of history: {res}");
        assert_eq!(
            res["not_sent"], true,
            "repro #304: prompt cancelled before on_admit returned without not_sent=true (draft lost): {res}"
        );
    })
    .await
}

#[tokio::test]
async fn cancel_during_first_lease_before_admission_returns_not_sent() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "should never be reached" }]);
        let h = env.host().faux(script).build().await;
        expose(&h);
        exec(
            &h,
            r#"
            LEASE_ENTERED_TX, LEASE_ENTERED_RX = mare.task.channel()
            ORIG_REQ_LEASE = C.cordis.kernel.request_lease
            C.cordis.kernel.request_lease = function(kself, tok)
                LEASE_ENTERED_TX:send(true)
                if tok then
                    tok:wait()
                end
                C.cordis.kernel.request_lease = ORIG_REQ_LEASE
                return nil, "cancelled waiting for first lease"
            end
            "#,
        )
        .await;

        let started = eval_json(&h, "return A:prompt('unsent first-lease draft')").await;
        let turn_id = started["turn_id"].as_str().unwrap().to_string();
        global(&h, "T_LEASE", turn_id);
        let entered: bool =
            eval(&h, "return mare.task.timeout(5000, function() return LEASE_ENTERED_RX:recv() end) == true").await;
        assert!(entered, "timed out waiting for acquire_first_lease");

        // Cancel while waiting in acquire_first_lease BEFORE on_admit runs (with no input-ready voters).
        exec(&h, "A:cancel()").await;
        let res = eval_json(&h, "return A:wait(T_LEASE)").await;
        let msg_count: i64 = eval(&h, "return #A:list_messages().data").await;
        h.shutdown().await;

        assert_eq!(msg_count, 0, "cancelled first-lease prompt must stay out of history (no <turn_aborted>): {res}");
        assert_eq!(res["termination_reason"], "interrupted", "{res}");
        assert_eq!(
            res["not_sent"], true,
            "prompt cancelled during acquire_first_lease must return not_sent=true so draft is restored: {res}"
        );
    })
    .await
}
