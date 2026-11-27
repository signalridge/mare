//! ScriptedModel port: provider error classification and usage cases ported from `vercel/ai@8ba3940`
//! and `anomalyco/opencode@367d24b` (recording replays of the same batch live in
//! `opencode_recordings.rs`).

use super::*;

fn retrying() -> Value {
    json!({ "plugins": { "llm": { "retries": 2, "retry_base_ms": 1, "retry_max_ms": 50 } } })
}

/// Ref: `vercel/ai` `packages/google/src/convert-google-usage.test.ts:5-49`.
/// `usageMetadata.toolUsePromptTokenCount` is prompt input too: input =
/// `promptTokenCount + toolUsePromptTokenCount - cachedContentTokenCount`.
#[tokio::test]
async fn google_usage_counts_tool_use_prompt_tokens_as_input() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"done\"}]},\"finishReason\":\"STOP\"}],",
            "\"usageMetadata\":{\"promptTokenCount\":55,\"toolUsePromptTokenCount\":89,\"cachedContentTokenCount\":100,",
            "\"candidatesTokenCount\":51,\"thoughtsTokenCount\":56,\"totalTokenCount\":251}}\n\n"
        );
        server.route("/google", vec![Resp::sse(body)]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let u = &ask(&h, "google/gemini-2.5-pro").await["usage"];
        assert_eq!(u["input"], 44, "55 + 89 - 100: {u}");
        assert_eq!(u["cache_read"], 100, "{u}");
        assert_eq!(u["output"], 107, "51 + 56: {u}");
        assert_eq!(u["reasoning"], 56, "{u}");
        h.shutdown().await;
    })
    .await
}

/// Ref: `vercel/ai` `packages/openai/src/chat/convert-openai-chat-usage.test.ts:5-24`.
/// `reasoning_tokens` larger than `completion_tokens` (seen in the wild) must not corrupt the
/// other fields: input excludes cached tokens, output stays `completion_tokens`.
#[tokio::test]
async fn compat_usage_survives_reasoning_tokens_above_completion_tokens() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = compat_events(&[
            compat_text("ok"),
            json!({
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {
                    "prompt_tokens": 951,
                    "completion_tokens": 6000,
                    "total_tokens": 6952,
                    "prompt_tokens_details": { "cached_tokens": 60 },
                    "completion_tokens_details": { "reasoning_tokens": 6001 }
                }
            }),
        ]);
        server.route("/compat", vec![Resp::sse(&sse)]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let u = &ask(&h, "deepseek/deepseek-chat").await["usage"];
        assert_eq!(u["input"], 891, "951 - 60: {u}");
        assert_eq!(u["cache_read"], 60, "{u}");
        assert_eq!(u["output"], 6000, "{u}");
        assert_eq!(u["reasoning"], 6001, "{u}");
        h.shutdown().await;
    })
    .await
}

/// Ref: `vercel/ai` `packages/anthropic/src/anthropic-error.test.ts:7-32`.
/// A mid-stream Anthropic `not_found_error` (with an unknown, evolving `details` shape) is final:
/// its message surfaces and it is not retried.
#[tokio::test]
async fn anthropic_stream_not_found_error_is_final() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err_sse = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"not_found_error\",\"message\":\"Thread not found\",\"details\":{\"error_code\":\"thread_not_found\",\"unsupported\":[{\"future\":true}]}}}\n\n";
        server.route("/anthropic", vec![Resp::sse(err_sse)]);
        let h = provider_host(&mut env, &server, retrying()).await;

        let msg = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(msg["stop_reason"], "error", "{msg}");
        assert!(msg["error"].as_str().unwrap_or_default().contains("Thread not found"), "{msg}");
        assert_eq!(server.count("/anthropic"), 1, "not_found_error must not retry: {msg}");
        h.shutdown().await;
    })
    .await
}

/// Ref: `anomalyco/opencode` `packages/ai/test/provider-error.test.ts:119-129`.
/// A specific 4xx status wins over retryable-sounding text: a 400 whose message is the retry
/// phrase "Upstream request failed ..." is `invalid` and is not retried.
#[tokio::test]
async fn a_4xx_status_wins_over_retryable_message_text() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![Resp::status(400, r#"{"error":{"message":"Upstream request failed: validation failed"}}"#)],
        );
        let h = provider_host(&mut env, &server, retrying()).await;

        let msg = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!(msg["error_kind"], "invalid", "{msg}");
        assert_eq!(server.count("/compat"), 1, "a 400 must not retry");
        h.shutdown().await;
    })
    .await
}
