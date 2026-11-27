//! Catalogue cache scoping, eviction, and request-identity isolation tests.

use super::*;

/// Ref: Codex `core/tests/suite/models_cache_auth.rs:109` & `model-provider/src/models_originator_tests.rs:191`
/// (cache isolated per auth account/source + stale in-flight refresh discarded on generation bump).
#[tokio::test]
async fn catalog_cache_isolates_auth_scopes_and_rejects_stale_refresh_generation() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(200, r#"{"models":[{"slug":"gpt-only-a","display_name":"GPT A","visibility":"list"}]}"#),
                Resp::status(200, r#"{"models":[{"slug":"gpt-only-b","display_name":"GPT B","visibility":"list"}]}"#),
                Resp::status(
                    200,
                    r#"{"models":[{"slug":"gpt-stale-a","display_name":"GPT Stale A","visibility":"list"}]}"#,
                )
                .hold(release.clone()),
            ],
        );
        server.route(
            "/openai/models",
            vec![Resp::status(200, r#"{"object":"list","data":[{"id":"gpt-api-key","name":"GPT API Key"}]}"#)],
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

        // 1. Account A caches gpt-only-a.
        let ok_a: bool = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-a", refresh = "ref-a", expires = 9999999999999, account_id = "acct-a" }
}), { mode = 384 })
C.cordis:get("bus"):emit("llm/login", { provider = "openai" })
LLM:refresh_catalog({ name = "openai", force = true })
return LLM:resolve("openai/gpt-only-a") ~= nil
"#,
        )
        .await;
        assert!(ok_a, "account A must see gpt-only-a");

        // 2. Switching to account B isolates away gpt-only-a and caches gpt-only-b.
        let (a_gone_in_b, ok_b): (bool, bool) = eval(
            &h,
            r#"
local function has_listed(ref)
  local m = LLM:resolve(ref)
  return m ~= nil and m.listed == true
end
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-b", refresh = "ref-b", expires = 9999999999999, account_id = "acct-b" }
}), { mode = 384 })
C.cordis:get("bus"):emit("llm/login", { provider = "openai" })
local before = not has_listed("openai/gpt-only-a")
LLM:refresh_catalog({ name = "openai", force = true })
return before, has_listed("openai/gpt-only-b") and not has_listed("openai/gpt-only-a")
"#,
        )
        .await;
        assert!(a_gone_in_b, "account A model must be cleared when switching to account B");
        assert!(ok_b, "account B must see only gpt-only-b");

        // 3. Logout falls back to API key scope; ChatGPT models are cleared and api-key model is cached.
        let (b_gone_on_api, ok_api): (bool, bool) = eval(
            &h,
            r#"
local function has_listed(ref)
  local m = LLM:resolve(ref)
  return m ~= nil and m.listed == true
end
LLM:logout("openai")
local before = not has_listed("openai/gpt-only-b")
LLM:refresh_catalog({ name = "openai", force = true })
return before, has_listed("openai/gpt-api-key") and not has_listed("openai/gpt-only-b")
"#,
        )
        .await;
        assert!(b_gone_on_api, "account B model must be cleared on logout");
        assert!(ok_api, "api_key scope must see gpt-api-key");

        // 4. Switching back to account A restores gpt-only-a from its isolated disk scope without HTTP.
        let count_before = server.count("/codex/codex/models");
        let restored_a: bool = eval(
            &h,
            r#"
local function has_listed(ref)
  local m = LLM:resolve(ref)
  return m ~= nil and m.listed == true
end
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-a", refresh = "ref-a", expires = 9999999999999, account_id = "acct-a" }
}), { mode = 384 })
C.cordis:get("bus"):emit("llm/login", { provider = "openai" })
return has_listed("openai/gpt-only-a") and not has_listed("openai/gpt-api-key")
"#,
        )
        .await;
        assert!(restored_a, "switching back to account A must restore its isolated cached model");

        // 5. In-flight refresh for account A is discarded if logout bumps refresh_gen before response arrives.
        let h_bg = h.clone();
        let bg = tokio::task::spawn_local(async move {
            exec(&h_bg, r#"LLM:refresh_models("openai", { force = true })"#).await;
        });
        wait_until(10, "held /codex/models request", || {
            let server = &server;
            let want = count_before + 1;
            async move { server.count("/codex/codex/models") >= want }
        })
        .await;
        exec(&h, r#"LLM:logout("openai")"#).await;
        release.notify_one();
        bg.await.unwrap();

        let stale_absent: bool = eval(&h, r#"local m = LLM:resolve("openai/gpt-stale-a"); return m == nil or m.listed == nil"#).await;
        assert!(stale_absent, "stale in-flight refresh must not register models after logout");
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/openai.json")).unwrap();
        assert!(!cache.contains("gpt-stale-a"), "stale in-flight refresh must not write to cache: {cache}");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: two API keys with identical first 3 and last 4 characters must not
/// collide in `catalog_scope`, and raw keys must not be written plaintext into `models-cache/*.json`.
#[tokio::test]
async fn catalog_cache_separates_keys_with_same_prefix_and_suffix() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-key-a","display_name":"Key A"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-key-b","display_name":"Key B"}]}"#),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let key_a = "sk-ant-key-AAAA-1234";
        let key_b = "sk-ant-key-BBBB-1234";
        let (has_a, has_b_only, no_a_under_b, restored_a): (bool, bool, bool, bool) = eval(
            &h,
            &format!(
                r#"
local function has_listed(ref)
  local m = LLM:resolve(ref)
  return m ~= nil and m.listed == true
end
mare.fs.write(mare.paths.auth, mare.json.encode({{ version = 1, anthropic = {{ key = "{key_a}" }} }}), {{ mode = 384 }})
C.cordis:get("bus"):emit("llm/login", {{ provider = "anthropic" }})
LLM:refresh_catalog({{ name = "anthropic", force = true }})
local has_a = has_listed("anthropic/claude-key-a")

mare.fs.write(mare.paths.auth, mare.json.encode({{ version = 1, anthropic = {{ key = "{key_b}" }} }}), {{ mode = 384 }})
C.cordis:get("bus"):emit("llm/login", {{ provider = "anthropic" }})
LLM:refresh_catalog({{ name = "anthropic", force = true }})
local has_b_only = has_listed("anthropic/claude-key-b")
local no_a_under_b = not has_listed("anthropic/claude-key-a")

mare.fs.write(mare.paths.auth, mare.json.encode({{ version = 1, anthropic = {{ key = "{key_a}" }} }}), {{ mode = 384 }})
C.cordis:get("bus"):emit("llm/login", {{ provider = "anthropic" }})
local restored_a = has_listed("anthropic/claude-key-a")
return has_a, has_b_only, no_a_under_b, restored_a
"#
            ),
        )
        .await;
        assert!(
            has_a && has_b_only && no_a_under_b && restored_a,
            "keys sharing prefix/suffix must isolate cache scopes (has_a={has_a}, has_b_only={has_b_only}, no_a_under_b={no_a_under_b}, restored_a={restored_a})"
        );
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/anthropic.json")).unwrap();
        assert!(!cache.contains(key_a) && !cache.contains(key_b), "cache must not store raw keys: {cache}");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: rotating the output of an `api_key = { cmd =... }` helper
/// must isolate `catalog_scope` by an irreversible fingerprint of the command-output credential,
/// rather than hashing only the command itself.
#[tokio::test]
async fn catalog_cache_isolates_cmd_credential_rotation() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-cmd-a","display_name":"Cmd A"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-cmd-b","display_name":"Cmd B"}]}"#),
            ],
        );
        let key_file = env.cwd.join("cmd-api-key");
        let key_a = "sk-ant-cmd-AAAA-9999";
        let key_b = "sk-ant-cmd-BBBB-9999";
        std::fs::write(&key_file, key_a).unwrap();
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "anthropic": {
                        "api_key": { "cmd": ["cat", key_file.to_string_lossy()], "refresh_interval_ms": 600000 }
                    }
                },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }),
        )
        .await;
        global(&h, "C", mare_kernel::test_ctx(&h.kernel).unwrap());
        let key_file_lua = key_file.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
        let (has_a, has_b_only, no_a_under_b, restored_a, no_b_under_a): (bool, bool, bool, bool, bool) = eval(
            &h,
            &format!(
                r#"
local function has_listed(ref)
  local m = LLM:resolve(ref)
  return m ~= nil and m.listed == true
end
LLM:refresh_catalog({{ name = "anthropic", force = true }})
local has_a = has_listed("anthropic/claude-cmd-a")

mare.fs.write("{key_file_lua}", "{key_b}", {{ mode = 384 }})
LLM:invalidate_api_key("anthropic")
C.cordis:get("bus"):emit("llm/login", {{ provider = "anthropic" }})
LLM:refresh_catalog({{ name = "anthropic", force = true }})
local has_b_only = has_listed("anthropic/claude-cmd-b")
local no_a_under_b = not has_listed("anthropic/claude-cmd-a")

mare.fs.write("{key_file_lua}", "{key_a}", {{ mode = 384 }})
LLM:invalidate_api_key("anthropic")
LLM:api_key("anthropic")
C.cordis:get("bus"):emit("llm/login", {{ provider = "anthropic" }})
local restored_a = has_listed("anthropic/claude-cmd-a")
local no_b_under_a = not has_listed("anthropic/claude-cmd-b")
return has_a, has_b_only, no_a_under_b, restored_a, no_b_under_a
"#
            ),
        )
        .await;
        assert!(
            has_a && has_b_only && no_a_under_b && restored_a && no_b_under_a,
            "cmd credential rotation must isolate cache scopes (has_a={has_a}, has_b_only={has_b_only}, no_a_under_b={no_a_under_b}, restored_a={restored_a}, no_b_under_a={no_b_under_a})"
        );
        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/anthropic.json")).unwrap();
        assert!(!cache.contains(key_a) && !cache.contains(key_b), "cache must not store raw cmd keys: {cache}");
        h.shutdown().await;
    })
    .await
}
/// Self-authored: when `catalog_scope` changes between `refresh_catalog`
/// calls (without `llm/login` or `llm/logout`), the in-memory catalog for that provider must be
/// cleared and rebuilt from the new scope's cache/listing, and `llm/providers-changed` must fire.
#[tokio::test]
async fn refresh_catalog_rebuilds_in_memory_on_scope_change() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-acct-a","display_name":"Acct A"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-acct-b","display_name":"Acct B"}]}"#),
                Resp::status(500, r#"{"error":{"message":"unavailable"}}"#),
            ],
        );
        let probe_dir = env.config_dir().join("plugins/scope-probe");
        std::fs::create_dir_all(&probe_dir).unwrap();
        std::fs::write(
            probe_dir.join("init.lua"),
            r#"return {
                name = "scope-probe",
                provide = { "scope_probe" },
                contracts = {
                    scope_probe = {
                        methods = {
                            count = {
                                doc = "count",
                                expose = "lua",
                                params = { type = "object", properties = {}, additionalProperties = false },
                                order = {},
                                result = { type = "integer" },
                            },
                        },
                    },
                },
                apply = function(ctx)
                    local count = 0
                    ctx:on("llm/providers-changed", function() count = count + 1 end)
                    local Svc = require("cordis").class("ScopeProbe", mare.Service)
                    function Svc:constructor(c)
                        Svc.super.constructor(self, c, "scope_probe")
                    end
                    function Svc:count()
                        return count
                    end
                    Svc.new(ctx)
                end,
            }"#,
        )
        .unwrap();
        let h = env
            .host()
            .user_plugins()
            .config(json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 3600000 },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let _: bool = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-ant-acct-a" } }), { mode = 384 })
LLM:refresh_catalog({ name = "anthropic", force = true })
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-ant-acct-b" } }), { mode = 384 })
LLM:refresh_catalog({ name = "anthropic", force = true })
return true
"#,
        )
        .await;
        let before_switch_back: i64 = h
            .call_service("scope_probe", "count", mlua::MultiValue::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            .and_then(|v| v.as_integer())
            .unwrap();
        let (has_a_on_switch, no_b_on_switch): (bool, bool) = eval(
            &h,
            r#"
local function has_listed(ref)
  local m = LLM:resolve(ref)
  return m ~= nil and m.listed == true
end
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-ant-acct-a" } }), { mode = 384 })
LLM:refresh_catalog({ name = "anthropic" })
return has_listed("anthropic/claude-acct-a"), not has_listed("anthropic/claude-acct-b")
"#,
        )
        .await;
        let after_switch_back: i64 = h
            .call_service("scope_probe", "count", mlua::MultiValue::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            .and_then(|v| v.as_integer())
            .unwrap();
        assert!(
            has_a_on_switch && no_b_on_switch && after_switch_back > before_switch_back,
            "switching to cached account A on refresh_catalog must rebuild in-memory catalog and emit llm/providers-changed (has_a={has_a_on_switch}, no_b={no_b_on_switch}, events={before_switch_back}->{after_switch_back})"
        );
        let no_a_on_failed_c: bool = eval(
            &h,
            r#"
local function has_listed(ref)
  local m = LLM:resolve(ref)
  return m ~= nil and m.listed == true
end
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-ant-acct-c" } }), { mode = 384 })
LLM:refresh_catalog({ name = "anthropic" })
return not has_listed("anthropic/claude-acct-a")
"#,
        )
        .await;
        let after_failed_c: i64 = h
            .call_service("scope_probe", "count", mlua::MultiValue::new())
            .await
            .unwrap()
            .into_iter()
            .next()
            .and_then(|v| v.as_integer())
            .unwrap();
        assert!(
            no_a_on_failed_c && after_failed_c > after_switch_back,
            "switching to uncached account C whose listing fails must still clear old-account models and emit llm/providers-changed (no_a={no_a_on_failed_c}, events={after_switch_back}->{after_failed_c})"
        );
        h.shutdown().await;
    })
    .await
}
/// Self-authored: catalog scope must include the normalized catalog
/// endpoint URL and affecting parameters so changing gateway `base_url` with the same key does not
/// reuse stale cached models.
#[tokio::test]
async fn catalog_scope_isolates_base_url_and_parameters() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/gw-a/v1/models",
            vec![Resp::status(200, r#"{"data":[{"id":"claude-gw-a","display_name":"Gateway A"}]}"#)],
        );
        server.route(
            "/gw-b/v1/models",
            vec![Resp::status(200, r#"{"data":[{"id":"claude-gw-b","display_name":"Gateway B"}]}"#)],
        );
        server.route(
            "/codex-a/codex/models",
            vec![Resp::status(200, r#"{"models":[{"slug":"gpt-codex-a","visibility":"list"}]}"#)],
        );
        server.route(
            "/codex-b/codex/models",
            vec![Resp::status(200, r#"{"models":[{"slug":"gpt-codex-b","visibility":"list"}]}"#)],
        );
        env.set_var("ANTHROPIC_API_KEY", "sk-ant-same");
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 86400000 },
                    "provider-anthropic": { "base_url": format!("{}/gw-a", server.base) },
                    "provider-openai": { "codex_base_url": format!("{}/codex-a", server.base) }
                }
            }),
        )
        .await;

        let (a_listed, codex_a_listed, dbg_a): (bool, bool, String) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })
local ra = LLM:refresh_catalog({ name = "anthropic" })
local ro = LLM:refresh_catalog({ name = "openai" })
local ma = LLM:resolve("anthropic/claude-gw-a")
local mo = LLM:resolve("openai/gpt-codex-a")
return (ma ~= nil and ma.listed == true), (mo ~= nil and mo.listed == true),
  mare.json.encode({ ra = ra, ro = ro })
"#,
        )
        .await;
        assert!(a_listed && codex_a_listed, "initial gateways must populate cache: {dbg_a}");
        h.shutdown().await;

        // Switch both gateway URLs while keeping the exact same credentials and long refresh_ms.
        let h2 = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 86400000 },
                    "provider-anthropic": { "base_url": format!("{}/gw-b", server.base) },
                    "provider-openai": { "codex_base_url": format!("{}/codex-b", server.base) }
                }
            }),
        )
        .await;
        let (b_listed, a_gone, codex_b_listed, codex_a_gone): (bool, bool, bool, bool) = eval(
            &h2,
            r#"
local ma_before = LLM:resolve("anthropic/claude-gw-a")
local mo_before = LLM:resolve("openai/gpt-codex-a")
LLM:refresh_catalog({ name = "anthropic" })
LLM:refresh_catalog({ name = "openai" })
local mb = LLM:resolve("anthropic/claude-gw-b")
local ma = LLM:resolve("anthropic/claude-gw-a")
local mob = LLM:resolve("openai/gpt-codex-b")
local moa = LLM:resolve("openai/gpt-codex-a")
return (mb ~= nil and mb.listed == true),
  (ma_before == nil or ma_before.listed == nil) and (ma == nil or ma.listed == nil),
  (mob ~= nil and mob.listed == true),
  (mo_before == nil or mo_before.listed == nil) and (moa == nil or moa.listed == nil)
"#,
        )
        .await;
        assert!(
            b_listed && a_gone && codex_b_listed && codex_a_gone,
            "changing gateway URL with same credentials must isolate scope and refresh (b={b_listed}, a_gone={a_gone}, codex_b={codex_b_listed}, codex_a_gone={codex_a_gone})"
        );
        h2.shutdown().await;
    })
    .await
}
/// Self-authored: rotating credentials must not grow the per-provider catalog cache
/// file without bound; old scopes are evicted while the current scope is kept.
#[tokio::test]
async fn catalog_cache_evicts_old_credential_scopes() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-r1"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-r2"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-r3"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-r4"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-r5"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-r6"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-r7"}]}"#),
            ],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;

        let ok_latest: bool = eval(
            &h,
            r#"
for i = 1, 7 do
  mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1, anthropic = { key = "sk-rot-" .. i } }), { mode = 384 })
  LLM:refresh_catalog({ name = "anthropic", force = true })
end
local m = LLM:resolve("anthropic/claude-r7")
return m ~= nil and m.listed == true
"#,
        )
        .await;
        assert!(ok_latest);
        let raw = std::fs::read_to_string(env.home.path().join("state/models-cache/anthropic.json")).unwrap();
        let j: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let scopes = j["scopes"].as_object().unwrap();
        assert!(scopes.len() <= 4, "rotating credentials must evict old scopes (got {} scopes): {raw}", scopes.len());
        assert!(raw.contains("claude-r7"), "latest scope must be kept: {raw}");
        h.shutdown().await;
    })
    .await
}
/// Ref: Codex `codex-rs/model-provider/src/models_endpoint_tests.rs:516` (catalog URL query params).
/// Self-authored: OpenAI OAuth `catalog_target` must include effective
/// `provider/request` headers, query parameters, and their dynamically resolved env values
/// (without storing raw secrets in plaintext) so changing them invalidates the cache.
#[tokio::test]
async fn openai_oauth_catalog_cache_respects_provider_request_headers_and_query_params() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(200, r#"{"models":[{"slug":"gpt-tenant-a","visibility":"list"}]}"#),
                Resp::status(200, r#"{"models":[{"slug":"gpt-tenant-b","visibility":"list"}]}"#),
                Resp::status(200, r#"{"models":[{"slug":"gpt-tenant-c","visibility":"list"}]}"#),
            ],
        );
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.provider("openai", { headers = { ["x-tenant"] = "MARE_TEST_TENANT_HDR" }, query_params = { tenant = "MARE_TEST_TENANT_Q" } })"#,
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .config(json!({
                "providers": { "deepseek": { "base_url": format!("{}/compat", server.base) } },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 60000, "retry_base_ms": 1 },
                    "provider-openai": {
                        "base_url": format!("{}/openai", server.base),
                        "codex_base_url": format!("{}/codex", server.base)
                    },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) },
                    "provider-google": { "base_url": format!("{}/google", server.base) }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (has_a, cache_hit_a, has_b, no_a_after_b, has_c, no_b_after_c, no_raw_secret): (
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
local env_map = {
  MARE_TEST_TENANT_HDR = "hdr-secret-a",
  MARE_TEST_TENANT_Q = "q-secret-a",
}
mare.env = function(k) return env_map[k] end
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })

LLM:refresh_catalog({ name = "openai" })
local m_a1 = LLM:resolve("openai/gpt-tenant-a")

-- Same params -> cache hit (no new HTTP request)
LLM:refresh_catalog({ name = "openai" })
local m_a2 = LLM:resolve("openai/gpt-tenant-a")

-- Change resolved query param value -> must miss cache and fetch tenant-b
env_map.MARE_TEST_TENANT_Q = "q-secret-b"
LLM:refresh_catalog({ name = "openai" })
local m_b = LLM:resolve("openai/gpt-tenant-b")
local m_a_after_b = LLM:resolve("openai/gpt-tenant-a")

-- Change resolved header value -> must miss cache and fetch tenant-c
env_map.MARE_TEST_TENANT_HDR = "hdr-secret-c"
LLM:refresh_catalog({ name = "openai" })
local m_c = LLM:resolve("openai/gpt-tenant-c")
local m_b_after_c = LLM:resolve("openai/gpt-tenant-b")

local disk_json = mare.fs.read(mare.paths.state .. "/models-cache/openai.json") or ""
local has_raw = disk_json:find("hdr-secret-c", 1, true) ~= nil or disk_json:find("q-secret-b", 1, true) ~= nil

return (m_a1 ~= nil and m_a1.listed == true),
  (m_a2 ~= nil and m_a2.listed == true),
  (m_b ~= nil and m_b.listed == true),
  (m_a_after_b == nil or m_a_after_b.listed ~= true),
  (m_c ~= nil and m_c.listed == true),
  (m_b_after_c == nil or m_b_after_c.listed ~= true),
  (not has_raw)
"#,
        )
        .await;

        assert!(has_a, "initial tenant A model must be published");
        assert!(cache_hit_a, "same params must reuse cached tenant A model");
        assert!(has_b, "changing resolved query_params must fetch tenant B model");
        assert!(no_a_after_b, "tenant A model must be replaced after query_params change");
        assert!(has_c, "changing resolved headers must fetch tenant C model");
        assert!(no_b_after_c, "tenant B model must be replaced after headers change");
        assert!(no_raw_secret, "catalog_target must only store digests, not raw sensitive header/query values");

        let reqs: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.path.starts_with("/codex/codex/models"))
            .collect();
        assert_eq!(reqs.len(), 3, "expected exactly 3 HTTP requests (1st, after query change, after header change)");
        assert!(reqs[1].path.contains("tenant=q-secret-b"), "2nd request must carry updated query param: {}", reqs[1].path);
        assert_eq!(
            reqs[2].headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-tenant")).map(|(_, v)| v.as_str()),
            Some("hdr-secret-c"),
            "3rd request must carry updated header"
        );
        h.shutdown().await;
    })
    .await
}
/// Ref: reference implementation (rescoping on credential switch);
/// Codex/Kimi Code 无跨进程 A→B→A 切回同类测试.
/// Self-authored: `refresh_models` must bind the catalog scope to the exact
/// OAuth credential and request snapshot used to fetch `/codex/models`, so an external
/// A -> B -> A credential switch across processes without a local `refresh_gen` bump cannot
/// publish or cache B's models under A's scope.
#[tokio::test]
async fn openai_oauth_cross_process_account_aba_switch_does_not_publish_b_under_a() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(200, r#"{"models":[{"slug":"gpt-acct-a","visibility":"list"}]}"#),
                Resp::status(200, r#"{"models":[{"slug":"gpt-acct-b","visibility":"list"}]}"#),
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

        let (seeded_a, saw_b_req, kept_a, no_b_mem, no_b_disk): (bool, bool, bool, bool, bool) = eval(
            &h,
            r#"
local acct_a = mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-a", refresh = "ref-a", expires = 9999999999999, account_id = "acct-a" }
})
local acct_b = mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-b", refresh = "ref-b", expires = 9999999999999, account_id = "acct-b" }
})
mare.fs.write(mare.paths.auth, acct_a, { mode = 384 })
LLM:refresh_catalog({ name = "openai", force = true })
local m_a0 = LLM:resolve("openai/gpt-acct-a")

-- Simulate another mare process switching auth.json A -> B -> A without triggering local
-- llm/login or refresh_gen updates in this process:
-- 1) Initial catalog_scope(p) reads A (four auth reads: catalog_scope + catalog_target).
-- 2) Before list_models_codex() calls fresh_oauth(), auth.json on disk is switched to B.
-- 3) During the HTTP request carrying acct-b, auth.json on disk is switched back to A.
local orig_read = mare.fs.read
local orig_write = mare.fs.write
local orig_http = mare.http.request
local auth_reads = 0
local saw_b_request = false

mare.fs.read = function(path)
  if path == mare.paths.auth then
    auth_reads = auth_reads + 1
    if auth_reads == 5 then
      orig_write(mare.paths.auth, acct_b, { mode = 384 })
    end
  end
  return orig_read(path)
end

mare.http.request = function(req)
  if type(req) == "table" and type(req.url) == "string" and req.url:find("/codex/codex/models", 1, true) then
    if type(req.headers) == "table" and req.headers["chatgpt-account-id"] == "acct-b" then
      saw_b_request = true
    end
    orig_write(mare.paths.auth, acct_a, { mode = 384 })
  end
  return orig_http(req)
end

LLM:refresh_models("openai", { force = true })
mare.fs.read = orig_read
mare.http.request = orig_http

local m_a1 = LLM:resolve("openai/gpt-acct-a")
local m_b1 = LLM:resolve("openai/gpt-acct-b")
local disk = mare.fs.read(mare.paths.state .. "/models-cache/openai.json") or ""

return (m_a0 ~= nil and m_a0.listed == true),
  saw_b_request,
  (m_a1 ~= nil and m_a1.listed == true),
  (m_b1 == nil or m_b1.listed ~= true),
  disk:find("gpt-acct-b", 1, true) == nil
"#,
        )
        .await;

        assert!(seeded_a, "initial refresh must publish account A model");
        assert!(saw_b_req, "in-flight request must have used account B credentials");
        assert!(kept_a, "account A model must remain published after A->B->A switch");
        assert!(no_b_mem, "account B model must NOT be published under account A");
        assert!(no_b_disk, "account B model must NOT be cached on disk under account A");
        h.shutdown().await;
    })
    .await
}
/// Config-overridden `Authorization` header must be distinguished from the
/// auto-generated OAuth bearer and hashed into both cache identity and request snapshot.
#[tokio::test]
async fn openai_oauth_catalog_cache_invalidates_on_config_authorization_header_override() {
    local(async {
        let env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(200, r#"{"models":[{"slug":"gpt-custom-a","visibility":"list"}]}"#),
                Resp::status(200, r#"{"models":[{"slug":"gpt-custom-b","visibility":"list"}]}"#),
            ],
        );
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"mare.provider("openai", { headers = { Authorization = "MARE_TEST_CUSTOM_AUTH" } })"#,
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .config(json!({
                "providers": { "deepseek": { "base_url": format!("{}/compat", server.base) } },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 60000, "retry_base_ms": 1 },
                    "provider-openai": {
                        "base_url": format!("{}/openai", server.base),
                        "codex_base_url": format!("{}/codex", server.base)
                    },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) },
                    "provider-google": { "base_url": format!("{}/google", server.base) }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (has_a, cache_hit_a, has_b, no_a_after_b, no_raw_secret): (
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            r#"
local env_map = {
  MARE_TEST_CUSTOM_AUTH = "Bearer custom-cred-a",
}
mare.env = function(k) return env_map[k] end
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "oauth-tok-1", refresh = "ref-1", expires = 9999999999999, account_id = "acct-1" }
}), { mode = 384 })

LLM:refresh_catalog({ name = "openai" })
local m_a1 = LLM:resolve("openai/gpt-custom-a")

-- Same config Authorization -> cache hit (no new HTTP request)
LLM:refresh_catalog({ name = "openai" })
local m_a2 = LLM:resolve("openai/gpt-custom-a")

-- Switch config-overridden Authorization -> must miss cache and fetch custom-b
env_map.MARE_TEST_CUSTOM_AUTH = "Bearer custom-cred-b"
LLM:refresh_catalog({ name = "openai" })
local m_b = LLM:resolve("openai/gpt-custom-b")
local m_a_after_b = LLM:resolve("openai/gpt-custom-a")

local disk_json = mare.fs.read(mare.paths.state .. "/models-cache/openai.json") or ""
local has_raw = disk_json:find("custom-cred-a", 1, true) ~= nil or disk_json:find("custom-cred-b", 1, true) ~= nil

return (m_a1 ~= nil and m_a1.listed == true),
  (m_a2 ~= nil and m_a2.listed == true),
  (m_b ~= nil and m_b.listed == true),
  (m_a_after_b == nil or m_a_after_b.listed ~= true),
  (not has_raw)
"#,
        )
        .await;

        assert!(has_a, "initial custom-cred-a model must be published");
        assert!(cache_hit_a, "same config Authorization must reuse cached model");
        assert!(has_b, "switching config Authorization must fetch custom-cred-b model");
        assert!(no_a_after_b, "custom-cred-a model must be replaced after Authorization switch");
        assert!(no_raw_secret, "catalog_target must only store digests, not raw Authorization values");

        let reqs: Vec<_> = server
            .requests()
            .into_iter()
            .filter(|r| r.path.starts_with("/codex/codex/models"))
            .collect();
        assert_eq!(reqs.len(), 2, "expected exactly 2 HTTP requests (initial + after Authorization switch)");
        fn auth_hdr(headers: &[(String, String)]) -> Vec<&str> {
            headers
                .iter()
                .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                .map(|(_, v)| v.as_str())
                .collect()
        }
        assert_eq!(auth_hdr(&reqs[0].headers), vec!["Bearer custom-cred-a"]);
        assert_eq!(auth_hdr(&reqs[1].headers), vec!["Bearer custom-cred-b"]);
        h.shutdown().await;
    })
    .await
}

/// Users sharing a ChatGPT workspace (`account_id`) but with distinct `email`
/// must not share catalog caches, while same-user OAuth token refreshes still hit cache.
#[tokio::test]
async fn openai_oauth_shared_account_id_isolates_distinct_emails_and_reuses_refresh() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/codex/codex/models",
            vec![
                Resp::status(200, r#"{"models":[{"slug":"gpt-alice","visibility":"list"}]}"#),
                Resp::status(200, r#"{"models":[{"slug":"gpt-bob","visibility":"list"}]}"#),
            ],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 60000 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", server.base) }
                }
            }),
        )
        .await;

        let (alice_ok, refresh_hit_ok, bob_ok, alice_dropped): (bool, bool, bool, bool) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = {
    type = "oauth",
    access = "tok-alice-1",
    refresh = "ref-alice-1",
    expires = 9999999999999,
    account_id = "org-shared",
    email = "alice@example.com",
  }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai" })
local m_alice_1 = LLM:resolve("openai/gpt-alice")

-- Same account_id + same email with rotated access/refresh tokens must reuse cache (no HTTP).
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = {
    type = "oauth",
    access = "tok-alice-2",
    refresh = "ref-alice-2",
    expires = 9999999999999,
    account_id = "org-shared",
    email = "alice@example.com",
  }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai" })
local m_alice_2 = LLM:resolve("openai/gpt-alice")

-- Same account_id + different email in the same ChatGPT workspace must miss cache and fetch Bob's list.
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = {
    type = "oauth",
    access = "tok-bob-1",
    refresh = "ref-bob-1",
    expires = 9999999999999,
    account_id = "org-shared",
    email = "bob@example.com",
  }
}), { mode = 384 })
LLM:refresh_catalog({ name = "openai" })
local m_bob = LLM:resolve("openai/gpt-bob")
local m_alice_after = LLM:resolve("openai/gpt-alice")

return (m_alice_1 ~= nil and m_alice_1.listed == true),
  (m_alice_2 ~= nil and m_alice_2.listed == true),
  (m_bob ~= nil and m_bob.listed == true),
  (m_alice_after == nil or m_alice_after.listed ~= true)
"#,
        )
        .await;

        assert!(alice_ok, "alice initial catalog must resolve gpt-alice");
        assert!(refresh_hit_ok, "same-user OAuth token refresh must hit catalog cache");
        assert!(bob_ok, "different email sharing account_id must miss cache and fetch gpt-bob");
        assert!(alice_dropped, "alice-only model must not linger in Bob's workspace catalog");
        assert_eq!(server.count("/codex/codex/models"), 2);
        h.shutdown().await;
    })
    .await
}

/// An `api_key = { cmd = ... }` credential must not run on startup, and when unparsed
/// its `catalog_scope` is nil so stale cached models are not loaded before resolution.
#[tokio::test]
async fn unresolved_cmd_credential_skips_startup_execution_and_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![Resp::status(200, r#"{"data":[{"id":"claude-live-cmd","display_name":"Live Cmd"}]}"#)],
        );
        let run_flag = env.cwd.join("cmd-ran-count.txt");
        let run_flag_str = run_flag.to_string_lossy().to_string();
        let cmd_script = format!("printf 'x' >> '{run_flag_str}'; printf 'sk-ant-cmd-startup-test'");

        // Pre-populate legacy/cached catalog entry to verify startup does not load stale unparsed cache.
        let cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(
            cache_dir.join("anthropic.json"),
            r#"{"fetched_at":9999999999,"models":[{"id":"claude-stale-cached"}]}"#,
        )
        .unwrap();

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "anthropic": {
                        "api_key": { "cmd": ["sh", "-c", cmd_script], "refresh_interval_ms": 600000 }
                    }
                },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }),
        )
        .await;
        assert!(!run_flag.exists(), "key command must NOT run during startup register_provider / catalog_scope check");
        let stale_loaded: bool = eval(
            &h,
            r#"
local m = LLM:resolve("anthropic/claude-stale-cached")
return m ~= nil and m.listed == true
"#,
        )
        .await;
        assert!(!stale_loaded, "unresolved cmd credential must not load stale disk cache before resolution");

        let refreshed_ok: bool = eval(
            &h,
            r#"
local r = LLM:refresh_catalog({ name = "anthropic", force = true })
local m = LLM:resolve("anthropic/claude-live-cmd")
return type(r.anthropic) == "number" and m ~= nil and m.listed == true
"#,
        )
        .await;
        assert!(refreshed_ok, "explicit refresh resolves cmd key and caches live model");
        assert!(run_flag.exists(), "refresh_catalog resolves key command once");
        h.shutdown().await;
    })
    .await
}

/// custom compat `mare.config.providers` with `api_key = { cmd = ... }`
/// must not crash `provider-openai-compat` startup (`sha256(table)`) and must not run `cmd` at startup.
#[tokio::test]
async fn custom_compat_cmd_credential_does_not_crash_startup() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/vllm/models", vec![Resp::status(200, r#"{"data":[{"id":"llama-live","name":"Llama Live"}]}"#)]);
        let run_flag = env.cwd.join("vllm-cmd-ran.txt");
        let cmd_script = format!("printf '1' >> '{}'; printf 'sk-vllm-cmd-ok'", run_flag.display());

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "vllm": {
                        "base_url": format!("{}/vllm", server.base),
                        "api_key": { "cmd": ["sh", "-c", cmd_script], "refresh_interval_ms": 600000 },
                        "models": [{ "model_id": "llama-seed", "name": "Llama Seed" }]
                    }
                },
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 }
                }
            }),
        )
        .await;
        assert!(h.kernel.problems().is_empty(), "startup plugin problems: {:?}", h.kernel.problems());
        assert!(!run_flag.exists(), "compat custom cmd credential must not run at startup");
        let seed_ok: bool = eval(&h, r#"return LLM:resolve("vllm/llama-seed") ~= nil"#).await;
        assert!(seed_ok, "custom compat provider must register its models on startup");

        let refreshed_ok: bool = eval(
            &h,
            r#"
local r = LLM:refresh_catalog({ name = "vllm", force = true })
local m = LLM:resolve("vllm/llama-live")
return type(r.vllm) == "number" and m ~= nil and m.listed == true
"#,
        )
        .await;
        assert!(refreshed_ok, "explicit refresh on custom compat provider must resolve cmd key and list live model");
        assert!(run_flag.exists(), "refresh_catalog should resolve key command");
        h.shutdown().await;
    })
    .await
}

/// Custom compat credentials use the same 401 invalidation as built-in providers.
/// Ref: Codex `model-provider/src/auth_tests.rs:357` (refresh after unauthorized response).
#[tokio::test]
async fn custom_compat_cmd_credential_refreshes_after_401() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/vllm/chat/completions",
            vec![Resp::status(401, r#"{"error":{"message":"expired"}}"#), Resp::sse(fixture("openai-compat.sse"))],
        );
        let counter = env.cwd.join("vllm-key-runs");
        let script = r#"n=$(cat "$1" 2>/dev/null || printf 0); n=$((n+1)); printf '%s' "$n" > "$1"; printf 'vllm-key-%s' "$n""#;
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "vllm": {
                        "base_url": format!("{}/vllm", server.base),
                        "api_key": { "cmd": ["sh", "-c", script, "key-helper", counter.to_string_lossy()], "refresh_interval_ms": 600000 },
                        "models": [{ "model_id": "llama-seed", "name": "Llama Seed" }]
                    }
                },
                "plugins": { "llm": { "catalog_refresh_ms": 0 } }
            }),
        )
        .await;
        assert!(!counter.exists(), "key helper must not run at startup");
        let message = stream(&h, "vllm/llama-seed").await;
        assert_fixture_message("custom compat", &message, 0);
        assert_eq!(std::fs::read_to_string(counter).unwrap(), "2", "401 must re-run the helper exactly once");
        let authorization: Vec<String> = server
            .requests()
            .iter()
            .filter(|request| request.path.starts_with("/vllm/chat/completions"))
            .filter_map(|request| request.header("authorization").map(str::to_owned))
            .collect();
        assert_eq!(authorization, ["Bearer vllm-key-1", "Bearer vllm-key-2"]);
        h.shutdown().await;
    })
    .await
}

/// A child agent VM (`mare.mode == "child"`) reads cached catalogs on startup when
/// resolved, never initiates network refresh calls (`refresh_catalog` / `refresh_models` / `ui/ready`),
/// and never writes catalog files to disk.
#[tokio::test]
async fn child_vm_reads_cache_without_network_refresh_or_write() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(200, r#"{"data":[{"id":"claude-from-parent","display_name":"Parent"}]}"#),
                Resp::status(200, r#"{"data":[{"id":"claude-forbidden-child","display_name":"Child"}]}"#),
            ],
        );
        server.route("/models.dev.json", vec![Resp::status(200, r#"{}"#)]);
        env.set_var("ANTHROPIC_API_KEY", "sk-ant-child-scope");

        let h_parent = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0, "models_dev_url": format!("{}/models.dev.json", server.base) },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }),
        )
        .await;
        let _: bool =
            eval(&h_parent, r#"LLM:refresh_catalog({ name = "anthropic", force = true }); return true"#).await;
        h_parent.shutdown().await;
        assert_eq!(server.count("/anthropic/v1/models"), 1);

        let h_child = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0, "models_dev_url": format!("{}/models.dev.json", server.base) },
                    "provider-anthropic": { "base_url": format!("{}/anthropic", server.base) }
                }
            }),
        )
        .await;
        expose(&h_child);
        let (has_cached, rc, rm): (bool, String, i64) = eval(
            &h_child,
            r#"
local args = { model = LLM:resolve("anthropic/claude-from-parent") }
LLM:prepare_request(args)
local before = LLM:resolve("anthropic/claude-from-parent")
local prev_mode = mare.mode
mare.mode = "child"
C.cordis:get("bus"):emit("ui/ready")
local rc = LLM:refresh_catalog({ name = "anthropic", force = true })
local rm = LLM:refresh_models("anthropic", { force = true })
mare.mode = prev_mode
return (before ~= nil and before.listed == true), mare.json.encode(rc), rm
"#,
        )
        .await;
        assert!(has_cached, "cached parent model must still load in child/new VM from disk cache");
        assert_eq!(rc, "{}", "child refresh_catalog must return empty result without refreshing: {rc}");
        assert_eq!(rm, 0, "child refresh_models must return 0 without refreshing");
        assert_eq!(server.count("/anthropic/v1/models"), 1, "child VM must make 0 HTTP refresh requests");
        assert!(server.count("/models.dev.json") <= 1, "child VM must not fetch models.dev");
        h_child.shutdown().await;
    })
    .await
}
