//! Model-step participants: `agent:step_participant` / `step_state` / `step_preview`, the
//! `agent/step-ready` admission vote, the atomic `agent/step-state` + disclosure commit, and the
//! `step_id` / `turn_id` carried by `agent/turn-start`.
//! Atomic step participant preview/commit and lifecycle coverage. Scenarios also from reference implementation
//! packages/plan/plan-mode/tests/plan-mode.spec.ts:321 (state staged before the step starts),
//! :434 (a failed pre-step write is contained) and packages/context/time-context/tests/
//! time-context.spec.ts:167,211 (per-step durable context with turn / step identity).

use crate::common::*;
use serde_json::{Value, json};

#[tokio::test]
async fn transaction_step_sees_detached_requested_tool_names_without_raw_definitions() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text":"done" }])).build().await;
        expose(&h);
        let (result, observed): (String, String) = eval(
            &h,
            r#"
          local observed
          local remove = A:step_participant("test.tool-names", function(_, step)
            observed = mare.json.encode(step.tools)
            return {state={ok=true}, disclosures={{source="test", text="ok"}}}
          end, {output="transaction"})
          local turn = A:prompt("inspect", {tools={"read", {name="write"}}})
          local result = A:wait(turn.turn_id)
          remove()
          return result.text or "", observed or "missing"
        "#,
        )
        .await;
        assert_eq!(result, "done");
        assert_eq!(serde_json::from_str::<Value>(&observed).unwrap(), json!(["read"]), "{observed}");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn step_participants_preview_and_commit_atomically() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "step-ok" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            PREVIEWS, STARTS, REQUESTS = {}, {}, {}
            local rm_state = A:step_participant("demo.state", function(prev, step)
                return { count = (prev and prev.count or 0) + 1, turn = step.turn_id }
            end)
            local rm_tx = A:step_participant("demo.tx", function(prev, step)
                return {
                    state = { disclosed = (prev and prev.disclosed or 0) + 1 },
                    disclosures = { { source = "demo.tx", text = "step disclosure " .. tostring(step.step) } },
                }
            end, { output = "transaction" })
            C.cordis:on("step/started", function(ev)
                STARTS[#STARTS + 1] = { step_id = ev.step_id, turn_id = ev.turn_id, turn = ev.turn }
                local p, err = A:step_preview("demo.state", ev.step_id)
                local bad, bad_err = A:step_preview("demo.state", "wrong-step-id")
                PREVIEWS[#PREVIEWS + 1] = { preview = p, err = err, bad = bad, bad_err = bad_err }
            end)
            C.cordis:on("request/build", function(req, next)
                local seen = {}
                for _, m in ipairs(req.messages) do
                    if m.context_disclosure then
                        seen[#seen + 1] = { ns = m.context_disclosure.namespace, step_id = m.context_disclosure.step_id }
                    end
                end
                REQUESTS[#REQUESTS + 1] = { step_id = req.step_id, disclosures = seen }
                return next()
            end)
            local res = A:wait(A:prompt("hello step").turn_id)
            local st = A:step_state("demo.state")
            local tx_st = A:step_state("demo.tx")
            rm_state()
            rm_tx()
            local records = S:list_records("agent/step-state").data
            local disclosed_entries = 0
            for _, e in ipairs(S:entries()) do
                if e.type == "message" and e.message.context_disclosure then
                    disclosed_entries = disclosed_entries + 1
                end
            end
            return mare.json.encode({
                res_text = res.text,
                turn_id = res.turn_id,
                starts = STARTS,
                previews = PREVIEWS,
                requests = REQUESTS,
                state = st,
                tx_state = tx_st,
                msg_count = #A:list_messages().data,
                records = records,
                disclosed_entries = disclosed_entries,
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(r["res_text"], "step-ok");
        // STEP-EVT: step/started carries the step and run identity.
        let step_id = r["starts"][0]["step_id"].as_str().expect("step_id on step/started");
        assert_eq!(r["starts"][0]["turn_id"], r["turn_id"]);
        assert_eq!(r["previews"][0]["preview"]["count"], 1);
        assert!(r["previews"][0]["err"].is_null());
        assert!(r["previews"][0]["bad"].is_null());
        assert_eq!(r["previews"][0]["bad_err"], "inactive step preview");
        // the request carries the step id and the step's disclosure
        assert_eq!(r["requests"][0]["step_id"], step_id);
        assert_eq!(r["requests"][0]["disclosures"], json!([{ "ns": "demo.tx", "step_id": step_id }]));
        assert_eq!(r["state"]["count"], 1);
        assert_eq!(r["tx_state"]["disclosed"], 1);
        assert_eq!(r["msg_count"], 3, "user + disclosure + assistant");
        // one durable step record and the disclosure as a session message
        let records = r["records"].as_array().unwrap();
        assert_eq!(records.len(), 1, "{r}");
        assert_eq!(records[0]["step_id"], step_id);
        assert_eq!(records[0]["states"]["demo.state"]["count"], 1);
        assert_eq!(r["disclosed_entries"], 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn step_ready_denial_stops_the_run_before_any_request() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "must-not-be-sent" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            BUILT = 0
            C.cordis:get("policy"):voter("agent/step-ready", function(ev)
                assert(type(ev.id) == "string" and ev.step == 1)
                return { decision = "deny", reason = "STEP_POLICY_REFUSAL" }
            end)
            C.cordis:on("request/build", function(req, next) BUILT = BUILT + 1 return next() end)
            local res = A:wait(A:prompt("hello").turn_id)
            return mare.json.encode({ error = res.error, built = BUILT,
                records = #S:list_records("agent/step-state").data })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["error"]["message"].as_str().unwrap_or("").contains("STEP_POLICY_REFUSAL"), "{r}");
        assert_eq!(r["built"], 0);
        assert_eq!(r["records"], 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn required_participant_failure_fails_the_step_and_defer_failure_does_not() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "deferred-ok" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            BUILT = 0
            C.cordis:on("request/build", function(req, next) BUILT = BUILT + 1 return next() end)
            local rm = A:step_participant("demo.required", function() error("PREPARE_BROKE") end)
            local failed = A:wait(A:prompt("first").turn_id)
            rm()
            local rm_defer = A:step_participant("demo.defer", function() error("DEFER_BROKE") end,
                { failure_policy = "defer" })
            local rm_ok = A:step_participant("demo.ok", function(prev) return { n = 1 } end)
            local ok = A:wait(A:prompt("second").turn_id)
            rm_defer()
            rm_ok()
            return mare.json.encode({
                failed_error = failed.error, ok_text = ok.text, ok_error = ok.error, built = BUILT,
                ok_state = A:step_state("demo.ok"), defer_state = A:step_state("demo.defer"),
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["failed_error"]["message"].as_str().unwrap_or("").contains("PREPARE_BROKE"), "{r}");
        assert!(r["ok_error"].is_null(), "{r}");
        assert_eq!(r["ok_text"], "deferred-ok");
        assert_eq!(r["built"], 1, "only the second run reached the request");
        assert_eq!(r["ok_state"]["n"], 1);
        assert!(r["defer_state"].is_null());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn invalid_participant_registration_is_rejected() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([])).build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local function rejects(...)
                local ok = pcall(A.step_participant, A, ...)
                return not ok
            end
            local rm = A:step_participant("dup", function() end)
            local result = rejects("dup", function() end)
                and rejects("", function() end)
                and rejects("x", "not a function")
                and rejects("y", function() end, { failure_policy = "maybe" })
                and rejects("z", function() end, { output = "stream" })
                and rejects("w", function() end, { unknown = true })
            rm()
            -- after removal the name is free again
            local rm2 = A:step_participant("dup", function() end)
            rm2()
            return result
        "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn disclosure_and_step_state_project_registered_secrets() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "ok" }])).build().await;
        expose(&h);
        let raw_secret = "STEP_SECRET_VALUE_9999";
        let result_json: String = eval(
            &h,
            &format!(
                r#"
                local KEEP = mare.projection.register("{raw_secret}")
                local emitted = {{}}
                C.cordis:on("item/completed", function(ev)
                    if ev.item and ev.item.message then
                        emitted[#emitted + 1] = ev.item.message
                    end
                end)
                local rm = A:step_participant("demo.secret", function()
                    return {{
                        state = {{ token = "{raw_secret}" }},
                        disclosures = {{ {{ source = "ctx", text = "diff: {raw_secret}" }} }},
                    }}
                end, {{ output = "transaction" }})
                local res = A:wait(A:prompt("hello").turn_id)
                rm()
                local msgs = A:list_messages().data
                return mare.json.encode({{
                    err = res.error,
                    emitted = emitted,
                    msgs = msgs,
                    state = A:step_state("demo.secret"),
                }})
            "#
            ),
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["err"].is_null(), "{r}");
        let emitted_str = r["emitted"].to_string();
        assert!(!emitted_str.contains(raw_secret), "secret leaked in item/completed: {emitted_str}");
        let msgs_str = r["msgs"].to_string();
        assert!(!msgs_str.contains(raw_secret), "secret leaked in A.messages: {msgs_str}");
        let state_str = r["state"].to_string();
        assert!(!state_str.contains(raw_secret), "secret leaked in step_state: {state_str}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn step_ready_denial_keeps_rejected_prompt_and_queue_out_of_history() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "clean-reply" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            local allow = false
            local seen_prompts = {}
            C.cordis:on("request/build", function(req, next)
                seen_prompts[#seen_prompts + 1] = req.messages
                return next()
            end)
            C.cordis:get("policy"):voter("agent/step-ready", function()
                if not allow then
                    return { decision = "deny", reason = "env not ready" }
                end
                return { decision = "allow" }
            end)
            local denied = A:wait(A:prompt("rejected-first-prompt").turn_id)
            local after_deny_len = #A:list_messages().data
            allow = true
            local ok = A:wait(A:prompt("accepted-second-prompt").turn_id)
            return mare.json.encode({
                denied_error = denied.error,
                after_deny_len = after_deny_len,
                ok_text = ok.text,
                seen_prompts = seen_prompts,
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["denied_error"]["message"].as_str().unwrap_or("").contains("env not ready"), "{r}");
        assert_eq!(r["after_deny_len"], 0, "rejected prompt must not enter history: {r}");
        let seen_str = r["seen_prompts"].to_string();
        assert!(!seen_str.contains("rejected-first-prompt"), "rejected prompt leaked into later request: {seen_str}");
        assert!(seen_str.contains("accepted-second-prompt"), "{seen_str}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn step_committed_sees_disclosure_state_and_fires_for_pure_state_tx() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "tx-ok" }, { "text": "state-ok" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            local seen_in_committed = {}
            C.cordis:on("agent/step-committed", function(ev)
                seen_in_committed[#seen_in_committed + 1] = {
                    step = ev.step,
                    tx = A:step_state("demo.tx"),
                    st = A:step_state("demo.state"),
                }
            end)
            local rm_tx = A:step_participant("demo.tx", function()
                return {
                    state = { last_id = "d1" },
                    disclosures = { { source = "hooks", text = "ctx" } },
                }
            end, { output = "transaction" })
            local r1 = A:wait(A:prompt("first").turn_id)
            rm_tx()

            local rm_st = A:step_participant("demo.state", function()
                return { count = 7 }
            end)
            local r2 = A:wait(A:prompt("second").turn_id)
            rm_st()

            return mare.json.encode({
                e1 = r1.error,
                e2 = r2.error,
                seen = seen_in_committed,
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["e1"].is_null() && r["e2"].is_null(), "{r}");
        let seen = r["seen"].as_array().unwrap();
        assert_eq!(seen.len(), 2, "agent/step-committed must fire for both tx and pure state commits: {r}");
        assert_eq!(seen[0]["tx"]["last_id"], "d1", "tx state must be visible inside agent/step-committed: {r}");
        assert_eq!(seen[1]["st"]["count"], 7, "pure state tx must fire agent/step-committed: {r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn readonly_session_commits_required_disclosure_in_memory() {
    local(async {
        let env = TestEnv::new().await;
        let h_owner = env.host().faux(json!([{ "text": "seeded" }])).build().await;
        expose(&h_owner);
        exec(&h_owner, r#"A:wait(A:prompt("seed").turn_id)"#).await;
        let path: String = eval(&h_owner, "return S:info().path").await;
        h_owner.shutdown().await;

        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(mare_store::lock_path(std::path::Path::new(&path)))
            .unwrap();
        lock.try_lock().unwrap();
        let h_ro = env.host().faux(json!([{ "text": "ro-ok" }])).build().await;
        expose(&h_ro);
        global(&h_ro, "PATH", path.clone());
        let result_json: String = eval(
            &h_ro,
            r#"
            mare.ui.connected = function() return true end
            mare.ui.select = function() return 2 end
            S:load({ path = PATH })
            local info = S:info()
            assert(info.readonly, "expected readonly session")
            local rm = A:step_participant("demo.required_ro", function()
                return {
                    state = { ok = true },
                    disclosures = { { source = "time", text = "now" } },
                }
            end, { output = "transaction", failure_policy = "required" })
            local res = A:wait(A:prompt("hi").turn_id)
            rm()
            return mare.json.encode({ text = res.text, err = res.error })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["err"].is_null(), "readonly session must commit required disclosure in memory: {r}");
        assert_eq!(r["text"], "ro-ok", "{r}");

        drop(lock);
        h_ro.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn step_ready_denial_on_second_step_preserves_follow_up_in_queue() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "first-ok" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            local queued_once = false
            C.cordis:on("request/build", function(req, next)
                if not queued_once then
                    queued_once = true
                    A:prompt("queued-follow-up", { when_busy = "queue" })
                end
                return next()
            end)
            C.cordis:get("policy"):voter("agent/step-ready", function(ev)
                if ev.step >= 2 then
                    return { decision = "deny", reason = "step 2 denied" }
                end
                return { decision = "allow" }
            end)
            local res = A:wait(A:prompt("first-step").turn_id)
            local q = {}
            for _, item in ipairs(A:list_queue().data) do
                if item.lane == "follow_up" then q[#q + 1] = item end
            end
            local msgs = A:list_messages().data
            return mare.json.encode({
                err = res.error,
                follow_len = #q,
                follow_text = q[1] and q[1].text or nil,
                msgs = msgs,
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["err"]["message"].as_str().unwrap_or("").contains("step 2 denied"), "{r}");
        assert_eq!(r["follow_len"], 1, "denied step 2 must leave follow-up in queue: {r}");
        assert_eq!(r["follow_text"], "queued-follow-up", "{r}");
        assert!(!r["msgs"].to_string().contains("queued-follow-up"), "follow-up must not enter history: {r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn terminate_tool_preserves_stopping_and_follow_up_across_step_ready() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "tool_calls": [{ "name": "term_probe", "arguments": {} }] },
                { "text": "continued-after-terminate" },
                { "tool_calls": [{ "name": "term_probe", "arguments": {} }] },
            ]))
            .build()
            .await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            T:register("term_probe", {
                description = "terminating probe",
                parameters = { type = "object", properties = {} },
                execute = function()
                    return { content = "ok", terminate = true }
                end,
            })
            C.cordis:on("agent/turn-stopping", function(ev)
                if ev.turn == 1 then
                    return { message = "stopping-continue" }
                end
            end)
            local res1 = A:wait(A:prompt("run-1").turn_id)
            local msgs1 = A:list_messages().data

            local queued_once = false
            C.cordis:on("request/build", function(req, next)
                if not queued_once then
                    queued_once = true
                    A:prompt("follow-after-terminate", { when_busy = "queue" })
                end
                return next()
            end)
            local saw_deny = false
            C.cordis:get("policy"):voter("agent/step-ready", function(ev)
                if not saw_deny and ev.step >= 2 then
                    saw_deny = true
                    return { decision = "deny", reason = "deny after terminate" }
                end
                return { decision = "allow" }
            end)
            local starts = 0
            local restarted_tx, restarted_rx = mare.task.channel()
            C.cordis:on("turn/started", function()
                starts = starts + 1
                if starts > 1 then restarted_tx:send(true) end
            end)
            local res2 = A:wait(A:prompt("run-2").turn_id)
            -- A bounded wait verifies that deferred wakeups do not start another turn.
            local _, restarted = mare.task.select(
                function() return restarted_rx:recv() end,
                function() mare.task.sleep(250); return false end
            )
            assert(not restarted, "a denied step must not restart its pending follow-up")
            local q2 = {}
            for _, item in ipairs(A:list_queue().data) do
                if item.lane == "follow_up" then q2[#q2 + 1] = item end
            end
            local msgs2 = A:list_messages().data
            return mare.json.encode({
                res1_text = res1.text,
                msgs1 = msgs1,
                res2_err = res2.error,
                follow_len = #q2,
                follow_text = q2[1] and q2[1].text or nil,
                msgs2 = msgs2,
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(r["res1_text"], "continued-after-terminate", "{r}");
        assert!(
            r["msgs1"].to_string().contains("stopping-continue"),
            "stopping message must be injected after terminate: {r}"
        );
        assert!(r["res2_err"]["message"].as_str().unwrap_or("").contains("deny after terminate"), "{r}");
        assert_eq!(r["follow_len"], 1, "denied step after terminate must keep follow-up in queue: {r}");
        assert_eq!(r["follow_text"], "follow-after-terminate", "{r}");
        assert!(!r["msgs2"].to_string().contains("follow-after-terminate"), "{r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn step_participants_may_run_async_fs_read_while_keeping_json_isolation() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("step-fixture.txt"), "from-disk-value").unwrap();
        let h = env.host().faux(json!([{ "text": "async-ok" }, { "text": "isolated-ok" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            local mutable = { nested = { value = "before" } }
            local rm_state = A:step_participant("demo.async_state", function()
                local body = assert(mare.fs.read("step-fixture.txt"))
                mutable.nested.value = body
                return mutable
            end)
            local rm_tx = A:step_participant("demo.async_tx", function()
                local body = assert(mare.fs.read("step-fixture.txt"))
                return {
                    state = { body = body },
                    disclosures = { { source = "demo.async_tx", text = "disk:" .. body } },
                }
            end, { output = "transaction" })
            local r1 = A:wait(A:prompt("first").turn_id)
            mutable.nested.value = "mutated-after"
            local st_after = A:step_state("demo.async_state")
            local tx_after = A:step_state("demo.async_tx")
            rm_state()
            rm_tx()

            local rm_cycle = A:step_participant("demo.cycle", function()
                local t = {}
                t.self = t
                return t
            end, { failure_policy = "required" })
            local r_cycle = A:wait(A:prompt("cycle").turn_id)
            rm_cycle()

            return mare.json.encode({
                r1_text = r1.text,
                r1_err = r1.error,
                st_after = st_after,
                tx_after = tx_after,
                cycle_err = r_cycle.error,
            })
        "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert!(r["r1_err"].is_null(), "async fs.read in step_participant must succeed: {r}");
        assert_eq!(r["r1_text"], "async-ok", "{r}");
        assert_eq!(
            r["st_after"]["nested"]["value"], "from-disk-value",
            "state must be detached from caller table: {r}"
        );
        assert_eq!(r["tx_after"]["body"], "from-disk-value", "{r}");
        assert!(
            r["cycle_err"]["message"].as_str().is_some_and(|e| !e.is_empty()),
            "cyclic return from step_participant must still fail JSON capture: {r}"
        );
        h.shutdown().await;
    })
    .await
}

/// Calling `agent.wait(turn_id)` or `agent.wait()` while a `turn/completed` listener is still
/// running (`s.settling > 0`, `A.busy == false`) must wait for full settlement rather than
/// failing with `unknown turn_id` or returning a prior turn.
#[tokio::test]
async fn agent_wait_during_turn_completed_listener_waits_for_settlement() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().faux(json!([{ "text": "settled-ok" }])).build().await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local listener_done = false
            C.cordis:on("turn/completed", function(ev)
                entered_tx:send(ev.turn_id)
                release_rx:recv()
                listener_done = true
            end)

            local started = A:prompt("wait during settle")
            local tid = assert(entered_rx:recv(), "turn/completed listener must run")

            local explicit_tx, explicit_rx = mare.task.channel()
            local implicit_tx, implicit_rx = mare.task.channel()
            C.cordis:spawn(function()
                local ok, res = pcall(A.wait, A, tid)
                explicit_tx:send({ ok = ok, done = listener_done, res = res })
            end)
            C.cordis:spawn(function()
                local ok, res = pcall(A.wait, A)
                implicit_tx:send({ ok = ok, done = listener_done, res = res })
            end)

            mare.task.yield()
            mare.task.yield()
            release_tx:send(true)

            local explicit = assert(explicit_rx:recv())
            local implicit = assert(implicit_rx:recv())
            return mare.json.encode({
                tid = tid,
                started_tid = started.turn_id,
                explicit_ok = explicit.ok,
                explicit_done = explicit.done,
                explicit_tid = type(explicit.res) == "table" and explicit.res.turn_id or tostring(explicit.res),
                explicit_text = type(explicit.res) == "table" and explicit.res.text or nil,
                implicit_ok = implicit.ok,
                implicit_done = implicit.done,
                implicit_tid = type(implicit.res) == "table" and implicit.res.turn_id or tostring(implicit.res),
                implicit_text = type(implicit.res) == "table" and implicit.res.text or nil,
            })
            "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(r["tid"], r["started_tid"], "{r}");
        assert_eq!(r["explicit_ok"], true, "explicit A:wait(turn_id) during turn/completed failed: {r}");
        assert_eq!(r["explicit_done"], true, "explicit A:wait(turn_id) returned before turn/completed settled: {r}");
        assert_eq!(r["explicit_tid"], r["tid"], "{r}");
        assert_eq!(r["explicit_text"], "settled-ok", "{r}");
        assert_eq!(r["implicit_ok"], true, "implicit A:wait() during turn/completed failed: {r}");
        assert_eq!(r["implicit_done"], true, "implicit A:wait() returned before turn/completed settled: {r}");
        assert_eq!(r["implicit_tid"], r["tid"], "{r}");
        assert_eq!(r["implicit_text"], "settled-ok", "{r}");
        h.shutdown().await;
    })
    .await
}

/// Switching sessions in-place must reset cached turn results and `last_turn_id` so the new
/// session never observes turns from the previous session, and late turn completions from a
/// pre-switch session must not pollute the new session's cache.
#[tokio::test]
async fn agent_wait_resets_across_session_switch_and_drops_late_turn_records() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "session-a-answer" },
                { "text": "session-b-answer" }
            ]))
            .build()
            .await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            S:create()
            local turn_a = A:prompt("in session A").turn_id
            local res_a = A:wait(turn_a)
            assert(res_a.text == "session-a-answer")

            -- Switch to fresh session B: neither implicit wait() nor A's turn_id may resolve.
            S:switch({ kind = "new" })
            local ok_b_implicit, err_b_implicit = pcall(A.wait, A)
            local ok_b_old_id, err_b_old_id = pcall(A.wait, A, turn_a)

            -- Start a turn in B whose agent/settled listener switches to C before publish_end
            -- writes the turn record. Late completion from B must not leak into C.
            local switched_tx, switched_rx = mare.task.channel()
            local did_switch = false
            C.cordis:on("agent/settled", function(ev)
                if not did_switch and ev.busy == false then
                    did_switch = true
                    S:switch({ kind = "new" })
                    switched_tx:send(ev.turn_id)
                end
            end)
            local turn_b = A:prompt("in session B").turn_id
            local settled_b = assert(switched_rx:recv(), "expected agent/settled to switch to C")
            assert(settled_b == turn_b)
            -- Let B's publish_end finish after agent/settled returns.
            mare.task.yield()
            mare.task.yield()

            local ok_c_implicit, err_c_implicit = pcall(A.wait, A)
            local ok_c_old_id, err_c_old_id = pcall(A.wait, A, turn_b)
            return mare.json.encode({
                ok_b_implicit = ok_b_implicit,
                err_b_implicit = tostring(err_b_implicit),
                ok_b_old_id = ok_b_old_id,
                err_b_old_id = tostring(err_b_old_id),
                ok_c_implicit = ok_c_implicit,
                err_c_implicit = tostring(err_c_implicit),
                ok_c_old_id = ok_c_old_id,
                err_c_old_id = tostring(err_c_old_id),
            })
            "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(r["ok_b_implicit"], false, "implicit wait() in fresh session B must fail: {r}");
        assert!(
            r["err_b_implicit"].as_str().unwrap_or("").contains("no active or completed turn"),
            "unexpected error for implicit wait in B: {r}"
        );
        assert_eq!(r["ok_b_old_id"], false, "explicit wait(turn_a) in session B must fail: {r}");
        assert!(
            r["err_b_old_id"].as_str().unwrap_or("").contains("unknown turn_id"),
            "unexpected error for old turn_id in B: {r}"
        );
        assert_eq!(r["ok_c_implicit"], false, "late record from B must not populate C's last_turn_id: {r}");
        assert_eq!(r["ok_c_old_id"], false, "late record from B must not populate C's turns cache: {r}");
        h.shutdown().await;
    })
    .await
}

/// Calling implicit `agent.wait()` while an `agent/settled` listener is still running (after
/// `A.turn_id` was cleared and `s.settling` dropped to 0) must still target the currently settling
/// turn rather than falling back to the previous turn's `last_turn_id`.
#[tokio::test]
async fn agent_wait_during_agent_settled_listener_waits_for_current_turn() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "turn-one" },
                { "text": "turn-two" }
            ]))
            .build()
            .await;
        expose(&h);
        let result_json: String = eval(
            &h,
            r#"
            -- Run turn 1 so s.last_turn_id is non-nil before turn 2 settles.
            local t1 = A:prompt("first turn").turn_id
            local r1 = A:wait(t1)
            assert(r1.text == "turn-one")

            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local listener_done = false
            C.cordis:on("agent/settled", function(ev)
                if ev.turn_id ~= t1 then
                    entered_tx:send(ev.turn_id)
                    release_rx:recv()
                    listener_done = true
                end
            end)

            local t2 = A:prompt("second turn").turn_id
            local settled_tid = assert(entered_rx:recv(), "agent/settled listener must run for turn 2")

            local implicit_tx, implicit_rx = mare.task.channel()
            local explicit_tx, explicit_rx = mare.task.channel()
            C.cordis:spawn(function()
                local ok, res = pcall(A.wait, A)
                implicit_tx:send({ ok = ok, done = listener_done, res = res })
            end)
            C.cordis:spawn(function()
                local ok, res = pcall(A.wait, A, settled_tid)
                explicit_tx:send({ ok = ok, done = listener_done, res = res })
            end)

            mare.task.yield()
            mare.task.yield()
            release_tx:send(true)

            local implicit = assert(implicit_rx:recv())
            local explicit = assert(explicit_rx:recv())
            return mare.json.encode({
                t1 = t1,
                t2 = t2,
                settled_tid = settled_tid,
                implicit_ok = implicit.ok,
                implicit_done = implicit.done,
                implicit_tid = type(implicit.res) == "table" and implicit.res.turn_id or tostring(implicit.res),
                implicit_text = type(implicit.res) == "table" and implicit.res.text or nil,
                explicit_ok = explicit.ok,
                explicit_done = explicit.done,
                explicit_tid = type(explicit.res) == "table" and explicit.res.turn_id or tostring(explicit.res),
                explicit_text = type(explicit.res) == "table" and explicit.res.text or nil,
            })
            "#,
        )
        .await;
        let r: Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(r["settled_tid"], r["t2"], "{r}");
        assert_eq!(r["implicit_ok"], true, "implicit A:wait() during agent/settled failed: {r}");
        assert_eq!(r["implicit_done"], true, "implicit A:wait() woke before agent/settled finished: {r}");
        assert_eq!(r["implicit_tid"], r["t2"], "implicit A:wait() returned stale previous turn instead of turn 2: {r}");
        assert_eq!(r["implicit_text"], "turn-two", "{r}");
        assert_eq!(r["explicit_ok"], true, "explicit A:wait(t2) during agent/settled failed: {r}");
        assert_eq!(r["explicit_done"], true, "explicit A:wait(t2) woke before agent/settled finished: {r}");
        assert_eq!(r["explicit_tid"], r["t2"], "{r}");
        assert_eq!(r["explicit_text"], "turn-two", "{r}");
        h.shutdown().await;
    })
    .await
}
