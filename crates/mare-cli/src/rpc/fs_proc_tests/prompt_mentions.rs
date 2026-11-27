use super::*;

#[tokio::test]
async fn rpc_prompt_and_try_enforce_read_policy_and_emit_ordered_mention_blocks() {
    // 1x1 transparent PNG
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
            std::fs::write(ws.path().join("secret.png"), PNG_1X1).unwrap();
            std::fs::write(ws.path().join("allowed.png"), PNG_1X1).unwrap();
            std::fs::write(ws.path().join("secret.txt"), "top-secret-body").unwrap();

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": {"allow": ["agent.prompt", "agent.wait"]},
                    "plugins": {"permissions": {"preset": "auto", "deny_read": ["secret.png", "secret.txt"]}}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            set_lua_sandbox(
                &host,
                r#"{ level = "workspace-write", exclude_slash_tmp = true, exclude_tmpdir_env_var = true, restrict_read = true, deny_read = { "secret.png", "secret.txt" } }"#,
            );

            let owner = super::call_service(&host, "session", "info", vec![]).await.unwrap();
            let sid = owner["session_id"].as_str().unwrap().to_string();
            let session_epoch = owner["session_epoch"].as_str().unwrap().to_string();

            // Peer connection cannot attach image paths on `agent.prompt` (with `when_busy` queue or reject)
            let (p_client, p_server) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(p_server);
            let peer_task = tokio::task::spawn_local(serve(host.clone(), psr, psw, false, std::future::pending()));
            let (pcr, mut pcw) = tokio::io::split(p_client);
            let mut pcr = BufReader::new(pcr);
            let _ = call_rpc(&mut pcw, &mut pcr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;

            let peer_prompt = call_rpc(
                &mut pcw,
                &mut pcr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert_eq!(peer_prompt["error"]["code"], codes::INVALID_PARAMS, "{peer_prompt}");
            assert!(
                peer_prompt["error"]["message"].as_str().unwrap_or("").contains("peer connections cannot attach image paths"),
                "{peer_prompt}"
            );

            let peer_try = call_rpc(
                &mut pcw,
                &mut pcr,
                2,
                "agent.prompt",
                json!({
                    "session_id": sid,
                    "when_busy": "reject",
                    "expected_epoch": session_epoch,
                    "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]
                }),
            )
            .await;
            assert_eq!(peer_try["error"]["code"], codes::INVALID_PARAMS, "{peer_try}");

            // Peer file attachment does not read secret.txt and emits a typed mention block
            let peer_file_start = call_rpc(
                &mut pcw,
                &mut pcr,
                3,
                "agent.prompt",
                json!({
                    "session_id": sid,
                    "input": [
                        {"type": "text", "text": "inspect"},
                        {"type": "file", "path": "secret.txt", "name": "Secret", "mime": "text/plain"}
                    ]
                }),
            )
            .await;
            assert!(peer_file_start.get("error").is_none(), "{peer_file_start}");
            let peer_file = call_rpc(
                &mut pcw,
                &mut pcr,
                103,
                "agent.wait",
                json!({"session_id": sid, "turn_id": peer_file_start["result"]["turn_id"]}),
            )
            .await;
            assert!(peer_file.get("error").is_none(), "{peer_file}");
            assert!(!peer_file.to_string().contains("top-secret-body"), "{peer_file}");
            let user_content = &peer_file["result"]["messages"][0]["content"];
            assert_eq!(user_content[0], json!({"type": "text", "text": "inspect"}), "{user_content}");
            assert_eq!(
                user_content[1],
                json!({"type": "mention", "path": "secret.txt", "name": "Secret", "mime": "text/plain"}),
                "{user_content}"
            );

            // Force a new model-catalog disclosure after the user's next input, as can happen
            // when a deferred provider becomes ready between turns.
            host.lua().load(r#"
                local llm = ...
                llm.list_models = function()
                    return { data = mare.json.array({
                        { ref = "faux/echo", available = true, ready = true },
                        { ref = "faux/echo2", available = true, ready = true },
                        { ref = "faux/mention-test", available = true, ready = true },
                    }) }
                end
            "#).call::<()>(host.kernel.service("llm").unwrap()).unwrap();

            // Peer plain `@path` must not read secret.txt and emits a typed mention block
            let peer_at_start = call_rpc(
                &mut pcw,
                &mut pcr,
                4,
                "agent.prompt",
                json!({
                    "session_id": sid,
                    "input": "inspect @secret.txt"
                }),
            )
            .await;
            assert!(peer_at_start.get("error").is_none(), "{peer_at_start}");
            let peer_at = call_rpc(
                &mut pcw,
                &mut pcr,
                104,
                "agent.wait",
                json!({"session_id": sid, "turn_id": peer_at_start["result"]["turn_id"]}),
            )
            .await;
            assert!(peer_at.get("error").is_none(), "{peer_at}");
            assert!(!peer_at.to_string().contains("top-secret-body"), "{peer_at}");
            let at_msgs = peer_at["result"]["messages"].as_array().unwrap();
            assert!(at_msgs.iter().any(|m| m["context_disclosure"]["namespace"] == "subagent.models"), "{at_msgs:?}");
            let last_peer_at = at_msgs
                .iter()
                .rev()
                .find(|m| m["role"] == "user" && m["content"][0]["text"] == "inspect @secret.txt")
                .unwrap_or_else(|| panic!("peer input missing from turn messages: {at_msgs:?}"));
            let at_content = &last_peer_at["content"];
            assert_eq!(at_content[0], json!({"type": "text", "text": "inspect @secret.txt"}), "{at_content}");
            assert_eq!(at_content[1], json!({"type": "mention", "path": "secret.txt"}), "{at_content}");

            drop(pcr);
            drop(pcw);
            peer_task.await.unwrap();

            // Primary connection under sandbox policy is denied reading secret.png and outside_img
            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let server_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, true, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;

            let deny_secret = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "secret.png"}]}),
            )
            .await;
            assert_eq!(deny_secret["error"]["code"], codes::INVALID_PARAMS, "{deny_secret}");
            let deny_secret_msg = deny_secret["error"]["message"].as_str().unwrap_or("");
            assert!(
                deny_secret_msg.contains("secret.png") || deny_secret_msg.contains("sandbox") || deny_secret_msg.contains("deny"),
                "expected sandbox/deny_read reason, got: {deny_secret_msg}"
            );

            let deny_outside = call_rpc(
                &mut cw,
                &mut cr,
                2,
                "agent.prompt",
                json!({
                    "session_id": sid,
                    "when_busy": "reject",
                    "expected_epoch": session_epoch,
                    "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": outside_img}]
                }),
            )
            .await;
            assert_eq!(deny_outside["error"]["code"], codes::INVALID_PARAMS, "{deny_outside}");
            let deny_outside_msg = deny_outside["error"]["message"].as_str().unwrap_or("");
            assert!(
                deny_outside_msg.contains("outside.png")
                    || deny_outside_msg.contains("sandbox")
                    || deny_outside_msg.contains("read"),
                "expected sandbox read denial, got: {deny_outside_msg}"
            );

            // Control: permitted image in the same workspace succeeds under the same config
            let allow_start = call_rpc(
                &mut cw,
                &mut cr,
                3,
                "agent.prompt",
                json!({"session_id": sid, "input": [{"type": "text", "text": "hi"}, {"type": "image", "path": "allowed.png"}]}),
            )
            .await;
            assert!(allow_start.get("error").is_none(), "expected allowed.png to succeed: {allow_start}");
            let allow_ok = call_rpc(
                &mut cw,
                &mut cr,
                105,
                "agent.wait",
                json!({"session_id": sid, "turn_id": allow_start["result"]["turn_id"]}),
            )
            .await;
            assert!(allow_ok.get("error").is_none(), "expected allowed.png to settle: {allow_ok}");

            // Ordered content parts: text -> mention (path, name, mime) -> text
            let ordered_start = call_rpc(
                &mut cw,
                &mut cr,
                3,
                "agent.prompt",
                json!({
                    "session_id": sid,
                    "input": [
                        {"type": "text", "text": "before"},
                        {"type": "file", "path": "secret.txt", "name": "Notes", "mime": "text/markdown"},
                        {"type": "text", "text": "after"}
                    ]
                }),
            )
            .await;
            assert!(ordered_start.get("error").is_none(), "{ordered_start}");
            let ordered = call_rpc(
                &mut cw,
                &mut cr,
                106,
                "agent.wait",
                json!({"session_id": sid, "turn_id": ordered_start["result"]["turn_id"]}),
            )
            .await;
            assert!(ordered.get("error").is_none(), "{ordered}");
            assert!(!ordered.to_string().contains("top-secret-body"), "{ordered}");
            let msgs = ordered["result"]["messages"].as_array().unwrap();
            let last_user = msgs.iter().rev().find(|m| m["role"] == "user").unwrap();
            assert_eq!(
                last_user["content"],
                json!([
                    {"type": "text", "text": "before"},
                    {"type": "mention", "path": "secret.txt", "name": "Notes", "mime": "text/markdown"},
                    {"type": "text", "text": "after"}
                ]),
                "{last_user}"
            );

            drop(cr);
            drop(cw);
            server_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}

#[tokio::test]
async fn rpc_peer_source_propagated_across_transition_agent_spawn_steer_and_quoted_mention() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let ws = tempfile::tempdir().unwrap();
            std::fs::write(ws.path().join("secret.txt"), "top-secret-body").unwrap();
            std::fs::write(ws.path().join("my file.txt"), "quoted-secret-body").unwrap();

            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": {"allow": ["session.current", "session.transition", "subagent.start", "subagent.steer", "subagent.wait", "agent.prompt", "agent.wait"]}
                }),
                load_init: false,
                project_trusted: true,
                args: json!({"no_session": true}),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let sid = super::call_service(&host, "session", "info", vec![]).await.unwrap()["session_id"]
                .as_str()
                .unwrap()
                .to_string();

            let (client, server) = tokio::io::duplex(65536);
            let (sr, sw) = tokio::io::split(server);
            let peer_task = tokio::task::spawn_local(serve(host.clone(), sr, sw, false, std::future::pending()));
            let (cr, mut cw) = tokio::io::split(client);
            let mut cr = BufReader::new(cr);
            let _ = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;

            // 1. session.transition is internal Lua-only (`expose = "lua"`), not exposed over RPC:
            // peer must be rejected with METHOD_NOT_FOUND even when `peer.allow` includes `session.transition`.
            let trans = call_rpc(
                &mut cw,
                &mut cr,
                1,
                "session.transition",
                json!({
                    "spec": {
                        "mode": "new",
                        "initial": {
                            "kind": "input",
                            "source": "rpc",
                            "content": "@secret.txt please"
                        }
                    }
                }),
            )
            .await;
            assert_eq!(trans["error"]["code"], codes::METHOD_NOT_FOUND, "peer session.transition must be denied: {trans}");

            // 2. Direct peer text containing <peer-message> must NOT entity-decode @"my file.txt"
            // because `peer_message` encoding marker is only set by trusted inbound delivery.
            let quoted_start = call_rpc(
                &mut cw,
                &mut cr,
                2,
                "agent.prompt",
                json!({
                    "session_id": sid,
                    "input": "<peer-message from=\"@alice\">\ncheck @&quot;my file.txt&quot;\n</peer-message>\nand @\"a &amp; b.txt\""
                }),
            )
            .await;
            assert!(quoted_start.get("error").is_none(), "{quoted_start}");
            let quoted_res = call_rpc(
                &mut cw,
                &mut cr,
                102,
                "agent.wait",
                json!({
                    "session_id": sid,
                    "turn_id": quoted_start["result"]["turn_id"]
                }),
            )
            .await;
            assert!(quoted_res.get("error").is_none(), "{quoted_res}");
            let q_msgs = quoted_res["result"]["messages"].as_array().unwrap();
            let q_mentions: Vec<&str> = q_msgs[0]["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|b| b["type"] == "mention")
                .filter_map(|b| b["path"].as_str())
                .collect();
            assert!(!q_mentions.contains(&"my file.txt"), "direct peer prompt must not unescape <peer-message>: {q_mentions:?}");
            assert!(!serde_json::to_string(&q_msgs).unwrap().contains("quoted-secret-body"));

            // 3. subagent.start + subagent.steer from peer must propagate source = "peer" and consume the steered message
            #[cfg(unix)]
            {
                let gate_ws = tempfile::tempdir().unwrap();
                std::fs::write(gate_ws.path().join("secret.txt"), "top-secret-body").unwrap();
                let fifo_path = gate_ws.path().join("gate.fifo");
                let mkfifo_status = std::process::Command::new("mkfifo").arg(&fifo_path).status().unwrap();
                assert!(mkfifo_status.success(), "mkfifo failed: {mkfifo_status}");
                let gate_script = serde_json::to_string(&json!([
                    {
                        "tool_calls": [{
                            "id": "call-1",
                            "name": "bash",
                            "arguments": {
                                "command": "cat gate.fifo"
                            }
                        }]
                    },
                    {"text": "turn 2"}
                ]))
                .unwrap();
                let gate_host = Host::new(mare_host::HostOptions {
                    cwd: gate_ws.path().to_path_buf(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({
                        "model": "faux/echo",
                        "permissions": {"preset": "full-access"},
                        "peer": {"allow": ["subagent.start", "subagent.steer", "subagent.wait"]},
                        "plugins": {
                            "provider-faux": {
                                "script_json": gate_script
                            }
                        }
                    }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({"no_session": true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let (gc, gs) = tokio::io::duplex(65536);
                let (gsr, gsw) = tokio::io::split(gs);
                let g_task = tokio::task::spawn_local(serve(gate_host.clone(), gsr, gsw, false, std::future::pending()));
                let (gcr, mut gcw) = tokio::io::split(gc);
                let mut gcr = BufReader::new(gcr);
                let init = call_rpc(&mut gcw, &mut gcr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
                let parent_sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

                let fifo_open_path = fifo_path.clone();
                let fifo_writer_task = tokio::task::spawn_blocking(move || {
                    std::fs::OpenOptions::new().write(true).open(&fifo_open_path)
                });

                let spawn_res = call_rpc(&mut gcw, &mut gcr, 1, "subagent.start", json!({"session_id": parent_sid, "prompt": "child @secret.txt"})).await;
                assert!(spawn_res.get("error").is_none(), "{spawn_res}");
                let subagent_sid = spawn_res["result"]["subagent_id"].as_str().unwrap().to_string();

                let fifo_writer = match tokio::time::timeout(std::time::Duration::from_secs(10), fifo_writer_task).await {
                    Ok(Ok(Ok(f))) => f,
                    other => {
                        let wait_res = call_rpc(&mut gcw, &mut gcr, 3, "subagent.wait", json!({"session_id": parent_sid, "subagent_id": subagent_sid})).await;
                        panic!("steer-gate turn 1 did not open fifo: {other:?}; wait={wait_res}");
                    }
                };

                let steer_res = call_rpc(&mut gcw, &mut gcr, 2, "subagent.steer", json!({"session_id": parent_sid, "subagent_id": subagent_sid, "text": "steered @secret.txt"})).await;
                assert!(steer_res.get("error").is_none(), "steer failed: {steer_res}");
                assert_eq!(steer_res["result"], json!({}), "expected empty steer ack: {steer_res}");

                // Close the write end so `cat gate.fifo` receives EOF and turn 1 completes.
                drop(fifo_writer);

                let wait_res = call_rpc(&mut gcw, &mut gcr, 3, "subagent.wait", json!({"session_id": parent_sid, "subagent_id": subagent_sid})).await;
                assert!(wait_res.get("error").is_none(), "wait failed: {wait_res}");
                let wait_text = serde_json::to_string(&wait_res).unwrap();
                assert!(!wait_text.contains("top-secret-body"), "subagent.start/steer leaked secret.txt: {wait_text}");
                let child_msgs = wait_res["result"]["messages"].as_array().unwrap();
                assert!(
                    child_msgs.iter().any(|m| {
                        m["role"] == "user"
                            && m["source"] == "peer"
                            && serde_json::to_string(m).unwrap().contains("steered @secret.txt")
                    }),
                    "expected consumed steered message with source=peer: {wait_res}"
                );
                let spawn_denied_err =
                    call_rpc(&mut gcw, &mut gcr, 4, "subagent.cancel", json!({"session_id": parent_sid, "subagent_id": subagent_sid})).await;
                assert_eq!(
                    spawn_denied_err["error"]["data"]["code"], "denied",
                    "ungranted subagent.cancel must be denied: {spawn_denied_err}"
                );
                drop(gcr);
                drop(gcw);
                g_task.await.unwrap();
                gate_host.shutdown().await;

                // Peer with empty peer.allow is denied on subagent.start and subagent.cancel.
                let bare_host = Host::new(mare_host::HostOptions {
                    cwd: gate_ws.path().to_path_buf(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({ "model": "faux/echo", "permissions": {"preset": "full-access"} }),
                    load_init: false,
                    project_trusted: false,
                    args: json!({"no_session": true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let (bc, bs) = tokio::io::duplex(65536);
                let (bsr, bsw) = tokio::io::split(bs);
                let b_task = tokio::task::spawn_local(serve(bare_host.clone(), bsr, bsw, false, std::future::pending()));
                let (bcr, mut bcw) = tokio::io::split(bc);
                let mut bcr = BufReader::new(bcr);
                let binit = call_rpc(&mut bcw, &mut bcr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}})).await;
                let bare_sid = binit["result"]["session"]["session_id"].as_str().unwrap().to_string();
                for (req_id, method, params) in [
                    (1, "subagent.start", json!({"session_id": bare_sid, "prompt": "denied"})),
                    (2, "subagent.cancel", json!({"session_id": bare_sid, "subagent_id": "any"})),
                ] {
                    let denied = call_rpc(&mut bcw, &mut bcr, req_id, method, params).await;
                    assert_eq!(denied["error"]["data"]["code"], "denied", "{method} must fail with denied: {denied}");
                }
                drop(bcr);
                drop(bcw);
                b_task.await.unwrap();
                bare_host.shutdown().await;
            }

            drop(cr);
            drop(cw);
            peer_task.await.unwrap();
            host.shutdown().await;
        })
        .await;
}

/// schema-driven default-closed admission sweep across ALL
/// config options, ALL primary-only contract operations, and ALL protocol methods
/// (confirming `peer.allow = ["*"]` never opens owner-only items and ungranted peers only see
/// declared `PeerDefault` + `status.get`).
#[tokio::test]
async fn sf_default_closed_options_contracts_and_protocol_sweep() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            let host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({
                    "model": "faux/echo",
                    "peer": { "allow": ["*"] },
                }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();

            // Enumerate every owner-only option (top-level + plugin schema).
            let schemas = mare_shared::lua::core::config::plugin_schemas(host.lua())
                .unwrap()
                .map(mlua::Value::Table)
                .unwrap_or(mlua::Value::Nil);
            let owner_only_options: Vec<String> = serde_json::from_str(
                &host
                    .lua()
                    .load(
                        r#"
                        local plugin_schemas = ...
                        local out = {}
                        for k in pairs(mare.options or {}) do
                            if k ~= "effort" and k ~= "model" then
                                out[#out + 1] = k
                            end
                        end
                        for p, opts in pairs(plugin_schemas or {}) do
                            if type(opts) == "table" then
                                for k in pairs(opts) do
                                    out[#out + 1] = "plugins." .. p .. "." .. k
                                end
                            end
                        end
                        table.sort(out)
                        return mare.json.encode(out)
                        "#,
                    )
                    .call::<String>(schemas)
                    .unwrap(),
            )
            .unwrap();
            assert!(!owner_only_options.is_empty(), "expected owner-only options in schema");

            // 1. Primary connection collects every exported contract method to find all owner-only ops.
            let (oc, os) = tokio::io::duplex(65536);
            let (osr, osw) = tokio::io::split(os);
            let owner_task = tokio::task::spawn_local(serve(host.clone(), osr, osw, true, std::future::pending()));
            let (ocr, mut ocw) = tokio::io::split(oc);
            let mut ocr = BufReader::new(ocr);
            let _ =
                call_rpc(&mut ocw, &mut ocr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                    .await;
            let primary_services = call_rpc(&mut ocw, &mut ocr, 1, "services.list", json!({})).await;
            assert!(primary_services["error"].is_null(), "{primary_services}");
            let mut owner_ops = Vec::new();
            let mut peer_ops = Vec::new();
            let mut peer_default_ops = Vec::new();
            for svc in primary_services["result"]["data"].as_array().unwrap() {
                let sname = svc["name"].as_str().unwrap();
                for (mname, spec) in svc["methods"].as_object().unwrap() {
                    let full = format!("{sname}.{mname}");
                    match spec["access"].as_str() {
                        Some("peer_default") => peer_default_ops.push(full),
                        Some("peer") => peer_ops.push(full),
                        _ => owner_ops.push(full),
                    }
                }
            }
            assert!(!owner_ops.is_empty() && !peer_ops.is_empty() && !peer_default_ops.is_empty());

            // 2. Wildcard peer connection (`peer.allow = ["*"]`).
            let (pc, ps) = tokio::io::duplex(65536);
            let (psr, psw) = tokio::io::split(ps);
            let peer_task = tokio::task::spawn_local(serve(host.clone(), psr, psw, false, std::future::pending()));
            let (pcr, mut pcw) = tokio::io::split(pc);
            let mut pcr = BufReader::new(pcr);
            let p_init =
                call_rpc(&mut pcw, &mut pcr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                    .await;
            let p_methods: std::collections::BTreeSet<String> = p_init["result"]["capabilities"]["methods"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();

            let mut req_id = 1u64;
            // Every non-peer option fails closed at `options.set` (now MethodAccess::Primary -> METHOD_NOT_FOUND
            // even under `peer.allow = ["*"]`).
            for key in &owner_only_options {
                let resp = call_rpc(&mut pcw, &mut pcr, req_id, "options.set", json!({ "values": { key: "x" } })).await;
                req_id += 1;
                assert_eq!(
                    resp["error"]["code"],
                    codes::METHOD_NOT_FOUND,
                    "option `{key}` escaped primary-only gate: {resp}"
                );
            }

            // `peer_default` ops (`status.get`) are advertised even under `["*"]`; prefix wildcards
            // no longer exist so `["*"]` grants NO `peer` contract ops and NO `Peer` protocol methods.
            for op in &peer_default_ops {
                assert!(p_methods.contains(op), "peer_default op `{op}` must be advertised");
            }
            for op in &peer_ops {
                assert!(!p_methods.contains(op), "`*` wildcard must not grant peer op `{op}`");
                let denied = call_rpc(&mut pcw, &mut pcr, req_id, op, json!({})).await;
                req_id += 1;
                assert_eq!(
                    denied["error"]["data"]["code"], "denied",
                    "wildcard `*` must not grant peer op `{op}`: {denied}"
                );
            }

            // Every owner-only contract op must not appear in `p_methods` and must fail with `denied`.
            let peer_services = call_rpc(&mut pcw, &mut pcr, req_id, "services.list", json!({})).await;
            req_id += 1;
            assert_eq!(
                peer_services["error"]["code"],
                codes::METHOD_NOT_FOUND,
                "`*` must not grant `services.list` to a peer: {peer_services}"
            );
            for op in &owner_ops {
                assert!(!p_methods.contains(op), "owner op `{op}` advertised");
                let denied = call_rpc(&mut pcw, &mut pcr, req_id, op, json!({})).await;
                req_id += 1;
                assert_eq!(
                    denied["error"]["data"]["code"], "denied",
                    "wildcard peer must be denied owner op `{op}`: {denied}"
                );
            }

            // Every Primary protocol method must not be advertised and must fail with METHOD_NOT_FOUND.
            for (method, access) in mare_protocol::protocol_methods() {
                if *access == mare_protocol::MethodAccess::Primary {
                    assert!(!p_methods.contains(method), "primary protocol method `{method}` advertised");
                    let denied = call_rpc(&mut pcw, &mut pcr, req_id, method, json!({})).await;
                    req_id += 1;
                    assert_eq!(
                        denied["error"]["code"],
                        codes::METHOD_NOT_FOUND,
                        "wildcard peer must be denied primary method `{method}`: {denied}"
                    );
                }
            }

            drop(pcr);
            drop(pcw);
            peer_task.await.unwrap();
            drop(ocr);
            drop(ocw);
            owner_task.await.unwrap();
            host.shutdown().await;

            // 3. Ungranted peer (`peer.allow = []`): non-default protocol methods & non-default peer ops refused.
            let bare_host = Host::new(mare_host::HostOptions {
                cwd: ws.path().to_owned(),
                mode: mare_plugin::Mode::Print,
                config: json!({ "model": "faux/echo" }),
                load_init: false,
                project_trusted: true,
                args: json!({ "no_session": true }),
                no_user_plugins: true,
            })
            .await
            .unwrap();
            let (bc, bs) = tokio::io::duplex(65536);
            let (bsr, bsw) = tokio::io::split(bs);
            let b_task = tokio::task::spawn_local(serve(bare_host.clone(), bsr, bsw, false, std::future::pending()));
            let (bcr, mut bcw) = tokio::io::split(bc);
            let mut bcr = BufReader::new(bcr);
            let b_init =
                call_rpc(&mut bcw, &mut bcr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities": {}}))
                    .await;
            let b_methods: std::collections::BTreeSet<String> = b_init["result"]["capabilities"]["methods"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            let mut bare_id = 1u64;
            for (method, access) in mare_protocol::protocol_methods() {
                if *access != mare_protocol::MethodAccess::PeerDefault {
                    assert!(!b_methods.contains(method), "non-default method `{method}` advertised to ungranted peer");
                    let denied = call_rpc(&mut bcw, &mut bcr, bare_id, method, json!({})).await;
                    bare_id += 1;
                    assert_eq!(
                        denied["error"]["code"],
                        codes::METHOD_NOT_FOUND,
                        "ungranted peer must be denied `{method}`: {denied}"
                    );
                }
            }
            for op in &peer_ops {
                if op != "status.get" {
                    let denied = call_rpc(&mut bcw, &mut bcr, bare_id, op, json!({})).await;
                    bare_id += 1;
                    assert_eq!(
                        denied["error"]["data"]["code"], "denied",
                        "ungranted peer must be denied `{op}`: {denied}"
                    );
                }
            }
            drop(bcr);
            drop(bcw);
            b_task.await.unwrap();
            bare_host.shutdown().await;
        })
        .await;
}

/// An explicitly named operation still requires the capability that owns it;
/// a peer allow list cannot grant a primary-only method.
#[tokio::test]
async fn explicit_peer_allow_cannot_grant_primary_mutations() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let _host_guard = HOST_LOCK.lock().await;
            let ws = tempfile::tempdir().unwrap();
            for (method, params, contract) in [
                ("state.compare_exchange", json!({"namespace":"approval.probe","expected_revision":0,"value":{"n":1}}), false),
                ("services.register", json!({"name":"peer-probe","methods":{"forward":{"parameters":{"type":"object"},"result":{"type":"object"},"order":[]}}}), false),
                ("fs.write", json!({"path":"peer-probe.txt","data_base64":"d3JpdHRlbg=="}), true),
            ] {
                let host = Host::new(mare_host::HostOptions {
                    cwd: ws.path().to_owned(),
                    mode: mare_plugin::Mode::Print,
                    config: json!({"model":"faux/echo", "peer":{"allow":[method]}}),
                    load_init: false,
                    project_trusted: true,
                    args: json!({"no_session":true}),
                    no_user_plugins: true,
                })
                .await
                .unwrap();
                let (owner, owner_server) = tokio::io::duplex(65536);
                let (osr, osw) = tokio::io::split(owner_server);
                let owner_task = tokio::task::spawn_local(serve(host.clone(), osr, osw, true, std::future::pending()));
                let (ocr, mut ocw) = tokio::io::split(owner);
                let mut ocr = BufReader::new(ocr);
                let owner_init = call_rpc(&mut ocw, &mut ocr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities":{"services":true}})).await;
                let owner_methods = owner_init["result"]["capabilities"]["methods"].as_array().unwrap();
                assert!(owner_methods.iter().any(|m| m.as_str() == Some(method)), "{method} missing from owner: {owner_init}");
                let (client, server) = tokio::io::duplex(65536);
                let (sr, sw) = tokio::io::split(server);
                let task = tokio::task::spawn_local(serve(host.clone(), sr, sw, false, std::future::pending()));
                let (cr, mut cw) = tokio::io::split(client);
                let mut cr = BufReader::new(cr);
                let init = call_rpc(&mut cw, &mut cr, 0, "initialize", json!({"protocol_version": "3.0", "capabilities":{"services":true}})).await;
                let methods = init["result"]["capabilities"]["methods"].as_array().unwrap();
                assert!(!methods.iter().any(|m| m.as_str() == Some(method)), "{method} advertised: {init}");
                let denied = call_rpc(&mut cw, &mut cr, 1, method, params).await;
                if contract {
                    assert_eq!(denied["error"]["data"]["code"], "denied", "{method}: {denied}");
                } else {
                    assert_eq!(denied["error"]["code"], codes::METHOD_NOT_FOUND, "{method}: {denied}");
                }
                assert!(!ws.path().join("peer-probe.txt").exists(), "peer fs.write created a file");
                drop(cr);
                drop(cw);
                task.await.unwrap();
                drop(ocr);
                drop(ocw);
                owner_task.await.unwrap();
                host.shutdown().await;
            }
        })
        .await;
}
