//! ScriptedModel port (Kimi Code): agent loop order, multi-turn tools, steer/follow-up insertion
//! points, post-abort history, partial-output-then-error, and retry-after edge cases ported from
//! `MoonshotAI/reference implementation` (`packages/agent-core-v2/test/agent/loop/loop.test.ts` and
//! `packages/agent-core-v2/test/app/llmProtocol/errors.test.ts`) onto `ScriptedModel`.
//!
//! Each test exercises a real mare public path (`Host::prompt`/`steer`/`follow_up`/`abort` ->
//! `agent` plugin -> provider plugin -> `ScriptedModel`) and asserts outcomes (`text`,
//! `termination_reason`, `error_kind`, `usage`, `llm/retry`, `ScriptedModel::turns()`, session
//! resume), not raw SSE frames.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::common::scripted::{Finish, Part, ScriptedModel, Step, Wire};

async fn scripted_host(env: &mut TestEnv, mock: &ScriptedModel, llm_cfg: Value, resume: bool) -> Arc<Host> {
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
    let b = env.host().config(cfg);
    let h = if resume { b.resume() } else { b }.build().await;
    expose(&h);
    h
}

/// Kimi `loop.test.ts:262-287` (`stops the turn when provider reports tool_calls without any tool
/// call structure`): when the model finishes with `Finish::ToolCalls` but emits no tool_call
/// blocks, the agent loop completes the turn cleanly in 1 request without hanging or erroring.
#[tokio::test]
async fn tool_calls_finish_reason_without_calls_completes_cleanly() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![Step::Reply(vec![Part::Text("done".into()), Part::Finish(Finish::ToolCalls)])],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        let res = h.prompt("Hello", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["text"], "done", "{res}");
        assert_eq!(res["termination_reason"], "completed", "{res}");
        h.shutdown().await;
        assert_eq!(mock.requests().len(), 1);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:524-626` (`runs an agent turn through registered tool approval and
/// execution`): multi-turn tool loop across two sequential tool steps accumulates usage across
/// steps and sends both tool rounds in exact model order on the final request.
#[tokio::test]
async fn multi_turn_tool_loop_preserves_request_history_and_usage() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(env.cwd.join("b.txt"), "beta\n").unwrap();
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![
                    Part::Text("reading a".into()),
                    Part::ToolCall { id: "call_a".into(), name: "read".into(), args: json!({ "path": "a.txt" }) },
                    Part::Usage { input: 10, output: 5, cached: 0 },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![
                    Part::Text("reading b".into()),
                    Part::ToolCall { id: "call_b".into(), name: "read".into(), args: json!({ "path": "b.txt" }) },
                    Part::Usage { input: 20, output: 8, cached: 0 },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![
                    Part::Text("both read".into()),
                    Part::Usage { input: 30, output: 12, cached: 0 },
                    Part::Finish(Finish::Stop),
                ]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        let res = h.prompt("read a and b", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(res["text"], "both read", "{res}");
        assert_eq!(res["usage"]["input"], 60, "{res}");
        assert_eq!(res["usage"]["output"], 25, "{res}");
        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 3);
        let last = &turns[2];
        assert_eq!(last.calls.len(), 2, "{last:?}");
        assert_eq!(last.calls[0].1, "read");
        assert_eq!(last.calls[0].2["path"], "a.txt");
        assert_eq!(last.calls[1].1, "read");
        assert_eq!(last.calls[1].2["path"], "b.txt");
        assert_eq!(last.results.len(), 2, "{last:?}");
        assert!(last.results[0].1.contains("alpha"), "{last:?}");
        assert!(last.results[1].1.contains("beta"), "{last:?}");
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:628-698` (`does not abort sibling tools when a parallel batch tool
/// completes first`): a fast concurrency-safe tool finishing does not cancel a slower sibling
/// in the same parallel batch; both results reach the next model step in model order.
#[tokio::test]
async fn parallel_batch_fast_sibling_does_not_abort_slow_sibling() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![
                    Part::ToolCall { id: "call-fast-1".into(), name: "fast_tool".into(), args: json!({}) },
                    Part::ToolCall { id: "call-slow-1".into(), name: "slow_tool".into(), args: json!({}) },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![Part::Text("all done".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        exec(
            &h,
            r#"
            FAST_DONE, SLOW_STARTED, SLOW_CANCELLED = false, false, nil
            local gate_tx, gate_rx = mare.task.channel()
            RELEASE_SLOW = gate_tx
            T:register("fast_tool", {
                description = "fast",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                execute = function()
                    FAST_DONE = true
                    return "fast result"
                end,
            })
            T:register("slow_tool", {
                description = "slow",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                execute = function(_, tctx)
                    SLOW_STARTED = true
                    gate_rx:recv()
                    SLOW_CANCELLED = tctx.cancel:is_cancelled()
                    return "slow result"
                end,
            })
            "#,
        )
        .await;

        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("use both", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "fast_tool done and slow_tool started", || {
            let h = h.clone();
            async move { eval::<(bool, bool)>(&h, "return FAST_DONE, SLOW_STARTED").await == (true, true) }
        })
        .await;
        exec(&h, "RELEASE_SLOW:send(true)").await;
        let res = run.await.unwrap();
        assert_eq!(res["text"], "all done", "{res}");
        assert_eq!(eval::<Option<bool>>(&h, "return SLOW_CANCELLED").await, Some(false));
        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].results.len(), 2);
        assert_eq!(turns[1].results[0].1, "fast result");
        assert_eq!(turns[1].results[1].1, "slow result");
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:897-959` (`keeps the terminal real 503 after an earlier real 429`):
/// when attempt 1 fails with 429 (`rate_limit`) and attempt 2 fails with 503 (`overloaded`),
/// the terminal turn error reports the final 503 status/kind, not the earlier 429.
#[tokio::test]
async fn terminal_503_after_earlier_429_preserves_final_status_and_kind() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::HttpError {
                    status: 429,
                    retry_after: Some("0".into()),
                    message: "first attempt rate limited".into(),
                },
                Step::HttpError { status: 503, retry_after: None, message: "final attempt unavailable".into() },
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1, "retries": 1 }), false).await;
        exec(
            &h,
            r#"RETRIES = {}
               C.cordis:on("step/completed", function(e) if e.will_retry then RETRIES[#RETRIES + 1] = tostring(e.error_kind) end end)"#,
        )
        .await;
        let res = h.prompt("Hello", json!({})).await.unwrap();
        let retries: Vec<String> = eval(&h, "return RETRIES").await;
        assert_eq!(retries, vec!["rate_limit".to_string()]);
        assert_eq!(res["error_kind"], "server", "{res}");
        assert_eq!(res["termination_reason"], "error", "{res}");
        let err = res["error"]["message"].as_str().unwrap_or("");
        assert!(err.contains("503") && err.contains("final attempt unavailable"), "{res}");
        h.shutdown().await;
        assert_eq!(mock.requests().len(), 2);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:1094-1138` + steer insertion semantics: when two sequential (non-readonly)
/// tool calls `[t1, t2]` are returned and a `steer` arrives while `t1` is running, `t2` is
/// skipped without executing and the steer message is injected after the tool results before
/// the next LLM request.
#[tokio::test]
async fn steer_mid_tool_batch_skips_remaining_tools_and_injects_before_next_request() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![
                    Part::ToolCall { id: "call_t1".into(), name: "t1".into(), args: json!({}) },
                    Part::ToolCall { id: "call_t2".into(), name: "t2".into(), args: json!({}) },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![Part::Text("steered pivot".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        exec(
            &h,
            r#"
            T1_STARTED, T2_RUNS = false, 0
            local tx, rx = mare.task.channel()
            RELEASE_T1 = tx
            T:register("t1", {
                description = "t1",
                parameters = { type = "object", properties = {} },
                execute = function()
                    T1_STARTED = true
                    rx:recv()
                    return "t1 ok"
                end,
            })
            T:register("t2", {
                description = "t2",
                parameters = { type = "object", properties = {} },
                execute = function()
                    T2_RUNS = T2_RUNS + 1
                    return "t2 ok"
                end,
            })
            "#,
        )
        .await;

        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("go", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "t1 started", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T1_STARTED").await }
        })
        .await;
        h.steer("stop and pivot").await.unwrap();
        exec(&h, "RELEASE_T1:send(true)").await;
        let res = run.await.unwrap();
        assert_eq!(res["text"], "steered pivot", "{res}");
        assert_eq!(eval::<i64>(&h, "return T2_RUNS").await, 0, "t2 must be skipped after steer");
        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].results.len(), 2);
        assert_eq!(turns[1].results[0].1, "t1 ok");
        assert!(turns[1].results[1].1.contains("Skipped"), "{:?}", turns[1].results);
        assert!(turns[1].text.iter().any(|t| t == "stop and pivot"), "{:?}", turns[1].text);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:1094-1138, 1247-1339`: when both `follow_up` and `steer` are queued while
/// a tool is executing, `steer` runs inside the next step of the current turn and `follow_up`
/// waits until the current turn finishes before starting a new turn.
#[tokio::test]
async fn steer_and_follow_up_queued_together_respect_insertion_points() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![
                    Part::ToolCall { id: "call_w".into(), name: "work".into(), args: json!({}) },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![Part::Text("steered reply".into()), Part::Finish(Finish::Stop)]),
                Step::Reply(vec![Part::Text("followup reply".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        exec(
            &h,
            r#"
            WORK_STARTED = false
            local tx, rx = mare.task.channel()
            RELEASE_WORK = tx
            T:register("work", {
                description = "work",
                parameters = { type = "object", properties = {} },
                execute = function()
                    WORK_STARTED = true
                    rx:recv()
                    return "work done"
                end,
            })
            "#,
        )
        .await;

        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("initial", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "work started", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return WORK_STARTED").await }
        })
        .await;
        h.follow_up("after turn").await.unwrap();
        h.steer("mid turn").await.unwrap();
        exec(&h, "RELEASE_WORK:send(true)").await;
        let res = run.await.unwrap();
        assert_eq!(res["text"], "followup reply", "{res}");

        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 3);
        assert!(turns[1].text.iter().any(|t| t == "mid turn"), "{:?}", turns[1].text);
        assert!(
            !turns[1].text.iter().any(|t| t == "after turn"),
            "follow_up leaked into steer step: {:?}",
            turns[1].text
        );
        assert!(turns[2].text.iter().any(|t| t == "steered reply"), "{:?}", turns[2].text);
        assert!(turns[2].text.iter().any(|t| t == "after turn"), "{:?}", turns[2].text);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:1094-1138` (`queues consecutive nextTurn requests in FIFO order without
/// overlapping turns`): multiple follow-up prompts queued while the first turn is held do not
/// overlap the active turn and are delivered in strict FIFO order on the next step.
#[tokio::test]
async fn follow_up_fifo_order_without_overlapping_turns() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![Part::Text("one".into()), Part::Finish(Finish::Stop)]),
                Step::Reply(vec![Part::Text("two".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let release_first = Arc::new(tokio::sync::Notify::new());
        mock.hold(0, release_first.clone());
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;

        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("first", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "first request arrives", || {
            let n = mock.requests().len();
            async move { n == 1 }
        })
        .await;
        h.follow_up("second").await.unwrap();
        h.follow_up("third").await.unwrap();
        release_first.notify_one();

        let res = run.await.unwrap();
        assert_eq!(res["text"], "two", "{res}");
        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, vec!["first"]);
        assert_eq!(turns[1].text, vec!["first", "one", "second", "third"]);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:2094-2152, 2266-2285`: aborting mid-stream preserves the streamed partial
/// assistant text in history and the next prompt continues cleanly with valid request structure.
#[tokio::test]
async fn post_abort_mid_stream_preserves_partial_and_continues_cleanly() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::StallAfter {
                    parts: vec![Part::Text("partial answer".into()), Part::Text(" more".into())],
                    events: 3,
                    ms: 5000,
                },
                Step::Reply(vec![Part::Text("second answer".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        exec(
            &h,
            r#"GOT_DELTA = false
               C.cordis:on("item/delta", function(ev)
                   if ev.kind == "text" then GOT_DELTA = true end
               end)"#,
        )
        .await;

        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("Hello", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "first text_delta emitted", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return GOT_DELTA").await }
        })
        .await;
        h.abort().await.unwrap();
        let aborted = tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
        assert_eq!(aborted["error"]["code"], "aborted", "{aborted}");

        let next = h.prompt("Next", json!({})).await.unwrap();
        assert_eq!(next["text"], "second answer", "{next}");
        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 2);
        assert!(turns[1].text.iter().any(|t| t == "Hello"), "{:?}", turns[1].text);
        assert!(turns[1].text.iter().any(|t| t == "partial answer"), "{:?}", turns[1].text);
        assert_eq!(turns[1].text.last().map(String::as_str), Some("Next"), "{:?}", turns[1].text);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:2449-2523` (`does not duplicate recorded content when cancelled during
/// tool execution`): aborting during step-2 tool execution records every tool result once
/// without duplicating messages, and both the next prompt and session resume remain valid.
#[tokio::test]
async fn post_abort_during_tool_execution_records_results_once_and_resumes_cleanly() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::Reply(vec![
                    Part::Text("working".into()),
                    Part::ToolCall { id: "call-work-1".into(), name: "work".into(), args: json!({}) },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![
                    Part::Text("still working".into()),
                    Part::ToolCall { id: "call-work-2".into(), name: "work".into(), args: json!({}) },
                    Part::Finish(Finish::ToolCalls),
                ]),
                Step::Reply(vec![Part::Text("follow-up answer".into()), Part::Finish(Finish::Stop)]),
                Step::Reply(vec![Part::Text("resumed answer".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        exec(
            &h,
            r#"
            WORK_CALLS, SECOND_STARTED = 0, false
            T:register("work", {
                description = "work",
                parameters = { type = "object", properties = {} },
                execute = function(_, tctx)
                    WORK_CALLS = WORK_CALLS + 1
                    if WORK_CALLS == 1 then return "step1 ok" end
                    SECOND_STARTED = true
                    while not tctx.cancel:is_cancelled() do
                        mare.task.sleep(5)
                    end
                    return { content = "cancelled", is_error = true, cancelled = true }
                end,
            })
            "#,
        )
        .await;

        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("do work", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "second tool call started", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return SECOND_STARTED").await }
        })
        .await;
        h.abort().await.unwrap();
        let aborted = tokio::time::timeout(Duration::from_secs(5), run).await.unwrap().unwrap();
        assert_eq!(aborted["error"]["code"], "aborted", "{aborted}");

        let follow = h.prompt("again", json!({})).await.unwrap();
        assert_eq!(follow["text"], "follow-up answer", "{follow}");
        let result_ids: Vec<String> = eval(
            &h,
            r#"local ids = {}
               for _, m in ipairs(A:list_messages().data) do
                   if m.role == "tool_result" then ids[#ids + 1] = m.tool_call_id end
               end
               return ids"#,
        )
        .await;
        assert_eq!(result_ids, vec!["call-work-1", "call-work-2"]);
        h.shutdown().await;

        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), true).await;
        exec(
            &h,
            r#"T:register("work", {
                description = "work",
                parameters = { type = "object", properties = {} },
                execute = function() return "ok" end,
            })"#,
        )
        .await;
        let resumed = h.prompt("after resume", json!({})).await.unwrap();
        assert_eq!(resumed["text"], "resumed answer", "{resumed}");
        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 4);
        assert_eq!(turns[2].results.len(), 2, "req2={}", mock.requests()[2].body);
        assert_eq!(turns[3].results.len(), 2, "req3={}", mock.requests()[3].body);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:414-460`: when a request emits partial text and then hits a terminal
/// stream cut (`max_retries = 0`), the turn fails without retrying and the next prompt still
/// sends valid request history.
#[tokio::test]
async fn partial_output_then_stream_error_with_no_retry_keeps_history_valid_for_next_turn() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::CutAfter { parts: vec![Part::Text("partial before crash".into())], events: 2 },
                Step::Reply(vec![Part::Text("recovered".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1, "retries": 0 }), false).await;
        let err = h.prompt("Hello", json!({})).await.unwrap();
        assert_eq!(err["error_kind"], "stream_closed", "{err}");
        let ok = h.prompt("Continue", json!({})).await.unwrap();
        assert_eq!(ok["text"], "recovered", "{ok}");
        h.shutdown().await;

        let turns = mock.turns();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].text.last().map(String::as_str), Some("Continue"));
        mock.finish();
    })
    .await
}

/// Kimi `app/llmProtocol/errors.test.ts:464-478` (`parseRetryAfterMs`): `Retry-After: 0`
/// is honored as a `0ms` delay instead of falling back to `retry_base_ms`.
#[tokio::test]
async fn retry_after_zero_seconds_retries_without_exponential_backoff() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::HttpError { status: 429, retry_after: Some("0".into()), message: "rate limited".into() },
                Step::Reply(vec![Part::Text("ok".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 5000 }), false).await;
        exec(
            &h,
            r#"DELAYS = {}
               C.cordis:on("step/completed", function(e) if e.will_retry then DELAYS[#DELAYS + 1] = math.floor(e.delay_ms or -1) end end)"#,
        )
        .await;
        let t0 = Instant::now();
        let res = h.prompt("go", json!({})).await.unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(res["text"], "ok", "{res}");
        assert_eq!(eval::<Vec<i64>>(&h, "return DELAYS").await, vec![0]);
        assert!(elapsed < Duration::from_secs(2), "Retry-After: 0 must not sleep retry_base_ms: {elapsed:?}");
        h.shutdown().await;
        mock.finish();
    })
    .await
}

/// Kimi `app/llmProtocol/errors.test.ts:469-478`: a past HTTP-date `Retry-After` clamps to `0ms`,
/// whereas a non-numeric/invalid `Retry-After` header falls back to exponential backoff.
#[tokio::test]
async fn retry_after_past_http_date_and_invalid_header_edge_cases() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![
                Step::HttpError {
                    status: 429,
                    retry_after: Some("Wed, 21 Oct 2020 07:28:00 GMT".into()),
                    message: "past date".into(),
                },
                Step::HttpError {
                    status: 429,
                    retry_after: Some("not-a-delay".into()),
                    message: "invalid header".into(),
                },
                Step::Reply(vec![Part::Text("ok".into()), Part::Finish(Finish::Stop)]),
            ],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 200 }), false).await;
        exec(
            &h,
            r#"DELAYS = {}
               C.cordis:on("step/completed", function(e) if e.will_retry then DELAYS[#DELAYS + 1] = math.floor(e.delay_ms or -1) end end)"#,
        )
        .await;
        let res = h.prompt("go", json!({})).await.unwrap();
        assert_eq!(res["text"], "ok", "{res}");
        let delays: Vec<i64> = eval(&h, "return DELAYS").await;
        assert_eq!(delays.len(), 2, "{delays:?}");
        assert_eq!(delays[0], 0, "past HTTP-date must clamp to 0ms: {delays:?}");
        assert!(
            (350..=450).contains(&delays[1]),
            "invalid Retry-After on attempt 2 must fall back to backoff (~400ms): {delays:?}"
        );
        h.shutdown().await;
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:1053-1092`: when a tool returns `terminate = true`, the agent loop
/// commits the tool result and stops the turn without issuing another LLM request.
#[tokio::test]
async fn tool_stopping_turn_with_terminate_skips_further_steps() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![Step::Reply(vec![
                Part::Text("Stopping.".into()),
                Part::ToolCall { id: "call_stop".into(), name: "stopper".into(), args: json!({}) },
                Part::Finish(Finish::ToolCalls),
            ])],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1 }), false).await;
        exec(
            &h,
            r#"
            STOPPER_RUNS = 0
            T:register("stopper", {
                description = "stopper",
                parameters = { type = "object", properties = {} },
                execute = function()
                    STOPPER_RUNS = STOPPER_RUNS + 1
                    return { content = "stopped", terminate = true }
                end,
            })
            "#,
        )
        .await;
        let res = h.prompt("stop", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "{res}");
        assert_eq!(eval::<i64>(&h, "return STOPPER_RUNS").await, 1);
        h.shutdown().await;
        assert_eq!(mock.requests().len(), 1);
        mock.finish();
    })
    .await
}

/// Kimi `loop.test.ts:462-480`: cancelling a turn during retry backoff aborts immediately
/// without sleeping the remaining backoff or sending a second request.
#[tokio::test]
async fn abort_during_retry_backoff_cancels_immediately_without_second_request() {
    local(async {
        let mut env = TestEnv::new().await;
        let mock = ScriptedModel::start(
            Wire::Anthropic,
            vec![Step::HttpError { status: 429, retry_after: Some("5".into()), message: "rate limited".into() }],
        )
        .await;
        let h = scripted_host(&mut env, &mock, json!({ "retry_base_ms": 1, "retry_after_max_ms": 60000 }), false).await;
        exec(
            &h,
            r#"IN_BACKOFF = false
               C.cordis:on("step/completed", function(e) if e.will_retry then IN_BACKOFF = true end end)"#,
        )
        .await;
        let t0 = Instant::now();
        let h2 = h.clone();
        let run = tokio::task::spawn_local(async move { h2.prompt("Cancel during backoff", json!({})).await.unwrap() });
        wait_until(WAIT_SECS, "retry backoff entered", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return IN_BACKOFF").await }
        })
        .await;
        h.abort().await.unwrap();
        let res = tokio::time::timeout(Duration::from_secs(2), run).await.unwrap().unwrap();
        let elapsed = t0.elapsed();
        assert_eq!(res["error"]["code"], "aborted", "{res}");
        assert!(elapsed < Duration::from_secs(2), "must abort backoff without sleeping 5s: {elapsed:?}");
        h.shutdown().await;
        assert_eq!(mock.requests().len(), 1);
        mock.finish();
    })
    .await
}
