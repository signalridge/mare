//! Vendor-specific native deferred tool loading (`defer_loading`) on OpenAI-compatible wires:
//! - Kimi (`kimi`/`moonshot` dialect on `openai-chat`): top-level `tools[]` omits deferred tools
//!   so the prefix stays byte-stable across turns, and loaded deferred tools are declared in-place
//!   via `{ role = "system", tools = [...] }` messages (no `content` key) right after the
//!   `tool_result` batch.
//! - Non-Kimi dialects and Kimi K2 models (without `defer_loading`) never emit tool-bearing
//!   system messages.
//! - DeepSeek built-in preset uses `openai-responses`, with `tool_update = "addition_only"` on
//!   `deepseek-flash` (native `tool_search` / `tool_search_call` / `tool_search_output`) and not
//!   on `deepseek-v4-pro`; third-party `anthropic-messages` data-tier providers register cleanly
//!   without implicitly enabling `tool_update`.

use super::*;
use crate::suite::kernel::write_plugin;

const DEFERRED_TOOLS_PLUGIN: &str = r#"return {
  name = "vendor-deferred-test",
  inject = { "tools" },
  apply = function(ctx)
    ctx.tools:register("alpha_tool", {
      description = "Run alpha action",
      exposure = "deferred",
      side_effect = "read",
      parameters = {
        type = "object",
        properties = { value = { type = "string", description = "Input value" } },
        required = { "value" },
      },
      execute = function(a)
        return { content = "alpha:" .. tostring(a.value or "") }
      end,
    })
    ctx.tools:register("beta_tool", {
      description = "Run beta action",
      exposure = "deferred",
      side_effect = "read",
      parameters = { type = "object", properties = {} },
      execute = function()
        return { content = "beta ok" }
      end,
    })
  end,
}"#;

fn chat_tool_call_sse(id: &str, name: &str, args_json: &str) -> Resp {
    let chunk0 = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": "kimi-k3",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "index": 0,
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": args_json },
                }],
            },
            "finish_reason": Value::Null,
        }],
    });
    let chunk1 = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": "kimi-k3",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }],
        "usage": { "prompt_tokens": 50, "completion_tokens": 10, "total_tokens": 60 },
    });
    Resp::sse(format!("data: {chunk0}\n\ndata: {chunk1}\n\ndata: [DONE]\n\n"))
}

fn chat_text_sse(text: &str) -> Resp {
    let chunk0 = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": "kimi-k3",
        "choices": [{ "index": 0, "delta": { "role": "assistant", "content": text }, "finish_reason": Value::Null }],
    });
    let chunk1 = json!({
        "id": "chatcmpl-2",
        "object": "chat.completion.chunk",
        "created": 1700000000,
        "model": "kimi-k3",
        "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 60, "completion_tokens": 5, "total_tokens": 65 },
    });
    Resp::sse(format!("data: {chunk0}\n\ndata: {chunk1}\n\ndata: [DONE]\n\n"))
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|t| t["function"]["name"].as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

fn system_tool_messages(body: &Value) -> Vec<&Value> {
    body["messages"]
        .as_array()
        .map(|arr| arr.iter().filter(|m| m["role"] == "system" && m.get("tools").is_some()).collect())
        .unwrap_or_default()
}

#[tokio::test]
async fn kimi_defer_loading_keeps_top_level_tools_stable_and_injects_system_tools() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MOONSHOT_API_KEY", "test-moonshot");
        write_plugin(&env, "vendor-deferred-test", DEFERRED_TOOLS_PLUGIN);

        let server = MockServer::start().await;
        server.route(
            "/kimi/chat/completions",
            vec![
                chat_tool_call_sse("call_search_1", "tool_search", r#"{"query":"select:alpha_tool"}"#),
                chat_tool_call_sse("call_alpha_1", "alpha_tool", r#"{"value":"hello"}"#),
                chat_text_sse("all done"),
            ],
        );

        let base = &server.base;
        let h = env
            .host()
            .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
            .user_plugins()
            .config(json!({
                "providers": {
                    "kimi": { "base_url": format!("{base}/kimi") },
                },
                "plugins": {
                    "llm": { "model": "kimi/kimi-k3", "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        global(&h, "TOOLS", h.service_table("tools").unwrap());

        let (k3_defer, k2_defer, k2t_defer): (bool, bool, bool) = eval(
            &h,
            r#"
            local k3 = assert(LLM:resolve("kimi/kimi-k3"), "kimi/kimi-k3 missing")
            local k2 = assert(LLM:resolve("kimi/kimi-k2-0905-preview"), "kimi-k2 missing")
            local k2t = assert(LLM:resolve("kimi/kimi-k2-thinking"), "kimi-k2-thinking missing")
            return k3.tool_update == "addition_only", k2.tool_update == "addition_only", k2t.tool_update == "addition_only"
            "#,
        )
        .await;
        assert!(k3_defer, "kimi/kimi-k3 must enable tool_update = addition_only");
        assert!(!k2_defer, "kimi/kimi-k2-0905-preview must not enable tool_update");
        assert!(!k2t_defer, "kimi/kimi-k2-thinking must not enable tool_update");

        exec(&h, r#"assert(LLM:set_model("kimi/kimi-k3", { save = false }))"#).await;
        let rev_before: i64 = eval(&h, "return TOOLS:revision()").await;
        let res = h.prompt("load alpha_tool and run it", json!({})).await.unwrap();
        let rev_after: i64 = eval(&h, "return TOOLS:revision()").await;
        h.shutdown().await;

        assert!(res["error"].is_null(), "turn failed: {res}");
        assert_eq!(res["text"], "all done", "{res}");
        assert_eq!(
            rev_before, rev_after,
            "native Kimi defer_loading must not bump tools:revision() when loading deferred tools"
        );

        let reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path == "/kimi/chat/completions").collect();
        assert_eq!(reqs.len(), 3, "expected 3 model steps, got {}", reqs.len());

        let b0 = reqs[0].json();
        let b1 = reqs[1].json();
        let b2 = reqs[2].json();

        // Step 0: top-level tools[] includes tool_search, omits deferred tools and call_tool, and sends no defer_loading key.
        let names0 = tool_names(&b0);
        assert!(names0.iter().any(|n| n == "tool_search"), "step 0 missing tool_search: {names0:?}");
        assert!(!names0.iter().any(|n| n == "call_tool"), "native Kimi must omit call_tool: {names0:?}");
        assert!(!names0.iter().any(|n| n == "alpha_tool"), "step 0 leaked deferred alpha_tool: {names0:?}");
        assert!(!names0.iter().any(|n| n == "beta_tool"), "step 0 leaked deferred beta_tool: {names0:?}");
        for t in b0["tools"].as_array().unwrap() {
            assert!(t.get("defer_loading").is_none(), "openai-chat wire must not send defer_loading key: {t}");
        }
        assert!(system_tool_messages(&b0).is_empty(), "step 0 must have no system tool declaration");

        // Step 1 & 2: top-level tools[] is byte-stable across all steps.
        assert_eq!(b1["tools"], b0["tools"], "step 1 top-level tools[] drifted");
        assert_eq!(b2["tools"], b0["tools"], "step 2 top-level tools[] drifted");

        // Step 1: Kimi shrinks the tool_search tool message to a compact line so schemas aren't duplicated,
        // and emits `{ role: "system", tools: [...] }` right after `role: "tool"`.
        let msgs1 = b1["messages"].as_array().unwrap();
        let tool_pos = msgs1
            .iter()
            .position(|m| m["role"] == "tool" && m["tool_call_id"] == "call_search_1")
            .expect("step 1 missing tool_search result");
        assert_eq!(
            msgs1[tool_pos]["content"], "Loaded: alpha_tool",
            "Kimi wire must compact tool_search content when emitting system tools: {}",
            msgs1[tool_pos]
        );
        let sys_msg = &msgs1[tool_pos + 1];
        assert_eq!(sys_msg["role"], "system", "expected system message after tool_search result: {msgs1:?}");
        assert!(sys_msg.get("content").is_none(), "Kimi tool declaration message must omit `content`: {sys_msg}");
        let declared = sys_msg["tools"].as_array().expect("system message missing tools array");
        assert_eq!(declared.len(), 1, "expected exactly 1 newly loaded tool: {declared:?}");
        assert_eq!(declared[0]["type"], "function");
        assert_eq!(declared[0]["function"]["name"], "alpha_tool");
        assert_eq!(declared[0]["function"]["description"], "Run alpha action");
        assert_eq!(declared[0]["function"]["parameters"]["required"], json!(["value"]));

        // Step 2: prefix up through step 1's messages is byte-identical (KV cache preserved).
        let msgs2 = b2["messages"].as_array().unwrap();
        assert!(msgs2.len() > msgs1.len(), "step 2 should append alpha_tool call + result");
        assert_eq!(&msgs2[..msgs1.len()], &msgs1[..], "step 2 prefix diverged from step 1 messages");
        assert_eq!(system_tool_messages(&b2).len(), 1, "alpha_tool must only be declared once");
    })
    .await
}

#[tokio::test]
async fn kimi_re_search_deduplicates_and_parallel_tool_results_flush_after_batch() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MOONSHOT_API_KEY", "test-moonshot");

        let server = MockServer::start().await;
        server.route("/moonshot/chat/completions", vec![chat_text_sse("ok")]);

        let base = &server.base;
        let h = env
            .host()
            .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
            .config(json!({
                "providers": {
                    "custom-moonshot": {
                        "wire": "openai-chat",
                        "dialect": "moonshot",
                        "base_url": format!("{base}/moonshot"),
                        "api_key": "test-moonshot-custom",
                        "tool_update": "addition_only",
                        "models": [
                            { "model_id": "custom-k3", "name": "Custom K3" }
                        ],
                    },
                },
                "plugins": {
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        exec(
            &h,
            r#"
            local m = assert(LLM:resolve("custom-moonshot/custom-k3"))
            assert(m.tool_update == "addition_only", "custom moonshot provider tool_update should inherit")
            local history = {
              { role = "user", content = { { type = "text", text = "go" } } },
              -- Parallel batch: tool_search + read in the same assistant step.
              { role = "assistant", content = {
                  { type = "tool_call", id = "call_s1", name = "tool_search", arguments = { query = "select:alpha" } },
                  { type = "tool_call", id = "call_r1", name = "read", arguments = { path = "a.txt" } },
              } },
              { role = "tool_result", tool_call_id = "call_s1", tool_name = "tool_search",
                content = { { type = "text", text = "loaded alpha" } },
                details = { loaded_tools = { "alpha" } } },
              { role = "tool_result", tool_call_id = "call_r1", tool_name = "read",
                content = { { type = "text", text = "file text" } } },
              -- Second search: re-loads already-declared alpha plus new beta.
              { role = "assistant", content = {
                  { type = "tool_call", id = "call_s2", name = "tool_search", arguments = { query = "select:alpha,beta" } },
              } },
              { role = "tool_result", tool_call_id = "call_s2", tool_name = "tool_search",
                content = { { type = "text", text = "loaded alpha, beta" } },
                details = { loaded_tools = { "alpha", "beta" } } },
              -- Errored tool_result must never declare gamma.
              { role = "assistant", content = {
                  { type = "tool_call", id = "call_s3", name = "tool_search", arguments = { query = "select:gamma" } },
              } },
              { role = "tool_result", tool_call_id = "call_s3", tool_name = "tool_search",
                is_error = true,
                content = { { type = "text", text = "boom" } },
                details = { loaded_tools = { "gamma" } } },
            }
            local tools = {
              { name = "read", description = "Read file", parameters = { type = "object", properties = {} } },
              { name = "tool_search", description = "Search tools", parameters = { type = "object", properties = {} } },
              { name = "alpha", description = "Alpha tool", defer_loading = true, parameters = { type = "object", properties = {} } },
              { name = "beta", description = "Beta tool", defer_loading = true, parameters = { type = "object", properties = {} } },
              { name = "gamma", description = "Gamma tool", defer_loading = true, parameters = { type = "object", properties = {} } },
            }
            local msg = LLM:stream({ model = m, system = "SYS", messages = history, tools = tools }, function() end)
            assert(not msg.error, tostring(msg.error))
            "#,
        )
        .await;
        h.shutdown().await;

        let req = server
            .requests()
            .into_iter()
            .find(|r| r.path == "/moonshot/chat/completions")
            .expect("missing request");
        let body = req.json();

        assert_eq!(tool_names(&body), vec!["read", "tool_search"]);
        let msgs = body["messages"].as_array().unwrap();

        // Parallel batch: both tool results appear before the injected system tool message.
        let pos_s1 = msgs.iter().position(|m| m["role"] == "tool" && m["tool_call_id"] == "call_s1").unwrap();
        let pos_r1 = msgs.iter().position(|m| m["role"] == "tool" && m["tool_call_id"] == "call_r1").unwrap();
        assert_eq!(pos_r1, pos_s1 + 1, "parallel tool results must stay contiguous: {msgs:?}");
        let sys1 = &msgs[pos_r1 + 1];
        assert_eq!(sys1["role"], "system");
        assert!(sys1.get("content").is_none(), "system tool message must omit content: {sys1}");
        let sys1_names: Vec<_> = sys1["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(sys1_names, vec!["alpha"]);

        // Second search deduplicates alpha and declares only beta.
        let pos_s2 = msgs.iter().position(|m| m["role"] == "tool" && m["tool_call_id"] == "call_s2").unwrap();
        let sys2 = &msgs[pos_s2 + 1];
        assert_eq!(sys2["role"], "system");
        assert!(sys2.get("content").is_none(), "second system tool message must omit content: {sys2}");
        let sys2_names: Vec<_> = sys2["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap()).collect();
        assert_eq!(sys2_names, vec!["beta"]);

        // Errored search does not inject gamma.
        assert_eq!(system_tool_messages(&body).len(), 2, "errored tool_result must not emit tools: {msgs:?}");
    })
    .await
}

#[tokio::test]
async fn kimi_listed_models_dynamically_loaded_tools_sets_defer_loading() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("KIMI_API_KEY", "test-kimi-code");

        let server = MockServer::start().await;
        server.route(
            "/kimi-code/models",
            vec![Resp::status(
                200,
                json!({
                    "data": [
                        {
                            "id": "k3-listed",
                            "capabilities": ["tool_use", "thinking", "dynamically_loaded_tools"],
                        },
                        {
                            "id": "k3-bool",
                            "dynamically_loaded_tools": true,
                        },
                        {
                            "id": "kimi-for-coding-highspeed",
                            "capabilities": ["tool_use"],
                        },
                    ]
                })
                .to_string(),
            )],
        );
        server.route("/kimi-code/chat/completions", vec![chat_text_sse("ok")]);

        let base = &server.base;
        let h = env
            .host()
            .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
            .config(json!({
                "providers": {
                    "kimi-code": { "base_url": format!("{base}/kimi-code") },
                },
                "plugins": {
                    "llm": { "retry_base_ms": 1, "models_dev": false },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (listed_cap, listed_bool, highspeed): (bool, bool, bool) = eval(
            &h,
            r#"
            LLM:refresh_models("kimi-code", { force = true })
            local m1 = assert(LLM:resolve("kimi-code/k3-listed"), "k3-listed missing")
            local m2 = assert(LLM:resolve("kimi-code/k3-bool"), "k3-bool missing")
            local m3 = assert(LLM:resolve("kimi-code/kimi-for-coding-highspeed"), "highspeed missing")
            return m1.tool_update == "addition_only", m2.tool_update == "addition_only", m3.tool_update == "addition_only"
            "#,
        )
        .await;
        assert!(listed_cap, "capabilities=['dynamically_loaded_tools'] must set tool_update");
        assert!(listed_bool, "dynamically_loaded_tools=true must set tool_update");
        assert!(!highspeed, "model without dynamically_loaded_tools must leave tool_update nil");

        exec(
            &h,
            r#"
            local m = assert(LLM:resolve("kimi-code/k3-listed"))
            local msg = LLM:stream({
              model = m,
              system = "SYS",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                { role = "assistant", content = { { type = "tool_call", id = "c1", name = "tool_search", arguments = { query = "x" } } } },
                { role = "tool_result", tool_call_id = "c1", tool_name = "tool_search",
                  content = { { type = "text", text = "ok" } }, details = { loaded_tools = { "dyn_tool" } } },
              },
              tools = {
                { name = "tool_search", description = "search", parameters = { type = "object", properties = {} } },
                { name = "dyn_tool", description = "dynamic", defer_loading = true, parameters = { type = "object", properties = {} } },
              },
            }, function() end)
            assert(not msg.error, tostring(msg.error))
            "#,
        )
        .await;
        h.shutdown().await;

        let req = server
            .requests()
            .into_iter()
            .find(|r| r.path == "/kimi-code/chat/completions")
            .expect("missing chat completions request");
        let body = req.json();
        assert_eq!(tool_names(&body), vec!["tool_search"]);
        let sys = system_tool_messages(&body);
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["tools"][0]["function"]["name"], "dyn_tool");
    })
    .await
}

#[tokio::test]
async fn non_kimi_and_non_deferred_kimi_do_not_emit_system_tools_messages() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");
        env.set_var("MOONSHOT_API_KEY", "test-moonshot");

        let server = MockServer::start().await;
        server.route("/deepseek/chat/completions", vec![chat_text_sse("ok")]);
        server.route("/kimi/chat/completions", vec![chat_text_sse("ok")]);
        server.route("/generic/chat/completions", vec![chat_text_sse("ok")]);

        let base = &server.base;
        let h = env
            .host()
            .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
            .config(json!({
                "providers": {
                    "deepseek": { "wire": "openai-chat", "base_url": format!("{base}/deepseek") },
                    "kimi": { "base_url": format!("{base}/kimi") },
                    "generic-compat": {
                        "wire": "openai-chat",
                        "base_url": format!("{base}/generic"),
                        "api_key": "test-generic",
                        "tool_update": "addition_only",
                        "models": [{ "model_id": "generic-1", "name": "Generic" }],
                    },
                },
                "models": {
                    "deepseek/deepseek-flash": { "tool_update": "addition_only" },
                    "kimi/kimi-k3-non-defer": { "tool_update": false },
                },
                "plugins": {
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        exec(
            &h,
            r#"
            local history = {
              { role = "user", content = { { type = "text", text = "hi" } } },
              { role = "assistant", content = { { type = "tool_call", id = "c1", name = "tool_search", arguments = { query = "x" } } } },
              { role = "tool_result", tool_call_id = "c1", tool_name = "tool_search",
                content = { { type = "text", text = "ok" } }, details = { loaded_tools = { "alpha" } } },
            }
            local tools = {
              { name = "tool_search", description = "search", parameters = { type = "object", properties = {} } },
              { name = "alpha", description = "alpha", defer_loading = true, parameters = { type = "object", properties = {} } },
            }
            for _, ref in ipairs({ "deepseek/deepseek-flash", "kimi/kimi-k3-non-defer", "generic-compat/generic-1" }) do
              local m = assert(LLM:resolve(ref), "missing " .. ref)
              local msg = LLM:stream({ model = m, system = "SYS", messages = history, tools = tools }, function() end)
              assert(not msg.error, ref .. ": " .. tostring(msg.error))
            end
            "#,
        )
        .await;
        h.shutdown().await;

        for path in ["/deepseek/chat/completions", "/kimi/chat/completions", "/generic/chat/completions"] {
            let req = server
                .requests()
                .into_iter()
                .find(|r| r.path == path)
                .unwrap_or_else(|| panic!("missing request for {path}"));
            let body = req.json();
            assert!(
                system_tool_messages(&body).is_empty(),
                "{path} must never emit Kimi system tools messages: {}",
                body["messages"]
            );
            assert_eq!(
                tool_names(&body),
                vec!["tool_search", "alpha"],
                "{path} must not strip deferred tools from top-level tools[]"
            );
        }
    })
    .await
}

#[tokio::test]
async fn deepseek_flash_uses_openai_responses_native_deferred_tools_and_anthropic_data_provider_registers() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("DEEPSEEK_API_KEY", "test-deepseek");

        let server = MockServer::start().await;
        let turn1_sse = [
            responses_event(json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "id": "tsc_ds_1",
                    "type": "tool_search_call",
                    "call_id": "call_ds_search_1",
                    "execution": "client",
                    "status": "completed",
                    "arguments": { "query": "alpha" }
                }
            })),
            responses_event(json!({
                "type": "response.completed",
                "response": { "usage": { "input_tokens": 50, "output_tokens": 10 } }
            })),
        ]
        .concat();
        server.route("/deepseek/responses", vec![Resp::sse(turn1_sse), Resp::sse(responses_text("done"))]);

        let base = &server.base;
        let h = env
            .host()
            .builtins(&["tools-apply-patch", "mcp", "usage", "compact", "user-config", "jobs"])
            .config(json!({
                "providers": {
                    "deepseek": { "base_url": format!("{base}/deepseek") },
                    "deepseek-anthropic": {
                        "wire": "anthropic-messages",
                        "base_url": "https://api.deepseek.com/anthropic",
                        "api_key_env": "DEEPSEEK_API_KEY",
                        "models": [{ "model_id": "deepseek-flash", "name": "DeepSeek Flash" }],
                    },
                },
                "plugins": {
                    "llm": { "retry_base_ms": 1 },
                },
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (ds_flash_defer, ds_pro_defer, ds_wire, da_state, da_wire, da_route, da_model_ok, da_defer): (
            bool,
            bool,
            String,
            String,
            String,
            String,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
                local df = assert(LLM:resolve("deepseek/deepseek-flash"))
                local dp_m = assert(LLM:resolve("deepseek/deepseek-v4-pro"))
                local dp = assert(LLM:provider("deepseek"))
                local st = require("llm.directory").status("deepseek-anthropic") or {}
                local dap = assert(LLM:provider("deepseek-anthropic"))
                local dam = LLM:resolve("deepseek-anthropic/deepseek-flash")

                local tools = {
                  { name = "tool_search", description = "search", parameters = { type = "object", properties = {} } },
                  { name = "alpha", description = "alpha tool", defer_loading = true, parameters = { type = "object", properties = {} } },
                }
                local m1 = LLM:stream({
                  model = df,
                  system = "SYS",
                  messages = { { role = "user", content = { { type = "text", text = "find alpha" } } } },
                  tools = tools,
                }, function() end)
                assert(not m1.error, tostring(m1.error))
                local history = {
                  { role = "user", content = { { type = "text", text = "find alpha" } } },
                  m1,
                  {
                    role = "tool_result",
                    tool_call_id = "call_ds_search_1",
                    tool_name = "tool_search",
                    content = { { type = "text", text = "loaded alpha" } },
                    details = {
                      loaded_tools = { "alpha" },
                    },
                  },
                }
                local m2 = LLM:stream({ model = df, system = "SYS", messages = history, tools = tools }, function() end)
                assert(not m2.error, tostring(m2.error))

                return df.tool_update == "addition_only",
                  dp_m.tool_update == "addition_only",
                  tostring(dp.wire or ""),
                  tostring(st.state or ""),
                  tostring(dap.wire or ""),
                  tostring(dap.route or ""),
                  dam ~= nil,
                  dam ~= nil and dam.tool_update == "addition_only"
                "#,
        )
        .await;
        h.shutdown().await;

        assert!(ds_flash_defer, "deepseek-flash on openai-responses must enable tool_update = addition_only");
        assert!(!ds_pro_defer, "deepseek-v4-pro must keep tool_update = nil");
        assert_eq!(ds_wire, "openai-responses");
        assert_eq!(da_state, "registered", "anthropic-messages must accept data-tier configured endpoints");
        assert_eq!(da_wire, "anthropic-messages");
        assert_eq!(da_route, "anthropic");
        assert!(da_model_ok, "deepseek-anthropic/deepseek-flash must resolve");
        assert!(!da_defer, "third-party anthropic-messages data providers must not implicitly enable tool_update");

        let reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path == "/deepseek/responses").collect();
        assert_eq!(reqs.len(), 2, "expected 2 Responses requests to /deepseek/responses");
        let b1 = reqs[0].json();
        assert!(
            b1["tools"].as_array().unwrap().iter().any(|t| t["type"] == "tool_search" && t["execution"] == "client"),
            "turn 1 must declare client tool_search tool: {}",
            b1["tools"]
        );
        assert!(
            b1["tools"].as_array().unwrap().iter().any(|t| t["name"] == "alpha" && t["defer_loading"] == true),
            "turn 1 must mark alpha defer_loading=true: {}",
            b1["tools"]
        );
        let b2 = reqs[1].json();
        let input2 = b2["input"].as_array().unwrap();
        assert!(
            input2.iter().any(|it| it["type"] == "tool_search_call" && it["call_id"] == "call_ds_search_1"),
            "turn 2 must replay tool_search_call: {input2:?}"
        );
        let tso = input2
            .iter()
            .find(|it| it["type"] == "tool_search_output" && it["call_id"] == "call_ds_search_1")
            .unwrap_or_else(|| panic!("turn 2 must replay tool_search_output: {input2:?}"));
        assert!(
            tso["tools"].as_array().unwrap().iter().any(|t| t["name"] == "alpha"),
            "turn 2 tool_search_output.tools must contain alpha: {tso}"
        );
    })
    .await
}
