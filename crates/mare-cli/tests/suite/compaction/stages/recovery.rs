use super::*;

/// Re-summarizing only the previous checkpoint preserves a usable log range across resume.
#[tokio::test]
async fn a_previous_summary_can_be_recompressed_with_a_durable_recovery_range() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": {
            "auto": false, "keep_recent_tokens": 1, "recent_user_tokens": 0, "stages": ["local"],
            "model_thresholds": { "faux/echo2": { "tokens": base + 1000 } } } } });
        let script = json!([{ "text": "a1" }, { "text": "a2" },
            { "text": "previous summary ".repeat(500) }, { "text": "turn prefix" },
            { "text": "SHORT-SUMMARY" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg.clone()).build().await;
        h.prompt(&"first ".repeat(2000), json!({})).await.unwrap();
        h.prompt(&"second ".repeat(1700), json!({})).await.unwrap();
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        let (first, first_error, second, second_error, path, replies): (bool, String, bool, String, String, String) = eval(
            &h,
            r#"
            local first, first_error = pcall(K.run, K)
            local replies, complete = {}, L.complete
            L.complete = function(self, req)
                local res = complete(self, req)
                replies[#replies + 1] = #(L:text_of(res) or "")
                return res
            end
            -- A smaller target now recompresses the checkpoint during model admission.
            local second, second_error = pcall(L.set_model, L, "faux/echo2")
            return first and first_error.status == "compacted", tostring(first_error), second == true, tostring(second_error), S:info().path, mare.json.encode(replies)"#,
        )
        .await;
        assert!(first && second, "first: {first_error}; second: {second_error}; reply lengths: {replies}");
        h.shutdown().await;
        let entries: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|entry: &serde_json::Value| entry["type"] == "compaction")
            .collect();
        assert_eq!(entries.len(), 2);
        let old_id = entries[0]["entry_id"].as_str().unwrap();
        assert_eq!(entries[1]["range"]["from_entry_id"], old_id);
        assert_eq!(entries[1]["range"]["to_entry_id"], old_id);
        assert!(
            entries[1]["summary"].as_str().unwrap().contains(old_id),
            "the recovery footer points to the old checkpoint"
        );
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).resume().build().await;
        expose(&h);
        let resumed: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(resumed.contains("SHORT-SUMMARY") && resumed.contains(old_id), "{resumed}");
        h.shutdown().await;
    })
    .await
}

/// An extension may recover after the built-in local stage exhausts its candidates.
#[tokio::test]
async fn a_later_custom_stage_can_recover_after_local_candidates_fail() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let summary = json!({ "text": "large but valid summary ".repeat(1000) });
        let cfg = json!({ "plugins": { "compact": { "threshold_tokens": (base + 2200) * 10 / 9,
            "keep_recent_tokens": 1, "recent_user_tokens": 100, "stages": ["local", "rescue"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([summary, summary, summary])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;
        let (ok, attempts, rescued, stalled, requests, ranges_differ): (bool, i64, bool, bool, i64, bool) = eval(
            &h,
            r#"
            ATTEMPTS, RESCUED, STALLED = 0, false, false
            local requests, complete = {}, L.complete
            L.complete = function(self, req)
                if req.purpose == "compact" then requests[#requests + 1] = mare.json.encode(req.messages) end
                return complete(self, req)
            end
            C.cordis:on("compact/stage", function(ev) if ev.stage == "local" and ev.phase == "candidate" then ATTEMPTS = ATTEMPTS + 1 end end)
            C.cordis:on("compact/completed", function(ev) if ev.stalled then STALLED = true end end)
            K:register_stage("rescue", function()
                RESCUED = true
                A:set_messages({})
                return true
            end, { summary = true })
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("first ", 3000) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("answer ", 3000) } } },
                { role = "user", content = { { type = "text", text = string.rep("last ", 300) } } },
            })
            local after = nil
            C.cordis:on("compact/completed", function(ev) after = ev end)
            PREPARE_REQUEST()
            return after and after.status == "completed", ATTEMPTS, RESCUED, STALLED, #requests,
                #requests == 2 and requests[1] ~= requests[2]"#,
        )
        .await;
        assert!(ok && rescued, "the extension successfully reduced the context");
        assert_eq!(attempts, 3, "the built-in stage exhausted its candidates first");
        assert_eq!(requests, 2, "equal ranges share a request, changed ranges do not");
        assert!(ranges_differ, "distinct requests must contain different summary inputs");
        assert!(!stalled, "a recovered run must not trip the circuit breaker");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn register_stage_out_of_order_disposal_does_not_resurrect_or_leak_summary() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "stages": ["custom", "tail"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (r1, r2, r3, r4, r5): (String, String, String, String, String) = eval(
            &h,
            r#"
            local calls = {}
            K:register_stage("tail", function()
                calls[#calls + 1] = "tail"
                return true
            end)
            local function run_once()
                calls = {}
                K:run()
                return table.concat(calls, ",")
            end

            -- Case 1: out-of-order disposal (register A, register B, dispose A, dispose B)
            local offA = K:register_stage("custom", function()
                calls[#calls + 1] = "A"
                return true
            end, { summary = true })
            local offB = K:register_stage("custom", function()
                calls[#calls + 1] = "B"
                return true
            end, { summary = false })
            local r1 = run_once() -- B active (not summary) -> B,tail
            offA()
            local r2 = run_once() -- A disposed, B still active -> B,tail
            offB()
            local r3 = run_once() -- B disposed, A must NOT resurrect -> tail

            -- Case 2: in-order disposal restores A and its SUMMARY_STAGES flag, then clears both
            local offA2 = K:register_stage("custom", function()
                calls[#calls + 1] = "A2"
                return true
            end, { summary = true })
            local offB2 = K:register_stage("custom", function()
                calls[#calls + 1] = "B2"
                return true
            end, { summary = false })
            offB2()
            local r4 = run_once() -- A2 restored with summary=true -> stops before tail -> A2
            offA2()
            local r5 = run_once() -- both disposed -> tail

            return r1, r2, r3, r4, r5"#,
        )
        .await;
        assert_eq!(r1, "B,tail", "B should override A and not be a summary stage");
        assert_eq!(r2, "B,tail", "disposing A out of order must not clobber B");
        assert_eq!(r3, "tail", "disposing B must not resurrect already-disposed A");
        assert_eq!(r4, "A2", "disposing B2 must restore A2 and its summary=true flag");
        assert_eq!(r5, "tail", "disposing A2 must clean up custom stage and summary flag");
        h.shutdown().await;
    })
    .await
}

/// Overflow retries when pre-summary pruning persisted progress even if the summary stage fails
/// .
#[tokio::test]
async fn overflow_retries_when_prune_persisted_progress_even_if_summary_fails() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("big.txt"), "line of filler text\n".repeat(3000)).unwrap();
        let cfg = json!({ "plugins": { "compact": {
            "auto": false, "keep_recent_tokens": 1, "prune_tool_result_chars": 2000,
            "prune_protect_tokens": 100, "prune_min_saving_tokens": 1000,
            "stages": ["prune", "local"] } } });
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let summary_fail = json!({ "error": "summarizer down", "retryable": false });
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "big.txt" } }] },
            { "text": "read it" },
            overflow,
            summary_fail,
            { "text": "after-prune-retry" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).build().await;
        expose(&h);
        h.prompt("read big.txt", json!({})).await.unwrap();
        let res = h.prompt("continue", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["text"], "after-prune-retry", "{res}");
        let (pruned, path): (bool, String) = eval(
            &h,
            "return mare.json.encode(A:list_messages().data):find('tool result middle pruned', 1, true) ~= nil, S:info().path",
        )
        .await;
        assert!(pruned, "the pruned tool result must stay in effect");
        h.shutdown().await;
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.lines().any(|l| l.contains(r#""type":"prune""#)), "prune must be persisted");
        assert!(!raw.lines().any(|l| l.contains(r#""type":"compaction""#)), "summary failed so no compaction entry");
    })
    .await
}

/// b20 CMP1b: persisted pruning progress does not turn a later stage's exception into success.
/// The stage after `prune` raises (as the image offload's mkdir does on a read-only attachment
/// directory): the error is reported, the pruning stays committed, and the agent does not send
/// the request again. Reference test: reference implementation
/// packages/compaction/compaction-basic/tests/manual-compaction.spec.ts:620 (a commit failure is
/// reported, not retried over).
#[tokio::test]
async fn overflow_reports_a_stage_exception_even_after_persisted_progress() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("big.txt"), "line of filler text\n".repeat(3000)).unwrap();
        let cfg = json!({ "plugins": { "compact": {
            "auto": false, "keep_recent_tokens": 1, "prune_tool_result_chars": 2000,
            "prune_protect_tokens": 100, "prune_min_saving_tokens": 1000,
            "stages": ["prune", "boom"] } } });
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "big.txt" } }] },
            { "text": "read it" },
            overflow,
            { "text": "MUST NOT BE SENT AGAIN" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        h.prompt("read big.txt", json!({})).await.unwrap();
        exec(
            &h,
            r#"AFTER = {}
               AGENT_REQUESTS = 0
               local stream = L.stream
               L.stream = function(self, req, sink)
                   if req.purpose == "agent" then AGENT_REQUESTS = AGENT_REQUESTS + 1 end
                   return stream(self, req, sink)
               end
               C.cordis:on("compact/completed", function(e) AFTER[#AFTER + 1] = e end)
               K:register_stage("boom", function() error("mkdir: permission denied", 0) end)"#,
        )
        .await;
        let res = h.prompt("continue", json!({})).await.unwrap();
        assert!(res["error"]["message"].is_string(), "the turn ends with an error, not a retried request: {res}");
        let (compact_ok, compact_code, compact_err): (bool, String, String) = eval(
            &h,
            "local e = AFTER[#AFTER]; return e.status == 'completed', type(e.error) == 'table' and tostring(e.error.code or '') or '', type(e.error) == 'table' and tostring(e.error.message or '') or tostring(e.error)",
        )
        .await;
        assert!(!compact_ok, "the overflow compaction must report failure");
        assert_eq!(compact_code, "compact.not_compacted", "compact/completed error must be structured");
        assert!(compact_err.contains("mkdir: permission denied"), "the stage error is reported: {compact_err}");
        assert_ne!(res["text"], "MUST NOT BE SENT AGAIN", "the request must not be retried: {res}");
        // Counted at the LLM service — the overflowed request is the turn's only one.
        assert_eq!(eval::<i64>(&h, "return AGENT_REQUESTS").await, 1, "no model request after the failed compaction");
        let (pruned, path): (bool, String) = eval(
            &h,
            "return mare.json.encode(A:list_messages().data):find('tool result middle pruned', 1, true) ~= nil, S:info().path",
        )
        .await;
        assert!(pruned, "the committed pruning stays in effect");
        h.shutdown().await;
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.lines().any(|l| l.contains(r#""type":"prune""#)), "prune stays persisted");
    })
    .await
}

/// Counter-example: without persisted progress, overflow compaction must not retry or loop
/// .
#[tokio::test]
async fn overflow_does_not_loop_when_no_progress_is_persisted() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "stages": ["noop", "local"] } } });
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([overflow, overflow, { "text": "must-not-reach" }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"AGENT_CALLS = 0
               local stream = L.stream
               L.stream = function(self, req, sink)
                   if req.purpose ~= "compact" then AGENT_CALLS = AGENT_CALLS + 1 end
                   return stream(self, req, sink)
               end
               K:register_stage("noop", function() return true end)
               K:register_stage("local", function() return false, "no summary" end, { summary = true })"#,
        )
        .await;
        let res = h.prompt("hello", json!({})).await.unwrap();
        let calls: i64 = eval(&h, "return AGENT_CALLS").await;
        assert_eq!(calls, 1, "no progress must not retry or loop: {res}");
        assert!(res["error"]["message"].as_str().unwrap_or_default().contains("context length exceeded"), "{res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn measure_propagates_error_instead_of_returning_zero() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "CTX", h.service_table("context").unwrap());
        let (ok1, err1, ok2, err2): (bool, String, bool, String) = eval(
            &h,
            r#"CTX.system_prompt = function() error("measure-boom") end
               local ok1, err1 = pcall(K.measure, K)
               local ok2, err2 = pcall(K.measure, K, nil, { by_source = true })
               return ok1, tostring(err1), ok2, tostring(err2)"#,
        )
        .await;
        assert!(!ok1 && err1.contains("measure-boom"), "measure() must fail loudly: ok={ok1} err={err1}");
        assert!(
            !ok2 && err2.contains("measure-boom"),
            "measure({{by_source=true}}) must fail loudly: ok={ok2} err={err2}"
        );
        h.shutdown().await;
    })
    .await
}

/// A stage leaves `measure()` throwing after `busy` was taken: the error surfaces and the next run
/// is admitted. What releases `busy` on this path is the admission's `<close>` guard
/// (`busy_guard` in `compact/stages.lua`, from ADMIT); with that guard removed this test fails
/// with "already compacting" (b1-1520 CMP1b #2).
/// Reference test: reference implementation
/// (admission is released after a summarizer failure and the next request runs).
#[tokio::test]
async fn compaction_clears_busy_when_post_stage_measure_throws_and_allows_retry() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "a1" }, { "text": "a2" }, { "text": "## Summary\nRETRY-AFTER-MEASURE-FAIL" }]);
        let h = env
            .host().builtins(&["compact", "usage"])
            .faux(script)
            .config(json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } }))
            .no_session()
            .build()
            .await;
        h.prompt(&"one detailed request ".repeat(200), json!({})).await.unwrap();
        h.prompt("two", json!({})).await.unwrap();
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "CTX", h.service_table("context").unwrap());
        let (ok1, err1, ok2, err2, text): (bool, String, bool, String, String) = eval(
            &h,
            r#"local orig_sp = CTX.system_prompt
               local fail_once, fail_measure = true, false
               CTX.system_prompt = function(self, caller, model)
                   if fail_measure then
                       error("post-stage-measure-boom")
                   end
                   return orig_sp(self, caller, model)
               end
               K:register_stage("prune", function()
                   if fail_once then
                       fail_once = false
                       fail_measure = true
                   end
                   return false
               end)
               local ok1, err1 = pcall(K.run, K, nil)
               fail_measure = false
               local ok2, err2 = pcall(K.run, K)
               return ok1, tostring(err1), ok2 and err2.status == "compacted", tostring(err2), mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(
            !ok1 && err1.contains("post-stage-measure-boom"),
            "first run should surface measure error: ok={ok1} err={err1}"
        );
        assert!(ok2, "second run after measure failure must not be stuck busy: {err2}");
        assert!(text.contains("RETRY-AFTER-MEASURE-FAIL"), "{text}");
        h.shutdown().await;
    })
    .await
}

/// Overflow progress must compare local message estimates on the same scale before and after
/// projection, never provider-reported usage before vs local estimate after:
/// - grew (e.g. prune_tool_result_chars=2000 with default 4096 head / 1024 tail on a ~3000-char
///   tool result): must NOT report progressed
/// - unchanged: must NOT report progressed
/// - shrank: MUST report progressed
#[tokio::test]
async fn overflow_progressed_compares_local_estimate_consistently_across_grew_same_and_shrank() {
    local(async {
        let env = TestEnv::new().await;
        // 1. Actual growth: ~3000-char tool result with prune_tool_result_chars=2000 and default
        //    head=4096 / tail=1024 duplicates overlapping head+tail when force-pruned on overflow.
        let cfg_grew = json!({ "plugins": { "compact": {
            "auto": false,
            "keep_recent_tokens": 1,
            "recent_user_tokens": 0,
            "prune_tool_result_chars": 2000,
            "prune_protect_tokens": 0,
            "prune_min_saving_tokens": 1,
            "stages": ["prune"]
        } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg_grew).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());
        let (ok_grew, err_grew, before_est, after_est): (bool, String, i64, i64) = eval(
            &h,
            r#"A:set_messages({
                   { role = "user", content = { { type = "text", text = "read it" } } },
                   { role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = { path = "x" } } } },
                   { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = string.rep("A", 3000) } } },
                   { role = "assistant", content = { { type = "text", text = "done" } } },
                   { role = "user", content = { { type = "text", text = "next" } } },
                   { role = "assistant", content = { { type = "text", text = "ok" } } }
               })
               local before_est = K:measure()
               U:set_context(50000)
               local reply = C.cordis:get("bus"):waterfall("compact/overflow", {})
               local ok, err = reply.succeeded == true, reply.res
               local after_est = K:measure()
               return ok and (err.status == "compacted" or err.status == "unchanged"), tostring(err), before_est, after_est"#,
        )
        .await;
        assert!(
            after_est > before_est,
            "expected tool result to grow: {before_est} -> {after_est} (ok={ok_grew}, err={err_grew})"
        );
        assert!(
            !ok_grew,
            "overflow must NOT report progress when local estimate grew ({before_est} -> {after_est}): {err_grew}"
        );
        h.shutdown().await;

        // 2. Unchanged size: projection commits same-size messages while provider usage was higher.
        let cfg_same = json!({ "plugins": { "compact": {
            "auto": false,
            "stages": ["same_proj"]
        } } });
        let h2 = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg_same).no_session().build().await;
        expose(&h2);
        global(&h2, "K", h2.service_table("compact").unwrap());
        global(&h2, "U", h2.service_table("usage").unwrap());
        let (ok_same, err_same): (bool, String) = eval(
            &h2,
            r#"A:set_messages({
                   { role = "user", content = { { type = "text", text = "hello world" } } },
                   { role = "assistant", content = { { type = "text", text = "reply world" } } }
               })
               K:register_stage("same_proj", function()
                   local msgs = A:list_messages().data
                   A:set_messages(msgs)
                   U:set_context(1000)
                   return true
               end)
               U:set_context(50000)
               local reply = C.cordis:get("bus"):waterfall("compact/overflow", {})
               local ok, err = reply.succeeded == true, reply.res
               return ok and (err.status == "compacted" or err.status == "unchanged"), tostring(err)"#,
        )
        .await;
        assert!(!ok_same, "overflow must NOT report progress when local estimate is unchanged: {err_same}");
        h2.shutdown().await;

        // 3. Actually shrank: prune shrinks a large tool result and overflow reports progress.
        let cfg_shrank = json!({ "plugins": { "compact": {
            "auto": false,
            "keep_recent_tokens": 1,
            "recent_user_tokens": 0,
            "prune_tool_result_chars": 200,
            "prune_head_chars": 50,
            "prune_tail_chars": 50,
            "prune_protect_tokens": 0,
            "prune_min_saving_tokens": 1,
            "stages": ["prune"]
        } } });
        let h3 = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg_shrank).no_session().build().await;
        expose(&h3);
        global(&h3, "K", h3.service_table("compact").unwrap());
        global(&h3, "U", h3.service_table("usage").unwrap());
        let (ok_shrank, err_shrank, b3, a3): (bool, String, i64, i64) = eval(
            &h3,
            r#"A:set_messages({
                   { role = "user", content = { { type = "text", text = "read it" } } },
                   { role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = { path = "x" } } } },
                   { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = string.rep("B", 3000) } } },
                   { role = "assistant", content = { { type = "text", text = "done" } } },
                   { role = "user", content = { { type = "text", text = "next" } } },
                   { role = "assistant", content = { { type = "text", text = "ok" } } }
               })
               local b = K:measure()
               U:set_context(50000)
               local reply = C.cordis:get("bus"):waterfall("compact/overflow", {})
               local ok, err = reply.succeeded == true, reply.res
               local a = K:measure()
               return ok and (err.status == "compacted" or err.status == "unchanged"), tostring(err), b, a"#,
        )
        .await;
        assert!(a3 < b3, "expected tool result to shrink: {b3} -> {a3}");
        assert!(ok_shrank, "overflow MUST report progress when local estimate shrank ({b3} -> {a3}): {err_shrank}");
        h3.shutdown().await;
    })
    .await
}

/// Pruning skips tool results inside the
/// prompt-cache prefix (`last_cache_read`) when uncached tool outputs alone save enough, skips
/// pruning when only cached outputs exist and context is under threshold, and falls back to
/// pruning the cached prefix when context is over threshold (`st.over`).
#[tokio::test]
async fn prune_respects_prompt_cache_boundary_until_over_threshold() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        // Case A: auto compaction over threshold where uncached tool result #2 alone saves enough
        // -> #1 inside last_cache_read prefix stays intact, #2 after boundary is pruned!
        let cfg_a = json!({
            "plugins": {
                "compact": {
                    "auto": true,
                    "prune_protect_tokens": 20,
                    "prune_min_saving_tokens": 400,
                    "prune_tool_result_chars": 500,
                    "prune_head_chars": 80,
                    "prune_tail_chars": 40,
                    "threshold_tokens": base + 2200,
                    "stages": ["prune"]
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg_a).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;

        let (first_pruned_a, second_pruned_a): (bool, bool) = eval(
            &h,
            r#"local big1 = "CACHED1_" .. string.rep("alpha ", 1200) .. "_END1"
               local big2 = "UNCACHED2_" .. string.rep("beta ", 1200) .. "_END2"
               local m1 = { role = "user", content = { { type = "text", text = "step 1" } } }
               local m2 = { role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = { path = "a" } } } }
               local m3 = { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = big1 } } }
               local m4 = { role = "assistant", content = { { type = "tool_call", id = "c2", name = "read", arguments = { path = "b" } } } }
               local m5 = { role = "tool_result", tool_call_id = "c2", tool_name = "read", content = { { type = "text", text = big2 } } }
               local m6 = { role = "assistant", content = { { type = "text", text = "recent protected reply" } } }
               A:set_messages({ m1, m2, m3, m4, m5, m6 })
               local cached_prefix = mare.text.tokens(m1) + mare.text.tokens(m2) + mare.text.tokens(m3) + 50
               C.cordis:get("bus"):emit("item/completed", {
                 status = "completed",
                 item = { message = { role = "assistant", content = {}, usage = { input = 100, output = 10, cache_read = cached_prefix } } },
               })
               PREPARE_REQUEST()
               local msgs = A:list_messages().data
               local p1 = msgs[3].content[1].text:find("tool result middle pruned", 1, true) ~= nil
               local p2 = msgs[5].content[1].text:find("tool result middle pruned", 1, true) ~= nil
               return p1, p2"#,
        )
        .await;
        assert!(!first_pruned_a, "tool output inside last_cache_read prefix must stay untouched");
        assert!(second_pruned_a, "tool output after last_cache_read boundary must be pruned");
        h.shutdown().await;

        // Case B: only tool result #1 is big (inside cache prefix), and context IS over threshold ->
        // fallback `plan(false)` prunes inside the cached prefix.
        let cfg_over = json!({
            "plugins": {
                "compact": {
                    "auto": true,
                    "prune_protect_tokens": 20,
                    "prune_min_saving_tokens": 400,
                    "prune_tool_result_chars": 500,
                    "prune_head_chars": 80,
                    "prune_tail_chars": 40,
                    "threshold_tokens": base + 500,
                    "stages": ["prune"]
                }
            }
        });
        let h2 = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg_over).no_session().build().await;
        expose(&h2);
        global(&h2, "K", h2.service_table("compact").unwrap());
        global(&h2, "L", h2.service_table("llm").unwrap());
        exec(&h2, PREPARE_REQUEST).await;
        let first_pruned_b: bool = eval(
            &h2,
            r#"local big1 = "CACHED_OVER_" .. string.rep("gamma ", 1200) .. "_END"
               A:set_messages({
                 { role = "user", content = { { type = "text", text = "step 1" } } },
                 { role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = { path = "a" } } } },
                 { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = big1 } } },
                 { role = "assistant", content = { { type = "text", text = "recent" } } },
               })
               C.cordis:get("bus"):emit("item/completed", {
                 status = "completed",
                 item = { message = { role = "assistant", content = {}, usage = { input = 100, output = 10, cache_read = 50000 } } },
               })
               PREPARE_REQUEST()
               local msgs = A:list_messages().data
               return msgs[3].content[1].text:find("tool result middle pruned", 1, true) ~= nil"#,
        )
        .await;
        assert!(first_pruned_b, "over-threshold prune must fall back to plan(false) and prune cached prefix");
        h2.shutdown().await;

        // Case C: only tool result #1 is big (inside cache prefix), and context is UNDER threshold ->
        // manual prune must skip it rather than breaking the prompt cache.
        let cfg_under = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "prune_protect_tokens": 20,
                    "prune_min_saving_tokens": 400,
                    "prune_tool_result_chars": 500,
                    "prune_head_chars": 80,
                    "prune_tail_chars": 40,
                    "threshold_tokens": base + 50000,
                    "stages": ["prune"]
                }
            }
        });
        let h3 = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg_under).no_session().build().await;
        expose(&h3);
        global(&h3, "K", h3.service_table("compact").unwrap());
        let first_pruned_c: bool = eval(
            &h3,
            r#"local big1 = "CACHED_UNDER_" .. string.rep("delta ", 1200) .. "_END"
               A:set_messages({
                 { role = "user", content = { { type = "text", text = "step 1" } } },
                 { role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = { path = "a" } } } },
                 { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = big1 } } },
                 { role = "assistant", content = { { type = "text", text = "recent" } } },
               })
               C.cordis:get("bus"):emit("item/completed", {
                 status = "completed",
                 item = { message = { role = "assistant", content = {}, usage = { input = 100, output = 10, cache_read = 50000 } } },
               })
               pcall(K.run, K)
               local msgs = A:list_messages().data
               return msgs[3].content[1].text:find("tool result middle pruned", 1, true) ~= nil"#,
        )
        .await;
        assert!(!first_pruned_c, "under-threshold prune must not break the cached prefix");
        h3.shutdown().await;
    })
    .await
}
