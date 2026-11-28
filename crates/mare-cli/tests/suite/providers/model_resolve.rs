//! Acceptance tests for consistent model resolution across CLI, `/model`, ACP, and Lua
//! (`signalridge/mare#33`):
//! 1. Explicit unresolvable model never falls back to another configured provider (0 requests sent).
//! 2. Providers with a catalog reject unknown `provider/<id>` at the `/model` command.
//! 3. CLI, `/model`, ACP, and Lua share one deterministic bare-id disambiguator independent of
//!    provider registration order.
//! 4. Family aliases pick the newest dated snapshot (and prefer undated alias over dated).
//! 5. Effort is either sent in the request body (`kimi-k3` -> `reasoning_effort`) or omitted for unmapped
//!    routes, and `reasoning_format` / `effort_param` are validated at load time.
//! 6. An open `/model` picker refreshes live when a background catalog fetch completes, and
//!    selecting the newly added model sends the next request with it.

use std::process::Stdio;

use super::*;

/// Criterion 1: auto-selection of a default model happens only when the user did not specify one.
/// Headless `-p --model typo-provider/some-model` exits non-zero naming the original ref and sends
/// zero generation requests to the working mock provider; control run without `--model` sends 1.
#[tokio::test]
async fn explicit_unresolvable_model_refuses_without_falling_back_to_another_provider() {
    local(async {
        let server = MockServer::start().await;
        server.route("/mock/chat/completions", vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))]);

        let home = tempfile::tempdir().unwrap();
        let cfg_dir = home.path().join("config");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("init.lua"),
            format!(
                r#"
mare.provider("localmock", {{
  wire = "openai-chat",
  base_url = "{base}/mock",
  api_key = "test-key",
  list_models = false,
  default_model = "m1",
  models = {{ {{ model_id = "m1", context_window = 128000, max_output = 4096 }} }},
}})
"#,
                base = server.base
            ),
        )
        .unwrap();

        let run_cli = |extra: Vec<String>| {
            let home_path = home.path().to_path_buf();
            let cfg_dir = cfg_dir.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut cmd = mare_cmd();
                    cmd.env("MARE_HOME", &home_path)
                        .env("MARE_CONFIG_DIR", &cfg_dir)
                        .env("MARE_STATE_DIR", home_path.join("state"))
                        .env("MARE_DATA_DIR", home_path.join("data"))
                        .env("OPENAI_API_KEY", "")
                        .env("ANTHROPIC_API_KEY", "")
                        .env("DEEPSEEK_API_KEY", "")
                        .env("GEMINI_API_KEY", "")
                        .current_dir(&home_path)
                        .stdin(Stdio::null())
                        .args(["-p", "--set", "plugins.provider-faux.enabled=false"])
                        .args(&extra)
                        .arg("hi");
                    cmd.output().unwrap()
                })
                .await
                .unwrap()
            }
        };

        // 1. Explicit nonexistent provider: non-zero exit, names original ref, 0 generation requests.
        let bad = run_cli(vec!["--model".into(), "typo-provider/some-model".into()]).await;
        let bad_out = format!("{}\n{}", String::from_utf8_lossy(&bad.stdout), String::from_utf8_lossy(&bad.stderr));
        assert!(!bad.status.success(), "explicit unresolvable --model must exit non-zero: {bad_out}");
        assert!(bad_out.contains("typo-provider/some-model"), "error must name original ref: {bad_out}");
        assert_eq!(
            server.count("/mock/chat/completions"),
            0,
            "working mock provider must receive zero generation requests"
        );

        // 2. Control run without --model: auto-selects localmock/m1 and sends 1 request.
        let ok = run_cli(vec![]).await;
        let ok_out = format!("{}\n{}", String::from_utf8_lossy(&ok.stdout), String::from_utf8_lossy(&ok.stderr));
        assert!(ok.status.success(), "control run without --model must succeed: {ok_out}");
        assert_eq!(server.count("/mock/chat/completions"), 1, "control run must send 1 request");
    })
    .await;
}

/// Criterion 2: providers with a catalog reject unknown `provider/<id>` when set via `/model`.
/// Asserts the error text surfaced by the `/model` command itself (not only `resolve`).
#[tokio::test]
async fn model_command_rejects_unknown_id_on_catalogued_provider() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let h = provider_host(&mut env, &server, json!({ "model": "anthropic/claude-sonnet-5-5" })).await;
        expose(&h);

        let (before, run_ok, run_err, notes_json, after): (String, bool, String, String, String) = eval(
            &h,
            r#"
            local before = assert(LLM:current()).ref
            local notes = {}
            mare.ui.notify = function(msg, lvl)
              notes[#notes + 1] = tostring(lvl) .. ":" .. tostring(msg)
            end
            local ok, err = pcall(function()
              return C.cordis:get("commands"):run("/model anthropic/claude-sonet-4-5")
            end)
            local after = assert(LLM:current()).ref
            return before, ok, tostring(type(err) == "table" and err.message or err), mare.json.encode(notes), after
            "#,
        )
        .await;
        assert_eq!(before, "anthropic/claude-sonnet-5-5");
        assert_eq!(after, "anthropic/claude-sonnet-5-5", "current model must stay unchanged");
        assert!(
            !run_ok && run_err.contains("anthropic/claude-sonet-4-5"),
            "/model command must fail naming typo ref: {run_err}"
        );
        assert!(
            notes_json.contains("error:") && notes_json.contains("anthropic/claude-sonet-4-5"),
            "/model command must notify error naming typo ref: {notes_json}"
        );
        h.shutdown().await;
    })
    .await;
}

/// Criterion 3: CLI, `/model`, ACP, and Lua share one deterministic bare-id disambiguator.
/// Registers two mock providers with the same bare id in both orders; bare id lists identical
/// sorted candidates with 0 requests sent across all entry points; full ref routes only to chosen.
#[tokio::test]
async fn ambiguous_bare_id_is_rejected_identically_across_lua_command_cli_and_acp_in_both_orders() {
    local(async {
        let mut observed_errors = Vec::new();
        for order in [["alpha", "beta"], ["beta", "alpha"]] {
            let server = MockServer::start().await;
            let ok_sse = compat_events(&[compat_text("from-beta"), compat_stop()]);
            server.route("/alpha/chat/completions", vec![Resp::sse(ok_sse.clone())]);
            server.route("/beta/chat/completions", vec![Resp::sse(ok_sse)]);

            let home = tempfile::tempdir().unwrap();
            let cfg_dir = home.path().join("config");
            std::fs::create_dir_all(&cfg_dir).unwrap();
            let init_lua = format!(
                r#"
mare.provider("{p0}", {{
  wire = "openai-chat",
  base_url = "{base}/{p0}",
  api_key = "k-{p0}",
  list_models = false,
  models = {{ {{ model_id = "shared-x", context_window = 128000, max_output = 4096 }} }},
}})
mare.provider("{p1}", {{
  wire = "openai-chat",
  base_url = "{base}/{p1}",
  api_key = "k-{p1}",
  list_models = false,
  models = {{ {{ model_id = "shared-x", context_window = 128000, max_output = 4096 }} }},
}})
"#,
                p0 = order[0],
                p1 = order[1],
                base = server.base,
            );
            std::fs::write(cfg_dir.join("init.lua"), &init_lua).unwrap();

            // A) Lua + /model command in-process host
            let env = TestEnv::new().await;
            std::fs::create_dir_all(env.config_dir()).unwrap();
            std::fs::write(env.config_dir().join("init.lua"), &init_lua).unwrap();
            let h = env
                .host()
                .user_config(false)
                .config(json!({ "plugins": { "provider-faux": { "enabled": false } } }))
                .no_session()
                .build()
                .await;
            expose(&h);
            global(&h, "LLM", h.service_table("llm").unwrap());

            let (lua_err, cmd_ok, cmd_err): (String, bool, String) = eval(
                &h,
                r#"
                local ok, err = pcall(function() return LLM:set_model("shared-x", { save = false }) end)
                assert(not ok, "Lua set_model must reject ambiguous bare id")
                local notes = {}
                mare.ui.notify = function(msg, lvl)
                  notes[#notes + 1] = tostring(lvl) .. ":" .. tostring(msg)
                end
                local cok, cerr = pcall(function()
                  return C.cordis:get("commands"):run("/model shared-x")
                end)
                return tostring(type(err) == "table" and err.message or err), cok, table.concat(notes, "\n") .. "\n" .. tostring(cerr)
                "#,
            )
            .await;
            assert!(!cmd_ok, "order {order:?}: /model command must reject ambiguous bare id");
            assert!(
                lua_err.contains("alpha/shared-x, beta/shared-x"),
                "order {order:?}: Lua error must list sorted candidates: {lua_err}"
            );
            assert!(
                cmd_err.contains("alpha/shared-x, beta/shared-x"),
                "order {order:?}: /model error must list sorted candidates: {cmd_err}"
            );
            observed_errors.push(lua_err);
            h.shutdown().await;

            // B) Headless CLI with bare id -> non-zero, sorted candidates, 0 requests
            let home_path = home.path().to_path_buf();
            let cfg_dir_b = cfg_dir.clone();
            let cli_bad = tokio::task::spawn_blocking(move || {
                mare_cmd()
                    .env("MARE_HOME", &home_path)
                    .env("MARE_CONFIG_DIR", &cfg_dir_b)
                    .env("MARE_STATE_DIR", home_path.join("state"))
                    .env("MARE_DATA_DIR", home_path.join("data"))
                    .current_dir(&home_path)
                    .stdin(Stdio::null())
                    .args(["-p", "--set", "plugins.provider-faux.enabled=false", "--model", "shared-x", "hi"])
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            let cli_out =
                format!("{}\n{}", String::from_utf8_lossy(&cli_bad.stdout), String::from_utf8_lossy(&cli_bad.stderr));
            assert!(!cli_bad.status.success(), "CLI bare ambiguous id must fail: {cli_out}");
            assert!(cli_out.contains("alpha/shared-x, beta/shared-x"), "CLI must list sorted candidates: {cli_out}");

            // C) ACP session/set_config_option with bare id -> rejected with sorted candidates
            let home_path_c = home.path().to_path_buf();
            let cfg_dir_c = cfg_dir.clone();
            let acp_bad = tokio::task::spawn_blocking(move || {
                let mut acp_cmd = mare_cmd();
                acp_cmd
                    .args(["acp", "--model", "alpha/shared-x", "--set", "plugins.provider-faux.enabled=false"])
                    .env("MARE_HOME", &home_path_c)
                    .env("MARE_CONFIG_DIR", &cfg_dir_c)
                    .env("MARE_STATE_DIR", home_path_c.join("state"))
                    .env("MARE_DATA_DIR", home_path_c.join("data"))
                    .current_dir(&home_path_c)
                    .stderr(Stdio::null());
                let mut acp = RpcClient::spawn_cmd(&mut acp_cmd);
                let _ =
                    acp.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
                let cwd = dunce::canonicalize(&home_path_c).unwrap();
                let s = acp.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
                let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();
                let res = acp.call_raw(
                    "session/set_config_option",
                    json!({ "sessionId": sid, "configId": "model", "value": "shared-x" }),
                    |_| json!(null),
                );
                acp.kill_and_wait();
                res
            })
            .await
            .unwrap();
            let acp_msg = format!(
                "{} {}",
                acp_bad["error"]["message"].as_str().unwrap_or(""),
                acp_bad["error"]["data"].as_str().unwrap_or("")
            );
            assert!(
                acp_msg.contains("alpha/shared-x, beta/shared-x"),
                "ACP must reject ambiguous bare id with sorted candidates: {acp_bad}"
            );

            assert_eq!(server.count("/alpha/chat/completions"), 0, "ambiguous checks must send 0 requests to alpha");
            assert_eq!(server.count("/beta/chat/completions"), 0, "ambiguous checks must send 0 requests to beta");

            // D) Choosing full ref routes ONLY to beta
            let home_path_d = home.path().to_path_buf();
            let cfg_dir_d = cfg_dir.clone();
            let cli_ok = tokio::task::spawn_blocking(move || {
                mare_cmd()
                    .env("MARE_HOME", &home_path_d)
                    .env("MARE_CONFIG_DIR", &cfg_dir_d)
                    .env("MARE_STATE_DIR", home_path_d.join("state"))
                    .env("MARE_DATA_DIR", home_path_d.join("data"))
                    .current_dir(&home_path_d)
                    .stdin(Stdio::null())
                    .args(["-p", "--set", "plugins.provider-faux.enabled=false", "--model", "beta/shared-x", "hi"])
                    .output()
                    .unwrap()
            })
            .await
            .unwrap();
            assert!(
                cli_ok.status.success(),
                "full ref beta/shared-x must succeed: {}",
                String::from_utf8_lossy(&cli_ok.stderr)
            );
            assert_eq!(server.count("/alpha/chat/completions"), 0, "alpha must receive 0 requests");
            assert_eq!(server.count("/beta/chat/completions"), 1, "beta must receive 1 request");
        }
        assert_eq!(
            observed_errors[0], observed_errors[1],
            "disambiguation error must be identical in both registration orders"
        );
    })
    .await;
}

/// Criterion 4: family aliases select the newest dated snapshot (not alphabetical/oldest).
/// Catalog first lists an older snapshot, then refreshes adding a newer snapshot; the alias
/// request carries the newer dated snapshot id.
#[tokio::test]
async fn family_alias_picks_newest_dated_snapshot_after_catalog_update() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        server.route(
            "/anthropic/v1/models",
            vec![
                Resp::status(
                    200,
                    r#"{"data":[{"id":"claude-sonnet-9-0-20250514","display_name":"Sonnet 9.0 May"}]}"#,
                ),
                Resp::status(
                    200,
                    r#"{"data":[
                        {"id":"claude-sonnet-9-0-20250514","display_name":"Sonnet 9.0 May"},
                        {"id":"claude-sonnet-9-0-20250929","display_name":"Sonnet 9.0 Sep"}
                    ]}"#,
                ),
            ],
        );
        server.route(
            "/anthropic/v1/messages",
            vec![Resp::sse(fixture("anthropic-text.sse")), Resp::sse(fixture("anthropic-text.sse"))],
        );
        let h = provider_host(
            &mut env,
            &server,
            json!({ "plugins": { "llm": { "catalog_refresh_ms": 0 } } }),
        )
        .await;

        exec(
            &h,
            r#"
            LLM:refresh_catalog({ name = "anthropic", force = true })
            local m1 = assert(LLM:resolve("sonnet"))
            local r1 = LLM:stream({ model = m1, system = "S", messages = { { role = "user", content = { { type = "text", text = "hi 1" } } } } }, function() end)
            assert(not r1.error, tostring(r1.error))

            LLM:refresh_catalog({ name = "anthropic", force = true })
            local m2 = assert(LLM:resolve("sonnet"))
            local r2 = LLM:stream({ model = m2, system = "S", messages = { { role = "user", content = { { type = "text", text = "hi 2" } } } } }, function() end)
            assert(not r2.error, tostring(r2.error))
            "#,
        )
        .await;

        let msg_reqs: Vec<Value> = server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/anthropic/v1/messages")
            .map(|r| r.json())
            .collect();
        assert_eq!(msg_reqs.len(), 2);
        assert_eq!(msg_reqs[0]["model"], "claude-sonnet-9-0-20250514");
        assert_eq!(
            msg_reqs[1]["model"], "claude-sonnet-9-0-20250929",
            "family alias must switch to the newest dated snapshot"
        );
        h.shutdown().await;
    })
    .await;
}

/// Criterion 5: effort is either sent in the request body (`kimi-k3` -> top-level `reasoning_effort`)
/// or NOT offered for unmapped routed providers (`groq`) and non-K3 Kimi models (`kimi-k2-thinking`,
/// `kimi-code/kimi-for-coding`), and invalid `reasoning_format` / `effort_param` values are rejected
/// at load time.
#[tokio::test]
async fn effort_sent_for_kimi_hidden_for_unmapped_provider_and_invalid_format_rejected_at_load() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MOONSHOT_API_KEY", "test-kimi");
        env.set_var("KIMI_API_KEY", "test-kimi-code");
        let server = MockServer::start().await;
        let ok_sse = compat_events(&[compat_text("ok"), compat_stop()]);
        server.route(
            "/kimi/chat/completions",
            vec![
                Resp::sse(ok_sse.clone()),
                Resp::sse(ok_sse.clone()),
                Resp::sse(ok_sse.clone()),
                Resp::sse(ok_sse.clone()),
            ],
        );
        server.route("/kimi-code/chat/completions", vec![Resp::sse(ok_sse)]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "kimi": { "base_url": format!("{}/kimi", server.base) },
                    "kimi-code": { "base_url": format!("{}/kimi-code", server.base) },
                    "groq": {
                        "wire": "openai-chat",
                        "base_url": format!("{}/groq", server.base),
                        "api_key": "test-groq",
                        "list_models": false,
                        "models": [{ "model_id": "r1", "reasoning": true, "efforts": ["low", "high"] }]
                    },
                    "badfmt": {
                        "wire": "openai-chat",
                        "base_url": format!("{}/badfmt", server.base),
                        "api_key": "k",
                        "reasoning_format": "totally-invalid"
                    },
                    "badparam": {
                        "wire": "openai-chat",
                        "base_url": format!("{}/badparam", server.base),
                        "api_key": "k",
                        "effort_param": "typo-param"
                    }
                }
            }),
        )
        .await;
        expose(&h);

        // 1. kimi/kimi-k3 + /effort low -> real agent prompt (precomputed params path) AND direct
        //    stream both send top-level reasoning_effort = "low" (no `thinking` key);
        //    max -> top-level reasoning_effort = "max".
        //    kimi/kimi-k2-thinking and kimi-code/kimi-for-coding must send neither field.
        exec(
            &h,
            r#"
            LLM:set_model("kimi/kimi-k3", { save = false })
            C.cordis:get("commands"):run("/effort low")
            "#,
        )
        .await;
        let prompt_res = h.prompt("hi from agent", json!({})).await.unwrap();
        assert!(prompt_res["error"].is_null(), "agent prompt on kimi-k3 must succeed: {prompt_res}");

        let (k3_opts, bad_med_ok, bad_off_ok, groq_opts_nil): (String, bool, bool, bool) = eval(
            &h,
            r#"
            local m = assert(LLM:current())
            local opts = table.concat(LLM:effort_options(m) or {}, ",")
            local ok_med = pcall(LLM.set_effort, LLM, "medium", { save = false })
            local ok_off = pcall(LLM.set_effort, LLM, "off", { save = false })
            local groq_opts = LLM:effort_options(assert(LLM:resolve("groq/r1")))

            local r1 = LLM:stream({ model = m, system = "S", messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
            assert(not r1.error, tostring(r1.error))

            local r2 = LLM:stream({ model = m, effort = "max", system = "S", messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
            assert(not r2.error, tostring(r2.error))

            local k2 = assert(LLM:resolve("kimi/kimi-k2-thinking"))
            local r3 = LLM:stream({ model = k2, system = "S", messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
            assert(not r3.error, tostring(r3.error))

            local kc = assert(LLM:resolve("kimi-code/kimi-for-coding"))
            local r4 = LLM:stream({ model = kc, system = "S", messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
            assert(not r4.error, tostring(r4.error))
            return opts, ok_med, ok_off, groq_opts == nil
            "#,
        )
        .await;
        assert_eq!(k3_opts, "low,high,max", "effort_options must match kimi-k3 offered levels");
        assert!(!bad_med_ok, "set_effort must reject unsupported 'medium' on kimi-k3");
        assert!(!bad_off_ok, "set_effort must reject unsupported 'off' on kimi-k3");
        assert!(groq_opts_nil, "unmapped groq provider must have nil effort_options");

        let kimi_reqs: Vec<Value> = server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/kimi/chat/completions")
            .map(|r| r.json())
            .collect();
        assert_eq!(kimi_reqs.len(), 4);
        assert_eq!(
            kimi_reqs[0]["reasoning_effort"], "low",
            "agent prompt on kimi-k3 must send top-level reasoning_effort=low: {}",
            kimi_reqs[0]
        );
        assert!(kimi_reqs[0].get("thinking").is_none(), "kimi-k3 agent prompt must not send thinking: {}", kimi_reqs[0]);
        assert_eq!(
            kimi_reqs[1]["reasoning_effort"], "low",
            "kimi-k3 low effort stream must send top-level reasoning_effort: {}",
            kimi_reqs[1]
        );
        assert!(kimi_reqs[1].get("thinking").is_none(), "kimi-k3 must not send thinking object: {}", kimi_reqs[1]);
        assert_eq!(
            kimi_reqs[2]["reasoning_effort"], "max",
            "kimi-k3 max effort must send top-level reasoning_effort: {}",
            kimi_reqs[2]
        );
        assert!(kimi_reqs[2].get("thinking").is_none(), "kimi-k3 must not send thinking object: {}", kimi_reqs[2]);
        assert!(
            kimi_reqs[3].get("thinking").is_none() && kimi_reqs[3].get("reasoning_effort").is_none(),
            "kimi-k2-thinking must not receive K3 reasoning fields: {}",
            kimi_reqs[3]
        );
        let kc_req = server
            .requests()
            .into_iter()
            .find(|r| r.path == "/kimi-code/chat/completions")
            .expect("kimi-code request sent")
            .json();
        assert!(
            kc_req.get("thinking").is_none() && kc_req.get("reasoning_effort").is_none(),
            "kimi-code/kimi-for-coding must not receive K3 reasoning fields: {kc_req}"
        );

        // 2. Unmapped custom provider (groq) and non-K3 kimi models offer NO effort parameter;
        //    invalid reasoning_format / effort_param are rejected at load time.
        let (groq_has_effort, k2_has_effort, kc_has_effort, badfmt_state, badparam_state): (
            bool,
            bool,
            bool,
            String,
            String,
        ) = eval(
            &h,
            r#"
            local function has_effort(ref)
              local cap = LLM:capabilities(ref)
              for _, p in ipairs(cap.parameters or {}) do
                if p.parameter_id == "effort" then return true end
              end
              return false
            end
            local states = {}
            for _, row in ipairs(LLM:list_providers().data) do
              states[row.provider] = tostring(row.state) .. ":" .. tostring(row.reason or "")
            end
            return has_effort("groq/r1"),
              has_effort("kimi/kimi-k2-thinking"),
              has_effort("kimi-code/kimi-for-coding"),
              states.badfmt or "missing",
              states.badparam or "missing"
            "#,
        )
        .await;
        assert!(!groq_has_effort, "unmapped openai-chat provider must not offer effort parameter");
        assert!(!k2_has_effort, "kimi-k2-thinking must not offer unmapped effort parameter");
        assert!(!kc_has_effort, "kimi-code/kimi-for-coding must not offer unmapped effort parameter");
        assert!(
            badfmt_state.starts_with("unresolved:") && badfmt_state.contains("reasoning_format"),
            "invalid reasoning_format must be rejected at load time: {badfmt_state}"
        );
        assert!(
            badparam_state.starts_with("unresolved:") && badparam_state.contains("effort_param"),
            "invalid effort_param must be rejected at load time: {badparam_state}"
        );
        h.shutdown().await;
    })
    .await;
}

/// Criterion 6: open `/model` picker refreshes live when a held mock catalog response is released;
/// selecting the newly arrived model in the same picker sends the next request with that model.
#[tokio::test]
async fn open_model_picker_updates_live_and_next_request_uses_newly_selected_model() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let release = Arc::new(tokio::sync::Notify::new());
        server.route(
            "/compat/models",
            vec![Resp::status(
                200,
                r#"{"object":"list","data":[{"id":"deepseek-live-picked","name":"Live Picked"}]}"#,
            )],
        );
        server.set_hold("/compat/models", 0, release.clone());
        server.route(
            "/compat/chat/completions",
            vec![Resp::sse(compat_events(&[compat_text("ok"), compat_stop()]))],
        );

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "deepseek/deepseek-chat",
                "plugins": { "llm": { "catalog_open_refresh_ms": 0 } }
            }),
        )
        .await;
        expose(&h);

        let h_pick = h.clone();
        let picker_task = tokio::task::spawn_local(async move {
            exec(
                &h_pick,
                r#"
                _G.__picker_opened = false
                _G.__updated_tx, _G.__updated_rx = mare.task.channel()
                mare.ui.notify = function() end
                mare.ui.select = function(opts)
                  local items = opts.items
                  local open = true
                  local handle = {
                    is_open = function() return open end,
                    update = function(_, next_items)
                      items = next_items
                      for idx, it in ipairs(items) do
                        if it.item_id == "deepseek/deepseek-live-picked" then
                          _G.__updated_tx:send(idx)
                          break
                        end
                      end
                      return true
                    end,
                  }
                  if opts.on_open then opts.on_open(handle) end
                  _G.__picker_opened = true
                  local _, chosen = mare.task.select(function()
                    return _G.__updated_rx:recv()
                  end, function()
                    mare.task.sleep(5000)
                    return nil
                  end)
                  open = false
                  return chosen, 1, nil, "", items[chosen] and items[chosen].item_id or nil
                end
                C.cordis:get("commands"):run("/model")
                "#,
            )
            .await;
        });

        wait_until(5, "model picker opened", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return _G.__picker_opened == true").await }
        })
        .await;
        // Release held GET /models while the picker is still open.
        release.notify_one();
        picker_task.await.unwrap();

        // Next request uses the model selected in the live-updated picker.
        let cur: String = eval(
            &h,
            r#"
            local m = assert(LLM:current())
            local r = LLM:stream({ model = m, system = "S", messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
            assert(not r.error, tostring(r.error))
            return m.ref
            "#,
        )
        .await;
        assert_eq!(cur, "deepseek/deepseek-live-picked");
        let chat_req = server
            .requests()
            .into_iter()
            .find(|r| r.path == "/compat/chat/completions")
            .expect("chat request sent");
        assert_eq!(chat_req.json()["model"], "deepseek-live-picked");
        h.shutdown().await;
    })
    .await;
}

/// Regression: if a live `sel:update` reorders items while a non-updating frontend (e.g. plain mode)
/// or an in-flight confirmation still returns the old index with the old item's `item_id`, `/model`
/// resolves by `item_id` rather than picking whatever shifted into that index; if the `item_id` is
/// no longer present, `/model` warns and keeps the current selection unchanged.
#[tokio::test]
async fn stale_picker_reply_after_live_update_resolves_by_item_id() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;
        let release = Arc::new(tokio::sync::Notify::new());
        // `deepseek-000-inserted` sorts before `deepseek-chat`, shifting `deepseek-chat`'s index.
        server.route(
            "/compat/models",
            vec![Resp::status(
                200,
                r#"{"object":"list","data":[{"id":"deepseek-000-inserted","name":"Inserted First"}]}"#,
            )],
        );
        server.set_hold("/compat/models", 0, release.clone());

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "model": "anthropic/claude-sonnet-5-5",
                "plugins": { "llm": { "catalog_open_refresh_ms": 0 } }
            }),
        )
        .await;
        expose(&h);

        let h_pick = h.clone();
        let picker_task = tokio::task::spawn_local(async move {
            exec(
                &h_pick,
                r#"
                _G.__stale_opened = false
                _G.__stale_tx, _G.__stale_rx = mare.task.channel()
                mare.ui.notify = function() end
                mare.ui.select = function(opts)
                  local orig_items = opts.items
                  local orig_idx
                  for idx, it in ipairs(orig_items) do
                    if it.item_id == "deepseek/deepseek-chat" then
                      orig_idx = idx
                      break
                    end
                  end
                  assert(orig_idx, "expected deepseek/deepseek-chat in initial items")
                  local open = true
                  local handle = {
                    is_open = function() return open end,
                    update = function(_, next_items)
                      -- Confirm the new model actually shifted orig_idx to a different item.
                      if next_items[orig_idx] and next_items[orig_idx].item_id == "deepseek/deepseek-000-inserted" then
                        _G.__stale_tx:send(true)
                      end
                      return true
                    end,
                  }
                  if opts.on_open then opts.on_open(handle) end
                  _G.__stale_opened = true
                  local _, shifted = mare.task.select(function()
                    return _G.__stale_rx:recv()
                  end, function()
                    mare.task.sleep(5000)
                    return false
                  end)
                  assert(shifted == true, "expected live update to shift orig_idx")
                  open = false
                  -- Simulate plain frontend ignoring SelectUpdate: returns OLD index AND OLD item_id.
                  return orig_idx, 1, nil, "", orig_items[orig_idx].item_id
                end
                C.cordis:get("commands"):run("/model")
                "#,
            )
            .await;
        });

        wait_until(5, "stale picker opened", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return _G.__stale_opened == true").await }
        })
        .await;
        release.notify_one();
        picker_task.await.unwrap();

        let cur: String = eval(&h, "return assert(LLM:current()).ref").await;
        assert_eq!(
            cur, "deepseek/deepseek-chat",
            "stale index must resolve via item_id to deepseek/deepseek-chat, not deepseek-000-inserted"
        );

        // Missing item_id must warn and leave current selection untouched.
        let (after, notes): (String, String) = eval(
            &h,
            r#"
            local notes = {}
            mare.ui.notify = function(msg, lvl)
              notes[#notes + 1] = tostring(lvl) .. ":" .. tostring(msg)
            end
            mare.ui.select = function()
              return 1, 1, nil, "", "deepseek/no-longer-present"
            end
            C.cordis:get("commands"):run("/model")
            return assert(LLM:current()).ref, table.concat(notes, "\n")
            "#,
        )
        .await;
        assert_eq!(after, "deepseek/deepseek-chat");
        assert!(notes.contains("no-longer-present"), "missing item_id must warn: {notes}");
        h.shutdown().await;
        drop(env);

        // Refresh completing before on_open (between snapshot and on_open) must reconcile immediately
        // in on_open, and an update-raced reply without item_id must warn instead of falling back
        // to models[i].
        let mut env2 = TestEnv::new().await;
        let cache_dir2 = env2.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cache_dir2).unwrap();
        std::fs::write(
            cache_dir2.join("deepseek.json"),
            r#"{"fetched_at": 9999999999, "models": [{"id": "deepseek-chat"}]}"#,
        )
        .unwrap();
        let server2 = MockServer::start().await;
        server2.route(
            "/compat/models",
            vec![Resp::status(
                200,
                r#"{"object":"list","data":[{"id":"deepseek-000-pre-open","name":"Pre-Open Model"}]}"#,
            )],
        );
        let h2 = provider_host(
            &mut env2,
            &server2,
            json!({
                "model": "deepseek/deepseek-chat",
                "plugins": { "llm": { "catalog_open_refresh_ms": 600000 } }
            }),
        )
        .await;
        expose(&h2);
        let (reconciled, no_id_after, no_id_notes): (bool, String, String) = eval(
            &h2,
            r#"
            local notes = {}
            mare.ui.notify = function(msg, lvl)
              notes[#notes + 1] = tostring(lvl) .. ":" .. tostring(msg)
            end
            local saw_pre_open = false
            mare.ui.select = function(opts)
              -- Complete the refresh BEFORE calling on_open to exercise the snapshot->on_open gap.
              LLM:refresh_catalog({ name = "deepseek", force = true })
              local open = true
              local handle = {
                is_open = function() return open end,
                update = function(_, next_items)
                  for _, it in ipairs(next_items) do
                    if it.item_id == "deepseek/deepseek-000-pre-open" then
                      saw_pre_open = true
                    end
                  end
                  return true
                end,
              }
              if opts.on_open then opts.on_open(handle) end
              open = false
              -- Reply with an index but NO item_id (5th return value nil) after list_updated=true.
              return 1, 1, nil, "", nil
            end
            C.cordis:get("commands"):run("/model")
            return saw_pre_open, assert(LLM:current()).ref, table.concat(notes, "\n")
            "#,
        )
        .await;
        assert!(reconciled, "on_open must reconcile models refreshed before subscription");
        assert_eq!(
            no_id_after, "deepseek/deepseek-chat",
            "reply without item_id after live update must not pick shifted models[1]"
        );
        assert!(
            no_id_notes.contains("Model list updated while selecting"),
            "missing item_id after live update must warn user: {no_id_notes}"
        );
        h2.shutdown().await;
    })
    .await;
}

/// Regressions for explicit default preservation in `reset_session_model` and declared `default_model`
/// precedence on catalog-less providers with a user model override.
#[tokio::test]
async fn reset_session_model_keeps_unresolvable_default_and_catalogless_default_wins_over_override() {
    local(async {
        let mut env = TestEnv::new().await;
        let server = MockServer::start().await;

        // 1. Explicit unresolvable default model: reset_session_model must keep it unresolved and
        //    never silently fall back to another configured provider.
        let h1 = provider_host(
            &mut env,
            &server,
            json!({ "model": "typo-provider/missing-model" }),
        )
        .await;
        let (reset_nil, cur_nil, ready_ok, ready_reason): (bool, bool, bool, String) = eval(
            &h1,
            r#"
            local r = LLM:reset_session_model()
            local c = LLM:current()
            local ok, reason = LLM:ready()
            return r == nil, c == nil, ok == true, tostring(reason or "")
            "#,
        )
        .await;
        assert!(reset_nil, "reset_session_model must return nil for unresolvable explicit default");
        assert!(cur_nil, "current() must remain nil rather than falling back to another provider");
        assert!(!ready_ok, "readiness must deny unresolvable explicit default");
        assert!(
            ready_reason.contains("typo-provider/missing-model"),
            "readiness must report exact unresolved ref: {ready_reason}"
        );
        h1.shutdown().await;
        drop(env);

        // 2. Catalog-less provider with declared default_model + user override on an alphabetically
        //    earlier model id: auto-selection must still pick the declared default_model.
        let mut env2 = TestEnv::new().await;
        env2.set_var("GW_KEY", "test-gw");
        let h2 = env2
            .host()
            .config(json!({
                "model": "",
                "plugins": { "preset": "minimal", "permissions": { "enabled": false }, "provider-faux": { "enabled": false } },
                "providers": {
                    "gw": {
                        "wire": "openai-chat",
                        "base_url": format!("{}/gw", server.base),
                        "api_key_env": "GW_KEY",
                        "list_models": false,
                        "default_model": "z-default"
                    }
                },
                "models": {
                    "gw/a-override": { "context_window": 64000 }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h2, "LLM", h2.service_table("llm").unwrap());
        let picked: String = eval(&h2, "return assert(LLM:current()).ref").await;
        assert_eq!(picked, "gw/z-default", "declared default_model must win over alphabetical override");
        h2.shutdown().await;
        drop(env2);

        // 3. Explicit default_model whose provider is disabled: reset_session_model must keep it
        //    (never fall back to another configured provider) and prompting must send 0 requests.
        let mut env3 = TestEnv::new().await;
        env3.set_var("LOCALMOCK_KEY", "test-lm");
        server.route(
            "/disabled-fallback/chat/completions",
            vec![Resp::sse(compat_events(&[compat_text("wrong-provider"), compat_stop()]))],
        );
        let h3 = env3
            .host()
            .config(json!({
                "model": "anthropic/claude-sonnet-5-5",
                "plugins": {
                    "preset": "minimal",
                    "permissions": { "enabled": false },
                    "provider-faux": { "enabled": false },
                    "provider-anthropic": { "enabled": false }
                },
                "providers": {
                    "localmock": {
                        "wire": "openai-chat",
                        "base_url": format!("{}/disabled-fallback", server.base),
                        "api_key_env": "LOCALMOCK_KEY",
                        "list_models": false,
                        "default_model": "m1",
                        "models": [{ "model_id": "m1", "context_window": 128000, "max_output": 4096 }]
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h3, "LLM", h3.service_table("llm").unwrap());
        let (reset_nil3, cur_ref3): (bool, String) = eval(
            &h3,
            r#"
            local r = LLM:reset_session_model()
            local c = LLM:current()
            return r == nil, c and tostring(c.ref) or ""
            "#,
        )
        .await;
        assert!(reset_nil3, "reset_session_model must not fall back when explicit default provider is disabled");
        assert_eq!(cur_ref3, "anthropic/claude-sonnet-5-5", "explicit disabled model ref must stay selected");
        let res3 = h3.prompt("hi", json!({})).await.unwrap();
        assert!(!res3["error"].is_null(), "prompt with disabled explicit provider must fail: {res3}");
        assert_eq!(
            server.count("/disabled-fallback/chat/completions"),
            0,
            "working fallback provider must receive 0 requests when explicit model's provider is disabled"
        );
        h3.shutdown().await;
    })
    .await;
}

/// Regression: pending disk caches are hydrated before resolving non-exact refs so that
/// (1) a bare id present statically on `alpha` and only in `beta`'s disk cache reports ambiguity
/// identically across Lua `set_model`, `/model`, and headless `-p --model` with zero generation
/// requests, and (2) `beta/typo` is rejected against `beta`'s pending disk-cached catalog.
#[tokio::test]
async fn pending_disk_cache_hydrates_before_bare_id_and_catalog_typo_resolution() {
    local(async {
        let server = MockServer::start().await;
        let ok_sse = compat_events(&[compat_text("ok"), compat_stop()]);
        server.route("/alpha/chat/completions", vec![Resp::sse(ok_sse.clone())]);
        server.route("/beta/chat/completions", vec![Resp::sse(ok_sse)]);

        let home = tempfile::tempdir().unwrap();
        let cfg_dir = home.path().join("config");
        let cache_dir = home.path().join("state/models-cache");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::create_dir_all(&cache_dir).unwrap();
        let cache_json = r#"{"fetched_at": 9999999999, "models": [{"id": "shared-cached", "context_window": 128000}]}"#;
        std::fs::write(cache_dir.join("beta.json"), cache_json).unwrap();
        let init_lua = format!(
            r#"
mare.provider("alpha", {{
  wire = "openai-chat",
  base_url = "{base}/alpha",
  api_key = "k-alpha",
  list_models = false,
  models = {{ {{ model_id = "shared-cached", context_window = 128000, max_output = 4096 }} }},
}})
mare.provider("beta", {{
  wire = "openai-chat",
  base_url = "{base}/beta",
  api_key = "k-beta",
}})
"#,
            base = server.base,
        );
        std::fs::write(cfg_dir.join("init.lua"), &init_lua).unwrap();

        // 1. In-process host (disk cache pending, ui/ready not emitted):
        //    set_model("shared-cached") must report ambiguity, and set_model("beta/typo") must fail.
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), &init_lua).unwrap();
        let env_cache_dir = env.home.path().join("state/models-cache");
        std::fs::create_dir_all(&env_cache_dir).unwrap();
        std::fs::write(env_cache_dir.join("beta.json"), cache_json).unwrap();
        let h = env
            .host()
            .user_config(false)
            .config(json!({ "plugins": { "provider-faux": { "enabled": false } } }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let (bare_ok, bare_err, typo_ok, typo_err): (bool, String, bool, String) = eval(
            &h,
            r#"
            local ok1, e1 = pcall(function() return LLM:set_model("shared-cached", { save = false }) end)
            local ok2, e2 = pcall(function() return LLM:set_model("beta/typo", { save = false }) end)
            return ok1, tostring(type(e1) == "table" and e1.message or e1),
              ok2, tostring(type(e2) == "table" and e2.message or e2)
            "#,
        )
        .await;
        assert!(!bare_ok, "bare id across static + pending disk cache must be rejected as ambiguous");
        assert!(
            bare_err.contains("alpha/shared-cached, beta/shared-cached"),
            "bare id error must list both static and disk-cached candidates: {bare_err}"
        );
        assert!(!typo_ok, "typo on provider with pending disk cache must be rejected");
        assert!(
            typo_err.contains("unknown model: beta/typo") && typo_err.contains("not in beta catalog"),
            "typo error must name beta/typo and beta catalog: {typo_err}"
        );
        h.shutdown().await;
        drop(env);

        // 1b. Cold-start in-process prompt (`agent/loop_preflight.lua` passes `model = current()`):
        //     `model = "shared-cached"` with static `alpha/shared-cached` and pending disk cache
        //     `beta.json` must hydrate before admission and refuse as ambiguous with 0 requests.
        let env_cold = TestEnv::new().await;
        std::fs::create_dir_all(env_cold.config_dir()).unwrap();
        std::fs::write(env_cold.config_dir().join("init.lua"), &init_lua).unwrap();
        let cold_cache_dir = env_cold.home.path().join("state/models-cache");
        std::fs::create_dir_all(&cold_cache_dir).unwrap();
        std::fs::write(cold_cache_dir.join("beta.json"), cache_json).unwrap();
        let h_cold = env_cold
            .host()
            .user_config(false)
            .config(json!({
                "model": "shared-cached",
                "plugins": { "provider-faux": { "enabled": false } }
            }))
            .no_session()
            .build()
            .await;
        let cold_res = h_cold.prompt("hi", json!({})).await.unwrap();
        let cold_err = cold_res["error"]["message"].as_str().unwrap_or("");
        assert!(
            cold_err.contains("alpha/shared-cached, beta/shared-cached"),
            "cold-start prompt must hydrate disk cache and reject bare id as ambiguous: {cold_res}"
        );
        h_cold.shutdown().await;
        drop(env_cold);

        // 2. Headless CLI `-p --model shared-cached` (cold start, no ui/ready):
        //    must fail with sorted candidates and send 0 generation requests.
        let home_path = home.path().to_path_buf();
        let cfg_dir_cli = cfg_dir.clone();
        let cli_bare = tokio::task::spawn_blocking(move || {
            mare_cmd()
                .env("MARE_HOME", &home_path)
                .env("MARE_CONFIG_DIR", &cfg_dir_cli)
                .env("MARE_STATE_DIR", home_path.join("state"))
                .env("MARE_DATA_DIR", home_path.join("data"))
                .env_remove("MARE_MODEL")
                .current_dir(&home_path)
                .stdin(Stdio::null())
                .args(["-p", "--set", "plugins.provider-faux.enabled=false", "--model", "shared-cached", "hi"])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let cli_bare_out =
            format!("{}\n{}", String::from_utf8_lossy(&cli_bare.stdout), String::from_utf8_lossy(&cli_bare.stderr));
        assert!(!cli_bare.status.success(), "headless bare id must fail as ambiguous: {cli_bare_out}");
        assert!(
            cli_bare_out.contains("alpha/shared-cached, beta/shared-cached"),
            "headless bare id must list sorted candidates: {cli_bare_out}"
        );

        // 3. Headless CLI `-p --model beta/typo`: must fail against beta's pending disk cache.
        let home_path2 = home.path().to_path_buf();
        let cfg_dir_cli2 = cfg_dir.clone();
        let cli_typo = tokio::task::spawn_blocking(move || {
            mare_cmd()
                .env("MARE_HOME", &home_path2)
                .env("MARE_CONFIG_DIR", &cfg_dir_cli2)
                .env("MARE_STATE_DIR", home_path2.join("state"))
                .env("MARE_DATA_DIR", home_path2.join("data"))
                .env_remove("MARE_MODEL")
                .current_dir(&home_path2)
                .stdin(Stdio::null())
                .args(["-p", "--set", "plugins.provider-faux.enabled=false", "--model", "beta/typo", "hi"])
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        let cli_typo_out =
            format!("{}\n{}", String::from_utf8_lossy(&cli_typo.stdout), String::from_utf8_lossy(&cli_typo.stderr));
        assert!(!cli_typo.status.success(), "headless beta/typo must fail: {cli_typo_out}");
        assert!(
            cli_typo_out.contains("unknown model: beta/typo"),
            "headless beta/typo must report unknown model: {cli_typo_out}"
        );

        assert_eq!(server.count("/alpha/chat/completions"), 0, "alpha must receive 0 requests");
        assert_eq!(server.count("/beta/chat/completions"), 0, "beta must receive 0 requests");
    })
    .await;
}
