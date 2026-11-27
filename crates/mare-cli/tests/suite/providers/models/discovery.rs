use super::*;

#[tokio::test]
async fn switching_models_converts_the_history_without_touching_it() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic.sse"))]);
        let h = provider_host(&mut env, &server, json!({ "models": { "anthropic/claude-sonnet-5-5": { "images": false } } })).await;
        let (unchanged, n): (bool, i64) = eval(
            &h,
            r#"
local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
local long_id = "fc_" .. string.rep("x", 70) .. "|rs_1"
local history = {
  { role = "user", content = { { type = "text", text = "look" }, { type = "image", data = "AAAA", mime = "image/png", path = "/tmp/shot.png" } } },
  { role = "assistant", provider_id = "openai", model_id = "gpt-6.1-terra", content = {
      { type = "reasoning", text = "Earlier reasoning.", native = { ["openai-responses"] = { item = { type = "reasoning", encrypted_content = "enc-openai" } } } },
      { type = "tool_call", id = long_id, name = "read", arguments = { path = "a" } },
      { type = "tool_call", id = "call_2", name = "apply_patch", arguments = { input = "*** Begin Patch" } } } },
  { role = "tool_result", tool_call_id = long_id, tool_name = "read", content = { { type = "text", text = "A" } } },
  { role = "tool_result", tool_call_id = "call_2", tool_name = "apply_patch", content = { { type = "text", text = "applied" } } },
  { role = "user", content = { { type = "text", text = "go on" } } },
}
local before = mare.json.encode(history)
LLM:stream({ model = m, messages = history,
  tools = { { name = "read", description = "r", parameters = { type = "object", properties = {} } } } }, function() end)
return mare.json.encode(history) == before, #history
"#,
        )
        .await;
        assert!(unchanged, "the caller's (session's) history must not be rewritten");
        assert_eq!(n, 5);
        let body = server.requests().pop().unwrap().json();
        let text = body.to_string();
        assert!(text.contains("Earlier reasoning."), "other model's thinking becomes text: {text}");
        assert!(!text.contains("enc-openai"), "never another provider's signature");
        assert!(text.contains("Attachment: /tmp/shot.png"), "images → paths when unsupported");
        assert!(text.contains("called tool apply_patch"), "a tool no longer offered becomes text");
        let ids: Vec<String> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
            .filter_map(|b| b["id"].as_str().or(b["tool_use_id"].as_str()).map(str::to_string))
            .collect();
        assert!(!ids.is_empty() && ids.iter().all(|id| id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')), "{ids:?}");
        h.shutdown().await;
    })
    .await
}

/// Model catalogue: a provider that lists models gets its cached list at registration
/// (no request on the startup path); after ui/ready a stale cache is refreshed in the background,
/// new models are added and the cache rewritten; the static table's entries are never replaced.
#[tokio::test]
async fn model_catalogue_reads_the_disk_cache_and_refreshes_in_the_background() {
    local(async {
        let mut env = TestEnv::new().await;
        let dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("deepseek.json"), r#"{"fetched_at": 1, "models": [{"id": "cached-1"}]}"#).unwrap();
        let server = MockServer::start().await;
        server.route(
            "/compat/models",
            vec![Resp::status(
                200,
                r#"{"object":"list","data":[{"id":"deepseek-chat","context_length":1},{"id":"listed-2","context_length":65536}]}"#,
            )],
        );
        let h = provider_host(&mut env, &server, json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } })).await;
        expose(&h);
        // Trigger non-blocking disk-cache hydration via resolve and wait for it to populate.
        let _: bool = eval(&h, "return LLM:resolve('deepseek/cached-1') ~= nil").await;
        wait_until(10, "the cached list from disk", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "local m = LLM:resolve('deepseek/cached-1'); return m ~= nil and m.listed == true").await }
        })
        .await;
        assert_eq!(server.count("/compat/models"), 0, "no listing request on the startup path");
        exec(&h, "C.cordis:get('bus'):emit('ui/ready')").await;
        wait_until(10, "the refreshed model", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "local m = LLM:resolve('deepseek/listed-2'); return m ~= nil and m.listed == true").await }
        })
        .await;
        let (ctx_listed, ctx_static, name_static): (i64, i64, String) = eval(
            &h,
            r#"local l, s = LLM:resolve('deepseek/listed-2'), LLM:resolve('deepseek/deepseek-chat')
               return l.context_window, s.context_window, s.name"#,
        )
        .await;
        assert_eq!(ctx_listed, 65536);
        assert_eq!(
            (ctx_static, name_static.as_str()),
            (1, "DeepSeek V3.2"),
            "server context_window overrides unmeasured static entry"
        );
        let req = server.requests().into_iter().find(|r| r.path.starts_with("/compat/models")).unwrap();
        assert_eq!(req.header("authorization"), Some("Bearer test-deepseek"));
        let cache = std::fs::read_to_string(dir.join("deepseek.json")).unwrap();
        assert!(cache.contains("listed-2"), "the cache is rewritten: {cache}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn chatgpt_subscription_discovers_and_merges_codex_models_with_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let base = server.base.clone();
        server.route(
            "/codex/codex/models",
            vec![Resp::status(
                200,
                r#"{
                  "models": [
                    {
                      "slug": "gpt-6.1-terra",
                      "display_name": "Override Attempt",
                      "context_window": 1,
                      "visibility": "list"
                    },
                    {
                      "slug": "gpt-6-nova",
                      "display_name": "GPT-6-Nova",
                      "context_window": 272000,
                      "visibility": "list",
                      "supported_reasoning_levels": [
                        {"effort": "none"},
                        {"effort": "low"},
                        {"effort": "xhigh"}
                      ],
                      "input_modalities": ["text", "image"],
                      "support_verbosity": true,
                      "default_verbosity": "low",
                      "truncation_policy": {"mode": "tokens", "limit": 10000},
                      "apply_patch_tool_type": "freeform"
                    },
                    {
                      "slug": "gpt-6-hidden",
                      "display_name": "Hidden Model",
                      "context_window": 128000,
                      "visibility": "hide"
                    },
                    {
                      "slug": "gpt-6-none",
                      "display_name": "Disabled Model",
                      "context_window": 128000,
                      "visibility": "none"
                    }
                  ]
                }"#,
            )],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": { "codex_base_url": format!("{base}/codex") }
                }
            }),
        )
        .await;
        exec(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "sub-tok", refresh = "sub-ref", expires = 9999999999999, account_id = "acct-sub" }
}), { mode = 384 })
local out = LLM:refresh_catalog({ name = "openai", force = true })
assert(type(out.openai) == "number" and out.openai >= 2, "expected added models, got: " .. tostring(out.openai))
"#,
        )
        .await;

        let (static_ctx, static_name, static_listed): (i64, String, Option<bool>) = eval(
            &h,
            r#"local s = assert(LLM:resolve("openai/gpt-6.1-terra"))
               return s.context_window, s.name, s.listed"#,
        )
        .await;
        assert_eq!((static_ctx, static_name.as_str(), static_listed), (921000, "GPT-6.1 Terra", None));

        let (astra_ctx, nova_name, astra_listed, astra_efforts, astra_verb, astra_edit, astra_trunc, astra_inp): (
            i64,
            String,
            bool,
            String,
            String,
            String,
            i64,
            String,
        ) = eval(
            &h,
            r#"local a = assert(LLM:resolve("openai/gpt-6-nova"))
               return a.context_window, a.name, a.listed == true, table.concat(a.efforts or {}, ","), a.verbosity, a.edit_tool, a.truncation.limit, table.concat(a.input or {}, ",")"#,
        )
        .await;
        assert_eq!(astra_ctx, 272000);
        assert_eq!(nova_name, "GPT-6-Nova");
        assert!(astra_listed);
        assert_eq!(astra_efforts, "off,low,xhigh");
        assert_eq!(astra_verb, "low");
        assert_eq!(astra_edit, "apply_patch");
        assert_eq!(astra_trunc, 10000);
        assert_eq!(astra_inp, "text,image");

        let (hidden_ok, hidden_flag, hidden_in_picker, none_in_picker, none_unlisted): (bool, bool, bool, bool, bool) = eval(
            &h,
            r#"local h = LLM:resolve("openai/gpt-6-hidden")
               local hidden_picker, none_picker = false, false
               for _, m in ipairs(LLM:models()) do
                   if m.id == "gpt-6-hidden" then hidden_picker = true end
                   if m.id == "gpt-6-none" then none_picker = true end
               end
               local n = LLM:resolve("openai/gpt-6-none")
               return h ~= nil, h and h.hidden == true, hidden_picker, none_picker, n == nil"#,
        )
        .await;
        assert!(hidden_ok && hidden_flag, "visibility=hide resolves as hidden");
        assert!(!hidden_in_picker, "visibility=hide omitted from LLM:models()");
        assert!(!none_in_picker, "visibility=none omitted from LLM:models()");
        assert!(none_unlisted, "visibility=none is not added as a listed model");

        let req = server.requests().into_iter().find(|r| r.path.starts_with("/codex/codex/models")).unwrap();
        assert!(req.path.contains("client_version="), "missing client_version query: {}", req.path);
        assert_eq!(req.header("authorization"), Some("Bearer sub-tok"));
        assert_eq!(req.header("chatgpt-account-id"), Some("acct-sub"));
        assert_eq!(req.header("openai-beta"), Some("responses=experimental"));

        let cache = std::fs::read_to_string(env.home.path().join("state/models-cache/openai.json")).unwrap();
        assert!(cache.contains("gpt-6-nova"), "cache must record discovered model: {cache}");
        assert!(!cache.contains("gpt-6-none"), "cache must not record visibility=none model: {cache}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn chatgpt_model_discovery_refreshes_oauth_degrades_gracefully_and_guards_credentials() {
    local(async {
        {
            let mut env = TestEnv::new().await;
            let dir = env.home.path().join("state/models-cache");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("openai.json"), r#"{"fetched_at": 1, "models": [{"id": "cached-codex"}]}"#).unwrap();

            let server = MockServer::start().await;
            let base = server.base.clone();
            server.route(
                "/auth/oauth/token",
                vec![Resp::status(
                    200,
                    r#"{"access_token":"fresh-secret-tok","refresh_token":"fresh-secret-ref","expires_in":3600}"#,
                )],
            );
            server.route("/codex/codex/models", vec![Resp::status(500, r#"{"error":"backend down"}"#)]);

            let h = provider_host(
                &mut env,
                &server,
                json!({
                    "plugins": {
                        "provider-openai": {
                            "codex_base_url": format!("{base}/codex"),
                            "auth_base_url": format!("{base}/auth")
                        }
                    }
                }),
            )
            .await;

            let (err_msg, has_cached, has_static): (String, bool, bool) = eval(
                &h,
                r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "stale-secret-tok", refresh = "old-secret-ref", expires = 0, account_id = "acct-secret-1" }
}), { mode = 384 })
local out = LLM:refresh_catalog({ name = "openai", force = true })
return tostring(out.openai), LLM:resolve("openai/cached-codex") ~= nil, LLM:resolve("openai/gpt-6.1-terra") ~= nil
"#,
            )
            .await;

            assert!(err_msg.contains("HTTP 500"), "unexpected error: {err_msg}");
            for secret in ["fresh-secret-tok", "fresh-secret-ref", "stale-secret-tok", "old-secret-ref", "acct-secret-1"] {
                assert!(!err_msg.contains(secret), "error leaked credential {secret}: {err_msg}");
            }
            assert!(has_cached, "cached model must survive failed refresh");
            assert!(has_static, "static model must survive failed refresh");

            let req = server.requests().into_iter().find(|r| r.path.starts_with("/codex/codex/models")).unwrap();
            assert_eq!(req.header("authorization"), Some("Bearer fresh-secret-tok"), "must refresh expired OAuth token");
            let cache = std::fs::read_to_string(dir.join("openai.json")).unwrap();
            assert!(cache.contains("cached-codex"), "cache must not be wiped on error: {cache}");
            h.shutdown().await;
        }

        // Non-loopback plain HTTP codex_base_url must be refused before any network request.
        // Route openai through server2 as a proxy so any attempted request would hit server2.
        let mut env2 = TestEnv::new().await;
        env2.set_var("NO_PROXY", "");
        env2.set_var("no_proxy", "");
        let server2 = MockServer::start().await;
        std::fs::create_dir_all(env2.config_dir()).unwrap();
        std::fs::write(
            env2.config_dir().join("init.lua"),
            format!(r#"mare.provider("openai", {{ proxy = {:?} }})"#, server2.base),
        )
        .unwrap();
        let h2 = env2
            .host()
            .user_config(false)
            .config(json!({
                "plugins": {
                    "provider-openai": {
                        "codex_base_url": "http://example.com/backend-api"
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h2, "LLM", h2.service_table("llm").unwrap());
        let unsafe_err: String = eval(
            &h2,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "tok-never-sent", refresh = "ref-never-sent", expires = 9999999999999, account_id = "acct-never" }
}), { mode = 384 })
local out = LLM:refresh_catalog({ name = "openai", force = true })
return tostring(out.openai)
"#,
        )
        .await;
        assert!(unsafe_err.contains("https"), "expected https boundary error: {unsafe_err}");
        assert!(!unsafe_err.contains("tok-never-sent"));
        assert!(server2.requests().is_empty(), "server2 must not receive any proxied request: {:?}", server2.requests());
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn codex_models_discovery_proxy_headers_refresh_and_effort() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("NO_PROXY", "");
        env.set_var("no_proxy", "");
        let origin = MockServer::start().await;
        let proxy = MockServer::start().await;
        proxy.route(
            &format!("{}/codex/codex/models", origin.base),
            vec![
                Resp::status(
                    200,
                    json!({
                        "models": [
                            {
                                "slug": "gpt-6-ultra",
                                "display_name": "GPT-6 Ultra",
                                "visibility": "list",
                                "default_reasoning_level": "low",
                                "supported_reasoning_levels": [
                                    { "effort": "low" },
                                    { "effort": "medium" },
                                    { "effort": "high" },
                                    { "effort": "ultra" }
                                ]
                            },
                            {
                                "slug": "gpt-6-stale",
                                "display_name": "GPT-6 Stale",
                                "visibility": "list"
                            }
                        ]
                    })
                    .to_string(),
                ),
                Resp::status(
                    200,
                    json!({
                        "models": [
                            {
                                "slug": "gpt-6-ultra",
                                "display_name": "GPT-6 Ultra",
                                "visibility": "list",
                                "default_reasoning_level": "low",
                                "supported_reasoning_levels": [
                                    { "effort": "low" },
                                    { "effort": "medium" },
                                    { "effort": "high" },
                                    { "effort": "ultra" }
                                ]
                            },
                            {
                                "slug": "gpt-6-stale",
                                "display_name": "GPT-6 Stale",
                                "visibility": "none"
                            }
                        ]
                    })
                    .to_string(),
                ),
                Resp::status(500, "upstream down"),
            ],
        );
        proxy.route(&format!("{}/codex/codex/responses", origin.base), vec![Resp::sse(fixture("openai-responses.sse"))]);
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"mare.provider("openai", {{ proxy = {:?}, headers = {{ ["x-prov-custom"] = "prov-val" }} }})"#,
                proxy.base
            ),
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .config(json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0, "retry_base_ms": 1 },
                    "provider-openai": { "codex_base_url": format!("{}/codex", origin.base) }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let (default_eff, active_eff, has_ultra_opt): (String, String, bool) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "proxy-tok", refresh = "proxy-ref", expires = 9999999999999, account_id = "acct-proxy" }
}), { mode = 384 })
local out = LLM:refresh_catalog({ name = "openai", force = true })
assert(type(out.openai) == "number" and out.openai >= 2, tostring(out.openai))
assert(LLM:resolve("openai/gpt-6-stale") ~= nil, "stale model initially listed")
LLM:set_model("openai/gpt-6-ultra", { save = false })
local def_eff = LLM:effort()
local opts = LLM:effort_options() or {}
local found_ultra = false
for _, o in ipairs(opts) do if o == "ultra" then found_ultra = true end end
LLM:set_effort("ultra")
return def_eff, LLM:effort(), found_ultra
"#,
        )
        .await;
        assert_eq!(default_eff, "low", "default_reasoning_level=low must map to effort default");
        assert_eq!(active_eff, "ultra", "set_effort(ultra) must succeed");
        assert!(has_ultra_opt, "effort_options must include ultra");

        let stop_reason: String = eval(
            &h,
            r#"
local m = assert(LLM:resolve("openai/gpt-6-ultra"))
local msg = LLM:stream({
  model = m, system = "S",
  messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
}, function() end)
return tostring(msg.stop_reason)
"#,
        )
        .await;
        assert_eq!(stop_reason, "tool_use");

        let (stale_after_refresh, ultra_after_fail, err_msg): (bool, bool, String) = eval(
            &h,
            r#"
LLM:refresh_catalog({ name = "openai", force = true })
local stale_in_picker = false
for _, m in ipairs(LLM:models()) do
  if m.id == "gpt-6-stale" then stale_in_picker = true end
end
local r_stale = LLM:resolve("openai/gpt-6-stale")
local stale_present = stale_in_picker or (r_stale ~= nil and r_stale.listed == true)
local out2 = LLM:refresh_catalog({ name = "openai", force = true })
local r_ultra = LLM:resolve("openai/gpt-6-ultra")
local ultra_present = r_ultra ~= nil and r_ultra.listed == true
return stale_present, ultra_present, tostring(out2.openai)
"#,
        )
        .await;
        assert!(!stale_after_refresh, "stale model with visibility=none must be removed on refresh");
        assert!(ultra_after_fail, "cached model must survive failed refresh");
        assert!(err_msg.contains("HTTP 500"), "{err_msg}");

        assert!(origin.requests().is_empty(), "origin bypassed proxy: {:?}", origin.requests());
        let preqs = proxy.requests();
        let mreq = preqs.iter().find(|r| r.path.contains("/codex/codex/models")).unwrap();
        assert_eq!(mreq.header("x-prov-custom"), Some("prov-val"));
        let rreq = preqs.iter().find(|r| r.path.contains("/codex/codex/responses")).unwrap();
        let body: Value = serde_json::from_str(&rreq.body).unwrap();
        assert_eq!(body["reasoning"]["effort"], "ultra");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn fresh_oauth_during_discovery_uses_provider_proxy_without_leaking_headers() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("NO_PROXY", "");
        env.set_var("no_proxy", "");
        let origin = MockServer::start().await;
        let proxy = MockServer::start().await;
        proxy.route(
            &format!("{}/auth/oauth/token", origin.base),
            vec![Resp::status(
                200,
                r#"{"access_token":"refreshed-tok","refresh_token":"refreshed-ref","expires_in":3600}"#,
            )],
        );
        proxy.route(
            &format!("{}/codex/codex/models", origin.base),
            vec![Resp::status(
                200,
                r#"{"models":[{"slug":"gpt-6-proxied","display_name":"GPT-6 Proxied","visibility":"list"}]}"#,
            )],
        );
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            format!(
                r#"mare.provider("openai", {{ proxy = {:?}, connect_timeout_ms = 5000, headers = {{ ["x-prov-custom"] = "prov-val" }} }})"#,
                proxy.base
            ),
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .config(json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 0 },
                    "provider-openai": {
                        "codex_base_url": format!("{}/codex", origin.base),
                        "auth_base_url": format!("{}/auth", origin.base),
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());
        let (added, has_model): (i64, bool) = eval(
            &h,
            r#"
mare.fs.write(mare.paths.auth, mare.json.encode({ version = 1,
  ["openai-codex"] = { type = "oauth", access = "expired-tok", refresh = "old-ref", expires = 0, account_id = "acct-proxy" }
}), { mode = 384 })
local out = LLM:refresh_catalog({ name = "openai", force = true })
return assert(tonumber(out.openai), tostring(out.openai)), LLM:resolve("openai/gpt-6-proxied") ~= nil
"#,
        )
        .await;
        assert!(added >= 1 && has_model);
        assert!(origin.requests().is_empty(), "origin must not be contacted directly: {:?}", origin.requests());
        let preqs = proxy.requests();
        let areq = preqs.iter().find(|r| r.path.contains("/auth/oauth/token")).expect("auth request via proxy");
        assert_eq!(areq.header("x-prov-custom"), None, "model-endpoint headers must not leak to auth endpoint");
        let mreq = preqs.iter().find(|r| r.path.contains("/codex/codex/models")).expect("models request via proxy");
        assert_eq!(mreq.header("authorization"), Some("Bearer refreshed-tok"));
        assert_eq!(mreq.header("x-prov-custom"), Some("prov-val"));
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn codex_models_discovery_rejects_cross_origin_hook_rewrite() {
    local(async {
        let mut env = TestEnv::new().await;
        let origin = MockServer::start().await;
        let target = MockServer::start().await;
        let h = provider_host(
            &mut env,
            &origin,
            json!({
                "plugins": {
                    "provider-openai": { "codex_base_url": format!("{}/codex", origin.base) }
                }
            }),
        )
        .await;
        expose(&h);
        let err_msg: String = eval(
            &h,
            &format!(
                r#"
mare.fs.write(mare.paths.auth, mare.json.encode({{ version = 1,
  ["openai-codex"] = {{ type = "oauth", access = "secret-hook-tok", refresh = "secret-hook-ref", expires = 9999999999999, account_id = "acct-hook" }}
}}), {{ mode = 384 }})
C.cordis:on("provider/request", function(w, next)
  w.url = {:?} .. "/codex/models"
  return next()
end)
local out = LLM:refresh_catalog({{ name = "openai", force = true }})
return tostring(out.openai)
"#,
                target.base
            ),
        )
        .await;
        assert!(err_msg.contains("origin"), "expected origin mismatch error: {err_msg}");
        assert!(!err_msg.contains("secret-hook-tok"));
        assert!(target.requests().is_empty(), "rewritten target must receive no requests: {:?}", target.requests());
        assert!(origin.requests().is_empty(), "original server must receive no requests: {:?}", origin.requests());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn declared_aliases_and_model_defaults_work_through_provider_declarations() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({})).await;
        expose(&h);
        global(&h, "T", h.service_table("tools").unwrap());

        let (sonnet, opus, haiku, custom_alias, oa_mode, adhoc_gpt_mode, custom_patch_mode, gpt_rules_ok): (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            bool,
        ) = eval(
            &h,
            r#"
LLM:register_provider({
  provider = "decl-prov",
  is_configured = function() return true end,
  aliases = { fast_decl = "alpha-1", family_decl = { family = "beta" } },
  model_defaults = {
    ["*"] = { edit_tool = "replace" },
    ["*patch*"] = { edit_tool = "apply_patch" },
  },
  models = {
    { model_id = "alpha-1", name = "Alpha 1" },
    { model_id = "alpha-patch-2", name = "Alpha Patch 2" },
    { model_id = "beta-1", name = "Beta 1" },
    { model_id = "beta-3", name = "Beta 3" },
  },
})
local s = assert(LLM:alias_target("sonnet"))
local o = assert(LLM:alias_target("opus"))
local hk = assert(LLM:alias_target("haiku"))
local ca = assert(LLM:alias_target("fast_decl")) .. "|" .. assert(LLM:alias_target("family_decl"))
local oa_m = T:edit_mode(assert(LLM:resolve("openai/gpt-6.1-terra")))
local adhoc_gpt = { id = "gpt-5.5", provider = "openai", ref = "openai/gpt-5.5" }
local adhoc_m = T:edit_mode(adhoc_gpt)
local patch_model = assert(LLM:resolve("decl-prov/alpha-patch-2"))
local custom_m = T:edit_mode(patch_model)
assert(T:edit_mode(assert(LLM:resolve("decl-prov/alpha-1"))) == "replace")
local ctx_svc = C.cordis:get("context")
local p_gpt = ctx_svc:system_prompt(adhoc_gpt)
local p_patch = ctx_svc:system_prompt(patch_model)
local rules_ok = p_gpt:find("<gpt_rules>", 1, true) ~= nil
  and p_patch:find("<gpt_rules>", 1, true) == nil
  and LLM:model_default({ id = "gpt-5", provider = "deepseek" }, "gpt_rules") == true
  and LLM:model_default(patch_model, "gpt_rules") == nil
return s, o, hk, ca, oa_m, adhoc_m, custom_m, rules_ok
"#,
        )
        .await;

        assert_eq!(sonnet, "anthropic/claude-sonnet-5-5");
        assert_eq!(opus, "anthropic/claude-opus-5-5");
        assert_eq!(haiku, "anthropic/claude-haiku-5-5");
        assert_eq!(custom_alias, "decl-prov/alpha-1|decl-prov/beta-3");
        assert_eq!(oa_mode, "apply_patch");
        assert_eq!(adhoc_gpt_mode, "apply_patch");
        assert_eq!(custom_patch_mode, "apply_patch");
        assert!(gpt_rules_ok, "gpt_rules must be scoped to GPT/Codex models, not all apply_patch models");

        h.shutdown().await;
    })
    .await
}

fn strip_lua_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str: Option<u8> = None;
    while i < bytes.len() {
        if let Some(q) = in_str {
            out.push(bytes[i] as char);
            if bytes[i] == b'\\' && i + 1 < bytes.len() {
                i += 1;
                out.push(bytes[i] as char);
            } else if bytes[i] == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if bytes[i] == b'"' || bytes[i] == b'\'' {
            in_str = Some(bytes[i]);
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            // Check for long bracket comment --[=*[
            if i + 2 < bytes.len() && bytes[i + 2] == b'[' {
                let mut eqs = 0;
                let mut j = i + 3;
                while j < bytes.len() && bytes[j] == b'=' {
                    eqs += 1;
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'[' {
                    let close = format!("]{}]", "=".repeat(eqs));
                    if let Some(end_rel) = src[j + 1..].find(&close) {
                        i = j + 1 + end_rel + close.len();
                        out.push(' ');
                        continue;
                    }
                }
            }
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn extract_lua_strings(line: &str) -> Vec<String> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' || b[i] == b'\'' {
            let quote = b[i];
            i += 1;
            let mut s = String::new();
            while i < b.len() && b[i] != quote {
                if b[i] == b'\\' && i + 1 < b.len() {
                    s.push(b[i + 1] as char);
                    i += 2;
                    continue;
                }
                s.push(b[i] as char);
                i += 1;
            }
            if i < b.len() && b[i] == quote {
                out.push(s);
                i += 1;
            }
            continue;
        }
        i += 1;
    }
    out
}

fn extract_lua_field_list(src: &str, field: &str) -> Vec<String> {
    let stripped = strip_lua_comments(src);
    let mut out = Vec::new();
    let needle = field.to_string();
    let bytes = stripped.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let Some(pos) = stripped[i..].find(&needle) else { break };
        let start = i + pos;
        let prev_ok = start == 0 || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let mut j = start + needle.len();
        let next_ok = j >= bytes.len() || !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_');
        if prev_ok && next_ok {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'{' {
                    let mut depth = 0usize;
                    let body_start = j + 1;
                    while j < bytes.len() {
                        match bytes[j] {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    out.extend(extract_lua_strings(&stripped[body_start..j]));
                                    j += 1;
                                    break;
                                }
                            }
                            b'"' | b'\'' => {
                                let q = bytes[j];
                                j += 1;
                                while j < bytes.len() && bytes[j] != q {
                                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                                        j += 2;
                                    } else {
                                        j += 1;
                                    }
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                    i = j;
                    continue;
                }
            }
        }
        i = start + needle.len();
    }
    out
}

/// Extract top-level keys from a Lua table literal `local <table_name> = { key = ..., ["k-2"] = ... }`.
fn extract_lua_table_keys(src: &str, table_name: &str) -> Vec<String> {
    let stripped = strip_lua_comments(src);
    let bytes = stripped.as_bytes();
    let needle = table_name.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        let Some(pos) = stripped[i..].find(table_name) else {
            break;
        };
        let start = i + pos;
        let prev_ok = start == 0 || !(bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_');
        let mut j = start + needle.len();
        let next_ok = j >= bytes.len() || !(bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_');
        if prev_ok && next_ok {
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'=' {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'{' {
                    let mut depth = 0usize;
                    while j < bytes.len() {
                        match bytes[j] {
                            b'{' => {
                                depth += 1;
                                j += 1;
                            }
                            b'}' => {
                                depth -= 1;
                                j += 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            b'"' | b'\'' => {
                                let q = bytes[j];
                                j += 1;
                                while j < bytes.len() && bytes[j] != q {
                                    if bytes[j] == b'\\' && j + 1 < bytes.len() {
                                        j += 2;
                                    } else {
                                        j += 1;
                                    }
                                }
                                if j < bytes.len() {
                                    j += 1;
                                }
                            }
                            _ if depth == 1 => {
                                if bytes[j] == b'[' {
                                    let k_start = j + 1;
                                    while j < bytes.len() && bytes[j] != b']' {
                                        j += 1;
                                    }
                                    if j < bytes.len() {
                                        for lit in extract_lua_strings(&stripped[k_start..j]) {
                                            out.push(lit);
                                        }
                                        j += 1;
                                    }
                                } else if bytes[j].is_ascii_alphabetic() || bytes[j] == b'_' {
                                    let k_start = j;
                                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                                        j += 1;
                                    }
                                    let ident = &stripped[k_start..j];
                                    let mut k = j;
                                    while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                                        k += 1;
                                    }
                                    if k < bytes.len() && bytes[k] == b'=' && bytes.get(k + 1) != Some(&b'=') {
                                        out.push(ident.to_string());
                                    }
                                } else {
                                    j += 1;
                                }
                            }
                            _ => {
                                j += 1;
                            }
                        }
                    }
                    i = j;
                    continue;
                }
            }
        }
        i = start + needle.len();
    }
    out
}

/// Derive the banned vendor/plugin vocabulary directly from built-in plugin definitions:
/// declared `providers` + built-in `provider-*` plugin names + model-family extras (`gpt`,
/// `codex`, `gemini`), minus declared `wires` and route names (`chat`, `responses`, `anthropic`,
/// `google`).
fn derived_vendor_vocab(plugins_root: &std::path::Path) -> std::collections::BTreeSet<String> {
    let mut providers = std::collections::BTreeSet::new();
    let mut wires = std::collections::BTreeSet::new();
    let mut provider_plugins = std::collections::BTreeSet::new();
    for ent in std::fs::read_dir(plugins_root).unwrap() {
        let path = ent.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem.starts_with("provider-") {
            provider_plugins.insert(stem.to_string());
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for p in extract_lua_field_list(&src, "providers") {
            providers.insert(p);
        }
        for p in extract_lua_table_keys(&src, "PRESETS") {
            providers.insert(p);
        }
        for w in extract_lua_field_list(&src, "wires") {
            wires.insert(w);
        }
    }
    for route in ["chat", "responses", "anthropic", "google"] {
        wires.insert(route.to_string());
    }
    let mut vocab = std::collections::BTreeSet::new();
    for p in providers {
        if !wires.contains(&p) {
            vocab.insert(p);
        }
    }
    for plug in provider_plugins {
        vocab.insert(plug);
    }
    for family in ["gpt", "codex", "gemini"] {
        vocab.insert(family.to_string());
    }
    vocab
}

fn find_vendor_branches(lua_src: &str, vocab: &std::collections::BTreeSet<String>) -> Vec<(usize, String)> {
    let stripped = strip_lua_comments(lua_src);
    let mut hits = Vec::new();
    for (idx, line) in stripped.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.contains("@ai-sdk/") || trimmed.contains("PROTOCOL =") {
            continue;
        }
        let literals = extract_lua_strings(line);
        let matched = literals.iter().any(|lit| vocab.contains(lit) || lit == "provider-");
        if matched {
            hits.push((idx + 1, trimmed.to_string()));
        }
    }
    hits
}

/// no hardcoded vendor-name or provider-plugin literals in `llm` or `tools` outside provider
/// plugins (branches, lookups, and table/array literals alike), with a derived vocabulary and
/// an empty allowlist.
#[test]
fn no_vendor_name_branches_outside_provider_plugins() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../mare-plugin/preset/plugins");
    let vocab = derived_vendor_vocab(&root);
    assert!(
        vocab.contains("openai") && vocab.contains("deepseek") && vocab.contains("provider-openai-compat"),
        "{vocab:?}"
    );
    assert!(!vocab.contains("openai-chat") && !vocab.contains("anthropic") && !vocab.contains("google"), "{vocab:?}");

    // Self-test: synthetic vendor branches and table/array literals must be caught.
    for sample in [
        "if model.provider == \"openai\" then return \"apply_patch\" end",
        "if id:find(\"gpt\", 1, true) then return true end",
        "if plugin == \"provider-openai-compat\" then return true end",
        "local builtin = { \"deepseek\", \"kimi\", \"openrouter\" }",
        "local other = { [\"opencode-go\"] = true }",
        "local guess = \"provider-\" .. name",
        "local list = { \"provider-anthropic\", \"provider-google\" }",
    ] {
        assert!(!find_vendor_branches(sample, &vocab).is_empty(), "lint self-test failed to catch: {sample}");
    }
    assert!(
        find_vendor_branches(
            "-- model.provider == \"openai\"\nlocal x = model.edit_tool\nlocal r = { anthropic = \"anthropic\", wire = \"openai-chat\" }",
            &vocab,
        )
        .is_empty(),
        "comments, generic field reads, route names, and declared wire names must not trigger"
    );

    let mut files = vec![root.join("llm.lua"), root.join("tools.lua")];
    for dir_name in ["lua/llm", "lua/tools"] {
        let dir = root.join(dir_name);
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for ent in std::fs::read_dir(&d).unwrap() {
                let path = ent.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("lua") {
                    files.push(path);
                }
            }
        }
    }
    for ent in std::fs::read_dir(&root).unwrap() {
        let path = ent.unwrap().path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with("tools-")
            && name.ends_with(".lua")
        {
            files.push(path);
        }
    }
    files.sort();

    let allowlist: &[(&str, &str)] = &[];

    let mut seen_allowed = std::collections::HashSet::new();
    let mut unexpected = Vec::new();
    for path in files {
        let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let src = std::fs::read_to_string(&path).unwrap();
        let hits = find_vendor_branches(&src, &vocab);
        if let Some((_, _reason)) = allowlist.iter().find(|(p, _)| *p == rel) {
            assert!(!hits.is_empty(), "stale V10 allowlist entry {rel}: no vendor branches remain");
            seen_allowed.insert(rel);
            continue;
        }
        for (line_no, snippet) in hits {
            unexpected.push(format!("{rel}:{line_no}: {snippet}"));
        }
    }
    assert!(
        unexpected.is_empty(),
        "unexpected vendor-name literals outside provider plugins:\n{}",
        unexpected.join("\n")
    );
    assert_eq!(seen_allowed.len(), allowlist.len(), "allowlist check mismatch");

    // Also scan `mare_lib.lua` and `mare_lib/schema_sanitize.lua`: they may inspect dialect ids
    // and model families, but must never branch on provider names (`provider == ...`,
    // `provider_id`) or reference `provider-*` plugins.
    let mut mare_lib_hits = Vec::new();
    for rel in ["lua/mare_lib.lua", "lua/mare_lib/schema_sanitize.lua"] {
        let src = std::fs::read_to_string(root.join(rel)).unwrap();
        let stripped = strip_lua_comments(&src);
        for (idx, line) in stripped.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let lits = extract_lua_strings(line);
            let has_plugin_lit = lits.iter().any(|lit| lit == "provider-" || lit.starts_with("provider-"));
            let has_provider_branch = trimmed.contains("provider_id")
                || (trimmed.contains("provider ==") && lits.iter().any(|lit| vocab.contains(lit)))
                || (trimmed.contains("provider ~=") && lits.iter().any(|lit| vocab.contains(lit)));
            if has_plugin_lit || has_provider_branch {
                mare_lib_hits.push(format!("{rel}:{}: {trimmed}", idx + 1));
            }
        }
    }
    assert!(
        mare_lib_hits.is_empty(),
        "mare_lib must use dialect ids (not provider names or provider-* plugins):\n{}",
        mare_lib_hits.join("\n")
    );
}

/// No built-in provider plugin (`provider-*.lua` and its `lua/provider-*/*.lua` modules) may
/// name another provider plugin's declared providers (excluding declared wires and route names).
#[test]
fn provider_plugins_name_no_other_plugins_providers() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../mare-plugin/preset/plugins");
    let mut wires_and_routes: std::collections::BTreeSet<String> =
        ["chat", "responses", "anthropic", "google"].into_iter().map(String::from).collect();
    let mut plugin_owners: Vec<(String, std::collections::BTreeSet<String>)> = Vec::new();
    let mut all_providers = std::collections::BTreeSet::new();

    for ent in std::fs::read_dir(&root).unwrap() {
        let path = ent.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("lua") {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        if !stem.starts_with("provider-") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        for w in extract_lua_field_list(&src, "wires") {
            wires_and_routes.insert(w);
        }
        let mut owned: std::collections::BTreeSet<String> =
            extract_lua_field_list(&src, "providers").into_iter().collect();
        // Also collect preset keys if the plugin derives `providers` from top-level `PRESETS`.
        for p in extract_lua_table_keys(&src, "PRESETS") {
            owned.insert(p);
        }
        if let Some(stem_short) = stem.strip_prefix("provider-")
            && owned.is_empty()
        {
            owned.insert(stem_short.to_string());
        }
        for p in &owned {
            all_providers.insert(p.clone());
        }
        plugin_owners.push((stem, owned));
    }
    plugin_owners.sort_by(|a, b| a.0.cmp(&b.0));

    let mut violations = Vec::new();
    for (plugin, owned) in &plugin_owners {
        let mut forbidden = std::collections::BTreeSet::new();
        for p in &all_providers {
            if !owned.contains(p) && !wires_and_routes.contains(p) {
                forbidden.insert(p.clone());
            }
        }
        let mut files = vec![root.join(format!("{plugin}.lua"))];
        let sub = root.join("lua").join(plugin);
        if sub.is_dir() {
            let mut stack = vec![sub];
            while let Some(d) = stack.pop() {
                for ent in std::fs::read_dir(&d).unwrap() {
                    let path = ent.unwrap().path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("lua") {
                        files.push(path);
                    }
                }
            }
        }
        files.sort();
        for path in files {
            let rel = path.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
            let src = std::fs::read_to_string(&path).unwrap();
            let stripped = strip_lua_comments(&src);
            for (idx, line) in stripped.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let lits = extract_lua_strings(line);
                if let Some(bad) = lits.iter().find(|lit| forbidden.contains(lit.as_str())) {
                    violations.push(format!("{rel}:{}: references foreign provider `{bad}`: {trimmed}", idx + 1));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "provider plugins must not name other plugins' providers:\n{}",
        violations.join("\n")
    );
}

/// Providers declare `catalog()` instead of custom HTTP list_models loops; `llm` executes
/// one shared `MareHttp` GET with `same-origin` redirect protection and `provider/request`, and
/// effective config changes invalidate cached in-memory catalog entries.
#[tokio::test]
async fn declarative_provider_catalog_uses_shared_fetch_and_invalidates_on_config_change() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/decl-a/models", vec![Resp::status(200, r#"{"data":[{"id":"decl-model-a","name":"Decl A"}]}"#)]);
        server.route("/decl-b/models", vec![Resp::status(200, r#"{"data":[{"id":"decl-model-b","name":"Decl B"}]}"#)]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "llm": { "catalog_refresh_ms": 86400000 }
                }
            }),
        )
        .await;
        expose(&h);
        let base = server.base.clone();
        let (builtins_use_catalog, has_a, req_hook_saw_a, has_b_after_cfg, a_gone_after_cfg): (
            bool,
            bool,
            bool,
            bool,
            bool,
        ) = eval(
            &h,
            &format!(
                r#"
local builtins_use_catalog = true
for _, pname in ipairs({{ "openai", "anthropic", "google", "opencode-go", "deepseek" }}) do
  local p = assert(LLM:provider(pname))
  if type(p.catalog) ~= "function" then
    builtins_use_catalog = false
  end
end

local hook_urls = {{}}
C.cordis:on("provider/request", function(w, next)
  hook_urls[#hook_urls + 1] = w.url
  return next(w)
end)

local base = "{base}/decl-a"
local cred = "k-decl"
LLM:register_provider({{
  provider = "decl-prov",
  route = "chat",
  stream = function() error("unused") end,
  is_configured = function() return true end,
  catalog_target = function() return {{ url = base .. "/models" }} end,
  catalog_scope = function() return "decl:" .. mare.crypto.sha256(cred) end,
  catalog = function(for_fetch)
    return {{
      label = "decl-prov GET /models",
      url = base .. "/models",
      auth = "bearer",
      key = for_fetch and cred or nil,
      decode = function(raw)
        local items = require("mare_lib").decode_models_envelope(raw, "data", "id", "decl-prov GET /models")
        local out = {{}}
        for _, m in ipairs(items) do out[#out + 1] = {{ id = m.id, name = m.name }} end
        return out
      end,
    }}
  end,
}})

LLM:refresh_catalog({{ name = "decl-prov", force = true }})
local ma = LLM:resolve("decl-prov/decl-model-a")
local has_a = ma ~= nil and ma.listed == true
local saw_a = (#hook_urls == 1 and hook_urls[1] == "{base}/decl-a/models")

base = "{base}/decl-b"
LLM:refresh_catalog({{ name = "decl-prov" }})
local mb = LLM:resolve("decl-prov/decl-model-b")
local ma_after = LLM:resolve("decl-prov/decl-model-a")
local has_b = mb ~= nil and mb.listed == true
local a_gone = (ma_after == nil or ma_after.listed ~= true)
return builtins_use_catalog, has_a, saw_a, has_b, a_gone
"#
            ),
        )
        .await;
        assert!(builtins_use_catalog, "all built-in providers must declare provider.catalog()");
        assert!(has_a && req_hook_saw_a, "declarative catalog() must fetch through shared provider/request pipeline");
        assert!(
            has_b_after_cfg && a_gone_after_cfg,
            "effective catalog target change must invalidate memory cache and fetch new URL"
        );
        h.shutdown().await;
    })
    .await
}
