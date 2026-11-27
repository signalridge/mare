use super::*;

/// Failure modes replace nothing: an empty or truncated summary,
/// a cancel mid-summary, and a second compaction while one runs; a summarizer that calls a tool
/// despite tool_choice = none is retried once without tools.
#[tokio::test]
async fn failed_cancelled_or_concurrent_compactions_replace_nothing() {
    local(async {
        let env = TestEnv::new().await;
        let cfg = json!({ "plugins": { "compact": { "keep_recent_tokens": 1, "recent_user_tokens": 0 } } });
        let script = json!([
            { "text": "   " },
            { "text": "## Summary\ncut off", "stop": "length" },
            { "text": format!("## Summary\n{}", "slow summary that gets cancelled ".repeat(60)), "delay_ms": 50 },
            { "tool_calls": [{ "name": "ls", "arguments": {} }] },
            { "text": "## Summary\nAFTER-TOOL-RETRY" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, &long_history(3)).await;
        exec(&h, r#"local m = A:list_messages().data m[#m + 1] = { role = "user", content = { { type = "text", text = "TAIL" } } } A:set_messages(m)"#).await;
        global(&h, "K", h.service_table("compact").unwrap());
        for want in ["empty", "token cap"] {
            let (ok, err, n): (bool, String, i64) =
                eval(&h, "local ok, e = pcall(K.run, K) return ok and e.status == 'compacted', tostring(e), #A:list_messages().data").await;
            assert!(!ok && err.contains(want), "{want}: {err}");
            assert_eq!(n, 7, "{want}: history untouched");
        }
        let (ok, concurrent, n): (bool, String, i64) = eval(
            &h,
            r#"local second
               local t = mare.task.spawn(function() mare.task.sleep(20) local _, e = pcall(K.run, K) second = e K:cancel() end)
               local ok = pcall(K.run, K)
               t:wait()
               return ok == true, tostring(second), #A:list_messages().data"#,
        )
        .await;
        assert!(!ok, "the cancelled compaction failed");
        assert!(concurrent.contains("already compacting"), "{concurrent}");
        assert_eq!(n, 7, "cancelled: history untouched");
        let (ok, text): (bool, String) = eval(&h, "local r = K:run() return r.status == 'compacted', mare.json.encode(A:list_messages().data)").await;
        assert!(ok && text.contains("AFTER-TOOL-RETRY"), "{text}");
        h.shutdown().await;
    })
    .await
}

/// Failed candidates never enter the session log, and resuming sees the original messages.
#[tokio::test]
async fn oversized_candidates_are_not_persisted() {
    local(async {
        let env = TestEnv::new().await;
        let too_long = json!({ "text": "large summary ".repeat(400) });
        let script = json!([{ "text": "first answer" }, { "text": "second answer" },
            too_long, too_long, too_long]);
        let cfg = json!({ "plugins": { "compact": { "auto": false, "keep_recent_tokens": 1,
            "recent_user_tokens": 0, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg.clone()).build().await;
        h.prompt(&"first request ".repeat(30), json!({})).await.unwrap();
        h.prompt(&"second request ".repeat(30), json!({})).await.unwrap();
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, before, after, path): (bool, String, String, String) = eval(
            &h,
            r#"
            local before = mare.json.encode(A:list_messages().data)
            local ok, res = pcall(K.run, K)
            return ok and res.status == "compacted", before, mare.json.encode(A:list_messages().data), S:info().path"#,
        )
        .await;
        assert!(!ok);
        assert_eq!(after, before, "rejected summaries do not replace history");
        h.shutdown().await;
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(!raw.lines().any(|line| line.contains(r#""type":"compaction""#)), "no rejected candidate is recorded");
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).resume().build().await;
        expose(&h);
        let resumed: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert_eq!(resumed, before, "resume rebuilds the original history");
        h.shutdown().await;
    })
    .await
}

/// A replacement stage can leave the count above threshold; repeated transforms of the same
/// context must not call it again, while a growing context becomes eligible again.
#[tokio::test]
async fn auto_compaction_waits_for_context_growth() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": { "threshold_tokens": base + 10,
            "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;
        let (initial, unchanged, grown): (i64, i64, i64) = eval(&h, r#"
            COUNT = 0
            K:register_stage("local", function() COUNT = COUNT + 1 return true end, { summary = true })
            A:set_messages({ { role = "user", content = { { type = "text", text = string.rep("x", 400) } } } })
            local m = L:current()
            PREPARE_REQUEST(m)
            local initial = COUNT
            PREPARE_REQUEST(m)
            local unchanged = COUNT
            local messages = A:list_messages().data
            messages[#messages + 1] = { role = "assistant", content = { { type = "text", text = string.rep("y", 100) } } }
            A:set_messages(messages)
            PREPARE_REQUEST(m)
            return initial, unchanged, COUNT"#).await;
        assert_eq!((initial, unchanged, grown), (1, 1, 2));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn provider_failure_does_not_trip_compaction_circuit_breaker() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": { "threshold_tokens": base + 1500,
            "keep_recent_tokens": 1, "failure_backoff_ms": 600000, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;
        let (calls, stalled, changed, err_code, err_msg, first, second): (i64, bool, bool, String, String, i64, i64) =
            eval(
                &h,
                r#"
            CALLS, STALLED, LAST_ERROR = 0, false, nil
            L.complete = function()
                CALLS = CALLS + 1
                return { stop_reason = "error", error = "AUTH-FAIL-SENTINEL", error_kind = "auth" }
            end
            C.cordis:on("compact/completed", function(ev)
                if ev.stalled then STALLED = true end
                LAST_ERROR = ev.error
            end)
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("first ", 1600) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("answer ", 1600) } } },
                { role = "user", content = { { type = "text", text = string.rep("last ", 1600) } } },
            })
            local before = mare.json.encode(A:list_messages().data)
            local model = L:current()
            PREPARE_REQUEST(model)
            local first = CALLS
            PREPARE_REQUEST(model)
            return CALLS, STALLED, before ~= mare.json.encode(A:list_messages().data),
                type(LAST_ERROR) == "table" and tostring(LAST_ERROR.code or "") or "",
                type(LAST_ERROR) == "table" and tostring(LAST_ERROR.message or "") or tostring(LAST_ERROR),
                first, CALLS"#,
            )
            .await;
        assert_eq!((calls, first, second), (1, 1, 1), "provider failures must use the existing backoff");
        assert!(!stalled && !changed, "provider failure is not an irreducible context");
        assert_eq!(err_code, "compact.not_compacted", "compact/completed error must carry structured code");
        assert!(err_msg.contains("AUTH-FAIL-SENTINEL"), "{err_msg}");
        h.shutdown().await;
    })
    .await
}

/// Shrinking retained portions is done before the single durable compaction is written.
#[tokio::test]
async fn oversized_retention_converges_before_the_persisted_summary() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let limit = (base + 2500) * 10 / 9;
        let answer = "answer ".repeat(500);
        let summary = json!({ "text": format!("## Summary\n{}", "CONVERGED ".repeat(500)) });
        let script = json!([{ "text": answer }, { "text": answer }, summary, summary, summary, summary]);
        let cfg = json!({ "plugins": { "compact": { "auto": false, "threshold_tokens": limit,
            "keep_recent_tokens": 2000, "recent_user_tokens": 100, "stages": ["local"] } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg.clone()).build().await;
        h.prompt(&"first request ".repeat(430), json!({})).await.unwrap();
        h.prompt(&"second request ".repeat(400), json!({})).await.unwrap();
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, r#"ATTEMPTS = 0 C.cordis:on("compact/stage", function(ev) if ev.stage == "local" and ev.phase == "candidate" then ATTEMPTS = ATTEMPTS + 1 end end)"#).await;
        let (ok, attempts, used, path): (bool, i64, i64, String) = eval(
            &h,
            r#"
            local r = K:run()
            return r.status == "compacted", ATTEMPTS, K:measure(), S:info().path"#,
        )
        .await;
        assert!(ok, "a smaller retained portion should converge");
        assert!((2..=3).contains(&attempts), "candidate attempts: {attempts}");
        assert!(used <= limit * 9 / 10, "{used} exceeds 90% of {limit}");
        h.shutdown().await;
        let entries: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .filter(|entry: &serde_json::Value| entry["type"] == "compaction")
            .collect();
        assert_eq!(entries.len(), 1, "only the accepted candidate is persisted");
        assert!(entries[0]["tokens_after"].as_i64().unwrap() < entries[0]["tokens_before"].as_i64().unwrap());
        assert!(entries[0]["summary"].as_str().unwrap().contains("CONVERGED"));
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).resume().build().await;
        expose(&h);
        let resumed: String = eval(&h, "return mare.json.encode(A:list_messages().data)").await;
        assert!(resumed.contains("CONVERGED"), "resume restores the accepted summary: {resumed}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn last_compaction_tier_keeps_the_latest_user_message_truncated() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let limit = (base + 1000) * 10 / 9;
        let cfg = json!({ "plugins": { "compact": { "threshold_tokens": limit,
            "keep_recent_tokens": 10000, "recent_user_tokens": 10000, "stages": ["local"] } } });
        let summary = json!({ "text": "small summary" });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([summary, summary, summary])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;
        let (ok, err, latest, used, requests): (bool, String, String, i64, i64) = eval(&h, r#"
            REQUESTS = 0
            local complete = L.complete
            L.complete = function(self, req)
                if req.purpose == "compact" then REQUESTS = REQUESTS + 1 end
                return complete(self, req)
            end
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("first ", 1000) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("answer ", 1000) } } },
                { role = "user", content = { { type = "text", text = "LATEST-USER-START " .. string.rep("latest ", 15000) } } },
            })
            local after = nil
            C.cordis:on("compact/completed", function(ev) after = ev end)
            PREPARE_REQUEST()
            local ok, err = after and after.status == "completed", after and tostring(after.error or "") or "missing compact/completed"
            local kept = ""
            for _, m in ipairs(A:list_messages().data) do
                if m.preserved and m.role == "user" then
                    for _, b in ipairs(m.content or {}) do kept = kept .. (b.text or "") end
                end
            end
            return ok, err, kept, K:measure(), REQUESTS"#).await;
        assert!(ok, "{err}");
        assert!(latest.contains("LATEST-USER-START"), "latest user message must remain: {latest}");
        assert!(latest.contains("truncated"), "large latest user message should be truncated: {latest}");
        assert!(used <= limit * 9 / 10, "{used} exceeds target");
        assert_eq!(requests, 1, "infeasible retained tails must be skipped before calling the model");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn mid_turn_reuses_a_summary_when_only_user_retention_changes() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("notes.txt"), "notes for the task\n".repeat(1000)).unwrap();
        let base = overhead(&env).await;
        let limit = (base + 1500) * 10 / 9;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "threshold_tokens": limit,
            "recent_user_tokens": 1800, "stages": ["local"] } } });
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "notes.txt" } }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"
            REQUESTS, STARTS, COMPACT_OK, COMPACT_ERROR, KEPT = 0, 0, false, nil, ""
            local complete = L.complete
            L.complete = function(self, req)
                if req.purpose == "compact" then
                    REQUESTS = REQUESTS + 1
                    return { role = "assistant", stop_reason = "stop", content = {
                        { type = "text", text = "Summary\n" .. string.rep("summary ", 400) } } }
                end
                return complete(self, req)
            end
            C.cordis:on("compact/stage", function(ev) if ev.stage == "local" and ev.phase == "candidate" then STARTS = STARTS + 1 end end)
            C.cordis:on("context/transform", function(data, ev)
                local msgs = data.messages
                if msgs[#msgs].role ~= "tool_result" or DONE then return ev.next() end
                DONE = true
                local reply = C.cordis:get("bus"):waterfall("compact/overflow", {})
                COMPACT_OK, COMPACT_ERROR = reply.succeeded == true, reply.res
                COMPACT_OK = COMPACT_OK and COMPACT_ERROR.status == "compacted"
                for _, m in ipairs(A:list_messages().data) do
                    if m.preserved and m.role == "user" then KEPT = m.content[1].text end
                end
                if COMPACT_OK then data.messages = table.copy(A:list_messages().data) end
                return ev.next(data)
            end)"#,
        )
        .await;
        let prompt = format!("MID-USER-START {}", "user ".repeat(1000));
        let res = h.prompt(&prompt, json!({})).await.unwrap();
        let (ok, err, requests, starts, kept): (bool, String, i64, i64, String) =
            eval(&h, "return COMPACT_OK == true, tostring(COMPACT_ERROR), REQUESTS, STARTS, KEPT").await;
        assert_eq!(res["text"], "done", "{res}");
        assert!(ok, "{err}");
        assert!(starts >= 2, "the same range must be evaluated at multiple retention tiers: {starts}");
        assert_eq!(requests, 1, "unchanged summary input must not be requested again");
        assert!(kept.starts_with("MID-USER-START"), "{kept}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn in_place_history_change_discards_the_stale_summary() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join("notes.txt"), "notes for the task\n".repeat(1000)).unwrap();
        let base = overhead(&env).await;
        let limit = (base + 1500) * 10 / 9;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "threshold_tokens": limit,
            "recent_user_tokens": 1800, "stages": ["local"] } } });
        let script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "notes.txt" } }] },
            { "text": "done" }
        ]);
        let h = env.host().builtins(&["compact", "usage"]).faux(script).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(
            &h,
            r#"
            INPUTS, COMPACT_OK, COMPACT_ERROR = {}, false, nil
            local complete = L.complete
            L.complete = function(self, req)
                if req.purpose == "compact" then
                    INPUTS[#INPUTS + 1] = mare.json.encode(req.messages)
                    if #INPUTS == 1 then
                        local m = A:messages()
                        local tail = m[#m]
                        tail.content[1].text = tail.content[1].text .. " MUTATED-IN-PLACE"
                    end
                    return { role = "assistant", stop_reason = "stop", content = {
                        { type = "text", text = "Summary\n" .. string.rep("summary ", 400) } } }
                end
                return complete(self, req)
            end
            C.cordis:on("context/transform", function(data, ev)
                local msgs = data.messages
                if msgs[#msgs].role ~= "tool_result" or DONE then return ev.next() end
                DONE = true
                local reply = C.cordis:get("bus"):waterfall("compact/overflow", {})
                COMPACT_OK, COMPACT_ERROR = reply.succeeded == true, reply.res
                COMPACT_OK = COMPACT_OK and COMPACT_ERROR.status == "compacted"
                if COMPACT_OK then data.messages = table.copy(A:list_messages().data) end
                return ev.next(data)
            end)"#,
        )
        .await;
        let prompt = format!("MID-USER-START {}", "user ".repeat(1000));
        let res = h.prompt(&prompt, json!({})).await.unwrap();
        let (ok, err, requests, summaries): (bool, String, i64, i64) = eval(
            &h,
            r#"local n = 0
                for _, m in ipairs(A:list_messages().data) do if m.compaction then n = n + 1 end end
                return COMPACT_OK == true, tostring(COMPACT_ERROR), #INPUTS, n"#,
        )
        .await;
        assert_eq!(res["text"], "done", "{res}");
        assert!(!ok && err.contains("conversation changed during compaction"), "{err}");
        assert_eq!((requests, summaries), (1, 0), "a stale summary must be discarded without retry or commit");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tier_preflight_does_not_reject_an_image_that_can_be_offloaded() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let limit = (base + 600) * 10 / 9;
        let cfg = json!({ "plugins": { "compact": { "auto": false, "threshold_tokens": limit,
            "keep_recent_tokens": 1, "recent_user_tokens": 20000, "stages": ["local"] } } });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": "tiny summary" }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, err, messages): (bool, String, String) = eval(
            &h,
            r#"
            A:set_messages({
                { role = "user", content = { { type = "text", text = "image request" },
                    { type = "image", mime = "image/png", data = mare.crypto.base64_encode("PNG-CONTENT") } } },
                { role = "assistant", content = { { type = "text", text = string.rep("work ", 1200) } } },
                { role = "user", content = { { type = "text", text = "tail request" } } },
            })
            local ok, err = pcall(K.run, K)
            return ok and err.status == "compacted", tostring(err), mare.json.encode(A:list_messages().data)"#,
        )
        .await;
        assert!(ok, "an offloadable image must not make a feasible tier look impossible: {err}");
        assert!(
            messages.contains("[Attachment:") && messages.contains("tiny summary"),
            "the offloaded reference and summary must survive: {messages}"
        );
        h.shutdown().await;
    })
    .await
}

/// When attempt 1 (`cut <= #msgs`) returns a summary longer than the short
/// `history` slice, compaction must downgrade tiers (`attempt 1 -> 2 -> 3`) so `overflow` / `auto`
/// can fold `kept` and converge; if even attempt 3 still expands, `auto` must trip `S.stalled`
/// instead of backoff-looping on attempt 1 forever.
#[tokio::test]
async fn nonshrinking_attempt_1_downgrades_to_converge_and_stalls_when_irreducible() {
    local(async {
        // 1. Overflow converges by downgrading from attempt 1 (short history slice, nonshrinking)
        //    to a later attempt that folds the large kept tail into the summarized range.
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let limit = (base + 5000) * 10 / 9;
        let summary_text = format!("## Summary\n{}", "structured summary ".repeat(260)); // ~1235 tokens
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "threshold_tokens": limit,
                    "keep_recent_tokens": 3500,
                    "recent_user_tokens": 100,
                    "stages": ["local"]
                }
            }
        });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([{ "text": summary_text }, { "text": summary_text }, { "text": summary_text }]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, attempts, err): (bool, i64, String) = eval(
            &h,
            r#"
            ATTEMPTS = 0
            C.cordis:on("compact/stage", function(ev)
                if ev.stage == "local" and ev.phase == "candidate" then ATTEMPTS = ATTEMPTS + 1 end
            end)
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("early context ", 70) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("early reply ", 70) } } },
                { role = "user", content = { { type = "text", text = "big recent turn" } } },
                { role = "assistant", content = { { type = "text", text = string.rep("recent work detail ", 600) } } },
            })
            local reply = C.cordis:get("bus"):waterfall("compact/overflow", {})
            local res = reply.res
            return reply.succeeded == true and type(res) == "table" and res.status == "compacted", ATTEMPTS, mare.json.encode(res)
            "#,
        )
        .await;
        assert!(ok, "overflow must downgrade past attempt 1 nonshrinking slice and converge: attempts={attempts} err={err}");
        assert!(attempts >= 2, "expected tier downgrade attempts >= 2, got {attempts}");
        h.shutdown().await;
        drop(env);

        // 2. Irreducible auto compaction with `cut <= #msgs` at attempt 1 must walk tiers 1 -> 3
        //    and trip `stalled = true` so the next `PREPARE_REQUEST` does not loop on attempt 1.
        let env2 = TestEnv::new().await;
        let base2 = overhead(&env2).await;
        let limit2 = (base2 + 2200) * 10 / 9;
        let huge = format!("## Summary\n{}", "bloated summary ".repeat(1800)); // ~7200 tokens (> all tiers)
        let cfg2 = json!({
            "plugins": {
                "compact": {
                    "threshold_tokens": limit2,
                    "keep_recent_tokens": 1200,
                    "recent_user_tokens": 100,
                    "stages": ["local"]
                }
            }
        });
        let h2 = env2.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg2).no_session().build().await;
        expose(&h2);
        global(&h2, "K", h2.service_table("compact").unwrap());
        global(&h2, "L", h2.service_table("llm").unwrap());
        global(&h2, "HUGE", huge);
        exec(&h2, PREPARE_REQUEST).await;
        let (attempts2, reqs_first, reqs_second, stalled): (i64, i64, i64, bool) = eval(
            &h2,
            r#"
            ATTEMPTS, REQUESTS, STALLED = 0, 0, false
            L.complete = function()
                REQUESTS = REQUESTS + 1
                return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = HUGE } } }
            end
            C.cordis:on("compact/stage", function(ev)
                if ev.stage == "local" and ev.phase == "candidate" then ATTEMPTS = ATTEMPTS + 1 end
            end)
            C.cordis:on("compact/completed", function(ev)
                if ev.stalled then STALLED = true end
            end)
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("older prompt ", 250) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("older answer ", 250) } } },
                { role = "user", content = { { type = "text", text = string.rep("newer prompt ", 120) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("newer answer ", 250) } } },
            })
            PREPARE_REQUEST()
            local first = REQUESTS
            PREPARE_REQUEST()
            return ATTEMPTS, first, REQUESTS, STALLED
            "#,
        )
        .await;
        assert!(stalled, "irreducible auto compaction must trip S.stalled instead of backoff-looping: attempts={attempts2} reqs={reqs_first}");
        assert!(attempts2 >= 2, "expected downgrade through retention tiers before stalling: attempts={attempts2}");
        assert_eq!(reqs_first, reqs_second, "stalled auto compaction must not retry on subsequent PREPARE_REQUEST");
        h2.shutdown().await;
    })
    .await
}

/// A split turn whose short turn-prefix summary is longer than that tiny prefix
/// alone must not abort compaction when the overall history + split-turn summary shrinks context.
#[tokio::test]
async fn split_turn_short_prefix_does_not_abort_converging_compaction() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let limit = (base + 3000) * 10 / 9;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "auto": false,
                    "threshold_tokens": limit,
                    "keep_recent_tokens": 500,
                    "recent_user_tokens": 100,
                    "stages": ["local"]
                }
            }
        });
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([
                { "text": "## Summary\nCompressed earlier 4000 tokens cleanly." },
                { "text": format!("## Turn context\n{}", "detailed split-turn state ".repeat(140)) }
            ]))
            .config(cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (ok, err, encoded): (bool, String, String) = eval(
            &h,
            r#"
            A:set_messages({
                { role = "user", content = { { type = "text", text = string.rep("long history question ", 400) } } },
                { role = "assistant", content = { { type = "text", text = string.rep("long history answer ", 400) } } },
                -- split turn: tiny user prompt followed by kept tool call + result
                { role = "user", content = { { type = "text", text = "fix it" } } },
                { role = "assistant", content = { { type = "tool_call", id = "c1", name = "read", arguments = { path = "a.rs" } } } },
                { role = "tool_result", tool_call_id = "c1", content = { { type = "text", text = string.rep("file line ", 220) } } },
            })
            local ok, res = pcall(K.run, K)
            return ok and type(res) == "table" and res.status == "compacted", tostring(res), mare.json.encode(A:list_messages().data)
            "#,
        )
        .await;
        assert!(ok, "converging split-turn compaction must succeed even when turn-prefix summary exceeds tiny prefix: {err}");
        assert!(
            encoded.contains("Compressed earlier 4000 tokens cleanly") && encoded.contains("Turn context (split turn)"),
            "expected both history summary and split-turn section: {encoded}"
        );
        h.shutdown().await;
    })
    .await
}

/// Repeated auto-compaction failures double the retry
/// backoff from 30s (`failure_backoff_ms`) up to the 600s cap (`failure_backoff_max_ms`).
#[tokio::test]
async fn auto_compaction_failure_backoff_doubles_from_30s_to_600s_cap() {
    local(async {
        let env = TestEnv::new().await;
        let base = overhead(&env).await;
        let limit = (base + 400) * 10 / 9;
        let cfg = json!({
            "plugins": {
                "compact": {
                    "threshold_tokens": limit,
                    "keep_recent_tokens": 1,
                    "recent_user_tokens": 0,
                    "stages": ["local"]
                }
            }
        });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, PREPARE_REQUEST).await;

        let gaps: Vec<i64> = eval(
            &h,
            r#"local now = 1000000
               local orig_clock = mare.task.clock
               mare.task.clock = function() return now end
               local calls = 0
               L.complete = function()
                 calls = calls + 1
                 error({ message = "transient failure", kind = "server" })
               end
               A:set_messages({
                 { role = "user", content = { { type = "text", text = string.rep("history ", 400) } } },
                 { role = "assistant", content = { { type = "text", text = string.rep("answer ", 400) } } },
                 { role = "user", content = { { type = "text", text = "tail" } } },
               })
               local expected = { 30000, 60000, 120000, 240000, 480000, 600000, 600000 }
               local observed = {}
               -- Initial attempt triggers failure #1 (sets backoff = 30000)
               PREPARE_REQUEST()
               assert(calls == 1, "first attempt must run")
               for _, step in ipairs(expected) do
                 now = now + step - 1
                 local before_early = calls
                 PREPARE_REQUEST()
                 assert(calls == before_early, "must stay backed off 1ms before " .. step)
                 now = now + 1
                 PREPARE_REQUEST()
                 assert(calls == before_early + 1, "must retry exactly at " .. step)
                 observed[#observed + 1] = step
               end
               mare.task.clock = orig_clock
               return observed"#,
        )
        .await;
        assert_eq!(gaps, vec![30_000, 60_000, 120_000, 240_000, 480_000, 600_000, 600_000]);
        h.shutdown().await;
    })
    .await
}

/// Retained tail budget scales proportionally with `retain_ratio` when
/// `keep_recent_tokens` is above the ratio cap.
#[tokio::test]
async fn retain_ratio_scales_retained_tail_with_message_budget() {
    local(async {
        let env = TestEnv::new().await;
        // Run with small retain_ratio (0.04) vs larger retain_ratio (0.12) on identical 6-turn history.
        let mut kept_counts = Vec::new();
        for ratio in [0.04_f64, 0.12_f64] {
            let cfg = json!({
                "models": {
                    "faux/echo": { "context_window": 20000, "max_output": 2000 }
                },
                "plugins": {
                    "compact": {
                        "auto": false,
                        "headroom_tokens": 1000,
                        "keep_recent_tokens": 50000,
                        "recent_user_tokens": 0,
                        "retain_ratio": ratio,
                        "stages": ["local"]
                    }
                }
            });
            let h = env
                .host()
                .builtins(&["compact", "usage"])
                .faux(json!([{ "text": "## Summary\nShort" }]))
                .config(cfg)
                .no_session()
                .build()
                .await;
            expose(&h);
            global(&h, "K", h.service_table("compact").unwrap());
            let kept: i64 = eval(
                &h,
                r#"local msgs = {}
                   for i = 1, 6 do
                     msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = ("U%d "):format(i) .. string.rep("word ", 220) } } }
                     msgs[#msgs + 1] = { role = "assistant", content = { { type = "text", text = ("A%d "):format(i) .. string.rep("reply ", 220) } } }
                   end
                   A:set_messages(msgs)
                   local r = K:run()
                   assert(r.status == "compacted", tostring(r.error or r.status))
                   return #A:list_messages().data"#,
            )
            .await;
            kept_counts.push(kept);
            h.shutdown().await;
        }
        assert!(
            kept_counts[1] > kept_counts[0],
            "larger retain_ratio (0.40) must keep more recent messages than smaller (0.04): {kept_counts:?}"
        );
    })
    .await
}
