use super::*;

#[tokio::test]
async fn markdown_transforms_filter_surfaces_and_clean_up_on_dispose() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([{ "reasoning": "THINK_TAG body", "text": "ANS_TAG reply" }]);
        let (h, mut rx, _ui) = tui_host(&env, script).await;
        expose(&h);
        global(&h, "M", h.service_table("message").unwrap());
        global(&h, "U", h.service_table("ui").unwrap());

        let bad_surface: String = eval(
            &h,
            r#"
            local ok, e = pcall(function()
                U:register_markdown_transform({
                    transform = function(s) return s end,
                    message_types = { "bogus" },
                })
            end)
            assert(not ok, "unknown message_type must fail")
            return tostring(e)
            "#,
        )
        .await;
        assert!(bad_surface.contains("message_types") && bad_surface.contains("bogus"), "{bad_surface}");

        exec(
            &h,
            r#"
            SEEN_CTX = {}
            REM1 = U:register_markdown_transform({
                transform = function(md, ctx)
                    SEEN_CTX[#SEEN_CTX + 1] = ctx
                    return (md:gsub("ANS_TAG", "ANS_OK"):gsub("THINK_TAG", "THINK_OK"):gsub("USER_TAG", "USER_OK"))
                end,
            }) -- default: user, assistant, assistant-reasoning
            REM2 = U:register_markdown_transform({
                transform = function(md)
                    return (md:gsub("CUSTOM_TAG", "CUSTOM_OK"))
                end,
                message_types = { "custom", "summary", "changelog" },
            })
            "#,
        )
        .await;

        let _ = drain_cmds(&mut rx);
        exec(
            &h,
            r#"
            local r = A:wait(A:prompt("USER_TAG hello").turn_id)
            assert(not r.error, tostring(r.error))
            M:append({ type = "fallback/card", content = "CUSTOM_TAG body", display = true })
            "#,
        )
        .await;
        let cmds = drain_cmds(&mut rx);
        let blocks = collect_pushed_text(&cmds);
        assert!(blocks.iter().any(|(_, k, t)| k == "user" && t.contains("USER_OK")), "{blocks:?}");
        assert!(blocks.iter().any(|(_, k, t)| k == "assistant" && t.contains("ANS_OK")), "{blocks:?}");
        assert!(blocks.iter().any(|(_, k, t)| k == "custom" && t.contains("CUSTOM_OK")), "{blocks:?}");

        exec(
            &h,
            r#"
            assert(U:transform_markdown("CUSTOM_TAG", { message_type = "summary" }).text == "CUSTOM_OK")
            assert(U:transform_markdown("CUSTOM_TAG", { message_type = "changelog" }).text == "CUSTOM_OK")
            assert(U:transform_markdown("CUSTOM_TAG", { message_type = "assistant" }).text == "CUSTOM_TAG")
            -- Reasoning text was transformed at finish_thinking: check recorded contexts
            local saw_reasoning, saw_streaming = false, false
            for _, c in ipairs(SEEN_CTX) do
                if c.message_type == "assistant-reasoning" then saw_reasoning = true end
                if c.is_streaming == true then saw_streaming = true end
            end
            assert(saw_reasoning, "assistant-reasoning must trigger default transformer")
            assert(saw_streaming, "streaming assistant text must trigger transformer with is_streaming=true")
            REM1()
            REM2()
            assert(U:transform_markdown("ANS_TAG", { message_type = "assistant" }).text == "ANS_TAG")
            "#,
        )
        .await;
        h.shutdown().await;
    })
    .await;
}

#[test]
fn message_append_volatile_resume_invalid_blocks_and_peer_denial() {
    let tmp = tempfile::tempdir().unwrap();
    let faux = tmp.path().join("faux.json");
    std::fs::write(&faux, json!([{ "text": "slow turn ".repeat(4000), "delay_ms": 20 }]).to_string()).unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--peer", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", tmp.path())
        .env("MARE_FAUX_SCRIPT", &faux)
        .env("MARE_DISABLE_UPDATE_CHECK", "1")
        .stderr(std::process::Stdio::null());

    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init =
        rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }), |_| {
            json!(null)
        });
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();

    // Unauthorized peer is denied on all message.* RPC methods without peer.allow.
    let mut peer = crate::common::rpc::PeerClient::connect(tmp.path(), rpc.pid());
    peer.call("initialize", json!({ "protocol_version": init["result"]["protocol_version"] }));
    for (m, p) in [
        ("message.list_types", json!({})),
        ("message.register_type", json!({ "type": "peer/card" })),
        ("message.append", json!({ "session_id": sid, "type": "peer/card", "content": "nope" })),
    ] {
        let denied = peer.call(m, p);
        assert_eq!(denied["error"]["data"]["code"], "denied", "peer calling {m} must be denied: {denied}");
    }
    drop(peer);

    // Malformed content block / delivery args return invalid_arguments (-32602), never internal (-32603).
    for bad_params in [
        json!({ "session_id": sid, "type": "bad/card", "content": [{ "type": "unknown" }] }),
        json!({ "session_id": sid, "type": "bad/card", "content": [{ "type": "text", "text": 123 }] }),
        json!({ "session_id": sid, "type": "bad/card", "content": "ok", "delivery": "next_turn" }),
    ] {
        let err = rpc.call_raw("message.append", bad_params, |_| json!(null));
        assert_eq!(err["error"]["code"], -32602, "expected -32602 invalid_arguments, got: {err}");
        assert_eq!(err["error"]["data"]["code"], "invalid_arguments", "{err}");
    }

    // Volatile next_turn path with request_id: replays duplicate within live session,
    // does NOT write durable operation receipt so session.resume + retry re-queues without losing the message.
    let nt_req = json!({
        "session_id": sid,
        "type": "vol/next",
        "content": "remember-across-resume-next-turn",
        "delivery": { "deliver_as": "next_turn" },
        "request_id": "vol-next-1"
    });
    let nt1 = rpc.call_raw("message.append", nt_req.clone(), |_| json!(null));
    assert_eq!(nt1["result"]["status"], "queued", "{nt1}");
    assert!(nt1["result"].get("ok").is_none(), "{nt1}");
    assert!(nt1["result"].get("duplicate").is_none(), "{nt1}");
    let nt_live_dup = rpc.call_raw("message.append", nt_req.clone(), |_| json!(null));
    assert_eq!(nt_live_dup["result"]["duplicate"], true, "{nt_live_dup}");

    // Busy deferred path with request_id also must not write durable operation receipt.
    let hook = rpc.call_raw(
        "hooks.subscribe",
        json!({ "session_id": sid, "event": "request/ready", "callbacks": ["handle"] }),
        |_| json!(null),
    );
    assert!(hook["error"].is_null(), "{hook}");
    let mut held_req: Option<Value> = None;
    let prompt_id = rpc.request("agent.prompt", json!({ "session_id": sid, "input": "hold" }));
    loop {
        let msg = rpc.read();
        if msg["id"] == prompt_id {
            break;
        }
        if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "request/ready" {
            held_req = Some(msg["id"].clone());
        }
    }
    while held_req.is_none() {
        let msg = rpc.read();
        if msg["method"] == "callback.call" && msg["params"]["arguments"]["event"] == "request/ready" {
            held_req = Some(msg["id"].clone());
        }
    }
    let def_req = json!({
        "session_id": sid,
        "type": "vol/deferred",
        "content": "remember-across-resume-deferred",
        "request_id": "vol-def-1"
    });
    let def1 = rpc.call_raw("message.append", def_req.clone(), |_| json!(null));
    assert_eq!(def1["result"]["status"], "deferred", "{def1}");
    assert!(def1["result"].get("ok").is_none(), "{def1}");
    let abort_id = rpc.request("agent.cancel", json!({ "session_id": sid }));
    rpc.send(json!({
        "jsonrpc": "2.0",
        "id": held_req.unwrap(),
        "result": { "value": { "decision": "deny", "reason": "stop" } }
    }));
    loop {
        if rpc.read()["id"] == abort_id {
            break;
        }
    }

    // Close and reload `sid`: volatile queues are cleared, so retrying same request_id must re-queue!
    let other = rpc.call_raw("session.create", json!({}), |_| json!(null));
    assert!(other["error"].is_null(), "{other}");
    let closed = rpc.call_raw("session.close", json!({ "session_id": sid }), |_| json!(null));
    assert!(closed["error"].is_null(), "{closed}");
    let resumed = rpc.call_raw("session.load", json!({ "session_id": sid }), |_| json!(null));
    assert!(resumed["error"].is_null(), "{resumed}");
    let nt_after_resume = rpc.call_raw("message.append", nt_req, |_| json!(null));
    assert_eq!(nt_after_resume["result"]["status"], "queued", "{nt_after_resume}");
    assert_ne!(
        nt_after_resume["result"].get("duplicate").and_then(Value::as_bool),
        Some(true),
        "volatile next_turn after session.resume must not be falsely skipped as duplicate: {nt_after_resume}"
    );
    let def_after_resume = rpc.call_raw("message.append", def_req, |_| json!(null));
    assert_eq!(def_after_resume["result"]["status"], "appended", "{def_after_resume}");
    assert_ne!(
        def_after_resume["result"].get("duplicate").and_then(Value::as_bool),
        Some(true),
        "volatile deferred after session.resume must not be falsely skipped as duplicate: {def_after_resume}"
    );

    let shut = rpc.call_raw("shutdown", json!({}), |_| json!(null));
    assert!(shut["error"].is_null(), "{shut}");
    rpc.wait_exit();
}
