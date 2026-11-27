//! A real stdio server waits for an input callback longer than both tools/call budgets.
use super::*;

#[tokio::test]
async fn mcp_human_input_pauses_both_budgets_and_resumes_the_idle_timer() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "human-wait");
        let spec = with(fake.entry(json!({"tools": ["elicit", "elicit_slow"]})), json!({"name": "human-wait"}));
        let h = env.host().builtins(&["mcp"]).no_session().build().await;
        global(&h, "SPEC", mare_shared::lua::json_to_lua(h.lua(), &spec).unwrap());
        exec(
            &h,
            r#"
            CL = mare.mcp.connect(SPEC)
            ANSWER = "Ada"
            INPUT_OPEN = false
            RELEASE_INPUT = false
            mare.ui.input = function()
                INPUT_OPEN = true
                while not RELEASE_INPUT do mare.task.sleep(5) end
                INPUT_OPEN = false
                return ANSWER
            end
            EVENTS = mare.task.spawn(function()
                while true do
                    local e = CL:next_event()
                    if not e then return end
                    if e.type == "elicit" then
                        local name = mare.ui.input({title = e.params.message})
                        ANSWERED_AT = mare.task.clock()
                        CL:respond(e.id, name and {action = "accept", content = {name = name}} or {action = "cancel"})
                    end
                end
            end)
        "#,
        )
        .await;
        for (tool, cancel) in [("elicit", false), ("elicit", true), ("elicit_slow", false)] {
            global(&h, "TOOL", tool);
            global(&h, "CANCEL_INPUT", cancel);
            exec(
                &h,
                r#"
                DONE = false
                RELEASE_INPUT = false
                ANSWER = "Ada"
                if CANCEL_INPUT then ANSWER = nil end
                CALL = mare.task.spawn(function()
                    OK, RESULT = pcall(CL.call, CL, TOOL, {}, {timeout_ms = 100})
                    FINISHED_AT = mare.task.clock()
                    DONE = true
                end)
            "#,
            )
            .await;
            wait_until(5, "the server's blocking input callback", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return INPUT_OPEN").await }
            })
            .await;
            // 100ms idle and 1000ms total must both stop during the user's input.
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            assert!(!eval::<bool>(&h, "return DONE").await, "human input consumed the call budget");
            exec(&h, "RELEASE_INPUT = true").await;
            wait_until(5, "call completion after answering", || {
                let h = h.clone();
                async move { eval::<bool>(&h, "return DONE").await }
            })
            .await;
            if tool == "elicit_slow" {
                let (ok, error, active): (bool, String, f64) =
                    eval(&h, "return OK, tostring(RESULT), FINISHED_AT - ANSWERED_AT").await;
                assert!(!ok && error.contains("timed out after 100ms"), "{error}");
                assert!((50.0..500.0).contains(&active), "idle timer did not resume: {active}ms");
            } else {
                let (ok, action): (bool, String) =
                    eval(&h, "return OK, OK and RESULT.content[1].text or tostring(RESULT)").await;
                assert!(ok, "{action}");
                assert_eq!(action, if cancel { "cancel" } else { "accept" });
            }
        }
        exec(&h, "CL:close()").await;
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn registered_mcp_tool_keeps_agent_tui_input_outside_outer_timeout() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "pipeline-human-wait");
        let cfg = json!({"plugins": {
            "tools": {"timeout_ms": 100},
            "mcp": {"servers": {"human": with(fake.entry(json!({"tools":["elicit"]})),
                json!({"tier":"direct", "required":true, "call_timeout_ms":100}))}}
        }});
        let script = json!([
            {"tool_calls":[{"name":"mcp__human__elicit", "arguments":{}}]},
            {"text":"completed"}
        ]);
        let mut opts = env.host().builtins(&["mcp"]).faux(script).config(cfg).no_session().opts;
        opts.mode = mare_shared::ui::Mode::Tui;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        expose(&h);
        exec(&h, "C.cordis:on('tool/after', function(data, ev) TOOL_RESULT = data.result; return ev.next() end)").await;
        let host = h.clone();
        let prompt = tokio::task::spawn_local(async move { host.prompt("ask for my name", json!({})).await });
        let input = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), async {
            loop {
                match rx.recv().await.unwrap() {
                    mare_shared::ui::UiCmd::Select { reply, .. } => {
                        reply
                            .send(Ok(mare_shared::ui::SelectReply {
                                index: 0,
                                tab: 0,
                                key: None,
                                filter: String::new(),
                                ..Default::default()
                            }))
                            .unwrap();
                    }
                    mare_shared::ui::UiCmd::Input { reply, .. } => break reply,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert!(!prompt.is_finished(), "outer tool timeout consumed human input time");
        input.send(Ok("Ada".into())).unwrap();
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), prompt).await.unwrap().unwrap().unwrap();
        assert_eq!(result["text"], "completed", "{result}");
        assert!(fake.lines().iter().any(|line| line == "tools/call"));
        assert!(
            eval::<bool>(
                &h,
                "return TOOL_RESULT ~= nil and not TOOL_RESULT.is_error and TOOL_RESULT.content[1].text == 'accept'"
            )
            .await
        );
        h.shutdown().await;
    })
    .await
}

#[test]
fn registered_mcp_tool_keeps_rpc_ui_callback_outside_outer_timeout() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls":[{"name":"mcp__human__elicit", "arguments":{}}]},
            {"text":"completed"}
        ])
        .to_string(),
    )
    .unwrap();
    let log = home.path().join("mcp.log");
    let config = json!({"log":log,"tools":["elicit"]}).to_string();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        format!(
            r#"
        mare.setup {{ model = "faux/echo" }}
        mare.plugin("tools", {{ timeout_ms = 100 }})
        mare.plugin("mcp", {{ servers = {{ human = {{
            command = {}, args = {{ "scripted", {} }},
            tier = "direct", required = true, call_timeout_ms = 100
        }} }} }})
    "#,
            serde_json::to_string(fake_mcp_bin()).unwrap(),
            serde_json::to_string(&config).unwrap()
        ),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "capabilities":{"ui":true,"approvals":true}}),
        |_| json!(null),
    );
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let mut inputs = 0;
    let started = rpc.call_raw("agent.prompt", json!({"session_id": sid, "input": "ask my name"}), |_| json!(null));
    let mut handle_cb = |call: &Value| match call["method"].as_str().unwrap() {
        "ui.select" => json!({"index":0}),
        "ui.input" => {
            inputs += 1;
            // sleep-ok: simulates human response delay exceeding 1s threshold while turn waits on elicitation
            std::thread::sleep(std::time::Duration::from_millis(1200));
            json!({"text":"Ada"})
        }
        "approval.request" => json!({"decision":"allow"}),
        other => panic!("unexpected callback {other}: {call}"),
    };
    let done =
        rpc.call_raw("agent.wait", json!({"session_id": sid, "turn_id": started["result"]["turn_id"]}), &mut handle_cb);
    assert_eq!(inputs, 1, "registered tool did not reach real RPC input: {done}");
    assert_eq!(done["result"]["text"], "completed", "{done}");
    assert!(std::fs::read_to_string(log).unwrap().contains("tools/call"));
    let tool =
        done["result"]["messages"].as_array().unwrap().iter().find(|message| message["role"] == "tool_result").unwrap();
    assert_ne!(tool["is_error"], true, "{tool}");
    assert_eq!(tool["content"][0]["text"], "accept", "{tool}");
    rpc.shutdown_and_wait();
}

#[tokio::test]
async fn aborting_agent_releases_registered_tool_input_before_next_turn() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "cancel-human-wait");
        let cfg = json!({"plugins": {
            "tools": {"cancel_grace_ms": 20},
            "mcp": {"servers": {"human": with(fake.entry(json!({"tools":["elicit", "slow"]})),
                json!({"tier":"direct", "required":true, "call_timeout_ms":100}))}}
        }});
        let script = json!([
            {"tool_calls":[{"name":"mcp__human__elicit","arguments":{}}]},
            {"tool_calls":[{"name":"mcp__human__slow","arguments":{}}]},
            {"text":"second done"}
        ]);
        let mut opts = env.host().builtins(&["mcp"]).faux(script).config(cfg).no_session().opts;
        opts.mode = mare_shared::ui::Mode::Tui;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        expose(&h);
        wait_until(WAIT_SECS, "registered tools", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__human__elicit') ~= nil").await }
        })
        .await;
        let host = h.clone();
        let first = tokio::task::spawn_local(async move { host.prompt("ask for input", json!({})).await });
        let input = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), async {
            loop {
                match rx.recv().await.unwrap() {
                    mare_shared::ui::UiCmd::Select { reply, .. } => {
                        reply
                            .send(Ok(mare_shared::ui::SelectReply {
                                index: 0,
                                tab: 0,
                                key: None,
                                filter: String::new(),
                                ..Default::default()
                            }))
                            .unwrap();
                    }
                    mare_shared::ui::UiCmd::Input { reply, .. } => break reply,
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        // The scripted server logs cancellation but deliberately leaves elicitation open.
        h.abort().await.unwrap();
        first.await.unwrap().unwrap();
        let host = h.clone();
        let second = tokio::task::spawn_local(async move { host.prompt("second call", json!({})).await });
        let mut input = input;
        let input_closed =
            tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), input.closed()).await.is_ok();
        let second_finished = tokio::time::timeout(std::time::Duration::from_secs(WAIT_SECS), second).await;
        if !input_closed {
            // Release the fixture even on the buggy baseline, so failure leaves no UI task behind.
            let _ = input.send(Err(mare_shared::ui::Dismissed::Cancelled));
        }
        let second_result =
            second_finished.expect("abandoned elicitation kept the next call's idle timer paused").unwrap().unwrap();
        let tool_results = second_result["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool_result")
            .collect::<Vec<_>>();
        assert!(
            tool_results.iter().any(|message| message["is_error"] == true
                && message["content"].to_string().contains("timed out after 100ms")),
            "{second_result}"
        );
        let late_response = fake.lines().iter().any(|line| line == "elicitation cancel");
        h.shutdown().await;
        assert!(input_closed, "cancelled tool retained its pending UI input");
        assert!(late_response, "server never received elicitation cancellation");
    })
    .await
}

#[test]
fn rpc_abort_retracts_mcp_input_and_restores_next_call_idle_budget() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("faux.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls":[{"name":"mcp__human__elicit", "arguments":{}}]},
            {"tool_calls":[{"name":"mcp__human__slow", "arguments":{}}]},
            {"text":"second done"}
        ])
        .to_string(),
    )
    .unwrap();
    let log = home.path().join("mcp.log");
    let config = json!({"log":log,"tools":["elicit","slow"]}).to_string();
    std::fs::create_dir_all(home.path().join("config")).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        format!(
            r#"
        mare.setup {{ model = "faux/echo" }}
        mare.plugin("tools", {{ timeout_ms = 100 }})
        mare.plugin("mcp", {{ servers = {{ human = {{
            command = {}, args = {{ "scripted", {} }},
            tier = "direct", required = true, call_timeout_ms = 100
        }} }} }})
    "#,
            serde_json::to_string(fake_mcp_bin()).unwrap(),
            serde_json::to_string(&config).unwrap()
        ),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init = rpc.call_raw(
        "initialize",
        json!({"protocol_version": "3.0", "callback_timeout_ms":6000,"capabilities":{"ui":true,"approvals":true}}),
        |_| json!(null),
    );
    assert!(init.get("error").is_none(), "{init}");
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let first_start =
        rpc.call_raw("agent.prompt", json!({"session_id": sid, "input":"first"}), |call: &Value| match call["method"]
            .as_str()
        {
            Some("ui.select") => json!({"index":0}),
            Some("approval.request") => json!({"decision":"allow"}),
            _ => json!(null),
        });
    let first_id = rpc.request("agent.wait", json!({"session_id": sid, "turn_id": first_start["result"]["turn_id"]}));
    let input_id = loop {
        let message = rpc.read();
        match message["method"].as_str() {
            Some("ui.input") => break message["id"].clone(),
            Some("ui.select") => rpc.send(json!({"jsonrpc":"2.0","id":message["id"],"result":{"index":0}})),
            Some("approval.request") => {
                rpc.send(json!({"jsonrpc":"2.0","id":message["id"],"result":{"decision":"allow"}}))
            }
            _ => {}
        }
    };
    let abort_id = rpc.request("agent.cancel", json!({"session_id": sid}));
    let mut cancelled_input = false;
    let mut aborted = false;
    let mut first_finished = false;
    loop {
        let message = rpc.read();
        if message["method"] == "$/cancel_request" && message["params"]["id"] == input_id {
            cancelled_input = true;
        }
        if message.get("method").is_none() {
            aborted |= message["id"] == abort_id;
            first_finished |= message["id"] == first_id;
        }
        if aborted && first_finished && cancelled_input {
            break;
        }
    }
    let before = std::time::Instant::now();
    let next_start = rpc.call_raw("agent.prompt", json!({"session_id": sid, "input":"second"}), |call: &Value| {
        if call["method"] == "approval.request" { json!({"decision":"allow"}) } else { json!(null) }
    });
    let next_id = rpc.request("agent.wait", json!({"session_id": sid, "turn_id": next_start["result"]["turn_id"]}));
    let done = loop {
        let message = rpc.read();
        match message["method"].as_str() {
            Some("$/cancel_request") if message["params"]["id"] == input_id => cancelled_input = true,
            Some("approval.request") => {
                rpc.send(json!({"jsonrpc":"2.0","id":message["id"],"result":{"decision":"allow"}}))
            }
            _ => {}
        }
        if message["id"] == next_id && message.get("method").is_none() {
            break message;
        }
    };
    assert!(cancelled_input, "RPC input callback {input_id} was not retracted; result {done}");
    assert!(
        before.elapsed() < std::time::Duration::from_millis(4000),
        "next tool consumed the abandoned callback timeout"
    );
    assert_eq!(done["result"]["text"], "second done", "{done}");
    assert!(done.to_string().contains("timed out after 100ms"), "{done}");
    // A late UI answer must find no pending callback and cannot revive the old request.
    rpc.send(json!({"jsonrpc":"2.0","id":input_id,"result":{"text":"late"}}));
    rpc.shutdown_and_wait();
}

/// In headless mode (no connected UI and no `mcp/elicit`
/// hook taking over), elicitation requests are automatically declined.
#[tokio::test]
async fn mcp_headless_elicitation_declines_by_default() {
    local(async {
        let env = TestEnv::new().await;
        let fake = Fake::new(&env, "headless-elicit");
        let entry = with(fake.entry(json!({ "tools": ["elicit"] })), json!({ "tier": "direct" }));
        let h = env.host().builtins(&["mcp"]).config(servers(json!({ "human": entry }))).no_session().build().await;
        expose(&h);
        wait_until(15, "elicit tool ready", || {
            let h = h.clone();
            async move { eval::<bool>(&h, "return T:get('mcp__human__elicit') ~= nil").await }
        })
        .await;

        let text: String = eval(
            &h,
            r#"local r = T:execute({ id = "e1", name = "mcp__human__elicit", arguments = {} }, {})
               return type(r.content) == "table" and tostring(r.content[1].text) or tostring(r.content)"#,
        )
        .await;
        assert_eq!(text, "decline", "headless elicitation must default to decline: {text}");
        assert!(
            fake.lines().iter().any(|l| l == "elicitation decline"),
            "server must receive decline action: {:?}",
            fake.lines()
        );
        h.shutdown().await;
    })
    .await
}
