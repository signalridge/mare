use super::*;

/// reference implementation: a switched route must use a compatible summary.
#[tokio::test]
async fn cross_provider_fallback_never_calls_openai_remote_compaction() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let base = super::stages::overhead(&env).await;
        let cfg = json!({ "model": "openai/gpt-6.1-terra", "fallback_models": ["cmpfb/one"], "plugins": {
            "llm": { "retries": 0, "overloaded_retries": 0 },
            "compact": { "threshold_tokens": base + 5000, "keep_recent_tokens": 1, "recent_user_tokens": 0 }
        } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        let (offloads, reason): (i64, String) = eval(&h, r#"
            local offloads = 0
            local d = {
                ctx = {llm = {
                    current = function() return {ref = 'openai/gpt-6.1-terra'} end,
                    compact_remote = function() return nil, 'unavailable', 'server' end,
                }},
                cfg = {remote = true},
                S = {prepared = {model = {ref = 'cmpfb/one'}}, token = {is_cancelled = function() return false end}},
                tokens = function() return 1 end, estimate = function() return 1 end,
                threshold = function() return 100 end,
                offload_plan = function() offloads = offloads + 1; return {}, 0, 0 end,
                stage_impl = {}, prompts = {}, cut = {}, commit = {},
                messages = function() return {
                    {role = 'user', content = {{type = 'image', mime = 'image/png', data = 'UE5H'}}},
                    {role = 'assistant', content = {{type = 'text', text = 'answer'}}},
                } end,
            }
            require('compact.stages.remote')(d)
            local _, why = d.stage_impl.remote({})
            return offloads, tostring(why)
        "#).await;
        assert_eq!(offloads, 0, "cross-model rejection must precede even image offloading");
        assert!(reason.contains("text summary"), "{reason}");
        exec(&h, &long_history(8)).await;
        exec(&h, r###"
            REMOTE_CALLS, FALLBACK_REQUEST, AUTO_COMPACTED, FALLBACK_EFFORT = 0, nil, 0, nil
            C.cordis:on('compact/completed', function(ev)
                if ev.reason == 'auto' and ev.status == 'completed' then AUTO_COMPACTED = AUTO_COMPACTED + 1 end
            end)
            C.cordis:on('request/build', function(req, next)
                if req.model.ref == 'cmpfb/one' then
                    req.system = req.system .. string.rep('x', 10200)
                    req.effort = 'max'
                end
                return next()
            end)
            L:provider('openai').stream = function(req, sink)
                if req.purpose == 'compact' then
                    return {role = 'assistant', stop_reason = 'stop', content = {{type = 'text', text = '## Summary\nPORTABLE-SUMMARY'}}}
                end
                error({message = 'quota', kind = 'quota', retryable = false})
            end
            L.compact_remote = function()
                REMOTE_CALLS = REMOTE_CALLS + 1
                return nil, 'remote should not run for another provider', 'server'
            end
            L:register_provider({provider = 'cmpfb', is_configured = function() return true end,
                models = {{model_id = 'one', context_window = 100000, max_output = 4096, reasoning = true, efforts = {'low', 'medium', 'high'}}},
                stream = function(req, sink)
                    FALLBACK_REQUEST = mare.json.encode(req.messages)
                    FALLBACK_EFFORT = req.effort
                    sink({type = 'text_delta', delta = 'done'})
                    return {role = 'assistant', stop_reason = 'stop', content = {{type = 'text', text = 'done'}}}
                end})
        "###).await;
        let (direct, expected, error): (String, String, String) = eval(&h, r#"
            local res = L:stream({model = L:current(), effort = 'max',
                messages = {{role = 'user', content = {{type = 'text', text = 'direct'}}}}}, function() end)
            return tostring(FALLBACK_EFFORT), tostring(L:clamp_effort(L:resolve('cmpfb/one'), 'max')), tostring(res.error or '')
        "#).await;
        assert!(error.is_empty(), "direct llm fallback failed: {error}");
        assert!(expected != "nil" && expected != "max", "fallback must support a lower effort: {expected}");
        assert_eq!(direct, expected, "direct llm calls must also clamp fallback effort");
        let result = h.prompt("CURRENT", json!({})).await.unwrap();
        let (remote, compacted, fallback, effort, expected): (i64, i64, String, String, String) =
            eval(&h, "return REMOTE_CALLS, AUTO_COMPACTED, FALLBACK_REQUEST or '', tostring(FALLBACK_EFFORT), tostring(L:clamp_effort(L:resolve('cmpfb/one'), 'max'))").await;
        assert_eq!(remote, 0, "OpenAI opaque compaction cannot run for a different fallback model: {result}");
        assert!(result["error"].is_null(), "{result}");
        assert_eq!(compacted, 1, "fallback must use portable local compaction");
        assert!(fallback.contains("PORTABLE-SUMMARY"), "{fallback}");
        assert!(expected != "nil" && expected != "max", "the fallback must define a lower supported effort: {expected}");
        assert_eq!(effort, expected, "fallback effort must be clamped before preparing its overlay");
        h.shutdown().await;
    }).await
}

/// reference implementation and Codex history_tests.rs:1935-1974:
/// pressure includes system instructions admitted after the stored history was assembled.
#[tokio::test]
async fn request_build_additions_trigger_compaction_once_and_reach_provider() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let cfg = json!({ "plugins": { "compact": {
            "threshold_tokens": base + 2700, "keep_recent_tokens": 1,
            "recent_user_tokens": 0, "stages": ["remote", "local"]
        } } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(4)).await;
        exec(
            &h,
            r###"
            BUILDS, COMPACTED, SENT, BREAKDOWN, REMOTE_MATCH = 0, 0, nil, nil, false
            C.cordis:on("compact/before", function()
                local ok, result = pcall(K.measure, K, { by_source = true })
                BREAKDOWN = ok and type(result) == "table" and type(result.tools) == "number"
            end)
            C.cordis:on("compact/completed", function(ev)
                if ev.status == "completed" then COMPACTED = COMPACTED + 1 end
            end)
            C.cordis:on("request/build", function(req, next)
                BUILDS = BUILDS + 1
                req.system = req.system .. "\nSYSTEM-HOOK " .. string.rep("system-extra ", 350)
                req.messages[#req.messages + 1] = { role = "user", content = {
                    { type = "text", text = "EXTRA-HOOK " .. string.rep("message-extra ", 200) } } }
                EXPECTED_SYSTEM, EXPECTED_TOOLS = req.system, mare.json.encode(req.tools)
                return next()
            end)
            L.supports_remote_compact = function() return true end
            L.compact_remote = function(_, req)
                REMOTE_MATCH = req.system == EXPECTED_SYSTEM and mare.json.encode(req.tools) == EXPECTED_TOOLS
                return nil, "remote unavailable", "server"
            end
            local provider = L:provider("faux")
            provider.stream = function(req, sink)
                if req.purpose == "compact" then
                    return { role = "assistant", stop_reason = "stop", content = {
                        { type = "text", text = "## Summary\nPREPARED-SUMMARY" } } }
                end
                SENT = mare.json.encode({ system = req.system, messages = req.messages })
                sink({ type = "text_delta", delta = "done" })
                return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
            end
        "###,
        )
        .await;
        let res = h.prompt("CURRENT-INPUT", json!({})).await.unwrap();
        let (builds, compacted, sent): (i64, i64, String) = eval(&h, "return BUILDS, COMPACTED, SENT or ''").await;
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(compacted, 1, "prepared request additions should trigger compaction");
        assert!(
            eval::<bool>(&h, "return BREAKDOWN == true").await,
            "by-source measurement must work during compaction"
        );
        assert!(
            eval::<bool>(&h, "return REMOTE_MATCH == true").await,
            "remote compaction must reuse the prepared system and tools"
        );
        assert_eq!(builds, 1, "request/build has side effects and must run once");
        let sent: serde_json::Value = serde_json::from_str(&sent).unwrap();
        assert!(sent["system"].as_str().unwrap().contains("SYSTEM-HOOK"));
        let messages = sent["messages"].to_string();
        assert!(messages.contains("PREPARED-SUMMARY") && messages.contains("EXTRA-HOOK"), "{messages}");
        assert!(!messages.contains("OLD-1"), "the provider must receive compacted history: {messages}");
        assert!(
            messages.find("CURRENT-INPUT") < messages.find("EXTRA-HOOK"),
            "injected message must remain after the history"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn each_prepared_input_contributes_once_to_pressure() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        for mode in ["none", "half", "system", "message", "tools", "schema"] {
            let cfg = json!({ "plugins": { "compact": {
                "threshold_tokens": base + 3500, "keep_recent_tokens": 1,
                "recent_user_tokens": 0, "stages": ["local"]
            } } });
            let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
            expose(&h);
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "MODE", mode);
            exec(&h, &long_history(4)).await;
            exec(&h, r###"
                BUILDS, COMPACTED, SENT = 0, 0, nil
                C.cordis:on("compact/completed", function(ev)
                    if ev.status == "completed" then COMPACTED = COMPACTED + 1 end
                end)
                C.cordis:on("request/build", function(req, next)
                    BUILDS = BUILDS + 1
                    local marker = "HOOK-" .. MODE .. " " .. string.rep("x", MODE == "half" and 5000 or 10200)
                    if MODE == "system" or MODE == "half" then req.system = req.system .. marker end
                    if MODE == "message" then req.messages[#req.messages + 1] = { role = "user", content = { { type = "text", text = marker } } } end
                    if MODE == "tools" then req.tools[1].description = marker end
                    if MODE == "schema" then req.output_schema = { type = "object", description = marker } end
                    return next()
                end)
                L:provider("faux").stream = function(req, sink)
                    if req.purpose == "compact" then
                        return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "## Summary\nSUMMARY" } } }
                    end
                    SENT = mare.json.encode({ system = req.system, messages = req.messages, tools = req.tools, output_schema = req.output_schema })
                    sink({ type = "text_delta", delta = "done" })
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
                end
            "###).await;
            let res = h.prompt("CURRENT", json!({})).await.unwrap();
            let (builds, compacted, sent): (i64, i64, String) = eval(&h, "return BUILDS, COMPACTED, SENT or ''").await;
            assert!(res["error"].is_null(), "{mode}: {res}");
            let want = i64::from(!matches!(mode, "none" | "half"));
            assert_eq!(compacted, want, "{mode}: prepared input pressure");
            assert_eq!(builds, 1, "{mode}: build hook must run once");
            if mode != "none" {
                assert!(sent.contains(&format!("HOOK-{mode}")), "{mode}: provider lost injected content");
            }
            if want == 1 {
                assert!(sent.contains("SUMMARY"), "{mode}: provider must receive the summary");
            }
            h.shutdown().await;
        }
    }).await
}

#[tokio::test]
async fn rewritten_or_ambiguous_history_is_sent_without_compaction() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        for mode in ["rewrite", "reorder", "delete", "duplicate", "transform"] {
            let h = env
                .host()
                .builtins(&["compact", "usage"])
                .faux(json!([]))
                .config(json!({ "plugins": { "compact": {
                "threshold_tokens": base + 3000, "keep_recent_tokens": 1, "stages": ["local"]
            } } }))
                .no_session()
                .build()
                .await;
            expose(&h);
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "MODE", mode);
            exec(&h, &long_history(4)).await;
            exec(
                &h,
                r#"
                BUILDS, COMPACTED, DIAGNOSTIC, EXPECTED, SENT = 0, 0, false, nil, nil
                C.cordis:on("compact/started", function() COMPACTED = COMPACTED + 1 end)
                C.cordis:on("host/notice", function(ev)
                    if tostring(ev.text):find("cannot be mapped uniquely", 1, true) then DIAGNOSTIC = true end
                end)
                C.cordis:on("context/transform", function(data, ev)
                    if MODE == "transform" then data.messages[1].content[1].text = "TRANSFORMED" end
                    return ev.next(data)
                end)
                C.cordis:on("request/build", function(req, next)
                    BUILDS = BUILDS + 1
                    local messages = req.messages
                    if MODE == "rewrite" then messages[1].content[1].text = "REWRITTEN" end
                    if MODE == "reorder" then messages[1], messages[2] = messages[2], messages[1] end
                    if MODE == "delete" then table.remove(messages, 1) end
                    if MODE == "duplicate" then
                        local copy = require("mare_lib").copy_deep(messages)
                        for _, message in ipairs(copy) do messages[#messages + 1] = message end
                    end
                    req.system = req.system .. string.rep("x", 12000)
                    local expected = require("mare_lib").copy_deep(messages)
                    for _, m in ipairs(expected) do m.turn_id = nil end
                    EXPECTED = mare.json.encode(expected)
                    return next()
                end)
                L:provider("faux").stream = function(req, sink)
                    SENT = mare.json.encode(req.messages)
                    sink({ type = "text_delta", delta = "done" })
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
                end
            "#,
            )
            .await;
            let res = h.prompt("CURRENT", json!({})).await.unwrap();
            let (builds, compacted, diagnostic, equal, preserved): (i64, i64, bool, bool, bool) = eval(
                &h,
                r#"return BUILDS, COMPACTED, DIAGNOSTIC, EXPECTED == SENT,
                    A:list_messages().data[1].content[1].text:find("OLD-1", 1, true) ~= nil"#,
            )
            .await;
            assert!(res["error"].is_null(), "{mode}: {res}");
            assert_eq!((builds, compacted), (1, 0), "{mode}");
            assert!(
                diagnostic && equal && preserved,
                "{mode}: preserve both the request and persisted history (diagnostic={diagnostic}, equal={equal}, preserved={preserved})"
            );
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn prepared_compaction_rejects_history_changed_after_admission() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(json!({ "plugins": { "compact": {
            "threshold_tokens": base + 2700, "keep_recent_tokens": 1,
            "recent_user_tokens": 0, "stages": ["local"]
        } } })).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, &long_history(4)).await;
        exec(&h, r###"
            BUILDS, COMMITS, SENT = 0, 0, nil
            C.cordis:on("request/build", function(req, next)
                BUILDS = BUILDS + 1
                req.system = req.system .. string.rep("system-extra ", 350)
                req.messages[#req.messages + 1] = { role = "user", content = {
                    { type = "text", text = "EXTRA " .. string.rep("message-extra ", 200) } } }
                return next()
            end)
            C.cordis:on("compact/before", function()
                local messages = A:list_messages().data
                messages[1].content[1].text = "CONCURRENT-EDIT"
                A:set_messages(messages)
            end)
            C.cordis:on("compact/completed", function(ev) if ev.changed then COMMITS = COMMITS + 1 end end)
            L:provider("faux").stream = function(req, sink)
                if req.purpose == "compact" then
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "## Summary\nSTALE-SUMMARY" } } }
                end
                SENT = mare.json.encode(req.messages)
                sink({ type = "text_delta", delta = "done" })
                return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
            end
        "###).await;
        let result = h.prompt("CURRENT", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let (builds, commits, history, sent): (i64, i64, String, String) = eval(&h,
            "return BUILDS, COMMITS, mare.json.encode(A:list_messages().data), SENT or ''").await;
        assert_eq!((builds, commits), (1, 0), "a stale compaction must not commit");
        assert!(history.contains("CONCURRENT-EDIT") && !history.contains("STALE-SUMMARY"));
        assert!(sent.contains("OLD-1") && !sent.contains("CONCURRENT-EDIT"), "keep the original prepared request");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn cancelling_prepared_compaction_does_not_publish_a_partial_summary() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let h = env
            .host()
            .builtins(&["compact", "usage"])
            .faux(json!([]))
            .config(json!({ "plugins": { "compact": {
            "threshold_tokens": base + 2700, "keep_recent_tokens": 1,
            "recent_user_tokens": 0, "stages": ["local"]
        } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        global(&h, "K", h.service_table("compact").unwrap());
        exec(&h, &long_history(4)).await;
        exec(
            &h,
            r#"
            BUILDS, SUMMARIES, CANCELLED, CHANGED, SENT = 0, 0, false, false, nil
            C.cordis:on("request/build", function(req, next)
                BUILDS = BUILDS + 1
                req.system = req.system .. string.rep("system-extra ", 350)
                req.messages[#req.messages + 1] = { role = "user", content = {
                    { type = "text", text = "EXTRA " .. string.rep("message-extra ", 200) } } }
                return next()
            end)
            C.cordis:on("compact/before", function() K:cancel() end)
            C.cordis:on("compact/completed", function(ev) CANCELLED, CHANGED = ev.cancelled == true, ev.changed end)
            L:provider("faux").stream = function(req, sink)
                if req.purpose == "compact" then SUMMARIES = SUMMARIES + 1 end
                SENT = mare.json.encode(req.messages)
                sink({ type = "text_delta", delta = "done" })
                return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
            end
        "#,
        )
        .await;
        let result = h.prompt("CURRENT", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let (builds, summaries, cancelled, changed, sent): (i64, i64, bool, bool, String) =
            eval(&h, "return BUILDS, SUMMARIES, CANCELLED, CHANGED, SENT or ''").await;
        assert_eq!((builds, summaries, cancelled, changed), (1, 0, true, false));
        assert!(sent.contains("OLD-1") && sent.contains("EXTRA"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fallback_requests_rebuild_once_and_share_only_committed_history() {
    local(async {
        let env = TestEnv::new().await;
        let base = super::stages::overhead(&env).await;
        let cfg = json!({ "fallback_models": ["cmpfb/one", "cmpfb/two"], "plugins": {
            "llm": { "retries": 0, "overloaded_retries": 0 },
            "compact": { "threshold_tokens": base + 3500, "keep_recent_tokens": 1,
                         "recent_user_tokens": 0, "stages": ["local"] }
        } });
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, &long_history(4)).await;
        exec(&h, r###"
            BUILDS, MAPS, REQUESTS, COMPACTED, SUMMARY_REQUEST = {}, {}, {}, 0, nil
            local apply = L.apply_params
            L.apply_params = function(self, req, ...)
                MAPS[req.model.ref] = (MAPS[req.model.ref] or 0) + 1
                return apply(self, req, ...)
            end
            C.cordis:on("compact/completed", function(ev)
                if ev.reason == "auto" and ev.status == "completed" then COMPACTED = COMPACTED + 1 end
            end)
            C.cordis:on("request/build", function(req, next)
                local ref = req.model.ref
                BUILDS[ref] = (BUILDS[ref] or 0) + 1
                if ref == "faux/echo" then req.system = req.system .. " PRIMARY-MARK" end
                if ref == "cmpfb/one" then req.system = req.system .. " FALLBACK-ONE " .. string.rep("x", 10200) end
                if ref == "cmpfb/two" then req.system = req.system .. " FALLBACK-TWO" end
                return next()
            end)
            L:provider("faux").stream = function(req, sink)
                if req.purpose == "compact" then
                    SUMMARY_REQUEST = mare.json.encode({ system = req.system, messages = req.messages })
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "## Summary\nFALLBACK-SUMMARY" } } }
                end
                REQUESTS[req.model.ref] = mare.json.encode({ system = req.system, messages = req.messages })
                error({ message = "quota", kind = "quota", retryable = false })
            end
            L:register_provider({ provider = "cmpfb", is_configured = function() return true end,
                models = { { model_id = "one", context_window = 100000, max_output = 4096 },
                           { model_id = "two", context_window = 100000, max_output = 4096 } },
                stream = function(req, sink)
                    REQUESTS[req.model.ref] = mare.json.encode({ system = req.system, messages = req.messages })
                    if req.model.id == "one" then error({ message = "quota", kind = "quota", retryable = false }) end
                    sink({ type = "text_delta", delta = "done" })
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
                end })
        "###).await;
        let res = h.prompt("CURRENT", json!({})).await.unwrap();
        let (primary, one, two, compacted, maps, first, second, summary): (i64, i64, i64, i64, String, String, String, String) = eval(
            &h, r#"return BUILDS["faux/echo"] or 0, BUILDS["cmpfb/one"] or 0, BUILDS["cmpfb/two"] or 0,
                COMPACTED, (MAPS["faux/echo"] or 0) .. "," .. (MAPS["cmpfb/one"] or 0) .. "," .. (MAPS["cmpfb/two"] or 0),
                REQUESTS["cmpfb/one"] or "", REQUESTS["cmpfb/two"] or "", SUMMARY_REQUEST or """#,
        ).await;
        assert!(res["error"].is_null(), "{res}");
        assert_eq!((primary, one, two, compacted), (1, 1, 1, 1));
        assert_eq!(maps, "1,1,1", "each model resolves its overlay once");
        assert!(first.contains("PRIMARY-MARK") && first.contains("FALLBACK-ONE") && first.contains("FALLBACK-SUMMARY"));
        assert!(second.contains("PRIMARY-MARK") && second.contains("FALLBACK-TWO") && second.contains("FALLBACK-SUMMARY"));
        assert!(!second.contains("FALLBACK-ONE"), "fallback-only injection must not enter the shared baseline");
        assert!(!summary.contains("<analysis>"), "summary request must not ask for an analysis scratchpad");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn network_retry_reuses_the_prepared_request_and_build_hook() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).config(json!({ "plugins": {
            "llm": { "retries": 1, "retry_base_ms": 1 }, "compact": { "auto": false }
        } })).no_session().build().await;
        expose(&h);
        global(&h, "L", h.service_table("llm").unwrap());
        exec(&h, r#"
            BUILDS, MAPS, REQUESTS = 0, 0, {}
            local apply = L.apply_params
            L.apply_params = function(self, ...) MAPS = MAPS + 1 return apply(self, ...) end
            C.cordis:on("request/build", function(req, next) BUILDS = BUILDS + 1 return next() end)
            L:provider("faux").stream = function(req, sink)
                REQUESTS[#REQUESTS + 1] = mare.json.encode({ system = req.system, messages = req.messages, tools = req.tools })
                if #REQUESTS == 1 then error({ message = "temporary", kind = "server", retryable = true }) end
                sink({ type = "text_delta", delta = "done" })
                return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "done" } } }
            end
        "#).await;
        let res = h.prompt("CURRENT", json!({})).await.unwrap();
        let (builds, maps, attempts, identical): (i64, i64, i64, bool) = eval(&h, "return BUILDS, MAPS, #REQUESTS, REQUESTS[1] == REQUESTS[2]").await;
        assert!(res["error"].is_null(), "{res}");
        assert_eq!((builds, maps, attempts, identical), (1, 1, 2, true));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn default_headroom_keeps_a_full_64k_output_reserve_with_a_128k_window() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
        global(&h, "K", h.service_table("compact").unwrap());
        let (threshold, err): (Option<i64>, String) = eval(
            &h,
            r#"
            local threshold, err = K:threshold({ ref = "test/default-budget", context_window = 128000, max_output = 64000 })
            return threshold, tostring(err or "")
        "#,
        )
        .await;
        assert_eq!(threshold, Some(31232), "default budget must retain the full output reserve: {err}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn budgets_honor_output_cap_and_reject_nonfinite_values() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host().builtins(&["compact", "usage"])
            .faux(json!([]))
            .config(json!({ "plugins": { "compact": {
            "threshold_ratio": 1, "headroom_tokens": 10000, "stages": ["local"]
        } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        let (threshold, explicit, fallback, invalid, empty_tools): (i64, i64, i64, bool, i64) = eval(
            &h,
            r#"
            local model = { ref = "test/big", context_window = 300000, max_output = 64000 }
            local threshold = K:threshold(model)
            A:set_messages({ { role = "user", content = { { type = "text", text = "history" } } } })
            local history = A:list_messages().data
            local observed
            C.cordis:on("compact/before", function(ev) observed = ev.threshold return true end)
            K:prepare_request({ model = model, max_tokens = 80000, system = string.rep("x", 900000),
                messages = history, tools = {} }, history)
            local cfg = { threshold_ratio = 1, headroom_tokens = 10000, model_thresholds = {} }
            local metrics = require("compact.measure").create(C, cfg, { svc = {} })
            local fallback = assert(metrics.budgets({ ref = "test/default", context_window = 100000 })).pressure
            local invalid = true
            for _, value in ipairs({ 0/0, math.huge, -1, 1.5, "bad" }) do
                local b = metrics.budgets(model, value)
                invalid = invalid and b == nil
            end
            for _, name in ipairs({ "threshold_ratio", "threshold_percent", "threshold_tokens", "headroom_tokens", "summary_max_tokens" }) do
                local old = cfg[name]
                cfg[name] = 0/0
                invalid = invalid and not metrics.validate_config()
                cfg[name] = old
            end
            for _, value in ipairs({ 0, -1, 1.5, math.huge }) do
                cfg.summary_max_tokens = value
                invalid = invalid and not metrics.validate_config()
            end
            cfg.summary_max_tokens = nil
            for _, name in ipairs({ "ratio", "percent", "tokens", "headroom_tokens" }) do
                cfg.model_thresholds = { ["test/big"] = { [name] = 0/0 } }
                invalid = invalid and not metrics.validate_config()
            end
            local empty_tools = metrics.request_tokens({system = '', tools = {}, messages = mare.json.array()})
            return threshold, observed, fallback, invalid, empty_tools
        "#,
        )
        .await;
        assert_eq!(threshold, 226000);
        assert_eq!(explicit, 210000, "request max_tokens overrides the model reserve");
        assert_eq!(fallback, 73616, "missing output limit uses the 16384 catalog default");
        assert_eq!(empty_tools, 0, "an empty tools list adds no token overhead");
        assert!(invalid, "NaN, infinity, fractional and invalid caps or thresholds must be rejected");
        h.shutdown().await;
    })
    .await
}

/// reference implementation rejects requests with no remaining budget.
#[tokio::test]
async fn prepared_request_rejects_exhausted_pressure_and_unserializable_messages() {
    local(async {
        for mode in ["budget", "unserializable"] {
            let env = TestEnv::new().await;
            let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
            expose(&h);
            global(&h, "L", h.service_table("llm").unwrap());
            global(&h, "K", h.service_table("compact").unwrap());
            global(&h, "MODE", mode);
            if mode == "unserializable" {
                let (ok, error): (bool, String) = eval(
                    &h,
                    r#"
                    A:set_messages({{role = 'user', content = {{type = 'text', text = 'history'}}}})
                    local history = C.cordis:get('agent'):messages()
                    local messages = require('mare_lib').copy_deep(history)
                    messages[1].unsupported = function() end
                    local ok, err = pcall(K.prepare_request, K,
                        {model = L:current(), messages = messages, system = '', tools = {}}, history)
                    return ok, tostring(err)
                "#,
                )
                .await;
                assert!(!ok, "strict message validation must reject an unserializable field: {error}");
            }
            exec(
                &h,
                r#"
                SENT = 0
                C.cordis:on('request/build', function(req, next)
                    if MODE == 'budget' then req.max_tokens = req.model.context_window - 32768
                    else req.messages[1].unsupported = function() end end
                    return next()
                end)
                L:provider('faux').stream = function() SENT = SENT + 1; error('provider must not run') end
            "#,
            )
            .await;
            let result = h.prompt("input", json!({})).await.unwrap();
            assert!(!result["error"].is_null(), "{mode}: invalid request must fail: {result}");
            if mode == "budget" {
                assert!(result["error"]["message"].as_str().unwrap_or("").contains("pressure budget"), "{result}");
            }
            assert_eq!(eval::<i64>(&h, "return SENT").await, 0, "{mode}: provider must not be called");
            h.shutdown().await;
        }
    })
    .await
}

#[tokio::test]
async fn long_history_can_be_matched_rebuilt_and_measured_without_stack_expansion() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["compact", "usage"]).faux(json!([])).no_session().build().await;
        let (count, first, last, tokens, found): (i64, String, String, i64, i64) = eval(
            &h,
            r#"
            local lib = require("mare_lib")
            local history = mare.json.array()
            for i = 1, 120001 do
                history[i] = { role = "user", content = { { type = "text", text = tostring(i) } } }
            end
            mare.task.sleep(1)
            local request = lib.copy_deep(history)
            mare.task.sleep(1)
            local found = lib.find_history(request, history)
            mare.task.sleep(1)
            local rebuilt = lib.replace_history(request, found, #history, history)
            mare.task.sleep(1)
            return #rebuilt, rebuilt[1].content[1].text, rebuilt[#rebuilt].content[1].text,
                lib.estimate_messages(rebuilt), found
        "#,
        )
        .await;
        assert_eq!((count, first.as_str(), last.as_str(), found), (120001, "1", "120001", 1));
        assert!(tokens > 120001);
        h.shutdown().await;
    })
    .await
}

/// Verify compact plugin config schema defaults (40k / 20k / 8192 / 4096 / 1024, plus
/// headroom, backoff, and retain_ratio) without overriding them.
#[tokio::test]
async fn compact_default_config_values_match_spec() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env.host().builtins(&["compact"]).no_session();
        b.opts.load_init = true;
        let h = b.build().await;
        let vals: (i64, i64, i64, i64, i64, i64, i64, i64, f64) = eval(
            &h,
            r#"local keys = {
                 "plugins.compact.prune_protect_tokens",
                 "plugins.compact.prune_min_saving_tokens",
                 "plugins.compact.prune_tool_result_chars",
                 "plugins.compact.prune_head_chars",
                 "plugins.compact.prune_tail_chars",
                 "plugins.compact.headroom_tokens",
                 "plugins.compact.failure_backoff_ms",
                 "plugins.compact.failure_backoff_max_ms",
                 "plugins.compact.retain_ratio",
               }
               local d = mare.config_describe(keys)
               return d[keys[1]].default, d[keys[2]].default, d[keys[3]].default,
                      d[keys[4]].default, d[keys[5]].default, d[keys[6]].default,
                      d[keys[7]].default, d[keys[8]].default, d[keys[9]].default"#,
        )
        .await;
        assert_eq!(vals.0, 40_000, "prune_protect_tokens default");
        assert_eq!(vals.1, 20_000, "prune_min_saving_tokens default");
        assert_eq!(vals.2, 8_192, "prune_tool_result_chars default");
        assert_eq!(vals.3, 4_096, "prune_head_chars default");
        assert_eq!(vals.4, 1_024, "prune_tail_chars default");
        assert_eq!(vals.5, 32_768, "headroom_tokens default");
        assert_eq!(vals.6, 30_000, "failure_backoff_ms default");
        assert_eq!(vals.7, 600_000, "failure_backoff_max_ms default");
        assert!((vals.8 - 0.16).abs() < 1e-9, "retain_ratio default: {}", vals.8);
        h.shutdown().await;
    })
    .await
}
