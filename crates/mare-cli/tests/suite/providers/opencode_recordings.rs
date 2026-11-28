//! Provider protocol replay tests covering HTTP error bodies, Anthropic cache/compat usage,
//! Gemini cachedContentTokenCount, OpenAI Responses tool calls, and OpenAI-compat reasoning/tool edges.

use super::*;

fn find_block<'a>(msg: &'a Value, ty: &str) -> &'a Value {
    msg["content"]
        .as_array()
        .unwrap_or_else(|| panic!("missing content array in {msg}"))
        .iter()
        .find(|b| b["type"] == ty)
        .unwrap_or_else(|| panic!("no {ty} block in {msg}"))
}

/// HTTP 400 error body on OpenAI-compat chat: a vendor numeric-string `code` with no `type`;
/// the upstream message must reach the caller.
/// (The Responses-side 400 body is covered by `provider_recordings::recorded_gateway_provider_responses`.)
#[tokio::test]
async fn opencode_zai_http_400_error_body() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::status(400, provider_fixture("openai-compat/400-disabled-thinking.json"))]);

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let zai = stream(&h, "compat/chat-model").await;
        assert_eq!(zai["stop_reason"], "error", "{zai}");
        assert!(
            zai["error"].as_str().unwrap_or("").contains("always engages in thinking"),
            "expected ZAI error message in {zai}"
        );
        h.shutdown().await;
    })
    .await
}

/// Anthropic & Anthropic-compatible cache/usage/stop fixtures.
#[tokio::test]
async fn opencode_anthropic_cache_and_minimax_compat_usage() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(provider_fixture("anthropic/cache-lookback-tool-turn.sse")),
                Resp::sse(provider_fixture("anthropic/compat-tool-call.sse")),
            ],
        );

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let cache_msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(cache_msg["stop_reason"], "length", "{cache_msg}");
        assert_eq!(cache_msg["usage"]["input"], 5, "{cache_msg}");
        assert_eq!(cache_msg["usage"]["output"], 16, "{cache_msg}");
        assert_eq!(cache_msg["usage"]["cache_write"], 12288, "{cache_msg}");
        assert_eq!(cache_msg["usage"]["cache_read"], 0, "{cache_msg}");

        let mm = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(mm["stop_reason"], "tool_use", "{mm}");
        let tc = find_block(&mm, "tool_call");
        assert_eq!(tc["name"], "get_weather", "{tc}");
        assert_eq!(mm["usage"]["output"], 27, "{mm}");
        assert_eq!(mm["usage"]["input"], 290, "{mm}");
        h.shutdown().await;
    })
    .await
}

/// Google Gemini `cachedContentTokenCount` fixture.
#[tokio::test]
async fn opencode_gemini_cached_content_tokens() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(provider_fixture("google/cached-content-tokens.sse"))]);

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let msg = stream(&h, "google/gemini-2.5-pro").await;
        assert_eq!(msg["stop_reason"], "stop", "{msg}");
        assert_eq!(find_block(&msg, "text")["text"], "Hi.", "{msg}");
        assert_eq!(msg["usage"]["cache_read"], 1100, "{msg}");
        assert_eq!(msg["usage"]["input"], 100, "{msg}");
        assert_eq!(msg["usage"]["output"], 2, "{msg}");
        h.shutdown().await;
    })
    .await
}

/// OpenAI Responses tool call fixture.
#[tokio::test]
async fn opencode_openai_responses_tool_call() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(provider_fixture("openai-responses/tool-call.sse"))]);

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let msg = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(msg["stop_reason"], "tool_use", "{msg}");
        let tc = find_block(&msg, "tool_call");
        assert_eq!(tc["name"], "get_weather", "{tc}");
        assert_eq!(tc["arguments"]["city"], "Paris", "{tc}");
        assert_eq!(msg["usage"]["input"], 74, "{msg}");
        assert_eq!(msg["usage"]["output"], 18, "{msg}");
        h.shutdown().await;
    })
    .await
}

/// OpenAI-compat reasoning + tool-call edge cases:
/// - `reasoning_content` + non-streamed tool args
/// - parallel tool calls + `choices: []` usage chunk
/// - `delta.reasoning` + tool call
#[tokio::test]
async fn opencode_compat_reasoning_and_tool_edges() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                Resp::sse(provider_fixture("openai-compat/nonstream-tool-args.sse")),
                Resp::sse(provider_fixture("openai-compat/parallel-tool-calls-usage-chunk.sse")),
                Resp::sse(provider_fixture("openai-compat/delta-reasoning-tool-call.sse")),
            ],
        );

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let zai = stream(&h, "compat/chat-model").await;
        assert_eq!(zai["stop_reason"], "tool_use", "{zai}");
        assert!(!find_block(&zai, "reasoning")["text"].as_str().unwrap_or("").is_empty(), "{zai}");
        assert_eq!(find_block(&zai, "tool_call")["name"], "get_weather", "{zai}");
        assert_eq!(zai["usage"]["input"], 174, "{zai}");
        assert_eq!(zai["usage"]["output"], 95, "{zai}");

        let groq = stream(&h, "compat/chat-model").await;
        assert_eq!(groq["stop_reason"], "tool_use", "{groq}");
        let tool_calls: Vec<&Value> =
            groq["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(tool_calls.len(), 2, "expected 2 parallel tool calls: {groq}");
        assert_eq!(groq["usage"]["input"], 326, "{groq}");
        assert_eq!(groq["usage"]["output"], 54, "{groq}");

        let cerebras = stream(&h, "compat/chat-model").await;
        assert_eq!(cerebras["stop_reason"], "tool_use", "{cerebras}");
        assert!(!find_block(&cerebras, "reasoning")["text"].as_str().unwrap_or("").is_empty(), "{cerebras}");
        let ctc = find_block(&cerebras, "tool_call");
        assert_eq!(ctc["name"], "get_weather", "{ctc}");
        assert_eq!(ctc["arguments"]["city"], "Paris", "{ctc}");
        assert_eq!(cerebras["usage"]["input"], 137, "{cerebras}");
        h.shutdown().await;
    })
    .await
}

/// Native `thinking` array content chunks over OpenAI-compat chat.
#[tokio::test]
async fn opencode_mistral_native_reasoning_content_array() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::sse(provider_fixture("openai-compat/native-reasoning-content-array.sse"))]);

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let msg = stream(&h, "compat/chat-model").await;
        assert_eq!(msg["stop_reason"], "stop", "{msg}");
        assert!(!find_block(&msg, "reasoning")["text"].as_str().unwrap_or("").is_empty(), "{msg}");
        assert!(find_block(&msg, "text")["text"].as_str().unwrap_or("").contains("91"), "{msg}");
        assert_eq!(msg["usage"]["input"], 35, "{msg}");
        assert_eq!(msg["usage"]["output"], 160, "{msg}");
        h.shutdown().await;
    })
    .await
}

/// Single-chunk tool call over OpenAI-compat chat: the whole tool call (id, name, full arguments),
/// `finish_reason: "tool_calls"` and `usage` arrive in ONE chunk, followed only by `[DONE]`.
#[tokio::test]
async fn docker_agent_mistral_single_chunk_tool_call() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::sse(provider_fixture("openai-compat/single-chunk-tool-call.sse"))]);

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let msg = stream(&h, "compat/chat-model").await;
        assert_eq!(msg["stop_reason"], "tool_use", "{msg}");
        let tc = find_block(&msg, "tool_call");
        assert_eq!(tc["id"], "D9WYdiHxV", "{tc}");
        assert_eq!(tc["name"], "list_directory", "{tc}");
        assert_eq!(tc["arguments"]["path"], "testdata/working_dir", "{tc}");
        assert_eq!(msg["usage"]["input"], 1210, "{msg}");
        assert_eq!(msg["usage"]["output"], 15, "{msg}");
        h.shutdown().await;
    })
    .await
}

/// Gemini-over-OpenAI-compat replay: `extra_content.google.thought_signature` is replayed on
/// the first call (the only one that carried it). Turn-1 parsing is covered by
/// `provider_recordings::recorded_gateway_provider_responses`; this checks the replay.
#[tokio::test]
async fn opencode_gemini_compat_thought_signature_is_replayed() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                Resp::sse(provider_fixture("openai-compat/parallel-tool-signatures.sse")),
                Resp::sse(compat_events(&[compat_text("sunny in Paris"), compat_stop()])),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let _: mlua::Value = eval(
            &h,
            r#"
local m = assert(LLM:resolve("compat/chat-model"))
local tools = { { name = "get_weather", description = "Get weather", parameters = { type = "object", properties = { city = { type = "string" } } } } }
local user = { role = "user", content = { { type = "text", text = "What is the weather in Paris and in Tokyo?" } } }
local m1 = LLM:stream({ model = m, tools = tools, messages = { user } }, function() end)
local history = { user, m1 }
for _, b in ipairs(m1.content) do
  if b.type == "tool_call" then
    history[#history + 1] = { role = "tool_result", tool_call_id = b.id, tool_name = b.name, content = { { type = "text", text = "{}" } } }
  end
end
return LLM:stream({ model = m, tools = tools, messages = history }, function() end)
"#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let turn2: Value = serde_json::from_str(&reqs[1].body).unwrap();
        let assistant = turn2["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        let calls = assistant["tool_calls"].as_array().unwrap();
        assert_eq!(calls.len(), 2, "{assistant}");
        assert_eq!(calls[0]["id"], "call_723181");
        assert!(
            calls[0]["extra_content"]["google"]["thought_signature"].as_str().is_some_and(|s| s.starts_with("ErYCCrMCA")),
            "recorded thought_signature must be replayed on call_723181: {}",
            calls[0]
        );
        assert!(calls[1]["extra_content"].is_null(), "call_723184 had no signature: {}", calls[1]);
        h.shutdown().await;
    })
    .await
}
