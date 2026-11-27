//! Codex / provider model-catalogue parsing, merging, and response validation tests.

use super::*;

/// Self-authored: built-in static models merge server reasoning capabilities and defaults while
/// preserving curated static metadata, and revert to static defaults on logout.
#[tokio::test]
async fn builtin_model_merges_server_reasoning_capabilities_and_restores_on_logout() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![Resp::status(
                200,
                r#"{"models":[{
                    "slug":"gpt-6.1-terra",
                    "display_name":"Overridden Name",
                    "context_window":1,
                    "visibility":"list",
                    "supported_reasoning_levels":[{"effort":"low"},{"effort":"xhigh"}],
                    "default_reasoning_level":"xhigh",
                    "supports_reasoning_summary_parameter":false,
                    "support_verbosity":true,
                    "default_verbosity":"low"
                }]}"#,
            )],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let (orig_name, orig_ctx, orig_default_nil): (String, i64, bool) = eval(
            &h,
            r#"local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
               return m.name, m.context_window, m.default_effort == nil"#,
        )
        .await;
        assert!(orig_default_nil, "built-in gpt-6.1-terra starts with nil default_effort");

        let (listed_nil, name_after, ctx_after, efforts_after, default_after, summary_after, verb_after): (
            bool,
            String,
            i64,
            String,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai", force = true })
local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
return m.listed == nil, m.name, m.context_window, table.concat(m.efforts or {}, ","), tostring(m.default_effort), m.reasoning_summary == true, tostring(m.verbosity)
"#,
        )
        .await;
        assert!(listed_nil, "built-in model must remain static (listed == nil)");
        assert_eq!(name_after, orig_name, "curated name must be preserved");
        assert_eq!(ctx_after, orig_ctx, "curated context_window must be preserved");
        assert_eq!(efforts_after, "low,xhigh", "server efforts must be merged");
        assert_eq!(default_after, "xhigh", "server default_effort must be merged");
        assert!(!summary_after, "server reasoning_summary=false must be merged");
        assert_eq!(verb_after, "low", "server verbosity must be merged");

        let restored_default_nil: bool = eval(
            &h,
            r#"
LLM:logout("openai")
local m = assert(LLM:resolve("openai/gpt-6.1-terra"))
return m.default_effort == nil
"#,
        )
        .await;
        assert!(restored_default_nil, "logout must restore static default_effort (nil)");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: missing `supported_reasoning_levels` is treated as unknown (`nil`, enrichable by
/// models.dev and preserving static capabilities), whereas `[]` is unsupported (`false`).
#[tokio::test]
async fn codex_models_distinguishes_missing_vs_empty_supported_reasoning_levels() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![Resp::status(
                200,
                r#"{"models":[
                    {"slug":"gpt-missing-levels","display_name":"Missing Levels","visibility":"list"},
                    {"slug":"gpt-empty-levels","display_name":"Empty Levels","visibility":"list","supported_reasoning_levels":[]},
                    {"slug":"gpt-6.1-terra","visibility":"list"},
                    {"slug":"gpt-6.1-sol","visibility":"list","supported_reasoning_levels":[]}
                ]}"#,
            )],
        );
        server.route(
            "/models.dev.json",
            vec![Resp::status(
                200,
                r#"{"openai":{"models":{
                    "gpt-missing-levels":{"reasoning":true},
                    "gpt-empty-levels":{"reasoning":true}
                }}}"#,
            )],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0, "models_dev_url": format!("{}/models.dev.json", server.base) },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        let (entry_missing_nil, entry_empty_false, missing_enriched, empty_kept_false, terra_kept, sol_disabled): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
local cm = require("provider-openai.codex_models")
local e_miss = cm.entry({ slug = "x", visibility = "list" })
local e_empty = cm.entry({ slug = "y", visibility = "list", supported_reasoning_levels = {} })

mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai", force = true })
local m_miss = assert(LLM:resolve("openai/gpt-missing-levels"))
local m_empty = assert(LLM:resolve("openai/gpt-empty-levels"))
local m_terra = assert(LLM:resolve("openai/gpt-6.1-terra"))
local m_sol = assert(LLM:resolve("openai/gpt-6.1-sol"))
return e_miss.reasoning == nil and e_miss.reasoning_summary == nil,
  e_empty.reasoning == false and e_empty.reasoning_summary == false,
  m_miss.reasoning == true,
  m_empty.reasoning == false,
  m_terra.reasoning == true and m_terra.efforts ~= nil and #m_terra.efforts > 0,
  m_sol.reasoning == false and m_sol.efforts == nil
"#,
        )
        .await;
        assert!(entry_missing_nil, "missing supported_reasoning_levels must map to nil (unknown)");
        assert!(entry_empty_false, "empty supported_reasoning_levels must map to false (unsupported)");
        assert!(missing_enriched, "unknown reasoning must be enrichable by models.dev");
        assert!(empty_kept_false, "explicit unsupported reasoning must not be overridden by models.dev");
        assert!(terra_kept, "static model must keep curated reasoning when server omits levels");
        assert!(sol_disabled, "static model must disable reasoning when server sends empty levels");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: Anthropic, Google, and OpenAI-compat providers reject abnormal `/models` payloads
/// without clearing in-memory or on-disk model caches.
#[tokio::test]
async fn other_providers_reject_abnormal_model_responses_and_preserve_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-cached","display_name":"Claude Cached"}]}"#),
                Resp::status(200, r#"{"error":{"message":"overloaded"},"data":[{"id":"claude-bad"}]}"#),
                Resp::status(200, r#"{"data":{}}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-ok"},{"error":"bad"}]}"#),
                Resp::status(200, r#"{"data":[null]}"#),
                Resp::status(200, "not-json{"),
            ],
        );
        server.route(
            "/google/models",
            vec![
                Resp::status(
                    200,
                    r#"{"models":[{"name":"models/gemini-cached","displayName":"Gemini Cached","supportedGenerationMethods":["generateContent"]}]}"#,
                ),
                Resp::status(200, r#"{"error":"bad","models":[]}"#),
                Resp::status(200, r#"{"models":[null]}"#),
                Resp::status(
                    200,
                    r#"{"models":[{"name":"models/","supportedGenerationMethods":["generateContent"]}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"models":[{"name":"models/gemini-bad-methods","supportedGenerationMethods":"generateContent"}]}"#,
                ),
            ],
        );
        server.route(
            "/compat/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"deepseek-cached","name":"DeepSeek Cached"}]}"#),
                Resp::status(200, r#"{"data":"not-array"}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;
        let (a_ok, a_errs, a_kept, a_dbg, g_ok, g_errs, g_kept, d_ok, d_err, d_kept): (
            bool,
            bool,
            bool,
            String,
            bool,
            bool,
            bool,
            bool,
            String,
            bool,
        ) = eval(
            &h,
            r#"
local a1 = LLM:refresh_catalog({ name = "anthropic", force = true })
local a_ok = type(a1.anthropic) == "number" and LLM:resolve("anthropic/claude-cached") ~= nil
local a2 = LLM:refresh_catalog({ name = "anthropic", force = true })
local a3 = LLM:refresh_catalog({ name = "anthropic", force = true })
local a4 = LLM:refresh_catalog({ name = "anthropic", force = true })
local a5 = LLM:refresh_catalog({ name = "anthropic", force = true })
local a6 = LLM:refresh_catalog({ name = "anthropic", force = true })
local a_errs = tostring(a2.anthropic):find("invalid response", 1, true) ~= nil
  and tostring(a3.anthropic):find("invalid response", 1, true) ~= nil
  and tostring(a4.anthropic):find("invalid response", 1, true) ~= nil
  and tostring(a5.anthropic):find("invalid response", 1, true) ~= nil
  and tostring(a6.anthropic):find("invalid response", 1, true) ~= nil
local r_cached = LLM:resolve("anthropic/claude-cached")
local r_bad = LLM:resolve("anthropic/claude-bad")
local a_kept = (r_cached ~= nil and r_cached.listed == true) and (r_bad == nil or r_bad.listed == nil)
local a_dbg = mare.json.encode({ a1 = a1, a2 = a2, a3 = a3, a4 = a4, a5 = a5, a6 = a6 })

local g1 = LLM:refresh_catalog({ name = "google", force = true })
local rg1 = LLM:resolve("google/gemini-cached")
local g_ok = type(g1.google) == "number" and rg1 ~= nil and rg1.listed == true and rg1.name == "Gemini Cached"
local g2 = LLM:refresh_catalog({ name = "google", force = true })
local g3 = LLM:refresh_catalog({ name = "google", force = true })
local g4 = LLM:refresh_catalog({ name = "google", force = true })
local g5 = LLM:refresh_catalog({ name = "google", force = true })
local g_errs = tostring(g2.google):find("invalid response", 1, true) ~= nil
  and tostring(g3.google):find("invalid response", 1, true) ~= nil
  and tostring(g4.google):find("invalid response", 1, true) ~= nil
  and tostring(g5.google):find("invalid response", 1, true) ~= nil
local rg_kept = LLM:resolve("google/gemini-cached")
local rg_bad = LLM:resolve("google/gemini-bad-methods")
local g_kept = (rg_kept ~= nil and rg_kept.listed == true and rg_kept.name == "Gemini Cached")
  and (rg_bad == nil or rg_bad.listed == nil)

local d1 = LLM:refresh_catalog({ name = "deepseek", force = true })
local rd1 = LLM:resolve("deepseek/deepseek-cached")
local d_ok = type(d1.deepseek) == "number" and rd1 ~= nil and rd1.listed == true and rd1.name == "DeepSeek Cached"
local d2 = LLM:refresh_catalog({ name = "deepseek", force = true })
local rd_kept = LLM:resolve("deepseek/deepseek-cached")
local d_kept = rd_kept ~= nil and rd_kept.listed == true and rd_kept.name == "DeepSeek Cached"

return a_ok, a_errs, a_kept, a_dbg, g_ok, g_errs, g_kept, d_ok, tostring(d2.deepseek), d_kept
"#,
        )
        .await;
        assert!(a_ok && a_errs && a_kept, "anthropic must reject abnormal payloads and keep cache: ok={a_ok} errs={a_errs} kept={a_kept} dbg={a_dbg}");
        assert!(g_ok && g_errs && g_kept, "google must reject abnormal payloads and keep cache");
        assert!(d_ok && d_err.contains("invalid response") && d_kept, "deepseek compat must keep cache: {d_err}");

        let acache = std::fs::read_to_string(env.home.path().join("state/models-cache/anthropic.json")).unwrap();
        assert!(acache.contains("claude-cached") && !acache.contains("claude-bad"), "{acache}");
        let gcache = std::fs::read_to_string(env.home.path().join("state/models-cache/google.json")).unwrap();
        assert!(gcache.contains("gemini-cached"), "{gcache}");
        let dcache = std::fs::read_to_string(env.home.path().join("state/models-cache/deepseek.json")).unwrap();
        assert!(dcache.contains("deepseek-cached"), "{dcache}");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: `codex_models.origin` and `valid_base` normalize IPv6 literals to RFC 5952 form
/// so equivalent IPv6 encodings compare equal, while invalid IPv6 literals are rejected.
#[tokio::test]
async fn codex_models_normalizes_ipv6_origins_and_rejects_invalid_ipv6() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        let (eq_long, eq_loopback, eq_v4mapped, eq_short_ipv4, valid_loopback, reject_zone, reject_dbl): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
local cm = require("provider-openai.codex_models")
local eq_long = cm.origin("https://[2001:0DB8:0000:0000:0000:0000:0000:0001]:8443/codex")
  == cm.origin("https://[2001:db8::1]:8443/other")
local eq_loopback = cm.origin("http://[0:0:0:0:0:0:0:1]/codex") == cm.origin("http://[::1]/codex")
local eq_v4mapped = cm.origin("https://[::ffff:192.0.2.1]/codex") == cm.origin("https://[::ffff:c000:201]/codex")
local eq_short_ipv4 = cm.origin("https://127.1/codex") == cm.origin("https://127.0.0.1/other")
local valid_loopback = cm.valid_base("http://[0:0:0:0:0:0:0:1]:8080/codex/") == "http://[0:0:0:0:0:0:0:1]:8080/codex"
local reject_zone = cm.origin("http://[::1%eth0]/codex") == nil
local reject_dbl = cm.origin("http://[1::2::3]/codex") == nil
return eq_long, eq_loopback, eq_v4mapped, eq_short_ipv4, valid_loopback, reject_zone, reject_dbl
"#,
        )
        .await;
        assert!(eq_long, "expanded and compressed IPv6 origins must match");
        assert!(eq_loopback, "expanded and ::1 loopback IPv6 origins must match");
        assert!(eq_v4mapped, "IPv4-mapped IPv6 origins must match");
        assert!(eq_short_ipv4, "short IPv4 127.1 and 127.0.0.1 origins must match");
        assert!(valid_loopback, "expanded IPv6 loopback must pass valid_base");
        assert!(reject_zone, "IPv6 zone id must be rejected");
        assert!(reject_dbl, "multiple :: in IPv6 must be rejected");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: when `supported_reasoning_levels` is missing (`efforts == nil`)
/// but `default_reasoning_level` is present (e.g. `"ultra"`), `params` must preserve that default
/// rather than clamping it into `low/medium/high`.
#[tokio::test]
async fn codex_models_missing_efforts_preserves_custom_default_effort() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![Resp::status(
                200,
                r#"{"models":[
                    {"slug":"gpt-ultra-default","visibility":"list","default_reasoning_level":"ultra"}
                ]}"#,
            )],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let (efforts_nil, default_is_ultra, effective_ultra): (bool, bool, bool) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai", force = true })
local m = assert(LLM:resolve("openai/gpt-ultra-default"))
LLM:set_model(m.ref)
return m.efforts == nil, m.default_effort == "ultra", LLM:effort() == "ultra"
"#,
        )
        .await;
        assert!(
            efforts_nil && default_is_ultra && effective_ultra,
            "missing efforts with custom default_effort=ultra must preserve effective effort=ultra (got {efforts_nil}, {default_is_ultra}, {effective_ultra})"
        );
        h.shutdown().await;
    })
    .await
}
/// Self-authored: same-name built-in models must merge server fields
/// (`max_output`, `input`, `hidden`), preserve measured `context_window`, and not clamp a server
/// `default_effort` when `efforts` was omitted by the server.
#[tokio::test]
async fn builtin_model_merges_server_limits_and_unclamped_default_effort() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![Resp::status(
                200,
                r#"{"data":[{"id":"claude-sonnet-5-5","display_name":"Claude Sonnet 5.5","max_tokens":16384,"max_input_tokens":150000}]}"#,
            )],
        );
        server.route(
            "/codex/codex/models",
            vec![Resp::status(
                200,
                r#"{"models":[{
                    "slug":"gpt-6.1-terra",
                    "visibility":"hide",
                    "context_window":1,
                    "max_output":32768,
                    "input_modalities":["text"],
                    "default_reasoning_level":"ultra"
                }]}"#,
            )],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());

        let (a_max_out, a_ctx, t_ctx, t_max_out, t_hidden, t_input, t_default, t_effective): (
            i64,
            i64,
            i64,
            i64,
            bool,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
LLM:refresh_catalog({ force = true })
local ma = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
local mt = assert(LLM:resolve("openai/gpt-6.1-terra"))
LLM:set_model(mt.ref)
return ma.max_output, ma.context_window, mt.context_window, mt.max_output, mt.hidden == true,
  table.concat(mt.input or {}, ","), tostring(mt.default_effort), tostring(LLM:effort())
"#,
        )
        .await;
        assert_eq!(a_max_out, 16384, "anthropic built-in model must adopt server max_output");
        assert_eq!(a_ctx, 150000, "anthropic built-in model without measured flag must adopt server context_window");
        assert_eq!(t_ctx, 921000, "measured built-in context_window must be preserved");
        assert_eq!(t_max_out, 32768, "built-in model must adopt server max_output");
        assert!(t_hidden, "built-in model must adopt server visibility=hide");
        assert_eq!(t_input, "text", "built-in model must adopt server input_modalities");
        assert_eq!(t_default, "ultra", "server default_effort=ultra must be preserved");
        assert_eq!(t_effective, "ultra", "omitted efforts must not clamp server default_effort=ultra");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: missing `supported_reasoning_levels` with explicit
/// `supports_reasoning_summary_parameter=false` must disable `reasoning.summary` in requests, and
/// `supported_reasoning_levels=[]` with `default_reasoning_level="none"` must disable reasoning.
#[tokio::test]
async fn codex_models_reasoning_and_summary_combinations_affect_request() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![Resp::status(
                200,
                r#"{"models":[
                    {"slug":"gpt-6.1-terra","visibility":"list","supports_reasoning_summary_parameter":false},
                    {"slug":"gpt-6.1-sol","visibility":"list","supported_reasoning_levels":[],"default_reasoning_level":"none"}
                ]}"#,
            )],
        );
        server.route(
            "/codex/codex/responses",
            vec![
                Resp::sse(fixture("openai-responses.sse")),
                Resp::sse(fixture("openai-responses.sse")),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());

        let (terra_r, terra_sum, sol_r, sol_efforts_nil, sol_def_nil): (bool, bool, bool, bool, bool) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai", force = true })
local mt = assert(LLM:resolve("openai/gpt-6.1-terra"))
local ms = assert(LLM:resolve("openai/gpt-6.1-sol"))
LLM:stream({ model = mt, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
LLM:stream({ model = ms, messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
return mt.reasoning == true, mt.reasoning_summary == false,
  ms.reasoning == false, ms.efforts == nil, ms.default_effort == nil
"#,
        )
        .await;
        assert!(terra_r && terra_sum, "missing levels + summary=false must disable reasoning_summary");
        assert!(sol_r && sol_efforts_nil && sol_def_nil, "empty levels + default=none must disable reasoning");

        let reqs: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/codex/codex/responses")
            .collect();
        assert_eq!(reqs.len(), 2);
        let body_terra = reqs[0].json();
        assert!(
            body_terra.get("reasoning").and_then(|r| r.get("summary")).is_none(),
            "terra request must omit reasoning.summary when supports_reasoning_summary_parameter=false: {body_terra}"
        );
        let body_sol = reqs[1].json();
        assert!(
            body_sol.get("reasoning").is_none(),
            "sol request must omit reasoning when supported_reasoning_levels=[]: {body_sol}"
        );
        h.shutdown().await;
    })
    .await
}
/// Ref: reference implementation/Codex/Kimi Code 三家均无同类测试.
/// Self-authored: Google `supportedGenerationMethods` must validate on the
/// lossless JSON shape so `{}` (empty object) and `[null]` are rejected as invalid responses
/// rather than treated as empty arrays, preserving both in-memory and disk caches.
#[tokio::test]
async fn google_models_rejects_object_and_null_supported_generation_methods_preserving_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/google/models",
            vec![
                Resp::status(
                    200,
                    r#"{"models":[{"name":"models/gemini-cached","displayName":"Gemini Cached","supportedGenerationMethods":["generateContent"]}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"models":[{"name":"models/gemini-obj","supportedGenerationMethods":{}}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"models":[{"name":"models/gemini-null","supportedGenerationMethods":[null]}]}"#,
                ),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;

        let (cached_ok, obj_err, mem_after_obj, disk_after_obj, null_err, mem_after_null, disk_after_null): (
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, google = { key = "gk-test" } }), { mode = 384 })
local r1 = LLM:refresh_catalog({ name = "google", force = true })
local m1 = LLM:resolve("google/gemini-cached")

local r2 = LLM:refresh_catalog({ name = "google", force = true })
local m2 = LLM:resolve("google/gemini-cached")
local d2 = mare.fs.read(mare.paths.state .. "/models-cache/google.json") or ""

local r3 = LLM:refresh_catalog({ name = "google", force = true })
local m3 = LLM:resolve("google/gemini-cached")
local d3 = mare.fs.read(mare.paths.state .. "/models-cache/google.json") or ""

return (type(r1.google) == "number" and m1 ~= nil and m1.listed == true),
  type(r2.google) == "string",
  (m2 ~= nil and m2.listed == true),
  d2:find("gemini-cached", 1, true) ~= nil,
  type(r3.google) == "string",
  (m3 ~= nil and m3.listed == true),
  d3:find("gemini-cached", 1, true) ~= nil
"#,
        )
        .await;

        assert!(cached_ok, "initial Google refresh must cache gemini-cached");
        assert!(obj_err, "supportedGenerationMethods={{}} must fail as invalid response");
        assert!(mem_after_obj, "in-memory gemini-cached must be preserved after {{}} error");
        assert!(disk_after_obj, "disk cache gemini-cached must be preserved after {{}} error");
        assert!(null_err, "supportedGenerationMethods=[null] must fail as invalid response");
        assert!(mem_after_null, "in-memory gemini-cached must be preserved after [null] error");
        assert!(disk_after_null, "disk cache gemini-cached must be preserved after [null] error");
        h.shutdown().await;
    })
    .await
}

/// Empty `efforts = {}` overrides static efforts cleanly (distinct from missing `nil`),
/// and per-field provenance (`measured`, `server`, `static`, `models_dev`, `user`) is tracked for
/// `llm:model_sources` and `doctor`.
#[tokio::test]
async fn empty_efforts_overrides_static_and_model_sources_tracked_for_doctor() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![Resp::status(
                200,
                r#"{"models":[
                    {"slug":"gpt-6.1-terra","context_window":1,"max_output":32768,"visibility":"list","supported_reasoning_levels":[{"effort":"low"}]},
                    {"slug":"gpt-custom-empty","visibility":"list","supported_reasoning_levels":[]}
                ]}"#,
            )],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "models": {
                    "openai/gpt-6.1-terra": { "name": "User Terra Name" }
                },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;
        expose(&h);
        let (terra_name_src, terra_ctx_src, terra_eff_src, custom_empty_eff_ok, empty_static_override_ok, doctor_has_sources): (
            String,
            String,
            String,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai", force = true })
local terra_src = assert(LLM:model_sources("openai/gpt-6.1-terra"))

-- Verify empty `efforts = {}` overrides static `{ "low", "high" }` on a provider catalog merge.
LLM:register_provider({
  provider = "empty-eff-prov",
  route = "chat",
  stream = function() error("unused") end,
  is_configured = function() return true end,
  models = {
    { model_id = "m1", context_window = 64000, max_output = 4096, reasoning = true, efforts = { "low", "high" } },
  },
  catalog_target = function() return { url = "mem://empty-eff-prov" } end,
  catalog_scope = function() return "static" end,
  list_models = function()
    return { { id = "m1", reasoning = true, efforts = {} } }
  end,
})
LLM:refresh_models("empty-eff-prov")
local m1 = assert(LLM:resolve("empty-eff-prov/m1"))
local m1_src = assert(LLM:model_sources("empty-eff-prov/m1"))

assert(LLM:set_model("openai/gpt-6.1-terra", { save = false }))
local checks = {}
C.cordis:get("bus"):emit("kernel/doctor", checks)
local found_doc = false
for _, c in ipairs(checks) do
  if type(c.detail) == "string" and (c.detail:find("measured", 1, true) or c.name == "model sources") then
    found_doc = true
  end
end

return tostring(terra_src.name),
  tostring(terra_src.context_window),
  tostring(terra_src.efforts),
  (LLM:resolve("openai/gpt-custom-empty").reasoning == false),
  (type(m1.efforts) == "table" and #m1.efforts == 0 and m1_src.efforts == "server"),
  found_doc
"#,
        )
        .await;
        assert_eq!(terra_name_src, "user");
        assert_eq!(terra_ctx_src, "measured");
        assert_eq!(terra_eff_src, "server");
        assert!(custom_empty_eff_ok);
        assert!(empty_static_override_ok, "empty efforts={{}} from server must override static efforts");
        assert!(doctor_has_sources, "doctor checks must include model field sources");
        h.shutdown().await;
    })
    .await
}
