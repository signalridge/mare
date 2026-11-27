//! Provider thinking switches, Anthropic display/beta headers, and Messages `disabled` retry.
//! References:
//! - pi packages/ai/src/api/openai-completions.ts:869-976, 1606-1700
//! - pi packages/ai/src/api/anthropic-messages.ts:1393-1401, 1548-1573

use super::*;

async fn lua_call(h: &Host, code: &str, args: Value) -> Value {
    let f = h.lua().load(code).into_function().unwrap();
    let mut mv = mlua::MultiValue::new();
    if let Value::Array(items) = args {
        for item in items {
            mv.push_back(mare_shared::lua::json_to_lua(h.lua(), &item).unwrap());
        }
    }
    let v: mlua::Value = f.call_async(mv).await.unwrap();
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

#[tokio::test]
async fn openai_compat_thinking_formats_and_budget_fields() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let sse = Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]));
        server.route("/compat", (0..20).map(|_| sse.clone()).collect());
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "zai": {
                        "base_url": format!("{}/compat", server.base),
                        "dialect": "zai",
                        "api_key": "k",
                        "models": [{ "model_id": "glm-5", "reasoning": true, "efforts": ["off", "low", "high"] }]
                    },
                    "qwen_ep": {
                        "base_url": format!("{}/compat", server.base),
                        "api_key": "k",
                        "reasoning_format": "qwen",
                        "reasoning_budget_field": "thinking_token_budget",
                        "models": [{ "model_id": "qwen3-thinking", "reasoning": true, "efforts": ["off", "low", "high"] }]
                    },
                    "qwen_tpl": {
                        "base_url": format!("{}/compat", server.base),
                        "api_key": "k",
                        "reasoning_format": "qwen-chat-template",
                        "models": [{ "model_id": "qwen3-vllm", "reasoning": true, "efforts": ["off", "high"] }]
                    },
                    "together": {
                        "base_url": format!("{}/compat", server.base),
                        "dialect": "together",
                        "api_key": "k",
                        "models": [{ "model_id": "r1", "reasoning": true, "efforts": ["off", "high"] }]
                    }
                }
            }),
        )
        .await;

        let probe = r#"
local model_ref, effort = ...
local m = assert(LLM:resolve(model_ref), "unknown model " .. model_ref)
local msg = LLM:stream({
    model = m,
    system = "sys",
    effort = effort,
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
}, function() end)
assert(msg and not msg.error, tostring(msg and msg.error))
"#;

        // 1. DeepSeek reasoning model: enabled on high, disabled on off; non-reasoning chat sends none.
        lua_call(&h, probe, json!(["deepseek/deepseek-reasoner", "high"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["thinking"], json!({ "type": "enabled" }));

        lua_call(&h, probe, json!(["deepseek/deepseek-reasoner", "off"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["thinking"], json!({ "type": "disabled" }));

        lua_call(&h, probe, json!(["deepseek/deepseek-chat", "high"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert!(b.get("thinking").is_none());

        // 2. z.ai: enabled + clear_thinking=false on, disabled off
        lua_call(&h, probe, json!(["zai/glm-5", "high"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["thinking"], json!({ "type": "enabled", "clear_thinking": false }));

        lua_call(&h, probe, json!(["zai/glm-5", "off"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["thinking"], json!({ "type": "disabled" }));

        // 3. Qwen + thinking_budget_field
        lua_call(&h, probe, json!(["qwen_ep/qwen3-thinking", "low"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["enable_thinking"], json!(true));
        assert_eq!(b["thinking_token_budget"], json!(2048));

        lua_call(&h, probe, json!(["qwen_ep/qwen3-thinking", "off"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["enable_thinking"], json!(false));
        assert!(b.get("thinking_token_budget").is_none());

        // 4. Qwen chat-template
        lua_call(&h, probe, json!(["qwen_tpl/qwen3-vllm", "high"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(
            b["chat_template_kwargs"],
            json!({ "enable_thinking": true, "preserve_thinking": true })
        );

        lua_call(&h, probe, json!(["qwen_tpl/qwen3-vllm", "off"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(
            b["chat_template_kwargs"],
            json!({ "enable_thinking": false, "preserve_thinking": true })
        );

        // 5. Together
        lua_call(&h, probe, json!(["together/r1", "high"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["reasoning"], json!({ "enabled": true }));

        lua_call(&h, probe, json!(["together/r1", "off"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["reasoning"], json!({ "enabled": false }));

        // 6. Session-level effort selection (LLM:set_model + LLM:set_effort):
        //    - deepseek-reasoner only declares efforts={"high"} (always-on thinking per DeepSeek docs),
        //      so session effort="off" clamps to "high" -> thinking.type="enabled".
        //    - zai/glm-5 declares "off", so session effort="off" sends thinking.type="disabled".
        let session_probe = r#"
local model_ref, effort = ...
LLM:set_model(model_ref, { save = false })
LLM:set_effort(effort, { save = false })
local m = assert(LLM:resolve(model_ref))
local msg = LLM:stream({
    model = m,
    system = "sys",
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
}, function() end)
assert(msg and not msg.error, tostring(msg and msg.error))
"#;
        lua_call(&h, session_probe, json!(["deepseek/deepseek-reasoner", "off"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["thinking"], json!({ "type": "enabled" }));

        lua_call(&h, session_probe, json!(["zai/glm-5", "off"])).await;
        let b: Value = serde_json::from_str(&server.requests().pop().unwrap().body).unwrap();
        assert_eq!(b["thinking"], json!({ "type": "disabled" }));

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn anthropic_display_betas_and_disabled_retry() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::status(
                    400,
                    r#"{"error":{"type":"invalid_request_error","message":"\"thinking.type.disabled\" is not supported for this model."}}"#,
                ),
                Resp::sse(fixture("anthropic-text.sse")),
                Resp::status(
                    400,
                    r#"{"error":{"type":"invalid_request_error","message":"send \"thinking\": {\"type\": \"between_tools\"} instead of {\"type\": \"disabled\"}"}}"#,
                ),
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let probe = r#"
local model_ref, effort, with_tools = ...
local m = assert(LLM:resolve(model_ref))
local tools = with_tools and {
    { name = "echo", description = "echo", parameters = { type = "object", properties = { s = { type = "string" } } } }
} or nil
local msg = LLM:stream({
    model = m,
    system = "sys",
    effort = effort,
    tools = tools,
    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
}, function() end)
assert(msg and not msg.error, tostring(msg and msg.error))
"#;

        // 1. Adaptive 5.x model: thinking.display = "summarized"
        lua_call(&h, probe, json!(["anthropic/claude-sonnet-5-5", "high", false])).await;
        let req = server.requests().pop().unwrap();
        let body = req.json();
        assert_eq!(body["thinking"], json!({ "type": "adaptive", "display": "summarized" }));
        assert_eq!(body["output_config"]["effort"], json!("high"));

        // 2. Non-adaptive 4.5 model with thinking + tools: display + interleaved + fine-grained betas
        lua_call(&h, probe, json!(["anthropic/claude-sonnet-4-5", "high", true])).await;
        let req = server.requests().pop().unwrap();
        let body = req.json();
        assert_eq!(body["thinking"]["type"], json!("enabled"));
        assert_eq!(body["thinking"]["display"], json!("summarized"));
        let beta = req.header("anthropic-beta").unwrap_or("");
        assert!(beta.contains("interleaved-thinking-2025-05-14"), "beta: {beta}");
        assert!(beta.contains("fine-grained-tool-streaming-2025-05-14"), "beta: {beta}");

        // 3. Non-adaptive with effort="off" + tools: fine-grained beta only, no interleaved beta
        lua_call(&h, probe, json!(["anthropic/claude-sonnet-4-5", "off", true])).await;
        let req = server.requests().pop().unwrap();
        let beta = req.header("anthropic-beta").unwrap_or("");
        assert!(!beta.contains("interleaved-thinking-2025-05-14"), "beta: {beta}");
        assert!(beta.contains("fine-grained-tool-streaming-2025-05-14"), "beta: {beta}");

        // 4. Compatible Messages endpoint rejecting thinking={type="disabled"} retries cleanly
        expose(&h);
        exec(
            &h,
            r#"
            C.cordis:on("provider/request", function(wire, next)
                wire.body.thinking = { type = "disabled" }
                return next()
            end)
            "#,
        )
        .await;
        lua_call(&h, probe, json!(["anthropic/claude-sonnet-5-5", "off", false])).await;
        let reqs = server.requests();
        let retried_strip = reqs[reqs.len() - 1].json();
        assert!(retried_strip.get("thinking").is_none(), "stripped retry body: {retried_strip}");

        lua_call(&h, probe, json!(["anthropic/claude-sonnet-5-5", "off", false])).await;
        let reqs = server.requests();
        let retried_between = reqs[reqs.len() - 1].json();
        assert_eq!(retried_between["thinking"], json!({ "type": "between_tools" }));

        h.shutdown().await;
    })
    .await;
}
