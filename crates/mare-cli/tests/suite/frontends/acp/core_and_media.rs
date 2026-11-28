use super::*;

const PNG_1X1: [u8; 67] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49,
    0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00,
    0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

#[test]
fn acp_prompt_ends_turn_cancel_cancels_and_unknown_session_errors() {
    let (mut rpc, home) = acp(json!([
        { "text": "hello from mare" },
        { "text": "x".repeat(4000), "delay_ms": 20 }
    ]));
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["protocolVersion"], 1, "{init}");
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    // prompt → streamed message chunks → end_turn
    let id = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hi" }] }));
    let mut chunks = String::new();
    let res = loop {
        let m = rpc.read();
        if m["method"] == "session/update" && m["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            chunks.push_str(m["params"]["update"]["content"]["text"].as_str().unwrap_or(""));
        } else if m["id"] == id {
            break m;
        }
    };
    assert_eq!(res["result"]["stopReason"], "end_turn", "{res}");
    assert_eq!(chunks, "hello from mare");

    // a slow prompt is cancelled by session/cancel
    let id = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "long" }] }));
    loop {
        // wait until this prompt is streaming (late updates of the first prompt don't count)
        let m = rpc.read();
        if m["params"]["update"]["content"]["text"].as_str().is_some_and(|t| t.starts_with('x')) {
            break;
        }
    }
    rpc.notify("session/cancel", json!({ "sessionId": sid }));
    let res = rpc.wait_raw(id, |_| json!(null));
    assert_eq!(res["result"]["stopReason"], "cancelled", "{res}");

    // an unknown session is a JSON-RPC error, never end_turn
    let res = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": "nope", "prompt": [{ "type": "text", "text": "x" }] }),
        |_| json!(null),
    );
    assert!(res.get("error").is_some(), "{res}");
    rpc.kill_and_wait();
}

#[test]
fn acp_capabilities_authenticate_and_config_options() {
    let (mut rpc, home) = acp(json!([{ "text": "ok" }]));
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let caps = &init["result"]["agentCapabilities"];
    assert_eq!(caps["mcpCapabilities"]["http"], true, "{init}");
    assert_eq!(caps["promptCapabilities"]["image"], true, "{init}");
    assert_eq!(caps["promptCapabilities"]["embeddedContext"], true, "{init}");
    assert!(caps["sessionCapabilities"]["list"].is_object(), "{init}");
    assert!(caps["sessionCapabilities"]["resume"].is_object(), "{init}");
    assert!(caps["sessionCapabilities"]["close"].is_object(), "{init}");

    let auth = rpc.call_raw("authenticate", json!({ "methodId": "none" }), |_| json!(null));
    assert!(auth.get("result").is_some(), "{auth}");

    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();
    let opts = s["result"]["configOptions"].as_array().unwrap_or_else(|| panic!("{s}"));
    let model_opt = opts.iter().find(|o| o["id"] == "model").unwrap_or_else(|| panic!("{s}"));
    assert_eq!(model_opt["currentValue"], "faux/echo", "{s}");

    // set_config_option rejects unknown configId and unknown model
    let bad_id = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "nope", "value": "x" }),
        |_| json!(null),
    );
    assert_eq!(bad_id["error"]["code"], -32602, "{bad_id}");

    let bad_val = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "model", "value": "no/such-model" }),
        |_| json!(null),
    );
    assert_eq!(bad_val["error"]["code"], -32602, "{bad_val}");

    // valid set_config_option succeeds
    let ok_set = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "model", "value": "faux/echo" }),
        |_| json!(null),
    );
    assert!(ok_set["result"]["configOptions"].is_array(), "{ok_set}");

    rpc.kill_and_wait();
}

#[test]
fn acp_session_list_resume_and_close() {
    let (mut rpc, home) =
        acp_with_args(json!([{ "text": "first turn" }, { "text": "resumed turn" }]), &["--name", "named-acp"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    // Run one prompt so the session is persisted to disk
    let p1 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "hi" }] }),
        |_| json!(null),
    );
    assert_eq!(p1["result"]["stopReason"], "end_turn", "{p1}");

    // Active session is excluded from session/list
    let listed_active = rpc.call_raw("session/list", json!({ "cwd": cwd }), |_| json!(null));
    let arr = listed_active["result"]["sessions"].as_array().unwrap_or_else(|| panic!("{listed_active}"));
    assert!(!arr.iter().any(|e| e["sessionId"] == sid), "{listed_active}");

    // Resuming an already-active session fails with invalid params
    let dup_resume =
        rpc.call_raw("session/resume", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert_eq!(dup_resume["error"]["code"], -32602, "{dup_resume}");

    // Close the session
    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    // Now session/list includes the closed session with title from --name
    let listed = rpc.call_raw("session/list", json!({ "cwd": cwd }), |_| json!(null));
    let arr = listed["result"]["sessions"].as_array().unwrap_or_else(|| panic!("{listed}"));
    let entry = arr.iter().find(|e| e["sessionId"] == sid).unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(entry["title"], "named-acp", "{listed}");

    // Resume with mismatched cwd fails
    let wrong_cwd = std::env::temp_dir().join("nonexistent-acp-cwd");
    let bad_resume =
        rpc.call_raw("session/resume", json!({ "sessionId": sid, "cwd": wrong_cwd, "mcpServers": [] }), |_| {
            json!(null)
        });
    assert_eq!(bad_resume["error"]["code"], -32602, "{bad_resume}");

    // Resume with matching cwd succeeds and can run a prompt
    let resumed =
        rpc.call_raw("session/resume", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    assert!(resumed["result"]["configOptions"].is_array(), "{resumed}");
    let p2 = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "again" }] }),
        |_| json!(null),
    );
    assert_eq!(p2["result"]["stopReason"], "end_turn", "{p2}");

    rpc.kill_and_wait();
}

#[test]
fn acp_mcp_validation_and_df5_cli_flags() {
    // --strict-config with unparseable MARE_* env var fails startup with exit code 2
    let (mut bad_rpc, _) = acp_with_args(json!([]), &["--strict-config"], &[("MARE_EFFORT", "[bad")]);
    let status = bad_rpc.wait_exit_status().unwrap();
    assert_eq!(status.code(), Some(2));

    // -C, --trust, --approve-mcps work and session/new validates mcpServers & cwd
    let workdir = tempfile::tempdir().unwrap();
    let workdir_canon = dunce::canonicalize(workdir.path()).unwrap();
    let (mut rpc, _) = acp_with_args(
        json!([{ "text": "ok" }]),
        &["--trust", "--approve-mcps", "-C", workdir_canon.to_str().unwrap()],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));

    // Relative stdio command is rejected
    let bad_mcp = rpc.call_raw(
        "session/new",
        json!({
            "cwd": workdir_canon,
            "mcpServers": [{ "name": "rel", "command": "relative/bin", "args": [], "env": [] }]
        }),
        |_| json!(null),
    );
    assert_eq!(bad_mcp["error"]["code"], -32602, "{bad_mcp}");

    // Empty cwd falls back to -C workdir
    let ok = rpc.call_raw("session/new", json!({ "cwd": "", "mcpServers": [] }), |_| json!(null));
    assert!(ok["result"]["sessionId"].is_string(), "{ok}");

    rpc.kill_and_wait();
}

#[test]
fn acp_permission_request_forwards_and_resolves_tool_approval() {
    let (mut rpc, home) = acp_with_args(
        json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "acp-test.txt", "content": "ok" } }] },
            { "text": "ran tool" }
        ]),
        &["--permission", "read-only", "--approval-policy", "ask"],
        &[],
    );
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let mut saw_permission_req = false;
    let id = rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "run" }] }));
    let res = rpc.wait_raw(id, |req| {
        if req["method"] == "session/request_permission" {
            saw_permission_req = true;
            assert_eq!(req["params"]["sessionId"], sid, "{req}");
            json!({ "outcome": { "outcome": "selected", "optionId": "allow-once" } })
        } else {
            json!(null)
        }
    });
    assert!(saw_permission_req, "expected session/request_permission call");
    assert_eq!(res["result"]["stopReason"], "end_turn", "{res}");

    rpc.kill_and_wait();
}

#[test]
fn acp_prompt_preserves_interleaved_text_and_image_order() {
    let (mut rpc, home) = acp(json!([{ "text": "ok" }]));
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{s}")).to_string();

    let png_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, PNG_1X1);
    let res = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "before" },
                { "type": "image", "data": png_b64, "mimeType": "image/png" },
                { "type": "text", "text": "after" }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(res["result"]["stopReason"], "end_turn", "{res}");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");

    rpc.kill_and_wait();

    // Verify the persisted user message kept [text("before"), image, text("after")] in order.
    let mut session_files = Vec::new();
    let mut dirs = vec![home.path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("index.jsonl")
                {
                    session_files.push(p);
                }
            }
        }
    }
    assert_eq!(session_files.len(), 1, "expected 1 session file, got {session_files:?}");
    let raw = std::fs::read_to_string(&session_files[0]).unwrap();
    let user_msg = raw
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "message" && v["message"]["role"] == "user")
        .unwrap_or_else(|| panic!("missing user message in {raw}"));
    let content = user_msg["message"]["content"].as_array().unwrap_or_else(|| panic!("{user_msg}"));
    assert_eq!(content.len(), 3, "{user_msg}");
    assert_eq!(content[0], json!({ "type": "text", "text": "before" }), "{user_msg}");
    assert_eq!(content[1]["type"], "image", "{user_msg}");
    assert_eq!(content[1]["mime"], "image/png", "{user_msg}");
    assert!(content[1]["attachment"].is_string() || content[1]["data"] == png_b64, "{user_msg}");
    assert_eq!(content[2], json!({ "type": "text", "text": "after" }), "{user_msg}");
}

#[test]
fn acp_image_prompt_is_refused_when_the_current_model_has_no_image_input() {
    let home = tempfile::tempdir().unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("text-only-model.lua"),
        r#"
return {
    name = "text-only-model",
    inject = { "llm" },
    apply = function(ctx)
        ctx.llm:register_provider({
            provider = "text-only",
            no_key = true,
            models = { { model_id = "plain", name = "Plain", images = false } },
            stream = function()
                return { role = "assistant", content = { { type = "text", text = "ok" } }, stop_reason = "stop" }
            end,
        })
    end,
}
"#,
    )
    .unwrap();
    let script_path = home.path().join("faux.json");
    std::fs::write(&script_path, json!([{ "text": "seed response" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let session = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = session["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{session}")).to_string();

    let seed = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "seed" }] }),
        |_| json!(null),
    );
    assert_eq!(seed["result"]["stopReason"], "end_turn", "{seed}");
    let selected = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "model", "value": "text-only/plain" }),
        |_| json!(null),
    );
    let options = selected["result"]["configOptions"].as_array().unwrap_or_else(|| panic!("{selected}"));
    let model = options.iter().find(|option| option["id"] == "model").unwrap_or_else(|| panic!("{selected}"));
    assert_eq!(model["currentValue"], "text-only/plain", "{selected}");

    let mut session_files = Vec::new();
    let mut dirs = vec![home.path().to_path_buf()];
    while let Some(dir) = dirs.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl")
                    && p.file_name().and_then(|n| n.to_str()) != Some("index.jsonl")
                {
                    session_files.push(p);
                }
            }
        }
    }
    assert_eq!(session_files.len(), 1, "expected 1 session file, got {session_files:?}");
    let before = std::fs::read(&session_files[0]).unwrap();

    let png_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, PNG_1X1);
    let rejected = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "must not be saved" },
                { "type": "image", "data": png_b64, "mimeType": "image/png" }
            ]
        }),
        |_| json!(null),
    );
    let message = "the current model `text-only/plain` does not accept images; switch the model (config option `model`) or send text only";
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(rejected["error"]["data"], message, "{rejected}");
    assert_eq!(std::fs::read(&session_files[0]).unwrap(), before, "rejected prompt changed the session file");

    let text_only = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "text still works" }] }),
        |_| json!(null),
    );
    assert_eq!(text_only["result"]["stopReason"], "end_turn", "{text_only}");

    rpc.kill_and_wait();
}

#[test]
fn acp_audio_prompt_content_is_refused() {
    let (mut rpc, home) = acp(json!([{ "text": "ok" }]));
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let session = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = session["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{session}"));
    let rejected = rpc.call_raw(
        "session/prompt",
        json!({ "sessionId": sid, "prompt": [{ "type": "audio", "data": "AQID", "mimeType": "audio/wav" }] }),
        |_| json!(null),
    );
    assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
    assert_eq!(rejected["error"]["data"], "audio prompt content is not supported", "{rejected}");

    rpc.kill_and_wait();
}

#[test]
fn acp_image_prompt_returns_internal_when_get_model_fails() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config");
    let plugins_dir = config_dir.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(config_dir.join("init.lua"), "mare.plugin(\"llm\", { enabled = false })\n").unwrap();
    std::fs::write(
        plugins_dir.join("broken-llm.lua"),
        r#"return {
    name = "broken-llm",
    provide = { "llm" },
    apply = function(ctx)
        local S = require("cordis").class("BrokenLlm", mare.Service)
        function S:constructor(c)
            S.super.constructor(self, c, "llm")
        end
        local methods = {
            "list_models", "get_model", "set_model", "set_effort", "list_providers",
            "login", "logout", "register_provider", "register_providers", "provider_from_config",
            "provider_config", "login_io", "no_browser", "api_key_source",
            "refresh_models", "efforts", "providers", "provider", "models",
            "resolve", "current", "ready", "prepare", "get_selection",
            "restore_selection", "reset_session_model",
            "reset_session_effort", "effort", "raw_effort", "clamp_effort",
            "effort_options", "define_params", "param_list", "resolve_params",
            "apply_params", "set_param", "cycle_param", "explain_params",
            "explain_lines", "route_of", "register_wire", "wire",
            "describe_wire", "wire_owner", "remember_param", "note_param",
            "params_log", "subagent_params", "stream", "classify", "http_error",
            "close_response", "stream_error", "transform_messages", "fingerprint",
            "invalidate_api_key", "complete", "supports_remote_compact",
            "compact_remote", "text_of", "auth_get", "auth_set",
            "refresh_credential", "logged_in", "credential_sources",
            "active_credential", "status_lines", "identify_key_provider", "has_credentials", "alias_target",
            "model_default", "refresh_catalog", "api_key", "has_api_key",
            "has_auth", "auth_keys", "list_auth_keys", "active_scope", "global_effort", "scoped_effort",
            "recent", "set_active_scope", "clear_scope", "login_api_key",
            "base_url", "constrain_tools", "constrain_request", "readiness",
            "prepare_request", "request_ready",
        }
        for _, m in ipairs(methods) do
            S[m] = function() end
        end
        function S:models() return {} end
        function S:list_models() return {} end
        function S:effort_options() return {} end
        function S:recent() return {} end
        function S:get_model() error("get_model boom") end
        ctx:plugin(S)
    end,
}"#,
    )
    .unwrap();
    let script_path = home.path().join("faux.json");
    std::fs::write(&script_path, json!([{ "text": "ok" }]).to_string()).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--model", "faux/echo", "acp"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script_path)
        .stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let session = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = session["result"]["sessionId"].as_str().unwrap_or_else(|| panic!("{session}")).to_string();

    let png_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, PNG_1X1);
    let rejected = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{ "type": "image", "data": png_b64, "mimeType": "image/png" }]
        }),
        |_| json!(null),
    );
    assert_eq!(rejected["error"]["code"], -32603, "get_model lookup failure must be InternalError: {rejected}");

    rpc.kill_and_wait();
}

#[test]
fn acp_initialize_clamps_protocol_version_and_rejects_overlapping_prompts() {
    let (mut rpc, home) = acp(json!([
        { "text": "x".repeat(2000), "delay_ms": 30 },
        { "text": "second" }
    ]));
    let init = rpc.call_raw("initialize", json!({ "protocolVersion": 999, "clientCapabilities": {} }), |_| json!(null));
    assert_eq!(init["result"]["protocolVersion"], 1, "protocolVersion must clamp to latest supported: {init}");

    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let id1 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "slow" }] }));
    loop {
        let m = rpc.read();
        if m["method"] == "session/update" {
            break;
        }
    }
    let id2 =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "overlap" }] }));
    let overlap_res = rpc.wait_raw(id2, |_| json!(null));
    assert_eq!(overlap_res["error"]["code"], -32602, "overlapping prompt must fail InvalidParams: {overlap_res}");

    let first_res = rpc.wait_raw(id1, |_| json!(null));
    assert_eq!(first_res["result"]["stopReason"], "end_turn", "{first_res}");
    rpc.kill_and_wait();
}

#[test]
fn acp_text_resources_preserve_uri_and_image_blobs_match_top_level_images() {
    use base64::Engine as _;

    let png_b64 = base64::engine::general_purpose::STANDARD.encode(PNG_1X1);

    let home = tempfile::tempdir().unwrap();
    let reqs_dir = home.path().join("reqs");
    std::fs::create_dir_all(&reqs_dir).unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("record-media.lua"),
        format!(
            r#"local n = 0
return {{
    name = "record-media",
    inject = {{ "llm" }},
    apply = function(ctx)
        ctx.llm:register_provider({{
            provider = "rec",
            no_key = true,
            models = {{
                {{ model_id = "img", name = "Rec Img", images = true }},
                {{ model_id = "plain", name = "Rec Plain", images = false }},
            }},
            stream = function(req)
                n = n + 1
                local path = {:?} .. "/req-" .. tostring(n) .. ".json"
                mare.fs.write(path, mare.json.encode({{ model = req.model and req.model.ref, messages = req.messages }}))
                return {{ role = "assistant", content = {{ {{ type = "text", text = "ok" }} }}, stop_reason = "stop" }}
            end,
        }})
    end,
}}"#,
            reqs_dir.to_string_lossy()
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "rec/img", "acp"]).env("MARE_HOME", home.path()).stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    // 1. Two embedded text resources with the same basename keep distinct URIs + texts.
    let r1 = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "resource", "resource": { "uri": "file:///dir-a/notes.txt", "text": "alpha-body" } },
                { "type": "resource", "resource": { "uri": "file:///dir-b/notes.txt", "text": "beta-body" } }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");

    let req1: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-1.json")).unwrap()).unwrap();
    let msg1_str = req1["messages"].to_string();
    assert!(msg1_str.contains("file:///dir-a/notes.txt") && msg1_str.contains("alpha-body"), "{req1}");
    assert!(msg1_str.contains("file:///dir-b/notes.txt") && msg1_str.contains("beta-body"), "{req1}");

    // 2. Top-level image vs embedded PNG blob resource produce equivalent image blocks.
    let r2 = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "top" },
                { "type": "image", "data": png_b64, "mimeType": "image/png" }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");

    let r3 = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "blob" },
                { "type": "resource", "resource": { "uri": "file:///img/pic.png", "mimeType": "image/png", "blob": png_b64 } }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(r3["result"]["stopReason"], "end_turn", "{r3}");

    let req2: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-2.json")).unwrap()).unwrap();
    let req3: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-3.json")).unwrap()).unwrap();
    let img_block = |req: &Value| -> Value {
        req["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["source"] == "acp")
            .unwrap_or_else(|| panic!("missing acp user message in {req}"))["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "image")
            .cloned()
            .unwrap_or_else(|| panic!("missing image block in {req}"))
    };
    let b2 = img_block(&req2);
    let b3 = img_block(&req3);
    assert_eq!(b2["type"], b3["type"]);
    assert_eq!(b2["mime"], b3["mime"]);
    assert_eq!(b2["data"], b3["data"]);
    assert_eq!(b3["data"], png_b64);

    // 3. Unsupported binary blob still fails; embedded PNG blob goes through image_refusal when model has images=false.
    let bad_bin = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{ "type": "resource", "resource": { "uri": "file:///a.bin", "mimeType": "application/octet-stream", "blob": "AQID" } }]
        }),
        |_| json!(null),
    );
    assert_eq!(bad_bin["error"]["code"], -32602, "{bad_bin}");

    let switched = rpc.call_raw(
        "session/set_config_option",
        json!({ "sessionId": sid, "configId": "model", "value": "rec/plain" }),
        |_| json!(null),
    );
    assert!(switched.get("error").is_none(), "{switched}");
    let refused_blob = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{ "type": "resource", "resource": { "uri": "file:///img/pic.png", "mimeType": "image/png", "blob": png_b64 } }]
        }),
        |_| json!(null),
    );
    assert_eq!(refused_blob["error"]["code"], -32602, "{refused_blob}");
    assert!(refused_blob["error"]["data"].as_str().unwrap_or("").contains("does not accept images"), "{refused_blob}");

    rpc.kill_and_wait();
}

#[test]
fn acp_tool_results_forward_images_live_and_on_replay() {
    use base64::Engine as _;

    let png_b64 = base64::engine::general_purpose::STANDARD.encode(PNG_1X1);

    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let pic_path = ws.path().join("pic.png");
    std::fs::write(&pic_path, PNG_1X1).unwrap();

    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("img-only-tool.lua"),
        format!(
            r#"return {{
    name = "img-only-tool",
    inject = {{ "tools" }},
    apply = function(ctx)
        ctx.tools:register("img_only", {{
            description = "returns image only",
            side_effect = "read",
            parameters = {{ type = "object", additionalProperties = false }},
            execute = function()
                return {{ content = {{ {{ type = "image", mime = "image/png", data = "{png_b64}" }} }} }}
            end,
        }})
    end,
}}"#
        ),
    )
    .unwrap();

    let script = json!([
        { "tool_calls": [{ "name": "read", "arguments": { "path": "pic.png" } }] },
        { "tool_calls": [{ "name": "img_only", "arguments": {} }] },
        { "text": "done" }
    ]);
    let mut rpc = acp_in_home(home.path(), script, &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let p_id =
        rpc.request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "read pic" }] }));
    let (p_resp, live_updates) = read_until(&mut rpc, p_id, Some(&sid));
    assert_eq!(p_resp["result"]["stopReason"], "end_turn", "{p_resp}");

    let assert_has_tool_images = |updates: &[Value], phase: &str| {
        let tool_updates: Vec<&Value> = updates.iter().filter(|u| u["sessionUpdate"] == "tool_call_update").collect();
        assert_eq!(tool_updates.len(), 2, "{phase}: expected 2 tool_call_update items: {updates:?}");

        // First tool (read): text + image content block with valid PNG bytes.
        let read_content =
            tool_updates[0]["content"].as_array().unwrap_or_else(|| panic!("{phase}: {:?}", tool_updates[0]));
        assert!(
            read_content.iter().any(|c| c["content"]["type"] == "text"
                && c["content"]["text"].as_str().is_some_and(|t| t.contains("Image pic.png"))),
            "{phase}: missing read text block: {read_content:?}"
        );
        let read_img = read_content
            .iter()
            .find(|c| c["content"]["type"] == "image")
            .unwrap_or_else(|| panic!("{phase}: missing read image block: {read_content:?}"));
        assert_eq!(read_img["content"]["mimeType"], "image/png");
        let read_bytes =
            base64::engine::general_purpose::STANDARD.decode(read_img["content"]["data"].as_str().unwrap()).unwrap();
        let decoded = image::load_from_memory(&read_bytes).expect("decoded read PNG must be valid");
        assert_eq!((decoded.width(), decoded.height()), (1, 1), "{phase}: unexpected image dims");

        // Second tool (img_only): image only, no empty text block.
        let only_content =
            tool_updates[1]["content"].as_array().unwrap_or_else(|| panic!("{phase}: {:?}", tool_updates[1]));
        assert_eq!(only_content.len(), 1, "{phase}: image-only tool must not emit empty text: {only_content:?}");
        assert_eq!(only_content[0]["content"]["type"], "image");
        assert_eq!(only_content[0]["content"]["mimeType"], "image/png");
        let only_bytes = base64::engine::general_purpose::STANDARD
            .decode(only_content[0]["content"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(only_bytes, PNG_1X1, "{phase}: image-only bytes mismatch");
    };

    assert_has_tool_images(&live_updates, "live");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");
    rpc.kill_and_wait();

    // Restart ACP process and replay via session/load.
    let mut rpc2 = acp_in_home(home.path(), json!([]), &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let load_id = rpc2.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
    let (load_resp, replay_updates) = read_until(&mut rpc2, load_id, Some(&sid));
    assert!(load_resp.get("error").is_none(), "{load_resp}");
    assert_has_tool_images(&replay_updates, "replay");
    rpc2.kill_and_wait();
}

#[test]
fn acp_tool_results_forward_large_truncated_images_live_and_on_replay() {
    use base64::Engine as _;
    use std::io::Cursor;

    // Build an incompressible >800 KiB PNG (<= 1.5 MiB so `read` preserves PNG, > 1 MiB once
    // base64-encoded so the kernel wire tap truncates inline `data` on `item/completed`).
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let img = image::RgbImage::from_fn(620, 620, |_, _| {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let r = (state >> 16) as u8;
        let g = (state >> 24) as u8;
        let b = (state >> 32) as u8;
        image::Rgb([r, g, b])
    });
    let mut large_png = Vec::new();
    img.write_to(&mut Cursor::new(&mut large_png), image::ImageFormat::Png).unwrap();
    assert!(
        large_png.len() > 850_000 && large_png.len() <= 1_500_000,
        "expected PNG between 850 KiB and 1.5 MiB, got {} bytes",
        large_png.len()
    );

    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let pic_path = ws.path().join("large.png");
    std::fs::write(&pic_path, &large_png).unwrap();

    let script = json!([
        { "tool_calls": [{ "name": "read", "arguments": { "path": pic_path.to_string_lossy() } }] },
        { "text": "large-done" }
    ]);
    let mut rpc = acp_in_home(home.path(), script, &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(ws.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let p_id = rpc
        .request("session/prompt", json!({ "sessionId": sid, "prompt": [{ "type": "text", "text": "read large" }] }));
    let (p_resp, live_updates) = read_until(&mut rpc, p_id, Some(&sid));
    assert_eq!(p_resp["result"]["stopReason"], "end_turn", "{p_resp}");

    let extract_large_img = |updates: &[Value], phase: &str| -> Vec<u8> {
        let tu = updates
            .iter()
            .find(|u| u["sessionUpdate"] == "tool_call_update")
            .unwrap_or_else(|| panic!("{phase}: missing tool_call_update: {updates:?}"));
        let content = tu["content"].as_array().unwrap_or_else(|| panic!("{phase}: {tu}"));
        let img_block = content
            .iter()
            .find(|c| c["content"]["type"] == "image")
            .unwrap_or_else(|| panic!("{phase}: missing large image block: {content:?}"));
        assert_eq!(img_block["content"]["mimeType"], "image/png");
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(img_block["content"]["data"].as_str().unwrap()).unwrap();
        let decoded = image::load_from_memory(&bytes).expect("decoded large PNG must be valid");
        assert_eq!((decoded.width(), decoded.height()), (620, 620), "{phase}: unexpected dims");
        bytes
    };

    let live_bytes = extract_large_img(&live_updates, "live");

    let closed = rpc.call_raw("session/close", json!({ "sessionId": sid }), |_| json!(null));
    assert!(closed.get("result").is_some(), "{closed}");
    rpc.kill_and_wait();

    let mut rpc2 = acp_in_home(home.path(), json!([]), &["--permission", "auto", "--approval-policy", "never"], &[]);
    let _ = rpc2.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let load_id = rpc2.request("session/load", json!({ "sessionId": sid, "cwd": cwd, "mcpServers": [] }));
    let (load_resp, replay_updates) = read_until(&mut rpc2, load_id, Some(&sid));
    assert!(load_resp.get("error").is_none(), "{load_resp}");
    let replay_bytes = extract_large_img(&replay_updates, "replay");
    assert_eq!(live_bytes, replay_bytes, "live and replayed >1 MiB PNG bytes must match");
    rpc2.kill_and_wait();
}

#[test]
fn acp_validates_image_mime_types_for_mislabelled_corrupt_and_valid_images() {
    use base64::Engine as _;

    let png_b64 = base64::engine::general_purpose::STANDARD.encode(PNG_1X1);
    let mut jpg_bytes = Vec::new();
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 1, image::Rgb([12, 34, 56])))
        .write_to(&mut std::io::Cursor::new(&mut jpg_bytes), image::ImageFormat::Jpeg)
        .unwrap();
    let jpg_b64 = base64::engine::general_purpose::STANDARD.encode(&jpg_bytes);

    let home = tempfile::tempdir().unwrap();
    let reqs_dir = home.path().join("reqs");
    std::fs::create_dir_all(&reqs_dir).unwrap();
    let plugins_dir = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();
    std::fs::write(
        plugins_dir.join("record-mime93.lua"),
        format!(
            r#"local n = 0
return {{
    name = "record-mime93",
    inject = {{ "llm" }},
    apply = function(ctx)
        ctx.llm:register_provider({{
            provider = "rec93",
            no_key = true,
            models = {{
                {{ model_id = "img", name = "Rec93 Img", images = true }},
            }},
            stream = function(req)
                n = n + 1
                local path = {:?} .. "/req-" .. tostring(n) .. ".json"
                mare.fs.write(path, mare.json.encode({{ messages = req.messages }}))
                return {{ role = "assistant", content = {{ {{ type = "text", text = "ok" }} }}, stop_reason = "stop" }}
            end,
        }})
    end,
}}"#,
            reqs_dir.to_string_lossy()
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--model", "rec93/img", "acp"]).env("MARE_HOME", home.path()).stderr(Stdio::piped());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let _ = rpc.call_raw("initialize", json!({ "protocolVersion": 1, "clientCapabilities": {} }), |_| json!(null));
    let cwd = dunce::canonicalize(home.path()).unwrap();
    let s = rpc.call_raw("session/new", json!({ "cwd": cwd, "mcpServers": [] }), |_| json!(null));
    let sid = s["result"]["sessionId"].as_str().unwrap().to_string();

    let img_block = |req: &Value| -> Value {
        req["messages"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|m| m["source"] == "acp")
            .unwrap_or_else(|| panic!("missing acp user message in {req}"))["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "image")
            .cloned()
            .unwrap_or_else(|| panic!("missing image block in {req}"))
    };

    // 1. Mislabelled MIME (JPEG bytes declared as image/png) -> provider receives image/jpeg.
    let r1 = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "mislabelled top" },
                { "type": "image", "data": jpg_b64, "mimeType": "image/png" }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(r1["result"]["stopReason"], "end_turn", "{r1}");
    let req1: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-1.json")).unwrap()).unwrap();
    assert_eq!(img_block(&req1)["mime"], "image/jpeg", "{req1}");

    // 2. Mislabelled embedded blob resource -> provider receives image/jpeg.
    let r2 = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "mislabelled blob" },
                { "type": "resource", "resource": { "uri": "file:///photo.png", "mimeType": "image/png; charset=binary", "blob": jpg_b64 } }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(r2["result"]["stopReason"], "end_turn", "{r2}");
    let req2: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-2.json")).unwrap()).unwrap();
    assert_eq!(img_block(&req2)["mime"], "image/jpeg", "{req2}");

    // 2b. Embedded image blob declared with an unsupported image/* MIME (image/bmp) whose bytes
    // are actually a valid PNG -> validated by magic bytes and corrected to image/png, matching
    // direct ContentBlock::Image; genuine unsupported BMP bytes are rejected with -32602.
    let r2b = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "mislabelled bmp blob" },
                { "type": "resource", "resource": { "uri": "file:///photo.bmp", "mimeType": "image/bmp", "blob": png_b64 } }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(r2b["result"]["stopReason"], "end_turn", "{r2b}");
    let req2b: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-3.json")).unwrap()).unwrap();
    assert_eq!(img_block(&req2b)["mime"], "image/png", "{req2b}");

    let bmp_rejected = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "real bmp blob" },
                { "type": "resource", "resource": { "uri": "file:///real.bmp", "mimeType": "image/bmp", "blob": "Qk06AAAAAAAAADYAAAAoAAAAAQAAAAEAAAABABgAAAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" } }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(bmp_rejected["error"]["code"], -32602, "unsupported BMP bytes must fail with -32602: {bmp_rejected}");

    // 3. Corrupt image data -> rejected at entry point (-32602), never hits provider or poisons history.
    for bad in ["AQID", "iVBORw0KGgo=", "!!!not-base64!!!"] {
        let rejected = rpc.call_raw(
            "session/prompt",
            json!({
                "sessionId": sid,
                "prompt": [
                    { "type": "text", "text": "corrupt prompt" },
                    { "type": "image", "data": bad, "mimeType": "image/png" }
                ]
            }),
            |_| json!(null),
        );
        assert_eq!(rejected["error"]["code"], -32602, "corrupt payload {bad:?} must fail with -32602: {rejected}");
    }
    assert!(!reqs_dir.join("req-4.json").exists(), "corrupt prompts must never invoke provider");

    // 4. Correctly labelled PNG -> succeeds and provider sees image/png with clean history.
    let r4 = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "valid png" },
                { "type": "image", "data": png_b64, "mimeType": "image/png" }
            ]
        }),
        |_| json!(null),
    );
    assert_eq!(r4["result"]["stopReason"], "end_turn", "{r4}");
    let req4: Value = serde_json::from_str(&std::fs::read_to_string(reqs_dir.join("req-4.json")).unwrap()).unwrap();
    assert_eq!(img_block(&req4)["mime"], "image/png", "{req4}");
    assert!(!req4.to_string().contains("corrupt prompt"), "rejected corrupt prompt must not enter history: {req4}");
    assert!(!req4.to_string().contains("real bmp blob"), "rejected BMP blob must not enter history: {req4}");

    rpc.kill_and_wait();
}
