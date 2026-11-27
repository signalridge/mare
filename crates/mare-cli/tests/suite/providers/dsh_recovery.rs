//! ScriptedModel port: stream interruption & recovery, error classification, backoff/budget,
//! overflow compaction, and tool-turn integrity ported from deepseek-ai/reference implementation
//! (`packages/test-support/llm-mock-server/src/index.ts`, `packages/llm/llm-retry/tests/`,
//! `packages/llm/llm/tests/`, `packages/core/agent-loop/tests/`) onto `ScriptedModel`.
//!
//! Each test exercises a real mare public path (`Host::prompt` -> provider plugin -> `ScriptedModel`)
//! and asserts outcomes (`text`, `termination_reason`, `error_kind`, `llm/retry`, `turns()`, tool
//! execution counts), not raw SSE frames.

use super::*;
use crate::common::scripted::{Finish, Part, ScriptedModel, Step, Wire};

struct Outcome {
    res: Value,
    retries: Vec<(String, i64, bool)>,
    requests: usize,
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
                   RETRY[#RETRY + 1] = { tostring(e.error_kind), math.floor(e.delay_ms or -1), e.reset == true }
               end
           end)"#,
    )
    .await;
    if let Some(lua) = setup_lua {
        exec(&h, lua).await;
    }
    let res = h.prompt("go", json!({})).await.unwrap();
    let retries: Vec<(String, i64, bool)> = eval::<Vec<Vec<mlua::Value>>>(&h, "return RETRY")
        .await
        .into_iter()
        .map(|r| {
            (r[0].to_string().unwrap_or_default(), r[1].as_i64().unwrap_or(-1), r[2].as_boolean().unwrap_or(false))
        })
        .collect();
    h.shutdown().await;
    let requests = mock.requests().len();
    mock.finish();
    Outcome { res, retries, requests }
}

fn recovered() -> Step {
    Step::Reply(vec![Part::Text("recovered".into())])
}

fn anthropic_http(status: u16, body: Value) -> Step {
    Step::Raw(Resp::status(status, body.to_string()))
}

/// reference implementation (`stream_disconnect`) & `transport-recovery.spec.ts:129-164`:
/// a 200 SSE stream that disconnects before any chunk arrives is retried and recovers.
#[tokio::test]
async fn stream_disconnect_before_any_chunk_retries_and_recovers() {
    local(async {
        let o = run_wire(
            Wire::Anthropic,
            vec![Step::Raw(Resp::sse("").cut(0)), recovered()],
            json!({ "retry_base_ms": 1 }),
            None,
        )
        .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
    })
    .await
}

/// reference implementation (`partialToolFailure`) & `:727-758` &
/// `transport-recovery.spec.ts:129-164` (`partial_disconnect`): a stream cut after emitting
/// partial text and a tool call never executes the tool, resets the UI stream, and keeps the
/// discarded output out of the retried request.
#[tokio::test]
async fn partial_disconnect_with_tool_call_discards_attempt_and_never_executes_tool() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::CutAfter {
                    parts: vec![
                        Part::Text("discarded partial output".into()),
                        Part::ToolCall { id: "call_danger".into(), name: "danger".into(), args: json!({}) },
                    ],
                    events: 6,
                },
                recovered(),
            ],
        )
        .await;
        let h = mock.host(&mut env, false, &[]).await;
        exec(
            &h,
            r#"RAN = 0
               RESET_COUNT = 0
               C.cordis:on("step/completed", function(e) if e.will_retry and e.reset then RESET_COUNT = RESET_COUNT + 1 end end)
               T:register("danger", {
                   description = "must not run on failed attempt",
                   parameters = { type = "object" },
                   execute = function() RAN = RAN + 1 return "unexpected" end,
               })"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        let (ran, reset_count): (i64, i64) = eval(&h, "return RAN, RESET_COUNT").await;
        h.shutdown().await;
        let turns = mock.turns();
        mock.finish();

        assert_eq!(ran, 0, "tool from cut attempt must never execute");
        assert_eq!(reset_count, 1, "retry must emit reset=true after partial chunks");
        assert_eq!(res["text"], "recovered", "{res}");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1], turns[0], "discarded partial output must not leak into retried request");
    })
    .await
}

/// reference implementation (`empty_body`): a 200 SSE response with an empty body
/// is retried and recovers.
#[tokio::test]
async fn empty_body_200_stream_is_retried_and_recovers() {
    local(async {
        let o =
            run_wire(Wire::Anthropic, vec![Step::Raw(Resp::sse("")), recovered()], json!({ "retry_base_ms": 1 }), None)
                .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
    })
    .await
}

/// reference implementation (`stream_eof`): a 200 SSE stream that emits only
/// `message_start` and closes cleanly without content or `message_stop` is retried.
#[tokio::test]
async fn stream_eof_after_message_start_without_content_is_retried() {
    local(async {
        let eof_after_start = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n";
        let o = run_wire(
            Wire::Anthropic,
            vec![Step::Raw(Resp::sse(eof_after_start)), recovered()],
            json!({ "retry_base_ms": 1 }),
            None,
        )
        .await;
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
    })
    .await
}

/// reference implementation (`malformed_json`): invalid JSON in an SSE `data:`
/// line on the OpenAI-compat wire fails with `malformed_response` without retrying.
#[tokio::test]
async fn compat_malformed_json_in_sse_fails_without_retry() {
    local(async {
        let bad_sse = "data: {not-json\n\ndata: [DONE]\n\n";
        let o = run_wire(
            Wire::Compat,
            vec![Step::Raw(Resp::sse(bad_sse))],
            json!({ "retries": 2, "retry_base_ms": 1 }),
            None,
        )
        .await;
        assert_eq!(o.requests, 1, "malformed JSON must not be retried: {:?}", o.res);
        assert!(o.retries.is_empty(), "{:?}", o.retries);
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        assert_eq!(o.res["error_kind"], "malformed_response", "{}", o.res);
    })
    .await
}

/// reference implementation (`malformed_event`): an Anthropic SSE event with
/// `content_block: null` fails without retrying.
#[tokio::test]
async fn anthropic_malformed_event_null_content_block_fails_without_retry() {
    local(async {
        let bad_event = "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":null}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let o = run_wire(
            Wire::Anthropic,
            vec![Step::Raw(Resp::sse(bad_event))],
            json!({ "retries": 2, "retry_base_ms": 1 }),
            None,
        )
        .await;
        assert_eq!(o.requests, 1, "malformed event must not be retried: {:?}", o.res);
        assert!(o.retries.is_empty(), "{:?}", o.retries);
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        assert_eq!(o.res["error_kind"], "malformed_response", "{}", o.res);
    })
    .await
}

/// reference implementation (`BlockAssembler.interruptedBlocks`) &
/// `llm-mock-server/src/index.ts:456`: a stream truncated mid-`tool_call` (incomplete JSON
/// arguments, no `content_block_stop`, no `message_stop`) is retried, never executes the tool,
/// and leaves no partial call in the retried request.
#[tokio::test]
async fn truncated_mid_partial_tool_call_never_executes_and_recovers() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![
                    Part::Text("prefix".into()),
                    Part::PartialToolCall {
                        id: "call_partial".into(),
                        name: "danger".into(),
                        raw_args: "{\"x\":".into(),
                    },
                ]),
                recovered(),
            ],
        )
        .await;
        let h = mock.host(&mut env, false, &[]).await;
        exec(
            &h,
            r#"RAN = 0
               RETRY_COUNT = 0
               C.cordis:on("step/completed", function(e) if e.will_retry and e.delay_ms ~= nil then RETRY_COUNT = RETRY_COUNT + 1 end end)
               T:register("danger", {
                   description = "must not run",
                   parameters = { type = "object" },
                   execute = function() RAN = RAN + 1 return "ran" end,
               })"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        let (ran, retry_count): (i64, i64) = eval(&h, "return RAN, RETRY_COUNT").await;
        h.shutdown().await;
        let turns = mock.turns();
        mock.finish();

        assert_eq!(ran, 0, "partial tool call must never execute");
        assert_eq!(res["text"], "recovered", "{res}");
        assert_eq!(retry_count, 1);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1], turns[0], "partial tool call must not leak into retry");
        assert!(turns[1].calls.is_empty(), "no dangling calls: {:?}", turns[1]);
    })
    .await
}

/// reference implementation (`applies bounded exponential jitter and stops
/// after the configured budget`) & `transport-recovery.spec.ts:297-320`: retries grow
/// exponentially with ±10% jitter, stay clamped at `retry_max_ms`, and terminate after `retries`.
#[tokio::test]
async fn exponential_backoff_with_cap_and_budget_exhaustion() {
    local(async {
        let err = || {
            anthropic_http(
                500,
                json!({ "type": "error", "error": { "type": "api_error", "message": "mock server error" } }),
            )
        };
        let o = run_wire(
            Wire::Anthropic,
            vec![err(), err(), err()],
            json!({ "retries": 2, "retry_base_ms": 100, "retry_max_ms": 150 }),
            None,
        )
        .await;
        assert_eq!(o.requests, 3, "1 initial + 2 retries");
        assert_eq!(o.retries.len(), 2, "{:?}", o.retries);
        assert_eq!(o.retries[0].0, "server");
        assert_eq!(o.retries[1].0, "server");
        assert!((90..=110).contains(&o.retries[0].1), "attempt 1 delay must be 100ms ±10%: {:?}", o.retries);
        assert!(
            (135..=150).contains(&o.retries[1].1),
            "attempt 2 delay must be capped at retry_max_ms=150 (with -10%..0% jitter): {:?}",
            o.retries
        );
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        assert_eq!(o.res["error_kind"], "server", "{}", o.res);
    })
    .await
}

/// reference implementation (`invalid_request`): an HTTP 400 `invalid_request_error`
/// (non-overflow) is terminal without retry and surfaces the provider's message.
#[tokio::test]
async fn invalid_request_400_is_terminal_without_retry() {
    local(async {
        let bad = anthropic_http(
            400,
            json!({ "type": "error", "error": { "type": "invalid_request_error", "message": "mock invalid request" } }),
        );
        let o = run_wire(Wire::Anthropic, vec![bad], json!({ "retries": 2, "retry_base_ms": 1 }), None).await;
        assert_eq!(o.requests, 1, "400 invalid_request must not be retried");
        assert!(o.retries.is_empty(), "{:?}", o.retries);
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        assert!(o.res["error"]["message"].as_str().is_some_and(|e| e.contains("mock invalid request")), "{}", o.res);
    })
    .await
}

/// reference implementation (`context_overflow`): an HTTP 400
/// `context_length_exceeded` triggers compaction and resends the request without consuming a
/// backoff retry.
#[tokio::test]
async fn context_overflow_400_triggers_compaction_and_retries() {
    local(async {
        let mut env = TestEnv::new().await;
        let overflow = Step::Raw(Resp::status(
            400,
            json!({
                "error": {
                    "message": "mock input exceeds the model context window",
                    "type": "invalid_request_error",
                    "code": "context_length_exceeded",
                }
            })
            .to_string(),
        ));
        let mock = ScriptedModel::start(Wire::Compat, vec![overflow, recovered()]).await;
        let h = mock.host(&mut env, false, &["compact", "usage"]).await;
        exec(
            &h,
            r#"COMPACTED = 0
               RETRY_COUNT = 0
               C.cordis:on("step/completed", function(e) if e.will_retry and e.delay_ms ~= nil then RETRY_COUNT = RETRY_COUNT + 1 end end)
               C.cordis:on("compact/overflow", function() COMPACTED = COMPACTED + 1 return { handled = true, succeeded = true, res = { status = "compacted", tokens_before = 0, tokens_after = 0 } } end, { prepend = true })"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        let (compacted, retry_count): (i64, i64) = eval(&h, "return COMPACTED, RETRY_COUNT").await;
        h.shutdown().await;
        mock.finish();

        assert_eq!(compacted, 1, "400 context_length_exceeded must trigger compaction: {res}");
        assert_eq!(retry_count, 0, "compaction recovery should not emit llm/retry");
        assert_eq!(res["text"], "recovered", "{res}");
    })
    .await
}

/// reference implementation (`starts a fresh retry budget...`): exhausting
/// retry on step 1 still leaves a fresh retry budget for step 2 after a tool call.
#[tokio::test]
async fn retry_budget_resets_for_next_step_after_tool_call() {
    local(async {
        let err = || {
            anthropic_http(500, json!({ "type": "error", "error": { "type": "api_error", "message": "transient" } }))
        };
        let o = run_wire(
            Wire::Anthropic,
            vec![
                err(),
                Step::Reply(vec![
                    Part::ToolCall { id: "c1".into(), name: "ping".into(), args: json!({}) },
                    Part::Finish(Finish::ToolCalls),
                ]),
                err(),
                Step::Reply(vec![Part::Text("done".into())]),
            ],
            json!({ "retries": 1, "retry_base_ms": 1 }),
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
        assert_eq!(o.requests, 4);
        assert_eq!(o.retries.len(), 2, "one retry on step 1 + one retry on step 2: {:?}", o.retries);
    })
    .await
}

/// reference implementation (`max-tokens does not leak across turns`):
/// a turn ending in `max_tokens` (`Finish::Length`) reports `length` (`max_tokens`), and the
/// following turn ending normally reports `completed` (`stop`).
#[tokio::test]
async fn max_tokens_does_not_leak_across_turns() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![Part::Text("cut".into()), Part::Finish(Finish::Length)]),
                Step::Reply(vec![Part::Text("full".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = mock.host(&mut env, false, &[]).await;
        let first = h.prompt("turn 1", json!({})).await.unwrap();
        let second = h.prompt("turn 2", json!({})).await.unwrap();
        h.shutdown().await;
        mock.finish();

        assert_eq!(first["text"], "cut", "{first}");
        assert_eq!(first["messages"].as_array().unwrap().last().unwrap()["stop_reason"], "length", "{first}");
        assert_eq!(second["text"], "full", "{second}");
        assert_eq!(second["messages"].as_array().unwrap().last().unwrap()["stop_reason"], "stop", "{second}");
    })
    .await
}

/// reference implementation (`orders pre-execute denials and errors
/// without dispatching them`): a `tool/before` denial skips execution of the denied call while
/// executing its sibling, and returns both results to the model in model order.
#[tokio::test]
async fn pre_execute_tool_denial_skips_execution_and_reports_to_model() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![
                    Part::ToolCall { id: "c1".into(), name: "denied_tool".into(), args: json!({}) },
                    Part::ToolCall { id: "c2".into(), name: "allowed_tool".into(), args: json!({}) },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![Part::Text("handled".into())]),
            ],
        )
        .await;
        let h = mock.host(&mut env, false, &[]).await;
        exec(
            &h,
            r#"DENIED_RAN, ALLOWED_RAN = 0, 0
               T:register("denied_tool", {
                   description = "denied",
                   parameters = { type = "object" },
                   execute = function() DENIED_RAN = DENIED_RAN + 1 return "should not run" end,
               })
               T:register("allowed_tool", {
                   description = "allowed",
                   parameters = { type = "object" },
                   execute = function() ALLOWED_RAN = ALLOWED_RAN + 1 return "allowed ok" end,
               })
               C.cordis:get("policy"):voter("tool/before", function(call)
                   if call.name == "denied_tool" then return { decision = "deny", reason = "blocked by policy" } end
               end)"#,
        )
        .await;
        let res = h.prompt("run both", json!({})).await.unwrap();
        let (denied_ran, allowed_ran): (i64, i64) = eval(&h, "return DENIED_RAN, ALLOWED_RAN").await;
        h.shutdown().await;
        let turns = mock.turns();
        mock.finish();

        assert_eq!(denied_ran, 0, "denied tool must not execute");
        assert_eq!(allowed_ran, 1, "allowed sibling must execute once");
        assert_eq!(res["text"], "handled", "{res}");
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].results.len(), 2, "{:?}", turns[1].results);
        assert_eq!(turns[1].results[0].0, "c1");
        assert!(turns[1].results[0].1.contains("blocked by policy"), "{:?}", turns[1].results[0]);
        assert_eq!(turns[1].results[1].0, "c2");
        assert!(turns[1].results[1].1.contains("allowed ok"), "{:?}", turns[1].results[1]);
    })
    .await
}
