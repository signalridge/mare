//! Replay tests for provider anomaly & edge-case fixtures.
//!
//! Covers Anthropic, OpenAI Responses, Google Gemini, and OpenAI-compat edge cases
//! (refusal, fallback, 429 quota, no-args tool calls, severed streams, mid-stream errors,
//! missing message_stop, and Responses failure/incomplete events).

use super::*;

#[tokio::test]
async fn anthropic_refusal_and_fallback_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(chunks_to_sse(&provider_fixture("anthropic/refusal.chunks.txt"), false)),
                Resp::sse(chunks_to_sse(&provider_fixture("anthropic/fallback.chunks.txt"), false)),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let refusal = ask(&h, "anthropic/claude-sonnet-5-5").await;
        // mare maps Anthropic `stop_reason: "refusal"` to `stop_reason: "filtered"` and
        // surfaces `stop_details.explanation` as the error text.
        assert_eq!(refusal["stop_reason"], "filtered", "{refusal}");
        assert!(
            refusal["error"].as_str().is_some_and(|e| e.contains("was blocked under Anthropic's Usage Policy")),
            "expected stop_details.explanation in error: {refusal}"
        );

        let fallback = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(fallback["stop_reason"], "stop", "{fallback}");
        assert_eq!(
            text_of(&fallback),
            "The printing press was invented by Johannes Gutenberg around 1440.",
            "{fallback}"
        );
        assert_eq!(fallback["usage"]["input"], 412, "{fallback}");
        assert_eq!(fallback["usage"]["output"], 264, "{fallback}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_429_and_no_args_tool_call_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err_429 = provider_fixture("google/429-retry-info.json");
        let stream_no_args = chunks_to_sse(&provider_fixture("google/stream-no-args-tool-call.chunks.txt"), false);
        server.route("/google", vec![Resp::status(429, &err_429), Resp::sse(stream_no_args)]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let quota = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(quota["stop_reason"], "error", "{quota}");
        assert_eq!(quota["error_kind"], "quota", "{quota}");
        assert!(quota["error"].as_str().is_some_and(|e| e.contains("quota")), "expected quota message: {quota}");

        let msg = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(msg["stop_reason"], "tool_use", "{msg}");
        let content = msg["content"].as_array().unwrap();
        let thinking = content.iter().find(|b| b["type"] == "reasoning").expect("reasoning block");
        assert!(thinking["text"].as_str().unwrap().contains("Processing User Requests"), "{thinking}");
        let calls: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(calls.len(), 4, "expected read_theme + 3x read_screen: {calls:?}");
        assert_eq!(calls[0]["name"], "read_theme");
        assert_eq!(calls[0]["arguments"], json!({}), "got {:?}", calls[0]["arguments"]);
        assert_eq!(calls[1]["name"], "read_screen");
        assert_eq!(calls[1]["arguments"]["id"], "A");
        assert_eq!(calls[2]["arguments"]["id"], "B");
        assert_eq!(calls[3]["arguments"]["id"], "C");
        assert_eq!(msg["usage"]["input"], 249, "{msg}");
        assert_eq!(msg["usage"]["reasoning"], 183, "{msg}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_error_refusal_and_compat_tool_call_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let openai_err = chunks_to_sse(&provider_fixture("openai-responses/error-quota.chunks.txt"), false);
        let openai_refusal = chunks_to_sse(&provider_fixture("openai-responses/refusal.chunks.txt"), false);
        let compat_tool = chunks_to_sse(&provider_fixture("openai-compat/reasoning-and-tool-call.chunks.txt"), true);
        server.route("/openai", vec![Resp::sse(openai_err), Resp::sse(openai_refusal)]);
        server.route("/xai", vec![Resp::sse(compat_tool)]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "xai": {
                        "base_url": format!("{}/xai", server.base),
                        "no_key": true,
                        "models": [{ "model_id": "grok-3-mini", "reasoning": true }]
                    }
                },
                "plugins": {
                    "llm": { "retries": 0 }
                }
            }),
        )
        .await;

        let err = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(err["stop_reason"], "error", "{err}");
        assert!(
            err["error"].as_str().is_some_and(|e| e.contains("insufficient_quota") || e.contains("quota")),
            "{err}"
        );

        let refusal = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(refusal["stop_reason"], "stop", "{refusal}");
        assert_eq!(refusal["usage"]["input"], 55, "{refusal}");
        assert_eq!(refusal["usage"]["output"], 13, "{refusal}");

        let compat = ask(&h, "xai/grok-3-mini").await;
        assert_eq!(compat["stop_reason"], "tool_use", "{compat}");
        let content = compat["content"].as_array().unwrap();
        let thinking = content.iter().find(|b| b["type"] == "reasoning").expect("reasoning");
        assert_eq!(thinking["text"], "First, the user is");
        let call = content.iter().find(|b| b["type"] == "tool_call").expect("tool_call");
        assert_eq!(call["name"], "weather");
        assert_eq!(call["arguments"]["location"], "San Francisco");
        assert_eq!(compat["usage"]["input"], 1, "291 prompt - 290 cached_tokens = 1: {compat}");
        assert_eq!(compat["usage"]["cache_read"], 290, "{compat}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_severed_no_usage_and_post_stop_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(provider_fixture("anthropic/severed-upstream.sse")),
                Resp::sse(provider_fixture("anthropic/message-delta-no-usage.sse")),
                Resp::sse(provider_fixture("anthropic/unknown-post-stop.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let severed = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(severed["stop_reason"], "error", "severed stream without message_delta must error: {severed}");
        assert_eq!(text_of(&severed), "hel", "partial text before disconnect preserved: {severed}");

        let no_usage = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(no_usage["stop_reason"], "stop", "{no_usage}");
        assert_eq!(text_of(&no_usage), "Hello");
        assert_eq!(no_usage["usage"]["input"], 12, "message_start usage preserved: {no_usage}");

        let post_stop = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(post_stop["stop_reason"], "stop", "{post_stop}");
        assert_eq!(text_of(&post_stop), "Hello");
        assert_eq!(post_stop["usage"]["output"], 5, "{post_stop}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_stream_envelope_error_and_preamble_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(provider_fixture("anthropic/mid-stream-transient-error.sse")),
                Resp::sse(provider_fixture("anthropic/mid-stream-deterministic-error.sse")),
                Resp::sse(provider_fixture("anthropic/preamble-ping-unknown.sse")),
                Resp::sse(provider_fixture("anthropic/missing-message-stop.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let transient = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(transient["stop_reason"], "error", "{transient}");
        assert_eq!(text_of(&transient), "Here is the first half", "{transient}");
        assert!(transient["error"].as_str().is_some_and(|e| e.contains("upstream stream truncated")), "{transient}");

        let det = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(det["stop_reason"], "error", "{det}");
        assert!(det["error"].as_str().is_some_and(|e| e.contains("prompt is too long")), "{det}");

        let preamble = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(preamble["stop_reason"], "stop", "{preamble}");
        assert_eq!(text_of(&preamble), "ok", "{preamble}");

        let no_stop = ask(&h, "anthropic/claude-sonnet-5-5").await;
        // Note: mare requires `message_stop` on Anthropic SSE; closing after `message_delta`
        // without `message_stop` produces `stream_closed` + `interrupted: true` while keeping partial text.
        assert_eq!(no_stop["stop_reason"], "error", "{no_stop}");
        assert_eq!(no_stop["error_kind"], "stream_closed", "{no_stop}");
        assert_eq!(no_stop["interrupted"], true, "{no_stop}");
        assert_eq!(text_of(&no_stop), "partial", "{no_stop}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_error_and_incomplete_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![
                Resp::status(429, provider_fixture("openai-responses/usage-limit-429.json")),
                Resp::sse(provider_fixture("openai-responses/usage-limit-failed.sse")),
                Resp::sse(provider_fixture("openai-responses/failed-retry-after.sse")),
                Resp::sse(provider_fixture("openai-responses/failed-server-overloaded.sse")),
                Resp::sse(provider_fixture("openai-responses/incomplete-partial-output.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let err_429 = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(err_429["stop_reason"], "error", "{err_429}");
        // subscription_sharing_usage_limit_exceeded is a non-retryable quota, not a rate limit.
        assert_eq!(err_429["error_kind"], "quota", "{err_429}");
        assert!(err_429["error"].as_str().is_some_and(|e| e.contains("Usage limit reached.")), "{err_429}");

        let err_failed = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(err_failed["stop_reason"], "error", "{err_failed}");
        assert!(err_failed["error"].as_str().is_some_and(|e| e.contains("Usage limit reached.")), "{err_failed}");

        let retry_msg = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(retry_msg["stop_reason"], "error", "{retry_msg}");
        assert_eq!(retry_msg["error_kind"], "rate_limit", "{retry_msg}");

        let overloaded = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(overloaded["stop_reason"], "error", "{overloaded}");
        assert!(overloaded["error"].as_str().is_some_and(|e| e.contains("Server is overloaded.")), "{overloaded}");

        let incomplete = ask(&h, "openai/gpt-6.1-terra").await;
        // Non-max_output_tokens `response.incomplete` (`reason: "invalid_recipient"`) maps to
        // `stop_reason: "error"`, `error_kind: "fatal"`.
        assert_eq!(incomplete["stop_reason"], "error", "{incomplete}");
        assert_eq!(incomplete["error_kind"], "fatal", "{incomplete}");
        assert!(incomplete["error"].as_str().is_some_and(|e| e.contains("invalid_recipient")), "{incomplete}");

        h.shutdown().await;
    })
    .await
}

/// Issue #185: a completed Responses `function_call` item with a missing/empty tool name and
/// call_id must be rejected as malformed instead of being persisted into session history with a
/// synthesized tool call ID.
#[tokio::test]
async fn responses_malformed_function_call_does_not_enter_history() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let malformed_sse = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"fc_bad\",\"type\":\"function_call\",\"call_id\":\"\",\"name\":\"\",\"arguments\":\"{}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_bad\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        let malformed_custom_sse = concat!(
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"ctc_bad\",\"type\":\"custom_tool_call\",\"call_id\":\"call_ok\",\"name\":\"\",\"input\":\"patch\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_bad2\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        // Queue malformed function_call, malformed custom_tool_call, then a clean follow-up reply.
        server.route(
            "/openai",
            vec![
                Resp::sse(malformed_sse),
                Resp::sse(malformed_custom_sse),
                Resp::sse(responses_text("conversation still usable")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "model": "openai/gpt-6.1-terra", "plugins": { "llm": { "retries": 0 } } }),
        )
        .await;
        expose(&h);
        global(&h, "AG", h.service_table("agent").unwrap());

        let first = h.prompt("run tool", json!({})).await.unwrap();
        assert_eq!(first["error_kind"], "malformed_response", "expected structured malformed_response: {first}");

        let second = h.prompt("run custom tool", json!({})).await.unwrap();
        assert_eq!(second["error_kind"], "malformed_response", "expected structured malformed_response: {second}");

        let msgs_json: String = eval(&h, "return mare.json.encode(AG:list_messages().data)").await;
        let msgs: Value = serde_json::from_str(&msgs_json).unwrap();
        let bad_calls: Vec<&Value> = msgs
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_call")
            .collect();
        assert!(
            bad_calls.is_empty(),
            "malformed empty-name Responses function_call reached session history (with synthesized id): {bad_calls:?}\nall messages: {msgs}"
        );

        // Next turn in the same conversation must succeed without replaying any invalid tool call.
        let followup = h.prompt("continue", json!({})).await.unwrap();
        assert_eq!(followup["text"], "conversation still usable", "{followup}");
        let reqs = server.requests();
        let last_input = reqs.last().unwrap().json()["input"].to_string();
        assert!(
            !last_input.contains("function_call") && !last_input.contains("custom_tool_call"),
            "malformed tool call must not replay on follow-up turn: {last_input}"
        );
        h.shutdown().await;
    })
    .await
}

/// Review regression (#329): explicit `item.id` mismatch at the same `output_index` must NOT
/// borrow another item's name/call_id, conflicting `added` vs `done` identities must fail as
/// `malformed_response`, and index-only correlation succeeds only when no explicit ID is present
/// and the index uniquely identifies an unfinished item.
#[tokio::test]
async fn responses_tool_call_identity_does_not_borrow_across_distinct_item_ids_or_conflicts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        // 1) output_index 0 has added item A (`fc_a`), then done arrives for distinct explicit id `fc_b`
        //    at the same output_index 0 with empty name and call_id -> MUST reject as malformed_response.
        let cross_id_borrow_sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"probe_tool\",\"arguments\":\"\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"fc_b\",\"type\":\"function_call\",\"call_id\":\"\",\"name\":\"\",\"arguments\":\"{}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_cross\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        // 2) Same item.id (`fc_a`) on added and done, but conflicting call_id / name -> MUST reject.
        let conflicting_id_sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_a\",\"name\":\"probe_tool\",\"arguments\":\"\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"fc_a\",\"type\":\"function_call\",\"call_id\":\"call_other\",\"name\":\"other_tool\",\"arguments\":\"{}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_conflict\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        // 3) Ambiguous output_index (two added items share index 0), followed by ID-less done -> MUST reject.
        let ambiguous_index_sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_1\",\"type\":\"function_call\",\"call_id\":\"call_1\",\"name\":\"probe_tool\",\"arguments\":\"\"}}\n\n",
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_2\",\"type\":\"function_call\",\"call_id\":\"call_2\",\"name\":\"probe_tool\",\"arguments\":\"\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"arguments\":\"{}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ambig\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        // 4) Valid correlation: added has id+call_id+name, done matches id and omits name/call_id -> succeeds.
        let valid_correlated_sse = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc_ok\",\"type\":\"function_call\",\"call_id\":\"call_ok\",\"name\":\"probe_tool\",\"arguments\":\"\"}}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"id\":\"fc_ok\",\"type\":\"function_call\",\"arguments\":\"{}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_ok\",\"status\":\"completed\",\"usage\":{\"input_tokens\":5,\"output_tokens\":2}}}\n\n",
        );
        server.route(
            "/openai",
            vec![
                Resp::sse(cross_id_borrow_sse),
                Resp::sse(conflicting_id_sse),
                Resp::sse(ambiguous_index_sse),
                Resp::sse(valid_correlated_sse),
                Resp::sse(responses_text("tool finished")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "model": "openai/gpt-6.1-terra", "plugins": { "llm": { "retries": 0 } } }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"T:register("probe_tool", {
                description = "probe",
                side_effect = "read",
                parameters = { type = "object", properties = {} },
                execute = function()
                    mare.g.PROBE_RAN = (mare.g.PROBE_RAN or 0) + 1
                    return "probe_ok"
                end,
            })"#,
        )
        .await;

        let r1 = h.prompt("cross id", json!({})).await.unwrap();
        assert_eq!(r1["error_kind"], "malformed_response", "distinct item.id must not borrow index 0: {r1}");

        let r2 = h.prompt("conflict id", json!({})).await.unwrap();
        assert_eq!(r2["error_kind"], "malformed_response", "conflicting added/done identity must fail: {r2}");

        let r3 = h.prompt("ambiguous index", json!({})).await.unwrap();
        assert_eq!(r3["error_kind"], "malformed_response", "ambiguous output_index must fail: {r3}");

        assert_eq!(eval::<i64>(&h, "return mare.g.PROBE_RAN or 0").await, 0, "malformed turns must not run tool");

        let r4 = h.prompt("valid correlated", json!({})).await.unwrap();
        assert_eq!(r4["text"], "tool finished", "{r4}");
        assert_eq!(eval::<i64>(&h, "return mare.g.PROBE_RAN or 0").await, 1, "valid correlated turn must run tool");

        h.shutdown().await;
    })
    .await
}
