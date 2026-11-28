use super::*;

#[tokio::test]
async fn auto_compaction_summarizes_and_resume_sees_the_same_context() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": { "threshold_tokens": base + 2200, "keep_recent_tokens": 1 } } });
        let script = json!([
            { "text": format!("first answer {}", "detail ".repeat(700)) },
            { "text": "## Summary\nSUMMARY-MARKER: user asked twice" },
            { "text": "second answer" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script.clone()).config(cfg.clone()).build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"COMPACTED = 0 C.cordis:on("compact/completed", function(ev) if not ev.error then COMPACTED = COMPACTED + 1 end end)
               local p = L:provider("faux")
               local stream = p.stream
               p.stream = function(req, sink)
                   if req.purpose == "agent" then LAST = mare.json.encode(req.messages) end
                   return stream(req, sink)
               end"#,
        )
        .await;
        h.prompt("hi", json!({})).await.unwrap();
        assert_eq!(eval::<i64>(&h, "return COMPACTED").await, 0, "below the threshold nothing is compacted");
        let res = h.prompt(&"please consider this long request ".repeat(150), json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["text"], "second answer");
        assert_eq!(eval::<i64>(&h, "return COMPACTED").await, 1);
        let last: String = eval(&h, "return LAST").await;
        assert!(last.contains("SUMMARY-MARKER"), "summary must lead the next request: {last}");
        assert!(!last.contains("first answer"), "compacted turns are not resent verbatim: {last}");
        assert!(last.contains("<recovery>"), "the summary says where the originals are: {last}");
        assert!(
            last.contains("`entry_id` and `parent_entry_id`") && !last.contains("`id` and `parent`"),
            "recovery footer must cite entry_id and parent_entry_id: {last}"
        );
        let (count, path): (i64, String) = eval(&h, "return #A:list_messages().data, S:info().path").await;
        h.shutdown().await;

        let entries: Vec<serde_json::Value> =
            std::fs::read_to_string(&path).unwrap().lines().filter_map(|l| serde_json::from_str(l).ok()).collect();
        let c = entries.iter().find(|e| e["type"] == "compaction").expect("compaction entry");
        assert!(c["first_kept_entry_id"].is_string() && c["summary"].as_str().unwrap().contains("SUMMARY-MARKER"), "{c}");
        assert_eq!(c["method"], "local", "{c}");
        assert!(c["range"]["from_entry_id"].is_string() && c["range"]["to_entry_id"].is_string(), "{c}");

        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).resume().build().await;
        assert_eq!(h.state().await["message_count"], count, "resume rebuilds the post-compaction context");
        h.shutdown().await;
        assert_registered(env.home.path());
    })
    .await
}

#[tokio::test]
async fn manual_compact_command_rewrites_history() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "text": "a1" }, { "text": "a2" }, { "text": "## Summary\nMANUAL-SUMMARY" }]);
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
        let before: i64 = eval(&h, "return #A:list_messages().data").await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, after, text): (bool, i64, String) = eval(
            &h,
            "local r = K:run(); return r.status == 'compacted', #A:list_messages().data, mare.json.encode(A:list_messages().data)",
        )
        .await;
        assert!(ok);
        assert!(after < before, "{before} → {after}");
        assert!(text.contains("MANUAL-SUMMARY"), "{text}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn stages_run_in_order_and_stop_once_below_the_threshold() {
    local(async {
        let env = TestEnv::new().await;
        // the read returns ~12k tokens (capped at 50KB); the threshold is 14k tokens
        std::fs::write(env.cwd.join("big.txt"), "line of filler text\n".repeat(3000)).unwrap();
        let base = overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": {
            "threshold_tokens": base + 14_000, "keep_recent_tokens": 5000, "prune_tool_result_chars": 2000,
            "prune_protect_tokens": 5000, "prune_min_saving_tokens": 1000 } } });
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "big.txt" } }] },
            { "text": "read it" },
            { "text": "answer without any summary" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).build().await;
        expose(&h);
        exec(
            &h,
            r#"STAGES, SUMMARIZED = {}, 0
            C.cordis:on("compact/stage", function(ev)
                if ev.phase == "after" then STAGES[#STAGES + 1] = ev.stage .. (ev.changed and "+" or "-") end
                if ev.stage == "local" and ev.phase == "candidate" then SUMMARIZED = SUMMARIZED + 1 end
            end)"#,
        )
        .await;
        h.prompt("read big.txt", json!({})).await.unwrap();
        // the next prompt crosses the threshold; the (now old) big output is pruned instead
        let res = h.prompt(&"please answer now ".repeat(1000), json!({})).await.unwrap();
        assert_eq!(res["text"], "answer without any summary", "{res}");
        let (stages, summarized, path): (String, i64, String) =
            eval(&h, "return table.concat(STAGES, ','), SUMMARIZED, S:info().path").await;
        assert_eq!(stages, "prune+", "pruning alone was enough: later stages never ran");
        assert_eq!(summarized, 0);
        h.shutdown().await;
        let raw = std::fs::read_to_string(&path).unwrap();
        let prune = raw.lines().find(|l| l.contains(r#""type":"prune""#)).expect("the prune is persisted");
        assert!(prune.contains(r#""method":"prune""#), "{prune}");
    })
    .await
}

#[tokio::test]
async fn overflow_compacts_at_most_three_times_in_a_row_then_stops() {
    local(async {
        let env = TestEnv::new().await;
        let overflow = json!({ "error": "context length exceeded", "kind": "context_overflow", "retryable": false });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([overflow, overflow, overflow, overflow, overflow]))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        // a summary stage that always "succeeds": each overflow round compacts, then retries
        exec(
            &h,
            r#"ROUNDS = 0
               K:register_stage("local", function() ROUNDS = ROUNDS + 1 return true end, { summary = true })"#,
        )
        .await;
        let res = h.prompt("hello", json!({})).await.unwrap();
        let rounds: i64 = eval(&h, "return ROUNDS").await;
        assert_eq!(rounds, 3, "three compactions, then no more");
        let err = res["error"]["message"].as_str().unwrap_or_default();
        assert!(err.contains("after 3 compactions"), "{res}");
        h.shutdown().await;
    })
    .await
}

/// A target below the fixed overhead cannot be reached even with an empty summary.
/// Detect this before calling the summarizer, leave the conversation intact and stop the turn.
#[tokio::test]
async fn auto_compaction_impossible_budget_stalls_without_summary_requests() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("notes.txt"), "a line of notes to read\n".repeat(80)).unwrap();
        let base = overhead(&env).await;
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "notes.txt" } }] },
            { "text": "## Summary\nstill reading notes.txt" },
            { "text": "## Summary\nstill reading notes.txt" },
            { "text": "## Summary\nstill reading notes.txt" },
            { "text": "done" }
        ]);
        let cfg = json!({ "plugins": { "compact": {
            "threshold_tokens": base + 200, "keep_recent_tokens": 1 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"ATTEMPTS, REQUESTS, STALLED = 0, 0, nil
            local complete = L.complete
            L.complete = function(self, req)
                if req.purpose == "compact" then REQUESTS = REQUESTS + 1 end
                return complete(self, req)
            end
            C.cordis:on("compact/stage", function(ev) if ev.stage == "local" and ev.phase == "candidate" then ATTEMPTS = ATTEMPTS + 1 end end)
            C.cordis:on("compact/completed", function(ev) if ev.stalled then STALLED = ev end end)"#,
        )
        .await;
        let res = h.prompt("read notes.txt in steps", json!({})).await.unwrap();
        let (attempts, requests, stalled, count, summaries, history): (i64, i64, bool, i64, i64, String) = eval(
            &h,
            r#"
            local n = 0 for _, m in ipairs(A:list_messages().data) do if m.compaction then n = n + 1 end end
            return ATTEMPTS, REQUESTS, STALLED ~= nil, STALLED and STALLED.compactions or -1, n, mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert_eq!((attempts, requests), (0, 0), "impossible candidates must not request summaries: {res}");
        assert!(stalled, "compact/stalled is emitted");
        assert_eq!(count, 0, "stalled event reports the actual number of candidates");
        assert_eq!(summaries, 0, "failed candidates never replace the context");
        assert!(
            history.contains("read notes.txt in steps") && history.contains("a line of notes to read"),
            "{history}"
        );
        let err = res["error"]["message"].as_str().unwrap_or_default();
        assert!(err.contains("can't get the context below the compaction threshold"), "{res}");
        assert!(err.contains("after 0 attempts"), "{err}");
        assert!(err.contains("/new") && err.contains("keep_recent_tokens"), "actionable: {err}");
        assert!(res["text"] != "done", "the run stopped before the next request: {res}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn no_summary_is_requested_when_even_minimal_user_retention_cannot_fit() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let probe = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
        let (envelope, minimum_user): (i64, i64) = eval(&probe, r#"
            local envelope = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n\n</summary>"
            local user = "L\n[... truncated to fit the compacted context ...]"
            return mare.text.tokens({ { type = "text", text = envelope } }),
                   mare.text.tokens({ { type = "text", text = user } })"#).await;
        probe.shutdown().await;
        let limit = (base + envelope + minimum_user / 2) * 10 / 9;
        let cfg = json!({ "plugins": { "compact": { "threshold_tokens": limit,
            "keep_recent_tokens": 1, "recent_user_tokens": 1, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;
        let (ok, requests, stalled, unchanged): (bool, i64, bool, bool) = eval(&h, r#"
            REQUESTS, STALLED = 0, false
            L.complete = function() REQUESTS = REQUESTS + 1; return { stop_reason = "stop", content = {
                { type = "text", text = "summary" } } } end
            C.cordis:on("compact/completed", function(ev) if ev.stalled then STALLED = true end end)
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("first ", 1000) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("answer ", 1000) } } },
                { role = "user", content = { { type = "text", text = "LATEST " .. string.rep("last ", 1000) } } },
            })
            local before = mare.json.encode(A:list_messages().data)
            local after = nil
            C.cordis:on("compact/completed", function(ev) after = ev end)
            PREPARE_REQUEST()
            return after and after.status == "completed", REQUESTS, STALLED, before == mare.json.encode(A:list_messages().data)"#).await;
        assert!(!ok && stalled && unchanged, "unreachable budget must fail without changing the history");
        assert_eq!(requests, 0, "even the minimum retained user message cannot fit");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn a_nonshrinking_mid_turn_summary_stops_without_repeated_requests() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": { "threshold_tokens": (base + 900) * 10 / 9,
            "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;
        let (ok, requests, stalled, attempts, unchanged): (bool, i64, bool, i64, bool) = eval(
            &h,
            r#"
            REQUESTS, STALLED = 0, nil
            L.complete = function()
                REQUESTS = REQUESTS + 1
                return { role = "assistant", stop_reason = "stop", content = {
                    { type = "text", text = string.rep("long summary ", 500) } } }
            end
            C.cordis:on("compact/completed", function(ev) if ev.stalled then STALLED = ev end end)
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("request ", 400) } } },
                { role = "assistant", content = { { type = "tool_call", id = "call-1", name = "read",
                    arguments = { path = "notes.txt" } } } },
                { role = "tool_result", tool_call_id = "call-1", content = {
                    { type = "text", text = string.rep("result ", 400) } } },
            })
            A.activity = function() return { busy = true } end
            local before = mare.json.encode(A:list_messages().data)
            local after = nil
            C.cordis:on("compact/completed", function(ev) after = ev end)
            PREPARE_REQUEST()
            return after and after.status == "completed", REQUESTS, STALLED ~= nil, STALLED and STALLED.compactions or -1,
                before == mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(!ok && stalled && unchanged, "ok={ok} stalled={stalled} unchanged={unchanged} requests={requests}");
        assert_eq!((requests, attempts), (1, 1), "a full-range nonshrinking summary cannot benefit from retries");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn provider_usage_is_void_after_compaction() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([{ "text": "## Summary\nS" }])).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, &long_history(4)).await;
        global(&h, "K", h.service_table("compact").unwrap());
        // the provider reported a nearly full window for the current context
        exec(&h, r#"C.cordis:get("bus"):emit("item/completed", { item_id = "it-usage", status = "completed", item = { id = "it-usage", type = "assistant_message", message = { role = "assistant", usage = { input = 90000, output = 0, cache_read = 0, cache_write = 0 } } } })"#).await;
        let stale: i64 = eval(&h, "return K:measure()").await;
        assert!(stale >= 90000, "usage of the current context counts: {stale}");
        let (ok, fresh): (bool, i64) = eval(&h, "local r = K:run(); return r.status == 'compacted', K:measure()").await;
        assert!(ok);
        assert!(fresh < 90000, "the old high-water mark no longer applies (new context version): {fresh}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_prune_stays_persisted_when_the_later_summary_fails() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("big.txt"), "line of filler text\n".repeat(3000)).unwrap();
        let cfg = json!({ "plugins": { "compact": {
            "keep_recent_tokens": 1, "prune_tool_result_chars": 2000,
            "prune_protect_tokens": 100, "prune_min_saving_tokens": 1000 } } });
        let fail = json!({ "error": "summarizer down", "retryable": false });
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "big.txt" } }] },
            { "text": "read it" },
            fail, fail, fail
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).build().await;
        expose(&h);
        h.prompt("read big.txt", json!({})).await.unwrap();
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, text, path): (bool, String, String) =
            eval(&h, "local ok, res = pcall(K.run, K); return ok and res.status == 'compacted', mare.json.encode(A:list_messages().data), S:info().path")
                .await;
        assert!(!ok, "the summary failed");
        assert!(
            text.contains("tool result middle pruned"),
            "the prune is still in effect: {}",
            &text[..text.len().min(400)]
        );
        h.shutdown().await;
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.lines().any(|l| l.contains(r#""type":"prune""#)), "and persisted");
        assert!(!raw.lines().any(|l| l.contains(r#""type":"compaction""#)), "no summary was recorded");
    })
    .await
}

#[tokio::test]
async fn a_model_without_a_pressure_budget_is_a_configuration_error() {
    local(async {
        let env = TestEnv::new().await;
        // faux: 100k window, 4096 output reserve → 95904 message budget; 99000 headroom leaves none
        let cfg = json!({ "plugins": { "compact": { "headroom_tokens": 99_000 } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, r#"NOTICES = {} C.cordis:on("host/notice", function(ev) NOTICES[#NOTICES + 1] = ev.text end)"#).await;
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, &long_history(3)).await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, err): (bool, String) =
            eval(&h, "local ok, err = pcall(K.run, K); return ok and err.status == 'compacted', tostring(err)").await;
        assert!(!ok);
        assert!(err.contains("no pressure budget") && err.contains("headroom"), "{err}");
        // selecting the model reports it right away
        let (notices, accepted, selected): (Vec<String>, bool, String) = eval(
            &h,
            r#"
            local accepted = pcall(L.set_model, L, "faux/echo2")
            mare.task.sleep(10)
            return NOTICES, accepted, L:current().ref
        "#,
        )
        .await;
        assert!(notices.iter().any(|n| n.contains("no pressure budget")), "{notices:?}");
        assert!(!accepted, "invalid pressure budgets must reject model admission");
        assert_eq!(selected, "faux/echo");
        h.shutdown().await;
    })
    .await
}

/// A third-party compaction stage that sets
/// `state.stop` halts the pipeline immediately without invoking later stages.
#[tokio::test]
async fn custom_stage_setting_state_stop_halts_later_stages() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "stages": ["stopper", "never_runs"]
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, err, second_ran): (bool, String, bool) = eval(
            &h,
            r#"local ran2 = false
               K:register_stage("stopper", function(st)
                 st.stop = "halted by custom stage"
                 return false, "halted by custom stage"
               end)
               K:register_stage("never_runs", function()
                 ran2 = true
                 return true
               end)
               A:set_messages({
                 { role = "user", content = { { type = "text", text = "hello" } } },
                 { role = "assistant", content = { { type = "text", text = "world" } } },
               })
               local ok, res = pcall(K.run, K)
               return ok and type(res) == "table" and res.status == "compacted", tostring(res), ran2"#,
        )
        .await;
        assert!(!ok && err.contains("halted by custom stage"), "expected stop error: ok={ok} err={err}");
        assert!(!second_ran, "later stage must not run after st.stop is set");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn repro_260_summary_trim_retains_tail_diagnostics() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let cfg = json!({
            "models": {
                "faux/echo": { "context_window": base + 4000, "max_output": 500 }
            },
            "plugins": {
                "compact": {
                    "auto": false,
                    "headroom_tokens": 500,
                    "keep_recent_tokens": 100,
                    "recent_user_tokens": 0,
                    "stages": ["local"]
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());

        let (status, captured_json): (String, String) = eval(
            &h,
            r###"
            local captured = ""
            L.complete = function(_, req)
                if req and req.purpose == "compact" then
                    captured = mare.json.encode(req.messages)
                end
                return {
                    role = "assistant",
                    stop_reason = "stop",
                    content = { { type = "text", text = "## Goal\nFix failure" } },
                }
            end
            local big_err = "BUILD_START\n" .. string.rep("compiling crate-foo-0123456789\n", 800) .. "\nFINAL_ERROR_TAIL_DIAGNOSTIC_999"
            A:set_messages({
                { role = "user", content = { { type = "text", text = "run tests" } } },
                { role = "assistant", content = { { type = "tool_call", id = "c1", name = "bash", arguments = { command = "cargo test" } } } },
                { role = "tool_result", tool_call_id = "c1", tool_name = "bash", is_error = true, content = { { type = "text", text = big_err } } },
                { role = "assistant", content = { { type = "text", text = "checking error" } } },
                { role = "user", content = { { type = "text", text = "next" } } },
                { role = "assistant", content = { { type = "text", text = "ok" } } },
            })
            local r = K:run()
            return tostring(r.status), captured
            "###,
        )
        .await;
        h.shutdown().await;
        assert_eq!(status, "compacted", "compaction must complete with trimmed summary input");
        assert!(
            captured_json.contains("BUILD_START"),
            "trimmed summary input must retain head context alongside tail diagnostics:\n{}",
            &captured_json[..captured_json.len().min(600)]
        );
        assert!(
            captured_json.contains("FINAL_ERROR_TAIL_DIAGNOSTIC_999"),
            "trimmed summary input must preserve tail diagnostics instead of head-only truncation:\n{}",
            &captured_json[..captured_json.len().min(600)]
        );
    })
    .await
}
