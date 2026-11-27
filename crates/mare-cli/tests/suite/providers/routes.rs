//! Per-model routes: which layer picks a model's route, unsigned thinking
//! replayed only within the current tool loop on the same route, precise blame when a route's
//! wire plugin is off, the legacy config and the gateway's MissingSessionID on both routes. The
//! request bytes on each route are `golden_wire`.

use super::*;

async fn routes_host(env: &mut TestEnv, server: &MockServer, extra: Value) -> Arc<Host> {
    env.set_var("OPENCODE_API_KEY", "test-opencode");
    let base = &server.base;
    let mut config = json!({ "plugins": {
        "llm": { "retries": 0 },
        "provider-opencode-go": { "base_url": format!("{base}/gw") },
    } });
    merge(&mut config, extra);
    let h = provider_host(env, server, config).await;
    expose(&h);
    h
}

fn to_json(h: &Host, v: mlua::Value) -> Value {
    mare_shared::lua::lua_to_json(h.lua(), v).unwrap()
}

fn merge(dst: &mut Value, src: Value) {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            for (k, v) in s {
                merge(d.entry(k).or_insert(Value::Null), v);
            }
        }
        (d, s) if !s.is_null() => *d = s,
        _ => {}
    }
}

/// `route source` of a model, as `llm:route_of` reports it.
async fn route(h: &Host, model: &str) -> String {
    eval(
        h,
        &format!(
            r#"local r, src = LLM:route_of(assert(LLM:resolve("{model}")))
               return r .. " ← " .. src"#
        ),
    )
    .await
}

/// The five layers, highest first: `mare.model{route=}`, the plugin's `model_routes` glob (the
/// longest pattern wins), the catalogue entry, models.dev's protocol (only when turned on and only
/// onto a configured route — never the chat route, which isn't), the provider default.
#[tokio::test]
async fn a_models_route_comes_from_the_first_layer_that_names_one() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = routes_host(
            &mut env,
            &server,
            json!({
                "models": { "opencode-go/glm-5.3-flash": { "route": "responses" } },
                "plugins": { "provider-opencode-go": { "model_routes": { "glm-*": "anthropic", "glm-5.3-*": "responses", "deep*": "anthropic" } } },
            }),
        )
        .await;
        assert_eq!(route(&h, "opencode-go/glm-5.3-flash").await, "responses ← mare.model");
        assert_eq!(route(&h, "opencode-go/deepseek-v4.1-flash").await, r#"anthropic ← model_routes "deep*""#);
        assert_eq!(route(&h, "opencode-go/crouton-alpha-free").await, "responses ← provider default");
        // the longest glob wins over a shorter one
        exec(&h, r#"mare.config.models = {}"#).await;
        assert_eq!(route(&h, "opencode-go/glm-5.3-flash").await, r#"responses ← model_routes "glm-5.3-*""#);
        // a catalogue entry's route, then models.dev's protocol (off by default)
        exec(
            &h,
            r#"local p = LLM:provider("opencode-go")
               p.model_routes = {}
               local c = LLM:resolve("opencode-go/crouton-alpha-free")
               c.protocol = "messages"
               local g = LLM:resolve("opencode-go/glm-5.3-flash")
               g.protocol = "chat-completions"
               g.route = nil -- copied from the cleared mare.model override
               LLM:resolve("opencode-go/deepseek-v4.1-flash").route = "anthropic""#,
        )
        .await;
        assert_eq!(route(&h, "opencode-go/deepseek-v4.1-flash").await, "anthropic ← built-in catalogue");
        assert_eq!(route(&h, "opencode-go/crouton-alpha-free").await, "responses ← provider default");
        exec(&h, r#"LLM:provider("opencode-go").models_dev_routes = true"#).await;
        assert_eq!(route(&h, "opencode-go/crouton-alpha-free").await, "anthropic ← models.dev (messages)");
        assert_eq!(
            route(&h, "opencode-go/glm-5.3-flash").await,
            "responses ← provider default",
            "chat isn't configured: models.dev never picks it"
        );
        // a provider with one route ignores a per-model route
        exec(&h, r#"mare.config.models = { ["openai/gpt-6.1-terra"] = { route = "anthropic" } }"#).await;
        assert_eq!(route(&h, "openai/gpt-6.1-terra").await, "responses ← provider");
        // /model, /doctor: the route and the layer that chose it
        let lines: String =
            eval(&h, r#"return table.concat(LLM:explain_lines("opencode-go/crouton-alpha-free"), "\n")"#).await;
        assert!(lines.starts_with("route anthropic ← models.dev (messages)\n"), "{lines}");
        let lines: String = eval(&h, r#"return table.concat(LLM:explain_lines("openai/gpt-6.1-terra"), "\n")"#).await;
        assert!(!lines.contains("route "), "one-route providers show no route line: {lines}");
        h.shutdown().await;
    })
    .await
}

/// An Anthropic-route answer whose thinking carries no signature (the gateway returns glm's so).
fn unsigned_thinking_sse(thought: &str) -> String {
    let events = [
        json!({"type":"message_start","message":{"id":"m","role":"assistant","content":[],"usage":{"input_tokens":5,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":thought}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
        json!({"type":"message_stop"}),
    ];
    events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap())).collect()
}

/// Sends `turns` on glm (Anthropic route): the first answer (unsigned thinking + a tool call)
/// becomes history; returns the assistant `thinking` blocks the follow-up request carried.
const REPLAY: &str = r#"
local new_turn = ...
local m = assert(LLM:resolve("opencode-go/glm-5.3-flash"))
local tools = { { name = "read", description = "Read", parameters = { type = "object", properties = { path = { type = "string" } } } } }
local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
local first = LLM:stream({ model = m, system = "S", session_id = "s1", messages = msgs, tools = tools, no_fallback = true }, function() end)
assert(first.route == "anthropic", "the answer records its route")
msgs[#msgs + 1] = first
msgs[#msgs + 1] = { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = { { type = "text", text = "A" } } }
if new_turn then
  msgs[#msgs + 1] = { role = "user", content = { { type = "text", text = "next" } } }
end
LLM:stream({ model = m, system = "S", session_id = "s1", messages = msgs, tools = tools, no_fallback = true }, function() end)
"#;

fn thinking_blocks(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "assistant")
        .flat_map(|m| m["content"].as_array().unwrap().clone())
        .filter(|b| b["type"] == "thinking")
        .collect()
}

/// Unsigned thinking goes back as it came within the current tool loop on the same model and
/// route; an earlier turn's is dropped; `replay_unsigned_thinking = false` drops it (the
/// Anthropic provider's behaviour); after a route switch it is not replayed as thinking.
#[tokio::test]
async fn unsigned_thinking_is_replayed_only_within_the_current_tool_loop() {
    local(async {
        let run = |extra: Value, new_turn: bool| async move {
            let mut env = TestEnv::new().await;
            let server = MockServer::start().await;
            server.route("/gw/anthropic", (0..2).map(|_| Resp::sse(unsigned_thinking_sse("PLAN-A"))).collect());
            let mut cfg = json!({ "plugins": { "provider-opencode-go": { "model_routes": { "glm-*": "anthropic" } } } });
            merge(&mut cfg, extra);
            let h = routes_host(&mut env, &server, cfg).await;
            let f = h.lua().load(REPLAY).into_function().unwrap();
            f.call_async::<()>(new_turn).await.unwrap();
            let body = server.requests()[1].json();
            h.shutdown().await;
            body
        };
        let body = run(Value::Null, false).await;
        assert_eq!(thinking_blocks(&body), vec![json!({ "type": "thinking", "thinking": "PLAN-A", "signature": "" })]);
        let body = run(Value::Null, true).await;
        assert!(thinking_blocks(&body).is_empty(), "an earlier turn's unsigned thinking is dropped: {body}");
        let off = json!({ "plugins": { "provider-opencode-go": { "routes": { "anthropic": { "replay_unsigned_reasoning": false } } } } });
        let body = run(off, false).await;
        assert!(thinking_blocks(&body).is_empty(), "{body}");

        // the same history on the Responses route: no reasoning item, the thought as text
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/gw/anthropic", vec![Resp::sse(unsigned_thinking_sse("PLAN-A"))]);
        server.route("/gw/openai", vec![Resp::sse(responses_text("ok"))]);
        let h = routes_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "model_routes": { "glm-*": "anthropic" } } } }),
        )
        .await;
        exec(&h, r#"LLM:provider("opencode-go").model_routes = { ["glm-*"] = "anthropic" }"#).await;
        exec(
            &h,
            r#"local m = assert(LLM:resolve("opencode-go/glm-5.3-flash"))
               local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
               local first = LLM:stream({ model = m, system = "S", messages = msgs, no_fallback = true }, function() end)
               msgs[#msgs + 1] = first
               msgs[#msgs + 1] = { role = "tool_result", tool_call_id = "call_1", tool_name = "read", content = { { type = "text", text = "A" } } }
               LLM:provider("opencode-go").model_routes = {}
               LLM:stream({ model = m, system = "S", messages = msgs, no_fallback = true }, function() end)"#,
        )
        .await;
        let reqs = server.requests();
        assert!(reqs[1].path.ends_with("/gw/openai/v1/responses"), "{}", reqs[1].path);
        let body = reqs[1].json();
        let input = body["input"].as_array().unwrap();
        assert!(!input.iter().any(|i| i["type"] == "reasoning"), "{body}");
        assert!(reqs[1].body.contains("PLAN-A"), "the other route's thinking stays readable as text: {body}");
        h.shutdown().await;
    })
    .await
}

/// With provider-anthropic turned off only the Anthropic-route models are refused, and the
/// reason names the plugin; the Responses-route models still work.
#[tokio::test]
async fn a_disabled_wire_plugin_refuses_only_the_models_on_its_route() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/gw/openai", vec![Resp::sse(responses_text("ok"))]);
        let h = routes_host(
            &mut env,
            &server,
            json!({ "plugins": {
                "provider-anthropic": { "enabled": false },
                "provider-opencode-go": { "model_routes": { "glm-*": "anthropic" } },
            } }),
        )
        .await;
        let why: String = eval(
            &h,
            r#"local p = LLM:provider("opencode-go")
               return p.unavailable_for(LLM:resolve("opencode-go/glm-5.3-flash"))"#,
        )
        .await;
        assert_eq!(why, "provider opencode-go route anthropic unavailable (plugin provider-anthropic is disabled)");
        let (decision, reason): (String, String) = eval(
            &h,
            r#"LLM:set_model("opencode-go/glm-5.3-flash", { save = false })
               local v = C.cordis:get("policy"):decide("request/ready", {})
               return v.decision, tostring(v.reason)"#,
        )
        .await;
        assert_eq!(decision, "deny", "{reason}");
        assert!(reason.contains("route anthropic unavailable (plugin provider-anthropic is disabled)"), "{reason}");
        let ok: bool = eval(
            &h,
            r#"local p = LLM:provider("opencode-go")
               local c = LLM:resolve("opencode-go/crouton-alpha-free")
               assert(p.unavailable_for(c) == nil)
               local r = LLM:stream({ model = c, system = "S", no_fallback = true,
                 messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
               return r.stop_reason ~= "error""#,
        )
        .await;
        assert!(ok, "the Responses route is unaffected");
        h.shutdown().await;
    })
    .await
}

/// The Responses route's address comes from provider-opencode-go's route configuration.
#[tokio::test]
async fn configured_opencode_go_route_sets_the_address() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/gw/og", vec![Resp::sse(responses_text("ok"))]);
        let h = routes_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "routes": { "responses": { "path": "/og" } } } } }),
        )
        .await;
        exec(
            &h,
            r#"LLM:stream({ model = LLM:resolve("opencode-go/crouton-alpha-free"), system = "S", no_fallback = true,
                 messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)"#,
        )
        .await;
        assert_eq!(server.requests()[0].path, "/gw/og/responses");
        let check: mlua::Value = eval(
            &h,
            r#"local checks = {} C.cordis:get("bus"):emit("kernel/doctor", checks)
               for _, c in ipairs(checks) do if c.name == "opencode Go routes" then return c end end"#,
        )
        .await;
        let check = to_json(&h, check);
        assert_eq!(check["ok"], true, "{check}");
        assert!(check["detail"].as_str().unwrap().contains("/gw/og/responses"), "{check}");
        h.shutdown().await;
    })
    .await
}

/// The gateway's 400 MissingSessionID is one configuration error on both routes (not retried).
#[tokio::test]
async fn missing_session_id_is_one_config_error_on_both_routes() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"MissingSessionID: x-opencode-session header is required"}}"#;
        server.route("/gw/anthropic", vec![Resp::status(400, body)]);
        server.route("/gw/openai", vec![Resp::status(400, r#"{"error":{"message":"MissingSessionID"}}"#)]);
        let h = routes_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": { "model_routes": { "glm-*": "anthropic" } } } }),
        )
        .await;
        for model in ["opencode-go/glm-5.3-flash", "opencode-go/crouton-alpha-free"] {
            let r: mlua::Value = eval(
                &h,
                &format!(
                    r#"return LLM:stream({{ model = LLM:resolve("{model}"), system = "S", no_fallback = true,
                         messages = {{ {{ role = "user", content = {{ {{ type = "text", text = "hi" }} }} }} }} }}, function() end)"#
                ),
            )
            .await;
            let r = to_json(&h, r);
            assert_eq!(r["stop_reason"], "error", "{model}: {r}");
            assert_eq!(r["error_kind"], "invalid", "{model}: {r}");
            assert!(r["error"].as_str().unwrap().contains("x-opencode-session header on every request"), "{model}: {r}");
        }
        assert_eq!(server.requests().len(), 2, "not retried");
        h.shutdown().await;
    })
    .await
}
