use super::*;

#[test]
fn rpc_options_get_redacts_secrets_and_describes_metadata() {
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join("config");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let init_lua = r#"
mare.setup({
    border = "single",
    editor = "sk-FAKE_RAW_TOKEN_1234567890abcdef",
    modle = "typo-triggers-warning",
})
mare.provider("example", {
    api_key = "FAKE_KEY_SENTINEL",
    headers = { ["X-Custom"] = "FAKE_HEADER_SENTINEL" },
    query_params = { routing = "FAKE_QUERY_PARAMS_SENTINEL" },
    query = { tok = "FAKE_QUERY_SENTINEL" },
    base_url = "https://example.invalid/?routing=FAKE_URL_SENTINEL",
    proxy = "http://FAKE_PROXY_USER_SENTINEL:pass@proxy.invalid:8080",
    retries = 2,
})
mare.provider("clean", {
    base_url = "https://clean.example.invalid/v1",
    connect_timeout_ms = 5000,
})
mare.plugin("probe", {
    private_value = "FAKE_PLUGIN_SENTINEL",
    hidden = "FAKE_OPTION_SENSITIVE_SENTINEL",
    public_value = "ok_val",
    nested = {
        headers = { Authorization = "Bearer FAKE_NESTED_HDR_SENTINEL" },
        query = { auth = "FAKE_NESTED_QUERY_SENTINEL" },
        credentials = { client_secret = "FAKE_NESTED_CRED_SENTINEL" },
        auth_tokens = { github = "FAKE_AUTH_TOKENS_SENTINEL" },
        max_tokens = 4096,
    },
})
mare.options["plugins.probe.hidden"] = { type = "string", sensitive = true }
mare.config_sensitive("plugins.probe.private_value")
"#;
    std::fs::write(cfg_dir.join("init.lua"), init_lua).unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0" }), |_| panic!("unexpected callback"));
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    let methods: Vec<&str> =
        init["result"]["capabilities"]["methods"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert!(methods.contains(&"options.get") && methods.contains(&"config.describe"), "{init}");

    let sentinels = [
        "FAKE_RAW_TOKEN_1234567890abcdef",
        "FAKE_KEY_SENTINEL",
        "FAKE_HEADER_SENTINEL",
        "FAKE_QUERY_PARAMS_SENTINEL",
        "FAKE_QUERY_SENTINEL",
        "FAKE_URL_SENTINEL",
        "FAKE_PROXY_USER_SENTINEL",
        "FAKE_PLUGIN_SENTINEL",
        "FAKE_OPTION_SENSITIVE_SENTINEL",
        "FAKE_NESTED_HDR_SENTINEL",
        "FAKE_NESTED_QUERY_SENTINEL",
        "FAKE_NESTED_CRED_SENTINEL",
        "FAKE_AUTH_TOKENS_SENTINEL",
    ];
    let assert_no_leaks = |label: &str, v: &Value| {
        let dumped = v.to_string();
        for s in sentinels {
            assert!(!dumped.contains(s), "{label}: {s} leaked in {dumped}");
        }
    };

    // 1. Empty keys ({session_id} and {"session_id", "keys": []}) redact every sensitive field while keeping normal shapes.
    for (label, params) in
        [("omitted keys", json!({ "session_id": sid })), ("empty keys list", json!({ "session_id": sid, "keys": [] }))]
    {
        let res = rpc.call_raw("options.get", params, |_| panic!("unexpected callback"));
        assert!(res.get("error").is_none(), "{label}: {res}");
        assert_no_leaks(label, &res);
        let r = &res["result"];
        assert_eq!(r["border"], "single", "{r}");
        assert_eq!(r["model"], "faux/echo", "{r}");
        assert_eq!(r["editor"], "[REDACTED]", "{r}");
        assert_eq!(r["providers"]["example"]["api_key"], "[REDACTED]", "{r}");
        assert_eq!(r["providers"]["example"]["headers"], "[REDACTED]", "{r}");
        assert_eq!(r["providers"]["example"]["query_params"], "[REDACTED]", "{r}");
        assert_eq!(r["providers"]["example"]["query"], "[REDACTED]", "{r}");
        assert_eq!(r["providers"]["example"]["base_url"], "[REDACTED]", "{r}");
        assert_eq!(r["providers"]["example"]["proxy"], "[REDACTED]", "{r}");
        assert_eq!(r["providers"]["example"]["retries"], 2, "{r}");
        assert_eq!(r["providers"]["clean"]["base_url"], "https://clean.example.invalid/v1", "{r}");
        assert_eq!(r["plugins"]["probe"]["private_value"], "[REDACTED]", "{r}");
        assert_eq!(r["plugins"]["probe"]["hidden"], "[REDACTED]", "{r}");
        assert_eq!(r["plugins"]["probe"]["public_value"], "ok_val", "{r}");
        assert_eq!(r["plugins"]["probe"]["nested"]["headers"], "[REDACTED]", "{r}");
        assert_eq!(r["plugins"]["probe"]["nested"]["query"], "[REDACTED]", "{r}");
        assert_eq!(r["plugins"]["probe"]["nested"]["credentials"], "[REDACTED]", "{r}");
        assert_eq!(r["plugins"]["probe"]["nested"]["auth_tokens"], "[REDACTED]", "{r}");
        assert_eq!(r["plugins"]["probe"]["nested"]["max_tokens"], 4096, "{r}");
    }

    // 2. keys = ["providers", "plugins.probe"] must redact keys/headers/query/credentials/auth_tokens/sensitive subpaths.
    let prov =
        rpc.call_raw("options.get", json!({ "session_id": sid, "keys": ["providers", "plugins.probe"] }), |_| {
            panic!("unexpected callback")
        });
    assert_no_leaks("keys=[providers, plugins.probe]", &prov);
    assert_eq!(prov["result"]["providers"]["example"]["api_key"], "[REDACTED]");
    assert_eq!(prov["result"]["providers"]["example"]["headers"], "[REDACTED]");
    assert_eq!(prov["result"]["providers"]["example"]["query"], "[REDACTED]");
    assert_eq!(prov["result"]["providers"]["clean"]["connect_timeout_ms"], 5000);
    assert_eq!(prov["result"]["plugins.probe"]["hidden"], "[REDACTED]");
    assert_eq!(prov["result"]["plugins.probe"]["nested"]["auth_tokens"], "[REDACTED]");
    assert_eq!(prov["result"]["plugins.probe"]["nested"]["max_tokens"], 4096);

    // 3. Direct dotted paths (custom config_sensitive, options[path].sensitive, auth_tokens, and descendants).
    let dotted = rpc.call_raw(
        "options.get",
        json!({
            "session_id": sid,
            "keys": [
                "plugins.probe.private_value",
                "plugins.probe.hidden",
                "plugins.probe.public_value",
                "providers.example.headers",
                "providers.example.headers.X-Custom",
                "providers.example.query.tok",
                "plugins.probe.nested.credentials",
                "plugins.probe.nested.auth_tokens",
                "plugins.probe.nested.auth_tokens.github",
                "plugins.probe.nested.max_tokens"
            ]
        }),
        |_| panic!("unexpected callback"),
    );
    assert_no_leaks("dotted keys", &dotted);
    assert_eq!(dotted["result"]["plugins.probe.private_value"], "[REDACTED]");
    assert_eq!(dotted["result"]["plugins.probe.hidden"], "[REDACTED]");
    assert_eq!(dotted["result"]["plugins.probe.public_value"], "ok_val");
    assert_eq!(dotted["result"]["providers.example.headers"], "[REDACTED]");
    assert_eq!(dotted["result"]["providers.example.headers.X-Custom"], "[REDACTED]");
    assert_eq!(dotted["result"]["providers.example.query.tok"], "[REDACTED]");
    assert_eq!(dotted["result"]["plugins.probe.nested.credentials"], "[REDACTED]");
    assert_eq!(dotted["result"]["plugins.probe.nested.auth_tokens"], "[REDACTED]");
    assert_eq!(dotted["result"]["plugins.probe.nested.auth_tokens.github"], "[REDACTED]");
    assert_eq!(dotted["result"]["plugins.probe.nested.max_tokens"], 4096);

    // 4. Describe mode on options.get: effective/default/source/description/redacted.
    let desc = rpc.call_raw(
        "options.get",
        json!({
            "session_id": sid,
            "keys": ["border", "flavor", "providers", "plugins.probe.private_value", "plugins.probe.public_value"],
            "describe": true
        }),
        |_| panic!("unexpected callback"),
    );
    assert_no_leaks("options.get describe", &desc);
    let dr = &desc["result"];
    assert_eq!(dr["border"]["effective"], "single");
    assert_eq!(dr["border"]["default"], "rounded");
    assert_eq!(dr["border"]["source"]["layer"], "user");
    assert!(dr["border"]["source"]["where"].as_str().unwrap().contains("init.lua"), "{dr}");
    assert_eq!(dr["border"]["type"], "string");
    assert_eq!(dr["border"]["redacted"], false);
    assert!(dr["border"]["description"].as_str().unwrap().contains("borders"));

    assert_eq!(dr["flavor"]["effective"], "auto");
    assert_eq!(dr["flavor"]["default"], "auto");
    assert_eq!(dr["flavor"]["source"]["layer"], "default");
    assert_eq!(dr["flavor"]["redacted"], false);

    assert_eq!(dr["providers"]["redacted"], true);
    assert_eq!(dr["providers"]["effective"]["example"]["api_key"], "[REDACTED]");
    assert_eq!(dr["providers"]["effective"]["clean"]["base_url"], "https://clean.example.invalid/v1");

    assert_eq!(dr["plugins.probe.private_value"]["effective"], "[REDACTED]");
    assert_eq!(dr["plugins.probe.private_value"]["sensitive"], true);
    assert_eq!(dr["plugins.probe.private_value"]["redacted"], true);
    assert_eq!(dr["plugins.probe.public_value"]["effective"], "ok_val");
    assert_eq!(dr["plugins.probe.public_value"]["sensitive"], false);
    assert_eq!(dr["plugins.probe.public_value"]["redacted"], false);

    // 5. Dedicated config.describe returns options + warnings with the same redaction.
    let cfg_desc = rpc.call_raw(
        "config.describe",
        json!({ "keys": ["border", "providers", "plugins.probe.private_value"] }),
        |_| panic!("unexpected callback"),
    );
    assert_no_leaks("config.describe", &cfg_desc);
    assert_eq!(cfg_desc["result"]["options"]["border"]["effective"], "single");
    assert_eq!(cfg_desc["result"]["options"]["providers"]["redacted"], true);
    assert_eq!(cfg_desc["result"]["options"]["plugins.probe.private_value"]["effective"], "[REDACTED]");
    let warnings = cfg_desc["result"]["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|w| w.as_str().unwrap_or("").contains("modle")), "{cfg_desc}");

    rpc.shutdown_and_wait();
}

#[test]
fn rpc_initialize_reports_no_media_capabilities() {
    let home = tempfile::tempdir().unwrap();

    let schema_out = mare_cmd().args(["rpc", "schema"]).env("MARE_HOME", home.path()).run_output();
    assert!(schema_out.status.success(), "{}", String::from_utf8_lossy(&schema_out.stderr));
    let schema: Value = serde_json::from_slice(&schema_out.stdout).unwrap();
    for m in ["realtime.start", "realtime.append", "realtime.stop"] {
        assert!(schema["methods"].get(m).is_none(), "schema must not include {m}");
    }
    assert!(schema["notifications"].get("realtime.audio_delta").is_none());
    let server_caps = &schema["methods"]["initialize"]["result"]["$defs"]["ServerCapabilities"]["properties"];
    for field in ["video", "realtime", "media"] {
        assert!(server_caps.get(field).is_none(), "ServerCapabilities must not include {field}");
    }

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", home.path().join("unused.json"))
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0" }), |_| json!(null));
    let caps = &init["result"]["capabilities"];
    for field in ["video", "realtime", "media"] {
        assert!(caps.get(field).is_none(), "initialize capabilities must not include {field}: {init}");
    }
    let methods = caps["methods"].as_array().unwrap();
    for m in ["realtime.start", "realtime.append", "realtime.stop"] {
        assert!(!methods.iter().any(|v| v == m), "methods must not include {m}: {init}");
    }

    for (method, params) in [
        ("realtime.start", json!({ "outputModality": "audio", "voice": "alloy" })),
        ("realtime.append", json!({ "sessionId": "rt-1", "text": "hi" })),
        ("realtime.stop", json!({ "sessionId": "rt-1" })),
    ] {
        let res = rpc.call_raw(method, params, |_| json!(null));
        assert_eq!(res["error"]["code"], -32601, "{method}: {res}");
        assert_eq!(res["error"]["data"]["code"], "method_not_found", "{method}: {res}");
    }

    rpc.shutdown_and_wait();
}

#[test]
fn rpc_audio_and_video_input_is_refused_on_every_entry_and_form() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(&script, json!([{ "text": "still alive" }]).to_string()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0" }), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

    let cases = [
        // audio forms via ContentPart[]
        (
            "audio",
            "content",
            "kind",
            json!({ "session_id": sid, "input": [{ "type": "audio", "mime": "audio/wav", "data": "AAAA" }] }),
        ),
        (
            "audio",
            "content",
            "extension",
            json!({ "session_id": sid, "input": [{ "type": "file", "path": "clip.mp3" }] }),
        ),
        (
            "audio",
            "content",
            "mime",
            json!({ "session_id": sid, "input": [{ "type": "file", "path": "blob.bin", "mime": "audio/wav" }] }),
        ),
        (
            "audio",
            "content",
            "base64",
            json!({ "session_id": sid, "input": [{ "type": "image", "data": "AAAA", "mime": "audio/ogg" }] }),
        ),
        // video forms via ContentPart[]
        (
            "video",
            "content",
            "kind",
            json!({ "session_id": sid, "input": [{ "type": "video", "mime": "video/mp4", "data": "AAAA" }] }),
        ),
        (
            "video",
            "content",
            "extension",
            json!({ "session_id": sid, "input": [{ "type": "file", "path": "clip.mp4" }] }),
        ),
        (
            "video",
            "content",
            "mime",
            json!({ "session_id": sid, "input": [{ "type": "file", "path": "blob.bin", "mime": "video/mp4" }] }),
        ),
        (
            "video",
            "content",
            "base64",
            json!({ "session_id": sid, "input": [{ "type": "image", "data": "AAAA", "mime": "video/mp4" }] }),
        ),
    ];

    let mut failures = Vec::new();
    for (capability, entry, form, payload) in cases {
        let res = rpc.call_raw("agent.prompt", payload, |_| json!(null));
        let msg = res["error"]["message"].as_str().unwrap_or("");
        let ok = res.get("result").is_none()
            && res["error"]["code"] == -32000
            && res["error"]["data"]["code"] == "unsupported"
            && res["error"]["data"]["data"]["capability"] == capability
            && msg.to_lowercase().contains("unsupported")
            && msg.to_lowercase().contains(capability)
            && !msg.to_lowercase().contains("realtime");
        if !ok {
            failures.push(format!("{capability}/{entry}/{form}: {res}"));
        }
    }
    assert!(failures.is_empty(), "refusal failures:\n{}", failures.join("\n"));

    let sid = rpc.call_raw("status.get", json!({}), |_| json!(null))["result"]["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let msgs = rpc.call_raw("agent.list_messages", json!({ "session_id": sid }), |_| json!(null));
    assert_eq!(msgs["result"]["data"], json!([]), "refused media prompts must not queue or record messages: {msgs}");

    let ok_start = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "hello" }), |_| json!(null));
    assert!(ok_start.get("error").is_none(), "{ok_start}");
    let ok = rpc.call_raw("agent.wait", json!({ "session_id": sid, "turn_id": ok_start["result"]["turn_id"] }), |_| {
        json!(null)
    });
    assert!(ok.get("error").is_none(), "{ok}");
    assert_eq!(ok["result"]["text"], "still alive", "{ok}");
    rpc.shutdown_and_wait();
}
