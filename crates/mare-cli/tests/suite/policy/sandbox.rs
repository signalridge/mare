use super::echo_host;
use crate::common::*;
use serde_json::json;

#[tokio::test]
async fn any_caller_of_tools_execute_is_voted_and_sandboxed() {
    local(async {
        let env = TestEnv::new().await;
        let outside = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let out = dunce::canonicalize(outside.path()).unwrap();
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "OUT", out.to_string_lossy().to_string());
        // a plugin that replaced the agent loop calls the tools service directly
        let (blocked, wrote): (String, String) = eval(
            &h,
            r#"mare.proc.sandbox.set_current({ level = "workspace-write", exclude_tmpdir_env_var = true, exclude_slash_tmp = true })
               T:register("w", { description = "x", parameters = { type = "object", properties = {} },
                 execute = function() local ok, e = pcall(mare.fs.write, OUT .. "/x", "1") return tostring(ok) .. ":" .. tostring(e) end })
               local off = C.cordis:get("policy"):voter("tool/before", function(call) if call.name == "w" then return { decision = "deny", reason = "org" } end end)
               local a = T:execute({ id = "1", name = "w", arguments = {} }, {})
               off()
               -- a caller-supplied sandbox can only narrow the voted one, never switch it off
               local b = T:execute({ id = "2", name = "w", arguments = {} }, { sandbox = "danger-full-access" })
               mare.proc.sandbox.set_current(nil)
               return a.content, b.content"#,
        )
        .await;
        assert!(blocked.contains("Tool call blocked: org"), "{blocked}");
        assert!(wrote.starts_with("false"), "{wrote}");
        assert!(!out.join("x").exists());
        h.shutdown().await;
    })
    .await
}

/// A later policy vote must not replace an earlier listener's stricter sandbox.
#[tokio::test]
async fn sandbox_votes_cannot_widen_each_other() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (forward, reverse): (String, String) = eval(
            &h,
            r#"
            T:register("write_probe", { description = "test", parameters = { type = "object", properties = {} },
              execute = function() local ok = pcall(mare.fs.write, CWD .. "/probe", "x") return tostring(ok) end })
            local function run(first, second, id)
              local a = C.cordis:get("policy"):voter("tool/before", function() return { decision = "allow", sandbox = first } end)
              local b = C.cordis:get("policy"):voter("tool/before", function() return { decision = "allow", sandbox = second } end)
              local result = T:execute({ id = id, name = "write_probe", arguments = {} }, {})
              a(); b(); return result.content
            end
            return run("read-only", "danger-full-access", "one"),
                   run("danger-full-access", "read-only", "two")"#,
        )
        .await;
        assert_eq!(forward, "false");
        assert_eq!(reverse, "false");
        assert!(!env.cwd.join("probe").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn explicit_grants_never_override_restrictions_and_need_approval() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let (approved, constrained, denied, conflict, approval_vote): (String, String, String, String, String) = eval(&h, r#"
            mare.proc.sandbox.set_current("read-only")
            T:register("grant_probe", { description = "test", parameters = { type = "object", properties = {} },
              execute = function() local ok = pcall(mare.fs.write, CWD .. "/grant", "x") return tostring(ok) end })
            local n = 0
            local function run(opts)
              n = n + 1
              local r = T:execute({ id = tostring(n), name = "grant_probe", arguments = {} }, opts or {})
              return tostring(r.content)
            end
            local grant = C.cordis:get("policy"):voter("tool/before", function() return { decision = "ask", sandbox_grant = "danger-full-access" } end)
            local approved = run({ approve = function() return { decision = "allow" } end })
            local restriction = C.cordis:get("policy"):voter("tool/before", function() return { decision = "allow", sandbox = "read-only" } end)
            local constrained = run({ approve = function() return { decision = "allow" } end })
            local denied = run({ approve = function() return { decision = "deny" } end })
            restriction()
            local second = C.cordis:get("policy"):voter("tool/before", function() return { decision = "allow", sandbox_grant = "danger-full-access" } end)
            local conflict = run({ approve = function() return { decision = "allow" } end })
            second()
            local approval_restriction = C.cordis:get("policy"):voter("tool/approval", function() return { decision = "allow", sandbox = "read-only" } end)
            local approval_vote = run()
            approval_restriction(); grant(); mare.proc.sandbox.set_current(nil)
            return approved, constrained, denied, conflict, approval_vote"#).await;
        assert_eq!(approved, "true", "an approved explicit grant still works");
        assert_eq!(constrained, "false", "another vote's restriction survives the grant");
        assert!(denied.contains("Tool call denied"), "{denied}");
        assert!(conflict.contains("conflicting policy sandbox grants"), "{conflict}");
        assert_eq!(approval_vote, "false", "approval-stage restrictions reach the tool");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn a_later_listener_cannot_mutate_an_earlier_sandbox_vote() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let result: String = eval(
            &h,
            r#"
            local earlier = { level = "read-only" }
            C.cordis:get("policy"):voter("tool/before", function() return { decision = "allow", sandbox = earlier } end)
            C.cordis:get("policy"):voter("tool/before", function() earlier.level = "danger-full-access" end)
            T:register("mutation_probe", { description = "test", parameters = { type = "object", properties = {} },
              execute = function() return tostring(pcall(mare.fs.write, CWD .. "/mutated", "x")) end })
            return T:execute({ id = "m", name = "mutation_probe", arguments = {} }, {}).content"#,
        )
        .await;
        assert_eq!(result, "false");
        assert!(!env.cwd.join("mutated").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn nested_permission_verdict_constraints_reach_the_tool() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .user_config(true)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "full-access" } } }))
            .no_session()
            .build()
            .await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let result: String = eval(
            &h,
            r#"
            C.cordis:get("policy"):voter("permission/verdict", function() return { decision = "allow", sandbox = "read-only" } end)
            T:register("nested_probe", { description = "test", parameters = { type = "object", properties = {} },
              execute = function() return tostring(pcall(mare.fs.write, CWD .. "/nested", "x")) end })
            return T:execute({ id = "nested", name = "nested_probe", arguments = {} }, {}).content"#,
        )
        .await;
        assert_eq!(result, "false");
        assert!(!env.cwd.join("nested").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn approval_callback_cannot_mutate_the_proposed_sandbox_grant() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());
        let result: String = eval(&h, r#"
            C.cordis:get("policy"):voter("tool/before", function() return { decision = "ask", sandbox_grant = { level = "read-only" } } end)
            T:register("approval_mutation", { description = "test", parameters = { type = "object", properties = {} },
              execute = function() return tostring(pcall(mare.fs.write, CWD .. "/approval-mutation", "x")) end })
            local r = T:execute({ id = "approval-mutation", name = "approval_mutation", arguments = {} }, {
              approve = function(_, _, verdict) verdict.sandbox_grant.level = "danger-full-access"; return { decision = "allow" } end,
            })
            return r.content"#).await;
        assert_eq!(result, "false");
        assert!(!env.cwd.join("approval-mutation").exists());
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn malformed_sandbox_constraint_lists_fail_closed() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (decision, reason): (String, String) = eval(
            &h,
            r#"
            C.cordis:get("policy"):voter("tool/before", function() return { decision = "allow",
              sandbox_constraints = { [1] = "read-only", [3] = "danger-full-access" } } end)
            local v = C.cordis:get("policy"):decide("tool/before", { {} })
            return v.decision, tostring(v.reason)"#,
        )
        .await;
        assert_eq!(decision, "deny");
        assert!(reason.contains("sandbox_constraints"), "{reason}");
        h.shutdown().await;
    })
    .await
}

/// A persistent shell started under a grant must not be reused by a later restricted call.
#[cfg(unix)]
#[tokio::test]
async fn persistent_shell_is_not_reused_across_different_call_sandboxes() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        let (first, second, third): (String, String, String) = eval(&h, r#"
            C.cordis:get("policy"):voter("tool/before", function(call)
              if call.name ~= "bash" then return nil end
              if call.id:match("^wide") then return { decision = "allow", sandbox_grant = "danger-full-access" } end
              return { decision = "allow", sandbox = "read-only" }
            end)
            local a = T:execute({ id = "wide", name = "bash", arguments = { command = "printf first" } }, {})
            local b = T:execute({ id = "narrow", name = "bash", arguments = { command = "printf escaped > restricted.txt" } }, {})
            local c = T:execute({ id = "wide-again", name = "bash", arguments = { command = "printf allowed > allowed.txt" } }, {})
            return a.content, b.content, c.content"#).await;
        assert!(first.contains("first"), "{first}");
        assert!(!env.cwd.join("restricted.txt").exists(), "a wider persistent shell was reused: {second}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("allowed.txt")).unwrap(), "allowed", "{third}");
        h.shutdown().await;
    }).await
}

/// A coded sandbox refusal (here SANDBOX_INVALID_ROOT: a spawn's extra writable directory is
/// missing) reaches the model and the UI as a short tool error — code, reason, fix — not as a
/// "runtime error:" with a Lua traceback; an unexpected error keeps its text.
#[tokio::test]
async fn coded_sandbox_refusals_are_short_tool_errors() {
    local(async {
        let env = TestEnv::new().await;
        let gone = env.cwd.join("no-such-dir");
        let h = env.host().no_session().build().await;
        expose(&h);
        global(&h, "GONE", gone.to_string_lossy().to_string());
        let (content, code, plain): (String, String, String) = eval(
            &h,
            r#"T:register("w", { description = "x", parameters = { type = "object", properties = {} },
                 execute = function()
                   mare.proc.run({ cmd = "git", args = { "--version" }, sandbox = { level = "workspace-write", writable = { GONE }, network = false } })
                   return "ran"
                 end })
               T:register("boom", { description = "x", parameters = { type = "object", properties = {} },
                 execute = function() error("kaput") end })
               local r = T:execute({ id = "1", name = "w", arguments = {} }, {})
               local b = T:execute({ id = "2", name = "boom", arguments = {} }, {})
               return r.content, tostring(r.error_code), b.content"#,
        )
        .await;
        assert!(content.starts_with("SANDBOX_INVALID_ROOT: invalid additional writable directory"), "{content}");
        assert!(content.contains("cannot read the directory"), "the reason stays: {content}");
        assert!(!content.contains('\n') && !content.contains("runtime error") && !content.contains("traceback"), "{content}");
        assert_eq!(code, "sandbox_invalid_root");
        assert!(plain.contains("kaput"), "{plain}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn unix_sockets_outside_the_allowed_list_are_refused_inside_tool_calls() {
    local(async {
        let (_env, h) = echo_host().await;
        let out: String = eval(
            &h,
            r#"mare.proc.sandbox.set_current({ level = "workspace-write", network = false })
               T:register("sock", { description = "x", parameters = { type = "object", properties = {} },
                 execute = function()
                   local conn, err = mare.net.unix.connect("/tmp/mare-r3a-nonexistent.sock")
                   return tostring(conn) .. ":" .. tostring(err)
                 end })
               local r = T:execute({ id = "1", name = "sock", arguments = {} }, {})
               mare.proc.sandbox.set_current(nil)
               return r.content"#,
        )
        .await;
        assert!(out.starts_with("nil:") && out.contains("unix:"), "{out}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn tool_exec_ctx_rejects_sandbox_set_current_raw_profile_missing_call_id_and_store_host_ops() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);

        let (set_err, profile_err, store_err, cancel_wins): (String, String, String, bool) = eval(
            &h,
            r#"
            -- set_current inside a tool call must error.
            T:register("try_set_current", {
                description = "test",
                parameters = { type = "object", properties = {} },
                execute = function()
                    local ok, err = pcall(mare.proc.sandbox.set_current, { level = "workspace-write" })
                    return ok and "ok" or tostring(err)
                end,
            })
            local r1 = T:execute({ id = "c-set", name = "try_set_current", arguments = {} }, {})

            -- Nested exec_ctx with a raw Seatbelt profile must be rejected.
            T:register("try_raw_profile", {
                description = "test",
                parameters = { type = "object", properties = {} },
                execute = function()
                    local ok, err = pcall(mare.exec.run, {
                        sandbox = { profile = "(version 1)(allow default)" },
                    }, function() return "unexpected" end)
                    return ok and "ok" or tostring(err)
                end,
            })
            local r2 = T:execute({ id = "c-prof", name = "try_raw_profile", arguments = {} }, {})

            -- Store host operations are unavailable inside a tool execution context.
            T:register("try_store_host", {
                description = "test",
                parameters = { type = "object", properties = {} },
                execute = function()
                    local ok, err = pcall(mare.store.open, mare.paths.state .. "/sessions/blocked.jsonl")
                    return ok and "ok" or tostring(err)
                end,
            })
            local r4 = T:execute({ id = "c-store", name = "try_store_host", arguments = {} }, {})

            -- Cancelled takes precedence over timed_out.
            local tok = mare.task.token()
            tok:cancel()
            local pr = mare.proc.run({ cmd = "sh", args = { "-c", "sleep 2" }, timeout_ms = 50, cancel = tok })
            local cancel_beats_timeout = pr.cancelled == true and pr.timed_out ~= true

            return tostring(r1.content), tostring(r2.content), tostring(r4.content), cancel_beats_timeout
            "#,
        )
        .await;
        assert!(set_err.contains("cannot change session policy inside a tool call"), "{set_err}");
        assert!(profile_err.contains("raw profile is not allowed"), "{profile_err}");
        assert!(store_err.contains("unavailable inside a tool call"), "{store_err}");
        assert!(cancel_wins, "cancelled must take precedence over timed_out");
        h.shutdown().await;
        drop(env);

        // While a session journal is active, mare.exec.run without tool_call_id is refused.
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        h.prompt("open session", json!({})).await.unwrap();
        let call_id_err: String = eval(
            &h,
            r#"local ok, err = pcall(mare.exec.run, { cwd = mare.cwd }, function() return "unexpected" end)
               return ok and "ok" or tostring(err)"#,
        )
        .await;
        assert!(call_id_err.contains("EXEC_NOT_DURABLE") && call_id_err.contains("tool_call_id"), "{call_id_err}");
        h.shutdown().await;
    })
    .await
}
