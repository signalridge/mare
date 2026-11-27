//! OpenAI Responses specifics: Codex headers, usage limits and incomplete responses.

use super::*;

/// The turn-state token is replayed within a turn and dropped when the next prompt starts (the
/// plugin's agent/start listener; ported from Codex
/// codex-rs/core/tests/suite/turn_state.rs:24 responses_turn_state_persists_within_turn_and_resets_after).
#[tokio::test]
async fn openai_sends_codex_session_headers_and_replays_turn_state() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![
                Resp::sse(fixture("openai-responses.sse")).header("x-codex-turn-state", "ts-abc"),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        openai_call(&h).await.unwrap();
        openai_call(&h).await.unwrap();
        h.emit_json("turn/started", json!({ "turn_id": "turn-2", "trigger": "prompt" })).await;
        openai_call(&h).await.unwrap();
        let reqs = server.requests();
        let first = &reqs[0];
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            assert_eq!(first.header(name), Some("sess-1"), "{name}");
        }
        assert_eq!(first.header("session_id"), None, "the old underscore header is gone");
        assert_eq!(first.header("x-codex-turn-state"), None);
        assert_eq!(reqs[1].header("x-codex-turn-state"), Some("ts-abc"), "sticky routing token replayed");
        assert_eq!(reqs[2].header("x-codex-turn-state"), None, "a new prompt starts without the last turn's token");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_captures_codex_turn_state_from_metadata_events_over_ws_and_sse() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        // 1. SSE path: legacy `response.metadata` event with mixed-case `X-Codex-Turn-State`
        let sse_with_meta = format!(
            "data: {{\"type\":\"response.metadata\",\"headers\":{{\"X-Codex-Turn-State\":\"sse-meta-ts-1\"}}}}\n\n{}",
            fixture("openai-responses.sse")
        );
        server.route(
            "/openai",
            vec![
                Resp::sse(&sse_with_meta),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h_sse = provider_host(&mut env, &server, json!({})).await;
        openai_call(&h_sse).await.unwrap();
        openai_call(&h_sse).await.unwrap();
        h_sse.emit_json("turn/started", json!({ "turn_id": "turn-sse-2", "trigger": "prompt" })).await;
        openai_call(&h_sse).await.unwrap();
        let sse_reqs = server.requests();
        assert_eq!(sse_reqs[0].header("x-codex-turn-state"), None);
        assert_eq!(
            sse_reqs[1].header("x-codex-turn-state"),
            Some("sse-meta-ts-1"),
            "response.metadata event must populate x-codex-turn-state for same-turn continuation"
        );
        assert_eq!(sse_reqs[2].header("x-codex-turn-state"), None, "new turn must clear turn_state");
        h_sse.shutdown().await;

        // 2. WebSocket path: `codex.response.metadata` event on request 1 must be replayed on the
        //    reused WebSocket's request 2 frame via `client_metadata["x-codex-turn-state"]` without
        //    opening a second connection, and cleared on `turn/started`.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new((0usize, Vec::<Value>::new())));
        let seen_srv = seen.clone();
        let base_events: Vec<String> = fixture("openai-responses.sse")
            .lines()
            .filter_map(|l| l.strip_prefix("data: ").map(str::to_string))
            .collect();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let seen_srv = seen_srv.clone();
                let base_events = base_events.clone();
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else { return };
                    seen_srv.lock().unwrap().0 += 1;
                    let mut req_idx = 0usize;
                    while let Some(Ok(msg)) = ws.next().await {
                        let Message::Text(t) = msg else { continue };
                        req_idx += 1;
                        seen_srv.lock().unwrap().1.push(serde_json::from_str(t.as_str()).unwrap());
                        if req_idx == 1 {
                            let meta = json!({
                                "type": "codex.response.metadata",
                                "headers": { "x-codex-turn-state": "ws-codex-ts-9" }
                            });
                            if ws.send(Message::Text(meta.to_string().into())).await.is_err() {
                                return;
                            }
                        }
                        for e in &base_events {
                            if ws.send(Message::Text(e.clone().into())).await.is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });

        let h_ws = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": {
                        "base_url": format!("http://127.0.0.1:{port}/openai"),
                        "transport": "websocket"
                    }
                }
            }),
        )
        .await;
        openai_call(&h_ws).await.unwrap();
        openai_call(&h_ws).await.unwrap();
        h_ws.emit_json("turn/started", json!({ "turn_id": "turn-ws-2", "trigger": "prompt" })).await;
        openai_call(&h_ws).await.unwrap();

        let (conns, frames) = seen.lock().unwrap().clone();
        assert_eq!(conns, 1, "same-turn continuation with turn_state must reuse the pooled WebSocket");
        assert_eq!(frames.len(), 3, "{frames:?}");
        assert!(frames[0].pointer("/client_metadata/x-codex-turn-state").is_none(), "{:?}", frames[0]);
        assert_eq!(
            frames[1].pointer("/client_metadata/x-codex-turn-state").and_then(Value::as_str),
            Some("ws-codex-ts-9"),
            "codex.response.metadata turn_state must be replayed on continuation frame: {:?}",
            frames[1]
        );
        assert!(frames[2].pointer("/client_metadata/x-codex-turn-state").is_none(), "{:?}", frames[2]);
        h_ws.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_replay_sets_completed_status_on_function_and_custom_tool_calls() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);
        exec(
            &h,
            r#"
            local tools = C.cordis:get("tools")
            tools:register("patch_custom", {
                description = "Custom grammar tool",
                parameters = { type = "object", properties = { input = { type = "string" } }, required = { "input" } },
                freeform = { syntax = "lark", definition = 'start: /.+/' },
                side_effect = "write",
                execute = function() return { content = "ok" } end,
            })
            local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
            LLM:stream({
                model = m,
                system = "S",
                messages = {
                    { role = "user", content = { { type = "text", text = "hi" } } },
                    {
                        role = "assistant",
                        provider_id = "openai",
                        model_id = "gpt-6.1-terra",
                        content = {
                            { type = "tool_call", id = "call_fn_1", name = "read", arguments = { path = "a.txt" } },
                            { type = "tool_call", id = "call_ct_1", name = "patch_custom", arguments = { input = "diff" } },
                        },
                    },
                    { role = "tool_result", tool_call_id = "call_fn_1", tool_name = "read", content = { { type = "text", text = "a" } } },
                    { role = "tool_result", tool_call_id = "call_ct_1", tool_name = "patch_custom", content = { { type = "text", text = "ok" } } },
                    { role = "user", content = { { type = "text", text = "continue" } } },
                },
            }, function() end)
            "#,
        )
        .await;
        let reqs = server.requests();
        assert_eq!(reqs.len(), 1);
        let input = reqs[0].json()["input"].as_array().unwrap().clone();
        let fc = input.iter().find(|i| i["type"] == "function_call").expect("function_call");
        let ctc = input.iter().find(|i| i["type"] == "custom_tool_call").expect("custom_tool_call");
        assert_eq!(fc["status"], "completed", "replayed function_call must carry status=completed: {fc}");
        assert_eq!(ctc["status"], "completed", "replayed custom_tool_call must carry status=completed: {ctc}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_usage_limit_is_fatal_and_names_the_reset() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body =
            r#"{"error":{"type":"usage_limit_reached","plan_type":"plus","resets_at":1900000000,"message":"limit"}}"#;
        server.route("/openai", vec![Resp::status(429, body)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let err = openai_call(&h).await.unwrap_err();
        assert!(err.contains("usage limit reached") && err.contains("plus plan") && err.contains("resets at"), "{err}");
        assert_eq!(server.count("/openai"), 1, "a subscription limit must not be retried");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_rate_limit_waits_the_time_named_in_the_message() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        // no retry-after header: the delay comes from the message (Codex try_parse_retry_after);
        // retry_base_ms is huge, so finishing fast proves the parsed 10ms was used
        let body =
            r#"{"error":{"code":"rate_limit_exceeded","message":"Rate limit reached. Please try again in 10ms."}}"#;
        server.route("/openai", vec![Resp::status(429, body), Resp::sse(fixture("openai-responses.sse"))]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retry_base_ms": 60000 } } })).await;
        let t0 = std::time::Instant::now();
        openai_call(&h).await.unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(10), "retry ignored the delay in the message");
        assert_eq!(server.count("/openai"), 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_incomplete_response_reports_its_reason() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"content_filter\"},\"usage\":{}}}\n\n";
        server.route("/openai", vec![Resp::sse(sse)]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let err = openai_call(&h).await.unwrap_err();
        assert!(err.contains("content_filter"), "{err}");
        assert_eq!(server.count("/openai"), 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_hosted_web_search_is_opt_in_and_passes_policy_without_affecting_other_providers() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MYRESP_KEY", "test-myresp");
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok")); 4]);
        server.route("/myresp", vec![Resp::sse(responses_text("ok")); 2]);
        server.route("/myresp-ws", vec![Resp::sse(responses_text("ok"))]);

        // 1. Default: hosted web_search is disabled when not explicitly configured.
        let h_default = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "myresp": { "wire_api": "responses", "base_url": format!("{}/myresp", server.base), "api_key_env": "MYRESP_KEY" }
                }
            }),
        )
        .await;
        exec(
            &h_default,
            r#"
            local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
            LLM:stream({
                model = m, system = "S",
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                tools = { { name = "read", description = "Read", parameters = { type = "object", properties = {} } } },
            }, function() end)
            "#,
        )
        .await;
        let reqs = server.requests();
        let default_tools = reqs[0].json()["tools"].as_array().unwrap().clone();
        assert_eq!(default_tools.len(), 1);
        assert_eq!(default_tools[0]["type"], "function");
        assert_eq!(default_tools[0]["name"], "read");
        h_default.shutdown().await;

        // 2. Explicit opt-in on provider-openai: passes mode + policy config, keeps function tools,
        // respects per-request overrides, and does not leak to non-OpenAI Responses providers.
        let h_enabled = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "myresp": { "wire_api": "responses", "base_url": format!("{}/myresp", server.base), "api_key_env": "MYRESP_KEY" },
                    "myresp-ws": { "wire_api": "responses", "base_url": format!("{}/myresp-ws", server.base), "api_key_env": "MYRESP_KEY", "supports_web_search": true },
                    "myresp-no-ws": { "wire_api": "responses", "base_url": format!("{}/myresp-no-ws", server.base), "api_key_env": "MYRESP_KEY", "supports_web_search": false, "web_search": "live" }
                },
                "plugins": {
                    "provider-openai": {
                        "web_search": "live",
                        "web_search_config": {
                            "filters": { "allowed_domains": ["example.com"] },
                            "user_location": { "country": "US", "timezone": "America/Los_Angeles" },
                            "search_context_size": "low",
                            "search_content_types": ["text", "image"]
                        }
                    }
                }
            }),
        )
        .await;
        exec(
            &h_enabled,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local other = assert(LLM:resolve("myresp/some-model"))
            local other_ws = assert(LLM:resolve("myresp-ws/some-model"))
            local other_no_ws = assert(LLM:resolve("myresp-no-ws/some-model"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
            local fn_tools = { { name = "read", description = "Read", parameters = { type = "object", properties = {} } } }
            -- (a) OpenAI with configured live + policy
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = fn_tools }, function() end)
            -- (b) OpenAI with per-request cached override via hosted tool entry
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = { { type = "hosted", name = "web_search", options = { mode = "cached" } } } }, function() end)
            -- (c) OpenAI with per-request disabled override via hosted tool entry
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = { fn_tools[1], { type = "hosted", name = "web_search", options = { mode = "disabled" } } } }, function() end)
            -- (d) Non-OpenAI Responses provider must not receive provider-openai's web_search
            LLM:stream({ model = other, system = "S", messages = msgs, tools = fn_tools }, function() end)
            -- (e) Non-OpenAI Responses provider without supports_web_search must ignore per-request hosted web_search
            LLM:stream({ model = other, system = "S", messages = msgs, tools = { fn_tools[1], { type = "hosted", name = "web_search", options = { mode = "live" } } } }, function() end)
            -- (f) Custom Responses provider with supports_web_search = true accepts per-request hosted web_search
            LLM:stream({ model = other_ws, system = "S", messages = msgs, tools = { fn_tools[1], { type = "hosted", name = "web_search", options = { mode = "live" } } } }, function() end)
            -- (g) Custom Responses provider with explicit supports_web_search = false rejects both provider.web_search and per-request hosted web_search
            LLM:stream({ model = other_no_ws, system = "S", messages = msgs, tools = { fn_tools[1], { type = "hosted", name = "web_search", options = { mode = "live" } } } }, function() end)
            "#,
        )
        .await;

        let all_reqs = server.requests();
        assert_eq!(all_reqs.len(), 8, "1 default + 7 enabled requests");

        let oa_live_tools = all_reqs[1].json()["tools"].as_array().unwrap().clone();
        assert_eq!(oa_live_tools.len(), 2, "{oa_live_tools:?}");
        assert_eq!(oa_live_tools[0]["type"], "function");
        assert_eq!(oa_live_tools[1]["type"], "web_search");
        assert_eq!(oa_live_tools[1]["external_web_access"], true);
        assert!(oa_live_tools[1].get("indexed_web_access").is_none());
        assert_eq!(oa_live_tools[1]["filters"], json!({ "allowed_domains": ["example.com"] }));
        assert_eq!(
            oa_live_tools[1]["user_location"],
            json!({ "type": "approximate", "country": "US", "timezone": "America/Los_Angeles" })
        );
        assert_eq!(oa_live_tools[1]["search_context_size"], "low");
        assert_eq!(oa_live_tools[1]["search_content_types"], json!(["text", "image"]));

        let oa_cached_tools = all_reqs[2].json()["tools"].as_array().unwrap().clone();
        assert_eq!(oa_cached_tools.len(), 1);
        assert_eq!(oa_cached_tools[0]["type"], "web_search");
        assert_eq!(oa_cached_tools[0]["external_web_access"], false);

        let oa_disabled_tools = all_reqs[3].json()["tools"].as_array().unwrap().clone();
        assert_eq!(oa_disabled_tools.len(), 1);
        assert_eq!(oa_disabled_tools[0]["type"], "function");

        let other_tools = all_reqs[4].json()["tools"].as_array().unwrap().clone();
        assert_eq!(other_tools.len(), 1);
        assert_eq!(other_tools[0]["type"], "function");
        assert_eq!(other_tools[0]["name"], "read");

        let other_req_ws_tools = all_reqs[5].json()["tools"].as_array().unwrap().clone();
        assert_eq!(other_req_ws_tools.len(), 1);
        assert_eq!(other_req_ws_tools[0]["type"], "function");

        let other_ws_tools = all_reqs[6].json()["tools"].as_array().unwrap().clone();
        assert_eq!(other_ws_tools.len(), 2);
        assert_eq!(other_ws_tools[0]["type"], "function");
        assert_eq!(other_ws_tools[1]["type"], "web_search");
        assert_eq!(other_ws_tools[1]["external_web_access"], true);

        let other_no_ws_tools = all_reqs[7].json()["tools"].as_array().unwrap().clone();
        assert_eq!(other_no_ws_tools.len(), 1);
        assert_eq!(other_no_ws_tools[0]["type"], "function");
        assert_eq!(other_no_ws_tools[0]["name"], "read");

        h_enabled.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_hosted_web_search_streams_events_preserves_citations_and_replays_history() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");
        let sse_search = [
            ev(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": { "id": "ws_1", "type": "web_search_call", "status": "in_progress" }
                }),
            ),
            ev(
                "response.web_search_call.searching",
                json!({ "type": "response.web_search_call.searching", "item_id": "ws_1", "output_index": 0 }),
            ),
            ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": "ws_1",
                        "type": "web_search_call",
                        "status": "completed",
                        "action": { "type": "search", "query": "mare web search" }
                    }
                }),
            ),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "delta": "See the docs." })),
            ev(
                "response.output_text.annotation.added",
                json!({
                    "type": "response.output_text.annotation.added",
                    "item_id": "msg_1",
                    "content_index": 0,
                    "annotation_index": 0,
                    "annotation": {
                        "type": "url_citation",
                        "url": "https://example.com/docs",
                        "title": "Example Docs",
                        "start_index": 4,
                        "end_index": 12
                    }
                }),
            ),
            ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 1,
                    "item": {
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": "See the docs.",
                            "annotations": [{
                                "type": "url_citation",
                                "url": "https://example.com/docs",
                                "title": "Example Docs",
                                "start_index": 4,
                                "end_index": 12
                            }]
                        }]
                    }
                }),
            ),
            ev(
                "response.completed",
                json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 10, "output_tokens": 5 } } }),
            ),
        ]
        .concat();

        server.route("/openai", vec![Resp::sse(sse_search), Resp::sse(responses_text("follow-up"))]);
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-openai": { "web_search": "cached" } } }),
        )
        .await;

        let out_json: String = eval(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local ant = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local events = {}
            local m1 = LLM:stream({
                model = oa, system = "S",
                messages = { { role = "user", content = { { type = "text", text = "search docs" } } } },
            }, function(e) events[#events + 1] = e end)
            local history = {
                { role = "user", content = { { type = "text", text = "search docs" } } },
                m1,
                { role = "user", content = { { type = "text", text = "summarize" } } },
            }
            LLM:stream({ model = oa, system = "S", messages = history }, function() end)
            LLM:stream({ model = ant, system = "S", messages = history }, function() end)
            return mare.json.encode({ msg = m1, events = events })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();

        let event_types: Vec<&str> = out["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["type"].as_str())
            .collect();
        assert_eq!(
            event_types,
            vec!["hosted_tool_start", "hosted_tool_update", "hosted_tool_done", "text_delta", "annotation"],
            "{event_types:?}"
        );

        let content = out["msg"]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "tool_call");
        assert_eq!(content[0]["hosted"], true);
        assert_eq!(content[0]["name"], "web_search");
        assert_eq!(content[0]["id"], "ws_1");
        assert_eq!(content[0]["native"]["openai-responses"]["item"]["type"], "web_search_call");
        assert_eq!(content[0]["native"]["openai-responses"]["item"]["id"], "ws_1");
        assert_eq!(content[0]["native"]["openai-responses"]["item"]["action"]["query"], "mare web search");

        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "See the docs.");
        assert_eq!(content[1]["citations"][0]["url"], "https://example.com/docs");
        assert_eq!(content[1]["citations"][0]["title"], "Example Docs");
        assert_eq!(content[1]["citations"][0]["start_index"], 4);
        assert_eq!(content[1]["citations"][0]["end_index"], 12);

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3);
        let replay_input = reqs[1].json()["input"].as_array().unwrap().clone();
        assert_eq!(replay_input[1]["type"], "web_search_call", "{replay_input:?}");
        assert_eq!(replay_input[1]["id"], "ws_1", "{replay_input:?}");
        assert_eq!(
            replay_input[2]["content"][0]["annotations"][0]["url"],
            "https://example.com/docs",
            "{replay_input:?}"
        );
        assert_eq!(
            replay_input[2]["content"][0]["annotations"][0]["title"],
            "Example Docs",
            "{replay_input:?}"
        );

        // Anthropic replay drops OpenAI provider_raw and succeeds cleanly.
        let ant_messages = reqs[2].json()["messages"].as_array().unwrap().clone();
        assert_eq!(ant_messages[1]["role"], "assistant");
        assert_eq!(ant_messages[1]["content"][0]["text"], "See the docs.");

        h.shutdown().await;
    })
    .await
}

/// A function tool named `web_search` (e.g. from the built-in `web` plugin) must not
/// suppress OpenAI's hosted `web_search` tool; only an existing `t.type == "web_search"` entry
/// skips appending the hosted tool.
#[tokio::test]
async fn openai_hosted_web_search_coexists_with_web_search_function_tool() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok")); 2]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": { "web_search": "live" }
                }
            }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
            local fn_ws = {
                name = "web_search",
                description = "Built-in web_search function tool",
                parameters = { type = "object", properties = { query = { type = "string" } } },
            }
            -- (a) Function tool named `web_search` + hosted `web_search` both present
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = { fn_ws } }, function() end)
            -- (b) Existing hosted `{ type = "hosted", name = "web_search" }` is not duplicated
            LLM:stream({
                model = oa,
                system = "S",
                messages = msgs,
                tools = { fn_ws, { type = "hosted", name = "web_search", options = { mode = "cached" } } },
            }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);

        let tools_a = reqs[0].json()["tools"].as_array().unwrap().clone();
        assert_eq!(tools_a.len(), 2, "{tools_a:?}");
        assert_eq!(tools_a[0]["type"], "function");
        assert_eq!(tools_a[0]["name"], "web_search");
        assert_eq!(tools_a[1]["type"], "web_search");
        assert_eq!(tools_a[1]["external_web_access"], true);

        let tools_b = reqs[1].json()["tools"].as_array().unwrap().clone();
        assert_eq!(tools_b.len(), 2, "{tools_b:?}");
        assert_eq!(tools_b[0]["type"], "function");
        assert_eq!(tools_b[0]["name"], "web_search");
        assert_eq!(tools_b[1]["type"], "web_search");
        assert_eq!(tools_b[1]["external_web_access"], false);

        h.shutdown().await;
    })
    .await
}

/// OpenAI hosted `web_search` must respect `--tools` (`mare.config.tools_enabled`),
/// `--exclude-tools` (`tools:is_disabled("web_search")`), subagent `mare.subagent.tool_allowed`,
/// and `/side` read-only requests (`req.purpose == "side"`).
#[tokio::test]
async fn openai_hosted_web_search_respects_tool_filters_subagent_ceiling_and_side_purpose() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok")); 6]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-openai": { "web_search": "live" }
                }
            }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            -- (1) Baseline: hosted web_search IS attached when allowed.
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = {} }, function() end)

            -- (2) --tools "" (`tools_enabled = {}`) suppresses hosted web_search.
            mare.config.tools_enabled = {}
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = {} }, function() end)

            -- (3) --tools read (`tools_enabled = { "read" }`) suppresses hosted web_search.
            mare.config.tools_enabled = { "read" }
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = {} }, function() end)
            mare.config.tools_enabled = nil

            -- (4) --exclude-tools web_search (`tools:disable("web_search")`) suppresses hosted web_search.
            local T = C.cordis:get("tools")
            T:disable("web_search")
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = {} }, function() end)
            T:enable("web_search")

            -- (5) /side request (`purpose = "side"`) suppresses hosted web_search.
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = {}, purpose = "side" }, function() end)

            -- (6) Subagent ceiling (`mare.subagent.tool_allowed("web_search") == false`) suppresses hosted web_search.
            local orig_sa = mare.subagent
            mare.subagent = { tool_allowed = function(n) return n ~= "web_search" end }
            LLM:stream({ model = oa, system = "S", messages = msgs, tools = {} }, function() end)
            mare.subagent = orig_sa
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 6);
        let has_ws = |idx: usize| -> bool {
            reqs[idx]
                .json()
                .get("tools")
                .and_then(|t| t.as_array())
                .is_some_and(|arr| arr.iter().any(|t| t.get("type").and_then(|v| v.as_str()) == Some("web_search")))
        };
        assert!(has_ws(0), "baseline request must include hosted web_search");
        assert!(!has_ws(1), "--tools \"\" must suppress hosted web_search");
        assert!(!has_ws(2), "--tools read must suppress hosted web_search");
        assert!(!has_ws(3), "--exclude-tools web_search must suppress hosted web_search");
        assert!(!has_ws(4), "/side request must suppress hosted web_search");
        assert!(!has_ws(5), "subagent tool_allowed=false must suppress hosted web_search");

        h.shutdown().await;
    })
    .await
}

/// Regression: OpenAI Responses SSE decoding must preserve paragraph breaks across multiple
/// `content_part`s in both streaming deltas and final `response.output_item.done` assembly
/// (adapted from reference implementation and
/// reference implementation).
/// Without the multi-part separator fix, both streaming and final text collapse to `"段落一段落二"`.
#[tokio::test]
async fn openai_responses_sse_preserves_newlines_and_content_parts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");

        // Multiple content_part blocks in one message item (each part without trailing `\n\n`).
        let sse_multipart = [
            ev(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": { "id": "msg_2", "type": "message", "role": "assistant", "status": "in_progress", "content": [] }
                }),
            ),
            ev(
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "item_id": "msg_2",
                    "output_index": 0,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "" }
                }),
            ),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_2", "output_index": 0, "content_index": 0, "delta": "段落一" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_2", "output_index": 0, "content_index": 0, "part": { "type": "output_text", "text": "段落一" } })),
            ev(
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "item_id": "msg_2",
                    "output_index": 0,
                    "content_index": 1,
                    "part": { "type": "output_text", "text": "" }
                }),
            ),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_2", "output_index": 0, "content_index": 1, "delta": "段落二" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_2", "output_index": 0, "content_index": 1, "part": { "type": "output_text", "text": "段落二" } })),
            ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": "msg_2",
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            { "type": "output_text", "text": "段落一", "annotations": [] },
                            { "type": "output_text", "text": "段落二", "annotations": [] }
                        ]
                    }
                }),
            ),
            ev("response.completed", json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 5 } } })),
        ]
        .concat();

        server.route("/openai", vec![Resp::sse(sse_multipart)]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let out_json: String = eval(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local chunks = {}
            local msg = LLM:stream({
                model = oa, system = "S",
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function(e)
                if e.type == "text_delta" then
                    chunks[#chunks + 1] = e.delta
                end
            end)
            return mare.json.encode({ streamed = table.concat(chunks), final = LLM:text_of(msg) })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["streamed"], "段落一\n\n段落二");
        assert_eq!(out["final"], "段落一\n\n段落二");

        h.shutdown().await;
    })
    .await
}

/// Baseline invariant: a single `output_text` part with explicit `\n`, `\n\n`, and standalone
/// newline deltas forwards them verbatim without dropping or duplicating newlines.
#[tokio::test]
async fn openai_responses_sse_single_part_preserves_explicit_newlines() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");

        let sse_deltas = [
            ev(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": { "id": "msg_1", "type": "message", "role": "assistant", "status": "in_progress", "content": [] }
                }),
            ),
            ev(
                "response.content_part.added",
                json!({
                    "type": "response.content_part.added",
                    "item_id": "msg_1",
                    "output_index": 0,
                    "content_index": 0,
                    "part": { "type": "output_text", "text": "" }
                }),
            ),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "第一段第一行" })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "\n" })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "第一段第二行\n\n" })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_1", "output_index": 0, "content_index": 0, "delta": "第二段" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_1", "output_index": 0, "content_index": 0, "part": { "type": "output_text", "text": "第一段第一行\n第一段第二行\n\n第二段" } })),
            ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "第一段第一行\n第一段第二行\n\n第二段", "annotations": [] }]
                    }
                }),
            ),
            ev("response.completed", json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 5 } } })),
        ]
        .concat();

        server.route("/openai", vec![Resp::sse(sse_deltas)]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let out_json: String = eval(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local chunks = {}
            local msg = LLM:stream({
                model = oa, system = "S",
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            }, function(e)
                if e.type == "text_delta" then
                    chunks[#chunks + 1] = e.delta
                end
            end)
            return mare.json.encode({ streamed = table.concat(chunks), final = LLM:text_of(msg) })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["streamed"], "第一段第一行\n第一段第二行\n\n第二段");
        assert_eq!(out["final"], "第一段第一行\n第一段第二行\n\n第二段");

        h.shutdown().await;
    })
    .await
}

/// Regression: empty `output_text` parts at the start, middle, or end of a message must not
/// introduce stray `\n\n` separators or shift annotation offsets in either streaming or final
/// `output_item.done` results (self-written).
#[tokio::test]
async fn openai_responses_sse_ignores_empty_content_parts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");

        let sse_empty_around = [
            ev(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": { "id": "msg_empty_1", "type": "message", "role": "assistant", "status": "in_progress", "content": [] }
                }),
            ),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_empty_1", "output_index": 0, "content_index": 0, "delta": "" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_empty_1", "output_index": 0, "content_index": 0, "part": { "type": "output_text", "text": "" } })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_empty_1", "output_index": 0, "content_index": 1, "delta": "alpha" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_empty_1", "output_index": 0, "content_index": 1, "part": { "type": "output_text", "text": "alpha" } })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_empty_1", "output_index": 0, "content_index": 2, "delta": "" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_empty_1", "output_index": 0, "content_index": 2, "part": { "type": "output_text", "text": "" } })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_empty_1", "output_index": 0, "content_index": 3, "delta": "beta" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_empty_1", "output_index": 0, "content_index": 3, "part": { "type": "output_text", "text": "beta" } })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_empty_1", "output_index": 0, "content_index": 4, "delta": "" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_empty_1", "output_index": 0, "content_index": 4, "part": { "type": "output_text", "text": "" } })),
            ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": "msg_empty_1",
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            { "type": "output_text", "text": "", "annotations": [] },
                            {
                                "type": "output_text",
                                "text": "alpha",
                                "annotations": [{ "type": "url_citation", "url": "https://a.example", "title": "A", "start_index": 0, "end_index": 5 }]
                            },
                            { "type": "output_text", "text": "", "annotations": [] },
                            {
                                "type": "output_text",
                                "text": "beta",
                                "annotations": [{ "type": "url_citation", "url": "https://b.example", "title": "B", "start_index": 0, "end_index": 4 }]
                            },
                            { "type": "output_text", "text": "", "annotations": [] }
                        ]
                    }
                }),
            ),
            ev("response.completed", json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 5 } } })),
        ]
        .concat();

        let sse_empty_only = [
            ev(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": { "id": "msg_empty_2", "type": "message", "role": "assistant", "status": "in_progress", "content": [] }
                }),
            ),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_empty_2", "output_index": 0, "content_index": 0, "delta": "" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_empty_2", "output_index": 0, "content_index": 0, "part": { "type": "output_text", "text": "" } })),
            ev("response.output_text.delta", json!({ "type": "response.output_text.delta", "item_id": "msg_empty_2", "output_index": 0, "content_index": 1, "delta": "" })),
            ev("response.content_part.done", json!({ "type": "response.content_part.done", "item_id": "msg_empty_2", "output_index": 0, "content_index": 1, "part": { "type": "output_text", "text": "" } })),
            ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": {
                        "id": "msg_empty_2",
                        "type": "message",
                        "role": "assistant",
                        "content": [
                            { "type": "output_text", "text": "", "annotations": [] },
                            { "type": "output_text", "text": "", "annotations": [] }
                        ]
                    }
                }),
            ),
            ev("response.completed", json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 2, "output_tokens": 0 } } })),
        ]
        .concat();

        server.route("/openai", vec![Resp::sse(sse_empty_around), Resp::sse(sse_empty_only)]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let out_json: String = eval(
            &h,
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local function run_one()
                local chunks = {}
                local msg = LLM:stream({
                    model = oa, system = "S",
                    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                }, function(e)
                    if e.type == "text_delta" then
                        chunks[#chunks + 1] = e.delta
                    end
                end)
                return {
                    streamed = table.concat(chunks),
                    final = LLM:text_of(msg),
                    annotations = msg.content[1] and msg.content[1].native and msg.content[1].native["openai-responses"] and msg.content[1].native["openai-responses"].annotations or nil,
                }
            end
            return mare.json.encode({ r1 = run_one(), r2 = run_one() })
            "#,
        )
        .await;
        let out: Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["r1"]["streamed"], "alpha\n\nbeta");
        assert_eq!(out["r1"]["final"], "alpha\n\nbeta");
        let anns = out["r1"]["annotations"].as_array().unwrap();
        assert_eq!(anns.len(), 2);
        assert_eq!(anns[0]["start_index"], 0);
        assert_eq!(anns[0]["end_index"], 5);
        assert_eq!(anns[1]["start_index"], 7);
        assert_eq!(anns[1]["end_index"], 11);
        assert_eq!(out["r2"]["streamed"], "");
        assert_eq!(out["r2"]["final"], "");

        h.shutdown().await;
    })
    .await
}

/// Regression: adjacent content parts that already carry leading/trailing newlines (including
/// newline-only parts and separators split across multiple deltas) must not receive duplicate
/// `\n\n` separators, and streaming must match `output_item.done` text and annotation offsets
/// (self-written).
#[tokio::test]
async fn openai_responses_sse_respects_existing_boundary_newlines() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let ev = |t: &str, d: Value| format!("event: {t}\ndata: {d}\n\n");

        // Helper to build a two-part or multi-part SSE stream from `(part_deltas, done_parts)`.
        type BoundaryPart<'a> = (&'a [&'a str], &'a str, Option<(usize, usize)>);
        let make_stream = |id: &str, parts: &[BoundaryPart<'_>], with_part_done: bool| {
            let mut events = vec![ev(
                "response.output_item.added",
                json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": { "id": id, "type": "message", "role": "assistant", "status": "in_progress", "content": [] }
                }),
            )];
            let mut done_content = Vec::new();
            for (idx, (deltas, full_text, ann)) in parts.iter().enumerate() {
                for delta in *deltas {
                    events.push(ev(
                        "response.output_text.delta",
                        json!({
                            "type": "response.output_text.delta",
                            "item_id": id,
                            "output_index": 0,
                            "content_index": idx,
                            "delta": delta
                        }),
                    ));
                }
                if with_part_done {
                    events.push(ev(
                        "response.content_part.done",
                        json!({
                            "type": "response.content_part.done",
                            "item_id": id,
                            "output_index": 0,
                            "content_index": idx,
                            "part": { "type": "output_text", "text": full_text }
                        }),
                    ));
                }
                let anns = match ann {
                    Some((s, e)) => json!([{
                        "type": "url_citation",
                        "url": "https://b.example",
                        "title": "B",
                        "start_index": s,
                        "end_index": e
                    }]),
                    None => json!([]),
                };
                done_content.push(json!({ "type": "output_text", "text": full_text, "annotations": anns }));
            }
            events.push(ev(
                "response.output_item.done",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": { "id": id, "type": "message", "role": "assistant", "content": done_content }
                }),
            ));
            events.push(ev(
                "response.completed",
                json!({ "type": "response.completed", "response": { "usage": { "input_tokens": 5, "output_tokens": 5 } } }),
            ));
            events.concat()
        };

        let cases = vec![
            // 1. Trailing `\n\n` in part 1 + `"beta"` in part 2 -> `"alpha\n\nbeta"` (not 4 newlines).
            make_stream(
                "m1",
                &[(&["alpha\n\n"], "alpha\n\n", None), (&["beta"], "beta", Some((0, 4)))],
                true,
            ),
            // 2. Trailing `\n` in part 1 + `"beta"` in part 2 -> adds 1 `\n`.
            make_stream(
                "m2",
                &[(&["alpha\n"], "alpha\n", None), (&["beta"], "beta", Some((0, 4)))],
                true,
            ),
            // 3. `"alpha"` in part 1 + leading `\n\nbeta` in part 2 -> adds 0 `\n`.
            make_stream(
                "m3",
                &[(&["alpha"], "alpha", None), (&["\n\nbeta"], "\n\nbeta", Some((2, 6)))],
                true,
            ),
            // 4. `"alpha\n"` in part 1 + leading `\nbeta` in part 2 -> adds 0 `\n`.
            make_stream(
                "m4",
                &[(&["alpha\n"], "alpha\n", None), (&["\nbeta"], "\nbeta", Some((1, 5)))],
                true,
            ),
            // 5. Newline-only middle part: `"alpha"` + `"\n"` + `"beta"` -> `"alpha\n\nbeta"`.
            make_stream(
                "m5",
                &[
                    (&["alpha"], "alpha", None),
                    (&["\n"], "\n", None),
                    (&["beta"], "beta", Some((0, 4))),
                ],
                true,
            ),
            // 6. Newline-only trailing part: `"alpha"` + `"\n"` -> `"alpha\n\n"`.
            make_stream("m6", &[(&["alpha"], "alpha", None), (&["\n"], "\n", None)], true),
            // 7. Separator spanning across deltas: `["\n", "\n", "beta"]` -> `"alpha\n\nbeta"`.
            make_stream(
                "m7",
                &[(&["alpha"], "alpha", None), (&["\n", "\n", "beta"], "\n\nbeta", Some((2, 6)))],
                true,
            ),
            // 8. Separator spanning across deltas: `["\n", "beta"]` -> `"alpha\n\nbeta"`.
            make_stream(
                "m8",
                &[(&["alpha"], "alpha", None), (&["\n", "beta"], "\nbeta", Some((1, 5)))],
                true,
            ),
            // 9. Separator spanning across deltas: `["\n", "\nbeta"]` -> `"alpha\n\nbeta"`.
            make_stream(
                "m9",
                &[(&["alpha"], "alpha", None), (&["\n", "\nbeta"], "\n\nbeta", Some((2, 6)))],
                true,
            ),
            // 10. Boundary without `content_part.done` (content_index transition + newline-only part).
            make_stream(
                "m10",
                &[
                    (&["alpha"], "alpha", None),
                    (&["\n"], "\n", None),
                    (&["beta"], "beta", Some((0, 4))),
                ],
                false,
            ),
            // 11. Boundary without `content_part.done` ending on a newline-only part -> `"alpha\n\n"`.
            make_stream("m11", &[(&["alpha"], "alpha", None), (&["\n"], "\n", None)], false),
            // 12. Boundary without `content_part.done` across consecutive newline-only parts -> `"alpha\n\n\nbeta"`.
            make_stream(
                "m12",
                &[
                    (&["alpha"], "alpha", None),
                    (&["\n"], "\n", None),
                    (&["\n"], "\n", None),
                    (&["beta"], "beta", Some((0, 4))),
                ],
                false,
            ),
        ];

        let n_cases = cases.len();
        server.route("/openai", cases.into_iter().map(Resp::sse).collect());
        let h = provider_host(&mut env, &server, json!({})).await;

        let lua_script = format!(
            r#"
            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local results = {{}}
            for i = 1, {n_cases} do
                local chunks = {{}}
                local msg = LLM:stream({{
                    model = oa, system = "S",
                    messages = {{ {{ role = "user", content = {{ {{ type = "text", text = "hi" }} }} }} }},
                }}, function(e)
                    if e.type == "text_delta" then
                        chunks[#chunks + 1] = e.delta
                    end
                end)
                results[i] = {{
                    streamed = table.concat(chunks),
                    final = LLM:text_of(msg),
                    annotations = msg.content[1] and msg.content[1].native and msg.content[1].native["openai-responses"] and msg.content[1].native["openai-responses"].annotations or nil,
                }}
            end
            return mare.json.encode(results)
            "#
        );

        let out_json: String = eval(&h, &lua_script).await;
        let results: Vec<Value> = serde_json::from_str(&out_json).unwrap();
        assert_eq!(results.len(), n_cases);

        let expected = [
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\n", None),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\nbeta", Some((7, 11))),
            ("alpha\n\n", None),
            ("alpha\n\n\nbeta", Some((8, 12))),
        ];

        for (i, (exp_text, exp_ann)) in expected.iter().enumerate() {
            assert_eq!(results[i]["streamed"], *exp_text, "case {} streamed", i + 1);
            assert_eq!(results[i]["final"], *exp_text, "case {} final", i + 1);
            if let Some((s, e)) = exp_ann {
                let anns = results[i]["annotations"].as_array().expect("annotations array");
                assert_eq!(anns.len(), 1, "case {} ann len", i + 1);
                assert_eq!(anns[0]["start_index"], *s, "case {} start_index", i + 1);
                assert_eq!(anns[0]["end_index"], *e, "case {} end_index", i + 1);
            }
        }

        h.shutdown().await;
    })
    .await
}

/// Responses `max_output_tokens` shrinks near the context window using `output_cap`, while
/// ChatGPT OAuth login omits `max_output_tokens` from the request body.
#[tokio::test]
async fn responses_output_cap_shrinks_near_context_window_and_chatgpt_omits_cap() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok")), Resp::sse(responses_text("ok"))]);
        server.route("/codex", vec![Resp::sse(responses_text("ok"))]);
        let base = server.base.clone();
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "openai/gpt-6.1-terra": { "context_window": 20000, "max_output": 16000 }
                },
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": format!("{base}/codex"),
                        "auth_base_url": format!("{base}/auth")
                    }
                }
            }),
        )
        .await;

        exec(
            &h,
            r#"local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
               LLM:stream({
                 model = m,
                 system = "S",
                 messages = { { role = "user", content = { { type = "text", text = string.rep("token ", 9000) } } } },
               }, function() end)
               LLM:stream({
                 model = m,
                 system = "S",
                 messages = { { role = "user", content = { { type = "text", text = string.rep("token ", 18000) } } } },
               }, function() end)"#,
        )
        .await;
        let oa_reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path.starts_with("/openai")).collect();
        assert_eq!(oa_reqs.len(), 2);
        let cap1 = oa_reqs[0].json()["max_output_tokens"].as_i64().expect("first request sends max_output_tokens");
        let cap2 = oa_reqs[1].json()["max_output_tokens"].as_i64().expect("second request sends max_output_tokens");
        assert!(cap1 < 16000 && cap1 > 1000, "expected shrunk cap near window (20k - ~9k - 4096): {cap1}");
        assert_eq!(cap2, 1, "over-window estimate must clamp max_output_tokens to 1: {cap2}");

        exec(
            &h,
            r#"mare.fs.write(mare.paths.auth, mare.json.encode({
                 version = 1,
                 ["openai-codex"] = {
                   type = "oauth",
                   access = "tok-codex",
                   refresh = "ref-codex",
                   expires = mare.task.now() + 3600000,
                   account_id = "acct-1",
                 },
               }), { mode = 384 })
               local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
               LLM:stream({
                 model = m,
                 system = "S",
                 messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
               }, function() end)"#,
        )
        .await;
        let codex_req = server.requests().into_iter().find(|r| r.path.starts_with("/codex")).unwrap();
        assert!(
            codex_req.json().get("max_output_tokens").is_none(),
            "ChatGPT OAuth requests must omit max_output_tokens: {}",
            codex_req.json()
        );
        h.shutdown().await;
    })
    .await
}

/// Structured `hosted_tools` provider configuration enables/disables Responses hosted
/// web_search with policy options.
#[tokio::test]
async fn hosted_tools_table_and_boolean_config_control_web_search_declaration() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("HT_KEY", "test-ht");
        let server = MockServer::start().await;
        server.route("/ht-on", vec![Resp::sse(responses_text("ok"))]);
        server.route("/ht-off", vec![Resp::sse(responses_text("ok"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "ht-on": {
                        "wire_api": "responses",
                        "base_url": format!("{}/ht-on", server.base),
                        "api_key_env": "HT_KEY",
                        "hosted_tools": {
                            "web_search": {
                                "supported": true,
                                "mode": "cached",
                                "config": { "search_context_size": "low" }
                            }
                        }
                    },
                    "ht-off": {
                        "wire_api": "responses",
                        "base_url": format!("{}/ht-off", server.base),
                        "api_key_env": "HT_KEY",
                        "hosted_tools": { "web_search": false }
                    }
                }
            }),
        )
        .await;
        exec(
            &h,
            r#"local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
               LLM:stream({ model = assert(LLM:resolve("ht-on/m1")), system = "S", messages = msgs }, function() end)
               LLM:stream({ model = assert(LLM:resolve("ht-off/m1")), system = "S", messages = msgs, tools = { { type = "hosted", name = "web_search", options = { mode = "live" } } } }, function() end)"#,
        )
        .await;
        let on_req = server.requests().into_iter().find(|r| r.path.starts_with("/ht-on")).unwrap().json();
        let ws = on_req["tools"]
            .as_array()
            .and_then(|arr| arr.iter().find(|t| t["type"] == "web_search"))
            .cloned()
            .unwrap_or_else(|| panic!("expected hosted web_search tool: {on_req}"));
        assert_eq!(ws["external_web_access"], false, "cached mode sets external_web_access=false: {ws}");
        assert_eq!(ws["search_context_size"], "low", "{ws}");

        let off_req = server.requests().into_iter().find(|r| r.path.starts_with("/ht-off")).unwrap().json();
        assert!(off_req.get("tools").is_none(), "hosted_tools.web_search=false must suppress web_search: {off_req}");
        h.shutdown().await;
    })
    .await
}

/// Auxiliary requests (`purpose = "side"`, `"title"`, etc.) must neither send the main turn's
/// `x-codex-turn-state` header nor overwrite/occupy the main turn's slot, and `turn/completed`
/// must clear the completed turn's token.
#[tokio::test]
async fn openai_auxiliary_requests_do_not_leak_or_clobber_turn_state() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![
                Resp::sse(fixture("openai-responses.sse")).header("x-codex-turn-state", "main-ts-1"),
                Resp::sse(fixture("openai-responses.sse")).header("x-codex-turn-state", "side-ts-poison"),
                Resp::sse(fixture("openai-responses.sse")).header("x-codex-turn-state", "title-ts-poison"),
                Resp::sse(fixture("openai-responses.sse")).header("x-codex-turn-state", "omitted-purpose-poison"),
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        h.emit_json("turn/started", json!({ "turn_id": "turn-main-1", "trigger": "prompt" })).await;
        exec(
            &h,
            r#"local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
               local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
               -- 1. Main agent turn request captures main-ts-1.
               LLM:stream({ model = m, system = "S", messages = msgs, purpose = "agent" }, function() end)
               -- 2. Auxiliary /side request must neither send main-ts-1 nor overwrite it.
               LLM:stream({ model = m, system = "S", messages = msgs, purpose = "side" }, function() end)
               -- 3. Auxiliary title request must neither send main-ts-1 nor overwrite it.
               LLM:stream({ model = m, system = "S", messages = msgs, purpose = "title" }, function() end)
               -- 4. Unlabelled auxiliary request (purpose == nil, e.g. approval reviewer) must neither send nor overwrite main-ts-1.
               LLM:stream({ model = m, system = "S", messages = msgs }, function() end)
               -- 5. Main turn continuation must still replay main-ts-1.
               LLM:stream({ model = m, system = "S", messages = msgs, purpose = "agent" }, function() end)"#,
        )
        .await;
        h.emit_json("turn/completed", json!({ "turn_id": "turn-main-1" })).await;
        openai_call(&h).await.unwrap();

        let reqs = server.requests();
        assert_eq!(reqs.len(), 6);
        assert_eq!(reqs[0].header("x-codex-turn-state"), None);
        assert_eq!(reqs[1].header("x-codex-turn-state"), None, "/side must not inherit main turn_state");
        assert_eq!(reqs[2].header("x-codex-turn-state"), None, "title must not inherit main turn_state");
        assert_eq!(reqs[3].header("x-codex-turn-state"), None, "omitted purpose must not inherit main turn_state");
        assert_eq!(
            reqs[4].header("x-codex-turn-state"),
            Some("main-ts-1"),
            "auxiliary responses must not clobber main turn_state"
        );
        assert_eq!(reqs[5].header("x-codex-turn-state"), None, "turn/completed must clear turn_state");
        h.shutdown().await;
    })
    .await
}
