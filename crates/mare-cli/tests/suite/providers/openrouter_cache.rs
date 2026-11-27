//! OpenRouter + Claude prompt caching (`cache_control`), session affinity (`x-session-id`),
//! and `prompt_cache_key` behavior in `provider-openai-compat.lua`.
//!
//! References:
//! - `pi` `packages/ai/src/api/openai-completions.ts:1068-1182` (`getCompatCacheControl`,
//!   `applyAnthropicCacheControl`), `:1606,1643,1682-1684` (`detectCompat`)
//! - `pi` `packages/ai/src/api/openai-completions.ts:761-771` (`x-session-id` for OpenRouter),
//!   `:808-822` (`prompt_cache_key`) & `openai-prompt-cache.ts:1-8` (`clampOpenAIPromptCacheKey`)

use super::*;

#[tokio::test]
async fn openrouter_claude_attaches_cache_control_to_system_last_user_and_last_tool() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let server = MockServer::start().await;
        server.route("/openrouter", vec![Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "openrouter": { "base_url": format!("{}/openrouter", server.base) }
            } }),
        )
        .await;
        expose(&h);

        exec(
            &h,
            r#"
            local m = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-5-5"))
            local msgs = {
              { role = "user", content = { { type = "text", text = "first turn" } } },
              { role = "assistant", content = { { type = "text", text = "first reply" } } },
              { role = "user", content = {
                { type = "text", text = "second turn" },
                { type = "image", mime = "image/png", data = "AAAA" },
              } },
            }
            local tools = {
              { name = "read", description = "Read", parameters = { type = "object", properties = {} } },
              { name = "write", description = "Write", parameters = { type = "object", properties = {} } },
            }
            LLM:stream({
              model = m,
              system = "SYSTEM-PROMPT",
              messages = msgs,
              tools = tools,
              session_id = "sess-or-cache",
            }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();

        // 1. System message converted to array with cache_control on text block
        let sys = &body["messages"][0];
        assert_eq!(sys["role"], "system");
        assert_eq!(
            sys["content"],
            json!([{ "type": "text", "text": "SYSTEM-PROMPT", "cache_control": { "type": "ephemeral" } }])
        );

        // 2. Earlier user/assistant messages do not have cache_control
        assert!(body["messages"][1]["content"].is_string(), "first user remains plain string: {body}");
        assert!(body["messages"][2]["content"].is_string(), "assistant remains plain string: {body}");

        // 3. Last user message's last text block gets cache_control (image_url left untouched)
        let last_user = &body["messages"][3];
        assert_eq!(last_user["role"], "user");
        assert_eq!(
            last_user["content"][0]["cache_control"],
            json!({ "type": "ephemeral" }),
            "last text part in last user message: {last_user}"
        );
        assert!(last_user["content"][1].get("cache_control").is_none());

        // 4. Only the last tool gets cache_control
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert!(tools[0].get("cache_control").is_none(), "first tool must not have cache_control");
        assert_eq!(tools[1]["cache_control"], json!({ "type": "ephemeral" }), "last tool must have cache_control");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openrouter_claude_respects_cache_disable_and_matches_openrouter_url() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        env.set_var("CUSTOM_OR_KEY", "test-custom-or");
        let server = MockServer::start().await;
        server.route(
            "/openrouter",
            vec![Resp::sse(fixture("openai-compat.sse")), Resp::sse(fixture("openai-compat.sse"))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "openrouter": { "base_url": format!("{}/openrouter", server.base) },
                "custom_or": {
                    "base_url": "https://openrouter.ai/api/v1",
                    "api_key_env": "CUSTOM_OR_KEY",
                    "models": [{ "model_id": "anthropic/claude-3.5-sonnet", "context_window": 200000, "max_output": 8192 }]
                }
            } }),
        )
        .await;
        expose(&h);
        global(&h, "SSE", fixture("openai-compat.sse"));

        // cache = false and cache = "read" must suppress cache_control on OpenRouter + Claude
        exec(
            &h,
            r#"
            local m = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-5-5"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
            local tools = { { name = "read", description = "Read", parameters = { type = "object", properties = {} } } }
            LLM:stream({ model = m, system = "S", messages = msgs, tools = tools, cache = false }, function() end)
            LLM:stream({ model = m, system = "S", messages = msgs, tools = tools, cache = "read" }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        for r in &reqs {
            assert!(
                !r.body.contains("cache_control"),
                "cache=false / cache=read must not emit cache_control: {}",
                r.body
            );
        }

        // Custom endpoint whose base_url is openrouter.ai also gets cache_control and x-session-id
        let captured_json: String = eval(
            &h,
            r#"
            local captured
            C.cordis:on("provider/transport", function(t)
                captured = { headers = t.request.headers, body = t.request.body }
                local evs = {}
                for block in (SSE .. "\n\n"):gmatch("(.-)\n\n") do
                    local data = block:match("data: ([^\n]*)")
                    if data then evs[#evs + 1] = { data = data } end
                end
                local i = 0
                return { ok = true, status = 200, headers = {}, next_event = function() i = i + 1; return evs[i] end }
            end)
            local m = assert(LLM:resolve("custom_or/anthropic/claude-3.5-sonnet"))
            LLM:stream({
              model = m,
              system = "Custom OR system",
              session_id = "sess-custom-or",
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function() end)
            return mare.json.encode(captured)
            "#,
        )
        .await;
        let captured: Value = serde_json::from_str(&captured_json).unwrap();

        assert_eq!(captured["headers"]["x-session-id"], "sess-custom-or");
        assert_eq!(captured["body"]["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(captured["body"]["messages"][1]["content"][0]["cache_control"], json!({ "type": "ephemeral" }));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openrouter_non_anthropic_and_other_endpoints_omit_cache_control() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let server = MockServer::start().await;
        server.route("/openrouter", vec![Resp::sse(fixture("openai-compat.sse"))]);
        server.route("/compat", vec![Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "openrouter": { "base_url": format!("{}/openrouter", server.base) }
            } }),
        )
        .await;
        expose(&h);

        exec(
            &h,
            r#"
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
            local tools = { { name = "read", description = "Read", parameters = { type = "object", properties = {} } } }
            local m_or_ds = assert(LLM:resolve("openrouter/deepseek/deepseek-chat"))
            LLM:stream({ model = m_or_ds, system = "S", messages = msgs, tools = tools, session_id = "sess-or-ds" }, function() end)
            local m_ds = assert(LLM:resolve("deepseek/deepseek-chat"))
            LLM:stream({ model = m_ds, system = "S", messages = msgs, tools = tools, session_id = "sess-ds" }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);

        // OpenRouter non-anthropic model: no cache_control, but keeps x-session-id
        assert!(!reqs[0].body.contains("cache_control"), "{}", reqs[0].body);
        assert_eq!(reqs[0].header("x-session-id"), Some("sess-or-ds"));

        // Non-OpenRouter endpoint (deepseek): no cache_control, no x-session-id, no prompt_cache_key
        assert!(!reqs[1].body.contains("cache_control"), "{}", reqs[1].body);
        assert_eq!(reqs[1].header("x-session-id"), None);
        let ds_body: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert!(ds_body.get("prompt_cache_key").is_none(), "{}", ds_body);

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openrouter_sends_x_session_id_and_openai_compat_clamps_prompt_cache_key() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        env.set_var("CUSTOM_OAI_KEY", "test-custom-oai");
        let server = MockServer::start().await;
        server.route("/openrouter", vec![Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "openrouter": { "base_url": format!("{}/openrouter", server.base) },
                "custom_oai": {
                    "base_url": "https://api.openai.com/v1",
                    "api_key_env": "CUSTOM_OAI_KEY",
                    "models": [{ "model_id": "gpt-4o", "context_window": 128000, "max_output": 16384 }]
                }
            } }),
        )
        .await;
        expose(&h);
        global(&h, "SSE", fixture("openai-compat.sse"));

        exec(
            &h,
            r#"
            local m = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-5-5"))
            LLM:stream({
              model = m,
              system = "S",
              session_id = "sess-or-sticky-1",
              messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].header("x-session-id"), Some("sess-or-sticky-1"));
        let or_body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert!(or_body.get("prompt_cache_key").is_none(), "OpenRouter should not send prompt_cache_key: {or_body}");

        // OpenAI-hosted chat-completions endpoint gets clamped 64-char prompt_cache_key (pi openai-prompt-cache.ts:1-8)
        let long_sid = "s".repeat(80);
        let captured_json: String = eval(
            &h,
            &format!(
                r#"
                local captured
                C.cordis:on("provider/transport", function(t)
                    captured = {{ headers = t.request.headers, body = t.request.body }}
                    local evs = {{}}
                    for block in (SSE .. "\n\n"):gmatch("(.-)\n\n") do
                        local data = block:match("data: ([^\n]*)")
                        if data then evs[#evs + 1] = {{ data = data }} end
                    end
                    local i = 0
                    return {{ ok = true, status = 200, headers = {{}}, next_event = function() i = i + 1; return evs[i] end }}
                end)
                local m = assert(LLM:resolve("custom_oai/gpt-4o"))
                LLM:stream({{
                  model = m,
                  system = "S",
                  session_id = "{long_sid}",
                  messages = {{ {{ role = "user", content = {{ {{ type = "text", text = "hi" }} }} }} }},
                }}, function() end)
                return mare.json.encode(captured)
                "#
            ),
        )
        .await;
        let captured: Value = serde_json::from_str(&captured_json).unwrap();

        assert_eq!(
            captured["body"]["prompt_cache_key"].as_str().map(str::len),
            Some(64),
            "prompt_cache_key must be clamped to 64 chars: {captured}"
        );
        assert!(
            captured["headers"].get("x-session-id").is_none(),
            "non-OpenRouter endpoint must not get x-session-id: {captured}"
        );

        h.shutdown().await;
    })
    .await
}
