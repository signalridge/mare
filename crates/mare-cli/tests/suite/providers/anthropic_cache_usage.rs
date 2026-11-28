//! Anthropic cache-write usage across stream events: one test per (arrival order, completeness)
//! cell. Rules (provider-anthropic/wire.lua resolve_cache_write): every usage field an event
//! carries is the cumulative value (Anthropic streaming docs: "The token counts shown in the
//! usage field of the message_delta event are cumulative"); an absent field keeps its value.
//! Total + both buckets agree -> exact; disagree -> cost unknown. Total + one bucket from the
//! same event -> the other is the remainder; from different events -> cost unknown. Total only
//! -> all 5m. Buckets only -> their sum. Unknown keeps the larger token count.
//!
//! Reference: pi packages/ai/src/api/anthropic-messages.ts:1099-1128 (each message_delta usage
//! field replaces the earlier value when present) and test/anthropic-cache-write-1h-cost.test.ts:61
//! (1h priced apart from 5m); Kimi Code packages/core/test/llm/anthropic/fixtures/decode/
//! cache-usage.ts:3 (cache counts from message_start survive an output-only message_delta);
//! reference implementation (cumulative stream usage); Codex
//! core/tests/suite/client.rs:3763 (the final usage is what is reported). None has a 5m/1h
//! split-across-events case.
use super::*;

/// claude-sonnet-5-5 prices per million tokens: 5m cache write $3.75, 1h cache write $6.
const M: i64 = 1_000_000;

fn sse(start: Value, deltas: &[Value]) -> String {
    let mut events = vec![
        json!({ "type": "message_start", "message": {
            "id": "msg_usage", "type": "message", "role": "assistant", "content": [],
            "model": "claude-sonnet-5-5", "stop_reason": null, "usage": start } }),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
        json!({"type": "content_block_stop", "index": 0}),
    ];
    for (i, usage) in deltas.iter().enumerate() {
        let stop = if i + 1 == deltas.len() { json!({"stop_reason": "end_turn"}) } else { json!({}) };
        events.push(json!({ "type": "message_delta", "delta": stop, "usage": usage }));
    }
    events.push(json!({"type": "message_stop"}));
    sse_events(&events)
}

/// Runs one turn; asserts cache_write, cache_write_1h and the cost (None = unknown).
async fn check(start: Value, deltas: &[Value], cache_write: i64, cache_write_1h: Option<i64>, cost: Option<f64>) {
    let mut env = TestEnv::new().await;
    let server = MockServer::start().await;
    server.route("/anthropic", vec![Resp::sse(sse(start, deltas))]);
    let h = provider_host(&mut env, &server, json!({})).await;
    expose(&h);
    global(&h, "USAGE", h.service_table("usage").unwrap());
    let msg = ask(&h, "anthropic/claude-sonnet-5-5").await;
    let (total_cost, unknown): (f64, bool) =
        eval(&h, "local t = USAGE:totals() return t.cost, t.cost_unknown == true").await;
    h.shutdown().await;
    assert_eq!(msg["usage"]["cache_write"].as_i64(), Some(cache_write), "{msg}");
    assert_eq!(msg["usage"]["cache_write_1h"].as_i64(), cache_write_1h, "{msg}");
    match cost {
        Some(expected) => {
            assert!(!unknown, "cost must be known: {msg}");
            assert!((total_cost - expected).abs() < 1e-6, "expected ${expected:.2}, got {total_cost} ({msg})");
        }
        None => {
            assert!(unknown, "cost must be unknown, not a guess: {msg}");
            assert_eq!(msg["usage"]["cost_unknown"], true, "{msg}");
            assert_eq!(total_cost, 0.0, "an unknown request adds nothing to the known total");
        }
    }
}

fn out(n: i64) -> Value {
    json!({ "output_tokens": n })
}

// --- official shapes -------------------------------------------------------------------------

#[tokio::test]
async fn total_only_is_priced_as_5m() {
    local(check(
        json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": M}),
        &[out(0)],
        M,
        None,
        Some(3.75),
    ))
    .await
}

#[tokio::test]
async fn repro_268_anthropic_server_tool_iterations_do_not_trigger_false_silent_overflow() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let multi_iter = sse(
            json!({"input_tokens": 80_000, "output_tokens": 1}),
            &[json!({
                "output_tokens": 20,
                "iterations": [
                    {"type": "message", "input_tokens": 80_000, "cache_read_input_tokens": 0},
                    {"type": "message", "input_tokens": 85_000, "cache_read_input_tokens": 0},
                    {"type": "message", "input_tokens": 90_000, "cache_read_input_tokens": 0}
                ]
            })],
        );
        let ok_after_compact = sse(json!({"input_tokens": 100, "output_tokens": 1}), &[out(5)]);
        server.route("/anthropic", vec![Resp::sse(multi_iter), Resp::sse(ok_after_compact)]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "anthropic/claude-sonnet-5-5",
                "models": { "anthropic/claude-sonnet-5-5": { "context_window": 200_000 } }
            }),
        )
        .await;
        expose(&h);
        global(&h, "USAGE", h.service_table("usage").unwrap());
        exec(
            &h,
            r#"COMPACTED = 0
            C.cordis:on("compact/overflow", function() COMPACTED = COMPACTED + 1 return { handled = true, succeeded = true, res = { status = "compacted", tokens_before = 0, tokens_after = 0 } } end, { prepend = true })"#,
        )
        .await;
        let result = h.prompt("go", json!({})).await.unwrap();
        let (compacted, billed_input, ctx_tokens): (i64, i64, i64) =
            eval(&h, "return COMPACTED, USAGE:totals().input, USAGE:context_tokens()").await;
        h.shutdown().await;
        assert_eq!(
            compacted, 0,
            "3 server-tool iterations (max 90k <= 200k window, cumulative 255k) must NOT trigger silent context overflow compaction: {result}"
        );
        assert_eq!(server.count("/anthropic"), 1, "must not retry after false silent overflow");
        assert_eq!(billed_input, 255_000, "cumulative billed input across iterations must be preserved");
        assert_eq!(ctx_tokens, 90_020, "context occupancy must use the per-window iteration count, not cumulative sum");
    })
    .await
}

/// Review regression (#381):
/// 1) Final-iteration context occupancy uses the final iteration's output tokens (not cumulative billed output).
/// 2) Empty `stop_reason = "length"` (`max_tokens`) in `llm/stream.lua` uses the same per-window
///    input measurement instead of misclassifying cumulative multi-iteration input as `context_overflow`.
#[tokio::test]
async fn anthropic_iterations_use_last_iteration_output_and_window_input_for_empty_length() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        // Case 1: two iterations with outputs 40 and 30 (cumulative billed output = 70),
        // final iteration input = 12 -> context occupancy = 12 + 30 = 42, billed output = 70.
        let two_iter_output = sse(
            json!({"input_tokens": 10, "output_tokens": 0}),
            &[json!({
                "output_tokens": 70,
                "iterations": [
                    {"type": "message", "input_tokens": 10, "output_tokens": 40},
                    {"type": "message", "input_tokens": 12, "output_tokens": 30}
                ]
            })],
        );
        // Case 2: empty max_tokens response with 3 iterations summing to 255k (> 200k window),
        // max single iteration = 90k (< 200k), output = 0 -> must NOT report context_overflow.
        let empty_length_multi_iter = [
            json!({"type": "message_start", "message": {"id": "m_empty_len", "role": "assistant", "content": [],
                "usage": {"input_tokens": 80_000, "output_tokens": 0}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {
                "output_tokens": 0,
                "iterations": [
                    {"type": "message", "input_tokens": 80_000, "cache_read_input_tokens": 0, "output_tokens": 0},
                    {"type": "message", "input_tokens": 85_000, "cache_read_input_tokens": 0, "output_tokens": 0},
                    {"type": "message", "input_tokens": 90_000, "cache_read_input_tokens": 0, "output_tokens": 0}
                ]
            }}),
            json!({"type": "message_stop"}),
        ]
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect::<String>();

        // Case 3: real single-iteration empty max_tokens at >=99% window still classifies as context_overflow.
        let empty_length_real_overflow = [
            json!({"type": "message_start", "message": {"id": "m_empty_ovf", "role": "assistant", "content": [],
                "usage": {"input_tokens": 199_000, "output_tokens": 0}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {
                "output_tokens": 0,
                "iterations": [
                    {"type": "message", "input_tokens": 199_000, "cache_read_input_tokens": 0, "output_tokens": 0}
                ]
            }}),
            json!({"type": "message_stop"}),
        ]
        .iter()
        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
        .collect::<String>();

        server.route(
            "/anthropic",
            vec![
                Resp::sse(two_iter_output),
                Resp::sse(empty_length_multi_iter),
                Resp::sse(empty_length_real_overflow),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "anthropic/claude-sonnet-5-5",
                "models": { "anthropic/claude-sonnet-5-5": { "context_window": 200_000 } },
                "plugins": { "llm": { "retries": 0 } }
            }),
        )
        .await;
        expose(&h);
        global(&h, "USAGE", h.service_table("usage").unwrap());

        let r1 = h.prompt("turn 1", json!({})).await.unwrap();
        assert_eq!(r1["status"], "completed", "{r1}");
        let (billed_out, ctx_tokens): (i64, i64) = eval(&h, "return USAGE:totals().output, USAGE:context_tokens()").await;
        assert_eq!(billed_out, 70, "billed output must keep cumulative total 70");
        assert_eq!(ctx_tokens, 42, "context occupancy must be final iteration input (12) + final iteration output (30)");

        let r2 = h.prompt("turn 2", json!({})).await.unwrap();
        assert_eq!(r2["status"], "failed", "{r2}");
        assert_ne!(
            r2["error"]["code"], "context_overflow",
            "empty max_tokens with multi-iteration sum > window (max single iter 90k < 200k) must NOT misclassify as context_overflow: {r2}"
        );
        let err_msg = r2["error"]["message"].as_str().unwrap_or("");
        assert!(
            err_msg.contains("cut at max_tokens"),
            "expected max_tokens empty-response classification, got: {r2}"
        );

        let r3_kind: String = eval(
            &h,
            r#"
            local ok, res = pcall(function()
                return LLM:stream({
                    model = LLM:resolve("anthropic/claude-sonnet-5-5"),
                    system = "sys",
                    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                }, function() end)
            end)
            if ok then return tostring(res.error_kind or "ok") end
            return type(res) == "table" and tostring(res.kind or res.code or "") or tostring(res)
            "#,
        )
        .await;
        assert_eq!(
            r3_kind, "context_overflow",
            "real single-iteration empty max_tokens at >=99% window must still report context_overflow"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn total_with_matching_breakdown_in_message_start() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": M,
        "cache_creation": {"ephemeral_5m_input_tokens": 600_000, "ephemeral_1h_input_tokens": 400_000}});
    local(check(start, &[out(0)], M, Some(400_000), Some(4.65))).await
}

#[tokio::test]
async fn cumulative_delta_repeating_the_total_keeps_it_exact() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": M,
        "cache_creation": {"ephemeral_5m_input_tokens": 600_000, "ephemeral_1h_input_tokens": 400_000}});
    let delta = json!({"output_tokens": 0, "cache_creation_input_tokens": M});
    local(check(start, &[delta], M, Some(400_000), Some(4.65))).await
}

#[tokio::test]
async fn total_with_one_bucket_in_the_same_event_derives_the_other() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": M,
        "cache_creation": {"ephemeral_1h_input_tokens": 400_000}});
    local(check(start, &[out(0)], M, Some(400_000), Some(4.65))).await
}

// --- buckets without a total -----------------------------------------------------------------

#[tokio::test]
async fn buckets_split_across_events_without_a_total_sum_5m_then_1h() {
    let start =
        json!({"input_tokens": 0, "output_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 600_000}});
    let delta = json!({"output_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens": 400_000}});
    local(check(start, &[delta], M, Some(400_000), Some(4.65))).await
}

#[tokio::test]
async fn buckets_split_across_events_without_a_total_sum_1h_then_5m() {
    let start =
        json!({"input_tokens": 0, "output_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens": 400_000}});
    let delta = json!({"output_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 600_000}});
    local(check(start, &[delta], M, Some(400_000), Some(4.65))).await
}

// --- three events, the last one output-only --------------------------------------------------

#[tokio::test]
async fn three_events_consistent_then_output_only_keeps_the_exact_state() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": M,
        "cache_creation": {"ephemeral_5m_input_tokens": 600_000, "ephemeral_1h_input_tokens": 400_000}});
    let grew = json!({"output_tokens": 0, "cache_creation_input_tokens": 1_100_000,
        "cache_creation": {"ephemeral_1h_input_tokens": 500_000}});
    // 0.6M * 3.75 + 0.5M * 6 = 2.25 + 3.00
    local(check(start, &[grew, out(0)], 1_100_000, Some(500_000), Some(5.25))).await
}

// An output-only delta must not shrink an earlier total to the bucket sum.
#[tokio::test]
async fn disagreeing_parts_then_output_only_keep_the_total_and_mark_unknown() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": M,
        "cache_creation": {"ephemeral_5m_input_tokens": 600_000}});
    let one_h = json!({"output_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens": 100_000}});
    local(check(start, &[one_h, out(7)], M, Some(100_000), None)).await
}

// --- undecidable: the total and the breakdown disagree, or came from different events --------

// A later total smaller than the buckets must not overwrite them.
#[tokio::test]
async fn bucket_first_then_other_bucket_with_a_smaller_total_is_unknown() {
    let start =
        json!({"input_tokens": 0, "output_tokens": 0, "cache_creation": {"ephemeral_5m_input_tokens": 600_000}});
    let delta = json!({"output_tokens": 0, "cache_creation_input_tokens": 400_000,
        "cache_creation": {"ephemeral_1h_input_tokens": 400_000}});
    local(check(start, &[delta], M, Some(400_000), None)).await
}

// A total without breakdown, then one bucket later: 600k total or 600k + 400k?
#[tokio::test]
async fn total_first_then_one_bucket_later_is_unknown() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 600_000});
    let delta = json!({"output_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens": 400_000}});
    local(check(start, &[delta], 600_000, Some(400_000), None)).await
}

#[tokio::test]
async fn one_bucket_first_then_a_total_later_is_unknown() {
    let start =
        json!({"input_tokens": 0, "output_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens": 400_000}});
    let delta = json!({"output_tokens": 0, "cache_creation_input_tokens": M});
    local(check(start, &[delta], M, Some(400_000), None)).await
}

#[tokio::test]
async fn total_and_bucket_then_the_other_bucket_without_a_total_is_unknown() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 600_000,
        "cache_creation": {"ephemeral_5m_input_tokens": 600_000}});
    let delta = json!({"output_tokens": 0, "cache_creation": {"ephemeral_1h_input_tokens": 400_000}});
    local(check(start, &[delta], M, Some(400_000), None)).await
}

#[tokio::test]
async fn a_total_that_grew_past_its_breakdown_is_unknown() {
    let start = json!({"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": M,
        "cache_creation": {"ephemeral_5m_input_tokens": 600_000, "ephemeral_1h_input_tokens": 400_000}});
    let delta = json!({"output_tokens": 0, "cache_creation_input_tokens": 1_200_000});
    local(check(start, &[delta], 1_200_000, Some(400_000), None)).await
}
