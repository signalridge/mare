//! Proxy `call_tool` tests: enablement, outer `side_effect`, schema validation without schema
//! echo, stringified JSON arguments, permissions/approvals/hooks on inner tool name, UI single-card
//! display, parallelism (read-read concurrent, write-read serial), and subagent tool ceiling.

use super::*;
use crate::suite::kernel::write_plugin;

const CALL_TOOL_PLUGIN: &str = r#"return {
  name = "call-tool-test",
  inject = { "tools" },
  apply = function(ctx)
    mare._active = 0
    mare._max_active = 0
    mare._hook_names = {}
    ctx.tools:register("def_echo", {
      description = "Echo a required string value",
      exposure = "deferred",
      side_effect = "read",
      parameters = {
        type = "object",
        properties = {
          msg = { type = "string", description = "Message to echo" },
          delay_ms = { type = "integer" },
        },
        required = { "msg" },
      },
      summary = function(a)
        return "Echo(" .. tostring(a.msg or "") .. ")"
      end,
      execute = function(a)
        mare._active = (mare._active or 0) + 1
        if mare._active > (mare._max_active or 0) then
          mare._max_active = mare._active
        end
        if tonumber(a.delay_ms) and tonumber(a.delay_ms) > 0 then
          mare.task.sleep(tonumber(a.delay_ms))
        end
        mare._active = mare._active - 1
        return { content = "echo:" .. tostring(a.msg or ""), details = { echoed = a.msg } }
      end,
    })
    ctx.tools:register("def_write", {
      description = "Mutating deferred tool",
      exposure = "deferred",
      side_effect = "write",
      parameters = {
        type = "object",
        properties = {
          target = { type = "string" },
          delay_ms = { type = "integer" },
        },
        required = { "target" },
      },
      execute = function(a)
        mare._active = (mare._active or 0) + 1
        if mare._active > (mare._max_active or 0) then
          mare._max_active = mare._active
        end
        if tonumber(a.delay_ms) and tonumber(a.delay_ms) > 0 then
          mare.task.sleep(tonumber(a.delay_ms))
        end
        mare._active = mare._active - 1
        return { content = "wrote:" .. tostring(a.target or "") }
      end,
    })
    ctx.tools:register("def_external", {
      description = "External deferred tool requiring approval",
      exposure = "deferred",
      side_effect = "external",
      unconfined = true,
      parameters = {
        type = "object",
        properties = { action = { type = "string" } },
        required = { "action" },
      },
      execute = function(a)
        return { content = "ext:" .. tostring(a.action or "") }
      end,
    })
    ctx.tools:register("inner_nested", {
      description = "Nested-only tool",
      exposure = "nested",
      side_effect = "read",
      parameters = { type = "object", properties = {} },
      execute = function()
        return { content = "nested" }
      end,
    })
  end,
}"#;

#[tokio::test]
async fn call_tool_enablement_side_effect_validation_and_target_guards() {
    local(async {
        let env = TestEnv::new().await;
        // Without deferred tools, call_tool is not declared.
        let h_empty = host(&env).await;
        let empty_has_call_tool: bool = eval(
            &h_empty,
            r#"for _, d in ipairs(T:definitions({})) do if d.name == "call_tool" then return true end end return false"#,
        )
        .await;
        assert!(!empty_has_call_tool, "call_tool must not be declared when no deferred tools exist");
        h_empty.shutdown().await;

        write_plugin(&env, "call-tool-test", CALL_TOOL_PLUGIN);
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);

        // Lock outer call_tool side_effect="read", unconfined=false, enabled only on non-native models.
        let (se, unconfined, non_native_has, native_has): (String, bool, bool, bool) = eval(
            &h,
            r#"local se, _, unc = T:side_effect("call_tool")
               local function has_ct(m)
                 for _, d in ipairs(T:definitions(m)) do
                   if d.name == "call_tool" then return true end
                 end
                 return false
               end
               return se, unc, has_ct({}), has_ct({ tool_update = "addition_only" })"#,
        )
        .await;
        assert_eq!(se, "read", "outer call_tool must declare side_effect='read' to never prompt externally");
        assert!(!unconfined, "outer call_tool must not be unconfined");
        assert!(non_native_has, "non-native models must declare call_tool");
        assert!(!native_has, "native tool_update='addition_only' models must omit call_tool");

        // Success path returns inner model-visible content and details.
        let ok = call(&h, "call_tool", json!({ "name": "def_echo", "arguments": { "msg": "hello" } })).await;
        assert!(ok["is_error"].is_null() || ok["is_error"] == false, "{ok}");
        assert_eq!(text(&ok), "echo:hello");
        assert_eq!(ok["details"]["echoed"], "hello");

        // Stringified JSON arguments are decoded cleanly.
        let str_args = call(&h, "call_tool", json!({ "name": "def_echo", "arguments": r#"{"msg":"from-json"}"# })).await;
        assert!(str_args["is_error"].is_null() || str_args["is_error"] == false, "{str_args}");
        assert_eq!(text(&str_args), "echo:from-json");

        // Schema validation error returns error text ONLY (no schema appended), and retry succeeds.
        let bad = call(&h, "call_tool", json!({ "name": "def_echo", "arguments": { "msg": 123 } })).await;
        assert_eq!(bad["is_error"], true, "{bad}");
        let bad_text = text(&bad);
        assert!(bad_text.contains("Invalid arguments"), "{bad_text}");
        assert!(
            !bad_text.contains("Message to echo") && !bad_text.contains("\"properties\""),
            "validation failure must not append schema text: {bad_text}"
        );
        let retry = call(&h, "call_tool", json!({ "name": "def_echo", "arguments": { "msg": "fixed" } })).await;
        assert_eq!(text(&retry), "echo:fixed");

        // Reject direct, nested, self, tool_search, and nonexistent targets.
        for (target, expected_substr) in [
            ("read", "not a deferred tool"),
            ("inner_nested", "cannot be invoked through call_tool"),
            ("call_tool", "Invalid deferred tool name"),
            ("tool_search", "Invalid deferred tool name"),
            ("does_not_exist", "Unknown deferred tool"),
        ] {
            let res = call(&h, "call_tool", json!({ "name": target, "arguments": {} })).await;
            assert_eq!(res["is_error"], true, "expected error for target {target}: {res}");
            assert!(
                text(&res).contains(expected_substr),
                "expected '{expected_substr}' for target {target}, got: {}",
                text(&res)
            );
        }

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn call_tool_permissions_single_inner_approval_hooks_and_ui_card() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "call-tool-test", CALL_TOOL_PLUGIN);
        let h = env
            .host()
            .builtins(&["permissions", "ui"])
            .user_plugins()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "preset": "auto",
                        "rules": [
                            { "tool": "def_write", "decision": "forbidden" }
                        ]
                    }
                }
            }))
            .no_session()
            .build()
            .await;
        expose(&h);

        // 1. Deny rule targeting inner name blocks invocation via call_tool.
        let denied = call(&h, "call_tool", json!({ "name": "def_write", "arguments": { "target": "x" } })).await;
        assert_eq!(denied["is_error"], true, "{denied}");
        assert!(
            text(&denied).to_lowercase().contains("denied") || text(&denied).to_lowercase().contains("forbidden"),
            "deny rule on inner tool must block call_tool: {denied}"
        );

        // 2. Inner tool requiring approval prompts ONCE with inner tool name as primary + via call_tool source marker,
        // and tool/before + tool/rewrite hooks observe the inner real tool name.
        let out_json: String = eval(
            &h,
            r#"
            local policy = C.cordis:get("policy")
            local seen_before, seen_rewrite, prompts = {}, {}, {}
            policy:rewriter("tool/rewrite", function(c)
              seen_rewrite[#seen_rewrite + 1] = c.name
              return nil
            end)
            policy:voter("tool/before", function(c)
              seen_before[#seen_before + 1] = c.name
              return nil
            end)
            local res = T:execute({
              id = "c_ext",
              name = "call_tool",
              arguments = { name = "def_external", arguments = { action = "deploy" } },
            }, {
              approve = function(c, _info, verdict)
                prompts[#prompts + 1] = { tool = c.name, reason = verdict and verdict.reason or "" }
                return { decision = "allow" }
              end,
            })
            return mare.json.encode({
              res = res,
              prompts = prompts,
              seen_before = seen_before,
              seen_rewrite = seen_rewrite,
            })
            "#,
        )
        .await;
        let out: serde_json::Value = serde_json::from_str(&out_json).unwrap();
        assert_eq!(text(&out["res"]), "ext:deploy", "{out}");
        let prompts = out["prompts"].as_array().unwrap();
        assert_eq!(prompts.len(), 1, "must prompt exactly once (for inner tool only): {out}");
        assert_eq!(prompts[0]["tool"], "def_external", "approval prompt must show inner tool name: {out}");
        assert!(
            prompts[0]["reason"].as_str().unwrap_or("").contains("via call_tool"),
            "approval reason must mark call_tool as source: {out}"
        );
        assert!(
            out["seen_before"].as_array().unwrap().iter().any(|n| n == "def_external"),
            "tool/before hook must see inner tool name: {out}"
        );
        assert!(
            out["seen_rewrite"].as_array().unwrap().iter().any(|n| n == "def_external"),
            "tool/rewrite hook must see inner tool name: {out}"
        );

        // 3. UI tool_node renders call_tool as a single card with inner tool as primary and via call_tool source marker.
        let ui_json: String = eval(
            &h,
            r#"
            local tn = require("ui.tool_nodes")({
              ctx = C.cordis,
              cfg = { group_lines = 4, preview_lines = 2 },
              U = mare.ui,
            })
            local node = tn.tool_node(
              { id = "u1", name = "call_tool", arguments = { name = "def_echo", arguments = { msg = "hi" } } },
              { content = "echo:hi", is_error = false },
              false
            )
            return mare.json.encode(node)
            "#,
        )
        .await;
        assert!(
            ui_json.contains(r#""text":"Echo""#) && ui_json.contains(r#""text":"(hi)""#),
            "UI card must use inner tool summary as primary: {ui_json}"
        );
        assert!(ui_json.contains("via call_tool"), "UI card must mark call_tool as source: {ui_json}");
        assert!(!ui_json.contains(r#""text":"call_tool""#), "UI must not render a raw outer call_tool card: {ui_json}");

        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn call_tool_parallelism_matches_inner_concurrency_safety() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "call-tool-test", CALL_TOOL_PLUGIN);

        // Two read-only call_tool calls in one turn run concurrently (max_active >= 2).
        let h_par = env
            .host()
            .faux(json!([
                {
                    "tool_calls": [
                        { "name": "call_tool", "arguments": { "name": "def_echo", "arguments": { "msg": "a", "delay_ms": 120 } } },
                        { "name": "call_tool", "arguments": { "name": "def_echo", "arguments": { "msg": "b", "delay_ms": 120 } } },
                    ]
                },
                { "text": "par done" }
            ]))
            .user_plugins()
            .no_session()
            .build()
            .await;
        let res_par = h_par.prompt("run two reads", json!({})).await.unwrap();
        assert!(res_par["error"].is_null(), "{res_par}");
        let max_par: i64 = eval(&h_par, "return mare._max_active").await;
        h_par.shutdown().await;
        assert!(max_par >= 2, "two read-only call_tool calls must execute concurrently, got max_active={max_par}");

        // One write + one read call_tool call in one turn serialize (max_active == 1).
        let h_ser = env
            .host()
            .faux(json!([
                {
                    "tool_calls": [
                        { "name": "call_tool", "arguments": { "name": "def_write", "arguments": { "target": "f", "delay_ms": 100 } } },
                        { "name": "call_tool", "arguments": { "name": "def_echo", "arguments": { "msg": "r", "delay_ms": 100 } } },
                    ]
                },
                { "text": "ser done" }
            ]))
            .user_plugins()
            .no_session()
            .build()
            .await;
        let res_ser = h_ser.prompt("run write and read", json!({})).await.unwrap();
        assert!(res_ser["error"].is_null(), "{res_ser}");
        let max_ser: i64 = eval(&h_ser, "return mare._max_active").await;
        h_ser.shutdown().await;
        assert_eq!(max_ser, 1, "write + read call_tool calls must serialize, got max_active={max_ser}");
    })
    .await
}

#[tokio::test]
async fn call_tool_enforces_subagent_tool_allowed_ceiling() {
    local(async {
        let env = TestEnv::new().await;
        write_plugin(&env, "call-tool-test", CALL_TOOL_PLUGIN);
        write_plugin(
            &env,
            "subagent-call-tool-probe",
            r#"return {
  name = "subagent-call-tool-probe",
  inject = { "bus", "tools" },
  contracts = {
    probe = {
      owner = "subagent-call-tool-probe",
      events = {
        ["call-tool-ceiling"] = {
          doc = "probe",
          mode = "emit",
          expose = "both",
          delivery = "required",
          ignorable = false,
          data = { type = "object", additionalProperties = true },
          example = {},
        },
      },
    },
  },
  apply = function(ctx)
    if mare.mode == "child" then
      ctx:on("turn/started", function()
        local blocked = ctx.tools:execute({
          id = "sub_blocked",
          name = "call_tool",
          arguments = { name = "def_write", arguments = { target = "nope" } },
        }, {})
        local allowed = ctx.tools:execute({
          id = "sub_allowed",
          name = "call_tool",
          arguments = { name = "def_echo", arguments = { msg = "ok" } },
        }, {})
        ctx.bus:emit("probe/call-tool-ceiling", {
          blocked_err = blocked.is_error == true,
          blocked_text = type(blocked.content) == "table" and blocked.content[1].text or tostring(blocked.content),
          allowed_err = allowed.is_error == true,
          allowed_text = type(allowed.content) == "table" and allowed.content[1].text or tostring(allowed.content),
        })
      end)
    end
  end,
}"#,
        );
        let h = env
            .host()
            .builtins(&["subagent", "permissions", "context", "session"])
            .user_plugins()
            .faux(json!([{ "text": "done" }]))
            .no_session()
            .build()
            .await;
        expose(&h);
        let probe_json: String = eval(
            &h,
            r#"
            local svc = assert(C.cordis:get("subagent"), "subagent service missing")
            local remove_type = svc:register_type({
              name = "call-tool-narrow",
              description = "Narrow child with call_tool + def_echo only",
              tools = { "call_tool", "def_echo" },
            })
            local spawn, handle = mare.subagent.start, nil
            mare.subagent.start = function(opts) handle = spawn(opts) return handle end
            local child = svc:start({
              type = "call-tool-narrow",
              prompt = "test ceiling",
              tools = { "call_tool", "def_echo" },
            })
            local payload
            while true do
              local ev = handle:next_event()
              if not ev then break end
              if ev.name == "probe/call-tool-ceiling" then
                payload = ev.payload
                break
              end
            end
            svc:wait(child.subagent_id, { timeout_ms = 10000 })
            remove_type()
            local result = assert(payload, "probe/call-tool-ceiling not observed")
            return mare.json.encode(result)
            "#,
        )
        .await;
        h.shutdown().await;

        let probe: serde_json::Value = serde_json::from_str(&probe_json).unwrap();
        assert_eq!(probe["blocked_err"], true, "excluded deferred tool must be blocked via call_tool: {probe}");
        assert!(
            probe["blocked_text"].as_str().unwrap_or("").contains("Unknown or unavailable"),
            "unexpected blocked error: {probe}"
        );
        assert_eq!(probe["allowed_err"], false, "allowed deferred tool must succeed via call_tool: {probe}");
        assert_eq!(probe["allowed_text"], "echo:ok", "{probe}");
    })
    .await
}

/// `call_tool` is a pure dispatcher:
/// 1. Inner deferred tools that receive an approved sandbox escalation (e.g. `read-only` -> write)
///    or run unconfined under `fs_deny_read` must not be clamped by an outer un-elevated session
///    sandbox wrapper around `call_tool`.
/// 2. Orphan recovery (`agent.orphans`) must classify an interrupted `call_tool` by its inner tool's
///    `side_effect` (reporting `outcome_unknown` for writes instead of `safe to run it again`).
/// 3. `llm:constrain_request` filters apply to deferred tools dispatched via `call_tool`.
#[tokio::test]
async fn call_tool_preserves_inner_escalation_orphan_side_effect_and_constrain_request() {
    local(async {
        let env = TestEnv::new().await;
        let denied_file = env.cwd.join("secret.txt");
        std::fs::write(&denied_file, "top-secret").unwrap();
        let out_file = env.cwd.join("approved_write.txt");

        let h = env
            .host()
            .builtins(&["permissions"])
            .project_trusted(true)
            .no_session()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only",
                        "deny_read": [denied_file.to_string_lossy().to_string()],
                        "accept_partial": ["fs_deny_read", "fs_protect", "network"]
                    }
                }
            }))
            .faux(json!([]))
            .build_exposed()
            .await;

        global(&h, "OUT_PATH", out_file.to_string_lossy().to_string());
        let ok: bool = eval(
            &h,
            r#"
            -- 1a. Deferred write tool approved under read-only preset must actually be able to write
            -- without outer call_tool clamping the inner approved workspace-write sandbox back to read-only.
            T:register("deferred_writer", {
                description = "Deferred file writer",
                exposure = "deferred",
                side_effect = "write",
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                execute = function(a)
                    mare.fs.write(a.path, "written-via-call-tool")
                    return { content = "wrote:" .. a.path }
                end,
            })
            local write_asks = 0
            local r_write = T:execute({
                id = "ct_w1",
                name = "call_tool",
                arguments = { name = "deferred_writer", arguments = { path = OUT_PATH } },
            }, {
                approve = function()
                    write_asks = write_asks + 1
                    return { decision = "allow" }
                end,
            })
            assert(write_asks == 1, "expected inner approval ask, got " .. tostring(write_asks))
            assert(not r_write.is_error, "approved deferred write via call_tool must not fail with SANDBOX_DENIED: " .. tostring(r_write.content))

            -- 1b. Direct call vs call_tool parity under `deny_read`: `call_tool` must not add any
            -- extra sandbox restriction beyond what a direct top-level execution sees, and must
            -- preserve `sandbox` escalation granted to the inner tool.
            T:register("deferred_unconfined_probe", {
                description = "Deferred unconfined probe",
                exposure = "deferred",
                unconfined = true,
                side_effect = "read",
                parameters = { type = "object" },
                execute = function()
                    local ec = mare.exec.current()
                    local dr = ec and ec.sandbox and ec.sandbox.deny_read or {}
                    return { content = "level=" .. tostring(ec and ec.sandbox and ec.sandbox.level) .. ",dr=" .. tostring(#dr) }
                end,
            })
            local r_unconf = T:execute({
                id = "ct_u1",
                name = "call_tool",
                arguments = { name = "deferred_unconfined_probe", arguments = {} },
            }, {
                approve = function() return { decision = "allow" } end,
            })
            local r_direct = T:execute({
                id = "ct_u2",
                name = "deferred_unconfined_probe",
                arguments = {},
            }, {
                approve = function() return { decision = "allow" } end,
            })
            assert(
                not r_unconf.is_error and tostring(r_unconf.content) == tostring(r_direct.content),
                "call_tool must match top-level execution policy for deferred tools: call_tool=" .. tostring(r_unconf.content) .. " direct=" .. tostring(r_direct.content)
            )

            -- 2. Orphan hint for interrupted call_tool reflects inner tool's side_effect.
            local orphans = require("agent.orphans")({
                ctx = C.cordis,
                A = { tool_states = {} },
                append = function() end,
            })
            local write_hint = orphans.text({
                id = "ct_orphan_w",
                name = "call_tool",
                arguments = { name = "deferred_writer", arguments = { path = OUT_PATH } },
            })
            assert(
                write_hint:find("outcome_unknown", 1, true),
                "interrupted call_tool wrapping write tool must report outcome_unknown, got: " .. tostring(write_hint)
            )
            T:register("deferred_reader", {
                description = "Deferred reader",
                exposure = "deferred",
                side_effect = "read",
                parameters = { type = "object" },
                execute = function() return { content = "ok" } end,
            })
            local read_hint = orphans.text({
                id = "ct_orphan_r",
                name = "call_tool",
                arguments = { name = "deferred_reader", arguments = {} },
            })
            assert(
                read_hint:find("safe to run it again", 1, true),
                "interrupted call_tool wrapping read tool should remain safe to re-run, got: " .. tostring(read_hint)
            )

            -- 3. llm:constrain_tools filter also applies to call_tool dispatch.
            local llm = C.cordis:get("llm")
            local off_constrain = llm:constrain_tools(function()
                return { "deferred_writer" }
            end)
            local _cleanup <close> = setmetatable({}, { __close = function() off_constrain() end })
            local r_constrained = T:execute({
                id = "ct_c1",
                name = "call_tool",
                arguments = { name = "deferred_reader", arguments = {} },
            }, {})
            assert(
                r_constrained.is_error == true and tostring(r_constrained.content):find("not available", 1, true),
                "deferred tool dropped by llm:constrain_tools must be blocked via call_tool: " .. tostring(r_constrained.content)
            )
            return true
            "#,
        )
        .await;
        assert!(ok);
        assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "written-via-call-tool");
        h.shutdown().await;
    })
    .await
}

/// End-to-end regressions for `call_tool` and `tools:output_channel`:
/// - `call_tool` uses `timeout_policy = "managed"` so outer `tools.timeout_ms` does not cut off
///   inner approval waits (while inner tools still enforce their own timeout budget).
/// - Stringified `call_tool` arguments decode via `decode_lossless` (preserving explicit JSON
///   `null` identically to object arguments) and reject JSON arrays like `"[]"`.
/// - Caller `opts.sandbox` and outer `tool/before` sandbox constraints are forwarded narrow-only
///   through `call_tool` to the inner deferred tool.
/// - `call_tool` forwards `on_output` for model-facing calls and maps `tool_call_id` to the
///   outer call ID so `tools:output_channel()` works identically direct vs via `call_tool`.
/// - Closing an output channel (explicit `close()` or `session/changed` bulk close) disposes the
///   owner effect created by `lib.owned` instead of leaking it until plugin unload.
#[tokio::test]
async fn call_tool_managed_timeout_lossless_null_sandbox_output_channel_and_disposal() {
    local(async {
        let env = TestEnv::new().await;
        let out_file = env.cwd.join("sandbox_cap_write.txt");

        let h = env
            .host()
            .builtins(&["permissions"])
            .project_trusted(true)
            .no_session()
            .config(json!({
                "plugins": {
                    "tools": { "timeout_ms": 150 },
                    "permissions": {
                        "enabled": true,
                        "preset": "auto"
                    }
                }
            }))
            .faux(json!([]))
            .build_exposed()
            .await;

        global(&h, "OUT_PATH", out_file.to_string_lossy().to_string());
        let ok: bool = eval(
            &h,
            r#"
            -- 1. Outer call_tool must not time out during inner approval wait (> 150 ms).
            T:register("def_ext_wait", {
                description = "External deferred tool with approval wait",
                exposure = "deferred",
                side_effect = "external",
                unconfined = true,
                parameters = { type = "object", properties = { msg = { type = "string" } }, required = { "msg" } },
                execute = function(a)
                    return { content = "approved:" .. tostring(a.msg) }
                end,
            })
            local r_wait = T:execute({
                id = "ct_timeout_approve",
                name = "call_tool",
                arguments = { name = "def_ext_wait", arguments = { msg = "ok" } },
            }, {
                approve = function()
                    mare.task.sleep(280)
                    return { decision = "allow" }
                end,
            })
            assert(not r_wait.is_error and r_wait.content == "approved:ok",
                "call_tool must not time out during inner approval wait: " .. mare.json.encode(r_wait))

            -- Inner tool's own execution budget still times out when it exceeds tools.timeout_ms.
            T:register("def_slow_exec", {
                description = "Slow deferred tool",
                exposure = "deferred",
                side_effect = "read",
                parameters = { type = "object", properties = {} },
                execute = function()
                    mare.task.sleep(400)
                    return { content = "too late" }
                end,
            })
            local r_slow = T:execute({
                id = "ct_timeout_slow",
                name = "call_tool",
                arguments = { name = "def_slow_exec", arguments = {} },
            }, {})
            assert(r_slow.is_error == true and r_slow.error_code == "tool_timeout",
                "inner tool exceeding timeout_ms must still time out: " .. mare.json.encode(r_slow))

            -- 2. String vs object arguments preserve explicit JSON null, and "[]" is rejected.
            T:register("def_nullable", {
                description = "Nullable deferred tool",
                exposure = "deferred",
                side_effect = "read",
                parameters = {
                    type = "object",
                    properties = {
                        value = { type = { "string", "null" } },
                        keep = { type = "string" },
                    },
                    required = { "value", "keep" },
                },
                execute = function(a)
                    return { content = mare.json.encode_strict(a) }
                end,
            })
            local obj_in = mare.json.decode_lossless('{"value":null,"keep":"x"}')
            local r_obj = T:execute({
                id = "ct_null_obj",
                name = "call_tool",
                arguments = { name = "def_nullable", arguments = obj_in },
            }, {})
            local r_str = T:execute({
                id = "ct_null_str",
                name = "call_tool",
                arguments = { name = "def_nullable", arguments = '{"value":null,"keep":"x"}' },
            }, {})
            assert(not r_obj.is_error and not r_str.is_error,
                "nullable call failed: obj=" .. mare.json.encode(r_obj) .. " str=" .. mare.json.encode(r_str))
            assert(r_obj.content == r_str.content and tostring(r_str.content):find('"value":null', 1, true),
                "string and object arguments must preserve explicit JSON null identically: obj="
                    .. tostring(r_obj.content) .. " str=" .. tostring(r_str.content))
            local r_arr = T:execute({
                id = "ct_null_arr",
                name = "call_tool",
                arguments = { name = "def_nullable", arguments = "[]" },
            }, {})
            assert(r_arr.is_error == true, "JSON array string must be rejected as arguments: " .. mare.json.encode(r_arr))

            -- 3. Explicit opts.sandbox and outer voter sandbox constraints narrow the inner call.
            T:register("def_fs_writer", {
                description = "Deferred file writer",
                exposure = "deferred",
                side_effect = "write",
                parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
                execute = function(a)
                    mare.fs.write(a.path, "written")
                    return { content = "wrote:" .. a.path }
                end,
            })
            local r_direct_ro = T:execute({
                id = "ct_sb_direct",
                name = "def_fs_writer",
                arguments = { path = OUT_PATH },
            }, { sandbox = { level = "read-only" } })
            local r_proxy_ro = T:execute({
                id = "ct_sb_proxy",
                name = "call_tool",
                arguments = { name = "def_fs_writer", arguments = { path = OUT_PATH } },
            }, { sandbox = { level = "read-only" } })
            assert(r_direct_ro.is_error == true, "direct read-only sandbox must block write: " .. mare.json.encode(r_direct_ro))
            assert(r_proxy_ro.is_error == true, "call_tool read-only opts.sandbox must block inner write: " .. mare.json.encode(r_proxy_ro))
            assert(not mare.fs.exists(OUT_PATH), "read-only sandbox must prevent file creation")

            -- Outer tool/before voter constraining call_tool to read-only also narrows inner call.
            local policy = C.cordis:get("policy")
            local voter_active = true
            policy:voter("tool/before", function(c)
                if voter_active and c.name == "call_tool" then
                    return { decision = "allow", sandbox = { level = "read-only" } }
                end
                return nil
            end)
            local r_voter_ro = T:execute({
                id = "ct_sb_voter",
                name = "call_tool",
                arguments = { name = "def_fs_writer", arguments = { path = OUT_PATH } },
            }, {})
            voter_active = false
            assert(r_voter_ro.is_error == true, "outer voter read-only sandbox must narrow inner write: " .. mare.json.encode(r_voter_ro))
            assert(not mare.fs.exists(OUT_PATH), "outer voter read-only sandbox must prevent file creation")

            -- Without narrowing cap, auto preset still allows writing via call_tool.
            local r_write_ok = T:execute({
                id = "ct_sb_ok",
                name = "call_tool",
                arguments = { name = "def_fs_writer", arguments = { path = OUT_PATH } },
            }, {})
            assert(not r_write_ok.is_error and mare.fs.exists(OUT_PATH),
                "unconstrained call_tool under auto preset must succeed: " .. mare.json.encode(r_write_ok))

            -- 4. on_output forwarding + call ID mapping for tools:output_channel() via call_tool.
            local received = {}
            local function capture_output(blocks, meta)
                received[#received + 1] = {
                    text = blocks[1] and blocks[1].text or "",
                    tool_call_id = meta and meta.tool_call_id or nil,
                    inner_tool_call_id = meta and meta.inner_tool_call_id or nil,
                }
                return { status = "accepted" }
            end
            T:register("def_emitter", {
                description = "Deferred tool using output_channel",
                exposure = "deferred",
                side_effect = "read",
                parameters = { type = "object", properties = { tag = { type = "string" } }, required = { "tag" } },
                execute = function(a)
                    local ch, err = T:output_channel()
                    if not ch then
                        return { content = "no-channel:" .. tostring(err), is_error = true }
                    end
                    local sent = T:send_output(ch, { { type = "text", text = "chunk:" .. a.tag } })
                    ch:close()
                    return { content = "sent:" .. tostring(sent.status) .. ":" .. tostring(sent.error or "") }
                end,
            })
            local r_out_direct = T:execute({
                id = "direct_out_1",
                name = "def_emitter",
                arguments = { tag = "direct" },
            }, { on_output = capture_output })
            local r_out_proxy = T:execute({
                id = "proxy_out_1",
                name = "call_tool",
                arguments = { name = "def_emitter", arguments = { tag = "proxy" } },
            }, { on_output = capture_output })
            assert(r_out_direct.content == "sent:accepted:", "direct output_channel failed: " .. mare.json.encode(r_out_direct))
            assert(r_out_proxy.content == "sent:accepted:", "call_tool output_channel failed: " .. mare.json.encode(r_out_proxy))
            assert(#received == 2, "expected 2 output deliveries, got " .. tostring(#received))
            assert(received[1].text == "chunk:direct" and received[1].tool_call_id == "direct_out_1")
            assert(received[2].text == "chunk:proxy" and received[2].tool_call_id == "proxy_out_1",
                "call_tool must map output meta.tool_call_id to outer call ID: " .. mare.json.encode(received[2]))
            assert(received[2].inner_tool_call_id == "proxy_out_1/nested/1",
                "call_tool should preserve inner_tool_call_id: " .. mare.json.encode(received[2]))

            -- 5. Explicit close() and session/changed bulk close dispose the owner effect once.
            local registered_effects, disposed_effects = 0, 0
            local mare_lib = require("mare_lib")
            local orig_owned = mare_lib.owned
            mare_lib.owned = function(owner, fn, label)
                local real_dispose = orig_owned(owner, fn, label)
                if label == "tool output channel" then
                    registered_effects = registered_effects + 1
                    local done = false
                    return function()
                        if not done then
                            done = true
                            disposed_effects = disposed_effects + 1
                        end
                        return real_dispose()
                    end
                end
                return real_dispose
            end
            local held_bulk
            T:register("def_chan_loop", {
                description = "Open/close output channels repeatedly",
                exposure = "deferred",
                side_effect = "read",
                parameters = { type = "object", properties = {} },
                execute = function()
                    for _ = 1, 20 do
                        local ch = assert(T:output_channel())
                        ch:close()
                        ch:close() -- idempotent second close must not double-dispose
                    end
                    held_bulk = assert(T:output_channel())
                    return { content = "ok" }
                end,
            })
            local r_loop = T:execute({
                id = "loop_out_1",
                name = "def_chan_loop",
                arguments = {},
            }, { on_output = capture_output })
            mare_lib.owned = orig_owned
            assert(not r_loop.is_error, "channel loop failed: " .. mare.json.encode(r_loop))
            assert(registered_effects == 21 and disposed_effects == 20,
                "explicit close() must dispose each owner effect once: reg="
                    .. tostring(registered_effects) .. " disp=" .. tostring(disposed_effects))
            C.cordis:get("bus"):emit("session/changed", {})
            assert(disposed_effects == 21,
                "session/changed bulk close must dispose remaining owner effect: disp=" .. tostring(disposed_effects))
            local after_bulk = T:send_output(held_bulk, { { type = "text", text = "late" } })
            assert(after_bulk.status == "rejected" and after_bulk.error == "channel_closed",
                "bulk-closed channel must report channel_closed: " .. mare.json.encode(after_bulk))
            return true
            "#,
        )
        .await;
        assert!(ok);
        h.shutdown().await;
    })
    .await
}
