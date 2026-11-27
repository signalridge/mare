use super::*;

/// "Always allow" in a sub-agent's approval saves the rule the child proposed, unchanged, like
/// Codex (`codex_delegate.rs:519`/`:576` forwards the proposed amendment, `handlers.rs:381`
/// persists it): the parent's dialog shows that rule's own fields ("don't ask again for commands
/// that start with …", Codex `approval_overlay.rs:855`) and exactly the object shown is written —
/// not the child's description, and no amendment arriving later with a sub-agent's call.
#[tokio::test]
async fn a_child_always_allow_saves_exactly_the_rule_the_dialog_showed() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", touch_script("made.txt")).await;
        let (item, token): (String, i64) = eval(
            &h,
            r#"local item, token
               C.cordis:on("pipeline/approval-decided", function(ev) token = ev.rule_token end)
               mare.ui.connected = function() return true end
               mare.ui.approve = function(o)
                   item = tostring(o.amendment_description or "")
                   return "allow_amend" -- remember
               end
               SPAWN({ prompt = "touch", label = "one", permission = "read-only" })
               return item, token"#,
        )
        .await;
        let doc = rules_file(&env);
        let rules = doc["rules"].as_array().expect("saved").clone();
        assert_eq!(rules.len(), 1, "{doc}");
        let rule = &rules[0];
        let cwd = rule["cwd"].as_str().unwrap();
        let deny = rule["deny_args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect::<Vec<_>>();
        assert_eq!(
            item,
            format!(
                "don't ask again for commands that start with `touch made.txt`, \
                 with `all` access, in {cwd} (not with {})",
                deny.join(" ")
            )
        );
        let mut keys: Vec<&str> = rule.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["argv", "cwd", "decision", "deny_args", "expires", "scope"], "no description: {rule}");
        assert_eq!(
            (&rule["argv"], &rule["scope"], &rule["decision"], &rule["expires"]),
            (&json!(["touch", "made.txt"]), &json!("all"), &json!("allow"), &json!("never"))
        );
        // a different amendment arriving afterwards with a sub-agent's call — under the spent
        // token, or none — is not saved
        global(&h, "TOKEN", token);
        exec(
            &h,
            r#"for _, t in ipairs({ TOKEN, TOKEN + 1, false }) do
                   C.cordis:get("bus"):emit("pipeline/approval-decided", {
                       call = { name = "bash", origin = { label = "one" }, arguments = { command = "rm -rf x" } },
                       decision = "allow_amend", rule_token = t or nil,
                       amendment = { argv = { "rm" }, decision = "allow", scope = "all" } })
               end"#,
        )
        .await;
        assert_eq!(rules_file(&env)["rules"], json!([rule]), "only the shown rule");
        h.shutdown().await;
    })
    .await
}

/// A rule saved from one child escalates every child sharing the parent's configuration (Codex
/// 5548c95d66: the child uses the parent's exec policy, `agent/control.rs:648-653`, and an allow
/// match runs with `bypass_sandbox`, `exec_policy.rs:413-427`): the second child isn't asked and
/// its call runs past its read-only ceiling with the rule's scope.
#[tokio::test]
async fn a_shared_allow_escalates_a_second_child() {
    local(async {
        let mut env = TestEnv::new().await;
        // Exercise the default Windows tier's missing network control on every platform.
        env.set_var("MARE_SANDBOX_CAPS", "fs_write,fs_protect,unix_socket");
        let h = parent(&env, "read-only", touch_script("made.txt")).await;
        let dialogs: usize = eval(
            &h,
            r#"N = 0
               mare.ui.connected = function() return true end
               mare.ui.approve = function() N = N + 1 return "allow_amend" end -- remember
               SPAWN({ prompt = "touch", label = "one" })
               return N"#,
        )
        .await;
        assert_eq!(dialogs, 1);
        std::fs::remove_file(env.cwd.join("made.txt")).expect("the first child's approved call ran");
        let (dialogs, messages, parent_vote): (usize, String, String) = eval(
            &h,
            r#"local a = mare.subagent.start({ prompt = "touch", label = "two", cwd = CWD, no_session = true })
               local v = P:check({ name = "bash", arguments = { command = "touch made.txt",
                   sandbox_permissions = "all", justification = "x" } }, { side_effect = "external", command_tool = true, command = { shell = "bash", arg = "command" } })
               return N, mare.json.encode(a:wait().messages), v.decision"#,
        )
        .await;
        assert_eq!(dialogs, 1, "the second child used the shared rule");
        assert_eq!(parent_vote, "allow", "the parent sees the child's rule");
        assert!(env.cwd.join("made.txt").exists(), "it ran escalated, not capped at read-only: {messages}");
        h.shutdown().await;
    })
    .await
}

/// Under a shared allow a deny rule still refuses the command, and under a ceiling that restricts
/// reads the escalation widens only what the rule grants and stays sandboxed (Codex
/// `sandboxing.rs:246-252`: a deny-read policy is never run unsandboxed; the denied reads
/// themselves: `suite::deny_read`).
#[tokio::test]
async fn deny_rules_and_read_restrictions_still_hold_under_a_shared_allow() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.home.path().join("state")).unwrap();
        std::fs::write(
            env.home.path().join("state/permissions.json"),
            json!({ "version": 1, "rules": [
                { "tool": "write", "decision": "allow" },
                { "argv": ["touch"], "decision": "allow", "scope": "all" },
                { "argv": ["touch", "blocked.txt"], "decision": "forbidden" },
            ] })
            .to_string(),
        )
        .unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "w.txt", "content": "hi" } }] },
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "touch blocked.txt",
                "sandbox_permissions": "all", "justification": "x" } }] },
            { "text": "done" }
        ]);
        let h = parent(&env, "read-only", script).await;
        let (asked, messages): (usize, String) = eval(
            &h,
            r#"LISTEN()
               local a = mare.subagent.start({ prompt = "go", cwd = CWD, no_session = true })
               return #ASKED, mare.json.encode(a:wait().messages)"#,
        )
        .await;
        assert_eq!(asked, 0, "{messages}");
        assert!(env.cwd.join("w.txt").exists(), "the shared tool rule escalated the write: {messages}");
        assert!(messages.contains("forbidden by a permission rule"), "{messages}");
        assert!(!env.cwd.join("blocked.txt").exists());
        // the same shared rule under a ceiling that restricts reads: the write is granted (the
        // workspace is readable), sandboxed with the reads kept
        std::fs::remove_file(env.cwd.join("w.txt")).unwrap();
        let messages: String = eval(
            &h,
            r#"mare.proc.sandbox.set_current({ level = "read-only", restrict_read = true, readable = { CWD } })
               local a = mare.subagent.start({ prompt = "go", cwd = CWD, no_session = true })
               return mare.json.encode(a:wait().messages)"#,
        )
        .await;
        assert!(env.cwd.join("w.txt").exists(), "{messages}");
        assert!(messages.contains("forbidden by a permission rule"), "{messages}");
        h.shutdown().await;
    })
    .await
}

/// Rules a child gets from its spawn-time `config` aren't the parent's: the child doesn't share
/// the parent's configuration and stays capped by its ceiling.
#[tokio::test]
async fn spawn_time_permission_rules_stay_capped() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", write_script("cap.txt")).await;
        let (asked, messages): (usize, String) = eval(
            &h,
            r#"LISTEN()
               local a = mare.subagent.start({ prompt = "write", cwd = CWD, no_session = true,
                   config = { plugins = { permissions = { rules = { { tool = "write", decision = "allow" } } } } } })
               return #ASKED, mare.json.encode(a:wait().messages)"#,
        )
        .await;
        assert_eq!(asked, 0, "the child's own rule allowed it: {messages}");
        assert!(!env.cwd.join("cap.txt").exists(), "capped at read-only: {messages}");
        h.shutdown().await;
    })
    .await
}

/// Two children ask for the same call. A remembered allow resolves the queued request like a
/// fresh call; a deny arriving while the first dialog is open voids that approval and also denies
/// the queued request (not Kimi's "same action, approve the rest", `approval/controller.ts:14-25`).
#[tokio::test]
async fn a_queued_child_request_is_resolved_like_a_fresh_call() {
    for deny in [false, true] {
        local(async {
            let mut env = TestEnv::new().await;
            env.set_var("MARE_SANDBOX_CAPS", "fs_write,fs_protect,unix_socket");
            let h = parent(&env, "read-only", touch_script("q.txt")).await;
            global(&h, "DENY", deny);
            let (dialogs, results): (usize, String) = eval(
                &h,
                r#"local n, pending, results = 0, 0, {}
                   C.cordis:on("agent/approval-queue", function(ev) pending = #ev.pending end)
                   C.cordis:on("pipeline/approval-decided", function(ev)
                       results[#results + 1] = ev.source .. ":" .. ev.decision
                   end)
                   mare.ui.connected = function() return true end
                   mare.ui.approve = function()
                       n = n + 1
                       for _ = 1, 200 do -- the other child's request queues up behind this one
                           if pending > 0 then break end
                           mare.task.sleep(10)
                       end
                       if DENY then
                           mare.fs.write(mare.paths.state .. "/permissions.json", mare.json.encode({ version = 1,
                               rules = { { argv = { "touch", "q.txt" }, decision = "forbidden" } } }))
                       end
                       return "allow_amend" -- remember
                   end
                   local a = mare.subagent.start({ prompt = "one", label = "a", cwd = CWD, no_session = true })
                   local b = mare.subagent.start({ prompt = "two", label = "b", cwd = CWD, no_session = true })
                   a:wait() b:wait()
                   return n, table.concat(results, " ")"#,
            )
            .await;
            assert_eq!(dialogs, 1, "{results}");
            let results_expected = if deny { "rule:deny rule:deny" } else { "policy:allow_amend rule:allow" };
            assert_eq!(results, results_expected);
            h.shutdown().await;
        })
        .await
    }
}

/// "Allow for this session" and session-scoped rules from a child stay with that child: nothing
/// reaches the parent's session or the shared rules file; a rule the child offers that doesn't
/// match its call (a forged amendment) is never saved.
#[tokio::test]
async fn a_child_session_grant_and_a_forged_rule_stay_out_of_the_shared_table() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", write_script("l.txt")).await;
        let parent_vote: String = eval(
            &h,
            r#"mare.ui.connected = function() return true end
               mare.ui.approve = function() return "allow_amend" end -- "allow all write calls this session"
               SPAWN({ prompt = "write l", label = "one" })
               return P:check({ name = "write", arguments = { path = "l.txt", content = "x" } },
                   { side_effect = "write" }).decision"#,
        )
        .await;
        assert_eq!(parent_vote, "ask", "the child's session rule didn't reach the parent");
        assert!(env.cwd.join("l.txt").exists());
        // a forged amendment arriving with a child's call: `rm` for a `touch` call
        exec(
            &h,
            r#"C.cordis:get("bus"):emit("pipeline/approval-decided", {
                   call = { name = "bash", origin = { label = "x" }, arguments = { command = "touch a" } },
                   decision = "allow_amend",
                   amendment = { argv = { "rm" }, decision = "allow", scope = "all" } })"#,
        )
        .await;
        assert!(!env.home.path().join("state/permissions.json").exists(), "nothing was saved");
        h.shutdown().await;
    })
    .await
}

/// Concurrent sub-agent approvals: one dialog at a time, in order; while one is open the others
/// are announced as waiting (`agent/approval-queue`, the TUI's "approval needed: <child> (+N)"
/// line; Codex `pending_thread_approvals.rs`), and the dialog's title names the child.
#[tokio::test]
async fn waiting_child_approvals_are_announced_while_one_dialog_is_open() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("a")).unwrap();
        std::fs::create_dir_all(env.cwd.join("b")).unwrap();
        let h = parent(&env, "auto", write_script("q.txt")).await;
        let seen: String = eval(
            &h,
            r#"local pending, seen = {}, {}
               C.cordis:on("agent/approval-queue", function(ev)
                   pending = {}
                   for _, o in ipairs(ev.pending) do pending[#pending + 1] = o.label end
               end)
               mare.ui.connected = function() return true end
               mare.ui.approve = function(o)
                   -- the first dialog waits until the other child's request has queued up
                   for _ = 1, 500 do
                       if #seen > 0 or #pending > 0 then break end
                       mare.task.sleep(10)
                   end
                   seen[#seen + 1] = o.wait_title:match("^%[(%a)") .. ":" .. table.concat(pending, ",")
                   return "allow"
               end
               local a = mare.subagent.start({ prompt = "one", label = "a", cwd = CWD .. "/a", no_session = true,
                 permission = "read-only" })
               local b = mare.subagent.start({ prompt = "two", label = "b", cwd = CWD .. "/b", no_session = true,
                 permission = "read-only" })
               a:wait() b:wait()
               return table.concat(seen, " ")"#,
        )
        .await;
        // whichever asked first: its dialog lists the other as waiting; the second lists nobody
        assert!(seen == "a:b b:" || seen == "b:a a:", "{seen}");
        h.shutdown().await;
    })
    .await
}

/// Headless (no UI, no approval listener): a child's approval is denied without any dialog, the
/// child's tool result says nobody could answer, and the parent reports `unavailable` (what
/// headless exit code 3 counts).
#[tokio::test]
async fn headless_child_approval_is_denied_without_a_dialog() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", write_script("n.txt")).await;
        let (dialogs, decision, messages): (usize, String, String) = eval(
            &h,
            r#"local n, result = 0, nil
               mare.ui.connected = function() return false end
               mare.ui.approve = function() n = n + 1 return "allow" end
               C.cordis:on("pipeline/approval-decided", function(ev) result = ev end)
               local a = mare.subagent.start({ prompt = "write n", cwd = CWD, no_session = true })
               local res = a:wait()
               return n, result.decision, mare.json.encode(res.messages)"#,
        )
        .await;
        assert_eq!(dialogs, 0);
        assert_eq!(decision, "unavailable");
        assert!(messages.contains("nobody can answer"), "{messages}");
        assert!(!env.cwd.join("n.txt").exists());
        h.shutdown().await;
    })
    .await
}

/// The dialog is a plugin (`agent-approvals`) and the host stays fail-closed without it (reference implementation
/// `ApprovalService`: no answerer, a throwing one or a rogue answer is `unavailable`): turned off,
/// failing, or answering outside the closed set of choices, the request is denied as
/// `unavailable` with the reason in the child's tool result — never approved.
#[tokio::test]
async fn without_a_working_approval_dialog_a_child_request_is_unavailable() {
    // (built-ins off, Lua run before the child asks)
    let cases = [
        (json!(["agent-approvals"]), "", "no approval dialog answered"),
        (json!([]), r#"mare.ui.approve = function() error("boom") end"#, "no approval dialog answered"),
        (
            json!([]),
            r#"C.cordis:on("agent/approval-dialog", function() return "allow" end, { prepend = true })"#,
            "returned an invalid answer",
        ),
        (
            json!([]),
            r#"C.cordis:on("agent/approval-dialog", function() return { choice = "yes" } end, { prepend = true })"#,
            "returned an invalid answer",
        ),
    ];
    for (disabled, setup, why) in cases {
        local(async {
            let env = TestEnv::new().await;
            let h = parent_with(&env, "read-only", write_script("u.txt"), disabled).await;
            exec(&h, &format!("mare.ui.connected = function() return true end {setup}")).await;
            let (decision, messages): (String, String) = eval(
                &h,
                r#"local result
                   C.cordis:on("pipeline/approval-decided", function(ev) result = ev end)
                   local a = mare.subagent.start({ prompt = "write u", cwd = CWD, no_session = true })
                   local res = a:wait()
                   return result.decision, mare.json.encode(res.messages)"#,
            )
            .await;
            assert_eq!(decision, "unavailable", "{setup}");
            assert!(messages.contains(why), "{setup}: {messages}");
            assert!(!env.cwd.join("u.txt").exists(), "{setup}");
            h.shutdown().await;
        })
        .await
    }
}

/// A plugin can replace the dialog (a prepended `agent/approval-dialog` listener). What it
/// gets as `rule` is what "remember" stores, field for field; an amendment in its answer is
/// ignored — the host keeps its own snapshot.
#[tokio::test]
async fn a_replacing_dialog_shows_the_rule_that_is_stored_and_cannot_swap_it() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", touch_script("r.txt")).await;
        let shown: String = eval(
            &h,
            r#"local shown
               mare.ui.connected = function() return true end
               C.cordis:on("agent/approval-dialog", function(ev)
                   shown = mare.json.encode(ev.rule)
                   return { choice = "allow_amend",
                            amendment = { argv = { "rm" }, decision = "allow", scope = "all" } }
               end, { prepend = true })
               SPAWN({ prompt = "touch", label = "one" })
               return shown"#,
        )
        .await;
        let shown: Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(rules_file(&env)["rules"], json!([shown]), "the stored rule is the shown one");
        assert_eq!(shown["argv"], json!(["touch", "r.txt"]));
        assert!(env.cwd.join("r.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_auto_review_preserves_empty_arrays_and_null_fields_end_to_end() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.home.path().join("config/plugins")).unwrap();
        std::fs::write(
            env.home.path().join("config/plugins/shape-probe.lua"),
            r#"
            return {
                name = "shape-probe",
                inject = { "tools" },
                apply = function(ctx)
                    ctx.tools:register("shape_probe", {
                        description = "probe JSON shape",
                        side_effect = "write",
                        parameters = {
                            type = "object",
                            properties = {
                                paths = { type = "array", items = { type = "string" } },
                                mode = { default = mare.json.null },
                            },
                        },
                        execute = function(args)
                            assert(mare.fs.write(mare.cwd .. "/executed.json", mare.json.encode_strict(args)))
                            return { content = "ok" }
                        end,
                    })
                end,
            }
            "#,
        )
        .unwrap();

        let script = json!([
            { "tool_calls": [{ "name": "shape_probe", "arguments": { "paths": [], "mode": null } }] },
            { "text": "done" }
        ]);
        let mut b = env
            .host()
            .builtins(&["subagent", "agents"])
            .faux(script)
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "read-only" },
                    "approvals": { "reviewer": "fixture" }
                }
            }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "CWD", env.cwd.to_string_lossy().to_string());

        let (ok, reviewed_args, call_schema_null, action_schema_null): (bool, String, bool, bool) = eval(
            &h,
            r#"
            local A = assert(C.cordis:get("approvals"))
            local reviewed_args, call_schema_null, action_schema_null = "", false, false
            A:register_reviewer("fixture", function(action)
                reviewed_args = mare.json.encode_strict(action.call.arguments)
                call_schema_null = action.call.schema.properties.mode.default == mare.json.null
                action_schema_null = action.schema.properties.mode.default == mare.json.null
                return { decision = "allow" }
            end)
            local a = mare.subagent.start({ prompt = "probe shape", cwd = CWD, no_session = true })
            local ok = pcall(a.wait, a)
            return ok, reviewed_args, call_schema_null, action_schema_null
            "#,
        )
        .await;
        assert!(ok, "child agent should succeed");
        assert_eq!(reviewed_args, r#"{"mode":null,"paths":[]}"#);
        let executed = std::fs::read_to_string(env.cwd.join("executed.json")).unwrap();
        assert_eq!(executed, reviewed_args);
        assert!(call_schema_null, "call.schema.properties.mode.default must preserve null");
        assert!(action_schema_null, "action.schema.properties.mode.default must preserve null");
        h.shutdown().await;
    })
    .await;
}

/// After `ui.questions` and `ui.editor` time out via `mare.task.timeout`, a real sub-agent
/// approval (which blocks on `UiWaits::until_idle()` in `approvals/child.rs`) must not hang:
/// the approval dialog opens, is answered, the child's approved tool call executes, and every
/// wait ID records `start` then `end` exactly once even with yielding wait listeners.
#[tokio::test]
async fn child_approval_completes_after_questions_and_editor_timeouts() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", write_script("made.txt")).await;
        let (q_ok, e_ok, child_done, child_ok, approvals, log, ids): (
            bool,
            bool,
            bool,
            bool,
            i64,
            Vec<String>,
            Vec<i64>,
        ) = eval(
            &h,
            r#"local wait_log, wait_ids = {}, {}
               local all_logged_tx, all_logged_rx = mare.task.channel()
               local function note_log(entry)
                   table.insert(wait_log, entry)
                   if #wait_log == 6 then all_logged_tx:send(true) end
               end
               mare.on("ui/wait-started", function(ev)
                   mare.task.sleep(5)
                   note_log("start:" .. tostring(ev.wait_id))
                   table.insert(wait_ids, ev.wait_id)
               end)
               mare.on("ui/wait-completed", function(ev)
                   mare.task.sleep(5)
                   note_log("end:" .. tostring(ev.wait_id))
               end)
               mare.ui.connected = function() return true end
               mare.ui.questions = function() mare.task.sleep(5000) end
               mare.ui.editor = function() mare.task.sleep(5000) end
               local approvals = 0
               mare.ui.approve = function()
                   approvals = approvals + 1
                   return "allow"
               end

               local q_ok = mare.task.timeout(250, function()
                   mare.ui.questions({ title = "TimeoutQ" })
               end)
               local e_ok = mare.task.timeout(250, function()
                   mare.ui.editor({ title = "TimeoutE" })
               end)

               local child_done, child_ok = mare.task.timeout(30000, function()
                   return SPAWN({ prompt = "write", label = "one", permission = "read-only" })
               end)

               if #wait_log < 6 then
                   mare.task.timeout(5000, function() all_logged_rx:recv() end)
               end
               return q_ok, e_ok, child_done, child_ok == true, approvals, wait_log, wait_ids"#,
        )
        .await;
        assert!(!q_ok, "questions should have timed out");
        assert!(!e_ok, "editor should have timed out");
        assert!(child_done, "sub-agent approval must not hang in until_idle() after timeouts");
        assert!(child_ok, "child agent should succeed after approval");
        assert_eq!(approvals, 1, "expected 1 child approval dialog");
        assert!(env.cwd.join("made.txt").exists(), "approved child tool call should have run");
        assert_eq!(ids.len(), 3, "expected 3 distinct wait starts, got ids={ids:?} log={log:?}");
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_ne!(ids[0], ids[2]);
        assert_eq!(log.len(), 6, "expected 3 start + 3 end entries, got {log:?}");
        for id in &ids {
            let s_pos = log.iter().position(|e| e == &format!("start:{id}"));
            let e_pos = log.iter().position(|e| e == &format!("end:{id}"));
            assert_eq!(log.iter().filter(|e| *e == &format!("start:{id}")).count(), 1, "start count for {id}: {log:?}");
            assert_eq!(log.iter().filter(|e| *e == &format!("end:{id}")).count(), 1, "end count for {id}: {log:?}");
            assert!(s_pos < e_pos, "start must precede end for wait id {id}: {log:?}");
        }
        h.shutdown().await;
    })
    .await;
}
