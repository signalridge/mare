//! ScriptedModel port (Codex): error classification and retry on the Responses wire, ported from
//! openai/codex@5cb38e7 `codex-rs/core/tests/suite/retry_after.rs` onto `ScriptedModel`. Each test
//! names its Codex case; what Codex tests about its own transport, tiers, hooks or WebSocket is
//! left out. Delays are read from `llm/retry` (`delay_ms`), so the
//! assertions are about the decision, not the wall clock.

use super::*;
use crate::common::scripted::{Part, ScriptedModel, Step, Wire};

/// A `response.failed` stream with `error` (Codex `responses::sse_failed`).
fn failed(error: Value) -> Step {
    Step::Raw(Resp::sse(sse_events(&[
        json!({ "type": "response.created", "response": { "id": "resp_f", "status": "in_progress" } }),
        json!({ "type": "response.failed", "response": { "id": "resp_f", "status": "failed", "error": error } }),
    ])))
}

/// An HTTP error with a JSON body and optional Retry-After.
fn http(status: u16, body: Value, retry_after: Option<&str>) -> Step {
    let mut r = Resp::status(status, body.to_string());
    if let Some(ra) = retry_after {
        r = r.header("retry-after", ra);
    }
    Step::Raw(r)
}

fn recovered() -> Step {
    Step::Reply(vec![Part::Text("recovered".into())])
}

/// What one prompt did: the result, and each `llm/retry` as (kind, delay_ms).
struct Outcome {
    res: Value,
    retries: Vec<(String, i64)>,
    requests: usize,
}

/// One prompt against `steps` on the Responses wire, with `llm` options `llm_cfg`.
async fn run(steps: Vec<Step>, llm_cfg: Value) -> Outcome {
    let mut env = TestEnv::new().await;
    env.set_var("OPENAI_API_KEY", "test-openai");
    let mock = ScriptedModel::start(Wire::Responses, steps).await;
    let mut cfg = mock.host_config();
    merge(&mut cfg, json!({ "plugins": { "llm": llm_cfg } }));
    let h = env.host().config(cfg).build().await;
    expose(&h);
    exec(
        &h,
        r#"RETRY = {}
           C.cordis:on("step/completed", function(e) if e.will_retry then RETRY[#RETRY + 1] = { tostring(e.error_kind), math.floor(e.delay_ms or -1) } end end)"#,
    )
    .await;
    let res = h.prompt("go", json!({})).await.unwrap();
    let retries: Vec<(String, i64)> = eval::<Vec<Vec<mlua::Value>>>(&h, "return RETRY")
        .await
        .into_iter()
        .map(|r| (r[0].to_string().unwrap_or_default(), r[1].as_i64().unwrap_or(-1)))
        .collect();
    h.shutdown().await;
    let requests = mock.requests().len();
    mock.finish();
    Outcome { res, retries, requests }
}

/// Codex retry_after.rs:280 `responses_http_uses_retry_after`: a 503 `server_is_overloaded` with
/// `Retry-After: 1` waits the server's second, then the next attempt answers.
#[tokio::test]
async fn an_overloaded_503_waits_for_its_retry_after_and_recovers() {
    local(async {
        let overload = json!({ "error": { "code": "server_is_overloaded", "message": "overloaded" } });
        let o = run(vec![http(503, overload, Some("1")), recovered()], json!({ "retry_base_ms": 1 })).await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries, [("overloaded".to_string(), 1000)], "the server's Retry-After decides the delay");
    })
    .await
}

/// Codex retry_after.rs:507 `responses_http_overload_respects_retry_limits` (with and without
/// advice): retry advice changes the delay, never the budget. `retries = 2`: three requests, then
/// one overloaded error.
async fn overload_advice_never_extends_the_budget(advice: Option<&str>) {
    let overload =
        || http(503, json!({ "error": { "code": "server_is_overloaded", "message": "at capacity" } }), advice);
    let o = run(vec![overload(), overload(), overload()], json!({ "retries": 2, "retry_base_ms": 1 })).await;
    assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
    assert_eq!(o.res["error_kind"], "overloaded", "{}", o.res);
    assert_eq!(o.requests, 3, "the budget is 1 + retries requests whatever the advice");
    assert_eq!(o.retries.len(), 2, "{:?}", o.retries);
}

#[tokio::test]
async fn overload_retry_advice_never_extends_the_budget() {
    local(async {
        for advice in [Some("0"), None] {
            overload_advice_never_extends_the_budget(advice).await;
        }
    })
    .await
}

/// Codex retry_after.rs:590 `responses_http_504_preserves_status_after_stream_retry`: after its
/// retries a gateway timeout is reported with its status.
#[tokio::test]
async fn a_504_keeps_its_status_after_the_retries() {
    local(async {
        let o = run(
            vec![http(504, json!({ "error": { "message": "upstream timed out" } }), None); 2],
            json!({ "retries": 1, "retry_base_ms": 1 }),
        )
        .await;
        assert_eq!(o.requests, 2);
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        assert!(o.res["error"]["message"].as_str().is_some_and(|e| e.contains("504")), "the status is lost: {}", o.res);
    })
    .await
}

/// Codex retry_after.rs:1213 `sse_failure_uses_server_advice` (azure_style_plaintext): a streamed
/// `rate_limit_exceeded` whose message says "Try again in 1 second." waits that second.
#[tokio::test]
async fn a_streamed_rate_limit_waits_for_the_advice_in_its_message() {
    local(async {
        let err = json!({ "code": "rate_limit_exceeded", "message": "Rate limit exceeded. Try again in 1 second." });
        let o = run(vec![failed(err), recovered()], json!({ "retry_base_ms": 1 })).await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.retries, [("rate_limit".to_string(), 1000)]);
    })
    .await
}

/// Codex retry_after.rs:1213 (slow_down_fractional_plaintext): `slow_down` is a rate limit too,
/// and "Please try again in 1.000s." is one second.
#[tokio::test]
async fn a_streamed_slow_down_is_a_rate_limit_with_fractional_advice() {
    local(async {
        let err = json!({ "code": "slow_down", "message": "Please try again in 1.000s." });
        let o = run(vec![failed(err), recovered()], json!({ "retry_base_ms": 1 })).await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.retries, [("rate_limit".to_string(), 1000)]);
    })
    .await
}

/// Codex retry_after.rs:1213 (azure_style_header): an error event's own `headers.Retry-After`
/// beats the advice in its message.
#[tokio::test]
async fn a_streamed_errors_retry_after_header_beats_its_message() {
    local(async {
        let err = json!({ "code": "rate_limit_exceeded", "message": "Rate limit exceeded. Try again in 4 seconds.",
                          "headers": { "Retry-After": "1" } });
        let o = run(vec![failed(err), recovered()], json!({ "retry_base_ms": 1 })).await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.retries, [("rate_limit".to_string(), 1000)], "the header's second, not the message's four");
    })
    .await
}

/// Codex retry_after.rs:1213 (malformed_header): a header that does not parse falls back to the
/// message's advice.
#[tokio::test]
async fn a_malformed_nested_retry_after_falls_back_to_the_message() {
    local(async {
        let err = json!({ "code": "rate_limit_exceeded", "message": "Try again in 1 second.",
                          "headers": { "Retry-After": "bad" } });
        let o = run(vec![failed(err), recovered()], json!({ "retry_base_ms": 1 })).await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.retries, [("rate_limit".to_string(), 1000)]);
    })
    .await
}

/// Codex retry_after.rs:1124 `sse_failure_without_retry_after_exhausts_stream_retries`: a streamed
/// rate limit without advice uses the local backoff, exhausts the retries and ends in one
/// rate-limit error.
#[tokio::test]
async fn a_streamed_rate_limit_without_advice_exhausts_the_retries() {
    local(async {
        let err = || failed(json!({ "code": "rate_limit_exceeded", "message": "Rate limit exceeded." }));
        let o = run(vec![err(), err()], json!({ "retries": 1, "retry_base_ms": 1 })).await;
        assert_eq!(o.requests, 2);
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        assert_eq!(o.res["error_kind"], "rate_limit", "{}", o.res);
        assert_eq!(o.retries.len(), 1);
        assert!(o.retries[0].1 < 100, "local backoff, not a server delay: {:?}", o.retries);
    })
    .await
}

/// Codex retry_after.rs:1451 `http_and_sse_provider_errors_are_terminal` (model): a model that is
/// gone is final however it arrives — not retried even with `Retry-After: 300` — and its message
/// reaches the user.
async fn model_not_found_is_terminal(step: Step) {
    let o = run(vec![step], json!({ "retries": 2, "retry_base_ms": 1 })).await;
    assert_eq!(o.requests, 1, "a missing model was retried");
    assert!(o.retries.is_empty(), "{:?}", o.retries);
    assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
    assert!(
        o.res["error"]["message"].as_str().is_some_and(|e| e.contains("Access to this deployment ended.")),
        "the provider's message is lost: {}",
        o.res
    );
}

const GONE: &str = "Access to this deployment ended.";

#[tokio::test]
async fn model_not_found_over_http_is_terminal() {
    local(model_not_found_is_terminal(http(
        404,
        json!({ "error": { "code": "model_not_found", "message": GONE } }),
        Some("300"),
    )))
    .await
}

#[tokio::test]
async fn model_not_found_as_a_stream_error_event_is_terminal() {
    local(model_not_found_is_terminal(Step::Raw(Resp::sse(sse_events(&[json!({
        "type": "error", "error": { "code": "model_not_found", "message": GONE }
    })])))))
    .await
}

#[tokio::test]
async fn model_not_found_as_response_failed_is_terminal() {
    local(model_not_found_is_terminal(failed(json!({ "code": "model_not_found", "message": GONE })))).await
}
