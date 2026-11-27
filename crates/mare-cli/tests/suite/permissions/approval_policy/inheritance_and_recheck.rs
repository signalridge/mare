use super::*;

#[tokio::test]
async fn child_sensitive_rule_is_not_saved_after_path_identity_changes() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "old secret\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        exec(&h, r#"
            ENTER_TX, ENTER_RX = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            local first = true
            C.cordis:get("policy"):voter("tool/approval-host", function(_, _, verdict)
                if first then
                    first = false
                    ENTER_TX:send(true)
                    RELEASE_RX:recv()
                    return { decision = "allow", grant = "allow_amend", amendment = verdict.amendment, source = "user" }
                end
                return { decision = "deny", source = "user" }
            end)
            local script = {
                { tool_calls = {{ name = "read", arguments = { path = ".env" } }} },
                { text = "done" },
            }
            CHILD = mare.subagent.start({ prompt = "read", no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(script) } } } })
            ENTER_RX:recv()
        "#).await;
        std::fs::rename(env.cwd.join(".env"), env.cwd.join("old.env")).unwrap();
        std::fs::write(env.cwd.join(".env"), "replacement secret\n").unwrap();
        exec(&h, r#"RELEASE_TX:send(true); pcall(function() CHILD:wait() end)"#).await;
        h.shutdown().await;
        let file = env.home.path().join("state/permissions.json");
        if file.exists() {
            let value: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            assert!(value["rules"].as_array().unwrap().is_empty(), "changed child access must not persist a rule: {value}");
        }
    })
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn child_sensitive_rule_is_not_saved_if_path_changes_during_result_event() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "old secret\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        exec(&h, r#"
            EVENT_ENTER_TX, EVENT_ENTER_RX = mare.task.channel()
            EVENT_RELEASE_TX, EVENT_RELEASE_RX = mare.task.channel()
            local first_event, first_vote = true, true
            C.cordis:on("pipeline/approval-decided", function(ev)
                if ev.origin and first_event then
                    first_event = false
                    EVENT_ENTER_TX:send(true)
                    EVENT_RELEASE_RX:recv()
                end
            end, { prepend = true })
            C.cordis:get("policy"):voter("tool/approval-host", function(_, _, verdict)
                if first_vote then
                    first_vote = false
                    return { decision = "allow", grant = "allow_amend", amendment = verdict.amendment, source = "user" }
                end
                return { decision = "deny", source = "user" }
            end)
            local script = {
                { tool_calls = {{ name = "read", arguments = { path = ".env" } }} },
                { text = "done" },
            }
            EVENT_CHILD = mare.subagent.start({ prompt = "read", no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(script) } } } })
            EVENT_ENTER_RX:recv()
        "#).await;
        std::fs::rename(env.cwd.join(".env"), env.cwd.join("old.env")).unwrap();
        std::fs::write(env.cwd.join(".env"), "replacement secret\n").unwrap();
        exec(&h, r#"EVENT_RELEASE_TX:send(true); pcall(function() EVENT_CHILD:wait() end)"#).await;
        h.shutdown().await;
        let file = env.home.path().join("state/permissions.json");
        if file.exists() {
            let value: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            assert!(value["rules"].as_array().unwrap().is_empty(), "changed child access must not persist a rule: {value}");
        }
    }).await
}

#[tokio::test]
async fn child_sensitive_rule_is_not_saved_if_policy_changes_during_result_event() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.cwd.join(".env"), "secret\n").unwrap();
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        exec(&h, r#"
            local changed = false
            C.cordis:on("pipeline/approval-decided", function(ev)
                if ev.origin and not changed then
                    changed = true
                    C.cordis:get("permissions"):add_rules({ { tool = "read", decision = "forbidden" } })
                end
            end, { prepend = true })
            local first = true
            C.cordis:get("policy"):voter("tool/approval-host", function(_, _, verdict)
                if first then
                    first = false
                    return { decision = "allow", grant = "allow_amend", amendment = verdict.amendment, source = "user" }
                end
                return { decision = "deny", source = "user" }
            end)
            local script = {
                { tool_calls = {{ name = "read", arguments = { path = ".env" } }} },
                { text = "done" },
            }
            local child = mare.subagent.start({ prompt = "read", no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(script) } } } })
            pcall(function() child:wait() end)
            assert(changed, "the policy must change during the result event")
        "#).await;
        h.shutdown().await;
        let file = env.home.path().join("state/permissions.json");
        if file.exists() {
            let value: Value = serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
            assert!(value["rules"].as_array().unwrap().is_empty(), "changed policy must not persist a rule: {value}");
        }
    }).await
}

#[tokio::test]
async fn higher_priority_policy_listener_cannot_mutate_arguments_for_later_deny() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}}))
            .build()
            .await;
        expose(&h);
        exec(
            &h,
            r#"
            -- both run ahead of the built-in voters, the mutating one first: registered last
            -- with `prepend`, it goes in front of the deciding one
            C.cordis:get("policy"):voter("tool/approval", function(call)
                if call.arguments.path == "mutated-secret.txt" then
                    return { decision = "deny", reason = "blocked original path" }
                end
                return { decision = "allow" }
            end, { prepend = true })
            C.cordis:get("policy"):voter("tool/approval", function(call)
                call.arguments.path = "benign.txt"
                return nil
            end, { prepend = true })
            local r = C.cordis:get("tools"):execute({
                id = "mutate-1",
                name = "write",
                arguments = { path = "mutated-secret.txt", content = "nope" }
            })
            assert(r.is_error == true, mare.json.encode(r))
        "#,
        )
        .await;
        assert!(!env.cwd.join("mutated-secret.txt").exists());
        assert!(!env.cwd.join("benign.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_local_allow_and_custom_approve_cannot_bypass_parent_live_policy() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}})).build().await;
        expose(&h);
        exec(&h, r#"
            C.cordis:get("policy"):voter("tool/approval", function()
                return { decision = "deny", reason = "parent live deny" }
            end)
            C.cordis:get("tools"):register("child_bypass_probe", {
                description = "child probe",
                side_effect = "read",
                parameters = { type = "object" },
                execute = function()
                    local turns = {{ text = "done" }}
                    local child = mare.subagent.start({
                        prompt = "probe",
                        no_session = true,
                        permission = "read-only",
                        config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(turns) } } }
                    })
                    local res = child:wait()
                    return mare.json.encode(res)
                end
            })
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "child-bypass.txt", content = "bad" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                permission = "read-only",
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } } }
            })
            child:wait()
        "#).await;
        assert!(!env.cwd.join("child-bypass.txt").exists());
        // Also test child-local tool/approval allow + custom opts.approve directly inside a child
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("child-local-allow.lua"), r#"
            return { name = "child-local-allow", inject = { "policy", "tools" }, apply = function(ctx)
                ctx.policy:voter("tool/approval", function()
                    return { decision = "allow", source = "policy" }
                end)
                ctx:on("turn/started", function()
                    local tools = ctx.tools
                    local custom_called = false
                    local ok, res = pcall(function()
                        return tools:execute({
                            id = "child-custom-1",
                            name = "write",
                            arguments = { path = "child-local-allow.txt", content = "bad" },
                        }, {
                            approve = function()
                                custom_called = true
                                return { decision = "allow", source = "user" }
                            end,
                        })
                    end)
                    mare.fs.write("child-local-allow-probe.json", mare.json.encode({
                        ok = ok,
                        custom_called = custom_called,
                        is_error = ok and res and res.is_error or false,
                        err = not ok and tostring(res) or nil,
                    }))
                end)
            end }
        "#).unwrap();
        exec(&h, r#"
            local child_turns = {
                { text = "ok" }
            }
            local child = mare.subagent.start({
                prompt = "ok",
                no_session = true,
                permission = "read-only",
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_turns) } } }
            })
            child:wait()
        "#).await;
        let probe: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(env.cwd.join("child-local-allow-probe.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(probe["ok"], true, "child turn/started probe failed: {probe}");
        assert_eq!(
            probe["custom_called"], false,
            "child tool/approval-host must preempt child custom opts.approve: {probe}"
        );
        assert_eq!(
            probe["is_error"], true,
            "child custom opts.approve must not bypass parent read-only ceiling: {probe}"
        );
        assert!(!env.cwd.join("child-local-allow.txt").exists());
        assert!(!env.cwd.join("child-local-allow.txt").exists());
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn newly_registered_deny_during_reviewer_wait_blocks_execution() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"},"approvals":{"reviewer":"fixture"}}})).build().await;
        expose(&h);
        exec(&h, r#"
            local entered_tx, entered_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            C.cordis:get("approvals"):register_reviewer("fixture", function()
                entered_tx:send(true)
                release_rx:recv()
                return { decision = "allow" }
            end)
            mare.task.spawn(function()
                entered_rx:recv()
                C.cordis:get("policy"):voter("tool/approval", function()
                    return { decision = "deny", reason = "late registered deny" }
                end)
                release_tx:send(true)
            end)
            local r = C.cordis:get("tools"):execute({
                id = "race-1",
                name = "write",
                arguments = { path = "race-denied.txt", content = "nope" }
            })
            assert(r.is_error == true, mare.json.encode(r))
        "#).await;
        assert!(!env.cwd.join("race-denied.txt").exists());
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn parent_rule_check_error_fails_closed_on_child_effect() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}})).build().await;
        expose(&h);
        exec(&h, r#"
            C.cordis:get("policy"):voter("tool/approval", function() return { decision = "allow", source = "policy" } end)
            local perms = C.cordis:get("permissions")
            local rule_called = false
            perms.rule_verdict = function()
                rule_called = true
                error("simulated rule check failure")
            end
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "rule-error-child.txt", content = "bad" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } } }
            })
            child:wait()
            assert(rule_called, "expected parent rule_verdict to be invoked")
        "#).await;
        assert!(!env.cwd.join("rule-error-child.txt").exists());
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn child_cancellation_during_rule_recheck_never_persists_amendment() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}})).build().await;
        expose(&h);
        // the parent's re-check calls rule_verdict on the published permissions table
        // (mare-host approvals/shared_rule.rs evaluate): the pause goes on that table, where the
        // host's call reads it — a proxy reached through a ctx is a different object on the tree
        global(&h, "PERMS", h.service_table("permissions").unwrap());
        exec(&h, r#"
            local recheck_entered_tx, recheck_entered_rx = mare.task.channel()
            local recheck_release_tx, recheck_release_rx = mare.task.channel()
            local public_tx, public_rx = mare.task.channel()
            local answered, paused = false, false
            RESULTS = {}
            local perms = PERMS
            local orig_verdict = perms.rule_verdict
            perms.rule_verdict = function(self, caller, call, info)
                -- the first rule check after the answer is the post-answer re-check
                if answered and not paused then
                    paused = true
                    recheck_entered_tx:send(true)
                    recheck_release_rx:recv()
                end
                return orig_verdict(self, caller, call, info)
            end
            C.cordis:on("pipeline/approval-decided", function(ev)
                if ev.origin then
                    RESULTS[#RESULTS + 1] = { id = ev.child_approval_id, decision = ev.decision, source = ev.source }
                end
            end)
            -- the public result is the last thing the parent publishes for the approval
            C.cordis:on("approval/resolved", function() public_tx:send(true) end)
            C.cordis:get("policy"):voter("tool/approval-host", function(call, info, verdict)
                answered = true
                return {
                    decision = "allow",
                    grant = "allow_amend",
                    amendment = verdict and verdict.amendment or nil,
                    source = "user",
                    human_approval = mare.crypto.random_hex(32),
                }
            end)
            local child_script = {
                { tool_calls = {{ name = "bash", arguments = { command = "touch recheck-cancel-child.txt", sandbox_permissions = "write:" .. mare.cwd, justification = "write file" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "run",
                no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } } }
            })
            recheck_entered_rx:recv()
            child:cancel()
            -- the child is gone before the recheck resumes: cancellation wins deterministically
            local ok, err = pcall(function() return child:wait() end)
            recheck_release_tx:send(true)
            assert(not ok and tostring(err):find("cancelled", 1, true), "expected cancelled child, got: " .. tostring(err))
            -- hang guard only: wait until the parent has published its final result
            assert(mare.task.timeout(15000, function() public_rx:recv() end), "no public approval/resolved")
        "#).await;
        let results: String = eval(&h, "return mare.json.encode(RESULTS)").await;
        let results: Vec<serde_json::Value> = serde_json::from_str(&results).unwrap();
        assert_eq!(results.len(), 1, "exactly one terminal result for the approval: {results:?}");
        assert!(results[0]["id"].is_number() || results[0]["id"].is_string(), "{results:?}");
        assert_eq!((results[0]["decision"].as_str(), results[0]["source"].as_str()), (Some("deny"), Some("cancelled")), "{results:?}");
        assert!(!env.cwd.join("recheck-cancel-child.txt").exists());
        let perms_path = env.home.path().join("state/permissions.json");
        if perms_path.exists() {
            let json: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&perms_path).unwrap()).unwrap();
            let rules = json.get("rules").and_then(|r| r.as_array()).expect("rules must be an array");
            assert!(rules.is_empty(), "cancelled child must not persist remembered rule: {rules:?}");
        }
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn child_recheck_catches_new_deny_listener_registered_during_policy_yield() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .no_session()
            .config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}}))
            .build()
            .await;
        expose(&h);
        exec(&h, r#"
            local recheck_entered_tx, recheck_entered_rx = mare.task.channel()
            local recheck_release_tx, recheck_release_rx = mare.task.channel()
            local policy_calls = 0
            local results = {}
            C.cordis:on("pipeline/approval-decided", function(ev)
                if ev and ev.origin then
                    results[#results + 1] = ev
                end
            end)
            C.cordis:get("policy"):voter("tool/approval", function()
                policy_calls = policy_calls + 1
                if policy_calls == 3 then
                    recheck_entered_tx:send(true)
                    recheck_release_rx:recv()
                end
                return { decision = "ask" }
            end)
            C.cordis:get("policy"):voter("tool/approval-host", function()
                return {
                    decision = "allow",
                    grant = "allow",
                    source = "user",
                    human_approval = mare.crypto.random_hex(32),
                }
            end)
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "epoch-race-child.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "run",
                no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } } }
            })
            recheck_entered_rx:recv()
            C.cordis:get("policy"):voter("tool/approval", function()
                return { decision = "deny", reason = "late parent deny listener" }
            end)
            recheck_release_tx:send(true)
            child:wait()
            assert(policy_calls >= 4, "expected epoch-change retry after outer recheck, got policy_calls=" .. policy_calls)
            assert(#results == 1, "expected 1 child approval result, got " .. #results)
            assert(results[1].decision == "deny", "expected deny, got " .. tostring(results[1].decision))
            assert(
                results[1].reason == "late parent deny listener",
                "expected late parent deny listener reason, got " .. tostring(results[1].reason)
            )
        "#).await;
        assert!(!env.cwd.join("epoch-race-child.txt").exists(), "late deny listener must block child write");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn child_policy_ask_and_abstain_preserve_tristate_and_reach_host_allow_and_deny() {
    for (policy_vote, host_decision, expect_exists) in [
        ("{ decision = 'ask' }", "allow", true),
        ("{ decision = 'ask' }", "deny", false),
        ("nil", "allow", true),
        ("nil", "deny", false),
    ] {
        local(async {
            let env = TestEnv::new().await;
            let h = env
                .host()
                .no_session()
                .config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}}))
                .build()
                .await;
            expose(&h);
            let script = format!(
                r#"
                local host_calls = 0
                C.cordis:get("policy"):voter("tool/approval", function()
                    return {policy_vote}
                end)
                C.cordis:get("policy"):voter("tool/approval-host", function()
                    host_calls = host_calls + 1
                    if "{host_decision}" == "allow" then
                        return {{
                            decision = "allow",
                            grant = "allow",
                            source = "user",
                            human_approval = mare.crypto.random_hex(32),
                        }}
                    end
                    return {{ decision = "deny", reason = "host rejected" }}
                end)
                local child_script = {{
                    {{ tool_calls = {{{{ name = "write", arguments = {{ path = "tristate-child.txt", content = "ok" }} }}}} }},
                    {{ text = "done" }}
                }}
                local child = mare.subagent.start({{
                    prompt = "run",
                    no_session = true,
                    config = {{ model = "faux/echo", plugins = {{ ["provider-faux"] = {{ script_json = mare.json.encode(child_script) }} }} }}
                }})
                child:wait()
                assert(host_calls == 1, "expected tool/approval-host to be called once, got " .. host_calls)
                "#
            );
            exec(&h, &script).await;
            assert_eq!(
                env.cwd.join("tristate-child.txt").exists(),
                expect_exists,
                "policy={policy_vote} host={host_decision}"
            );
            h.shutdown().await;
        })
        .await;
    }
}

#[tokio::test]
async fn network_session_cache_respects_later_net_approval_deny() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            NET_CALLS = 0
            NET_MODE = "allow_session"
            NET_RESULTS = {}
            C.cordis:get("policy"):voter("net/approval", function()
                NET_CALLS = NET_CALLS + 1
                if NET_MODE == "allow_session" then
                    return { decision = "allow", grant = "allow_session" }
                elseif NET_MODE == "sandbox_deny" then
                    return { decision = "allow", sandbox_constraints = {{ network = false }} }
                end
                return { decision = "deny", reason = "later net deny" }
            end)
            C.cordis:on("approval/resolved", function(ev)
                if ev.category == "network" then
                    NET_RESULTS[#NET_RESULTS + 1] = ev.result.decision
                end
            end)
        "#,
        )
        .await;
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target_listener.local_addr().unwrap().port();
        let target_task = tokio::task::spawn_local(async move {
            let _ = target_listener.accept().await;
        });
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "cached-then-denied.invalid".to_string(),
            vec!["127.0.0.1".parse().unwrap()],
        )])));
        let policy_lua = mlua::Lua::new();
        let policy_tbl: mlua::Table = policy_lua.load("return { allow_local = true }").eval().unwrap();
        let policy = mare_binding::netauth::NetPolicy::from_lua(&policy_tbl).unwrap();
        let proxy = mare_binding::netproxy::ProxyHandle::start_with_approver(
            policy,
            None,
            None,
            mare_binding::netproxy::approver(h.lua()),
        )
        .unwrap();
        let connect_req = format!(
            "CONNECT cached-then-denied.invalid:{target_port} HTTP/1.1\r\nHost: cached-then-denied.invalid:{target_port}\r\n\r\n"
        );
        let mut s1 = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s1.write_all(connect_req.as_bytes()).await.unwrap();
        let mut buf = [0u8; 1024];
        let n1 =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s1.read(&mut buf)).await.unwrap().unwrap();
        assert!(n1 > 0, "expected proxy response on first request");
        assert!(
            !String::from_utf8_lossy(&buf[..n1]).contains("403"),
            "first request must be approved, got {}",
            String::from_utf8_lossy(&buf[..n1])
        );
        let (calls1, first_decision): (i64, String) = eval(&h, "return NET_CALLS, NET_RESULTS[1] or ''").await;
        assert_eq!(calls1, 2, "expected pre-check and post-check net/approval calls on first request");
        assert_eq!(first_decision, "allow_session");
        // Same listener (unchanged policy epoch) switches to allow + network=false sandbox constraint.
        exec(&h, r#"NET_MODE = "sandbox_deny""#).await;
        let mut s2 = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s2.write_all(connect_req.as_bytes()).await.unwrap();
        let n2 =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s2.read(&mut buf)).await.unwrap().unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n2]).contains("403"),
            "expected 403 after sandbox_constraints network=false, got {}",
            String::from_utf8_lossy(&buf[..n2])
        );
        // Same listener switches to explicit deny on cache hit.
        exec(&h, r#"NET_MODE = "deny""#).await;
        let mut s3 = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s3.write_all(connect_req.as_bytes()).await.unwrap();
        let n3 =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s3.read(&mut buf)).await.unwrap().unwrap();
        assert!(
            String::from_utf8_lossy(&buf[..n3]).contains("403"),
            "expected 403 after later deny, got {}",
            String::from_utf8_lossy(&buf[..n3])
        );
        mare_binding::netauth::set_resolver_override(None);
        target_task.abort();
        h.shutdown().await;
    })
    .await
}
