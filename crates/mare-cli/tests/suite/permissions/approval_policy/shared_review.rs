use super::*;

#[tokio::test]
async fn child_and_net_approvals_consult_registered_rpc_host_even_after_policy_allow() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        exec(
            &h,
            r#"
            HOST_CHILD_CALLED = 0
            HOST_NET_CALLED = 0
            C.cordis:get("policy"):voter("tool/approval", function() return { decision = "allow", source = "policy" } end)
            C.cordis:get("policy"):voter("tool/approval-host", function()
                HOST_CHILD_CALLED = HOST_CHILD_CALLED + 1
                return { decision = "deny", reason = "host denied child", source = "host" }
            end)
            C.cordis:get("policy"):voter("net/approval", function() return { decision = "allow", source = "policy" } end)
            C.cordis:get("policy"):voter("net/approval-host", function()
                HOST_NET_CALLED = HOST_NET_CALLED + 1
                return { decision = "deny", reason = "host denied net", source = "host" }
            end)
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "host-denied.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } } }
            })
            child:wait()
            "#,
        )
        .await;
        assert!(!env.cwd.join("host-denied.txt").exists());
        assert_eq!(eval::<i64>(&h, "return HOST_CHILD_CALLED").await, 1);

        let proxy = mare_binding::netproxy::ProxyHandle::start_with_approver(
            mare_binding::netproxy::NetPolicy::deny_all(),
            None,
            None,
            mare_binding::netproxy::approver(h.lua()),
        )
        .unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s.write_all(b"CONNECT host-denied.invalid:443 HTTP/1.1\r\nHost: host-denied.invalid:443\r\n\r\n").await.unwrap();
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s.read(&mut buf)).await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("403"), "expected 403 from net/approval-host deny");
        assert_eq!(eval::<i64>(&h, "return HOST_NET_CALLED").await, 1);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn shared_allow_with_review_all_consults_registered_rpc_host_and_honors_deny() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only",
                        "rules": [
                            { "tool": "write", "decision": "allow" },
                            { "tool": "edit", "decision": "forbidden" }
                        ]
                    },
                    "approvals": { "review_all": true }
                }
            }))
            .no_session();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        exec(
            &h,
            r#"
            HOST_CHILD_CALLED = 0
            C.cordis:get("policy"):voter("tool/approval-host", function()
                HOST_CHILD_CALLED = HOST_CHILD_CALLED + 1
                return { decision = "deny", reason = "rpc host denied", source = "host" }
            end)
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "shared-allow-host-denied.txt", content = "nope" } }} },
                { tool_calls = {{ name = "edit", arguments = { path = "shared-deny.txt", old_string = "a", new_string = "b" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write and edit",
                no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } } }
            })
            child:wait()
            "#,
        )
        .await;
        assert!(!env.cwd.join("shared-allow-host-denied.txt").exists(), "RPC host denial must not be bypassed by shared allow");
        assert_eq!(eval::<i64>(&h, "return HOST_CHILD_CALLED").await, 1, "shared allow must still consult RPC host, and shared deny must not");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_review_all_with_shared_allow_requires_dialog_when_no_rpc_host() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only",
                        "rules": [{ "tool": "write", "decision": "allow" }]
                    },
                    "approvals": { "review_all": false }
                }
            }))
            .no_session();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        exec(
            &h,
            r#"
            DIALOG_CALLED = 0
            mare.ui.connected = function() return true end
            C.cordis:on("agent/approval-dialog", function()
                DIALOG_CALLED = DIALOG_CALLED + 1
                return { choice = "deny", reason = "dialog denied" }
            end, { prepend = true })
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "child-review-all.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = {
                        approvals = { review_all = true },
                        ["provider-faux"] = { script_json = mare.json.encode(child_script) }
                    }
                }
            })
            child:wait()
            "#,
        )
        .await;
        assert!(!env.cwd.join("child-review-all.txt").exists(), "shared allow must not bypass child review_all");
        assert_eq!(
            eval::<i64>(&h, "return DIALOG_CALLED").await,
            1,
            "child review_all must require explicit dialog answer"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn parent_review_all_with_shared_allow_requires_dialog_when_no_rpc_host() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only",
                        "rules": [{ "tool": "write", "decision": "allow" }]
                    },
                    "approvals": { "review_all": true }
                }
            }))
            .no_session();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        exec(
            &h,
            r#"
            DIALOG_CALLED = 0
            mare.ui.connected = function() return true end
            C.cordis:on("agent/approval-dialog", function()
                DIALOG_CALLED = DIALOG_CALLED + 1
                return { choice = "deny", reason = "dialog denied" }
            end, { prepend = true })
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "parent-review-all.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } }
                }
            })
            child:wait()
            "#,
        )
        .await;
        assert!(!env.cwd.join("parent-review-all.txt").exists(), "shared allow must not bypass parent review_all");
        assert_eq!(
            eval::<i64>(&h, "return DIALOG_CALLED").await,
            1,
            "parent review_all must require explicit dialog answer"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn headless_review_all_with_shared_allow_and_no_rpc_host_denies() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only",
                        "rules": [{ "tool": "write", "decision": "allow" }]
                    },
                    "approvals": { "review_all": false }
                }
            }))
            .no_session();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        let child_decision: String = eval(
            &h,
            r#"
            local decision
            C.cordis:on("approval/resolved", function(ev)
                decision = ev.result and ev.result.decision
            end)
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "headless-child-review.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = {
                        approvals = { review_all = true },
                        ["provider-faux"] = { script_json = mare.json.encode(child_script) }
                    }
                }
            })
            child:wait()
            return decision
            "#,
        )
        .await;
        assert_eq!(child_decision, "unavailable", "headless child review_all with shared allow must deny as unavailable");
        assert!(!env.cwd.join("headless-child-review.txt").exists());
        h.shutdown().await;
        drop(env);

        let env2 = TestEnv::new().await;
        let mut b2 = env2
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only",
                        "rules": [{ "tool": "write", "decision": "allow" }]
                    },
                    "approvals": { "review_all": true }
                }
            }))
            .no_session();
        b2.opts.project_trusted = true;
        let h2 = b2.build().await;
        expose(&h2);
        let parent_decision: String = eval(
            &h2,
            r#"
            local decision
            C.cordis:on("approval/resolved", function(ev)
                decision = ev.result and ev.result.decision
            end)
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "headless-parent-review.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } }
                }
            })
            child:wait()
            return decision
            "#,
        )
        .await;
        assert_eq!(parent_decision, "unavailable", "headless parent review_all with shared allow must deny as unavailable");
        assert!(!env2.cwd.join("headless-parent-review.txt").exists());
        h2.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn mandatory_veto_registered_while_waiting_blocks_main_and_net() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let (main_err, wrote): (bool, bool) = eval(
            &h,
            r#"
            local waiting_tx, waiting_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            local res
            local f = mare.task.spawn(function()
                res = C.cordis:get("tools"):execute({ id = "late-veto-1", name = "write", arguments = { path = "late-veto.txt", content = "nope" } }, {
                    approve = function()
                        waiting_tx:send(true)
                        release_rx:recv()
                        return { decision = "allow", source = "user" }
                    end,
                })
            end)
            waiting_rx:recv()
            C.cordis:get("policy"):voter("tool/approval", function() return { decision = "deny", reason = "late mandatory veto" } end)
            release_tx:send(true)
            f:wait()
            return res.is_error == true, mare.fs.exists("late-veto.txt")
            "#,
        )
        .await;
        assert!(main_err && !wrote, "late mandatory veto must block main tool execution");

        exec(
            &h,
            r#"
            NET_WAIT_TX, NET_WAIT_RX = mare.task.channel()
            NET_REL_TX, NET_REL_RX = mare.task.channel()
            C.cordis:get("policy"):voter("net/approval-host", function()
                NET_WAIT_TX:send(true)
                NET_REL_RX:recv()
                return { decision = "allow", source = "host" }
            end)
            "#,
        )
        .await;
        let proxy = mare_binding::netproxy::ProxyHandle::start_with_approver(
            mare_binding::netproxy::NetPolicy::deny_all(),
            None,
            None,
            mare_binding::netproxy::approver(h.lua()),
        )
        .unwrap();
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port())).await.unwrap();
        s.write_all(b"CONNECT late-veto.invalid:443 HTTP/1.1\r\nHost: late-veto.invalid:443\r\n\r\n").await.unwrap();
        exec(
            &h,
            r#"
            NET_WAIT_RX:recv()
            C.cordis:get("policy"):voter("net/approval", function() return { decision = "deny", reason = "late net veto" } end)
            NET_REL_TX:send(true)
            "#,
        )
        .await;
        let mut buf = [0u8; 1024];
        let n = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), s.read(&mut buf)).await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("403"), "expected 403 after late net/approval veto");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_hidden_values_reject_headless_and_forged_child_user_source() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("child-hidden-ok.lua"),
            r#"
            return { name = "child-hidden-ok", inject = { "policy", "tools" }, apply = function(ctx)
                local lease = mare.projection.register("CHILD_SECRET_77")
                ctx.policy:voter("tool/approval", function()
                    return { decision = "allow", source = "user" }
                end)
                ctx.tools:register({
                    name = "hidden_child_tool",
                    description = "restores hidden value",
                    side_effect = "external",
                    sensitive_inputs = { "token" },
                    parameters = { type = "object", properties = { token = { type = "string" } }, required = { "token" } },
                    execute = function(args)
                        assert(args.token == "CHILD_SECRET_77", "expected restored secret, got " .. tostring(args.token))
                        return { content = "child-restored:" .. lease.reference }
                    end,
                })
            end }
            "#,
        )
        .unwrap();
        let (ok, messages): (bool, String) = eval(
            &h,
            r#"
            local child_script = {
                { tool_calls = {{ name = "hidden_child_tool", arguments = { token = "CHILD_SECRET_77" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "run",
                no_session = true,
                config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } } }
            })
            local res = child:wait()
            local encoded = mare.json.encode(res.messages)
            return encoded:find("child-restored:", 1, true) ~= nil, encoded
            "#,
        )
        .await;
        assert!(!ok, "headless and forged child source=user without parent native human receipt must deny: {messages}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn grandchild_review_all_with_shared_allow_and_no_rpc_host_denies_as_unavailable() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("spawn-grandchild.lua"),
            r#"
            return { name = "spawn-grandchild", apply = function(ctx)
                if mare.mode ~= "child" or mare.config.spawn_grandchild ~= true then return end
                ctx:on("turn/started", function()
                    local grandchild_script = {
                        { tool_calls = {{ name = "write", arguments = { path = "grandchild-review.txt", content = "nope" } }} },
                        { text = "grandchild done" }
                    }
                    local grandchild = mare.subagent.start({
                        prompt = "write",
                        no_session = true,
                        config = {
                            model = "faux/echo",
                            plugins = {
                                approvals = { review_all = true },
                                ["provider-faux"] = { script_json = mare.json.encode(grandchild_script) }
                            }
                        }
                    })
                    grandchild:wait()
                end)
            end }
            "#,
        )
        .unwrap();
        let mut b = env
            .host()
            .config(json!({
                "agents": { "max_active": 4, "max_depth": 2 },
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only",
                        "rules": [{ "tool": "write", "decision": "allow" }]
                    },
                    "approvals": { "review_all": false }
                }
            }))
            .no_session();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        let decision: String = eval(
            &h,
            r#"
            local decision
            C.cordis:on("approval/resolved", function(ev)
                decision = ev.result and ev.result.decision
            end)
            local child_script = {
                { text = "child done" }
            }
            local child = mare.subagent.start({
                prompt = "delegate",
                no_session = true,
                config = {
                    model = "faux/echo",
                    spawn_grandchild = true,
                    plugins = {
                        approvals = { review_all = false },
                        ["provider-faux"] = { script_json = mare.json.encode(child_script) }
                    }
                }
            })
            child:wait()
            return decision
            "#,
        )
        .await;
        assert_eq!(
            decision, "unavailable",
            "headless grandchild review_all with shared allow on root must deny as unavailable"
        );
        assert!(!env.cwd.join("grandchild-review.txt").exists());
        h.shutdown().await;
    })
    .await
}
