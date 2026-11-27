//! The local summary request: what it keeps, drops and merges.

use super::*;

#[tokio::test]
async fn the_most_recent_user_message_is_kept_and_truncated_to_the_budget() {
    local(async {
        let env = TestEnv::new().await;
        // one long turn: a huge request (over the 20k user budget), then work; the cut lands
        // inside the turn, so the request itself is in the summarized range
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1 } } });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "## Summary\nS" }, { "text": "## Turn\nT" }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"BIG = "BIG-START " .. string.rep("requirement ", 15000) .. "BIG-END"
               local msgs = { { role = "user", content = { { type = "text", text = BIG } } } }
               for i = 1, 6 do
                   msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = "step " .. i } } }
               end
               A:set_messages(msgs)"#,
        )
        .await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, kept, tokens): (bool, String, i64) = eval(
            &h,
            r#"local r = K:run()
               for _, m in ipairs(A:list_messages().data) do
                   if m.role == "user" and not m.compaction then
                       local t = m.content[1].text
                       return r.status == "compacted", t, mare.text.tokens(t)
                   end
               end
               return r.status == "compacted", "", 0"#,
        )
        .await;
        assert!(ok);
        assert!(kept.starts_with("BIG-START "), "the newest user message is kept: {}", &kept[..kept.len().min(60)]);
        assert!(!kept.contains("BIG-END") && kept.contains("truncated"), "truncated to what is left of the budget");
        assert!(tokens <= 20_100, "within the 20k user budget: {tokens}");
        h.shutdown().await;
    })
    .await
}

/// Mid-turn compaction follows Codex: the whole turn goes into the
/// summary, which is the last message; the turn's prompt is kept verbatim before it. The
/// summary request reports its usage as `compact`, the conversation's as `agent`.
#[tokio::test]
async fn mid_turn_compaction_puts_the_summary_last() {
    local(async {
        let env = TestEnv::new().await;
        // Give the tool result enough content for a framed summary to actually reduce history.
        for i in 0..100 {
            std::fs::write(env.cwd.join(format!("file-with-a-long-name-for-compaction-{i:03}")), "x").unwrap();
        }
        let script = json!([
            { "tool_calls": [{ "name": "ls", "arguments": { "path": "." } }] },
            { "text": "## Summary\nMID-TURN" },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"PURPOSES, ORDER = {}, nil
               C.cordis:on("llm/usage", function(ev) PURPOSES[#PURPOSES + 1] = ev.purpose end)
               -- compact right before the request that follows the tool results (an overflow
               -- there runs the same staged compaction)
               C.cordis:on("context/transform", function(data, ev)
                   local msgs = data.messages
                   if msgs[#msgs].role ~= "tool_result" or DONE then return ev.next() end
                   DONE = true
                   assert(C.cordis:get("bus"):waterfall("compact/overflow", {}).res.status == "compacted")
                   local roles = {}
                   for _, m in ipairs(A:list_messages().data) do
                       roles[#roles + 1] = m.compaction and "summary" or m.role
                   end
                   ORDER = table.concat(roles, ",")
                   data.messages = table.copy(A:list_messages().data)
                   return ev.next(data)
               end)"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(res["text"], "done", "{res}");
        let (order, purposes): (String, String) = eval(&h, "return ORDER, table.concat(PURPOSES, ',')").await;
        assert_eq!(order, "user,summary", "the turn's prompt kept before the summary, nothing after it");
        assert_eq!(purposes, "agent,compact,agent");
        let kept: String = eval(&h, "return A:list_messages().data[1].content[1].text").await;
        assert_eq!(kept, "go");
        h.shutdown().await;
        // the persisted compaction rebuilds the same context
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).resume().build().await;
        expose(&h);
        let roles: String = eval(
            &h,
            r#"local roles = {}
               for _, m in ipairs(A:list_messages().data) do roles[#roles + 1] = m.compaction and "summary" or m.role end
               return table.concat(roles, ",")"#,
        )
        .await;
        assert_eq!(roles, "user,summary,assistant");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_local_summary_merges_the_previous_summary_with_newer_messages() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let script = json!([{ "text": "## Summary\nFIRST-SUMMARY" }, { "text": "## Summary\nSECOND-SUMMARY" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, &long_history(3)).await;
        // a trailing user message: the cut lands on it (no split turn, one request per run)
        exec(&h, r#"local m = A:list_messages().data m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL-1" } } } A:set_messages(m)"#).await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        global(&h, "K", h.service_table("compact").unwrap());
        assert!(eval::<bool>(&h, "return K:run().status == 'compacted'").await);
        // newer messages after the first summary, then a second compaction
        exec(
            &h,
            r#"local m = A:list_messages().data
               for i = 1, 3 do
                   m[#m + 1] = { role = "user", content = { { type = "text", text = ("NEW-%d "):format(i) .. string.rep("words ", 50) } } }
                   m[#m + 1] = { role = "assistant", content = { { type = "text", text = "ack" } } }
               end
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL-2" } } }
               A:set_messages(m)"#,
        )
        .await;
        assert!(eval::<bool>(&h, "return K:run().status == 'compacted'").await);
        let sent: Vec<String> = eval(&h, "return SENT").await;
        assert_eq!(sent.len(), 2, "one request per compaction");
        let second = &sent[1];
        assert!(second.contains("<previous-summary>") && second.contains("FIRST-SUMMARY"), "{second}");
        assert!(second.contains("NEW-1 "), "the newer messages are merged in");
        assert!(!second.contains("OLD-1 "), "what the previous summary covers is not resent: {second}");
        let text: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(text.contains("SECOND-SUMMARY"), "{text}");
        h.shutdown().await;
    })
    .await
}

/// History: an earlier summary, then a tool call with a big result, then more turns.
const TOOL_HISTORY: &str = r#"A:set_messages({
    { role = "user", compaction = true, summary = "PREV-SUMMARY", content = { { type = "text", text = "<summary>PREV-SUMMARY</summary>" } } },
    { role = "user", content = { { type = "text", text = "list files" } } },
    { role = "assistant", content = { { type = "tool_call", id = "c1", name = "ls", arguments = { path = "OLD-ARGS" } } } },
    { role = "tool_result", tool_call_id = "c1", tool_name = "ls", content = { { type = "text", text = "OLD-RESULT" } } },
    { role = "assistant", content = { { type = "text", text = "listed " .. string.rep("detail ", 300) } } },
    { role = "user", content = { { type = "text", text = "KEEP-ME question" } } },
    { role = "assistant", content = { { type = "text", text = "answer" } } },
    { role = "user", content = { { type = "text", text = "last" } } },
})"#;

#[tokio::test]
async fn an_overlong_summary_request_drops_the_oldest_tool_call_together_with_its_result() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let script = json!([overflow, overflow, { "text": "## Summary\nAFTER-DROP" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, TOOL_HISTORY).await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, text): (bool, String) =
            eval(&h, "local r = K:run(); return r.status == 'compacted', mare.json.encode(A:list_messages().data)")
                .await;
        assert!(ok, "the lossy summary still commits");
        let sent: Vec<String> = eval(&h, "return SENT").await;
        assert_eq!(sent.len(), 3);
        // first retry: the oldest unit ("list files") is gone; second: the tool call AND its result
        let last = &sent[2];
        assert!(!last.contains("OLD-ARGS") && !last.contains("OLD-RESULT"), "call and result dropped together: {last}");
        for s in &sent[..2] {
            assert_eq!(s.contains("OLD-ARGS"), s.contains("OLD-RESULT"), "never one without the other: {s}");
        }
        assert!(last.contains("KEEP-ME"), "only the oldest units are dropped");
        assert!(
            text.contains("AFTER-DROP") && text.contains("did not fit"),
            "the summary says what was left out: {text}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_previous_summary_is_never_dropped_from_a_summary_request() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let script = json!([overflow, overflow, overflow, { "text": "## Summary\nS" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, TOOL_HISTORY).await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        global(&h, "K", h.service_table("compact").unwrap());
        let _ = eval::<bool>(&h, "return K:run().status == 'compacted'").await;
        let sent: Vec<String> = eval(&h, "return SENT").await;
        assert!(sent.len() >= 3, "{}", sent.len());
        for s in &sent {
            assert!(s.contains("PREV-SUMMARY"), "every attempt keeps the earlier summary: {s}");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_single_message_larger_than_the_window_fails_without_replacing() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "## Summary\nS" }])).config(cfg).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"A:set_messages({
                { role = "user", content = { { type = "text", text = "huge paste " .. string.rep("x", 420000) } } },
                { role = "user", content = { { type = "text", text = "next" } } },
                { role = "assistant", content = { { type = "text", text = "done" } } },
            })"#,
        )
        .await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, err, n): (bool, String, i64) = eval(
            &h,
            "local ok, err = pcall(K.run, K); return ok and err.status == 'compacted', tostring(err), #A:list_messages().data",
        )
        .await;
        assert!(!ok);
        assert!(err.contains("tokens") && err.contains("window allows") && err.contains("nothing replaced"), "{err}");
        assert_eq!(n, 3, "history untouched");
        h.shutdown().await;
    })
    .await
}

/// Manual compaction does nothing when the whole conversation fits in the retained tail.
#[tokio::test]
async fn manual_compaction_with_no_cut_keeps_history_and_sends_no_request() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "stages": ["local"], "keep_recent_tokens": 32000 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "short summary" }])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        let (ok, err, before, after, requests): (bool, String, String, String, i64) = eval(
            &h,
            r#"
            A:set_messages({
              { role = "user", content = { { type = "text", text = string.rep("hello ", 200) } } },
              { role = "assistant", content = { { type = "text", text = string.rep("answer ", 200) } } },
            })
            local before = mare.json.encode(A:list_messages().data)
            local r = K:run()
            return r.status == "compacted", tostring(r.status), before, mare.json.encode(A:list_messages().data), #SENT"#,
        )
        .await;
        assert!(!ok && err == "nothing_to_compact", "{err}");
        assert_eq!(requests, 0, "a no-op must not send a model request");
        assert_eq!(before, after, "a no-op must preserve the conversation exactly");
        h.shutdown().await;
    })
    .await
}

/// /compact twice with nothing new: no model call and the kept user messages stay (pi
/// compaction.ts:901); a later compaction lets earlier kept messages compete again (Codex
/// compact.rs:526-545); the previous summary is sent once, without mare's footer and lists.
#[tokio::test]
async fn compacting_again_reuses_the_kept_messages_and_sends_the_previous_summary_once() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1 } } });
        let script = json!([{ "text": "## Summary\nFIRST-SUMMARY" }, { "text": "## Summary\nSECOND-SUMMARY" }]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).build().await;
        expose(&h);
        exec(&h, &long_history(3)).await;
        // Retained users stay verbatim; enough assistant content must be removed to pay for the summary.
        exec(&h, r#"local m = A:list_messages().data
            for _, item in ipairs(m) do if item.role == "assistant" then item.content[1].text = string.rep("work ", 400) end end
            A:set_messages(m)"#).await;
        exec(&h, r#"local m = A:list_messages().data m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL-1" } } } A:set_messages(m)"#).await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        global(&h, "K", h.service_table("compact").unwrap());
        let kept = r#"local out = {} for _, m in ipairs(A:list_messages().data) do if m.preserved then out[#out + 1] = m.content[1].text:sub(1, 6) end end return table.concat(out, ",")"#;
        assert!(eval::<bool>(&h, "return K:run().status == 'compacted'").await);
        let first_kept: String = eval(&h, kept).await;
        assert_eq!(first_kept, "OLD-1 ,OLD-2 ,OLD-3 ", "recent user messages are kept verbatim");
        let (again, err): (bool, String) = eval(&h, "local r = K:run() return r.status == 'compacted', tostring(r.status)").await;
        assert!(!again && err == "nothing_to_compact", "{err}");
        assert_eq!(eval::<i64>(&h, "return #SENT").await, 1, "no model call for nothing new");
        assert_eq!(eval::<String>(&h, kept).await, first_kept, "the kept messages survive");
        exec(&h, r#"local m = A:list_messages().data
            m[#m + 1] = { role = "assistant", content = { { type = "text", text = string.rep("work ", 400) } } }
            m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL-2" } } }
            A:set_messages(m)"#).await;
        assert!(eval::<bool>(&h, "return K:run().status == 'compacted'").await);
        let second_kept: String = eval(&h, kept).await;
        assert_eq!(second_kept, "OLD-1 ,OLD-2 ,OLD-3 ,TAIL-1", "earlier kept messages compete again");
        let sent: Vec<String> = eval(&h, "return SENT").await;
        let req = &sent[1];
        assert_eq!(req.matches("FIRST-SUMMARY").count(), 1, "the previous summary goes in once: {req}");
        assert!(!req.contains("<recovery>") && !req.contains("Context Recovery"), "no recovery footer: {req}");
        h.shutdown().await;
    })
    .await
}

/// /compact <focus> reaches the split-turn summary too (pi compaction.ts:1079)...
#[tokio::test]
async fn a_compact_focus_reaches_the_turn_prefix_summary() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "## Summary\nS" }, { "text": "## Turn\nT" }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(&h, &long_history(2)).await;
        exec(
            &h,
            r#"local m = A:list_messages().data
            m[#m + 1] = { role = "user", content = { { type = "text", text = "do the work" } } }
            for i = 1, 3 do m[#m + 1] = { role = "assistant", content = { { type = "text", text = "step " .. i } } } end
            A:set_messages(m)"#,
        )
        .await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        global(&h, "K", h.service_table("compact").unwrap());
        assert!(eval::<bool>(&h, r#"return K:run("FOCUS-ON-TESTS").status == "compacted""#).await);
        let sent: Vec<String> = eval(&h, "return SENT").await;
        assert_eq!(sent.len(), 2, "history summary + turn prefix");
        for s in &sent {
            assert!(s.contains("Additional focus: FOCUS-ON-TESTS"), "{s}");
        }
        h.shutdown().await;
    })
    .await
}

/// A summary request that doesn't fit first cuts tool results to 2000 characters in the request
/// copy (pi utils.ts:95, :150), and only then drops the oldest units.
#[tokio::test]
async fn an_overlong_summary_request_trims_tool_results_before_dropping_anything() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([overflow, { "text": "## Summary\nS" }])).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, r#"A:set_messages({
            { role = "user", content = { { type = "text", text = "FIRST-REQUEST" } } },
            { role = "assistant", content = { { type = "tool_call", id = "c1", name = "ls", arguments = { path = "CALL-ARGS" } } } },
            { role = "tool_result", tool_call_id = "c1", content = { { type = "text", text = "RESULT-HEAD " .. string.rep("z", 9000) .. " RESULT-END" } } },
            { role = "assistant", content = { { type = "text", text = "listed" } } },
            { role = "user", content = { { type = "text", text = "last" } } },
        })"#).await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, RECORD_REQUESTS).await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, text): (bool, String) =
            eval(&h, "local r = K:run() return r.status == 'compacted', mare.json.encode(A:list_messages().data)").await;
        assert!(ok);
        let sent: Vec<String> = eval(&h, "return SENT").await;
        assert_eq!(sent.len(), 2);
        assert!(sent[0].contains("RESULT-END"), "the first attempt sends the result as is");
        let retry = &sent[1];
        assert!(retry.contains("RESULT-HEAD") && !retry.contains("RESULT-END"), "cut to 2000 characters");
        assert!(retry.contains("more characters truncated") && retry.contains("FIRST-REQUEST") && retry.contains("CALL-ARGS"));
        assert!(!text.contains("did not fit"), "nothing was dropped: {text}");
        h.shutdown().await;
    })
    .await
}

/// Summary model window/limit failure falls back once to the current model and records both usages
/// (Codex `codex-rs/core/src/compact_model_fallback.rs:9-35`).
#[tokio::test]
async fn summary_model_window_or_limit_failure_falls_back_to_current_model_and_records_both_usages() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": {
            "llm": { "retries": 0 },
            "compact": {
                "auto": false,
                "keep_recent_tokens": 1,
                "recent_user_tokens": 0,
                "summary_model": "faux/echo2",
                "stages": ["local"]
            }
        } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "U", h.service_table("usage").unwrap());
        exec(&h, &long_history(3)).await;
        exec(
            &h,
            r###"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } }
               A:set_messages(m)
               MODELS, USAGES, MODE = {}, {}, "rate_limit"
               C.cordis:on("llm/usage", function(ev)
                   USAGES[#USAGES + 1] = { model = ev.model_ref, purpose = ev.purpose, input = ev.usage and ev.usage.input }
               end)
               local fp = L:provider("faux")
               fp.stream = function(req, sink)
                   MODELS[#MODELS + 1] = req.model and req.model.ref or "nil"
                   if #MODELS == 1 then
                       error({
                           message = MODE == "rate_limit" and "rate limit exceeded" or "context length exceeded",
                           kind = MODE,
                           usage = { input = 111, output = 7 },
                       })
                   end
                   sink({ type = "text_delta", id = "t1", delta = "## Summary\nFALLBACK-OK" })
                   return {
                       role = "assistant",
                       stop_reason = "stop",
                       usage = { input = 222, output = 13 },
                       content = { { type = "text", text = "## Summary\nFALLBACK-OK" } },
                   }
               end"###,
        )
        .await;
        let (ok, err, models, usages, total_in, text): (bool, String, Vec<String>, String, i64, String) = eval(
            &h,
            r#"local ok, err = pcall(K.run, K)
               local tot = U:totals()
               return ok and err.status == 'compacted', tostring(err), MODELS, mare.json.encode(USAGES), tot and tot.input or 0, mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(ok, "fallback to current model must succeed: {err}");
        assert_eq!(models, vec!["faux/echo2", "faux/echo"]);
        assert!(text.contains("FALLBACK-OK"), "{text}");
        assert_eq!(total_in, 333, "usage service must sum both attempts: {total_in}");
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&usages).unwrap();
        assert_eq!(parsed.len(), 2, "both attempts must record usage: {usages}");
        assert_eq!(parsed[0]["model"], "faux/echo2");
        assert_eq!(parsed[0]["input"], 111);
        assert_eq!(parsed[0]["purpose"], "compact");
        assert_eq!(parsed[1]["model"], "faux/echo");
        assert_eq!(parsed[1]["input"], 222);
        assert_eq!(parsed[1]["purpose"], "compact");

        // Window failure (context_overflow) also falls back once to current model.
        exec(&h, &long_history(3)).await;
        let (ok2, err2, models2): (bool, String, Vec<String>) = eval(
            &h,
            r#"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL2" } } }
               A:set_messages(m)
               MODELS, MODE = {}, "context_overflow"
               local ok, err = pcall(K.run, K)
               return ok and err.status == "compacted", tostring(err), MODELS"#,
        )
        .await;
        assert!(ok2, "context_overflow fallback must succeed: {err2}");
        assert_eq!(models2, vec!["faux/echo2", "faux/echo"]);
        h.shutdown().await;
    })
    .await
}

/// b1-1520 CMP1b #1: the summary model's own request opts out of `fallback_models` so the
/// current model is tried next, but errors `fallback_models` answer (overloaded, a missing model)
/// must still reach them: summary model → current model → fallback_models.
/// Reference tests: Codex core/tests/suite/compact.rs:3391 (a not-found previous model falls
/// back to the selected model) and :3765 (advised errors fall back, then retry on the selected
/// model); Kimi packages/agent-core-v2/test/agent/fullCompaction/fullCompaction.test.ts:908 (a
/// filtered response fails fast — kept as the policy counter-example below). reference implementation none.
#[tokio::test]
async fn summary_model_overload_or_missing_model_tries_current_then_global_fallback() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "fallback_models": ["summary-fallback/m"],
            "plugins": {
                "llm": { "retries": 0, "overloaded_retries": 0 },
                "compact": {
                    "auto": false,
                    "keep_recent_tokens": 1,
                    "recent_user_tokens": 0,
                    "summary_model": "faux/echo2",
                    "stages": ["local"]
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r###"MODELS, FAIL = {}, {}
               local function answer(ref, sink)
                   MODELS[#MODELS + 1] = ref
                   local e = FAIL[ref]
                   if e then
                       error({ message = e.message, kind = e.kind, retryable = e.retryable or false, retry_after_ms = e.retry_after_ms })
                   end
                   sink({ type = "text_delta", id = "t1", delta = "## Summary\nBY " .. ref })
                   return { role = "assistant", stop_reason = "stop", usage = { input = 10, output = 2 },
                       content = { { type = "text", text = "## Summary\nBY " .. ref } } }
               end
               L:provider("faux").stream = function(req, sink) return answer(req.model.ref, sink) end
               L:register_provider({ provider = "summary-fallback", is_configured = function() return true end,
                   models = { { model_id = "m", name = "Fallback", context_window = 100000, max_output = 4096 } },
                   stream = function(req, sink) return answer(req.model.ref, sink) end })"###,
        )
        .await;
        let run = |fail: &'static str| {
            let h = h.clone();
            async move {
                exec(&h, &long_history(3)).await;
                eval::<(bool, String, Vec<String>, String)>(
                    &h,
                    &format!(
                        r#"local m = A:list_messages().data
                           m[#m + 1] = {{ role = "user", content = {{ {{ type = "text", text = "TAIL" }} }} }}
                           A:set_messages(m)
                           MODELS, FAIL = {{}}, {fail}
                           local ok, err = pcall(K.run, K)
                           return ok and err.status == "compacted", tostring(err), MODELS, mare.json.encode(A:list_messages().data)"#
                    ),
                )
                .await
            }
        };

        // overloaded everywhere but the fallback: all three, in order
        let (ok, err, models, text) = run(r#"{ ["faux/echo2"] = { message = "overloaded", kind = "overloaded" },
                 ["faux/echo"] = { message = "overloaded", kind = "overloaded" } }"#)
        .await;
        assert!(ok, "fallback_models must still answer: {err}");
        assert_eq!(models, ["faux/echo2", "faux/echo", "summary-fallback/m"]);
        assert!(text.contains("BY summary-fallback/m"), "{text}");

        // a missing summary model (recognised by its message) hands over to the current model
        let (ok, err, models, text) =
            run(r#"{ ["faux/echo2"] = { message = "404: model not found", kind = "invalid" } }"#).await;
        assert!(ok, "the current model must answer: {err}");
        assert_eq!(models, ["faux/echo2", "faux/echo"]);
        assert!(text.contains("BY faux/echo"), "{text}");

        // the summary model's own request never jumps to fallback_models ahead of the current model
        let (ok, err, models, _) = run(r#"{ ["faux/echo2"] = { message = "overloaded", kind = "overloaded" } }"#).await;
        assert!(ok, "{err}");
        assert_eq!(models, ["faux/echo2", "faux/echo"]);
        h.shutdown().await;
    })
    .await
}

/// b11 CMP1b #1: an overloaded summary model that asks to wait longer than
/// `retry_after_max_ms` fails as `retry_delay_too_long` (original kind `overloaded`); that is
/// still an overload, so it hands over: summary model → current model → fallback_models.
/// Reference test: Codex core/tests/suite/compact.rs:3765 (advised errors fall back, then retry
/// on the selected model); retry_after.rs:862 (server advice decides the wait).
#[tokio::test]
async fn summary_model_overloaded_with_a_too_long_retry_after_still_hands_over() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "fallback_models": ["summary-fallback/m"],
            "plugins": {
                "llm": { "retries": 2, "overloaded_retries": 2, "retry_after_max_ms": 1000 },
                "compact": {
                    "auto": false,
                    "keep_recent_tokens": 1,
                    "recent_user_tokens": 0,
                    "summary_model": "faux/echo2",
                    "stages": ["local"]
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r###"MODELS = {}
               local function answer(ref, sink)
                   MODELS[#MODELS + 1] = ref
                   if ref ~= "summary-fallback/m" then
                       error({ message = "overloaded", kind = "overloaded", retryable = true, retry_after_ms = 3600000 })
                   end
                   sink({ type = "text_delta", id = "t1", delta = "## Summary\nBY " .. ref })
                   return { role = "assistant", stop_reason = "stop", usage = { input = 10, output = 2 },
                       content = { { type = "text", text = "## Summary\nBY " .. ref } } }
               end
               L:provider("faux").stream = function(req, sink) return answer(req.model.ref, sink) end
               L:register_provider({ provider = "summary-fallback", is_configured = function() return true end,
                   models = { { model_id = "m", name = "Fallback", context_window = 100000, max_output = 4096 } },
                   stream = function(req, sink) return answer(req.model.ref, sink) end })"###,
        )
        .await;
        exec(&h, &long_history(3)).await;
        let (ok, err, models, text): (bool, String, Vec<String>, String) = eval(
            &h,
            r#"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } }
               A:set_messages(m)
               local ok, err = pcall(K.run, K)
               return ok and err.status == "compacted", tostring(err), MODELS, mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(ok, "a long Retry-After on an overloaded model must still fall back: {err}");
        assert_eq!(models, ["faux/echo2", "faux/echo", "summary-fallback/m"], "no waiting retries, one call each");
        assert!(text.contains("BY summary-fallback/m"), "{text}");
        h.shutdown().await;
    })
    .await
}

/// b20 CMP1b (should-fix): a filtered answer is a refusal even when it carries text — it is
/// never committed as the summary and never handed to the current model.
/// Reference test: Kimi packages/agent-core-v2/test/agent/fullCompaction/fullCompaction.test.ts:908.
#[tokio::test]
async fn filtered_summary_text_is_not_a_summary() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": {
            "auto": false, "keep_recent_tokens": 1, "recent_user_tokens": 0,
            "summary_model": "faux/echo2", "stages": ["local"]
        } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(3)).await;
        let (ok, err, models, text): (bool, String, Vec<String>, String) = eval(
            &h,
            r#"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } }
               A:set_messages(m)
               MODELS = {}
               L.complete = function(self, req)
                   MODELS[#MODELS + 1] = req.model and req.model.ref or "nil"
                   return { role = "assistant", stop_reason = "filtered",
                       content = { { type = "text", text = "REFUSAL TEXT" } } }
               end
               local ok, err = pcall(K.run, K)
               return ok and err.status == "compacted", tostring(err), MODELS, mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(!ok && err.contains("filtered"), "a filtered answer must fail compaction: {err}");
        assert_eq!(models, ["faux/echo2"], "a refusal is not handed to the current model");
        assert!(!text.contains("REFUSAL TEXT"), "the refusal must not become the summary: {text}");
        h.shutdown().await;
    })
    .await
}

/// Counter-example: policy refusal and cancellation never fall back to the current model
/// (Codex `codex-rs/core/src/compact_model_fallback.rs:15-22`, `tests/suite/compact.rs:1451`).
#[tokio::test]
async fn summary_model_policy_refusal_and_cancellation_do_not_fall_back() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": {
            "auto": false,
            "keep_recent_tokens": 1,
            "recent_user_tokens": 0,
            "summary_model": "faux/echo2",
            "stages": ["local"]
        } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(3)).await;
        exec(
            &h,
            r#"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } }
               A:set_messages(m)
               MODELS, KIND = {}, "policy"
               L.complete = function(self, req)
                   MODELS[#MODELS + 1] = req.model and req.model.ref or "nil"
                   if KIND == "aborted" then
                       return { role = "assistant", stop_reason = "aborted" }
                   end
                   return {
                       role = "assistant",
                       stop_reason = "error",
                       error = "blocked by policy",
                       error_kind = KIND,
                   }
               end"#,
        )
        .await;
        let (ok1, err1, models1): (bool, String, Vec<String>) = eval(
            &h,
            r#"local ok, err = pcall(K.run, K); return ok and err.status == "compacted", tostring(err), MODELS"#,
        )
        .await;
        assert!(!ok1 && err1.contains("policy"), "policy refusal must fail: {err1}");
        assert_eq!(models1, vec!["faux/echo2"], "policy refusal must not fall back");

        let (ok2, err2, models2): (bool, String, Vec<String>) = eval(
            &h,
            r#"MODELS, KIND = {}, "aborted"
               local ok, err = pcall(K.run, K); return ok and err.status == "compacted", tostring(err), MODELS"#,
        )
        .await;
        assert!(!ok2 && err2.contains("cancelled"), "cancellation must fail: {err2}");
        assert_eq!(models2, vec!["faux/echo2"], "cancellation must not fall back");
        h.shutdown().await;
    })
    .await
}

/// Budget/window failure on the configured summary model must also return an error kind and fall
/// back once to the current model (Codex `codex-rs/core/src/compact_model_fallback.rs:9-22`).
#[tokio::test]
async fn summary_model_budget_failure_falls_back_to_current_model() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": {
            "auto": false,
            "keep_recent_tokens": 1,
            "recent_user_tokens": 0,
            "summary_model": "faux/echo2",
            "model_thresholds": { "faux/echo2": { "headroom_tokens": 9_999_999 } },
            "stages": ["local"]
        } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(3)).await;
        exec(
            &h,
            r##"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } }
               A:set_messages(m)
               MODELS = {}
               L.complete = function(self, req)
                   MODELS[#MODELS + 1] = req.model and req.model.ref or "nil"
                   return {
                       role = "assistant",
                       stop_reason = "stop",
                       content = { { type = "text", text = '## Summary\nBUDGET-FALLBACK-OK' } },
                   }
               end"##,
        )
        .await;
        let (ok, err, models, text): (bool, String, Vec<String>, String) = eval(
            &h,
            r#"local ok, err = pcall(K.run, K)
               return ok and err.status == "compacted", tostring(err), MODELS, mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(ok, "budget failure on summary_model must fall back to current model: {err}");
        assert_eq!(models, vec!["faux/echo"]);
        assert!(text.contains("BUDGET-FALLBACK-OK"), "{text}");
        h.shutdown().await;
    })
    .await
}

/// Summary fallback must try `summary_model -> current` even when global `fallback_models` is
/// configured and the global fallback fails (or would succeed before `current` is tried).
/// Runs through the real `llm.stream` wrapper (`stream_with_fallback`).
#[tokio::test]
async fn summary_model_quota_falls_back_to_current_model_despite_global_fallback_models() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "fallback_models": ["testfb/fb1"],
            "plugins": { "compact": {
                "auto": false,
                "keep_recent_tokens": 1,
                "recent_user_tokens": 0,
                "summary_model": "faux/echo2",
                "stages": ["local"]
            } }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(3)).await;
        exec(
            &h,
            r###"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } }
               A:set_messages(m)
               MODELS = {}
               L:register_provider({
                   provider = "testfb",
                   is_configured = function() return true end,
                   models = {
                       { model_id = "fb1", context_window = 128000, max_output = 4096 },
                   },
                   stream = function(req, sink)
                       MODELS[#MODELS + 1] = req.model and req.model.ref or "nil"
                       error({ message = "bad api key", kind = "auth" })
                   end,
               })
               local fp = L:provider("faux")
               fp.stream = function(req, sink)
                   MODELS[#MODELS + 1] = req.model and req.model.ref or "nil"
                   if req.model and req.model.ref == "faux/echo2" then
                       error({ message = "quota exceeded", kind = "quota" })
                   end
                   sink({ type = "text_delta", id = "t1", delta = "## Summary\nCURRENT-AFTER-QUOTA" })
                   return {
                       role = "assistant",
                       stop_reason = "stop",
                       content = { { type = "text", text = "## Summary\nCURRENT-AFTER-QUOTA" } },
                   }
               end"###,
        )
        .await;
        let (ok, err, models, text): (bool, String, Vec<String>, String) = eval(
            &h,
            r#"local ok, err = pcall(K.run, K)
               return ok and err.status == "compacted", tostring(err), MODELS, mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(ok, "quota on summary_model must fall back to current model, not global fallback: {err}");
        assert_eq!(models, vec!["faux/echo2", "faux/echo"]);
        assert!(text.contains("CURRENT-AFTER-QUOTA"), "{text}");
        h.shutdown().await;
    })
    .await
}

/// When `llm.stream` aborts retries because `retry_after_ms > retry_after_max_ms`, the original
/// `rate_limit` kind must be preserved so summary fallback still retries with the current model.
#[tokio::test]
async fn summary_model_long_retry_after_falls_back_to_current_model_via_real_llm_stream() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "llm": { "retry_after_max_ms": 1000 },
                "compact": {
                    "auto": false,
                    "keep_recent_tokens": 1,
                    "recent_user_tokens": 0,
                    "summary_model": "faux/echo2",
                    "stages": ["local"]
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(3)).await;
        exec(
            &h,
            r###"local m = A:list_messages().data
               m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } }
               A:set_messages(m)
               MODELS = {}
               local fp = L:provider("faux")
               fp.stream = function(req, sink)
                   MODELS[#MODELS + 1] = req.model and req.model.ref or "nil"
                   if req.model and req.model.ref == "faux/echo2" then
                       error({
                           message = "rate limit exceeded",
                           kind = "rate_limit",
                           retry_after_ms = 120000,
                       })
                   end
                   sink({ type = "text_delta", id = "t1", delta = "## Summary\nCURRENT-AFTER-LONG-RETRY" })
                   return {
                       role = "assistant",
                       stop_reason = "stop",
                       content = { { type = "text", text = "## Summary\nCURRENT-AFTER-LONG-RETRY" } },
                   }
               end"###,
        )
        .await;
        let (ok, err, models, text): (bool, String, Vec<String>, String) = eval(
            &h,
            r#"local ok, err = pcall(K.run, K)
               return ok and err.status == "compacted", tostring(err), MODELS, mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(ok, "long Retry-After rate_limit must still fall back to current model: {err}");
        assert_eq!(models, vec!["faux/echo2", "faux/echo"]);
        assert!(text.contains("CURRENT-AFTER-LONG-RETRY"), "{text}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn compaction_rejects_unfinished_paused_or_non_stop_summary() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(2)).await;
        exec(
            &h,
            r###"
            L:provider("faux").stream = function(_req, _sink)
                return {
                    role = "assistant",
                    stop_reason = "paused",
                    content = { { type = "text", text = "## Summary\nPARTIAL-PAUSED" } },
                }
            end
            "###,
        )
        .await;
        let (ok, err, encoded): (bool, String, String) = eval(
            &h,
            "local ok, err = pcall(K.run, K); return ok and err.status == 'compacted', tostring(err), mare.json.encode(A:list_messages().data)",
        )
        .await;
        assert!(!ok, "paused summary must not be committed as active compaction");
        assert!(err.contains("did not complete cleanly") || err.contains("paused"), "{err}");
        assert!(!encoded.contains("PARTIAL-PAUSED"), "{encoded}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_oversized_older_unit_is_dropped_while_newer_units_summarize() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "## Summary\nKEPT-NEWER" }])).config(cfg).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"A:set_messages({
                { role = "user", content = { { type = "text", text = "huge paste " .. string.rep("x", 420000) } } },
                { role = "assistant", content = { { type = "text", text = "newer unit fits fine" } } },
                { role = "user", content = { { type = "text", text = "next" } } },
                { role = "assistant", content = { { type = "text", text = "done" } } },
            })"#,
        )
        .await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, err, summary): (bool, String, String) = eval(
            &h,
            "local ok, res = pcall(K.run, K); return ok and res.status == 'compacted', tostring(res), mare.json.encode(A:list_messages().data[1])",
        )
        .await;
        assert!(ok, "oversized older unit must be dropped rather than failing up front: {err}");
        assert!(summary.contains("KEPT-NEWER"), "{summary}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn compaction_handles_413_payload_too_large_strips_images_and_bounds_live_shrink() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 6, "recent_user_tokens": 0, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"A:set_messages({
                { role = "user", content = { { type = "text", text = "old" }, { type = "image", mime = "image/png", data = "AAA=" } } },
                { role = "assistant", content = { { type = "text", text = "reply1" } } },
                { role = "user", content = { { type = "text", text = "u2" } } },
                { role = "assistant", content = { { type = "text", text = "reply2" } } },
                { role = "user", content = { { type = "text", text = "u3" } } },
                { role = "assistant", content = { { type = "text", text = "reply3" } } },
                { role = "user", content = { { type = "text", text = "tail" } } },
                { role = "assistant", content = { { type = "text", text = "done" } } },
            })"#,
        )
        .await;
        exec(
            &h,
            r###"
            CALLS = 0
            HAD_IMAGE = {}
            L:register_provider({
                provider = "imgsum",
                is_configured = function() return true end,
                models = { { model_id = "m", context_window = 128000, max_output = 4096, images = true } },
                stream = function(req, sink)
                    CALLS = CALLS + 1
                    local enc = mare.json.encode(req.messages)
                    if CALLS == 1 then
                        FIRST_ENC = enc
                    end
                    HAD_IMAGE[CALLS] = enc:find('"type":"image"', 1, true) ~= nil
                    if CALLS == 1 then
                        error({ message = "payload too large", kind = "payload_too_large", should_retry = false })
                    elseif CALLS == 2 then
                        error({ message = "still overflowing", kind = "context_overflow", should_retry = false })
                    end
                    sink({ type = "text_delta", id = "t1", delta = "## Summary\nSTRIPPED-AND-SHRUNK" })
                    return {
                        role = "assistant",
                        stop_reason = "stop",
                        content = { { type = "text", text = "## Summary\nSTRIPPED-AND-SHRUNK" } },
                    }
                end,
            })
            assert(L:set_model("imgsum/m"))
            "###,
        )
        .await;
        let (ok, err, calls, had_json, summary): (bool, String, i64, String, String) = eval(
            &h,
            r#"
            local ok, res = pcall(K.run, K, "", {
                summarize_only = true,
                messages = {
                    { role = "user", content = { { type = "text", text = "old" }, { type = "image", mime = "image/png", data = "AAA=" } } },
                    { role = "assistant", content = { { type = "text", text = "reply1" } } },
                    { role = "user", content = { { type = "text", text = "u2" } } },
                    { role = "assistant", content = { { type = "text", text = "reply2" } } },
                    { role = "user", content = { { type = "text", text = "u3" } } },
                    { role = "assistant", content = { { type = "text", text = "reply3" } } },
                },
            })
            return ok and type(res) == "table" and res.status == "summarized", mare.json.encode(res), CALLS, mare.json.encode(HAD_IMAGE), type(res) == "table" and tostring(res.summary or "") or ""
            "#,
        )
        .await;
        assert!(ok, "413 + context_overflow must strip images then shrink: {err}, had={had_json}");
        assert_eq!(had_json, "[true,false,false]", "second attempt must strip images: calls={calls}, err={err}");
        assert_eq!(calls, 3, "err={err}, summary={summary}");
        assert!(summary.contains("STRIPPED-AND-SHRUNK"), "{summary}");
        h.shutdown().await;
    })
    .await
}

/// Local summary reads `todo:snapshot()` (not removed `todo:render()`) to render
/// `## Current Todo List` when todos are present, and omits the section when empty.
#[tokio::test]
async fn local_summary_renders_current_todo_list_from_snapshot() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0, "stages": ["local"] } } });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "## Summary\nCompact body" }, { "text": "## Summary\nSecond compact body" }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok1, text1, ok2, text2): (bool, String, bool, String) = eval(
            &h,
            r#"
            local snap = {
                todos = {
                    { content = "Ship checklist summary", status = "in_progress" },
                    { content = "Verify tests", status = "pending" },
                    { content = "Read spec", status = "completed" },
                },
                explanation = "Working through F1b",
            }
            C.cordis:provide("todo", {
                snapshot = function() return snap end,
            })
            local long_text = string.rep("context history detail ", 40)
            A:set_messages({
                { role = "user", content = { { type = "text", text = long_text } } },
                { role = "assistant", content = { { type = "text", text = long_text } } },
                { role = "user", content = { { type = "text", text = "tail" } } },
            })
            local r1 = K:run()
            local t1 = A:list_messages().data[1].content[1].text or ""

            snap = { todos = mare.json.null }
            A:set_messages({
                { role = "user", content = { { type = "text", text = long_text } } },
                { role = "assistant", content = { { type = "text", text = long_text } } },
                { role = "user", content = { { type = "text", text = "tail" } } },
            })
            local r2 = K:run()
            local t2 = A:list_messages().data[1].content[1].text or ""
            return r1.status == "compacted", t1, r2.status == "compacted", t2
            "#,
        )
        .await;
        assert!(ok1, "first compaction must succeed");
        assert!(text1.contains("## Current Todo List"), "missing todo section: {text1}");
        assert!(text1.contains("[~] Ship checklist summary"), "missing in_progress item: {text1}");
        assert!(text1.contains("[ ] Verify tests"), "missing pending item: {text1}");
        assert!(text1.contains("[x] Read spec"), "missing completed item: {text1}");
        assert!(ok2, "second compaction must succeed");
        assert!(!text2.contains("## Current Todo List"), "empty todo snapshot must omit section: {text2}");
        h.shutdown().await;
    })
    .await
}

/// Enforce the "summary is longer than what it summarizes" guard without a 2000-token floor, and run
/// convergence checks on `summarize_only` too.
#[tokio::test]
async fn short_summary_expansion_and_summarize_only_fail_convergence_check() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0, "stages": ["local"] } } });
        let bloated = format!("## Summary\n{}", "verbose expansion text ".repeat(260));
        let mildly_longer = format!("## Summary\n{}", "longer than branch input ".repeat(25));
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([
                { "text": bloated },
                { "text": bloated },
                { "text": bloated },
                { "text": mildly_longer },
                { "text": "short summary" }
            ]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok_local, err_local, ok_only, err_only, ok_good, before_good, after_good): (
            bool,
            String,
            bool,
            String,
            bool,
            i64,
            i64,
        ) = eval(
            &h,
            r#"
            -- Part A: history below 2000 tokens (above summary envelope) whose summary is longer than its input must fail
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("short question about build ", 30) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("short answer about build ", 30) } } },
                { role = "user", content = { { type = "text", text = "ok" } } },
            })
            local ok_local, err_local = pcall(K.run, K)

            -- Part B: summarize_only with a summary longer than its messages (yet shorter than messages + focus instruction)
            -- must still fail the summarize_only convergence check in stages.lua
            local short_msgs = {
                { role = "user", content = { { type = "text", text = "branch question" } } },
                { role = "assistant", content = { { type = "text", text = "branch answer" } } },
            }
            local focus = string.rep("focus guidance for branch summary ", 40)
            local ok_only, err_only = pcall(K.run, K, focus, { summarize_only = true, messages = short_msgs })

            -- Part C: summarize_only with a genuinely shorter summary succeeds and reports token counts
            local long_msgs = {
                { role = "user", content = { { type = "text", text = string.rep("detailed branch discussion ", 40) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("detailed branch resolution ", 40) } } },
            }
            local ok_good, res_good = pcall(K.run, K, nil, { summarize_only = true, messages = long_msgs })
            return ok_local, tostring(err_local),
                ok_only, tostring(err_only),
                ok_good and type(res_good) == "table" and res_good.status == "summarized",
                type(res_good) == "table" and (res_good.tokens_before or 0) or 0,
                type(res_good) == "table" and (res_good.tokens_after or 0) or 0
            "#,
        )
        .await;
        assert!(!ok_local && err_local.contains("longer than what it summarizes"), "short local summary expansion must fail: ok={ok_local} err={err_local}");
        assert!(!ok_only && err_only.contains("summary did not shrink the input"), "summarize_only expansion must hit stages.lua convergence check: ok={ok_only} err={err_only}");
        assert!(ok_good, "converging summarize_only must succeed");
        assert!(before_good > after_good && after_good > 0, "expected tokens_after < tokens_before: before={before_good} after={after_good}");
        h.shutdown().await;
    })
    .await
}

/// When summarization fails, a
/// `compact/summary-error` waterfall listener can supply a replacement summary string.
#[tokio::test]
async fn compact_summary_error_waterfall_can_supply_fallback_summary() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "keep_recent_tokens": 1,
                    "recent_user_tokens": 0,
                    "stages": ["local"]
                }
            }
        });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "error": "provider unavailable", "error_kind": "server" }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, encoded): (bool, String) = eval(
            &h,
            r###"C.cordis:on("compact/summary-error", function(data, ev)
                 data.summary = "## Summary\nFALLBACK-FROM-WATERFALL"
                 return ev.next(data)
               end)
               A:set_messages({
                 { role = "user", content = { { type = "text", text = string.rep("history ", 300) } } },
                 { role = "assistant", content = { { type = "text", text = string.rep("answer ", 300) } } },
                 { role = "user", content = { { type = "text", text = "tail" } } },
               })
               local r = K:run()
               return r.status == "compacted", mare.json.encode(A:list_messages().data)"###,
        )
        .await;
        assert!(ok, "waterfall replacement summary must allow compaction to succeed: {encoded}");
        assert!(encoded.contains("FALLBACK-FROM-WATERFALL"), "{encoded}");
        h.shutdown().await;
    })
    .await
}

/// Local summary appends `<read-files>`,
/// `<modified-files>` (accumulated across incremental compactions), and the live `## Current Todo
/// List` rendered from `todo:snapshot()`.
#[tokio::test]
async fn local_summary_tracks_cumulative_files_and_live_todo_list() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "keep_recent_tokens": 1,
                    "recent_user_tokens": 0,
                    "stages": ["local"]
                }
            }
        });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "## Summary\nFirst pass" },
                { "text": "## Summary\nSecond pass" }
            ]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        // Register a lightweight `todo` service implementing `snapshot()` like mare-plugins/todo.
        let (first_summary, second_summary): (String, String) = eval(
            &h,
            r#"C.cordis:provide("todo", {
                 snapshot = function()
                   return {
                     explanation = "Active plan",
                     todos = {
                       { status = "completed", content = "Audit T1" },
                       { status = "in_progress", content = "Write tests" },
                       { status = "pending", content = "Run gate" },
                     },
                   }
                 end,
               })
               A:set_messages({
                 { role = "user", content = { { type = "text", text = string.rep("turn one ", 200) } } },
                 { role = "assistant", content = {
                   { type = "tool_call", id = "c1", name = "read", arguments = { path = "src/alpha.rs" } },
                   { type = "tool_call", id = "c2", name = "write", arguments = { path = "src/beta.rs" } },
                 } },
                 { role = "tool_result", tool_call_id = "c1", tool_name = "read", content = { { type = "text", text = string.rep("code ", 150) } } },
                 { role = "tool_result", tool_call_id = "c2", tool_name = "write", content = { { type = "text", text = "ok" } } },
                 { role = "assistant", content = { { type = "text", text = "edited beta" } } },
                 { role = "user", content = { { type = "text", text = "tail1" } } },
               })
               assert(K:run().status == "compacted")
               local s1 = A:list_messages().data[1].content[1].text

               -- Add another turn reading a new file, then compact incrementally.
               local m2 = A:list_messages().data
               m2[#m2 + 1] = { role = "assistant", content = {
                 { type = "tool_call", id = "c3", name = "read", arguments = { path = "src/gamma.rs" } },
               } }
               m2[#m2 + 1] = { role = "tool_result", tool_call_id = "c3", tool_name = "read", content = { { type = "text", text = string.rep("more ", 250) } } }
               m2[#m2 + 1] = { role = "assistant", content = { { type = "text", text = string.rep("done ", 250) } } }
               m2[#m2 + 1] = { role = "user", content = { { type = "text", text = "tail2" } } }
               A:set_messages(m2)
               assert(K:run().status == "compacted")
               local s2 = A:list_messages().data[1].content[1].text
               return s1, s2"#,
        )
        .await;
        assert!(first_summary.contains("<read-files>\nsrc/alpha.rs\n</read-files>"), "{first_summary}");
        assert!(first_summary.contains("<modified-files>\nsrc/beta.rs\n</modified-files>"), "{first_summary}");
        assert!(
            first_summary.contains("## Current Todo List")
                && first_summary.contains("- [x] Audit T1")
                && first_summary.contains("- [~] Write tests")
                && first_summary.contains("- [ ] Run gate"),
            "{first_summary}"
        );
        assert!(
            second_summary.contains("src/alpha.rs") && second_summary.contains("src/gamma.rs"),
            "second summary must accumulate prior and new read files: {second_summary}"
        );
        assert!(
            second_summary.contains("<modified-files>\nsrc/beta.rs\n</modified-files>"),
            "second summary must preserve prior modified files: {second_summary}"
        );
        h.shutdown().await;
    })
    .await
}
