use super::*;

// ---- 4. Retry-After capping (llm.retry_after_max_ms) -------------------------------------------

#[tokio::test]
async fn retry_after_above_configured_max_ms_fails_immediately_without_sleeping() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![Resp::status(429, "{}").header("retry-after", "30"), Resp::sse(fixture("anthropic-text.sse"))],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retry_after_max_ms": 5000 } } })).await;
        let t0 = Instant::now();
        let msg = ask(&h, "anthropic/claude-sonnet-5-5").await;
        let elapsed = t0.elapsed();

        assert_eq!(msg["stop_reason"], "error", "{msg}");
        assert_eq!(msg["error_kind"], "retry_delay_too_long", "{msg}");
        let err = msg["error"].as_str().unwrap();
        assert!(err.contains("asked to wait 30s") && err.contains("limit 5s"), "{err}");
        assert_eq!(server.count("/anthropic"), 1, "must not retry when Retry-After exceeds cap");
        assert!(elapsed < Duration::from_secs(2), "must not sleep for Retry-After over cap: {elapsed:?}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn retry_after_from_error_body_above_max_ms_fails_without_sleeping() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body = r#"{"error":{"message":"Rate limit reached. Please try again in 60s."}}"#;
        server.route(
            "/compat",
            vec![Resp::status(429, body), Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retry_after_max_ms": 2000 } } })).await;
        let t0 = Instant::now();
        let msg = ask(&h, "deepseek/deepseek-reasoner").await;
        let elapsed = t0.elapsed();

        assert_eq!(msg["stop_reason"], "error", "{msg}");
        assert_eq!(msg["error_kind"], "retry_delay_too_long", "{msg}");
        let err = msg["error"].as_str().unwrap();
        assert!(err.contains("asked to wait 60s") && err.contains("limit 2s"), "{err}");
        assert_eq!(server.count("/compat"), 1);
        assert!(elapsed < Duration::from_secs(2), "must not sleep: {elapsed:?}");
        h.shutdown().await;
    })
    .await
}

// ---- 5. PROV-W1 wire protocol fixes ------------------------------------------------------------

/// `chat.go:415-417` (`chatReplaysReasoning`: host or model contains "deepseek").
#[tokio::test]
async fn deepseek_chat_tool_turn_without_reasoning_emits_space_placeholder() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-or");
        let server = MockServer::start().await;
        let ok = || Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]));
        server.route("/compat", vec![ok(), ok()]);
        server.route("/openrouter", vec![ok()]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "openrouter": { "base_url": format!("{}/openrouter", server.base) }
            } }),
        )
        .await;

        let cross_provider_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read" }] },
            {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-sonnet-5-5",
                "content": [
                    { "type": "thinking", "text": "anthropic thought", "signature": "sig_a" },
                    { "type": "tool_call", "id": "call_1", "name": "read", "arguments": { "path": "a.txt" } }
                ]
            },
            { "role": "tool_result", "tool_call_id": "call_1", "tool_name": "read", "content": [{ "type": "text", "text": "ok" }] }
        ]);

        // 1. Cross-provider switch into deepseek/deepseek-reasoner
        stream_with_history(&h, "deepseek/deepseek-reasoner", cross_provider_history.clone(), Some(read_tool_decl())).await;
        let b1 = server.requests().pop().unwrap().json();
        let a1 = b1["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(a1["reasoning_content"], " ", "cross-provider tool turn on deepseek must emit space placeholder: {a1}");

        // 2. Same-provider deepseek-reasoner with empty/missing thinking
        let empty_same_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read" }] },
            {
                "role": "assistant",
                "provider": "deepseek",
                "model": "deepseek-reasoner",
                "content": [
                    { "type": "thinking", "text": "" },
                    { "type": "tool_call", "id": "call_2", "name": "read", "arguments": { "path": "b.txt" } }
                ]
            },
            { "role": "tool_result", "tool_call_id": "call_2", "tool_name": "read", "content": [{ "type": "text", "text": "ok" }] }
        ]);
        stream_with_history(&h, "deepseek/deepseek-reasoner", empty_same_history, Some(read_tool_decl())).await;
        let b2 = server.requests().pop().unwrap().json();
        let a2 = b2["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(a2["reasoning_content"], " ", "empty thinking on deepseek tool turn must emit space placeholder: {a2}");

        // 3. Non-deepseek endpoint serving a deepseek model (openrouter/deepseek/deepseek-chat)
        stream_with_history(&h, "openrouter/deepseek/deepseek-chat", cross_provider_history, Some(read_tool_decl())).await;
        let b3 = server.requests().pop().unwrap().json();
        let a3 = b3["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(a3["reasoning_content"], " ", "deepseek model on openrouter must emit space placeholder: {a3}");

        // 4. Plain assistant answer (no tool_calls) -> switch to DeepSeek -> request with tools:
        //    DeepSeek thinking mode with tools requires reasoning_content on prior plain assistant turns too.
        server.route("/compat", vec![ok()]);
        let plain_then_tools_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "hello" }] },
            {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-sonnet-5-5",
                "content": [{ "type": "text", "text": "hi there" }]
            },
            { "role": "user", "content": [{ "type": "text", "text": "now read a.txt" }] }
        ]);
        stream_with_history(&h, "deepseek/deepseek-reasoner", plain_then_tools_history, Some(read_tool_decl())).await;
        let b4 = server.requests().pop().unwrap().json();
        let a4 = b4["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert_eq!(a4["reasoning_content"], " ", "prior plain assistant turn with tools on deepseek must emit space placeholder: {a4}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn deepseek_responses_tool_turn_without_reasoning_prepends_space_reasoning_item() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route("/og", vec![Resp::sse(responses_text("ok"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "base_url": server.base, "routes": { "responses": { "path": "/og" } } } } }),
        )
        .await;

        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "read" }] },
            {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-sonnet-5-5",
                "content": [
                    { "type": "tool_call", "id": "call_1", "name": "read", "arguments": { "path": "a.txt" } }
                ]
            },
            { "role": "tool_result", "tool_call_id": "call_1", "tool_name": "read", "content": [{ "type": "text", "text": "ok" }] }
        ]);

        stream_with_history(&h, "opencode-go/deepseek-v4.1-flash", history, Some(read_tool_decl())).await;
        let req = server.requests().pop().unwrap().json();
        let input = req["input"].as_array().unwrap();
        let r_idx = input
            .iter()
            .position(|i| i["type"] == "reasoning")
            .unwrap_or_else(|| panic!("expected placeholder reasoning item before function_call: {input:?}"));
        let fc_idx = input.iter().position(|i| i["type"] == "function_call").unwrap();
        assert!(r_idx < fc_idx, "reasoning item must precede function_call: {input:?}");
        assert_eq!(input[r_idx]["content"][0]["type"], "reasoning_text");
        assert_eq!(input[r_idx]["content"][0]["text"], " ");

        // Plain assistant answer -> switch to DeepSeek Responses -> request with tools
        server.route("/og", vec![Resp::sse(responses_text("ok"))]);
        let plain_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
            {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-sonnet-5-5",
                "content": [{ "type": "text", "text": "hello" }]
            },
            { "role": "user", "content": [{ "type": "text", "text": "read a.txt" }] }
        ]);
        stream_with_history(&h, "opencode-go/deepseek-v4.1-flash", plain_history, Some(read_tool_decl())).await;
        let req2 = server.requests().pop().unwrap().json();
        let input2 = req2["input"].as_array().unwrap();
        let r2_idx = input2
            .iter()
            .position(|i| i["type"] == "reasoning")
            .unwrap_or_else(|| panic!("expected placeholder reasoning item for prior plain assistant turn: {input2:?}"));
        assert_eq!(input2[r2_idx]["content"][0]["text"], " ");

        h.shutdown().await;
    })
    .await
}

/// pi `packages/ai/src/api/google-shared.ts:157-162,225-256` & `test/google-shared-signed-empty-blocks.test.ts:58-126`.
#[tokio::test]
async fn google_gemini3_signatures_ids_and_signed_empty_blocks() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/google",
            vec![
                Resp::sse(fixture("google.sse")),
                Resp::sse(fixture("google.sse")),
                Resp::sse(fixture("google.sse")),
                Resp::sse(fixture("google.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        let valid_sig = "AAAAAAAAAAAAAAAAAAAAAA==";

        // 1. Gemini 3: signed empty thinking + signed empty text preserved; unsigned empty blocks dropped;
        //    parallel functionCall in same turn keeps valid base64 sig on 1st call and leaves 2nd call unsigned (no validator skip);
        //    functionCall.id & functionResponse.id included.
        let g3_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            {
                "role": "assistant",
                "provider_id": "google",
                "model_id": "gemini-3-pro-preview",
                "content": [
                    { "type": "reasoning", "text": "", "native": { "google-generate": { "thought_signature": valid_sig } } },
                    { "type": "reasoning", "text": "" },
                    { "type": "text", "text": "", "native": { "google-generate": { "thought_signature": valid_sig } } },
                    { "type": "text", "text": "   " },
                    { "type": "tool_call", "id": "gcall_1", "name": "read", "arguments": { "path": "a.txt" }, "native": { "google-generate": { "thought_signature": valid_sig } } },
                    { "type": "tool_call", "id": "gcall_2", "name": "read", "arguments": { "path": "b.txt" } }
                ]
            },
            { "role": "tool_result", "tool_call_id": "gcall_1", "tool_name": "read", "content": [{ "type": "text", "text": "A" }] },
            { "role": "tool_result", "tool_call_id": "gcall_2", "tool_name": "read", "content": [{ "type": "text", "text": "B" }] }
        ]);
        stream_with_history(&h, "google/gemini-3-pro-preview", g3_history, Some(read_tool_decl())).await;
        let b1 = server.requests().pop().unwrap().json();
        let contents1 = b1["contents"].as_array().unwrap();
        let model_parts = contents1[1]["parts"].as_array().unwrap();
        // Expect: signed empty thinking, signed empty text, functionCall 1, functionCall 2 (4 parts)
        assert_eq!(model_parts.len(), 4, "unsigned empty blocks must be dropped, signed empty kept: {model_parts:?}");
        assert_eq!(model_parts[0]["thought"], json!(true));
        assert_eq!(model_parts[0]["thoughtSignature"], valid_sig);
        assert_eq!(model_parts[1]["text"], "");
        assert_eq!(model_parts[1]["thoughtSignature"], valid_sig);
        assert_eq!(model_parts[2]["functionCall"]["id"], "gcall_1");
        assert_eq!(model_parts[2]["thoughtSignature"], valid_sig);
        assert_eq!(model_parts[3]["functionCall"]["id"], "gcall_2");
        assert!(
            model_parts[3].get("thoughtSignature").is_none(),
            "parallel call in same signed Gemini 3 turn must stay unsigned as returned: {model_parts:?}"
        );
        // Both tool_results merged into one user turn with functionResponse.id
        let resp_parts = contents1[2]["parts"].as_array().unwrap();
        assert_eq!(resp_parts[0]["functionResponse"]["id"], "gcall_1");
        assert_eq!(resp_parts[1]["functionResponse"]["id"], "gcall_2");

        // 2. Same provider ("google") but DIFFERENT model (gemini-2.5-pro -> gemini-3-pro-preview):
        //    signature must NOT replay; functionCall gets skip_thought_signature_validator.
        let diff_model_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            {
                "role": "assistant",
                "provider": "google",
                "model": "gemini-2.5-pro",
                "content": [
                    { "type": "thinking", "text": "", "signature": valid_sig },
                    { "type": "tool_call", "id": "gcall_3", "name": "read", "arguments": { "path": "c.txt" }, "signature": valid_sig }
                ]
            },
            { "role": "tool_result", "tool_call_id": "gcall_3", "tool_name": "read", "content": [{ "type": "text", "text": "C" }] }
        ]);
        stream_with_history(&h, "google/gemini-3-pro-preview", diff_model_history, Some(read_tool_decl())).await;
        let b2 = server.requests().pop().unwrap().json();
        let m2_parts = b2["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(m2_parts.len(), 1, "different-model signed empty thinking must be dropped: {m2_parts:?}");
        assert_eq!(m2_parts[0]["thoughtSignature"], "skip_thought_signature_validator");

        // 3. Invalid base64 signature even on same provider+model: rejected -> skip_thought_signature_validator
        let bad_b64_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            {
                "role": "assistant",
                "provider": "google",
                "model": "gemini-3-pro-preview",
                "content": [
                    { "type": "tool_call", "id": "gcall_4", "name": "read", "arguments": { "path": "d.txt" }, "signature": "not_valid_base64!" }
                ]
            },
            { "role": "tool_result", "tool_call_id": "gcall_4", "tool_name": "read", "content": [{ "type": "text", "text": "D" }] }
        ]);
        stream_with_history(&h, "google/gemini-3-pro-preview", bad_b64_history, Some(read_tool_decl())).await;
        let b3 = server.requests().pop().unwrap().json();
        let m3_parts = b3["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(m3_parts[0]["thoughtSignature"], "skip_thought_signature_validator");

        // 4. Legacy gemini-2.5-pro: no skip_thought_signature_validator and no functionCall/Response id
        let g2_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            {
                "role": "assistant",
                "provider": "anthropic",
                "model": "claude-sonnet-5-5",
                "content": [
                    { "type": "tool_call", "id": "gcall_5", "name": "read", "arguments": { "path": "e.txt" } }
                ]
            },
            { "role": "tool_result", "tool_call_id": "gcall_5", "tool_name": "read", "content": [{ "type": "text", "text": "E" }] }
        ]);
        stream_with_history(&h, "google/gemini-2.5-pro", g2_history, Some(read_tool_decl())).await;
        let b4 = server.requests().pop().unwrap().json();
        let fc4 = &b4["contents"][1]["parts"][0]["functionCall"];
        let fr4 = &b4["contents"][2]["parts"][0]["functionResponse"];
        assert!(fc4.get("id").is_none(), "gemini-2.5 must not emit functionCall.id: {fc4}");
        assert!(fr4.get("id").is_none(), "gemini-2.5 must not emit functionResponse.id: {fr4}");
        assert!(b4["contents"][1]["parts"][0].get("thoughtSignature").is_none());

        h.shutdown().await;
    })
    .await
}

/// Ref: pi `packages/ai/src/api/openai-completions.ts:1417`, `google-shared.ts:303`;
/// Kimi `packages/core/src/llm/protocol/rules/structure.ts:45-47`.
#[tokio::test]
async fn empty_tool_result_uses_placeholder_across_all_wires() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse")), Resp::sse(fixture("anthropic-text.sse"))]);
        server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()])), Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
        server.route("/google", vec![Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse"))]);
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let empty_err_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "run" }] },
            { "role": "assistant", "content": [{ "type": "tool_call", "id": "call_1", "name": "read", "arguments": { "path": "a" } }] },
            { "role": "tool_result", "tool_call_id": "call_1", "tool_name": "read", "is_error": true, "content": [] }
        ]);
        let img_only_history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "run" }] },
            { "role": "assistant", "content": [{ "type": "tool_call", "id": "call_2", "name": "read", "arguments": { "path": "a.png" } }] },
            { "role": "tool_result", "tool_call_id": "call_2", "tool_name": "read", "is_error": false, "content": [{ "type": "image", "mime": "image/png", "data": "AAAA" }] }
        ]);

        // Anthropic
        stream_with_history(&h, "anthropic/claude-sonnet-5-5", empty_err_history.clone(), Some(read_tool_decl())).await;
        let a_empty = server.requests().pop().unwrap().json();
        assert_eq!(a_empty["messages"][2]["content"][0]["content"], "(no tool output)");
        stream_with_history(&h, "anthropic/claude-sonnet-5-5", img_only_history.clone(), Some(read_tool_decl())).await;
        let a_img = server.requests().pop().unwrap().json();
        // PROV-I1: the image is attached; nothing claims it was omitted
        assert!(a_img.to_string().contains("AAAA") && !a_img.to_string().contains("image omitted"), "{a_img}");

        // Chat Completions
        stream_with_history(&h, "deepseek/deepseek-chat", empty_err_history.clone(), Some(read_tool_decl())).await;
        let c_empty = server.requests().pop().unwrap().json();
        assert_eq!(c_empty["messages"][3]["content"], "(no tool output)");
        stream_with_history(&h, "deepseek/deepseek-chat", img_only_history.clone(), Some(read_tool_decl())).await;
        let c_img = server.requests().pop().unwrap().json();
        assert!(c_img.to_string().contains("AAAA") && !c_img.to_string().contains("image omitted"), "{c_img}");

        // Google
        stream_with_history(&h, "google/gemini-2.5-pro", empty_err_history.clone(), Some(read_tool_decl())).await;
        let g_empty = server.requests().pop().unwrap().json();
        assert_eq!(g_empty["contents"][2]["parts"][0]["functionResponse"]["response"]["error"], "(no tool output)");
        stream_with_history(&h, "google/gemini-2.5-pro", img_only_history, Some(read_tool_decl())).await;
        let g_img = server.requests().pop().unwrap().json();
        assert!(g_img.to_string().contains("AAAA") && !g_img.to_string().contains("image omitted"), "{g_img}");

        // OpenAI Responses
        stream_with_history(&h, "openai/gpt-6.1-terra", empty_err_history, Some(read_tool_decl())).await;
        let o_empty = server.requests().pop().unwrap().json();
        let fco = o_empty["input"].as_array().unwrap().iter().find(|i| i["type"] == "function_call_output").unwrap();
        assert_eq!(fco["output"], "(no tool output)");

        h.shutdown().await;
    })
    .await
}
