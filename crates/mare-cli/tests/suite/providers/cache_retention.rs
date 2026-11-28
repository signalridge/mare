//! Prompt cache retention tier (`PROV-CACHE`: "none" | "short" | "long") tests.
//! References:
//! - reference implementation: `packages/llm/llm-pi-ai/src/config.ts:171,369`, `src/adapter.ts:159`
//! - reference implementation: `packages/ai/src/types.ts:252`, `src/api/anthropic-messages.ts:82-106,237`,
//!   `src/api/openai-responses.ts:81-142,361-373`, `src/api/openai-completions.ts:792-824,1068-1088,1643`,
//!   `catalog/providers/openai.ts:137`,
//!   `test/openai-responses-prompt-cache.test.ts:56-71`, `test/openai-completions-prompt-cache.test.ts:115-243`,
//!   `test/openai-responses-chatgpt-sign-in.test.ts:34-75`.

use super::*;

#[tokio::test]
async fn anthropic_cache_retention_tiers_and_precedence() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse")); 8]);
        let h = provider_host(
            &mut env,
            &server,
            json!({
                "cache_retention": "short",
                "plugins": {
                    "provider-anthropic": { "cache_retention": "long" }
                },
                "models": {
                    "anthropic/claude-sonnet-4-5": { "cache_retention": "short" },
                    "anthropic/claude-haiku-4-5": { "supports_long_cache_retention": false }
                }
            }),
        )
        .await;

        exec(
            &h,
            r#"
            local m55 = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            local m45 = assert(LLM:resolve("anthropic/claude-sonnet-4-5"))
            local mhk = assert(LLM:resolve("anthropic/claude-haiku-4-5"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            -- 1. Plugin config "long" -> cache_control.ttl = "1h"
            LLM:stream({ model = m55, system = "S", messages = msgs }, function() end)
            -- 2. Model override "short" beats plugin "long" -> no ttl
            LLM:stream({ model = m45, system = "S", messages = msgs }, function() end)
            -- 3. Per-request "long" beats model "short" -> ttl = "1h"
            LLM:stream({ model = m45, system = "S", messages = msgs, cache_retention = "long" }, function() end)
            -- 4. Per-request "none" omits cache_control
            LLM:stream({ model = m55, system = "S", messages = msgs, cache_retention = "none" }, function() end)
            -- 5. Legacy req.cache = false beats req.cache_retention = "long"
            LLM:stream({ model = m55, system = "S", messages = msgs, cache = false, cache_retention = "long" }, function() end)
            -- 6. Legacy req.cache = "read" omits cache_control even with "long"
            LLM:stream({ model = m55, system = "S", messages = msgs, cache = "read", cache_retention = "long" }, function() end)
            -- 7. supports_long_cache_retention = false omits ttl even with "long"
            LLM:stream({ model = mhk, system = "S", messages = msgs, cache_retention = "long" }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 7);

        let b0 = reqs[0].json();
        assert_eq!(b0["system"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));
        assert_eq!(b0["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));

        let b1 = reqs[1].json();
        assert_eq!(b1["system"][0]["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(b1["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral" }));

        let b2 = reqs[2].json();
        assert_eq!(b2["system"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));

        let b3 = reqs[3].json();
        assert!(b3["system"][0].get("cache_control").is_none());
        assert!(b3["messages"][0]["content"][0].get("cache_control").is_none());

        let b4 = reqs[4].json();
        assert!(b4["system"][0].get("cache_control").is_none());
        assert!(b4["messages"][0]["content"][0].get("cache_control").is_none());

        let b5 = reqs[5].json();
        assert!(b5["system"][0].get("cache_control").is_none());
        assert!(b5["messages"][0]["content"][0].get("cache_control").is_none());

        let b6 = reqs[6].json();
        assert_eq!(b6["system"][0]["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(b6["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral" }));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_cache_retention_tiers_and_caps() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MYRESP_KEY", "test-myresp");
        let server = MockServer::start().await;
        server.route("/openai", vec![Resp::sse(responses_text("ok")); 14]);
        server.route("/myresp", vec![Resp::sse(responses_text("ok")); 3]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "myresp": {
                        "wire_api": "responses",
                        "base_url": format!("{}/myresp", server.base),
                        "api_key_env": "MYRESP_KEY",
                        "supports_long_cache_retention": false
                    }
                },
                "models": {
                    "openai/gpt-5.5": { "id": "gpt-5.5" },
                    "openai/gpt-6-nova": { "id": "gpt-6-nova" },
                    "openai/gpt-6.1-luna": { "supports_explicit_prompt_cache_mode": false, "supports_in_memory_cache_retention": true }
                }
            }),
        )
        .await;

        exec(
            &h,
            r#"
            CACHE_WARNS = {}
            local orig_notify = mare.ui and mare.ui.notify
            mare.ui = mare.ui or {}
            mare.ui.notify = function(msg, level)
                CACHE_WARNS[#CACHE_WARNS + 1] = { msg = tostring(msg), level = tostring(level) }
                if orig_notify then pcall(orig_notify, msg, level) end
            end

            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local oa_legacy = assert(LLM:resolve("openai/gpt-6.1-luna"))
            local oa_55 = assert(LLM:resolve("openai/gpt-5.5"))
            local oa_dyn = assert(LLM:resolve("openai/gpt-6-nova"))
            local other = assert(LLM:resolve("myresp/some-model"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            -- 1. Explicit-mode model default ("short"): sends prompt_cache_key, omits prompt_cache_options/retention
            LLM:stream({ model = oa, system = "S", session_id = "sess-1", messages = msgs }, function() end)
            -- 2. Explicit-mode model "long": sends prompt_cache_key AND prompt_cache_options = { ttl = "12h" } (distinct from short 30m default)
            LLM:stream({ model = oa, system = "S", session_id = "sess-2", messages = msgs, cache_retention = "long" }, function() end)
            -- 3. Explicit-mode model "none": omits prompt_cache_key AND sends prompt_cache_options = { mode = "explicit" }
            LLM:stream({ model = oa, system = "S", session_id = "sess-3", messages = msgs, cache_retention = "none" }, function() end)
            -- 4. req.cache = "read" with "long": keeps prompt_cache_key, omits prompt_cache_options/retention
            LLM:stream({ model = oa, system = "S", session_id = "sess-4", messages = msgs, cache = "read", cache_retention = "long" }, function() end)
            -- 5. Non-explicit OpenAI model with supports_in_memory_cache_retention=true "short": sends prompt_cache_retention = "in_memory"
            LLM:stream({ model = oa_legacy, system = "S", session_id = "sess-5", messages = msgs, cache_retention = "short" }, function() end)
            -- 6. Non-explicit OpenAI model "long": sends prompt_cache_retention = "24h"
            LLM:stream({ model = oa_legacy, system = "S", session_id = "sess-6", messages = msgs, cache_retention = "long" }, function() end)
            -- 7. Provider with supports_long_cache_retention = false omits prompt_cache_retention
            LLM:stream({ model = other, system = "S", session_id = "sess-7", messages = msgs, cache_retention = "long" }, function() end)
            -- 8. Per-request cache_retention applies only to that request (Codex turn_input_submission.rs:67-97)
            LLM:stream({ model = oa, system = "S", session_id = "sess-8", messages = msgs }, function() end)
            -- 9. Non-explicit OpenAI model with "none" omits prompt_cache_key and emits caller-visible warning
            LLM:stream({ model = oa_legacy, system = "S", session_id = "sess-9", messages = msgs, cache_retention = "none" }, function() end)
            -- 10. GPT-5.5 default ("short") must NOT send unsupported prompt_cache_retention = "in_memory"
            LLM:stream({ model = oa_55, system = "S", session_id = "sess-10", messages = msgs }, function() end)
            -- 11. Dynamic model without cost metadata (gpt-6-nova) with "none" sends prompt_cache_options = { mode = "explicit" }
            LLM:stream({ model = oa_dyn, system = "S", session_id = "sess-11", messages = msgs, cache_retention = "none" }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 11);

        let b0 = reqs[0].json();
        assert_eq!(b0["prompt_cache_key"], "sess-1");
        assert!(b0.get("prompt_cache_retention").is_none());
        assert!(b0.get("prompt_cache_options").is_none());

        let b1 = reqs[1].json();
        assert_eq!(b1["prompt_cache_key"], "sess-2");
        assert_eq!(b1["prompt_cache_options"], json!({ "ttl": "12h" }));
        assert!(b1.get("prompt_cache_retention").is_none());

        let b2 = reqs[2].json();
        assert!(b2.get("prompt_cache_key").is_none());
        assert_eq!(b2["prompt_cache_options"], json!({ "mode": "explicit" }));

        let b3 = reqs[3].json();
        assert_eq!(b3["prompt_cache_key"], "sess-4");
        assert!(b3.get("prompt_cache_retention").is_none());
        assert!(b3.get("prompt_cache_options").is_none());

        let b4 = reqs[4].json();
        assert_eq!(b4["prompt_cache_key"], "sess-5");
        assert_eq!(b4["prompt_cache_retention"], "in_memory");

        let b5 = reqs[5].json();
        assert_eq!(b5["prompt_cache_key"], "sess-6");
        assert_eq!(b5["prompt_cache_retention"], "24h");

        let b6 = reqs[6].json();
        assert_eq!(b6["prompt_cache_key"], "sess-7");
        assert!(b6.get("prompt_cache_retention").is_none());

        let b7 = reqs[7].json();
        assert_eq!(b7["prompt_cache_key"], "sess-8");
        assert!(b7.get("prompt_cache_options").is_none());
        assert!(b7.get("prompt_cache_retention").is_none());

        let b8 = reqs[8].json();
        assert!(b8.get("prompt_cache_key").is_none());
        assert!(b8.get("prompt_cache_options").is_none());
        assert!(b8.get("prompt_cache_retention").is_none());

        let b9 = reqs[9].json();
        assert_eq!(b9["prompt_cache_key"], "sess-10");
        assert!(b9.get("prompt_cache_retention").is_none(), "gpt-5.5 must not receive in_memory: {b9}");

        let b10 = reqs[10].json();
        assert!(b10.get("prompt_cache_key").is_none());
        assert_eq!(b10["prompt_cache_options"], json!({ "mode": "explicit" }));

        let warns_val: mlua::Value = h.lua().load("return CACHE_WARNS").eval().unwrap();
        let warns = mare_shared::lua::lua_to_json(h.lua(), warns_val).unwrap();
        let warn_arr = warns.as_array().expect("CACHE_WARNS array");
        assert!(
            warn_arr.iter().any(|w| w["msg"].as_str().unwrap_or("").contains("gpt-6.1-luna")),
            "expected caller-visible ui.notify warning for unsupported none on gpt-6.1-luna: {warns}"
        );

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn opencode_go_routes_preserve_cache_retention_and_precedence() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENCODE_API_KEY", "test-og");
        let server = MockServer::start().await;
        server.route("/og-resp", vec![Resp::sse(responses_text("ok")); 4]);
        server.route("/og-anth", vec![Resp::sse(fixture("anthropic-text.sse")); 4]);

        // Case A: route-level settings on responses, and provider "none" beating plugin "long" on anthropic.
        let h1 = provider_host(
            &mut env,
            &server,
            json!({
                "cache_retention": "long",
                "providers": {
                    "opencode-go": { "cache_retention": "none" }
                },
                "plugins": {
                    "provider-opencode-go": {
                        "base_url": server.base,
                        "cache_retention": "long",
                        "model_routes": { "glm-*": "anthropic" },
                        "routes": {
                            "responses": { "path": "/og-resp", "supports_long_cache_retention": false },
                            "anthropic": { "path": "/og-anth" }
                        }
                    }
                }
            }),
        )
        .await;

        exec(
            &h1,
            r#"
            local og_resp = assert(LLM:resolve("opencode-go/crouton-alpha-free"))
            local og_anth = assert(LLM:resolve("opencode-go/glm-5.3-flash"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            -- 1. provider "none" beats plugin "long" on responses -> omits prompt_cache_key
            LLM:stream({ model = og_resp, system = "S", session_id = "sess-og1", messages = msgs }, function() end)
            -- 2. per-request "long" with route supports_long_cache_retention = false -> omits prompt_cache_retention
            LLM:stream({ model = og_resp, system = "S", session_id = "sess-og2", messages = msgs, cache_retention = "long" }, function() end)
            -- 3. provider "none" beats plugin "long" on anthropic -> omits cache_control
            LLM:stream({ model = og_anth, system = "S", messages = msgs }, function() end)
            "#,
        )
        .await;
        h1.shutdown().await;

        // Case B: route-level "none" on responses, and provider "long" beating plugin "none" on anthropic.
        let h2 = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "opencode-go": { "cache_retention": "long" }
                },
                "plugins": {
                    "provider-opencode-go": {
                        "base_url": server.base,
                        "cache_retention": "none",
                        "model_routes": { "glm-*": "anthropic" },
                        "routes": {
                            "responses": { "path": "/og-resp" },
                            "anthropic": { "path": "/og-anth" }
                        }
                    }
                }
            }),
        )
        .await;
        exec(
            &h2,
            r#"
            local og_resp = assert(LLM:resolve("opencode-go/crouton-alpha-free"))
            local og_anth = assert(LLM:resolve("opencode-go/glm-5.3-flash"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            -- 4. provider "long" beats plugin "none" on responses -> sends prompt_cache_retention = "24h"
            LLM:stream({ model = og_resp, system = "S", session_id = "sess-og3", messages = msgs }, function() end)
            -- 5. provider "long" beats plugin "none" on anthropic -> sends ttl = "1h"
            LLM:stream({ model = og_anth, system = "S", messages = msgs }, function() end)
            "#,
        )
        .await;
        h2.shutdown().await;

        // Case C: route-level cache_retention = "none" with provider unset and plugin = "long".
        let h3 = provider_host(
            &mut env,
            &server,
            json!({
                "plugins": {
                    "provider-opencode-go": {
                        "base_url": server.base,
                        "cache_retention": "long",
                        "model_routes": { "glm-*": "anthropic" },
                        "routes": {
                            "responses": { "path": "/og-resp", "cache_retention": "none" },
                            "anthropic": { "path": "/og-anth", "cache_retention": "none" }
                        }
                    }
                }
            }),
        )
        .await;
        exec(
            &h3,
            r#"
            OG_WARNS = {}
            mare.ui = mare.ui or {}
            mare.ui.notify = function(msg, level)
                OG_WARNS[#OG_WARNS + 1] = { msg = tostring(msg), level = tostring(level) }
            end
            local og_resp = assert(LLM:resolve("opencode-go/crouton-alpha-free"))
            local og_anth = assert(LLM:resolve("opencode-go/glm-5.3-flash"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            -- 6. route-level "none" beats plugin "long" on responses -> omits prompt_cache_key and notifies caller
            LLM:stream({ model = og_resp, system = "S", session_id = "sess-og4", messages = msgs }, function() end)
            -- 7. route-level "none" beats plugin "long" on anthropic -> omits cache_control
            LLM:stream({ model = og_anth, system = "S", messages = msgs }, function() end)
            "#,
        )
        .await;
        let og_warns_val: mlua::Value = h3.lua().load("return OG_WARNS").eval().unwrap();
        let og_warns = mare_shared::lua::lua_to_json(h3.lua(), og_warns_val).unwrap();
        assert!(
            og_warns
                .as_array()
                .is_some_and(|a| a.iter().any(|w| w["msg"].as_str().unwrap_or("").contains("opencode-go"))),
            "expected caller-visible ui.notify for route-level none on opencode-go responses: {og_warns}"
        );
        h3.shutdown().await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 7);

        let r0 = reqs[0].json();
        assert!(r0.get("prompt_cache_key").is_none());
        let r1 = reqs[1].json();
        assert_eq!(r1["prompt_cache_key"], "sess-og2");
        assert!(r1.get("prompt_cache_retention").is_none());
        assert!(reqs[2].path.starts_with("/og-anth"), "expected anthropic route, got {}", reqs[2].path);
        let r2 = reqs[2].json();
        assert!(r2["system"][0].get("cache_control").is_none());

        let r3 = reqs[3].json();
        assert_eq!(r3["prompt_cache_key"], "sess-og3");
        assert_eq!(r3["prompt_cache_retention"], "24h");
        assert!(reqs[4].path.starts_with("/og-anth"), "expected anthropic route, got {}", reqs[4].path);
        let r4 = reqs[4].json();
        assert_eq!(r4["system"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));

        let r5 = reqs[5].json();
        assert!(r5.get("prompt_cache_key").is_none(), "route-level none must omit prompt_cache_key: {r5}");
        assert!(reqs[6].path.starts_with("/og-anth"), "expected anthropic route, got {}", reqs[6].path);
        let r6 = reqs[6].json();
        assert!(r6["system"][0].get("cache_control").is_none(), "route-level none must omit cache_control: {r6}");
    })
    .await
}

#[tokio::test]
async fn custom_responses_provider_respects_runtime_updates() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MYRESP_KEY", "test-myresp");
        let server = MockServer::start().await;
        server.route("/myresp", vec![Resp::sse(responses_text("ok")); 3]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "myresp": {
                        "wire_api": "responses",
                        "base_url": format!("{}/myresp", server.base),
                        "api_key_env": "MYRESP_KEY",
                        "cache_retention": "long"
                    }
                }
            }),
        )
        .await;

        exec(
            &h,
            r#"
            local m = assert(LLM:resolve("myresp/some-model"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            LLM:stream({ model = m, system = "S", session_id = "sess-rt1", messages = msgs }, function() end)
            mare.config.providers.myresp.cache_retention = "none"
            LLM:stream({ model = m, system = "S", session_id = "sess-rt2", messages = msgs }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);
        let b0 = reqs[0].json();
        assert_eq!(b0["prompt_cache_key"], "sess-rt1");
        assert_eq!(b0["prompt_cache_retention"], "24h");

        let b1 = reqs[1].json();
        assert!(b1.get("prompt_cache_key").is_none());
        assert!(b1.get("prompt_cache_retention").is_none());

        h.shutdown().await;
    })
    .await
}

#[test]
fn strict_config_accepts_declared_plugin_cache_retention_and_rejects_unknown() {
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("init.lua"),
        r#"
mare.setup { model = "faux/echo", cache_retention = "short" }
mare.plugin("provider-faux", { script_json = '[{"text":"ok"}]' })
"#,
    )
    .unwrap();

    for (key, value) in [
        ("provider-anthropic.cache_retention", "long"),
        ("provider-openai.cache_retention", "short"),
        ("provider-openai-compat.cache_retention", "none"),
        ("provider-opencode-go.cache_retention", "short"),
        ("provider-opencode-go.supports_long_cache_retention", "false"),
    ] {
        let out = mare_cmd()
            .args(["--strict-config", "--no-session", "--set", &format!("plugins.{key}={value}"), "-p", "hi"])
            .env("MARE_HOME", home.path())
            .current_dir(home.path())
            .run_output();
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(0), "{key}={value} failed under --strict-config: {err}");
    }

    let bad = mare_cmd()
        .args([
            "--strict-config",
            "--no-session",
            "--set",
            "plugins.provider-anthropic.unknown_cache_opt=1",
            "-p",
            "hi",
        ])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .run_output();
    let bad_err = String::from_utf8_lossy(&bad.stderr);
    assert_eq!(bad.status.code(), Some(2), "unknown plugin option must fail --strict-config: {bad_err}");
    assert!(bad_err.contains("has no option `unknown_cache_opt`"), "unexpected error: {bad_err}");
}

#[tokio::test]
async fn openai_compat_cache_retention_and_openrouter_anthropic() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("OPENROUTER_API_KEY", "test-or");
        env.set_var("WORK_OR_KEY", "test-wor");
        env.set_var("OA_COMPAT_KEY", "test-oac");
        let server = MockServer::start().await;
        let sse = compat_events(&[compat_text("ok"), compat_stop()]);
        server.route("/compat", vec![Resp::sse(sse.clone()); 2]);
        server.route("/openrouter", vec![Resp::sse(sse.clone()); 6]);
        server.route("/work-or", vec![Resp::sse(sse.clone()); 3]);
        server.route("/oa-compat", vec![Resp::sse(sse.clone()); 5]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "openrouter": {
                        "base_url": format!("{}/openrouter", server.base),
                        "cache_retention": "none"
                    },
                    "work-openrouter": {
                        "wire_api": "chat",
                        "base_url": format!("{}/work-or", server.base),
                        "api_key_env": "WORK_OR_KEY",
                        "effort_param": "openrouter"
                    },
                    "work-openrouter-case": {
                        "wire_api": "chat",
                        "base_url": "https://API.OpenRouter.ai/api/v1",
                        "api_key_env": "WORK_OR_KEY"
                    },
                    "oac": {
                        "wire_api": "chat",
                        "base_url": format!("{}/oa-compat", server.base),
                        "api_key_env": "OA_COMPAT_KEY",
                        "supports_prompt_cache_key": true,
                        "supports_long_cache_retention": true
                    }
                },
                "models": {
                    "oac/no-key-model": { "supports_prompt_cache_key": false }
                }
            }),
        )
        .await;

        exec(
            &h,
            r#"
            local ds = assert(LLM:resolve("compat/reasoner-model"))
            local or_anth = assert(LLM:resolve("openrouter/anthropic/claude-sonnet-4-5"))
            local work_or = assert(LLM:resolve("work-openrouter/anthropic/claude-sonnet-4-5"))
            local oac = assert(LLM:resolve("oac/gpt-4o"))
            local oac_nokey = assert(LLM:resolve("oac/no-key-model"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            -- 1. DeepSeek default ("short"): no prompt_cache_key / prompt_cache_retention
            LLM:stream({ model = ds, system = "S", session_id = "sess-ds", messages = msgs }, function() end)
            -- 2. Custom chat provider with supports_prompt_cache_key + "short": sends prompt_cache_key only
            LLM:stream({ model = oac, system = "S", session_id = "sess-oac1", messages = msgs, cache_retention = "short" }, function() end)
            -- 3. Custom chat provider with supports_long_cache_retention + "long": sends both
            LLM:stream({ model = oac, system = "S", session_id = "sess-oac2", messages = msgs, cache_retention = "long" }, function() end)
            -- 4. Custom chat provider with "none": omits both
            LLM:stream({ model = oac, system = "S", session_id = "sess-oac3", messages = msgs, cache_retention = "none" }, function() end)
            -- 5. Model supports_prompt_cache_key = false overrides provider true
            LLM:stream({ model = oac_nokey, system = "S", session_id = "sess-nk", messages = msgs, cache_retention = "short" }, function() end)
            -- 6. OpenRouter endpoint cache_retention="none" NOT shadowed by unrelated providers.openrouter.base_url
            LLM:stream({ model = or_anth, system = "S", session_id = "sess-or0", messages = msgs }, function() end)
            -- 7. OpenRouter anthropic/* with "short": adds cache_control { type = "ephemeral" }
            LLM:stream({ model = or_anth, system = "S", session_id = "sess-or1", messages = msgs, cache_retention = "short" }, function() end)
            -- 8. OpenRouter anthropic/* with "long": adds cache_control { type = "ephemeral", ttl = "1h" }
            LLM:stream({ model = or_anth, system = "S", session_id = "sess-or2", messages = msgs, cache_retention = "long" }, function() end)
            -- 9. Custom-named OpenRouter provider gets Anthropic cache_control
            LLM:stream({ model = work_or, system = "S", session_id = "sess-wor1", messages = msgs, cache_retention = "long" }, function() end)
            -- 10. Custom-named OpenRouter provider with mixed-case host https://API.OpenRouter.ai gets Anthropic cache_control
            local work_or_case = assert(LLM:resolve("work-openrouter-case/anthropic/claude-sonnet-4-5"))
            CAPTURED_CASE_BODY = nil
            local orig_req = mare.http.request
            mare.http.request = function(o)
                if type(o) == "table" and o.url == "https://API.OpenRouter.ai/api/v1/chat/completions" then
                    CAPTURED_CASE_BODY = o.json
                    local lines = {
                        'data: {"choices":[{"index":0,"delta":{"content":"ok"}}]}\n\n',
                        'data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\n',
                        'data: [DONE]\n\n',
                    }
                    local i = 0
                    return {
                        ok = true,
                        status = 200,
                        headers = {},
                        next_event = function()
                            i = i + 1
                            if i <= #lines then return { data = lines[i]:match("data: (.-)\n") } end
                            return nil
                        end,
                    }
                end
                return orig_req(o)
            end
            LLM:stream({ model = work_or_case, system = "S", messages = msgs, cache_retention = "long" }, function() end)
            mare.http.request = orig_req
            "#,
        )
        .await;

        let case_val: mlua::Value = h.lua().load("return CAPTURED_CASE_BODY").eval().unwrap();
        let case_body = mare_shared::lua::lua_to_json(h.lua(), case_val).unwrap();
        assert_eq!(case_body["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));
        assert_eq!(case_body["messages"][1]["content"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));

        let reqs = server.requests();
        assert_eq!(reqs.len(), 9);

        let b0 = reqs[0].json();
        assert!(b0.get("prompt_cache_key").is_none());
        assert!(b0.get("prompt_cache_retention").is_none());

        let b1 = reqs[1].json();
        assert_eq!(b1["prompt_cache_key"], "sess-oac1");
        assert!(b1.get("prompt_cache_retention").is_none());

        let b2 = reqs[2].json();
        assert_eq!(b2["prompt_cache_key"], "sess-oac2");
        assert_eq!(b2["prompt_cache_retention"], "24h");

        let b3 = reqs[3].json();
        assert!(b3.get("prompt_cache_key").is_none());
        assert!(b3.get("prompt_cache_retention").is_none());

        let b4 = reqs[4].json();
        assert!(b4.get("prompt_cache_key").is_none());

        let b5 = reqs[5].json();
        assert_eq!(b5["messages"][0]["content"], json!("S"));
        assert_eq!(b5["messages"][1]["content"], json!("hi"));

        let b6 = reqs[6].json();
        assert_eq!(b6["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral" }));
        assert_eq!(b6["messages"][1]["content"][0]["cache_control"], json!({ "type": "ephemeral" }));

        let b7 = reqs[7].json();
        assert_eq!(b7["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));
        assert_eq!(b7["messages"][1]["content"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));

        let b8 = reqs[8].json();
        assert_eq!(b8["messages"][0]["content"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));
        assert_eq!(b8["messages"][1]["content"][0]["cache_control"], json!({ "type": "ephemeral", "ttl": "1h" }));

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn invalid_cache_retention_is_rejected() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route("/anthropic", vec![Resp::sse(fixture("anthropic-text.sse"))]);
        let h = provider_host(&mut env, &server, json!({})).await;

        let v: mlua::Value = h
            .lua()
            .load(
                r#"
            local m = assert(LLM:resolve("anthropic/claude-sonnet-5-5"))
            return LLM:stream({
                model = m,
                system = "S",
                messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                cache_retention = "forever",
            }, function() end)
            "#,
            )
            .into_function()
            .unwrap()
            .call_async(())
            .await
            .unwrap();
        let res = mare_shared::lua::lua_to_json(h.lua(), v).unwrap();

        assert_eq!(res["stop_reason"], "error");
        let err = res["error"].as_str().unwrap_or("");
        assert!(err.contains("cache_retention"), "unexpected error: {err}");
        assert_eq!(server.requests().len(), 0, "invalid cache_retention must not send HTTP request");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn openai_responses_compact_omits_12h_ttl_with_long_retention() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let compact_resp = json!({
            "output": [
                { "type": "compaction", "encrypted_content": "ENC-COMPACTED" }
            ]
        });
        server.route("/openai/responses/compact", vec![Resp::status(200, compact_resp.to_string())]);
        server.route("/openai/responses", vec![Resp::sse(responses_text("ok"))]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "openai": { "cache_retention": "long" }
                }
            }),
        )
        .await;

        exec(
            &h,
            r#"
            CACHE_WARNS = {}
            local orig_notify = mare.ui and mare.ui.notify
            mare.ui = mare.ui or {}
            mare.ui.notify = function(msg, level)
                CACHE_WARNS[#CACHE_WARNS + 1] = { msg = tostring(msg), level = tostring(level) }
                if orig_notify then pcall(orig_notify, msg, level) end
            end

            local oa = assert(LLM:resolve("openai/gpt-6.1-terra"))
            local msgs = { { role = "user", content = { { type = "text", text = "hi" } } } }

            local res, err = LLM:compact_remote({
                model = oa,
                system = "S",
                session_id = "sess-compact",
                messages = msgs,
            })
            assert(res, tostring(err))

            LLM:stream({
                model = oa,
                system = "S",
                session_id = "sess-stream",
                messages = msgs,
            }, function() end)
            "#,
        )
        .await;

        let reqs = server.requests();
        assert_eq!(reqs.len(), 2);

        let compact_req = reqs
            .iter()
            .find(|r| r.path == "/openai/responses/compact")
            .expect("compact request");
        let b_compact = compact_req.json();
        assert_eq!(b_compact["prompt_cache_key"], "sess-compact");
        assert!(
            b_compact.get("prompt_cache_options").is_none(),
            "/responses/compact must not send prompt_cache_options with ttl=12h: {b_compact}"
        );

        let stream_req = reqs
            .iter()
            .find(|r| r.path == "/openai/responses")
            .expect("stream request");
        let b_stream = stream_req.json();
        assert_eq!(b_stream["prompt_cache_key"], "sess-stream");
        assert_eq!(
            b_stream["prompt_cache_options"],
            json!({ "ttl": "12h" }),
            "normal /responses must keep 12h ttl"
        );

        let warns_val: mlua::Value = h.lua().load("return CACHE_WARNS").eval().unwrap();
        let warns = mare_shared::lua::lua_to_json(h.lua(), warns_val).unwrap();
        let warn_arr = warns.as_array().expect("CACHE_WARNS array");
        assert!(
            warn_arr.iter().any(|w| {
                let msg = w["msg"].as_str().unwrap_or("");
                msg.contains("gpt-6.1-terra") && msg.contains("/responses/compact")
            }),
            "expected caller-visible ui.notify warning when long retention cannot be satisfied on /responses/compact: {warns}"
        );

        h.shutdown().await;
    })
    .await
}

/// On an api.openai.com Chat Completions endpoint, `prompt_cache_key` is decided once: retention
/// "none" and an explicit `supports_prompt_cache_key = false` both omit it, otherwise the session
/// id is sent clamped to 64 chars (pi `openai-completions.ts:808-821`). The request goes through a
/// `provider/transport` hook because the mock URL would bypass the OpenAI host branch.
#[tokio::test]
async fn openai_host_compat_prompt_cache_key_honors_none_and_explicit_false() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("CUSTOM_OAI_KEY", "test-custom-oai");
        let server = MockServer::start().await;
        let h = provider_host(
            &mut env,
            &server,
            json!({ "providers": {
                "custom_oai": {
                    "base_url": "https://api.openai.com/v1",
                    "api_key_env": "CUSTOM_OAI_KEY",
                    "models": [{ "model_id": "gpt-4o", "context_window": 128000, "max_output": 16384 }]
                },
                "nokey_oai": {
                    "base_url": "https://api.openai.com/v1",
                    "api_key_env": "CUSTOM_OAI_KEY",
                    "supports_prompt_cache_key": false,
                    "models": [{ "model_id": "gpt-4o", "context_window": 128000, "max_output": 16384 }]
                }
            } }),
        )
        .await;
        expose(&h);
        global(&h, "SSE", fixture("openai-compat.sse"));

        let out: String = eval(
            &h,
            r#"
            local bodies = {}
            C.cordis:on("provider/transport", function(t)
                bodies[#bodies + 1] = t.request.body
                local evs = {}
                for block in (SSE .. "\n\n"):gmatch("(.-)\n\n") do
                    local data = block:match("data: ([^\n]*)")
                    if data then evs[#evs + 1] = { data = data } end
                end
                local i = 0
                return { ok = true, status = 200, headers = {}, next_event = function() i = i + 1; return evs[i] end }
            end)
            local function go(ref, extra)
                local req = {
                    model = assert(LLM:resolve(ref)),
                    system = "S",
                    session_id = "sess-oai",
                    messages = { { role = "user", content = { { type = "text", text = "hi" } } } },
                }
                for k, v in pairs(extra or {}) do req[k] = v end
                LLM:stream(req, function() end)
            end
            go("custom_oai/gpt-4o")
            go("custom_oai/gpt-4o", { cache_retention = "none" })
            go("nokey_oai/gpt-4o")
            go("custom_oai/gpt-4o", { cache = false })
            local keys = {}
            for i, b in ipairs(bodies) do
                local body = type(b) == "string" and mare.json.decode(b) or b
                keys[i] = tostring(body.prompt_cache_key)
            end
            return table.concat(keys, "|")
            "#,
        )
        .await;
        assert_eq!(out, "sess-oai|nil|nil|nil");

        h.shutdown().await;
    })
    .await
}
