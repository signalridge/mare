use super::*;

#[tokio::test]
async fn web_fetch_model_tool_enforces_readonly_approvals_ssrf_and_redirects() {
    local(async {
        let server = MockServer::start().await;
        let port = host_port(&server.base);
        server.route("/plain", vec![resp_with(200, "text/plain; charset=utf-8", "loopback-ok")]);
        server.route("/steal", vec![resp_with(200, "text/plain; charset=utf-8", "stolen")]);
        server.route(
            "/to-evil",
            vec![
                redirect_resp(&format!("http://evil.test:{port}/steal")),
                redirect_resp(&format!("http://evil.test:{port}/steal")),
            ],
        );
        server.route("/to-fakeip", vec![redirect_resp("http://198.18.0.5/steal")]);

        // 1. read-only preset with approval_policy = "never" blocks web_fetch to local/private IP.
        {
            let env = TestEnv::new().await;
            let script = json!([
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/plain", server.base) } }] },
                { "text": "done" }
            ]);
            let h = env
                .host().builtins(&["web", "web-fetch-http"])
                .faux(script)
                .args(json!({ "no_session": true }))
                .config(json!({
                    "plugins": {
                        "permissions": { "enabled": true, "preset": "read-only" },
                        "approvals": { "policy": "never" }
                    }
                }))
                .build()
                .await;
            let res = h.prompt("fetch", json!({})).await.unwrap();
            assert!(res.to_string().contains("WEB_BLOCKED_URL"), "{res}");
            assert_eq!(server.count("/plain"), 0);
            h.shutdown().await;
        }

        // 2. Approval on web_fetch (untrusted policy: deny then allow with allow_local=true).
        {
            let env = TestEnv::new().await;
            let script = json!([
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/plain", server.base) } }] },
                { "text": "denied-turn" },
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/plain", server.base) } }] },
                { "text": "allowed-turn" }
            ]);
            let mut b = env
                .host().builtins(&["web", "web-fetch-http"])
                .faux(script)
                .args(json!({ "no_session": true }))
                .config(json!({
                    "plugins": {
                        "permissions": {
                            "enabled": true,
                            "preset": "auto",
                            "network": { "default": "deny", "allow": ["127.0.0.1"], "allow_local": true }
                        },
                        "approvals": { "policy": "untrusted" }
                    }
                }));
            b.opts.project_trusted = true;
            let h = b.build().await;
            expose(&h);
            exec(
                &h,
                r#"
                APPROVAL_CALLS = 0
                C.cordis:get("policy"):voter("tool/approval", function()
                    APPROVAL_CALLS = APPROVAL_CALLS + 1
                    if APPROVAL_CALLS == 1 then
                        return { decision = "deny", reason = "test-deny" }
                    end
                    return { decision = "allow" }
                end)
                C.cordis:get("policy"):voter("net/approval", function()
                    return { decision = "allow" }
                end)
            "#,
            )
            .await;
            let r1 = h.prompt("first", json!({})).await.unwrap();
            let s1 = r1.to_string();
            assert!(s1.contains("test-deny") || s1.contains("denied"), "{r1}");
            assert_eq!(server.count("/plain"), 0);

            let r2 = h.prompt("second", json!({})).await.unwrap();
            assert!(r2.to_string().contains("loopback-ok"), "{r2}");
            assert_eq!(server.count("/plain"), 1);
            h.shutdown().await;
        }

        // 3. Non-public addresses (SSRF) + 4. Hop-by-hop redirects via real model tool web_fetch.
        // Hold TestEnv's process-wide lock before replacing the global DNS override (Should-fix 1).
        {
            let env = TestEnv::new().await;
            let _guard = ResolverGuard;
            let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
            let pub_ip: std::net::IpAddr = "93.184.216.34".parse().unwrap();
            let fake_ip: std::net::IpAddr = "198.18.0.5".parse().unwrap();
            mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([
                ("public.test".to_string(), vec![pub_ip]),
                ("rebind.test".to_string(), vec![ip]),
                ("fake.test".to_string(), vec![fake_ip]),
            ])));
            server.route("/to-rebind", vec![redirect_resp(&format!("http://rebind.test:{port}/steal"))]);
            {
                let script = json!([
                    { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://rebind.test:{port}/plain") } }] },
                    { "text": "ssrf-rebind" },
                    { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": "http://198.18.0.5/plain" } }] },
                    { "text": "ssrf-fakeip" },
                    { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": "http://fake.test/plain" } }] },
                    { "text": "ssrf-fake-host" }
                ]);
                let mut b = env
                    .host().builtins(&["web", "web-fetch-http"])
                    .faux(script)
                    .args(json!({ "no_session": true }))
                    .config(json!({
                        "plugins": {
                            "permissions": {
                                "enabled": true,
                                "preset": "auto",
                                "network": { "default": "deny", "allow": ["public.test", "rebind.test", "fake.test"], "allow_local": false }
                            },
                            "approvals": { "policy": "never" },
                            "web-fetch-http": { "redirect": "follow" }
                        }
                    }));
                b.opts.project_trusted = true;
                let h = b.build().await;
                let r_rebind = h.prompt("rebind", json!({})).await.unwrap();
                assert!(
                    r_rebind.to_string().contains("WEB_BLOCKED_URL")
                        && r_rebind.to_string().contains("local/private"),
                    "{r_rebind}"
                );

                let r_fakeip = h.prompt("fakeip", json!({})).await.unwrap();
                assert!(r_fakeip.to_string().contains("WEB_BLOCKED_URL"), "{r_fakeip}");

                // Default net.fake_ip = "off" rejects 198.18.x.x even for an allowlisted hostname.
                let r_fake_host = h.prompt("fakehost", json!({})).await.unwrap();
                assert!(
                    r_fake_host.to_string().contains("WEB_BLOCKED_URL")
                        && r_fake_host.to_string().contains("local/private"),
                    "{r_fake_host}"
                );
                h.shutdown().await;
            }
            drop(env);

            // Redirect to an allowlisted domain (`rebind.test`) that resolves to 127.0.0.1 must be
            // rejected by the non-public IP check (not masked by whitelist), and never hit /steal.
            {
                let mut env2 = TestEnv::new().await;
                // Loopback mock CONNECT proxy routes the non-loopback test host (resolver override); the offline
                // guard on the selected proxy route (NET1 8a974e0f) needs the same opt-in as suite::net.
                env2.set_var("MARE_TEST_ALLOW_LIVE", "1");
                let _guard2 = ResolverGuard;
                mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([
                    ("public.test".to_string(), vec![pub_ip]),
                    ("rebind.test".to_string(), vec![ip]),
                ])));
                let (proxy_port, proxy_reqs) = upstream_connect_proxy();
                let proxy_url = format!("http://127.0.0.1:{proxy_port}");
                let _env_guard = EnvVarGuard::set(&[
                    ("HTTP_PROXY", &proxy_url),
                    ("HTTPS_PROXY", &proxy_url),
                    ("NO_PROXY", ""),
                ]);
                let script2 = json!([
                    { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://public.test:{port}/to-rebind") } }] },
                    { "text": "redir-rebind" }
                ]);
                let mut b2 = env2
                    .host().builtins(&["web", "web-fetch-http"])
                    .faux(script2)
                    .args(json!({ "no_session": true }))
                    .config(json!({
                        "plugins": {
                            "permissions": {
                                "enabled": true,
                                "preset": "auto",
                                "network": { "default": "deny", "allow": ["public.test", "rebind.test"], "allow_local": false }
                            },
                            "approvals": { "policy": "never" },
                            "web-fetch-http": { "redirect": "follow" }
                        }
                    }));
                b2.opts.project_trusted = true;
                let h2 = b2.build().await;
                let r_rebind = h2.prompt("rebind", json!({})).await.unwrap();
                assert!(
                    r_rebind.to_string().contains("WEB_BLOCKED_URL")
                        && r_rebind.to_string().contains("local/private"),
                    "{r_rebind}"
                );
                assert_eq!(server.count("/steal"), 0);
                assert_eq!(
                    proxy_reqs.lock().unwrap().as_slice(),
                    [format!("CONNECT 93.184.216.34:{port} HTTP/1.1")]
                );
                h2.shutdown().await;
            }
        }
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_fetch_rpc_enforces_readonly_approvals_ssrf_and_redirects() {
    let server = MockServer::start().await;
    server.route(
        "/plain",
        vec![
            resp_with(200, "text/plain; charset=utf-8", "rpc-loopback-ok"),
            resp_with(200, "text/plain; charset=utf-8", "rpc-loopback-ok-2"),
            resp_with(200, "text/plain; charset=utf-8", "rpc-loopback-ok-3"),
        ],
    );
    server.route("/steal", vec![resp_with(200, "text/plain; charset=utf-8", "stolen")]);
    server
        .route("/to-fakeip", vec![redirect_resp("http://198.18.0.5/steal"), redirect_resp("http://198.18.0.5/steal")]);

    // Part A: --permission read-only --approval-policy ask (no --yolo)
    {
        let home = tempfile::tempdir().unwrap();
        let script = home.path().join("faux-ro.json");
        std::fs::write(
            &script,
            json!([
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/plain", server.base) } }] },
                { "text": "ro-local-blocked" },
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": "http://93.184.216.34/plain" } }] },
                { "text": "ro-public-denied" }
            ])
            .to_string(),
        )
        .unwrap();

        let mut cmd = mare_cmd();
        cmd.args([
            "--rpc",
            "--permission",
            "read-only",
            "--approval-policy",
            "ask",
            "--model",
            "faux/echo",
            "--no-session",
        ])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);
        let init = rpc.call_raw(
            "initialize",
            json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }),
            |_| json!(null),
        );
        let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

        // Direct web.fetch RPC in read-only blocks non-public address 127.0.0.1.
        let ro_rpc_local =
            rpc.call_raw("web.fetch", json!({ "url": format!("{}/plain", server.base) }), |_| json!(null));
        let ro_rpc_msg = ro_rpc_local["error"]["message"].as_str().unwrap_or("");
        assert!(ro_rpc_msg.contains("WEB_BLOCKED_URL"), "{ro_rpc_local}");

        // Direct web.fetch RPC in read-only asks approval for public IP; denying returns WEB_BLOCKED_URL.
        let mut rpc_approvals = 0;
        let ro_rpc_pub = rpc.call_raw("web.fetch", json!({ "url": "http://93.184.216.34/plain" }), |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            rpc_approvals += 1;
            json!({ "decision": "deny" })
        });
        assert_eq!(rpc_approvals, 1);
        assert!(ro_rpc_pub["error"]["message"].as_str().unwrap_or("").contains("WEB_BLOCKED_URL"), "{ro_rpc_pub}");

        // Model tool web_fetch via prompt in read-only blocks 127.0.0.1 without hitting server.
        let p1_s = rpc
            .call_raw("agent.prompt", json!({ "session_id": sid, "input": "go" }), |_| panic!("unexpected approval"));
        assert!(p1_s.get("error").is_none(), "{p1_s}");
        let p1 = rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": p1_s["result"]["turn_id"] }), |_| {
            panic!("unexpected approval")
        });
        assert!(p1.get("error").is_none(), "{p1}");
        assert_eq!(server.count("/plain"), 0);

        // Model tool web_fetch via prompt in read-only asks approval for public IP; denying blocks it.
        let mut tool_approvals = 0;
        let p2_s = rpc
            .call_raw("agent.prompt", json!({ "session_id": sid, "input": "go2" }), |_| panic!("unexpected approval"));
        assert!(p2_s.get("error").is_none(), "{p2_s}");
        let p2 =
            rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": p2_s["result"]["turn_id"] }), |req| {
                assert_eq!(req["method"], "approval.request", "{req}");
                tool_approvals += 1;
                json!({ "decision": "deny" })
            });
        assert!(p2.get("error").is_none(), "{p2}");
        assert_eq!(tool_approvals, 1);

        assert_eq!(rpc.call_raw("shutdown", json!({}), |_| json!(null))["result"], json!({}));
        rpc.wait_exit();
    }

    // Part B: --permission auto --approval-policy untrusted with allow={"127.0.0.1"}, allow_local=true, redirect="follow"
    {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("config")).unwrap();
        std::fs::write(
            home.path().join("config/init.lua"),
            r#"mare.setup({
                trust_default = "trust",
                plugins = {
                    permissions = { network = { default = "deny", allow = { "127.0.0.1" }, allow_local = true } },
                    ["web-fetch-http"] = { redirect = "follow" },
                }
            })"#,
        )
        .unwrap();
        let script = home.path().join("faux-auto.json");
        std::fs::write(
            &script,
            json!([
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/plain", server.base) } }] },
                { "text": "denied" },
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/plain", server.base) } }] },
                { "text": "allowed" },
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/to-fakeip", server.base) } }] },
                { "text": "redir-blocked" }
            ])
            .to_string(),
        )
        .unwrap();

        let mut cmd = mare_cmd();
        cmd.args([
            "--rpc",
            "--permission",
            "auto",
            "--approval-policy",
            "untrusted",
            "--model",
            "faux/echo",
            "--no-session",
        ])
        .current_dir(home.path())
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .stderr(Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);
        let init = rpc.call_raw(
            "initialize",
            json!({ "protocol_version": "3.0", "capabilities": { "approvals": true } }),
            |_| json!(null),
        );
        let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

        // Model tool web_fetch asks approval under untrusted -> deny -> allow.
        let mut a1 = 0;
        let s1 = rpc
            .call_raw("agent.prompt", json!({ "session_id": sid, "input": "t1" }), |_| panic!("unexpected approval"));
        let _ = rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": s1["result"]["turn_id"] }), |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            a1 += 1;
            json!({ "decision": "deny" })
        });
        assert_eq!(a1, 1);
        assert_eq!(server.count("/plain"), 0);

        let mut a2 = 0;
        let s2 = rpc
            .call_raw("agent.prompt", json!({ "session_id": sid, "input": "t2" }), |_| panic!("unexpected approval"));
        let _ = rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": s2["result"]["turn_id"] }), |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            a2 += 1;
            json!({ "decision": "allow" })
        });
        assert_eq!(a2, 1);
        assert_eq!(server.count("/plain"), 1);

        // Model tool web_fetch approved at tool/before, then redirect hop to 198.18.0.5 is blocked without hitting /steal.
        let mut a3 = 0;
        let s3 = rpc
            .call_raw("agent.prompt", json!({ "session_id": sid, "input": "t3" }), |_| panic!("unexpected approval"));
        let _ = rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": s3["result"]["turn_id"] }), |req| {
            assert_eq!(req["method"], "approval.request", "{req}");
            a3 += 1;
            json!({ "decision": "allow" })
        });
        assert_eq!(a3, 1);
        assert_eq!(server.count("/steal"), 0);

        // Direct web.fetch RPC: allowed host works, and redirect hop to 198.18.0.5 is blocked.
        let ok_rpc = rpc.call_raw("web.fetch", json!({ "url": format!("{}/plain", server.base) }), |req| {
            panic!("unexpected approval for allowed host: {req}");
        });
        assert!(ok_rpc["result"]["content"].as_str().unwrap_or("").contains("rpc-loopback-ok"), "{ok_rpc}");

        let redir_rpc = rpc.call_raw("web.fetch", json!({ "url": format!("{}/to-fakeip", server.base) }), |req| {
            panic!("SSRF redirect must not ask: {req}")
        });
        assert!(redir_rpc["error"]["message"].as_str().unwrap_or("").contains("WEB_BLOCKED_URL"), "{redir_rpc}");
        assert_eq!(server.count("/steal"), 0);

        assert_eq!(rpc.call_raw("shutdown", json!({}), |_| json!(null))["result"], json!({}));
        rpc.wait_exit();
    }
}

#[tokio::test]
async fn web_fetch_rpc_blocks_allowlisted_redirect_to_private_ip_via_proxy() {
    // 参考测试：codex-rs/network-proxy/src/host_policy_tests.rs:35 (approval permits DNS, must still reject private resolution)
    // Real RPC entrypoint (`mare_cli::rpc::serve` -> `web.fetch`): `rebind.test` is in `allow`,
    // so only the resolved non-public IP check rejects the redirect hop.
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    local(async {
        let server = MockServer::start().await;
        let port = host_port(&server.base);
        server.route("/to-rebind", vec![redirect_resp(&format!("http://rebind.test:{port}/steal"))]);
        server.route("/steal", vec![resp_with(200, "text/plain", "stolen")]);

        let mut env = TestEnv::new().await;
        // Loopback mock CONNECT proxy routes the non-loopback test host (resolver override); the offline
        // guard on the selected proxy route (NET1 8a974e0f) needs the same opt-in as suite::net.
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        let _guard = ResolverGuard;
        let pub_ip: std::net::IpAddr = "93.184.216.34".parse().unwrap();
        let priv_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([
            ("public.test".to_string(), vec![pub_ip]),
            ("rebind.test".to_string(), vec![priv_ip]),
        ])));
        let (proxy_port, proxy_reqs) = upstream_connect_proxy();
        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        let _env_guard = EnvVarGuard::set(&[("HTTP_PROXY", &proxy_url), ("NO_PROXY", "")]);

        let mut b = env.host().builtins(&["web", "web-fetch-http"]).no_session().config(json!({
            "plugins": {
                "permissions": {
                    "enabled": true,
                    "preset": "auto",
                    "network": { "default": "deny", "allow": ["public.test", "rebind.test"], "allow_local": false }
                },
                "approvals": { "policy": "never" },
                "web-fetch-http": { "redirect": "follow" }
            }
        }));
        b.opts.mode = mare_shared::ui::Mode::Rpc;
        b.opts.project_trusted = true;
        let h = b.build().await;

        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (srv_r, srv_w) = tokio::io::split(server_io);
        let (cli_r, mut cli_w) = tokio::io::split(client_io);
        let stop = tokio_util::sync::CancellationToken::new();
        let stop_c = stop.clone();
        let srv_task = tokio::task::spawn_local(mare_cli::rpc::serve(h.clone(), srv_r, srv_w, true, async move {
            stop_c.cancelled().await;
        }));

        let mut lines = tokio::io::BufReader::new(cli_r).lines();
        let mut rpc_call = async |id: u64, method: &str, params: serde_json::Value| -> serde_json::Value {
            let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
            cli_w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
            cli_w.flush().await.unwrap();
            loop {
                let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
                    .await
                    .expect("RPC response timed out")
                    .unwrap()
                    .expect("RPC stream closed");
                let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
                if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                    return msg;
                }
            }
        };

        let init = rpc_call(1, "initialize", json!({ "protocol_version": "3.0", "capabilities": {} })).await;
        assert!(init.get("result").is_some(), "initialize failed: {init}");

        let resp = rpc_call(2, "web.fetch", json!({ "url": format!("http://public.test:{port}/to-rebind") })).await;
        let err = resp["error"]["message"].as_str().unwrap_or("");
        assert!(err.contains("WEB_BLOCKED_URL") && err.contains("local/private"), "unexpected RPC response: {resp}");
        assert_eq!(server.count("/steal"), 0);
        assert_eq!(proxy_reqs.lock().unwrap().as_slice(), [format!("CONNECT 93.184.216.34:{port} HTTP/1.1")]);

        let _ = rpc_call(3, "shutdown", json!({})).await;
        stop.cancel();
        drop(cli_w);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), srv_task).await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn web_fetch_proxy_pins_ip_across_http_https_redirects_and_fake_ip() {
    // Items 1 & 2:
    // - HTTP & HTTPS via upstream proxy pin the authorized IP (`CONNECT <pinned_ip>:<port>`) on every hop.
    // - Fake-IP (198.18.x.x) is rejected by default (`net.fake_ip = "off"`) and via remote proxy even when `"auto"`.
    local(async {
        let server = MockServer::start().await;
        let port = host_port(&server.base);
        server.route("/hop1", vec![redirect_resp(&format!("http://hop2.test:{port}/hop2"))]);
        server.route("/hop2", vec![resp_with(200, "text/plain", "pinned-redirect-ok")]);

        // Minimal TCP listener for https:// CONNECT check: proxy must receive CONNECT <pinned_ip>:<port>.
        let tls_srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tls_port = tls_srv.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in tls_srv.incoming().flatten() {
                drop(c);
            }
        });

        let mut env = TestEnv::new().await;
        // Loopback mock CONNECT proxy routes the non-loopback test host (resolver override); the offline
        // guard on the selected proxy route (NET1 8a974e0f) needs the same opt-in as suite::net.
        env.set_var("MARE_TEST_ALLOW_LIVE", "1");
        let _guard = ResolverGuard;
        let pub_ip: std::net::IpAddr = "93.184.216.34".parse().unwrap();
        let fake_ip: std::net::IpAddr = "198.18.0.5".parse().unwrap();
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([
            ("hop1.test".to_string(), vec![pub_ip]),
            ("hop2.test".to_string(), vec![pub_ip]),
            ("tls.test".to_string(), vec![pub_ip]),
            ("fake.test".to_string(), vec![fake_ip]),
        ])));
        let (proxy_port, proxy_reqs) = upstream_connect_proxy();
        let proxy_url = format!("http://127.0.0.1:{proxy_port}");
        let _env_guard = EnvVarGuard::set(&[
            ("HTTP_PROXY", &proxy_url),
            ("HTTPS_PROXY", &proxy_url),
            ("NO_PROXY", ""),
        ]);

        let script = json!([
            { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://hop1.test:{port}/hop1") } }] },
            { "text": "http-redir" },
            { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("https://tls.test:{tls_port}/") } }] },
            { "text": "https-connect" },
            { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://fake.test:{port}/hop2") } }] },
            { "text": "fake-blocked" }
        ]);
        let mut b = env
            .host().builtins(&["web", "web-fetch-http"])
            .faux(script)
            .args(json!({ "no_session": true }))
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "auto",
                        "network": {
                            "default": "deny",
                            "allow": ["hop1.test", "hop2.test", "tls.test", "fake.test"],
                            "allow_local": false
                        }
                    },
                    "approvals": { "policy": "never" },
                    "web-fetch-http": { "redirect": "follow" }
                }
            }));
        b.opts.project_trusted = true;
        let h = b.build().await;

        let r1 = h.prompt("http-redir", json!({})).await.unwrap().to_string();
        assert!(r1.contains("pinned-redirect-ok"), "{r1}");

        // HTTPS fails at TLS handshake (dummy TCP listener closes), AFTER proxy receives CONNECT 93.184.216.34:<tls_port>.
        let _ = h.prompt("https-connect", json!({})).await.unwrap();

        let r3 = h.prompt("fake-blocked", json!({})).await.unwrap().to_string();
        assert!(r3.contains("WEB_BLOCKED_URL"), "{r3}");

        let seen = proxy_reqs.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                format!("CONNECT 93.184.216.34:{port} HTTP/1.1"),
                format!("CONNECT 93.184.216.34:{port} HTTP/1.1"),
                format!("CONNECT 93.184.216.34:{tls_port} HTTP/1.1"),
            ],
            "proxy must receive pinned IP CONNECT for every HTTP/HTTPS hop and never for fake.test: {seen:?}"
        );
        h.shutdown().await;
    })
    .await
}

/// `web_fetch` enforces netauth public-IP validation and DNS pinning even under
/// `preset = "auto"` (`Network::Open`).
#[tokio::test]
async fn web_fetch_blocks_ssrf_even_under_auto_open_network() {
    local(async {
        let server = MockServer::start().await;
        let port = host_port(&server.base);
        server.route("/plain", vec![resp_with(200, "text/plain; charset=utf-8", "should-not-reach")]);

        let env = TestEnv::new().await;
        let _guard = ResolverGuard;
        let ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
            "rebind-auto.test".to_string(),
            vec![ip],
        )])));

        let script = json!([
            { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("{}/plain", server.base) } }] },
            { "text": "done-loopback" },
            { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://rebind-auto.test:{port}/plain") } }] },
            { "text": "done-rebind" }
        ]);
        let mut b = env
            .host()
            .builtins(&["web", "web-fetch-http"])
            .faux(script)
            .args(json!({ "no_session": true }))
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "auto" },
                    "approvals": { "policy": "never" }
                }
            }));
        b.opts.project_trusted = true;
        let h = b.build().await;

        let r1 = h.prompt("loopback", json!({})).await.unwrap().to_string();
        assert!(r1.contains("WEB_BLOCKED_URL"), "loopback must be blocked under auto Network::Open: {r1}");
        let r2 = h.prompt("rebind", json!({})).await.unwrap().to_string();
        assert!(r2.contains("WEB_BLOCKED_URL"), "rebind must be blocked under auto Network::Open: {r2}");
        assert_eq!(server.count("/plain"), 0);
        h.shutdown().await;
    })
    .await
}

/// `web_fetch` under an open-network preset (`preset = "auto"`, `Network::Open`) must honour
/// `net.fake_ip`: `"off"` rejects `198.18.0.0/15` as private, whereas `"auto"` authorizes direct
/// hostnames resolving to `198.18.0.0/15` while keeping literal fake-IPs, loopback rebinding,
/// local-proxy routing (without `allow_local`), and remote-proxy routing rejected.
#[tokio::test]
async fn web_fetch_honors_net_fake_ip_off_vs_auto_under_open_network() {
    local(async {
        let server = MockServer::start().await;
        let port = host_port(&server.base);
        server.route("/plain", vec![resp_with(200, "text/plain; charset=utf-8", "should-not-reach")]);

        let loopback: std::net::IpAddr = "127.0.0.1".parse().unwrap();
        let fake_ip: std::net::IpAddr = "198.18.0.5".parse().unwrap();

        // 1. net.fake_ip = "off" (default): hostname resolving to 198.18.0.5 is blocked as private.
        {
            let env = TestEnv::new().await;
            let _guard = ResolverGuard;
            mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
                "fake-open.test".to_string(),
                vec![fake_ip],
            )])));
            let _env_guard = EnvVarGuard::set(&[("HTTP_PROXY", ""), ("HTTPS_PROXY", ""), ("NO_PROXY", "")]);
            let script = json!([
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://fake-open.test:{port}/plain") } }] },
                { "text": "done-off" }
            ]);
            let mut b = env
                .host()
                .builtins(&["web", "web-fetch-http"])
                .faux(script)
                .args(json!({ "no_session": true }))
                .config(json!({
                    "net": { "fake_ip": "off" },
                    "plugins": {
                        "permissions": { "enabled": true, "preset": "auto" },
                        "approvals": { "policy": "never" }
                    }
                }));
            b.opts.project_trusted = true;
            let h = b.build().await;
            let r = h.prompt("fake-off", json!({})).await.unwrap().to_string();
            assert!(
                r.contains("WEB_BLOCKED_URL") && r.contains("local/private"),
                "net.fake_ip=off must reject 198.18.x.x under open network: {r}"
            );
            h.shutdown().await;
        }

        // 2. net.fake_ip = "auto": direct hostname resolving to 198.18.0.5 passes authorization
        // (reaching the offline non-loopback guard instead of WEB_BLOCKED_URL), while literal
        // 198.18.0.5, loopback DNS rebinding, local proxy, and remote proxy stay rejected.
        {
            let env = TestEnv::new().await;
            let _guard = ResolverGuard;
            mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([
                ("fake-open.test".to_string(), vec![fake_ip]),
                ("rebind-open.test".to_string(), vec![loopback]),
            ])));
            let _env_guard = EnvVarGuard::set(&[("HTTP_PROXY", ""), ("HTTPS_PROXY", ""), ("NO_PROXY", "")]);
            let script = json!([
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://fake-open.test:{port}/plain") } }] },
                { "text": "done-auto-direct" },
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://198.18.0.5:{port}/plain") } }] },
                { "text": "done-auto-literal" },
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://rebind-open.test:{port}/plain") } }] },
                { "text": "done-auto-rebind" }
            ]);
            let mut b = env
                .host()
                .builtins(&["web", "web-fetch-http"])
                .faux(script)
                .args(json!({ "no_session": true }))
                .config(json!({
                    "net": { "fake_ip": "auto" },
                    "plugins": {
                        "permissions": { "enabled": true, "preset": "auto" },
                        "approvals": { "policy": "never" }
                    }
                }));
            b.opts.project_trusted = true;
            let h = b.build().await;

            let r_direct = h.prompt("auto-direct", json!({})).await.unwrap().to_string();
            assert!(
                !r_direct.contains("local/private") && r_direct.contains("offline test guard"),
                "net.fake_ip=auto must authorize direct fake-IP hostname under open network: {r_direct}"
            );

            let r_literal = h.prompt("auto-literal", json!({})).await.unwrap().to_string();
            assert!(
                r_literal.contains("WEB_BLOCKED_URL") && r_literal.contains("local/private"),
                "literal 198.18.0.5 must stay blocked even with net.fake_ip=auto: {r_literal}"
            );

            let r_rebind = h.prompt("auto-rebind", json!({})).await.unwrap().to_string();
            assert!(
                r_rebind.contains("WEB_BLOCKED_URL") && r_rebind.contains("local/private"),
                "loopback rebind must stay blocked even with net.fake_ip=auto: {r_rebind}"
            );
            h.shutdown().await;
        }

        // 3. net.fake_ip = "auto" via local proxy (allow_local = false on block_private_open) and
        // via remote proxy must keep their existing authorization denials unchanged.
        for (label, proxy_url, expected_fragment) in [
            ("local proxy", "http://127.0.0.1:18080", "by-name local proxy routing requires allow_local = true"),
            ("remote proxy", "http://198.51.100.10:8080", "cannot be routed through a non-local proxy"),
        ] {
            let mut env = TestEnv::new().await;
            env.set_var("MARE_TEST_ALLOW_LIVE", "1");
            let _guard = ResolverGuard;
            mare_binding::netauth::set_resolver_override(Some(std::collections::HashMap::from([(
                "fake-open.test".to_string(),
                vec![fake_ip],
            )])));
            let _env_guard =
                EnvVarGuard::set(&[("HTTP_PROXY", proxy_url), ("HTTPS_PROXY", proxy_url), ("NO_PROXY", "")]);
            let script = json!([
                { "tool_calls": [{ "name": "web_fetch", "arguments": { "url": format!("http://fake-open.test:{port}/plain") } }] },
                { "text": "done-proxy" }
            ]);
            let mut b = env
                .host()
                .builtins(&["web", "web-fetch-http"])
                .faux(script)
                .args(json!({ "no_session": true }))
                .config(json!({
                    "net": { "fake_ip": "auto" },
                    "plugins": {
                        "permissions": { "enabled": true, "preset": "auto" },
                        "approvals": { "policy": "never" }
                    }
                }));
            b.opts.project_trusted = true;
            let h = b.build().await;
            let r = h.prompt(label, json!({})).await.unwrap().to_string();
            assert!(
                r.contains("WEB_BLOCKED_URL") && r.contains(expected_fragment),
                "{label} fake-IP authorization rule must stay unchanged: {r}"
            );
            h.shutdown().await;
        }
    })
    .await
}
