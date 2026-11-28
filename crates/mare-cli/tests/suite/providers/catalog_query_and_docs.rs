//! Provider directory query parity (Lua & RPC), retired options, documentation sync,
//! key-prefix login detection, fallback_models validation, and data-driven placeholder reasoning.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn write_user_plugin(env: &TestEnv, name: &str, code: &str) {
    let dir = env.config_dir().join("plugins").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("init.lua"), code).unwrap();
}

#[test]
fn list_providers_rows_lua_equals_rpc_and_excludes_unavailable_from_login_and_credentials() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let cfg_dir = home.join("config");
    let plugins_dir = cfg_dir.join("plugins");
    std::fs::create_dir_all(plugins_dir.join("vendor-broken")).unwrap();
    std::fs::write(
        plugins_dir.join("vendor-broken/init.lua"),
        r#"return {
  name = "vendor-broken",
  inject = { "llm" },
  providers = { "broken-vendor" },
  apply = function()
    error("vendor-broken init boom")
  end,
}"#,
    )
    .unwrap();

    let marker = root.path().join("key-cmd-ran");
    let marker_posix = marker.to_string_lossy().replace('\\', "/");
    let cmd_script = serde_json::to_string(&format!("touch '{marker_posix}' && echo cmd-secret-value")).unwrap();
    let init_lua = format!(
        r#"
mare.setup {{ model = "faux/echo" }}
mare.plugin("provider-anthropic", {{ enabled = false }})
mare.provider("bad-wire", {{
  wire = "nonexistent-wire",
  base_url = "http://127.0.0.1:9/v1",
}})
mare.provider("cmd-vendor", {{
  base_url = "http://127.0.0.1:9/v1",
  api_key = {{ cmd = {{ "sh", "-c", {cmd_script} }} }},
}})
mare.provider("lit-vendor", {{
  base_url = "http://127.0.0.1:9/v1",
  api_key = "super-secret-literal-987654",
}})
"#
    );
    std::fs::write(cfg_dir.join("init.lua"), &init_lua).unwrap();

    let script = root.path().join("faux.json");
    std::fs::write(&script, "[]").unwrap();
    let env_map = BTreeMap::from([
        ("MARE_HOME".to_string(), home.display().to_string()),
        ("MARE_FAUX_SCRIPT".to_string(), script.display().to_string()),
    ]);
    let removed = [
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "OPENCODE_API_KEY",
        "DEEPSEEK_API_KEY",
        "MOONSHOT_API_KEY",
        "KIMI_API_KEY",
        "OPENROUTER_API_KEY",
    ];
    let mut client = crate::common::rpc::RpcClient::spawn_with_env_removed(
        &["--rpc", "--peer", "--yolo", "--model", "faux/echo"],
        &env_map,
        root.path(),
        &removed,
    );
    client.call("initialize", json!({ "protocol_version": "3.0" }));

    let rpc_res = client.call("llm.list_providers", json!({}));
    let rpc_rows = rpc_res["data"].as_array().expect("rpc list_providers.data array");
    let rpc_text = serde_json::to_string(&rpc_res).unwrap();
    assert!(!rpc_text.contains("super-secret-literal-987654"), "list_providers leaked literal secret");
    assert!(!rpc_text.contains("cmd-secret-value"), "list_providers leaked command secret");
    assert!(!marker.exists(), "list_providers must never execute key helper commands");

    let row_of = |rows: &[Value], name: &str| -> Value {
        rows.iter()
            .find(|r| r["provider"] == name)
            .unwrap_or_else(|| panic!("missing provider `{name}` in rows: {rows:?}"))
            .clone()
    };

    let ant = row_of(rpc_rows, "anthropic");
    assert_eq!(ant["state"], "disabled", "{ant}");
    assert_eq!(ant["plugin"], "provider-anthropic", "{ant}");
    assert_eq!(ant["origin"], "plugin", "{ant}");
    assert_eq!(ant["logged_in"], false, "{ant}");
    assert!(ant["reason"].as_str().is_some_and(|s| s.contains("disabled")), "{ant}");

    let broken = row_of(rpc_rows, "broken-vendor");
    assert_eq!(broken["state"], "failed", "{broken}");
    assert_eq!(broken["plugin"], "vendor-broken", "{broken}");
    assert_eq!(broken["origin"], "plugin", "{broken}");
    assert_eq!(broken["logged_in"], false, "{broken}");
    assert!(broken["reason"].as_str().is_some_and(|s| s.contains("vendor-broken init boom")), "{broken}");

    let bad = row_of(rpc_rows, "bad-wire");
    assert_eq!(bad["state"], "unresolved", "{bad}");
    assert_eq!(bad["origin"], "config", "{bad}");
    assert_eq!(bad["wire"], "nonexistent-wire", "{bad}");
    assert_eq!(bad["logged_in"], false, "{bad}");
    assert!(bad["reason"].as_str().is_some_and(|s| s.contains("unknown wire")), "{bad}");

    let ds = row_of(rpc_rows, "deepseek");
    assert_eq!(ds["state"], "registered", "{ds}");
    assert_eq!(ds["origin"], "plugin", "{ds}");
    assert_eq!(ds["plugin"], "provider-openai-compat", "{ds}");
    assert_eq!(ds["wire"], "openai-responses", "{ds}");

    let lit = row_of(rpc_rows, "lit-vendor");
    assert_eq!(lit["state"], "registered", "{lit}");
    assert_eq!(lit["origin"], "config", "{lit}");
    assert_eq!(lit["wire"], "openai-chat", "{lit}");
    assert_eq!(lit["logged_in"], true, "{lit}");

    // Non-registered rows cannot be logged into over RPC.
    for unavailable in ["anthropic", "broken-vendor", "bad-wire"] {
        let err = client.call_err("llm.login", json!({ "provider": unavailable, "key": "k" }));
        assert_eq!(err["data"]["code"], "llm.unknown_provider", "{unavailable}: {err}");
    }

    client.shutdown_and_wait();

    // Compare against Lua llm:list_providers() under the exact same configuration.
    let lua_rows: Value = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            local(async {
                let mut env = TestEnv::new().await;
                for var in removed {
                    env.set_var(var, "");
                }
                write_user_plugin(
                    &env,
                    "vendor-broken",
                    r#"return {
  name = "vendor-broken",
  inject = { "llm" },
  providers = { "broken-vendor" },
  apply = function()
    error("vendor-broken init boom")
  end,
}"#,
                );
                let h = env
                    .host()
                    .no_session()
                    .user_plugins()
                    .faux(json!([]))
                    .config(json!({
                        "model": "faux/echo",
                        "plugins": { "provider-anthropic": { "enabled": false } },
                        "providers": {
                            "bad-wire": {
                                "wire": "nonexistent-wire",
                                "base_url": "http://127.0.0.1:9/v1"
                            },
                            "cmd-vendor": {
                                "base_url": "http://127.0.0.1:9/v1",
                                "api_key": { "cmd": ["sh", "-c", format!("touch {} && echo cmd-secret-value", marker.display())] }
                            },
                            "lit-vendor": {
                                "base_url": "http://127.0.0.1:9/v1",
                                "api_key": "super-secret-literal-987654"
                            }
                        }
                    }))
                    .build_unchecked_exposed()
                    .await;
                global(&h, "LLM", h.service_table("llm").unwrap());
                let json_str: String = eval(&h, r#"return mare.json.encode(LLM:list_providers().data)"#).await;
                let saw_disabled_line: bool = eval(
                    &h,
                    r#"
                    local lines = LLM:status_lines()
                    for _, l in ipairs(lines) do
                        if l.provider == "anthropic" and l.unavailable == true then
                            return true
                        end
                    end
                    return false
                    "#,
                )
                .await;
                assert!(saw_disabled_line, "status_lines must mark disabled rows as unavailable");
                h.shutdown().await;
                serde_json::from_str::<Value>(&json_str).unwrap()
            })
            .await
        });

    assert_eq!(lua_rows, Value::Array(rpc_rows.clone()), "Lua and RPC list_providers rows must match exactly");
    assert!(!marker.exists(), "Lua list_providers must never execute key helper commands");
}

#[tokio::test]
async fn every_keyed_preset_sets_models_dev_and_endpoints_option_is_retired() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
mare.setup { model = "faux/echo" }
mare.plugin("provider-openai-compat", {
  endpoints = {
    legacy = { base_url = "http://127.0.0.1:9/v1" },
  },
})
"#,
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (warnings_json, legacy_registered, missing_mdev_json): (String, bool, String) = eval(
            &h,
            r#"
            local warnings = mare.config_warnings()
            local legacy = LLM:provider("legacy") ~= nil
            local missing = {}
            for _, p in ipairs(LLM:providers()) do
                local full = LLM:provider(p.name)
                if full and full.wire == "openai-chat" and not full.no_key then
                    if full.models_dev == nil then
                        missing[#missing + 1] = p.name
                    end
                end
            end
            return mare.json.encode(mare.json.array(warnings)), legacy, mare.json.encode(mare.json.array(missing))
            "#,
        )
        .await;

        let warnings: Vec<String> = serde_json::from_str(&warnings_json).unwrap();
        let expected_retired =
            "plugin `provider-openai-compat` option `endpoints` has been retired; define each endpoint with `mare.provider(name, {...})`";
        assert!(
            warnings.iter().any(|w| w.contains(expected_retired)),
            "expected retired endpoints warning without duplicate prefix, got: {warnings:?}"
        );
        assert!(
            !warnings.iter().any(|w| w.matches("has been retired").count() > 1),
            "retired warning must not repeat prefix: {warnings:?}"
        );
        assert!(!legacy_registered, "retired endpoints option must register nothing");

        let missing_mdev: Vec<String> = serde_json::from_str(&missing_mdev_json).unwrap();
        assert!(
            missing_mdev.is_empty(),
            "every keyed built-in preset must explicitly set models_dev (id or false); missing: {missing_mdev:?}"
        );

        h.shutdown().await;
    })
    .await;
}

#[test]
fn docs_provider_table_equals_builtin_declarations() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap();
    let plugins_dir = root.join("crates/mare-plugin/preset/plugins");

    let lua = mlua::Lua::new();
    let mut expected = BTreeSet::new();
    for entry in std::fs::read_dir(&plugins_dir).unwrap() {
        let path = entry.unwrap().path();
        let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        if !fname.starts_with("provider-") || !fname.ends_with(".lua") || fname == "provider-faux.lua" {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        let def: mlua::Table = lua.load(&src).set_name(fname).eval().unwrap();
        let provs: mlua::Table = def.get("providers").unwrap();
        for pair in provs.sequence_values::<String>() {
            expected.insert(pair.unwrap());
        }
    }
    assert!(!expected.is_empty(), "expected non-empty built-in provider declarations");

    for locale in ["en", "zh", "ja"] {
        let doc_path = root.join(format!("docs/{locale}/use/models.md"));
        let doc = std::fs::read_to_string(&doc_path).unwrap();
        let mut in_table = false;
        let mut found = BTreeSet::new();
        for line in doc.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("| `") {
                in_table = true;
                if let Some(end) = rest.find('`') {
                    found.insert(rest[..end].to_string());
                }
            } else if in_table {
                break;
            }
        }
        assert_eq!(found, expected, "docs/{locale}/use/models.md provider table must match built-in declarations");
    }
}

#[tokio::test]
async fn login_identifies_provider_from_declared_key_prefixes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({
                "model": "faux/echo",
                "providers": {
                    "acme": {
                        "base_url": "http://127.0.0.1:9/v1",
                        "key_prefixes": ["acme-live-"]
                    },
                    "dup-a": {
                        "base_url": "http://127.0.0.1:9/v1",
                        "key_prefixes": ["shared-pfx-"]
                    },
                    "dup-b": {
                        "base_url": "http://127.0.0.1:9/v1",
                        "key_prefixes": ["shared-pfx-"]
                    }
                }
            }))
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let res: String = eval(
            &h,
            r#"
            return mare.json.encode({
                anthropic = LLM:identify_key_provider("sk-ant-api03-example") or mare.json.null,
                openrouter = LLM:identify_key_provider("sk-or-v1-example") or mare.json.null,
                google = LLM:identify_key_provider("AIzaSyExampleKey123") or mare.json.null,
                acme = LLM:identify_key_provider("acme-live-secret") or mare.json.null,
                shared_sk = LLM:identify_key_provider("sk-proj-12345") or mare.json.null,
                ambiguous = LLM:identify_key_provider("shared-pfx-123") or mare.json.null,
                unknown = LLM:identify_key_provider("random-key") or mare.json.null,
            })
            "#,
        )
        .await;
        let v: Value = serde_json::from_str(&res).unwrap();
        assert_eq!(v["anthropic"], "anthropic", "{v}");
        assert_eq!(v["openrouter"], "openrouter", "{v}");
        assert_eq!(v["google"], "google", "{v}");
        assert_eq!(v["acme"], "acme", "{v}");
        assert_eq!(v["shared_sk"], Value::Null, "shared sk- prefix must not identify a provider: {v}");
        assert_eq!(v["ambiguous"], Value::Null, "ambiguous custom prefix must return nil: {v}");
        assert_eq!(v["unknown"], Value::Null, "{v}");

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn fallback_models_rejects_more_than_three_at_config_time() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
mare.setup {
  model = "faux/echo",
  fallback_models = { "faux/a", "faux/b", "faux/c", "faux/d" },
}
"#,
        )
        .unwrap();
        let h = env.host().user_config(false).no_session().build().await;
        global(&h, "BUS", h.service_table("bus").unwrap());

        let (warnings_json, kept_fallback, doctor_json): (String, bool, String) = eval(
            &h,
            r#"
            local checks = {}
            BUS:emit("kernel/doctor", checks)
            return mare.json.encode(mare.json.array(mare.config_warnings())),
                mare.config.fallback_models ~= nil,
                mare.json.encode(mare.json.array(checks))
            "#,
        )
        .await;

        let warnings: Vec<String> = serde_json::from_str(&warnings_json).unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("error:") && w.contains("fallback_models") && w.contains("3")),
            "expected config-time schema error when fallback_models exceeds max_items=3: {warnings:?}"
        );
        assert!(!kept_fallback, "invalid fallback_models list (>3 items) must be rejected at config time");

        let checks: Vec<Value> = serde_json::from_str(&doctor_json).unwrap();
        assert!(
            !checks.iter().any(|c| c["name"] == "fallback_models"),
            "fallback_models validation must live only in the option schema, not a duplicate doctor check: {checks:?}"
        );

        h.shutdown().await;
        drop(env);

        let ok_env = TestEnv::new().await;
        std::fs::create_dir_all(ok_env.config_dir()).unwrap();
        std::fs::write(
            ok_env.config_dir().join("init.lua"),
            r#"
mare.setup {
  model = "faux/echo",
  fallback_models = { "faux/a", "faux/b", "faux/c" },
}
"#,
        )
        .unwrap();
        let ok_h = ok_env.host().user_config(false).no_session().build().await;
        let (ok_warnings_json, ok_len): (String, usize) = eval(
            &ok_h,
            r#"
            return mare.json.encode(mare.json.array(mare.config_warnings())),
                type(mare.config.fallback_models) == "table" and #mare.config.fallback_models or 0
            "#,
        )
        .await;
        let ok_warnings: Vec<String> = serde_json::from_str(&ok_warnings_json).unwrap();
        assert!(ok_warnings.is_empty(), "3 fallback_models must produce zero warnings: {ok_warnings:?}");
        assert_eq!(ok_len, 3, "valid 3-item fallback_models must be retained");
        ok_h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn reasoning_placeholder_follows_data_capability_flag_before_heuristics() {
    local(async {
        let chat_sse = compat_events(&[compat_text("ok"), compat_stop()]);
        let resp_sse = responses_text("ok");

        let server = MockServer::start().await;
        server.route("/cap-chat/chat/completions", vec![Resp::sse(chat_sse.clone())]);
        server.route("/optout-chat/chat/completions", vec![Resp::sse(chat_sse)]);
        server.route("/cap-resp/responses", vec![Resp::sse(resp_sse.clone())]);
        server.route("/optout-resp/responses", vec![Resp::sse(resp_sse)]);

        let env = TestEnv::new().await;
        let base = &server.base;
        let h = env
            .host()
            .no_session()
            .config(json!({
                "model": "faux/echo",
                "providers": {
                    "cap-chat": {
                        "base_url": format!("{base}/cap-chat"),
                        "no_key": true,
                        "models": [{ "model_id": "acme-reasoner-v1", "needs_reasoning_placeholder": true }]
                    },
                    "optout-chat": {
                        "base_url": format!("{base}/optout-chat"),
                        "no_key": true,
                        "models": [{ "model_id": "deepseek-v3-custom", "needs_reasoning_placeholder": false }]
                    },
                    "cap-resp": {
                        "wire_api": "responses",
                        "base_url": format!("{base}/cap-resp"),
                        "no_key": true,
                        "models": [{ "model_id": "acme-resp-v1", "needs_reasoning_placeholder": true }]
                    },
                    "optout-resp": {
                        "wire_api": "responses",
                        "base_url": format!("{base}/optout-resp"),
                        "no_key": true,
                        "models": [{ "model_id": "deepseek-r1-custom", "needs_reasoning_placeholder": false }]
                    }
                }
            }))
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        for model_ref in [
            "cap-chat/acme-reasoner-v1",
            "optout-chat/deepseek-v3-custom",
            "cap-resp/acme-resp-v1",
            "optout-resp/deepseek-r1-custom",
        ] {
            let ok: bool = h
                .lua()
                .load(
                    r#"
                    local ref = ...
                    local m = assert(LLM:resolve(ref), "missing " .. ref)
                    local msg = LLM:stream({
                      model = m,
                      system = "sys",
                      tools = { { name = "ping", description = "ping", input_schema = { type = "object" } } },
                      messages = {
                        { role = "user", content = { { type = "text", text = "hi" } } },
                        { role = "assistant", content = { { type = "tool_use", id = "c1", name = "ping", input = {} } } },
                        { role = "tool", tool_call_id = "c1", content = { { type = "text", text = "pong" } } },
                      },
                    }, function() end)
                    assert(msg.stop_reason ~= "error", ref .. ": " .. tostring(msg.error))
                    return true
                    "#,
                )
                .into_function()
                .unwrap()
                .call_async::<bool>(model_ref.to_string())
                .await
                .unwrap();
            assert!(ok);
        }

        let req_by_path = |p: &str| {
            server
                .requests()
                .into_iter()
                .find(|r| r.path.starts_with(p))
                .unwrap_or_else(|| panic!("missing request for {p}"))
                .json()
        };

        let cap_chat = req_by_path("/cap-chat/");
        let cap_chat_asst = cap_chat["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert!(
            cap_chat_asst.get("reasoning_content").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()),
            "needs_reasoning_placeholder=true must inject placeholder on chat wire: {cap_chat_asst}"
        );

        let optout_chat = req_by_path("/optout-chat/");
        let optout_chat_asst =
            optout_chat["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap();
        assert!(
            optout_chat_asst.get("reasoning_content").is_none(),
            "needs_reasoning_placeholder=false must disable heuristic placeholder on chat wire: {optout_chat_asst}"
        );

        let cap_resp = req_by_path("/cap-resp/");
        assert!(
            cap_resp["input"].as_array().unwrap().iter().any(|item| item["type"] == "reasoning"),
            "needs_reasoning_placeholder=true must inject reasoning placeholder on responses wire: {}",
            cap_resp["input"]
        );

        let optout_resp = req_by_path("/optout-resp/");
        assert!(
            !optout_resp["input"].as_array().unwrap().iter().any(|item| item["type"] == "reasoning"),
            "needs_reasoning_placeholder=false must disable heuristic placeholder on responses wire: {}",
            optout_resp["input"]
        );

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn models_dev_enriches_without_routing_honors_false_and_maps_interleaved() {
    local(async {
        let server = MockServer::start().await;
        let base = &server.base;
        let chat_sse = compat_events(&[compat_text("ok"), compat_stop()]);

        server.route(
            "/models.dev.json",
            vec![Resp::status(
                200,
                json!({
                    "moonshotai": {
                        "api": "https://rogue-endpoint.invalid/v1",
                        "env": ["ROGUE_API_KEY"],
                        "models": {
                            "kimi-listed": {
                                "name": "Kimi Listed From ModelsDev",
                                "reasoning": true,
                                "interleaved": { "field": "reasoning_content" },
                                "limit": { "context": 300000, "output": 16000 }
                            },
                            "kimi-str": {
                                "name": "Kimi String Interleaved",
                                "reasoning": true,
                                "interleaved": "reasoning_content",
                                "limit": { "context": 200000, "output": 8000 }
                            },
                            "kimi-bool": {
                                "name": "Kimi Bool Interleaved",
                                "reasoning": true,
                                "interleaved": true,
                                "limit": { "context": 200000, "output": 8000 }
                            }
                        }
                    },
                    "no-enrich": {
                        "models": {
                            "plain-m": {
                                "name": "Should Not Be Enriched",
                                "limit": { "context": 999999, "output": 88888 }
                            }
                        }
                    }
                })
                .to_string(),
            )],
        );
        server.route(
            "/kimi/models",
            vec![Resp::status(
                200,
                json!({ "data": [{ "id": "kimi-listed" }, { "id": "kimi-str" }, { "id": "kimi-bool" }] }).to_string(),
            )],
        );
        server.route(
            "/kimi/chat/completions",
            vec![Resp::sse(chat_sse.clone()), Resp::sse(chat_sse.clone()), Resp::sse(chat_sse)],
        );
        server
            .route("/no-enrich/models", vec![Resp::status(200, json!({ "data": [{ "id": "plain-m" }] }).to_string())]);

        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({
                "model": "faux/echo",
                "plugins": {
                    "llm": {
                        "models_dev_url": format!("{base}/models.dev.json"),
                        "catalog_refresh_ms": 1
                    }
                },
                "providers": {
                    "kimi": {
                        "base_url": format!("{base}/kimi"),
                        "api_key": "kimi-test-key"
                    },
                    "no-enrich": {
                        "base_url": format!("{base}/no-enrich"),
                        "no_key": true,
                        "models_dev": false
                    }
                }
            }))
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (kimi_json, no_enrich_json, kimi_base, kimi_key_src): (String, String, String, String) = eval(
            &h,
            r#"
            LLM:refresh_catalog({ force = true })
            local km = assert(LLM:resolve("kimi/kimi-listed"), "kimi/kimi-listed missing")
            local ks = assert(LLM:resolve("kimi/kimi-str"), "kimi/kimi-str missing")
            local kb = assert(LLM:resolve("kimi/kimi-bool"), "kimi/kimi-bool missing")
            local nm = assert(LLM:resolve("no-enrich/plain-m"), "no-enrich/plain-m missing")
            for _, m in ipairs({ km, ks, kb }) do
                local msg = LLM:stream({
                  model = m,
                  system = "sys",
                  tools = { { name = "ping", description = "ping", input_schema = { type = "object" } } },
                  messages = {
                    { role = "user", content = { { type = "text", text = "hi" } } },
                    { role = "assistant", content = { { type = "tool_use", id = "c1", name = "ping", input = {} } } },
                    { role = "tool", tool_call_id = "c1", content = { { type = "text", text = "pong" } } },
                  },
                }, function() end)
                assert(msg.stop_reason ~= "error", m.ref .. " stream failed: " .. tostring(msg.error))
            end
            return mare.json.encode({
                name = km.name,
                context_window = km.context_window,
                max_output = km.max_output,
                table_flag = km.needs_reasoning_placeholder,
                str_flag = ks.needs_reasoning_placeholder,
                bool_flag = kb.needs_reasoning_placeholder,
            }), mare.json.encode({
                name = nm.name,
                context_window = nm.context_window,
                max_output = nm.max_output,
            }), LLM:base_url("kimi", "fallback"), tostring(LLM:api_key_source("kimi", { "MOONSHOT_API_KEY" }))
            "#,
        )
        .await;

        let km: Value = serde_json::from_str(&kimi_json).unwrap();
        assert_eq!(km["name"], "Kimi Listed From ModelsDev", "{km}");
        assert_eq!(km["context_window"], 300000, "{km}");
        assert_eq!(km["max_output"], 16000, "{km}");
        assert_eq!(
            km["table_flag"], true,
            "models.dev interleaved={{field=\"reasoning_content\"}} must map to true: {km}"
        );
        assert_eq!(km["str_flag"], true, "models.dev interleaved=\"reasoning_content\" must map to true: {km}");
        assert!(
            km.get("bool_flag").is_none_or(Value::is_null),
            "models.dev interleaved=true must map to nil, not true: {km}"
        );

        // models.dev `api`/`env` must never alter endpoint or credential lookup.
        assert_eq!(kimi_base, format!("{base}/kimi"));
        assert!(!kimi_key_src.contains("ROGUE_API_KEY"), "{kimi_key_src}");

        let kimi_reqs: Vec<Value> =
            server.requests().into_iter().filter(|r| r.path == "/kimi/chat/completions").map(|r| r.json()).collect();
        assert_eq!(kimi_reqs.len(), 3, "expected 3 kimi stream requests");
        let asst_of = |model_id: &str| {
            let body = kimi_reqs.iter().find(|b| b["model"] == model_id).unwrap();
            body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone()
        };
        assert!(
            asst_of("kimi-listed").get("reasoning_content").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()),
            "table interleaved must inject placeholder"
        );
        assert!(
            asst_of("kimi-str").get("reasoning_content").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()),
            "string interleaved must inject placeholder"
        );
        assert!(
            asst_of("kimi-bool").get("reasoning_content").is_none(),
            "boolean interleaved=true must NOT inject placeholder: {}",
            asst_of("kimi-bool")
        );

        let nm: Value = serde_json::from_str(&no_enrich_json).unwrap();
        assert_eq!(nm["name"], "plain-m", "models_dev = false must skip enrichment: {nm}");
        assert_ne!(nm["context_window"], 999999, "models_dev = false must not pull limits from models.dev: {nm}");

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn effective_config_drops_deleted_fields_and_endpoint_reads_live_headers_dialect_and_base_url() {
    local(async {
        let server = MockServer::start().await;
        let base = &server.base;
        let chat_sse = compat_events(&[compat_text("ok"), compat_stop()]);
        for path in ["/ds-v1/responses", "/ds-v2/responses"] {
            server.route(path, vec![Resp::sse(responses_text("ok"))]);
        }
        for path in ["/cust-v1/chat/completions", "/cust-v2/chat/completions"] {
            server.route(path, vec![Resp::sse(chat_sse.clone())]);
        }

        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({
                "model": "faux/echo",
                "providers": {
                    "deepseek": {
                        "base_url": format!("{base}/ds-v1"),
                        "api_key": "k-ds",
                        "headers": { "x-temp": "initial" }
                    },
                    "cust": {
                        "base_url": format!("{base}/cust-v1"),
                        "no_key": true,
                        "dialect": "deepseek",
                        "headers": { "x-custom": "old" },
                        "models": [{ "model_id": "neutral-m" }]
                    }
                }
            }))
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (ds_hdr_after, cust_hdr_after, cust_dialect_after): (bool, bool, bool) = h
            .lua()
            .load(
                r#"
                local base = ...
                local function call_once(ref)
                    local m = assert(LLM:resolve(ref), "missing " .. ref)
                    local msg = LLM:stream({
                        model = m,
                        system = "sys",
                        tools = { { name = "ping", description = "ping", input_schema = { type = "object" } } },
                        messages = {
                            { role = "user", content = { { type = "text", text = "hi" } } },
                            { role = "assistant", content = { { type = "tool_use", id = "c1", name = "ping", input = {} } } },
                            { role = "tool", tool_call_id = "c1", content = { { type = "text", text = "pong" } } },
                        },
                    }, function() end)
                    assert(msg.stop_reason ~= "error", ref .. ": " .. tostring(msg.error))
                end

                call_once("deepseek/deepseek-flash")
                call_once("cust/neutral-m")

                mare.config.providers.deepseek.headers = nil
                mare.config.providers.deepseek.base_url = base .. "/ds-v2"
                mare.config.providers.cust.headers = nil
                mare.config.providers.cust.dialect = nil
                mare.config.providers.cust.base_url = base .. "/cust-v2"

                call_once("deepseek/deepseek-flash")
                call_once("cust/neutral-m")

                return LLM:provider_config("deepseek").headers ~= nil,
                    LLM:provider_config("cust").headers ~= nil,
                    LLM:provider_config("cust").dialect ~= nil
                "#,
            )
            .into_function()
            .unwrap()
            .call_async(base.clone())
            .await
            .unwrap();

        assert!(!ds_hdr_after, "deleted overlay headers must not linger in effective_config(deepseek)");
        assert!(!cust_hdr_after, "deleted config headers must not linger in effective_config(cust)");
        assert!(!cust_dialect_after, "deleted config dialect must not linger in effective_config(cust)");

        let reqs = server.requests();
        let req_for = |p: &str| {
            reqs.iter().find(|r| r.path == p).unwrap_or_else(|| panic!("missing request for {p}"))
        };

        assert_eq!(req_for("/ds-v1/responses").header("x-temp"), Some("initial"));
        assert_eq!(
            req_for("/ds-v2/responses").header("x-temp"),
            None,
            "endpoint must stop sending deleted overlay header on subsequent requests"
        );

        let c1 = req_for("/cust-v1/chat/completions");
        assert_eq!(c1.header("x-custom"), Some("old"));
        let c1_asst = c1.json()["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone();
        assert!(c1_asst.get("reasoning_content").is_some(), "dialect=deepseek must inject placeholder before deletion");

        let c2 = req_for("/cust-v2/chat/completions");
        assert_eq!(c2.header("x-custom"), None, "endpoint must stop sending deleted config header");
        let c2_asst = c2.json()["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone();
        assert!(
            c2_asst.get("reasoning_content").is_none(),
            "endpoint must read live dialect after runtime deletion: {c2_asst}"
        );

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn reasoning_placeholder_precedence_user_model_provider_models_dev_preset_fallback() {
    local(async {
        let server = MockServer::start().await;
        let base = &server.base;
        let chat_sse = compat_events(&[compat_text("ok"), compat_stop()]);

        server.route(
            "/models.dev.json",
            vec![Resp::status(
                200,
                json!({
                    "deepseek": {
                        "models": {
                            "ds-mdev": {
                                "name": "DS Mdev",
                                "reasoning": true,
                                "interleaved": { "field": "reasoning_content" }
                            },
                            "ds-user-on": {
                                "name": "DS User On",
                                "reasoning": true,
                                "interleaved": { "field": "reasoning_content" }
                            }
                        }
                    },
                    "pack-dev": {
                        "models": {
                            "pack-mdev": {
                                "name": "Pack Mdev",
                                "reasoning": true,
                                "interleaved": "reasoning_content"
                            },
                            "pack-plain": {
                                "name": "Pack Plain",
                                "reasoning": false
                            }
                        }
                    }
                })
                .to_string(),
            )],
        );
        server.route(
            "/ds/models",
            vec![Resp::status(
                200,
                json!({ "data": [{ "id": "ds-mdev" }, { "id": "ds-user-on" }] }).to_string(),
            )],
        );
        server.route(
            "/pack/models",
            vec![Resp::status(
                200,
                json!({ "data": [{ "id": "pack-mdev" }, { "id": "pack-plain" }] }).to_string(),
            )],
        );
        server.route("/ds/chat/completions", vec![Resp::sse(chat_sse.clone()), Resp::sse(chat_sse.clone())]);
        server.route("/pack/chat/completions", vec![Resp::sse(chat_sse.clone()), Resp::sse(chat_sse)]);

        let env = TestEnv::new().await;
        write_user_plugin(
            &env,
            "pack-placeholder",
            &format!(
                r#"return {{
  name = "pack-placeholder",
  inject = {{ "llm" }},
  providers = {{ "pack-false" }},
  apply = function(ctx)
    ctx.llm:register_provider({{
      provider = "pack-false",
      wire = "openai-chat",
      base_url = "{base}/pack",
      no_key = true,
      models_dev = "pack-dev",
      needs_reasoning_placeholder = false,
      models = {{ {{ model_id = "deepseek-seed-off" }} }},
    }})
  end,
}}"#
            ),
        );

        let h = env
            .host()
            .no_session()
            .user_plugins()
            .config(json!({
                "model": "faux/echo",
                "plugins": {
                    "llm": {
                        "models_dev_url": format!("{base}/models.dev.json"),
                        "catalog_refresh_ms": 1
                    }
                },
                "models": {
                    "deepseek/ds-user-on": { "needs_reasoning_placeholder": true }
                },
                "providers": {
                    "deepseek": {
                        "wire": "openai-chat",
                        "base_url": format!("{base}/ds"),
                        "api_key": "k-ds",
                        "needs_reasoning_placeholder": false
                    }
                }
            }))
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let ok: bool = eval(
            &h,
            r#"
            LLM:refresh_catalog({ force = true })
            for _, ref in ipairs({
                "deepseek/ds-mdev",
                "deepseek/ds-user-on",
                "pack-false/pack-mdev",
                "pack-false/deepseek-seed-off",
            }) do
                local m = assert(LLM:resolve(ref), "missing " .. ref)
                local msg = LLM:stream({
                    model = m,
                    system = "sys",
                    tools = { { name = "ping", description = "ping", input_schema = { type = "object" } } },
                    messages = {
                        { role = "user", content = { { type = "text", text = "hi" } } },
                        { role = "assistant", content = { { type = "tool_use", id = "c1", name = "ping", input = {} } } },
                        { role = "tool", tool_call_id = "c1", content = { { type = "text", text = "pong" } } },
                    },
                }, function() end)
                assert(msg.stop_reason ~= "error", ref .. ": " .. tostring(msg.error))
            end
            return true
            "#,
        )
        .await;
        assert!(ok);

        let reqs = server.requests();
        let asst_for = |path: &str, model_id: &str| {
            let body = reqs
                .iter()
                .filter(|r| r.path == path)
                .map(|r| r.json())
                .find(|b| b["model"] == model_id)
                .unwrap_or_else(|| panic!("missing request {path} model {model_id}"));
            body["messages"].as_array().unwrap().iter().find(|m| m["role"] == "assistant").unwrap().clone()
        };

        // Tier 2 (user provider = false) beats Tier 3 (models.dev = true), Tier 4 (deepseek preset = true), and Tier 5 (heuristic).
        assert!(
            asst_for("/ds/chat/completions", "ds-mdev").get("reasoning_content").is_none(),
            "user provider needs_reasoning_placeholder=false must beat models.dev, preset, and fallback"
        );

        // Tier 1 (user model = true) beats Tier 2 (user provider = false).
        assert!(
            asst_for("/ds/chat/completions", "ds-user-on").get("reasoning_content").is_some(),
            "user model needs_reasoning_placeholder=true must beat user provider false"
        );

        // Tier 3 (models.dev = true) beats Tier 4 (preset = false).
        assert!(
            asst_for("/pack/chat/completions", "pack-mdev").get("reasoning_content").is_some(),
            "models.dev interleaved must beat preset needs_reasoning_placeholder=false"
        );

        // Tier 4 (preset = false) beats Tier 5 (deepseek substring heuristic).
        assert!(
            asst_for("/pack/chat/completions", "deepseek-seed-off").get("reasoning_content").is_none(),
            "preset needs_reasoning_placeholder=false must beat deepseek substring fallback"
        );

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn all_builtin_provider_presets_expose_current_models_and_metadata() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({ "model": "faux/echo" })).build().await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let info_json: String = eval(
            &h,
            r#"
            local by_prov = {}
            for _, m in ipairs(LLM:models()) do
                by_prov[m.provider] = by_prov[m.provider] or {}
                by_prov[m.provider][m.id] = m
            end
            local ds_p = assert(LLM:provider("deepseek"))
            local kimi_p = assert(LLM:provider("kimi"))
            local kc_p = assert(LLM:provider("kimi-code"))
            local ant_p = assert(LLM:provider("anthropic"))
            local oai_p = assert(LLM:provider("openai"))
            local goo_p = assert(LLM:provider("google"))
            local og_p = assert(LLM:provider("opencode-go"))

            local flash = assert(by_prov.deepseek["deepseek-flash"])
            local pro = assert(by_prov.deepseek["deepseek-v4-pro"])
            local k31 = assert(by_prov.kimi["kimi-k3.1-pro"])
            local k3 = assert(by_prov.kimi["kimi-k3"])
            local kc31 = assert(by_prov["kimi-code"]["k3.1-pro"])
            local or_sonnet = assert(by_prov.openrouter["anthropic/claude-sonnet-5.5"])
            local or_ds = assert(by_prov.openrouter["deepseek/deepseek-v4.1-flash"])
            local or_gem = assert(by_prov.openrouter["google/gemini-4-argon"])
            local fable = assert(by_prov.anthropic["claude-fable-5-2"])
            local sonnet = assert(by_prov.anthropic["claude-sonnet-5-5"])
            local gpt62 = assert(by_prov.openai["gpt-6.2-sol"])
            local gpt62a = assert(by_prov.openai["gpt-6.2-astra"])
            local luna = assert(by_prov.openai["gpt-6-luna"])
            local argon = assert(by_prov.google["gemini-4-argon"])
            local hydro = assert(by_prov.google["gemini-4-hydrogen"])
            local g38 = assert(by_prov.google["gemini-3.8-flash"])
            local og_pro = assert(by_prov["opencode-go"]["deepseek-v4-pro"])

            return mare.json.encode({
                ds_default = ds_p.default_model,
                ds_wire = ds_p.wire,
                ds_has_legacy = by_prov.deepseek["deepseek-chat"] ~= nil
                    or by_prov.deepseek["deepseek-reasoner"] ~= nil
                    or by_prov.deepseek["deepseek-v4-flash"] ~= nil,
                flash_ctx = flash.context_window,
                flash_out = flash.max_output,
                flash_images = flash.images,
                flash_efforts = flash.efforts,
                flash_default_effort = flash.default_effort,
                flash_clamped_xhigh = LLM:clamp_effort(flash, "xhigh"),
                flash_tool_update = flash.tool_update,
                flash_cost = flash.cost,
                pro_ctx = pro.context_window,
                pro_out = pro.max_output,
                pro_images = pro.images,
                pro_tool_update = pro.tool_update or mare.json.null,
                pro_cost = pro.cost,
                kimi_default = kimi_p.default_model,
                kimi_has_k2 = by_prov.kimi["kimi-k2-0905-preview"] ~= nil or by_prov.kimi["kimi-k2-thinking"] ~= nil,
                k31_ctx = k31.context_window,
                k31_out = k31.max_output,
                k31_images = k31.images,
                k3_ctx = k3.context_window,
                kc_default = kc_p.default_model,
                kc_mdev = kc_p.models_dev,
                kc31_ctx = kc31.context_window,
                or_default = LLM:provider("openrouter").default_model,
                or_sonnet_ctx = or_sonnet.context_window,
                or_has_legacy = by_prov.openrouter["deepseek/deepseek-chat"] ~= nil or by_prov.openrouter["google/gemini-2.5-pro"] ~= nil,
                or_ds_ctx = or_ds.context_window,
                or_gem_ctx = or_gem.context_window,
                ant_default = ant_p.default_model,
                ant_has_fable51 = by_prov.anthropic["claude-fable-5-1"] ~= nil,
                fable_ctx = fable.context_window,
                fable_out = fable.max_output,
                fable_cost = fable.cost,
                sonnet_ctx = sonnet.context_window,
                sonnet_out = sonnet.max_output,
                sonnet_cost = sonnet.cost,
                oai_default = oai_p.default_model,
                luna_efforts = luna.efforts,
                luna_default_effort = luna.default_effort,
                gpt62_ctx = gpt62.context_window,
                gpt62_out = gpt62.max_output,
                gpt62_cost = gpt62.cost,
                gpt62a_cost = gpt62a.cost,
                goo_default = goo_p.default_model,
                goo_has_legacy25 = by_prov.google["gemini-2.5-pro"] ~= nil or by_prov.google["gemini-2.5-flash"] ~= nil,
                argon_ctx = argon.context_window,
                argon_efforts = argon.efforts,
                hydro_efforts = hydro.efforts,
                g38_cost = g38.cost,
                og_default = og_p.default_model,
                og_mdev = og_p.models_dev,
                og_has_crouton = by_prov["opencode-go"]["crouton-alpha-free"] ~= nil,
                og_pro_ctx = og_pro.context_window,
            })
            "#,
        )
        .await;
        let v: Value = serde_json::from_str(&info_json).unwrap();
        assert_eq!(v["ds_default"], "deepseek-flash", "{v}");
        assert_eq!(v["ds_wire"], "openai-responses", "{v}");
        assert_eq!(v["ds_has_legacy"], false, "{v}");
        assert_eq!(v["flash_ctx"], 1_048_576, "{v}");
        assert_eq!(v["flash_out"], 393_216, "{v}");
        assert_eq!(v["flash_images"], true, "{v}");
        assert_eq!(v["flash_efforts"], json!(["off", "low", "high", "max"]), "{v}");
        assert_eq!(v["flash_default_effort"], "high", "{v}");
        assert_eq!(v["flash_clamped_xhigh"], "max", "{v}");
        assert_eq!(v["flash_tool_update"], "addition_only", "{v}");
        assert_eq!(v["flash_cost"], json!({ "input": 0.15, "output": 0.6, "cache_read": 0.003 }), "{v}");
        assert_eq!(v["pro_ctx"], 1_048_576, "{v}");
        assert_eq!(v["pro_out"], 393_216, "{v}");
        assert_eq!(v["pro_images"], false, "{v}");
        assert_eq!(v["pro_tool_update"], Value::Null, "{v}");
        assert_eq!(v["pro_cost"], json!({ "input": 0.66, "output": 1.98, "cache_read": 0.022 }), "{v}");
        assert_eq!(v["kimi_default"], "kimi-k3.1-pro", "{v}");
        assert_eq!(v["kimi_has_k2"], false, "{v}");
        assert_eq!(v["k31_ctx"], 1_048_576, "{v}");
        assert_eq!(v["k31_out"], 131_072, "{v}");
        assert_eq!(v["k31_images"], true, "{v}");
        assert_eq!(v["k3_ctx"], 1_048_576, "{v}");
        assert_eq!(v["kc_default"], "kimi-for-coding", "{v}");
        assert_eq!(v["kc_mdev"], "kimi-code-plan-global", "{v}");
        assert_eq!(v["kc31_ctx"], 1_048_576, "{v}");
        assert_eq!(v["or_default"], "anthropic/claude-sonnet-5.5", "{v}");
        assert_eq!(v["or_sonnet_ctx"], 1_000_000, "{v}");
        assert_eq!(v["or_has_legacy"], false, "{v}");
        assert_eq!(v["or_ds_ctx"], 1_048_576, "{v}");
        assert_eq!(v["or_gem_ctx"], 1_048_576, "{v}");
        assert_eq!(v["ant_default"], "claude-sonnet-5-5", "{v}");
        assert_eq!(v["ant_has_fable51"], false, "{v}");
        assert_eq!(v["fable_ctx"], 1_000_000, "{v}");
        assert_eq!(v["fable_out"], 128_000, "{v}");
        assert_eq!(v["fable_cost"], json!({ "input": 8, "output": 40, "cache_read": 0.2, "cache_write": 10 }), "{v}");
        assert_eq!(v["sonnet_ctx"], 1_000_000, "{v}");
        assert_eq!(v["sonnet_out"], 128_000, "{v}");
        assert_eq!(v["sonnet_cost"], json!({ "input": 2, "output": 10, "cache_read": 0.1, "cache_write": 2.5 }), "{v}");
        assert_eq!(v["oai_default"], "gpt-6-luna", "{v}");
        assert_eq!(v["luna_efforts"], json!(["off", "low", "medium", "high", "xhigh", "max"]), "{v}");
        assert_eq!(v["luna_default_effort"], "max", "{v}");
        assert_eq!(v["gpt62_ctx"], 922_000, "{v}");
        assert_eq!(v["gpt62_out"], 128_000, "{v}");
        assert_eq!(v["gpt62_cost"], json!({ "input": 2, "output": 10, "cache_read": 0.1, "cache_write": 2.5 }), "{v}");
        assert_eq!(v["gpt62a_cost"], json!({ "input": 8, "output": 40, "cache_read": 0.4, "cache_write": 10 }), "{v}");
        assert_eq!(v["goo_default"], "gemini-4-argon", "{v}");
        assert_eq!(v["goo_has_legacy25"], false, "{v}");
        assert_eq!(v["argon_ctx"], 1_048_576, "{v}");
        assert_eq!(v["argon_efforts"], json!(["low", "medium", "high"]), "{v}");
        assert_eq!(v["hydro_efforts"], json!(["low", "medium", "high", "xhigh"]), "{v}");
        assert_eq!(v["g38_cost"], json!({ "input": 0.75, "output": 3.75, "cache_read": 0.075 }), "{v}");
        assert_eq!(v["og_default"], "glm-5.3-flash", "{v}");
        assert_eq!(v["og_mdev"], "opencode-go", "{v}");
        assert_eq!(v["og_has_crouton"], false, "{v}");
        assert_eq!(v["og_pro_ctx"], 1_000_000, "{v}");

        h.shutdown().await;
    })
    .await;
}
