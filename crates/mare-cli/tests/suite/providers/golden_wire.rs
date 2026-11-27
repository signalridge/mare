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
    LLM:set_effort(c.effort, { save = false })
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
    server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
    server.route("/google", vec![Resp::sse(fixture("google.sse"))]);
    let compat = Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]));
    for p in ["/compat", "/openrouter", "/localre"] {
        server.route(p, vec![compat.clone()]);
    }
    let config = json!({
        "providers": {
            "myresp": { "wire_api": "responses", "base_url": format!("{base}/myresp"), "api_key_env": "MYRESP_KEY" },
            "openrouter": { "base_url": format!("{base}/openrouter") },
            "localre": { "base_url": format!("{base}/localre"), "no_key": true, "effort_param": "reasoning_effort",
                "models": [{ "model_id": "qwen3", "context_window": 32000, "max_output": 8000, "reasoning": true }] },
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
        // One default case per wire-mapping style (plus the two non-reasoning models).
        for r in [
            "openai/gpt-6.1-terra",
            "opencode-go/glm-5.3-flash",
            "myresp/some-model",
            "anthropic/claude-sonnet-5-5",
            "anthropic/claude-sonnet-4-5",
            "google/gemini-3-pro-preview",
            "google/gemini-2.5-pro",
            "deepseek/deepseek-reasoner",
            "openrouter/anthropic/claude-sonnet-5-5",
            "localre/qwen3",
            "deepseek/deepseek-chat",
            "openrouter/deepseek/deepseek-chat",
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
            "google/gemini-3-pro-preview",
            "google/gemini-2.5-pro",
            "google/gemini-2.5-flash",
            "openrouter/anthropic/claude-sonnet-5-5",
            "localre/qwen3",
        ] {
            cases.push(case(r, "request", Some("off"), None));
        }
        // An explicit effort is sent as given, on a style of each kind.
        for r in [
            "openai/gpt-6.1-terra",
            "anthropic/claude-sonnet-5-5",
            "google/gemini-3-pro-preview",
            "openrouter/anthropic/claude-sonnet-5-5",
            "localre/qwen3",
            "deepseek/deepseek-chat",
        ] {
            cases.push(case(r, "request", Some("high"), None));
        }
        // The session effort is clamped to the model's declared levels (down, tie, explicit map).
        for (r, e) in [
            ("openai/gpt-6-luna", "max"),
            ("google/gemini-3-pro-preview", "medium"),
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
            ("google/gemini-2.5-pro", 1000),
            ("deepseek/deepseek-reasoner", 1000),
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
        for r in ["opencode-go/glm-5.3-flash", "opencode-go/deepseek-v4.1-flash", "opencode-go/crouton-alpha-free"] {
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
        assert_eq!(kc_mdev, "kimi-for-coding");
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

        let goog_msg = stream(&h, "my-gemini/gem-m1").await;
        assert_fixture_message("my-gemini/gem-m1", &goog_msg, 0);

        let goog_nk_msg = stream(&h, "my-gemini-nk/gem-local").await;
        assert_fixture_message("my-gemini-nk/gem-local", &goog_nk_msg, 0);

        let listed_ok: bool = eval(
            &h,
            r#"
            LLM:refresh_models("my-gemini")
            local m = LLM:resolve("my-gemini/gem-listed")
            return m ~= nil and m.name == "Gemini Listed"
            "#,
        )
        .await;
        assert!(listed_ok, "GET /models must populate custom google provider catalog");

        h.shutdown().await;

        let reqs = server.requests();
        let resp_req = reqs
            .iter()
            .find(|r| r.path == "/my-resp/responses")
            .expect("trailing slash on responses base_url must normalize to /my-resp/responses");
        assert_eq!(resp_req.header("authorization"), Some("Bearer test-resp-custom-key"));
        assert_eq!(resp_req.header("x-resp-hdr"), Some("r1"));
        assert_eq!(resp_req.header("chatgpt-account-id"), None, "custom responses provider must not send Codex headers");

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
