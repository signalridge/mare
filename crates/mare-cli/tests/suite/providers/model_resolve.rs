//! Acceptance tests for consistent model resolution across CLI, `/model`, ACP, and Lua
//! (`signalridge/mare#33`):
//! 1. Explicit unresolvable model never falls back to another configured provider (0 requests sent).
//! 2. Providers with a catalog reject unknown `provider/<id>` at the `/model` command.
//! 3. CLI, `/model`, ACP, and Lua share one deterministic bare-id disambiguator independent of
//!    provider registration order.
//! 4. Family aliases pick the newest dated snapshot (and prefer undated alias over dated).
//! 5. Effort is either sent in the request body (`kimi` -> `thinking`) or omitted for unmapped
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

/// Criterion 5: effort is either sent in the request body (`kimi` -> `thinking`) or NOT offered
/// for unmapped routed providers (`groq`), and invalid `reasoning_format` / `effort_param` values
/// are rejected at load time.
#[tokio::test]
async fn effort_sent_for_kimi_hidden_for_unmapped_provider_and_invalid_format_rejected_at_load() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MOONSHOT_API_KEY", "test-kimi");
        let server = MockServer::start().await;
        let ok_sse = compat_events(&[compat_text("ok"), compat_stop()]);
        server.route("/kimi/chat/completions", vec![Resp::sse(ok_sse.clone()), Resp::sse(ok_sse)]);

        let h = provider_host(
            &mut env,
            &server,
            json!({
                "providers": {
                    "kimi": { "base_url": format!("{}/kimi", server.base) },
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

        // 1. kimi/kimi-k3 + /effort low -> thinking = { type = "enabled", effort = "low" }
        //    and /effort off -> thinking = { type = "disabled" }
        exec(
            &h,
            r#"
            LLM:set_model("kimi/kimi-k3", { save = false })
            C.cordis:get("commands"):run("/effort low")
            local m = assert(LLM:current())
            local r1 = LLM:stream({ model = m, system = "S", messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
            assert(not r1.error, tostring(r1.error))

            local r2 = LLM:stream({ model = m, effort = "off", system = "S", messages = { { role = "user", content = { { type = "text", text = "hi" } } } } }, function() end)
            assert(not r2.error, tostring(r2.error))
            "#,
        )
        .await;
        let kimi_reqs: Vec<Value> = server
            .requests()
            .into_iter()
            .filter(|r| r.path == "/kimi/chat/completions")
            .map(|r| r.json())
            .collect();
        assert_eq!(kimi_reqs.len(), 2);
        assert_eq!(
            kimi_reqs[0]["thinking"],
            json!({ "type": "enabled", "effort": "low" }),
            "kimi low effort must send thinking payload: {}",
            kimi_reqs[0]
        );
        assert_eq!(
            kimi_reqs[1]["thinking"],
            json!({ "type": "disabled" }),
            "kimi off effort must disable thinking: {}",
            kimi_reqs[1]
        );

        // 2. Unmapped custom provider (groq) offers NO effort parameter; invalid reasoning_format /
        //    effort_param are rejected at load time (not registered / reported in list_providers).
        let (groq_has_effort, badfmt_state, badparam_state): (bool, String, String) = eval(
            &h,
            r#"
            local cap = LLM:capabilities("groq/r1")
            local has_eff = false
            for _, p in ipairs(cap.parameters or {}) do
              if p.parameter_id == "effort" then has_eff = true end
            end
            local states = {}
            for _, row in ipairs(LLM:list_providers().data) do
              states[row.provider] = tostring(row.state) .. ":" .. tostring(row.reason or "")
            end
            return has_eff, states.badfmt or "missing", states.badparam or "missing"
            "#,
        )
        .await;
        assert!(!groq_has_effort, "unmapped openai-chat provider must not offer effort parameter");
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
                  return chosen, items[chosen] and items[chosen].label or nil, nil
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
