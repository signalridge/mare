use super::*;

#[test]
fn rpc_ui_forwarding_select_callbacks_editor_widgets_and_questions() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("c9_ui_probe.lua"),
        r#"return { name = "c9-ui-probe", inject = { "commands", "ui" }, apply = function(ctx)
      ctx.commands:register("c9-select", { run = function()
        local idx, tab, key, filter = ctx.ui:select({
          title = "Pick",
          title_right = mare.ui.Line({ mare.ui.Span("Sort") }),
          tabs = {
            {
              name = "One",
              items = {
                { item_id = "a", label = "Alpha", detail = "d1", preview = mare.ui.Markdown("prev **a**"), lines = { "r1", "r2" } },
                { label = "Hdr", header = true },
                { label = "Dis", disabled = true },
              },
              selected = 1,
            },
          },
          tab = 1,
          filter = "al",
          kind = "list",
          keys = { "w", { key = "ctrl+d", label = "delete" } },
          vim = true,
          on_highlight = function(i, tb)
            return mare.ui.Line({ mare.ui.Span("hi:" .. i .. ":" .. tb) })
          end,
          on_open = function(sel)
            assert(sel:is_open())
            sel:update({ items = { "Updated" } })
          end,
        })
        assert(idx == 1 and tab == 1 and key == "w" and filter == "al", "unexpected select return")
        ctx.ui:notify("c9-select:ok", "info")
      end })
      ctx.commands:register("c9-widgets", { run = function()
        local ed = ctx.ui:editor({ title = "Edit", prefill = "abc" })
        assert(ed == "edited text", "unexpected editor result")
        ctx.ui:set_editor("new", { keep_draft = true, prepend = true })
        ctx.ui:editor_hint("hint")
        ctx.ui:editor_config({ prompt = ">" })
        ctx.ui:widget("w1", { slot = "above-editor", order = 2, render = function() return mare.ui.Line("wid") end })
        ctx.ui:status("s1", { align = "left", order = 1, render = function() return mare.ui.Line("stat") end })
        ctx.ui:invalidate("s1")
        local qres, qerr = ctx.ui:questions({
          title = "Qs",
          questions = {
            {
              question_id = "q1",
              header = "H",
              question = "Which?",
              options = { { label = "A", description = "d", recommended = true } },
              multi_select = true,
              preview = "p",
            },
          },
        })
        assert(qerr == nil and qres and qres.answers and qres.answers.q1 and qres.answers.q1.selected[1] == "A")
        ctx.ui:notify("c9-widgets:ok", "info")
      end })
    end }"#,
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init =
        c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
    let sess_id = init["result"]["session"]["session_id"].as_str().unwrap().to_string();

    let cmd_id = c.request("commands.run", json!({"line": "/c9-select"}));
    let mut select_req_id = None;
    let mut select_params = Value::Null;
    let mut active_select_id = String::new();
    let mut saw_update = false;
    let mut highlight_id = None;
    let mut bad_idx_id = None;
    let mut unknown_id = None;
    let mut highlight_preview = None;
    let mut select_notes = Vec::new();
    let cmd_res = loop {
        let m = c.read();
        if m.get("method").and_then(Value::as_str) == Some("ui.select") {
            select_req_id = m.get("id").cloned();
            select_params = m["params"].clone();
            let sid = select_params["select_id"].as_str().unwrap().to_string();
            active_select_id = sid.clone();
            highlight_id = Some(c.request(
                "ui.get_select_preview",
                json!({"session_id": sess_id, "select_id": sid, "index": 0, "tab": 0}),
            ));
            bad_idx_id =
                Some(c.request("ui.get_select_preview", json!({"session_id": sess_id, "select_id": sid, "index": -1})));
            unknown_id = Some(c.request(
                "ui.get_select_preview",
                json!({"session_id": sess_id, "select_id": "missing-select", "index": 0}),
            ));
            continue;
        }
        if m.get("method").and_then(Value::as_str) == Some("ui/select-updated") {
            saw_update = m["params"]["items"][0]["label"] == "Updated";
            if highlight_preview.is_some()
                && let Some(id) = select_req_id.take()
            {
                c.send(json!({"jsonrpc":"2.0","id":id,"result":{"index":0,"tab":0,"key":"w","filter":"al"}}));
            }
            continue;
        }
        if m.get("method").and_then(Value::as_str) == Some("ui.notify") && m.get("id").is_none() {
            select_notes.push(m["params"].clone());
            continue;
        }
        if Some(&m["id"]) == bad_idx_id.as_ref().map(|id| json!(id)).as_ref() {
            assert_eq!(m["error"]["code"], -32602, "negative index must fail invalid_arguments: {m}");
            continue;
        }
        if Some(&m["id"]) == unknown_id.as_ref().map(|id| json!(id)).as_ref() {
            assert_eq!(
                m["error"]["data"]["code"], "ui.unknown_select",
                "unknown select_id must fail ui.unknown_select: {m}"
            );
            continue;
        }
        if Some(&m["id"]) == highlight_id.as_ref().map(|id| json!(id)).as_ref() {
            highlight_preview = Some(m["result"]["preview"].clone());
            if saw_update && let Some(id) = select_req_id.take() {
                c.send(json!({"jsonrpc":"2.0","id":id,"result":{"index":0,"tab":0,"key":"w","filter":"al"}}));
            }
            continue;
        }
        if m["id"] == cmd_id {
            break m;
        }
    };
    assert!(cmd_res.get("error").is_none(), "{cmd_res}");
    // Closed select must fail with ui.unknown_select afterwards, and legacy ui.select.highlight must fail -32601.
    let closed_preview =
        c.call_err("ui.get_select_preview", json!({"session_id": sess_id, "select_id": active_select_id, "index": 0}));
    assert_eq!(closed_preview["data"]["code"], "ui.unknown_select", "{closed_preview}");
    let legacy_preview = c.call_err("ui.select.highlight", json!({"select_id": "x", "index": 0}));
    assert_eq!(legacy_preview["code"], -32601, "{legacy_preview}");
    assert!(
        !select_notes.iter().any(|p| p["level"] == "error"),
        "c9-select emitted error notification: {select_notes:?}"
    );
    assert!(
        select_notes.iter().any(|p| p["text"] == "c9-select:ok" && p["level"] == "info"),
        "c9-select did not complete post-assert notify: {select_notes:?}"
    );
    assert_eq!(select_params["title_right"]["type"], "line", "{select_params}");
    assert_eq!(select_params["title_right"]["spans"][0]["text"], "Sort", "{select_params}");
    assert_eq!(select_params["filter"], "al");
    assert_eq!(select_params["kind"], "list");
    assert_eq!(select_params["vim"], true);
    assert_eq!(select_params["has_highlight"], true);
    assert_eq!(select_params["tabs"][0]["name"], "One");
    assert_eq!(select_params["tabs"][0]["items"][0]["preview"], json!({"type": "markdown", "text": "prev **a**"}));
    assert_eq!(select_params["tabs"][0]["items"][0]["lines"], json!(["r1", "r2"]));
    assert_eq!(select_params["tabs"][0]["items"][1]["header"], true);
    assert_eq!(select_params["tabs"][0]["items"][2]["disabled"], true);
    assert_eq!(select_params["keys"][0]["key"], "w");
    assert_eq!(select_params["keys"][1]["key"], "ctrl+d");
    assert_eq!(highlight_preview.as_ref().unwrap()["type"], "line", "{highlight_preview:?}");
    assert_eq!(highlight_preview.as_ref().unwrap()["spans"][0]["text"], "hi:1:1", "{highlight_preview:?}");
    assert!(saw_update, "on_open sel:update must emit ui/select-updated");

    let mut seen_notes = Vec::new();
    let mut editor_params = Value::Null;
    let mut questions_params = Value::Null;
    let w_id = c.request("commands.run", json!({"line": "/c9-widgets"}));
    let w_res = loop {
        let m = c.read();
        if let Some(method) = m.get("method").and_then(Value::as_str) {
            if let Some(id) = m.get("id") {
                let res = match method {
                    "ui.editor" => {
                        editor_params = m["params"].clone();
                        json!({"text": "edited text"})
                    }
                    "ui.questions" => {
                        questions_params = m["params"].clone();
                        json!({"answers": {"q1": {"selected": ["A"]}}})
                    }
                    _ => json!(null),
                };
                c.send(json!({"jsonrpc":"2.0","id":id,"result":res}));
            } else if method.starts_with("ui.") {
                seen_notes.push((method.to_string(), m["params"].clone()));
            }
            continue;
        }
        if m["id"] == w_id {
            break m;
        }
    };
    assert!(w_res.get("error").is_none(), "{w_res}");
    assert!(
        !seen_notes.iter().any(|(m, p)| m == "ui.notify" && p["level"] == "error"),
        "c9-widgets emitted error notification: {seen_notes:?}"
    );
    assert!(
        seen_notes.iter().any(|(m, p)| m == "ui.notify" && p["text"] == "c9-widgets:ok" && p["level"] == "info"),
        "c9-widgets did not complete post-assert notify: {seen_notes:?}"
    );
    assert_eq!(editor_params["title"], "Edit", "{editor_params}");
    assert_eq!(editor_params["prefill"], "abc", "{editor_params}");
    assert!(editor_params["session_id"].as_str().is_some_and(|s| !s.is_empty()), "{editor_params}");
    assert_eq!(questions_params["title"], "Qs");
    assert_eq!(questions_params["questions"][0]["multi_select"], true);
    assert!(
        seen_notes.iter().any(|(m, p)| m == "ui.set_editor"
            && p["text"] == "new"
            && p["keep_draft"] == true
            && p["prepend"] == true)
    );
    assert!(seen_notes.iter().any(|(m, p)| m == "ui.editor_hint" && p["text"] == "hint"));
    assert!(seen_notes.iter().any(|(m, p)| m == "ui.editor_config" && p["prompt"] == ">"));
    assert!(
        seen_notes.iter().any(|(m, p)| m == "ui.widget"
            && p["widget_id"] == "w1"
            && p["slot"] == "above-editor"
            && p["node"]["type"] == "line"
            && (p["node"]["spans"][0] == "wid" || p["node"]["spans"][0]["text"] == "wid")),
        "{seen_notes:?}"
    );
    assert!(
        seen_notes.iter().any(|(m, p)| m == "ui.status"
            && p["status_id"] == "s1"
            && p["align"] == "left"
            && p["node"].to_string().contains("stat")),
        "{seen_notes:?}"
    );

    c.shutdown_and_wait();
}

#[test]
fn rpc_native_human_receipts_authorize_protected_root_and_child_calls() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("protected-fixture.lua"),
        r#"
        return {name="protected-fixture",inject={"tools","policy"},apply=function(ctx)
            local secret="HIDDEN_PROTECTED_TEXT_71"
            local lease=mare.projection.register(secret)
            ctx:effect(function() return function() lease:close() end end)
            ctx.policy:voter("tool/approval",function() return {decision="allow",source="user"} end)
            ctx.tools:register("protected_child",{description="protected child fixture",side_effect="read",
                parameters={type="object",properties={path={type="string"}},required={"path"}},execute=function(args)
                    local script={{tool_calls={{name="write",arguments={path=args.path,content=secret}}}},{text="done"}}
                    local child=mare.subagent.start({prompt="write exact file",no_session=true,permission="read-only",
                        config={model="faux/echo",plugins={["provider-faux"]={script_json=mare.json.encode(script)}}}})
                    return mare.json.encode(child:wait())
                end})
        end}
    "#,
    )
    .unwrap();
    let script = home.path().join("protected-script.json");
    std::fs::write(&script,json!([
        {"tool_calls":[{"name":"write","arguments":{"path":"root-plain.txt","content":"PLAIN_TEXT_OK"}}]},{"text":"plain done"},
        {"tool_calls":[{"name":"write","arguments":{"path":"root-human.txt","content":"HIDDEN_PROTECTED_TEXT_71"}}]},{"text":"root done"},
        {"tool_calls":[{"name":"protected_child","arguments":{"path":"child-human.txt"}}]},{"text":"child done"}
    ]).to_string()).unwrap();
    let parent_script = std::fs::read_to_string(&script).unwrap();
    std::fs::write(
        home.path().join("config/init.lua"),
        format!("mare.setup {{ plugins = {{ ['provider-faux'] = {{ script_json = [=[{parent_script}]=] }} }} }}"),
    )
    .unwrap();
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--permission", "read-only", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env_remove("MARE_FAUX_SCRIPT")
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut rpc = RpcClient::spawn_cmd(&mut cmd);
    let init =
        rpc.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities":{"approvals":true}}), |_| {
            panic!("unexpected approval")
        });
    let sid = init["result"]["session"]["session_id"].as_str().expect("session_id").to_string();
    for (path, should_exist) in [("root-plain.txt", true), ("root-human.txt", true), ("child-human.txt", true)] {
        let mut approvals = 0;
        let start = rpc.call_raw("agent.prompt", json!({"session_id": sid, "input": "write exact file"}), |request| {
            assert_eq!(request["method"], "approval.request", "{request}");
            approvals += 1;
            json!({"decision":"allow"})
        });
        assert!(start.get("error").is_none(), "{start}");
        let response =
            rpc.call_raw("agent.wait", json!({"session_id": sid, "turn_id": start["result"]["turn_id"]}), |request| {
                assert_eq!(request["method"], "approval.request", "{request}");
                approvals += 1;
                json!({"decision":"allow"})
            });
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(approvals, 1, "{path}: {response}");
        assert_eq!(home.path().join(path).exists(), should_exist, "{path}: {response}");
    }
    rpc.kill_and_wait();
}

/// Ported from reference implementation and
/// Kimi Code `packages/node-sdk/test/session-question-handler.test.ts:33`.
#[test]
fn rpc_ui_select_close_cancel_and_missing_capability_fail_closed() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("c9_edges.lua"),
        r#"return { name = "c9-edges", inject = { "commands", "ui" }, apply = function(ctx)
      ctx.commands:register("c9-close", { run = function()
        local idx = ctx.ui:select({
          title = "CloseMe",
          items = { "A", "B" },
          on_open = function(sel)
            assert(sel:is_open())
            assert(sel:close())
            assert(not sel:is_open())
          end,
        })
        assert(idx == nil, "closed select should return nil: " .. tostring(idx))
        ctx.ui:notify("c9-close:ok", "info")
      end })
      local cancel_tok = mare.task.token()
      ctx.commands:register("c9-fire-cancel", { run = function() cancel_tok:cancel() end })
      ctx.commands:register("c9-cancel", { run = function()
        local idx, tab = ctx.ui:select({
          title = "CancelMe",
          items = { "A" },
          cancel = cancel_tok,
        })
        assert(idx == nil and tab == "cancelled", "expected cancelled select, got " .. tostring(tab))
        ctx.ui:notify("c9-cancel:ok", "info")
      end })
      ctx.commands:register("c9-nocap", { run = function()
        assert(not mare.ui.connected())
        local ed, ed_err = ctx.ui:editor({ title = "NoCap" })
        assert(ed == nil and ed_err ~= nil, "editor without ui cap must return err")
        local q, q_err = ctx.ui:questions({ title = "Qs", questions = { { question_id = "q1", question = "Why?", options = { { label = "a" } } } } })
        assert(q == nil and q_err ~= nil, "questions without ui cap must return err")
        ctx.ui:notify("c9-nocap:ok", "info")
      end })
    end }"#,
    )
    .unwrap();

    // 1. Without `capabilities.ui`, `ui.editor` / `ui.questions` fail closed without hanging.
    let mut cmd_nocap = mare_cmd();
    cmd_nocap
        .args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut nocap = RpcClient::spawn_cmd(&mut cmd_nocap);
    nocap.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": {} }), |_| json!(null));
    let nocap_res = nocap.call_raw("commands.run", json!({ "line": "/c9-nocap" }), |cb| {
        panic!("unexpected UI callback without capabilities.ui: {cb}")
    });
    assert!(nocap_res.get("error").is_none(), "{nocap_res}");
    nocap.shutdown_and_wait();

    // 2. With `capabilities.ui = true`, test `sel:close()` -> `ui/select-closed` and Lua cancel -> `$/cancel_request`.
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({ "protocol_version": "3.0", "capabilities": { "ui": true } }), |_| json!(null));

    let close_id = c.request("commands.run", json!({ "line": "/c9-close" }));
    let mut saw_close_note = false;
    let mut close_notes = Vec::new();
    let mut select_req_id = None;
    let close_res = loop {
        let m = c.read();
        if m.get("method").and_then(Value::as_str) == Some("ui.select") {
            select_req_id = m.get("id").cloned();
            continue;
        }
        if m.get("method").and_then(Value::as_str) == Some("ui/select-closed") {
            saw_close_note = m["params"]["select_id"].as_str().is_some();
            if let Some(id) = select_req_id.take() {
                c.send(json!({ "jsonrpc": "2.0", "id": id, "result": { "index": null } }));
            }
            continue;
        }
        if m.get("method").and_then(Value::as_str) == Some("ui.notify") {
            close_notes.push(m["params"].clone());
            continue;
        }
        if m["id"] == close_id {
            break m;
        }
    };
    assert!(close_res.get("error").is_none(), "{close_res}");
    assert!(saw_close_note, "sel:close() must emit ui/select-closed");
    assert!(close_notes.iter().any(|p| p["text"] == "c9-close:ok"), "{close_notes:?}");

    let cancel_id = c.request("commands.run", json!({ "line": "/c9-cancel" }));
    let mut fire_id = None;
    let mut saw_cancel_req = false;
    let mut cancel_notes = Vec::new();
    let mut pending_sel_id = None;
    let cancel_res = loop {
        let m = c.read();
        if m.get("method").and_then(Value::as_str) == Some("ui.select") {
            pending_sel_id = m.get("id").cloned();
            fire_id = Some(c.request("commands.run", json!({ "line": "/c9-fire-cancel" })));
            continue;
        }
        if Some(&m["id"]) == fire_id.as_ref().map(|id| json!(id)).as_ref() {
            continue;
        }
        if m.get("method").and_then(Value::as_str) == Some("$/cancel_request") {
            if pending_sel_id.as_ref() == Some(&m["params"]["id"]) {
                saw_cancel_req = true;
            }
            continue;
        }
        if m.get("method").and_then(Value::as_str) == Some("ui.notify") {
            cancel_notes.push(m["params"].clone());
            continue;
        }
        if m["id"] == cancel_id {
            break m;
        }
    };
    assert!(cancel_res.get("error").is_none(), "{cancel_res}");
    assert!(saw_cancel_req, "cancelling ui.select must send $/cancel_request");
    assert!(cancel_notes.iter().any(|p| p["text"] == "c9-cancel:ok"), "{cancel_notes:?}");

    c.shutdown_and_wait();
}

/// `sel:close()` and Lua cancel tokens emit `ui/select-closed` /
/// `$/cancel_request` and resolve `ui.select` to `(nil, "cancelled")` without waiting for a reply.
#[test]
fn rpc_ui_select_close_and_cancel_token_send_cancel_request() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("c9_close_probe.lua"),
        r#"return { name = "c9-close-probe", inject = { "commands", "ui" }, apply = function(ctx)
      ctx.commands:register("c9-close-on-open", { run = function()
        local idx, reason = ctx.ui:select({
          title = "Pick",
          items = { "Alpha", "Beta" },
          on_open = function(sel)
            sel:update({ items = { "Updated" } })
            assert(sel:is_open())
            assert(sel:close() == true)
            assert(sel:is_open() == false)
          end,
        })
        assert(idx == nil and reason == "cancelled", "on_open close: " .. tostring(idx) .. ":" .. tostring(reason))
        ctx.ui:notify("on-open-close:ok", "info")
      end })
      ctx.commands:register("c9-close-via-highlight", { run = function()
        local handle
        local idx, reason = ctx.ui:select({
          title = "Pick",
          items = { "Alpha", "Beta" },
          on_open = function(sel)
            handle = sel
          end,
          on_highlight = function()
            assert(handle and handle:close() == true)
            return mare.ui.Line("closing")
          end,
        })
        assert(idx == nil and reason == "cancelled", "highlight close: " .. tostring(idx) .. ":" .. tostring(reason))
        ctx.ui:notify("highlight-close:ok", "info")
      end })
      ctx.commands:register("c9-cancel-token", { run = function()
        local tok = mare.task.token()
        local idx, reason = ctx.ui:select({
          title = "Pick",
          items = { "Alpha" },
          cancel = tok,
          on_highlight = function()
            tok:cancel()
            return mare.ui.Line("cancelling")
          end,
        })
        assert(idx == nil and reason == "cancelled", "cancel token: " .. tostring(idx) .. ":" .. tostring(reason))
        ctx.ui:notify("cancel-token:ok", "info")
      end })
      ctx.commands:register("c9-close-while-open-awaits", { run = function()
        local handle
        local _tx, rx = mare.task.channel()
        local idx, reason = ctx.ui:select({
          title = "Pick",
          items = { "Alpha", "Beta" },
          on_open = function(sel)
            handle = sel
            rx:recv()
          end,
          on_highlight = function()
            assert(handle and handle:close() == true)
            return mare.ui.Line("closing")
          end,
        })
        assert(idx == nil and reason == "cancelled", "open await close: " .. tostring(idx) .. ":" .. tostring(reason))
        ctx.ui:notify("open-await-close:ok", "info")
      end })
      ctx.commands:register("c9-dismiss-null", { run = function()
        local idx, reason = ctx.ui:select({ title = "Pick", items = { "Alpha" } })
        assert(idx == nil and reason == "cancelled", "dismiss select null: " .. tostring(idx) .. ":" .. tostring(reason))
        local iv, ireason = ctx.ui:input({ title = "In" })
        assert(iv == nil and ireason == "cancelled", "dismiss input null: " .. tostring(iv) .. ":" .. tostring(ireason))
        local ev, ereason = ctx.ui:editor({ title = "Ed" })
        assert(ev == nil and ereason == "cancelled", "dismiss editor null: " .. tostring(ev) .. ":" .. tostring(ereason))
        ctx.ui:notify("dismiss-null:ok", "info")
      end })
    end }"#,
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));

    let drive = |c: &mut RpcClient,
                 cmd: &str,
                 trigger_highlight: bool,
                 expect_close_note: bool,
                 expected_notify: &str| {
        let cmd_id = c.request("commands.run", json!({"line": cmd}));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut select_req_id: Option<Value> = None;
        let mut saw_close_note = false;
        let mut saw_cancel_req = false;
        let mut saw_notify = false;
        let mut cmd_res = None;
        while cmd_res.is_none()
            || (trigger_highlight && !saw_cancel_req)
            || (expect_close_note && !saw_close_note)
            || !saw_notify
        {
            let msg = c
                .read_deadline(deadline)
                .unwrap_or_else(|e| panic!("{cmd} stalled ({e}): close={saw_close_note} cancel={saw_cancel_req} notify={saw_notify} cmd={cmd_res:?}"));
            match msg.get("method").and_then(Value::as_str) {
                Some("ui.select") if msg.get("id").is_some() => {
                    let req_id = msg["id"].clone();
                    select_req_id = Some(req_id.clone());
                    if trigger_highlight {
                        let select_id = msg["params"]["select_id"].clone();
                        let sess = msg["params"]["session_id"].clone();
                        c.request(
                            "ui.get_select_preview",
                            json!({"session_id": sess, "select_id": select_id, "index": 0, "tab": 0}),
                        );
                    }
                }
                Some("ui/select-closed") => {
                    assert!(msg["params"]["select_id"].is_string(), "missing selectId: {msg}");
                    saw_close_note = true;
                }
                Some("$/cancel_request") => {
                    if let Some(ref rid) = select_req_id
                        && msg["params"]["id"] == *rid
                    {
                        saw_cancel_req = true;
                    }
                }
                Some("ui.notify") => {
                    if msg["params"]["text"] == expected_notify {
                        saw_notify = true;
                    }
                }
                Some(_) => {}
                None if msg["id"] == cmd_id => {
                    assert!(msg.get("error").is_none(), "{cmd} returned error: {msg}");
                    cmd_res = Some(msg);
                }
                None => {}
            }
        }
    };

    drive(&mut c, "/c9-close-on-open", false, true, "on-open-close:ok");
    drive(&mut c, "/c9-close-via-highlight", true, true, "highlight-close:ok");
    drive(&mut c, "/c9-close-while-open-awaits", true, true, "open-await-close:ok");
    drive(&mut c, "/c9-cancel-token", true, false, "cancel-token:ok");

    // Dismissing ui.select / ui.input / ui.editor with null must yield (nil, "cancelled").
    let dismiss_res =
        c.call_raw("commands.run", json!({"line": "/c9-dismiss-null"}), |req| match req["method"].as_str() {
            Some("ui.select") => json!({"index": null}),
            Some("ui.input" | "ui.editor") => json!({"text": null}),
            other => panic!("unexpected server call: {other:?}: {req}"),
        });
    assert!(dismiss_res.get("error").is_none(), "dismiss error: {dismiss_res}");

    c.shutdown_and_wait();
}

/// Adapted from reference implementation, Kimi Code
/// `session-question-handler.test.ts:33`, and Codex `request_user_input.rs:49`: missing
/// `capabilities.ui`, mid-call capability withdrawal, and headless mode with no UI responder all
/// return `(nil, "unavailable")` without hanging.
#[test]
fn rpc_ui_unavailable_without_capability_when_withdrawn_and_headless() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let headless_marker = home.path().join("headless-probe.txt");
    let marker_lua = headless_marker.to_string_lossy().replace('\\', "/");
    std::fs::write(
        plugins.join("c9_unavailable_probe.lua"),
        format!(
            r#"local marker = "{marker_lua}"
local function check_all(ui)
  local s, serr = ui:select({{ title = "S", items = {{ "A", "B" }} }})
  assert(s == nil and serr == "unavailable", "select:" .. tostring(s) .. ":" .. tostring(serr))
  local i, ierr = ui:input({{ title = "I" }})
  assert(i == nil and ierr == "unavailable", "input:" .. tostring(i) .. ":" .. tostring(ierr))
  local e, eerr = ui:editor({{ title = "E" }})
  assert(e == nil and eerr == "unavailable", "editor:" .. tostring(e) .. ":" .. tostring(eerr))
  local c, cerr = ui:confirm({{ title = "C" }})
  assert(c == nil and cerr == "unavailable", "confirm:" .. tostring(c) .. ":" .. tostring(cerr))
  local q, qerr = ui:questions({{ title = "Q", questions = {{ {{ question_id = "q1", header = "H", question = "?", options = {{ {{ label = "A" }} }} }} }} }})
  assert(q == nil and qerr == "unavailable", "questions:" .. tostring(q) .. ":" .. tostring(qerr))
end
return {{ name = "c9-unavailable-probe", inject = {{ "commands", "tools", "ui" }}, apply = function(ctx)
  ctx.tools:register("c9_probe_headless", {{
    description = "probe unavailable ui in headless mode",
    side_effect = "read",
    parameters = {{ type = "object", properties = {{}} }},
    execute = function()
      check_all(ctx.ui)
      local f = assert(io.open(marker, "w"))
      f:write("ok")
      f:close()
      return "ok"
    end
  }})
  ctx.commands:register("c9-probe-all", {{ run = function()
    check_all(ctx.ui)
    local f = assert(io.open(marker, "w"))
    f:write("ok")
    f:close()
  end }})
  ctx.commands:register("c9-withdraw", {{ run = function(args)
    local kind = (args or ""):gsub("^%s+", ""):gsub("%s+$", "")
    local r, err
    if kind == "select" then
      r, err = ctx.ui:select({{ title = "W", items = {{ "A" }} }})
    elseif kind == "input" then
      r, err = ctx.ui:input({{ title = "W" }})
    elseif kind == "editor" then
      r, err = ctx.ui:editor({{ title = "W" }})
    elseif kind == "confirm" then
      r, err = ctx.ui:confirm({{ title = "W" }})
    elseif kind == "questions" then
      r, err = ctx.ui:questions({{ title = "W", questions = {{ {{ question_id = "q1", header = "H", question = "?", options = {{ {{ label = "A" }} }} }} }} }})
    else
      error("unknown kind: " .. kind)
    end
    assert(r == nil and err == "unavailable", kind .. ":" .. tostring(r) .. ":" .. tostring(err))
  end }})
end }}"#
        ),
    )
    .unwrap();

    // 1. No `capabilities.ui` over RPC -> immediate `(nil, "unavailable")` for all 5 UI calls.
    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {}}), |_| json!(null));
    let res = c.call_raw("commands.run", json!({"line": "/c9-probe-all"}), |req| {
        panic!("unexpected UI RPC request when capabilities.ui is false: {req}")
    });
    assert!(res.get("error").is_none(), "probe-all error: {res}");
    assert_eq!(std::fs::read_to_string(&headless_marker).unwrap(), "ok");
    std::fs::remove_file(&headless_marker).unwrap();

    // 2. Mid-call `connection.set_capabilities` withdrawing `capabilities.ui` resolves pending calls to "unavailable".
    for (kind, rpc_method) in [
        ("select", "ui.select"),
        ("input", "ui.input"),
        ("editor", "ui.editor"),
        ("confirm", "ui.confirm"),
        ("questions", "ui.questions"),
    ] {
        c.call_raw(
            "connection.set_capabilities",
            json!({"capabilities": {"callbacks": false, "ui": true, "approvals": false, "elicitation": false}}),
            |_| json!(null),
        );
        let cmd_id = c.request("commands.run", json!({"line": format!("/c9-withdraw {kind}")}));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        // Wait for the server -> host UI request and withhold any reply; withdraw ui capability instead.
        let mut ui_req_id: Option<Value> = None;
        while ui_req_id.is_none() {
            let msg = c.read_deadline(deadline).unwrap_or_else(|e| panic!("waiting for {rpc_method} ({kind}): {e}"));
            if msg.get("method").and_then(Value::as_str) == Some(rpc_method) && msg.get("id").is_some() {
                ui_req_id = Some(msg["id"].clone());
            } else if msg.get("id") == Some(&json!(cmd_id)) {
                panic!("{kind} finished before capability withdrawal: {msg}");
            }
        }
        let init_id = c.request(
            "connection.set_capabilities",
            json!({"capabilities": {"callbacks": false, "ui": false, "approvals": false, "elicitation": false}}),
        );
        let mut saw_init = false;
        let mut saw_cmd = false;
        let mut saw_cancel = false;
        while !saw_init || !saw_cmd || !saw_cancel {
            let msg = c
                .read_deadline(deadline)
                .unwrap_or_else(|e| panic!("{kind} hung after ui capability withdrawal ({e})"));
            if msg.get("method").and_then(Value::as_str) == Some("$/cancel_request")
                && Some(&msg["params"]["id"]) == ui_req_id.as_ref()
            {
                saw_cancel = true;
            } else if msg.get("method").is_none() && msg["id"] == init_id {
                assert!(msg.get("error").is_none(), "re-initialize error: {msg}");
                saw_init = true;
            } else if msg.get("method").is_none() && msg["id"] == cmd_id {
                assert!(msg.get("error").is_none(), "{kind} withdraw error: {msg}");
                saw_cmd = true;
            }
        }
    }

    c.shutdown_and_wait();

    // 3. Headless `-p` without any UI responder also returns `(nil, "unavailable")` for all 5 calls.
    let faux_script = home.path().join("headless-script.json");
    std::fs::write(
        &faux_script,
        json!([
            {"tool_calls": [{"name": "c9_probe_headless", "arguments": {}}]},
            {"text": "done"}
        ])
        .to_string(),
    )
    .unwrap();
    let out = mare_cmd()
        .args(["-p", "probe", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .env("MARE_FAUX_SCRIPT", &faux_script)
        .current_dir(home.path())
        .run_output();
    assert!(
        out.status.success(),
        "headless probe failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&headless_marker).unwrap(), "ok");
}

/// Adapted from reference implementation
/// and Kimi Code `packages/node-sdk/test/session-question-handler.test.ts:48`: mid-call host EOF/disconnect
/// and host JSON-RPC errors resolve pending UI calls to `(nil, "unavailable")`.
#[test]
fn rpc_ui_mid_call_disconnect_and_rpc_error_return_unavailable() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let marker = home.path().join("disconnect-probe.txt");
    let marker_lua = marker.to_string_lossy().replace('\\', "/");
    std::fs::write(
        plugins.join("c9_disconnect_probe.lua"),
        format!(
            r#"local marker = "{marker_lua}"
return {{ name = "c9-disconnect-probe", inject = {{ "commands", "ui", "task" }}, apply = function(ctx)
  ctx.commands:register("c9-error-prompts", {{ run = function()
    local c, cerr = ctx.ui:confirm({{ title = "ConfirmErr" }})
    assert(c == nil and cerr == "unavailable", "confirm err: " .. tostring(c) .. ":" .. tostring(cerr))
    local q, qerr = ctx.ui:questions({{ title = "QErr", questions = {{ {{ question_id = "q1", header = "H", question = "?", options = {{ {{ label = "A" }} }} }} }} }})
    assert(q == nil and qerr == "unavailable", "questions err: " .. tostring(q) .. ":" .. tostring(qerr))
  end }})
  ctx.commands:register("c9-disconnect-select", {{ run = function()
    ctx:spawn(function()
      local int_outcome = nil
      mare.on("interaction/resolved", function(ev)
        if ev.title == "DisconnectPick" then int_outcome = ev.outcome end
      end)
      local idx, err = ctx.ui:select({{ title = "DisconnectPick", items = {{ "Alpha" }} }})
      local f = assert(io.open(marker, "w"))
      f:write(tostring(idx) .. ":" .. tostring(err) .. ":" .. tostring(int_outcome))
      f:close()
    end)
  end }})
end }}"#
        ),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));

    // 1. Host RPC error on confirm/questions must yield (nil, "unavailable").
    let err_cmd = c.request("commands.run", json!({"line": "/c9-error-prompts"}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let msg = c.read_deadline(deadline).unwrap_or_else(|e| panic!("c9-error-prompts stalled: {e}"));
        match msg.get("method").and_then(Value::as_str) {
            Some("ui.confirm" | "ui.questions") if msg.get("id").is_some() => {
                c.send(json!({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32000, "message": "host failed"}}));
            }
            None if msg["id"] == err_cmd => {
                assert!(msg.get("error").is_none(), "rpc error: {msg}");
                break;
            }
            _ => {}
        }
    }

    // 2. Host disconnect (EOF) while ui.select is pending must yield (nil, "unavailable").
    let _disc_cmd = c.request("commands.run", json!({"line": "/c9-disconnect-select"}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let msg = c.read_deadline(deadline).unwrap_or_else(|e| panic!("waiting for ui.select: {e}"));
        if msg.get("method").and_then(Value::as_str) == Some("ui.select") && msg.get("id").is_some() {
            break;
        }
    }
    c.wait_exit();
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "nil:unavailable:unavailable");
}

/// `interaction/requested` and `interaction/resolved` emit in 1:1
/// pairs across answered (including nested confirm->select), cancelled, and unavailable outcomes;
/// observers (peer subscribers and Lua listeners) see the events without being able to reply.
#[test]
fn rpc_interaction_events_paired_and_observer_cannot_reply() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config");
    let plugins = config_dir.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        config_dir.join("init.lua"),
        r#"mare.setup { peer = { allow = { "events.subscribe", "events.list" } } }"#,
    )
    .unwrap();
    std::fs::write(
        plugins.join("c9_interaction_probe.lua"),
        r#"return { name = "c9-interaction-probe", inject = { "commands", "ui" }, apply = function(ctx)
      _G.OBSERVER_LOG = {}
      mare.on("interaction/requested", function(ev)
        table.insert(_G.OBSERVER_LOG, { phase = "requested", id = ev.interaction_id, kind = ev.kind, title = ev.title })
        -- Observer return values must never resolve or hijack an interactive prompt.
        return 1
      end)
      mare.on("interaction/resolved", function(ev)
        table.insert(_G.OBSERVER_LOG, { phase = "resolved", id = ev.interaction_id, kind = ev.kind, outcome = ev.outcome })
      end)
      ctx.commands:register("c9-int-flow", { run = function()
        local idx, _, _, _ = ctx.ui:select({ title = "IntPick", items = { "First", "Second" } })
        assert(idx == 2, "observer hijacked select or wrong index: " .. tostring(idx))
        -- confirm("ConfirmNo") chooses No (false): nested confirm -> select must emit once,
        -- and `false` is still a valid "answered" outcome (distinct from nil/cancelled).
        local ok, cerr = ctx.ui:confirm({ title = "ConfirmNo" })
        assert(ok == false and cerr == nil, "confirm No failed: " .. tostring(ok) .. ":" .. tostring(cerr))
        local tok = mare.task.token()
        local cidx, creason = ctx.ui:select({
          title = "IntCancel",
          items = { "Only" },
          cancel = tok,
          on_highlight = function()
            tok:cancel()
            return mare.ui.Line("bye")
          end,
        })
        assert(cidx == nil and creason == "cancelled", "cancel select: " .. tostring(cidx) .. ":" .. tostring(creason))
        local qres, qerr = ctx.ui:questions({
          title = "IntUnavail",
          questions = { { question_id = "q1", header = "H", question = "?", options = { { label = "A" } } } },
        })
        assert(qres == nil and qerr == "unavailable", "expected unavailable on questions: " .. tostring(qerr))
      end })
      ctx.commands:register("c9-int-check-log", { run = function()
        -- Expected 8 observer log entries (4 pairs: select answered, confirm answered, select cancelled, questions unavailable)
        assert(#_G.OBSERVER_LOG == 8, "expected 8 log entries, got " .. #_G.OBSERVER_LOG .. ": " .. mare.json.encode(_G.OBSERVER_LOG))
        for i = 1, 7, 2 do
          local r1, r2 = _G.OBSERVER_LOG[i], _G.OBSERVER_LOG[i + 1]
          assert(r1.phase == "requested" and r2.phase == "resolved", mare.json.encode(_G.OBSERVER_LOG))
          assert(r1.id == r2.id and r1.kind == r2.kind, mare.json.encode(_G.OBSERVER_LOG))
        end
        assert(_G.OBSERVER_LOG[1].kind == "select" and _G.OBSERVER_LOG[2].outcome == "answered")
        assert(_G.OBSERVER_LOG[3].kind == "confirm" and _G.OBSERVER_LOG[4].outcome == "answered")
        assert(_G.OBSERVER_LOG[5].kind == "select" and _G.OBSERVER_LOG[6].outcome == "cancelled")
        assert(_G.OBSERVER_LOG[7].kind == "questions" and _G.OBSERVER_LOG[8].outcome == "unavailable")
      end })
      ctx.commands:register("c9-int-nocap", { run = function()
        local before = #_G.OBSERVER_LOG
        local res, err = ctx.ui:select({ title = "NoCapPick", items = { "One" } })
        assert(res == nil and err == "unavailable", "expected unavailable when capabilities.ui=false: " .. tostring(err))
        assert(#_G.OBSERVER_LOG == before + 2)
        assert(_G.OBSERVER_LOG[before + 1].phase == "requested" and _G.OBSERVER_LOG[before + 2].phase == "resolved")
        assert(_G.OBSERVER_LOG[before + 2].outcome == "unavailable")
      end })
    end }"#,
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--peer", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    let init =
        c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
    let sid = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    c.call_raw("events.subscribe", json!({"session_id": sid, "include": ["interaction/*"]}), |_| json!(null));

    #[cfg(unix)]
    let mut peer = {
        let sock = home.path().join(format!("state/peers/{}.sock", c.pid()));
        let stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        let mut p = RpcClient::timed(BufReader::new(stream.try_clone().unwrap()), stream);
        // Even if peer requests `ui = true`, a peer connection never takes over UI or replies.
        p.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
        p.call_raw("events.subscribe", json!({"session_id": sid, "include": ["interaction/*"]}), |_| json!(null));
        p
    };

    let cmd_id = c.request("commands.run", json!({"line": "/c9-int-flow"}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut events = Vec::new();
    let mut cmd_done = false;
    while !cmd_done || events.len() < 8 {
        let msg = c.read_deadline(deadline).unwrap_or_else(|e| panic!("primary stalled ({e}): events={events:?}"));
        match msg.get("method").and_then(Value::as_str) {
            Some("ui.select") if msg.get("id").is_some() => {
                if msg["params"]["title"] == "IntPick" {
                    c.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":{"index":1}}));
                } else if msg["params"]["title"] == "IntCancel" {
                    let sid = msg["params"]["select_id"].clone();
                    let sess = msg["params"]["session_id"].clone();
                    c.request(
                        "ui.get_select_preview",
                        json!({"session_id": sess, "select_id": sid, "index": 0, "tab": 0}),
                    );
                }
            }
            Some("ui.confirm") if msg.get("id").is_some() => {
                c.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":{"confirmed":false}}));
            }
            Some("ui.questions") if msg.get("id").is_some() => {
                // Simulate responder failure by closing/failing the RPC error response.
                c.send(json!({"jsonrpc":"2.0","id":msg["id"],"error":{"code":-32000,"message":"no ui"}}));
            }
            Some("event") => {
                let p = msg["params"].clone();
                if p["type"].as_str().is_some_and(|t| t.starts_with("interaction/")) {
                    events.push(p);
                }
            }
            None if msg["id"] == cmd_id => {
                assert!(msg.get("error").is_none(), "flow error: {msg}");
                cmd_done = true;
            }
            _ => {}
        }
    }

    assert_eq!(events.len(), 8, "expected 4 pairs of interaction events: {events:?}");
    let expected = [
        ("interaction/requested", "select", None),
        ("interaction/resolved", "select", Some("answered")),
        ("interaction/requested", "confirm", None),
        ("interaction/resolved", "confirm", Some("answered")),
        ("interaction/requested", "select", None),
        ("interaction/resolved", "select", Some("cancelled")),
        ("interaction/requested", "questions", None),
        ("interaction/resolved", "questions", Some("unavailable")),
    ];
    for (idx, (ev, (etype, ekind, eout))) in events.iter().zip(expected.iter()).enumerate() {
        assert_eq!(ev["type"], *etype, "event[{idx}]: {ev}");
        assert_eq!(ev["data"]["kind"], *ekind, "event[{idx}]: {ev}");
        if let Some(out) = eout {
            assert_eq!(ev["data"]["outcome"], *out, "event[{idx}]: {ev}");
        }
    }
    for [req_ev, res_ev] in events.as_chunks::<2>().0 {
        assert_eq!(req_ev["data"]["interaction_id"], res_ev["data"]["interaction_id"], "{req_ev:?} vs {res_ev:?}");
    }

    #[cfg(unix)]
    {
        let mut peer_events = Vec::new();
        while peer_events.len() < 8 {
            let msg = peer
                .read_deadline(deadline)
                .unwrap_or_else(|e| panic!("peer missed interaction events ({e}): {peer_events:?}"));
            if let Some(m) = msg.get("method").and_then(Value::as_str) {
                assert_ne!(m, "ui.select", "peer must never receive ui.select request");
                if m == "event" && msg["params"]["type"].as_str().is_some_and(|t| t.starts_with("interaction/")) {
                    peer_events.push(msg["params"].clone());
                }
            }
        }
        assert_eq!(peer_events.len(), 8, "peer events: {peer_events:?}");
    }

    let check = c.call_raw("commands.run", json!({"line": "/c9-int-check-log"}), |_| json!(null));
    assert!(check.get("error").is_none(), "lua observer log check failed: {check}");

    // Withdrawing `capabilities.ui` triggers the real `unavailable` path with a paired event.
    c.call_raw(
        "connection.set_capabilities",
        json!({"capabilities": {"callbacks": false, "ui": false, "approvals": false, "elicitation": false}}),
        |_| json!(null),
    );
    let nocap = c.call_raw("commands.run", json!({"line": "/c9-int-nocap"}), |_| json!(null));
    assert!(nocap.get("error").is_none(), "nocap unavailable check failed: {nocap}");

    c.shutdown_and_wait();
}

/// & contract specification.8: `ui.print` emits lossy/ignorable `ui/print` without
/// touching session entries; `ui.card` emits `ui/card` with `persistent` flag, persisting only
/// persistent cards; `events.list` exposes `interaction/*`, `ui/print`, and `ui/card`.
#[test]
fn rpc_ui_print_and_card_events_and_session_boundaries() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("c9_print_card_probe.lua"),
        r#"return { name = "c9-print-card-probe", inject = { "commands", "ui" }, apply = function(ctx)
      ctx.commands:register("c9-emit-output", { run = function()
        _G.KEEP_PRINT_SECRET = mare.projection.register("PRINT_SECRET_99")
        ctx.ui:print(mare.ui.Line("ephemeral-banner token=PRINT_SECRET_99"))
        mare.ui.print("raw-print token=PRINT_SECRET_99")
        assert(ctx.ui:card({ title = "Temp Card", markdown = "tmp body", persist = false }) ~= nil)
        assert(ctx.ui:card({ title = "Saved Card", markdown = "saved body", persist = true, id = "c-saved" }) ~= nil)
        -- Verify UiService contract recognizes notify/select/input/confirm/overlay/print/card
        for _, name in ipairs({ "notify", "select", "input", "confirm", "overlay", "print", "card" }) do
          assert(mare.contracts.permitted("ui", name), "ui contract missing method: " .. name)
        end
      end })
    end }"#,
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));

    let listed = c.call_raw("events.list", json!({}), |_| json!(null));
    let event_defs = listed["result"]["data"].as_array().expect("events.list data");
    for name in ["interaction/requested", "interaction/resolved", "ui/print", "ui/card"] {
        assert!(event_defs.iter().any(|e| e["name"] == name), "events.list missing {name}");
    }
    let print_def = event_defs.iter().find(|e| e["name"] == "ui/print").unwrap();
    assert_eq!(print_def["delivery"], "lossy");
    assert_eq!(print_def["ignorable"], true);

    c.call_raw("events.subscribe", json!({"scope": "all", "include": ["ui/*"]}), |_| json!(null));
    let cmd_id = c.request("commands.run", json!({"line": "/c9-emit-output"}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut prints = Vec::new();
    let mut cards = Vec::new();
    let mut cmd_done = false;
    while !cmd_done || prints.len() < 2 || cards.len() < 2 {
        let msg =
            c.read_deadline(deadline).unwrap_or_else(|e| panic!("timed out ({e}): prints={prints:?} cards={cards:?}"));
        match msg.get("method").and_then(Value::as_str) {
            Some("event") => match msg["params"]["type"].as_str() {
                Some("ui/print") => prints.push(msg["params"]["data"].clone()),
                Some("ui/card") => cards.push(msg["params"]["data"].clone()),
                _ => {}
            },
            None if msg["id"] == cmd_id => {
                assert!(msg.get("error").is_none(), "c9-emit-output failed: {msg}");
                cmd_done = true;
            }
            _ => {}
        }
    }

    assert!(prints.iter().any(|p| p["text"].as_str().is_some_and(|t| t.contains("ephemeral-banner"))), "{prints:?}");
    assert!(prints.iter().any(|p| p["text"].as_str().is_some_and(|t| t.contains("raw-print"))), "{prints:?}");
    for p in &prints {
        let s = p.to_string();
        assert!(!s.contains("PRINT_SECRET_99"), "ui/print leaked registered secret: {p}");
    }

    let temp_card = cards.iter().find(|d| d["title"] == "Temp Card").expect("missing Temp Card event");
    assert_eq!(temp_card["persistent"], false, "{temp_card}");
    let saved_card = cards.iter().find(|d| d["title"] == "Saved Card").expect("missing Saved Card event");
    assert_eq!(saved_card["persistent"], true, "{saved_card}");

    let cur = c.call_raw("session.list", json!({ "loaded": true }), |_| json!(null))["result"]["data"][0]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let entries = c.call_raw("session.list_records", json!({ "session_id": cur, "kind": "ui/card" }), |_| json!(null));
    let entries_str = entries["result"]["data"].to_string();
    assert!(entries_str.contains("Saved Card"), "persistent card missing from session.list_records: {entries_str}");
    assert!(!entries_str.contains("Temp Card"), "temporary card leaked into session.list_records: {entries_str}");
    assert!(!entries_str.contains("ephemeral-banner"), "ui.print leaked into session.list_records: {entries_str}");

    c.shutdown_and_wait();
}

/// Interrupted/dropped waits still pair `interaction/requested` and
/// `interaction/resolved`, `resolved.session_id` matches `requested.session_id` even after a
/// mid-wait session switch, and `ui/print` is projected at the event log boundary.
#[tokio::test]
async fn interaction_interrupt_session_switch_and_tui_print_projection() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().user_plugins().no_session().build().await;
        expose(&h);
        let mut cursor =
            h.kernel.event_log().open_cursor(std::sync::Arc::new(|name| (name == "ui/print").then_some(true)));
        let ok: bool = eval(
            &h,
            r##"
            local reqs, ress, prints = {}, {}, {}
            local res_tx, res_rx = mare.task.channel()
            local print_tx, print_rx = mare.task.channel()
            mare.on("interaction/requested", function(ev) table.insert(reqs, ev) end)
            mare.on("interaction/resolved", function(ev)
              table.insert(ress, ev)
              res_tx:send(#ress)
            end)
            mare.on("ui/print", function(ev)
              table.insert(prints, ev)
              print_tx:send(#prints)
            end)

            -- 1) Interrupted/abandoned coroutine (timeout dropping call / wait) still emits resolved once.
            local started_tx, started_rx = mare.task.channel()
            local release_tx, release_rx = mare.task.channel()
            mare.ui.select = function()
              started_tx:send(true)
              release_rx:recv()
              return 1
            end
            local ok_to, finished = pcall(mare.task.timeout, 10, function()
              return mare.ui.select({ title = "DropWait", items = { "A" } })
            end)
            assert(ok_to and not finished, "expected timeout waiting on select")
            release_tx:send(true)
            res_rx:recv()
            assert(#reqs == 1 and #ress == 1, ("expected 1:1 pair on drop, got %d/%d"):format(#reqs, #ress))
            assert(reqs[1].interaction_id == ress[1].interaction_id and ress[1].outcome == "cancelled")

            -- 2) Session switch during active interaction keeps requested and resolved session_id identical.
            local before_sid = C.cordis:get("session"):info().session_id
            mare.ui.select = function()
              C.cordis:get("session"):create()
              return 1
            end
            local picked = mare.ui.select({ title = "SwitchWait", items = { "A" } })
            assert(picked == 1)
            res_rx:recv()
            local after_sid = C.cordis:get("session"):info().session_id
            assert(before_sid ~= after_sid, "session:create must switch session id")
            assert(#reqs == 2 and #ress == 2, ("expected 2 pairs, got %d/%d"):format(#reqs, #ress))
            assert(reqs[2].session_id == before_sid, "requested session_id mismatch: " .. tostring(reqs[2].session_id))
            assert(ress[2].session_id == before_sid, "resolved session_id must match requested: " .. tostring(ress[2].session_id))

            -- 3) The in-process listener receives the original payload.
            _G.KEEP_TUI_SECRET = mare.projection.register("TUI_PRINT_SECRET_77")
            mare.ui.print(mare.ui.Line("tui-msg TUI_PRINT_SECRET_77"))
            print_rx:recv()
            assert(#prints == 1, "expected exactly 1 ui/print event, got " .. #prints)
            assert(prints[1].text:find("tui-msg", 1, true), "missing tui-msg: " .. tostring(prints[1].text))
            assert(prints[1].text:find("TUI_PRINT_SECRET_77", 1, true), "in-process ui/print lost its original payload")
            return true
            "##,
        )
        .await;
        assert!(ok);
        let mut wire_prints = Vec::new();
        while let Some(record) = cursor.try_recv().unwrap() {
            if let mare_kernel::CursorRecord::Event { event, selected: true } = record {
                wire_prints.push(event);
            }
        }
        assert_eq!(wire_prints.len(), 1);
        let payload = &wire_prints[0].payload;
        assert!(!payload.to_string().contains("TUI_PRINT_SECRET_77"), "{payload}");
        assert!(payload["text"].as_str().unwrap().contains("[[mare-value:"));
        assert!(payload["lines"][0].as_str().unwrap().contains("[[mare-value:"));
        assert!(wire_prints[0].wire_json.contains("[[mare-value:"));
        assert!(!wire_prints[0].wire_json.contains("TUI_PRINT_SECRET_77"), "{}", wire_prints[0].wire_json);
        h.shutdown().await;
    })
    .await
}

/// remote RPC responders cancelling with `reason = "interrupted"` must
/// propagate `"interrupted"` to `ui.select`, `ui.input`, `ui.editor`, `ui.confirm`, and
/// `ui.questions` callers while pairing closed-enum `interaction/requested|resolved` events.
#[test]
fn rpc_ui_remote_interrupted_reason_propagates() {
    rpc_ui_remote_reason_propagates("interrupted");
}

#[test]
fn rpc_ui_remote_cancelled_reason_and_local_cancel_propagate() {
    rpc_ui_remote_reason_propagates("cancelled");
}

fn rpc_ui_remote_reason_propagates(reason: &str) {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("c9_interrupted_probe.lua"),
        r#"return { name = "c9-interrupted-probe", inject = { "commands", "ui" }, apply = function(ctx)
      local cancel_tok
      ctx.commands:register("c9-fire-local-cancel", { run = function() cancel_tok:cancel() end })
      ctx.commands:register("c9-interrupted", { run = function()
        local idx, sreason = ctx.ui:select({ title = "S", items = { "A" } })
        assert(idx == nil and sreason == "__REASON__", "select:" .. tostring(idx) .. ":" .. tostring(sreason))

        local iv, ireason = ctx.ui:input({ title = "I" })
        assert(iv == nil and ireason == "__REASON__", "input:" .. tostring(iv) .. ":" .. tostring(ireason))

        local ev, ereason = ctx.ui:editor({ title = "E" })
        assert(ev == nil and ereason == "__REASON__", "editor:" .. tostring(ev) .. ":" .. tostring(ereason))

        local cv, creason = ctx.ui:confirm({ prompt = "C" })
        assert(cv == nil and creason == "__REASON__", "confirm:" .. tostring(cv) .. ":" .. tostring(creason))

        local qres, qerr = ctx.ui:questions({
          title = "Q",
          questions = { { question_id = "q1", header = "H", question = "?", options = { { label = "A" } } } },
        })
        assert(qerr == nil and type(qres) == "table" and qres.dismissed == true and qres.reason == "__REASON__",
          "questions:" .. tostring(qerr) .. ":" .. mare.json.encode(qres or {}))

        if "__REASON__" == "cancelled" then
          for _, title in ipairs({"Q missing", "Q unknown"}) do
            local result, err = ctx.ui:questions({title = title, questions = {{question_id = "q1", question = "?"}}})
            assert(err == nil and result.dismissed == true and result.reason == "cancelled",
              title .. ":" .. tostring(err) .. ":" .. mare.json.encode(result or {}))
          end
          cancel_tok = mare.task.token()
          local value, why = ctx.ui:confirm({prompt = "Local cancel", cancel = cancel_tok})
          assert(value == nil and why == "cancelled", "local confirm:" .. tostring(value) .. ":" .. tostring(why))
        end
      end })
    end }"#
            .replace("__REASON__", reason),
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args(["--rpc", "--yolo", "--model", "faux/echo", "--no-session"])
        .env("MARE_HOME", home.path())
        .current_dir(home.path())
        .stderr(Stdio::null());
    let mut c = RpcClient::spawn_cmd(&mut cmd);
    c.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
    c.call_raw("events.subscribe", json!({"scope": "all", "include": ["interaction/*"]}), |_| json!(null));

    let cmd_id = c.request("commands.run", json!({"line": "/c9-interrupted"}));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut events = Vec::new();
    let mut cmd_done = false;
    let mut fire_id = None;
    let mut fire_done = reason != "cancelled";
    let mut local_ui_id = None;
    let mut saw_cancel = reason != "cancelled";
    let expected_events = if reason == "cancelled" { 16 } else { 10 };
    while !cmd_done || !fire_done || !saw_cancel || events.len() < expected_events {
        let msg = c.read_deadline(deadline).unwrap_or_else(|e| panic!("stalled ({e}): events={events:?}"));
        match msg.get("method").and_then(Value::as_str) {
            Some("ui.select") if msg.get("id").is_some() => {
                c.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":{"index":null,"reason":reason}}));
            }
            Some("ui.input" | "ui.editor") if msg.get("id").is_some() => {
                c.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":{"text":null,"reason":reason}}));
            }
            Some("ui.confirm") if msg["params"]["prompt"] == "Local cancel" => {
                local_ui_id = Some(msg["id"].clone());
                fire_id = Some(c.request("commands.run", json!({"line": "/c9-fire-local-cancel"})));
            }
            Some("ui.confirm") if msg.get("id").is_some() => {
                c.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":{"confirmed":null,"reason":reason}}));
            }
            Some("ui.questions") if msg.get("id").is_some() => {
                let response = match msg["params"]["title"].as_str() {
                    Some("Q missing") => json!({"answers":{},"dismissed":true}),
                    Some("Q unknown") => json!({"answers":{},"dismissed":true,"reason":"unknown"}),
                    _ => json!({"answers":{},"dismissed":true,"reason":reason}),
                };
                c.send(json!({"jsonrpc":"2.0","id":msg["id"],"result":response}));
            }
            Some("$/cancel_request") if local_ui_id.as_ref() == Some(&msg["params"]["id"]) => {
                saw_cancel = true;
                c.send(json!({"jsonrpc":"2.0","id":msg["params"]["id"],"result":{"confirmed":true}}));
            }
            Some("event") => {
                let p = msg["params"].clone();
                if p["type"].as_str().is_some_and(|t| t.starts_with("interaction/")) {
                    events.push(p);
                }
            }
            None if msg["id"] == cmd_id => {
                assert!(msg.get("error").is_none(), "cmd error: {msg}");
                assert!(msg["result"].is_object(), "cmd failed: {msg}");
                cmd_done = true;
            }
            None if fire_id.is_some_and(|id| msg["id"] == id) => {
                assert!(msg.get("error").is_none(), "cancel command error: {msg}");
                fire_done = true;
            }
            _ => {}
        }
    }
    assert_eq!(events.len(), expected_events, "expected interaction event pairs: {events:?}");
    for [req_ev, res_ev] in events.as_chunks::<2>().0 {
        assert_eq!(req_ev["type"], "interaction/requested");
        assert_eq!(res_ev["type"], "interaction/resolved");
        assert_eq!(req_ev["data"]["interaction_id"], res_ev["data"]["interaction_id"]);
        assert_eq!(res_ev["data"]["outcome"], "cancelled", "{res_ev}");
    }

    c.shutdown_and_wait();
}

#[test]
fn rpc_ui_get_select_preview_cross_session_cross_conn_nil_and_structured_node() {
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join("config/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(
        plugins.join("preview_probe.lua"),
        r#"return { name = "preview-probe", inject = { "commands", "ui" }, apply = function(ctx)
      ctx.commands:register("c9-preview-probe", { run = function()
        local idx = ctx.ui:select({
          title = "PreviewPick",
          items = { "NilRow", "NodeRow" },
          on_highlight = function(i)
            if i == 1 then
              return nil
            end
            return mare.ui.Lines({
              mare.ui.Line({ mare.ui.Span("row-" .. tostring(i)) }),
              mare.ui.Markdown("**structured**"),
            })
          end,
        })
        assert(idx == 2, "unexpected selected idx: " .. tostring(idx))
        ctx.ui:notify("c9-preview:ok", "info")
      end })
    end }"#,
    )
    .unwrap();

    let mut cmd = mare_cmd();
    cmd.args([
        "--rpc",
        "--peer",
        "--yolo",
        "--model",
        "faux/echo",
        "--no-session",
        "--set",
        r#"peer.allow=["ui.get_select_preview"]"#,
    ])
    .env("MARE_HOME", home.path())
    .current_dir(home.path())
    .stderr(Stdio::null());
    let mut primary = RpcClient::spawn_cmd(&mut cmd);
    let init = primary
        .call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
    assert!(init.get("error").is_none(), "{init}");
    let s1 = init["result"]["session"]["session_id"].as_str().unwrap().to_string();
    let s2 = primary.call("session.create", json!({"cwd": home.path().to_string_lossy()}))["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    #[cfg(unix)]
    let mut peer = {
        let sock = home.path().join(format!("state/peers/{}.sock", primary.pid()));
        let stream = std::os::unix::net::UnixStream::connect(&sock).unwrap();
        let mut p = RpcClient::timed(BufReader::new(stream.try_clone().unwrap()), stream);
        let p_init =
            p.call_raw("initialize", json!({"protocol_version": "3.0", "capabilities": {"ui": true}}), |_| json!(null));
        assert!(p_init.get("error").is_none(), "{p_init}");
        p
    };

    let cmd_id = primary.request("commands.run", json!({"line": "/c9-preview-probe"}));
    let mut select_req_id = None;
    let mut nil_id = None;
    let mut node_id = None;
    let mut cross_sess_id = None;
    let mut nil_res = None;
    let mut node_res = None;
    let mut saw_notify = false;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let cmd_res = loop {
        let m = primary.read_deadline(deadline).unwrap();
        if m.get("method").and_then(Value::as_str) == Some("ui.select") {
            select_req_id = m.get("id").cloned();
            let select_id = m["params"]["select_id"].as_str().unwrap().to_string();
            let active_sid = m["params"]["session_id"].as_str().unwrap().to_string();
            let other_sid = if active_sid == s1 { s2.clone() } else { s1.clone() };

            #[cfg(unix)]
            {
                let peer_err = peer.call_err(
                    "ui.get_select_preview",
                    json!({"session_id": active_sid, "select_id": select_id, "index": 0}),
                );
                assert_eq!(
                    peer_err["data"]["code"], "ui.unknown_select",
                    "cross-connection ui.get_select_preview must fail ui.unknown_select: {peer_err}"
                );
            }

            cross_sess_id = Some(primary.request(
                "ui.get_select_preview",
                json!({"session_id": other_sid, "select_id": select_id, "index": 0}),
            ));
            nil_id = Some(primary.request(
                "ui.get_select_preview",
                json!({"session_id": active_sid, "select_id": select_id, "index": 0}),
            ));
            node_id = Some(primary.request(
                "ui.get_select_preview",
                json!({"session_id": active_sid, "select_id": select_id, "index": 1}),
            ));
            continue;
        }
        if m.get("method").and_then(Value::as_str) == Some("ui.notify") && m["params"]["text"] == "c9-preview:ok" {
            saw_notify = true;
            continue;
        }
        if Some(&m["id"]) == cross_sess_id.as_ref().map(|id| json!(id)).as_ref() {
            assert_eq!(
                m["error"]["data"]["code"], "ui.unknown_select",
                "cross-session ui.get_select_preview must fail ui.unknown_select: {m}"
            );
            continue;
        }
        if Some(&m["id"]) == nil_id.as_ref().map(|id| json!(id)).as_ref() {
            assert!(m.get("error").is_none(), "nil preview failed: {m}");
            nil_res = Some(m["result"].clone());
        } else if Some(&m["id"]) == node_id.as_ref().map(|id| json!(id)).as_ref() {
            assert!(m.get("error").is_none(), "node preview failed: {m}");
            node_res = Some(m["result"].clone());
        } else if m["id"] == cmd_id {
            break m;
        }
        if nil_res.is_some()
            && node_res.is_some()
            && let Some(rid) = select_req_id.take()
        {
            primary.send(json!({"jsonrpc":"2.0","id":rid,"result":{"index":1}}));
        }
    };
    assert!(cmd_res.get("error").is_none(), "{cmd_res}");
    assert!(saw_notify, "preview probe did not finish");
    assert_eq!(nil_res.unwrap(), json!({}), "nil callback must map to empty object");
    let node_preview = &node_res.unwrap()["preview"];
    assert_eq!(node_preview["type"], "lines", "{node_preview}");
    assert_eq!(node_preview["items"][0]["type"], "line", "{node_preview}");
    assert_eq!(node_preview["items"][0]["spans"][0]["text"], "row-2", "{node_preview}");
    assert_eq!(node_preview["items"][1]["type"], "markdown", "{node_preview}");
    assert_eq!(node_preview["items"][1]["text"], "**structured**", "{node_preview}");

    #[cfg(unix)]
    drop(peer);
    primary.shutdown_and_wait();
}
