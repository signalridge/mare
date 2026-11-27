//! The model-parameters engine: rules merged by layer, options mapped to
//! wire fields per route (field tables and functions), the unsupported-option policies, the
//! source of every value, persistence and the custom-model shape. The built-in data's
//! byte-identity is `golden_wire`.

use super::*;

async fn params_host(env: &mut TestEnv, server: &MockServer, extra: Value) -> Arc<Host> {
    env.set_var("OPENCODE_API_KEY", "test-opencode");
    let base = &server.base;
    let ok = || (0..50).map(|_| Resp::sse(responses_text("ok"))).collect::<Vec<_>>();
    server.route("/openai", ok());
    server.route("/og", ok());
    server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
    server.route("/localre", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
    let mut config = json!({
        "providers": { "localre": { "base_url": format!("{base}/localre"), "no_key": true,
            "models": [{ "model_id": "qwen3", "context_window": 32000, "max_output": 8000 }] } },
        "plugins": {
            "llm": { "retries": 0 },
            "provider-opencode-go": { "base_url": base, "routes": { "responses": { "path": "/og" } } },
        },
    });
    if let (Value::Object(c), Value::Object(e)) = (&mut config, extra) {
        c.extend(e);
    }
    let h = provider_host(env, server, config).await;
    exec(&h, "LLM_ = LLM").await;
    h
}

/// One request with the current session parameters; the body sent.
async fn send(h: &Host, server: &MockServer, model: &str) -> Value {
    exec(
        h,
        &format!(
            r#"local m = assert(LLM:resolve("{model}"))
               local r = LLM:stream({{ model = m, system = "S", no_fallback = true,
                 messages = {{ {{ role = "user", content = {{ {{ type = "text", text = "hi" }} }} }} }} }}, function() end)
               LAST = r"#
        ),
    )
    .await;
    server.requests().last().expect("a request").json()
}

/// An agent request over an actual built-in wire, with a captured compaction budget and wire body.
async fn agent_cap(env: &mut TestEnv, model: &str, prefix: &str) -> (Value, i64, i64, i64, i64) {
    let server = MockServer::start().await;
    server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
    server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
    server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
    server.route(
        "/google",
        vec![Resp::sse(format!(
            "data: {}\n\n",
            json!({
                "candidates": [{ "content": { "parts": [{ "text": "ok" }] }, "finishReason": "STOP" }]
            })
        ))],
    );
    let h = provider_host(
        env,
        &server,
        json!({ "models": { (model): { "context_window": 60000, "max_output": 8000 } },
            "plugins": { "llm": { "retries": 0 }, "compact": { "threshold_ratio": 1, "headroom_tokens": 0 } }
        }),
    )
    .await;
    expose(&h);
    global(&h, "MODEL_REF", model);
    exec(
        &h,
        r#"
        LLM:set_model(MODEL_REF, { save = false })
        BUILDS, MAPS, WIRES, THRESHOLD = 0, 0, 0, nil
        local apply = LLM.apply_params
        LLM.apply_params = function(self, ...) MAPS = MAPS + 1 return apply(self, ...) end
        C.cordis:on("request/build", function(req, next)
            BUILDS = BUILDS + 1
            req.max_tokens, req.effort = 4096, "high"
            req.system = string.rep("x", 240000)
            return next()
        end)
        C.cordis:on("compact/before", function(ev) THRESHOLD = ev.threshold return true end)
        C.cordis:on("provider/request", function(wire, next) WIRES = WIRES + 1 return next() end)
    "#,
    )
    .await;
    let result = h.prompt("hi", json!({})).await.unwrap();
    assert!(result["error"].is_null(), "{model}: {result}");
    let (reserved, builds, maps, wires): (i64, i64, i64, i64) =
        eval(&h, "return 60000 - THRESHOLD, BUILDS, MAPS, WIRES").await;
    let body = server.requests().iter().find(|r| r.path.starts_with(prefix)).expect("model request").json();
    h.shutdown().await;
    (body, reserved, builds, maps, wires)
}

/// CMP1a reserves the actual output cap after the built-in Anthropic thinking mapping.
#[tokio::test]
async fn anthropic_explicit_output_cap_matches_compaction_reserve() {
    local(async {
        let mut env = TestEnv::new().await;
        let (body, reserved, builds, maps, wires) =
            agent_cap(&mut env, "anthropic/claude-sonnet-4-5", "/anthropic").await;
        assert_eq!((builds, maps, wires), (1, 1, 1));
        assert_eq!(body["max_tokens"].as_i64(), Some(reserved), "wire cap must agree with the prepared-request budget");
        assert_eq!(reserved, 8000);
        assert!(body["thinking"]["budget_tokens"].as_i64().unwrap() < reserved);
    })
    .await
}

#[tokio::test]
async fn prepared_output_caps_match_openai_chat_responses_and_google() {
    local(async {
        let mut env = TestEnv::new().await;
        for (model, prefix, key) in [
            ("deepseek/deepseek-chat", "/compat", "chat"),
            ("openai/gpt-6.1-terra", "/openai", "responses"),
            ("google/gemini-2.5-pro", "/google", "google"),
        ] {
            let (body, reserved, builds, maps, wires) = agent_cap(&mut env, model, prefix).await;
            assert_eq!((builds, maps, wires), (1, 1, 1), "{model}");
            let sent = match key {
                "chat" => &body["max_tokens"],
                "responses" => &body["max_output_tokens"],
                _ => &body["generationConfig"]["maxOutputTokens"],
            };
            assert_eq!(sent.as_i64(), Some(reserved), "{model}: {body}");
            assert_eq!(reserved, 4096, "{model}");
            if key != "chat" {
                assert!(body.get("max_tokens").is_none(), "{model} must not receive the overlay seed");
            }
        }
    })
    .await
}

/// Declarative parameter removals, lower bounds and nested fields retain their wire semantics
/// when they are resolved before the encoder has constructed the request body.
#[tokio::test]
async fn prepared_overlay_preserves_parameter_removals_and_lower_bounds() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "models": {
            "openai/gpt-6.1-terra": { "context_window": 60000, "max_output": 8000 }
        }, "plugins": { "compact": { "auto": false, "headroom_tokens": 0 } } }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
            mare.config.param_rules = { { match = "openai/gpt-6.1-terra", define = { effort = {
                only = { "deep" }, options = { { value = "deep", wire = { responses = {
                    ["parallel_tool_calls"] = false, ["max_output_tokens"] = { at_least = 20000 },
                    ["text.verbosity"] = "low" } } } } } } } }
            LLM:set_model("openai/gpt-6.1-terra", { save = false })
            BUILDS, MAPS, WIRES = 0, 0, 0
            local apply = LLM.apply_params
            LLM.apply_params = function(self, ...) MAPS = MAPS + 1 return apply(self, ...) end
            C.cordis:on("request/build", function(req, next)
                BUILDS = BUILDS + 1
                req.effort = "deep"
                req.max_tokens = 4096
                req.output_schema = { type = "object", properties = {}, additionalProperties = false }
                return next()
            end)
            C.cordis:on("provider/request", function(wire, next) WIRES = WIRES + 1 return next() end)
        "#,
        )
        .await;
        let result = h.prompt("hi", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let body = server.requests()[0].json();
        assert!(body.get("parallel_tool_calls").is_none(), "{body}");
        assert_eq!(body["max_output_tokens"], 20000);
        assert_eq!(body["text"]["verbosity"], "low");
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        assert!(body.get("max_tokens").is_none());
        assert_eq!(eval::<(i64, i64, i64)>(&h, "return BUILDS, MAPS, WIRES").await, (1, 1, 1));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn prepared_lower_bound_never_reduces_the_encoded_cap() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok")); 2]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "models": {
            "openai/gpt-6.1-terra": { "context_window": 60000, "max_output": 8000 }
        }, "plugins": { "compact": { "auto": false, "headroom_tokens": 0 } } }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
            mare.config.param_rules = { { match = "openai/gpt-6.1-terra", define = { effort = {
                only = { "deep" }, options = { { value = "deep", wire = { responses = {
                    ["max_output_tokens"] = { at_least = 4000 } } } } } } } } }
            LLM:set_model("openai/gpt-6.1-terra", { save = false })
            BUILDS = 0
            C.cordis:on("request/build", function(req, next)
                BUILDS = BUILDS + 1
                req.effort = "deep"
                if BUILDS == 2 then req.max_tokens = 5000 end
                return next()
            end)
        "#,
        )
        .await;
        let result = h.prompt("hi", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let body = server.requests()[0].json();
        assert_eq!(body["max_output_tokens"], 8000, "at_least must not lower the encoder's cap");
        let result = h.prompt("again", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let body = server.requests()[1].json();
        assert_eq!(body["max_output_tokens"], 5000, "at_least must not lower an explicit cap");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn prepared_lower_bound_is_sent_when_the_encoder_omits_the_cap() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()])); 3]);
        let h = provider_host(
            &mut env,
            &server,
            json!({ "models": {
            "deepseek/deepseek-reasoner": { "context_window": 60000, "max_output": 8000 }
        }, "plugins": { "compact": { "auto": false, "headroom_tokens": 0 } } }),
        )
        .await;
        expose(&h);
        exec(
            &h,
            r#"
            mare.config.param_rules = { { match = "deepseek/deepseek-reasoner", define = { effort = {
                only = { "deep" }, options = { { value = "deep", wire = { chat = {
                    ["max_tokens"] = { at_least = 4000 } } } } } } } } }
            LLM:set_model("deepseek/deepseek-reasoner", { save = false })
            C.cordis:on("request/build", function(req, next) req.effort = "deep" return next() end)
        "#,
        )
        .await;
        let result = h.prompt("hi", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let body = server.requests()[0].json();
        assert_eq!(body["max_tokens"], 4000, "a real lower-bound mapping is not the removable seed");
        exec(
            &h,
            r#"
            mare.config.param_rules = { { match = "deepseek/deepseek-reasoner", define = { effort = {
                only = { "deep" }, options = { { value = "deep", wire = { chat = { ["max_tokens"] = 8000 } } } } } } } }
        "#,
        )
        .await;
        let result = h.prompt("again", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let body = server.requests()[1].json();
        assert_eq!(body["max_tokens"], 8000, "an explicit mapping equal to the seed is still a real write");
        exec(
            &h,
            r#"
            mare.config.param_rules = { { match = "deepseek/deepseek-reasoner", define = { effort = {
                only = { "deep" }, options = { { value = "deep", wire = { chat = function(req)
                    req.body.max_tokens = req.body.max_tokens
                end } } } } } } }
        "#,
        )
        .await;
        let result = h.prompt("function mapping", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let body = server.requests()[2].json();
        assert_eq!(body["max_tokens"], 8000, "a function mapping explicitly writing the seed must reach the wire");
        h.shutdown().await;
    })
    .await
}

/// A provider's reply-room retry reuses the mapping but must still lower the transmitted cap.
#[tokio::test]
async fn prepared_overlay_retry_honors_the_server_reply_room() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let message = "This model's maximum context length is 60000 tokens. However, you requested 70000 tokens (50000 in the messages, 20000 in the completion).";
        server.route("/openai", vec![
            Resp::status(400, json!({ "error": { "message": message, "code": "invalid_request_error" } }).to_string()),
            Resp::sse(responses_text("ok")),
        ]);
        let h = provider_host(&mut env, &server, json!({ "models": {
            "openai/gpt-6.1-terra": { "context_window": 60000, "max_output": 8000 }
        }, "plugins": { "llm": { "retries": 1, "retry_base_ms": 1 },
                         "compact": { "auto": false, "headroom_tokens": 0 } } })).await;
        expose(&h);
        exec(&h, r#"
            mare.config.param_rules = { { match = "openai/gpt-6.1-terra", define = { effort = {
                only = { "deep" }, options = { { value = "deep", wire = { responses = {
                    ["max_output_tokens"] = { at_least = 20000 } } } } } } } } }
            LLM:set_model("openai/gpt-6.1-terra", { save = false })
            BUILDS, MAPS = 0, 0
            local apply = LLM.apply_params
            LLM.apply_params = function(self, ...) MAPS = MAPS + 1 return apply(self, ...) end
            C.cordis:on("request/build", function(req, next)
                BUILDS = BUILDS + 1 req.effort = "deep" req.max_tokens = 4096 return next()
            end)
        "#).await;
        let result = h.prompt("hi", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].json()["max_output_tokens"], 20000);
        assert_eq!(requests[1].json()["max_output_tokens"], 10000);
        assert_eq!(eval::<(i64, i64)>(&h, "return BUILDS, MAPS").await, (1, 1));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn direct_responses_mapping_keeps_its_pre_finalize_view() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"
            mare.config.param_rules = { { match = "openai/gpt-6.1-terra", define = { effort = {
                only = { "deep" }, options = { { value = "deep", wire = { responses = function(wire)
                    wire.body.pre_finalize_cap = wire.body.max_output_tokens == nil and "absent" or "present"
                end } } } } } } }
            local model = assert(LLM:resolve("openai/gpt-6.1-terra"))
            LLM:stream({ model = model, effort = "deep", max_tokens = 4096,
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
        "#,
        )
        .await;
        let body = server.requests()[0].json();
        assert_eq!(body["pre_finalize_cap"], "absent");
        assert_eq!(body["max_output_tokens"], 4096);
        h.shutdown().await;
    })
    .await
}

/// A custom option on one model: a function mapping sets the budget and max_tokens together on
/// the Anthropic route; a field table writes, removes (`false`) and raises (`at_least`) fields on
/// the Responses route. Both appear among the model's /effort levels.
#[tokio::test]
async fn custom_options_map_to_fields_and_functions_per_route() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"mare.config.param_rules = {
              { match = "anthropic/claude-sonnet-4-5", define = { effort = {
                  only = { "off", "low", "medium", "high", "deep" },
                  options = { { value = "deep", name = "Deep (48K budget)", cost = true, wire = { anthropic = function(req, c)
                    req.body.thinking = { type = "enabled", budget_tokens = 48000 }
                    req.body.max_tokens = math.max(req.body.max_tokens or 0, 48000 + 9000)
                  end } } } } } },
              { match = "openai/gpt-6.1-terra", define = { effort = {
                  only = { "low", "high", "deep" },
                  options = { { value = "deep", wire = { responses = {
                    ["reasoning.effort"] = "xhigh", ["parallel_tool_calls"] = false, ["max_output_tokens"] = { at_least = 200000 } } } } } } } },
            }"#,
        )
        .await;
        let levels: String =
            eval(&h, r#"return table.concat(LLM:param_list(LLM:resolve("anthropic/claude-sonnet-4-5"))[1].list, " ")"#)
                .await;
        assert_eq!(levels, "off low medium high deep");
        exec(&h, r#"LLM:set_model("anthropic/claude-sonnet-4-5", { save = false }) LLM:set_effort("deep", { save = false })"#).await;
        let body = send(&h, &server, "anthropic/claude-sonnet-4-5").await;
        assert_eq!(body["thinking"], json!({ "type": "enabled", "budget_tokens": 48000 }), "{body}");
        assert_eq!(body["max_tokens"], 64000, "max(64000, 57000)");
        exec(&h, r#"LLM:set_model("openai/gpt-6.1-terra", { save = false }) LLM:set_effort("deep", { save = false })"#).await;
        let body = send(&h, &server, "openai/gpt-6.1-terra").await;
        assert_eq!(body["reasoning"]["effort"], "xhigh", "{body}");
        assert!(body.get("parallel_tool_calls").is_none(), "removed by `false`: {body}");
        assert_eq!(body["max_output_tokens"], 200000, "at_least raised the default cap (128000)");
        // another Anthropic model doesn't define deep: a level it doesn't know clamps to the
        // middle (medium), and the adjustment is recorded for /profile
        exec(&h, r#"LLM:set_model("anthropic/claude-sonnet-5-5", { save = false }) LLM:set_effort("deep", { save = false })"#).await;
        let body = send(&h, &server, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(body["output_config"]["effort"], "medium", "{body}");
        let log: String = eval(
            &h,
            r#"local out = {} for _, r in ipairs(LLM:params_log()) do out[#out + 1] = r.model .. " " .. r.from .. ">" .. r.to end return table.concat(out, ";")"#,
        )
        .await;
        assert!(log.contains("anthropic/claude-sonnet-5-5 deep>medium"), "{log}");
        h.shutdown().await;
    })
    .await
}

/// `remove` drops options; on_unsupported: error refuses the request with the reason, as_is
/// sends the value through a `$value` mapping and falls back to clamping (noted) without one.
#[tokio::test]
async fn unsupported_options_clamp_error_or_pass_as_is() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"mare.config.param_rules = {
              { match = "opencode-go/glm-*", remove = { effort = { "high" } }, on_unsupported = "error" },
            }
            LLM:set_model("opencode-go/glm-5.3-flash", { save = false })
            LLM:set_effort("xhigh", { save = false })"#,
        )
        .await;
        let levels: String =
            eval(&h, r#"return table.concat(LLM:param_list(LLM:resolve("opencode-go/glm-5.3-flash"))[1].list, " ")"#)
                .await;
        assert_eq!(levels, "low medium", "high removed");
        let before = server.requests().len();
        exec(
            &h,
            r#"local m = assert(LLM:resolve("opencode-go/glm-5.3-flash"))
               LAST = LLM:stream({ model = m, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)"#,
        )
        .await;
        assert_eq!(server.requests().len(), before, "not sent");
        let err: String = eval(&h, "return LAST.error").await;
        assert!(
            err.contains("not sent: effort xhigh is not offered by opencode-go/glm-5.3-flash (route responses; options: low medium)"),
            "{err}"
        );
        // as_is: the Responses route passes "$value" through, so xhigh goes out as it is
        exec(&h, r#"mare.config.param_rules = { { match = "opencode-go/glm-*", on_unsupported = "as_is" } }"#).await;
        let body = send(&h, &server, "opencode-go/glm-5.3-flash").await;
        assert_eq!(body["reasoning"]["effort"], "xhigh", "{body}");
        // ... the Anthropic adaptive route has no "$value" mapping: clamped, and noted
        exec(&h, r#"mare.config.param_rules = { { route = "anthropic", on_unsupported = "as_is" } } LLM:set_model("anthropic/claude-sonnet-5-5", { save = false }) LLM:set_effort("max", { save = false })"#)
            .await;
        let body = send(&h, &server, "anthropic/claude-sonnet-5-5").await;
        assert_eq!(body["output_config"]["effort"], "max", "max clamped to xhigh (sent as max): {body}");
        let kinds: String = eval(
            &h,
            r#"local out = {} for _, r in ipairs(LLM:params_log()) do out[#out + 1] = r.kind end return table.concat(out, " ")"#,
        )
        .await;
        assert!(kinds.contains("as_is_clamped"), "{kinds}");
        // a rule naming an unknown route is refused with its reason (doctor lists it)
        exec(&h, r#"mare.config.param_rules = { { route = "telnet", define = {} } }"#).await;
        let probs: String = eval(&h, r#"return table.concat(LLM:explain_params("openai/gpt-6.1-terra").problems, ";")"#).await;
        assert!(probs.contains("unknown route \"telnet\""), "{probs}");
        h.shutdown().await;
    })
    .await
}

/// Values by layer: model default < state.json (model_params) < user init.lua < trusted project
/// < session < request; explain names the layer and the exact wire fields.
#[tokio::test]
async fn values_follow_the_layers_and_explain_names_their_source() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({ "model": "openai/gpt-6.1-terra" })).await;
        let verbosity = |h: &Host| {
            let f = h
                .lua()
                .load(
                    r#"local R = LLM:resolve_params(LLM:resolve("openai/gpt-6.1-terra"))
                       return R.params.verbosity .. " " .. R.sources.verbosity.layer"#,
                )
                .into_function()
                .unwrap();
            async move { f.call_async::<String>(()).await.unwrap() }
        };
        assert_eq!(verbosity(&h).await, "low model", "the model's metadata");
        exec(&h, r#"LLM:remember_param("openai/gpt-6.1-terra", "verbosity", "high")"#).await;
        assert_eq!(verbosity(&h).await, "high user", "state.json");
        let saved: String = eval(&h, r#"return mare.state.read().model_params["openai/gpt-6.1-terra"].verbosity"#).await;
        assert_eq!(saved, "high");
        exec(&h, r#"mare.config.param_rules = { { provider = "openai", define = { verbosity = { default = "medium" } }, _layer = "user" } }"#)
            .await;
        assert_eq!(verbosity(&h).await, "medium user", "init.lua wins over state.json");
        exec(
            &h,
            r#"table.insert(mare.config.param_rules, { provider = "openai", define = { verbosity = { default = "low" } }, _layer = "project" })
               mare.config.param_rules = { table.unpack(mare.config.param_rules) }"#,
        )
        .await;
        assert_eq!(verbosity(&h).await, "low project", "a trusted project wins over the user");
        let used: String = eval(&h, r#"return LLM:set_param("verbosity", "high", { save = false })"#).await;
        assert_eq!(used, "high");
        assert_eq!(verbosity(&h).await, "high session");
        let body = send(&h, &server, "openai/gpt-6.1-terra").await;
        assert_eq!(body["text"]["verbosity"], "high", "{body}");
        // a request's own value wins over the session
        exec(
            &h,
            r#"local m = LLM:resolve("openai/gpt-6.1-terra")
               LLM:stream({ model = m, params = { verbosity = "medium" }, no_fallback = true,
                 messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)"#,
        )
        .await;
        assert_eq!(server.requests().last().unwrap().json()["text"]["verbosity"], "medium");
        // explain: value, source and what goes on the wire
        exec(&h, r#"LLM:set_effort("high", { save = false })"#).await;
        let lines: String = eval(&h, r#"return table.concat(LLM:explain_lines("openai/gpt-6.1-terra"), "\n")"#).await;
        assert!(lines.contains("effort high ← session"), "{lines}");
        assert!(lines.contains(r#"sends reasoning.effort="high""#), "{lines}");
        assert!(lines.contains(r#"sends text.verbosity="high""#), "{lines}");
        assert!(lines.contains("options off low medium high xhigh"), "{lines}");
        // set_param("effort") stores the same per-model record as other parameters.
        exec(&h, r#"LLM:set_param("effort", "low")"#).await;
        let a: String = eval(&h, r#"return mare.state.read().model_params["openai/gpt-6.1-terra"].effort"#).await;
        assert_eq!(a, "low");
        h.shutdown().await;
    })
    .await
}

/// A custom model declares a bool parameter in config (no effort): its chat-template field goes
/// on the wire, /effort explains the model has none, and only cycleable parameters cycle.
#[tokio::test]
async fn a_custom_model_declares_its_own_bool_parameter() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let models = json!({ "localre/qwen3": { "params": { "thinking": { "type": "bool", "default": true, "cycleable": true,
            "wire": { "chat": {
                "true": { "chat_template_kwargs.enable_thinking": true },
                "false": { "chat_template_kwargs.enable_thinking": { "value": false } } } } } } } });
        let h = params_host(&mut env, &server, json!({ "models": models, "model": "localre/qwen3" })).await;
        let body = send(&h, &server, "localre/qwen3").await;
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], true, "{body}");
        let v: bool = eval(&h, r#"return LLM:cycle_param("thinking", 1)"#).await;
        assert!(!v);
        let body = send(&h, &server, "localre/qwen3").await;
        assert_eq!(body["chat_template_kwargs"]["enable_thinking"], false, "{body}");
        let (ok, err): (bool, String) = eval(&h, r#"local v, e = LLM:set_param("effort", "high") return v ~= nil, e"#).await;
        assert!(!ok && err.contains("has no effort parameter; its parameters: thinking"), "{err}");
        let err: String = eval(&h, r#"local _, e = LLM:cycle_param("verbosity", 1) return e"#).await;
        assert!(err.contains("has no verbosity parameter"), "{err}");
        h.shutdown().await;
    })
    .await
}

/// Parameter functions cannot observe credentials through a retained headers reference.
#[tokio::test]
async fn parameter_mapping_retained_headers_do_not_gain_provider_credentials() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok"))]);
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        server.route("/compat", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);
        server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;
        exec(&h, r#"SEEN, SAVED = {}, {}
          local function check(req, c)
            SAVED[c.model.ref] = req.headers
            for k in pairs(req.headers) do
              local n = string.lower(k)
              if n == "authorization" or n == "x-api-key" or n == "x-goog-api-key" then
                SEEN[c.model.ref] = true
              end
            end
            SEEN[c.model.ref] = SEEN[c.model.ref] or false
          end
          mare.config.param_rules = {
            { _layer = "project", route = "responses", define = { probe = { type = "bool", default = true, wire = { responses = check } } } },
            { _layer = "project", route = "anthropic", define = { probe = { type = "bool", default = true, wire = { anthropic = check } } } },
            { _layer = "project", route = "chat", define = { probe = { type = "bool", default = true, wire = { chat = check } } } },
            { _layer = "project", route = "google", define = { probe = { type = "bool", default = true, wire = { google = check } } } },
          }"#).await;
        for (model, header, expected) in [
            ("openai/gpt-6.1-terra", "authorization", "Bearer test-openai"),
            ("anthropic/claude-sonnet-5-5", "x-api-key", "test-anthropic"),
            ("deepseek/deepseek-reasoner", "authorization", "Bearer test-deepseek"),
            ("google/gemini-2.5-pro", "x-goog-api-key", "test-google"),
        ] {
            send(&h, &server, model).await;
            let seen: Option<bool> = eval(&h, &format!("return SEEN[{model:?}]")).await;
            assert_eq!(seen, Some(false), "mapping must run without credentials for {model}");
            let retained_has_auth: bool = eval(&h, &format!(r#"
              for k in pairs(SAVED[{model:?}]) do
                local n = string.lower(k)
                if n == "authorization" or n == "x-api-key" or n == "x-goog-api-key" then return true end
              end
              return false
            "#)).await;
            assert!(!retained_has_auth, "retained mapping headers gained credentials for {model}");
            assert_eq!(server.requests().pop().unwrap().header(header), Some(expected), "{model}");
        }
        h.shutdown().await;
    }).await
}

/// A project-scope function mapping may change the body but not the credentials headers.
#[tokio::test]
async fn a_project_function_mapping_cannot_write_credential_headers() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"local function fn(req, c)
                 req.headers.authorization = "Bearer stolen"
                 req.headers["X-Api-Key"] = "stolen"
                 req.headers["X-Goog-Api-Key"] = "stolen"
                 req.headers["x-extra"] = "ok"
                 req.body.reasoning = { effort = c.value .. "!" }
               end
               mare.config.param_rules = {
                 { _layer = "project", route = "responses", define = { effort = { wire = { responses = fn } } } },
               }
               LLM:set_model("opencode-go/glm-5.3-flash", { save = false })
               LLM:set_effort("high", { save = false })"#,
        )
        .await;
        let body = send(&h, &server, "opencode-go/glm-5.3-flash").await;
        assert_eq!(body["reasoning"]["effort"], "high!", "{body}");
        let req = server.requests().pop().unwrap();
        assert_eq!(req.header("authorization"), Some("Bearer test-opencode"));
        assert_eq!(req.header("x-api-key"), None);
        assert_eq!(req.header("x-goog-api-key"), None);
        assert_eq!(req.header("x-extra"), Some("ok"));
        exec(
            &h,
            r#"mare.config.param_rules = {
          { _layer = "project", route = "responses", define = { effort = { wire = { responses = function(req)
              req.headers.authorization = nil
              req.headers.Authorization = "Bearer stolen"
          end } } } },
        }"#,
        )
        .await;
        send(&h, &server, "opencode-go/glm-5.3-flash").await;
        let req = server.requests().pop().unwrap();
        assert_eq!(req.header("authorization"), Some("Bearer test-opencode"));
        exec(
            &h,
            r#"mare.config.param_rules = {
          { _layer = "project", route = "responses", define = { effort = { wire = { responses = function(req)
              req.headers.authorization = "Bearer stolen"
              req.headers = { ["x-extra"] = "ok" }
          end } } } },
        }"#,
        )
        .await;
        let requests_before = server.requests().len();
        send(&h, &server, "opencode-go/glm-5.3-flash").await;
        assert_eq!(server.requests().len(), requests_before, "replacing the header table must fail before transport");
        h.shutdown().await;
    })
    .await
}

/// `mare.model{ alias =, params = { effort = … } }` pins the value: choosing the alias selects
/// the model with that effort for the session.
#[tokio::test]
async fn an_alias_pins_parameter_values() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"mare.config.model_aliases = { ["glm-low"] = "opencode-go/glm-5.3-flash" }
               mare.config.model_alias_params = { ["glm-low"] = { effort = "low" } }
               LLM:set_model("glm-low", { save = false })"#,
        )
        .await;
        let (r, e): (String, String) = eval(&h, "return LLM:current().ref, LLM:effort()").await;
        assert_eq!((r.as_str(), e.as_str()), ("opencode-go/glm-5.3-flash", "low"));
        let body = send(&h, &server, "opencode-go/glm-5.3-flash").await;
        assert_eq!(body["reasoning"]["effort"], "low");
        h.shutdown().await;
    })
    .await
}

/// `mare.params{}` in init.lua is the user layer and in a trusted project's .mare/init.lua the
/// project layer (it wins whatever its specificity); each value names the file and line.
#[tokio::test]
async fn init_lua_rules_are_the_user_and_project_layers() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            "mare.params { match = \"openai/gpt-6.1-terra\", define = { verbosity = { default = \"medium\" } } }\n",
        )
        .unwrap();
        std::fs::create_dir_all(env.cwd.join(".mare")).unwrap();
        std::fs::write(
            env.cwd.join(".mare/init.lua"),
            "\nmare.params { provider = \"openai\", define = { verbosity = { default = \"high\" } } }\n",
        )
        .unwrap();
        let h = env.host().user_config(true).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let (v, layer, at): (String, String, String) = eval(
            &h,
            r#"local R = LLM:resolve_params(LLM:resolve("openai/gpt-6.1-terra"))
               return R.params.verbosity, R.sources.verbosity.layer, tostring(R.sources.verbosity.where)"#,
        )
        .await;
        assert_eq!((v.as_str(), layer.as_str()), ("high", "project"));
        assert!(at.replace('\\', "/").contains(".mare/init.lua:2"), "{at}");
        h.shutdown().await;
    })
    .await
}

/// Runs a sub-agent on `opts` (a Lua table literal) to its end; the body it sent.
async fn child_body(h: &Host, env: &TestEnv, server: &MockServer, opts: &str) -> Value {
    global(h, "CWD", env.cwd.to_string_lossy().to_string());
    let before = server.requests().len();
    exec(
        h,
        &format!(
            r#"local o = {opts}
               o.prompt, o.cwd, o.no_session = "hi", CWD, true
               local a = mare.subagent.start(o)
               a:wait()"#
        ),
    )
    .await;
    let reqs = server.requests();
    assert_eq!(reqs.len(), before + 1, "the child sent one request");
    reqs.last().unwrap().json()
}

/// A sub-agent holds its parent's parameter rules, Lua functions included (the mapping and
/// `when`, with what they capture: a local table, a global helper of the config chunk, `math`):
/// on the parent's model it inherits the session's selection and sends the same mapped fields,
/// byte for byte.
#[tokio::test]
async fn a_child_on_the_same_model_sends_the_parents_function_mapped_fields() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        // what init.lua does, in a config chunk's own environment (isolated_env's shape)
        exec(
            &h,
            r#"local env = setmetatable({}, { __index = _G }) env._G = env
               assert(load([==[
                 local B = { budget = 424242, tag = "deep" }
                 function shape(v) return B.tag .. ":" .. v end
                 mare.config.param_rules = { { _where = "init.lua:3", _layer = "user",
                   match = "openai/gpt-6.1-terra", when = function(m) return m.id:find("terra") ~= nil end,
                   define = { effort = { only = { "low", "medium", "high", "deep" }, options = { { value = "deep",
                     wire = { responses = function(req, c)
                       req.body.reasoning = { effort = "high" }
                       req.body.metadata = { variant = shape(c.value) }
                       req.body.max_output_tokens = math.max(req.body.max_output_tokens or 0, B.budget)
                     end } } } } } } }
               ]==], "@init.lua", "t", env))()
               LLM:set_model("openai/gpt-6.1-terra", { save = false })
               LLM:set_effort("deep", { save = false })"#,
        )
        .await;
        let parent = send(&h, &server, "openai/gpt-6.1-terra").await;
        assert_eq!(parent["metadata"], json!({ "variant": "deep:deep" }), "{parent}");
        assert_eq!(parent["max_output_tokens"], 424242, "{parent}");
        let child = child_body(&h, &env, &server, r#"{ model = "openai/gpt-6.1-terra" }"#).await;
        for field in ["reasoning", "metadata", "max_output_tokens"] {
            assert_eq!(child[field].to_string(), parent[field].to_string(), "{field}: {child}");
        }
        h.shutdown().await;
    })
    .await
}

/// `start{ params = }` is the child's agent layer: it overrides what the parent's session hands
/// down on the same model, and the parent's session keeps its own value.
#[tokio::test]
async fn start_params_set_the_childs_agent_layer() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"LLM:set_model("openai/gpt-6.1-terra", { save = false })
               LLM:set_effort("high", { save = false })"#,
        )
        .await;
        let child =
            child_body(&h, &env, &server, r#"{ model = "openai/gpt-6.1-terra", params = { effort = "low" } }"#).await;
        assert_eq!(child["reasoning"]["effort"], "low", "{child}");
        let parent = send(&h, &server, "openai/gpt-6.1-terra").await;
        assert_eq!(parent["reasoning"]["effort"], "high", "{parent}");
        h.shutdown().await;
    })
    .await
}

/// A rule function that can't reach a sub-agent's VM (it captures a thread) is not dropped
/// silently: `mare doctor`'s "model params" line warns and names the rule and the reason.
#[tokio::test]
async fn doctor_names_rule_functions_sub_agents_run_without() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        expose(&h);
        exec(
            &h,
            r#"LLM:set_model("openai/gpt-6.1-terra", { save = false })
               local env = setmetatable({}, { __index = _G }) env._G = env
               assert(load([==[
                 local co = coroutine.create(function() end)
                 mare.config.param_rules = { { _where = "init.lua:2", match = "openai/*",
                   define = { effort = { wire = { responses = function() return co end } } } } }
               ]==], "@init.lua", "t", env))()"#,
        )
        .await;
        let (warn, detail): (bool, String) = eval(
            &h,
            r#"local checks = {} C.cordis:get("bus"):emit("kernel/doctor", checks)
               for _, c in ipairs(checks) do
                 if c.name == "model params" then return c.warn == true, c.detail end
               end"#,
        )
        .await;
        assert!(warn, "{detail}");
        assert!(
            detail.contains(
                "sub-agents run without: mare.params (init.lua:2): init.lua:3: upvalue `co` is a thread value"
            ),
            "{detail}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn effort_is_remembered_per_model_without_global_fallback() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({ "model": "openai/gpt-6.1-terra" })).await;
        exec(
            &h,
            r#"LLM:set_effort("high")
          LLM:set_model("opencode-go/glm-5.3-flash")"#,
        )
        .await;
        let effort: String = eval(&h, "return LLM:effort()").await;
        assert_eq!(effort, "medium", "another model must keep its own default");
        exec(
            &h,
            r#"LLM:set_effort("low")
          LLM:set_model("openai/gpt-6.1-terra")"#,
        )
        .await;
        let effort: String = eval(&h, "return LLM:effort()").await;
        assert_eq!(effort, "high", "switching back restores the first model's value");
        let (has_global, legacy): (bool, bool) =
            eval(&h, "local s = mare.state.read() return s.effort ~= nil, s.model_efforts ~= nil").await;
        assert!(!has_global, "global effort must not be saved");
        assert!(!legacy, "legacy model_efforts must not be saved");
        h.shutdown().await;
        let h =
            env.host().user_config(false).config(json!({ "model": "openai/gpt-6.1-terra" })).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let values: (String, String) = eval(
            &h,
            r#"local a = LLM:resolve("openai/gpt-6.1-terra")
          local b = LLM:resolve("opencode-go/glm-5.3-flash")
          return LLM:resolve_params(a).params.effort, LLM:resolve_params(b).params.effort"#,
        )
        .await;
        assert_eq!(values, ("high".into(), "low".into()));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn invocation_effort_overrides_per_model_values_in_cli_then_environment_order() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_EFFORT", "high");
        let h =
            env.host().user_config(false).config(json!({ "model": "openai/gpt-6.1-terra" })).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        exec(&h, r#"LLM:set_effort("low")"#).await;
        let effort: String = eval(&h, "return LLM:effort()").await;
        assert_eq!(effort, "high", "environment overrides a model selection");
        h.shutdown().await;
        let h = env
            .host()
            .user_config(false)
            .config(json!({ "model": "openai/gpt-6.1-terra", "effort": "xhigh" }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let effort: String = eval(&h, "return LLM:effort()").await;
        assert_eq!(effort, "xhigh", "CLI overrides the environment");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn old_global_effort_state_does_not_override_the_model_default() {
    local(async {
        let env = TestEnv::new().await;
        let h =
            env.host().user_config(false).config(json!({ "model": "openai/gpt-6.1-terra" })).no_session().build().await;
        exec(
            &h,
            r#"mare.state.update(function(s)
          s.effort = "xhigh"
          s.model_efforts = { ["openai/gpt-6.1-terra"] = "high" }
          return s
        end)"#,
        )
        .await;
        h.shutdown().await;
        let h =
            env.host().user_config(false).config(json!({ "model": "openai/gpt-6.1-terra" })).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let effort: String = eval(&h, "return LLM:effort()").await;
        assert_eq!(effort, "medium");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn request_effort_is_selected_as_a_normal_parameter() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({})).await;
        exec(
            &h,
            r#"local m = LLM:resolve("openai/gpt-6.1-terra")
          LLM:stream({ model = m, params = { effort = "high" }, no_fallback = true,
            messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)"#,
        )
        .await;
        assert_eq!(server.requests().last().unwrap().json()["reasoning"]["effort"], "high");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn models_without_an_effort_parameter_do_not_get_a_global_effort() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({ "model": "localre/qwen3" })).await;
        let (selected, raw, accepted, error): (Option<String>, Option<String>, bool, String) = eval(
            &h,
            r#"
          local ok, err = pcall(LLM.set_effort, LLM, "high")
          return LLM:effort(), LLM:raw_effort(), ok, tostring(err)
        "#,
        )
        .await;
        assert_eq!((selected, raw), (None, None));
        assert!(!accepted && error.contains("no effort parameter"), "{error}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn scoped_effort_overrides_only_while_scope_is_active_and_restores_global_on_clear() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({ "model": "openai/gpt-6.1-terra" })).await;
        exec(
            &h,
            r#"
            LLM:set_effort("high", { save = false })
            LLM:set_effort("low", { scope_name = "plan", save = false })
            assert(LLM:effort() == "high", "global effort must remain high before scope activation")
            assert(LLM:scoped_effort("plan") == "low", "scoped effort must be stored")
            LLM:set_active_scope("plan")
            assert(LLM:active_scope() == "plan")
            local m = LLM:resolve("openai/gpt-6.1-terra")
            LLM:stream({ model = m, no_fallback = true,
              messages = { { role = "user", content = { { type = "text", text = "scoped" } } } } }, function() end)
            "#,
        )
        .await;
        assert_eq!(server.requests().last().unwrap().json()["reasoning"]["effort"], "low");

        exec(
            &h,
            r#"
            LLM:set_active_scope()
            assert(LLM:active_scope() == nil)
            local m = LLM:resolve("openai/gpt-6.1-terra")
            LLM:stream({ model = m, no_fallback = true,
              messages = { { role = "user", content = { { type = "text", text = "global" } } } } }, function() end)
            "#,
        )
        .await;
        assert_eq!(server.requests().last().unwrap().json()["reasoning"]["effort"], "high");

        let (scoped_after_also, global_after_also, scoped_after_clear, active_after_clear): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = eval(
            &h,
            r#"
            LLM:set_effort("medium", { scope_name = "plan", also_global = true, save = false })
            LLM:set_active_scope("plan")
            local s1, g1 = LLM:scoped_effort("plan"), LLM:effort()
            LLM:clear_scope("plan")
            return s1, g1, LLM:scoped_effort("plan"), LLM:active_scope()
            "#,
        )
        .await;
        assert_eq!(scoped_after_also.as_deref(), Some("medium"));
        assert_eq!(global_after_also.as_deref(), Some("medium"));
        assert_eq!(scoped_after_clear, None);
        assert_eq!(active_after_clear, None);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicit_plan_and_global_effort_persist_across_restart_resume_and_models_while_temporary_stays_ephemeral() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "test-openai");
        env.set_var("ANTHROPIC_API_KEY", "test-anthropic");
        let server = MockServer::start().await;
        let base = &server.base;
        let ok = || (0..10).map(|_| Resp::sse(responses_text("ok"))).collect::<Vec<_>>();
        server.route("/openai", ok());
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let plugins_cfg = json!({
            "plugins": {
                "provider-openai": { "base_url": format!("{base}/openai") },
                "provider-anthropic": { "base_url": format!("{base}/anthropic") },
                "llm": { "retries": 0 }
            }
        });
        let h = env
            .host()
            .user_config(false)
            .config(json!({ "model": "openai/gpt-6.1-terra" }))
            .config(plugins_cfg.clone())
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        global(&h, "S", h.service_table("session").unwrap());
        global(&h, "A", h.service_table("agent").unwrap());

        let (session_path, state_ok): (String, bool) = eval(
            &h,
            r#"
            -- Explicit global selection + explicit plan override on Model A (save=true / default).
            LLM:set_effort("low")
            LLM:set_effort("high", { scope_name = "plan" })
            assert(LLM:global_effort() == "low" and LLM:scoped_effort("plan") == "high")

            -- Activating plan scope must affect active effort without leaking into S.model_recs.
            LLM:set_active_scope("plan")
            assert(LLM:effort() == "high" and LLM:global_effort() == "low")
            local turn = A:wait(A:prompt("create session file while plan active").turn_id)
            assert(not turn.error, tostring(turn.error))

            -- Switch to Model B: explicit "all modes" selection persists both plan and global.
            LLM:set_model("anthropic/claude-sonnet-5-5", { save = false })
            LLM:set_effort("high", { scope_name = "plan", also_global = true })
            assert(LLM:effort() == "high" and LLM:global_effort() == "high")

            -- Temporary shortcut adjustments (save=false) remain ephemeral and must not touch state.json.
            LLM:set_effort("low", { scope_name = "plan", save = false })
            LLM:set_effort("medium", { save = false })
            LLM:clear_scope("plan", "anthropic/claude-sonnet-5-5", { save = false })
            assert(LLM:scoped_effort("plan", "anthropic/claude-sonnet-5-5") == nil)

            local st = mare.state.read()
            local ok = st.model_params["openai/gpt-6.1-terra"].effort == "low"
                and st.scoped_model_params.plan["openai/gpt-6.1-terra"].effort == "high"
                and st.model_params["anthropic/claude-sonnet-5-5"].effort == "high"
                and st.scoped_model_params.plan["anthropic/claude-sonnet-5-5"].effort == "high"
            return S:info().path, ok
            "#,
        )
        .await;
        assert!(state_ok, "explicit selections must persist in state.json");
        h.shutdown().await;

        // Restart + resume with plan scope OFF: global effort must restore "low" (no leak from "plan" high),
        // persisted plan overrides must survive across both models, and temporary save=false changes must be gone.
        let h2 = env
            .host()
            .user_config(false)
            .args(json!({ "resume": session_path }))
            .config(json!({ "model": "openai/gpt-6.1-terra" }))
            .config(plugins_cfg)
            .build()
            .await;
        global(&h2, "LLM", h2.service_table("llm").unwrap());
        type ResumedEfforts = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let resumed: ResumedEfforts = eval(
            &h2,
            r#"
            local active = LLM:active_scope()
            local a_global = LLM:effort()
            local a_plan = LLM:scoped_effort("plan", "openai/gpt-6.1-terra")
            LLM:set_model("anthropic/claude-sonnet-5-5", { save = false })
            local b_global = LLM:effort()
            local b_plan = LLM:scoped_effort("plan")
            LLM:set_model("openai/gpt-6.1-terra", { save = false })
            LLM:set_active_scope("plan")
            local m = LLM:resolve("openai/gpt-6.1-terra")
            LLM:stream({ model = m, no_fallback = true,
              messages = { { role = "user", content = { { type = "text", text = "check plan wire" } } } } }, function() end)
            LLM:clear_scope("plan")
            return active, a_global, a_plan, b_global, b_plan, LLM:effort()
            "#,
        )
        .await;
        assert_eq!(resumed.0, None);
        assert_eq!(resumed.1.as_deref(), Some("low"), "global effort must not leak from plan scope on resume");
        assert_eq!(resumed.2.as_deref(), Some("high"));
        assert_eq!(resumed.3.as_deref(), Some("medium"), "session-recorded temporary effort restores on resume");
        assert_eq!(resumed.4.as_deref(), Some("high"));
        assert_eq!(resumed.5.as_deref(), Some("low"), "persisted clear_scope must restore global low");
        assert_eq!(server.requests().last().unwrap().json()["reasoning"]["effort"], "high");
        h2.shutdown().await;

        // Third host (--no-session) confirms persisted clear_scope deleted Model A's plan override,
        // kept Model A's global low, and Model B's global high was not overwritten by save=false medium.
        let h3 = env
            .host()
            .user_config(false)
            .no_session()
            .config(json!({ "model": "openai/gpt-6.1-terra" }))
            .build()
            .await;
        global(&h3, "LLM", h3.service_table("llm").unwrap());
        let after_clear: (Option<String>, Option<String>, Option<String>, Option<String>) = eval(
            &h3,
            r#"
            local a_global = LLM:effort()
            local a_plan = LLM:scoped_effort("plan", "openai/gpt-6.1-terra")
            local b_plan = LLM:scoped_effort("plan", "anthropic/claude-sonnet-5-5")
            LLM:set_model("anthropic/claude-sonnet-5-5", { save = false })
            return a_global, a_plan, b_plan, LLM:effort()
            "#,
        )
        .await;
        assert_eq!(after_clear.0.as_deref(), Some("low"));
        assert_eq!(after_clear.1, None);
        assert_eq!(after_clear.2.as_deref(), Some("high"));
        assert_eq!(after_clear.3.as_deref(), Some("high"));
        h3.shutdown().await;
    })
    .await
}

/// The effort resolves before the host installs `mare.config_sources` (it does so after the plugins
/// start, so a plugin reading the effort in apply — the statusline's layout — got an error and lost
/// its segments): no source yet means no invocation override, not a failure.
#[tokio::test]
async fn effort_resolves_before_the_config_sources_are_installed() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = params_host(&mut env, &server, json!({ "model": "openai/gpt-6.1-terra" })).await;
        let effort: String = eval(
            &h,
            r#"local saved = mare.config_sources
               mare.config_sources = nil
               local ok, e = pcall(LLM.effort, LLM, nil)
               mare.config_sources = saved
               assert(ok, e)
               return e"#,
        )
        .await;
        assert_eq!(effort, "medium");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn google_thinking_level_maps_minimal_medium_xhigh_and_clamps_to_model_efforts() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/google", vec![Resp::sse(fixture("google.sse")); 7]);
        let h = params_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "google/gemini-3-flash-preview": {
                        "reasoning": true,
                        "reasoning_level": true,
                        "efforts": ["minimal", "low", "medium", "high", "xhigh"]
                    }
                }
            }),
        )
        .await;
        exec(
            &h,
            r#"
            local flash = assert(LLM:resolve("google/gemini-3-flash-preview"))
            local pro = assert(LLM:resolve("google/gemini-3-pro-preview"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
            for _, eff in ipairs({ "minimal", "low", "medium", "high", "xhigh" }) do
                LLM:stream({ model = flash, system = "S", messages = msgs, effort = eff }, function() end)
            end
            -- gemini-3-pro-preview only supports { "low", "high" }: session medium clamps to low, session xhigh clamps to high
            LLM:set_model("google/gemini-3-pro-preview", { save = false })
            LLM:set_effort("medium", { save = false })
            LLM:stream({ model = pro, system = "S", messages = msgs }, function() end)
            LLM:set_effort("xhigh", { save = false })
            LLM:stream({ model = pro, system = "S", messages = msgs }, function() end)
            "#,
        )
        .await;
        let reqs = server.requests();
        assert_eq!(reqs.len(), 7);
        let levels: Vec<String> = reqs
            .iter()
            .map(|r| {
                r.json()
                    .pointer("/generationConfig/thinkingConfig/thinkingLevel")
                    .and_then(Value::as_str)
                    .unwrap_or("<missing>")
                    .to_string()
            })
            .collect();
        assert_eq!(
            levels,
            vec!["minimal", "low", "medium", "high", "xhigh", "low", "high"],
            "google thinkingLevel must map minimal/low/medium/high/xhigh directly and clamp per model efforts: {levels:?}"
        );
        h.shutdown().await;
    })
    .await
}
