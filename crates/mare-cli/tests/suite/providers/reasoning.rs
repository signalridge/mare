//! Reasoning: usage accounting, effort-scaled timeouts and replaying reasoning across tool-call turns.

use super::*;

/// Usage per provider: OpenRouter cache writes + reasoning, Kimi's top-level cached_tokens, input
/// clamped at 0 (pi parseChunkUsage); Anthropic message_delta cache fields override only when
/// present; Responses cache writes.
#[tokio::test]
async fn usage_reports_cache_writes_reasoning_and_never_negative_input() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let with_usage = |u: Value| compat_events(&[compat_text("ok"), compat_stop(), json!({"choices":[],"usage":u})]);
        server.route(
            "/compat",
            vec![
                Resp::sse(with_usage(json!({"prompt_tokens":100,"completion_tokens":50,
                    "prompt_tokens_details":{"cached_tokens":20,"cache_write_tokens":10},
                    "completion_tokens_details":{"reasoning_tokens":30}}))),
                Resp::sse(with_usage(json!({"prompt_tokens":100,"completion_tokens":5,"cached_tokens":40}))),
                Resp::sse(with_usage(
                    json!({"prompt_tokens":5,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":20}}),
                )),
            ],
        );
        let anthropic = fixture("anthropic-text.sse").replace(
            r#""usage":{"output_tokens":3}"#,
            r#""usage":{"output_tokens":3,"cache_read_input_tokens":7,"cache_creation_input_tokens":9}"#,
        );
        server.route("/anthropic", vec![Resp::sse(anthropic)]);
        let openai = fixture("openai-responses.sse")
            .replace(r#""cached_tokens":20}"#, r#""cached_tokens":20,"cache_write_tokens":15}"#);
        server.route("/openai", vec![Resp::sse(openai)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let usage = |m: &Value, k: &str| m["usage"][k].as_i64();

        let m = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!(
            [
                usage(&m, "input"),
                usage(&m, "cache_read"),
                usage(&m, "cache_write"),
                usage(&m, "reasoning"),
                usage(&m, "output")
            ],
            [Some(70), Some(20), Some(10), Some(30), Some(50)],
            "{m}"
        );
        let m = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!([usage(&m, "input"), usage(&m, "cache_read")], [Some(60), Some(40)], "Kimi cached_tokens: {m}");
        let m = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!(usage(&m, "input"), Some(0), "clamped: {m}");

        let m = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(
            [usage(&m, "input"), usage(&m, "output"), usage(&m, "cache_read"), usage(&m, "cache_write")],
            [Some(10), Some(3), Some(7), Some(9)],
            "message_start input kept, delta cache fields applied: {m}"
        );

        let m = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!([usage(&m, "input"), usage(&m, "cache_write")], [Some(85), Some(15)], "{m}");
        h.shutdown().await;
    })
    .await
}

/// Anthropic 1h cache writes (`cache_creation.ephemeral_1h_input_tokens`) are recorded in
/// `usage.cache_write_1h` (subset of `cache_write`) and priced at 2x base input while the
/// remaining 5m writes use `cost.cache_write` (pi `test/anthropic-cache-write-1h-cost.test.ts:61-124`).
#[tokio::test]
async fn anthropic_1h_cache_write_usage_and_cost() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = |start_usage: Value, delta_usage: Value| {
            format!(
                "event: message_start\ndata: {}\n\n\
                 event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
                 event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"ok\"}}}}\n\n\
                 event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
                 event: message_delta\ndata: {}\n\n\
                 event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                json!({"type": "message_start", "message": {"id": "msg_1", "usage": start_usage}}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": delta_usage}),
            )
        };
        server.route(
            "/anthropic",
            vec![
                // Case 1: breakdown in message_start (600k 5m + 400k 1h = 1M cache_write).
                Resp::sse(sse(
                    json!({
                        "input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 1_000_000,
                        "cache_creation": { "ephemeral_5m_input_tokens": 600_000, "ephemeral_1h_input_tokens": 400_000 }
                    }),
                    json!({
                        "input_tokens": 0, "output_tokens": 0, "cache_read_input_tokens": 0,
                        "cache_creation_input_tokens": 1_000_000
                    }),
                )),
                // Case 2: breakdown only in message_delta (proxy/gateway style).
                Resp::sse(sse(
                    json!({ "input_tokens": 0, "output_tokens": 0 }),
                    json!({
                        "input_tokens": 0, "output_tokens": 0,
                        "cache_creation_input_tokens": 500_000,
                        "cache_creation": { "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 500_000 }
                    }),
                )),
                // Case 3: no breakdown -> all 1M priced at 5m rate.
                Resp::sse(sse(
                    json!({ "input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 1_000_000 }),
                    json!({ "input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 1_000_000 }),
                )),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);
        global(&h, "USAGE", h.service_table("usage").unwrap());

        let m1 = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(m1["usage"]["cache_write"].as_i64(), Some(1_000_000), "{m1}");
        assert_eq!(m1["usage"]["cache_write_1h"].as_i64(), Some(400_000), "{m1}");

        let m2 = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(m2["usage"]["cache_write"].as_i64(), Some(500_000), "{m2}");
        assert_eq!(m2["usage"]["cache_write_1h"].as_i64(), Some(500_000), "{m2}");

        let m3 = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(m3["usage"]["cache_write"].as_i64(), Some(1_000_000), "{m3}");
        assert_eq!(m3["usage"]["cache_write_1h"].as_i64().unwrap_or(0), 0, "{m3}");

        // `ask` calls `LLM:stream`, which emits `llm/usage` into the `usage` service:
        // claude-sonnet-5-5: input=3, cache_write(5m)=3.75, 1h=2*3=6.
        // m1: 600k*3.75 + 400k*6 = 2.25 + 2.40 = 4.65
        // m2: 500k*6 = 3.00
        // m3: 1M*3.75 = 3.75
        // total = 11.40
        let (total_cost, total_1h): (f64, i64) =
            eval(&h, "local t = USAGE:totals(); return t.cost, t.cache_write_1h or 0").await;
        assert!((total_cost - 11.40).abs() < 1e-6, "expected 11.40, got {total_cost}");
        assert_eq!(total_1h, 900_000);
        h.shutdown().await;
    })
    .await
}

/// OpenRouter `usage.cost` (including BYOK `cost_details`) populates `usage.cost` on the parsed
/// assistant message and overrides catalog estimation in the `usage` service
/// (pi `test/openrouter-cost.test.ts:24-78`, `openai-completions-provider-stream-event.test.ts:126,153`).
#[tokio::test]
async fn openrouter_reported_cost_overrides_catalog_estimate() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let server = MockServer::start().await;
        let with_usage = |u: Value| compat_events(&[compat_text("ok"), compat_stop(), json!({"choices":[],"usage":u})]);
        server.route(
            "/openrouter",
            vec![
                // Non-BYOK with zero cost (e.g. free model; catalog would be > 0).
                Resp::sse(with_usage(json!({
                    "prompt_tokens": 1_000_000, "completion_tokens": 0,
                    "cost": 0, "is_byok": false,
                    "cost_details": { "upstream_inference_cost": 0, "upstream_inference_prompt_cost": 0, "upstream_inference_completions_cost": 0 }
                }))),
                // Non-BYOK with actual billed cost.
                Resp::sse(with_usage(json!({
                    "prompt_tokens": 100, "completion_tokens": 50,
                    "cost": 0.0123, "is_byok": false
                }))),
                // BYOK with explicit upstream_inference_cost.
                Resp::sse(with_usage(json!({
                    "prompt_tokens": 100, "completion_tokens": 50,
                    "cost": 0.1, "is_byok": true,
                    "cost_details": { "upstream_inference_cost": 0.9, "upstream_inference_prompt_cost": 0.6, "upstream_inference_completions_cost": 0.3 }
                }))),
                // BYOK with upstream_inference_cost: null -> sum prompt + completions.
                Resp::sse(with_usage(json!({
                    "prompt_tokens": 100, "completion_tokens": 50,
                    "cost": 0.1, "is_byok": true,
                    "cost_details": { "upstream_inference_cost": null, "upstream_inference_prompt_cost": 0.6, "upstream_inference_completions_cost": 0.3 }
                }))),
                // Incomplete accounting -> fall back to catalog estimate (1M input * $3/M = $3.0).
                Resp::sse(with_usage(json!({
                    "prompt_tokens": 1_000_000, "completion_tokens": 0,
                    "cost": null, "is_byok": false, "cost_details": null
                }))),
            ],
        );
        // Non-OpenRouter endpoint should NOT treat arbitrary `usage.cost` as OpenRouter accounting.
        server.route(
            "/compat",
            vec![Resp::sse(with_usage(json!({
                "prompt_tokens": 1_000_000, "completion_tokens": 0,
                "cost": 0.0001, "is_byok": false
            })))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "openrouter": { "base_url": format!("{}/openrouter", server.base) }
            } }),
        )
        .await;
        expose(&h);
        global(&h, "USAGE", h.service_table("usage").unwrap());

        let m1 = ask(&h, "openrouter/anthropic/claude-sonnet-5-5").await;
        assert_eq!(m1["usage"]["cost"].as_f64(), Some(0.0), "free model: {m1}");

        let m2 = ask(&h, "openrouter/anthropic/claude-sonnet-5-5").await;
        assert!((m2["usage"]["cost"].as_f64().unwrap() - 0.0123).abs() < 1e-9, "{m2}");

        let m3 = ask(&h, "openrouter/anthropic/claude-sonnet-5-5").await;
        assert!((m3["usage"]["cost"].as_f64().unwrap() - 1.0).abs() < 1e-9, "{m3}");

        let m4 = ask(&h, "openrouter/anthropic/claude-sonnet-5-5").await;
        assert!((m4["usage"]["cost"].as_f64().unwrap() - 1.0).abs() < 1e-9, "{m4}");

        let m5 = ask(&h, "openrouter/anthropic/claude-sonnet-5-5").await;
        assert!(m5["usage"]["cost"].is_null(), "incomplete accounting leaves cost unset: {m5}");

        // 0 + 0.0123 + 1.0 + 1.0 + 3.0 (catalog fallback for m5) = 5.0123
        let or_cost: f64 = eval(&h, "return USAGE:totals().cost").await;
        assert!((or_cost - 5.0123).abs() < 1e-6, "expected 5.0123, got {or_cost}");

        let m_ds = ask(&h, "deepseek/deepseek-chat").await;
        assert!(m_ds["usage"]["cost"].is_null(), "non-OpenRouter ignores OpenRouter accounting: {m_ds}");
        // DeepSeek falls back to catalog pricing (1M * $0.28/M = $0.28).
        let total_cost: f64 = eval(&h, "return USAGE:totals().cost").await;
        assert!((total_cost - 5.2923).abs() < 1e-6, "expected 5.2923, got {total_cost}");
        h.shutdown().await;
    })
    .await
}

/// OpenRouter detection must match the parsed URL hostname (case-insensitive, ignoring port),
/// never substrings in userinfo, paths, queries, or lookalike domains.
#[tokio::test]
async fn openrouter_host_matching_rejects_spoofed_urls() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let (ok_exact, ok_port_case, ok_sub, bad_query, bad_prefix, bad_suffix, bad_userinfo, bad_path): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
            local lib = require("mare_lib")
            return lib.is_openrouter("custom", "https://openrouter.ai/api/v1"),
                lib.is_openrouter("custom", "https://OpenRouter.AI:8443/api/v1"),
                lib.is_openrouter("custom", "https://api.openrouter.ai/v1"),
                lib.is_openrouter("custom", "https://evil.example/?next=openrouter.ai"),
                lib.is_openrouter("custom", "https://notopenrouter.ai/api/v1"),
                lib.is_openrouter("custom", "https://openrouter.ai.evil.com/api/v1"),
                lib.is_openrouter("custom", "https://openrouter.ai@evil.com/api/v1"),
                lib.is_openrouter("custom", "http://localhost:8080/openrouter.ai")
            "#,
        )
        .await;
        assert!(ok_exact, "exact openrouter.ai host");
        assert!(ok_port_case, "case-insensitive host with port");
        assert!(ok_sub, "subdomain of openrouter.ai");
        assert!(!bad_query, "query param containing openrouter.ai must not match");
        assert!(!bad_prefix, "notopenrouter.ai must not match");
        assert!(!bad_suffix, "openrouter.ai.evil.com must not match");
        assert!(!bad_userinfo, "userinfo openrouter.ai@evil.com must not match");
        assert!(!bad_path, "path containing openrouter.ai must not match");
        h.shutdown().await;
    })
    .await
}

/// Missing `cost.input` must not price 1h cache writes at $0: fall back to `cost.cache_write`
/// when present, or mark cost unknown when neither `input` nor `cache_write` is configured.
#[tokio::test]
async fn missing_input_price_for_1h_cache_write_uses_cache_write_fallback_or_unknown() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "anthropic/claude-write-only": {
                        "cost": { "cache_write": 3.75 }
                    },
                    "anthropic/claude-output-only": {
                        "cost": { "output": 15 }
                    }
                }
            }),
        )
        .await;
        expose(&h);
        global(&h, "USAGE", h.service_table("usage").unwrap());

        let (cost_write_only, cost_text_after_unknown): (f64, String) = eval(
            &h,
            r#"
            C.cordis:get("bus"):emit("item/completed", {
                turn_id = "turn-1",
                item_id = "msg-1",
                status = "completed",
                item = {
                    id = "msg-1",
                    type = "assistant_message",
                    message = {
                        role = "assistant",
                        provider_id = "anthropic",
                        model_id = "claude-write-only",
                        usage = { input = 0, output = 0, cache_read = 0, cache_write = 1000000, cache_write_1h = 1000000 }
                    }
                }
            })
            local after_first = USAGE:totals().cost
            C.cordis:get("bus"):emit("item/completed", {
                turn_id = "turn-1",
                item_id = "msg-2",
                status = "completed",
                item = {
                    id = "msg-2",
                    type = "assistant_message",
                    message = {
                        role = "assistant",
                        provider_id = "anthropic",
                        model_id = "claude-output-only",
                        usage = { input = 0, output = 0, cache_read = 0, cache_write = 1000000, cache_write_1h = 1000000 }
                    }
                }
            })
            return after_first, USAGE:cost_text()
            "#,
        )
        .await;
        assert!((cost_write_only - 3.75).abs() < 1e-6, "expected 3.75 fallback, got {cost_write_only}");
        assert!(
            cost_text_after_unknown.contains("unknown") || cost_text_after_unknown.contains("incomplete"),
            "expected unknown/incomplete marker when price is missing, got {cost_text_after_unknown}"
        );
        h.shutdown().await;
    })
    .await
}

/// `turn/completed.usage.cost` must combine provider-reported actual costs and catalog estimates across
/// turns (never publishing a partial subtotal when one turn lacks reported `usage.cost`).
#[tokio::test]
async fn agent_end_usage_cost_handles_mixed_reported_and_estimated_requests() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let server = MockServer::start().await;
        let tool_call = json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "nonexistent_tool", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let turn1 = compat_events(&[
            tool_call,
            json!({"choices": [], "usage": { "prompt_tokens": 100, "completion_tokens": 10, "cost": 0.10, "is_byok": false }}),
        ]);
        let turn2 = compat_events(&[
            compat_text("done"),
            compat_stop(),
            json!({"choices": [], "usage": { "prompt_tokens": 1_000_000, "completion_tokens": 0, "cost": null, "is_byok": false }}),
        ]);
        server.route("/openrouter", vec![Resp::sse(turn1), Resp::sse(turn2)]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "openrouter": { "base_url": format!("{}/openrouter", server.base) }
            } }),
        )
        .await;
        expose(&h);
        global(&h, "AGENT", h.service_table("agent").unwrap());
        global(&h, "USAGE", h.service_table("usage").unwrap());
        exec(&h, r#"LLM:set_model("openrouter/anthropic/claude-sonnet-5-5")"#).await;

        let (end_cost, usage_cost): (Option<f64>, f64) = eval(
            &h,
            r#"
            local captured
            C.cordis:on("turn/completed", function(ev) captured = ev end)
            A:wait(AGENT:prompt("hi", {}).turn_id)
            return captured and captured.usage and captured.usage.cost or nil, USAGE:totals().cost
            "#,
        )
        .await;
        // Turn 1 reported $0.10 + Turn 2 catalog fallback (1M input * $3/M = $3.00) = $3.10.
        assert!((usage_cost - 3.10).abs() < 1e-6, "expected usage service 3.10, got {usage_cost}");
        assert!(
            end_cost.is_some_and(|c| (c - 3.10).abs() < 1e-6),
            "turn/completed.usage.cost must equal 3.10 (not partial 0.10), got {end_cost:?}"
        );
        h.shutdown().await;
    })
    .await
}

/// Anthropic wire must check `endpoint.dialect` (not provider name / label) so a
/// custom proxy URL with `dialect = "openrouter"` still captures OpenRouter `usage.cost`.
#[tokio::test]
async fn anthropic_wire_recognizes_openrouter_via_endpoint_provider() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = format!(
            "event: message_start\ndata: {}\n\n\
             event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
             event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"ok\"}}}}\n\n\
             event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
             event: message_delta\ndata: {}\n\n\
             event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
            json!({"type": "message_start", "message": {"id": "msg_1", "usage": {"input_tokens": 10, "output_tokens": 0}}}),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": { "input_tokens": 10, "output_tokens": 5, "cost": 0.042, "is_byok": false }
            }),
        );
        server.route("/proxy-or", vec![Resp::sse(sse)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let url = format!("{}/proxy-or", server.base);
        let cost: Option<f64> = eval(
            &h,
            &format!(
                r#"
                local wire = LLM:wire("anthropic-messages")
                local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
                local msg = wire(
                    {{ provider = "my-proxy", dialect = "openrouter", label = "Custom Proxy", url = "{url}", key = "k" }},
                    {{ model = m, messages = {{ {{ role = "user", content = {{ {{ type = "text", text = "hi" }} }} }} }} }},
                    function() end
                )
                return msg.usage and msg.usage.cost
                "#
            ),
        )
        .await;
        assert!(cost.is_some_and(|c| (c - 0.042).abs() < 1e-9), "expected 0.042, got {cost:?}");
        h.shutdown().await;
    })
    .await
}

/// One `llm:stream` call at a given effort; the message as JSON.
async fn stream_effort(h: &Host, model: &str, effort: &str) -> Value {
    let f = h
        .lua()
        .load(
            r#"local ref, effort = ...
local m = assert(LLM:resolve(ref))
return LLM:stream({ model = m, effort = effort,
  messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)"#,
        )
        .into_function()
        .unwrap();
    let v: mlua::Value = f.call_async((model, effort)).await.unwrap();
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

/// the idle watchdog and the response-header timeout share one base budget and scale
/// with the reasoning effort.
#[tokio::test]
async fn idle_and_header_timeouts_scale_with_the_reasoning_effort() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        exec(&h, SPY_HTTP).await;
        for effort in ["low", "high", "xhigh"] {
            let msg = stream_effort(&h, "anthropic/claude-sonnet-5-5", effort).await;
            assert_eq!(msg["stop_reason"], "stop", "{effort}: {msg}");
        }
        let seen: Vec<(i64, i64)> = eval::<mlua::Table>(&h, "return SEEN")
            .await
            .sequence_values::<mlua::Table>()
            .map(|t| {
                let t = t.unwrap();
                (t.get("idle").unwrap(), t.get("header").unwrap())
            })
            .collect();
        let [(idle, header), high, xhigh] = seen[..] else { panic!("{seen:?}") };
        assert!(idle > 0 && idle == header, "one base budget for both: {seen:?}");
        assert_eq!(high, (idle * 2, header * 2), "high x2: {seen:?}");
        assert_eq!(xhigh, (idle * 3, header * 3), "xhigh x3: {seen:?}");
        h.shutdown().await;
    })
    .await
}

/// opencode Go's DeepSeek V4.1 Flash declares DeepSeek's own levels (low|high|max): the session
/// effort is clamped to them and sent as Responses `reasoning.effort`; max stays max (never
/// collapsed to high, a pi bug), xhigh is as near to high as to max and takes the lower (the
/// clamp's tie rule), off is "none".
#[tokio::test]
async fn opencode_go_deepseek_sends_high_and_max_as_they_are() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route("/og", (0..4).map(|_| Resp::sse(responses_text("ok"))).collect());
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "base_url": server.base, "routes": { "responses": { "path": "/og" } } } } }),
        )
        .await;
        exec(
            &h,
            r#"local m = assert(LLM:resolve("opencode-go/deepseek-v4.1-flash"))
               LLM:set_model(m.ref, { save = false })
               for _, e in ipairs({ "high", "max", "xhigh", "off" }) do
                 LLM:set_effort(e, { save = false })
                 LLM:stream({ model = m, system = "S",
                   messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
               end"#,
        )
        .await;
        let sent: Vec<Value> = server.requests().iter().map(|r| r.json()["reasoning"]["effort"].clone()).collect();
        // high→high, max→max (never collapsed to high), xhigh→max through the model's explicit
        // effort_map (DeepSeek's own API reads xhigh as max), off→none
        assert_eq!(sent, [json!("high"), json!("max"), json!("max"), json!("none")]);
        h.shutdown().await;
    })
    .await
}

/// History with a tool call, then a second request built from what the first one returned.
pub(super) const ROUND_TRIP: &str = r#"
local ref = ...
local m = assert(LLM:resolve(ref))
local history = { { role = "user", content = { { type = "text", text = "read main" } } } }
local tools = { { name = "read", description = "Read a file", parameters = { type = "object", properties = { path = { type = "string" } } } } }
local first = LLM:stream({ model = m, system = "S", messages = history, tools = tools, effort = "high" }, function() end)
history[#history + 1] = first
for _, b in ipairs(first.content) do
  if b.type == "tool_call" then
    history[#history + 1] = { role = "tool_result", tool_call_id = b.id, tool_name = b.name, content = { { type = "text", text = "fn main() {}" } } }
  end
end
LLM:stream({ model = m, system = "S", messages = history, tools = tools, effort = "high" }, function() end)
"#;

/// DeepSeek interleaved thinking (models.dev `interleaved: reasoning_content`): an assistant turn
/// with tool calls carries its reasoning in the next request. On the chat wire (direct DeepSeek)
/// it is `reasoning_content` on that assistant message, verbatim.
#[tokio::test]
async fn compat_replays_reasoning_content_of_a_tool_call_turn() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                Resp::sse(fixture("openai-compat.sse")),
                Resp::sse(compat_events(&[compat_text("done"), compat_stop()])),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        h.lua().load(ROUND_TRIP).into_function().unwrap().call_async::<()>("deepseek/deepseek-reasoner").await.unwrap();
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let msgs = reqs[1].json()["messages"].as_array().unwrap().clone();
        let asst = msgs.iter().find(|m| m["role"] == "assistant").expect("assistant message replayed");
        assert_eq!(asst["reasoning_content"], "Need to read the file.", "{asst}");
        assert_eq!(asst["tool_calls"][0]["id"], "call_abc", "{asst}");
        assert!(msgs.iter().any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_abc"));
        h.shutdown().await;
    })
    .await
}

/// The same on opencode Go (Responses wire): the reasoning item the gateway returned is replayed
/// before the function call, so the gateway can hand DeepSeek its `reasoning_content` back
/// (before, only provider "openai" got its reasoning items replayed).
#[tokio::test]
async fn opencode_go_replays_the_reasoning_item_of_a_tool_call_turn() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route("/og", vec![Resp::sse(fixture("openai-responses.sse")), Resp::sse(responses_text("done"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "base_url": server.base, "routes": { "responses": { "path": "/og" } } } } }),
        )
        .await;
        h.lua().load(ROUND_TRIP).into_function().unwrap().call_async::<()>("opencode-go/deepseek-v4.1-flash").await.unwrap();
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let input = reqs[1].json()["input"].as_array().unwrap().clone();
        let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap_or("message")).collect();
        let r = types.iter().position(|t| *t == "reasoning").unwrap_or_else(|| panic!("no reasoning item: {types:?}"));
        let fc = types.iter().position(|t| *t == "function_call").expect("function call replayed");
        assert!(r < fc, "reasoning before its tool call: {types:?}");
        assert_eq!(input[r]["summary"][0]["text"], "Need to read the file.", "verbatim: {}", input[r]);
        assert_eq!(reqs[1].json()["reasoning"]["effort"], "high");
        h.shutdown().await;
    })
    .await
}

fn anthropic_sse(events: &[Value]) -> String {
    events
        .iter()
        .map(|ev| format!("event: {}\ndata: {ev}\n\n", ev["type"].as_str().unwrap_or("message")))
        .collect::<Vec<_>>()
        .join("")
}

/// usage-r2 #1: Anthropic `message_delta` with `cache_creation.ephemeral_1h_input_tokens` and
/// omitted `cache_creation_input_tokens` must reconstruct `cache_write` rather than pricing 1h
/// writes at $0, and inconsistent `cache_write_1h > cache_write` must return `nil` (unknown).
#[tokio::test]
async fn anthropic_partial_message_delta_1h_only_and_inconsistent_totals() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let partial_delta = anthropic_sse(&[
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_partial",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-sonnet-5-5",
                    "stop_reason": null,
                    "usage": { "input_tokens": 0, "output_tokens": 0 }
                }
            }),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn"},
                "usage": {
                    "output_tokens": 0,
                    "cache_creation": { "ephemeral_1h_input_tokens": 1_000_000 }
                }
            }),
            json!({"type": "message_stop"}),
        ]);
        server.route("/anthropic", vec![Resp::sse(partial_delta)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);
        global(&h, "USAGE", h.service_table("usage").unwrap());
        let msg = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(msg["usage"]["cache_write_1h"].as_i64(), Some(1_000_000), "{msg}");
        assert_eq!(msg["usage"]["cache_write"].as_i64(), Some(1_000_000), "{msg}");

        let (totals_cost, inconsistent_cost): (f64, Option<f64>) = eval(
            &h,
            r#"
            local lib = require("mare_lib")
            local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local inc = lib.usage_cost(m, { input = 0, output = 0, cache_read = 0, cache_write = 0, cache_write_1h = 1000000 }, false)
            return USAGE:totals().cost, inc
            "#,
        )
        .await;
        assert!((totals_cost - 6.0).abs() < 1e-6, "expected $6.00 for 1M 1h cache write, got {totals_cost}");
        assert!(inconsistent_cost.is_none(), "expected nil for cache_write_1h > cache_write, got {inconsistent_cost:?}");
        h.shutdown().await;
    })
    .await
}

/// usage-r2 #2: `agent/end` must price against the actual responding fallback model (not the
/// initial `req.model`), treat empty `{}` usage as unknown instead of `$0`, and expose
/// `cost_source` (`"actual"` | `"estimated"` | `"mixed"` | `"unknown"`).
#[tokio::test]
async fn agent_end_fallback_model_missing_usage_and_cost_source() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-openrouter");
        let server = MockServer::start().await;
        // Case 1: fallback from cheap ($1/M) model to anthropic/claude-sonnet-5-5 ($3/M input)
        let fb_sse = anthropic_sse(&[
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_fb",
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": "claude-sonnet-5-5",
                    "stop_reason": null,
                    "usage": { "input_tokens": 100_000, "output_tokens": 0 }
                }
            }),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 0}}),
            json!({"type": "message_stop"}),
        ]);
        server.route("/cheap", vec![Resp::status(404, "model_not_found")]);
        server.route("/anthropic", vec![Resp::sse(fb_sse)]);

        // Case 2: compat endpoint returns empty usage `{}` -> unknown instead of $0
        let empty_usage = compat_events(&[compat_text("ok"), compat_stop(), json!({"choices": [], "usage": {}})]);
        // Case 3: mixed actual ($0.10) + estimated ($3.00)
        let tool_call = json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_1",
                        "type": "function",
                        "function": { "name": "nonexistent_tool", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let mix_t1 = compat_events(&[
            tool_call,
            json!({"choices": [], "usage": { "prompt_tokens": 100, "completion_tokens": 10, "cost": 0.10, "is_byok": false }}),
        ]);
        let mix_t2 = compat_events(&[
            compat_text("done"),
            compat_stop(),
            json!({"choices": [], "usage": { "prompt_tokens": 1_000_000, "completion_tokens": 0, "cost": null, "is_byok": false }}),
        ]);
        server.route("/openrouter", vec![Resp::sse(empty_usage), Resp::sse(mix_t1), Resp::sse(mix_t2)]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "fallback_models": ["anthropic/claude-sonnet-5-5"],
                "providers": {
                    "openrouter": { "base_url": format!("{}/openrouter", server.base) },
                    "cheap": {
                        "base_url": format!("{}/cheap", server.base),
                        "api_key": "k",
                        "models": [{
                            "model_id": "cheap-1",
                            "cost": { "input": 1, "output": 1 }
                        }]
                    }
                }
            }),
        )
        .await;
        expose(&h);
        global(&h, "AGENT", h.service_table("agent").unwrap());

        let (fb_cost, fb_src): (Option<f64>, Option<String>) = eval(
            &h,
            r#"
            LLM:set_model("cheap/cheap-1")
            local captured
            C.cordis:on("turn/completed", function(ev) captured = ev end)
            A:wait(AGENT:prompt("hi", {}).turn_id)
            return captured and captured.usage and captured.usage.cost or nil,
                captured and captured.usage and captured.usage.cost_source or nil
            "#,
        )
        .await;
        assert!(fb_cost.is_some_and(|c| (c - 0.3).abs() < 1e-6), "expected fallback cost 0.3, got {fb_cost:?}");
        assert_eq!(fb_src.as_deref(), Some("estimated"));

        let (unk_cost, unk_src, unk_ev): (Option<f64>, Option<String>, String) = eval(
            &h,
            r#"
            LLM:set_model("openrouter/anthropic/claude-sonnet-5-5")
            local captured
            C.cordis:on("turn/completed", function(ev) captured = ev end)
            local res = A:wait(AGENT:prompt("hi", {}).turn_id)
            return captured and captured.usage and captured.usage.cost or nil,
                captured and captured.usage and captured.usage.cost_source or nil,
                mare.json.encode({ captured = captured, res = res })
            "#,
        )
        .await;
        assert!(unk_cost.is_none(), "expected nil cost for empty usage {{}}, got {unk_cost:?} ({unk_ev})");
        assert_eq!(unk_src.as_deref(), Some("unknown"), "{unk_ev}");

        let (mix_cost, mix_src): (Option<f64>, Option<String>) = eval(
            &h,
            r#"
            local captured
            C.cordis:on("turn/completed", function(ev) captured = ev end)
            A:wait(AGENT:prompt("hi", {}).turn_id)
            return captured and captured.usage and captured.usage.cost or nil,
                captured and captured.usage and captured.usage.cost_source or nil
            "#,
        )
        .await;
        assert!(mix_cost.is_some_and(|c| (c - 3.10).abs() < 1e-6), "expected mixed cost 3.10, got {mix_cost:?}");
        assert_eq!(mix_src.as_deref(), Some("mixed"));
        h.shutdown().await;
    })
    .await
}

/// usage-r2 #3: OpenRouter host matching must reject backslash authority delimiter bypasses.
#[tokio::test]
async fn openrouter_host_matching_rejects_backslash_bypass() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let (bad_bs, bad_bs_host, ok_pct, bad_pct_path): (bool, bool, bool, bool) = eval(
            &h,
            r#"
            local lib = require("mare_lib")
            return lib.is_openrouter("custom", "https://evil.example\\@openrouter.ai/v1"),
                lib.is_openrouter("custom", "https://evil.example\\openrouter.ai/v1"),
                lib.is_openrouter("custom", "https://open%72outer.ai/v1"),
                lib.is_openrouter("custom", "https://evil.example/https://open%72outer.ai/v1")
            "#,
        )
        .await;
        assert!(!bad_bs, "backslash before @openrouter.ai must not match");
        assert!(!bad_bs_host, "backslash before openrouter.ai must not match");
        assert!(ok_pct, "percent-encoded host https://open%72outer.ai/v1 must match");
        assert!(!bad_pct_path, "percent-encoded host in path must not match");
        h.shutdown().await;
    })
    .await
}

/// usage-r2 #4: `usage:get()`, `usage:totals()`, and `usage/changed` must expose `cost_unknown`.
#[tokio::test]
async fn usage_service_exposes_cost_unknown_in_structured_totals_and_events() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);
        global(&h, "USAGE", h.service_table("usage").unwrap());
        let flags: Vec<Option<bool>> = eval(
            &h,
            r#"
            local ev_flag
            C.cordis:on("usage/changed", function(ev) ev_flag = ev.cost_unknown end)
            C.cordis:get("bus"):emit("llm/usage", {
                usage = {},
                model = "anthropic/claude-sonnet-5-5",
                provider = "anthropic",
                purpose = "chat",
            })
            local t_unk, g_unk, e_unk = USAGE:totals().cost_unknown, USAGE:get().totals.cost_unknown, ev_flag
            C.cordis:get("bus"):emit("session/new", {})
            local r_unk = USAGE:totals().cost_unknown
            return { t_unk, g_unk, e_unk, ev_flag, r_unk }
            "#,
        )
        .await;
        assert_eq!(flags[0], Some(true));
        assert_eq!(flags[1], Some(true));
        assert_eq!(flags[2], Some(true));
        assert_eq!(flags[3], Some(false), "session/new must reset cost_unknown to boolean false, not 0");
        assert_eq!(flags[4], Some(false));
        h.shutdown().await;
    })
    .await
}

/// LLM defaults pin 300s idle watchdog and 16 MiB SSE caps, and
/// `effort = "max"` scales both idle and response-header timeouts by 4x (`IDLE_MULT.max`).
#[tokio::test]
async fn default_stream_idle_timeout_is_300s_and_max_effort_scales_by_4x() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route("/og", vec![Resp::sse(responses_text("ok")), Resp::sse(responses_text("ok"))]);

        // Verify plugin config schema defaults with load_init = true.
        let mut b = env.host().builtins(&[]).no_session();
        b.opts.load_init = true;
        let h_def = b.build().await;
        let (idle_def, line_def, event_def): (i64, i64, i64) = eval(
            &h_def,
            r#"local d = mare.config_describe({
                 "plugins.llm.stream_idle_timeout_ms",
                 "plugins.llm.sse_max_line_bytes",
                 "plugins.llm.sse_max_event_bytes",
               })
               return d["plugins.llm.stream_idle_timeout_ms"].default,
                      d["plugins.llm.sse_max_line_bytes"].default,
                      d["plugins.llm.sse_max_event_bytes"].default"#,
        )
        .await;
        assert_eq!(idle_def, 300_000, "stream_idle_timeout_ms default must be 300s");
        assert_eq!(line_def, 16 * 1024 * 1024, "sse_max_line_bytes default must be 16 MiB");
        assert_eq!(event_def, 16 * 1024 * 1024, "sse_max_event_bytes default must be 16 MiB");
        h_def.shutdown().await;

        // Verify `max` effort scales idle & header timeouts by 4x vs `low`.
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-opencode-go": {
                        "base_url": server.base,
                        "routes": { "responses": { "path": "/og" } }
                    }
                }
            }),
        )
        .await;
        exec(&h, SPY_HTTP).await;
        for effort in ["low", "max"] {
            let msg = stream_effort(&h, "opencode-go/deepseek-v4.1-flash", effort).await;
            assert_eq!(msg["stop_reason"], "stop", "{effort}: {msg}");
        }
        let seen: Vec<(i64, i64)> = eval::<mlua::Table>(&h, "return SEEN")
            .await
            .sequence_values::<mlua::Table>()
            .map(|t| {
                let t = t.unwrap();
                (t.get("idle").unwrap(), t.get("header").unwrap())
            })
            .collect();
        let [(idle, header), max_pair] = seen[..] else { panic!("{seen:?}") };
        assert_eq!(idle, 300_000, "default base idle timeout is 300s: {seen:?}");
        assert_eq!(header, 300_000, "default base header timeout is 300s: {seen:?}");
        assert_eq!(max_pair, (idle * 4, header * 4), "max effort scales by 4x: {seen:?}");
        h.shutdown().await;
    })
    .await
}
