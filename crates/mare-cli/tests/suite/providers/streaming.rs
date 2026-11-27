//! Stream parsing: SSE framing, caps, heartbeats, tool-call assembly and stop reasons.

use super::*;

/// A response cut off at the output limit stays "length" even though it holds a tool call, and
/// the call never runs: its arguments may be incomplete.
#[tokio::test]
async fn truncated_response_with_a_tool_call_never_runs_it() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let marker = env.cwd.join("written");
        let args = json!({ "path": marker, "content": "x" }).to_string();
        let truncated = responses_event(json!({ "type": "response.output_item.done", "item": {
                "type": "function_call", "call_id": "call_w", "name": "write", "arguments": args } }))
            + &responses_event(json!({ "type": "response.incomplete", "response": {
                "incomplete_details": { "reason": "max_output_tokens" }, "usage": {} } }));
        server.route("/openai", vec![Resp::sse(truncated), Resp::sse(responses_text("ok"))]);
        let h = provider_host(&mut env, &server, json!({ "model": "openai/gpt-6.1-terra" })).await;
        h.prompt("write it", json!({})).await.unwrap();
        assert!(!marker.exists(), "a tool call from a truncated response ran");
        let next = server.requests().pop().unwrap().json().to_string();
        assert!(next.contains("output token limit"), "the model must learn why: {next}");
        h.shutdown().await;
    })
    .await
}

/// The same on the other three wires (provider corpus): Anthropic `stop_reason:
/// max_tokens` with a complete `tool_use` block, Google `finishReason: MAX_TOKENS` with a
/// `functionCall`, Chat Completions `finish_reason: length` with complete `tool_calls`. Each
/// call's arguments are whole and valid, so only the stop reason keeps it from running; the
/// next request tells the model why. All wires run before the verdict.
#[tokio::test]
async fn truncated_response_with_a_tool_call_never_runs_it_on_every_wire() {
    let mut problems = Vec::new();
    for wire in ["anthropic", "google", "compat"] {
        local(async {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            let marker = env.cwd.join(format!("written-{wire}"));
            let args = json!({ "path": marker, "content": "x" });
            let anthropic = |events: &[Value]| -> String {
                events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
            };
            let start = json!({ "type": "message_start", "message": { "id": "m1", "type": "message",
                "role": "assistant", "content": [], "usage": { "input_tokens": 5, "output_tokens": 1 } } });
            let (route, model, truncated, answer) = match wire {
                "anthropic" => (
                    "/anthropic",
                    "anthropic/claude-sonnet-5-5",
                    anthropic(&[
                        start.clone(),
                        json!({ "type": "content_block_start", "index": 0, "content_block": {
                            "type": "tool_use", "id": "toolu_w", "name": "write", "input": {} } }),
                        json!({ "type": "content_block_delta", "index": 0, "delta": {
                            "type": "input_json_delta", "partial_json": args.to_string() } }),
                        json!({ "type": "content_block_stop", "index": 0 }),
                        json!({ "type": "message_delta", "delta": { "stop_reason": "max_tokens" },
                            "usage": { "output_tokens": 20 } }),
                        json!({ "type": "message_stop" }),
                    ]),
                    anthropic(&[
                        start.clone(),
                        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
                        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "ok" } }),
                        json!({ "type": "content_block_stop", "index": 0 }),
                        json!({ "type": "message_delta", "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 1 } }),
                        json!({ "type": "message_stop" }),
                    ]),
                ),
                "google" => (
                    "/google",
                    "google/gemini-2.5-pro",
                    format!(
                        "data: {}\n\n",
                        json!({ "candidates": [{ "content": { "role": "model", "parts": [
                            { "functionCall": { "name": "write", "args": args } }] }, "finishReason": "MAX_TOKENS" }] })
                    ),
                    format!(
                        "data: {}\n\n",
                        json!({ "candidates": [{ "content": { "role": "model", "parts": [{ "text": "ok" }] },
                            "finishReason": "STOP" }] })
                    ),
                ),
                _ => (
                    "/compat",
                    "deepseek/deepseek-reasoner",
                    compat_events(&[
                        json!({ "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "call_w",
                            "type": "function", "function": { "name": "write", "arguments": args.to_string() } }] } }] }),
                        json!({ "choices": [{ "index": 0, "delta": {}, "finish_reason": "length" }] }),
                    ]),
                    compat_events(&[compat_text("ok"), compat_stop()]),
                ),
            };
            server.route(route, vec![Resp::sse(truncated), Resp::sse(answer)]);
            let h = provider_host(&mut env, &server, json!({ "model": model })).await;
            let res = h.prompt("write it", json!({})).await.unwrap();
            h.shutdown().await;
            if marker.exists() {
                problems.push(format!("{wire}: a tool call from a truncated response ran"));
            }
            let reqs = server.requests();
            match reqs.last() {
                Some(next) if reqs.len() == 2 && next.body.contains("output token limit") => {}
                _ => {
                    let tail = reqs.last().map(|r| r.body.chars().rev().take(300).collect::<Vec<_>>()).unwrap_or_default();
                    let tail: String = tail.into_iter().rev().collect();
                    problems.push(format!(
                        "{wire}: the next request must tell the model why ({} requests, result {res}): …{tail}",
                        reqs.len()
                    ))
                }
            }
        })
        .await;
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Tool arguments that aren't valid JSON are never run as `{}`: the parse error goes back to the
/// model as the call's result (openai-compat, chat completions).
#[tokio::test]
async fn unparseable_tool_arguments_return_an_error_to_the_model() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let chunk = |delta: Value, finish: Value| {
            format!(
                "data: {}\n\n",
                json!({ "id": "c1", "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
            )
        };
        let bad = chunk(
            json!({ "tool_calls": [{ "index": 0, "id": "call_b", "type": "function",
                    "function": { "name": "ls", "arguments": "{\"path\": " } }] }),
            Value::Null,
        ) + &chunk(json!({}), json!("tool_calls"))
            + "data: [DONE]\n\n";
        let done = chunk(json!({ "content": "ok" }), json!("stop")) + "data: [DONE]\n\n";
        server.route("/compat", vec![Resp::sse(bad), Resp::sse(done)]);
        let h = provider_host(&mut env, &server, json!({ "model": "deepseek/deepseek-reasoner" })).await;
        let result = h.prompt("list", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let next = server.requests().pop().unwrap().json().to_string();
        assert!(next.contains("arguments are not valid JSON"), "{next}");
        assert!(next.contains(r#"{\"path\": "#), "the raw arguments are replayed, not {{}}: {next}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn compat_tool_arguments_before_the_name_are_buffered_and_get_an_id() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let tc = |f: Value| json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":f}]}}]});
        let body = compat_events(&[
            tc(json!({"arguments":"{\"path\":"})),
            tc(json!({"name":"read","arguments":"\"a.txt\"}"})),
            tc(json!({"name":"read"})), // a repeated name is not concatenated
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        server.route("/compat", vec![Resp::sse(body)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = stream(&h, "deepseek/deepseek-reasoner").await;
        let call = msg["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "tool_call")
            .cloned()
            .unwrap_or_else(|| panic!("{msg}"));
        assert_eq!(call["name"], "read", "{call}");
        assert_eq!(call["arguments"]["path"], "a.txt", "{call}");
        assert!(call["id"].as_str().is_some_and(|id| id.starts_with("call_")), "{call}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_pause_turn_and_refusal_map_to_their_own_stops() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");
        let with_stop = |sr: &str| {
            [
                ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":1}}})),
                ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srv_1","name":"web_search"}})),
                ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"x\"}"}})),
                ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
                ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":sr}})),
                ev("message_stop", json!({"type":"message_stop"})),
            ]
            .concat()
        };
        server.route("/anthropic", vec![Resp::sse(with_stop("pause_turn")), Resp::sse(with_stop("refusal"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let paused = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(paused["stop_reason"], "paused", "{paused}");
        let hosted = paused["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "tool_call" && b["hosted"] == true)
            .cloned()
            .unwrap();
        assert_eq!(
            hosted["native"]["anthropic-messages"]["block"]["input"]["query"], "x",
            "server tool block kept in native for replay: {hosted}"
        );
        let refused = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(refused["stop_reason"], "filtered", "{refused}");
        assert_eq!(server.count("/anthropic"), 2, "neither is retried");
        h.shutdown().await;
    })
    .await
}

/// SSE frames cut between `\r` and `\n`, and a UTF-8 character split across chunks, reassemble
/// end to end (pi sse parsing tests; mare-binding sse.rs + the UTF-8 guard in mare.http).
#[tokio::test]
async fn sse_frames_split_mid_crlf_and_mid_character_reassemble() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body: String = [compat_text("你好"), compat_text(" world"), compat_stop()]
            .iter()
            .map(|e| format!("data: {e}\r\n\r\n"))
            .collect::<String>()
            + "data: [DONE]\r\n\r\n";
        let mid_char = body.find("你").unwrap() + 1;
        let mid_crlf = body.find("\r\n\r\n").unwrap() + 1; // after the first `\r`
        let second_crlf = body[mid_crlf + 4..].find("\r\n").unwrap() + mid_crlf + 5;
        server.route("/compat", vec![Resp::sse(body).chunked(vec![mid_char, mid_crlf, second_crlf], 10)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!(text_of(&msg), "你好 world", "{msg}");
        assert_eq!(server.count("/compat"), 1);
        h.shutdown().await;
    })
    .await
}

/// An SSE line over `sse_max_line_bytes`, or an event over `sse_max_event_bytes` (llm config), is
/// a malformed_response and not retried: bytes keep arriving, so the idle watchdog never fires.
#[tokio::test]
async fn sse_lines_and_events_over_the_configured_caps_fail_without_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let long_line = format!("data: {}\n\n", compat_text(&"x".repeat(4096)));
        let long_event: String =
            (0..8).map(|_| format!("data: {}\n", compat_text(&"y".repeat(400)))).collect::<String>() + "\n";
        let n = long_line.len();
        server.route(
            "/compat",
            vec![Resp::sse(long_line).chunked((1..n / 512).map(|i| i * 512).collect(), 1), Resp::sse(long_event)],
        );
        let caps = json!({ "plugins": { "llm": { "sse_max_line_bytes": 1024, "sse_max_event_bytes": 2048 } } });
        let h = provider_host(&mut env, &server, caps).await;
        for (n, what) in [(1, "line is longer than 1024"), (2, "event is larger than 2048")] {
            let msg = ask(&h, "deepseek/deepseek-chat").await;
            assert_eq!(msg["error_kind"], "malformed_response", "{msg}");
            assert!(msg.to_string().contains(what), "{msg}");
            assert_eq!(server.count("/compat"), n, "not retried");
        }
        h.shutdown().await;
    })
    .await
}

/// `: ping` comments keep a stream alive whose events are further apart than the idle limit
/// (the watchdog counts bytes, not events; Codex stream_idle_timeout).
#[tokio::test]
async fn heartbeat_comments_keep_the_idle_watchdog_alive() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let full = fixture("anthropic.sse");
        let split = full.find("event: content_block_start").unwrap();
        let ping = ": ping\n\n";
        let body = format!("{}{ping}{ping}{ping}{}", &full[..split], &full[split..]);
        let at: Vec<usize> = (0..=3).map(|i| split + i * ping.len()).collect();
        server.route("/anthropic", vec![Resp::sse(body).chunked(at, 250)]);
        let h =
            provider_host(&mut env, &server, json!({ "plugins": { "llm": { "stream_idle_timeout_ms": 500 } } })).await;
        let t0 = std::time::Instant::now();
        let msg = stream(&h, "anthropic/claude-sonnet-5-5").await;
        assert_fixture_message("anthropic", &msg, 5);
        assert!(t0.elapsed() >= std::time::Duration::from_millis(900), "events were > idle apart");
        assert_eq!(server.count("/anthropic"), 1, "the heartbeats kept it alive: no retry");
        h.shutdown().await;
    })
    .await
}

/// Chat Completions tool-call deltas: without an index they continue the call by id / the last
/// call (pi openai-completions.ts:488-509); calls interleaved by index stay apart.
#[tokio::test]
async fn compat_tool_calls_without_index_and_interleaved_by_index() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let tc = |c: Value| json!({"choices":[{"index":0,"delta":{"tool_calls":[c]}}]});
        let calls_stop = json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
        let no_index = compat_events(&[
            tc(json!({"id":"c1","type":"function","function":{"name":"read","arguments":""}})),
            tc(json!({"function":{"arguments":"{\"path\":"}})),
            tc(json!({"id":"c1","function":{"arguments":"\"a.txt\"}"}})),
            calls_stop.clone(),
        ]);
        let interleaved = compat_events(&[
            tc(json!({"index":0,"id":"c1","function":{"name":"read","arguments":""}})),
            tc(json!({"index":1,"id":"c2","function":{"name":"grep","arguments":"{\"q\":"}})),
            tc(json!({"index":0,"function":{"arguments":"{\"path\":\"a.txt\"}"}})),
            tc(json!({"index":1,"function":{"arguments":"\"x\"}"}})),
            calls_stop,
        ]);
        server.route("/compat", vec![Resp::sse(no_index), Resp::sse(interleaved)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let calls = |msg: &Value| -> Vec<Value> {
            msg["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_call").cloned().collect()
        };
        let msg = ask(&h, "deepseek/deepseek-chat").await;
        let c = calls(&msg);
        assert_eq!(c.len(), 1, "{msg}");
        assert_eq!((c[0]["id"].as_str(), c[0]["arguments"]["path"].as_str()), (Some("c1"), Some("a.txt")), "{msg}");
        let msg = ask(&h, "deepseek/deepseek-chat").await;
        let c = calls(&msg);
        assert_eq!(c.len(), 2, "{msg}");
        assert_eq!((c[0]["name"].as_str(), c[0]["arguments"]["path"].as_str()), (Some("read"), Some("a.txt")));
        assert_eq!((c[1]["name"].as_str(), c[1]["arguments"]["q"].as_str()), (Some("grep"), Some("x")));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fragmented_tool_arguments_preserve_nested_null_and_empty_containers() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let expected = json!({"nested":{"n":null,"a":[],"o":{}},"list":[null,{},[]]});
        let pieces = [r#"{"nested":{"n":"#, r#"null,"a":[],"o":{}},"list":[null,{},[]]"#, "}"];
        let raw = pieces.concat();
        assert_eq!(serde_json::from_str::<Value>(&raw).unwrap(), expected);
        let sse = |d: Value| format!("data: {d}\n\n");
        let anth = |d: Value| format!("event: {}\ndata: {d}\n\n", d["type"].as_str().unwrap());
        let mut anthropic = anth(json!({"type":"content_block_start", "index":0,
            "content_block":{"type":"tool_use", "id":"id", "name":"read", "input":{}}}));
        for part in pieces {
            anthropic += &anth(json!({"type":"content_block_delta", "index":0,
                "delta":{"type":"input_json_delta", "partial_json":part}}));
        }
        anthropic += &anth(json!({"type":"content_block_stop", "index":0}));
        anthropic += &anth(json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}}));
        anthropic += &anth(json!({"type":"message_stop"}));

        let mut responses = responses_event(json!({"type":"response.output_item.added", "item":
            {"type":"function_call", "id":"item", "call_id":"id", "name":"read"}}));
        for part in pieces {
            responses += &responses_event(
                json!({"type":"response.function_call_arguments.delta", "item_id":"item", "delta":part}),
            );
        }
        responses += &responses_event(json!({"type":"response.output_item.done", "item":
            {"type":"function_call", "call_id":"id", "name":"read", "arguments":raw}}));
        responses += &responses_event(json!({"type":"response.completed", "response":{"usage":{}}}));

        let compat_chunk = |f: Value| {
            json!({"choices":[{"delta":{"tool_calls":[
            {"index":0, "id":"id", "type":"function", "function":f}]}}]})
        };
        let mut compat = sse(compat_chunk(json!({"name":"read", "arguments":pieces[0]})));
        for part in &pieces[1..] {
            compat += &sse(compat_chunk(json!({"arguments":part})));
        }
        compat += &sse(json!({"choices":[{"delta":{}, "finish_reason":"tool_calls"}]}));
        compat += "data: [DONE]\n\n";
        // Google sends a complete structured functionCall, not argument deltas. Fragment its
        // SSE bytes and include a preceding chunk to exercise the actual streaming transport.
        let google = sse(json!({"candidates":[{"content":{"parts":[{"text":"before"}]}}]}))
            + &sse(json!({"candidates":[{"content":{"parts":[{"functionCall":
                {"id":"id", "name":"read", "args":expected}}]}, "finishReason":"STOP"}]}));
        for (path, body) in
            [("/anthropic", anthropic), ("/openai", responses), ("/compat", compat), ("/google", google)]
        {
            let cut = body.find("nested").unwrap();
            server.route(path, vec![Resp::sse(body).chunked(vec![cut + 2, cut + 5], 1)]);
        }
        let h = provider_host(&mut env, &server, json!({})).await;
        for model in
            ["anthropic/claude-sonnet-5-5", "openai/gpt-6.1-terra", "deepseek/deepseek-chat", "google/gemini-2.5-pro"]
        {
            let msg = stream(&h, model).await;
            let call = msg["content"].as_array().unwrap().iter().find(|b| b["type"] == "tool_call").unwrap();
            assert_eq!(call["arguments"], expected, "{model}: {msg}");
            assert_eq!(msg["stop_reason"], "tool_use", "{model}: {msg}");
            let events = msg["events"].as_array().unwrap();
            assert!(events.iter().any(|e| e == "tool_call_start"), "{model}: {msg}");
            if !model.starts_with("google/") {
                assert_eq!(events.iter().filter(|e| *e == "tool_call_delta").count(), 3, "{model}: {msg}");
            }
        }
        h.shutdown().await;
    })
    .await
}

/// OpenAI-compatible streams ignore empty `data:` lines interleaved with deltas.
#[tokio::test]
async fn compat_skips_empty_data_lines_and_completes() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body = format!(
            "data:\n\ndata:   \n\ndata: {}\n\ndata: \n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            compat_text("hello "),
            compat_text("compat"),
            compat_stop()
        );
        server.route("/compat", vec![Resp::sse(body)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let msg = ask(&h, "deepseek/deepseek-chat").await;
        assert_eq!(msg["stop_reason"], "stop", "{msg}");
        assert_eq!(text_of(&msg), "hello compat", "{msg}");
        assert_eq!(server.count("/compat"), 1);
        h.shutdown().await;
    })
    .await
}

/// Anthropic unknown stop reasons map to `other` (preserving `raw_stop_reason`),
/// and server-side tool blocks (`server_tool_use`, `web_search_tool_result`) are replayed verbatim
/// on the next request to Anthropic.
#[tokio::test]
async fn anthropic_unknown_stop_reason_maps_to_other_and_server_blocks_replay_verbatim() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");
        let first = [
            ev("message_start", json!({"type":"message_start","message":{"usage":{"input_tokens":10}}})),
            ev("content_block_start", json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search"}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"mare\"}"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":0})),
            ev("content_block_start", json!({"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://example.test","title":"Mare"}]}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":1})),
            ev("content_block_start", json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}})),
            ev("content_block_delta", json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"found it"}})),
            ev("content_block_stop", json!({"type":"content_block_stop","index":2})),
            ev("message_delta", json!({"type":"message_delta","delta":{"stop_reason":"model_context_window_exceeded_v9"},"usage":{"output_tokens":5}})),
            ev("message_stop", json!({"type":"message_stop"})),
        ]
        .concat();
        server.route("/anthropic", vec![Resp::sse(first), Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let (stop, raw_stop): (String, String) = eval(
            &h,
            r#"local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
               local msg1 = LLM:stream({
                 model = m,
                 messages = { { role = "user", content = { { type = "text", text = "search" } } } },
               }, function() end)
               LLM:stream({
                 model = m,
                 messages = {
                   { role = "user", content = { { type = "text", text = "search" } } },
                   msg1,
                   { role = "user", content = { { type = "text", text = "continue" } } },
                 },
               }, function() end)
               return tostring(msg1.stop_reason), tostring(msg1.raw_stop_reason)"#,
        )
        .await;
        assert_eq!(stop, "other", "unknown Anthropic stop_reason must map to other");
        assert_eq!(raw_stop, "model_context_window_exceeded_v9");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let assistant_blocks = reqs[1].json()["messages"][1]["content"].as_array().unwrap().clone();
        assert_eq!(assistant_blocks[0]["type"], "server_tool_use", "{assistant_blocks:?}");
        assert_eq!(assistant_blocks[0]["id"], "srvtoolu_1", "{assistant_blocks:?}");
        assert_eq!(assistant_blocks[0]["input"]["query"], "mare", "{assistant_blocks:?}");
        assert_eq!(assistant_blocks[1]["type"], "web_search_tool_result", "{assistant_blocks:?}");
        assert_eq!(assistant_blocks[1]["tool_use_id"], "srvtoolu_1", "{assistant_blocks:?}");
        assert_eq!(assistant_blocks[2]["type"], "text", "{assistant_blocks:?}");
        h.shutdown().await;
    })
    .await
}
