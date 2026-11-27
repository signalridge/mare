use super::*;

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

    let res = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "before" },
                { "type": "image", "data": "AQID", "mimeType": "image/png" },
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
    assert!(content[1]["attachment"].is_string() || content[1]["data"] == "AQID", "{user_msg}");
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

    let rejected = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "must not be saved" },
                { "type": "image", "data": "AQID", "mimeType": "image/png" }
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

    let rejected = rpc.call_raw(
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{ "type": "image", "data": "AQID", "mimeType": "image/png" }]
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
