//! Finish reason handling, usage edge cases, and Google `partialArgs`/`willContinue` assembly
//! (PROV-W2).

use super::*;

/// Compat finish reasons: `error` is a retryable server error; `sensitive`,
/// `data_inspection_failed`, `DataInspectionFailed`, and `censorship_blocked` map to `filtered`;
/// unknown finish reasons map to `error` with `raw_stop_reason` rather than silently becoming `stop`.
#[tokio::test]
async fn compat_finish_reasons_error_filtered_and_unknown_never_become_stop() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let make_stream = |reason: &str| {
            compat_events(&[compat_text("partial"), json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})])
        };

        let make_error_with_usage = compat_events(&[
            compat_text("partial"),
            json!({
                "choices": [{"index": 0, "delta": {}, "finish_reason": "error"}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 2}
            }),
        ]);

        server.route(
            "/compat",
            vec![
                Resp::sse(make_error_with_usage),
                Resp::sse(make_stream("sensitive")),
                Resp::sse(make_stream("data_inspection_failed")),
                Resp::sse(make_stream("DataInspectionFailed")),
                Resp::sse(make_stream("censorship_blocked")),
                Resp::sse(make_stream("insufficient_system_resource")),
            ],
        );

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let err_msg = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(err_msg["stop_reason"], "error", "{err_msg}");
        assert_eq!(err_msg["error_kind"], "server", "{err_msg}");
        assert_eq!(err_msg["usage"]["input"], 7, "usage must be preserved on finish_reason=error: {err_msg}");
        assert_eq!(err_msg["usage"]["output"], 2, "{err_msg}");

        for reason in ["sensitive", "data_inspection_failed", "DataInspectionFailed", "censorship_blocked"] {
            let msg = stream(&h, "deepseek/deepseek-chat").await;
            assert_eq!(msg["stop_reason"], "filtered", "{reason}: {msg}");
            assert_eq!(msg["raw_stop_reason"], reason, "{reason}: {msg}");
            assert!(msg["error"].is_string(), "{reason}: {msg}");
        }

        let unknown = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(unknown["stop_reason"], "error", "{unknown}");
        assert_eq!(unknown["raw_stop_reason"], "insufficient_system_resource", "{unknown}");
        assert_eq!(unknown["error"], "Provider finish_reason: insufficient_system_resource", "{unknown}");
        h.shutdown().await;
    })
    .await
}

/// OpenAI-compatible Chat refusals:
/// - streamed `delta.refusal` chunks with `finish_reason: "stop"` preserve refusal text and map to `filtered`
/// - non-streamed `message.refusal` JSON body with `finish_reason: "stop"` preserves refusal text and maps to `filtered`
/// - `finish_reason: "refusal"` maps to `filtered` with `raw_stop_reason: "refusal"`
#[tokio::test]
async fn compat_refusals_streamed_non_streamed_and_finish_reason_refusal() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let streamed_refusal = compat_events(&[
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":null,"refusal":"I cannot "},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"refusal":"assist with that."},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":6}}),
        ]);
        let non_streamed_refusal = json!({
            "id": "chatcmpl-refusal-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "refusal": "Sorry, I can't fulfill that request."
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 9, "completion_tokens": 7}
        })
        .to_string();
        let finish_reason_refusal = compat_events(&[
            json!({"choices":[{"index":0,"delta":{"refusal":"Blocked by policy."},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"refusal"}]}),
        ]);
        let finish_reason_refusal_no_text = compat_events(&[
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"refusal"}]}),
        ]);

        server.route(
            "/compat",
            vec![
                Resp::sse(streamed_refusal),
                Resp::status(200, &non_streamed_refusal),
                Resp::sse(finish_reason_refusal),
                Resp::sse(finish_reason_refusal_no_text),
            ],
        );

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let m1 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(m1["stop_reason"], "filtered", "{m1}");
        assert_eq!(m1["raw_stop_reason"], "stop", "{m1}");
        assert_eq!(text_of(&m1), "I cannot assist with that.", "{m1}");
        assert_eq!(m1["error"], "I cannot assist with that.", "{m1}");
        assert_eq!(m1["usage"]["input"], 12, "{m1}");
        assert_eq!(m1["usage"]["output"], 6, "{m1}");

        let m2 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(m2["stop_reason"], "filtered", "{m2}");
        assert_eq!(m2["raw_stop_reason"], "stop", "{m2}");
        assert_eq!(text_of(&m2), "Sorry, I can't fulfill that request.", "{m2}");
        assert_eq!(m2["error"], "Sorry, I can't fulfill that request.", "{m2}");
        assert_eq!(m2["usage"]["input"], 9, "{m2}");
        assert_eq!(m2["usage"]["output"], 7, "{m2}");

        let m3 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(m3["stop_reason"], "filtered", "{m3}");
        assert_eq!(m3["raw_stop_reason"], "refusal", "{m3}");
        assert_eq!(text_of(&m3), "Blocked by policy.", "{m3}");
        assert_eq!(m3["error"], "Blocked by policy.", "{m3}");

        let m4 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(m4["stop_reason"], "filtered", "{m4}");
        assert_eq!(m4["raw_stop_reason"], "refusal", "{m4}");
        assert!(m4["error"].as_str().is_some_and(|e| e.contains("refusal")), "{m4}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn repro_267_zai_context_window_stop_reasons_route_to_overflow_recovery() {
    local(async {
        // 1) Anthropic wire: stop_reason = "model_context_window_exceeded"
        {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let overflow_sse = sse_events(&[
                json!({"type": "message_start", "message": {"id": "msg_ov", "type": "message", "role": "assistant", "content": [], "model": "claude-sonnet-5-5", "stop_reason": null, "usage": {"input_tokens": 500, "output_tokens": 1}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
                json!({"type": "content_block_stop", "index": 0}),
                json!({"type": "message_delta", "delta": {"stop_reason": "model_context_window_exceeded"}, "usage": {"output_tokens": 10}}),
                json!({"type": "message_stop"}),
            ]);
            let ok_sse = fixture("anthropic.sse");
            server.route("/anthropic", vec![Resp::sse(overflow_sse), Resp::sse(ok_sse)]);
            let h = provider_host(
                &mut env,
                &server,
                json!({
                    "model": "anthropic/claude-sonnet-5-5",
                    "plugins": { "llm": { "retries": 0 } }
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
            let compacted: i64 = eval(&h, "return COMPACTED").await;
            h.shutdown().await;
            assert_eq!(
                compacted, 1,
                "Anthropic stop_reason=model_context_window_exceeded must trigger overflow recovery: {result}"
            );
        }
    })
    .await
}

#[tokio::test]
async fn repro_267_compat_model_context_window_exceeded_classifies_as_context_overflow() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let overflow_sse = compat_events(&[
            compat_text("partial"),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"model_context_window_exceeded"}],"usage":{"prompt_tokens":500,"completion_tokens":10}}),
        ]);
        server.route("/compat", vec![Resp::sse(overflow_sse)]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;
        let msg = stream(&h, "deepseek/deepseek-chat").await;
        h.shutdown().await;
        assert_eq!(
            msg["error_kind"], "context_overflow",
            "Chat finish_reason=model_context_window_exceeded must classify as context_overflow, not malformed_response: {msg}"
        );
        assert_eq!(msg["stop_reason"], "error", "{msg}");
        assert_eq!(msg["raw_stop_reason"], "model_context_window_exceeded", "{msg}");
    })
    .await
}

#[tokio::test]
async fn compat_model_context_window_exceeded_routes_to_overflow_recovery_while_length_stays_length() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let overflow_sse = compat_events(&[
            compat_text("partial"),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"model_context_window_exceeded"}],"usage":{"prompt_tokens":500,"completion_tokens":10}}),
        ]);
        let ok_sse = compat_events(&[
            compat_text("recovered"),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":5}}),
        ]);
        let length_sse = compat_events(&[
            compat_text("truncated"),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}],"usage":{"prompt_tokens":100,"completion_tokens":4096}}),
        ]);
        server.route("/compat", vec![Resp::sse(overflow_sse), Resp::sse(ok_sse), Resp::sse(length_sse)]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "deepseek/deepseek-chat",
                "plugins": { "llm": { "retries": 0 } }
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
        let compacted: i64 = eval(&h, "return COMPACTED").await;
        assert_eq!(compacted, 1, "compat model_context_window_exceeded must trigger overflow recovery: {result}");

        // Ordinary finish_reason="length" still maps to stop_reason="length", not context_overflow.
        let len_msg = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(len_msg["stop_reason"], "length", "{len_msg}");
        assert!(len_msg["error_kind"].is_null(), "{len_msg}");
        h.shutdown().await;
    })
    .await
}

/// Google finish reasons: `IMAGE_SAFETY` / `IMAGE_PROHIBITED_CONTENT` -> `filtered`;
/// `MALFORMED_FUNCTION_CALL`, `UNEXPECTED_TOOL_CALL`, `TOO_MANY_TOOL_CALLS`, `OTHER`, `LANGUAGE`,
/// `NO_IMAGE`, `FINISH_REASON_UNSPECIFIED` -> `error` preserving `raw_stop_reason`, even with empty
/// content.
#[tokio::test]
async fn google_finish_reasons_image_safety_and_error_reasons() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let make_stream = |reason: &str| {
            format!(
                "data: {}\n\n",
                json!({
                    "candidates": [{"content": {"role": "model", "parts": []}, "finishReason": reason}],
                    "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 0}
                })
            )
        };

        let filtered_reasons = ["IMAGE_SAFETY", "IMAGE_PROHIBITED_CONTENT"];
        let error_reasons = [
            "MALFORMED_FUNCTION_CALL",
            "UNEXPECTED_TOOL_CALL",
            "TOO_MANY_TOOL_CALLS",
            "OTHER",
            "LANGUAGE",
            "NO_IMAGE",
            "FINISH_REASON_UNSPECIFIED",
        ];

        let mut resps = Vec::new();
        for r in filtered_reasons.iter().chain(error_reasons.iter()) {
            resps.push(Resp::sse(make_stream(r)));
        }
        server.route("/google", resps);

        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        for r in filtered_reasons {
            let msg = stream(&h, "google/gemini-2.5-pro").await;
            assert_eq!(msg["stop_reason"], "filtered", "{r}: {msg}");
            assert_eq!(msg["raw_stop_reason"], r, "{r}: {msg}");
        }

        for r in error_reasons {
            let msg = stream(&h, "google/gemini-2.5-pro").await;
            assert_eq!(msg["stop_reason"], "error", "{r}: {msg}");
            assert_eq!(msg["raw_stop_reason"], r, "{r}: {msg}");
            assert!(msg["error"].as_str().is_some_and(|e| e.contains(r)), "{r}: expected error naming reason in {msg}");
        }
        assert_eq!(server.count("/google"), filtered_reasons.len() + error_reasons.len());
        h.shutdown().await;
    })
    .await
}

/// OpenAI-compat `choices[0].usage` fallback and empty `choices: []` placeholder chunks.
#[tokio::test]
async fn compat_usage_choice_fallback_and_placeholder_chunks() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let hoist = provider_fixture("openai-compat/usage-hoist.sse");
        let placeholder = chunks_to_sse(&provider_fixture("openai-compat/placeholder-first-chunk.chunks.txt"), true);
        server.route("/compat", vec![Resp::sse(hoist), Resp::sse(placeholder)]);

        let h = provider_host(&mut env, &server, json!({})).await;

        let m1 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(m1["usage"]["input"], 5, "{m1}");
        assert_eq!(m1["usage"]["output"], 1, "{m1}");
        assert!(m1["usage"].get("reasoning").is_none_or(Value::is_null), "unreported reasoning must be null: {m1}");

        let m2 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(m2["content"][0]["text"], "Hello", "{m2}");
        assert_eq!(m2["usage"]["input"], 10, "{m2}");
        assert_eq!(m2["usage"]["output"], 1, "{m2}");
        h.shutdown().await;
    })
    .await
}

/// Usage edge cases:
/// - `usage: {}` must not overwrite previously captured usage with nil
/// - top-level `reasoning_tokens` fallback
/// - unreported `reasoning` remains null
/// - `provider_metadata` ignored, `reasoning_tokens: 16`
/// - Anthropic `message_delta.usage.input_tokens` (`61`)
/// - Anthropic `usage.iterations` input sum (`10 + 12 = 22`)
#[tokio::test]
async fn usage_edge_cases_empty_overwrite_reasoning_fallback_and_unknown() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let empty_overwrite = compat_events(&[
            compat_text("ok"),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":4,"reasoning_tokens":3}}),
            json!({"choices":[],"usage":{}}),
        ]);
        let gw_meta = provider_fixture("openai-compat/gateway-metadata-stream.sse");
        server.route("/compat", vec![Resp::sse(empty_overwrite)]);

        let uncounted = provider_fixture("anthropic/reasoning-uncounted.sse");
        let delta_input = chunks_to_sse(&provider_fixture("anthropic/message-delta-input-tokens.chunks.txt"), false);
        let fallback_mid = chunks_to_sse(&provider_fixture("anthropic/fallback-mid-output.chunks.txt"), false);
        server.route("/anthropic", vec![Resp::sse(uncounted), Resp::sse(delta_input), Resp::sse(fallback_mid)]);

        let google_no_thoughts = format!(
            "data: {}\n\n",
            json!({
                "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}, "finishReason": "STOP"}],
                "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2}
            })
        );
        server.route("/google", vec![Resp::sse(google_no_thoughts)]);

        let h = provider_host(&mut env, &server, json!({})).await;

        let c1 = stream(&h, "deepseek/deepseek-chat").await;
        assert_eq!(c1["usage"]["input"], 12, "usage:{{}} must not clobber prior usage: {c1}");
        assert_eq!(c1["usage"]["output"], 4, "{c1}");
        assert_eq!(c1["usage"]["reasoning"], 3, "top-level reasoning_tokens fallback: {c1}");

        let c2_lua = r#"
            local sse = ...
            local wire = require("provider-openai-compat.wire")
            local p = wire.new_parser(function() end)
            for line in sse:gmatch("[^\r\n]+") do
                local data = line:match("^data:%s*(.+)$")
                if data then p.feed({ data = data }) end
            end
            return p.finish_message()
        "#;
        let c2_val: mlua::Value = h.lua().load(c2_lua).call_async(gw_meta).await.unwrap();
        let c2 = mare_shared::lua::lua_to_json(h.lua(), c2_val).unwrap();
        assert_eq!(c2["stop_reason"], "length", "{c2}");
        assert_eq!(c2["usage"]["input"], 37, "{c2}");
        assert_eq!(c2["usage"]["output"], 16, "{c2}");
        assert_eq!(c2["usage"]["reasoning"], 16, "{c2}");

        let a1 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(a1["stop_reason"], "stop", "{a1}");
        assert_eq!(a1["usage"]["output"], 35, "{a1}");
        assert!(
            a1["usage"].get("reasoning").is_none_or(Value::is_null),
            "unreported reasoning must remain null, not 0: {a1}"
        );

        let a2 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(a2["usage"]["input"], 61, "message_delta input_tokens=61: {a2}");
        assert_eq!(a2["usage"]["output"], 2, "{a2}");

        let a3 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(a3["stop_reason"], "tool_use", "{a3}");
        assert_eq!(a3["usage"]["input"], 22, "iterations input sum 10+12: {a3}");
        assert_eq!(a3["usage"]["output"], 70, "{a3}");

        let g1 = stream(&h, "google/gemini-2.5-pro").await;
        assert!(
            g1["usage"].get("reasoning").is_none_or(Value::is_null),
            "google unreported thoughtsTokenCount must remain null: {g1}"
        );

        h.shutdown().await;
    })
    .await
}

/// Vertex OpenAI-compat `extra_content.google.thought` separates thinking from text,
/// and rotating `item_id`s in Responses SSE are merged by `output_index`.
#[tokio::test]
async fn vertex_thoughts_and_rotating_item_ids() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let vertex = provider_fixture("openai-compat/vertex-thoughts.sse");
        server.route("/compat", vec![Resp::sse(vertex)]);

        let rot_ws = provider_fixture("openai-responses/rotating-ids-web-search.sse");
        let rot_txt = chunks_to_sse(&provider_fixture("openai-responses/rotating-ids-text.chunks.txt"), true);
        server.route("/openai", vec![Resp::sse(rot_ws), Resp::sse(rot_txt)]);

        let h = provider_host(&mut env, &server, json!({})).await;

        let v = stream(&h, "deepseek/deepseek-chat").await;
        let v_content = v["content"].as_array().unwrap();
        assert_eq!(v_content[0]["type"], "reasoning", "{v}");
        assert!(v_content[0]["text"].as_str().unwrap().contains("Acknowledging Greeting"), "{v}");
        assert_eq!(v_content[1]["type"], "text", "{v}");
        assert_eq!(v_content[1]["text"], "你好！很高兴与你交流。请问有什么我可以帮你的吗？", "{v}");
        assert_eq!(v["usage"]["reasoning"], 219, "{v}");

        let o1 = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(o1["stop_reason"], "tool_use", "{o1}");
        let o1_events: Vec<&str> = o1["events"].as_array().unwrap().iter().filter_map(|e| e.as_str()).collect();
        assert!(o1_events.contains(&"tool_call_delta"), "rotating item_id must still emit tool_call_delta: {o1_events:?}");

        let o2 = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(o2["content"][1]["text"], "There are **3** letter **“r”**s in **“strawberry.”**\n\nBreakdown: **s t r a w b e r r y**  \nYou can see **r** at positions **3, 8, and 9**.", "{o2}");
        h.shutdown().await;
    })
    .await
}

/// Google Gemini 3 `partialArgs` + `willContinue` streaming function call assembly without a
/// terminal `functionCall` chunk.
#[tokio::test]
async fn google_partial_args_and_will_continue_assembles_nested_array_arguments() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body = chunks_to_sse(&provider_fixture("google/stream-tool-call-array-args.chunks.txt"), false);
        server.route("/google", vec![Resp::sse(body)]);

        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "google/gemini-2.5-pro").await;

        assert_eq!(msg["stop_reason"], "tool_use", "{msg}");
        let calls: Vec<&Value> =
            msg["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(calls.len(), 1, "{msg}");
        let call = calls[0];
        assert_eq!(call["name"], "writeItems", "{call}");
        assert!(
            call["native"]["google-generate"]["thought_signature"].as_str().is_some_and(|s| s.starts_with("AY89a19Zk")),
            "{call}"
        );
        assert!(call.get("parse_error").is_none_or(Value::is_null), "{call}");
        assert_eq!(
            call["arguments"],
            json!({
                "operations": [
                    { "action": "add", "description": "Fresh red apple", "itemid": "apple_001", "price": 0.5 },
                    { "action": "add", "description": "Ripe yellow banana", "itemid": "banana_001", "price": 0.3 }
                ]
            }),
            "{call}"
        );
        assert_eq!(msg["usage"]["input"], 54, "{msg}");
        assert_eq!(msg["usage"]["output"], 195, "{msg}");
        assert_eq!(msg["usage"]["reasoning"], 121, "{msg}");
        h.shutdown().await;
    })
    .await
}

/// Anthropic `message_delta.usage.iterations` cache_creation/cache_read counts are preserved
/// (not overwritten by `resolve_cache_write`) both when top-level `input_tokens` is absent
/// and when top-level `input_tokens` is present.
#[tokio::test]
async fn anthropic_iterations_cache_counts_preserved_with_and_without_top_level_input() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let make_stream = |delta_usage: Value| {
            [
                format!("data: {}\n\n", json!({"type":"message_start","message":{"id":"m1","role":"assistant","content":[],"usage":{"input_tokens":10,"output_tokens":0}}})),
                format!("data: {}\n\n", json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}})),
                format!("data: {}\n\n", json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}})),
                format!("data: {}\n\n", json!({"type":"content_block_stop","index":0})),
                format!("data: {}\n\n", json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":delta_usage})),
                format!("data: {}\n\n", json!({"type":"message_stop"})),
            ]
            .concat()
        };

        let no_top_input = make_stream(json!({
            "output_tokens": 20,
            "iterations": [
                {"type":"message","input_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":3,"output_tokens":8},
                {"type":"fallback_message","input_tokens":12,"cache_creation_input_tokens":7,"cache_read_input_tokens":4,"output_tokens":12}
            ]
        }));
        let with_top_input = make_stream(json!({
            "input_tokens": 50,
            "output_tokens": 15,
            "iterations": [
                {"type":"message","input_tokens":20,"cache_creation_input_tokens":9,"cache_read_input_tokens":6,"output_tokens":15}
            ]
        }));
        server.route("/anthropic", vec![Resp::sse(no_top_input), Resp::sse(with_top_input)]);

        let h = provider_host(&mut env, &server, json!({})).await;

        let m1 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(m1["usage"]["input"], 22, "{m1}");
        assert_eq!(m1["usage"]["cache_write"], 12, "{m1}");
        assert_eq!(m1["usage"]["cache_read"], 7, "{m1}");

        let m2 = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(m2["usage"]["input"], 50, "{m2}");
        assert_eq!(m2["usage"]["cache_write"], 9, "{m2}");
        assert_eq!(m2["usage"]["cache_read"], 6, "{m2}");
        h.shutdown().await;
    })
    .await
}

/// Counter-proof: Google `partialArgs` with `willContinue=true` at `STOP` or with a missing/invalid
/// `jsonPath` sets `parse_error` and NEVER executes the real tool in an agent turn.
#[tokio::test]
async fn google_partial_args_incomplete_or_invalid_json_path_never_executes_tool() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let marker1 = env.cwd.join("should_not_exist_1.txt");
        let marker2 = env.cwd.join("should_not_exist_2.txt");

        let incomplete_stream = [
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"write","willContinue":true}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.path","stringValue":marker1,"willContinue":false}],"willContinue":true}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.content","stringValue":"pwned","willContinue":true}],"willContinue":true}}]}}]})),
            // Another param completes (willContinue=false) and an empty terminal functionCall arrives,
            // while $.content still has willContinue=true.
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.extra","stringValue":"ok","willContinue":false}]}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}})),
        ]
        .concat();
        let done_1 = format!(
            "data: {}\n\n",
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"recovered 1"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2}})
        );

        let invalid_path_stream = [
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"write","willContinue":true}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.path","stringValue":marker2}],"willContinue":true}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.content","stringValue":"pwned"}],"willContinue":true}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"partialArgs":[{"jsonPath":"$.path..","stringValue":"bad"}]}}]}}]})),
            format!("data: {}\n\n", json!({"candidates":[{"content":{"role":"model","parts":[{"text":""}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5}})),
        ]
        .concat();
        let done_2 = format!(
            "data: {}\n\n",
            json!({"candidates":[{"content":{"role":"model","parts":[{"text":"recovered 2"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":20,"candidatesTokenCount":2}})
        );

        server.route(
            "/google",
            vec![
                Resp::sse(incomplete_stream),
                Resp::sse(done_1),
                Resp::sse(invalid_path_stream),
                Resp::sse(done_2),
            ],
        );

        let h = provider_host(&mut env, &server, json!({ "model": "google/gemini-2.5-pro" })).await;

        let res1 = h.prompt("write file 1", json!({})).await.unwrap();
        assert!(res1["error"].is_null(), "{res1}");
        assert!(!marker1.exists(), "incomplete partialArgs tool call must never execute");

        let res2 = h.prompt("write file 2", json!({})).await.unwrap();
        assert!(res2["error"].is_null(), "{res2}");
        assert!(!marker2.exists(), "invalid jsonPath partialArgs tool call must never execute");

        let reqs = server.requests();
        assert!(
            reqs[1].json().to_string().contains("incomplete streaming functionCall"),
            "second request must report incomplete error: {}",
            reqs[1].json()
        );
        assert!(
            reqs[3].json().to_string().contains("missing or invalid jsonPath"),
            "fourth request must report invalid jsonPath error: {}",
            reqs[3].json()
        );
        h.shutdown().await;
    })
    .await
}

/// Responses SSE: alternating `output_index` presence (`present -> absent`, `absent -> present`)
/// and rotating `item_id`s must resolve to the same text item without injecting extra newlines.
#[tokio::test]
async fn responses_output_index_present_absent_transitions_do_not_split_text() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;

        let run_case = |deltas: Vec<Value>, final_text: &str| {
            let mut sse = responses_event(json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"id": "msg_1", "type": "message", "role": "assistant", "content": []}
            }));
            for d in deltas {
                sse.push_str(&responses_event(d));
            }
            sse.push_str(&responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {"id": "msg_1", "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": final_text}]}
            })));
            sse.push_str(&responses_event(json!({
                "type": "response.completed",
                "response": {"usage": {"input_tokens": 5, "output_tokens": 3}}
            })));
            sse
        };

        // 1) output_index present -> absent
        let s1 = run_case(
            vec![
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "line1\n"}),
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "content_index": 0, "delta": "line2"}),
            ],
            "line1\nline2",
        );
        // 2) output_index absent -> present
        let s2 = run_case(
            vec![
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "content_index": 0, "delta": "lineA\n"}),
                json!({"type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "lineB"}),
            ],
            "lineA\nlineB",
        );
        // 3) rotating item_id with output_index present
        let s3 = run_case(
            vec![
                json!({"type": "response.output_text.delta", "item_id": "rot_a", "output_index": 0, "content_index": 0, "delta": "hello\n"}),
                json!({"type": "response.output_text.delta", "item_id": "rot_b", "output_index": 0, "content_index": 0, "delta": "world"}),
            ],
            "hello\nworld",
        );

        let lua_check = r#"
            local bodies = ...
            local wire = require("provider-openai.wire")
            local results = {}
            for _, sse in ipairs(bodies) do
                local text_deltas = {}
                local p = wire.new_parser(function(ev)
                    if ev.type == "text_delta" then
                        text_deltas[#text_deltas + 1] = ev.delta
                    end
                end)
                for block in sse:gmatch("data:%s*(.-)\n\n") do
                    p.feed({ data = block })
                end
                p.finish()
                results[#results + 1] = table.concat(text_deltas)
            end
            return results
        "#;
        let res_val: mlua::Value = h.lua().load(lua_check).call_async(vec![s1, s2, s3]).await.unwrap();
        let res = mare_shared::lua::lua_to_json(h.lua(), res_val).unwrap();
        assert_eq!(res, json!(["line1\nline2", "lineA\nlineB", "hello\nworld"]));
        h.shutdown().await;
    })
    .await
}
