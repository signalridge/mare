use super::*;

/// PROV-E1 / others #2: Context overflow patterns across ~15+ providers and non-overflow
/// exclusions (pi `overflow.test.ts:35-161`, opencode `provider-error.test.ts:7-60`,
/// reference implementation).
#[tokio::test]
async fn context_overflow_patterns_cover_providers_and_exclude_rate_limits() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let overflow_msgs = [
            "Prompt too long",                                                                     // z.ai 1261
            "Prompt exceeds max length",                                                           // z.ai CN 1261
            "Prompt 超长",                                                                         // z.ai CN
            "This model's maximum prompt length is 131072 but the request contains 537812",        // xAI
            "Please reduce the length of the messages or completion",                              // Groq
            "The input (516368 tokens) is longer than the model's context length (262144).",       // Together
            "the request exceeds the available context size, try increasing it",                   // llama.cpp
            "tokens to keep from the initial prompt is greater than the context length",           // LM Studio
            "Your request exceeded model token limit: 128000 (requested: 140000)",                 // Kimi For Coding
            "Prompt contains 50000 tokens, too large for model with 32768 maximum context length", // Mistral
            "Range of input length should be [1, 129024]",                                         // DashScope
            "prompt too long; exceeded max context length by 100918 tokens",                       // Ollama
            "Prompt has 256,468 tokens, but the configured context size is 256,000 tokens",        // DS4
            "Requested input length 600010 exceeds maximum input length 131071",                   // DeepInfra
            "`inputs` tokens + `max_new_tokens` must be <= 131073",                                // HF TGI
            "number of input tokens exceeds maximum length",                                       // Amazon Nova
            "This text request is too large for the model's configured input limit.",              // OpenCode Go
            "exceeded this model context window limit",                                            // Cloudflare
            "输入长度超过模型最大上下文限制",                                                      // Chinese
        ];
        let mut compat_resps: Vec<Resp> =
            overflow_msgs.iter().map(|m| Resp::status(400, json!({"error": {"message": m}}).to_string())).collect();
        compat_resps.push(Resp::status(400, ""));
        server.route("/compat", compat_resps);
        server.route("/cerebras", vec![Resp::status(400, "")]);

        // Exclusions: rate limits and Cohere max_tokens validation must NOT be context_overflow.
        let exclusion_msgs = [
            (429, "Throttling error: Too many tokens, please wait before trying again."),
            (400, "max_tokens must be less than or equal to 4096"),
            (429, "Rate limit exceeded, please retry after 30 seconds."),
            (429, "Token usage per minute exceeds the limit of 10000"),
            (429, "Rate limit exceeded: prompt is too long"),
        ];
        let mut anthropic_resps: Vec<Resp> = Vec::new();
        for (status, m) in &exclusion_msgs {
            anthropic_resps.push(
                Resp::status(*status, json!({"type": "error", "error": {"type": "error", "message": m}}).to_string())
                    .header("retry-after", "0"),
            );
            if *status == 429 {
                anthropic_resps.push(Resp::sse(fixture("anthropic.sse")));
            }
        }
        server.route("/anthropic", anthropic_resps);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "llm": { "retries": 1, "retry_base_ms": 1, "retry_max_ms": 2 },
                    "cerebras": {
                        "base_url": format!("{}/cerebras", server.base),
                        "dialect": "cerebras",
                        "api_key": "test-key",
                        "models": [{ "model_id": "llama-3.3-70b" }]
                    }
                }
            }),
        )
        .await;

        for (idx, expected_msg) in overflow_msgs.iter().enumerate() {
            let msg = stream(&h, "compat/chat-model").await;
            assert_eq!(msg["error_kind"], "context_overflow", "case {idx} ({expected_msg}): {msg}");
        }
        assert_eq!(server.count("/compat"), overflow_msgs.len(), "overflows must not be retried");

        // 1) Bedrock throttling "Too many tokens" -> retried as rate_limit, succeeds on 2nd try
        let m1 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_ne!(m1["stop_reason"], "error", "Bedrock throttling should retry and succeed: {m1}");
        // 2) Cohere max_tokens -> invalid, NOT context_overflow
        let m2 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(m2["error_kind"], "invalid", "Cohere max_tokens must not be context_overflow: {m2}");
        // 3) Generic rate limit -> retried as rate_limit, succeeds
        let m3 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_ne!(m3["stop_reason"], "error", "rate limit should retry and succeed: {m3}");
        // 4) Minute rate limit with "exceeds the limit of N" -> retried as rate_limit, succeeds
        let m4 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_ne!(m4["stop_reason"], "error", "minute rate limit must retry and succeed: {m4}");
        // 5) Rate limit whose message also contains provider overflow wording -> exclusion still wins
        let m5 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_ne!(m5["stop_reason"], "error", "rate-limit exclusion must override provider overflow: {m5}");

        // Cerebras bodyless 400 through the real compat provider route -> context_overflow (pi overflow.test.ts:133-139)
        let cerebras_msg = stream(&h, "cerebras/llama-3.3-70b").await;
        assert_eq!(
            cerebras_msg["error_kind"], "context_overflow",
            "Cerebras bodyless 400 must be context_overflow: {cerebras_msg}"
        );
        let deepseek_msg = stream(&h, "compat/chat-model").await;
        assert_eq!(
            deepseek_msg["error_kind"], "invalid",
            "Non-Cerebras bodyless 400 must not be context_overflow: {deepseek_msg}"
        );

        h.shutdown().await;
    })
    .await
}

/// PROV-E1 / others #2: Silent overflow (z.ai stop with input+cache_read > context_window;
/// Xiaomi MiMo length stop with output==0 and input >= 99% context_window) triggers compaction
/// (pi `overflow.ts:159-175`, `overflow.test.ts:192-234`).
#[tokio::test]
async fn silent_overflow_is_detected_by_agent_loop_and_triggers_compaction() {
    local(async {
        for (case_name, finish_reason, input_tokens, output_tokens) in [
            ("zai_silent_stop", "stop", 130_000, 10),
            ("mimo_filled_length", "length", 127_500, 0),
        ] {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let first = compat_events(&[
                json!({"choices":[{"index":0,"delta":{"content": if finish_reason == "stop" { "partial" } else { "" }},"finish_reason":finish_reason}],"usage":{"prompt_tokens":input_tokens,"completion_tokens":output_tokens,"total_tokens":input_tokens + output_tokens}}),
            ]);
            let second = compat_events(&[
                json!({"choices":[{"index":0,"delta":{"content":"recovered"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105}}),
            ]);
            server.route("/compat", vec![Resp::sse(first), Resp::sse(second)]);
            let h = provider_host(&mut env, &server, json!({
                "model": "compat/chat-model",
                "models": { "compat/chat-model": { "context_window": 128000 } }
            })).await;
            expose(&h);
            exec(&h, r#"COMPACTED = 0
                C.cordis:on("compact/overflow", function() COMPACTED = COMPACTED + 1 return { handled = true, succeeded = true, res = { status = "compacted", tokens_before = 0, tokens_after = 0 } } end, { prepend = true })"#).await;
            let result = h.prompt("go", json!({})).await.unwrap();
            let compacted: i64 = eval(&h, "return COMPACTED").await;
            assert_eq!(compacted, 1, "{case_name}: silent overflow must trigger compaction: {result}");
            assert_eq!(server.count("/compat"), 2, "{case_name}: should retry after compaction");
            h.shutdown().await;
        }
    })
    .await
}

/// PROV-E1: When 400 says the prompt fits in context_window and only prompt + max_tokens
/// exceeds the window, retry once with max_tokens = remaining window instead of compacting
#[tokio::test]
async fn max_tokens_exceeding_remaining_window_retries_with_remaining_room() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let too_high_cap = json!({
            "error": {
                "message": "This model's maximum context length is 1000000 tokens. However, you requested 1100000 tokens (706784 in the messages, 393216 in the completion). Please reduce the length of the messages or completion.",
                "type": "invalid_request_error",
                "code": "invalid_request_error"
            }
        });
        let small_valid_room = json!({
            "error": {
                "message": "This model's maximum context length is 1000000 tokens. However, you requested 1380000 tokens (996000 in the messages, 384000 in the completion).",
                "type": "invalid_request_error"
            }
        });
        let too_little_room = json!({
            "error": {
                "message": "This model's maximum context length is 1000000 tokens. However, you requested 1380000 tokens (999500 in the messages, 380500 in the completion).",
                "type": "invalid_request_error"
            }
        });
        server.route(
            "/compat",
            vec![
                Resp::status(400, too_high_cap.to_string()),
                Resp::sse(fixture("openai-compat.sse")),
                Resp::status(400, small_valid_room.to_string()),
                Resp::sse(fixture("openai-compat.sse")),
                Resp::status(400, too_little_room.to_string()),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;

        let msg = stream(&h, "compat/reasoner-model").await;
        assert_ne!(msg["stop_reason"], "error", "should retry with remaining window and succeed: {msg}");
        assert_eq!(server.count("/compat"), 2, "must retry once with capped max_tokens");
        let reqs = server.requests();
        let second_body: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert_eq!(second_body["max_tokens"], 293216, "second request must cap max_tokens to 1000000 - 706784");

        // Room < 8192 but valid (4000 left) must ALSO retry with capped max_tokens = 4000
        let msg_4k = stream(&h, "compat/reasoner-model").await;
        assert_ne!(msg_4k["stop_reason"], "error", "4000 remaining room must retry and succeed: {msg_4k}");
        assert_eq!(server.count("/compat"), 4, "4000 remaining room must retry once");
        let reqs = server.requests();
        let fourth_body: Value = serde_json::from_str(&reqs[3].body).unwrap();
        assert_eq!(fourth_body["max_tokens"], 4000, "fourth request must cap max_tokens to 4000");

        // Genuinely tiny room (500 left) must NOT retry and must surface as context_overflow
        let overflow = stream(&h, "compat/reasoner-model").await;
        assert_eq!(overflow["error_kind"], "context_overflow", "{overflow}");
        assert_eq!(server.count("/compat"), 5, "tiny room (500) must not retry");
        h.shutdown().await;
    })
    .await
}

/// PROV-E1: Chinese quota/rate-limit wording and per-minute 429 vs quota
#[tokio::test]
async fn chinese_error_wording_and_per_minute_429_classification() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                // 1) DashScope 429 TPM rate limit with "Allocated quota exceeded" -> rate_limit (retryable)
                Resp::status(
                    429,
                    r#"{"error":{"message":"Allocated quota exceeded, please increase your quota limit."}}"#,
                )
                .header("retry-after", "0"),
                Resp::sse(fixture("openai-compat.sse")),
                // 2) Chinese rate limit -> rate_limit (retryable)
                Resp::status(429, r#"{"error":{"message":"请求过于频繁，超出TPM限制，请稍后重试"}}"#)
                    .header("retry-after", "0"),
                Resp::sse(fixture("openai-compat.sse")),
                // 3) Chinese balance exhausted -> quota (non-retryable)
                Resp::status(429, r#"{"error":{"message":"账户余额不足，请充值后重试"}}"#),
                // 4) 429 with longer window ("per minute" + "in 2 hours") -> quota (non-retryable)
                Resp::status(429, r#"{"error":{"message":"rate limit per minute exceeded, quota resets in 2 hours"}}"#),
                // 5) 429 with OpenAI quota-exhaustion wording (plan and billing details) -> quota
                Resp::status(429, r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details."}}"#),
                // 6) 429 with type="insufficient_quota" and NO code (compat gateway) -> quota (non-retryable)
                Resp::status(429, r#"{"error":{"message":"Quota exceeded for this organization","type":"insufficient_quota"}}"#),
                // 7) 429 minute rate limit that also mentions "check billing" -> rate_limit (retryable)
                Resp::status(429, r#"{"error":{"message":"Rate limit per minute exceeded, please check billing settings and retry"}}"#)
                    .header("retry-after", "0"),
                Resp::sse(fixture("openai-compat.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": { "llm": { "retries": 1, "retry_base_ms": 1, "retry_max_ms": 2 } }
            }),
        )
        .await;

        let m1 = stream(&h, "compat/chat-model").await;
        assert_ne!(m1["stop_reason"], "error", "DashScope 429 Allocated quota exceeded must retry: {m1}");

        let m2 = stream(&h, "compat/chat-model").await;
        assert_ne!(m2["stop_reason"], "error", "Chinese TPM rate limit must retry: {m2}");

        let m3 = stream(&h, "compat/chat-model").await;
        assert_eq!(m3["error_kind"], "quota", "Chinese 余额不足 must be non-retryable quota: {m3}");

        let m4 = stream(&h, "compat/chat-model").await;
        assert_eq!(m4["error_kind"], "quota", "429 with longer window (hours) must be quota: {m4}");

        let m5 = stream(&h, "compat/chat-model").await;
        assert_eq!(m5["error_kind"], "quota", "429 exceeded your current quota must be quota: {m5}");

        let m6 = stream(&h, "compat/chat-model").await;
        assert_eq!(m6["error_kind"], "quota", "429 with type=insufficient_quota and no code must be quota: {m6}");

        let m7 = stream(&h, "compat/chat-model").await;
        assert_ne!(m7["stop_reason"], "error", "429 minute rate with check billing must still retry: {m7}");

        assert_eq!(server.count("/compat"), 10);
        h.shutdown().await;
    })
    .await
}

/// PROV-E1 / others #11: HTTP 413 payload_too_large triggers offload_images downgrade in agent loop
/// and retries when images are present; fails immediately when no images exist
/// (Kimi `media-degrade.ts:1-80`).
#[tokio::test]
async fn payload_too_large_413_offloads_images_and_retries() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ok_sse = compat_events(&[json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
        server.route(
            "/compat",
            vec![
                // 1) request_too_large byte limit 413 -> payload_too_large -> offload_images (Kimi errors.test.ts:433)
                Resp::status(
                    413,
                    r#"{"error":{"message":"request_too_large: Request exceeds the maximum allowed number of bytes"}}"#,
                ),
                Resp::sse(ok_sse.clone()),
                // 2) 413 with explicit token-overflow wording -> context_overflow -> compact (Kimi errors.test.ts:447)
                Resp::status(413, r#"{"error":{"message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#),
                Resp::sse(ok_sse),
                // 3) 413 with 0 images to offload -> error stands
                Resp::status(413, r#"{"error":{"message":"Request Entity Too Large"}}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({"model": "compat/chat-model"})).await;
        expose(&h);
        exec(
            &h,
            r#"OFFLOAD_CALLS = 0
            COMPACT_CALLS = 0
            K = C.cordis:get("compact")
            K.offload_images = function(_, opts)
                OFFLOAD_CALLS = OFFLOAD_CALLS + 1
                if OFFLOAD_CALLS == 1 then return 2 end
                return 0
            end
            C.cordis:on("compact/overflow", function()
                COMPACT_CALLS = COMPACT_CALLS + 1
                return { handled = true, succeeded = true, res = { status = "compacted", tokens_before = 0, tokens_after = 0 } }
            end, { prepend = true })"#,
        )
        .await;

        let ok = h.prompt("with images", json!({})).await.unwrap();
        assert_eq!(eval::<i64>(&h, "return OFFLOAD_CALLS").await, 1);
        assert_eq!(eval::<i64>(&h, "return COMPACT_CALLS").await, 0);
        assert_eq!(server.count("/compat"), 2, "413 request_too_large with offloaded images must retry: {ok}");

        let ok_token = h.prompt("token overflow 413", json!({})).await.unwrap();
        assert_eq!(
            eval::<i64>(&h, "return COMPACT_CALLS").await,
            1,
            "token-overflow 413 must trigger compaction: {ok_token}"
        );
        assert_eq!(eval::<i64>(&h, "return OFFLOAD_CALLS").await, 1, "token-overflow 413 must not call offload_images");
        assert_eq!(server.count("/compat"), 4, "token-overflow 413 must retry after compaction");

        // Third turn: no images left to offload (offload_images returns 0) -> error stands
        let err = h.prompt("no images", json!({})).await.unwrap();
        assert!(err.to_string().contains("413") || err.to_string().contains("too large"), "{err}");
        assert_eq!(server.count("/compat"), 5, "413 with 0 offloaded images must not retry");
        h.shutdown().await;
    })
    .await
}

/// PROV-E1: Empty response with stop_reason == "length" (thinking exhausted
/// max_tokens) must NOT be retried with the same parameters.
#[tokio::test]
async fn thinking_exhaustion_empty_length_response_does_not_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let empty_length = compat_events(&[
            json!({"choices":[{"index":0,"delta":{"reasoning_content":"thinking forever..."},"finish_reason":"length"}]}),
        ]);
        server.route("/compat", vec![Resp::sse(empty_length)]);
        let h = provider_host(&mut env, &server, json!({
            "providers": { "llm": { "retries": 2, "retry_base_ms": 1, "retry_max_ms": 2 } }
        })).await;

        let msg = stream(&h, "compat/reasoner-model").await;
        assert_eq!(msg["stop_reason"], "error", "{msg}");
        assert_ne!(msg["error_kind"], "server", "thinking exhaustion must not be classified as retryable server: {msg}");
        assert_eq!(server.count("/compat"), 1, "empty length response must not retry with same max_tokens");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn llm_errors_classify_stream_stalled_slow_down_and_402_billing_vs_in_flight() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ok_sse = compat_events(&[json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
        server.route(
            "/compat",
            vec![
                // 1) HTTP 402 OpenRouter in-flight credit reservation -> retryable rate_limit, succeeds on retry
                Resp::status(
                    402,
                    r#"{"error":{"message":"This request would exceed your available credits given your current in-flight requests. Retry after in-flight requests settle."}}"#,
                ),
                Resp::sse(ok_sse.clone()),
                // 2) HTTP 402 billing error -> non-retryable quota, triggers fallback_models to openai
                Resp::status(
                    402,
                    r#"{"error":{"type":"billing_error","code":"billing_not_configured","message":"Billing verification failed"}}"#,
                ),
                // 3) HTTP 402 requires more credits -> non-retryable quota
                Resp::status(
                    402,
                    r#"{"error":{"message":"This request requires more credits, or fewer max_tokens."}}"#,
                ),
                // 4) HTTP 402 subscription-only message (not billing exhaustion) -> invalid (no fallback)
                Resp::status(
                    402,
                    r#"{"error":{"message":"Pro subscription required to use this model."}}"#,
                ),
            ],
        );
        server.route("/openai", vec![Resp::sse(responses_text("fallback-ok"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "fallback_models": ["openai/gpt-6.1-terra"],
                "plugins": {
                    "llm": {
                        "retries": 1,
                        "retry_base_ms": 1,
                        "retry_max_ms": 2
                    }
                }
            }),
        )
        .await;

        // Unit-level classification checks via llm.errors
        let unit_ok: bool = eval(
            &h,
            r#"
            local E = require("llm.errors")
            for _, msg in ipairs({
                "stream stall while waiting for event",
                "Anthropic stream stalled while waiting for the next event",
                "Provider stream stalls while waiting",
                "Provider stream stalling mid-turn",
            }) do
                local k, r = E.classify({ message = msg })
                assert(k == "transport" and r == true, msg .. " -> " .. tostring(k) .. "," .. tostring(r))
            end
            for _, msg in ipairs({ "stream stallone", "upstream stalled" }) do
                local _, r = E.classify({ message = msg })
                assert(r == false, "must not match non-boundary wording: " .. msg)
            end
            local _, r400 = E.classify({ status = 400, message = "Anthropic stream stalled while waiting" })
            assert(r400 == false, "HTTP 400 must not become retryable")

            local kr, rr = E.classify({ message = "request rate increased too quickly" })
            assert(kr == "rate_limit" and rr == true, "rate increased too quickly -> " .. tostring(kr))
            local ks, rs = E.classify({ message = "slow_down: please back off" })
            assert(ks == "overloaded" and rs == true, "slow_down -> " .. tostring(ks))

            local k_empty402, r_empty402 = E.classify({ status = 402, message = "" })
            assert(k_empty402 == "quota" and r_empty402 == false, "empty 402 -> " .. tostring(k_empty402))
            return true
            "#,
        )
        .await;
        assert!(unit_ok);

        // 1) 402 in-flight credit reservation retries and succeeds on primary model
        let m1 = stream(&h, "compat/chat-model").await;
        assert_ne!(m1["stop_reason"], "error", "402 in-flight reservation must retry: {m1}");
        assert_eq!(m1["provider_id"], "compat", "should succeed on retry without fallback: {m1}");

        // 2) 402 billing_error classifies as quota and triggers fallback_models
        let m2 = stream(&h, "compat/chat-model").await;
        assert_ne!(m2["stop_reason"], "error", "402 billing_error must fall back: {m2}");
        assert_eq!(m2["provider_id"], "openai", "expected fallback to openai: {m2}");

        // 3) 402 requires more credits -> quota (disable fallback to observe exact error_kind)
        exec(&h, r#"mare.config.fallback_models = {}"#).await;
        let m3 = stream(&h, "compat/chat-model").await;
        assert_eq!(m3["stop_reason"], "error", "{m3}");
        assert_eq!(m3["error_kind"], "quota", "402 requires more credits must be quota: {m3}");

        // 4) 402 subscription-only -> invalid (not quota)
        let m4 = stream(&h, "compat/chat-model").await;
        assert_eq!(m4["stop_reason"], "error", "{m4}");
        assert_eq!(m4["error_kind"], "invalid", "402 subscription-only must stay invalid: {m4}");

        assert_eq!(server.count("/compat"), 5);
        h.shutdown().await;
    })
    .await
}

/// Issue #90: HTTP 400 effort refusals preserve the vendor's `"Supported values are …"` wording
/// and append a `/effort` guidance hint without mutating model capabilities (no automatic
/// learning), while non-effort parameter refusals (`text.verbosity`, unrelated values) are left
/// untouched.
#[tokio::test]
async fn effort_400_refusal_surfaces_supported_values_without_mutating_model() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                // 1) OpenAI-style 400 with param=reasoning.effort and inline "Supported values are"
                Resp::status(
                    400,
                    json!({
                        "error": {
                            "message": "Unsupported value: 'high' is not supported with the 'gpt-6-luna' model. Supported values are: 'none', 'low', and 'medium'.",
                            "type": "invalid_request_error",
                            "param": "reasoning.effort",
                            "code": "unsupported_value"
                        }
                    })
                    .to_string(),
                ),
                // 2) Vendor puts "Supported values are" only in another JSON field (raw body), not error.message
                Resp::status(
                    400,
                    json!({
                        "error": {
                            "message": "Invalid reasoning_effort value",
                            "param": "reasoning_effort"
                        },
                        "detail": "Supported values are: 'low', 'medium'."
                    })
                    .to_string(),
                ),
                // 3) Plan restriction on reasoning.effort without "Supported values are"
                Resp::status(
                    400,
                    json!({
                        "error": {
                            "message": "reasoning.effort 'xhigh' is not available on your plan",
                            "param": "reasoning.effort"
                        }
                    })
                    .to_string(),
                ),
                // 4) Non-effort param (text.verbosity) with identical "Unsupported value" wording -> NOT effort refusal
                Resp::status(
                    400,
                    json!({
                        "error": {
                            "message": "Unsupported value: 'high' is not supported. Supported values are: 'low', 'medium'.",
                            "param": "text.verbosity"
                        }
                    })
                    .to_string(),
                ),
                // 5) Missing param with non-effort value -> NOT effort refusal
                Resp::status(
                    400,
                    json!({
                        "error": {
                            "message": "Unsupported value: 'xml' is not supported with this model."
                        }
                    })
                    .to_string(),
                ),
                // 6) Missing param with effort-like word ('high') targeting verbosity -> NOT effort refusal
                Resp::status(
                    400,
                    json!({
                        "error": {
                            "message": "Unsupported value: 'high' is not supported for verbosity. Supported values are: 'low', 'medium'."
                        }
                    })
                    .to_string(),
                ),
                // 7) Missing param but error text explicitly names reasoning_effort -> IS effort refusal
                Resp::status(
                    400,
                    json!({
                        "error": {
                            "message": "Invalid reasoning_effort: unsupported value 'high' is not supported. Supported values are: 'low', 'medium'."
                        }
                    })
                    .to_string(),
                ),
            ],
        );

        let h = provider_host(&mut env, &server, json!({})).await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let efforts_before: String =
            eval(&h, r#"return mare.json.encode(LLM:resolve("compat/reasoner-model").efforts)"#).await;

        let m1 = stream(&h, "compat/reasoner-model").await;
        assert_eq!(m1["stop_reason"], "error", "{m1}");
        assert_eq!(m1["error_kind"], "invalid", "{m1}");
        let e1 = m1["error"].as_str().unwrap_or("");
        assert!(e1.contains("Supported values are: 'none', 'low', and 'medium'"), "must keep supported values: {e1}");
        assert!(e1.contains("/effort"), "must append /effort hint: {e1}");

        // Must NOT mutate model efforts (no automatic learning).
        let efforts_after: String =
            eval(&h, r#"return mare.json.encode(LLM:resolve("compat/reasoner-model").efforts)"#).await;
        assert_eq!(efforts_after, efforts_before, "400 effort refusal must not mutate model efforts");

        let m2 = stream(&h, "compat/reasoner-model").await;
        assert_eq!(m2["error_kind"], "invalid", "{m2}");
        let e2 = m2["error"].as_str().unwrap_or("");
        assert!(e2.contains("Supported values are: 'low', 'medium'"), "must surface supported values from body: {e2}");
        assert!(e2.contains("/effort"), "must append /effort hint: {e2}");

        let m3 = stream(&h, "compat/reasoner-model").await;
        assert_eq!(m3["error_kind"], "invalid", "{m3}");
        let e3 = m3["error"].as_str().unwrap_or("");
        assert!(e3.contains("/effort"), "must append /effort hint on plan refusal: {e3}");

        let m4 = stream(&h, "compat/reasoner-model").await;
        assert_eq!(m4["error_kind"], "invalid", "{m4}");
        let e4 = m4["error"].as_str().unwrap_or("");
        assert!(!e4.contains("/effort"), "text.verbosity refusal must not get /effort hint: {e4}");

        let m5 = stream(&h, "compat/reasoner-model").await;
        assert_eq!(m5["error_kind"], "invalid", "{m5}");
        let e5 = m5["error"].as_str().unwrap_or("");
        assert!(!e5.contains("/effort"), "non-effort value refusal must not get /effort hint: {e5}");

        let m6 = stream(&h, "compat/reasoner-model").await;
        assert_eq!(m6["error_kind"], "invalid", "{m6}");
        let e6 = m6["error"].as_str().unwrap_or("");
        assert!(
            !e6.contains("/effort"),
            "missing-param verbosity refusal with 'high' must not get /effort hint: {e6}"
        );

        let m7 = stream(&h, "compat/reasoner-model").await;
        assert_eq!(m7["error_kind"], "invalid", "{m7}");
        let e7 = m7["error"].as_str().unwrap_or("");
        assert!(e7.contains("Supported values are: 'low', 'medium'"), "must keep supported values: {e7}");
        assert!(e7.contains("/effort"), "missing-param error naming reasoning_effort must get /effort hint: {e7}");

        assert_eq!(server.count("/compat"), 7, "400 effort refusals must not retry");
        h.shutdown().await;
    })
    .await
}
