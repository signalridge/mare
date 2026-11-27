use super::*;

#[tokio::test]
async fn review_all_consecutive_same_tool_calls_not_cached_by_first_approval() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let results_path = env.home.path().join("grandchild-tool-results.json");
        let results_path_str = results_path.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(
            plugins.join("spawn-grandchild-twice.lua"),
            format!(
                r#"
            return {{ name = "spawn-grandchild-twice", apply = function(ctx)
                if mare.mode == "child" and mare.config.is_grandchild == true then
                    local exec_results = {{}}
                    ctx:on("item/completed", function(ev)
                        local item = ev and ev.item
                        if not item or item.type ~= "tool_call" then return end
                        local id = item.tool_call_id or ""
                        exec_results[id] = {{
                            result_status = item.result_status,
                            is_error = (item.message and item.message.is_error == true) or false,
                        }}
                        mare.fs.write("{results_path_str}", mare.json.encode(exec_results))
                    end)
                    return
                end
                if mare.mode ~= "child" or mare.config.spawn_grandchild_twice ~= true then return end
                ctx:on("turn/started", function()
                    local cmd = mare.os == "windows"
                        and {{
                            name = "powershell",
                            arguments = {{
                                command = "New-Item -ItemType File -Path twice-grandchild.txt -Force",
                                sandbox_permissions = "all",
                                justification = "write file",
                            }},
                        }}
                        or {{
                            name = "bash",
                            arguments = {{
                                command = "touch twice-grandchild.txt",
                                sandbox_permissions = "write:" .. mare.cwd,
                                justification = "write file",
                            }},
                        }}
                    local grandchild_script = {{
                        {{ tool_calls = {{ cmd }} }},
                        {{ tool_calls = {{ cmd }} }},
                        {{ text = "grandchild done" }}
                    }}
                    local grandchild = mare.subagent.start({{
                        prompt = "write twice",
                        no_session = true,
                        config = {{
                            model = "faux/echo",
                            is_grandchild = true,
                            plugins = {{
                                approvals = {{ review_all = true }},
                                ["provider-faux"] = {{ script_json = mare.json.encode(grandchild_script) }}
                            }}
                        }}
                    }})
                    grandchild:wait()
                end)
            end }}
            "#
            ),
        )
        .unwrap();
        let mut b = env
            .host()
            .config(json!({
                "agents": { "max_active": 4, "max_depth": 2 },
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "read-only"
                    },
                    "approvals": { "review_all": false }
                }
            }))
            .no_session();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        let summary_json: String = eval(
            &h,
            r#"
            local dialog_calls = {}
            local approval_results = {}
            local second_rule_verdict = nil
            mare.ui.connected = function() return true end
            C.cordis:on("approval/resolved", function(ev)
                local id = ev.result and ev.result.call and ev.result.call.tool_call_id or ""
                approval_results[id] = ev.result and ev.result.decision
            end)
            C.cordis:on("agent/approval-dialog", function(ev)
                local call = ev and ev.call or nil
                local info = ev and ev.info or nil
                local id = call and call.id or ""
                table.insert(dialog_calls, id)
                if #dialog_calls == 1 then
                    return { choice = "allow_amend" }
                end
                local perms = C.cordis:get("permissions")
                local rv = perms and perms:rule_verdict(call, info) or nil
                second_rule_verdict = rv and rv.decision or "none"
                return { choice = "deny", reason = "denied second call under review_all" }
            end, { prepend = true })
            local child_script = {
                { text = "child done" }
            }
            local child = mare.subagent.start({
                prompt = "delegate",
                no_session = true,
                config = {
                    model = "faux/echo",
                    spawn_grandchild_twice = true,
                    plugins = {
                        approvals = { review_all = false },
                        ["provider-faux"] = { script_json = mare.json.encode(child_script) }
                    }
                }
            })
            child:wait()
            return mare.json.encode({
                dialog_calls = dialog_calls,
                approval_results = approval_results,
                second_rule_verdict = second_rule_verdict,
            })
            "#,
        )
        .await;
        let summary: serde_json::Value = serde_json::from_str(&summary_json).unwrap();
        assert_eq!(
            summary["dialog_calls"],
            json!(["call_1_1", "call_2_1"]),
            "both consecutive calls must reach approval dialog: {summary}"
        );
        assert_eq!(
            summary["second_rule_verdict"], "allow",
            "first allow_amend must persist a shared allow rule matching the second call: {summary}"
        );
        assert_eq!(
            summary["approval_results"]["call_1_1"], "allow_amend",
            "first call approval verdict must be allow_amend: {summary}"
        );
        assert_eq!(
            summary["approval_results"]["call_2_1"], "deny",
            "second call approval verdict must be deny: {summary}"
        );

        let perms_path = env.home.path().join("state/permissions.json");
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&perms_path).unwrap()).unwrap();
        let rules = saved.get("rules").and_then(|r| r.as_array()).expect("rules must be an array");
        assert_eq!(rules.len(), 1, "first allow_amend must persist one shared rule: {rules:?}");

        assert!(env.cwd.join("twice-grandchild.txt").exists(), "first approved call must actually execute");
        let exec_results: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&results_path).unwrap()).unwrap();
        assert_eq!(
            exec_results["call_1_1"]["result_status"], "success",
            "first call must execute with success: {exec_results}"
        );
        assert_eq!(exec_results["call_1_1"]["is_error"], false, "first call must not error: {exec_results}");
        assert_eq!(
            exec_results["call_2_1"]["result_status"], "denied",
            "second call must be denied and not execute: {exec_results}"
        );
        assert_eq!(
            exec_results["call_2_1"]["is_error"], true,
            "second call must return error when denied: {exec_results}"
        );
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn parent_tool_approval_allow_does_not_bypass_shared_deny_rule_for_child() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "auto"
                    },
                    "approvals": { "policy": "untrusted", "review_all": false }
                }
            }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        // The child's write reaches the parent's policy gate, which allows it with no host to
        // ask; the shared forbidden rule must decide before that allow answers, so
        // the parent's approval audit never records an allow for the call. Observe the path and
        // the audit (session on), not just the missing file.
        let observed: String = eval(
            &h,
            r#"
            local gate_calls, results, tool_result = 0, {}, nil
            C.cordis:get("permissions"):add_rules({ { tool = "write", decision = "forbidden", justification = "no writes" } })
            C.cordis:get("policy"):voter("tool/approval", function(call)
                if call.origin and call.name == "write" then gate_calls = gate_calls + 1 end
                return { decision = "allow", source = "policy" }
            end)
            C.cordis:on("pipeline/approval-decided", function(ev)
                if ev.origin then results[#results + 1] = { decision = ev.decision, source = ev.source } end
            end)
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "child-shared-deny.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = { ["provider-faux"] = { script_json = mare.json.encode(child_script) } }
                }
            })
            local res = child:wait()
            for _, m in ipairs(res.messages or {}) do
                if m.role == "tool_result" then tool_result = { is_error = m.is_error == true } end
            end
            -- the parent's approval audit for the child's tool call: requested (call id) -> resolved
            local asked, decided = {}, {}
            for _, r in ipairs(S:list_records("approval/requested").data) do
                if r.category == "tool" then asked[r.approval_id] = r.tool_call_id end
            end
            for _, r in ipairs(S:list_records("approval/resolved").data) do
                if asked[r.approval_id] then decided[#decided + 1] = r.result.decision end
            end
            return mare.json.encode({ gate_calls = gate_calls, results = results, tool_result = tool_result, decided = decided })
            "#,
        )
        .await;
        let observed: serde_json::Value = serde_json::from_str(&observed).unwrap();
        assert!(observed["gate_calls"].as_i64().unwrap() >= 1, "the child's write must reach the parent gate: {observed}");
        assert_eq!(
            observed["results"],
            json!([{ "decision": "deny", "source": "rule" }]),
            "the shared rule, not the gate's allow, decides the child's call: {observed}"
        );
        assert_eq!(observed["tool_result"], json!({ "is_error": true }), "{observed}");
        assert_eq!(
            observed["decided"],
            json!(["deny"]),
            "the approval audit records the shared rule's deny, never the gate's allow: {observed}"
        );
        assert!(!env.cwd.join("child-shared-deny.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn parent_review_all_cannot_be_disabled_by_child() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .config(json!({
                "plugins": {
                    "permissions": {
                        "enabled": true,
                        "preset": "auto",
                        "rules": [{ "tool": "write", "decision": "allow" }]
                    },
                    "approvals": { "review_all": true }
                }
            }))
            .no_session();
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        exec(
            &h,
            r#"
            DIALOG_CALLED = 0
            mare.ui.connected = function() return true end
            C.cordis:on("agent/approval-dialog", function()
                DIALOG_CALLED = DIALOG_CALLED + 1
                return { choice = "deny", reason = "parent review_all enforced" }
            end, { prepend = true })
            local child_script = {
                { tool_calls = {{ name = "write", arguments = { path = "child-disable-review.txt", content = "nope" } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "write",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = {
                        approvals = { review_all = false },
                        ["provider-faux"] = { script_json = mare.json.encode(child_script) }
                    }
                }
            })
            child:wait()
            "#,
        )
        .await;
        assert!(
            !env.cwd.join("child-disable-review.txt").exists(),
            "child must not disable parent review_all"
        );
        assert_eq!(
            eval::<i64>(&h, "return DIALOG_CALLED").await,
            1,
            "parent review_all must still require dialog even if child sets review_all = false"
        );
        h.shutdown().await;
    })
    .await
}

/// A targeted reload of `approvals` (`/reload approvals`) keeps the policy the session recorded.
/// The reloaded plugin restores in `apply`, before its session follower is filled, and a targeted
/// reload sends no `session/loaded`; it reads the active session instead. Same host, same
/// session: the tool entry still asks after the reload (Codex thread_resume.rs:1352, an explicit
/// saved policy wins over the default; Kimi agentLifecycle.test.ts:1187).
#[tokio::test]
async fn a_targeted_reload_of_approvals_keeps_the_sessions_policy() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"full-access"}), json!({})).await;
        exec(&h, SETUP).await;
        let (before, after, refused, wrote): (String, String, bool, i64) = eval(
            &h,
            r#"
            local wrote = 0
            T:register("write_note", {description = "write", side_effect = "write", parameters = {type="object"},
                execute = function() wrote = wrote + 1; return {content="wrote"} end})
            Q:set("untrusted")
            local before = Q:policy()
            local kernel = C.cordis:get("kernel")
            kernel:reload("approvals")
            kernel:reload_barrier()
            Q = C.cordis:get("approvals")
            local after = Q:policy()
            local r = EXECUTE({id="after-reload", name="write_note", arguments={}}, {})
            return before, after, r.refused == true, wrote
        "#,
        )
        .await;
        assert_eq!((before.as_str(), after.as_str(), refused, wrote), ("untrusted", "untrusted", true, 0));
        h.shutdown().await;
    })
    .await
}

/// A failed `approval/requested` record write fails the approval before asking any answerer or
/// emitting `approval/requested`.
#[tokio::test]
async fn failed_approval_record_write_fails_approval() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"full-access"}), json!({})).await;
        exec(&h, SETUP).await;
        let (refused, ran_count, answered_count, requested_events): (bool, i64, i64, i64) = eval(
            &h,
            r#"
            local req_events = 0
            C.cordis:on("approval/requested", function() req_events = req_events + 1 end)
            local orig_record = S.add_record
            S.add_record = function(self, kind, data, opts)
                if kind == "approval/requested" then
                    error("injected audit write failure")
                end
                return orig_record(self, kind, data, opts)
            end
            local r = RUN()
            return r.refused == true, ran, answered, req_events
        "#,
        )
        .await;
        assert!(refused, "approval must fail when approval/requested record fails");
        assert_eq!((ran_count, answered_count, requested_events), (0, 0, 0));
        h.shutdown().await;
    })
    .await
}

/// An internal `assert` failure in `pipeline_guard` after `approval/requested` must still emit
/// a paired `approval/resolved` (`decision = "deny"`, `source = "error"`) and refuse execution.
#[tokio::test]
async fn pipeline_guard_assert_still_emits_paired_approval_resolved() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"full-access"}), json!({})).await;
        exec(&h, SETUP).await;
        let out: mlua::Value = eval(
            &h,
            r#"
            local requested, resolved = {}, {}
            local unconf_ran = 0
            T:register("unconf_assert", {
                description = "unconfined assert test",
                unconfined = true,
                parameters = { type = "object" },
                execute = function() unconf_ran = unconf_ran + 1; return { content = "ok" } end,
            })
            C.cordis:get("policy"):voter("tool/before", function(call)
                if call.name == "unconf_assert" then return { decision = "ask", reason = "test" } end
            end)
            C.cordis:on("approval/requested", function(ev) requested[#requested + 1] = ev end)
            C.cordis:on("approval/resolved", function(ev) resolved[#resolved + 1] = ev end)
            local r = EXECUTE({id="assert-call", name="unconf_assert", arguments={}}, {
                approve = function()
                    return { decision = "allow", permissions = { read = { mare.cwd } } }
                end,
            })
            return { refused = r.refused == true, ran = unconf_ran, content = tostring(r.content), requested = requested, resolved = resolved }
        "#,
        )
        .await;
        let out = mare_shared::lua::lua_to_json(h.lua(), out).unwrap();
        assert_eq!(out["refused"], true, "assert failure must refuse execution: {out}");
        assert_eq!(out["ran"], 0);
        let reqs = out["requested"].as_array().expect("requested");
        let ress = out["resolved"].as_array().expect("resolved");
        assert_eq!(reqs.len(), 1, "expected 1 requested: {out}");
        assert_eq!(ress.len(), 1, "expected 1 resolved: {out}");
        assert_eq!(ress[0]["approval_id"], reqs[0]["approval_id"]);
        assert_eq!(ress[0]["tool_call_id"], "assert-call");
        assert_eq!(ress[0]["result"]["decision"], "deny");
        assert_eq!(ress[0]["result"]["source"], "error");
        h.shutdown().await;
    })
    .await
}

/// A failed `approval/resolved` record write on the default `emit_resolved = true` path (e.g.
/// `mcp_elicitation`) still emits `approval/resolved` (`decision = "deny"`, `source = "error"`)
/// before propagating the persistence error.
#[tokio::test]
async fn default_emit_path_resolved_record_failure_still_emits_denied_resolved() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"full-access"}), json!({})).await;
        exec(&h, SETUP).await;
        let out: mlua::Value = eval(
            &h,
            r#"
            local requested, resolved = {}, {}
            C.cordis:on("approval/requested", function(ev) requested[#requested + 1] = ev end)
            C.cordis:on("approval/resolved", function(ev) resolved[#resolved + 1] = ev end)
            local orig_record = S.add_record
            S.add_record = function(self, kind, data, opts)
                if kind == "approval/resolved" then
                    error("injected resolved write failure")
                end
                return orig_record(self, kind, data, opts)
            end
            local ok, err = pcall(function()
                return Q:request("mcp_elicitation", { server = "srv" }, function()
                    return { decision = "ask" }
                end, function()
                    return { decision = "allow" }
                end)
            end)
            return { ok = ok, err = tostring(err), requested = requested, resolved = resolved }
        "#,
        )
        .await;
        let out = mare_shared::lua::lua_to_json(h.lua(), out).unwrap();
        assert_eq!(out["ok"], false, "resolved record write failure must error: {out}");
        let reqs = out["requested"].as_array().expect("requested");
        let ress = out["resolved"].as_array().expect("resolved");
        assert_eq!(reqs.len(), 1, "expected 1 requested: {out}");
        assert_eq!(ress.len(), 1, "expected 1 resolved: {out}");
        assert_eq!(ress[0]["approval_id"], reqs[0]["approval_id"]);
        assert_eq!(reqs[0]["category"], "mcp_elicitation");
        assert_eq!(ress[0]["result"]["decision"], "deny");
        assert_eq!(ress[0]["result"]["source"], "error");
        h.shutdown().await;
    })
    .await
}

/// Hard-cancelling a suspended Lua approval task (`task:cancel()`) pairs the already-emitted
/// `approval/requested` with a fallback `approval/resolved` (`decision = "deny"`,
/// `source = "cancelled"`, `outcome = "cancelled"`) on both `approvals:request` and `pipeline_guard`.
#[tokio::test]
async fn hard_cancelled_lua_task_emits_paired_cancelled_resolved() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({"preset":"full-access"}), json!({})).await;
        exec(&h, SETUP).await;
        let out: mlua::Value = eval(
            &h,
            r#"
            local function run_hard_cancel(fn_to_run)
                local requested, resolved = {}, {}
                local entered_tx, entered_rx = mare.task.channel()
                local gate_tx, gate_rx = mare.task.channel()
                local off_req = C.cordis:on("approval/requested", function(ev) requested[#requested + 1] = ev end)
                local off_res = C.cordis:on("approval/resolved", function(ev) resolved[#resolved + 1] = ev end)
                local task = mare.task.spawn(fn_to_run, entered_tx, gate_rx)
                assert(entered_rx:recv() == true, "approval wait did not start")
                for _ = 1, 20 do
                    if #requested >= 1 then break end
                    mare.task.yield()
                end
                assert(#requested == 1, "expected approval/requested before cancel")
                task:abort()
                for _ = 1, 40 do
                    if #resolved >= 1 then break end
                    mare.task.yield()
                end
                gate_rx:close()
                off_req()
                off_res()
                return requested, resolved
            end

            local req1, res1 = run_hard_cancel(function(entered_tx, gate_rx)
                return Q:request("mcp_elicitation", { server = "srv" }, function()
                    return { decision = "ask" }
                end, function()
                    entered_tx:send(true)
                    gate_rx:recv()
                    return { decision = "allow" }
                end)
            end)

            local req2, res2 = run_hard_cancel(function(entered_tx, gate_rx)
                return EXECUTE({id="hard-cancel-tool", name="effect", arguments={}}, {
                    approve = function()
                        entered_tx:send(true)
                        gate_rx:recv()
                        return { decision = "allow" }
                    end,
                })
            end)

            return { req1 = req1, res1 = res1, req2 = req2, res2 = res2 }
        "#,
        )
        .await;
        let out = mare_shared::lua::lua_to_json(h.lua(), out).unwrap();
        for (req_k, res_k, category) in [("req1", "res1", "mcp_elicitation"), ("req2", "res2", "tool")] {
            let reqs = out[req_k].as_array().expect(req_k);
            let ress = out[res_k].as_array().expect(res_k);
            assert_eq!(reqs.len(), 1, "{req_k}: {out}");
            assert_eq!(ress.len(), 1, "{res_k}: {out}");
            assert_eq!(reqs[0]["category"], category);
            assert_eq!(ress[0]["approval_id"], reqs[0]["approval_id"]);
            assert_eq!(ress[0]["result"]["decision"], "deny");
            assert_eq!(ress[0]["result"]["source"], "cancelled");
            assert_eq!(ress[0]["result"]["outcome"], "cancelled");
        }
        h.shutdown().await;
    })
    .await
}

/// `agent.rewrite_queued` must project `text`, `images`, and `image_markers` before entering
/// `session/commit-ready` or `agent:list_queue()`, and missing ids must not emit spurious `queue/changed` events.
#[tokio::test]
async fn edit_queued_projects_registered_values_before_commit_ready_and_noop_skips_queue_event() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let raw: String = eval(
            &h,
            r#"
            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get("policy"):voter("request/ready", function()
                if hold_tx then
                    local tx = hold_tx; hold_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = "deny", reason = "gate" }
                end
            end)
            local held_turn = A:prompt("hold")
            hold_rx:recv()

            local secret = "EDIT_QUEUED_SECRET_47"
            local kept = mare.projection.register(secret)
            local commit_ready = {}
            local queues = {}
            C.cordis:get("policy"):voter("session/commit-ready", function(batch)
                table.insert(commit_ready, mare.json.encode(batch))
            end)
            C.cordis:on("queue/changed", function(q)
                table.insert(queues, mare.json.encode(q))
            end)

            assert(A:prompt("initial queued item", { when_busy = "queue" }).status == "queued")
            local queued = A:list_queue().data
            assert(#queued == 1)
            local id = queued[1].input_id
            local commits_before_edit = #commit_ready
            local queues_before_edit = #queues

            local updated = A:update_queued(id, {
                input = "edited " .. secret .. " [marker " .. secret .. "] " .. secret,
                images = { { type = "image", mime = "image/png", data = "ZGF0YQ==", metadata = { tag = secret } } },
                image_markers = { { text = "[marker " .. secret .. "]", offset = #("edited " .. secret .. " ") } },
            })
            assert(updated and updated.input_id == id)
            local commits_after_edit = #commit_ready
            local queues_after_edit = #queues

            local missing_ok = pcall(A.update_queued, A, "missing-id", { input = "noop" })
            assert(missing_ok == false)
            local commits_after_noop = #commit_ready
            local queues_after_noop = #queues

            local last_batch = mare.json.decode(commit_ready[commits_after_edit])
            local edited_raw = last_batch.inbox.follow_up[1]
            local m = edited_raw.image_markers[1]
            assert(
                edited_raw.text:sub(m.offset + 1, m.offset + #m.text) == m.text,
                "image marker offset must track projected text"
            )
            local after_snapshot = A:list_queue().data
            A:cancel({ turn_id = held_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(held_turn.turn_id)

            return mare.json.encode({
                reference = kept.reference,
                commits_before_edit = commits_before_edit,
                commits_after_edit = commits_after_edit,
                commits_after_noop = commits_after_noop,
                queues_before_edit = queues_before_edit,
                queues_after_edit = queues_after_edit,
                queues_after_noop = queues_after_noop,
                edit_commit = commit_ready[commits_after_edit] or "",
                snapshot = mare.json.encode(after_snapshot),
            })
        "#,
        )
        .await;
        let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let reference = report["reference"].as_str().unwrap();
        assert_eq!(report["commits_before_edit"].as_i64().unwrap(), 1);
        assert_eq!(report["commits_after_edit"].as_i64().unwrap(), 2);
        assert_eq!(report["commits_after_noop"].as_i64().unwrap(), 2);
        assert_eq!(report["queues_before_edit"].as_i64().unwrap(), 1);
        assert_eq!(report["queues_after_edit"].as_i64().unwrap(), 2);
        for field in ["edit_commit", "snapshot"] {
            let body = report[field].as_str().unwrap();
            assert!(!body.contains("EDIT_QUEUED_SECRET_47"), "raw secret leaked in {field}: {body}");
            assert!(body.contains(reference), "projected reference missing in {field}: {body}");
        }
        assert_eq!(
            report["queues_after_noop"].as_i64().unwrap(),
            2,
            "missing update_queued must not emit queue/changed"
        );
        h.shutdown().await;
    })
    .await
}

/// Secrets registered *after* an entry is already queued must still be projected at `session.commit`
/// before `session/commit-ready` or disk persistence sees `inbox`, with `image_markers` offsets
/// kept aligned with the projected text.
#[tokio::test]
async fn session_commit_reprojects_previously_enqueued_inbox_before_commit_ready() {
    local(async {
        let env = TestEnv::new().await;
        let h = env.host().build().await;
        expose(&h);
        let raw: String = eval(
            &h,
            r#"
            local hold_tx, hold_rx = mare.task.channel()
            local rel_tx, rel_rx = mare.task.channel()
            C.cordis:get("policy"):voter("request/ready", function()
                if hold_tx then
                    local tx = hold_tx; hold_tx = nil
                    tx:send(true)
                    rel_rx:recv()
                    return { decision = "deny", reason = "gate" }
                end
            end)
            local held_turn = A:prompt("hold")
            hold_rx:recv()

            local secret = "LATE_QUEUED_SECRET_99"
            local commit_ready = {}
            C.cordis:get("policy"):voter("session/commit-ready", function(batch)
                table.insert(commit_ready, mare.json.encode(batch))
            end)

            assert(A:prompt("pre " .. secret .. " [marker " .. secret .. "]", {
                when_busy = "queue",
                images = { { type = "image", mime = "image/png", data = "ZGF0YQ==", metadata = { tag = secret } } },
                image_markers = { { text = "[marker " .. secret .. "]", offset = #("pre " .. secret .. " ") } },
            }).status == "queued")

            local kept = mare.projection.register(secret)
            assert(A:prompt("second trigger item", { when_busy = "queue" }).status == "queued")

            local second_batch = mare.json.decode(commit_ready[2])
            local first = second_batch.inbox.follow_up[1]
            local m = first.image_markers[1]
            assert(
                first.text:sub(m.offset + 1, m.offset + #m.text) == m.text,
                "reprojected inbox entry marker offset must match projected text"
            )
            local snap = A:list_queue().data
            A:cancel({ turn_id = held_turn.turn_id, clear_queue = false })
            rel_tx:send(true)
            A:wait(held_turn.turn_id)

            return mare.json.encode({
                reference = kept.reference,
                commit_count = #commit_ready,
                second_commit = commit_ready[2] or "",
                snapshot = mare.json.encode(snap),
            })
        "#,
        )
        .await;
        let report: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let reference = report["reference"].as_str().unwrap();
        assert_eq!(report["commit_count"].as_i64().unwrap(), 2);
        for field in ["second_commit", "snapshot"] {
            let body = report[field].as_str().unwrap();
            assert!(!body.contains("LATE_QUEUED_SECRET_99"), "raw secret leaked in {field}: {body}");
            assert!(body.contains(reference), "projected reference missing in {field}: {body}");
        }
        h.shutdown().await;
    })
    .await
}
