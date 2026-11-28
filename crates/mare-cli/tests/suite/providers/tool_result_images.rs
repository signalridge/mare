//! Regression tests for PROV-I1: `tool_result` images are preserved across all provider wires
//! (Anthropic Messages, Google Gemini >= 3 and < 3, and OpenAI-compatible Chat Completions).
//!
//! Reference tests & implementations:
//! - Anthropic: pi `packages/ai/src/api/anthropic-messages.ts:161-198` & `:1603-1610`
//! - Google: pi `packages/ai/src/api/google-shared.ts:285-340`
//! - OpenAI-compat: pi `packages/ai/src/api/openai-completions.ts:1405-1470`

use super::*;

async fn stream_history(h: &Host, model: &str, history_lua: &str) {
    let code = format!(
        r#"
local ref = ...
local m = assert(LLM:resolve(ref), "unknown model " .. ref)
local history = {history_lua}
return LLM:stream({{
  model = m,
  system = "SYSTEM-PROMPT",
  messages = history,
  tools = {{ {{ name = "read", description = "Read a file", parameters = {{ type = "object", properties = {{ path = {{ type = "string" }} }}, required = {{ "path" }} }} }} }},
}}, function() end)
"#
    );
    let f = h.lua().load(&code).into_function().unwrap();
    let _: mlua::Value = f.call_async(model).await.unwrap_or_else(|e| panic!("{model}: {e}"));
}

#[tokio::test]
async fn anthropic_non_error_tool_result_with_text_and_image_emits_content_array() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        stream_history(
            &h,
            "anthropic/claude-sonnet-5-5",
            r#"{
              { role = "user", content = { { type = "text", text = "look at this" } } },
              { role = "assistant", provider = "anthropic", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "shot.png" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "text", text = "Image shot.png (800x600)" },
                  { type = "image", data = "AAAA", mime = "image/png" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let msgs = body["messages"].as_array().unwrap();
        let user_blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(user_blocks.len(), 1, "should not displace non-error tool_result images: {user_blocks:?}");
        let tr = &user_blocks[0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["is_error"], false);
        let content = tr["content"].as_array().expect("tool_result content must be block array");
        assert_eq!(
            content,
            &vec![
                json!({ "type": "text", "text": "Image shot.png (800x600)" }),
                json!({ "type": "image", "source": { "type": "base64", "media_type": "image/png", "data": "AAAA" } }),
            ]
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_non_error_tool_result_image_only_adds_placeholder_text() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        stream_history(
            &h,
            "anthropic/claude-sonnet-5-5",
            r#"{
              { role = "user", content = { { type = "text", text = "look" } } },
              { role = "assistant", provider = "anthropic", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "shot.jpg" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "image", data = "BBBB", mime = "image/jpeg" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let tr = &body["messages"][2]["content"][0];
        let content = tr["content"].as_array().expect("tool_result content must be block array");
        assert_eq!(
            content,
            &vec![
                json!({ "type": "text", "text": "(see attached image)" }),
                json!({ "type": "image", "source": { "type": "base64", "media_type": "image/jpeg", "data": "BBBB" } }),
            ]
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_gemini_3_embeds_tool_result_images_in_function_response_parts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        stream_history(
            &h,
            "google/gemini-3-pro-preview",
            r#"{
              { role = "user", content = { { type = "text", text = "look" } } },
              { role = "assistant", provider = "other", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "a.png" } },
                  { type = "tool_call", id = "call_2", name = "read", arguments = { path = "b.png" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "text", text = "Image a.png" },
                  { type = "image", data = "AAAA", mime = "image/png" } } },
              { role = "tool_result", tool_call_id = "call_2", tool_name = "read", content = {
                  { type = "image", data = "BBBB", mime = "image/jpeg" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "Gemini 3 must not append separate user image turn: {contents:?}");
        let parts = contents[2]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0]["functionResponse"],
            json!({
                "id": "call_1",
                "name": "read",
                "response": { "output": "Image a.png" },
                "parts": [{ "inlineData": { "mimeType": "image/png", "data": "AAAA" } }]
            })
        );
        assert_eq!(
            parts[1]["functionResponse"],
            json!({
                "id": "call_2",
                "name": "read",
                "response": { "output": "(see attached image)" },
                "parts": [{ "inlineData": { "mimeType": "image/jpeg", "data": "BBBB" } }]
            })
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_gemini_2_5_appends_separate_user_turn_for_tool_result_images() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        stream_history(
            &h,
            "google/gemini-2.5-pro",
            r#"{
              { role = "user", content = { { type = "text", text = "look" } } },
              { role = "assistant", provider = "other", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "a.png" } },
                  { type = "tool_call", id = "call_2", name = "read", arguments = { path = "b.png" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "text", text = "Image a.png" },
                  { type = "image", data = "AAAA", mime = "image/png" } } },
              { role = "tool_result", tool_call_id = "call_2", tool_name = "read", content = {
                  { type = "image", data = "BBBB", mime = "image/jpeg" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 4, "Gemini 2.5 must append a separate user image turn: {contents:?}");
        let fn_parts = contents[2]["parts"].as_array().unwrap();
        assert!(fn_parts[0]["functionResponse"].get("parts").is_none());
        assert_eq!(fn_parts[0]["functionResponse"]["response"], json!({ "output": "Image a.png" }));
        assert!(fn_parts[1]["functionResponse"].get("parts").is_none());
        assert_eq!(fn_parts[1]["functionResponse"]["response"], json!({ "output": "(see attached image)" }));

        assert_eq!(contents[3]["role"], "user");
        assert_eq!(
            contents[3]["parts"],
            json!([
                { "text": "Tool result image:" },
                { "inlineData": { "mimeType": "image/png", "data": "AAAA" } },
                { "inlineData": { "mimeType": "image/jpeg", "data": "BBBB" } },
            ])
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_non_vision_model_downgrades_tool_result_images_to_text() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        let h =
            provider_host(&mut env, &server, json!({ "models": { "google/gemini-2.5-pro": { "input": ["text"] } } }))
                .await;

        stream_history(
            &h,
            "google/gemini-2.5-pro",
            r#"{
              { role = "user", content = { { type = "text", text = "look" } } },
              { role = "assistant", provider = "other", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "/tmp/shot.png" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "image", data = "AAAA", mime = "image/png", path = "/tmp/shot.png" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "no-image model must not emit trailing image turn: {contents:?}");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["response"]["output"], "Attachment: /tmp/shot.png");
        assert!(!body.to_string().contains("inlineData"), "no-image model must not emit inlineData: {body}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_compat_tool_result_images_emit_trailing_user_message() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        stream_history(
            &h,
            "deepseek/deepseek-reasoner",
            r#"{
              { role = "user", content = { { type = "text", text = "look" } } },
              { role = "assistant", provider = "other", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "a.png" } },
                  { type = "tool_call", id = "call_2", name = "read", arguments = { path = "b.webp" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "text", text = "Image a.png" },
                  { type = "image", data = "AAAA", mime = "image/png" } } },
              { role = "tool_result", tool_call_id = "call_2", tool_name = "read", content = {
                  { type = "image", data = "BBBB", mime = "image/webp" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let msgs = body["messages"].as_array().unwrap();
        // system, user, assistant, tool(call_1), tool(call_2), user(images)
        assert_eq!(msgs.len(), 6, "expected trailing user image message: {msgs:?}");
        assert_eq!(msgs[3], json!({ "role": "tool", "tool_call_id": "call_1", "content": "Image a.png" }));
        assert_eq!(msgs[4], json!({ "role": "tool", "tool_call_id": "call_2", "content": "(see attached image)" }));
        assert_eq!(
            msgs[5],
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "Attached image(s) from tool result:" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } },
                    { "type": "image_url", "image_url": { "url": "data:image/webp;base64,BBBB" } },
                ]
            })
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_compat_non_vision_model_downgrades_tool_result_images_to_text() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::sse(fixture("openai-compat.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "models": { "deepseek/deepseek-reasoner": { "input": ["text"] } } }),
        )
        .await;

        stream_history(
            &h,
            "deepseek/deepseek-reasoner",
            r#"{
              { role = "user", content = { { type = "text", text = "look" } } },
              { role = "assistant", provider = "other", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "/tmp/shot.png" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "image", data = "AAAA", mime = "image/png", path = "/tmp/shot.png" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let msgs = body["messages"].as_array().unwrap();
        // system, user, assistant, tool(call_1)
        assert_eq!(msgs.len(), 4, "no-image model must not emit trailing user image message: {msgs:?}");
        assert_eq!(
            msgs[3],
            json!({ "role": "tool", "tool_call_id": "call_1", "content": "Attachment: /tmp/shot.png" })
        );
        assert!(!body.to_string().contains("image_url"), "no-image model must not emit image_url: {body}");
        h.shutdown().await;
    })
    .await
}

const PNG_1X1: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

fn google_text_sse(text: &str) -> String {
    format!(
        "data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"{text}\"}}]}},\"finishReason\":\"STOP\"}}],\"usageMetadata\":{{\"promptTokenCount\":10,\"candidatesTokenCount\":5}}}}\n\n"
    )
}

fn openai_compat_text_sse(text: &str) -> String {
    format!(
        "data: {{\"id\":\"c2\",\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"{text}\"}}}}]}}\n\ndata: {{\"id\":\"c2\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn openai_responses_text_sse(text: &str) -> String {
    format!(
        "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_2\",\"status\":\"in_progress\"}}}}\n\nevent: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[]}}}}\n\nevent: response.output_text.delta\ndata: {{\"type\":\"response.output_text.delta\",\"item_id\":\"msg_2\",\"delta\":\"{text}\"}}\n\nevent: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"id\":\"msg_2\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\",\"annotations\":[]}}]}}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_2\",\"status\":\"completed\",\"usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}}}\n\n"
    )
}

#[tokio::test]
async fn agent_turn_read_image_reaches_anthropic_tool_result_wire() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("dot.png"), PNG_1X1).unwrap();
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(fixture("anthropic.sse").replace("src/main.rs", "dot.png")),
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "model": "anthropic/claude-sonnet-5-5" })).await;

        let res = h.prompt("read dot.png", json!({})).await.unwrap();
        assert_eq!(res["termination_reason"], "completed", "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2, "expected 2 LLM requests (tool_use + final)");
        let body = reqs[1].json();
        let msgs = body["messages"].as_array().unwrap();
        // user, assistant(tool_use), user(tool_result) — NO extra displaced user message
        assert_eq!(msgs.len(), 3, "tool_result image must stay inside tool_result, not a separate user msg: {msgs:?}");
        let user_blocks = msgs[2]["content"].as_array().unwrap();
        assert_eq!(user_blocks.len(), 1, "expected only tool_result block: {user_blocks:?}");
        let tr = &user_blocks[0];
        assert_eq!(tr["type"], "tool_result");
        assert_eq!(tr["is_error"], false);
        let content = tr["content"].as_array().expect("tool_result content must be block array");
        assert!(content.iter().any(|b| b["type"] == "text"), "expected text block in tool_result: {content:?}");
        assert!(
            content.iter().any(|b| b["type"] == "image" && b["source"]["media_type"] == "image/png"),
            "expected image block in tool_result: {content:?}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_turn_read_image_reaches_google_gemini_3_wire() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("dot.png"), PNG_1X1).unwrap();
        let server = MockServer::start().await;
        server.route(
            "/google",
            vec![
                Resp::sse(fixture("google.sse").replace("src/main.rs", "dot.png")),
                Resp::sse(google_text_sse("saw dot.png")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "model": "google/gemini-3-pro-preview" })).await;

        let res = h.prompt("read dot.png", json!({})).await.unwrap();
        assert_eq!(res["termination_reason"], "completed", "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let body = reqs[1].json();
        let contents = body["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3, "expected user, model, functionResponse (no extra user turn): {contents:?}");
        let fr = &contents[2]["parts"][0]["functionResponse"];
        let parts = fr["parts"].as_array().expect("Gemini 3 functionResponse.parts must contain image");
        assert_eq!(parts[0]["inlineData"]["mimeType"], "image/png", "{fr}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_turn_read_image_reaches_openai_compat_wire() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("dot.png"), PNG_1X1).unwrap();
        let server = MockServer::start().await;
        server.route(
            "/compat",
            vec![
                Resp::sse(fixture("openai-compat.sse").replace("src/main.rs", "dot.png")),
                Resp::sse(openai_compat_text_sse("saw dot.png")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "model": "deepseek/deepseek-reasoner" })).await;

        let res = h.prompt("read dot.png", json!({})).await.unwrap();
        assert_eq!(res["termination_reason"], "completed", "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let body = reqs[1].json();
        let msgs = body["messages"].as_array().unwrap();
        // system, user, assistant, tool, user(Attached image(s) from tool result:)
        assert_eq!(msgs.len(), 5, "expected trailing wire user image message: {msgs:?}");
        assert_eq!(msgs[3]["role"], "tool");
        assert_eq!(msgs[4]["role"], "user");
        assert_eq!(msgs[4]["content"][0]["text"], "Attached image(s) from tool result:");
        assert_eq!(msgs[4]["content"][1]["type"], "image_url");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_turn_read_image_reaches_openai_responses_wire() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("dot.png"), PNG_1X1).unwrap();
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![
                Resp::sse(fixture("openai-responses.sse").replace("src/main.rs", "dot.png")),
                Resp::sse(openai_responses_text_sse("saw dot.png")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "model": "openai/gpt-6.1-terra" })).await;

        let res = h.prompt("read dot.png", json!({})).await.unwrap();
        assert_eq!(res["termination_reason"], "completed", "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let body = reqs[1].json();
        let input = body["input"].as_array().unwrap();
        let fco = input.iter().find(|it| it["type"] == "function_call_output").expect("function_call_output in input");
        let output = fco["output"].as_array().expect("function_call_output.output must be array with input_image");
        assert!(output.iter().any(|b| b["type"] == "input_text"), "{output:?}");
        assert!(
            output.iter().any(|b| b["type"] == "input_image"
                && b["image_url"].as_str().is_some_and(|u| u.starts_with("data:image/png;base64,"))),
            "{output:?}"
        );
        assert!(
            !body.to_string().contains("[image output from the tool calls above]"),
            "must not emit synthetic loop_tools user message: {body}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn agent_turn_read_image_respects_non_vision_model() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::write(env.cwd.join("dot.png"), PNG_1X1).unwrap();
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(fixture("anthropic.sse").replace("src/main.rs", "dot.png")),
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "anthropic/claude-sonnet-5-5",
                "models": { "anthropic/claude-sonnet-5-5": { "input": ["text"] } },
            }),
        )
        .await;

        let res = h.prompt("read dot.png", json!({})).await.unwrap();
        assert_eq!(res["termination_reason"], "completed", "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let body = reqs[1].json();
        let tr = &body["messages"][2]["content"][0];
        assert_eq!(tr["type"], "tool_result");
        let content_str = tr["content"].as_str().expect("text-only tool_result content must be string");
        assert!(
            content_str.contains("Attachment: ") || content_str.contains("image omitted"),
            "non-vision model must downgrade image to text: {content_str:?}"
        );
        assert!(!body.to_string().contains(r#""type":"image""#), "non-vision model must not emit image block: {body}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_gemini_3_routes_unsupported_tool_result_image_mimes_to_user_turn() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        stream_history(
            &h,
            "google/gemini-3-pro-preview",
            r#"{
              { role = "user", content = { { type = "text", text = "look" } } },
              { role = "assistant", provider = "other", content = {
                  { type = "tool_call", id = "call_1", name = "read", arguments = { path = "a.png" } },
                  { type = "tool_call", id = "call_2", name = "read", arguments = { path = "b.heic" } } } },
              { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = {
                  { type = "text", text = "Mixed images" },
                  { type = "image", data = "PNG1", mime = "image/png" },
                  { type = "image", data = "HEIC1", mime = "image/heic" } } },
              { role = "tool_result", tool_call_id = "call_2", tool_name = "read", content = {
                  { type = "image", data = "HEIF1", mime = "image/heif" } } },
            }"#,
        )
        .await;

        let body = server.requests().pop().unwrap().json();
        let contents = body["contents"].as_array().unwrap();
        // user, model, user(2x functionResponse), user(Tool result image: + heic + heif)
        assert_eq!(contents.len(), 4, "expected trailing user turn for unsupported mimes: {contents:?}");

        let fr1 = &contents[2]["parts"][0]["functionResponse"];
        let fr1_parts = fr1["parts"].as_array().expect("supported image/png must stay in functionResponse.parts");
        assert_eq!(fr1_parts.len(), 1, "only image/png should be in functionResponse.parts: {fr1_parts:?}");
        assert_eq!(fr1_parts[0]["inlineData"]["mimeType"], "image/png");

        let fr2 = &contents[2]["parts"][1]["functionResponse"];
        assert_eq!(fr2["response"]["output"], "(see attached image)");
        assert!(fr2.get("parts").is_none(), "unsupported image/heif must not create functionResponse.parts: {fr2}");

        assert_eq!(contents[3]["role"], "user");
        assert_eq!(
            contents[3]["parts"],
            json!([
                { "text": "Tool result image:" },
                { "inlineData": { "mimeType": "image/heic", "data": "HEIC1" } },
                { "inlineData": { "mimeType": "image/heif", "data": "HEIF1" } },
            ])
        );
        h.shutdown().await;
    })
    .await
}
