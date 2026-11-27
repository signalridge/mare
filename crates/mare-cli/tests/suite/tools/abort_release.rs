//! Abort-safety regression tests and yield-point abort-injection fixture for:
//! - F1: persistent bash shell lock & `fg_commands` leak after `mare.task.select` forced abort
//! - F2: MCP lazy connection `e.connecting` stuck true after forced abort
//! - F3: key helper mutex lock lost after forced abort

use super::*;

/// Generic Lua abort-injection test harness: intercepts specified yieldable primitives,
/// triggers a `mare.task.select` forced abort at each yield index k = 1..max_yields,
/// then re-enters the target operation within a bounded timeout and asserts recovery.
const ABORT_INJECTION_HARNESS: &str = r#"
local function run_with_timeout(timeout_ms, fn)
  local which, a, b = mare.task.select(
    function()
      local ok, res = pcall(fn)
      return ok, res
    end,
    function()
      mare.task.sleep(timeout_ms)
      return "timeout"
    end
  )
  if which == 2 then
    return false, "timed out after " .. tostring(timeout_ms) .. "ms (likely leaked lock or stuck flag)"
  end
  return a, b
end

local function abort_at_every_yield(opts)
  local max_yields = opts.max_yields or 8
  local timeout_ms = opts.timeout_ms or 4000
  local abort_after_ms = opts.abort_after_ms or 30
  local results = {}
  for target_k = 1, max_yields do
    if opts.before_each then opts.before_each(target_k) end
    local count = 0
    local aborted = false
    local abort_tx, abort_rx = mare.task.channel()
    local restores = opts.install_hooks(function()
      count = count + 1
      if count == target_k and not aborted then
        aborted = true
        mare.task.spawn(function()
          mare.task.sleep(abort_after_ms)
          pcall(abort_tx.send, abort_tx, true)
        end)
      end
    end)
    local which = mare.task.select(
      function()
        pcall(opts.attempt, target_k)
        return "finished"
      end,
      function()
        abort_rx:recv()
        return "aborted"
      end
    )
    for i = #restores, 1, -1 do
      pcall(restores[i])
    end
    if not aborted then
      break
    end
    assert(which == 2, "expected abort branch to win at yield #" .. tostring(target_k))
    if opts.before_reenter then opts.before_reenter(target_k) end
    local ok, val = run_with_timeout(timeout_ms, function()
      return opts.reenter(target_k)
    end)
    results[#results + 1] = { k = target_k, ok = ok, val = val }
    assert(ok, "re-entry after abort at yield #" .. tostring(target_k) .. " failed: " .. tostring(val))
  end
  assert(#results > 0, "abort_at_every_yield observed 0 yield points")
  return results
end
"#;

#[cfg(unix)]
#[tokio::test]
async fn f1_persistent_bash_lock_and_fg_commands_survive_forced_abort() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["jobs", "ui"])
            .config(json!({
                "plugins": {
                    "tools": {
                        "shell_session": "persistent",
                        "cancel_grace_ms": 100
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);

        let script = format!(
            r#"
            {ABORT_INJECTION_HARNESS}
            -- 1. Trigger a real stubborn command (`trap '' INT TERM; sleep 30`), cancel, let pipeline
            -- force-abort after cancel_grace_ms (100ms) while h:stop is still waiting on SIGINT->SIGTERM.
            local tok = mare.task.token()
            mare.task.spawn(function()
              mare.task.sleep(150)
              tok:cancel()
            end)
            local first = T:execute({{
              id = "f1_stubborn",
              name = "bash",
              arguments = {{ command = "trap '' INT TERM; echo READY; sleep 30", description = "stubborn" }},
            }}, {{ cancel = tok }})

            -- 2. Re-enter persistent bash with a quick command: must return "ok" well within 3s.
            local ok, second = run_with_timeout(3000, function()
              return T:execute({{
                id = "f1_follow",
                name = "bash",
                arguments = {{ command = "echo ok", description = "check lock" }},
              }}, {{}})
            end)
            assert(ok, "persistent bash hung after forced abort: " .. tostring(second))
            local text = type(second.content) == "table" and second.content[1].text or tostring(second.content)

            -- 3. `fg_commands` must have returned to 0 so `background` action declines when idle.
            local ui_svc = C.cordis:get("ui")
            local bg_res = ui_svc and ui_svc:call_keymap("editor", nil, "background")
            return mare.json.encode({{
              first_cancelled = first.cancelled == true or first.is_error == true,
              second_error = second.is_error == true,
              second_text = text,
              bg_idle = type(bg_res) == "table" and bg_res.declined == true and bg_res.handled == false,
            }})
            "#
        );
        let out_json: String = eval(&h, &script).await;
        h.shutdown().await;
        let out: serde_json::Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["first_cancelled"], true, "{out}");
        assert_eq!(out["second_error"], false, "{out}");
        assert!(out["second_text"].as_str().unwrap_or("").contains("ok"), "{out}");
        assert_eq!(out["bg_idle"], true, "fg_commands leaked after abort (background action must return false): {out}");
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn f2_mcp_lazy_connect_clears_connecting_flag_on_forced_abort_and_reconnects() {
    local(async {
        let env = TestEnv::new().await;
        let log = env.home.path().join("f2_mcp.log");
        let gate = env.home.path().join("f2_gate");
        let wrapper = env.home.path().join("f2_mcp_wrapper.sh");
        let fake_cfg = json!({ "log": log, "tools": ["echo"] }).to_string();
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ ! -f '{}' ]; then\n  sleep 60\n  exit 1\nfi\nexec '{}' scripted '{}'\n",
                gate.display(),
                fake_mcp_bin(),
                fake_cfg.replace('\'', "'\\''"),
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cfg = json!({
            "plugins": {
                "tools": { "cancel_grace_ms": 100 },
                "mcp": {
                    "servers": {
                        "slow": {
                            "command": wrapper.to_string_lossy(),
                            "tier": "deferred",
                            "startup_timeout_ms": 20000
                        }
                    }
                }
            }
        });

        // 1. Warm the deferred tool cache with the gate open so `mcp__slow__echo` is registered
        //    lazily on next startup without connecting upfront.
        std::fs::write(&gate, "open").unwrap();
        let warm = env.host().builtins(&["mcp"]).config(cfg.clone()).no_session().build().await;
        expose(&warm);
        exec(
            &warm,
            r#"local r = T:execute({ id = "w1", name = "tool_search", arguments = { query = "echo" } }, {})
               assert(r.details and r.details.loaded_tools and r.details.loaded_tools[1] == "mcp__slow__echo")
               C.cordis:get("bus"):emit("request/prepare", {})"#,
        )
        .await;
        warm.shutdown().await;

        // 2. Close gate so `mare.mcp.connect` hangs during initialize handshake; start fresh host.
        std::fs::remove_file(&gate).unwrap();
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "M", h.service_table("mcp").unwrap());

        let gate_str = gate.to_string_lossy().to_string();
        let script = format!(
            r#"
            {ABORT_INJECTION_HARNESS}
            local gate_path = {gate_lit}
            local tok = mare.task.token()
            mare.task.spawn(function()
              mare.task.sleep(150)
              tok:cancel()
            end)
            local first = T:execute({{
              id = "f2_hang",
              name = "mcp__slow__echo",
              arguments = {{ text = "first" }},
            }}, {{ cancel = tok }})

            -- Inspect server state immediately after forced abort: must NOT stay stuck in "connecting".
            local snap = M:list_servers()
            local state_after_abort = ""
            for _, s in ipairs(snap.data or {{}}) do
              if s.name == "slow" then state_after_abort = s.state end
            end

            -- Open gate so server is healthy, then reconnect + invoke within 5s (NOT 65s hang).
            mare.fs.write(gate_path, "open")
            local ok_rec, rec = run_with_timeout(5000, function()
              return M:reconnect("slow")
            end)
            assert(ok_rec, "mcp reconnect hung after forced abort: " .. tostring(rec))

            local ok_call, call_res = run_with_timeout(5000, function()
              return T:execute({{
                id = "f2_recover",
                name = "mcp__slow__echo",
                arguments = {{ text = "recovered" }},
              }}, {{}})
            end)
            assert(ok_call, "mcp tool call hung after reconnect: " .. tostring(call_res))
            local call_text = type(call_res.content) == "table" and call_res.content[1].text or tostring(call_res.content)

            return mare.json.encode({{
              first_cancelled = first.cancelled == true or first.is_error == true,
              state_after_abort = state_after_abort,
              reconnected = rec ~= nil,
              call_error = call_res.is_error == true,
              call_text = call_text,
            }})
            "#,
            gate_lit = serde_json::to_string(&gate_str).unwrap()
        );
        let out_json: String = eval(&h, &script).await;
        h.shutdown().await;
        let out: serde_json::Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(out["first_cancelled"], true, "{out}");
        assert_ne!(out["state_after_abort"], "connecting", "e.connecting stuck true after forced abort: {out}");
        assert_eq!(out["reconnected"], true, "reconnect must succeed promptly after abort: {out}");
        assert_eq!(out["call_error"], false, "{out}");
        assert_eq!(out["call_text"], "echo: recovered", "{out}");
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn f3_key_helper_mutex_survives_select_abort() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({
                "providers": {
                    "deepseek": {
                        "api_key": {
                            "cmd": ["sh", "-c", "sleep 0.6; echo secret-key-f3"]
                        }
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        global(&h, "LLM", h.service_table("llm").unwrap());

        let script = format!(
            r#"
            {ABORT_INJECTION_HARNESS}
            local which = mare.task.select(
              function()
                return LLM:api_key("deepseek")
              end,
              function()
                mare.task.sleep(50)
                return "aborted"
              end
            )
            assert(which == 2, "expected 50ms select branch to abort api_key helper")

            local ok, key = run_with_timeout(3000, function()
              return LLM:api_key("deepseek")
            end)
            assert(ok, "LLM:api_key hung after select abort: " .. tostring(key))
            return tostring(key)
            "#
        );
        let key: String = eval(&h, &script).await;
        h.shutdown().await;
        assert_eq!(key, "secret-key-f3");
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn abort_injection_fixture_reenters_cleanly_at_every_yield_point() {
    local(async {
        let env = TestEnv::new().await;
        let log = env.home.path().join("inject_mcp.log");
        let fake_cfg = json!({ "log": log, "tools": ["echo"] }).to_string();
        let cfg = json!({
            "providers": {
                "deepseek": {
                    "api_key": {
                        "cmd": ["sh", "-c", "sleep 0.15; echo injected-key"]
                    }
                }
            },
            "plugins": {
                "tools": { "shell_session": "persistent", "cancel_grace_ms": 100 },
                "mcp": {
                    "servers": {
                        "inj": {
                            "command": fake_mcp_bin(),
                            "args": ["scripted", fake_cfg],
                            "tier": "deferred"
                        }
                    }
                }
            }
        });

        let h = env.host().builtins(&["jobs", "mcp", "ui"]).config(cfg).no_session().build().await;
        expose(&h);
        global(&h, "LLM", h.service_table("llm").unwrap());
        global(&h, "M", h.service_table("mcp").unwrap());

        let script = format!(
            r#"
            {ABORT_INJECTION_HARNESS}

            -- A) F3 abort injection across mare.proc.run yield point in llm:api_key
            local f3_res = abort_at_every_yield({{
              max_yields = 4,
              timeout_ms = 3000,
              abort_after_ms = 20,
              before_each = function()
                LLM:invalidate_api_key("deepseek")
              end,
              install_hooks = function(on_yield)
                local orig = mare.proc.run
                mare.proc.run = function(...)
                  on_yield()
                  return orig(...)
                end
                return {{ function() mare.proc.run = orig end }}
              end,
              attempt = function()
                return LLM:api_key("deepseek")
              end,
              reenter = function()
                local k = LLM:api_key("deepseek")
                assert(k == "injected-key", "unexpected key: " .. tostring(k))
                return k
              end,
            }})

            -- B) F2 abort injection across mare.mcp.connect yield point (and post-connect tools() yield)
            local f2_res = abort_at_every_yield({{
              max_yields = 4,
              timeout_ms = 5000,
              abort_after_ms = 25,
              before_each = function()
                local bus = C.cordis:get("bus")
                bus:emit("mcp/cli", {{ action = "disable", server = "inj" }})
                bus:emit("mcp/cli", {{ action = "enable", server = "inj" }})
              end,
              install_hooks = function(on_yield)
                local orig_connect = mare.mcp.connect
                mare.mcp.connect = function(spec, tok)
                  on_yield()
                  -- Yield #1: abort before native connect completes
                  mare.task.sleep(80)
                  local client = orig_connect(spec, tok)
                  -- Yield #2: abort after native connect returned a live client
                  on_yield()
                  mare.task.sleep(80)
                  return client
                end
                return {{ function() mare.mcp.connect = orig_connect end }}
              end,
              attempt = function()
                return M:reconnect("inj")
              end,
              reenter = function()
                local snap = M:list_servers()
                for _, s in ipairs(snap.data or {{}}) do
                  assert(s.name ~= "inj" or s.state ~= "connecting", "stuck in connecting")
                end
                local rec = M:reconnect("inj")
                assert(rec ~= nil, "reconnect failed after abort")
                local r = T:execute({{ id = "inj_ok", name = "mcp__inj__echo", arguments = {{ text = "hi" }} }}, {{}})
                local txt = type(r.content) == "table" and r.content[1].text or tostring(r.content)
                assert(txt == "echo: hi", "failed to call mcp tool after abort: " .. txt)
                return true
              end,
            }})

            -- C) F1 abort injection across persistent bash execution yield points
            local f1_res = abort_at_every_yield({{
              max_yields = 4,
              timeout_ms = 3000,
              abort_after_ms = 25,
              install_hooks = function(on_yield)
                local orig_start = mare.proc.start
                mare.proc.start = function(...)
                  on_yield()
                  mare.task.sleep(80)
                  return orig_start(...)
                end
                return {{ function() mare.proc.start = orig_start end }}
              end,
              attempt = function()
                return T:execute({{
                  id = "inj_bash",
                  name = "bash",
                  arguments = {{ command = "sleep 2; echo slow", description = "slow" }},
                }}, {{}})
              end,
              reenter = function()
                local r = T:execute({{
                  id = "inj_bash_ok",
                  name = "bash",
                  arguments = {{ command = "echo recovered_f1", description = "recover" }},
                }}, {{}})
                local txt = type(r.content) == "table" and r.content[1].text or tostring(r.content)
                assert(txt:find("recovered_f1", 1, true), "bash did not recover: " .. txt)
                local ui_svc = C.cordis:get("ui")
                local bg = ui_svc and ui_svc:call_keymap("editor", nil, "background")
                assert(type(bg) == "table" and bg.declined == true and bg.handled == false, "fg_commands leaked")
                return true
              end,
            }})

            return mare.json.encode({{
              f1_points = #f1_res,
              f2_points = #f2_res,
              f3_points = #f3_res,
            }})
            "#
        );
        let out_json: String = eval(&h, &script).await;
        h.shutdown().await;
        let out: serde_json::Value = serde_json::from_str(&out_json).unwrap();
        assert!(out["f1_points"].as_u64().unwrap_or(0) >= 1, "{out}");
        assert!(out["f2_points"].as_u64().unwrap_or(0) >= 2, "{out}");
        assert!(out["f3_points"].as_u64().unwrap_or(0) >= 1, "{out}");
    })
    .await
}
