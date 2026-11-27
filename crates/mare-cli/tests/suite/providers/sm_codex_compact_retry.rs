//! ScriptedModel port (Codex): compaction & stream/retry edge cases, ported from
//! openai/codex `codex-rs/core/tests/suite/{client.rs,compact.rs,compact_remote.rs}` onto
//! `ScriptedModel` (and `MockServer` where `/responses/compact` is exercised). Each test cites
//! its upstream Codex `file:line`; tests assert outcomes (`termination_reason`, `error_kind`,
//! `usage`, `turns()`, `retries`, compacted history), never raw frames.

use super::*;
use crate::common::scripted::{Finish, Part, ScriptedModel, Step, Wire};

fn failed(error: Value) -> Step {
    Step::Raw(Resp::sse(sse_events(&[
        json!({ "type": "response.created", "response": { "id": "resp_f", "status": "in_progress" } }),
        json!({ "type": "response.failed", "response": { "id": "resp_f", "status": "failed", "error": error } }),
    ])))
}

fn http(status: u16, body: Value, retry_after: Option<&str>) -> Step {
    let mut r = Resp::status(status, body.to_string());
    if let Some(ra) = retry_after {
        r = r.header("retry-after", ra);
    }
    Step::Raw(r)
}

struct Outcome {
    res: Value,
    retries: Vec<(String, i64)>,
    requests: usize,
    usage_totals: (i64, i64),
}

async fn run_responses(steps: Vec<Step>, llm_cfg: Value) -> Outcome {
    let mut env = TestEnv::new().await;
    env.set_var("OPENAI_API_KEY", "test-openai");
    let mock = ScriptedModel::start(Wire::Responses, steps).await;
    let mut cfg = mock.host_config();
    merge(&mut cfg, json!({ "plugins": { "llm": llm_cfg } }));
    let h = env.host().builtins(&["usage"]).config(cfg).build().await;
    expose(&h);
    global(&h, "U", h.service_table("usage").unwrap());
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
    let usage_totals: (i64, i64) = eval(&h, "local u = U:totals(); return u.input or 0, u.output or 0").await;
    h.shutdown().await;
    let requests = mock.requests().len();
    mock.finish();
    Outcome { res, retries, requests, usage_totals }
}

/// Codex client.rs:3836 `usage_limit_error_emits_rate_limit_event`: a 429 `usage_limit_reached`
/// subscription limit is terminal (never retried despite 429) and surfaces the plan & kind.
#[tokio::test]
async fn usage_limit_reached_429_is_terminal_and_surfaces_plan() {
    local(async {
        let mut r = Resp::status(
            429,
            json!({
                "error": {
                    "type": "usage_limit_reached",
                    "message": "limit reached",
                    "resets_at": 1704067242,
                    "plan_type": "pro"
                }
            })
            .to_string(),
        );
        r = r
            .header("x-codex-primary-used-percent", "100.0")
            .header("x-codex-rate-limit-reached-type", "workspace_member_usage_limit_reached");
        let o = run_responses(vec![Step::Raw(r)], json!({ "retries": 2, "retry_base_ms": 1 })).await;
        assert_eq!(o.requests, 1, "usage_limit_reached must not be retried");
        assert!(o.retries.is_empty(), "{:?}", o.retries);
        assert_eq!(o.res["termination_reason"], "usage_limit", "{}", o.res);
        assert_eq!(o.res["error_kind"], "usage_limit", "{}", o.res);
        let err = o.res["error"]["message"].as_str().unwrap_or_default();
        assert!(err.contains("usage limit reached") && err.contains("pro plan"), "unexpected error: {err}");
    })
    .await
}

/// Codex client.rs:3933 `context_window_error_sets_total_tokens_to_model_window`: a streamed
/// `context_length_exceeded` failure is classified as `context_overflow` without retrying.
#[tokio::test]
async fn context_window_exceeded_stream_error_is_overflow_without_retry() {
    local(async {
        let err = json!({
            "code": "context_length_exceeded",
            "message": "Your input exceeds the context window of this model."
        });
        let o = run_responses(vec![failed(err)], json!({ "retries": 2, "retry_base_ms": 1 })).await;
        assert_eq!(o.requests, 1, "context_length_exceeded must not be retried");
        assert!(o.retries.is_empty(), "{:?}", o.retries);
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        assert_eq!(o.res["error_kind"], "context_overflow", "{}", o.res);
    })
    .await
}

/// Codex client.rs:4026 `incomplete_response_emits_content_filter_error_message`:
/// `response.incomplete` with `reason = "content_filter"` is a terminal error (not retried)
/// naming the reason.
#[tokio::test]
async fn incomplete_response_content_filter_is_terminal_error() {
    local(async {
        let o = run_responses(
            vec![Step::Reply(vec![Part::Text("partial content".into()), Part::Finish(Finish::ContentFilter)])],
            json!({ "retries": 2, "retry_base_ms": 1 }),
        )
        .await;
        assert_eq!(o.requests, 1, "content_filter incomplete response must not be retried");
        assert!(o.retries.is_empty(), "{:?}", o.retries);
        assert_eq!(o.res["termination_reason"], "error", "{}", o.res);
        let err = o.res["error"]["message"].as_str().unwrap_or_default();
        assert!(err.contains("Incomplete response returned, reason: content_filter"), "unexpected error: {err}");
    })
    .await
}

/// Codex client.rs:4087 `incomplete_response_is_terminal` (`sse_usage`):
/// `response.incomplete` with `reason = "max_output_tokens"` is terminal (`length`, not retried)
/// and records the reported usage.
#[tokio::test]
async fn incomplete_response_max_output_tokens_with_usage_is_terminal_length() {
    local(async {
        let o = run_responses(
            vec![Step::Reply(vec![
                Part::Text("Partial reply.".into()),
                Part::Usage { input: 120, output: 30, cached: 0 },
                Part::Finish(Finish::Length),
            ])],
            json!({ "retries": 2, "retry_base_ms": 1 }),
        )
        .await;
        assert_eq!(o.requests, 1, "max_output_tokens incomplete response must not be retried");
        assert!(o.retries.is_empty(), "{:?}", o.retries);
        assert_eq!(o.res["termination_reason"], "completed", "{}", o.res);
        assert_eq!(o.res["messages"][1]["stop_reason"], "length", "{}", o.res);
        assert_eq!(o.res["text"], "Partial reply.", "{}", o.res);
        assert_eq!(o.usage_totals, (120, 30), "usage should be recorded for incomplete response");
    })
    .await
}

/// Codex client.rs:4082-4084 `incomplete_response_is_terminal` (`sse_missing_usage`,
/// `sse_malformed_usage`): null or malformed usage on `response.incomplete` (`max_output_tokens`)
/// still completes cleanly as `length` without crashing or retrying.
#[tokio::test]
async fn incomplete_response_max_output_tokens_handles_null_and_malformed_usage() {
    local(async {
        for usage in [json!(null), json!({ "malformed": true })] {
            let body = sse_events(&[
                json!({ "type": "response.created", "response": { "id": "resp-inc", "status": "in_progress" } }),
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "Partial reply." }]
                    }
                }),
                json!({
                    "type": "response.incomplete",
                    "response": {
                        "id": "resp-inc",
                        "incomplete_details": { "reason": "max_output_tokens" },
                        "usage": usage
                    }
                }),
            ]);
            let o = run_responses(vec![Step::Raw(Resp::sse(body))], json!({ "retries": 2, "retry_base_ms": 1 })).await;
            assert_eq!(o.requests, 1);
            assert!(o.retries.is_empty());
            assert_eq!(o.res["termination_reason"], "completed", "{}", o.res);
            assert_eq!(o.res["messages"][1]["stop_reason"], "length", "{}", o.res);
            assert_eq!(o.res["text"], "Partial reply.", "{}", o.res);
            assert_eq!(o.usage_totals, (0, 0));
        }
    })
    .await
}

/// Codex client.rs:4493 `history_dedupes_streamed_and_final_messages_across_turns`:
/// when a response streams `output_text.delta` AND emits `output_item.done` with the full text,
/// subsequent turns must not duplicate the assistant message in history.
#[tokio::test]
async fn history_dedupes_streamed_deltas_and_final_message_across_turns() {
    local(async {
        let sse_body = sse_events(&[
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": { "id": "msg-1", "type": "message", "role": "assistant", "content": [] }
            }),
            json!({ "type": "response.output_text.delta", "item_id": "msg-1", "output_index": 0, "content_index": 0, "delta": "Hey " }),
            json!({ "type": "response.output_text.delta", "item_id": "msg-1", "output_index": 0, "content_index": 0, "delta": "there" }),
            json!({ "type": "response.output_text.delta", "item_id": "msg-1", "output_index": 0, "content_index": 0, "delta": "!\n" }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "msg-1",
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "Hey there!\n" }]
                }
            }),
            json!({ "type": "response.completed", "response": { "id": "resp1", "status": "completed" } }),
        ]);
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let mock = ScriptedModel::start(
            Wire::Responses,
            vec![
                Step::Raw(Resp::sse(sse_body.clone())),
                Step::Raw(Resp::sse(sse_body.clone())),
                Step::Raw(Resp::sse(sse_body)),
            ],
        )
        .await;
        let h = env.host().config(mock.host_config()).build().await;
        expose(&h);
        for prompt in ["U1", "U2", "U3"] {
            let res = h.prompt(prompt, json!({})).await.unwrap();
            assert_eq!(res["text"], "Hey there!\n", "{res}");
        }
        h.shutdown().await;
        let turns = mock.turns();
        assert_eq!(turns.len(), 3);
        let r3_assistant_texts: Vec<&String> =
            turns[2].text.iter().filter(|t| t.contains("Hey there!")).collect();
        assert_eq!(
            r3_assistant_texts,
            vec!["Hey there!\n", "Hey there!\n"],
            "streamed deltas + output_item.done must not duplicate assistant messages: {:?}",
            turns[2].text
        );
        mock.finish();
    })
    .await
}

/// Codex client.rs / retry_after.rs: an SSE `type: "error"` event with `code = "server_error"`
/// is retried and recovers on the next attempt.
#[tokio::test]
async fn stream_error_event_server_error_retries_and_recovers() {
    local(async {
        let err_event = Step::Raw(Resp::sse(sse_events(&[json!({
            "type": "error",
            "error": { "code": "server_error", "message": "temporary server error" }
        })])));
        let o = run_responses(
            vec![err_event, Step::Reply(vec![Part::Text("recovered".into())])],
            json!({ "retries": 1, "retry_base_ms": 1 }),
        )
        .await;
        assert_eq!(o.requests, 2);
        assert_eq!(o.retries.len(), 1, "{:?}", o.retries);
        assert_eq!(o.res["text"], "recovered", "{}", o.res);
    })
    .await
}

/// Codex compact_remote.rs:1521 `remote_compact_v2_rewrites_multiple_trailing_function_call_outputs`:
/// before `/responses/compact`, trailing tool outputs that push the input over `context_window`
/// are rewritten to the truncation notice, stopping at the first non-tool-output item so older
/// tool outputs before a user message remain intact.
#[tokio::test]
async fn remote_compact_trims_trailing_tool_outputs_to_window() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let server = MockServer::start().await;
        let compact_resp = json!({
            "output": [{ "type": "compaction", "encrypted_content": "ENC-TRIMMED" }]
        });
        server.route("/openai/responses/compact", vec![Resp::status(200, compact_resp.to_string())]);
        let h = env
            .host().builtins(&["compact", "usage"])
            .config(json!({
                "model": "openai/gpt-6.1-terra",
                "models": { "openai/gpt-6.1-terra": { "context_window": 20000, "max_output": 1000 } },
                "plugins": {
                    "provider-openai": { "base_url": format!("{}/openai", server.base) },
                    "llm": { "retry_base_ms": 1, "retries": 0 },
                    "compact": { "stages": ["remote"], "keep_recent_tokens": 100, "headroom_tokens": 1000 }
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"A:set_messages({
                { role = "user", content = { { type = "text", text = "first user" } } },
                { role = "assistant", content = { { type = "tool_call", id = "retained-call", name = "bash", arguments = { cmd = "ls" } } } },
                { role = "tool_result", tool_call_id = "retained-call", content = { { type = "text", text = "retained tool output" } } },
                { role = "user", content = { { type = "text", text = "second user" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "trim-1", name = "bash", arguments = { cmd = "a" } },
                    { type = "tool_call", id = "trim-2", name = "bash", arguments = { cmd = "b" } },
                } },
                { role = "tool_result", tool_call_id = "trim-1", content = { { type = "text", text = string.rep("x", 100000) } } },
                { role = "tool_result", tool_call_id = "trim-2", content = { { type = "text", text = string.rep("y", 100000) } } },
            })"#,
        )
        .await;
        let (ok, err): (bool, String) = eval(&h, "local ok, err = pcall(K.run, K); return ok and err.status == 'compacted', tostring(err)").await;
        assert!(ok, "remote compaction should succeed after trimming trailing tool outputs: {err}");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        let body: Value = serde_json::from_str(&reqs[0].body).unwrap();
        let input = body["input"].as_array().expect("compact input array");
        let output_for = |call_id: &str| -> String {
            input
                .iter()
                .find(|it| it["type"] == "function_call_output" && it["call_id"] == call_id)
                .and_then(|it| it["output"].as_str())
                .unwrap_or("")
                .to_string()
        };
        assert_eq!(output_for("retained-call"), "retained tool output", "older tool output before user msg must stay intact");
        assert_eq!(output_for("trim-1"), "Output exceeded the available model context and was truncated");
        assert_eq!(output_for("trim-2"), "Output exceeded the available model context and was truncated");
        h.shutdown().await;
    })
    .await
}

/// Codex compact.rs:4782 `manual_compact_retries_after_context_window_error`:
/// when a local summary request fails with HTTP 400 `context_length_exceeded`, `summarize`
/// drops the oldest history unit and retries until the summary succeeds.
#[tokio::test]
async fn manual_compact_retries_after_server_context_window_error() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let mock = ScriptedModel::start(
            Wire::Responses,
            vec![
                http(
                    400,
                    json!({
                        "error": {
                            "type": "invalid_request_error",
                            "code": "context_length_exceeded",
                            "message": "Your input exceeds the context window of this model."
                        }
                    }),
                    None,
                ),
                Step::Reply(vec![Part::Text("## Summary\nCOMPACT_OK".into())]),
            ],
        )
        .await;
        let mut cfg = mock.host_config();
        merge(
            &mut cfg,
            json!({
                "models": { "openai/gpt-6.1-terra": { "remote_compact": false } },
                "plugins": {
                    "llm": { "retries": 0, "retry_base_ms": 1 },
                    "compact": { "keep_recent_tokens": 50, "headroom_tokens": 1000 }
                }
            }),
        );
        let h = env.host().builtins(&["compact", "usage"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"A:set_messages({
                { role = "user", content = { { type = "text", text = "OLD-TURN-1" } } },
                { role = "assistant", content = { { type = "text", text = "OLD-REPLY-1 " .. string.rep("detail ", 100) } } },
                { role = "user", content = { { type = "text", text = "MID-TURN-2" } } },
                { role = "assistant", content = { { type = "text", text = "MID-REPLY-2 " .. string.rep("middle ", 100) } } },
                { role = "user", content = { { type = "text", text = "NEW-TURN-3 " .. string.rep("recent ", 50) } } },
                { role = "assistant", content = { { type = "text", text = "NEW-REPLY-3" } } },
            })"#,
        )
        .await;
        let (ok, err, summary): (bool, String, String) =
            eval(&h, "local ok, err = pcall(K.run, K); return ok and err.status == 'compacted', tostring(err), mare.json.encode(A:list_messages().data)").await;
        assert!(ok, "compaction should recover after dropping oldest unit: {err}");
        assert!(summary.contains("COMPACT_OK"), "{summary}");
        h.shutdown().await;
        let reqs = mock.requests();
        assert_eq!(reqs.len(), 2, "expected initial failed summary and retry after drop");
        assert!(reqs[0].body.contains("OLD-TURN-1"), "first attempt includes oldest unit");
        assert!(!reqs[1].body.contains("OLD-TURN-1"), "retry must drop oldest unit: {}", reqs[1].body);
        mock.finish();
    })
    .await
}

/// Codex compact.rs:4881 `manual_compact_non_context_failure_retries_then_emits_task_error` +
/// compact.rs:1391 `terminal_compaction_failure_preserves_usage_and_history`:
/// a transient `server_error` during local compaction retries within budget and, if exhausted,
/// fails cleanly while leaving the conversation history intact.
#[tokio::test]
async fn manual_compact_server_error_retries_then_preserves_history() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let err = json!({ "code": "server_error", "message": "temporary compact failure" });
        let mock = ScriptedModel::start(Wire::Responses, vec![failed(err.clone()), failed(err)]).await;
        let mut cfg = mock.host_config();
        merge(
            &mut cfg,
            json!({
                "models": { "openai/gpt-6.1-terra": { "remote_compact": false } },
                "plugins": {
                    "llm": { "retries": 1, "retry_base_ms": 1 },
                    "compact": { "keep_recent_tokens": 100, "headroom_tokens": 1000 }
                }
            }),
        );
        let h = env.host().builtins(&["compact", "usage"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"A:set_messages({
                { role = "user", content = { { type = "text", text = "KEEP-ME-USER" } } },
                { role = "assistant", content = { { type = "text", text = "KEEP-ME-ASSISTANT " .. string.rep("detail ", 500) } } },
                { role = "user", content = { { type = "text", text = "KEEP-ME-USER-2" } } },
                { role = "assistant", content = { { type = "text", text = "KEEP-ME-ASSISTANT-2" } } },
            })"#,
        )
        .await;
        let (ok, msgs): (bool, String) =
            eval(&h, "local ok, res = pcall(K.run, K); return ok and res.status == 'compacted', mare.json.encode(A:list_messages().data)").await;
        assert!(!ok, "compaction should fail after retries are exhausted");
        assert!(msgs.contains("KEEP-ME-USER") && msgs.contains("KEEP-ME-ASSISTANT"), "history must be preserved: {msgs}");
        h.shutdown().await;
        assert_eq!(mock.requests().len(), 2, "expected 1 initial attempt + 1 retry");
        mock.finish();
    })
    .await
}

/// Codex compact.rs:3884 `manual_local_compaction_retries_only_when_the_error_allows_it`:
/// a non-retryable 400 `invalid_request_error` during local compaction is never retried even when
/// `retries > 0`, and leaves history intact.
#[tokio::test]
async fn manual_compact_non_retryable_invalid_request_does_not_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        let mock = ScriptedModel::start(
            Wire::Responses,
            vec![http(
                400,
                json!({ "error": { "type": "invalid_request_error", "message": "invalid compact input" } }),
                None,
            )],
        )
        .await;
        let mut cfg = mock.host_config();
        merge(
            &mut cfg,
            json!({
                "models": { "openai/gpt-6.1-terra": { "remote_compact": false } },
                "plugins": {
                    "llm": { "retries": 2, "retry_base_ms": 1 },
                    "compact": { "keep_recent_tokens": 100, "headroom_tokens": 1000 }
                }
            }),
        );
        let h = env.host().builtins(&["compact", "usage"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "K", h.service_table("compact").unwrap());
        exec(
            &h,
            r#"A:set_messages({
                { role = "user", content = { { type = "text", text = "USER-1" } } },
                { role = "assistant", content = { { type = "text", text = "ASSISTANT-1 " .. string.rep("detail ", 500) } } },
                { role = "user", content = { { type = "text", text = "USER-2" } } },
                { role = "assistant", content = { { type = "text", text = "ASSISTANT-2" } } },
            })"#,
        )
        .await;
        let (ok, msgs): (bool, String) =
            eval(&h, "local ok, res = pcall(K.run, K); return ok and res.status == 'compacted', mare.json.encode(A:list_messages().data)").await;
        assert!(!ok);
        assert!(msgs.contains("USER-1") && msgs.contains("ASSISTANT-1"), "{msgs}");
        h.shutdown().await;
        assert_eq!(mock.requests().len(), 1, "400 invalid_request_error must not be retried");
        mock.finish();
    })
    .await
}
