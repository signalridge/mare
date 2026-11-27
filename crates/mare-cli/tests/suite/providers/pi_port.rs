//! ScriptedModel port (pi): error classification & retry, overflow & non-overflow exclusions,
//! Anthropic SSE parsing edge cases, tool-argument streaming & malformed final JSON cleanup,
//! and cross-provider handoff / tool-turn integrity ported from `reference implementation/packages/ai/test/`
//! onto `ScriptedModel`.
//!
//! Each test exercises a real mare public path (`Host::prompt` or `LLM:stream` -> provider
//! plugin -> `ScriptedModel`) and asserts outcomes (`text`, `termination_reason`, `error_kind`,
//! `llm/retry`, `turns()`, tool execution counts), not raw SSE frames.

use super::*;
use crate::common::scripted::{Part, ScriptedModel, Step, Wire};

struct Outcome {
    res: Value,
    retries: Vec<(String, i64)>,
    requests: usize,
    bodies: Vec<Value>,
}

async fn run_wire(wire: Wire, steps: Vec<Step>, llm_cfg: Value, setup_lua: Option<&str>) -> Outcome {
    let mut env = TestEnv::new().await;
    let mock = ScriptedModel::start(wire, steps).await;
    for (k, v) in [
        ("OPENAI_API_KEY", "test-openai"),
        ("ANTHROPIC_API_KEY", "test-anthropic"),
        ("DEEPSEEK_API_KEY", "test-deepseek"),
        ("GEMINI_API_KEY", "test-google"),
    ] {
        env.set_var(k, v);
    }
    let mut cfg = mock.host_config();
    merge(&mut cfg, json!({ "plugins": { "llm": llm_cfg } }));
    let h = env.host().builtins(&["compact", "usage"]).config(cfg).build().await;
    expose(&h);
    exec(
        &h,
        r#"RETRY = {}
           C.cordis:on("step/completed", function(e)
               if e.will_retry and e.delay_ms ~= nil then
                   RETRY[#RETRY + 1] = { tostring(e.error_kind), math.floor(e.delay_ms or -1) }
               end
           end)"#,
    )
    .await;
    if let Some(lua) = setup_lua {
        exec(&h, lua).await;
    }
    let res = h.prompt("go", json!({})).await.unwrap();
    let retries: Vec<(String, i64)> = eval::<Vec<Vec<mlua::Value>>>(&h, "return RETRY")
        .await
        .into_iter()
        .map(|r| (r[0].to_string().unwrap_or_default(), r[1].as_i64().unwrap_or(-1)))
        .collect();
    h.shutdown().await;
    let bodies: Vec<Value> = mock.requests().into_iter().map(|r| r.json()).collect();
    let requests = bodies.len();
    mock.finish();
    Outcome { res, retries, requests, bodies }
}

fn recovered() -> Step {
    Step::Reply(vec![Part::Text("recovered".into())])
}

fn ev(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn responses_failed(error: Value) -> Step {
    Step::Raw(Resp::sse(
        ev(
            "response.created",
            json!({ "type": "response.created", "response": { "id": "resp_f", "status": "in_progress" } }),
        ) + &ev(
            "response.failed",
            json!({ "type": "response.failed", "response": { "id": "resp_f", "status": "failed", "error": error } }),
        ),
    ))
}

// ---- Priority 1: Error classification, retry, & overflow -------------------------------------

/// pi `packages/ai/test/overflow.test.ts:87-114`: LiteLLM-wrapped OpenAI, LiteLLM pre-call
/// `ContextWindowExceededError`, OpenAI-compat parenthesized max context length, and OpenRouter
/// Poolside input-length errors all classify as `context_overflow`, trigger compaction, and retry.
#[tokio::test]
async fn pi_overflow_litellm_openrouter_and_parenthesized_trigger_compaction() {
    local(async {
        let cases = [
            (
                "litellm_wrapped_openai",
                503,
                "Error: 503 litellm.ServiceUnavailableError: litellm.MidStreamFallbackError: litellm.APIConnectionError: APIConnectionError: OpenAIException - Requested token count exceeds the model's maximum context length of 131072 tokens.",
            ),
            (
                "litellm_pre_call",
                400,
                "400: {\"message\":\"litellm.ContextWindowExceededError: litellm.BadRequestError: litellm._pre_call_checks: Context Window exceeded for given call. Max Input Tokens=262144, Got=265262\"}",
            ),
            (
                "compat_parenthesized",
                400,
                "Error: 400 Input length (265330) exceeds model's maximum context length (262144).",
            ),
            (
                "openrouter_poolside",
                400,
                "Provider returned error: Input length 131393 exceeds the maximum allowed input length of 131040 tokens.",
            ),
        ];
        for (name, status, msg) in cases {
            let o = run_wire(
                Wire::Compat,
                vec![
                    Step::Raw(Resp::status(status, json!({ "error": { "message": msg } }).to_string())),
                    recovered(),
                ],
                json!({ "retry_base_ms": 1, "retries": 1 }),
                Some(
                    r#"COMPACTED = 0
                       C.cordis:on("compact/overflow", function() COMPACTED = COMPACTED + 1 return { handled = true, succeeded = true, res = { status = "compacted", tokens_before = 0, tokens_after = 0 } } end, { prepend = true })"#,
                ),
            )
            .await;
            assert_eq!(o.res["text"], "recovered", "{name}: {}", o.res);
            assert_eq!(o.requests, 2, "{name}: overflow should compact and resend once");
            assert!(o.retries.is_empty(), "{name}: overflow must not count as transient retry: {:?}", o.retries);
        }
    })
    .await
}

/// pi `packages/ai/test/overflow.test.ts:148-151` & `packages/ai/test/retry.test.ts:95-96`:
/// Bedrock `Service unavailable: The service is temporarily unavailable.` is NOT context overflow;
/// it is retried as a transient transport/server error and recovers without compaction.
#[tokio::test]
async fn pi_bedrock_service_unavailable_is_retried_not_overflow() {
    local(async {
        let o = run_wire(
            Wire::Responses,
            vec![
                responses_failed(json!({ "message": "Service unavailable: The service is temporarily unavailable." })),
                recovered(),
            ],
            json!({ "retry_base_ms": 1, "retries": 1 }),
            Some(
                r#"COMPACTED = 0
                   C.cordis:on("compact/overflow", function() COMPACTED = COMPACTED + 1 return { handled = true, succeeded = true, res = { status = "compacted", tokens_before = 0, tokens_after = 0 } } end, { prepend = true })"#,
            ),
        )
        .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
    })
    .await
}

/// pi `packages/ai/test/retry.test.ts:154-165`: ChatGPT subscription and Business workspace usage
/// limit errors on HTTP 429 are non-retryable `quota` errors (never retried despite 429).
#[tokio::test]
async fn pi_chatgpt_subscription_usage_limits_are_non_retryable() {
    local(async {
        for code in [
            "subscription_sharing_usage_limit_exceeded",
            "subscription_sharing_workplace_individual_usage_limit_exceeded",
        ] {
            let body = json!({ "error": { "code": code, "message": "Usage limit reached." } });
            let o = run_wire(
                Wire::Responses,
                vec![Step::Raw(Resp::status(429, body.to_string()))],
                json!({ "retry_base_ms": 1, "retries": 2 }),
                None,
            )
            .await;
            assert_eq!(o.res["termination_reason"], "usage_limit", "{code}: {}", o.res);
            assert_eq!(o.res["error_kind"], "quota", "{code}: {}", o.res);
            assert_eq!(o.requests, 1, "{code}: must not retry subscription limit");
            assert!(o.retries.is_empty(), "{code}: {:?}", o.retries);
        }
    })
    .await
}

/// pi `packages/ai/test/retry.test.ts:16,139-144`: Azure peak-load capacity errors without an
/// HTTP status code are retryable.
#[tokio::test]
async fn pi_azure_peak_load_capacity_error_retries_as_overloaded() {
    local(async {
        let msg = "The system is currently experiencing high demand and cannot process your request. Your request exceeds the maximum usage size allowed during peak load. For improved capacity reliability, consider switching to Provisioned Throughput.";
        let o = run_wire(
            Wire::Responses,
            vec![responses_failed(json!({ "message": msg })), recovered()],
            json!({ "retry_base_ms": 1, "retries": 1 }),
            None,
        )
        .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
    })
    .await
}

/// pi `packages/ai/test/retry.test.ts:125-129`: gateway `{"type":"error","error":{"type":"api_error","message":"Connection closed."}}`
/// (or `response.failed` with `"Connection closed."` and no code) is retryable.
#[tokio::test]
async fn pi_retry_gateway_connection_closed_without_code() {
    local(async {
        let o = run_wire(
            Wire::Responses,
            vec![responses_failed(json!({ "message": "Connection closed." })), recovered()],
            json!({ "retry_base_ms": 1, "retries": 1 }),
            None,
        )
        .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
    })
    .await
}

/// pi `packages/ai/test/retry.test.ts:7-8,19-35`: explicit provider retry guidance
/// (`"The system encountered an unexpected error during processing. Try your request again."`)
/// without an HTTP status or error code is retryable.
#[tokio::test]
async fn pi_retry_bedrock_explicit_retry_guidance_without_status() {
    local(async {
        let o = run_wire(
            Wire::Responses,
            vec![
                responses_failed(json!({
                    "message": "{\"message\":\"The system encountered an unexpected error during processing. Try your request again.\"}"
                })),
                recovered(),
            ],
            json!({ "retry_base_ms": 1, "retries": 1 }),
            None,
        )
        .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
    })
    .await
}

// ---- Priority 2: Anthropic SSE parsing, partial JSON, & streaming tool arguments --------------

/// pi `packages/ai/test/anthropic-sse-parsing.test.ts:481-577`: `content_block_start` initial
/// `text`, `thinking`, and `signature` are preserved and concatenated with subsequent deltas,
/// and the combined thinking signature is replayed on the next turn.
#[tokio::test]
async fn pi_anthropic_preserves_initial_content_from_content_block_start() {
    local(async {
        let sse = [
            ev(
                "message_start",
                json!({ "type": "message_start", "message": { "id": "msg_init", "usage": { "input_tokens": 12, "output_tokens": 0 } } }),
            ),
            ev(
                "content_block_start",
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "Initial thinking", "signature": "initial signature" } }),
            ),
            ev(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": " plus delta" } }),
            ),
            ev(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": " plus delta" } }),
            ),
            ev("content_block_stop", json!({ "type": "content_block_stop", "index": 0 })),
            ev(
                "content_block_start",
                json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "text", "text": "Initial text" } }),
            ),
            ev(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "text_delta", "text": " plus delta" } }),
            ),
            ev("content_block_stop", json!({ "type": "content_block_stop", "index": 1 })),
            ev(
                "content_block_start",
                json!({ "type": "content_block_start", "index": 2, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "ping", "input": {} } }),
            ),
            ev("content_block_stop", json!({ "type": "content_block_stop", "index": 2 })),
            ev(
                "message_delta",
                json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 5 } }),
            ),
            ev("message_stop", json!({ "type": "message_stop" })),
        ]
        .concat();
        let o = run_wire(
            Wire::Anthropic,
            vec![Step::Raw(Resp::sse(sse)), Step::Reply(vec![Part::Text("done".into())])],
            json!({}),
            Some(
                r#"T:register("ping", {
                       description = "ping",
                       parameters = { type = "object" },
                       execute = function() return "pong" end,
                   })"#,
            ),
        )
        .await;
        assert_eq!(o.res["text"], "done", "{}", o.res);
        assert_eq!(o.bodies.len(), 2);
        let turn2_body = o.bodies[1].to_string();
        assert!(turn2_body.contains("Initial text plus delta"), "missing initial+delta text: {turn2_body}");
        assert!(turn2_body.contains("Initial thinking plus delta"), "missing initial+delta thinking: {turn2_body}");
        assert!(turn2_body.contains("initial signature plus delta"), "missing initial+delta signature: {turn2_body}");
    })
    .await
}

/// pi `packages/ai/test/anthropic-sse-parsing.test.ts:716-735`: unknown SSE events after
/// `message_stop` (such as `event: done\ndata: [DONE]` and `event: proxy.stats\ndata: not json`)
/// are ignored once `message_stop` terminates the stream.
#[tokio::test]
async fn pi_anthropic_ignores_unknown_sse_events_after_message_stop() {
    local(async {
        let sse = [
            ev(
                "message_start",
                json!({ "type": "message_start", "message": { "id": "msg_trail", "usage": { "input_tokens": 10, "output_tokens": 0 } } }),
            ),
            ev(
                "content_block_start",
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
            ),
            ev(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } }),
            ),
            ev("content_block_stop", json!({ "type": "content_block_stop", "index": 0 })),
            ev(
                "message_delta",
                json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 2 } }),
            ),
            ev("message_stop", json!({ "type": "message_stop" })),
            "event: done\ndata: [DONE]\n\n".to_string(),
            "event: proxy.stats\ndata: not json\n\n".to_string(),
        ]
        .concat();
        let o = run_wire(Wire::Anthropic, vec![Step::Raw(Resp::sse(sse))], json!({}), None).await;
        assert_eq!(o.res["text"], "Hello", "{}", o.res);
        assert_eq!(o.res["termination_reason"], "completed", "{}", o.res);
        assert_eq!(o.res["messages"][1]["stop_reason"], "stop", "{}", o.res);
        assert_eq!(o.requests, 1);
    })
    .await
}

/// pi `packages/ai/test/anthropic-sse-parsing.test.ts:579-636`: `message_delta` with
/// `stop_reason: "refusal"` and `stop_details.explanation` preserves the explanation in the
/// error message rather than replacing it with a generic string.
#[tokio::test]
async fn pi_anthropic_refusal_preserves_stop_details_explanation() {
    local(async {
        let explanation = "This request triggered restrictions on violative cyber content and was blocked.";
        let sse = [
            ev(
                "message_start",
                json!({ "type": "message_start", "message": { "id": "msg_refusal", "usage": { "input_tokens": 10, "output_tokens": 0 } } }),
            ),
            ev(
                "message_delta",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": "refusal",
                        "stop_details": { "type": "refusal", "category": "cyber", "explanation": explanation }
                    },
                    "usage": { "output_tokens": 0 }
                }),
            ),
            ev("message_stop", json!({ "type": "message_stop" })),
        ]
        .concat();
        let o = run_wire(Wire::Anthropic, vec![Step::Raw(Resp::sse(sse))], json!({}), None).await;
        assert_eq!(o.res["messages"][1]["stop_reason"], "filtered", "{}", o.res);
        assert!(
            o.res["messages"][1]["error"].as_str().is_some_and(|e| e.contains(explanation)),
            "expected stop_details.explanation in error: {}",
            o.res
        );
    })
    .await
}

/// pi `packages/ai/test/openai-responses-partial-json-cleanup.test.ts:108-131`: when streamed
/// `function_call_arguments.delta` forms a valid JSON prefix, but `output_item.done` carries
/// malformed final arguments (`{"path":"README.md","content":"updated","mode":.5}`), the tool
/// must NOT execute with the parsed prefix; instead a parse error is reported to the model.
#[tokio::test]
async fn pi_responses_malformed_final_tool_arguments_discards_prefix_and_reports_error() {
    local(async {
        let bad_args = r#"{"path":"README.md","content":"updated","mode":.5}"#;
        let sse = [
            ev("response.created", json!({ "type": "response.created", "response": { "id": "resp_1", "status": "in_progress" } })),
            ev(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": { "type": "function_call", "id": "fc_test", "call_id": "call_test", "name": "edit_doc", "arguments": "" }
                }),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_test", "output_index": 0, "delta": r#"{"path":"README.md""# }),
            ),
            ev(
                "response.function_call_arguments.delta",
                json!({ "type": "response.function_call_arguments.delta", "item_id": "fc_test", "output_index": 0, "delta": r#","content":"updated"}"# }),
            ),
            ev(
                "response.function_call_arguments.done",
                json!({ "type": "response.function_call_arguments.done", "item_id": "fc_test", "output_index": 0, "arguments": bad_args }),
            ),
            ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": { "type": "function_call", "id": "fc_test", "call_id": "call_test", "name": "edit_doc", "arguments": bad_args }
                }),
            ),
            ev("response.completed", json!({ "type": "response.completed", "response": { "id": "resp_1", "status": "completed", "usage": {} } })),
        ]
        .concat();
        let o = run_wire(
            Wire::Responses,
            vec![Step::Raw(Resp::sse(sse)), Step::Reply(vec![Part::Text("recovered".into())])],
            json!({}),
            Some(
                r#"RAN = 0
                   T:register("edit_doc", {
                       description = "edit",
                       parameters = { type = "object", properties = { path = { type = "string" }, content = { type = "string" } } },
                       execute = function() RAN = RAN + 1 return "edited" end,
                   })"#,
            ),
        )
        .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.bodies.len(), 2);
        let turn2_body = o.bodies[1].to_string();
        assert!(turn2_body.contains("function_call_output"), "expected tool error output on turn 2: {turn2_body}");
        assert!(!turn2_body.contains("\"edited\""), "tool must not execute with prefix: {turn2_body}");
    })
    .await
}

/// pi `packages/ai/test/streaming-tool-arguments.test.ts:17-55,196-300`: large tool-call JSON
/// arguments with escaped quotes, backslashes, and newlines split across 60+ small deltas
/// assemble cleanly without truncation or corruption.
#[tokio::test]
async fn pi_streaming_tool_arguments_large_escaped_payload_across_deltas() {
    local(async {
        use std::fmt::Write as _;
        let mut content = String::new();
        for i in 0..80 {
            let _ = writeln!(content, "const line{i} = \"value {i}\"; // \\ done");
        }
        let full_args = json!({ "path": "/src/file.ts", "content": content }).to_string();
        let mut sse = ev(
            "message_start",
            json!({ "type": "message_start", "message": { "id": "msg_big", "usage": { "input_tokens": 10, "output_tokens": 0 } } }),
        );
        sse.push_str(&ev(
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_big", "name": "write_doc", "input": {} } }),
        ));
        for chunk in full_args.as_bytes().chunks(64) {
            let delta = std::str::from_utf8(chunk).unwrap();
            sse.push_str(&ev(
                "content_block_delta",
                json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": delta } }),
            ));
        }
        sse.push_str(&ev("content_block_stop", json!({ "type": "content_block_stop", "index": 0 })));
        sse.push_str(&ev(
            "message_delta",
            json!({ "type": "message_delta", "delta": { "stop_reason": "tool_use" }, "usage": { "output_tokens": 20 } }),
        ));
        sse.push_str(&ev("message_stop", json!({ "type": "message_stop" })));

        let o = run_wire(
            Wire::Anthropic,
            vec![Step::Raw(Resp::sse(sse)), Step::Reply(vec![Part::Text("written".into())])],
            json!({}),
            Some(
                r#"T:register("write_doc", {
                       description = "write",
                       parameters = { type = "object", properties = { path = { type = "string" }, content = { type = "string" } } },
                       execute = function(args) return args.path .. ":" .. tostring(#args.content) end,
                   })"#,
            ),
        )
        .await;
        assert_eq!(o.res["text"], "written", "{}", o.res);
        assert_eq!(o.bodies.len(), 2);
        let expected_len = content.len();
        let turn2_body = o.bodies[1].to_string();
        assert!(
            turn2_body.contains(&format!("/src/file.ts:{expected_len}")),
            "expected exact assembled payload length {expected_len} in turn 2: {turn2_body}"
        );
    })
    .await
}

// ---- Priority 3: Cross-provider handoff & tool-turn integrity --------------------------------

/// pi `packages/ai/test/anthropic-empty-thinking-signature-compat.test.ts:82-98` &
/// `packages/ai/test/cross-provider-handoff.test.ts:1-23`: unsigned / empty-signature thinking
/// blocks from another provider must be dropped before sending to Anthropic (Anthropic rejects
/// `thinking` blocks with empty signatures).
#[tokio::test]
async fn pi_anthropic_empty_thinking_signature_dropped_on_cross_provider_handoff() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ANTHROPIC_API_KEY", "test-anthropic");
        let mock = ScriptedModel::start(Wire::Anthropic, vec![Step::Reply(vec![Part::Text("ok".into())])]).await;
        let h = mock.host(&mut env, false, &[]).await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let res = eval::<mlua::Value>(
            &h,
            r#"
            local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local history = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                {
                    role = "assistant",
                    provider_id = "openai",
                    content = {
                        { type = "reasoning", text = "unsigned cross-provider thought" },
                        { type = "text", text = "visible answer" },
                    },
                },
                { role = "user", content = { { type = "text", text = "continue" } } },
            }
            return LLM:stream({ model = m, messages = history }, function() end)
            "#,
        )
        .await;
        let msg = mare_shared::lua::lua_to_json(h.lua(), res).unwrap();
        assert_eq!(msg["stop_reason"], "stop", "{msg}");
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        let body_json = reqs[0].json();
        let assistant = body_json["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        let blocks = assistant["content"].as_array().unwrap();
        assert!(
            blocks.iter().all(|b| b["type"] != "thinking"),
            "unsigned thinking must not be sent as thinking block: {assistant}"
        );
        assert!(
            blocks.iter().any(|b| b["type"] == "text" && b["text"] == "unsigned cross-provider thought"),
            "unsigned thinking should be converted to text: {assistant}"
        );
        assert!(
            blocks.iter().any(|b| b["type"] == "text" && b["text"] == "visible answer"),
            "assistant text must remain: {assistant}"
        );
        h.shutdown().await;
        mock.finish();
    })
    .await
}

/// pi `packages/ai/test/tool-call-without-result.test.ts:32-91`: an assistant `tool_call` in
/// history without a corresponding `tool_result` (e.g., aborted tool round) is repaired with a
/// synthetic error `tool_result` before the next request so the provider accepts it.
#[tokio::test]
async fn pi_tool_call_without_result_is_repaired_before_next_turn() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let mock = ScriptedModel::start(Wire::Responses, vec![Step::Reply(vec![Part::Text("4".into())])]).await;
        let _path = env.write_session(
            "orphan-call.jsonl",
            &[
                json!({
                    "type": "message",
                    "entry_id": "m1",
                    "parent_entry_id": Value::Null,
                    "created_at_ms": 1000,
                    "message": { "role": "user", "content": [{ "type": "text", "text": "calc 25 * 18" }] }
                }),
                json!({
                    "type": "message",
                    "entry_id": "m2",
                    "parent_entry_id": "m1",
                    "created_at_ms": 1001,
                    "message": {
                        "role": "assistant",
                        "content": [{
                            "type": "tool_call",
                            "id": "call_orphan_calc",
                            "name": "read",
                            "arguments": { "path": "a.txt" }
                        }]
                    }
                }),
            ],
        );
        let h = mock.host(&mut env, true, &[]).await;
        let res = h.prompt("Never mind, what is 2+2?", json!({})).await.unwrap();
        assert_eq!(res["text"], "4", "{res}");
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 1);
        let body = reqs[0].json().to_string();
        assert!(body.contains("call_orphan_calc"), "original tool call must remain: {body}");
        assert!(body.contains("function_call_output"), "missing tool_result must be synthesized: {body}");
        let turns = mock.turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].calls.len(), 1);
        assert_eq!(turns[0].results.len(), 1);
        assert_eq!(turns[0].results[0].0, "call_orphan_calc");
        assert!(
            turns[0].results[0].1.contains("interrupted:"),
            "expected agent/orphans settlement text, got: {:?}",
            turns[0].results[0].1
        );
        h.shutdown().await;
        mock.finish();
    })
    .await
}
