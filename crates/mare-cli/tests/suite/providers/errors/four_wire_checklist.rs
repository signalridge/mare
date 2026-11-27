//! Explicit 4-wire (`openai-responses`, `openai-chat`, `anthropic-messages`, `google-generate`)
//! × mandatory-case checklist for coded provider errors, preserved vendor metadata, retry
//! exhaustion, and caller-facing terminal assistant messages.

use std::collections::BTreeSet;

use super::*;

#[derive(Clone, Copy)]
struct WireTarget {
    wire: &'static str,
    expected_route: &'static str,
    model_ref: &'static str,
    route: &'static str,
    provider_label: &'static str,
}

const WIRES: [WireTarget; 4] = [
    WireTarget {
        wire: "openai-responses",
        expected_route: "responses",
        model_ref: "openai/gpt-6.1-terra",
        route: "/openai",
        provider_label: "OpenAI",
    },
    WireTarget {
        wire: "openai-chat",
        expected_route: "chat",
        model_ref: "deepseek/deepseek-chat",
        route: "/compat",
        provider_label: "deepseek",
    },
    WireTarget {
        wire: "anthropic-messages",
        expected_route: "anthropic",
        model_ref: "anthropic/claude-sonnet-5-5",
        route: "/anthropic",
        provider_label: "Anthropic",
    },
    WireTarget {
        wire: "google-generate",
        expected_route: "google",
        model_ref: "google/gemini-2.5-pro",
        route: "/google",
        provider_label: "Google",
    },
];

const MANDATORY_CASES: [&str; 9] = [
    "auth",
    "rate_limit",
    "quota",
    "overloaded",
    "server",
    "context_overflow",
    "invalid",
    "malformed_response",
    "stream_partial_error",
];

fn make_case_responses(
    wire: &str,
    case: &str,
) -> (Vec<Resp>, &'static str, Option<&'static str>, Option<&'static str>) {
    match (wire, case) {
        // ---- auth (401, non-retryable, 1 attempt) ----
        ("openai-responses" | "openai-chat", "auth") => (
            vec![Resp::status(
                401,
                r#"{"error":{"message":"invalid api key","type":"authentication_error","code":"invalid_api_key"}}"#,
            )],
            "auth",
            Some("invalid_api_key"),
            Some("authentication_error"),
        ),
        ("anthropic-messages", "auth") => (
            vec![Resp::status(
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            )],
            "auth",
            None,
            Some("authentication_error"),
        ),
        ("google-generate", "auth") => (
            vec![Resp::status(
                403,
                r#"{"error":{"code":403,"message":"API key invalid","status":"PERMISSION_DENIED"}}"#,
            )],
            "auth",
            Some("403"),
            Some("PERMISSION_DENIED"),
        ),

        // ---- rate_limit (429, retried 1+1=2 attempts, then terminal) ----
        ("openai-responses" | "openai-chat", "rate_limit") => (
            vec![
                Resp::status(
                    429,
                    r#"{"error":{"message":"rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#,
                )
                .header("retry-after", "0"),
                Resp::status(
                    429,
                    r#"{"error":{"message":"rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#,
                )
                .header("retry-after", "0"),
            ],
            "rate_limit",
            Some("rate_limit_exceeded"),
            Some("requests"),
        ),
        ("anthropic-messages", "rate_limit") => (
            vec![
                Resp::status(
                    429,
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"too many requests"}}"#,
                )
                .header("retry-after", "0"),
                Resp::status(
                    429,
                    r#"{"type":"error","error":{"type":"rate_limit_error","message":"too many requests"}}"#,
                )
                .header("retry-after", "0"),
            ],
            "rate_limit",
            None,
            Some("rate_limit_error"),
        ),
        ("google-generate", "rate_limit") => (
            vec![
                Resp::status(
                    429,
                    r#"{"error":{"code":429,"message":"rate limit exceeded","status":"RESOURCE_EXHAUSTED"}}"#,
                )
                .header("retry-after", "0"),
                Resp::status(
                    429,
                    r#"{"error":{"code":429,"message":"rate limit exceeded","status":"RESOURCE_EXHAUSTED"}}"#,
                )
                .header("retry-after", "0"),
            ],
            "rate_limit",
            Some("429"),
            Some("RESOURCE_EXHAUSTED"),
        ),

        // ---- quota (billing exhausted, non-retryable, 1 attempt) ----
        ("openai-responses", "quota") => (
            vec![Resp::status(
                429,
                r#"{"error":{"message":"subscription sharing usage limit exceeded","type":"usage_limit","code":"subscription_sharing_usage_limit_exceeded"}}"#,
            )],
            "quota",
            Some("subscription_sharing_usage_limit_exceeded"),
            Some("usage_limit"),
        ),
        ("openai-chat", "quota") => (
            vec![Resp::status(
                429,
                r#"{"error":{"message":"insufficient quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
            )],
            "quota",
            Some("insufficient_quota"),
            Some("insufficient_quota"),
        ),
        ("anthropic-messages", "quota") => (
            vec![Resp::status(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API."}}"#,
            )],
            "quota",
            None,
            Some("invalid_request_error"),
        ),
        ("google-generate", "quota") => (
            vec![Resp::status(
                429,
                r#"{"error":{"code":429,"message":"You exceeded your current quota, please check your plan and billing details.","status":"RESOURCE_EXHAUSTED"}}"#,
            )],
            "quota",
            Some("429"),
            Some("RESOURCE_EXHAUSTED"),
        ),

        // ---- overloaded (capacity, retried, then terminal) ----
        ("openai-responses" | "openai-chat", "overloaded") => (
            vec![
                Resp::status(
                    503,
                    r#"{"error":{"message":"engine overloaded","type":"server_error","code":"server_is_overloaded"}}"#,
                )
                .header("retry-after", "0"),
                Resp::status(
                    503,
                    r#"{"error":{"message":"engine overloaded","type":"server_error","code":"server_is_overloaded"}}"#,
                )
                .header("retry-after", "0"),
            ],
            "overloaded",
            Some("server_is_overloaded"),
            Some("server_error"),
        ),
        ("anthropic-messages", "overloaded") => (
            vec![
                Resp::status(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
                    .header("retry-after", "0"),
                Resp::status(529, r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#)
                    .header("retry-after", "0"),
            ],
            "overloaded",
            None,
            Some("overloaded_error"),
        ),
        ("google-generate", "overloaded") => (
            vec![
                Resp::status(
                    529,
                    r#"{"error":{"code":529,"message":"The model is overloaded. Please try again later.","status":"UNAVAILABLE"}}"#,
                )
                .header("retry-after", "0"),
                Resp::status(
                    529,
                    r#"{"error":{"code":529,"message":"The model is overloaded. Please try again later.","status":"UNAVAILABLE"}}"#,
                )
                .header("retry-after", "0"),
            ],
            "overloaded",
            Some("529"),
            Some("UNAVAILABLE"),
        ),

        // ---- server (500, retried, then terminal) ----
        ("openai-responses" | "openai-chat", "server") => (
            vec![
                Resp::status(500, r#"{"error":{"message":"internal failure","type":"server_error","code":"internal_error"}}"#)
                    .header("retry-after", "0"),
                Resp::status(500, r#"{"error":{"message":"internal failure","type":"server_error","code":"internal_error"}}"#)
                    .header("retry-after", "0"),
            ],
            "server",
            Some("internal_error"),
            Some("server_error"),
        ),
        ("anthropic-messages", "server") => (
            vec![
                Resp::status(500, r#"{"type":"error","error":{"type":"api_error","message":"internal server error"}}"#)
                    .header("retry-after", "0"),
                Resp::status(500, r#"{"type":"error","error":{"type":"api_error","message":"internal server error"}}"#)
                    .header("retry-after", "0"),
            ],
            "server",
            None,
            Some("api_error"),
        ),
        ("google-generate", "server") => (
            vec![
                Resp::status(
                    500,
                    r#"{"error":{"code":500,"message":"Internal error encountered.","status":"INTERNAL"}}"#,
                )
                .header("retry-after", "0"),
                Resp::status(
                    500,
                    r#"{"error":{"code":500,"message":"Internal error encountered.","status":"INTERNAL"}}"#,
                )
                .header("retry-after", "0"),
            ],
            "server",
            Some("500"),
            Some("INTERNAL"),
        ),

        // ---- context_overflow (non-retryable, 1 attempt) ----
        ("openai-responses" | "openai-chat", "context_overflow") => (
            vec![Resp::status(
                400,
                r#"{"error":{"message":"This model's maximum context length is 128000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
            )],
            "context_overflow",
            Some("context_length_exceeded"),
            Some("invalid_request_error"),
        ),
        ("anthropic-messages", "context_overflow") => (
            vec![Resp::status(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
            )],
            "context_overflow",
            None,
            Some("invalid_request_error"),
        ),
        ("google-generate", "context_overflow") => (
            vec![Resp::status(
                400,
                r#"{"error":{"code":400,"message":"The input token count exceeds the maximum number of tokens allowed 1048576.","status":"INVALID_ARGUMENT"}}"#,
            )],
            "context_overflow",
            Some("400"),
            Some("INVALID_ARGUMENT"),
        ),

        // ---- invalid (400, non-retryable, 1 attempt) ----
        ("openai-responses" | "openai-chat", "invalid") => (
            vec![Resp::status(
                400,
                r#"{"error":{"message":"invalid parameter value","type":"invalid_request_error","code":"invalid_value"}}"#,
            )],
            "invalid",
            Some("invalid_value"),
            Some("invalid_request_error"),
        ),
        ("anthropic-messages", "invalid") => (
            vec![Resp::status(
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"invalid schema field"}}"#,
            )],
            "invalid",
            None,
            Some("invalid_request_error"),
        ),
        ("google-generate", "invalid") => (
            vec![Resp::status(
                400,
                r#"{"error":{"code":400,"message":"Invalid JSON payload received.","status":"INVALID_ARGUMENT"}}"#,
            )],
            "invalid",
            Some("400"),
            Some("INVALID_ARGUMENT"),
        ),

        // ---- malformed_response (HTTP 200 non-SSE / unparseable, non-retryable, 1 attempt) ----
        (_, "malformed_response") => (
            vec![Resp::status(200, "<html>not-sse-proxy-page</html>")],
            "malformed_response",
            None,
            None,
        ),

        // ---- mid-stream terminal error event preserving partial content ----
        ("openai-responses", "stream_partial_error") => (
            vec![Resp::sse(concat!(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial responses text\"}\n\n",
                "data: {\"type\":\"error\",\"error\":{\"message\":\"subscription usage limit exceeded\",\"type\":\"usage_limit\",\"code\":\"subscription_sharing_usage_limit_exceeded\"}}\n\n"
            ))],
            "quota",
            Some("subscription_sharing_usage_limit_exceeded"),
            Some("usage_limit"),
        ),
        ("openai-chat", "stream_partial_error") => (
            vec![Resp::sse(concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial chat text\"}}]}\n\n",
                "data: {\"error\":{\"message\":\"upstream billing exhausted\",\"type\":\"insufficient_quota\",\"code\":\"insufficient_quota\"}}\n\n"
            ))],
            "quota",
            Some("insufficient_quota"),
            Some("insufficient_quota"),
        ),
        ("anthropic-messages", "stream_partial_error") => (
            vec![Resp::sse(concat!(
                "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial anthropic text\"}}\n\n",
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"billing_error\",\"message\":\"credit balance too low\"}}\n\n"
            ))],
            "quota",
            None,
            Some("billing_error"),
        ),
        ("google-generate", "stream_partial_error") => (
            vec![Resp::sse(concat!(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial google text\"}]}}]}\n\n",
                "data: {\"error\":{\"code\":403,\"message\":\"API key rejected\",\"status\":\"PERMISSION_DENIED\"}}\n\n"
            ))],
            "auth",
            Some("403"),
            Some("PERMISSION_DENIED"),
        ),
        _ => panic!("missing fixture matrix entry for ({wire}, {case})"),
    }
}

async fn run_wire_coded_error_and_retry_checklist(target: WireTarget) {
    local(async {
        let mut covered: BTreeSet<(String, &'static str)> = BTreeSet::new();

        for (case_idx, case) in MANDATORY_CASES.iter().copied().enumerate() {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let (resps, expected_kind, expected_raw_code, expected_raw_type) =
                make_case_responses(target.wire, case);
            let expected_attempts = resps.len();
            assert!(expected_attempts > 0, "{}/{}: fixture must define at least one response", target.wire, case);
            server.route(target.route, resps);

            let h = provider_host(
                &mut env,
                &server,
                json!({
                    "plugins": {
                        "llm": {
                            "retries": 1,
                            "overloaded_retries": 1,
                            "retry_base_ms": 1,
                            "retry_max_ms": 2
                        }
                    }
                }),
            )
            .await;

            // Part A: Direct normalized error boundaries (`http_error` / `stream_error`) carry stable
            // Mare `error_code` and preserve raw vendor metadata in `data` (run once per wire).
            if case_idx == 0 {
                let normalized_json: String = eval(
                    &h,
                    &format!(
                        r#"
                        local he = LLM:http_error({label:?}, {{
                          status = 429,
                          headers = {{ ["x-request-id"] = "req-norm-1" }},
                          text = function() return '{{"error":{{"message":"slow","code":"vendor_limit","type":"vendor_type"}}}}' end,
                        }}, {{ provider = "openai" }})
                        local se = LLM:stream_error({label:?}, {{
                          message = "You exceeded your current quota, please check your plan and billing details.",
                          code = "vendor_quota",
                          type = "billing_type",
                          status = 429,
                        }})
                        return mare.json.encode({{ http = he, stream = se }})
                        "#,
                        label = target.provider_label,
                    ),
                )
                .await;
                let normalized: Value = serde_json::from_str(&normalized_json).unwrap();
                assert_eq!(normalized["http"]["error_code"], "llm.rate_limit", "{target_wire}: {normalized}", target_wire = target.wire);
                assert_eq!(normalized["http"]["data"]["raw_code"], "vendor_limit");
                assert_eq!(normalized["http"]["data"]["raw_type"], "vendor_type");
                assert_eq!(normalized["http"]["data"]["error_code"], "llm.rate_limit");
                assert_eq!(normalized["stream"]["error_code"], "llm.quota");
                assert_eq!(normalized["stream"]["data"]["raw_code"], "vendor_quota");
                assert_eq!(normalized["stream"]["data"]["raw_type"], "billing_type");
            }

            // Part B: Full caller outcome via `LLM:stream` after retries.
            let msg = stream(&h, target.model_ref).await;
            let expected_mare_code = if expected_kind == "overloaded" {
                "llm.provider_overloaded".to_string()
            } else {
                format!("llm.{expected_kind}")
            };
            assert_eq!(msg["stop_reason"], "error", "{}/{}: {msg}", target.wire, case);
            assert_eq!(msg["error_kind"], expected_kind, "{}/{}: {msg}", target.wire, case);
            assert_eq!(msg["error_code"], expected_mare_code, "{}/{}: {msg}", target.wire, case);
            assert_eq!(msg["error_data"]["error_code"], expected_mare_code, "{}/{}: {msg}", target.wire, case);
            assert_eq!(msg["error_data"]["kind"], expected_kind, "{}/{}: {msg}", target.wire, case);

            if let Some(rc) = expected_raw_code {
                assert_eq!(msg["error_data"]["raw_code"], rc, "{}/{}: {msg}", target.wire, case);
            }
            if let Some(rt) = expected_raw_type {
                assert_eq!(msg["error_data"]["raw_type"], rt, "{}/{}: {msg}", target.wire, case);
            }

            if case == "stream_partial_error" {
                assert_eq!(msg["interrupted"], true, "{}/{}: partial text must mark interrupted: {msg}", target.wire, case);
                let content = msg["content"].as_array().expect("content array");
                assert!(
                    content.iter().any(|b| b["type"] == "text" && b["text"].as_str().is_some_and(|s| s.contains("partial"))),
                    "{}/{}: missing partial content: {msg}",
                    target.wire,
                    case
                );
            }
            assert_eq!(
                server.count(target.route),
                expected_attempts,
                "{}/{}: unexpected attempt count (expected {expected_attempts})",
                target.wire,
                case
            );

            // Record the runtime-resolved wire route (and verify the declared wire is actually
            // registered with that route) so matrix coverage comes from executed runtime state
            // rather than re-reading the static `WIRES` table.
            let runtime_wire_route: String = eval(
                &h,
                &format!(
                    r#"
                    local m = assert(LLM:get_model({model_ref:?}), "model missing")
                    local w = assert(LLM:describe_wire({wire:?}), "wire not registered: " .. {wire:?})
                    assert(m.wire_route == w.route, "model wire_route mismatch: " .. tostring(m.wire_route) .. " vs " .. tostring(w.route))
                    return assert(m.wire_route, "model missing wire_route")
                    "#,
                    model_ref = target.model_ref,
                    wire = target.wire,
                ),
            )
            .await;
            covered.insert((runtime_wire_route, case));
            h.shutdown().await;
        }

        let expected_matrix: BTreeSet<(String, &'static str)> = MANDATORY_CASES
            .into_iter()
            .map(|case| (target.expected_route.to_string(), case))
            .collect();
        assert_eq!(
            covered, expected_matrix,
            "{} x 9-case SPEC matrix must be 100% executed at runtime",
            target.wire
        );
    })
    .await;
}

#[tokio::test]
async fn four_wire_coded_error_and_retry_checklist_openai_responses() {
    run_wire_coded_error_and_retry_checklist(WIRES[0]).await;
}

#[tokio::test]
async fn four_wire_coded_error_and_retry_checklist_openai_chat() {
    run_wire_coded_error_and_retry_checklist(WIRES[1]).await;
}

#[tokio::test]
async fn four_wire_coded_error_and_retry_checklist_anthropic_messages() {
    run_wire_coded_error_and_retry_checklist(WIRES[2]).await;
}

#[tokio::test]
async fn four_wire_coded_error_and_retry_checklist_google_generate() {
    run_wire_coded_error_and_retry_checklist(WIRES[3]).await;
}

#[tokio::test]
async fn four_wire_coded_error_and_retry_contract_invariants() {
    let expected_routes: BTreeSet<&str> = ["responses", "chat", "anthropic", "google"].into_iter().collect();
    let declared_routes: BTreeSet<&str> = WIRES.iter().map(|w| w.expected_route).collect();
    assert_eq!(declared_routes, expected_routes, "WIRES table must cover all four wire routes");

    local(async {
        // Part C: Additional contract invariants:
        // 1) `llm:stream` without configured model emits coded `llm.no_model`.
        // 2) Google `MALFORMED_FUNCTION_CALL` & Chat unknown `finish_reason` returned-message
        //    outcomes carry coded `error_kind` / `error_code` / `error_data` while keeping
        //    content, usage, and `raw_stop_reason`.
        // 3) Custom `provider.errors.map` HTTP 503 -> overloaded keeps error_kind, retry budget,
        //    and error_code (`overloaded`) aligned and strips non-allowlisted adapter fields from `error_data`.
        {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            server.route(
                "/google",
                vec![Resp::sse(format!(
                    "data: {}\n\n",
                    json!({
                        "candidates": [{"content": {"role": "model", "parts": [{"text": "partial g"}]}, "finishReason": "MALFORMED_FUNCTION_CALL"}],
                        "usageMetadata": {"promptTokenCount": 7, "candidatesTokenCount": 2}
                    })
                ))],
            );
            server.route(
                "/compat",
                vec![Resp::sse(concat!(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial c\"}}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"weird_custom_stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\n\n"
                ))],
            );
            server.route(
                "/anthropic",
                vec![
                    Resp::status(503, r#"{"type":"error","error":{"type":"api_error","message":"busy"}}"#),
                    Resp::status(503, r#"{"type":"error","error":{"type":"api_error","message":"busy"}}"#),
                ],
            );
            let h = provider_host(
                &mut env,
                &server,
                json!({
                    "plugins": {
                        "llm": {
                            "retries": 4,
                            "overloaded_retries": 1,
                            "retry_base_ms": 1,
                            "retry_max_ms": 2
                        }
                    }
                }),
            )
            .await;

            // 1) llm.no_model (fresh host with no model configured and no configured providers)
            let mut no_model_builder = env.host().user_plugins();
            no_model_builder.opts.config = json!({
                "plugins": {
                    "preset": "minimal",
                    "permissions": { "enabled": false },
                    "provider-faux": { "enabled": false },
                    "provider-openai": { "enabled": false },
                    "provider-anthropic": { "enabled": false },
                    "provider-google": { "enabled": false },
                    "provider-openai-compat": { "enabled": false }
                }
            });
            let no_model_host = no_model_builder.build().await;
            global(&no_model_host, "NM_LLM", no_model_host.service_table("llm").unwrap());
            let (nm_ok, nm_code): (bool, String) = eval(
                &no_model_host,
                r#"
                local ok, err = pcall(function()
                    return NM_LLM:stream({ messages = { { role = "user", content = "hi" } } }, function() end)
                end)
                return ok, type(err) == "table" and tostring(err.code) or tostring(err)
                "#,
            )
            .await;
            no_model_host.shutdown().await;
            assert!(!nm_ok && nm_code == "llm.no_model", "expected llm.no_model, got ok={nm_ok} code={nm_code}");

            // 2) Google MALFORMED_FUNCTION_CALL & Chat unknown finish_reason
            let g_msg = stream(&h, "google/gemini-2.5-pro").await;
            assert_eq!(g_msg["stop_reason"], "error");
            assert_eq!(g_msg["raw_stop_reason"], "MALFORMED_FUNCTION_CALL");
            assert_eq!(g_msg["error_kind"], "malformed_response", "{g_msg}");
            assert_eq!(g_msg["error_code"], "llm.malformed_response", "{g_msg}");
            assert_eq!(g_msg["error_data"]["raw_code"], "MALFORMED_FUNCTION_CALL", "{g_msg}");
            assert!(g_msg["content"].as_array().is_some_and(|c| !c.is_empty()), "{g_msg}");
            assert!(g_msg["usage"].is_object(), "{g_msg}");

            let c_msg = stream(&h, "deepseek/deepseek-chat").await;
            assert_eq!(c_msg["stop_reason"], "error");
            assert_eq!(c_msg["raw_stop_reason"], "weird_custom_stop");
            assert_eq!(c_msg["error_kind"], "malformed_response", "{c_msg}");
            assert_eq!(c_msg["error_code"], "llm.malformed_response", "{c_msg}");
            assert_eq!(c_msg["error_data"]["raw_code"], "weird_custom_stop", "{c_msg}");

            // 3) Reclassification alignment + allowlist strip on custom provider.errors.map
            let mapped_json: String = eval(
                &h,
                r#"
                local wire_stream = LLM:wire("anthropic-messages")
                local function custom_endpoint(_req)
                  return {
                    provider = "custom-map",
                    label = "CustomMap",
                    url = mare.config.plugins["provider-anthropic"].base_url,
                    key = "test-key",
                  }
                end
                local _, reg_err = LLM:register_provider({
                  provider = "custom-map",
                  wire = "anthropic-messages",
                  stream = function(req, sink)
                    return wire_stream(custom_endpoint, req, sink)
                  end,
                  no_key = true,
                  models = { { model_id = "m1", name = "M1", context_window = 8192, max_output = 1024 } },
                  errors = {
                    map = function(raw)
                      raw.data = { secret_header = "do-not-leak", raw_code = "mapped_busy" }
                      return { kind = "overloaded", retryable = true }
                    end,
                  },
                })
                local m = assert(LLM:resolve("custom-map/m1"), "custom-map/m1 must resolve: " .. tostring(reg_err))
                local res = LLM:stream({ model = m, no_fallback = true, messages = { { role = "user", content = "hi" } } }, function() end)
                return mare.json.encode(res)
                "#,
            )
            .await;
            let mapped: Value = serde_json::from_str(&mapped_json).unwrap();
            assert_eq!(mapped["stop_reason"], "error", "{mapped}");
            assert_eq!(mapped["error_kind"], "overloaded", "{mapped}");
            assert_eq!(mapped["error_code"], "llm.provider_overloaded", "{mapped}");
            assert_eq!(mapped["error_data"]["error_code"], "llm.provider_overloaded", "{mapped}");
            assert_eq!(mapped["error_data"]["kind"], "overloaded", "{mapped}");
            assert!(mapped["error_data"].get("secret_header").is_none(), "arbitrary adapter data must not leak: {mapped}");
            assert_eq!(server.count("/anthropic"), 2, "overloaded_retries=1 -> 2 attempts (not 5)");

            // 4) Secret-bearing tables injected AFTER classify_raw (in an adapter returning `_classified = true`
            //    directly or returning `stop_reason = "error"`), message-less `LLM:stream_error`, public `LLM:classify`
            //    on `_classified` `context_overflow, true`, and `provider.errors.map` returning `llm.provider_overloaded`!
            let sanitized_json: String = eval(
                &h,
                r#"
                -- 4a. Thrown `_classified = true` adapter error bypasses `classify_raw`'s data replacement,
                --     proving `stream.lua`'s terminal allowlist itself strips raw leaked keys and invalid `original_kind`.
                assert(LLM:register_provider({
                  provider = "leaky-thrown",
                  stream = function()
                    error({
                      _classified = true,
                      kind = "fatal",
                      retryable = false,
                      secret_token = "sk-leak-99",
                      headers = { authorization = "Bearer tok-leak" },
                      original_kind = { leak = "obj-leak" },
                      provider = "spoofed-provider",
                      data = { secret_header = "post-classify-leak", status = 400 },
                    }, 0)
                  end,
                  no_key = true,
                  models = { { model_id = "m1", name = "M1", context_window = 8192, max_output = 1024 } },
                }))
                local m_thrown = assert(LLM:resolve("leaky-thrown/m1"))
                local res_thrown = LLM:stream({ model = m_thrown, no_fallback = true, messages = { { role = "user", content = "hi" } } }, function() end)

                -- 4b. Returned `stop_reason = "error"` with table `original_error_kind` & message-less `stream_error`.
                assert(LLM:register_provider({
                  provider = "leaky-returned",
                  stream = function()
                    return {
                      role = "assistant",
                      content = {},
                      stop_reason = "error",
                      error_kind = "malformed_response",
                      original_error_kind = { leak = "ret-obj-leak" },
                      error = { secret_field = "sk-ret-leak" },
                      provider = "spoofed-ret-provider",
                    }
                  end,
                  no_key = true,
                  models = { { model_id = "m1", name = "M1", context_window = 8192, max_output = 1024 } },
                }))
                local m_ret = assert(LLM:resolve("leaky-returned/m1"))
                local res_ret = LLM:stream({ model = m_ret, no_fallback = true, messages = { { role = "user", content = "hi" } } }, function() end)

                local se_no_msg = LLM:stream_error("Probe", { secret_token = "sk-se-leak", code = "e1" }, { source = "stream", provider = "openai" })

                -- 4c. Public `LLM:classify` preserves `_classified` `context_overflow, true`.
                local pub_k, pub_r = LLM:classify({
                  message = "context overflow preclassified",
                  kind = "context_overflow",
                  retryable = true,
                  _classified = true,
                })

                -- 4d. Custom `provider.errors.map` returning public code `llm.provider_overloaded` stays retryable overloaded.
                local ov_attempts = 0
                assert(LLM:register_provider({
                  provider = "ov-pub-code",
                  stream = function()
                    ov_attempts = ov_attempts + 1
                    error({ status = 503, message = "busy" }, 0)
                  end,
                  no_key = true,
                  models = { { model_id = "m1", name = "M1", context_window = 8192, max_output = 1024 } },
                  errors = {
                    map = function()
                      return { kind = "llm.provider_overloaded", retryable = true }
                    end,
                  },
                }))
                local m_ov = assert(LLM:resolve("ov-pub-code/m1"))
                local res_ov = LLM:stream({ model = m_ov, no_fallback = true, messages = { { role = "user", content = "hi" } } }, function() end)

                return mare.json.encode({
                  res_thrown = res_thrown,
                  res_ret = res_ret,
                  se_msg = se_no_msg.message,
                  pub_k = pub_k,
                  pub_r = pub_r,
                  res_ov = res_ov,
                  ov_attempts = ov_attempts,
                })
                "#,
            )
            .await;
            let san: Value = serde_json::from_str(&sanitized_json).unwrap();
            let err_str = san["res_thrown"]["error"].as_str().unwrap_or("");
            assert!(!err_str.contains("sk-leak-99"), "secret_token leaked into error text: {san}");
            assert!(!err_str.contains("tok-leak"), "headers leaked into error text: {san}");
            assert_eq!(san["res_thrown"]["provider_id"], "leaky-thrown", "{san}");
            assert_eq!(san["res_thrown"]["error_data"]["provider"], "leaky-thrown", "{san}");
            assert!(san["res_thrown"]["original_error_kind"].is_null(), "non-string original_kind must be dropped: {san}");
            assert!(
                san["res_thrown"]["error_data"].get("secret_header").is_none(),
                "terminal allowlist must strip post-classify secret_header: {san}"
            );

            assert!(!san["res_ret"]["error"].as_str().unwrap_or("").contains("sk-ret-leak"), "{san}");
            assert!(san["res_ret"]["original_error_kind"].is_null(), "returned table original_error_kind must be dropped: {san}");
            assert_eq!(san["res_ret"]["provider_id"], "leaky-returned", "{san}");
            assert!(!san["se_msg"].as_str().unwrap_or("").contains("sk-se-leak"), "stream_error leaked secret: {san}");

            assert_eq!(san["pub_k"], "context_overflow", "{san}");
            assert_eq!(san["pub_r"], true, "public LLM:classify must keep _classified context_overflow retryable=true: {san}");

            assert_eq!(san["res_ov"]["error_kind"], "overloaded", "{san}");
            assert_eq!(san["res_ov"]["error_code"], "llm.provider_overloaded", "{san}");
            assert_eq!(san["ov_attempts"], 2, "overloaded_retries=1 -> 2 attempts when provider.errors.map returns llm.provider_overloaded: {san}");
            h.shutdown().await;
        }
    })
    .await
}
