//! Golden wire bodies: the exact request body every provider sends for a fixed matrix of models
//! and reasoning efforts (the session effort clamped to the model, an explicit per-request
//! effort sent as it is, output caps and provider config). The model-parameters engine must
//! reproduce these bytes; `MARE_UPDATE_GOLDEN=1` rewrites the files (review the diff).

use std::fmt::Write as _;

use super::*;

/// Runs every case in order; each one sends exactly one request (no retries, no fallback).
/// A case: {ref, mode = "default" | "session" | "request" | "compact", effort?, max_tokens?}.
const DRIVER: &str = r#"
local cases = mare.json.decode(...)
local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }
for _, c in ipairs(cases) do
  local m = assert(LLM:resolve(c.ref), "unknown model " .. c.ref)
  local req = { model = m, system = "S", session_id = "sess-golden", messages = msgs,
    max_tokens = c.max_tokens, no_fallback = true }
  if c.mode == "session" then
    LLM:set_model(c.ref, { save = false })
    LLM:restore_selection({ effort = c.effort })
  elseif c.mode == "request" then
    req.effort = c.effort
  end
  if c.mode == "compact" then
    req.effort = c.effort
    LLM:compact_remote(req)
  else
    LLM:stream(req, function() end)
  end
end
"#;

fn case(r: &str, mode: &str, effort: Option<&str>, max_tokens: Option<i64>) -> Value {
    json!({ "ref": r, "mode": mode, "effort": effort, "max_tokens": max_tokens })
}

async fn golden_host(env: &mut TestEnv, server: &MockServer, extra: Value) -> Arc<Host> {
    env.set_var("OPENCODE_API_KEY", "test-opencode");
    env.set_var("OPENROUTER_API_KEY", "test-openrouter");
    env.set_var("MYRESP_KEY", "test-myresp");
    let base = &server.base;
    let ok_responses = || (0..400).map(|_| Resp::sse(responses_text("ok"))).collect::<Vec<_>>();
    server.route("/openai/responses/compact", vec![Resp::status(200, r#"{"output":[]}"#)]);
    server.route("/openai", ok_responses());
    server.route("/og", ok_responses());
    server.route("/myresp", ok_responses());
    server.route("/compat", ok_responses());
    server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
    server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
    let compat = Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]));
    for p in ["/openrouter", "/localre"] {
        server.route(p, vec![compat.clone()]);
    }
    let config = json!({
        "providers": {
            "myresp": { "wire_api": "responses", "base_url": format!("{base}/myresp"), "api_key_env": "MYRESP_KEY" },
            "openrouter": { "base_url": format!("{base}/openrouter") },
            "localre": { "base_url": format!("{base}/localre"), "no_key": true, "effort_param": "reasoning_effort",
                "models": [
                    { "model_id": "qwen3", "context_window": 32000, "max_output": 8000, "reasoning": true },
                    { "model_id": "qwen3-plain", "context_window": 32000, "max_output": 8000 },
                ] },
        },
        "plugins": {
            "llm": { "retries": 0 },
            "provider-opencode-go": { "base_url": base, "routes": { "responses": { "path": "/og" } } },
        },
    });
    let mut config = config;
    merge(&mut config, extra);
    provider_host(env, server, config).await
}

/// JSON objects merge recursively; anything else replaces.
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

/// Sends every case, then compares (or with `MARE_UPDATE_GOLDEN=1` writes) the golden file:
/// per case a `# label` line, the request path, the `headers` named (in that order; a secret
/// value is masked) and the raw body bytes.
async fn check(h: &Host, server: &MockServer, cases: &[Value], name: &str, headers: &[&str]) {
    let f = h.lua().load(DRIVER).into_function().unwrap();
    f.call_async::<()>(Value::Array(cases.to_vec()).to_string()).await.unwrap_or_else(|e| panic!("{name}: {e}"));
    let reqs = server.requests();
    assert_eq!(reqs.len(), cases.len(), "{name}: one request per case");
    let mut out = String::new();
    for (c, r) in cases.iter().zip(&reqs) {
        let label = [&c["ref"], &c["mode"], &c["effort"], &c["max_tokens"]]
            .iter()
            .filter(|v| !v.is_null())
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(out, "# {label}\nPOST {}", r.path);
        for name in headers {
            let shown = match (*name, r.header(name)) {
                (_, None) => "<absent>".to_string(),
                ("authorization", Some(v)) => v.split_once(' ').map_or("***".into(), |(s, _)| format!("{s} ***")),
                ("x-api-key", Some(_)) => "***".to_string(),
                (_, Some(v)) => v.to_string(),
            };
            let _ = writeln!(out, "{name}: {shown}");
        }
        let _ = writeln!(out, "{}\n", r.body);
    }
    assert!(!out.contains(&server.base), "{name}: no host-specific address in the golden file");
    assert!(!out.contains("test-opencode"), "{name}: no key in the golden file");
    let path = format!("{}/tests/fixtures/golden-wire/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    if std::env::var_os("MARE_UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, &out).unwrap();
        return;
    }
    let want =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e} (MARE_UPDATE_GOLDEN=1 writes it)"));
    if want != out {
        let (w, o): (Vec<&str>, Vec<&str>) = (want.lines().collect(), out.lines().collect());
        let at = w.iter().zip(&o).position(|(a, b)| a != b).unwrap_or(w.len().min(o.len()));
        panic!(
            "{name}: wire bodies differ from the golden file at line {}\n  golden: {}\n  now:    {}",
            at + 1,
            w.get(at).unwrap_or(&"<end>"),
            o.get(at).unwrap_or(&"<end>")
        );
    }
}

/// Default config: one case per wire-mapping style (default effort, explicit `off`/`high`, session
/// clamping, non-reasoning models, output caps, /responses/compact). Per-model level lists and
/// budget tables are catalog data and are not enumerated here.
#[tokio::test]
async fn wire_bodies_match_the_golden_file() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = golden_host(&mut env, &server, Value::Null).await;
        let mut cases = vec![];
        // One default case per wire-mapping style (plus a non-reasoning model).
        for r in [
            "openai/gpt-6.2-sol",
            "openai/gpt-6.1-terra",
            "openai/gpt-6-luna",
            "opencode-go/glm-5.3-flash",
            "myresp/some-model",
            "anthropic/claude-sonnet-5-5",
            "anthropic/claude-sonnet-4-5",
            "google/gemini-4-argon",
            "google/gemini-3.5-flash-lite",
            "deepseek/deepseek-flash",
            "openrouter/anthropic/claude-sonnet-5.5",
            "localre/qwen3",
            "localre/qwen3-plain",
        ] {
            cases.push(case(r, "default", None, None));
        }
        // How each style spells "off" (field removed / none / budget 0 / disabled), explicitly.
        for r in [
            "openai/gpt-6.1-terra",
            "opencode-go/glm-5.3-flash",
            "myresp/some-model",
            "anthropic/claude-sonnet-5-5",
            "anthropic/claude-sonnet-4-5",
            "google/gemini-4-argon",
            "google/gemini-3.5-flash-lite",
            "deepseek/deepseek-flash",
            "openrouter/anthropic/claude-sonnet-5.5",
            "localre/qwen3",
        ] {
            cases.push(case(r, "request", Some("off"), None));
        }
        // An explicit effort is sent as given, on a style of each kind.
        for r in [
            "openai/gpt-6.1-terra",
            "anthropic/claude-sonnet-5-5",
            "google/gemini-4-argon",
            "openrouter/anthropic/claude-sonnet-5.5",
            "localre/qwen3",
            "localre/qwen3-plain",
        ] {
            cases.push(case(r, "request", Some("high"), None));
        }
        // Fable 5.2 distinguishes native xhigh and max on Anthropic output_config.effort.
        cases.push(case("anthropic/claude-fable-5-2", "request", Some("xhigh"), None));
        cases.push(case("anthropic/claude-fable-5-2", "request", Some("max"), None));
        // The session effort is clamped to the model's declared levels (down, tie, explicit map).
        for (r, e) in [
            ("openai/gpt-6-luna", "max"),
            ("google/gemini-4-argon", "xhigh"),
            ("deepseek/deepseek-flash", "xhigh"),
            ("opencode-go/deepseek-v4.1-flash", "xhigh"),
            ("anthropic/claude-sonnet-4-5", "high"),
        ] {
            cases.push(case(r, "session", Some(e), None));
        }
        // Output caps: below, at and above the thinking budget (answer headroom).
        for (r, n) in [
            ("anthropic/claude-sonnet-4-5", 2000),
            ("anthropic/claude-sonnet-4-5", 30000),
            ("anthropic/claude-sonnet-5-5", 2000),
            ("openai/gpt-6.1-terra", 1000),
            ("google/gemini-3.5-flash-lite", 1000),
            ("deepseek/deepseek-flash", 1000),
        ] {
            cases.push(case(r, "request", Some("high"), Some(n)));
        }
        for e in [None, Some("off")] {
            cases.push(case("openai/gpt-6.1-terra", "compact", e, None));
        }
        check(&h, &server, &cases, "default", &[]).await;
        h.shutdown().await;
    })
    .await
}

/// Provider and model config that reach the wire: the configured model effort, the
/// Responses verbosity override, Anthropic's thinking budgets and answer headroom.
#[tokio::test]
async fn wire_bodies_with_provider_config_match_the_golden_file() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let extra = json!({
            "models": { "openai/gpt-6.1-terra": { "params": { "effort": "high" } } },
            "plugins": {
                "provider-openai": { "verbosity": "medium" },
                "provider-anthropic": {
                    "reasoning_budgets": { "minimal": 1100, "low": 1200, "medium": 3000, "high": 4000, "xhigh": 5000 },
                    "answer_headroom": 10,
                },
            },
        });
        let h = golden_host(&mut env, &server, extra).await;
        let cases = vec![
            case("openai/gpt-6.1-terra", "default", None, None),
            case("anthropic/claude-sonnet-4-5", "request", Some("medium"), Some(3500)),
            case("anthropic/claude-sonnet-4-5", "session", Some("off"), None),
        ];
        check(&h, &server, &cases, "config", &[]).await;
        h.shutdown().await;
    })
    .await
}

/// opencode Go over both routes: glm by a `model_routes` glob and deepseek by
/// `mare.model{route=}` on the Anthropic route (`/anthropic/v1/messages`, Bearer auth, the session
/// header, adaptive thinking with `output_config.effort`, off → disabled), crouton on the default
/// Responses route under the new gateway root. The Responses golden files above keep the legacy
/// the configured Responses route and stay byte-identical.
#[tokio::test]
async fn wire_bodies_on_the_opencode_go_routes_match_the_golden_file() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-opencode");
        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/gw/anthropic", (0..60).map(|_| Resp::sse(fixture("anthropic-text.sse"))).collect());
        server.route("/gw/openai", (0..60).map(|_| Resp::sse(responses_text("ok"))).collect());
        let config = json!({
            "models": { "opencode-go/deepseek-v4.1-flash": { "route": "anthropic" } },
            "plugins": {
                "llm": { "retries": 0 },
                "provider-opencode-go": { "base_url": format!("{base}/gw"), "model_routes": { "glm-*": "anthropic" } },
            },
        });
        let h = provider_host(&mut env, &server, config).await;
        let mut cases = vec![];
        for r in ["opencode-go/glm-5.3-flash", "opencode-go/deepseek-v4.1-flash", "opencode-go/gpt-6-luna"] {
            cases.push(case(r, "default", None, None));
        }
        cases.push(case("opencode-go/glm-5.3-flash", "request", Some("off"), None));
        cases.push(case("opencode-go/glm-5.3-flash", "request", Some("high"), Some(2000)));
        cases.push(case("opencode-go/deepseek-v4.1-flash", "request", Some("off"), None));
        cases.push(case("opencode-go/deepseek-v4.1-flash", "session", Some("xhigh"), None));
        let headers = ["authorization", "x-api-key", "anthropic-version", "accept", "x-opencode-session"];
        check(&h, &server, &cases, "opencode-go-routes", &headers).await;
        h.shutdown().await;
    })
    .await
}

/// New shipped presets (`kimi-code` and `lmstudio`): verify wire body shape, keyless local
/// execution for `lmstudio`, and `KIMI_API_KEY` + `kimi` dialect + `models_dev = "kimi-for-coding"`
/// for `kimi-code`.
#[tokio::test]
async fn wire_bodies_for_kimi_code_and_lmstudio_presets() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("KIMI_API_KEY", "test-kimi-code");
        let server = MockServer::start().await;
        let base = &server.base;
        let compat = Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]));
        server.route("/kimi-code", vec![compat.clone()]);
        server.route("/lmstudio", vec![compat]);

        let h = env
            .host()
            .config(json!({
                "providers": {
                    "kimi-code": { "base_url": format!("{base}/kimi-code") },
                    "lmstudio": {
                        "base_url": format!("{base}/lmstudio"),
                        "models": [{ "model_id": "qwen3", "context_window": 32000, "max_output": 4096 }]
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (kc_mdev, kc_def, lm_no_key): (String, String, bool) = eval(
            &h,
            r#"
            local kc = assert(LLM:provider("kimi-code"), "kimi-code preset missing")
            local lm = assert(LLM:provider("lmstudio"), "lmstudio preset missing")
            return tostring(kc.models_dev), tostring(kc.default_model), lm.no_key == true
            "#,
        )
        .await;
        assert_eq!(kc_mdev, "kimi-code-plan-cn");
        assert_eq!(kc_def, "kimi-for-coding");
        assert!(lm_no_key, "lmstudio must be keyless");

        let kc_msg = ask(&h, "kimi-code/kimi-for-coding").await;
        assert_eq!(text_of(&kc_msg), "ok");
        let lm_msg = ask(&h, "lmstudio/qwen3").await;
        assert_eq!(text_of(&lm_msg), "ok");

        let kc_req = server.requests().into_iter().find(|r| r.path == "/kimi-code/chat/completions").unwrap();
        assert_eq!(kc_req.header("authorization"), Some("Bearer test-kimi-code"));
        assert_eq!(kc_req.json()["model"], "kimi-for-coding");
        let lm_req = server.requests().into_iter().find(|r| r.path == "/lmstudio/chat/completions").unwrap();
        assert_eq!(lm_req.header("authorization"), None, "lmstudio must send no Authorization header");
        assert_eq!(lm_req.json()["model"], "qwen3");

        h.shutdown().await;
    })
    .await
}

fn anthropic_text_sse(text: &str) -> String {
    format!(
        "event: message_start\n\
         data: {{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_txt\",\"role\":\"assistant\",\"content\":[],\"usage\":{{\"input_tokens\":12,\"output_tokens\":1}}}}}}\n\n\
         event: content_block_start\n\
         data: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\n\
         data: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\n\
         event: content_block_stop\n\
         data: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: message_delta\n\
         data: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":5}}}}\n\n\
         event: message_stop\n\
         data: {{\"type\":\"message_stop\"}}\n\n",
        serde_json::to_string(text).unwrap()
    )
}

/// Pure-config `mare.provider(..., { wire_api = "anthropic" })` executes a full streaming agent
/// turn with a tool call, native `x-api-key` + `anthropic-version` headers (no dependency on
/// built-in `provider-anthropic` credentials), `/v1` base URL normalization, `no_key`, and model
/// catalog discovery.
#[tokio::test]
async fn custom_anthropic_data_provider_end_to_end_streaming_with_tool_call() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ANTHROPIC_API_KEY", "");
        std::fs::write(env.cwd.join("note.txt"), "hello from custom anthropic\n").unwrap();

        let server = MockServer::start().await;
        let base = &server.base;
        server.route(
            "/my-ant/v1/messages",
            vec![
                Resp::sse(request_shape::anthropic_tool_use_sse(
                    "toolu_custom_1",
                    "read",
                    r#"{"path":"note.txt"}"#,
                )),
                Resp::sse(anthropic_text_sse("The file says: hello from custom anthropic.")),
            ],
        );
        server.route(
            "/my-ant/v1/models",
            vec![Resp::status(
                200,
                r#"{"data":[{"id":"claude-listed","display_name":"Claude Listed","max_input_tokens":200000,"max_tokens":16000}]}"#,
            )],
        );
        server.route("/my-ant-v1/v1/messages", vec![Resp::sse(anthropic_text_sse("local-ant-ok"))]);

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
mare.setup {{ model = "my-claude/claude-custom" }}
mare.provider("my-claude", {{
  wire_api = "anthropic",
  base_url = "{base}/my-ant",
  api_key = "test-ant-custom-key",
  headers = {{ ["x-custom-tenant"] = "acme" }},
  default_model = "claude-custom",
  models = {{
    {{
      model_id = "claude-custom",
      name = "Claude Custom",
      context_window = 200000,
      max_output = 8192,
      reasoning = true,
      adaptive = true,
      efforts = {{ "off", "low", "medium", "high" }},
    }},
  }},
}})
mare.provider("my-claude-v1", {{
  wire_api = "anthropic",
  base_url = "{base}/my-ant-v1/v1/",
  no_key = true,
  list_models = false,
  models = {{
    {{ model_id = "claude-local", context_window = 64000, max_output = 4096 }},
  }},
}})
"#
            ),
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        expose(&h);

        exec(
            &h,
            r#"
            DELTAS = {}
            C.cordis:on("item/delta", function(e)
              if e.kind == "text" then table.insert(DELTAS, e.delta) end
            end)
            "#,
        )
        .await;

        let res = h.prompt("Read note.txt and report what it says.", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "custom anthropic turn failed: {res}");
        assert_eq!(res["text"], "The file says: hello from custom anthropic.", "{res}");
        let streamed: String = eval(&h, "return table.concat(DELTAS)").await;
        assert_eq!(streamed, "The file says: hello from custom anthropic.");

        // Verify /v1 suffix normalization and no_key on the second custom Anthropic endpoint.
        let local_msg = ask(&h, "my-claude-v1/claude-local").await;
        assert_eq!(text_of(&local_msg), "local-ant-ok");

        // Verify live catalog fetch over GET /v1/models with native x-api-key + anthropic-version.
        let listed_ok: bool = eval(
            &h,
            r#"
            LLM:refresh_models("my-claude")
            local m = LLM:resolve("my-claude/claude-listed")
            return m ~= nil and m.name == "Claude Listed"
            "#,
        )
        .await;
        assert!(listed_ok, "GET /v1/models must populate custom anthropic provider catalog");

        h.shutdown().await;

        let reqs = server.requests();
        let msg_reqs: Vec<_> = reqs.iter().filter(|r| r.path == "/my-ant/v1/messages").collect();
        assert_eq!(msg_reqs.len(), 2, "expected tool-call + final-answer requests: {reqs:#?}");
        for r in &msg_reqs {
            assert_eq!(r.header("x-api-key"), Some("test-ant-custom-key"));
            assert_eq!(r.header("anthropic-version"), Some("2023-06-01"));
            assert_eq!(r.header("x-custom-tenant"), Some("acme"));
            assert_eq!(r.header("authorization"), None, "anthropic wire must not send Bearer auth by default");
            assert_eq!(r.json()["model"], "claude-custom");
        }
        let body2 = msg_reqs[1].json().to_string();
        assert!(body2.contains("toolu_custom_1"), "second request must carry tool_use_id: {body2}");
        assert!(
            body2.contains("hello from custom anthropic"),
            "second request must carry read tool_result output: {body2}"
        );

        let local_req = reqs
            .iter()
            .find(|r| r.path == "/my-ant-v1/v1/messages")
            .expect("base_url ending in /v1/ must normalize to /v1/messages without duplicating /v1");
        assert_eq!(local_req.header("x-api-key"), None, "no_key=true must omit x-api-key");
        assert_eq!(local_req.header("authorization"), None, "no_key=true must omit authorization");
        assert_eq!(local_req.header("anthropic-version"), Some("2023-06-01"));

        let models_req = reqs
            .iter()
            .find(|r| r.path.starts_with("/my-ant/v1/models"))
            .expect("catalog refresh must hit /my-ant/v1/models");
        assert_eq!(models_req.header("x-api-key"), Some("test-ant-custom-key"));
        assert_eq!(models_req.header("anthropic-version"), Some("2023-06-01"));
    })
    .await
}

/// Pure-config `mare.provider` works end-to-end on `wire_api = "responses"` and
/// `wire_api = "google"` with each protocol's native auth header, custom headers, trailing-slash
/// base URL normalization, and `no_key = true`.
#[tokio::test]
async fn custom_responses_and_google_data_providers_end_to_end() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENAI_API_KEY", "");
        env.set_var("GEMINI_API_KEY", "");
        env.set_var("GOOGLE_API_KEY", "");

        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/my-resp/responses", vec![Resp::sse(responses_text("custom-resp-ok"))]);
        server.route(
            "/my-resp/models",
            vec![Resp::status(200, r#"{"data":[{"id":"resp-listed"}]}"#)],
        );
        server.route("/my-resp-nk/responses", vec![Resp::sse(responses_text("custom-resp-nk-ok"))]);
        server.route(
            "/my-resp-nk/models",
            vec![Resp::status(200, r#"{"data":[{"id":"resp-nk-listed"}]}"#)],
        );
        server.route(
            "/my-goog/v1beta/models/gem-m1:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse"))],
        );
        server.route(
            "/my-goog/v1beta/models",
            vec![Resp::status(
                200,
                r#"{"models":[{"name":"models/gem-listed","displayName":"Gemini Listed","inputTokenLimit":100000,"outputTokenLimit":8192,"supportedGenerationMethods":["generateContent"]}]}"#,
            )],
        );
        server.route(
            "/my-goog-nk/v1beta/models/gem-local:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse"))],
        );

        std::fs::create_dir_all(env.config_dir()).unwrap();
        let auth_path = env.config_dir().join("auth.json");
        std::fs::write(&auth_path, r#"{"version":1,"my-resp-nk":{"type":"api_key","key":"stale-resp-login-key"}}"#)
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
mare.provider("my-resp", {{
  wire_api = "responses",
  base_url = "{base}/my-resp/",
  api_key = "test-resp-custom-key",
  headers = {{ ["x-resp-hdr"] = "r1" }},
  models = {{ {{ model_id = "resp-m1", context_window = 128000, max_output = 8192 }} }},
}})
mare.provider("my-resp-nk", {{
  wire_api = "responses",
  base_url = "{base}/my-resp-nk",
  no_key = true,
  models = {{ {{ model_id = "resp-nk", context_window = 128000, max_output = 8192 }} }},
}})
mare.provider("my-gemini", {{
  wire_api = "google",
  base_url = "{base}/my-goog/v1beta/",
  api_key = "test-goog-custom-key",
  headers = {{ ["x-goog-hdr"] = "g1" }},
  models = {{ {{ model_id = "gem-m1", context_window = 1048576, max_output = 65536, reasoning = true }} }},
}})
mare.provider("my-gemini-nk", {{
  wire_api = "google",
  base_url = "{base}/my-goog-nk/v1beta",
  no_key = true,
  list_models = false,
  models = {{ {{ model_id = "gem-local", context_window = 128000, max_output = 8192 }} }},
}})
"#
            ),
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let resp_msg = ask(&h, "my-resp/resp-m1").await;
        assert_eq!(text_of(&resp_msg), "custom-resp-ok");

        let resp_nk_msg = ask(&h, "my-resp-nk/resp-nk").await;
        assert_eq!(text_of(&resp_nk_msg), "custom-resp-nk-ok");

        let goog_msg = stream(&h, "my-gemini/gem-m1").await;
        assert_fixture_message("my-gemini/gem-m1", &goog_msg, 0);

        let goog_nk_msg = stream(&h, "my-gemini-nk/gem-local").await;
        assert_fixture_message("my-gemini-nk/gem-local", &goog_nk_msg, 0);

        let (resp_listed_ok, resp_nk_listed_ok, goog_listed_ok): (bool, bool, bool) = eval(
            &h,
            r#"
            LLM:refresh_models("my-resp")
            local rm = LLM:resolve("my-resp/resp-listed")
            LLM:refresh_models("my-resp-nk")
            local rnm = LLM:resolve("my-resp-nk/resp-nk-listed")
            LLM:refresh_models("my-gemini")
            local gm = LLM:resolve("my-gemini/gem-listed")
            return (rm ~= nil and rm.listed == true),
              (rnm ~= nil and rnm.listed == true),
              (gm ~= nil and gm.name == "Gemini Listed")
            "#,
        )
        .await;
        assert!(resp_listed_ok, "GET /models must populate custom responses provider catalog");
        assert!(resp_nk_listed_ok, "no_key GET /models must populate custom responses provider catalog");
        assert!(goog_listed_ok, "GET /models must populate custom google provider catalog");

        h.shutdown().await;

        let reqs = server.requests();
        let resp_req = reqs
            .iter()
            .find(|r| r.path == "/my-resp/responses")
            .expect("trailing slash on responses base_url must normalize to /my-resp/responses");
        assert_eq!(resp_req.header("authorization"), Some("Bearer test-resp-custom-key"));
        assert_eq!(resp_req.header("x-resp-hdr"), Some("r1"));
        assert_eq!(resp_req.header("chatgpt-account-id"), None, "custom responses provider must not send Codex headers");

        let resp_models_req = reqs
            .iter()
            .find(|r| r.path == "/my-resp/models")
            .expect("catalog refresh must hit /my-resp/models");
        assert_eq!(resp_models_req.header("authorization"), Some("Bearer test-resp-custom-key"));

        let resp_nk_post = reqs
            .iter()
            .find(|r| r.path == "/my-resp-nk/responses")
            .expect("keyless responses POST must hit /my-resp-nk/responses");
        let resp_nk_get = reqs
            .iter()
            .find(|r| r.path == "/my-resp-nk/models")
            .expect("keyless responses GET must hit /my-resp-nk/models");
        for r in [resp_nk_post, resp_nk_get] {
            assert_eq!(r.header("authorization"), None, "no_key responses must omit authorization on {}", r.path);
            let serialized = format!("{:?}", r.headers);
            assert!(!serialized.contains("stale-resp-login-key"), "stale responses key leaked on {}", r.path);
        }

        let goog_req = reqs
            .iter()
            .find(|r| r.path == "/my-goog/v1beta/models/gem-m1:streamGenerateContent?alt=sse")
            .expect("google custom provider must hit streamGenerateContent?alt=sse");
        assert_eq!(goog_req.header("x-goog-api-key"), Some("test-goog-custom-key"));
        assert_eq!(goog_req.header("x-goog-hdr"), Some("g1"));
        assert_eq!(goog_req.header("authorization"), None);

        let goog_nk_req = reqs
            .iter()
            .find(|r| r.path == "/my-goog-nk/v1beta/models/gem-local:streamGenerateContent?alt=sse")
            .expect("keyless google custom provider must hit streamGenerateContent?alt=sse");
        assert_eq!(goog_nk_req.header("x-goog-api-key"), None, "no_key=true must omit x-goog-api-key");
        assert_eq!(goog_nk_req.header("authorization"), None);

        let goog_models_req = reqs
            .iter()
            .find(|r| r.path.starts_with("/my-goog/v1beta/models?"))
            .expect("catalog refresh must hit /my-goog/v1beta/models");
        assert_eq!(goog_models_req.header("x-goog-api-key"), Some("test-goog-custom-key"));
    })
    .await
}

/// `no_key = true` on data-tier Anthropic and Google providers must never fetch or transmit a
/// previously saved `/login` credential on either streaming POST or catalog GET requests.
#[tokio::test]
async fn no_key_custom_anthropic_and_google_never_send_saved_login_credentials() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ANTHROPIC_API_KEY", "");
        env.set_var("GEMINI_API_KEY", "");
        env.set_var("GOOGLE_API_KEY", "");

        let server = MockServer::start().await;
        let base = &server.base;
        server.route("/nk-ant/v1/messages", vec![Resp::sse(anthropic_text_sse("nk-ant-ok"))]);
        server.route(
            "/nk-ant/v1/models",
            vec![Resp::status(200, r#"{"data":[{"id":"claude-nk","display_name":"Claude NK"}]}"#)],
        );
        server.route(
            "/nk-goog/v1beta/models/gem-nk:streamGenerateContent",
            vec![Resp::sse(fixture("google.sse"))],
        );
        server.route(
            "/nk-goog/v1beta/models",
            vec![Resp::status(
                200,
                r#"{"models":[{"name":"models/gem-nk-listed","displayName":"Gem NK","supportedGenerationMethods":["generateContent"]}]}"#,
            )],
        );

        std::fs::create_dir_all(env.config_dir()).unwrap();
        let auth_path = env.config_dir().join("auth.json");
        std::fs::write(
            &auth_path,
            r#"{"version":1,"my-ant-nk":{"type":"api_key","key":"stale-ant-login-key"},"my-goog-nk":{"type":"api_key","key":"stale-goog-login-key"}}"#,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
mare.provider("my-ant-nk", {{
  wire_api = "anthropic",
  base_url = "{base}/nk-ant",
  no_key = true,
  models = {{ {{ model_id = "claude-nk", context_window = 64000, max_output = 4096 }} }},
}})
mare.provider("my-goog-nk", {{
  wire_api = "google",
  base_url = "{base}/nk-goog/v1beta",
  no_key = true,
  models = {{ {{ model_id = "gem-nk", context_window = 64000, max_output = 4096 }} }},
}})
"#
            ),
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        assert_eq!(text_of(&ask(&h, "my-ant-nk/claude-nk").await), "nk-ant-ok");
        assert_fixture_message("my-goog-nk/gem-nk", &stream(&h, "my-goog-nk/gem-nk").await, 0);

        let catalogs_ok: bool = eval(
            &h,
            r#"
            LLM:refresh_models("my-ant-nk")
            LLM:refresh_models("my-goog-nk")
            return LLM:resolve("my-ant-nk/claude-nk") ~= nil and LLM:resolve("my-goog-nk/gem-nk-listed") ~= nil
            "#,
        )
        .await;
        assert!(catalogs_ok, "no_key catalog refresh must succeed for both wires");

        h.shutdown().await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 4, "expected 2 POST + 2 GET requests: {reqs:#?}");
        for r in &reqs {
            assert_eq!(r.header("x-api-key"), None, "no_key leaked x-api-key on {}", r.path);
            assert_eq!(r.header("x-goog-api-key"), None, "no_key leaked x-goog-api-key on {}", r.path);
            assert_eq!(r.header("authorization"), None, "no_key leaked authorization on {}", r.path);
            let serialized = format!("{:?}", r.headers);
            assert!(!serialized.contains("stale-ant-login-key"), "stale anthropic key leaked on {}", r.path);
            assert!(!serialized.contains("stale-goog-login-key"), "stale google key leaked on {}", r.path);
        }
    })
    .await
}

/// Custom Anthropic, Google, and Responses catalog scope computation must stay static (never
/// firing extra `provider/request` hooks during scope checks) so a dynamic per-request hook
/// neither sees phantom GETs nor invalidates the freshly fetched catalog.
#[tokio::test]
async fn custom_catalog_target_is_static_without_extra_request_hook_calls() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = &server.base;
        server.route(
            "/static-ant/v1/models",
            vec![Resp::status(200, r#"{"data":[{"id":"ant-static-a","display_name":"Ant A"}]}"#)],
        );
        server.route(
            "/static-goog/v1beta/models",
            vec![Resp::status(
                200,
                r#"{"models":[{"name":"models/goog-static-a","displayName":"Goog A","supportedGenerationMethods":["generateContent"]}]}"#,
            )],
        );
        server.route(
            "/static-resp/models",
            vec![Resp::status(200, r#"{"data":[{"id":"resp-static-a"}]}"#)],
        );

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
mare.plugin("llm", {{ catalog_refresh_ms = 60000 }})
mare.provider("static-ant", {{
  wire_api = "anthropic",
  base_url = "{base}/static-ant",
  api_key = "shared-ant-key",
  headers = {{ ["x-tenant"] = "secret-hdr" }},
  query_params = {{ tenant = "secret-q" }},
}})
mare.provider("static-goog", {{
  wire_api = "google",
  base_url = "{base}/static-goog/v1beta",
  api_key = "shared-goog-key",
  headers = {{ ["x-tenant"] = "secret-hdr" }},
  query_params = {{ tenant = "secret-q" }},
}})
mare.provider("static-resp", {{
  wire_api = "responses",
  base_url = "{base}/static-resp",
  api_key = "shared-resp-key",
  headers = {{ ["x-tenant"] = "secret-hdr" }},
  query_params = {{ tenant = "secret-q" }},
}})
"#
            ),
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        expose(&h);

        let (hook_counts, all_listed, no_plaintext): (String, bool, bool) = eval(
            &h,
            r#"
            local counts = { ["static-ant"] = 0, ["static-goog"] = 0, ["static-resp"] = 0 }
            local seq = 0
            C.cordis:on("provider/request", function(wire)
              if counts[wire.provider] ~= nil then
                counts[wire.provider] = counts[wire.provider] + 1
                seq = seq + 1
                wire.headers = wire.headers or {}
                wire.headers["x-req-nonce"] = "nonce-" .. tostring(seq)
              end
              return wire
            end)

            local ok = true
            local clean = true
            for _, spec in ipairs({
              { "static-ant", "ant-static-a" },
              { "static-goog", "goog-static-a" },
              { "static-resp", "resp-static-a" },
            }) do
              local p, mid = spec[1], spec[2]
              LLM:refresh_catalog({ name = p })
              local m = LLM:resolve(p .. "/" .. mid)
              ok = ok and (m ~= nil and m.listed == true)
              local disk = assert(mare.fs.read(mare.paths.state .. "/models-cache/" .. p .. ".json"))
              if disk:find("secret-hdr", 1, true) ~= nil or disk:find("secret-q", 1, true) ~= nil then
                clean = false
              end
            end
            return ("%d,%d,%d"):format(counts["static-ant"], counts["static-goog"], counts["static-resp"]), ok, clean
            "#,
        )
        .await;
        assert_eq!(hook_counts, "1,1,1", "provider/request must fire once per real GET, never during scope calculation");
        assert!(all_listed, "dynamic provider/request nonce must not discard freshly fetched catalog");
        assert!(no_plaintext, "disk catalog cache must hash scope rather than storing plaintext config");

        h.shutdown().await;
    })
    .await
}

/// Custom Anthropic data providers honor `auth = "bearer"` (on POST and catalog GET),
/// `fine_grained_tool_streaming = false`, `replay_unsigned_reasoning` (with model-level explicit
/// `false` override), and explicit `tool_update = "addition_only"` (`defer_loading` + beta).
#[tokio::test]
async fn custom_anthropic_bearer_unsigned_replay_and_explicit_addition_only() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ANTHROPIC_API_KEY", "");
        std::fs::write(env.cwd.join("note.txt"), "bearer note").unwrap();

        let server = MockServer::start().await;
        let base = &server.base;
        let turn1_sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_b1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-bearer\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"unsigned plan\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b1\",\"name\":\"read\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"note.txt\\\"}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":8}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        let text_sse = |id: &str, text: &str| {
            format!(
                concat!(
                    "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"{id}\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-bearer\",\"content\":[],\"stop_reason\":null,\"usage\":{{\"input_tokens\":10,\"output_tokens\":1}}}}}}\n\n",
                    "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n",
                    "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n",
                    "event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n",
                    "event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":4}}}}\n\n",
                    "event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
                ),
                id = id,
                text = text
            )
        };
        server.route(
            "/bearer-ant/v1/messages",
            vec![
                Resp::sse(turn1_sse),
                Resp::sse(text_sse("msg_b2", "bearer-done")),
                Resp::sse(text_sse("msg_b3", "no-replay-done")),
            ],
        );
        server.route(
            "/bearer-ant/v1/models",
            vec![Resp::status(200, r#"{"data":[{"id":"claude-listed","display_name":"Bearer Listed"}]}"#)],
        );

        let plugin_dir = env.config_dir().join("plugins/deferred-probe");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("init.lua"),
            r#"return {
  name = "deferred-probe",
  inject = { "tools" },
  apply = function(ctx)
    ctx.tools:register("probe_deferred", {
      description = "Deferred probe tool",
      exposure = "deferred",
      side_effect = "read",
      parameters = { type = "object", properties = {} },
      execute = function() return { content = "ok" } end,
    })
  end,
}"#,
        )
        .unwrap();

        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
mare.setup {{ model = "bearer-ant/claude-bearer" }}
mare.provider("bearer-ant", {{
  wire_api = "anthropic",
  base_url = "{base}/bearer-ant",
  auth = "bearer",
  api_key = "test-bearer-tok",
  replay_unsigned_reasoning = true,
  fine_grained_tool_streaming = false,
  tool_update = "addition_only",
  models = {{
    {{ model_id = "claude-bearer", context_window = 200000, max_output = 8192, reasoning = true }},
    {{
      model_id = "claude-no-replay",
      context_window = 200000,
      max_output = 8192,
      reasoning = true,
      replay_unsigned_reasoning = false,
    }},
  }},
}})
"#
            ),
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let res = h.prompt("Read note.txt.", json!({})).await.unwrap();
        assert!(res["error"].is_null(), "bearer custom anthropic turn failed: {res}");
        assert_eq!(res["text"], "bearer-done", "{res}");

        let (listed_ok, no_replay_ok): (bool, bool) = eval(
            &h,
            r#"
            LLM:refresh_models("bearer-ant")
            local listed = LLM:resolve("bearer-ant/claude-listed")
            local m = assert(LLM:resolve("bearer-ant/claude-no-replay"))
            local out = LLM:stream({
              model = m,
              system = "S",
              messages = {
                { role = "user", content = { { type = "text", text = "hi" } } },
                {
                  role = "assistant",
                  content = {
                    { type = "reasoning", text = "should-not-replay" },
                    { type = "tool_call", id = "toolu_nr", name = "read", arguments = { path = "note.txt" } },
                  },
                },
                {
                  role = "tool_result",
                  tool_call_id = "toolu_nr",
                  tool_name = "read",
                  content = { { type = "text", text = "bearer note" } },
                },
              },
            })
            return (listed ~= nil and listed.name == "Bearer Listed"), (out.stop_reason == "stop")
            "#,
        )
        .await;
        assert!(listed_ok, "bearer catalog refresh must succeed");
        assert!(no_replay_ok, "model-level replay_unsigned_reasoning=false stream must succeed");

        h.shutdown().await;

        let reqs = server.requests();
        let posts: Vec<_> = reqs.iter().filter(|r| r.path == "/bearer-ant/v1/messages").collect();
        assert_eq!(posts.len(), 3, "expected 2 agent turns + 1 model override stream: {reqs:#?}");
        for r in &posts {
            assert_eq!(r.header("authorization"), Some("Bearer test-bearer-tok"));
            assert_eq!(r.header("x-api-key"), None, "auth=bearer must not send x-api-key");
        }
        let beta1 = posts[0].header("anthropic-beta").unwrap_or("");
        assert!(
            beta1.contains("advanced-tool-use-2025-11-20"),
            "explicit tool_update=addition_only must enable advanced-tool-use beta: {beta1:?}"
        );
        assert!(
            !beta1.contains("fine-grained-tool-streaming-2025-05-14"),
            "fine_grained_tool_streaming=false must omit fine-grained-tool-streaming beta: {beta1:?}"
        );
        let tools1 = posts[0].json()["tools"].as_array().cloned().unwrap_or_default();
        assert!(
            tools1.iter().any(|t| t["name"] == "probe_deferred" && t["defer_loading"] == true),
            "deferred tool must carry defer_loading=true: {tools1:?}"
        );

        let body2 = posts[1].json();
        let has_replayed_thinking = body2["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .any(|b| b["type"] == "thinking" && b["thinking"] == "unsigned plan");
        assert!(has_replayed_thinking, "replay_unsigned_reasoning=true must replay unsigned thinking: {body2}");

        let body3 = posts[2].json();
        let leaked_thinking = body3["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["role"] == "assistant")
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .any(|b| b["type"] == "thinking");
        assert!(!leaked_thinking, "explicit model replay_unsigned_reasoning=false must override provider true: {body3}");

        let models_req = reqs
            .iter()
            .find(|r| r.path.starts_with("/bearer-ant/v1/models"))
            .expect("catalog GET must hit /bearer-ant/v1/models");
        assert_eq!(models_req.header("authorization"), Some("Bearer test-bearer-tok"));
        assert_eq!(models_req.header("x-api-key"), None, "auth=bearer catalog GET must not send x-api-key");
    })
    .await
}

/// Provider-level `reasoning_budgets` and `reasoning_display` configured in `init.lua` must win
/// over the generic Anthropic route rules for both budget and adaptive models at startup.
#[tokio::test]
async fn custom_anthropic_init_provider_reasoning_budgets_and_display_win() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("ANTHROPIC_API_KEY", "");

        let server = MockServer::start().await;
        let base = &server.base;
        let ok_sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_r1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-budget\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":5,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        server.route("/tuned-ant/v1/messages", vec![Resp::sse(ok_sse), Resp::sse(ok_sse)]);

        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"
mare.provider("tuned-ant", {{
  wire_api = "anthropic",
  base_url = "{base}/tuned-ant",
  api_key = "test-tuned-key",
  list_models = false,
  reasoning_budgets = {{ medium = 3333 }},
  reasoning_display = "omitted",
  models = {{
    {{ model_id = "claude-budget", context_window = 200000, max_output = 16000, reasoning = true, adaptive = false }},
    {{ model_id = "claude-adaptive", context_window = 200000, max_output = 16000, reasoning = true, adaptive = true }},
  }},
}})
"#
            ),
        )
        .unwrap();

        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let ok: bool = eval(
            &h,
            r#"
            for _, ref in ipairs({ "tuned-ant/claude-budget", "tuned-ant/claude-adaptive" }) do
              local m = assert(LLM:resolve(ref))
              local res = LLM:stream({
                model = m,
                system = "S",
                effort = "medium",
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
              })
              assert(res.stop_reason == "stop", "unexpected stop_reason for " .. ref)
            end
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;

        let reqs: Vec<_> = server.requests().into_iter().filter(|r| r.path == "/tuned-ant/v1/messages").collect();
        assert_eq!(reqs.len(), 2, "expected budget + adaptive requests: {reqs:#?}");

        let budget_body = reqs[0].json();
        assert_eq!(budget_body["thinking"]["type"], "enabled", "{budget_body}");
        assert_eq!(budget_body["thinking"]["budget_tokens"], 3333, "provider reasoning_budgets must win: {budget_body}");
        assert_eq!(
            budget_body["thinking"]["display"], "omitted",
            "provider reasoning_display must apply to budget model: {budget_body}"
        );

        let adaptive_body = reqs[1].json();
        assert_eq!(adaptive_body["thinking"]["type"], "adaptive", "{adaptive_body}");
        assert_eq!(
            adaptive_body["thinking"]["display"], "omitted",
            "provider reasoning_display must win for adaptive model: {adaptive_body}"
        );
    })
    .await
}
