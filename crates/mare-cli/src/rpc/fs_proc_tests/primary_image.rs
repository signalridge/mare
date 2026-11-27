use super::*;

#[tokio::test]
async fn rpc_primary_image_permissions_read_rule_and_symlink() {
    const PNG_1X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21,
        196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1, 13, 10, 45, 180, 0, 0, 0, 0, 73, 69,
        78, 68, 174, 66, 96, 130,
    ];
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let outside_img = outside.path().join("outside.png");
            std::fs::write(&outside_img, PNG_1X1).unwrap();
            std::fs::write(ws.path().join("allowed.png"), PNG_1X1).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside_img, ws.path().join("link.png")).unwrap();

            // 1. Fail-closed when permissions plugin is disabled
            let host_no_perms = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"enabled": false}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task =
                tokio::task::spawn_local(serve(host_no_perms.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_no_perms, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
            let no_perms = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert_eq!(no_perms["error"]["code"], codes::INVALID_PARAMS, "{no_perms}");
            assert!(
                no_perms["error"]["message"].as_str().unwrap_or("").contains("permissions policy cannot adjudicate"),
                "{no_perms}"
            );
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_no_perms.shutdown().await;

            // 2. Permissions read forbidden rule denies primary image attachment
            let host_forbidden = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"rules": [{"tool": "read", "decision": "forbidden"}]}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task =
                tokio::task::spawn_local(serve(host_forbidden.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_forbidden, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
            let forbidden = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert_eq!(forbidden["error"]["code"], codes::INVALID_PARAMS, "{forbidden}");
            let forbidden_msg = forbidden["error"]["message"].as_str().unwrap_or("");
            assert!(
                !forbidden_msg.contains("cannot adjudicate") && !forbidden_msg.contains("did not adjudicate"),
                "must fail due to forbidden rule, not adjudication failure: {forbidden_msg}"
            );
            assert!(
                forbidden_msg.contains("forbidden")
                    || forbidden_msg.contains("rule")
                    || forbidden_msg.contains("blocked"),
                "expected specific forbidden rule denial, got: {forbidden_msg}"
            );
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_forbidden.shutdown().await;

            // 3. Permissions allow permits allowed.png but rejects symlink escaping workspace
            let host_allow = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"preset": "auto"}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host_allow.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_allow, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
            let ok_res = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert!(ok_res.get("error").is_none(), "{ok_res}");
            let _ = call_rpc(
                &mut cw,
                &mut cr,
                101,
                "agent.wait",
                json!({"session_id": sid, "turn_id": ok_res["result"]["turn_id"]}),
            )
            .await;
            #[cfg(unix)]
            {
                let sym_res = call_rpc(
                    &mut cw,
                    &mut cr,
                    2,
                    "agent.prompt",
                    json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "link.png"}]}),
                )
                .await;
                assert_eq!(sym_res["error"]["code"], codes::INVALID_PARAMS, "{sym_res}");
                let sym_msg = sym_res["error"]["message"].as_str().unwrap_or("");
                assert!(
                    sym_msg.contains("cannot open image")
                        || sym_msg.contains("symlink")
                        || sym_msg.contains("symbolic"),
                    "expected symlink open failure, got: {sym_msg}"
                );
            }
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_allow.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_primary_image_enforces_verdict_sandboxes_explicit_provider_and_parent_relative_paths() {
    const PNG_1X1: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21,
        196, 137, 0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0, 1, 13, 10, 45, 180, 0, 0, 0, 0, 73, 69,
        78, 68, 174, 66, 96, 130,
    ];
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let parent = tempfile::tempdir().unwrap();
            let ws = parent.path().join("ws");
            let pics = parent.path().join("pictures");
            std::fs::create_dir_all(ws.join(".mare/plugins")).unwrap();
            std::fs::create_dir_all(&pics).unwrap();
            std::fs::write(ws.join("allowed.png"), PNG_1X1).unwrap();
            std::fs::write(ws.join("blocked_by_verdict.png"), PNG_1X1).unwrap();
            std::fs::write(pics.join("outside_allowed.png"), PNG_1X1).unwrap();

            // 1. Dummy permissions provider without a tool/before listener must be rejected even though Host registers tool/before
            std::fs::write(
                ws.join(".mare/plugins/dummy-perms.lua"),
                r#"return {
                    name = "dummy-perms",
                    provide = { "permissions" },
                    apply = function(ctx)
                        local noop = function() end
                        require("mare_lib").service(ctx, "permissions", {
                            grants = function() return {} end,
                            request = noop,
                            commit_request = noop,
                            describe_request = noop,
                            resolve_request = noop,
                            policy_revision = function() return 1 end,
                            is_partial = function() return false end,
                            preset = function() return "auto" end,
                            get = function() return {} end,
                            list = function() return { data = {} } end,
                            set = noop,
                            presets = function() return {} end,
                            rules = function() return {} end,
                            check = noop,
                            rule_verdict = noop,
                            describe = noop,
                            add_rules = noop,
                        })
                    end
                }"#,
            )
            .unwrap();
            let host_dummy = Host::new(mare_host::HostOptions {
                cwd: ws.clone(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"enabled": false}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: false,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host_dummy.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_dummy, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
            let no_adj = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert_eq!(no_adj["error"]["code"], codes::INVALID_PARAMS, "{no_adj}");
            let no_adj_msg = no_adj["error"]["message"].as_str().unwrap_or("");
            assert!(no_adj_msg.contains("did not adjudicate"), "expected adjudication denial, got: {no_adj_msg}");
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_dummy.shutdown().await;
            let _ = std::fs::remove_file(ws.join(".mare/plugins/dummy-perms.lua"));

            // 2. Valid ../pictures/outside_allowed.png path when allowed by permissions readable root
            let host_rel = Host::new(mare_host::HostOptions {
                cwd: ws.clone(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"preset": "auto", "readable": [pics.to_string_lossy()]}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host_rel.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_rel, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
            let rel_ok = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "../pictures/outside_allowed.png"}]}),
            )
            .await;
            assert!(rel_ok.get("error").is_none(), "expected ../pictures/outside_allowed.png to succeed: {rel_ok}");
            let _ = call_rpc(
                &mut cw,
                &mut cr,
                102,
                "agent.wait",
                json!({"session_id": sid, "turn_id": rel_ok["result"]["turn_id"]}),
            )
            .await;
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_rel.shutdown().await;

            // 3. tool/before verdict sandboxes (deny_read via permission/verdict) must block blocked_by_verdict.png while allowing allowed.png
            std::fs::write(
                ws.join(".mare/plugins/test-verdict.lua"),
                r#"return {
                    name = "test-verdict-sandbox",
                    apply = function(ctx)
                        ctx:get("policy"):voter("permission/verdict", function()
                            return {
                                decision = "allow",
                                sandbox = { level = "workspace-write", restrict_read = true, deny_read = { "blocked_by_verdict.png" } }
                            }
                        end)
                    end
                }"#,
            )
            .unwrap();
            let host_verdict = Host::new(mare_host::HostOptions {
                cwd: ws.clone(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"preset": "auto"}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: false,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host_verdict.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_verdict, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
            let blocked = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "blocked_by_verdict.png"}]}),
            )
            .await;
            assert_eq!(blocked["error"]["code"], codes::INVALID_PARAMS, "{blocked}");
            let blocked_msg = blocked["error"]["message"].as_str().unwrap_or("");
            assert!(
                blocked_msg.contains("blocked_by_verdict.png")
                    && blocked_msg.contains("sandbox: file access denied"),
                "expected verdict sandbox denial, got: {blocked_msg}"
            );
            let allowed = call_rpc(
                &mut cw,
                &mut cr,
                2,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert!(allowed.get("error").is_none(), "expected allowed.png to pass verdict sandbox: {allowed}");
            let _ = call_rpc(
                &mut cw,
                &mut cr,
                103,
                "agent.wait",
                json!({"session_id": sid, "turn_id": allowed["result"]["turn_id"]}),
            )
            .await;
            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_verdict.shutdown().await;
            let _ = std::fs::remove_file(ws.join(".mare/plugins/test-verdict.lua"));

            // 4. Non-existent intermediate directory before `..` must fail (not collapse lexically)
            //    and broad sandbox_grant must still preserve session deny_read for images and @file refs
            std::fs::write(ws.join("secret.png"), PNG_1X1).unwrap();
            std::fs::write(ws.join("secret.txt"), "top-secret-text").unwrap();
            std::fs::write(ws.join("allowed.txt"), "allowed-text").unwrap();
            std::fs::write(ws.join("bad_utf8.bin"), [0xff, 0xfe, 0xfd]).unwrap();
            std::fs::write(
                ws.join(".mare/plugins/test-broad-grant.lua"),
                r#"return {
                    name = "test-broad-grant",
                    apply = function(ctx)
                        ctx:get("policy"):voter("tool/before", function(call)
                            if call.id == "rpc-image-attachment" and call.arguments.path:match("secret%.png$") then
                                mare._broad_grant_secret_image_voted = true
                            elseif call.id == "file-ref" and call.arguments.path:match("secret%.txt$") then
                                mare._broad_grant_secret_ref_voted = true
                            end
                            return { decision = "allow", sandbox_grant = "danger-full-access" }
                        end)
                    end
                }"#,
            )
            .unwrap();
            let host_grant = Host::new(mare_host::HostOptions {
                cwd: ws.clone(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"preset": "auto"}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: false,
            })
            .await
            .unwrap();
            set_lua_sandbox(
                &host_grant,
                r#"{ level = "workspace-write", deny_read = { "secret.png", "secret.txt" } }"#,
            );
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host_grant.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_grant, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;

            let missing_dotdot = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "missing/../allowed.png"}]}),
            )
            .await;
            assert_eq!(missing_dotdot["error"]["code"], codes::INVALID_PARAMS, "{missing_dotdot}");

            let secret_img = call_rpc(
                &mut cw,
                &mut cr,
                2,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "secret.png"}]}),
            )
            .await;
            assert!(
                host_grant.lua().load("return mare._broad_grant_secret_image_voted == true").eval::<bool>().unwrap(),
                "broad-grant voter did not vote on the secret.png RPC read"
            );
            assert_eq!(secret_img["error"]["code"], codes::INVALID_PARAMS, "{secret_img}");
            let secret_msg = secret_img["error"]["message"].as_str().unwrap_or("");
            assert!(
                secret_msg.contains("secret.png")
                    && secret_msg.contains("sandbox: file access denied"),
                "expected deny_read rejection for secret.png, got: {secret_msg}"
            );
            let allowed_under_deny = call_rpc(
                &mut cw,
                &mut cr,
                3,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert!(
                allowed_under_deny.get("error").is_none(),
                "expected allowed.png to succeed under deny_read policy: {allowed_under_deny}"
            );
            let _ = call_rpc(
                &mut cw,
                &mut cr,
                104,
                "agent.wait",
                json!({"session_id": sid, "turn_id": allowed_under_deny["result"]["turn_id"]}),
            )
            .await;

            let fileref_start = call_rpc(
                &mut cw,
                &mut cr,
                4,
                "agent.prompt",
                json!({"session_id": sid, "input": "read @secret.txt and @allowed.txt and @bad_utf8.bin"}),
            )
            .await;
            let fileref_res = call_rpc(
                &mut cw,
                &mut cr,
                105,
                "agent.wait",
                json!({"session_id": sid, "turn_id": fileref_start["result"]["turn_id"]}),
            )
            .await;
            assert!(
                host_grant.lua().load("return mare._broad_grant_secret_ref_voted == true").eval::<bool>().unwrap(),
                "broad-grant voter did not vote on the secret.txt file reference"
            );
            assert!(fileref_res.get("error").is_none(), "{fileref_res}");
            let fileref_json = serde_json::to_string(&fileref_res).unwrap();
            assert!(!fileref_json.contains("top-secret-text"), "file-ref leaked deny_read file: {fileref_json}");
            assert!(fileref_json.contains("allowed-text"), "file-ref should attach allowed.txt: {fileref_json}");
            assert!(fileref_json.contains("invalid UTF-8"), "file-ref should reject invalid UTF-8: {fileref_json}");

            // Growth-race cap check: grow the file in the hook between f.metadata() and read_to_end()
            #[cfg(debug_assertions)]
            {
                let growing = ws.join("growing.txt");
                std::fs::write(&growing, "a".repeat(32)).unwrap();
                let f = std::fs::File::open(&growing).unwrap();
                let growing_for_hook = growing.clone();
                mare_binding::fs::testing::set_read_capped_after_stat_hook(move || {
                    use std::io::Write;
                    let mut wf = std::fs::OpenOptions::new().append(true).open(&growing_for_hook).unwrap();
                    wf.write_all(&b"b".repeat(128)).unwrap();
                });
                let capped_err = mare_binding::fs::read_file_capped(f, Some(64), "growing.txt").unwrap_err();
                let err_s = capped_err.to_string();
                assert!(
                    err_s.contains("the content exceeds") && err_s.contains("too_large"),
                    "expected post-stat growth cap error, got: {err_s}"
                );
            }

            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_grant.shutdown().await;
            let _ = std::fs::remove_file(ws.join(".mare/plugins/test-broad-grant.lua"));

            // 5. Primary RPC without permissions service has no confirmable authorization:
            //    must preserve source = "rpc" and emit only a mention block without reading @secret.txt.
            let host_no_perms = Host::new(mare_host::HostOptions {
                cwd: ws.clone(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "plugins": {"permissions": {"enabled": false}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host_no_perms.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let sid = super::call_service(&host_no_perms, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;

            let rpc_no_perms_start = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": "read @secret.txt"}),
            )
            .await;
            assert!(rpc_no_perms_start.get("error").is_none(), "{rpc_no_perms_start}");
            let rpc_no_perms = call_rpc(
                &mut cw,
                &mut cr,
                106,
                "agent.wait",
                json!({"session_id": sid, "turn_id": rpc_no_perms_start["result"]["turn_id"]}),
            )
            .await;
            assert!(rpc_no_perms.get("error").is_none(), "{rpc_no_perms}");
            let rpc_no_perms_json = serde_json::to_string(&rpc_no_perms).unwrap();
            assert!(
                !rpc_no_perms_json.contains("top-secret-text"),
                "primary RPC without permissions leaked secret.txt: {rpc_no_perms_json}"
            );
            let msg0 = &rpc_no_perms["result"]["messages"][0];
            assert_eq!(msg0["source"], "rpc", "primary RPC must preserve source=rpc: {msg0}");
            assert_eq!(
                msg0["content"][1],
                json!({"type": "mention", "path": "secret.txt"}),
                "primary RPC without permissions must emit only mention: {msg0}"
            );

            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host_no_perms.shutdown().await;
        })
        .await;
}
