//! RPC methods (`mcp.servers/list/connect/reconnect/resources/read/prompts/get`),
//! `mcp.elicitation` callback forwarding, and public Lua API contracts (`schema_error`, `is_current`).

use super::*;
use std::process::Stdio;

fn write_rpc_init(home: &std::path::Path, servers_lua: &str) {
    std::fs::create_dir_all(home.join("config")).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        format!(
            r#"
mare.setup {{ model = "faux/echo" }}
mare.plugin("mcp", {{ servers = {servers_lua} }})
"#
        ),
    )
    .unwrap();
}

#[test]
fn rpc_mcp_methods_cover_servers_list_connect_reconnect_resources_read_prompts_get() {
    let home = tempfile::tempdir().unwrap();
    let rich_log = home.path().join("rich.log");
    let lazy_log = home.path().join("lazy.log");
    let rich_cfg = json!({ "log": rich_log, "rich": true, "tools": ["echo"] }).to_string();
    let lazy_cfg = json!({ "log": lazy_log, "tools": ["echo", "other"] }).to_string();
    let bin = serde_json::to_string(fake_mcp_bin()).unwrap();
    write_rpc_init(
        home.path(),
        &format!(
            r#"{{
  rich = {{ command = {bin}, args = {{ "scripted", {} }}, tier = "direct", required = true }},
  lazy = {{ command = {bin}, args = {{ "scripted", {} }}, tier = "deferred" }}
}}"#,
            serde_json::to_string(&rich_cfg).unwrap(),
            serde_json::to_string(&lazy_cfg).unwrap()
        ),
    );
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session"]).env("MARE_HOME", home.path()).current_dir(home.path()).stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let methods: Vec<&str> =
        init["result"]["capabilities"]["methods"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    for m in [
        "mcp.list_servers",
        "mcp.list_tools",
        "mcp.connect",
        "mcp.reconnect",
        "mcp.list_resources",
        "mcp.read_resource",
        "mcp.list_prompts",
        "mcp.get_prompt",
    ] {
        assert!(methods.contains(&m), "missing {m} in {methods:?}");
    }

    // Subscribe to MCP readiness events before checking `mcp.list_servers` so a direct server still
    // in `connecting` right after `initialize` settles via its `mcp/ready` event instead of racing.
    let sub =
        rpc.call_raw("events.subscribe", json!({ "scope": "all", "include": ["mcp/ready", "mcp/error"] }), |_| {
            json!(null)
        });
    assert!(sub.get("error").is_none(), "events.subscribe failed: {sub}");

    let mut servers_res = rpc.call_raw("mcp.list_servers", json!({}), |_| json!(null));
    let rich_state = |res: &Value| {
        res["result"]["data"]
            .as_array()
            .and_then(|rows| rows.iter().find(|s| s["name"] == "rich"))
            .and_then(|s| s["state"].as_str())
            .map(str::to_owned)
    };
    if rich_state(&servers_res).as_deref() == Some("connecting") {
        let deadline =
            std::time::Instant::now() + mare_cli::testkit::scaled_test_timeout(std::time::Duration::from_secs(15));
        loop {
            let msg = rpc
                .read_deadline(deadline)
                .unwrap_or_else(|e| panic!("timed out waiting for `rich` mcp/ready event: {e}"));
            if msg.get("method").and_then(Value::as_str) != Some("event") {
                continue;
            }
            if msg["params"]["data"]["server"].as_str() != Some("rich") {
                continue;
            }
            match msg["params"]["type"].as_str() {
                Some("mcp/ready") => break,
                Some("mcp/error") => panic!("rich MCP server failed while connecting: {msg}"),
                _ => {}
            }
        }
        servers_res = rpc.call_raw("mcp.list_servers", json!({}), |_| json!(null));
    }
    let rows = servers_res["result"]["data"].as_array().unwrap();
    let by_name: std::collections::HashMap<&str, &Value> =
        rows.iter().map(|s| (s["name"].as_str().unwrap(), s)).collect();
    assert_eq!(by_name["rich"]["state"], "connected", "{servers_res}");
    assert_eq!(by_name["lazy"]["state"], "idle", "{servers_res}");
    assert_eq!(by_name["lazy"]["tools"], 0, "{servers_res}");

    let conn_res = rpc.call_raw("mcp.connect", json!({ "server": "lazy" }), |_| json!(null));
    assert_eq!(conn_res["result"]["state"], "connected", "{conn_res}");
    assert_eq!(conn_res["result"]["tools"], 2, "{conn_res}");

    let list_res = rpc.call_raw("mcp.list_tools", json!({ "server": "lazy" }), |_| json!(null));
    let tool_names: Vec<&str> =
        list_res["result"]["data"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(tool_names, vec!["echo", "other"], "{list_res}");

    let res_list = rpc.call_raw("mcp.list_resources", json!({ "server": "rich" }), |_| json!(null));
    assert_eq!(res_list["result"]["data"][0]["uri"], "mem://a", "{res_list}");
    assert_eq!(res_list["result"]["resource_templates"][0]["uriTemplate"], "mem://{x}", "{res_list}");

    let read_res = rpc.call_raw("mcp.read_resource", json!({ "server": "rich", "uri": "mem://hi" }), |_| json!(null));
    assert_eq!(read_res["result"]["contents"][0]["text"], "body of mem://hi", "{read_res}");

    let prompts_res = rpc.call_raw("mcp.list_prompts", json!({ "server": "rich" }), |_| json!(null));
    assert_eq!(prompts_res["result"]["data"][0]["name"], "greet", "{prompts_res}");

    let get_res = rpc.call_raw(
        "mcp.get_prompt",
        json!({ "server": "rich", "name": "greet", "arguments": { "who": "Ada" } }),
        |_| json!(null),
    );
    assert_eq!(get_res["result"]["messages"][0]["content"]["text"], "Hello Ada", "{get_res}");

    let reconn_res = rpc.call_raw("mcp.reconnect", json!({ "server": "rich" }), |_| json!(null));
    assert_eq!(reconn_res["result"]["state"], "connected", "{reconn_res}");

    let missing_res = rpc.call_raw("mcp.connect", json!({ "server": "no_such" }), |_| json!(null));
    assert!(missing_res.get("error").is_some(), "{missing_res}");
    assert_eq!(missing_res["error"]["data"]["code"], "mcp.unknown_server", "{missing_res}");

    rpc.shutdown_and_wait();
}

#[test]
fn rpc_mcp_elicitation_capability_handles_form_url_and_abort_cancel() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls":[{"name":"mcp__human__elicit", "arguments":{}}]},
            {"text":"form done"},
            {"tool_calls":[{"name":"mcp__human__elicit_url", "arguments":{}}]},
            {"text":"url done"},
            {"tool_calls":[{"name":"mcp__human__elicit_slow", "arguments":{}}]},
            {"text":"never"}
        ])
        .to_string(),
    )
    .unwrap();
    let log = home.path().join("mcp.log");
    let cfg = json!({"log": log, "tools": ["elicit", "elicit_url", "elicit_slow"]}).to_string();
    let bin = serde_json::to_string(fake_mcp_bin()).unwrap();
    write_rpc_init(
        home.path(),
        &format!(
            r#"{{ human = {{ command = {bin}, args = {{ "scripted", {} }}, tier = "direct", required = true }} }}"#,
            serde_json::to_string(&cfg).unwrap()
        ),
    );
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);

    // Enable capabilities.elicitation without capabilities.ui: both form and url route via mcp.elicitation.
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"elicitation":true,"approvals":true}}),
        |_| json!(null),
    );
    assert_eq!(init["result"]["capabilities"]["callbacks"]["elicitation"], true, "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    // 1. Form mode elicitation -> accept with content
    let mut form_reqs = vec![];
    let form_started = rpc.call_raw("agent.prompt", json!({"session_id": sid, "input": "form"}), |_| json!(null));
    let form_res =
        rpc.call_raw("agent.wait", json!({"session_id": sid, "turn_id": form_started["result"]["turn_id"]}), |m| {
            if m.get("method").and_then(Value::as_str) == Some("mcp.elicitation") {
                form_reqs.push(m["params"].clone());
                json!({"action":"accept","content":{"name":"Ada"}})
            } else {
                json!(null)
            }
        });
    assert_eq!(form_res["result"]["text"], "form done", "{form_res}");
    let tool =
        form_res["result"]["messages"].as_array().unwrap().iter().find(|msg| msg["role"] == "tool_result").unwrap();
    assert_eq!(tool["content"][0]["text"], "accept", "{tool}");
    assert_eq!(form_reqs.len(), 1);
    assert_eq!(form_reqs[0]["server"], "human");
    assert_eq!(form_reqs[0]["mode"], "form");
    assert_eq!(form_reqs[0]["message"], "Your name?");
    assert_eq!(form_reqs[0]["requested_schema"]["required"][0], "name");

    // 2. URL mode elicitation -> decline
    let mut url_reqs = vec![];
    let url_started = rpc.call_raw("agent.prompt", json!({"session_id": sid, "input": "url"}), |_| json!(null));
    let url_res =
        rpc.call_raw("agent.wait", json!({"session_id": sid, "turn_id": url_started["result"]["turn_id"]}), |m| {
            if m.get("method").and_then(Value::as_str) == Some("mcp.elicitation") {
                url_reqs.push(m["params"].clone());
                json!({"action":"decline"})
            } else {
                json!(null)
            }
        });
    assert_eq!(url_res["result"]["text"], "url done", "{url_res}");
    let tool =
        url_res["result"]["messages"].as_array().unwrap().iter().find(|msg| msg["role"] == "tool_result").unwrap();
    assert_eq!(tool["content"][0]["text"], "decline", "{tool}");
    assert_eq!(url_reqs.len(), 1);
    assert_eq!(url_reqs[0]["mode"], "url");
    assert_eq!(url_reqs[0]["url"], "https://example.test/auth");
    assert_eq!(url_reqs[0]["elicitation_id"], "elicit-url-1");

    // 3. Abort while mcp.elicitation is pending sends $/cancel_request for its request id
    let p_id = rpc.request("agent.prompt", json!({"session_id": sid, "input": "slow"}));
    let mut turn4_id = None;
    let elicit_id = loop {
        let m = rpc.read();
        if m["id"] == p_id {
            turn4_id = Some(m["result"]["turn_id"].clone());
        } else if m.get("method").and_then(Value::as_str) == Some("mcp.elicitation") {
            break m["id"].clone();
        }
    };
    let a_id = rpc.request("agent.cancel", json!({"session_id": sid, "turn_id": turn4_id}));
    let (mut saw_cancel, mut saw_abort_done, mut wait_id) = (false, false, None::<u64>);
    if let Some(tid) = turn4_id.clone() {
        wait_id = Some(rpc.request("agent.wait", json!({"session_id": sid, "turn_id": tid})));
    }
    let mut saw_wait_done = false;
    while !(saw_cancel && saw_abort_done && saw_wait_done) {
        let m = rpc.read();
        if m.get("method").and_then(Value::as_str) == Some("$/cancel_request") && m["params"]["id"] == elicit_id {
            saw_cancel = true;
        } else if m["id"] == p_id {
            let tid = m["result"]["turn_id"].clone();
            if wait_id.is_none() {
                wait_id = Some(rpc.request("agent.wait", json!({"session_id": sid, "turn_id": tid})));
            }
        } else if m["id"] == a_id {
            saw_abort_done = true;
        } else if wait_id.is_some_and(|w| m["id"] == w) {
            saw_wait_done = true;
        }
    }
    rpc.shutdown_and_wait();
}

#[tokio::test]
async fn mcp_client_exposes_public_schema_error_and_is_current() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "public-fields");
        let entry = fake.entry(json!({ "tools": ["echo", "huge"], "invalid_large": true }));
        let cfg = servers(json!({ "fake": with(entry, json!({ "tier": "direct", "required": true })) }));
        let h = env.host().builtins(&["mcp"]).config(cfg).no_session().build().await;
        expose(&h);
        global(
            &h,
            "SPEC",
            mare_shared::lua::json_to_lua(
                h.lua(),
                &fake.entry(json!({ "tools": ["echo", "huge"], "invalid_large": true })),
            )
            .unwrap(),
        );
        let (has_schema_err, no_legacy_err, is_current_ok): (bool, bool, bool) = eval(
            &h,
            r#"local c = mare.mcp.connect(SPEC)
               local tools = c:tools()
               c:close()
               local err_field, legacy_field = false, false
               for _, t in ipairs(tools) do
                   if t.name == "huge" then
                       err_field = type(t.schema_error) == "string"
                       legacy_field = t.__mare_schema_error ~= nil
                   end
               end
               local d = T:get("mcp__fake__echo")
               return err_field, legacy_field, type(d.is_current) == "function" and d.is_current() == true"#,
        )
        .await;
        assert!(has_schema_err, "expected public t.schema_error on unrepresentable schema");
        assert!(!no_legacy_err, "t.__mare_schema_error must no longer be exposed");
        assert!(is_current_ok, "expected public d.is_current() returning true");
        h.shutdown().await;
    })
    .await
}

#[cfg(unix)]
#[test]
fn rpc_mcp_peer_access_and_error_contracts() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("config")).unwrap();
    let rich_log = root.path().join("rich.log");
    let lazy_log = root.path().join("lazy.log");
    let rich_cfg = json!({ "log": rich_log, "rich": true, "tools": ["echo"] }).to_string();
    let lazy_cfg = json!({ "log": lazy_log, "tools": ["echo", "other"] }).to_string();
    let bin = serde_json::to_string(fake_mcp_bin()).unwrap();
    std::fs::write(
        home.join("config/init.lua"),
        format!(
            r#"
mare.setup {{ model = "faux/echo", peer = {{ allow = {{ "mcp.list_servers", "mcp.list_tools", "mcp.list_resources", "mcp.read_resource", "mcp.list_prompts", "mcp.get_prompt", "mcp.connect", "mcp.reconnect" }} }} }}
mare.plugin("mcp", {{ servers = {{
  rich = {{ command = {bin}, args = {{ "scripted", {} }}, tier = "direct", required = true }},
  lazy = {{ command = {bin}, args = {{ "scripted", {} }}, tier = "deferred" }},
  broken = {{ command = "/nonexistent/mare-mcp-server", tier = "deferred" }}
}} }})
"#,
            serde_json::to_string(&rich_cfg).unwrap(),
            serde_json::to_string(&lazy_cfg).unwrap()
        ),
    )
    .unwrap();

    let env = std::collections::BTreeMap::from([("MARE_HOME".to_string(), home.display().to_string())]);
    let mut client = crate::common::rpc::RpcClient::spawn(
        &["--rpc", "--peer", "--model", "faux/echo", "--no-session"],
        &env,
        root.path(),
    );
    let init = client.call("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }));

    let mut peer = crate::common::rpc::PeerClient::connect(&home, client.pid());
    let peer_init = peer.call("initialize", json!({ "protocol_version": init["protocol_version"] }));
    let peer_methods = peer_init["result"]["capabilities"]["methods"].as_array().unwrap();
    for m in [
        "mcp.list_servers",
        "mcp.list_tools",
        "mcp.list_resources",
        "mcp.read_resource",
        "mcp.list_prompts",
        "mcp.get_prompt",
    ] {
        assert!(peer_methods.iter().any(|v| v == m), "peer should see {m}: {peer_methods:?}");
    }
    for m in ["mcp.connect", "mcp.reconnect"] {
        assert!(!peer_methods.iter().any(|v| v == m), "peer must not see {m}: {peer_methods:?}");
    }
    for legacy in ["mcp.servers", "mcp.list", "mcp.resources", "mcp.read", "mcp.prompts", "mcp.get"] {
        let resp = peer.call(legacy, json!({}));
        assert_eq!(resp["error"]["code"], -32601, "legacy {legacy} must be gone: {resp}");
    }

    let conn_denied = peer.call("mcp.connect", json!({ "server": "lazy" }));
    assert_eq!(conn_denied["error"]["data"]["code"], "denied", "{conn_denied}");
    let reconn_denied = peer.call("mcp.reconnect", json!({ "server": "rich" }));
    assert_eq!(reconn_denied["error"]["data"]["code"], "denied", "{reconn_denied}");

    let servers = peer.call("mcp.list_servers", json!({}));
    assert!(servers.get("error").is_none(), "{servers}");
    assert!(servers["result"]["data"].is_array(), "{servers}");

    let conn_ok = client.call("mcp.connect", json!({ "server": "lazy" }));
    assert_eq!(conn_ok["name"], "lazy", "{conn_ok}");
    assert_eq!(conn_ok["state"], "connected", "{conn_ok}");
    assert!(conn_ok.get("ok").is_none(), "connect returns ServerStatus directly, no ok flag: {conn_ok}");

    let unknown_err = client.call_err("mcp.connect", json!({ "server": "no_such" }));
    assert_eq!(unknown_err["data"]["code"], "mcp.unknown_server", "{unknown_err}");

    let fail_err = client.call_err("mcp.connect", json!({ "server": "broken" }));
    assert_eq!(fail_err["data"]["code"], "mcp.connect_failed", "{fail_err}");

    let tools = peer.call("mcp.list_tools", json!({ "server": "lazy" }));
    assert!(tools.get("error").is_none(), "{tools}");
    let tool_names: Vec<&str> =
        tools["result"]["data"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(tool_names, vec!["echo", "other"], "{tools}");

    let resources = peer.call("mcp.list_resources", json!({ "server": "rich" }));
    assert_eq!(resources["result"]["data"][0]["uri"], "mem://a", "{resources}");
    assert_eq!(resources["result"]["resource_templates"][0]["uriTemplate"], "mem://{x}", "{resources}");

    let read = peer.call("mcp.read_resource", json!({ "server": "rich", "uri": "mem://hi" }));
    assert_eq!(read["result"]["contents"][0]["text"], "body of mem://hi", "{read}");

    let prompts = peer.call("mcp.list_prompts", json!({ "server": "rich" }));
    assert_eq!(prompts["result"]["data"][0]["name"], "greet", "{prompts}");

    let got = peer.call("mcp.get_prompt", json!({ "server": "rich", "name": "greet", "arguments": { "who": "Ada" } }));
    assert_eq!(got["result"]["messages"][0]["content"]["text"], "Hello Ada", "{got}");

    drop(peer);
    client.shutdown_and_wait();
}
