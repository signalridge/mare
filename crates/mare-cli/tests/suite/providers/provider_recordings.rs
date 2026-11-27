//! Replay tests for synthetic multi-provider protocol fixtures covering error payloads,
//! reasoning items, parallel tool calls, thought signatures, and thinking budgets.

use super::*;

#[tokio::test]
async fn recorded_gateway_provider_responses() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MOONSHOT_API_KEY", "test-moonshot");
        let server = MockServer::start().await;

        server.route(
            "/openai",
            vec![
                Resp::status(400, provider_fixture("openai-responses/400-unsupported-tool-choice.json")),
                Resp::sse(provider_fixture("openai-responses/encrypted-reasoning.sse")),
            ],
        );
        server.route("/compat", vec![Resp::sse(provider_fixture("openai-compat/parallel-tool-signatures.sse"))]);
        server.route("/kimi", vec![Resp::sse(provider_fixture("openai-compat/reasoning-content-tool.sse"))]);
        server.route("/google", vec![Resp::sse(provider_fixture("google/vertex-tool-call.sse"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "kimi": { "base_url": format!("{}/kimi", server.base) }
                },
                "plugins": {
                    "llm": { "retries": 0 }
                }
            }),
        )
        .await;

        // 1. 400 invalid_request_error JSON body
        let bad_req = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(bad_req["stop_reason"], "error", "{bad_req}");
        assert!(bad_req["error"].as_str().is_some_and(|e| e.contains("unsupported tool_choice")), "{bad_req}");

        // 2. OpenAI Responses reasoning + encrypted content + text
        let oai_reasoning = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(oai_reasoning["stop_reason"], "stop", "{oai_reasoning}");
        assert!(
            oai_reasoning["content"].as_array().unwrap().iter().any(|b| b["type"] == "reasoning"),
            "expected reasoning block: {oai_reasoning}"
        );
        assert!(!text_of(&oai_reasoning).is_empty(), "{oai_reasoning}");

        // 3. Gemini parallel tool calls over OpenAI-compat Chat SSE
        let parallel = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!(parallel["stop_reason"], "tool_use", "{parallel}");
        let calls: Vec<&Value> =
            parallel["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(calls.len(), 2, "expected 2 parallel get_weather calls: {calls:?}");
        assert_eq!(calls[0]["name"], "get_weather");
        assert_eq!(calls[0]["arguments"]["city"], "Paris");
        assert_eq!(calls[1]["name"], "get_weather");
        assert_eq!(calls[1]["arguments"]["city"], "Tokyo");
        assert_eq!(parallel["usage"]["input"], 74, "{parallel}");
        assert_eq!(parallel["usage"]["output"], 32, "{parallel}");

        // 4. Chat SSE with reasoning_content + tool_calls
        let kimi = ask(&h, "kimi/kimi-k2-thinking").await;
        assert_eq!(kimi["stop_reason"], "tool_use", "{kimi}");
        let kimi_content = kimi["content"].as_array().unwrap();
        assert!(kimi_content.iter().any(|b| b["type"] == "reasoning"), "{kimi}");
        assert!(kimi_content.iter().any(|b| b["type"] == "tool_call"), "{kimi}");
        assert_eq!(kimi["usage"]["input"], 84, "{kimi}");
        assert_eq!(kimi["usage"]["output"], 96, "{kimi}");

        // 5. Google Vertex Gemini SSE with thoughtSignature + functionCall
        let g = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(g["stop_reason"], "tool_use", "{g}");
        let g_content = g["content"].as_array().unwrap();
        let g_call = g_content.iter().find(|b| b["type"] == "tool_call").expect("tool_call");
        assert_eq!(g_call["name"], "lookup_weather");
        assert_eq!(g_call["arguments"]["city"], "Paris");
        assert!(
            g_call["native"]["google-generate"]["thought_signature"].as_str().is_some_and(|s| !s.is_empty()),
            "{g_call}"
        );
        assert_eq!(g["usage"]["input"], 39, "{g}");
        assert_eq!(g["usage"]["reasoning"], 101, "{g}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn recorded_turn0_tool_and_thinking_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        server.route("/anthropic", vec![Resp::sse(provider_fixture("anthropic/tool-call-turn0.sse"))]);
        server.route("/openai", vec![Resp::sse(provider_fixture("openai-responses/thinking-budget.sse"))]);
        server.route("/google", vec![Resp::sse(provider_fixture("google/sampling-tools-turn0.sse"))]);

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let ant = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(ant["stop_reason"], "tool_use", "{ant}");
        let ant_call = ant["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_call").unwrap();
        assert_eq!(ant_call["name"], "list_directory");
        assert_eq!(ant_call["arguments"]["path"], "testdata/working_dir");
        assert_eq!(ant["usage"]["input"], 3, "{ant}");
        assert_eq!(ant["usage"]["cache_write"], 1276, "{ant}");
        assert_eq!(ant["usage"]["output"], 58, "{ant}");

        let oai = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(oai["stop_reason"], "stop", "{oai}");
        let oai_content = oai["content"].as_array().unwrap();
        let thinking = oai_content.iter().find(|b| b["type"] == "reasoning").expect("reasoning block");
        assert!(thinking["text"].as_str().unwrap().contains("Providing a simple arithmetic answer"), "{thinking}");
        assert!(!text_of(&oai).is_empty(), "{oai}");

        let gem = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(gem["stop_reason"], "tool_use", "{gem}");
        let gem_call = gem["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_call").unwrap();
        assert_eq!(gem_call["name"], "ask_with_calculator");
        assert_eq!(gem_call["arguments"]["question"], "What is 17 plus 25?");
        assert_eq!(gem["usage"]["input"], 128, "{gem}");
        assert_eq!(gem["usage"]["output"], 87, "{gem}");
        assert_eq!(gem["usage"]["reasoning"], 61, "{gem}");

        h.shutdown().await;
    })
    .await
}
