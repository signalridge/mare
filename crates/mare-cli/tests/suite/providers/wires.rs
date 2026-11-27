//! Third-party wires: a wire a user plugin registers with a `describe`
//! makes its params route known (an init.lua rule maps it and the wire applies it), shows in the
//! opencode Go doctor line at its own path, and is blamed on the plugin declaring it
//! (`wires = { … }`) while that plugin is turned off. Nothing here names a built-in wire.

use super::*;

/// A wire that answers with what it was handed: the endpoint's url and auth, and the `level`
/// field its params route mapped into the body.
const ECHO_WIRE: &str = r#"return {
  name = "echo-wire", inject = { "llm" }, wires = { "echo-chat" },
  apply = function(ctx)
    local function echo_spec(name, route, path, auth)
      return {
        name = name,
        route = route,
        path = path,
        auth = auth,
        encode = function(req, e)
          return { url = e.url, headers = {}, body = {} }, {}
        end,
        finalize = function(_req, e, w)
          w.echo_text = e.url .. " " .. tostring(e.auth) .. " " .. tostring(w.body.level)
        end,
        decoder = function(sink)
          local text = ""
          return {
            event = function(ev)
              text = tostring(ev.data or "")
              sink({ type = "text_delta", delta = text })
              return true
            end,
            finish = function()
              return {
                role = "assistant",
                content = { { type = "text", text = text } },
                stop_reason = "stop",
                usage = { input = 1, output = 1, cache_read = 0, cache_write = 0 },
              }
            end,
          }
        end,
      }
    end
    _G.__KEEP_UNUSED_SPEC = echo_spec
    ctx:on("provider/transport", function(ev)
      if type(ev.request) == "table" and ev.request.echo_text ~= nil then
        local sent = false
        return {
          ok = true,
          status = 200,
          headers = {},
          close = function() end,
          next_event = function()
            if sent then return nil end
            sent = true
            return { data = ev.request.echo_text }
          end,
        }
      end
    end)
    ctx.llm:register_wire(echo_spec("echo-chat", "echo", "/echo/send", { "bearer" }))
  end,
}"#;

async fn wire_host(env: &mut TestEnv, server: &MockServer, extra: Value) -> Arc<Host> {
    env.set_var("OPENCODE_API_KEY", "test-opencode");
    let dir = env.config_dir().join("plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("echo-wire.lua"), ECHO_WIRE).unwrap();
    let base = &server.base;
    let h = env
        .host()
        .builtins(&["compact", "usage"])
        .config(json!({ "plugins": {
            "llm": { "retries": 0 },
            "provider-opencode-go": {
                "base_url": format!("{base}/gw"),
                "routes": { "echo": { "path": "/x", "wire": "echo-chat" } },
                "model_routes": { "crouton-*": "echo" },
            },
        } }))
        .config(extra)
        .user_plugins()
        .no_session()
        .build()
        .await;
    global(&h, "LLM", h.service_table("llm").unwrap());
    expose(&h);
    h
}

/// The opencode Go doctor line.
async fn doctor_routes(h: &Host) -> String {
    eval(
        h,
        r#"local checks = {} C.cordis:get("bus"):emit("kernel/doctor", checks)
           for _, c in ipairs(checks) do if c.name == "opencode Go routes" then return c.detail end end"#,
    )
    .await
}

#[tokio::test]
async fn a_third_party_wire_gets_params_mappings_and_shows_in_doctor() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(&mut env, &server, json!({})).await;
        exec(&h, r#"mare.config.param_rules = { { route = "echo", define = { effort = { wire = { echo = { level = "$value" } } } } } }"#)
            .await;
        let probs: String = eval(&h, r#"return table.concat(LLM:explain_params("opencode-go/crouton-alpha-free").problems, ";")"#).await;
        assert!(!probs.contains("unknown route"), "the wire's describe makes route echo known: {probs}");
        let text: String = eval(
            &h,
            r#"local r = LLM:stream({ model = LLM:resolve("opencode-go/crouton-alpha-free"), system = "S",
                 no_fallback = true, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
               return r.content[1].text"#,
        )
        .await;
        assert_eq!(text, format!("{}/gw/x/echo/send bearer medium", server.base), "url from describe.path, effort mapped");
        let detail = doctor_routes(&h).await;
        assert!(detail.contains(&format!("echo → {}/gw/x/echo/send (echo-chat)", server.base)), "{detail}");
        // a route nothing provides is still named (and listed against the known ones)
        exec(&h, r#"mare.config.param_rules = { { route = "telnet", define = {} } }"#).await;
        let probs: String = eval(&h, r#"return table.concat(LLM:explain_params("openai/gpt-6.1-terra").problems, ";")"#).await;
        assert!(probs.contains("unknown route \"telnet\"") && probs.contains("echo"), "{probs}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_turned_off_wire_plugin_is_blamed_by_its_declaration() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(&mut env, &server, json!({ "plugins": { "echo-wire": { "enabled": false } } })).await;
        let (owner, wires): (String, String) = eval(
            &h,
            r#"return LLM:wire_owner("echo-chat"), table.concat(mare.plugins.status("echo-wire").wires, ",")"#,
        )
        .await;
        assert_eq!((owner.as_str(), wires.as_str()), ("echo-wire", "echo-chat"));
        let why: String = eval(
            &h,
            r#"return LLM:provider("opencode-go").unavailable_for(LLM:resolve("opencode-go/crouton-alpha-free"))"#,
        )
        .await;
        assert_eq!(why, "provider opencode-go route echo unavailable (plugin echo-wire is disabled)");
        let err: String = eval(
            &h,
            r#"local m = LLM:resolve("opencode-go/crouton-alpha-free")
          local msg = LLM:stream({ model = m, messages = {}, no_fallback = true }, function() end)
          return msg.error"#,
        )
        .await;
        assert!(err.contains("echo-chat") && err.contains("wire"), "missing wire must refuse the request: {err}");
        let detail = doctor_routes(&h).await;
        assert!(detail.contains(&format!("echo → {}/gw/x (echo-chat, unavailable)", server.base)), "{detail}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn gateway_alias_uses_the_wires_route_for_selection_and_mapping() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": {
            "routes": { "alias": { "path": "/alias", "wire": "echo-chat" } },
            "model_routes": { "crouton-*": "alias" },
        } } }),
        )
        .await;
        exec(
            &h,
            r#"mare.config.param_rules = { { route = "echo", define = { effort = {
          default = "high", wire = { echo = { level = "$value" } }
        } } } }"#,
        )
        .await;
        let (route, wire_route, value): (String, String, String) = eval(
            &h,
            r#"
          local m = LLM:resolve("opencode-go/crouton-alpha-free")
          local gateway, _, wire = LLM:route_of(m)
          return gateway, wire, LLM:resolve_params(m).params.effort
        "#,
        )
        .await;
        assert_eq!((route.as_str(), wire_route.as_str(), value.as_str()), ("alias", "echo", "high"));
        let lines: String =
            eval(&h, r#"return table.concat(LLM:explain_lines("opencode-go/crouton-alpha-free"), "\n")"#).await;
        assert!(lines.contains("route alias → echo"), "preview shows both routes: {lines}");
        let text: String = eval(
            &h,
            r#"local m = LLM:resolve("opencode-go/crouton-alpha-free")
          local msg = LLM:stream({ model = m, messages = {}, no_fallback = true }, function() end)
          return msg.content[1].text"#,
        )
        .await;
        assert_eq!(text, format!("{}/gw/alias/echo/send bearer high", server.base));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn gateway_auth_is_checked_against_the_selected_wire() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(
            &mut env,
            &server,
            json!({ "plugins": { "provider-opencode-go": {
          "routes": { "echo": { "auth": "x-api-key" } },
        } } }),
        )
        .await;
        let (why, stop, err): (String, String, String) = eval(
            &h,
            r#"
          local m = LLM:resolve("opencode-go/crouton-alpha-free")
          local why = LLM:provider("opencode-go").unavailable_for(m)
          local msg = LLM:stream({ model = m, messages = {}, no_fallback = true }, function() end)
          return tostring(why), msg.stop_reason, tostring(msg.error)
        "#,
        )
        .await;
        assert!(why.contains("x-api-key") && why.contains("echo-chat"), "{why}");
        assert_eq!(stop, "error");
        assert!(err.contains("x-api-key") && err.contains("echo-chat"), "{err}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn gateway_defaults_to_the_selected_wires_first_auth_style() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(&mut env, &server, json!({})).await;
        let text: String = eval(
            &h,
            r#"
          LLM:register_wire({
            name = "echo-alt",
            route = "echo",
            path = "/echo/send",
            auth = { "x-api-key", "bearer" },
            encode = function(_req, e) return { url = e.url, headers = {}, body = {} }, {} end,
            finalize = function(_req, e, w) w.echo_text = e.url .. " " .. tostring(e.auth) .. " " .. tostring(w.body.level) end,
            decoder = function(sink)
              local t = ""
              return {
                event = function(ev) t = tostring(ev.data or ""); sink({ type = "text_delta", delta = t }); return true end,
                finish = function() return { role = "assistant", content = { { type = "text", text = t } }, stop_reason = "stop" } end,
              }
            end,
          })
          LLM:provider("opencode-go").routes.echo.wire = "echo-alt"
          local m = LLM:resolve("opencode-go/crouton-alpha-free")
          local msg = LLM:stream({ model = m, messages = {}, no_fallback = true }, function() end)
          return msg.content[1].text
        "#,
        )
        .await;
        assert_eq!(text, format!("{}/gw/x/echo/send x-api-key nil", server.base));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn an_inflight_request_uses_the_wire_that_selected_its_parameters() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(&mut env, &server, json!({})).await;
        let text: String = eval(&h, r#"
          mare.config.param_rules = { { route = "echo", define = { effort = { wire = { echo = { level = "$value" } } } } } }
          LLM:register_wire({
            name = "echo-alt",
            route = "other",
            path = "/other",
            auth = { "bearer" },
            encode = function(req, e) return { url = e.url, headers = {}, body = {} }, {} end,
            finalize = function(_req, _e, w) w.echo_text = "wrong wire" end,
            decoder = function(sink)
              local t = ""
              return {
                event = function(ev) t = tostring(ev.data or ""); sink({ type = "text_delta", delta = t }); return true end,
                finish = function() return { role = "assistant", content = { { type = "text", text = t } }, stop_reason = "stop" } end,
              }
            end,
          })
          local p = LLM:provider("opencode-go")
          local original = p.stream
          p.stream = function(req, sink)
            p.routes.echo.wire = "echo-alt" -- change the registry after llm resolved the request
            return original(req, sink)
          end
          local m = LLM:resolve("opencode-go/crouton-alpha-free")
          local msg = LLM:stream({ model = m, messages = {}, no_fallback = true }, function() end)
          return msg.content[1].text
        "#).await;
        assert_eq!(text, format!("{}/gw/x/echo/send bearer medium", server.base));
        h.shutdown().await;
    }).await
}

/// Drive a wire through the real transport and inspect events and the final message, as in
/// Codex codex-rs/codex-api/tests/sse_end_to_end.rs:98.
#[tokio::test]
async fn a_spec_wire_sends_and_decodes_through_the_shared_path() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/gw/x/send", vec![Resp::sse("data: {\"text\":\"hello\"}\n\ndata: {\"text\":\" world\"}\n\n")]);
        let h = wire_host(&mut env, &server, json!({})).await;
        let result: String = eval(
            &h,
            r#"
          LLM:register_wire({
            name = "spec-chat", route = "echo", path = "/send", auth = { "bearer" },
            encode = function(req, endpoint)
              return { method = "POST", url = endpoint.url, headers = { accept = "text/event-stream" },
                body = { model = req.model.id, input = "hello" } }
            end,
            decoder = function(sink)
              local text = ""
              return {
                event = function(ev)
                  local part = mare.json.decode(ev.data).text
                  text = text .. part
                  sink({ type = "text_delta", delta = part })
                end,
                finish = function()
                  return { role = "assistant", content = { { type = "text", text = text } },
                    stop_reason = "stop", usage = { input = 1, output = 2 } }
                end,
              }
            end,
            error = function(payload) return payload.error end,
          })
          LLM:provider("opencode-go").routes.echo.wire = "spec-chat"
          local seen = {}
          C.cordis:on("provider/request", function(w, ev)
            w.headers["x-wire-hook"] = "yes"
            return ev.next()
          end)
          C.cordis:on("provider/event", function(data, ev)
            seen[#seen + 1] = "raw"
            return ev.next()
          end)
          local msg = LLM:stream({ model = LLM:resolve("opencode-go/crouton-alpha-free"),
            messages = {}, no_fallback = true }, function(ev) seen[#seen + 1] = ev.delta end)
          return (msg.content[1] and msg.content[1].text or tostring(msg.error)) .. "|" .. table.concat(seen, ",")
        "#,
        )
        .await;
        assert_eq!(result, "hello world|raw,hello,raw, world");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].header("authorization"), Some("Bearer test-opencode"));
        assert_eq!(requests[0].header("x-wire-hook"), Some("yes"));
        assert_eq!(requests[0].json()["input"], "hello");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn prepared_overlay_does_not_run_wire_callbacks_during_compaction_measurement() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(&mut env, &server, json!({ "models": {
            "opencode-go/crouton-alpha-free": { "context_window": 100000, "max_output": 8000 }
        }, "plugins": { "compact": { "threshold_ratio": 1, "headroom_tokens": 0 } } })).await;
        exec(&h, r#"
            BUILDS, APPLIES, MAPS, ENCODES, FINALIZES, WIRES, COMPACT = 0, 0, 0, 0, 0, 0, 0
            LLM:register_wire({ name = "counting-wire", route = "echo", path = "/send", auth = { "bearer" },
                encode = function(req, endpoint)
                    ENCODES = ENCODES + 1
                    return { url = endpoint.url, headers = {}, body = {} }, {}
                end,
                finalize = function(_req, _endpoint, wire)
                    FINALIZES = FINALIZES + 1
                    wire.echo_text = tostring(wire.body.level)
                end,
                decoder = function(sink)
                    local text = ""
                    return { event = function(ev) text = ev.data sink({ type = "text_delta", delta = text }) return true end,
                        finish = function() return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = text } } } end }
                end })
            LLM:provider("opencode-go").routes.echo.wire = "counting-wire"
            mare.config.param_rules = { { route = "echo", define = { effort = { wire = { echo = function(wire, c)
                MAPS = MAPS + 1
                wire.body.level = c.value
            end } } } } }
            LLM:set_model("opencode-go/crouton-alpha-free", { save = false })
            local apply = LLM.apply_params
            LLM.apply_params = function(self, ...) APPLIES = APPLIES + 1 return apply(self, ...) end
            C.cordis:on("request/build", function(req, next)
                BUILDS = BUILDS + 1 req.effort = "high" req.system = string.rep("x", 400000) return next()
            end)
            C.cordis:on("compact/before", function() COMPACT = COMPACT + 1 return true end)
            C.cordis:on("provider/request", function(wire, next) WIRES = WIRES + 1 return next() end)
        "#).await;
        let result = h.prompt("hi", json!({})).await.unwrap();
        assert!(result["error"].is_null(), "{result}");
        assert_eq!(result["text"], "high");
        assert_eq!(eval::<(i64, i64, i64, i64, i64, i64, i64)>(&h,
            "return BUILDS, APPLIES, MAPS, ENCODES, FINALIZES, WIRES, COMPACT").await, (1, 1, 1, 1, 1, 1, 1));
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn spec_wire_dispatches_json_to_body_or_reports_a_malformed_response() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/gw/x/send", vec![Resp::status(200, r#"{"text":"body"}"#)]);
        let h = wire_host(&mut env, &server, json!({})).await;
        let (body, kind): (String, String) = eval(
            &h,
            r#"
          local with_body = true
          LLM:register_wire({
            name = "json-chat", route = "echo", path = "/send", auth = { "bearer" },
            encode = function(req, endpoint)
              return { method = "POST", url = endpoint.url, headers = {}, body = { model = req.model.id } }
            end,
            decoder = function()
              local text
              return {
                event = function() error("JSON must not be fed as an SSE event") end,
                body = with_body and function(raw) text = mare.json.decode(raw).text end or nil,
                finish = function() return { role = "assistant", stop_reason = "stop",
                  content = { { type = "text", text = text } }, usage = { input = 1, output = 1 } } end,
              }
            end,
          })
          LLM:provider("opencode-go").routes.echo.wire = "json-chat"
          local function call()
            return LLM:stream({ model = LLM:resolve("opencode-go/crouton-alpha-free"),
              messages = {}, no_fallback = true }, function() end)
          end
          local ok = call()
          with_body = false
          local bad = call()
          return ok.content[1] and ok.content[1].text or tostring(ok.error), tostring(bad.error_kind)
        "#,
        )
        .await;
        assert_eq!((body.as_str(), kind.as_str()), ("body", "malformed_response"));
        assert_eq!(server.requests().len(), 2);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn spec_wire_extracts_the_http_error_from_its_own_payload_shape() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/gw/x/send",
            vec![Resp::status(400, r#"{"issue":{"code":"bad_input","message":"bad custom input"}}"#)],
        );
        let h = wire_host(&mut env, &server, json!({})).await;
        let (message, kind): (String, String) = eval(
            &h,
            r#"
          LLM:register_wire({
            name = "error-chat", route = "echo", path = "/send", auth = { "bearer" },
            encode = function(req, endpoint)
              return { method = "POST", url = endpoint.url, headers = {}, body = { model = req.model.id } }
            end,
            decoder = function() error("an HTTP error must not start a decoder") end,
            error = function(payload)
              return { api_code = payload.issue.code, message = payload.issue.message }
            end,
          })
          LLM:provider("opencode-go").routes.echo.wire = "error-chat"
          local msg = LLM:stream({ model = LLM:resolve("opencode-go/crouton-alpha-free"),
            messages = {}, no_fallback = true }, function() end)
          return tostring(msg.error), tostring(msg.error_kind)
        "#,
        )
        .await;
        assert!(message.ends_with(": bad custom input"), "{message}");
        assert!(!message.contains("issue") && !message.contains('{'), "{message}");
        assert_eq!(kind, "invalid");
        assert_eq!(server.requests().len(), 1);
        h.shutdown().await;
    })
    .await
}

/// Returning HTTP 200 with a non-SSE Content-Type on a streaming-only wire (Anthropic) fails as
/// non-retryable `malformed_response`.
#[tokio::test]
async fn anthropic_non_sse_200_response_fails_as_non_retryable_malformed_response() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::status(200, r#"{"type":"error","error":{"message":"unexpected"}}"#)]);
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 2 } } })).await;
        let (stop, kind, err): (String, String, String) = eval(
            &h,
            r#"
          local m = LLM:resolve("anthropic/claude-sonnet-5-5")
          local msg = LLM:stream({ model = m, messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            no_fallback = true }, function() end)
          return tostring(msg.stop_reason), tostring(msg.error_kind), tostring(msg.error)
        "#,
        )
        .await;
        assert_eq!((stop.as_str(), kind.as_str()), ("error", "malformed_response"), "{err}");
        assert!(err.contains("unexpected non-streaming response"), "{err}");
        assert_eq!(server.requests().len(), 1, "malformed_response must not be retried");
        h.shutdown().await;
    })
    .await
}

/// A top-level Anthropic HTTP error body without an `error` object must keep the baseline
/// message/type/code parsing (not promote top-level `type`/`code` into API error classification).
#[tokio::test]
async fn anthropic_top_level_error_body_without_error_table_keeps_baseline_classification() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic",
            vec![Resp::status(
                400,
                r#"{"error":"plain string","type":"rate_limit_error","message":"should stay invalid"}"#,
            )],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 2 } } })).await;
        let (stop, kind, err): (String, String, String) = eval(
            &h,
            r#"
          local m = LLM:resolve("anthropic/claude-sonnet-5-5")
          local msg = LLM:stream({ model = m, messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
            no_fallback = true }, function() end)
          return tostring(msg.stop_reason), tostring(msg.error_kind), tostring(msg.error)
        "#,
        )
        .await;
        assert_eq!((stop.as_str(), kind.as_str()), ("error", "invalid"), "{err}");
        assert!(err.contains("plain string") && !err.contains("should stay invalid"), "{err}");
        assert_eq!(server.requests().len(), 1, "invalid 400 must not be retried as rate_limit");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn register_wire_validates_spec_name_encode_and_decoder() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let errs: String = eval(
            &h,
            r#"
          local function err_of(fn)
            local ok, e = pcall(fn)
            assert(not ok)
            return tostring(e)
          end
          local e1 = err_of(function() LLM:register_wire({ encode = function() end, decoder = function() end }) end)
          local e2 = err_of(function() LLM:register_wire({ name = "bad-1", decoder = function() end }) end)
          local e3 = err_of(function() LLM:register_wire({ name = "bad-2", encode = function() end }) end)
          return e1 .. "|" .. e2 .. "|" .. e3
        "#,
        )
        .await;
        assert!(errs.contains("wire name"), "{errs}");
        assert!(errs.contains("spec.encode"), "{errs}");
        assert!(errs.contains("spec.decoder"), "{errs}");
        h.shutdown().await;
    })
    .await
}

/// Google Generative Language (`google-generate`) registers as a wire spec, generates
/// the model-id URL, decodes plain-JSON 200 responses identically to SSE, classifies stream errors
/// via `spec.errors`, and rejects malformed non-JSON bodies without retrying.
#[tokio::test]
async fn google_wire_generates_model_url_decodes_plain_json_and_matches_sse_events() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        // authored fixture: same chunk sequence as `fixtures/google.sse` returned
        // as a plain-JSON array (Content-Type: application/json).
        let authored_json = serde_json::to_string(&json!([
            {"candidates":[{"content":{"role":"model","parts":[{"text":"Need to read the file.","thought":true}]}}]},
            {"candidates":[{"content":{"role":"model","parts":[{"text":"Reading "}]}}]},
            {"candidates":[{"content":{"role":"model","parts":[{"text":"it now."}]}}]},
            {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"read","args":{"path":"src/main.rs"}},"thoughtSignature":"c2lnMTIz"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":120,"cachedContentTokenCount":20,"candidatesTokenCount":35,"thoughtsTokenCount":10}}
        ]))
        .unwrap();
        let stream_err_json = serde_json::to_string(&json!({
            "error": { "code": 429, "status": "RESOURCE_EXHAUSTED", "message": "quota hit" }
        }))
        .unwrap();
        server.route(
            "/google",
            vec![
                Resp::status(200, authored_json).header("content-type", "application/json"),
                Resp::sse(fixture("google.sse")),
                Resp::status(200, "not valid json").header("content-type", "text/plain"),
                Resp::status(200, stream_err_json).header("content-type", "application/json"),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "retries": 0 } } })).await;

        let (owner, route, auth0, prov_wire, same_stream_as_sse, json_usage, bad_kind, bad_err, err_kind, err_msg): (
            String,
            String,
            String,
            String,
            bool,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
          local owner = assert(LLM:wire_owner("google-generate"))
          local d = assert(LLM:describe_wire("google-generate"))
          local p = assert(LLM:provider("google"))
          local m = assert(LLM:resolve("google/gemini-2.5-pro"))
          local req = { model = m, system = "S", no_fallback = true,
            messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }

          local ev_json, ev_sse = {}, {}
          local m_json = LLM:stream(req, function(ev) ev_json[#ev_json + 1] = ev end)
          local m_sse = LLM:stream(req, function(ev) ev_sse[#ev_sse + 1] = ev end)
          local events_match = #ev_json == #ev_sse
          for i = 1, #ev_json do
            if ev_json[i].type ~= ev_sse[i].type or ev_json[i].delta ~= ev_sse[i].delta or ev_json[i].name ~= ev_sse[i].name then
              events_match = false
            end
          end
          local same = events_match
            and m_json.stop_reason == m_sse.stop_reason
            and m_json.content[1].text == m_sse.content[1].text
            and m_json.content[2].text == m_sse.content[2].text
            and m_json.content[3].name == m_sse.content[3].name
            and m_json.usage.input == m_sse.usage.input
            and m_json.usage.output == m_sse.usage.output
            and m_json.usage.cache_read == m_sse.usage.cache_read
            and m_json.usage.reasoning == m_sse.usage.reasoning

          local m_bad = LLM:stream(req, function() end)
          local m_err = LLM:stream(req, function() end)
          return owner, d.route, d.auth[1], tostring(p.wire), same, mare.json.encode(m_json.usage),
            tostring(m_bad.error_kind), tostring(m_bad.error),
            tostring(m_err.error_kind), tostring(m_err.error)
        "#,
        )
        .await;

        assert_eq!(owner, "provider-google");
        assert_eq!(route, "google");
        assert_eq!(auth0, "x-goog-api-key");
        assert_eq!(prov_wire, "google-generate");
        assert!(same_stream_as_sse, "plain-JSON 200 must decode to identical events/message as SSE: {json_usage}");
        assert_eq!((bad_kind.as_str(), bad_err.contains("neither SSE nor JSON")), ("malformed_response", true), "{bad_err}");
        assert_eq!(err_kind, "rate_limit", "{err_msg}");
        assert!(err_msg.contains("quota hit"), "{err_msg}");

        let reqs = server.requests();
        assert_eq!(reqs.len(), 4);
        assert_eq!(reqs[0].path, "/google/models/gemini-2.5-pro:streamGenerateContent?alt=sse");
        assert_eq!(reqs[0].header("x-goog-api-key"), Some("test-google"));
        h.shutdown().await;
    })
    .await
}

/// The Responses wire registers as a spec and sends via a gateway.
#[tokio::test]
async fn responses_wire_registers_spec_and_sends_via_gateway() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/gw/openai/v1/responses", vec![Resp::sse(responses_text("old wire"))]);
        let h = wire_host(&mut env, &server, json!({})).await;
        let (text, route, path): (String, String, String) = eval(
            &h,
            r#"
          local p = LLM:provider("opencode-go")
          p.model_routes = {}
          local d = LLM:describe_wire("openai-responses")
          local msg = LLM:stream({ model = LLM:resolve("opencode-go/crouton-alpha-free"),
            messages = {}, no_fallback = true }, function() end)
          return msg.content[1] and msg.content[1].text or tostring(msg.error), d.route, d.path
        "#,
        )
        .await;
        assert_eq!((text.as_str(), route.as_str(), path.as_str()), ("old wire", "responses", "/responses"));
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.requests()[0].header("authorization"), Some("Bearer test-opencode"));
        h.shutdown().await;
    })
    .await
}

/// Parser state is reachable through each wire module's `new_parser`. Exercise raw table operations
/// while a stream is active, including the effect of a caller replacing an accumulated field.
#[tokio::test]
async fn parser_state_is_opaque_during_streaming() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let checks: String = eval(
            &h,
            r#"
local ANTHROPIC = require("provider-anthropic.wire")
local RESPONSES = require("provider-openai.wire")
local COMPAT = require("provider-openai-compat.wire")
local GOOGLE = require("provider-google.wire")
local function feed(p, d) p.feed({data = mare.json.encode(d)}) end
local function check(t, key, after)
  local found = false
  for k in next, t do if k == key then found = true end end
  assert(rawget(t, key) == nil and not found and t[key] == "a")
  t[key] = "replaced"
  after()
  assert(t[key] == "replacedz")
  t[key] = nil
  assert(not pcall(after))
  t[key] = "backing"
  rawset(t, key, "shadow")
  assert(rawget(t, key) == "shadow")
  rawset(t, key, nil)
  assert(t[key] == "backing")
  after()
  return t[key]
end
local a = ANTHROPIC.new_parser(function() end)
feed(a, {type="content_block_start", index=0, content_block={type="text", text=""}})
feed(a, {type="content_block_delta", index=0, delta={type="text_delta", text="a"}})
local ar = check(a.blocks[1], "text", function()
  feed(a, {type="content_block_delta", index=0, delta={type="text_delta", text="z"}})
end)
local r = RESPONSES.new_parser(function() end)
feed(r, {type="response.output_item.added", item={type="function_call", id="item", call_id="call", name="f"}})
feed(r, {type="response.function_call_arguments.delta", item_id="item", delta="a"})
local rr = check(r.tools.item, "args", function()
  feed(r, {type="response.function_call_arguments.delta", item_id="item", delta="z"})
end)
local c = COMPAT.new_parser(function() end)
feed(c, {choices={{delta={content="a"}}}})
local cr = check(c, "text", function() feed(c, {choices={{delta={content="z"}}}}) end)
local g = GOOGLE.new_parser(function() end)
feed(g, {candidates={{content={parts={{text="a"}}}}}})
local gr = check(g.content[1], "text", function()
  feed(g, {candidates={{content={parts={{text="z"}}}}}})
end)
return table.concat({ar, rr, cr, gr}, "|")
"#,
        )
        .await;
        assert_eq!(checks, "backingz|backingz|backingz|backingz");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn buffered_provider_parsers_preserve_events_and_final_messages() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        let result: bool = eval(&h, r#"
local ANTHROPIC = require("provider-anthropic.wire")
local RESPONSES = require("provider-openai.wire")
local COMPAT = require("provider-openai-compat.wire")
local GOOGLE = require("provider-google.wire")
local function feed(p, d) p.feed({data = mare.json.encode(d)}) end
local function sink(events) return function(e) events[#events+1] = e end end
local function check(events, expected)
  assert(mare.json.encode(events) == mare.json.encode(expected))
end
local aev = {}
local a = ANTHROPIC.new_parser(sink(aev))
feed(a, {type="content_block_start", index=0, content_block={type="thinking", thinking="start", signature="s"}})
feed(a, {type="content_block_delta", index=0, delta={type="thinking_delta", thinking="é"}})
feed(a, {type="content_block_delta", index=0, delta={type="signature_delta", signature="ig"}})
feed(a, {type="content_block_start", index=1, content_block={type="tool_use", id="id", name="f", input={}}})
feed(a, {type="content_block_delta", index=1, delta={type="input_json_delta", partial_json='{"x":'}})
feed(a, {type="content_block_delta", index=1, delta={type="input_json_delta", partial_json='"yes"}'}})
feed(a, {type="content_block_stop", index=1})
feed(a, {type="message_delta", delta={stop_reason="tool_use"}, usage={output_tokens=3}})
feed(a, {type="message_stop"})
local am = a.finish()
assert(am.content[1].type == "reasoning" and am.content[1].text == "starté" and am.content[1].native["anthropic-messages"].signature == "sig")
assert(am.content[2].arguments.x == "yes" and am.content[2].json == nil and am.stop_reason == "tool_use" and am.usage.output == 3)
assert(mare.json.decode(mare.json.encode(am)).content[1].text == "starté")
check(aev, {{type="reasoning_delta", delta="é"}, {type="tool_call_start", tool_call_id="id", name="f"},
 {type="tool_call_delta", tool_call_id="id", delta='{"x":'}, {type="tool_call_delta", tool_call_id="id", delta='"yes"}'}})
local rev = {}
local r = RESPONSES.new_parser(sink(rev))
feed(r, {type="response.output_item.added", item={type="function_call", id="item", call_id="id", name="f"}})
feed(r, {type="response.function_call_arguments.delta", item_id="item", delta='{"x":'})
feed(r, {type="response.function_call_arguments.delta", item_id="item", delta='"yes"}'})
feed(r, {type="response.output_item.done", item={type="function_call", call_id="id", name="f", arguments='{"x":"yes"}'}})
feed(r, {type="response.completed", response={usage={input_tokens=2, output_tokens=3}}})
local rm = r.finish()
assert(rm.content[1].arguments.x == "yes" and rm.stop_reason == "tool_use" and rm.usage.output == 3)
check(rev, {{type="tool_call_start", tool_call_id="id", name="f"}, {type="tool_call_delta", tool_call_id="id", delta='{"x":'}, {type="tool_call_delta", tool_call_id="id", delta='"yes"}'}})
-- A synchronous sink can observe and reenter the current parser after an append.
local observed, nested, reentrant = {}, false, nil
reentrant = COMPAT.new_parser(function(e)
  observed[#observed + 1] = reentrant.text
  if not nested then
    nested = true
    feed(reentrant, {choices={{delta={content="nested"}}}})
  end
end)
feed(reentrant, {choices={{delta={content="first"}, finish_reason="stop"}}})
assert(observed[1] == "first" and observed[2] == "firstnested")
assert(reentrant.finish_message().content[1].text == "firstnested")
local cev = {}
local c = COMPAT.new_parser(sink(cev))
feed(c, {choices={{delta={content="a", reasoning_content="é", tool_calls={{index=0, id="id", ["function"]={arguments='{"x":'}}}}}}})
feed(c, {choices={{delta={content="b", tool_calls={{index=0, ["function"]={name="f", arguments='"yes"}'}}}}, finish_reason="tool_calls"}}})
local cm = c.finish_message()
assert(cm.content[1].type == "reasoning" and cm.content[1].text == "é" and cm.content[2].text == "ab" and cm.content[3].arguments.x == "yes" and cm.stop_reason == "tool_use")
check(cev, {{type="reasoning_delta", delta="é"}, {type="text_delta", delta="a"}, {type="text_delta", delta="b"},
 {type="tool_call_start", tool_call_id="id", name="f"}, {type="tool_call_delta", tool_call_id="id", delta='{"x":'}, {type="tool_call_delta", tool_call_id="id", delta='"yes"}'}})
local gev = {}
local g = GOOGLE.new_parser(sink(gev))
feed(g, {candidates={{content={parts={{text="a"}, {text="é"}, {thought=true, text="think", thoughtSignature="sig"}, {functionCall={id="id", name="f", args={x="yes"}}}}}, finishReason="STOP"}}, usageMetadata={candidatesTokenCount=3}})
local gm = g.finish_message()
assert(gm.content[1].text == "aé" and gm.content[2].type == "reasoning" and gm.content[2].text == "think" and gm.content[2].native["google-generate"].thought_signature == "sig")
assert(gm.content[3].arguments.x == "yes" and gm.stop_reason == "tool_use" and gm.usage.output == 3)
assert(mare.json.decode(mare.json.encode(gm)).content[1].text == "aé")
check(gev, {{type="text_delta", delta="a"}, {type="text_delta", delta="é"}, {type="reasoning_delta", delta="think"}, {type="tool_call_start", tool_call_id="id", name="f"}})
return true
"#).await;
        assert!(result);
        h.shutdown().await;
    }).await
}

/// `openai-responses` registers via the wire spec (`encode`, `decoder`, `errors`, `finalize`),
/// supports `on_unauthorized` HTTP 401 credential refresh + re-encode + single retry.
#[tokio::test]
async fn responses_wire_uses_responses_spec_and_on_unauthorized_refresh() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/openai/responses",
            vec![
                Resp::status(401, r#"{"error":{"message":"expired token","code":"invalid_api_key"}}"#),
                Resp::sse(responses_text("refreshed ok")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "retries": 0 },
                    "provider-openai": { "base_url": format!("{}/openai", server.base), "transport": "http" }
                }
            }),
        )
        .await;
        expose(&h);
        let (has_spec_fields, text, req_hook_count): (bool, String, i64) = eval(
            &h,
            r#"
local _, owner = LLM:wire("openai-responses")
local desc = assert(LLM:describe_wire("openai-responses"))
local has_spec = (owner == "provider-openai") and desc.route == "responses" and desc.path == "/responses"

local hook_count = 0
C.cordis:on("provider/request", function(w, next)
  hook_count = hook_count + 1
  return next(w)
end)

local key_val = "stale-key"
local p = assert(LLM:provider("openai"))
local orig_stream = p.stream
local ep_calls = 0
local refreshed = false
LLM:register_provider({
  provider = "unauth-resp",
  replace = true,
  route = "responses",
  is_configured = function() return true end,
  models = { { model_id = "gpt-test", name = "Test" } },
  on_unauthorized = function()
    if refreshed then return false end
    refreshed = true
    key_val = "fresh-key"
    return true
  end,
  stream = function(req, sink)
    return LLM:wire("openai-responses")(function()
      ep_calls = ep_calls + 1
      return {
        provider = "unauth-resp",
        label = "UnauthResp",
        url = p.endpoint and nil or (mare.config.plugins["provider-openai"].base_url .. "/responses"),
        key = key_val,
        auth = "bearer",
        transport = "http",
        on_unauthorized = function()
          if refreshed then return false end
          refreshed = true
          key_val = "fresh-key"
          return true
        end,
      }
    end, req, sink)
  end,
})
local msg = LLM:stream({ model = LLM:resolve("unauth-resp/gpt-test"), messages = {
  { role = "user", content = { { type = "text", text = "hi" } } }
}, no_fallback = true }, function() end)
return has_spec, (msg.content[1] and msg.content[1].text or tostring(msg.error)), hook_count
"#,
        )
        .await;
        assert!(has_spec_fields, "openai-responses must register as S5 spec");
        assert_eq!(text, "refreshed ok");
        assert_eq!(req_hook_count, 2, "401 + on_unauthorized retry must re-encode and fire provider/request twice");
        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].header("authorization"), Some("Bearer stale-key"));
        assert_eq!(reqs[1].header("authorization"), Some("Bearer fresh-key"));
        h.shutdown().await;
    })
    .await
}

/// The temporary 3-arg `register_wire(name, fn, describe)` adapter is removed, and
/// `provider-openai-compat` registers `openai-chat` as a wire spec usable over gateways.
#[tokio::test]
async fn rejects_three_arg_register_wire_and_registers_openai_chat_spec() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/gw/openai/v1/chat/completions",
            vec![Resp::sse(compat_events(&[compat_text("gateway chat ok"), compat_stop()]))],
        );
        let h = wire_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-opencode-go": {
                        "routes": { "chat": { "path": "/openai/v1", "wire": "openai-chat" } }
                    }
                }
            }),
        )
        .await;
        let (rejected_three_arg, chat_owner, gw_text): (bool, String, String) = eval(
            &h,
            r#"
local ok3 = pcall(LLM.register_wire, LLM, "legacy-three", function() end, { route = "chat" })
local _, owner = LLM:wire("openai-chat")
local p = LLM:provider("opencode-go")
p.model_routes = { ["*"] = "chat" }
local msg = LLM:stream({ model = LLM:resolve("opencode-go/crouton-alpha-free"),
  messages = { { role = "user", content = { { type = "text", text = "hi" } } } }, no_fallback = true }, function() end)
return (not ok3), tostring(owner), (msg.content[1] and msg.content[1].text or tostring(msg.error))
"#,
        )
        .await;
        assert!(rejected_three_arg, "3-argument register_wire must be rejected once S9 lands");
        assert_eq!(chat_owner, "provider-openai-compat");
        assert_eq!(gw_text, "gateway chat ok");
        assert_eq!(server.count("/gw/openai/v1/chat/completions"), 1);
        h.shutdown().await;
    })
    .await
}

/// Across all 4 built-in wires (`openai-responses`, `openai-chat`, `anthropic-messages`,
/// `google-generate`), `provider/request` fire count equals actual mock HTTP requests (normal + retry + 401 refresh).
#[tokio::test]
async fn provider_request_count_matches_mock_requests_across_all_four_wires() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let cmd_key_file = env.cwd.join("v8-cmd-key.txt");
        std::fs::write(&cmd_key_file, "cmd-key-1").unwrap();
        let cmd_script = format!("printf 'cmd-key-2' > '{}'; cat '{}'", cmd_key_file.display(), cmd_key_file.display());

        server.route(
            "/openai/responses",
            vec![
                Resp::status(500, r#"{"error":{"message":"transient"}}"#),
                Resp::sse(responses_text("resp-ok")),
            ],
        );
        server.route(
            "/compat/chat/completions",
            vec![
                Resp::status(401, r#"{"error":{"message":"rotated"}}"#),
                Resp::sse(compat_events(&[compat_text("chat-ok"), compat_stop()])),
            ],
        );
        server.route(
            "/anthropic/v1/messages",
            vec![
                Resp::status(500, r#"{"error":{"type":"api_error","message":"boom"}}"#),
                Resp::sse(fixture("anthropic-text.sse")),
            ],
        );
        server.route(
            "/google",
            vec![Resp::sse(fixture("google.sse"))],
        );

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "deepseek": {
                        "base_url": format!("{}/compat", server.base),
                        "api_key": { "cmd": ["sh", "-c", cmd_script], "refresh_interval_ms": 600000 }
                    }
                },
                "plugins": {
                    "llm": { "retries": 2, "retry_base_ms": 1, "retry_max_ms": 5 },
                    "provider-openai": { "base_url": format!("{}/openai", server.base), "transport": "http" },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) },
                    "provider-google": { "base_url": format!("{}/google", server.base) }
                }
            }),
        )
        .await;
        expose(&h);
        let counts: String = eval(
            &h,
            r#"
local counts = {}
C.cordis:on("provider/request", function(w, next)
  counts[w.provider] = (counts[w.provider] or 0) + 1
  return next(w)
end)
local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
local r1 = LLM:stream({ model = LLM:resolve("openai/gpt-6.1-terra"), messages = msgs, no_fallback = true }, function() end)
local r2 = LLM:stream({ model = LLM:resolve("deepseek/deepseek-chat"), messages = msgs, no_fallback = true }, function() end)
local r3 = LLM:stream({ model = LLM:resolve("anthropic/claude-opus-5-5"), messages = msgs, no_fallback = true }, function() end)
local r4 = LLM:stream({ model = LLM:resolve("google/gemini-2.5-pro"), messages = msgs, no_fallback = true }, function() end)
assert(r1.stop_reason ~= "error" and r2.stop_reason ~= "error" and r3.stop_reason ~= "error" and r4.stop_reason ~= "error")
return string.format("%d,%d,%d,%d", counts.openai or 0, counts.deepseek or 0, counts.anthropic or 0, counts.google or 0)
"#,
        )
        .await;
        assert_eq!(counts, "2,2,2,1", "provider/request count must equal mock requests on every wire");
        assert_eq!(server.count("/openai/responses"), 2);
        assert_eq!(server.count("/compat/chat/completions"), 2);
        assert_eq!(server.count("/anthropic/v1/messages"), 2);
        assert_eq!(server.count("/google"), 1);
        h.shutdown().await;
    })
    .await
}

/// Wire plugins declare token estimation hints (`estimate_hints`) in `llm:register_wire(spec)`:
/// built-in wires cap Anthropic `media_type`, Gemini `mimeType`, OpenAI `input_image` and
/// `encrypted_content`, custom wires register and dispose cleanly on unload, and malformed
/// `estimate_hints` are rejected by validation.
#[tokio::test]
async fn wire_estimate_hints_register_with_wire_and_dispose_on_unload() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = wire_host(&mut env, &server, json!({})).await;
        let (anthropic_tok, gemini_tok, enc_tok, custom_before, custom_after, custom_unloaded, bad_unknown, bad_cap): (
            i64,
            i64,
            i64,
            i64,
            i64,
            i64,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
            local blob = string.rep("A", 50000)
            local anthropic = { type = "image", source = { type = "base64", media_type = "image/png", data = blob } }
            local gemini = { inlineData = { mimeType = "image/png", data = blob } }
            local enc = { { type = "compaction", encrypted_content = blob } }
            local custom = { { custom_mime = "image/webp", data = blob }, { custom_opaque = blob } }
            local before = mare.text.tokens(custom)
            local fiber = C.cordis:use({
                name = "custom-hint-plugin",
                inject = { "llm" },
                apply = function(ctx)
                    ctx.llm:register_wire({
                        name = "custom-hint-wire",
                        route = "custom-hint",
                        path = "/custom",
                        auth = { "bearer" },
                        encode = function() return { method = "POST", url = "", headers = {}, body = {} } end,
                        decoder = function() return { event = function() end, finish = function() return { role = "assistant", content = {} } end } end,
                        estimate_hints = {
                            image_mime_keys = { "custom_mime" },
                            capped_fields = { custom_opaque = 1024 },
                        },
                    })
                end,
            })
            fiber:await():await()
            local after = mare.text.tokens(custom)
            fiber:dispose():await()
            local unloaded = mare.text.tokens(custom)

            local base_spec = {
                name = "bad-hint-wire",
                route = "bad-hint",
                path = "/bad",
                auth = { "bearer" },
                encode = function() return { method = "POST", url = "", headers = {}, body = {} } end,
                decoder = function() return { event = function() end, finish = function() return { role = "assistant", content = {} } end } end,
            }
            local s1 = table.copy(base_spec)
            s1.estimate_hints = { unknown_field = { "x" } }
            local ok_unknown = pcall(function() LLM:register_wire(s1) end)
            local s2 = table.copy(base_spec)
            s2.estimate_hints = { capped_fields = { opaque = -5 } }
            local ok_cap = pcall(function() LLM:register_wire(s2) end)

            return mare.text.tokens(anthropic), mare.text.tokens(gemini), mare.text.tokens(enc), before, after, unloaded, ok_unknown, ok_cap
            "#,
        )
        .await;
        assert!(anthropic_tok < 2500, "built-in anthropic wire must declare media_type hint: {anthropic_tok}");
        assert!(gemini_tok < 2500, "built-in google wire must declare mimeType hint: {gemini_tok}");
        assert!(enc_tok <= 1300, "built-in openai wire must declare encrypted_content hint: {enc_tok}");
        assert!(custom_before > 20000, "unregistered custom wire shape should not be capped: {custom_before}");
        assert!(custom_after < 2000, "registered custom wire estimate_hints must cap custom shapes: {custom_after}");
        assert!(
            custom_unloaded > 20000,
            "disposing custom wire fiber must remove its estimate_hints: {custom_unloaded}"
        );
        assert!(!bad_unknown, "register_wire must reject estimate_hints with unknown fields");
        assert!(!bad_cap, "register_wire must reject estimate_hints with negative capped_fields");
        h.shutdown().await;
    })
    .await
}
