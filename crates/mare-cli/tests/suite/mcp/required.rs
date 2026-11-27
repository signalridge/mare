//! Required servers: requests are blocked until they are ready, and fail closed.

use super::*;

#[tokio::test]
async fn required_server_failure_blocks_every_request_before_provider() {
    local(async {
        let env = TestEnv::new().await;
        let missing = env.cwd.join("missing-required-mcp-executable");
        let cfg = json!({ "plugins": { "mcp": { "servers": { "required_test": {
            "command": missing.to_string_lossy(), "tier": "direct", "required": true
        } } } } });
        let h = env.host().builtins(&["mcp"]).faux(json!([{ "text": "provider must not run" }])).config(cfg).no_session().build().await;
        expose(&h);
        exec(&h, "REQUEST_BUILDS = 0; C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)").await;
        for text in ["first", "second"] {
            let result = h.prompt(text, json!({})).await.unwrap();
            let error = result["error"]["message"].as_str().unwrap_or("");
            assert!(error.contains("required MCP server required_test"), "{result}");
        }
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0, "provider request was built despite required MCP failure");
        h.shutdown().await;
    }).await
}

/// The TUI's first prompt, sent before ui/ready (a command-line prompt), gets the required
/// server's refusal before any request is built: the host is handed out once every plugin has
/// started, mcp included, so its readiness vote is in for the first request. A TUI-mode host made
/// directly: nothing here sends its ui/ready. Codex: a required server that cannot start fails the
/// first request (`exec/tests/suite/mcp_required_exit.rs:38`).
#[tokio::test]
async fn required_server_failure_blocks_the_first_tui_prompt_before_ui_ready() {
    local(async {
        let env = TestEnv::new().await;
        let missing = env.cwd.join("missing-required-mcp-executable");
        let cfg = json!({ "plugins": { "mcp": { "servers": { "required_test": {
            "command": missing.to_string_lossy(), "tier": "direct", "required": true
        } } } } });
        let mut b = env.host().builtins(&["mcp"]).faux(json!([{ "text": "provider must not run" }])).config(cfg).no_session();
        b.opts.mode = mare_shared::ui::Mode::Tui;
        let h = mare_host::Host::new(b.opts).await.unwrap();
        expose(&h);
        exec(&h, "REQUEST_BUILDS = 0; C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)").await;
        let result = h.prompt("first", json!({})).await.unwrap();
        let builds = eval::<i64>(&h, "return REQUEST_BUILDS").await;
        h.shutdown().await;
        let error = result["error"]["message"].as_str().unwrap_or("");
        assert!(error.contains("required MCP server required_test"), "{result}");
        assert_eq!(builds, 0, "provider request was built before mcp voted");
    }).await
}

#[tokio::test]
async fn required_mcp_waits_until_tool_registration_finishes() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"
            mare.setup { model = 'faux/echo' }
            mare.mcp.connect = function()
              return {
                info = function() return {} end,
                status = function() return mare._test_mcp_status or 'connected' end,
                tools_generation = function() return 0 end,
                resources = function() return {} end,
                resource_templates = function() return {} end,
                tools = function() mare.task.sleep(300); return { { name = 'ready_tool', inputSchema = { type = 'object' } } }, 0 end,
                prompts = function() return {} end,
                close = function() end,
                next_event = function() mare.task.sleep(5000); return nil end,
              }
            end
            mare.plugin('mcp', { servers = { delayed = { command = 'fake', tier = 'direct', required = true } } })
        "#).unwrap();
        let h = env.host().user_config(false).faux(json!([{ "text": "ok" }, { "text": "ok again" }])).no_session().build().await;
        expose(&h);
        exec(&h, "DECLARED = false; C.cordis:on('request/build', function(req, next) for _, t in ipairs(req.tools) do if t.name == 'mcp__delayed__ready_tool' then DECLARED = true end end; return next() end)").await;
        let result = h.prompt("hello", json!({})).await.unwrap();
        assert_eq!(result["text"], "ok", "{result}");
        assert!(eval::<bool>(&h, "return DECLARED").await, "required tool was absent from the first request");
        exec(&h, "mare._test_mcp_status = 'disconnected'").await;
        let failed = h.prompt("must wait", json!({})).await.unwrap();
        assert!(failed["error"]["message"].as_str().unwrap_or("").contains("required MCP server delayed"), "{failed}");
        exec(&h, "mare._test_mcp_status = 'connected'").await;
        let retried = h.prompt("after recovery", json!({})).await.unwrap();
        assert_eq!(retried["text"], "ok again", "{retried}");
        h.shutdown().await;
    }).await
}

/// a slow non-required `direct` server does not delay the first request; its tools are
/// absent from that request and declared in the next prompt's.
#[tokio::test]
async fn slow_direct_mcp_does_not_delay_first_request_and_joins_later() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"
            mare.setup { model = 'faux/echo' }
            mare.mcp.connect = function()
              return {
                info = function() return {} end,
                status = function() return 'connected' end,
                tools_generation = function() return 0 end,
                resources = function() return {} end,
                resource_templates = function() return {} end,
                tools = function() while not RELEASE do mare.task.sleep(20) end; return { { name = 'late_tool', inputSchema = { type = 'object' } } }, 0 end,
                prompts = function() return {} end,
                close = function() end,
                next_event = function() mare.task.sleep(5000); return nil end,
              }
            end
            mare.plugin('mcp', { servers = { slow = { command = 'fake', tier = 'direct' } } })
        "#).unwrap();
        let h = env.host().user_config(false).faux(json!([{ "text": "one" }, { "text": "two" }])).no_session().build().await;
        expose(&h);
        exec(&h, "SEEN = {}; C.cordis:on('request/build', function(req, next) local f = false; for _, t in ipairs(req.tools) do if t.name == 'mcp__slow__late_tool' then f = true end end; SEEN[#SEEN + 1] = f; return next() end)").await;
        // the server never finishes connecting until released: the first prompt completing at all
        // proves it did not wait (the timeout is only a hang guard, not a timing assertion)
        let first = tokio::time::timeout(std::time::Duration::from_secs(60), h.prompt("hello", json!({})))
            .await
            .expect("the first request waited for the connecting server")
            .unwrap();
        assert_eq!(first["text"], "one", "{first}");
        assert_eq!(eval::<String>(&h, "return C.cordis:get('mcp'):list_servers().data[1].state").await, "connecting");
        exec(&h, "RELEASE = true").await;
        wait_until(30, "slow MCP to connect", || { let h = h.clone(); async move { eval::<bool>(&h, "return C.cordis:get('mcp'):list_servers().data[1].state == 'connected'").await } }).await;
        let second = h.prompt("again", json!({})).await.unwrap();
        assert_eq!(second["text"], "two", "{second}");
        let seen: Vec<bool> = eval(&h, "return SEEN").await;
        assert_eq!(seen, vec![false, true], "tools join from the next turn");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn readiness_listener_error_fails_closed_before_provider() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["mcp"]).faux(json!([{ "text": "must not run" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"REQUEST_BUILDS = 0
            C.cordis:get("policy"):voter('request/ready', function() error('readiness sentinel') end)
            C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)"#,
        )
        .await;
        let result = h.prompt("hello", json!({})).await.unwrap();
        assert!(result["error"]["message"].as_str().unwrap_or("").contains("readiness sentinel"), "{result}");
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn cancelling_while_required_mcp_connects_stops_before_provider() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"
            mare.setup { model = 'faux/echo' }
            mare.mcp.connect = function()
              mare._mcp_waiting = true
              mare.task.sleep(600000) -- outlasts the test: only the abort ends it
              error('connection did not complete')
            end
            mare.plugin('mcp', { servers = { slow = { command = 'fake', tier = 'direct', required = true } } })
        "#).unwrap();
        let h = env.host().user_config(false).faux(json!([{ "text": "must not run" }])).no_session().build().await;
        expose(&h);
        exec(&h, "REQUEST_BUILDS = 0; C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)").await;
        let worker = h.clone();
        let prompt = tokio::task::spawn_local(async move { worker.prompt("cancel me", json!({})).await });
        wait_until(WAIT_SECS, "required MCP to start", || { let h = h.clone(); async move { eval::<bool>(&h, "return mare._mcp_waiting == true and A:get().busy").await } }).await;
        // the ceiling only guards a hang; the stop itself must be prompt even under load
        let aborted_at = std::time::Instant::now();
        h.abort().await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), prompt).await.expect("cancellation did not stop readiness wait").unwrap().unwrap();
        let stop = aborted_at.elapsed();
        assert!(stop < std::time::Duration::from_secs(5), "the readiness wait took {stop:?} to stop after the abort");
        assert!(result["error"]["message"].is_string(), "{result}");
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0);
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn request_preparation_failure_blocks_provider_request() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["mcp"]).faux(json!([{ "text": "must not run" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"REQUEST_BUILDS = 0
            C.cordis:on('request/prepare', function() error('preparation sentinel') end)
            C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)"#,
        )
        .await;
        let result = h.prompt("hello", json!({})).await.unwrap();
        assert!(result["error"]["message"].as_str().unwrap_or("").contains("preparation sentinel"), "{result}");
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn request_preparation_listener_timeout_blocks_provider() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["mcp"]).faux(json!([{ "text": "must not run" }])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"REQUEST_BUILDS = 0
            -- a hung preparation step under its owner's ordered registration (llm readiness; native
            -- listeners have no per-listener timeout): its 50 ms limit blocks the request
            C.cordis:get('llm'):readiness('test-hang', { order = 40, timeout_ms = 50,
              prepare = function() mare.task.sleep(600000) end })
            C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)"#,
        )
        .await;
        // the 50 ms step timeout must end the wait promptly: the ceiling only guards a hang,
        // the latency is asserted separately
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), h.prompt("hello", json!({})))
            .await
            .unwrap()
            .unwrap();
        let took = started.elapsed();
        assert!(
            took < std::time::Duration::from_secs(5),
            "the 50 ms preparation timeout took {took:?} to block the request"
        );
        assert!(result["error"]["message"].as_str().unwrap_or("").contains("timed out"), "{result}");
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn required_mcp_metadata_failure_never_marks_server_ready() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(env.config_dir().join("init.lua"), r#"
            mare.setup { model = 'faux/echo' }
            mare.mcp.connect = function()
              return {
                info = function() return {} end,
                status = function() return 'connected' end,
                tools_generation = function() return 0 end,
                resources = function() return {} end,
                resource_templates = function() return {} end,
                tools = function() error('metadata sentinel') end,
                prompts = function() return {} end,
                close = function() mare._mcp_closed = true end,
              }
            end
            mare.plugin('mcp', { servers = { broken = { command = 'fake', tier = 'direct', required = true } } })
        "#).unwrap();
        let h = env.host().user_config(false).faux(json!([{ "text": "must not run" }])).no_session().build().await;
        expose(&h);
        exec(&h, "REQUEST_BUILDS = 0; C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)").await;
        for _ in 0..2 {
            let result = h.prompt("hello", json!({})).await.unwrap();
            let error = result["error"]["message"].as_str().unwrap_or("");
            assert!(error.contains("required MCP server broken") && error.contains("metadata sentinel"), "{result}");
        }
        assert!(eval::<bool>(&h, "return mare._mcp_closed == true").await);
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0);
        h.shutdown().await;
    }).await
}

/// `mare mcp list` exits 0 when only optional servers fail or await approval, and exits 1 when a
/// `required` server is unavailable.
#[tokio::test]
async fn mcp_cli_list_exits_nonzero_only_for_required_servers() {
    local(async {
        let env = TestEnv::new().await;
        let missing = env.cwd.join("nonexistent-mcp-bin");
        let cfg = servers(json!({
            "opt_broken": { "command": missing.to_string_lossy(), "tier": "direct" },
        }));
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        let (code, output): (i64, String) = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return req.code, table.concat(req.lines, "\n")"#,
        )
        .await;
        assert_eq!(code, 0, "optional broken server must not cause exit code 1:\n{output}");
        assert!(output.contains("opt_broken") && output.contains("error"), "{output}");
        h.shutdown().await;

        let req_cfg = servers(json!({
            "req_broken": { "command": missing.to_string_lossy(), "tier": "direct", "required": true },
        }));
        let h = env.host().builtins(&["mcp"]).config(req_cfg).no_session().build().await;
        expose(&h);
        let (code, output): (i64, String) = eval(
            &h,
            r#"local req = { action = "list", lines = {}, code = 0 }
               C.cordis:get("bus"):emit("mcp/cli", req)
               return req.code, table.concat(req.lines, "\n")"#,
        )
        .await;
        assert_eq!(code, 1, "required broken server must cause exit code 1:\n{output}");
        assert!(output.contains("req_broken") && output.contains("error"), "{output}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn required_disconnected_server_reconnects_automatically_before_prompt() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { model = 'faux/echo' }
            mare._test_mcp_status = 'connected'
            mare._test_reconnect_calls = 0
            mare.mcp.connect = function()
              return {
                info = function() return {} end,
                status = function() return mare._test_mcp_status end,
                reconnect = function()
                  mare._test_reconnect_calls = mare._test_reconnect_calls + 1
                  mare._test_mcp_status = 'connected'
                end,
                tools_generation = function() return 0 end,
                resources = function() return {} end,
                resource_templates = function() return {} end,
                tools = function() return { { name = 'ping', inputSchema = { type = 'object' } } }, 0 end,
                prompts = function() return {} end,
                close = function() end,
                next_event = function()
                  local _, rx = mare.task.channel()
                  rx:recv()
                  return nil
                end,
              }
            end
            mare.plugin('mcp', { servers = { req_srv = { command = 'fake', tier = 'direct', required = true } } })
        "#,
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .faux(json!([{ "text": "first ok" }, { "text": "reconnected ok" }]))
            .no_session()
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("first", json!({})).await.unwrap();
        assert_eq!(r1["text"], "first ok", "{r1}");

        // Simulate disconnect (e.g. stdio exit or expired HTTP session) on a required server.
        exec(&h, "mare._test_mcp_status = 'disconnected'").await;
        let r2 = h.prompt("after disconnect", json!({})).await.unwrap();
        assert_eq!(r2["text"], "reconnected ok", "required server must auto-reconnect in prepare: {r2}");
        assert_eq!(eval::<i64>(&h, "return mare._test_reconnect_calls").await, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn stale_mcp_connect_generation_does_not_clobber_newer_connection() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        expose(&h);
        let ok: bool = eval(
            &h,
            r#"
            local conn_factory = require("mcp.connection")
            local e = {
                name = "race_srv",
                srv = { command = "fake", tier = "direct", timeout_ms = 5000 },
                tier = "direct",
                state = "idle",
                connecting = false,
                registered = {},
                registered_defs = {},
                registered_stale = {},
                registered_activate = {},
                tools = {},
                names = {},
            }
            local servers = { race_srv = e }
            local unregistered = {}
            local orig_connect = mare.mcp.connect
            local connect_n = 0
            local c1_entered_tx, c1_entered_rx = mare.task.channel()
            local c1_release_tx, c1_release_rx = mare.task.channel()
            local c1_closed = false
            local c2_client
            mare.mcp.connect = function()
                connect_n = connect_n + 1
                if connect_n == 1 then
                    c1_entered_tx:send(true)
                    c1_release_rx:recv()
                    error("stale connect #1 failed late")
                end
                c2_client = {
                    info = function() return {} end,
                    status = function() return "connected" end,
                    tools_generation = function() return 1 end,
                    resources = function() return {} end,
                    resource_templates = function() return {} end,
                    tools = function() return { { name = "fresh", inputSchema = { type = "object" } } }, 1 end,
                    prompts = function() return {} end,
                    close = function() c1_closed = true end,
                    next_event = function()
                        local _, rx = mare.task.channel()
                        rx:recv()
                        return nil
                    end,
                }
                return c2_client
            end
            local D = {
                ctx = C.cordis,
                cfg = { call_timeout_ms = 5000, startup_timeout_ms = 5000 },
                startup_cap = 60000,
                codemode = function() return false end,
                servers = servers,
                owned = {},
                pending_cache = {},
                svc = {},
                config = {
                    blocked = function() return nil end,
                },
                exposure = {
                    name_tools = function(_ent, tools)
                        local out = {}
                        for _, t in ipairs(tools) do out[t.name] = "mcp__race_srv__" .. t.name end
                        return out
                    end,
                    filter_tools = function(_ent, tools) return tools end,
                    tool_tier = function() return "direct" end,
                },
                prompts = { apply_prompts = function() end },
                results = { content_of = function(_, r) return tostring(r) end },
                elicit = { elicit = function() end },
                agent_notify = function() end,
            }
            local conn = conn_factory(D)
            local done1_tx, done1_rx = mare.task.channel()
            C.cordis:spawn(function()
                conn.connect("race_srv", true)
                done1_tx:send(true)
            end)
            assert(mare.task.timeout(5000, function() return c1_entered_rx:recv() end), "connect #1 did not start")
            -- Supersede connect #1 with a newer generation and complete connect #2 while #1 is still in flight.
            e.connecting = false
            assert(conn.connect("race_srv", true) == true, "connect #2 must succeed")
            assert(e.client == c2_client and e.state == "connected" and e.connecting == false)
            -- Now release stale connect #1 to fail: it must NOT clobber connect #2's state, client, or tools.
            c1_release_tx:send(true)
            assert(mare.task.timeout(5000, function() return done1_rx:recv() end), "connect #1 did not finish")
            mare.mcp.connect = orig_connect
            assert(e.client == c2_client, "stale failure must not clear newer client")
            assert(e.state == "connected", "stale failure must not overwrite newer connected state: " .. tostring(e.state))
            assert(e.connecting == false and e.error == nil, "stale failure must not set error on newer generation")
            assert(T:get("mcp__race_srv__fresh") ~= nil, "stale failure must not unregister newer connection tools")
            conn.disconnect(e)
            assert(c1_closed == true, "cleanup disconnect must close newer client")
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}

/// A `required=true` MCP server excluded by `--tools` (`tools_enabled`) is skipped at startup and
/// must also be excluded from `prepare`, `pending`, and `ready` so plain prompts are not blocked
/// before provider I/O. When `--tools` includes that server's tools, required readiness is still
/// enforced.
#[tokio::test]
async fn required_mcp_excluded_by_tools_enabled_does_not_block_prompt() {
    local(async {
        let env = TestEnv::new().await;
        let missing = env.cwd.join("missing-excluded-required-mcp");
        for tools_enabled in [json!(["read"]), json!([])] {
            let cfg = json!({
                "tools_enabled": tools_enabled,
                "plugins": {
                    "mcp": {
                        "servers": {
                            "req_excluded": {
                                "command": missing.to_string_lossy(),
                                "tier": "direct",
                                "required": true
                            }
                        }
                    }
                }
            });
            let h = env
                .host()
                .builtins(&["mcp"])
                .faux(json!([{ "text": "plain prompt ok" }]))
                .config(cfg)
                .no_session()
                .build()
                .await;
            let result = h.prompt("hello", json!({})).await.unwrap();
            assert!(result["error"].is_null(), "excluded required MCP must not block prompt: {result}");
            assert_eq!(result["text"], "plain prompt ok", "{result}");
            h.shutdown().await;
        }

        // When `--tools` explicitly allows a tool from the required server, readiness still blocks.
        let allowed_cfg = json!({
            "tools_enabled": ["mcp__req_excluded__ping"],
            "plugins": {
                "mcp": {
                    "servers": {
                        "req_excluded": {
                            "command": missing.to_string_lossy(),
                            "tier": "direct",
                            "required": true
                        }
                    }
                }
            }
        });
        let h = env
            .host()
            .builtins(&["mcp"])
            .faux(json!([{ "text": "must not run" }]))
            .config(allowed_cfg)
            .no_session()
            .build()
            .await;
        expose(&h);
        exec(&h, "REQUEST_BUILDS = 0; C.cordis:on('request/build', function(req, next) REQUEST_BUILDS = REQUEST_BUILDS + 1; return next() end)").await;
        let blocked = h.prompt("hello", json!({})).await.unwrap();
        assert!(
            blocked["error"]["message"].as_str().unwrap_or("").contains("required MCP server req_excluded"),
            "included required MCP must still fail closed: {blocked}"
        );
        assert_eq!(eval::<i64>(&h, "return REQUEST_BUILDS").await, 0);
        h.shutdown().await;
    })
    .await
}

/// When a required MCP server's reconnect fails transiently:
/// 1. `prepare` releases `e.connecting` before `disconnect` so it does not stall for 5 seconds.
/// 2. Subsequent prompts retry `connect` from non-auth `error` state and recover once the server is up.
/// 3. Auth failures (`needs_login`) stay non-retryable until explicit login.
#[tokio::test]
async fn required_mcp_failed_reconnect_does_not_stall_and_retries_on_next_prompt() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { model = 'faux/echo' }
            mare._test_mcp_status = 'connected'
            mare._test_connect_calls = 0
            mare._test_reconnect_calls = 0
            mare._test_reconnect_err = 'transient transport reset'
            mare.mcp.connect = function()
              mare._test_connect_calls = mare._test_connect_calls + 1
              mare._test_mcp_status = 'connected'
              return {
                info = function() return {} end,
                status = function() return mare._test_mcp_status end,
                reconnect = function()
                  mare._test_reconnect_calls = mare._test_reconnect_calls + 1
                  error(mare._test_reconnect_err)
                end,
                tools_generation = function() return mare._test_connect_calls end,
                resources = function() return {} end,
                resource_templates = function() return {} end,
                tools = function() return { { name = 'ping', inputSchema = { type = 'object' } } }, mare._test_connect_calls end,
                prompts = function() return {} end,
                close = function() end,
                next_event = function()
                  local _, rx = mare.task.channel()
                  rx:recv()
                  return nil
                end,
              }
            end
            mare.plugin('mcp', { servers = { req_srv = { command = 'fake', tier = 'direct', required = true } } })
        "#,
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .faux(json!([{ "text": "initial ok" }, { "text": "recovered ok" }]))
            .no_session()
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("first", json!({})).await.unwrap();
        assert_eq!(r1["text"], "initial ok", "{r1}");
        assert_eq!(eval::<i64>(&h, "return mare._test_connect_calls").await, 1);

        // Drop connection and fail the first reconnect transiently: must NOT hit the 5 s disconnect stall.
        exec(&h, "mare._test_mcp_status = 'disconnected'").await;
        let t0 = std::time::Instant::now();
        let r2 = h.prompt("during outage", json!({})).await.unwrap();
        let elapsed = t0.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "failed reconnect must not stall on self-held e.connecting (took {elapsed:?})"
        );
        assert!(
            r2["error"]["message"].as_str().unwrap_or("").contains("transient transport reset"),
            "expected failed reconnect denial: {r2}"
        );
        assert_eq!(eval::<i64>(&h, "return mare._test_reconnect_calls").await, 1);

        // Next prompt must retry connect from non-auth `error` state and succeed.
        let r3 = h.prompt("after recovery", json!({})).await.unwrap();
        assert_eq!(r3["text"], "recovered ok", "subsequent prompt must recover via fresh connect: {r3}");
        assert_eq!(eval::<i64>(&h, "return mare._test_connect_calls").await, 2);

        // Auth failure (401 -> needs_login) must remain non-retryable on subsequent prompts.
        exec(&h, "mare._test_reconnect_err = 'HTTP 401 unauthorized'; mare._test_mcp_status = 'disconnected'").await;
        let r4 = h.prompt("auth fail", json!({})).await.unwrap();
        assert!(r4["error"]["message"].as_str().unwrap_or("").contains("401"), "{r4}");
        let r5 = h.prompt("still needs login", json!({})).await.unwrap();
        assert!(r5["error"]["message"].as_str().unwrap_or("").contains("401"), "{r5}");
        assert_eq!(
            eval::<i64>(&h, "return mare._test_connect_calls").await,
            2,
            "needs_login state must not auto-retry connect before login"
        );
        h.shutdown().await;
    })
    .await
}

/// When a required MCP server drops, fails its first reconnect, and then has its retry connect
/// cancelled mid-flight (leaving `state = "idle"` and `client = nil`), the next prompt must still
/// retry connecting from `idle` instead of denying every future prompt permanently.
#[tokio::test]
async fn required_mcp_cancelled_reconnect_retries_on_next_prompt() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.config_dir()).unwrap();
        std::fs::write(
            env.config_dir().join("init.lua"),
            r#"
            mare.setup { model = 'faux/echo' }
            mare._test_mcp_status = 'connected'
            mare._test_connect_calls = 0
            mare._test_hang_second_connect = true
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            mare._test_entered_rx = entered_rx
            mare._test_release_tx = release_tx
            mare.mcp.connect = function(_spec, cancel)
              mare._test_connect_calls = mare._test_connect_calls + 1
              if mare._test_connect_calls == 2 and mare._test_hang_second_connect then
                entered_tx:send(true)
                release_rx:recv()
                assert(cancel and cancel:is_cancelled(), 'expected connect cancel token to be cancelled')
                error('cancelled')
              end
              mare._test_mcp_status = 'connected'
              return {
                info = function() return {} end,
                status = function() return mare._test_mcp_status end,
                reconnect = function()
                  error('transient disconnect')
                end,
                tools_generation = function() return mare._test_connect_calls end,
                resources = function() return {} end,
                resource_templates = function() return {} end,
                tools = function() return { { name = 'ping', inputSchema = { type = 'object' } } }, mare._test_connect_calls end,
                prompts = function() return {} end,
                close = function() end,
                next_event = function()
                  local _, rx = mare.task.channel()
                  rx:recv()
                  return nil
                end,
              }
            end
            mare.plugin('mcp', { servers = { req_srv = { command = 'fake', tier = 'direct', required = true } } })
        "#,
        )
        .unwrap();
        let h = env
            .host()
            .user_config(false)
            .faux(json!([{ "text": "initial ok" }, { "text": "recovered after cancel" }]))
            .no_session()
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("first", json!({})).await.unwrap();
        assert_eq!(r1["text"], "initial ok", "{r1}");
        assert_eq!(eval::<i64>(&h, "return mare._test_connect_calls").await, 1);

        // 1. Disconnect and fail reconnect -> transitions to `error` with client cleared.
        exec(&h, "mare._test_mcp_status = 'disconnected'").await;
        let r2 = h.prompt("reconnect fails", json!({})).await.unwrap();
        assert!(
            r2["error"]["message"].as_str().unwrap_or("").contains("transient disconnect"),
            "expected reconnect failure: {r2}"
        );

        // 2. Start prompt 2 so `prepare` calls `connect` (attempt #2), then cancel the turn mid-connect
        //    so `connection.lua` resets `state = "idle"` and `client = nil`.
        let idle_state: String = eval(
            &h,
            r#"
            local ag = C.cordis:get("agent")
            local started = ag:prompt("cancelled during reconnect")
            assert(mare._test_entered_rx:recv(), "second connect must start")
            ag:cancel()
            mare._test_release_tx:send(true)
            local res = ag:wait(started.turn_id)
            assert(res.status == "cancelled" or res.error ~= nil, "second turn should not succeed")
            for _, row in ipairs(C.cordis:get("mcp"):list_servers().data or {}) do
                if row.name == "req_srv" then
                    return tostring(row.state)
                end
            end
            return "missing"
            "#,
        )
        .await;
        assert_eq!(idle_state, "idle", "cancelled connect must leave server in idle state");
        assert_eq!(eval::<i64>(&h, "return mare._test_connect_calls").await, 2);

        // 3. Prompt 3 must retry from `idle`, reconnect cleanly, and succeed.
        let r3 = h.prompt("after cancel", json!({})).await.unwrap();
        assert_eq!(
            r3["text"], "recovered after cancel",
            "prompt after cancelled required MCP reconnect must retry from idle and succeed: {r3}"
        );
        assert_eq!(eval::<i64>(&h, "return mare._test_connect_calls").await, 3);
        h.shutdown().await;
    })
    .await
}
