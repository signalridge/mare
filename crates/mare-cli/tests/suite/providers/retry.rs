//! Retry policy: Retry-After, backoff, idle timeouts, cancellation and what is never retried.

use super::*;

#[tokio::test]
async fn rate_limit_is_retried_after_retry_after() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        server.route(
            "/anthropic",
            vec![Resp::status(429, err).header("retry-after", "0"), Resp::sse(fixture("anthropic.sse"))],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_fixture_message("anthropic", &msg, 5);
        assert_eq!(server.count("/anthropic"), 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn server_retry_delay_over_the_limit_fails_without_retrying() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        // the server asks for 100s; clamping to the cap would only hit the limit again (pi
        // provider-retry.ts:41-50): fail at once and say why
        server.route(
            "/google",
            vec![Resp::status(503, "{}").header("retry-after", "100"), Resp::sse(fixture("google.sse"))],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "google/gemini-2.5-pro").await;
        assert_eq!(msg["stop_reason"], "error", "{msg}");
        assert!(msg["error"].as_str().unwrap().contains("asked to wait 100s"), "{msg}");
        assert_eq!(server.count("/google"), 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn stream_cut_mid_message_is_retried_without_duplicating_text() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let full = fixture("openai-responses.sse");
        let cut = full.find("it now.").unwrap() + 40; // after the text, before the tool call
        server.route("/openai", vec![Resp::sse(full.clone()).cut(cut), Resp::sse(full)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "openai/gpt-6.1-terra").await;
        assert_fixture_message("openai", &msg, 0);
        let texts = msg["content"].as_array().unwrap().iter().filter(|b| b["type"] == "text").count();
        assert_eq!(texts, 1, "half-streamed text must not be committed twice: {msg}");
        assert_eq!(server.count("/openai"), 2);
        h.shutdown().await;
    })
    .await
}

// ---- provider and streaming boundary cases ---------------------------------

/// A shortened copy of a fixture without its final event block(s) matching `drop`.
fn without(fixture_name: &str, drop: &str) -> String {
    fixture(fixture_name).split("\n\n").filter(|b| !b.contains(drop)).collect::<Vec<_>>().join("\n\n")
}

#[tokio::test]
async fn streams_without_their_end_signal_are_retried() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        // Anthropic needs message_stop, compat a non-empty finish_reason ([DONE] alone isn't one),
        // Google a finishReason; anything less is a cut stream (pi utils/retry.ts, Codex error.rs)
        server.route(
            "/anthropic",
            vec![Resp::sse(without("anthropic.sse", "message_stop")), Resp::sse(fixture("anthropic.sse"))],
        );
        server.route(
            "/compat",
            vec![
                Resp::sse(without("openai-compat.sse", "finish_reason\":\"tool_calls")),
                Resp::sse(fixture("openai-compat.sse")),
            ],
        );
        let google_cut = fixture("google.sse").replace(",\"finishReason\":\"STOP\"", "");
        server.route("/google", vec![Resp::sse(google_cut), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        for (model, prefix, cw) in [
            ("anthropic/claude-sonnet-5-5", "/anthropic", 5),
            ("compat/reasoner-model", "/compat", 0),
            ("google/gemini-2.5-pro", "/google", 0),
        ] {
            let msg = stream(&h, model).await;
            assert_fixture_message(model, &msg, cw);
            assert_eq!(server.count(prefix), 2, "{model}");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_idle_stream_times_out_and_is_retried() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![Resp::sse(fixture("anthropic.sse")).stall(60, 5000), Resp::sse(fixture("anthropic.sse"))],
        );
        let h =
            provider_host(&mut env, &server, json!({ "plugins": { "llm": { "stream_idle_timeout_ms": 300 } } })).await;
        let t0 = std::time::Instant::now();
        let msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_fixture_message("anthropic", &msg, 5);
        assert_eq!(server.count("/anthropic"), 2);
        assert!(t0.elapsed() < std::time::Duration::from_secs(4), "the idle watchdog didn't fire: {:?}", t0.elapsed());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelling_a_stalled_stream_is_never_retried() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic.sse")).stall(60, 5000)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        // cancel once the server has the request (not after a fixed delay: on a loaded machine
        // the request may not even have gone out yet, and then nothing was cancelled mid-stream)
        let seen = server.counter("/anthropic");
        let seen = h.lua().create_function(move |_, ()| Ok(seen())).unwrap();
        h.lua().globals().set("SEEN", seen).unwrap();
        let v: mlua::Value = h
            .lua()
            .load(r#"
local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
local tok = mare.task.token()
mare.task.spawn(function()
  while SEEN() < 1 do mare.task.sleep(10) end
  mare.task.sleep(100)
  tok:cancel()
end)
return LLM:stream({ model = m, cancel = tok, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
"#)
            .eval_async()
            .await
            .unwrap();
        let msg = mare_shared::lua::lua_to_json(h.lua(), v).unwrap();
        assert_eq!(msg["stop_reason"], "aborted", "{msg}");
        assert_eq!(server.count("/anthropic"), 1, "a user cancel is never wrapped or retried");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn retry_after_ms_is_honored() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
        server.route(
            "/anthropic",
            vec![Resp::status(429, err).header("retry-after-ms", "30"), Resp::sse(fixture("anthropic.sse"))],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retry_base_ms": 30000 } } })).await;
        let t0 = std::time::Instant::now();
        let msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_fixture_message("anthropic", &msg, 5);
        assert!(t0.elapsed() < std::time::Duration::from_secs(5), "retry-after-ms not used: {:?}", t0.elapsed());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn x_should_retry_overrides_the_status() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let bad = r#"{"error":{"message":"try later"}}"#;
        server.route(
            "/compat",
            vec![Resp::status(400, bad).header("x-should-retry", "true"), Resp::sse(fixture("openai-compat.sse"))],
        );
        server.route("/google", vec![Resp::status(503, "{}").header("x-should-retry", "false")]);
        let h = provider_host(&mut env, &server, json!({})).await;
        assert_fixture_message("compat", &stream(&h, "compat/reasoner-model").await, 0);
        assert_eq!(server.count("/compat"), 2, "400 + x-should-retry: true is retried");
        let msg = stream(&h, "google/gemini-2.5-pro").await;
        assert_eq!(msg["stop_reason"], "error", "{msg}");
        assert_eq!(server.count("/google"), 1, "503 + x-should-retry: false is not");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn exhausted_quota_is_not_retried_but_a_per_minute_limit_is() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let quota = r#"{"error":{"code":"insufficient_quota","message":"You exceeded your current quota, check your plan and billing details"}}"#;
        server.route("/compat", vec![Resp::status(429, quota)]);
        let per_minute = r#"[{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded for metric generate_content_requests per minute"}}]"#;
        server.route("/google", vec![Resp::status(429, per_minute), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "compat/reasoner-model").await;
        assert_eq!(msg["error_kind"], "quota", "{msg}");
        assert_eq!(server.count("/compat"), 1);
        assert_fixture_message("google", &stream(&h, "google/gemini-2.5-pro").await, 0);
        assert_eq!(server.count("/google"), 2, "a per-minute quota is a rate limit");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn overloaded_is_retried_but_only_a_few_times() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let err = r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
        server.route("/anthropic", vec![Resp::status(529, err)]);
        // overloaded has its own, smaller budget than the general `retries`
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "llm": { "retries": 5, "overloaded_retries": 1 } } }),
        )
        .await;
        let msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(msg["error_kind"], "overloaded", "{msg}");
        assert_eq!(server.count("/anthropic"), 2, "1 + overloaded_retries (1), not 1 + retries");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_empty_answer_is_retried_unless_it_was_filtered() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let empty = compat_events(&[json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})]);
        let filtered = compat_events(&[json!({"choices":[{"index":0,"delta":{},"finish_reason":"content_filter"}]})]);
        server.route("/compat", vec![Resp::sse(empty), Resp::sse(fixture("openai-compat.sse")), Resp::sse(filtered)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        assert_fixture_message("compat", &stream(&h, "compat/reasoner-model").await, 0);
        assert_eq!(server.count("/compat"), 2, "an empty answer is retried");
        let msg = stream(&h, "compat/reasoner-model").await;
        assert_eq!(msg["stop_reason"], "filtered", "{msg}");
        assert_eq!(server.count("/compat"), 3, "a filtered empty answer is not retried");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn backoff_jitter_is_within_ten_percent() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::status(500, "{}"), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retry_base_ms": 100 } } })).await;
        exec(&h, r#"DELAYS = {}; mare.on("step/completed", function(e) if e.will_retry then DELAYS[#DELAYS + 1] = e.delay_ms end end)"#).await;
        assert_fixture_message("google", &stream(&h, "google/gemini-2.5-pro").await, 0);
        let d: f64 = eval(&h, "return DELAYS[1]").await;
        assert!((90.0..=110.0).contains(&d), "symmetric ±10% around 100ms: {d}");
        h.shutdown().await;
    })
    .await
}

/// An error response whose body can't be read (connection reset mid-body) is a retryable
/// transport failure, even for a status that would otherwise be final.
#[tokio::test]
async fn a_failed_error_body_read_is_retried() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let cut = Resp::status(400, r#"{"error":{"message":"bad"#).header("content-length", "1000").cut(10);
        server.route("/compat", vec![cut, Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        assert_fixture_message("compat", &stream(&h, "compat/reasoner-model").await, 0);
        assert_eq!(server.count("/compat"), 2);
        h.shutdown().await;
    })
    .await
}

/// Retryable status codes found in error text match as whole words only (pi #11057): not
/// inside a request id, a byte count or a longer number.
#[tokio::test]
async fn retryable_status_codes_in_messages_match_whole_words() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let cases: Vec<(String, bool)> = eval::<mlua::Table>(
            &h,
            r#"
local out = {}
for _, m in ipairs({
  "upstream answered 408 Request Timeout", "HTTP 409: conflict, try again", "(409)",
  "request id req_4089abc failed", "wrote 14080 bytes", "error 5290 in module", "code=4091",
}) do
  local _, retry = LLM:classify({ message = m })
  out[#out + 1] = { m, retry }
end
return out"#,
        )
        .await
        .sequence_values::<mlua::Table>()
        .map(|t| {
            let t = t.unwrap();
            (t.get(1).unwrap(), t.get(2).unwrap())
        })
        .collect();
        let retried: Vec<&str> = cases.iter().filter(|(_, r)| *r).map(|(m, _)| m.as_str()).collect();
        assert_eq!(
            retried,
            ["upstream answered 408 Request Timeout", "HTTP 409: conflict, try again", "(409)"],
            "{cases:?}"
        );
        // and by status: a 409 response is retried
        server.route(
            "/compat",
            vec![Resp::status(409, r#"{"error":{"message":"busy"}}"#), Resp::sse(fixture("openai-compat.sse"))],
        );
        assert_fixture_message("compat", &stream(&h, "compat/reasoner-model").await, 0);
        assert_eq!(server.count("/compat"), 2);
        h.shutdown().await;
    })
    .await
}

/// A compatible endpoint that ends the stream with `finish_reason: "network_error"` failed in
/// transit: retried (pi openai-completions.ts:1579), and the partial text isn't kept twice.
#[tokio::test]
async fn compat_network_error_finish_is_retried() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let broken = compat_events(&[
            compat_text("Reading"),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"network_error"}]}),
        ]);
        server.route("/compat", vec![Resp::sse(broken), Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "compat/reasoner-model").await;
        assert_fixture_message("compat", &msg, 0);
        let texts = msg["content"].as_array().unwrap().iter().filter(|b| b["type"] == "text").count();
        assert_eq!(texts, 1, "{msg}");
        assert_eq!(server.count("/compat"), 2);
        h.shutdown().await;
    })
    .await
}

/// A text-only stream on one wire, its frames split at `stall_at` (the byte after the frame
/// carrying `text`'s delta).
fn text_stream(wire: &str, text: &str) -> (String, usize) {
    let frames: Vec<String> = match wire {
        "openai" => vec![
            json!({"type":"response.created","response":{"id":"resp_1","status":"in_progress"}}),
            json!({"type":"response.output_item.added","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[]}}),
            json!({"type":"response.output_text.delta","item_id":"msg_1","delta":text}),
            json!({"type":"response.output_item.done","output_index":0,"item":{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}}),
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","usage":{"input_tokens":5,"output_tokens":2}}}),
        ],
        _ => vec![
            json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"usage":{"input_tokens":5,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            json!({"type":"message_stop"}),
        ],
    }
    .into_iter()
    .map(|f| format!("event: {}\ndata: {f}\n\n", f["type"].as_str().unwrap()))
    .collect();
    let stall_at = frames[..3].iter().map(String::len).sum();
    (frames.concat(), stall_at)
}

/// the text a
/// failed attempt already streamed reaches the sink before the failure, the retry tells the UI
/// to reset it (`llm/retry.reset`), and only the retried answer is kept. The first response
/// delivers one complete text delta and then goes silent until the idle watchdog fires.
#[tokio::test]
async fn text_streamed_before_an_idle_failure_reaches_the_sink_and_is_reset_on_retry() {
    for (wire, model) in [("openai", "openai/gpt-6.1-terra"), ("anthropic", "anthropic/claude-sonnet-5-5")] {
        local(async {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let (first, stall_at) = text_stream(wire, "FIRST_PARTIAL");
            let (retried, _) = text_stream(wire, "FINAL");
            // wire_server: b — silent after the first text delta (an idle stream, not a cut)
            server.route(&format!("/{wire}"), vec![Resp::sse(first).stall(stall_at, 5000), Resp::sse(retried)]);
            let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "stream_idle_timeout_ms": 300 } } }))
                .await;
            expose(&h);
            let (seen, text): (String, String) = eval(
                &h,
                &format!(
                    r#"local seen = {{}}
                       C.cordis:on("step/completed", function(ev) if ev.will_retry then seen[#seen + 1] = "retry reset=" .. tostring(ev.reset) end end)
                       local m = assert(LLM:resolve("{model}"))
                       local msg = LLM:stream({{ model = m, system = "S", turn_id = "turn-1", step_id = "step-1",
                           messages = {{ {{ role = "user", content = {{ {{ type = "text", text = "hi" }} }} }} }} }},
                           function(ev) if ev.type == "text_delta" then seen[#seen + 1] = "text " .. ev.delta end end)
                       local text = {{}}
                       for _, b in ipairs(msg.content or {{}}) do if b.type == "text" then text[#text + 1] = b.text end end
                       return table.concat(seen, " | "), table.concat(text)"#
                ),
            )
            .await;
            assert_eq!(server.count(&format!("/{wire}")), 2, "{wire}: one idle failure, one retry");
            assert_eq!(
                seen, "text FIRST_PARTIAL | retry reset=true | text FINAL",
                "{wire}: the failed attempt's text must reach the sink before the retry resets it"
            );
            assert_eq!(text, "FINAL", "{wire}: the half-streamed text must not be kept");
            h.shutdown().await;
        })
        .await;
    }
}

#[tokio::test]
async fn retry_emits_reset_true_after_partial_stream_and_false_before_output() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let full = fixture("openai-responses.sse");
        let cut = full.find("it now.").unwrap() + 40;
        server.route("/openai", vec![Resp::sse(full.clone()).cut(cut), Resp::sse(full)]);
        server.route("/google", vec![Resp::status(500, "{}"), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"
RESETS = {}
mare.on("step/completed", function(e) if e.will_retry then RESETS[#RESETS + 1] = e.reset end end)
"#,
        )
        .await;
        assert_fixture_message("openai", &stream(&h, "openai/gpt-6.1-terra").await, 0);
        assert_fixture_message("google", &stream(&h, "google/gemini-2.5-pro").await, 0);
        let resets: Vec<bool> = eval(&h, "return RESETS").await;
        assert_eq!(resets, [true, false], "partial stream sets reset=true; pre-output error sets reset=false");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn persist_failure_in_on_attempt_stops_without_retrying() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::status(500, "{}"), Resp::sse(fixture("google.sse"))]);
        server.route("/compat", vec![Resp::status(500, "{}"), Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let v_false: mlua::Value = eval(
            &h,
            r#"
local m = assert(LLM:resolve("google/gemini-2.5-pro"))
return LLM:stream({
    model = m,
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
    on_attempt = function() return false end,
}, function() end)
"#,
        )
        .await;
        let msg_false = mare_shared::lua::lua_to_json(h.lua(), v_false).unwrap();
        assert_eq!(msg_false["stop_reason"], "error", "{msg_false}");
        assert_eq!(msg_false["error_kind"], "persist", "{msg_false}");
        assert_eq!(server.count("/google"), 1, "on_attempt returning false must not send a second request");

        let v_err: mlua::Value = eval(
            &h,
            r#"
local m = assert(LLM:resolve("compat/reasoner-model"))
return LLM:stream({
    model = m,
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
    on_attempt = function() error("disk full") end,
}, function() end)
"#,
        )
        .await;
        let msg_err = mare_shared::lua::lua_to_json(h.lua(), v_err).unwrap();
        assert_eq!(msg_err["stop_reason"], "error", "{msg_err}");
        assert_eq!(msg_err["error_kind"], "persist", "{msg_err}");
        assert_eq!(server.count("/compat"), 1, "on_attempt throwing must not send a second request");
        h.shutdown().await;
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn llm_exponential_backoff_sequence_is_pinned_with_paused_time() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/google",
            vec![
                Resp::status(500, "{}"),
                Resp::status(500, "{}"),
                Resp::status(500, "{}"),
                Resp::sse(fixture("google.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "llm": { "retry_base_ms": 100, "retry_max_ms": 250, "retries": 3 } } }),
        )
        .await;
        exec(&h, r#"DELAYS = {}; mare.on("step/completed", function(e) if e.will_retry then DELAYS[#DELAYS + 1] = e.delay_ms end end)"#).await;
        assert_fixture_message("google", &stream(&h, "google/gemini-2.5-pro").await, 0);
        assert_eq!(server.count("/google"), 4);
        let delays: Vec<f64> = eval(&h, "return DELAYS").await;
        assert_eq!(delays.len(), 3, "{delays:?}");
        assert!((90.0..=110.0).contains(&delays[0]), "attempt 1: {delays:?}");
        assert!((180.0..=220.0).contains(&delays[1]), "attempt 2: {delays:?}");
        assert!((225.0..=250.0).contains(&delays[2]), "attempt 3 clamped at cap: {delays:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn prepare_backoff_sequence_and_budget_are_pinned_with_paused_time() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let times = Arc::new(std::sync::Mutex::new(Vec::<tokio::time::Instant>::new()));
        let times_srv = times.clone();
        let auth_srv = tokio::spawn(async move {
            let err_body = r#"{"error":"temporarily_unavailable"}"#;
            let ok_body = r#"{"access_token":"new-access","refresh_token":"r2","expires_in":3600}"#;
            let err_resp = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{err_body}",
                err_body.len()
            );
            let ok_resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ok_body}",
                ok_body.len()
            );
            let responses = [
                err_resp.as_str(),
                err_resp.as_str(),
                ok_resp.as_str(),
                // Second prepare() call: 4x 503 to verify RETRIES=2 stops after 3 attempts (1 + 2).
                err_resp.as_str(),
                err_resp.as_str(),
                err_resp.as_str(),
                err_resp.as_str(),
            ];
            for resp in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                times_srv.lock().unwrap().push(tokio::time::Instant::now());
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });
        let base = server.base.clone();
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": { "codex_base_url": format!("{base}/codex"), "auth_base_url": format!("http://{addr}") } } }),
        )
        .await;
        let ok: bool = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, ["openai-codex"] = { type = "oauth", access = "old", refresh = "r1", expires = 0, account_id = "acct" } }), { mode = 384 })
local ok = LLM:prepare(assert(LLM:resolve("openai/gpt-6.1-terra")))
return ok == true
"#,
        )
        .await;
        assert!(ok, "prepare must succeed after 2 retries");
        let seen1 = {
            let mut guard = times.lock().unwrap();
            let v = guard.clone();
            guard.clear();
            v
        };
        assert_eq!(seen1.len(), 3, "1 initial + 2 retries: {seen1:?}");
        let d1 = seen1[1].duration_since(seen1[0]);
        let d2 = seen1[2].duration_since(seen1[1]);
        assert!(d1 >= std::time::Duration::from_millis(250) && d1 < std::time::Duration::from_millis(700), "first interval ~250ms: {d1:?}");
        assert!(d2 >= std::time::Duration::from_millis(1000) && d2 < std::time::Duration::from_millis(1600), "second interval ~1000ms: {d2:?}");
        assert!(d2 > d1 * 2, "second interval must reflect 4x exponential backoff over first: d1={d1:?}, d2={d2:?}");

        // Counter-example: exceeding RETRIES = 2 stops retrying after 3 requests (1 initial + 2 retries).
        let ok_exhausted: bool = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, ["openai-codex"] = { type = "oauth", access = "old", refresh = "r1", expires = 0, account_id = "acct" } }), { mode = 384 })
local ok = LLM:prepare(assert(LLM:resolve("openai/gpt-6.1-terra")))
return ok == true
"#,
        )
        .await;
        assert!(ok_exhausted, "prepare proceeds with cached token after exhausting RETRIES=2");
        let seen2 = times.lock().unwrap().clone();
        assert_eq!(seen2.len(), 3, "exhausting RETRIES=2 must stop after 3 requests (1 initial + 2 retries): {seen2:?}");
        auth_srv.abort();
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn insufficient_quota_is_classified_as_quota_and_openai_compact_retries_401_and_redacts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let quota_body = r#"{"error":{"type":"insufficient_quota","code":"insufficient_quota","message":"quota gone Bearer sk-ABCDEFGHIJKLMNOPQRSTUVWX"}}"#;
        server.route(
            "/openai/responses/compact",
            vec![Resp::status(502, "bad gateway Bearer sk-ABCDEFGHIJKLMNOPQRSTUVWX")],
        );
        server.route("/openai/responses", vec![Resp::status(429, quota_body)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(msg["stop_reason"], "error");
        assert_eq!(msg["error_kind"], "quota", "insufficient_quota must classify as quota: {msg}");
        assert!(!msg["error"].as_str().unwrap_or("").contains("sk-ABC"), "API error must redact secret: {msg}");

        // /responses/compact redacts error snippets cleanly
        let (ok, err, kind): (bool, String, String) = eval(
            &h,
            r#"
            local ok, res, kind = LLM:compact_remote({
                model = assert(LLM:resolve("openai/gpt-6.1-terra")),
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            })
            return ok ~= nil, tostring(res), tostring(kind)
            "#,
        )
        .await;
        assert!(!ok);
        assert_eq!(kind, "server");
        assert!(!err.contains("sk-ABC"), "compact error must redact secrets: {err}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_lowers_tool_choice_none_and_rejects_empty_function_call_name() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let bad_sse = "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"functionCall\":{\"args\":{\"x\":1}}}]},\"finishReason\":\"STOP\"}]}\n\n";
        server.route("/google", vec![Resp::sse(fixture("google.sse")), Resp::sse(bad_sse)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let ok: bool = eval(
            &h,
            r#"
            local res = LLM:complete({
                model = assert(LLM:resolve("google/gemini-2.5-pro")),
                tool_choice = "none",
                messages = { { role = "user", content = { { type = "text", text = "summarize" } } } },
            })
            return res.stop_reason ~= "error"
            "#,
        )
        .await;
        assert!(ok);
        let req = server.requests().into_iter().find(|r| r.path.contains("/google")).unwrap();
        let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "NONE", "{body}");

        // Reject missing fc.name cleanly as error
        let msg = stream(&h, "google/gemini-2.5-pro").await;
        assert_eq!(msg["stop_reason"], "error", "empty/nil fc.name must fail: {msg}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_clamps_budget_thinking_to_max_output_and_reply_cap() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let cap_err = r#"{"type":"error","error":{"type":"invalid_request_error","message":"input length and max_tokens exceed context limit: 1000 + 4000 > 3048"}}"#;
        server.route("/anthropic", vec![Resp::status(400, cap_err), Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let (ok, why): (bool, String) = eval(
            &h,
            r#"
            local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            m.max_output = 4000
            m.adaptive = false
            local res = LLM:complete({
                model = m,
                effort = "high",
                messages = { { role = "user", content = { { type = "text", text = "think" } } } },
            })
            return res.stop_reason == "stop", mare.json.encode(res)
            "#,
        )
        .await;
        assert!(ok, "{why}");
        let reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path.starts_with("/anthropic")).collect();
        assert_eq!(reqs.len(), 2);
        let b1: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        let b2: serde_json::Value = serde_json::from_str(&reqs[1].body).unwrap();
        assert!(b1["max_tokens"].as_i64().unwrap() <= 4000, "{b1}");
        assert!(b1["thinking"]["budget_tokens"].as_i64().unwrap() < b1["max_tokens"].as_i64().unwrap(), "{b1}");
        assert!(b2["max_tokens"].as_i64().unwrap() <= 2048, "{b2}");
        assert!(b2["thinking"]["budget_tokens"].as_i64().unwrap() < b2["max_tokens"].as_i64().unwrap(), "{b2}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn history_defers_textified_removed_results_and_send_forwards_ws_timeouts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);
        let (roles, ws_ok): (Vec<String>, bool) = eval(
            &h,
            r#"
            local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local transformed = LLM:transform_messages({
                { role = "user", content = { { type = "text", text = "go" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "c_removed", name = "old_tool", arguments = {} },
                    { type = "tool_call", id = "c_kept", name = "ls", arguments = {} },
                } },
                { role = "tool_result", tool_call_id = "c_removed", content = { { type = "text", text = "removed out" } } },
                { role = "tool_result", tool_call_id = "c_kept", content = { { type = "text", text = "kept out" } } },
            }, m, { { name = "ls", parameters = { type = "object" } } })
            local roles = {}
            for _, msg in ipairs(transformed) do
                roles[#roles + 1] = msg.role
            end

            local seen = nil
            C.cordis:on("provider/transport", function(ev)
                seen = { idle = ev.idle_timeout_ms, conn = ev.connect_timeout_ms }
                return { aborted = true, message = "probe" }
            end)
            pcall(function()
                LLM:complete({
                    model = m,
                    idle_timeout_ms = 12345,
                    messages = { { role = "user", content = { { type = "text", text = "ws" } } } },
                })
            end)
            return roles, seen ~= nil and seen.idle == 12345
            "#,
        )
        .await;
        assert_eq!(roles, vec!["user", "assistant", "tool_result", "user"], "textified removed result must trail contiguous tool_results: {roles:?}");
        assert!(ws_ok, "provider/transport must receive idle_timeout_ms");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn singleflight_refresh_upgrades_force_when_forced_caller_joins_non_force_flight() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let (c1, c2, calls): (String, String, i64) = eval(
            &h,
            r#"
            local future_ms = mare.task.now() + 3600 * 1000
            mare.fs.write(mare.paths.auth, mare.json.encode({
                version = 1,
                ["openai-codex"] = { type = "oauth", access = "tok-0", refresh = "r0", expires = future_ms, account_id = "acct" }
            }), { mode = 384 })
            local entered_tx, entered_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            local calls = 0
            local function refresh_fn(cur)
                calls = calls + 1
                if calls == 1 then
                    entered_tx:send(true)
                    rel_rx:recv()
                end
                return { type = "oauth", access = "tok-" .. calls, refresh = "r" .. calls, expires = future_ms, account_id = "acct" }
            end
            -- Task 1 starts an initial forced/expired refresh returning tok-1 (simulating a flight returning tok-1
            -- when initial for Task 2 is tok-1, OR Task 1 non-force when expired -> valid concurrently).
            -- Simulate non-force flight that sees another valid token (tok-0) while holding lock, joined by forced caller:
            local lock_entered_tx, lock_entered_rx = mare.task.channel()
            local lock_rel_tx, lock_rel_rx = mare.task.channel()
            local t1 = mare.task.spawn(function()
                return LLM:refresh_credential("openai", "openai-codex", refresh_fn, {
                    valid = function()
                        return false
                    end,
                    force = false,
                })
            end)
            -- Let t1 acquire flight and reach refresh_fn or second valid() via direct file lock contention:
            -- Simpler deterministic race: t1 waits in refresh_fn producing tok-1, while t2 with initial=tok-0 joins with force=true,
            -- and we also verify a forced caller joining an already-dispatched flight whose result equals its rejected initial retries!
            entered_rx:recv()
            local t2 = mare.task.spawn(function()
                return LLM:refresh_credential("openai", "openai-codex", refresh_fn, {
                    valid = function() return true end,
                    force = true,
                })
            end)
            mare.task.sleep(10)
            rel_tx:send(true)
            local r1 = t1:wait()
            local r2 = t2:wait()
            -- Now verify joining a non-force flight that skips refresh because another process wrote a valid (but rejected) token:
            -- If t3 non-force sees valid=false initially, enters flight, then t4 force=true joins before t3 checks valid(current):
            -- flight.force was upgraded to true so refresh_fn runs!
            local auth_before = mare.json.encode({
                version = 1,
                ["openai-codex"] = { type = "oauth", access = "rejected-tok", refresh = "rx", expires = future_ms, account_id = "acct" }
            })
            mare.fs.write(mare.paths.auth, auth_before, { mode = 384 })
            local l = mare.lock.file(mare.paths.auth .. ".lock", { timeout_ms = 5000 })
            local flip = 0
            local t3 = mare.task.spawn(function()
                return LLM:refresh_credential("openai", "openai-codex", refresh_fn, {
                    valid = function(c)
                        flip = flip + 1
                        -- outer check (flip==1) returns false so t3 enters flight; inner check after lock returns true!
                        return flip > 1
                    end,
                    force = false,
                })
            end)
            mare.task.sleep(20)
            local t4 = mare.task.spawn(function()
                return LLM:refresh_credential("openai", "openai-codex", refresh_fn, {
                    valid = function() return true end,
                    force = true,
                })
            end)
            mare.task.sleep(20)
            l:release()
            local r3 = t3:wait()
            local r4 = t4:wait()
            return r3.access, r4.access, calls
            "#,
        )
        .await;
        assert_ne!(c1, "rejected-tok", "upgraded flight must refresh past rejected-tok: c1={c1}, c2={c2}");
        assert_ne!(c2, "rejected-tok", "forced caller must never receive stale rejected token: c2={c2}");
        assert!(calls >= 2, "expected forced refresh invocation, calls={calls}");
        h.shutdown().await;
    })
    .await
}

/// Non-rate-limit errors must not parse "try again in ..." from the error body, while
/// genuine rate-limit errors still honor it.
#[tokio::test]
async fn non_rate_limit_error_does_not_parse_try_again_in_from_body() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse_server_err = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"Temporary internal failure, please try again in 90s\"}}\n\n";
        let sse_rate_limit = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"Rate limited, please try again in 90s\"}}\n\n";
        server.route(
            "/anthropic",
            vec![
                Resp::sse(sse_server_err),
                Resp::sse(fixture("anthropic.sse")),
                Resp::sse(sse_rate_limit),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "llm": { "retry_base_ms": 20, "retry_after_max_ms": 5000 } } }),
        )
        .await;
        let msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_fixture_message("anthropic", &msg, 5);
        assert_eq!(server.count("/anthropic"), 2, "non-rate-limit stream error must ignore 90s text and retry");

        let rl = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(rl["stop_reason"], "error", "{rl}");
        assert!(
            rl["error"].as_str().unwrap_or("").contains("asked to wait 90s"),
            "rate-limit stream error must still honor try-again-in text: {rl}"
        );
        assert_eq!(server.count("/anthropic"), 3, "rate-limit error over delay cap must fail without retrying");
        h.shutdown().await;
    })
    .await
}

/// Response-header timeout fires when a server stalls before sending
/// headers, and compat + Google streams both retry when their SSE body stalls past the idle cap.
#[tokio::test]
async fn response_header_timeout_and_compat_google_idle_streams_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let hold_headers = std::sync::Arc::new(tokio::sync::Notify::new());
        server.route(
            "/openai",
            vec![
                Resp::sse(fixture("openai-responses.sse")).hold(hold_headers.clone()),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        server.route(
            "/compat",
            vec![Resp::sse(fixture("openai-compat.sse")).stall(40, 5000), Resp::sse(fixture("openai-compat.sse"))],
        );
        server
            .route("/google", vec![Resp::sse(fixture("google.sse")).stall(40, 5000), Resp::sse(fixture("google.sse"))]);
        let h =
            provider_host(&mut env, &server, json!({ "plugins": { "llm": { "stream_idle_timeout_ms": 200 } } })).await;

        let oa = stream(&h, "openai/gpt-6.1-terra").await;
        assert_fixture_message("openai", &oa, 0);
        assert_eq!(server.count("/openai"), 2, "stalled response headers must time out and retry");
        hold_headers.notify_one();

        let compat = stream(&h, "compat/reasoner-model").await;
        assert_fixture_message("compat", &compat, 0);
        assert_eq!(server.count("/compat"), 2, "stalled compat stream must time out and retry");

        let google = stream(&h, "google/gemini-2.5-pro").await;
        assert_fixture_message("google", &google, 0);
        assert_eq!(server.count("/google"), 2, "stalled Google stream must time out and retry");
        h.shutdown().await;
    })
    .await
}

/// HTTP 408 status is classified as retryable by status code alone, even when
/// the response body contains no timeout wording.
#[tokio::test]
async fn http_408_without_timeout_text_is_retried() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![Resp::status(408, r#"{"error":{"message":"opaque"}}"#), Resp::sse(fixture("openai-compat.sse"))],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "compat/reasoner-model").await;
        assert_fixture_message("compat", &msg, 0);
        assert_eq!(server.count("/compat"), 2, "HTTP 408 must retry by status code alone");
        h.shutdown().await;
    })
    .await
}
