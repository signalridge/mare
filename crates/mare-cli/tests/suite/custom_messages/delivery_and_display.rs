use super::*;

#[tokio::test]
async fn custom_message_validate_render_display_and_model_context() {
    local(async {
        let env = TestEnv::new().await;
        // Second turn has no text, so faux echoes the last user/custom message in model context.
        let script = json!([{ "text": "seen" }, {}]);
        let (h, mut rx, _ui) = tui_host(&env, script).await;
        expose(&h);
        global(&h, "M", h.service_table("message").unwrap());

        exec(
            &h,
            r#"
            M:register_type("audit/note", {
                validate = function(msg)
                    if type(msg.details) ~= "table" or not msg.details.tag then
                        return false, "details.tag is required"
                    end
                    return true
                end,
                render = function(msg, opts)
                    local body = msg.content[1] and msg.content[1].text or ""
                    local prefix = opts.expanded and "[AUDIT FULL] " or "[AUDIT] "
                    return ui.Line({ ui.Span(prefix .. body .. " (" .. msg.details.tag .. ")"):hl("MareAccent") })
                end,
            })
            "#,
        )
        .await;

        let err: String = eval(
            &h,
            r#"
            local ok, e = pcall(function()
                M:append({ type = "audit/note", content = "bad" })
            end)
            assert(not ok, "validation must reject missing details.tag")
            return tostring(e)
            "#,
        )
        .await;
        assert!(err.contains("details.tag is required"), "{err}");

        let _ = drain_cmds(&mut rx);

        // display = true -> renders via registered renderer AND enters model context
        let res1 = eval_json(
            &h,
            r#"
            return M:append({
                type = "audit/note",
                content = "visible-custom",
                display = true,
                details = { tag = "v1" },
            })
            "#,
        )
        .await;
        assert_eq!(res1["status"], "appended");
        assert!(res1.get("ok").is_none(), "{res1}");
        let cmds1 = drain_cmds(&mut rx);
        let rendered1 = collect_pushed_text(&cmds1);
        assert!(
            rendered1.iter().any(|(_, k, t)| k == "custom" && t.contains("[AUDIT] visible-custom (v1)")),
            "expected custom renderer output in UI commands: {rendered1:?}"
        );

        // display = false -> hidden from transcript display, STILL enters model context
        let res2 = eval_json(
            &h,
            r#"
            return M:append({
                type = "audit/note",
                content = "hidden-context-only",
                display = false,
                details = { tag = "secret" },
            })
            "#,
        )
        .await;
        assert_eq!(res2["status"], "appended");
        assert!(res2.get("ok").is_none(), "{res2}");
        let cmds2 = drain_cmds(&mut rx);
        let rendered2 = collect_pushed_text(&cmds2);
        assert!(rendered2.is_empty(), "display=false custom message must not push UI block: {rendered2:?}");

        // Verify both custom messages are in agent:messages() and entered the model request!
        let echo: String = eval(
            &h,
            r#"
            local msgs = A:list_messages().data
            assert(#msgs == 2, "expected 2 custom messages in agent context, got " .. #msgs)
            assert(msgs[1].role == "custom" and msgs[1].custom_type == "audit/note")
            assert(msgs[2].role == "custom" and msgs[2].display == false)
            local r = A:wait(A:continue().turn_id)
            assert(not r.error, tostring(r.error))
            return r.text
            "#,
        )
        .await;
        assert_eq!(echo, "seen");

        let echo2: String = eval(
            &h,
            r#"
            -- Append another hidden custom message and continue to verify faux sees its converted text
            M:append({
                type = "audit/note",
                content = "model-sees-hidden",
                display = false,
                details = { tag = "ctx" },
            })
            local r = A:wait(A:continue().turn_id)
            assert(not r.error, tostring(r.error))
            return r.text
            "#,
        )
        .await;
        assert_eq!(echo2, "echo: model-sees-hidden");
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn custom_entry_stays_out_of_model_context_and_replays_via_entry_renderer() {
    local(async {
        let env = TestEnv::new().await;
        let script = json!([]);
        let (h, mut rx, _ui) = tui_host(&env, script).await;
        expose(&h);
        global(&h, "U", h.service_table("ui").unwrap());

        exec(
            &h,
            r#"
            ENTRY_EVENTS = {}
            FORGED_ITEM_EVENTS = 0
            C.cordis:on("session/recorded", function(ev)
                if type(ev) == "table" and type(ev.entry) == "table" then
                    table.insert(ENTRY_EVENTS, ev.entry)
                end
            end)
            C.cordis:on("item/completed", function(ev)
                if ev.item and ev.item.type == "custom" then
                    FORGED_ITEM_EVENTS = FORGED_ITEM_EVENTS + 1
                end
            end)
            U:register_entry_renderer("milestone/v1", {
                compact = function(entry)
                    return ui.Line({ ui.Span("★ " .. tostring(entry.data.label)):hl("MareAccent") })
                end,
                expanded = function(entry)
                    return ui.Line({ ui.Span("★ FULL " .. tostring(entry.data.label)):hl("MareAccent") })
                end,
            })
            U:register_entry_renderer("state/hidden", {
                render = function()
                    return nil
                end,
            })
            U:register_entry_renderer("broken/entry", {
                render = function()
                    error("boom-entry-render")
                end,
            })
            assert(S:add_record("milestone/v1", { label = "phase-1" }, { persist = true }))
            assert(S:add_record("state/hidden", { secret = "hidden-sentinel-42" }, { persist = true }))
            assert(S:add_record("broken/entry", { x = 1 }, { persist = true }))
            "#,
        )
        .await;

        let cmds = drain_cmds(&mut rx);
        let blocks = collect_pushed_text(&cmds);
        assert!(blocks.iter().any(|(_, k, t)| k == "entry" && t.contains("★ phase-1")), "{blocks:?}");
        assert!(
            blocks.iter().any(|(_, k, t)| k == "error" && t.contains("boom-entry-render")),
            "expected isolated renderer error notice: {blocks:?}"
        );
        assert!(
            !blocks.iter().any(|(_, _, t)| t.contains("hidden-sentinel-42")),
            "nil renderer must not display: {blocks:?}"
        );

        // Custom entries MUST NOT enter agent:messages() or LLM context!
        let (session_path, model_echo): (String, String) = eval(
            &h,
            r#"
            assert(#A:list_messages().data == 0, "custom entries must never enter model context")
            assert(#ENTRY_EVENTS == 3, "expected 3 session/recorded custom entry events")
            assert(FORGED_ITEM_EVENTS == 0, "custom entries must not forge item/completed events")
            local r = A:wait(A:prompt("only-user-prompt").turn_id)
            assert(not r.error, tostring(r.error))
            return S:info().path, r.text
            "#,
        )
        .await;
        assert_eq!(model_echo, "echo: only-user-prompt");

        // Reload the session in the same TUI host and verify entry_renderer replays milestone/v1!
        let _ = drain_cmds(&mut rx);
        exec(&h, &format!(r#"assert(S:load({{ path = {session_path:?} }}))"#)).await;
        let replay_cmds = drain_cmds(&mut rx);
        let replay_blocks = collect_pushed_text(&replay_cmds);
        assert!(
            replay_blocks.iter().any(|(_, k, t)| k == "entry" && t.contains("★ phase-1")),
            "expected replayed custom entry on session/loaded: {replay_blocks:?}"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn custom_message_delivery_modes_and_tool_pairing_safety() {
    local(async {
        let env = TestEnv::new().await;
        // Turn 1: model emits tool call; Turn 2 (after tool result): model says after-tool;
        // Turn 3 (follow_up / triggered): faux echoes last user/custom message.
        let script = json!([
            { "tool_calls": [{ "id": "c1", "name": "slow_gate", "arguments": {} }] },
            { "text": "after-tool" },
            {}
        ]);
        let h = env.host().builtins(&["message"]).faux(script).build().await;
        expose(&h);
        global(&h, "M", h.service_table("message").unwrap());

        exec(
            &h,
            r#"
            TOOL_STARTED = false
            local tx, rx = mare.task.channel()
            local rtx, rrx = mare.task.channel()
            GATE_START_RX = rx
            GATE_RELEASE_TX = rtx
            T:register("slow_gate", {
                description = "blocks until released",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                execute = function()
                    TOOL_STARTED = true
                    tx:send(true)
                    rrx:recv()
                    return { content = { { type = "text", text = "gate-ok" } } }
                end,
            })

            -- 1. next_turn waits until the next prompt starts
            local q = M:append({ type = "ctx/pre", content = "queued-next-turn", display = false, delivery = { deliver_as = "next_turn" } })
            assert(q.status == "queued", q.status)
            assert(#A:list_messages().data == 0, "next_turn must wait until prompt starts")
            "#,
        )
        .await;

        // Run prompt inside Lua task so we can deterministically inject mid-turn messages on channel signal.
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<Value>();
        let lua_fut = async {
            let res = eval_json(
                &h,
                r#"
                local driver = mare.task.spawn(function()
                    GATE_START_RX:recv()
                    -- While busy inside tool execution:
                    -- a) trigger_turn=false defers until after tool_result & run finish
                    local def = M:append({ type = "ctx/def", content = "deferred-after-run", display = true, delivery = { trigger_turn = false } })
                    assert(def.status == "deferred", def.status)
                    -- b) follow_up queues a follow-up turn
                    local fu = M:append({ type = "ctx/fu", content = "followup-custom", delivery = { deliver_as = "follow_up" } })
                    assert(fu.status == "queued", fu.status)
                    GATE_RELEASE_TX:send(true)
                end)
                local res = A:wait(A:prompt("start").turn_id)
                assert(not res.error, tostring(res.error))
                return res
                "#,
            )
            .await;
            let _ = done_tx.send(res);
        };
        lua_fut.await;
        let prompt_res = done_rx.await.unwrap();
        assert_eq!(prompt_res["text"], "echo: followup-custom", "expected follow_up custom message to drive turn 3: {prompt_res}");

        let msgs = eval_json(&h, "return A:list_messages().data").await;
        let arr = msgs.as_array().unwrap();
        // Order: user("start"), custom(next_turn), assistant(tool_call), tool_result(gate-ok),
        // assistant("after-tool"), custom(follow_up), assistant("followup-custom"), custom(deferred)
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "custom");
        assert_eq!(arr[1]["custom_type"], "ctx/pre");
        assert_eq!(arr[2]["role"], "assistant");
        assert_eq!(arr[3]["role"], "tool_result");
        assert_eq!(arr.last().unwrap()["custom_type"], "ctx/def");
        h.shutdown().await;
    })
    .await;
}

/// Ported from Codex `codex-rs/app-server/tests/suite/v2/turn_steer.rs:258`,
/// reference implementation, and
/// Kimi Code `packages/node-sdk/test/session-prompt-input.test.ts:59`.
#[tokio::test]
async fn custom_message_steer_delivery_preserves_tool_pairing_and_reaches_next_step() {
    local(async {
        let env = TestEnv::new().await;
        // Turn 1 emits a tool call; Turn 2 (right after tool_result + mid-turn steer) echoes last user/custom msg.
        let script = json!([
            { "tool_calls": [{ "id": "c_steer", "name": "slow_gate", "arguments": {} }] },
            {}
        ]);
        let h = env.host().builtins(&["message"]).faux(script).build().await;
        expose(&h);
        global(&h, "M", h.service_table("message").unwrap());

        exec(
            &h,
            r#"
            local tx, rx = mare.task.channel()
            local rtx, rrx = mare.task.channel()
            GATE_START_RX = rx
            GATE_RELEASE_TX = rtx
            T:register("slow_gate", {
                description = "blocks until released",
                parameters = { type = "object", properties = {} },
                side_effect = "read",
                execute = function()
                    tx:send(true)
                    rrx:recv()
                    return { content = { { type = "text", text = "gate-ok" } } }
                end,
            })
            "#,
        )
        .await;

        let res = eval_json(
            &h,
            r#"
            STEER_STATUS = nil
            mare.task.spawn(function()
                GATE_START_RX:recv()
                local st = M:append({
                    type = "ctx/steer",
                    content = "steer-note",
                    display = false,
                    delivery = { deliver_as = "steer" },
                })
                STEER_STATUS = st.status
                GATE_RELEASE_TX:send(true)
            end)
            local r = A:wait(A:prompt("start-steer").turn_id)
            assert(STEER_STATUS == "queued", tostring(STEER_STATUS))
            assert(not r.error, tostring(r.error))
            return r
            "#,
        )
        .await;
        assert_eq!(res["text"], "echo: steer-note", "steer custom message must drive turn 2: {res}");

        let msgs = eval_json(&h, "return A:list_messages().data").await;
        let arr = msgs.as_array().unwrap();
        assert_eq!(arr.len(), 5, "expected user, assistant(tool_call), tool_result, custom(steer), assistant: {msgs}");
        assert_eq!(arr[0]["role"], "user");
        assert_eq!(arr[1]["role"], "assistant");
        assert_eq!(arr[2]["role"], "tool_result");
        assert_eq!(arr[3]["role"], "custom");
        assert_eq!(arr[3]["custom_type"], "ctx/steer");
        assert_eq!(arr[4]["role"], "assistant");
        assert_eq!(arr[4]["content"][0]["text"], "echo: steer-note");
        h.shutdown().await;

        // Also verify RPC `message.append` with `deliverAs: "steer"` during an in-flight host tool call.
        let cfg_dir = env.config_dir();
        std::fs::create_dir_all(cfg_dir.join("plugins")).unwrap();
        std::fs::write(
            cfg_dir.join("plugins/c10_steer_rpc.lua"),
            r#"return { name = "c10_steer_rpc", inject = { "message" }, apply = function() end }"#,
        )
        .unwrap();
        let rpc_script = env.home.path().join("faux_rpc_steer.json");
        std::fs::write(
            &rpc_script,
            json!([
                { "tool_calls": [{ "id": "rc1", "name": "host_gate", "arguments": {} }] },
                {}
            ])
            .to_string(),
        )
        .unwrap();
        let mut cmd = mare_cmd();
        cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
            .env("MARE_HOME", env.home.path())
            .env("MARE_FAUX_SCRIPT", &rpc_script)
            .current_dir(&env.cwd)
            .stderr(Stdio::null());
        let mut rpc = RpcClient::spawn_cmd(&mut cmd);
        let init = rpc.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": { "callbacks": true } }), |_| {
            json!(null)
        });
        assert!(init["error"].is_null(), "{init}");
        let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
        let reg = rpc.call_raw(
            "tools.register",
            json!({ "name": "host_gate", "description": "gate", "parameters": { "type": "object", "properties": {} }, "side_effect": "read", "callbacks": ["execute"] }),
            |_| json!(null),
        );
        assert!(reg["error"].is_null(), "{reg}");

        let prompt_resp = rpc.call_raw("agent.prompt", json!({ "session_id": sid, "input": "go" }), |_| json!(null));
        assert!(prompt_resp["error"].is_null(), "{prompt_resp}");
        let tool_cb = loop {
            let m = rpc.read();
            if m.get("method").and_then(Value::as_str) == Some("callback.call")
                && m["params"].get("callback").and_then(Value::as_str) == Some("execute")
            {
                break m;
            }
        };
        let steer_id = json!(rpc.request(
            "message.append",
            json!({
                "session_id": sid,
                "type": "rpc/steer",
                "content": "rpc-steer-note",
                "display": false,
                "delivery": { "deliver_as": "steer" }
            }),
        ));
        let steer_resp = loop {
            let m = rpc.read();
            if m["id"] == steer_id {
                break m;
            }
        };
        assert_eq!(steer_resp["result"]["status"], "queued", "{steer_resp}");
        assert!(steer_resp["result"].get("ok").is_none(), "{steer_resp}");
        rpc.send(json!({ "jsonrpc": "2.0", "id": tool_cb["id"], "result": { "content": "gate-done" } }));
        let wait_resp = rpc.call_raw(
            "agent.wait",
            json!({ "session_id": sid, "turn_id": prompt_resp["result"]["turn_id"] }),
            |_| json!(null),
        );
        assert_eq!(wait_resp["result"]["text"], "echo: rpc-steer-note", "{wait_resp}");

        rpc.shutdown_and_wait();
    })
    .await;
}

/// Ported from reference implementation
/// and Codex `codex-rs/tui/tests/suite/vt100_live_commit.rs:6` (compact vs expanded view + Ctrl+T).
#[tokio::test]
async fn custom_message_and_entry_renderers_populate_expanded_nodes_and_ctrl_t_transcript() {
    local(async {
        {
            let env = TestEnv::new().await;
            let (h, mut rx, _ui) = tui_host(&env, json!([])).await;
            expose(&h);
            global(&h, "M", h.service_table("message").unwrap());
            global(&h, "U", h.service_table("ui").unwrap());

            exec(
                &h,
                r#"
                M:register_type("audit/exp", {
                    render = function(msg, opts)
                        local prefix = opts.expanded and "[AUDIT FULL] " or "[AUDIT] "
                        return ui.Line({ ui.Span(prefix .. (msg.content[1].text or "")):hl("MareAccent") })
                    end,
                })
                U:register_entry_renderer("milestone/exp", {
                    compact = function(entry)
                        return ui.Line({ ui.Span("★ " .. tostring(entry.data.label)):hl("MareAccent") })
                    end,
                    expanded = function(entry)
                        return ui.Line({ ui.Span("★ FULL " .. tostring(entry.data.label)):hl("MareAccent") })
                    end,
                })
                U:register_entry_renderer("milestone/exp_err", {
                    compact = function(entry)
                        return ui.Line({ ui.Span("COMPACT_SURVIVES " .. tostring(entry.data.label)):hl("MareAccent") })
                    end,
                    expanded = function()
                        error("expanded-boom")
                    end,
                })
                M:append({ type = "audit/exp", content = "vis-item", display = true })
                assert(S:add_record("milestone/exp", { label = "phase-x" }, { persist = true }))
                assert(S:add_record("milestone/exp_err", { label = "safe" }, { persist = true }))
                "#,
            )
            .await;

            let blocks = collect_pushed_blocks(&drain_cmds(&mut rx));
            assert!(
                blocks.iter().any(|(_, k, c, e)| k == "custom"
                    && c.contains("[AUDIT] vis-item")
                    && e.as_deref().is_some_and(|s| s.contains("[AUDIT FULL] vis-item"))),
                "custom message must populate both compact and expanded nodes: {blocks:?}"
            );
            assert!(
                blocks.iter().any(|(_, k, c, e)| k == "entry"
                    && c.contains("★ phase-x")
                    && e.as_deref().is_some_and(|s| s.contains("★ FULL phase-x"))),
                "entry renderer must populate both compact and expanded nodes: {blocks:?}"
            );
            assert!(
                blocks.iter().any(|(_, k, c, _)| k == "entry" && c.contains("COMPACT_SURVIVES safe")),
                "error in expanded renderer must not drop compact block: {blocks:?}"
            );
            h.shutdown().await;
        }

        // Real TUI Vt: wait for startup, render compact custom message & entry, then press Ctrl+T.
        let env2 = TestEnv::new().await;
        let mut b = env2.host().builtins(&["message"]).faux(json!([])).config(json!({
            "logo": false,
            "plugins": { "permissions": { "enabled": true }, "onboarding": { "plugins": false } }
        }));
        b.opts.mode = Mode::Tui;
        let mut vt = mare_cli::testkit::Vt::new(b.opts, 100, 30).await.unwrap();
        let nz = |n: u16| std::num::NonZeroU16::new(n).unwrap();
        let mut parser = vt100::Parser::new(nz(30), nz(100), 500);
        let start_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while vt.starting() {
            vt.frame().await;
            parser.process(&vt.output());
            assert!(std::time::Instant::now() < start_deadline, "startup never settled:\n{}", parser.screen().contents());
            tokio::time::sleep(std::time::Duration::from_millis(15)).await; // sleep-ok: poll startup completion
        }

        expose(&vt.host);
        global(&vt.host, "M", vt.host.service_table("message").unwrap());
        global(&vt.host, "U", vt.host.service_table("ui").unwrap());
        exec(
            &vt.host,
            r#"
            M:register_type("tui/exp", {
                render = function(msg, opts)
                    local prefix = opts.expanded and "[TUI_FULL] " or "[TUI_COMPACT] "
                    return ui.Line({ ui.Span(prefix .. (msg.content[1].text or "")):hl("MareAccent") })
                end,
            })
            U:register_entry_renderer("tui/entry", {
                compact = function(entry) return ui.Line({ ui.Span("COMPACT_" .. entry.data.v):hl("MareAccent") }) end,
                expanded = function(entry) return ui.Line({ ui.Span("EXPANDED_" .. entry.data.v):hl("MareAccent") }) end,
            })
            M:append({ type = "tui/exp", content = "note1", display = true })
            assert(S:add_record("tui/entry", { v = "row1" }, { persist = true }))
            "#,
        )
        .await;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            vt.frame().await;
            parser.process(&vt.output());
            let scr = parser.screen().contents();
            if scr.contains("[TUI_COMPACT] note1") && scr.contains("COMPACT_row1") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "compact blocks never appeared:\n{scr}");
            tokio::time::sleep(std::time::Duration::from_millis(15)).await; // sleep-ok: poll virtual terminal frame until rendered
        }
        vt.key(mare_tui::tty::KeyEvent::new(
            mare_tui::tty::KeyCode::Char('t'),
            mare_tui::tty::Modifiers::CONTROL,
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            vt.frame().await;
            parser.process(&vt.output());
            let scr = parser.screen().contents();
            if scr.contains("[TUI_FULL] note1") && scr.contains("EXPANDED_row1") {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "expanded blocks never appeared in Ctrl+T:\n{scr}");
            tokio::time::sleep(std::time::Duration::from_millis(15)).await; // sleep-ok: poll expanded transcript overlay until rendered
        }
        vt.shutdown().await;
    })
    .await;
}
