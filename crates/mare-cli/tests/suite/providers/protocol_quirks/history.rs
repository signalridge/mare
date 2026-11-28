use super::*;

// ---- 1. Thinking signatures --------------------------------------------------------------------

#[tokio::test]
async fn thinking_signature_replays_signed_and_redacted_and_drops_empty_or_missing_on_anthropic() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "step 1" }] },
            {
                "role": "assistant",
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-5-5",
                "content": [
                    { "type": "reasoning", "text": "signed thought", "native": { "anthropic-messages": { "signature": "sig_valid_123" } } },
                    { "type": "reasoning", "text": "", "redacted": true, "native": { "anthropic-messages": { "redacted_data": "redacted_blob" } } },
                    { "type": "reasoning", "text": "empty sig thought", "native": { "anthropic-messages": { "signature": "" } } },
                    { "type": "reasoning", "text": "missing sig thought" },
                    { "type": "text", "text": "done 1" }
                ]
            },
            { "role": "user", "content": [{ "type": "text", "text": "step 2" }] }
        ]);

        let msg = stream_with_history(&h, "anthropic/claude-sonnet-5-5", history, None).await;
        assert_eq!(msg["stop_reason"], "stop", "{msg}");

        let body = server.requests().pop().unwrap().json();
        let asst = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message");
        let blocks = asst["content"].as_array().unwrap();

        assert_eq!(
            blocks,
            &vec![
                json!({ "type": "thinking", "thinking": "signed thought", "signature": "sig_valid_123" }),
                json!({ "type": "redacted_thinking", "data": "redacted_blob" }),
                json!({ "type": "text", "text": "done 1" }),
            ],
            "only valid signed & redacted thinking blocks may replay; empty/missing signatures must be dropped, never fabricated"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn repro_225_projected_anthropic_thinking_block_does_not_replay_stale_signature() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse_turn1 = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-5-5\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"using key sk-proj-secret-225 here\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_orig_225\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"done 1\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]
        .concat();
        server.route("/anthropic", vec![Resp::sse(sse_turn1), Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        exec(&h, r#"KEEP_225 = mare.projection.register("sk-proj-secret-225")"#).await;

        let turn1 = stream_with_history(
            &h,
            "anthropic/claude-sonnet-5-5",
            json!([{ "role": "user", "content": [{ "type": "text", "text": "step 1" }] }]),
            None,
        )
        .await;
        assert_eq!(turn1["stop_reason"], "stop", "{turn1}");
        // Verify projection did mutate the thinking block text (protecting the secret).
        let projected_thinking = turn1["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "reasoning")
            .expect("reasoning block")["text"]
            .as_str()
            .unwrap();
        assert!(!projected_thinking.contains("sk-proj-secret-225"), "projection must redact secret: {projected_thinking}");

        // Replay turn 1's projected assistant message in turn 2.
        let history2 = json!([
            { "role": "user", "content": [{ "type": "text", "text": "step 1" }] },
            turn1,
            { "role": "user", "content": [{ "type": "text", "text": "step 2" }] }
        ]);
        let turn2 = stream_with_history(&h, "anthropic/claude-sonnet-5-5", history2, None).await;
        assert_eq!(turn2["stop_reason"], "stop", "{turn2}");

        let body = server.requests().pop().unwrap().json();
        let asst = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .expect("assistant message in turn 2 request");
        let stale_replayed = asst["content"].as_array().unwrap().iter().any(|b| {
            b["type"] == "thinking"
                && b["signature"].as_str() == Some("sig_orig_225")
                && b["thinking"].as_str() != Some("using key sk-proj-secret-225 here")
        });
        assert!(
            !stale_replayed,
            "Anthropic adapter replayed mutated thinking text with stale original signature: {}",
            asst["content"]
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_thinking_unmutated_replays_and_active_tool_continuation_recovers_on_mutation() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse_clean = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m_clean\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-5-5\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"clean reasoning\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_clean_225\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok clean\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]
        .concat();
        let sse_tool_secret = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m_tool\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-sonnet-5-5\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"secret sk-tool-secret-225 in active turn\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_tool_225\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_225\",\"name\":\"read\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"a.txt\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ]
        .concat();
        let strict_reject_missing_thinking = Resp::status(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"messages.1.content.0: Expected `thinking` or `redacted_thinking`, but found `tool_use`. When `thinking` is enabled, a final `assistant` message must start with a thinking block."}}"#,
        );
        server.route(
            "/anthropic",
            vec![
                Resp::sse(sse_clean),
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::sse(sse_tool_secret),
                // Strict endpoint rejects an active tool continuation missing its original thinking block.
                strict_reject_missing_thinking,
                // Next user turn (new turn boundary) succeeds with the invalidated block omitted.
                Resp::sse(fixture("anthropic-text.sse")),
                // Legacy pre-upgrade projected block without text_hash at a completed turn boundary.
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "llm": { "retries": 0 } } }),
        )
        .await;
        exec(&h, r#"KEEP_225_TOOL = mare.projection.register("sk-tool-secret-225")"#).await;

        // 1. Unmutated signed thinking block streamed through provider-anthropic replays cleanly.
        let clean_turn = stream_with_history(
            &h,
            "anthropic/claude-sonnet-5-5",
            json!([{ "role": "user", "content": [{ "type": "text", "text": "hello" }] }]),
            None,
        )
        .await;
        let _ = stream_with_history(
            &h,
            "anthropic/claude-sonnet-5-5",
            json!([
                { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
                clean_turn,
                { "role": "user", "content": [{ "type": "text", "text": "next" }] }
            ]),
            None,
        )
        .await;
        let req_clean = server.requests().pop().unwrap().json();
        let asst_clean = req_clean["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert!(
            asst_clean["content"].as_array().unwrap().iter().any(|b| {
                b["type"] == "thinking"
                    && b["thinking"] == "clean reasoning"
                    && b["signature"] == "sig_clean_225"
            }),
            "unmutated streamed thinking block must still replay with its valid signature: {}",
            asst_clean["content"]
        );

        // 2. Active tool-use continuation with a projected (mutated) thinking block against a
        //    strict fixture that rejects missing thinking blocks:
        //    - never replays the stale signature or leaks the secret,
        //    - sends explicit `thinking = { type = "disabled" }` instead of silently deleting `body.thinking`,
        //    - surfaces a clear non-retryable recoverable error when the strict endpoint rejects it,
        //    - and recovers cleanly once a new user turn begins (legal turn boundary).
        let tool_turn = stream_with_history(
            &h,
            "anthropic/claude-sonnet-5-5",
            json!([{ "role": "user", "content": [{ "type": "text", "text": "read a.txt" }] }]),
            Some(read_tool_decl()),
        )
        .await;
        assert_eq!(tool_turn["stop_reason"], "tool_use", "{tool_turn}");
        let cont_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read a.txt" }] },
            tool_turn.clone(),
            { "role": "tool_result", "tool_call_id": "toolu_225", "tool_name": "read", "content": [{ "type": "text", "text": "file contents" }] }
        ]);
        let cont = stream_with_history(&h, "anthropic/claude-sonnet-5-5", cont_history, Some(read_tool_decl())).await;
        assert_eq!(cont["stop_reason"], "error", "strict endpoint rejection must surface explicit error: {cont}");
        assert_eq!(cont["error_kind"], "invalid", "{cont}");
        assert!(
            cont["error"].as_str().is_some_and(|e| e.contains("active tool turn") && e.contains("new user message")),
            "expected actionable recovery message, got: {cont}"
        );
        let req_cont = server.requests().pop().unwrap();
        assert!(
            !req_cont.body.contains("sk-tool-secret-225"),
            "continuation request must never leak projected secret: {}",
            req_cont.body
        );
        let req_cont_json = req_cont.json();
        let asst_cont = req_cont_json["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "assistant")
            .unwrap();
        assert!(
            !asst_cont["content"].as_array().unwrap().iter().any(|b| b["type"] == "thinking"),
            "mutated thinking block in active continuation must be omitted: {}",
            asst_cont["content"]
        );
        assert_eq!(
            req_cont_json["thinking"],
            json!({ "type": "disabled" }),
            "continuation request must explicitly disable thinking instead of omitting the field: {req_cont_json}"
        );

        // Next user turn moves turn_start past the invalidated tool turn -> legal boundary, recovers cleanly.
        let recovered_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read a.txt" }] },
            tool_turn,
            { "role": "tool_result", "tool_call_id": "toolu_225", "tool_name": "read", "content": [{ "type": "text", "text": "file contents" }] },
            { "role": "user", "content": [{ "type": "text", "text": "continue from fresh turn" }] }
        ]);
        let recovered =
            stream_with_history(&h, "anthropic/claude-sonnet-5-5", recovered_history, Some(read_tool_decl())).await;
        assert_eq!(recovered["stop_reason"], "stop", "new user turn must recover cleanly: {recovered}");
        let req_rec = server.requests().pop().unwrap().json();
        let asst_rec = req_rec["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert!(
            !asst_rec["content"].as_array().unwrap().iter().any(|b| b["type"] == "thinking"),
            "invalidated prior-turn thinking block must stay omitted: {}",
            asst_rec["content"]
        );

        // 3. Legacy pre-upgrade persisted session: projected thinking block with NO text_hash
        //    must still be detected and omitted at a legal turn boundary without replaying its stale signature.
        let legacy_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "legacy turn 1" }] },
            {
                "role": "assistant",
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-5-5",
                "content": [
                    {
                        "type": "reasoning",
                        "text": "using secret [[mare-value:1]] here",
                        "native": { "anthropic-messages": { "signature": "sig_legacy_stale" } }
                    },
                    { "type": "text", "text": "legacy done" }
                ]
            },
            { "role": "user", "content": [{ "type": "text", "text": "legacy turn 2" }] }
        ]);
        let legacy_res = stream_with_history(&h, "anthropic/claude-sonnet-5-5", legacy_history, None).await;
        assert_eq!(legacy_res["stop_reason"], "stop", "{legacy_res}");
        let legacy_req = server.requests().pop().unwrap().body;
        assert!(
            !legacy_req.contains("sig_legacy_stale"),
            "legacy pre-upgrade projected thinking block without text_hash must not replay stale signature: {legacy_req}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn thinking_signature_cross_provider_strips_foreign_signatures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
        server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
        server.route("/google", vec![Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse"))]);
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let anthropic_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            {
                "role": "assistant",
                "provider_id": "anthropic",
                "model_id": "claude-sonnet-5-5",
                "content": [
                    { "type": "reasoning", "text": "anthropic secret thought", "native": { "anthropic-messages": { "signature": "sig_anthropic_only" } } },
                    { "type": "reasoning", "text": "", "redacted": true, "native": { "anthropic-messages": { "redacted_data": "redacted_anthropic" } } },
                    { "type": "text", "text": "hello" }
                ]
            },
            { "role": "user", "content": [{ "type": "text", "text": "next" }] }
        ]);

        // Anthropic -> Responses, Chat Completions, Google: foreign signatures must be stripped
        for target in ["openai/gpt-6.1-terra", "deepseek/deepseek-reasoner", "google/gemini-2.5-pro"] {
            let msg = stream_with_history(&h, target, anthropic_history.clone(), None).await;
            assert!(msg.get("error").is_none_or(Value::is_null), "{target}: {msg}");
            let raw = server.requests().pop().unwrap().body;
            assert!(!raw.contains("sig_anthropic_only"), "{target} leaked anthropic signature: {raw}");
            assert!(!raw.contains("redacted_anthropic"), "{target} leaked anthropic redacted block: {raw}");
        }

        // Google -> Google (same provider): thoughtSignature preserved on thinking & tool_call
        let google_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read" }] },
            {
                "role": "assistant",
                "provider_id": "google",
                "model_id": "gemini-2.5-pro",
                "content": [
                    { "type": "reasoning", "text": "google thought", "native": { "google-generate": { "thought_signature": "dHNpZ19nb29nbGVfdGhpbms=" } } },
                    { "type": "tool_call", "id": "gcall_1", "name": "read", "arguments": { "path": "a.txt" }, "native": { "google-generate": { "thought_signature": "dHNpZ19nb29nbGVfY2FsbA==" } } }
                ]
            },
            { "role": "tool_result", "tool_call_id": "gcall_1", "tool_name": "read", "content": [{ "type": "text", "text": "ok" }] }
        ]);
        stream_with_history(&h, "google/gemini-2.5-pro", google_history.clone(), Some(read_tool_decl())).await;
        let g_req = server.requests().pop().unwrap().body;
        assert!(g_req.contains("dHNpZ19nb29nbGVfdGhpbms="), "google should replay its own thinking thoughtSignature: {g_req}");
        assert!(g_req.contains("dHNpZ19nb29nbGVfY2FsbA=="), "google should replay its own tool_call thoughtSignature: {g_req}");

        // Google -> Anthropic (cross provider): Google thoughtSignature stripped
        stream_with_history(&h, "anthropic/claude-sonnet-5-5", google_history, Some(read_tool_decl())).await;
        let a_req = server.requests().pop().unwrap().body;
        assert!(!a_req.contains("dHNpZ19nb29nbGVfdGhpbms="), "anthropic must strip google thoughtSignature: {a_req}");
        assert!(!a_req.contains("dHNpZ19nb29nbGVfY2FsbA=="), "anthropic must strip google tool_call signature: {a_req}");

        h.shutdown().await;
    })
    .await
}

// ---- 2. Tool call IDs --------------------------------------------------------------------------

#[tokio::test]
async fn tool_call_id_formats_and_truncates_per_wire() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
        server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let long_dirty_id = format!("bad:id/with.chars!_{}", "x".repeat(80));
        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read" }] },
            {
                "role": "assistant",
                "provider": "other",
                "content": [
                    { "type": "tool_call", "id": long_dirty_id, "name": "read", "arguments": { "path": "a.txt" } }
                ]
            },
            {
                "role": "tool_result",
                "tool_call_id": long_dirty_id,
                "tool_name": "read",
                "content": [{ "type": "text", "text": "ok" }]
            }
        ]);

        let valid_chars =
            |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');

        // Anthropic: [A-Za-z0-9_-], max 64
        stream_with_history(&h, "anthropic/claude-sonnet-5-5", history.clone(), Some(read_tool_decl())).await;
        let a_body = server.requests().pop().unwrap().json();
        let a_call_id = a_body["messages"][1]["content"][0]["id"].as_str().unwrap().to_string();
        let a_res_id = a_body["messages"][2]["content"][0]["tool_use_id"].as_str().unwrap().to_string();
        assert_eq!(a_call_id, a_res_id);
        assert!(valid_chars(&a_call_id) && a_call_id.len() <= 64, "anthropic id: {a_call_id}");

        // Responses: [A-Za-z0-9_-], max 64
        stream_with_history(&h, "openai/gpt-6.1-terra", history.clone(), Some(read_tool_decl())).await;
        let o_body = server.requests().pop().unwrap().json();
        let o_input = o_body["input"].as_array().unwrap();
        let o_call_id =
            o_input.iter().find(|i| i["type"] == "function_call").unwrap()["call_id"].as_str().unwrap().to_string();
        let o_res_id = o_input.iter().find(|i| i["type"] == "function_call_output").unwrap()["call_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(o_call_id, o_res_id);
        assert!(valid_chars(&o_call_id) && o_call_id.len() <= 64, "responses id: {o_call_id}");

        // Chat Completions: [A-Za-z0-9_-], max 40
        stream_with_history(&h, "deepseek/deepseek-reasoner", history.clone(), Some(read_tool_decl())).await;
        let c_body = server.requests().pop().unwrap().json();
        let c_msgs = c_body["messages"].as_array().unwrap();
        let c_call_id = c_msgs.iter().find(|m| m["role"] == "assistant").unwrap()["tool_calls"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let c_res_id =
            c_msgs.iter().find(|m| m["role"] == "tool").unwrap()["tool_call_id"].as_str().unwrap().to_string();
        assert_eq!(c_call_id, c_res_id);
        assert!(valid_chars(&c_call_id) && c_call_id.len() <= 40, "chat id: {c_call_id}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_call_id_duplicate_ids_across_turns_are_deduplicated_on_all_wires() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
        server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read 1" }] },
            { "role": "assistant", "content": [{ "type": "tool_call", "id": "fc_0", "name": "read", "arguments": { "path": "a.txt" } }] },
            { "role": "tool_result", "tool_call_id": "fc_0", "tool_name": "read", "content": [{ "type": "text", "text": "A" }] },
            { "role": "user", "content": [{ "type": "text", "text": "read 2" }] },
            { "role": "assistant", "content": [{ "type": "tool_call", "id": "fc_0", "name": "read", "arguments": { "path": "b.txt" } }] },
            { "role": "tool_result", "tool_call_id": "fc_0", "tool_name": "read", "content": [{ "type": "text", "text": "B" }] }
        ]);

        // Anthropic
        stream_with_history(&h, "anthropic/claude-sonnet-5-5", history.clone(), Some(read_tool_decl())).await;
        let a_msgs = server.requests().pop().unwrap().json()["messages"].as_array().unwrap().clone();
        let a_calls: Vec<String> = a_msgs
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_use")
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect();
        let a_outs: Vec<String> = a_msgs
            .iter()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "tool_result")
            .map(|b| b["tool_use_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(a_calls.len(), 2);
        assert_ne!(a_calls[0], a_calls[1], "anthropic duplicate tool_call ids must be deduplicated");
        assert_eq!(a_calls, a_outs);

        // Responses
        stream_with_history(&h, "openai/gpt-6.1-terra", history.clone(), Some(read_tool_decl())).await;
        let o_input = server.requests().pop().unwrap().json()["input"].as_array().unwrap().clone();
        let o_calls: Vec<String> = o_input.iter().filter(|i| i["type"] == "function_call").map(|i| i["call_id"].as_str().unwrap().to_string()).collect();
        let o_outs: Vec<String> = o_input.iter().filter(|i| i["type"] == "function_call_output").map(|i| i["call_id"].as_str().unwrap().to_string()).collect();
        assert_eq!(o_calls.len(), 2);
        assert_ne!(o_calls[0], o_calls[1], "responses duplicate call_ids must be deduplicated");
        assert_eq!(o_calls, o_outs);

        // Chat Completions
        stream_with_history(&h, "deepseek/deepseek-reasoner", history.clone(), Some(read_tool_decl())).await;
        let c_msgs = server.requests().pop().unwrap().json()["messages"].as_array().unwrap().clone();
        let c_calls: Vec<String> = c_msgs.iter().filter(|m| m["role"] == "assistant").map(|m| m["tool_calls"][0]["id"].as_str().unwrap().to_string()).collect();
        let c_outs: Vec<String> = c_msgs.iter().filter(|m| m["role"] == "tool").map(|m| m["tool_call_id"].as_str().unwrap().to_string()).collect();
        assert_eq!(c_calls.len(), 2);
        assert_ne!(c_calls[0], c_calls[1], "chat duplicate tool_call ids must be deduplicated");
        assert_eq!(c_calls, c_outs);

        // Google (Gemini 3+ carries functionCall/functionResponse ids on the wire)
        stream_with_history(&h, "google/gemini-3-pro", history, Some(read_tool_decl())).await;
        let g_contents = server.requests().pop().unwrap().json()["contents"].as_array().unwrap().clone();
        let g_calls: Vec<String> = g_contents.iter().filter(|c| c["role"] == "model").map(|c| c["parts"][0]["functionCall"]["id"].as_str().unwrap_or("").to_string()).collect();
        let g_outs: Vec<String> = g_contents.iter().filter_map(|c| c["parts"][0].get("functionResponse")).map(|r| r["id"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(g_calls.len(), 2);
        assert_ne!(g_calls[0], g_calls[1], "google duplicate functionCall ids must be deduplicated");
        assert_eq!(g_calls, g_outs);

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_call_id_missing_id_is_deterministic_across_replays() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let compat_rec_42 = compat_events(&[
            json!({"id":"chatcmpl_rec_42","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}},{"index":1,"function":{"name":"read","arguments":"{\"path\":\"b.txt\"}"}}]}}]}),
            json!({"id":"chatcmpl_rec_42","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        let compat_rec_43 = compat_events(&[
            json!({"id":"chatcmpl_rec_43","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"read","arguments":"{\"path\":\"c.txt\"}"}}]}}]}),
            json!({"id":"chatcmpl_rec_43","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        server.route("/compat", vec![Resp::sse(compat_rec_42.clone()), Resp::sse(compat_rec_43), Resp::sse(compat_rec_42)]);

        let google_rec_42 = format!(
            "data: {}\n\n",
            json!({
                "responseId": "resp_google_42",
                "candidates": [{
                    "content": { "role": "model", "parts": [
                        { "functionCall": { "name": "read", "args": { "path": "a.txt" } } },
                        { "functionCall": { "name": "read", "args": { "path": "b.txt" } } }
                    ] },
                    "finishReason": "STOP"
                }]
            })
        );
        let google_rec_43 = format!(
            "data: {}\n\n",
            json!({
                "responseId": "resp_google_43",
                "candidates": [{
                    "content": { "role": "model", "parts": [{ "functionCall": { "name": "read", "args": { "path": "c.txt" } } }] },
                    "finishReason": "STOP"
                }]
            })
        );
        server.route("/google", vec![Resp::sse(google_rec_42.clone()), Resp::sse(google_rec_43), Resp::sse(google_rec_42)]);

        let h = provider_host(&mut env, &server, json!({})).await;

        let tool_ids = |msg: &Value| -> Vec<String> {
            msg["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "tool_call")
                .map(|b| b["id"].as_str().unwrap().to_string())
                .collect()
        };

        let c_a1 = tool_ids(&ask(&h, "deepseek/deepseek-reasoner").await);
        let c_b = tool_ids(&ask(&h, "deepseek/deepseek-reasoner").await);
        let c_a2 = tool_ids(&ask(&h, "deepseek/deepseek-reasoner").await);
        assert_eq!(c_a1.len(), 2);
        assert!(c_a1.iter().all(|id| !id.is_empty()));
        assert_ne!(c_a1[0], c_a1[1], "chat: multiple missing IDs in one record must be distinct: {c_a1:?}");
        assert_eq!(c_b.len(), 1);
        assert!(!c_b[0].is_empty());
        assert_ne!(c_b[0], c_a1[0], "chat: different records must produce distinct IDs");
        assert_ne!(c_b[0], c_a1[1], "chat: different records must produce distinct IDs");
        assert_eq!(c_a1, c_a2, "chat: repeated replay of same record must produce identical IDs");

        let g_a1 = tool_ids(&ask(&h, "google/gemini-2.5-pro").await);
        let g_b = tool_ids(&ask(&h, "google/gemini-2.5-pro").await);
        let g_a2 = tool_ids(&ask(&h, "google/gemini-2.5-pro").await);
        assert_eq!(g_a1.len(), 2);
        assert!(g_a1.iter().all(|id| !id.is_empty()));
        assert_ne!(g_a1[0], g_a1[1], "google: multiple missing IDs in one record must be distinct: {g_a1:?}");
        assert_eq!(g_b.len(), 1);
        assert!(!g_b[0].is_empty());
        assert_ne!(g_b[0], g_a1[0], "google: different records must produce distinct IDs");
        assert_ne!(g_b[0], g_a1[1], "google: different records must produce distinct IDs");
        assert_eq!(g_a1, g_a2, "google: repeated replay of same record must produce identical IDs");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn deterministic_missing_call_ids_skip_existing_ids_on_both_wires() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let (chat_collision, chat_next, google_collision, google_next): (String, String, String, String) = eval(
            &h,
            r#"
            local function id(seed, len) return "call_" .. mare.crypto.sha256(seed):sub(1, len) end
            return id("chatcmpl_collision:read:1:1", 32), id("chatcmpl_collision:read:1:2", 32),
                   id("google_collision:read:1", 24), id("google_collision:read:2", 24)
            "#,
        )
        .await;
        let compat = compat_events(&[
            json!({"id":"chatcmpl_collision","choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":chat_collision,"type":"function","function":{"name":"existing","arguments":"{}"}},
                {"index":1,"type":"function","function":{"name":"read","arguments":"{}"}}
            ]}}]}),
            json!({"id":"chatcmpl_collision","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        server.route("/compat", vec![Resp::sse(compat)]);
        let google = format!(
            "data: {}\n\n",
            json!({ "responseId": "google_collision", "candidates": [{ "content": { "role": "model", "parts": [
                { "functionCall": { "id": google_collision, "name": "existing", "args": {} } },
                { "functionCall": { "name": "read", "args": {} } }
            ] }, "finishReason": "STOP" }] })
        );
        server.route("/google", vec![Resp::sse(google)]);
        let chat = ask(&h, "deepseek/deepseek-reasoner").await;
        let google = ask(&h, "google/gemini-2.5-pro").await;
        let ids = |message: &Value| -> Vec<String> {
            message["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["type"] == "tool_call")
                .map(|item| item["id"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(ids(&chat), [chat_collision, chat_next]);
        assert_eq!(ids(&google), [google_collision, google_next]);
        h.shutdown().await;
    })
    .await
}

// ---- 3. Encrypted reasoning content (Responses encrypted_content) ------------------------------

#[tokio::test]
async fn encrypted_content_replayed_only_to_same_provider_and_model() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(responses_text("same-model-ok")),
                Resp::sse(responses_text("other-model-ok")),
            ],
        );
        server.route("/og", vec![Resp::sse(responses_text("other-provider-ok"))]);
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": { "opencode-go/gpt-6.1-terra": { "route": "responses" } },
                "plugins": { "provider-opencode-go": { "base_url": server.base, "routes": { "responses": { "path": "/og" } } } }
            }),
        )
        .await;

        let first = ask(&h, "openai/gpt-6.1-terra").await;
        let mut history = vec![
            json!({ "role": "user", "content": [{ "type": "text", "text": "read main" }] }),
            first.clone(),
        ];
        for b in first["content"].as_array().unwrap() {
            if b["type"] == "tool_call" {
                history.push(json!({
                    "role": "tool_result",
                    "tool_call_id": b["id"],
                    "tool_name": b["name"],
                    "content": [{ "type": "text", "text": "fn main() {}" }]
                }));
            }
        }
        let history = Value::Array(history);

        // Same provider + same model -> encrypted_content MUST be replayed
        stream_with_history(&h, "openai/gpt-6.1-terra", history.clone(), Some(read_tool_decl())).await;
        let same_req = server.requests().pop().unwrap().json();
        let r_item = same_req["input"].as_array().unwrap().iter().find(|i| i["type"] == "reasoning").expect("reasoning item");
        assert_eq!(r_item["encrypted_content"], "enc123", "same provider+model must carry encrypted_content");

        // Same provider + DIFFERENT model -> encrypted_content MUST be dropped
        stream_with_history(&h, "openai/gpt-5.4", history.clone(), Some(read_tool_decl())).await;
        let diff_model_req = server.requests().pop().unwrap().body;
        assert!(!diff_model_req.contains("enc123"), "switching model on same provider must drop encrypted_content: {diff_model_req}");

        // DIFFERENT provider + SAME model (same Responses wire) -> encrypted_content MUST be dropped
        stream_with_history(&h, "opencode-go/gpt-6.1-terra", history.clone(), Some(read_tool_decl())).await;
        let diff_prov_req = server.requests().pop().unwrap().body;
        assert!(
            !diff_prov_req.contains("enc123"),
            "switching provider with same model id must drop encrypted_content: {diff_prov_req}"
        );

        // DIFFERENT provider (Anthropic wire) -> encrypted_content MUST be dropped
        stream_with_history(&h, "anthropic/claude-sonnet-5-5", history, Some(read_tool_decl())).await;
        let anth_req = server.requests().pop().unwrap().body;
        assert!(!anth_req.contains("enc123"), "switching to anthropic must drop encrypted_content: {anth_req}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn encrypted_content_absent_from_gateway_keeps_summary_only() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        let no_enc_sse = responses_event(json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": { "id": "rs_noenc", "type": "reasoning", "summary": [{ "type": "summary_text", "text": "Gateway summary only." }] }
        })) + &responses_event(json!({
            "type": "response.output_item.done",
            "output_index": 1,
            "item": { "id": "fc_1", "type": "function_call", "call_id": "call_gw", "name": "read", "arguments": "{\"path\":\"a.txt\"}" }
        })) + &responses_event(json!({ "type": "response.completed", "response": { "usage": {} } }));
        server.route("/og", vec![Resp::sse(no_enc_sse), Resp::sse(responses_text("done"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": { "opencode-go/gpt-6.1-terra": { "route": "responses" } },
                "plugins": { "provider-opencode-go": { "base_url": server.base, "routes": { "responses": { "path": "/og" } } } }
            }),
        )
        .await;

        let first = ask(&h, "opencode-go/deepseek-v4.1-flash").await;
        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read" }] },
            first,
            { "role": "tool_result", "tool_call_id": "call_gw", "tool_name": "read", "content": [{ "type": "text", "text": "ok" }] }
        ]);
        stream_with_history(&h, "opencode-go/deepseek-v4.1-flash", history, Some(read_tool_decl())).await;
        let req = server.requests().pop().unwrap().json();
        let r_item = req["input"].as_array().unwrap().iter().find(|i| i["type"] == "reasoning").expect("reasoning item");
        assert_eq!(r_item["summary"][0]["text"], "Gateway summary only.");
        assert!(r_item.get("encrypted_content").is_none(), "must not invent encrypted_content: {r_item}");

        h.shutdown().await;
    })
    .await
}
