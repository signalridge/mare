use super::*;

/// Responses SSE with reasoning summary + encrypted_content + function_call and history replay.
#[tokio::test]
async fn responses_reasoning_then_call_replay_fixture() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = provider_fixture("openai-responses/reasoning-then-call.sse");
        server.route("/openai", vec![Resp::sse(sse), Resp::sse(responses_text("ok"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let first = ask(&h, "openai/gpt-6.1-terra").await;
        assert_eq!(first["stop_reason"], "tool_use", "{first}");
        let content = first["content"].as_array().unwrap();
        let thinking = content.iter().find(|b| b["type"] == "reasoning").expect("reasoning block");
        assert!(thinking["native"]["openai-responses"]["item"].is_object(), "{thinking}");
        let call = content.iter().find(|b| b["type"] == "tool_call").expect("tool_call block");
        assert_eq!(call["name"], "session_title");

        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "title" }] },
            first,
            { "role": "tool_result", "tool_call_id": call["id"], "tool_name": "session_title", "content": [{ "type": "text", "text": "ok" }] }
        ]);
        stream_with_history(&h, "openai/gpt-6.1-terra", history, None).await;
        let req = server.requests().pop().unwrap().json();
        let input = req["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "reasoning"), "reasoning item must replay: {input:?}");
        assert!(input.iter().any(|i| i["type"] == "function_call"), "function_call must replay: {input:?}");

        h.shutdown().await;
    })
    .await
}

/// Anthropic duplicate message_start, spliced message_start, and zero-arg tool call edge cases.
#[tokio::test]
async fn anthropic_message_start_and_no_args_edge_fixtures() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(chunks_to_sse(&provider_fixture("anthropic/duplicate-message-start.chunks.txt"), false)),
                Resp::sse(chunks_to_sse(&provider_fixture("anthropic/spliced-message-start.chunks.txt"), false)),
                Resp::sse(chunks_to_sse(&provider_fixture("anthropic/tool-no-args.chunks.txt"), false)),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;

        let dup = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(dup["stop_reason"], "stop", "{dup}");
        assert_eq!(dup["content"][0]["text"], "Hello, World!");
        assert_eq!(dup["usage"]["input"], 17);
        assert_eq!(dup["usage"]["output"], 227);

        let spliced = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(spliced["stop_reason"], "tool_use", "{spliced}");
        let spliced_calls: Vec<&Value> =
            spliced["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_call").collect();
        let last_call = spliced_calls.last().expect("tool_call");
        assert_eq!(last_call["name"], "test-tool");
        assert_eq!(last_call["arguments"]["value"], "Sparkle Day");

        let no_args = ask(&h, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(no_args["stop_reason"], "tool_use", "{no_args}");
        assert_eq!(no_args["content"][0]["text"], "I'll update the issue list for you.");
        assert_eq!(no_args["content"][1]["name"], "updateIssueList");
        assert_eq!(no_args["content"][1]["arguments"], json!({}));

        h.shutdown().await;
    })
    .await
}

/// Minimal JS/TS object-literal to JSON converter for Google fixture `.ts` files.
fn js_literal_to_json(src: &str) -> Value {
    let mut out = String::with_capacity(src.len());
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            out.push('"');
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    out.push(chars[i]);
                    out.push(chars[i + 1]);
                    i += 2;
                } else {
                    if chars[i] == '"' {
                        out.push('\\');
                    }
                    out.push(chars[i]);
                    i += 1;
                }
            }
            out.push('"');
            i += 1;
        } else if c == '"' {
            out.push('"');
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    out.push(chars[i]);
                    out.push(chars[i + 1]);
                    i += 2;
                } else {
                    out.push(chars[i]);
                    i += 1;
                }
            }
            out.push('"');
            i += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let ident: String = chars[start..i].iter().collect();
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < chars.len() && chars[j] == ':' {
                out.push('"');
                out.push_str(&ident);
                out.push('"');
            } else {
                out.push_str(&ident);
            }
        } else if c == ',' {
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j] == '}' || chars[j] == ']') {
                i += 1;
            } else {
                out.push(c);
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("failed to parse JS literal: {e}\n{out}"))
}

fn extract_ts_const_block(ts: &str, const_name: &str) -> String {
    let needle = format!("const {const_name} = ");
    let start = ts.find(&needle).unwrap_or_else(|| panic!("missing `{needle}`")) + needle.len();
    let rest = &ts[start..];
    let mut depth = 0i32;
    let mut in_sq = false;
    let mut in_dq = false;
    let mut prev = '\0';
    for (idx, ch) in rest.char_indices() {
        if ch == '\'' && !in_dq && prev != '\\' {
            in_sq = !in_sq;
        } else if ch == '"' && !in_sq && prev != '\\' {
            in_dq = !in_dq;
        } else if !in_sq && !in_dq {
            if ch == '{' || ch == '[' {
                depth += 1;
            } else if ch == '}' || ch == ']' {
                depth -= 1;
                if depth == 0 {
                    return rest[..=idx].to_string();
                }
            }
        }
        prev = ch;
    }
    panic!("unterminated `{const_name}` block");
}

/// Ref: Google Gemini thoughtSignature docs & pi `test/google-shared-signed-empty-blocks.test.ts`:
/// SSE -> next request roundtrip preserves signed empty text/thinking part boundaries and
/// parallel functionCall signatures (1st signed, 2nd unsigned as returned).
#[tokio::test]
async fn google_gemini3_sse_roundtrip_preserves_signed_empty_part_boundary() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let valid_sig = "AAAAAAAAAAAAAAAAAAAAAA==";
        let sse = [
            format!(
                "data: {}\n\n",
                json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": [{ "text": "", "thoughtSignature": valid_sig }] }
                    }]
                })
            ),
            format!(
                "data: {}\n\n",
                json!({
                    "candidates": [{
                        "content": { "role": "model", "parts": [
                            { "text": "Running tools." },
                            { "functionCall": { "id": "p1", "name": "read", "args": { "path": "a.txt" } }, "thoughtSignature": valid_sig },
                            { "functionCall": { "id": "p2", "name": "read", "args": { "path": "b.txt" } } }
                        ] },
                        "finishReason": "STOP"
                    }],
                    "usageMetadata": { "promptTokenCount": 20, "candidatesTokenCount": 10 }
                })
            ),
        ]
        .join("");
        server.route("/google", vec![Resp::sse(sse), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let first = ask(&h, "google/gemini-3-pro-preview").await;
        assert_eq!(first["stop_reason"], "tool_use", "{first}");
        let content = first["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "");
        assert_eq!(content[0]["native"]["google-generate"]["thought_signature"], valid_sig);
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "Running tools.");
        assert!(content[1].get("native").is_none(), "second text part must stay separate & unsigned: {content:?}");

        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            first,
            { "role": "tool_result", "tool_call_id": "p1", "tool_name": "read", "content": [{ "type": "text", "text": "A" }] },
            { "role": "tool_result", "tool_call_id": "p2", "tool_name": "read", "content": [{ "type": "text", "text": "B" }] }
        ]);
        stream_with_history(&h, "google/gemini-3-pro-preview", history, Some(read_tool_decl())).await;
        let req = server.requests().pop().unwrap().json();
        let parts = req["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 4, "signed empty text, normal text, and 2 parallel calls must all keep separate boundaries: {parts:?}");
        assert_eq!(parts[0]["text"], "");
        assert_eq!(parts[0]["thoughtSignature"], valid_sig);
        assert_eq!(parts[1]["text"], "Running tools.");
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(parts[2]["functionCall"]["id"], "p1");
        assert_eq!(parts[2]["thoughtSignature"], valid_sig);
        assert_eq!(parts[3]["functionCall"]["id"], "p2");
        assert!(parts[3].get("thoughtSignature").is_none(), "2nd parallel call must remain unsigned: {parts:?}");

        h.shutdown().await;
    })
    .await
}

fn google_sse(chunks: &[Value]) -> String {
    chunks.iter().map(|c| format!("data: {c}\n\n")).collect()
}

/// Ref: pi `packages/ai/test/google-thinking-signature.test.ts:23` (later deltas omit the
/// signature) and `src/api/google-shared.ts:135` (never merge signatures across parts); Google
/// thought-signature docs (keep each signature on its own part). Multi-chunk SSE -> next request.
#[tokio::test]
async fn google_gemini3_sse_roundtrip_keeps_streamed_thinking_and_same_signature_parts_apart() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let (s1, s2) = ("AAAAAAAAAAAAAAAAAAAAAA==", "BBBBBBBBBBBBBBBBBBBBBB==");
        let part = |p: Value| json!({ "candidates": [{ "content": { "role": "model", "parts": [p] } }] });
        let sse = google_sse(&[
            // signed first thinking delta, unsigned continuation (pi: signature only on the first)
            part(json!({ "text": "one ", "thought": true, "thoughtSignature": s1 })),
            part(json!({ "text": "two", "thought": true })),
            // a signature-only empty part, then an independent part carrying the same signature
            part(json!({ "text": "", "thoughtSignature": s2 })),
            part(json!({ "text": "Answer", "thoughtSignature": s2 })),
            part(json!({ "text": " more" })),
            // a further independent signed part, again with the same opaque value
            json!({
                "candidates": [{ "content": { "role": "model", "parts": [{ "text": "Again", "thoughtSignature": s2 }] }, "finishReason": "STOP" }],
                "usageMetadata": { "promptTokenCount": 5, "candidatesTokenCount": 5 }
            }),
        ]);
        server.route("/google", vec![Resp::sse(sse), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let first = ask(&h, "google/gemini-3-pro-preview").await;
        let history = json!([
            { "role": "user", "content": [{ "type": "text", "text": "go" }] },
            first,
            { "role": "user", "content": [{ "type": "text", "text": "next" }] }
        ]);
        stream_with_history(&h, "google/gemini-3-pro-preview", history, None).await;
        let req = server.requests().pop().unwrap().json();
        assert_eq!(
            req["contents"][1]["parts"],
            json!([
                { "text": "one two", "thought": true, "thoughtSignature": s1 },
                { "text": "", "thoughtSignature": s2 },
                { "text": "Answer more", "thoughtSignature": s2 },
                { "text": "Again", "thoughtSignature": s2 }
            ]),
            "streamed thinking must survive replay and same-signature parts must stay separate"
        );

        h.shutdown().await;
    })
    .await
}

/// Google: the first function call of each step must carry a signature; a signature on a text
/// or thinking part does not stand in for it (thought-signatures docs, function calling
/// "parallel" example: first call signed, later ones as returned).
#[tokio::test]
async fn google_gemini3_first_function_call_gets_fallback_even_when_other_parts_are_signed() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let sig = "AAAAAAAAAAAAAAAAAAAAAA==";
        let later = "CCCCCCCCCCCCCCCCCCCCCC==";
        let call = |id: &str, s: Option<&str>| {
            let mut c = json!({ "type": "tool_call", "id": id, "name": "read", "arguments": { "path": id } });
            if let Some(s) = s {
                c["native"] = json!({ "google-generate": { "thought_signature": s } });
            }
            c
        };
        let result = |id: &str| json!({ "role": "tool_result", "tool_call_id": id, "tool_name": "read", "content": [{ "type": "text", "text": "ok" }] });
        let cases = [
            // signed text, unsigned first call
            (json!([{ "type": "text", "text": "Reading.", "native": { "google-generate": { "thought_signature": sig } } }, call("c1", None)]), vec!["skip_thought_signature_validator"]),
            // unsigned first call, signed later parallel call
            (json!([call("c1", None), call("c2", Some(later))]), vec!["skip_thought_signature_validator", later]),
        ];
        for (content, want) in cases {
            let mut history = vec![
                json!({ "role": "user", "content": [{ "type": "text", "text": "go" }] }),
                json!({ "role": "assistant", "provider_id": "google", "model_id": "gemini-3-pro-preview", "content": content }),
            ];
            for id in ["c1", "c2"].iter().take(want.len()) {
                history.push(result(id));
            }
            stream_with_history(&h, "google/gemini-3-pro-preview", json!(history), Some(read_tool_decl())).await;
            let req = server.requests().pop().unwrap().json();
            let sigs: Vec<&str> = req["contents"][1]["parts"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|p| p.get("functionCall").is_some())
                .map(|p| p["thoughtSignature"].as_str().unwrap_or("<none>"))
                .collect();
            assert_eq!(sigs, want, "{req}");
        }

        h.shutdown().await;
    })
    .await
}

/// Google Gemini decode fixtures (`function-call.ts`, `text-and-thought-signatures.ts`)
/// and Gemini 2.5 / 3 encode roundtrips.
#[tokio::test]
async fn google_genai_signature_fixtures_decode_and_encode() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        let fc_ts = provider_fixture("google/function-call.ts");
        let fc_resp = js_literal_to_json(&extract_ts_const_block(&fc_ts, "response"));

        let ts_ts = provider_fixture("google/text-and-thought-signatures.ts");
        let ts_usage = extract_ts_const_block(&ts_ts, "usageMetadata");
        let ts_resp_raw = extract_ts_const_block(&ts_ts, "response")
            .replace("usageMetadata,", &format!("usageMetadata: {ts_usage},"));
        let ts_resp = js_literal_to_json(&ts_resp_raw);

        server.route(
            "/google",
            vec![
                Resp::sse(format!("data: {fc_resp}\n\n")),
                Resp::sse(format!("data: {ts_resp}\n\n")),
                Resp::sse(fixture("google.sse")),
                Resp::sse(fixture("google.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({})).await;

        let fc_msg = ask(&h, "google/gemini-3-pro-preview").await;
        assert_eq!(fc_msg["stop_reason"], "tool_use", "{fc_msg}");
        assert_eq!(fc_msg["content"][0]["id"], "native-call");
        assert_eq!(fc_msg["content"][0]["native"]["google-generate"]["thought_signature"], "sig-tool");

        let ts_msg = ask(&h, "google/gemini-3-pro-preview").await;
        assert_eq!(ts_msg["stop_reason"], "stop", "{ts_msg}");
        assert_eq!(ts_msg["usage"]["input"], 8);
        assert_eq!(ts_msg["usage"]["cache_read"], 2);
        assert_eq!(ts_msg["usage"]["output"], 3);
        assert_eq!(ts_msg["content"][0]["native"]["google-generate"]["thought_signature"], "sig-text");
        assert_eq!(ts_msg["content"][1]["type"], "reasoning");
        assert_eq!(ts_msg["content"][1]["native"]["google-generate"]["thought_signature"], "sig-think");

        // Encode cases for Gemini 3 native-call signature/id replay and Gemini 2.5 unsigned tool history.
        // Encode Gemini 3: native-call with base64 signature preserves id & signature
        let g3_hist = json!([
            { "role": "user", "content": [{ "type": "text", "text": "weather" }] },
            { "role": "assistant", "provider_id": "google", "model_id": "gemini-3-pro-preview", "content": [
                { "type": "tool_call", "id": "native-call", "name": "weather", "arguments": { "city": "sf" }, "native": { "google-generate": { "thought_signature": "c2lnLXRvb2w=" } } }
            ] },
            { "role": "tool_result", "tool_call_id": "native-call", "tool_name": "weather", "content": [{ "type": "text", "text": "sunny" }] }
        ]);
        stream_with_history(&h, "google/gemini-3-pro-preview", g3_hist, None).await;
        let g3_req = server.requests().pop().unwrap().json();
        assert_eq!(g3_req["contents"][1]["parts"][0]["functionCall"]["id"], "native-call");
        assert_eq!(g3_req["contents"][1]["parts"][0]["thoughtSignature"], "c2lnLXRvb2w=");
        assert_eq!(g3_req["contents"][2]["parts"][0]["functionResponse"]["id"], "native-call");

        // Encode Gemini 2.5: no id, no skip_thought_signature_validator
        let g2_hist = json!([
            { "role": "user", "content": [{ "type": "text", "text": "weather" }] },
            { "role": "assistant", "provider": "google", "model": "gemini-2.5-flash", "content": [
                { "type": "tool_call", "id": "local_call", "name": "weather", "arguments": {} }
            ] },
            { "role": "tool_result", "tool_call_id": "local_call", "tool_name": "weather", "content": [{ "type": "text", "text": "sunny" }] }
        ]);
        stream_with_history(&h, "google/gemini-2.5-flash", g2_hist, None).await;
        let g2_req = server.requests().pop().unwrap().json();
        assert!(g2_req["contents"][1]["parts"][0]["functionCall"].get("id").is_none());
        assert!(g2_req["contents"][2]["parts"][0]["functionResponse"].get("id").is_none());
        assert!(g2_req["contents"][1]["parts"][0].get("thoughtSignature").is_none());

        h.shutdown().await;
    })
    .await
}

/// Gemini `functionCall` without `args` must parse `arguments` as `{}` (matching Anthropic
/// `tool_use`), not `null`, so zero-arg tool calls do not fail object validation or replay as null.
#[tokio::test]
async fn gemini_zero_arg_function_call_defaults_arguments_to_empty_object() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = format!(
            "data: {}\n\n",
            json!({
                "candidates": [{
                    "content": {
                        "role": "model",
                        "parts": [{ "functionCall": { "name": "read_theme" } }]
                    },
                    "finishReason": "STOP"
                }],
                "usageMetadata": { "promptTokenCount": 10, "candidatesTokenCount": 5 }
            })
        );
        server.route("/google", vec![Resp::sse(sse)]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let msg = ask(&h, "google/gemini-2.5-pro").await;
        assert_eq!(msg["stop_reason"], "tool_use", "{msg}");
        let calls: Vec<&Value> =
            msg["content"].as_array().unwrap().iter().filter(|b| b["type"] == "tool_call").collect();
        assert_eq!(calls.len(), 1, "{msg}");
        assert_eq!(calls[0]["name"], "read_theme");
        assert_eq!(calls[0]["arguments"], json!({}), "got {:?}", calls[0]["arguments"]);
        assert!(calls[0].get("parse_error").is_none() || calls[0]["parse_error"].is_null(), "{msg}");

        h.shutdown().await;
    })
    .await
}
