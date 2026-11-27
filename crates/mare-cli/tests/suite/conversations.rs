//! Integration tests for `SES-FORK` (`session.snapshot` / `session.fork { switch = false }` /
//! `session:snapshot_context`) and the `conversations` side-conversation service.
use crate::common::*;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::process::Stdio;
use std::time::{Duration, Instant};

const PNG_1X1_B64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO7+X8cAAAAASUVORK5CYII=";

#[tokio::test]
async fn conversations_service_side_roundtrip_isolates_parent_history() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {"text": "parent answer"},
                {"text": "side answer"}
            ]))
            .build()
            .await;
        expose(&h);

        h.prompt("main question", json!({})).await.unwrap();

        exec(
            &h,
            r#"
            local conversations = C.cordis:get("conversations")
            assert(conversations ~= nil, "conversations service must be available")
            assert(conversations.send == nil, "legacy blocking conversations.send must be removed")
            local parent_count = #A:messages()

            local info, err = conversations:start()
            assert(info ~= nil, "conversations:start() failed: " .. tostring(err))
            assert(info.selected == true and info.busy == false)

            local focused_state = A:get()
            assert(focused_state.side_id == nil, "agent.get must not expose side_id")
            assert(focused_state.parent_status == nil, "agent.get must not expose parent_status")
            assert(focused_state.params == nil, "agent.get must not expose params")
            assert(type(focused_state.activity) == "table" and type(focused_state.activity.extra) == "table" and type(focused_state.activity.extra_errors) == "table", mare.json.encode(focused_state))

            local started = A:prompt("side question")
            assert(started and started.status == "started" and type(started.turn_id) == "string", mare.json.encode(started))
            local reply = A:wait(started.turn_id)
            assert(reply and reply.status == "completed" and reply.text == "side answer", mare.json.encode(reply))

            local after = conversations:info()
            assert(after ~= nil and after.message_count > parent_count)
            assert(#A:messages() == parent_count, "side conversation mutated parent history")

            assert(conversations:close() == true)
            assert(conversations:info() == nil)
        "#,
        )
        .await;

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn conversations_cancel_reports_cancelled_in_result_and_event() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                {"text": "parent answer"},
                {"tool_calls": [{"id": "side-call", "name": "hold_side", "arguments": {}}]}
            ]))
            .build()
            .await;
        expose(&h);
        h.prompt("parent question", json!({})).await.unwrap();
        exec(
            &h,
            r#"
            local entered_tx, entered_rx = mare.task.channel()
            local _, hold_rx = mare.task.channel()
            T:register("hold_side", {
                side_effect = "read", description = "hold side turn", parameters = { type = "object" },
                execute = function() entered_tx:send(true); hold_rx:recv() end,
            })
            local settled
            C.cordis:on("conversation/settled", function(event) settled = event end)
            local conv = C.cordis:get("conversations")
            assert(conv:start())
            local started = A:prompt("side question")
            assert(started and started.status == "started" and type(started.turn_id) == "string", mare.json.encode(started))
            assert(mare.task.timeout(5000, function() return entered_rx:recv() end), "side tool never started")
            local cancel_res = A:cancel({ turn_id = started.turn_id })
            assert(type(cancel_res) == "table" and cancel_res.turn_id == started.turn_id and type(cancel_res.removed) == "table" and cancel_res.side_only == nil, mare.json.encode(cancel_res))
            local result = A:wait(started.turn_id)
            assert(result and result.status == "cancelled" and type(result.error) == "table" and result.error.code == "cancelled", mare.json.encode(result))
            assert(settled and type(settled.error) == "table" and settled.error.code == "cancelled", mare.json.encode(settled))
            "#,
        )
        .await;
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn ses_fork_non_switching_snapshot_mid_turn_synthesizes_tools_and_preserves_attachments() {
    local(async {
        let env = TestEnv::new().await;
        // Write a tiny empty/no-session host check first: empty session must reject with `no_history`.
        {
            let empty_host = env.host().no_session().faux(json!([])).build().await;
            expose(&empty_host);
            let err_code: String = eval(
                &empty_host,
                r#"
                local ok, err = pcall(function()
                    return S:snapshot({ session_id = S:info().session_id })
                end)
                assert(not ok, "empty no-history session must fail snapshot")
                return type(err) == "table" and err.code or tostring(err)
                "#,
            )
            .await;
            assert_eq!(err_code, "session.no_history");
            empty_host.shutdown().await;
        }

        let h = env
            .host()
            .faux(json!([
                { "text": "seeded parent reply" },
                { "tool_calls": [{ "id": "call_mid_1", "name": "pause_parent", "arguments": {} }] },
                { "text": "parent finished normally" }
            ]))
            .build()
            .await;
        expose(&h);

        // Turn 1 with image attachment so we can verify independent attachment hydration.
        let r1 = h
            .prompt("turn 1 with image", json!({ "images": [{ "type": "image", "data": PNG_1X1_B64, "mime": "image/png" }] }))
            .await
            .unwrap();
        assert_eq!(r1["text"], "seeded parent reply");

        exec(
            &h,
            r#"
            ENTERED_TX, ENTERED_RX = mare.task.channel()
            RELEASE_TX, RELEASE_RX = mare.task.channel()
            T:register("pause_parent", {
                side_effect = "read",
                description = "pauses mid-turn so session.snapshot observes started call",
                parameters = { type = "object" },
                execute = function()
                    ENTERED_TX:send(true)
                    RELEASE_RX:recv()
                    return { content = "live tool ok" }
                end,
            })
            PARENT_ID = S:info().session_id
            PARENT_TASK = mare.task.spawn(function()
                return A:wait(A:prompt("turn 2 mid-flight").turn_id)
            end)
            assert(mare.task.timeout(5000, function() return ENTERED_RX:recv() end), "pause_parent never started")
            A:steer("queued steer must not leak")
            "#,
        )
        .await;

        // Take a non-switching snapshot AND non-switching session.fork while parent turn is mid-flight.
        exec(
            &h,
            r#"
            SNAP = S:snapshot({
                session_id = PARENT_ID,
                model = "faux/other",
                effort = "low",
                name = "snap-mid",
                request_id = "req-snap-1",
            })
            assert(SNAP.session_id ~= PARENT_ID)
            assert(SNAP.parent_session_id == PARENT_ID)
            assert(SNAP.loaded == false)
            assert(SNAP.name == "snap-mid")
            -- Active session and running turn must be untouched.
            assert(S:info().session_id == PARENT_ID, "session.snapshot must not switch active session")
            assert(A:get().busy == true, "session.snapshot must not cancel active parent turn")

            -- Idempotent replay vs conflict.
            local replay = S:snapshot({
                session_id = PARENT_ID,
                model = "faux/other",
                effort = "low",
                name = "snap-mid",
                request_id = "req-snap-1",
            })
            assert(replay.session_id == SNAP.session_id)
            local ok_conflict, err_conflict = pcall(function()
                S:snapshot({ session_id = PARENT_ID, name = "different", request_id = "req-snap-1" })
            end)
            assert(not ok_conflict and err_conflict.code == "request_id_conflict")

            -- Unknown before_item_id rejection.
            local ok_bad, err_bad = pcall(function()
                S:snapshot({ session_id = PARENT_ID, before_item_id = "missing-item" })
            end)
            assert(not ok_bad and err_bad.code == "session.unknown_item", tostring(err_bad))

            -- session.snapshot creates a second non-switching cold snapshot; session.fork no longer accepts switch=false.
            FORK_NOSWITCH = S:snapshot({
                session_id = PARENT_ID,
                name = "fork-noswitch",
            })
            assert(FORK_NOSWITCH.session_id ~= PARENT_ID and FORK_NOSWITCH.session_id ~= SNAP.session_id)
            assert(FORK_NOSWITCH.loaded == false and FORK_NOSWITCH.session_epoch == nil)
            local ok_old_switch = pcall(function()
                S:fork({ session_id = PARENT_ID, switch = false })
            end)
            assert(not ok_old_switch)
            assert(S:info().session_id == PARENT_ID)

            -- session.snapshot honors expected_epoch in Lua just like session.fork / close / rewind.
            local ok_bad_epoch_type, err_bad_epoch_type = pcall(function()
                S:snapshot({ session_id = PARENT_ID, expected_epoch = 123 })
            end)
            assert(not ok_bad_epoch_type and err_bad_epoch_type.code == "invalid_arguments", tostring(err_bad_epoch_type))
            local ok_stale_epoch, err_stale_epoch = pcall(function()
                S:snapshot({ session_id = PARENT_ID, expected_epoch = "stale-epoch" })
            end)
            assert(not ok_stale_epoch and err_stale_epoch.code == "stale_session", tostring(err_stale_epoch))
            local snap_with_epoch = S:snapshot({
                session_id = PARENT_ID,
                expected_epoch = C.cordis:get("kernel"):session_epoch(),
                name = "snap-epoch-ok",
            })
            assert(snap_with_epoch.parent_session_id == PARENT_ID)
            "#,
        )
        .await;

        // Release parent turn and confirm it settles cleanly on its original session
        // (processing the queued steer that stays solely on the parent).
        exec(
            &h,
            r#"
            RELEASE_TX:send(true)
            local res = PARENT_TASK:wait()
            assert(res and res.text == "parent finished normally", mare.json.encode(res))
            local saw_steer = false
            for _, m in ipairs(res.messages or {}) do
                if m.role == "user" and m.content and m.content[1] and m.content[1].text == "queued steer must not leak" then
                    saw_steer = true
                end
            end
            assert(saw_steer, mare.json.encode(res.messages))
            assert(S:info().session_id == PARENT_ID)
            "#,
        )
        .await;

        // Switch away, delete parent (removing parent's attachments directory), and load SNAP:
        // SNAP must still load intact with synthesized tool result, <turn_aborted>, model/effort
        // override, and independent attachment hydration, without the parent's steer queue.
        exec(
            &h,
            r#"
            local snap_eff = S:snapshot({ session_id = PARENT_ID, model = "faux/echo", effort = "low", name = "snap-eff" })
            S:create({ name = "scratch" })
            S:delete(PARENT_ID)
            local loaded = S:load({ session_id = SNAP.session_id })
            assert(loaded.session_id == SNAP.session_id and loaded.loaded == true)
            local L = C.cordis:get("llm")
            assert(L:current().ref == "faux/other", tostring(L:current().ref))
            local has_effort_entry = false
            for _, e in ipairs(S:entries()) do
                if e.type == "model" and e.model == "faux/other" and e.effort == "low" then
                    has_effort_entry = true
                end
            end
            assert(has_effort_entry, "snapshot must persist requested effort entry")
            -- Also verify live effort restoration against an effort-capable model.
            S:load({ session_id = snap_eff.session_id })
            assert(L:current().ref == "faux/echo", tostring(L:current().ref))
            assert(L:raw_effort() == "low", tostring(L:raw_effort()))
            S:load({ session_id = SNAP.session_id })
            local found_aborted, found_tool_synth, found_img, found_steer = false, false, false, false
            for _, m in ipairs(A:messages()) do
                if m.role == "tool_result" and m.tool_name == "pause_parent" then
                    for _, b in ipairs(m.content or {}) do
                        if b.type == "text" and b.text:find("interrupted: this call did not finish", 1, true) then
                            found_tool_synth = true
                        end
                    end
                end
                for _, b in ipairs(m.content or {}) do
                    if b.type == "text" and b.text:find("<turn_aborted>", 1, true) then
                        found_aborted = true
                    end
                    if b.type == "text" and b.text:find("queued steer must not leak", 1, true) then
                        found_steer = true
                    end
                    if b.type == "image" then
                        if type(b.data) == "string" and #b.data > 0 then
                            found_img = true
                        elseif type(b.attachment) == "string" and b.attachment ~= "" then
                            local bytes = mare.store.read_attachment(loaded.path, b.attachment, 1024 * 1024)
                            assert(type(bytes) == "string" and #bytes > 0, "copied snapshot attachment must be readable")
                            found_img = true
                        end
                    end
                end
            end
            assert(not found_steer, "snapshot must not inherit parent steer queue")
            assert(found_tool_synth, "mid-turn snapshot must synthesize interrupted/unknown tool_result")
            assert(found_aborted, "mid-turn snapshot must close turn with <turn_aborted>")
            assert(found_img, "snapshot attachment must survive parent deletion")
            local recs = S:list_records("agent/run-end").data
            assert(#recs >= 1, "mid-turn snapshot must record agent/run-end")
            "#,
        )
        .await;

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn conversations_side_permissions_and_ephemeral_read_only_confinement() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .config(json!({ "model": "faux/echo", "plugins": { "permissions": { "enabled": true } } }))
            .faux(json!([
                { "text": "parent baseline" },
                { "tool_calls": [
                    { "id": "call_side_read", "name": "side_read_probe", "arguments": {} },
                    { "id": "call_side_write", "name": "side_write_probe", "arguments": {} }
                ] },
                { "text": "side finished with read only" }
            ]))
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("seed parent", json!({})).await.unwrap();
        assert_eq!(r1["text"], "parent baseline");

        exec(
            &h,
            r#"
            READ_LIFETIME = nil
            WRITE_EXECUTED = false
            T:register("side_read_probe", {
                side_effect = "read",
                description = "confined read probe",
                parameters = { type = "object" },
                execute = function()
                    local ec = mare.exec.current()
                    READ_LIFETIME = ec and ec.lifetime or nil
                    return { content = "read-ok" }
                end,
            })
            T:register("side_write_probe", {
                side_effect = "write",
                description = "write tool blocked in side conversation",
                parameters = { type = "object" },
                execute = function()
                    WRITE_EXECUTED = true
                    return { content = "should-never-run" }
                end,
            })
            local conv = C.cordis:get("conversations")
            local info, err = conv:start()
            assert(info ~= nil, tostring(err))
            local started = A:prompt("use tools")
            local reply = A:wait(started.turn_id)
            assert(reply and reply.text == "side finished with read only", mare.json.encode(reply))
            assert(
                READ_LIFETIME == "ephemeral-read-only",
                "expected ephemeral-read-only lifetime, got: " .. tostring(READ_LIFETIME) .. " reply=" .. mare.json.encode(reply)
            )
            assert(WRITE_EXECUTED == false, "side conversation must deny non-read tool execution")
            assert(conv:close() == true)
            "#,
        )
        .await;

        h.shutdown().await;
    })
    .await;
}

#[test]
fn conversations_rpc_abort_and_stale_turn_id_do_not_cancel_parent_approval() {
    let home = tempfile::tempdir().unwrap();
    let pdir = home.path().join("config/plugins");
    std::fs::create_dir_all(&pdir).unwrap();
    std::fs::write(
        pdir.join("rpc-ask.lua"),
        r#"
return {
    name = "rpc-ask-probe",
    inject = { "tools", "policy" },
    apply = function(ctx)
        ctx.tools:register("parent_ask_tool", {
            side_effect = "write",
            description = "parent write tool gated by approval",
            parameters = { type = "object" },
            execute = function()
                return { content = "parent-approved-ok" }
            end,
        })
        ctx.policy:voter("tool/before", function(call)
            if call.name == "parent_ask_tool" then
                return { decision = "ask", reason = "needs parent approval" }
            end
        end)
    end,
}
"#,
    )
    .unwrap();
    let script = home.path().join("script.json");
    std::fs::write(
        &script,
        json!([
            {"tool_calls": [{"id": "call_parent_ask", "name": "parent_ask_tool", "arguments": {}}]},
            {"text": "parent completed after approval"}
        ])
        .to_string(),
    )
    .unwrap();

    let mut child = mare_cmd()
        .args(["--rpc", "--permission", "read-only", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &script)
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(output).lines().map_while(Result::ok) {
            if tx.send(serde_json::from_str::<Value>(&line).unwrap()).is_err() {
                break;
            }
        }
    });
    let send = |input: &mut std::process::ChildStdin, msg: Value| {
        writeln!(input, "{msg}").unwrap();
        input.flush().unwrap();
    };
    let next = |stage: &str, timeout: Duration| {
        rx.recv_timeout(timeout).unwrap_or_else(|e| panic!("RPC timed out waiting for {stage}: {e:?}"))
    };

    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocol_version": "3.0", "capabilities":{"approvals":true}}}),
    );
    let sid = loop {
        let m = next("initialize", Duration::from_secs(30));
        if m["id"] == 1 {
            break m["result"]["session"]["session_id"].as_str().expect("initialize session_id").to_string();
        }
    };

    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":2,"method":"agent.prompt","params":{"session_id":sid,"input":"start parent"}}),
    );
    let ask = loop {
        let m = next("approval.request", Duration::from_secs(10));
        if m["method"] == "approval.request" {
            break m;
        }
    };
    let ask_id = ask["id"].clone();

    // Start side conversation while parent waits on approval.request.
    send(&mut input, json!({"jsonrpc":"2.0","id":10,"method":"commands.run","params":{"line":"/side"}}));
    loop {
        let m = next("commands.run /side", Duration::from_secs(10));
        if m["id"] == 10 {
            assert!(m.get("error").is_none(), "commands.run /side failed: {m}");
            break;
        }
    }

    // Stale turn_id on agent.cancel must not cancel parent or pending approval.
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":11,"method":"agent.cancel","params":{"session_id":sid,"turn_id":"stale-wrong-turn"}}),
    );
    while next("stale agent.cancel", Duration::from_secs(10))["id"] != 11 {}

    // Closing side conversation via `/side close` preserves parent approval.
    send(&mut input, json!({"jsonrpc":"2.0","id":12,"method":"commands.run","params":{"line":"/side close"}}));
    loop {
        let m = next("abort response", Duration::from_secs(10));
        assert_ne!(m["method"], "$/cancel_request", "side abort must not cancel parent approval.request: {m}");
        if m["id"] == 12 {
            assert!(m.get("error").is_none(), "{m}");
            break;
        }
    }

    // Confirm parent is still busy waiting on the approval, then approve it.
    send(&mut input, json!({"jsonrpc":"2.0","id":13,"method":"agent.get","params":{"session_id":sid}}));
    loop {
        let m = next("agent.get", Duration::from_secs(10));
        assert_ne!(m["method"], "$/cancel_request", "{m}");
        if m["id"] == 13 {
            assert_eq!(m["result"]["busy"], true, "parent turn must still be awaiting approval: {m}");
            break;
        }
    }
    send(&mut input, json!({"jsonrpc":"2.0","id":ask_id,"result":{"decision":"allow"}}));
    let mut trace = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let rem = deadline.saturating_duration_since(Instant::now());
        let m = rx
            .recv_timeout(rem)
            .unwrap_or_else(|e| panic!("timeout waiting for parent prompt completion ({e:?}); trace={trace:?}"));
        if m["method"] == "event" && m["params"]["type"] == "turn/completed" {
            assert_eq!(m["params"]["data"]["status"], "completed", "{m}");
            assert_eq!(m["params"]["data"]["termination_reason"], "completed", "{m}");
            assert_eq!(m["params"]["data"]["text"], "parent completed after approval", "{m}");
            break;
        }
        trace.push(m);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let _ = deadline;
    send(&mut input, json!({"jsonrpc":"2.0","id":99,"method":"shutdown","params":{}}));
    let _ = child.wait();
}

#[tokio::test]
async fn conversations_submit_isolation_fifo_settled_projection_and_discard_on_leave() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "turn1-answer" },
                { "text": "turn2-answer" },
                { "text": "side-1 sk-side-secret-xyz" },
                { "text": "side-2 ok" },
                { "text": "held side turn" }
            ]))
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("FIRST-PARENT-MSG", json!({})).await.unwrap();
        assert_eq!(r1["text"], "turn1-answer");
        let r2 = h.prompt("SECOND-PARENT-MSG", json!({})).await.unwrap();
        assert_eq!(r2["text"], "turn2-answer");

        exec(
            &h,
            r#"
            mare.projection.register("sk-side-secret-xyz", "[REDACTED]")
            local conv = C.cordis:get("conversations")
            local ui_svc = C.cordis:get("ui")
            local cmds = C.cordis:get("commands")

            -- Gap #5: context/transform strips FIRST-PARENT-MSG from parent effective request,
            -- yet side conversation starts from full projected session snapshot history.
            C.cordis:on("context/transform", function(ev)
                local out = {}
                for _, m in ipairs(ev.messages or {}) do
                    local txt = type(m.content) == "string" and m.content or ""
                    if not txt:find("FIRST-PARENT-MSG", 1, true) then
                        out[#out + 1] = m
                    end
                end
                return out
            end)

            SETTLED = {}
            C.cordis:on("conversation/settled", function(ev)
                SETTLED[#SETTLED + 1] = ev
            end)

            local info, err = conv:start()
            assert(info ~= nil, tostring(err))
            -- Verify snapshot seed preserved FIRST-PARENT-MSG.
            assert(info.message_count >= 4, "expected full parent history in side seed: " .. tostring(info.message_count))

            -- Gap #3: while side is focused, parent-mutating slash command & `!` shell are rejected
            -- cleanly without running against parent.
            if ui_svc and ui_svc.submit then
                local ev_slash = { text = "/retry" }
                ui_svc:submit(ev_slash)
                assert(ev_slash.command_result and ev_slash.command_result.status == "rejected", "slash command must be rejected in side")
                local ev_bang = { text = "!echo nope" }
                ui_svc:submit(ev_bang)
                assert(ev_bang.command_result and ev_bang.command_result.status == "rejected", "! shell must be rejected in side")
            end

            -- Gap #4 & #7: queue two FIFO side prompts and verify non-blocking agent.prompt + agent.wait and projected settled events.
            local s1 = A:prompt("q1 sk-side-secret-xyz")
            assert(s1 and s1.status == "started" and type(s1.turn_id) == "string", mare.json.encode(s1))
            local s2 = A:prompt("q2 follow")
            assert(s2 and (s2.status == "queued" or s2.status == "started") and type(s2.turn_id) == "string", mare.json.encode(s2))
            local res1 = A:wait(s1.turn_id)
            local res2 = A:wait(s2.turn_id)
            assert(res1 and res1.turn_id == s1.turn_id and res1.status == "completed", mare.json.encode(res1))
            assert(res2 and res2.turn_id == s2.turn_id and res2.status == "completed", mare.json.encode(res2))
            assert(#SETTLED == 2, "expected 2 conversation/settled events, got " .. tostring(#SETTLED))
            assert(SETTLED[1].turn_id == s1.turn_id and SETTLED[2].turn_id == s2.turn_id)
            local enc = mare.json.encode(SETTLED)
            assert(not enc:find("sk-side-secret-xyz", 1, true), "secret leaked in conversation/settled: " .. enc)
            assert(SETTLED[1].prompt and SETTLED[1].prompt:find("[[mare-value:", 1, true), enc)
            assert(SETTLED[1].text and SETTLED[1].text:find("[[mare-value:", 1, true), enc)
            assert(type(SETTLED[1].message) == "table", enc)

            -- Gap #6: /side back discards ephemeral side conversation without mutating parent.
            local parent_before = #A:messages()
            cmds:run("/side back")
            assert(conv:info() == nil, "/side back must discard side conversation")
            assert(#A:messages() == parent_before, "side operations must not mutate parent history")
            "#,
        )
        .await;

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn ses_fork_snapshot_does_not_disarm_parent_exec_gate_for_subsequent_tools() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "seeded parent" },
                { "tool_calls": [{ "id": "call_post_snap_1", "name": "post_snap_tool", "arguments": {} }] },
                { "text": "parent tool after snapshot succeeded" }
            ]))
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("seed parent", json!({})).await.unwrap();
        assert_eq!(r1["text"], "seeded parent");

        exec(
            &h,
            r#"
            POST_SNAP_RAN = false
            T:register("post_snap_tool", {
                side_effect = "read",
                description = "runs after session.snapshot to verify parent exec_gate stays armed",
                parameters = { type = "object" },
                execute = function()
                    local ec = mare.exec.current()
                    assert(ec and ec.tool == "post_snap_tool", "expected active execution context")
                    local entries = mare.fs.list(mare.cwd)
                    assert(type(entries) == "table")
                    POST_SNAP_RAN = true
                    return { content = "post-snap-tool-ok" }
                end,
            })
            local snap = S:snapshot({ session_id = S:info().session_id, name = "aux-snap" })
            assert(snap and snap.session_id ~= S:info().session_id)
            "#,
        )
        .await;

        let r2 = h.prompt("run tool after snapshot", json!({})).await.unwrap();
        assert_eq!(r2["text"], "parent tool after snapshot succeeded", "{r2}");
        assert!(eval::<bool>(&h, "return POST_SNAP_RAN").await, "post-snapshot parent tool did not run");

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn conversations_btw_unifies_with_side_and_hydrates_persisted_parent_images() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .faux(json!([
                { "text": "parent saw image" }
            ]))
            .build()
            .await;
        expose(&h);

        let r1 = h
            .prompt("remember this image", json!({ "images": [{ "type": "image", "data": PNG_1X1_B64, "mime": "image/png" }] }))
            .await
            .unwrap();
        assert_eq!(r1["text"], "parent saw image");

        exec(
            &h,
            r#"
            -- Reload session so attachments load with defer_hydration = true (`attachment` present, `data` nil).
            local sid = S:info().session_id
            S:load({ session_id = sid })

            local L = C.cordis:get("llm")
            CAPTURED_IMAGE = nil
            CAPTURED_ATTACHMENT = nil
            local STREAM_DONE_TX, STREAM_DONE_RX = mare.task.channel()
            L:register_provider({
                provider = "imgprobe",
                is_configured = function() return true end,
                models = {
                    { model_id = "m", context_window = 128000, max_output = 4096 },
                },
                stream = function(req, emit)
                    for _, m in ipairs(req.messages or {}) do
                        if type(m.content) == "table" then
                            for _, b in ipairs(m.content) do
                                if b.type == "image" then
                                    CAPTURED_IMAGE = b.data
                                    CAPTURED_ATTACHMENT = b.attachment
                                end
                            end
                        end
                    end
                    STREAM_DONE_TX:send(true)
                    if emit then
                        emit({ type = "text_delta", delta = "btw hydrated answer" })
                    end
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "btw hydrated answer" } } }
                end,
            })
            assert(L:set_model("imgprobe/m"))

            local conv = C.cordis:get("conversations")
            local cmds = C.cordis:get("commands")
            assert(type(conv.cancel) == "function", "conversations.cancel method must exist per vocabulary")

            -- `/btw <question>` must route through the unified `conversations` service.
            cmds:run("/btw describe the image")
            assert(mare.task.timeout(5000, function() return STREAM_DONE_RX:recv() end), "/btw never invoked provider stream")
            local info = conv:info()
            assert(info ~= nil and info.selected == true, "/btw must open the conversations side session")
            assert(CAPTURED_ATTACHMENT == nil, "hydrated request must clear disk attachment handle")
            assert(type(CAPTURED_IMAGE) == "string" and #CAPTURED_IMAGE > 0, "persisted image was not hydrated for /btw /side")
            assert(CAPTURED_IMAGE == mare.crypto.base64_encode(mare.crypto.base64_decode(CAPTURED_IMAGE)))

            cmds:run("/btw back")
            assert(conv:info() == nil, "/btw back must close and discard the side conversation")
            "#,
        )
        .await;

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn conversations_side_loop_inserts_synthetic_tool_results_after_owning_assistant() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().config(json!({ "model": "faux/echo" })).faux(json!([])).no_session().build().await;
        expose(&h);
        exec(
            &h,
            r#"
            local L = C.cordis:get("llm")
            CAPTURED_ROLES = {}
            local done_tx, done_rx = mare.task.channel()
            L:register_provider({
                provider = "sideprobe",
                is_configured = function() return true end,
                models = { { model_id = "m", context_window = 128000, max_output = 4096 } },
                stream = function(req, emit)
                    CAPTURED_ROLES = {}
                    for _, m in ipairs(req.messages or {}) do
                        CAPTURED_ROLES[#CAPTURED_ROLES + 1] = m.role .. (m.tool_call_id and (":" .. m.tool_call_id) or "")
                    end
                    done_tx:send(true)
                    if emit then emit({ type = "text_delta", delta = "side ok" }) end
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = "side ok" } } }
                end,
            })
            assert(L:set_model("sideprobe/m"))
            A:set_messages({
                { role = "user", content = { { type = "text", text = "q1" } } },
                { role = "assistant", content = { { type = "tool_call", id = "c_orphan", name = "ls", arguments = {} } } },
                { role = "user", content = { { type = "text", text = "later user" } } },
                { role = "assistant", content = { { type = "text", text = "later ok" } } },
            })
            local cmds = C.cordis:get("commands")
            cmds:run("/btw check order")
            assert(mare.task.timeout(5000, function() return done_rx:recv() end))
            assert(
                CAPTURED_ROLES[1] == "user"
                    and CAPTURED_ROLES[2] == "assistant"
                    and CAPTURED_ROLES[3] == "tool_result:c_orphan"
                    and CAPTURED_ROLES[4] == "user",
                "synthetic tool_result must immediately follow owning assistant: " .. mare.json.encode(CAPTURED_ROLES)
            )
            cmds:run("/btw back")
            "#,
        )
        .await;
        h.shutdown().await;
    })
    .await;
}

/// A `/side` conversation must enforce its confined read-only gate on inner calls dispatched
/// through `call_tool` (whose synthetic ids are `<parent>/nested/<n>`), even under `full-access`
/// where a deferred unconfined MCP-like write tool would otherwise be auto-allowed, and must not
/// persist `tool/nested` records into the main session.
#[tokio::test]
async fn side_conversation_blocks_deferred_unconfined_and_write_tools_via_call_tool() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .project_trusted(true)
            .config(json!({
                "model": "faux/echo",
                "plugins": {
                    "permissions": { "enabled": true, "preset": "full-access" }
                }
            }))
            .faux(json!([
                { "text": "parent baseline" },
                { "tool_calls": [
                    { "id": "call_side_defer_write", "name": "call_tool", "arguments": { "name": "deferred_mcp_write", "arguments": {} } },
                    { "id": "call_side_defer_read", "name": "call_tool", "arguments": { "name": "deferred_confined_read", "arguments": {} } }
                ] },
                { "text": "side finished" }
            ]))
            .build()
            .await;
        expose(&h);

        let r1 = h.prompt("seed parent", json!({})).await.unwrap();
        assert_eq!(r1["text"], "parent baseline");

        exec(
            &h,
            r#"
            DEFERRED_WRITE_RAN = false
            DEFERRED_READ_LIFETIME = nil
            T:register("deferred_mcp_write", {
                exposure = "deferred",
                unconfined = true,
                side_effect = "write",
                description = "deferred unconfined MCP-like write tool",
                parameters = { type = "object" },
                execute = function()
                    DEFERRED_WRITE_RAN = true
                    return { content = "mcp-write-ran" }
                end,
            })
            T:register("deferred_confined_read", {
                exposure = "deferred",
                side_effect = "read",
                description = "deferred confined read tool",
                parameters = { type = "object" },
                execute = function()
                    local ec = mare.exec.current()
                    DEFERRED_READ_LIFETIME = ec and ec.lifetime or nil
                    return { content = "deferred-read-ok" }
                end,
            })
            local conv = C.cordis:get("conversations")
            local info, err = conv:start()
            assert(info ~= nil, tostring(err))
            local started = A:prompt("invoke deferred tools")
            local reply = A:wait(started.turn_id)
            assert(reply and reply.text == "side finished", mare.json.encode(reply))
            assert(DEFERRED_WRITE_RAN == false, "side conversation must deny deferred unconfined write tool via call_tool")
            local encoded_msgs = mare.json.encode(reply.messages or {})
            assert(
                encoded_msgs:find("Side conversations require confined read-only exploration", 1, true),
                "expected side voter denial reason in tool_result: " .. encoded_msgs
            )
            assert(
                DEFERRED_READ_LIFETIME == "ephemeral-read-only",
                "deferred confined read tool inside /side must keep ephemeral-read-only lifetime, got: " .. tostring(DEFERRED_READ_LIFETIME)
            )
            local nested_recs = S:list_records("tool/nested").data
            assert(#nested_recs == 0, "side nested call_tool must not write tool/nested into main session: " .. mare.json.encode(nested_recs))
            assert(conv:close() == true)
            "#,
        )
        .await;

        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn side_ui_entrypoints_suppress_cancel_warning_and_format_structured_errors() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["conversations"])
            .faux(json!([
                { "text": "parent seed" },
                { "text": "slow-cancel-1", "delay_ms": 60000 },
                { "error": "side-cmd-upstream-failure" },
                { "text": "slow-cancel-2", "delay_ms": 60000 },
                { "error": "side-submit-upstream-failure" }
            ]))
            .build()
            .await;
        expose(&h);

        let seed = h.prompt("seed parent", json!({})).await.unwrap();
        assert_eq!(seed["text"], "parent seed");

        let (cmd_cancel_warns, cmd_err_msg, submit_cancel_warns, submit_err_msg): (usize, String, usize, String) =
            eval(
                &h,
                r#"
            local notifications = {}
            local notify_tx, notify_rx = mare.task.channel()
            local settled_tx, settled_rx = mare.task.channel()
            local orig_notify = mare.ui.notify
            local orig_connected = mare.ui.connected
            mare.ui.connected = function() return true end
            mare.ui.notify = function(msg, level)
                local text = tostring(msg)
                notifications[#notifications + 1] = { msg = text, level = tostring(level) }
                if text:find("Side conversation:", 1, true) then
                    notify_tx:send(text)
                end
            end
            C.cordis:on("conversation/settled", function(ev) settled_tx:send(ev) end)
            local conv = C.cordis:get("conversations")
            local CMDS = C.cordis:get("commands")
            local UI = C.cordis:get("ui")
            local function cancel_when_running()
                for _ = 1, 200 do
                    if conv:cancel() then return end
                    mare.task.yield()
                end
                error("side turn never became cancellable")
            end
            local function side_warnings()
                local out = {}
                for _, n in ipairs(notifications) do
                    if n.msg:find("Side conversation:", 1, true) then
                        out[#out + 1] = n.msg
                    end
                end
                return out
            end

            -- Entry point 1a: `/side <question>` cancelled -> NO warning notification.
            notifications = {}
            CMDS:run("/side cancel via cmd")
            cancel_when_running()
            assert(settled_rx:recv(), "expected side turn settlement")
            mare.task.yield()
            mare.task.yield()
            local cmd_cancel_warns = #side_warnings()
            conv:close()

            -- Entry point 1b: `/side <question>` error -> human-readable error message, never `table: 0x...`.
            notifications = {}
            CMDS:run("/side fail via cmd")
            assert(settled_rx:recv(), "expected side turn settlement")
            local cmd_err_msg = assert(notify_rx:recv(), "expected side error notification")
            conv:close()

            -- Entry point 2a: submit inside open `/side` cancelled -> NO warning notification.
            notifications = {}
            CMDS:run("/side")
            local ev1 = { text = "cancel via submit", handled = false }
            C.cordis.bus:emit("input/submit", ev1)
            assert(ev1.handled == true, "side submit handler must handle input/submit")
            cancel_when_running()
            assert(settled_rx:recv(), "expected side turn settlement")
            mare.task.yield()
            mare.task.yield()
            local submit_cancel_warns = #side_warnings()
            conv:close()

            -- Entry point 2b: submit inside open `/side` error -> human-readable error message.
            notifications = {}
            CMDS:run("/side")
            local ev2 = { text = "fail via submit", handled = false }
            C.cordis.bus:emit("input/submit", ev2)
            assert(ev2.handled == true, "side submit handler must handle input/submit")
            assert(settled_rx:recv(), "expected side turn settlement")
            local submit_err_msg = assert(notify_rx:recv(), "expected side error notification")
            conv:close()

            mare.ui.notify = orig_notify
            mare.ui.connected = orig_connected
            return cmd_cancel_warns, cmd_err_msg, submit_cancel_warns, submit_err_msg
            "#,
            )
            .await;

        assert_eq!(cmd_cancel_warns, 0, "cancelled `/side <q>` must not emit a warning");
        assert!(
            cmd_err_msg.contains("side-cmd-upstream-failure") && !cmd_err_msg.contains("table: 0x"),
            "command entry error must surface message, got: {cmd_err_msg:?}"
        );
        assert_eq!(submit_cancel_warns, 0, "cancelled submit in `/side` must not emit a warning");
        assert!(
            submit_err_msg.contains("side-submit-upstream-failure") && !submit_err_msg.contains("table: 0x"),
            "submit entry error must surface message, got: {submit_err_msg:?}"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn side_turn_cache_clears_on_session_switch_so_wait_does_not_leak_across_sessions() {
    local(async {
        let env = TestEnv::new().await;
        let h = env
            .host()
            .builtins(&["conversations"])
            .faux(json!([
                { "text": "seed A" },
                { "text": "side answer from A" },
                { "text": "seed B" }
            ]))
            .build()
            .await;
        expose(&h);

        let a_seed = h.prompt("seed session A", json!({})).await.unwrap();
        assert_eq!(a_seed["text"], "seed A");

        let (noarg_err, old_id_err): (String, String) = eval(
            &h,
            r#"
            local conv = C.cordis:get("conversations")
            assert(conv:start())
            local started = A:prompt("side question in A")
            local reply = A:wait(started.turn_id)
            assert(reply.text == "side answer from A", "unexpected side reply: " .. tostring(reply.text))

            -- Switch to a brand-new session B (`session/changed` runs `conversations` close).
            S:create()
            local b_seed = A:prompt("seed session B")
            local b_done = A:wait(b_seed.turn_id)
            assert(b_done.text == "seed B")

            -- Open /side in session B with no turns yet: no-arg wait and old turn_id wait must both fail.
            assert(conv:start())
            local ok_noarg, err_noarg = pcall(function() return A:wait() end)
            assert(not ok_noarg, "no-arg wait in B must not return A's settled side turn")
            local ok_old, err_old = pcall(function() return A:wait(started.turn_id) end)
            assert(not ok_old, "wait(old_turn_id) in B must not return A's settled side turn")
            conv:close()
            return tostring(type(err_noarg) == "table" and err_noarg.message or err_noarg),
                   tostring(type(err_old) == "table" and err_old.message or err_old)
            "#,
        )
        .await;
        assert!(
            noarg_err.contains("no active or completed side turn"),
            "expected no-turn error on no-arg wait after switch, got: {noarg_err:?}"
        );
        assert!(
            old_id_err.contains("unknown turn_id") || old_id_err.contains("no turn"),
            "expected unknown turn_id error after switch, got: {old_id_err:?}"
        );
        h.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn side_and_btw_answer_on_empty_session_with_project_context_and_keep_no_history_forks() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::write(env.work.path().join("AGENTS.md"), "PROJECT_CONTEXT_MARKER_42\n").unwrap();
        let h = env
            .host()
            .builtins(&["conversations", "usage"])
            .faux(json!([
                { "text": "main turn after empty side" }
            ]))
            .build()
            .await;
        expose(&h);

        // Brand-new persistent session: nothing sent yet, no session file yet.
        exec(
            &h,
            r#"
            assert(#A:messages() == 0, "main session must start empty")
            local L = C.cordis:get("llm")
            local done_tx, done_rx = mare.task.channel()
            CAPTURED = {}
            L:register_provider({
                provider = "emptyprobe",
                is_configured = function() return true end,
                models = {
                    { model_id = "m", context_window = 128000, max_output = 4096 },
                    { model_id = "alt", context_window = 128000, max_output = 4096 },
                },
                stream = function(req, emit)
                    CAPTURED[#CAPTURED + 1] = {
                        purpose = req.purpose,
                        system = req.system or "",
                        model_id = req.model and req.model.model_id or nil,
                        tool_count = #(req.tools or {}),
                        msg_count = #(req.messages or {}),
                    }
                    done_tx:send(true)
                    local reply = "empty-side-ok-" .. tostring(#CAPTURED)
                    if emit then emit({ type = "text_delta", delta = reply }) end
                    return { role = "assistant", stop_reason = "stop", content = { { type = "text", text = reply } } }
                end,
            })
            assert(L:set_model("emptyprobe/m"))

            local cmds = C.cordis:get("commands")
            local conv = C.cordis:get("conversations")

            -- 1. /btw on brand-new unpersisted session answers with project system prompt and tools.
            cmds:run("/btw first empty question")
            assert(mare.task.timeout(5000, function() return done_rx:recv() end), "/btw never ran on empty session")
            local info = conv:info()
            assert(info ~= nil and info.selected == true, "side conversation should be open")
            cmds:run("/btw back")
            assert(conv:info() == nil, "/btw back should close side conversation")
            assert(#A:messages() == 0, "main session history must remain empty")

            -- 2. Second /side after usage/side-v1 may have created S.file (still S.leaf == nil) also answers.
            assert(L:set_model("emptyprobe/alt"))
            cmds:run("/side second empty question")
            assert(mare.task.timeout(5000, function() return done_rx:recv() end), "second /side never ran on empty session")
            cmds:run("/side back")
            assert(#A:messages() == 0, "main session history must still remain empty")

            assert(#CAPTURED == 2, "expected 2 side calls, got " .. tostring(#CAPTURED))
            for i, cap in ipairs(CAPTURED) do
                assert(cap.purpose == "side", "expected purpose=side on call " .. i)
                assert(cap.system:find("PROJECT_CONTEXT_MARKER_42", 1, true), "missing project AGENTS.md context on call " .. i)
                assert(cap.tool_count > 0, "expected tools bound on call " .. i)
                assert(cap.msg_count == 2, "expected boundary + user message on empty fork, got " .. tostring(cap.msg_count))
            end
            assert(CAPTURED[2].model_id == "alt", "expected updated session model on second side call")

            -- 3. Forks that genuinely require history still fail with session.no_history.
            local sid = S:info().session_id
            local ok_snap, err_snap = pcall(function() return S:snapshot({ session_id = sid }) end)
            assert(not ok_snap and err_snap.code == "session.no_history", tostring(err_snap))
            local ok_ctx, err_ctx = pcall(function() return S:snapshot_context({ session_id = sid, allow_memory = false }) end)
            assert(not ok_ctx and err_ctx.code == "session.no_history", tostring(err_ctx))
            local ok_ctx_default, err_ctx_default = pcall(function() return S:snapshot_context({ session_id = sid }) end)
            assert(not ok_ctx_default and err_ctx_default.code == "session.no_history", tostring(err_ctx_default))
            "#,
        )
        .await;

        // 4. Subsequent main conversation turn works normally.
        exec(&h, r#"assert(C.cordis:get("llm"):set_model("faux/echo"))"#).await;
        let main_reply = h.prompt("hello main", json!({})).await.unwrap();
        assert_eq!(main_reply["text"], "main turn after empty side");

        h.shutdown().await;

        // 5. --no-session empty session also answers /btw while rejecting S:snapshot.
        let nosess = env.host().no_session().faux(json!([{ "text": "nosess side ok" }])).build().await;
        expose(&nosess);
        exec(
            &nosess,
            r#"
            local conv = C.cordis:get("conversations")
            local info, err = conv:start()
            assert(info ~= nil, "empty --no-session conversations:start failed: " .. tostring(err))
            local started = A:prompt("empty nosess question")
            local reply = A:wait(started.turn_id)
            assert(reply and reply.text == "nosess side ok", mare.json.encode(reply))
            assert(conv:close() == true)
            local ok_snap, err_snap = pcall(function() return S:snapshot({ session_id = S:info().session_id }) end)
            assert(not ok_snap and err_snap.code == "session.no_history", tostring(err_snap))
            "#,
        )
        .await;
        nosess.shutdown().await;
    })
    .await;
}
