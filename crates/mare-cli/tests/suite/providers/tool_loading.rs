//! End-to-end tool loading tests:
//! - `tool_update` capability validation (rejects `"in-history"` and booleans)
//! - Prefix byte-stability on DeepSeek, GLM, and older OpenAI (`gpt-5.3`): 3× `tool_search` + `call_tool`
//! - Resume (non-native & native), compaction, and mid-session model switch (non-native <-> Anthropic)
//! - Per-wire tolerance for undeclared historical tool names across all 4 wires

use super::request_shape::anthropic_tool_use_sse;
use super::*;
use crate::suite::kernel::write_plugin;

const MULTI_DEFERRED_PLUGIN: &str = r#"return {
  name = "tool-loading-e2e",
  inject = { "tools" },
  apply = function(ctx)
    for _, spec in ipairs({
      { name = "alpha_lookup", desc = "Lookup alpha records by key", prop = "key" },
      { name = "beta_query", desc = "Query beta items by filter", prop = "filter" },
      { name = "gamma_fetch", desc = "Fetch gamma detail by id", prop = "id" },
    }) do
      ctx.tools:register(spec.name, {
        description = spec.desc,
        exposure = "deferred",
        side_effect = "read",
        parameters = {
          type = "object",
          properties = { [spec.prop] = { type = "string" } },
          required = { spec.prop },
        },
        execute = function(a)
          return { content = spec.name .. ":" .. tostring(a[spec.prop] or "") }
        end,
      })
    end
  end,
}"#;

fn chat_tool_call_sse(model: &str, id: &str, name: &str, args_json: &str) -> Resp {
    let c0 = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args_json },
                }],
            },
            "finish_reason": Value::Null,
        }],
    });
    let c1 = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
        "usage": { "prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60 },
    });
    Resp::sse(format!("data: {c0}\n\ndata: {c1}\n\ndata: [DONE]\n\n"))
}

fn chat_text_sse(model: &str, text: &str) -> Resp {
    let c0 = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": model,
        "choices": [{ "index": 0, "delta": { "role": "assistant", "content": text }, "finish_reason": Value::Null }],
    });
    let c1 = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": model,
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 60, "completion_tokens": 5, "total_tokens": 65 },
    });
    Resp::sse(format!("data: {c0}\n\ndata: {c1}\n\ndata: [DONE]\n\n"))
}

fn responses_fn_call_sse(id: &str, name: &str, args_json: &str) -> Resp {
    Resp::sse(
        [
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": format!("fc_{id}"),
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": args_json
                }
            })),
            responses_event(json!({
                "type": "response.completed",
                "response": { "usage": { "input_tokens": 100, "output_tokens": 10 } }
            })),
        ]
        .concat(),
    )
}

#[tokio::test]
async fn validate_rejects_unsupported_tool_update_values() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let out_json: String = eval(
            &h,
            r#"
            local V = require("llm.validate")
            local ok_in_hist, _, r1 = V.provider({ provider = "bad1", tool_update = "in-history", stream = function() end })
            local ok_bool, _, r2 = V.provider({ provider = "bad2", tool_update = true, stream = function() end })
            local ok_model_bad, _, r3 = V.provider({
              provider = "bad3",
              stream = function() end,
              models = { { model_id = "m1", tool_update = "in-history" } },
            })
            local ok_good, _, r4 = V.provider({
              provider = "good1",
              tool_update = "addition_only",
              stream = function() end,
              models = { { model_id = "m1", tool_update = "addition_only" } },
            })
            return mare.json.encode({
              ok_in_hist = ok_in_hist, r1 = r1,
              ok_bool = ok_bool, r2 = r2,
              ok_model_bad = ok_model_bad, r3 = r3,
              ok_good = ok_good, r4 = r4,
            })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["ok_in_hist"], false, "{out}");
        assert_eq!(out["ok_bool"], false, "{out}");
        assert_eq!(out["ok_model_bad"], false, "{out}");
        assert_eq!(out["ok_good"], true, "{out}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn non_native_vendors_keep_tools_and_system_byte_stable_across_three_searches_and_call_tool() {
    local(async {
        let mut env = TestEnv::new().await;
        write_plugin(&env, "tool-loading-e2e", MULTI_DEFERRED_PLUGIN);
        env.set_var("DEEPSEEK_API_KEY", "test-ds");
        env.set_var("OPENAI_API_KEY", "test-oai");

        let server = MockServer::start().await;
        // 1. DeepSeek chat (openai-chat)
        server.route(
            "/deepseek/chat/completions",
            vec![
                chat_tool_call_sse("deepseek-chat", "c1", "tool_search", r#"{"query":"alpha"}"#),
                chat_tool_call_sse("deepseek-chat", "c2", "tool_search", r#"{"query":"beta"}"#),
                chat_tool_call_sse("deepseek-chat", "c3", "tool_search", r#"{"query":"select:gamma_fetch"}"#),
                chat_tool_call_sse(
                    "deepseek-chat",
                    "c4",
                    "call_tool",
                    r#"{"name":"gamma_fetch","arguments":{"id":"g42"}}"#,
                ),
                chat_text_sse("deepseek-chat", "ds done"),
            ],
        );
        // 2. GLM (openai-chat)
        server.route(
            "/glm/chat/completions",
            vec![
                chat_tool_call_sse("glm-5", "g1", "tool_search", r#"{"query":"alpha"}"#),
                chat_tool_call_sse("glm-5", "g2", "tool_search", r#"{"query":"beta"}"#),
                chat_tool_call_sse("glm-5", "g3", "tool_search", r#"{"query":"select:alpha_lookup"}"#),
                chat_tool_call_sse("glm-5", "g4", "call_tool", r#"{"name":"alpha_lookup","arguments":{"key":"k1"}}"#),
                chat_text_sse("glm-5", "glm done"),
            ],
        );
        // 3. Older OpenAI (gpt-5.3, openai-responses without tool_update)
        server.route(
            "/openai/responses",
            vec![
                responses_fn_call_sse("o1", "tool_search", r#"{"query":"alpha"}"#),
                responses_fn_call_sse("o2", "tool_search", r#"{"query":"beta"}"#),
                responses_fn_call_sse("o3", "tool_search", r#"{"query":"select:beta_query"}"#),
                responses_fn_call_sse("o4", "call_tool", r#"{"name":"beta_query","arguments":{"filter":"f9"}}"#),
                Resp::sse(responses_text("oai53 done")),
            ],
        );

        let base = &server.base;
        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "providers": {
                    "deepseek": { "base_url": format!("{base}/deepseek") },
                    "glm-test": {
                        "wire": "openai-chat",
                        "base_url": format!("{base}/glm"),
                        "api_key": "test-glm",
                        "models": [{ "model_id": "glm-5", "name": "GLM-5" }],
                    },
                },
                "plugins": {
                    "provider-openai": { "base_url": format!("{base}/openai") },
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        for (model_ref, expected_text, expected_tool_out, path, is_responses) in [
            ("deepseek/deepseek-chat", "ds done", "gamma_fetch:g42", "/deepseek/chat/completions", false),
            ("glm-test/glm-5", "glm done", "alpha_lookup:k1", "/glm/chat/completions", false),
            ("openai/gpt-5.3", "oai53 done", "beta_query:f9", "/openai/responses", true),
        ] {
            exec(&h, &format!(r#"assert(LLM:set_model("{model_ref}", {{ save = false }}))"#)).await;
            let marks_before: usize = eval(
                &h,
                r#"local n = 0
                   for _, e in ipairs(mare.profile.events and mare.profile.events() or {}) do
                     if e.label == "prompt cache invalidated: tool list changed" then n = n + 1 end
                   end
                   return n"#,
            )
            .await;
            let res = h.prompt("search three times and call", json!({})).await.unwrap();
            assert!(res["error"].is_null(), "{model_ref}: {res}");
            assert_eq!(res["text"], expected_text, "{model_ref}: {res}");
            assert!(
                res["messages"].to_string().contains(expected_tool_out),
                "{model_ref} missing inner tool output {expected_tool_out}: {res}"
            );

            let marks_after: usize = eval(
                &h,
                r#"local n = 0
                   for _, e in ipairs(mare.profile.events and mare.profile.events() or {}) do
                     if e.label == "prompt cache invalidated: tool list changed" then n = n + 1 end
                   end
                   return n"#,
            )
            .await;
            assert_eq!(
                marks_after, marks_before,
                "{model_ref}: prompt cache invalidation mark must not fire across 3x tool_search + call_tool"
            );

            let reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path == path).collect();
            assert_eq!(reqs.len(), 5, "{model_ref}: expected 5 requests, got {}", reqs.len());
            let b0 = reqs[0].json();
            let tools0 = serde_json::to_string(&b0["tools"]).unwrap();
            assert!(tools0.contains("tool_search") && tools0.contains("call_tool"), "{model_ref}: {tools0}");
            assert!(!tools0.contains("alpha_lookup") && !tools0.contains("gamma_fetch"), "{model_ref}: {tools0}");
            let sys0 = if is_responses {
                b0["instructions"].as_str().unwrap().to_string()
            } else {
                b0["messages"][0]["content"].as_str().unwrap().to_string()
            };
            for (idx, req) in reqs.iter().enumerate().skip(1) {
                let bi = req.json();
                let toolsi = serde_json::to_string(&bi["tools"]).unwrap();
                let sysi = if is_responses {
                    bi["instructions"].as_str().unwrap().to_string()
                } else {
                    bi["messages"][0]["content"].as_str().unwrap().to_string()
                };
                assert_eq!(toolsi, tools0, "{model_ref} step {idx}: tools[] drifted!");
                assert_eq!(sysi, sys0, "{model_ref} step {idx}: system prompt drifted!");
            }
        }

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn resume_preserves_non_native_tools_and_native_activation_records() {
    local(async {
        let mut env = TestEnv::new().await;
        write_plugin(&env, "tool-loading-e2e", MULTI_DEFERRED_PLUGIN);
        env.set_var("DEEPSEEK_API_KEY", "test-ds");
        env.set_var("ANTHROPIC_API_KEY", "test-ant");

        let server = MockServer::start().await;
        // Session 1 (non-native DeepSeek): search + call_tool; then resume and call_tool WITHOUT re-searching.
        server.route(
            "/deepseek/chat/completions",
            vec![
                chat_tool_call_sse("deepseek-chat", "ds_s1", "tool_search", r#"{"query":"select:alpha_lookup"}"#),
                chat_tool_call_sse(
                    "deepseek-chat",
                    "ds_c1",
                    "call_tool",
                    r#"{"name":"alpha_lookup","arguments":{"key":"before"}}"#,
                ),
                chat_text_sse("deepseek-chat", "session1 done"),
                chat_tool_call_sse(
                    "deepseek-chat",
                    "ds_c2",
                    "call_tool",
                    r#"{"name":"alpha_lookup","arguments":{"key":"resumed"}}"#,
                ),
                chat_text_sse("deepseek-chat", "resumed done"),
            ],
        );

        let base = server.base.clone();
        let ds_cfg = json!({
            "model": "deepseek/deepseek-chat",
            "providers": { "deepseek": { "base_url": format!("{base}/deepseek") } },
            "plugins": { "llm": { "retry_base_ms": 1 } },
        });
        let h1 = env.host().user_plugins().config(ds_cfg.clone()).build().await;
        let r1 = h1.prompt("first turn", json!({})).await.unwrap();
        assert_eq!(r1["text"], "session1 done", "{r1}");
        h1.shutdown().await;

        let h2 = env.host().user_plugins().config(ds_cfg).resume().build().await;
        let r2 = h2.prompt("call alpha again", json!({})).await.unwrap();
        assert_eq!(r2["text"], "resumed done", "{r2}");
        assert!(r2["messages"].to_string().contains("alpha_lookup:resumed"), "{r2}");
        h2.shutdown().await;
        drop(env);

        let ds_reqs: Vec<_> =
            server.requests().into_iter().filter(|r| r.path == "/deepseek/chat/completions").collect();
        assert_eq!(ds_reqs.len(), 5);
        assert_eq!(
            ds_reqs[3].json()["tools"],
            ds_reqs[0].json()["tools"],
            "resumed non-native tools[] must match pre-resume tools[]"
        );

        // Session 2 (native Anthropic): search + call; resume still emits tool_reference activation record.
        let mut env_ant = TestEnv::new().await;
        write_plugin(&env_ant, "tool-loading-e2e", MULTI_DEFERRED_PLUGIN);
        env_ant.set_var("ANTHROPIC_API_KEY", "test-ant");
        server.route(
            "/anthropic",
            vec![
                Resp::sse(anthropic_tool_use_sse("at1", "tool_search", r#"{"query":"select:alpha_lookup"}"#)),
                Resp::sse(anthropic_tool_use_sse("at2", "alpha_lookup", r#"{"key":"ant1"}"#)),
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        let ant_cfg = json!({
            "model": "anthropic/claude-sonnet-5-5",
            "plugins": {
                "provider-anthropic": { "base_url": format!("{base}/anthropic") },
                "llm": { "retry_base_ms": 1 },
            },
        });
        let ha1 = env_ant.host().user_plugins().config(ant_cfg.clone()).build().await;
        let ra1 = ha1.prompt("native turn 1", json!({})).await.unwrap();
        assert_eq!(ra1["text"], "All done.", "{ra1}");
        ha1.shutdown().await;

        let ha2 = env_ant.host().user_plugins().config(ant_cfg).resume().build().await;
        let ra2 = ha2.prompt("native turn 2", json!({})).await.unwrap();
        assert_eq!(ra2["text"], "All done.", "{ra2}");
        ha2.shutdown().await;

        let ant_reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path.starts_with("/anthropic")).collect();
        assert_eq!(ant_reqs.len(), 4);
        let resumed_body = ant_reqs[3].json();
        assert!(
            resumed_body["messages"].to_string().contains("tool_reference")
                && resumed_body["messages"].to_string().contains("alpha_lookup"),
            "resumed Anthropic request must still emit tool_reference activation record: {resumed_body}"
        );
    })
    .await
}

#[tokio::test]
async fn compaction_keeps_deferred_list_and_re_search_works() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "tool-loading-e2e", MULTI_DEFERRED_PLUGIN);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);

        let before: String = eval(&h, "return C.cordis:get('context'):system_prompt({ ref = 'test/non-native' })").await;
        exec(
            &h,
            r#"
            local r1 = T:execute({ id = "s1", name = "tool_search", arguments = { query = "select:alpha_lookup" } }, {})
            assert(r1.details and r1.details.loaded_tools[1] == "alpha_lookup")
            C.cordis:get("bus"):emit("compact/completed", { summary = "compacted" })
            C.cordis:get("context"):invalidate()
            "#,
        )
        .await;
        let after: String = eval(&h, "return C.cordis:get('context'):system_prompt({ ref = 'test/non-native' })").await;
        assert_eq!(after, before, "system prompt deferred list must stay unchanged after compaction");

        let research_and_call: String = eval(
            &h,
            r#"
            local r2 = T:execute({ id = "s2", name = "tool_search", arguments = { query = "select:alpha_lookup" } }, {})
            assert(r2.details and r2.details.loaded_tools[1] == "alpha_lookup")
            local c = T:execute({ id = "c2", name = "call_tool", arguments = { name = "alpha_lookup", arguments = { key = "post-compact" } } }, {})
            return type(c.content) == "table" and c.content[1].text or tostring(c.content)
            "#,
        )
        .await;
        assert_eq!(research_and_call, "alpha_lookup:post-compact");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mid_session_model_switch_and_all_four_wires_tolerate_undeclared_historical_tools() {
    local(async {
        let mut env = TestEnv::new().await;
        write_plugin(&env, "tool-loading-e2e", MULTI_DEFERRED_PLUGIN);
        let server = MockServer::start().await;

        let ant_ok = fixture("anthropic-text.sse");
        let google_ok = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"gem ok\"}]},\"finishReason\":\"STOP\"}]}\n\n";
        let compat_ok = compat_events(&[compat_text("compat ok"), compat_stop()]);

        // 1. Non-native (DeepSeek) turn 1: tool_search -> call_tool -> done; then Anthropic -> Non-native switch + undeclared check
        server.route(
            "/compat/chat/completions",
            vec![
                chat_tool_call_sse("deepseek-chat", "c_s1", "tool_search", r#"{"query":"select:alpha_lookup"}"#),
                chat_tool_call_sse(
                    "deepseek-chat",
                    "c_p1",
                    "call_tool",
                    r#"{"name":"alpha_lookup","arguments":{"key":"prev"}}"#,
                ),
                chat_text_sse("deepseek-chat", "ds turn 1"),
                chat_tool_call_sse(
                    "deepseek-chat",
                    "ds_proxy",
                    "call_tool",
                    r#"{"name":"alpha_lookup","arguments":{"key":"back"}}"#,
                ),
                chat_text_sse("deepseek-chat", "back ok"),
                Resp::sse(compat_ok),
            ],
        );
        // 2. Non-native -> Anthropic switch + undeclared historical tool name on Anthropic
        server.route(
            "/anthropic",
            vec![
                Resp::sse(anthropic_tool_use_sse("a_direct", "alpha_lookup", r#"{"key":"switched"}"#)),
                Resp::sse(ant_ok.clone()),
                Resp::sse(ant_ok.clone()),
            ],
        );
        // 3. Undeclared historical tool name on openai-responses & google-generate
        server.route("/openai", vec![Resp::sse(responses_text("oai ok"))]);
        server.route("/google", vec![Resp::sse(google_ok.to_string())]);

        let base = &server.base;
        for (k, v) in [
            ("OPENAI_API_KEY", "test-openai"),
            ("ANTHROPIC_API_KEY", "test-anthropic"),
            ("DEEPSEEK_API_KEY", "test-deepseek"),
            ("GEMINI_API_KEY", "test-google"),
        ] {
            env.set_var(k, v);
        }
        let h = env
            .host()
            .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
            .user_plugins()
            .config(json!({
                "model": "deepseek/deepseek-chat",
                "providers": { "deepseek": { "base_url": format!("{base}/compat") } },
                "plugins": {
                    "provider-openai": { "base_url": format!("{base}/openai") },
                    "provider-anthropic": { "base_url": format!("{base}/anthropic") },
                    "provider-google": { "base_url": format!("{base}/google") },
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        expose(&h);

        // A) Turn 1 on non-native DeepSeek: tool_search + call_tool.
        let res_ds1 = h.prompt("find and call alpha", json!({})).await.unwrap();
        assert_eq!(res_ds1["text"], "ds turn 1", "{res_ds1}");
        assert!(res_ds1["messages"].to_string().contains("alpha_lookup:prev"), "{res_ds1}");

        // B) Switch Non-native -> Anthropic: historical loaded_tools becomes tool_reference,
        // call_tool is dropped from tools[], and direct invocation of alpha_lookup succeeds.
        exec(&h, r#"assert(LLM:set_model("anthropic/claude-sonnet-5-5", { save = false }))"#).await;
        let res_ant = h.prompt("now call alpha directly", json!({})).await.unwrap();
        assert_eq!(res_ant["text"], "All done.", "{res_ant}");
        assert!(res_ant["messages"].to_string().contains("alpha_lookup:switched"), "{res_ant}");

        // C) Switch Anthropic -> Non-native: history now contains direct `alpha_lookup` call;
        // switching back to DeepSeek replays that direct call cleanly and declares `call_tool`.
        exec(&h, r#"assert(LLM:set_model("deepseek/deepseek-chat", { save = false }))"#).await;
        let res_ds2 = h.prompt("now call alpha via proxy", json!({})).await.unwrap();
        assert_eq!(res_ds2["text"], "back ok", "{res_ds2}");
        assert!(res_ds2["messages"].to_string().contains("alpha_lookup:back"), "{res_ds2}");

        // D) Explicit per-wire check across ALL 4 wires: undeclared historical tool name in history.
        exec(
            &h,
            r#"
            local undeclared_history = {
              { role = "user", content = { { type = "text", text = "hi" } } },
              { role = "assistant", content = {
                  { type = "tool_call", id = "call_legacy_1", name = "legacy_undeclared_tool", arguments = { x = 1 } },
              } },
              { role = "tool_result", tool_call_id = "call_legacy_1", tool_name = "legacy_undeclared_tool",
                content = { { type = "text", text = "legacy ok" } } },
              { role = "user", content = { { type = "text", text = "continue" } } },
            }
            local small_tools = {
              { name = "read", description = "Read", parameters = { type = "object", properties = {} } },
            }
            for _, ref in ipairs({
              "anthropic/claude-sonnet-5-5",
              "openai/gpt-6.1-terra",
              "deepseek/deepseek-chat",
              "google/gemini-3-pro-preview",
            }) do
              local m = assert(LLM:resolve(ref), "missing " .. ref)
              local r = LLM:stream({ model = m, system = "SYS", messages = undeclared_history, tools = small_tools }, function() end)
              assert(not r.error, ref .. " failed: " .. tostring(r.error))
            end
            "#,
        )
        .await;
        h.shutdown().await;

        let reqs = server.requests();
        // Verify Non-native -> Anthropic switch request:
        let ant_reqs: Vec<_> = reqs.iter().filter(|r| r.path.starts_with("/anthropic")).collect();
        let ant0 = ant_reqs[0].json();
        assert!(
            !ant0["tools"].as_array().unwrap().iter().any(|t| t["name"] == "call_tool"),
            "Anthropic tools[] must not contain call_tool"
        );
        assert!(
            ant0["messages"].to_string().contains("tool_reference")
                && ant0["messages"].to_string().contains("alpha_lookup"),
            "Non-native -> Anthropic must turn historical loaded_tools into tool_reference: {ant0}"
        );

        // Verify Anthropic -> Non-native switch request (ds_reqs[3] is the first request after switching back):
        let ds_reqs: Vec<_> = reqs.iter().filter(|r| r.path.starts_with("/compat")).collect();
        let ds_back = ds_reqs[3].json();
        assert!(
            ds_back["tools"].as_array().unwrap().iter().any(|t| t["function"]["name"] == "call_tool"),
            "Non-native tools[] must declare call_tool after switch"
        );
        assert!(
            ds_back["messages"].to_string().contains("alpha_lookup"),
            "Anthropic -> Non-native must replay historical direct alpha_lookup call: {ds_back}"
        );

        // Verify all 4 wires faithfully serialized `legacy_undeclared_tool` even though `tools` only declared `read`.
        let ant_legacy = ant_reqs.last().unwrap().json();
        assert!(ant_legacy["messages"].to_string().contains("legacy_undeclared_tool"), "{ant_legacy}");
        let ds_legacy = ds_reqs.last().unwrap().json();
        assert!(ds_legacy["messages"].to_string().contains("legacy_undeclared_tool"), "{ds_legacy}");
        let oai_legacy = reqs.iter().find(|r| r.path.starts_with("/openai")).unwrap().json();
        assert!(oai_legacy["input"].to_string().contains("legacy_undeclared_tool"), "{oai_legacy}");
        let gem_legacy = reqs.iter().find(|r| r.path.starts_with("/google")).unwrap().json();
        assert!(gem_legacy["contents"].to_string().contains("legacy_undeclared_tool"), "{gem_legacy}");
    })
    .await
}
