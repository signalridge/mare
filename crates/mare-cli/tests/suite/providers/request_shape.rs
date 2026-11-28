//! The request each provider builds, and the stream it reads back.

use super::*;

async fn replay(model: &str, prefix: &str, fixture_name: &str, cache_write: i64, path: &str, markers: &[&str]) {
    let mut env = TestEnv::new().await;
    let server = MockServer::start().await;
    server.route(prefix, vec![Resp::sse(fixture(fixture_name))]);
    let h = provider_host(&mut env, &server, json!({})).await;
    let msg = stream(&h, model).await;
    assert_fixture_message(model, &msg, cache_write);
    let req = server.requests().pop().expect("request recorded");
    assert!(req.path.starts_with(path), "{model}: path {}", req.path);
    for m in markers {
        assert!(req.body.contains(m), "{model}: request body lacks {m}: {}", req.body);
    }
    h.shutdown().await;
}

#[tokio::test]
async fn openai_responses_stream_and_request_shape() {
    local(replay(
        "openai/gpt-6.1-terra",
        "/openai",
        "openai-responses.sse",
        0,
        "/openai/responses",
        &["\"instructions\":\"SYSTEM-PROMPT\"", "function_call_output", "FILE-A-CONTENT"],
    ))
    .await
}

#[tokio::test]
async fn anthropic_stream_and_request_shape_with_cache_markers() {
    local(replay(
        "anthropic/claude-sonnet-5-5",
        "/anthropic",
        "anthropic.sse",
        5,
        "/anthropic/v1/messages",
        &["cache_control", "tool_result", "FILE-A-CONTENT"],
    ))
    .await
}

#[tokio::test]
async fn openai_compat_stream_and_request_shape() {
    local(replay(
        "deepseek/deepseek-reasoner",
        "/compat",
        "openai-compat.sse",
        0,
        "/compat/chat/completions",
        &["stream_options", "\"role\":\"tool\"", "FILE-A-CONTENT"],
    ))
    .await
}

#[tokio::test]
async fn google_stream_and_request_shape() {
    local(replay(
        "google/gemini-2.5-pro",
        "/google",
        "google.sse",
        0,
        "/google/models/gemini-2.5-pro:streamGenerateContent",
        &["functionResponse", "systemInstruction", "FILE-A-CONTENT"],
    ))
    .await
}

#[tokio::test]
async fn agent_request_carries_system_prompt_and_ordered_tool_schemas() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({ "model": "anthropic/claude-sonnet-5-5" })).await;
        let res = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(res["text"], "All done.", "{res}");
        let body = server.requests().pop().unwrap().json();
        assert!(body["system"].to_string().contains("mare"), "system prompt missing: {}", body["system"]);
        let bash = body["tools"].as_array().unwrap().iter().find(|t| t["name"] == "bash").expect("bash tool");
        let keys: Vec<&String> = bash["input_schema"]["properties"].as_object().unwrap().keys().collect();
        assert_eq!(keys.first().map(|k| k.as_str()), Some("description"), "param order: {keys:?}");
        assert!(!bash.to_string().contains("x-mare-order"), "the order hint must not reach the provider");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn wire_schema_preserves_order_key_inside_const_and_property_names() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({ "model": "anthropic/claude-sonnet-5-5" })).await;
        expose(&h);
        exec(
            &h,
            r#"T:register("identity_schema_probe", {
            description = "probe", parameters = mare.json.decode_lossless([=[
                {"type":"object", "properties":{"x-mare-order":{"const":{"x-mare-order":["keep"],"properties":{}}}}}
            ]=]), execute = function() return "ok" end,
        })"#,
        )
        .await;
        h.prompt("hello", json!({})).await.unwrap();
        let body = server.requests().pop().unwrap().json();
        let tool = body["tools"].as_array().unwrap().iter().find(|t| t["name"] == "identity_schema_probe").unwrap();
        assert_eq!(tool["input_schema"]["properties"]["x-mare-order"]["const"]["x-mare-order"], json!(["keep"]));
        assert_eq!(tool["input_schema"]["properties"]["x-mare-order"]["const"]["properties"], json!({}));
        h.shutdown().await;
    })
    .await
}

pub(super) fn anthropic_tool_use_sse(id: &str, name: &str, input_json: &str) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_{id}\",\"role\":\"assistant\",\"content\":[],\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\",\"input\":{{}}}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"tool_use\"}},\"usage\":{{\"output_tokens\":3}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n",
        serde_json::to_string(input_json).unwrap()
    )
}

#[tokio::test]
async fn anthropic_defer_loading_tools_and_tool_reference_replay() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let script = r#"
            local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local history = {
                { role = "user", content = { { type = "text", text = "find echo" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "call_s1", name = "tool_search", arguments = { query = "echo" } },
                    { type = "tool_call", id = "call_r1", name = "read", arguments = { path = "a.txt" } },
                } },
                { role = "tool_result", tool_call_id = "call_s1", tool_name = "tool_search",
                  content = { { type = "text", text = "Loaded mcp__fake__echo" } },
                  details = { loaded_tools = { "mcp__fake__echo", "mcp__fake__missing" } } },
                { role = "tool_result", tool_call_id = "call_r1", tool_name = "read",
                  content = { { type = "text", text = "FILE-A" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "call_s2", name = "tool_search", arguments = { query = "echo again" } },
                    { type = "tool_call", id = "call_s3", name = "tool_search", arguments = { query = "other" } },
                } },
                { role = "tool_result", tool_call_id = "call_s2", tool_name = "tool_search",
                  content = { { type = "text", text = "Already loaded mcp__fake__echo" } },
                  details = { loaded_tools = { "mcp__fake__echo" } } },
                { role = "tool_result", tool_call_id = "call_s3", tool_name = "tool_search", is_error = true,
                  content = { { type = "text", text = "search failed" } },
                  details = { loaded_tools = { "mcp__fake__other" } } },
            }
            return LLM:stream({
                model = m,
                system = "SYS",
                messages = history,
                tools = {
                    { name = "read", description = "Read", parameters = { type = "object", properties = {} } },
                    { name = "tool_search", description = "Search", parameters = { type = "object", properties = {} } },
                    { name = "mcp__fake__echo", description = "Echo", defer_loading = true, parameters = { type = "object", properties = {} } },
                    { name = "mcp__fake__other", description = "Other", defer_loading = true, parameters = { type = "object", properties = {} } },
                },
            }, function() end)
        "#;
        let f = h.lua().load(script).into_function().unwrap();
        let _: mlua::Value = f.call_async(()).await.unwrap();

        let req = server.requests().pop().expect("request recorded");
        assert!(
            req.header("anthropic-beta").is_some_and(|v| v.contains("advanced-tool-use-2025-11-20")),
            "expected advanced-tool-use beta header, got {:?}",
            req.headers
        );
        let body = req.json();
        let tools = body["tools"].as_array().unwrap();
        let by_name = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap();
        assert!(by_name("read").get("defer_loading").is_none());
        assert!(by_name("tool_search").get("defer_loading").is_none());
        assert_eq!(by_name("mcp__fake__echo")["defer_loading"], json!(true));
        assert_eq!(by_name("mcp__fake__other")["defer_loading"], json!(true));

        let msgs = body["messages"].as_array().unwrap();
        // First tool_result user message: two tool_result blocks first, then displaced text block.
        let tr1 = msgs[2]["content"].as_array().unwrap();
        assert_eq!(tr1.len(), 3, "expected 2 tool_results + 1 displaced text: {tr1:?}");
        assert_eq!(tr1[0]["type"], "tool_result");
        assert_eq!(tr1[0]["tool_use_id"], "call_s1");
        assert_eq!(tr1[0]["content"], json!([{ "type": "tool_reference", "tool_name": "mcp__fake__echo" }]));
        assert_eq!(tr1[1]["type"], "tool_result");
        assert_eq!(tr1[1]["tool_use_id"], "call_r1");
        assert_eq!(tr1[1]["content"], "FILE-A");
        assert_eq!(tr1[2]["type"], "text");
        assert_eq!(tr1[2]["text"], "Loaded mcp__fake__echo");

        // Second tool_result user message: duplicate load degrades to text; is_error stays text.
        let tr2 = msgs[4]["content"].as_array().unwrap();
        assert_eq!(tr2.len(), 2, "expected 2 plain tool_results: {tr2:?}");
        assert_eq!(tr2[0]["type"], "tool_result");
        assert_eq!(tr2[0]["content"], "Already loaded mcp__fake__echo");
        assert_eq!(tr2[1]["type"], "tool_result");
        assert_eq!(tr2[1]["is_error"], json!(true));
        assert_eq!(tr2[1]["content"], "search failed");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_deferred_mcp_tool_search_preserves_tools_array_and_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let log = env.home.path().join("mcp.log");
        let mcp_cfg = json!({
            "plugins": {
                "mcp": {
                    "servers": {
                        "fake": {
                            "command": fake_mcp_bin(),
                            "args": ["tiered", log.to_string_lossy(), "0"],
                            "tools": { "shout": "hidden" }
                        }
                    }
                }
            }
        });
        // Warm the disk tool-list cache in an initial host (just like a prior session).
        let warm = env.host().builtins(&["mcp"]).config(mcp_cfg.clone()).no_session().build().await;
        expose(&warm);
        exec(
            &warm,
            r#"T:execute({ id = "w1", name = "tool_search", arguments = { query = "echo" } }, {})
               C.cordis:get("bus"):emit("request/prepare", {})"#,
        )
        .await;
        warm.shutdown().await;

        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(anthropic_tool_use_sse("t1", "tool_search", r#"{"query":"select:mcp__fake__echo"}"#)),
                Resp::sse(anthropic_tool_use_sse("t2", "mcp__fake__echo", r#"{"text":"hello"}"#)),
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "anthropic/claude-sonnet-5-5",
                "plugins": mcp_cfg["plugins"]
            }),
        )
        .await;
        expose(&h);

        // Models without native defer_loading keep deferred MCP tools hidden until tool_search loads them.
        let non_native_has_echo: bool = eval(
            &h,
            r#"for _, ref in ipairs({ "deepseek/deepseek-chat", "openai/gpt-5.3" }) do
                 local m = LLM:resolve(ref)
                 for _, d in ipairs(T:definitions(m)) do
                   if d.name == "mcp__fake__echo" then return true end
                 end
               end
               return false"#,
        )
        .await;
        assert!(!non_native_has_echo, "models without native defer_loading must not declare deferred tools upfront");

        let res = h.prompt("use echo", json!({})).await.unwrap();
        assert_eq!(res["text"], "All done.", "{res}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 3, "expected 3 turns (tool_search -> echo -> done)");
        assert!(
            reqs[0].header("anthropic-beta").is_some_and(|v| v.contains("advanced-tool-use-2025-11-20")),
            "expected advanced-tool-use beta header on turn 1"
        );
        let b0 = reqs[0].json();
        let b1 = reqs[1].json();
        let b2 = reqs[2].json();

        let tools0 = b0["tools"].as_array().unwrap();
        let echo0 = tools0.iter().find(|t| t["name"] == "mcp__fake__echo").expect("deferred echo on turn 1");
        assert_eq!(echo0["defer_loading"], json!(true));
        let search0 = tools0.iter().find(|t| t["name"] == "tool_search").expect("tool_search on turn 1");
        assert!(search0.get("defer_loading").is_none());
        assert!(tools0.iter().all(|t| t["name"] != "mcp__fake__shout"), "hidden tool must stay excluded");

        assert_eq!(b1["tools"], b0["tools"], "tools array must stay identical after tool_search");
        assert_eq!(b2["tools"], b0["tools"], "tools array must stay identical after calling loaded tool");

        let msgs1 = b1["messages"].as_array().unwrap();
        let tool_ref_present = msgs1.iter().any(|m| {
            m["content"].as_array().is_some_and(|blocks| {
                blocks.iter().any(|b| {
                    b["type"] == "tool_result"
                        && b["content"].as_array().is_some_and(|inner| {
                            inner.iter().any(|r| r["type"] == "tool_reference" && r["tool_name"] == "mcp__fake__echo")
                        })
                })
            })
        });
        assert!(tool_ref_present, "turn 2 must replay tool_search load as tool_reference: {msgs1:?}");

        let echo_result_present = b2["messages"].to_string().contains("echo: hello");
        assert!(echo_result_present, "turn 3 must carry mcp__fake__echo result");
        h.shutdown().await;
    })
    .await
}

/// Regression after pi `anthropic-error-tool-result-images.test.ts:67-140` (#10794):
/// Anthropic rejects error tool results that contain images, so images in `is_error` tool results
/// are replaced with a placeholder and moved into a subsequent `user` message after all tool
/// results of the turn.
#[tokio::test]
async fn anthropic_error_tool_result_splits_images_into_subsequent_user_message() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let script = r#"
            local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local history = {
                { role = "user", content = { { type = "text", text = "Show the screenshots" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "call-1", name = "codemode", arguments = {} },
                    { type = "tool_call", id = "call-2", name = "read", arguments = {} },
                } },
                { role = "tool_result", tool_call_id = "call-1", tool_name = "codemode", is_error = true,
                  content = {
                      { type = "text", text = "Script failed" },
                      { type = "image", data = "AAAA", mime = "image/png" },
                      { type = "image", data = "CCCC", mime = "image/png" },
                      { type = "text", text = "Script error" },
                  } },
                { role = "tool_result", tool_call_id = "call-2", tool_name = "read", is_error = false,
                  content = { { type = "text", text = "ok" } } },
            }
            return LLM:stream({
                model = m,
                system = "SYS",
                messages = history,
            }, function() end)
        "#;
        let f = h.lua().load(script).into_function().unwrap();
        let _: mlua::Value = f.call_async(()).await.unwrap();

        let body = server.requests().pop().expect("request recorded").json();
        let msgs = body["messages"].as_array().unwrap();
        let tr = msgs[2]["content"].as_array().unwrap();
        assert_eq!(tr.len(), 5, "expected 2 tool_results + caption + 2 images: {tr:?}");
        assert_eq!(tr[0]["type"], "tool_result");
        assert_eq!(tr[0]["tool_use_id"], "call-1");
        assert_eq!(tr[0]["is_error"], json!(true));
        assert_eq!(tr[0]["content"], "Script failed\n(image attached after the tool results)\nScript error");
        assert_eq!(tr[1]["type"], "tool_result");
        assert_eq!(tr[1]["tool_use_id"], "call-2");
        assert_eq!(tr[1]["is_error"], json!(false));
        assert_eq!(tr[1]["content"], "ok");
        assert_eq!(tr[2]["type"], "text");
        assert_eq!(tr[2]["text"], "Images from the error result of tool call call-1 (codemode):");
        assert_eq!(tr[3]["type"], "image");
        assert_eq!(tr[3]["source"]["data"], "AAAA");
        assert_eq!(tr[4]["type"], "image");
        assert_eq!(tr[4]["source"]["data"], "CCCC");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_error_tool_result_without_images_and_non_vision_unchanged() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![Resp::sse(fixture("anthropic-text.sse")), Resp::sse(fixture("anthropic-text.sse"))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "models": { "anthropic/claude-haiku-5-5": { "images": false } } }),
        )
        .await;
        let script = r#"
            local vision = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local no_img = assert(LLM:resolve("anthropic/claude-haiku-5-5"))
            local history = {
                { role = "user", content = { { type = "text", text = "run" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "call_err_txt", name = "bash", arguments = {} },
                } },
                { role = "tool_result", tool_call_id = "call_err_txt", tool_name = "bash", is_error = true,
                  content = { { type = "text", text = "exit 1" } } },
            }
            LLM:stream({ model = vision, system = "SYS", messages = history }, function() end)
            local img_history = {
                { role = "user", content = { { type = "text", text = "run" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "call_err_img", name = "codemode", arguments = {} },
                } },
                { role = "tool_result", tool_call_id = "call_err_img", tool_name = "codemode", is_error = true,
                  content = { { type = "image", data = "IMG1", mime = "image/png" } } },
            }
            LLM:stream({ model = no_img, system = "SYS", messages = img_history }, function() end)
        "#;
        let f = h.lua().load(script).into_function().unwrap();
        let _: mlua::Value = f.call_async(()).await.unwrap();

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let b0 = reqs[0].json();
        let tr0 = b0["messages"][2]["content"].as_array().unwrap();
        assert_eq!(tr0.len(), 1, "plain error tool_result must stay single block: {tr0:?}");
        assert_eq!(tr0[0]["content"], "exit 1");
        assert_eq!(tr0[0]["is_error"], json!(true));

        let b1 = reqs[1].json();
        let tr1 = b1["messages"][2]["content"].as_array().unwrap();
        assert_eq!(tr1.len(), 1, "no-image model must not emit trailing image blocks: {tr1:?}");
        assert_eq!(tr1[0]["content"], "(image omitted: model does not support images)");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_error_tool_result_keeps_paired_image_and_drops_orphaned_image() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai",
            vec![Resp::sse(fixture("openai-responses.sse")), Resp::sse(fixture("openai-responses.sse"))],
        );
        let h = provider_host(&mut env, &server, json!({})).await;
        let script = r#"
            local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local paired_history = {
                { role = "user", content = { { type = "text", text = "check" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "call_1", name = "codemode", arguments = {} },
                } },
                { role = "tool_result", tool_call_id = "call_1", tool_name = "codemode", is_error = true,
                  content = {
                      { type = "text", text = "failed" },
                      { type = "image", data = "AAAA", mime = "image/png" },
                  } },
            }
            LLM:stream({ model = m, system = "SYS", messages = paired_history }, function() end)

            local orphaned_history = {
                { role = "user", content = { { type = "text", text = "check" } } },
                { role = "tool_result", tool_call_id = "call_orphan", tool_name = "codemode", is_error = true,
                  content = {
                      { type = "text", text = "orphan failed" },
                      { type = "image", data = "ORPHAN", mime = "image/png" },
                  } },
            }
            LLM:stream({ model = m, system = "SYS", messages = orphaned_history }, function() end)
        "#;
        let f = h.lua().load(script).into_function().unwrap();
        let _: mlua::Value = f.call_async(()).await.unwrap();

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);

        // Paired error tool_result keeps its input_image inside function_call_output, with no extra user message.
        let paired_input = reqs[0].json()["input"].as_array().unwrap().clone();
        let fco = paired_input.iter().find(|it| it["type"] == "function_call_output").expect("function_call_output");
        let output = fco["output"].as_array().expect("output array with input_image");
        assert!(
            output.iter().any(|b| b["type"] == "input_image"),
            "expected input_image inside function_call_output: {output:?}"
        );
        assert_eq!(
            paired_input.iter().filter(|it| it["role"] == "user").count(),
            1,
            "must not emit extra user message for OpenAI Responses: {paired_input:?}"
        );

        // Orphaned error tool_result is dropped completely by normalize, leaving 0 images.
        let orphan_body = reqs[1].json();
        assert!(
            !orphan_body.to_string().contains("ORPHAN"),
            "orphaned error tool_result image must be dropped: {orphan_body}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn opencode_go_anthropic_error_tool_result_image_preserves_unsigned_thinking() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        server.route("/opencode-go", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-opencode-go": {
                        "base_url": format!("{}/opencode-go", server.base),
                        "model_routes": { "claude-*": "anthropic" },
                    }
                }
            }),
        )
        .await;
        let script = r#"
            local m = assert(LLM:resolve("opencode-go/claude-haiku-5-5"))
            local history = {
                { role = "user", content = { { type = "text", text = "check" } } },
                { role = "assistant", provider_id = "opencode-go", route = "anthropic", content = {
                    { type = "reasoning", text = "Need to inspect screenshot." },
                    { type = "tool_call", id = "call_1", name = "codemode", arguments = {} },
                } },
                { role = "tool_result", tool_call_id = "call_1", tool_name = "codemode", is_error = true,
                  content = {
                      { type = "text", text = "failed" },
                      { type = "image", data = "AAAA", mime = "image/png" },
                  } },
            }
            return LLM:stream({ model = m, system = "SYS", messages = history }, function() end)
        "#;
        let f = h.lua().load(script).into_function().unwrap();
        let _: mlua::Value = f.call_async(()).await.unwrap();

        let body = server.requests().pop().expect("request recorded").json();
        let msgs = body["messages"].as_array().unwrap();
        let asst_blocks = msgs[1]["content"].as_array().unwrap();
        assert!(
            asst_blocks.iter().any(|b| b["type"] == "thinking" && b["thinking"] == "Need to inspect screenshot."),
            "unsigned thinking in current tool loop must be preserved: {asst_blocks:?}"
        );
        let tr = msgs[2]["content"].as_array().unwrap();
        assert_eq!(tr[0]["type"], "tool_result");
        assert_eq!(tr[0]["is_error"], json!(true));
        assert_eq!(tr[0]["content"], "failed\n(image attached after the tool results)");
        assert_eq!(tr[1]["type"], "text");
        assert_eq!(tr[2]["type"], "image");
        assert_eq!(tr[2]["source"]["data"], "AAAA");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn anthropic_mixed_textified_and_error_image_tool_results_flush_images_after_all_tool_results() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        let script = r#"
            local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local history = {
                { role = "user", content = { { type = "text", text = "run tools" } } },
                { role = "assistant", content = {
                    { type = "tool_call", id = "call_a", name = "read", arguments = {} },
                    { type = "tool_call", id = "call_b1", name = "removed_tool_1", arguments = {} },
                    { type = "tool_call", id = "call_b2", name = "removed_tool_2", arguments = {} },
                    { type = "tool_call", id = "call_c", name = "bash", arguments = {} },
                } },
                { role = "tool_result", tool_call_id = "call_a", tool_name = "read", is_error = true,
                  content = {
                      { type = "text", text = "A failed" },
                      { type = "image", data = "IMG_A", mime = "image/png" },
                  } },
                { role = "tool_result", tool_call_id = "call_b1", tool_name = "removed_tool_1", is_error = false,
                  content = { { type = "text", text = "B1 text" } } },
                { role = "tool_result", tool_call_id = "call_b2", tool_name = "removed_tool_2", is_error = false,
                  content = { { type = "text", text = "B2 text" } } },
                { role = "tool_result", tool_call_id = "call_c", tool_name = "bash", is_error = true,
                  content = {
                      { type = "text", text = "C failed" },
                      { type = "image", data = "IMG_C", mime = "image/png" },
                  } },
            }
            return LLM:stream({
                model = m,
                system = "SYS",
                messages = history,
                tools = {
                    { name = "read", description = "r", parameters = { type = "object", properties = {} } },
                    { name = "bash", description = "b", parameters = { type = "object", properties = {} } },
                },
            }, function() end)
        "#;
        let f = h.lua().load(script).into_function().unwrap();
        let _: mlua::Value = f.call_async(()).await.unwrap();

        let body = server.requests().pop().expect("request recorded").json();
        let blocks = body["messages"][2]["content"].as_array().unwrap();
        let first_non_tool_result_idx =
            blocks.iter().position(|b| b["type"] != "tool_result").expect("non-tool_result block");
        let last_tool_result_idx =
            blocks.iter().rposition(|b| b["type"] == "tool_result").expect("tool_result block");
        assert!(
            last_tool_result_idx < first_non_tool_result_idx,
            "all tool_result blocks must precede every non-tool_result block (textified results and displaced images): {blocks:?}"
        );
        assert_eq!(first_non_tool_result_idx, 2, "expected 2 tool_result blocks first: {blocks:?}");
        let imgs: Vec<&str> =
            blocks.iter().filter(|b| b["type"] == "image").filter_map(|b| b["source"]["data"].as_str()).collect();
        assert_eq!(imgs, vec!["IMG_A", "IMG_C"]);
        h.shutdown().await;
    })
    .await
}

/// Issue #189: a valid no-argument tool with bare `{"type":"object"}` schema must emit an
/// explicit empty `"properties": {}` map on OpenAI Chat Completions wire requests.
#[tokio::test]
async fn openai_chat_wire_emits_empty_properties_map_for_bare_object_tool() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
        let h = provider_host(&mut env, &server, json!({ "model": "deepseek/deepseek-chat" })).await;
        expose(&h);
        exec(
            &h,
            r#"T:register("bare_no_args_tool", {
                description = "no-argument tool",
                parameters = { type = "object" },
                execute = function() return "ok" end,
            })
            T:register("strict_no_args_tool", {
                description = "strict no-argument tool",
                parameters = { type = "object", additionalProperties = false },
                execute = function() return "ok" end,
            })"#,
        )
        .await;
        h.prompt("hello", json!({})).await.unwrap();
        let body = server.requests().pop().unwrap().json();
        let tools = body["tools"].as_array().unwrap();
        let tool =
            tools.iter().find(|t| t["function"]["name"] == "bare_no_args_tool").expect("bare_no_args_tool in request");
        assert_eq!(
            tool["function"]["parameters"].get("properties"),
            Some(&json!({})),
            "OpenAI Chat tool parameters omitted empty properties map: {}",
            tool["function"]["parameters"]
        );
        let strict_tool = tools
            .iter()
            .find(|t| t["function"]["name"] == "strict_no_args_tool")
            .expect("strict_no_args_tool in request");
        assert_eq!(
            strict_tool["function"]["parameters"],
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            "OpenAI Chat tool parameters must add empty properties map without dropping existing fields: {}",
            strict_tool["function"]["parameters"]
        );
        h.shutdown().await;
    })
    .await
}
