use super::*;

/// PROV-E1 / others #15: HTTP errors extract and surface server request-id / trace-id headers
/// (Kimi `errors.test.ts:200-297`, `openai-quota-error.ts`).
#[tokio::test]
async fn http_errors_preserve_server_request_id() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                Resp::status(400, r#"{"error":{"message":"bad param"}}"#).header("x-request-id", "req-abc-123"),
                Resp::status(400, r#"{"error":{"message":"bad param"}}"#).header("x-trace-id", "trace-xyz-789"),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;

        let m1 = stream(&h, "deepseek/deepseek-chat").await;
        assert!(m1["error"].as_str().unwrap().contains("req-abc-123"), "x-request-id must appear in error: {m1}");

        let m2 = stream(&h, "deepseek/deepseek-chat").await;
        assert!(m2["error"].as_str().unwrap().contains("trace-xyz-789"), "x-trace-id must appear in error: {m2}");

        let rid: Option<String> = eval(
            &h,
            r#"local e = LLM:http_error("Anthropic", { status = 400, headers = { ["request-id"] = "req-anth-42" }, text = function() return '{"error":{"message":"bad"}}' end })
               return e.request_id"#,
        ).await;
        assert_eq!(rid.as_deref(), Some("req-anth-42"));
        h.shutdown().await;
    })
    .await
}

/// PROV-E1 review #1: Minute rate limit 429 ("Token usage per minute exceeds the limit of 10000")
/// must retry as rate_limit and succeed WITHOUT triggering agent compaction.
#[tokio::test]
async fn minute_rate_limit_429_retries_without_compaction() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ok_sse = compat_events(&[json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]})]);
        server.route(
            "/compat",
            vec![
                Resp::status(429, r#"{"error":{"message":"Token usage per minute exceeds the limit of 10000"}}"#)
                    .header("retry-after", "0"),
                Resp::sse(ok_sse),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "deepseek/deepseek-chat",
                "providers": { "llm": { "retries": 1, "retry_base_ms": 1, "retry_max_ms": 2 } }
            }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"COMPACTED = 0
            C.cordis:on("compact/overflow", function() COMPACTED = COMPACTED + 1 return { handled = true, succeeded = true, res = { status = "compacted", tokens_before = 0, tokens_after = 0 } } end, { prepend = true })"#,
        )
        .await;

        let result = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(
            eval::<i64>(&h, "return COMPACTED").await,
            0,
            "429 minute rate limit must not trigger compaction: {result}"
        );
        assert_eq!(server.count("/compat"), 2, "429 minute rate limit must retry and succeed: {result}");
        h.shutdown().await;
    })
    .await
}

/// PROV-E1 review #4: OpenAI Responses route (HTTP and in-stream error paths) wires
/// parse_reply_left and retries with max_output_tokens = remaining room.
#[tokio::test]
async fn responses_route_retries_with_remaining_reply_room() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err_msg = "This model's maximum context length is 100000 tokens. However, you requested 120000 tokens (80000 in the messages, 40000 in the completion).";
        let stream_err = format!(
            "event: response.failed\ndata: {}\n\n",
            json!({
                "type": "response.failed",
                "response": { "error": { "message": err_msg, "code": "context_length_exceeded" } }
            })
        );
        server.route(
            "/openai",
            vec![
                Resp::status(400, json!({"error": {"message": err_msg, "code": "invalid_request_error"}}).to_string()),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(stream_err),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;

        let m_http = stream(&h, "openai/gpt-6.1-terra").await;
        assert_ne!(m_http["stop_reason"], "error", "Responses HTTP 400 with reply_left must retry: {m_http}");
        let reqs = server.requests();
        let body2: Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert_eq!(body2["max_output_tokens"], 20000, "second Responses request must set max_output_tokens = 20000");

        let m_stream = stream(&h, "openai/gpt-6.1-terra").await;
        assert_ne!(m_stream["stop_reason"], "error", "Responses in-stream error with reply_left must retry: {m_stream}");
        let reqs = server.requests();
        let body4: Value = serde_json::from_str(&reqs[3].body).unwrap();
        assert_eq!(body4["max_output_tokens"], 20000, "fourth Responses request must set max_output_tokens = 20000");
        assert_eq!(server.count("/openai"), 4);
        h.shutdown().await;
    })
    .await
}

/// PROV-E1 review #6: Budget-adjusted reply_left retry must run on_attempt (stopping on
/// persistence failure) and emit llm/retry with reset = true when partial output was streamed.
#[tokio::test]
async fn budget_adjusted_retry_runs_on_attempt_and_emits_retry_reset() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err_msg = "This model's maximum context length is 100000 tokens. However, you requested 120000 tokens (80000 in the messages, 40000 in the completion).";
        let partial_then_err = format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"partial "}}]}),
            json!({"error":{"message": err_msg, "code": "invalid_request_error"}})
        );
        server.route(
            "/compat",
            vec![
                // Case 1: on_attempt fails -> no second call
                Resp::status(400, json!({"error": {"message": err_msg}}).to_string()),
                // Case 2: partial output then error -> on_attempt succeeds, llm/retry emitted with reset=true
                Resp::sse(partial_then_err),
                Resp::sse(fixture("openai-compat.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);

        // Case 1: on_attempt returns false -> must fail with kind="persist" and NOT make a 2nd request
        let m_persist: Value = mare_shared::lua::lua_to_json(
            h.lua(),
            h.lua()
                .load(
                    r#"local m = LLM:resolve("deepseek/deepseek-reasoner")
                    ON_ATTEMPT_CALLS = 0
                    return LLM:stream({
                        model = m,
                        messages = {{ role = "user", content = {{ { type = "text", text = "hi" } }} }},
                        on_attempt = function() ON_ATTEMPT_CALLS = ON_ATTEMPT_CALLS + 1 return false end,
                    })"#,
                )
                .call_async::<mlua::Value>(())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(m_persist["error_kind"], "persist", "on_attempt failure must surface as persist: {m_persist}");
        assert_eq!(eval::<i64>(&h, "return ON_ATTEMPT_CALLS").await, 1);
        assert_eq!(server.count("/compat"), 1, "on_attempt failure must prevent second provider call");

        // Case 2: partial output before error -> llm/retry emitted with reset = true
        let m_reset: Value = mare_shared::lua::lua_to_json(
            h.lua(),
            h.lua()
                .load(
                    r#"local m = LLM:resolve("deepseek/deepseek-reasoner")
                    RETRY_EVENTS = {}
                    ON_ATTEMPT_CALLS = 0
                    C.cordis:on("step/completed", function(ev) if ev.will_retry then table.insert(RETRY_EVENTS, ev) end end)
                    return LLM:stream({
                        model = m,
                        turn_id = "turn-1",
                        step_id = "step-1",
                        messages = {{ role = "user", content = {{ { type = "text", text = "hi" } }} }},
                        on_attempt = function() ON_ATTEMPT_CALLS = ON_ATTEMPT_CALLS + 1 return true end,
                    })"#,
                )
                .call_async::<mlua::Value>(())
                .await
                .unwrap(),
        )
        .unwrap();
        assert_ne!(m_reset["stop_reason"], "error", "partial+error with reply_left must retry and succeed: {m_reset}");
        assert_eq!(eval::<i64>(&h, "return ON_ATTEMPT_CALLS").await, 1);
        assert_eq!(eval::<i64>(&h, "return #RETRY_EVENTS").await, 1);
        assert!(eval::<bool>(&h, "return RETRY_EVENTS[1].reset == true").await);
        assert_eq!(server.count("/compat"), 3);
        h.shutdown().await;
    })
    .await
}

/// b47 review #1: reply_left retry must respect the `retries` budget (retries = 0 sends no 2nd request).
#[tokio::test]
async fn reply_left_retry_respects_retries_zero() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err_msg = "This model's maximum context length is 100000 tokens. However, you requested 120000 tokens (80000 in the messages, 40000 in the completion).";
        server.route(
            "/openai",
            vec![
                Resp::status(400, json!({"error": {"message": err_msg, "code": "invalid_request_error"}}).to_string()),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;
        let m = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(m["stop_reason"], "error", "retries = 0 must not retry on reply_left: {m}");
        assert_eq!(m["error_kind"], "context_overflow", "must fall through to context_overflow: {m}");
        assert_eq!(server.count("/openai"), 1, "retries = 0 must make exactly 1 request");
        h.shutdown().await;
    })
    .await
}

/// b47 review #2: Responses route with drop_fields containing max_output_tokens skips reply_left retry.
#[tokio::test]
async fn responses_drop_fields_skips_reply_left_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        let err_msg = "This model's maximum context length is 100000 tokens. However, you requested 120000 tokens (80000 in the messages, 40000 in the completion).";
        server.route(
            "/openai",
            vec![
                Resp::status(400, json!({"error": {"message": err_msg, "code": "invalid_request_error"}}).to_string()),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-opencode-go": {
                        "base_url": format!("{}/openai", server.base),
                        "routes": {
                            "responses": {
                                "drop_fields": ["max_output_tokens"]
                            }
                        }
                    }
                }
            }),
        )
        .await;
        let m = stream(&h, "opencode-go/glm-5.3-flash").await;
        assert_eq!(m["stop_reason"], "error", "drop_fields=max_output_tokens must not retry on reply_left: {m}");
        assert_eq!(m["error_kind"], "context_overflow", "must fall through to context_overflow: {m}");
        assert_eq!(server.count("/openai"), 1, "must make exactly 1 request when max_output_tokens is dropped");
        h.shutdown().await;
    })
    .await
}

/// Anthropic `stop_reason: "sensitive"` with empty content must map to `stop_reason: "filtered"`
/// (never retried as a server error) and preserve `raw_stop_reason: "sensitive"`.
#[tokio::test]
async fn anthropic_sensitive_stop_reason_is_non_retryable_filtered_and_preserves_raw() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sensitive_sse = provider_fixture("anthropic/sensitive-stop.sse");
        server.route("/anthropic", vec![Resp::sse(sensitive_sse.clone()), Resp::sse(sensitive_sse)]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 2 } } })).await;

        let m = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_ne!(m["error_kind"], "server", "must not map sensitive stop to retryable server: {m}");
        assert_eq!(m["stop_reason"], "filtered", "{m}");
        assert_eq!(m["raw_stop_reason"], "sensitive", "{m}");
        assert!(m["error"].as_str().is_some_and(|e| e.contains("sensitive")), "{m}");
        assert_eq!(server.count("/anthropic"), 1, "sensitive stop_reason must not be retried: {m}");

        h.shutdown().await;
    })
    .await
}

/// Real session persistence + index scan for an Anthropic `stop_reason: "sensitive"` empty-content
/// refusal (Lua encodes `{}` as `"content":{}`): must neither fail `session.invalid_record` nor
/// drop the message/tokens from `index::scan`.
#[tokio::test]
async fn anthropic_sensitive_refusal_persists_and_scans_in_session() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sensitive_sse = "\
event: message_start\n\
data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_sensitive\",\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}}\n\n\
event: message_delta\n\
data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"sensitive\"},\"usage\":{\"input_tokens\":12,\"output_tokens\":0}}\n\n\
event: message_stop\n\
data: {\"type\":\"message_stop\"}\n\n";
        server.route("/anthropic", vec![Resp::sse(sensitive_sse)]);
        env.set_var("ANTHROPIC_API_KEY", "test-anthropic");
        let h = env
            .host()
            .config(json!({
                "model": "anthropic/claude-sonnet-5-5",
                "plugins": {
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) },
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .build()
            .await;

        let res = h.prompt("hello", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["messages"][1]["stop_reason"], "filtered", "{res}");
        expose(&h);
        let (path, persist_err, indexed): (String, Option<String>, String) = eval(
            &h,
            r#"
            local rows = mare.store.index_read({ scope = 'all', kind = 'any', archived = 'any' })
            return S:info().path, A:persist_error(), mare.json.encode(rows)
            "#,
        )
        .await;
        assert!(persist_err.is_none(), "empty content refusal must not fail persistence: {persist_err:?}");
        let entries = mare_store::read_entries(std::path::Path::new(&path)).unwrap();
        let assistant = entries
            .iter()
            .find(|e| e["type"] == "message" && e["message"]["role"] == "assistant")
            .expect("assistant message must be persisted");
        assert_eq!(assistant["message"]["stop_reason"], "filtered");
        assert_eq!(assistant["message"]["raw_stop_reason"], "sensitive");
        let (rows, total) = mare_store::index::scan_list(
            std::path::Path::new(&path).parent().unwrap().parent().unwrap(),
            &mare_store::index::Query { kinds: mare_store::index::Kinds::Any, ..Default::default() },
        )
        .unwrap();
        assert_eq!(total, 1, "{indexed}");
        let row = &rows[0];
        assert_eq!(row.message_count, 2, "user + filtered assistant must both be scanned: {indexed}");
        assert_eq!(row.tokens_in, 12);
        assert_eq!(row.tokens_out, 0);

        h.shutdown().await;
    })
    .await
}

/// OpenAI Responses `response.failed` with `error.code = "model_not_found"` and no `message`
/// must preserve the code in `error` and classify as non-retryable `error_kind = "model_not_found"`.
#[tokio::test]
async fn openai_responses_model_not_found_preserves_code_and_kind() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = provider_fixture("openai-responses/failed-model-not-found.sse");
        server.route("/openai", vec![Resp::sse(sse.clone()), Resp::sse(sse)]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 2 } } })).await;

        let m = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(m["stop_reason"], "error", "{m}");
        assert_eq!(m["error_kind"], "model_not_found", "{m}");
        assert!(m["error"].as_str().is_some_and(|e| e.contains("model_not_found")), "{m}");
        assert_eq!(server.count("/openai"), 1, "model_not_found must not be retried: {m}");

        h.shutdown().await;
    })
    .await
}

/// table-driven error classification across every built-in provider, `errors.map` /
/// `errors.codes` overrides, generic fallback, and removal of `provider-openai` `M.classify` and
/// hardcoded vendor rules from `llm/errors.lua`.
/// Reference: pi `packages/ai/test/error-body.test.ts:23-99`; Codex `core/src/client_tests.rs:1872`.
#[tokio::test]
async fn provider_errors_classify_through_one_table() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;

        struct Sample {
            model: &'static str,
            route: &'static str,
            resp: Resp,
            expect_kind: &'static str,
            expect_retried: bool,
        }

        let anthropic_ok = || Resp::sse(fixture("anthropic.sse"));
        let responses_ok = || Resp::sse(fixture("openai-responses.sse"));
        let google_ok = || Resp::sse(fixture("google.sse"));
        let compat_ok = || Resp::sse(fixture("openai-compat.sse"));

        let samples = vec![
            // Anthropic
            Sample {
                model: "anthropic/claude-sonnet-5-5",
                route: "/anthropic",
                resp: Resp::status(
                    429,
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                )
                .header("retry-after", "0"),
                expect_kind: "rate_limit",
                expect_retried: true,
            },
            Sample {
                model: "anthropic/claude-sonnet-5-5",
                route: "/anthropic",
                resp: Resp::status(
                    529,
                    r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                ),
                expect_kind: "overloaded",
                expect_retried: true,
            },
            Sample {
                model: "anthropic/claude-sonnet-5-5",
                route: "/anthropic",
                resp: Resp::status(
                    400,
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long"}}"#,
                ),
                expect_kind: "context_overflow",
                expect_retried: false,
            },
            Sample {
                model: "anthropic/claude-sonnet-5-5",
                route: "/anthropic",
                resp: Resp::status(
                    400,
                    r#"{"type":"error","error":{"type":"invalid_request_error","message":"too many images"}}"#,
                ),
                expect_kind: "image_limit",
                expect_retried: false,
            },
            // OpenAI Responses
            Sample {
                model: "openai/gpt-6.1-terra",
                route: "/openai",
                resp: Resp::status(
                    429,
                    r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"Usage limit reached."}}"#,
                ),
                expect_kind: "quota",
                expect_retried: false,
            },
            Sample {
                model: "openai/gpt-6.1-terra",
                route: "/openai",
                resp: Resp::status(
                    429,
                    r#"{"error":{"type":"usage_limit_reached","plan_type":"plus","resets_at":1900000000,"message":"limit"}}"#,
                ),
                expect_kind: "usage_limit",
                expect_retried: false,
            },
            Sample {
                model: "openai/gpt-6.1-terra",
                route: "/openai",
                resp: Resp::status(503, r#"{"error":{"code":"server_is_overloaded","message":"overloaded"}}"#),
                expect_kind: "overloaded",
                expect_retried: true,
            },
            Sample {
                model: "openai/gpt-6.1-terra",
                route: "/openai",
                resp: Resp::sse(responses_event(json!({
                    "type": "response.failed",
                    "response": { "error": { "code": "model_not_found" } }
                }))),
                expect_kind: "model_not_found",
                expect_retried: false,
            },
            // Google
            Sample {
                model: "google/gemini-2.5-pro",
                route: "/google",
                resp: Resp::status(
                    429,
                    r#"[{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded per minute"}}]"#,
                )
                .header("retry-after", "0"),
                expect_kind: "rate_limit",
                expect_retried: true,
            },
            Sample {
                model: "google/gemini-2.5-pro",
                route: "/google",
                resp: Resp::status(
                    429,
                    r#"[{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"credit balance exhausted"}}]"#,
                ),
                expect_kind: "quota",
                expect_retried: false,
            },
            Sample {
                model: "google/gemini-2.5-pro",
                route: "/google",
                resp: Resp::sse(
                    "data: {\"error\":{\"code\":429,\"message\":\"Resource has been exhausted\",\"status\":\"RESOURCE_EXHAUSTED\"}}\n\n",
                ),
                expect_kind: "rate_limit",
                expect_retried: true,
            },
            // OpenAI Compat (DeepSeek + Cerebras)
            Sample {
                model: "deepseek/deepseek-chat",
                route: "/compat",
                resp: Resp::status(
                    429,
                    r#"{"error":{"message":"请求过于频繁，超出TPM限制，请稍后重试"}}"#,
                )
                .header("retry-after", "0"),
                expect_kind: "rate_limit",
                expect_retried: true,
            },
            Sample {
                model: "deepseek/deepseek-chat",
                route: "/compat",
                resp: Resp::status(429, r#"{"error":{"message":"账户余额不足，请充值后重试"}}"#),
                expect_kind: "quota",
                expect_retried: false,
            },
            Sample {
                model: "deepseek/deepseek-chat",
                route: "/compat",
                resp: Resp::status(400, r#"{"error":{"message":"Prompt too long"}}"#),
                expect_kind: "context_overflow",
                expect_retried: false,
            },
            Sample {
                model: "cerebras/llama-3.3-70b",
                route: "/cerebras",
                resp: Resp::status(400, ""),
                expect_kind: "context_overflow",
                expect_retried: false,
            },
            Sample {
                model: "cerebras/llama-3.3-70b",
                route: "/cerebras",
                resp: Resp::status(400, "400 status code (no body)"),
                expect_kind: "context_overflow",
                expect_retried: false,
            },
            // opencode Go (both routes)
            Sample {
                model: "opencode-go/glm-5.3-flash",
                route: "/gw/anthropic",
                resp: Resp::status(400, r#"{"type":"error","error":{"type":"invalid_request_error","message":"MissingSessionID"}}"#),
                expect_kind: "invalid",
                expect_retried: false,
            },
            Sample {
                model: "opencode-go/crouton-alpha-free",
                route: "/gw/openai",
                resp: Resp::status(429, r#"{"error":{"code":"goUsageLimitError","message":"limit reached"}}"#),
                expect_kind: "quota",
                expect_retried: false,
            },
            Sample {
                model: "opencode-go/crouton-alpha-free",
                route: "/gw/openai",
                resp: Resp::status(429, r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit reached"}}"#),
                expect_kind: "quota",
                expect_retried: false,
            },
        ];

        let mut routes: std::collections::BTreeMap<&str, Vec<Resp>> = std::collections::BTreeMap::new();
        for s in &samples {
            let q = routes.entry(s.route).or_default();
            q.push(s.resp.clone());
            if s.expect_retried {
                q.push(match s.route {
                    "/anthropic" | "/gw/anthropic" => anthropic_ok(),
                    "/openai" | "/gw/openai" => responses_ok(),
                    "/google" => google_ok(),
                    _ => compat_ok(),
                });
            }
        }
        // Extra routes for `errors.map` / `errors.codes` overrides and generic fallback.
        routes.insert(
            "/custom-map",
            vec![
                Resp::status(400, r#"{"error":{"code":"soft_throttle","message":"slow"}}"#),
                compat_ok(),
                Resp::status(503, r#"{"error":{"code":"bad_tenant","message":"blocked"}}"#),
                Resp::status(429, r#"{"error":{"code":"goUsageLimitError","message":"generic limit"}}"#).header("retry-after", "0"),
                compat_ok(),
                Resp::status(418, r#"{"error":{"message":"plain teapot"}}"#),
            ],
        );
        for (path, resps) in routes {
            server.route(path, resps);
        }

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "cerebras": {
                        "base_url": format!("{}/cerebras", server.base),
                        "dialect": "cerebras",
                        "no_key": true,
                        "models": [{ "model_id": "llama-3.3-70b" }]
                    },
                    "custom-ep": {
                        "base_url": format!("{}/custom-map", server.base),
                        "no_key": true,
                        "models": [{ "model_id": "m1" }]
                    }
                },
                "plugins": {
                    "llm": { "retries": 1, "retry_base_ms": 1, "retry_max_ms": 2 },
                    "provider-opencode-go": {
                        "base_url": format!("{}/gw", server.base),
                        "routes": { "responses": { "path": "/openai" }, "anthropic": { "path": "/anthropic" } },
                        "model_routes": { "glm-*": "anthropic" }
                    }
                }
            }),
        )
        .await;

        let mut expected_counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for (idx, s) in samples.iter().enumerate() {
            let msg = stream(&h, s.model).await;
            let inc = if s.expect_retried { 2 } else { 1 };
            *expected_counts.entry(s.route).or_insert(0) += inc;
            assert_eq!(server.count(s.route), expected_counts[s.route], "sample {idx} ({}): {msg}", s.model);
            if s.expect_retried {
                assert_ne!(msg["stop_reason"], "error", "sample {idx} ({}) should recover on retry: {msg}", s.model);
            } else {
                assert_eq!(msg["stop_reason"], "error", "sample {idx} ({}): {msg}", s.model);
                assert_eq!(msg["error_kind"], s.expect_kind, "sample {idx} ({}): {msg}", s.model);
            }
        }

        // Verify provider.errors.map / provider.errors.codes override generic rules, generic fallback
        // handles unknown errors without vendor words in llm/errors.lua, and RESPONSES.classify is gone.
        let (has_legacy_classify, k1, k2, k3, k4): (bool, String, String, String, String) = eval(
            &h,
            r#"
          local RESPONSES = require("provider-openai.wire")
          local has_legacy = RESPONSES.classify ~= nil
          local p = LLM:provider("custom-ep")
          p.errors = {
            codes = { bad_tenant = { kind = "invalid", retryable = false } },
            map = function(raw)
              if raw.api_code == "soft_throttle" then
                return { kind = "rate_limit", retryable = true, hint = "mapped" }
              end
            end,
          }
          local r1 = LLM:stream({ model = LLM:resolve("custom-ep/m1"), messages = {}, no_fallback = true }, function() end)
          local r2 = LLM:stream({ model = LLM:resolve("custom-ep/m1"), messages = {}, no_fallback = true }, function() end)
          -- Remove vendor override and verify unknown provider no longer hits opencode-go's vendor word
          -- in llm/errors.lua (429 falls back to generic rate_limit and retries, 418 falls back to invalid).
          p.errors = nil
          local r3 = LLM:stream({ model = LLM:resolve("custom-ep/m1"), messages = {}, no_fallback = true }, function() end)
          local r4 = LLM:stream({ model = LLM:resolve("custom-ep/m1"), messages = {}, no_fallback = true }, function() end)
          return has_legacy, tostring(r1.stop_reason), tostring(r2.error_kind), tostring(r3.stop_reason), tostring(r4.error_kind)
        "#,
        )
        .await;
        assert!(!has_legacy_classify, "provider-openai wire.M.classify must be deleted");
        assert_eq!(k1, "tool_use", "errors.map retryable=true override must retry 400 and recover");
        assert_eq!(k2, "invalid", "errors.codes must override 503 to non-retryable invalid");
        assert_eq!(k3, "tool_use", "removed vendor code on unrelated provider must fall back to generic 429 rate_limit");
        assert_eq!(k4, "invalid", "unknown 4xx error must fall back to generic invalid");
        assert_eq!(server.count("/custom-map"), 6);

        h.shutdown().await;
    })
    .await
}

/// OpenAI 429 `slow_down` arriving inside a `response.failed` SSE event has no HTTP status and
/// carries `"Request rate increased too quickly."`: it must classify as retryable `rate_limit`,
/// while billing/quota phrases (`LIMIT_WORDS`) still take precedence over it.
#[tokio::test]
async fn request_rate_increased_too_quickly_retries_unless_quota_limit() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let slow_down_sse = responses_event(json!({
            "type": "response.failed",
            "response": { "error": { "code": "slow_down", "message": "Request rate increased too quickly. Please slow down." } }
        }));
        let quota_and_slow_down_sse = responses_event(json!({
            "type": "response.failed",
            "response": { "error": { "message": "You exceeded your current quota; request rate increased too quickly." } }
        }));
        server.route(
            "/openai",
            vec![
                Resp::sse(slow_down_sse),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(quota_and_slow_down_sse),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "llm": { "retries": 1, "retry_base_ms": 1, "retry_max_ms": 2 } } }),
        )
        .await;

        let ok_after_retry = stream(&h, "openai/gpt-6.1-terra").await;
        assert_ne!(
            ok_after_retry["stop_reason"], "error",
            "in-stream 'request rate increased too quickly' must retry as rate_limit: {ok_after_retry}"
        );
        assert_eq!(server.count("/openai"), 2, "must retry once and succeed");

        let quota_err = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(quota_err["stop_reason"], "error", "{quota_err}");
        assert_eq!(quota_err["error_kind"], "quota", "LIMIT_WORDS must still beat RETRY_WORDS: {quota_err}");
        assert_eq!(server.count("/openai"), 3, "quota error must not retry");

        h.shutdown().await;
    })
    .await
}
