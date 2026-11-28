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

/// Issue #191: dynamically discovered Gemini 3+ models from Google `/models` must select
/// `thinkingLevel` rather than falling back to `thinkingBudget` because `reasoning_level` was
/// omitted by `decode_google_models`.
#[tokio::test]
async fn dynamically_discovered_gemini_3_model_uses_thinking_level() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/google/models/gemini-3-flash-preview:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse"))],
        );
        server.route(
            "/google/models/gemini-3.1-pro-preview:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse")), Resp::sse(fixture("google.sse"))],
        );
        server.route("/google/models/gemini-4-carbon:streamGenerateContent", vec![Resp::sse(fixture("google.sse"))]);
        server.route(
            "/google/models/gemini-4-unknown-preview:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse"))],
        );
        server.route(
            "/google/models/gemini-4-custom-efforts:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse"))],
        );
        server.route(
            "/google/models/gemini-3-budget-override:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse"))],
        );
        server.route(
            "/google/models",
            vec![Resp::status(
                200,
                json!({
                    "models": [
                        {
                            "name": "models/gemini-3-flash-preview",
                            "displayName": "Gemini 3 Flash Preview",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "thinking": true,
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-3.1-pro-preview",
                            "displayName": "Gemini 3.1 Pro Preview",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "thinking": true,
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-4-carbon",
                            "displayName": "Gemini 4 Carbon",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "thinking": true,
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-4-unknown-preview",
                            "displayName": "Gemini 4 Unknown Preview",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "thinking": true,
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-4-custom-efforts",
                            "displayName": "Gemini 4 Custom Efforts",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "thinking": true,
                            "supportedGenerationMethods": ["generateContent"]
                        },
                        {
                            "name": "models/gemini-3-budget-override",
                            "displayName": "Gemini 3 Budget Override",
                            "inputTokenLimit": 1048576,
                            "outputTokenLimit": 65536,
                            "thinking": true,
                            "supportedGenerationMethods": ["generateContent"]
                        }
                    ]
                })
                .to_string(),
            )],
        );

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "google/gemini-3-budget-override": {
                        "reasoning": true,
                        "reasoning_level": false
                    },
                    "google/gemini-4-custom-efforts": {
                        "reasoning": true,
                        "efforts": ["minimal", "low", "medium", "high", "xhigh"]
                    }
                },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 1, "models_dev": false }
                }
            }),
        )
        .await;

        lua_call(
            &h,
            r#"
LLM:refresh_catalog({ force = true })
local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

-- 1. Real model #1: Gemini 3 Flash supports minimal..high (clamps xhigh -> high).
local g3_flash = assert(LLM:resolve("google/gemini-3-flash-preview"), "discovered gemini-3-flash-preview missing")
local msg = LLM:stream({ model = g3_flash, system = "sys", effort = "high", messages = msgs }, function() end)
assert(msg and not msg.error, tostring(msg and msg.error))
LLM:set_model("google/gemini-3-flash-preview", { save = false })
LLM:set_effort("minimal", { save = false })
assert(LLM:stream({ model = g3_flash, system = "sys", messages = msgs }, function() end).error == nil)
LLM:set_effort("xhigh", { save = false })
assert(LLM:stream({ model = g3_flash, system = "sys", messages = msgs }, function() end).error == nil)

-- 2. Real model #2: Gemini 3.1 Pro supports low/medium/high (clamps minimal -> low, xhigh -> high).
local g31_pro = assert(LLM:resolve("google/gemini-3.1-pro-preview"), "discovered gemini-3.1-pro-preview missing")
LLM:set_model("google/gemini-3.1-pro-preview", { save = false })
LLM:set_effort("minimal", { save = false })
assert(LLM:stream({ model = g31_pro, system = "sys", messages = msgs }, function() end).error == nil)
LLM:set_effort("xhigh", { save = false })
assert(LLM:stream({ model = g31_pro, system = "sys", messages = msgs }, function() end).error == nil)

-- 3. gemini-4-carbon supports low/medium/high only (must NOT advertise/send xhigh).
local g4_carbon = assert(LLM:resolve("google/gemini-4-carbon"), "discovered gemini-4-carbon missing")
LLM:set_model("google/gemini-4-carbon", { save = false })
LLM:set_effort("xhigh", { save = false })
assert(LLM:stream({ model = g4_carbon, system = "sys", messages = msgs }, function() end).error == nil)

-- 4. Unknown Gemini 4 model falls back conservatively to low/high (clamps xhigh -> high).
local g4_unk = assert(LLM:resolve("google/gemini-4-unknown-preview"), "discovered gemini-4-unknown-preview missing")
LLM:set_model("google/gemini-4-unknown-preview", { save = false })
LLM:set_effort("xhigh", { save = false })
assert(LLM:stream({ model = g4_unk, system = "sys", messages = msgs }, function() end).error == nil)

-- 5. Explicit `efforts` override in mare.model permits xhigh on a custom/future model.
local g4_custom = assert(LLM:resolve("google/gemini-4-custom-efforts"), "discovered gemini-4-custom-efforts missing")
LLM:set_model("google/gemini-4-custom-efforts", { save = false })
LLM:set_effort("xhigh", { save = false })
assert(LLM:stream({ model = g4_custom, system = "sys", messages = msgs }, function() end).error == nil)

-- 6. Explicit model override `reasoning_level = false` forces thinkingBudget even on gemini-3*.
local ov = assert(LLM:resolve("google/gemini-3-budget-override"), "discovered gemini-3-budget-override missing")
local msg_ov = LLM:stream({ model = ov, system = "sys", effort = "high", messages = msgs }, function() end)
assert(msg_ov and not msg_ov.error, tostring(msg_ov and msg_ov.error))
"#,
            json!([]),
        )
        .await;

        let reqs = server.requests();
        let g3_reqs: Vec<_> =
            reqs.iter().filter(|r| r.path.contains("gemini-3-flash-preview:streamGenerateContent")).collect();
        assert_eq!(g3_reqs.len(), 3, "expected 3 requests for gemini-3-flash-preview");
        let tc = &g3_reqs[0].json()["generationConfig"]["thinkingConfig"];
        assert!(
            tc.get("thinkingLevel").is_some() && tc.get("thinkingBudget").is_none(),
            "dynamically discovered Gemini 3 model received thinkingBudget instead of thinkingLevel: {tc}"
        );
        assert_eq!(tc["thinkingLevel"], json!("high"));
        assert_eq!(
            g3_reqs[1].json()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("minimal"),
            "Gemini 3 Flash should support minimal"
        );
        assert_eq!(
            g3_reqs[2].json()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("high"),
            "Gemini 3 Flash should clamp unsupported xhigh to high"
        );

        let g31_reqs: Vec<_> =
            reqs.iter().filter(|r| r.path.contains("gemini-3.1-pro-preview:streamGenerateContent")).collect();
        assert_eq!(g31_reqs.len(), 2, "expected 2 requests for gemini-3.1-pro-preview");
        assert_eq!(
            g31_reqs[0].json()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("low"),
            "Gemini 3.1 Pro should clamp minimal to low"
        );
        assert_eq!(
            g31_reqs[1].json()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("high"),
            "Gemini 3.1 Pro should clamp xhigh to high"
        );

        let carbon_req = reqs
            .iter()
            .find(|r| r.path.contains("gemini-4-carbon:streamGenerateContent"))
            .expect("generateContent request for gemini-4-carbon");
        assert_eq!(
            carbon_req.json()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("high"),
            "gemini-4-carbon must clamp xhigh to high instead of sending unsupported xhigh"
        );

        let unk_req = reqs
            .iter()
            .find(|r| r.path.contains("gemini-4-unknown-preview:streamGenerateContent"))
            .expect("generateContent request for gemini-4-unknown-preview");
        assert_eq!(
            unk_req.json()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("high"),
            "unknown Gemini 4 model must fall back conservatively and clamp xhigh to high"
        );

        let custom_req = reqs
            .iter()
            .find(|r| r.path.contains("gemini-4-custom-efforts:streamGenerateContent"))
            .expect("generateContent request for gemini-4-custom-efforts");
        assert_eq!(
            custom_req.json()["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            json!("xhigh"),
            "explicit efforts override must allow xhigh"
        );

        let ov_req = reqs
            .iter()
            .find(|r| r.path.contains("gemini-3-budget-override:streamGenerateContent"))
            .expect("generateContent request for gemini-3-budget-override");
        let ov_tc = &ov_req.json()["generationConfig"]["thinkingConfig"];
        assert!(
            ov_tc.get("thinkingBudget").is_some() && ov_tc.get("thinkingLevel").is_none(),
            "explicit reasoning_level=false override must force thinkingBudget: {ov_tc}"
        );
        h.shutdown().await;
    })
    .await;
}
