use super::*;

#[tokio::test]
async fn read_only_parent_caps_type_config_and_nested_child_writes() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(plugins.join("nested-ceiling-probe.lua"), r#"
          return {name="nested-ceiling-probe", inject={"subagent", "bus"},
            contracts={probe={owner="nested-ceiling-probe", events={
              ["nested-ceiling"]={doc="probe", mode="emit", expose="both", delivery="required", ignorable=false, data={type="object", additionalProperties=true}, example={}},
            }}}, apply=function(ctx)
            if mare.mode ~= "child" or mare.config.ceiling_probe_depth ~= 1 then return end
            ctx:on("turn/started", function()
              local child = ctx.subagent:start({type="general", prompt="nested write", config={
                ceiling_probe_depth=2, plugins={permissions={preset="full-access", rules={{tool="write", decision="allow"}}}}
              }})
              local result = ctx.subagent:wait(child.subagent_id, {timeout_ms=10000})
              ctx.bus:emit("probe/nested-ceiling", {status=result.status, error=result.error})
            end)
          end}
        "#).unwrap();
        let mut builder = env.host().builtins(SUBAGENT_BUILTINS).user_plugins().faux(write_script("nested-ceiling-escape"))
            .config(json!({"plugins":{"permissions":{"enabled":true,"preset":"read-only"}}}))
            .args(json!({"no_session":true}));
        builder.opts.project_trusted = true;
        let h = builder.build().await;
        expose(&h);
        let (outer, nested): (String, String) = eval(&h, r#"
          C.cordis:get("policy"):voter("tool/approval", function() return {decision="deny", reason="read only"} end)
          local svc, spawn, handle = C.cordis:get("subagent"), mare.subagent.start
          local release = svc:register_type({name="write-plus", description="requests writing", tools={"read", "write", "subagent"}})
          mare.subagent.start = function(opts) handle=spawn(opts); return handle end
          local child = svc:start({type="write-plus", prompt="outer write", config={
            ceiling_probe_depth=1, plugins={permissions={preset="full-access", rules={{tool="write",decision="allow"}}}}
          }})
          mare.subagent.start = spawn
          local nested
          while true do
            local event=handle:next_event()
            if not event then break end
            if event.name == "probe/nested-ceiling" then nested=event.payload.status; break end
          end
          local done=svc:wait(child.subagent_id, {timeout_ms=10000})
          release()
          return done.status, nested or "missing"
        "#).await;
        assert_eq!((outer.as_str(), nested.as_str()), ("completed", "completed"));
        assert!(!env.cwd.join("nested-ceiling-escape").exists(), "type or child config widened the read-only ceiling");
        h.shutdown().await;
    }).await;
}

#[tokio::test]
async fn a_child_inherits_the_parent_preset_and_cannot_exceed_it() {
    local(async {
        let env = TestEnv::new().await;
        // parent read-only: the child asks for full-access and still needs approval to write
        let h = parent(&env, "read-only", write_script("x.txt")).await;
        let asked: usize = eval(
            &h,
            r#"ANSWER = "deny" LISTEN()
               SPAWN({ prompt = "write x", label = "writer", permission = "full-access",
                       permission_ceiling = "full-access" })
               return #ASKED"#,
        )
        .await;
        assert_eq!(asked, 1, "the read-only ceiling held: the write needed the parent's approval");
        assert!(!env.cwd.join("x.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_child_inherits_auto_instead_of_the_untrusted_default() {
    local(async {
        let env = TestEnv::new().await;
        // parent auto: a workspace write needs no approval in the child either
        let h = parent(&env, "auto", write_script("y.txt")).await;
        let asked: usize = eval(&h, r#"LISTEN() SPAWN({ prompt = "write y" }) return #ASKED"#).await;
        assert_eq!(asked, 0);
        assert!(env.cwd.join("y.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_child_may_choose_a_stricter_preset() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", write_script("z.txt")).await;
        let asked: usize = eval(
            &h,
            r#"ANSWER = "deny" LISTEN() SPAWN({ prompt = "write z", permission = "read-only" }) return #ASKED"#,
        )
        .await;
        assert_eq!(asked, 1, "read-only child: the write asks");
        assert!(!env.cwd.join("z.txt").exists());
        h.shutdown().await;
    })
    .await
}

/// A caller's own stricter `permission_ceiling` is kept: the inherited preset never overwrites
/// it, and a `permission` above that ceiling is narrowed to it.
#[tokio::test]
async fn a_caller_supplied_stricter_ceiling_is_kept() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", write_script("c.txt")).await;
        let asked: usize = eval(
            &h,
            r#"ANSWER = "deny" LISTEN()
               SPAWN({ prompt = "write c", permission_ceiling = "read-only", permission = "auto" })
               return #ASKED"#,
        )
        .await;
        assert_eq!(asked, 1, "the caller's read-only ceiling held under an auto parent: the write asked");
        assert!(!env.cwd.join("c.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_child_approval_reaches_the_parent_with_its_origin_and_allow_runs() {
    local(async {
        let env = TestEnv::new().await;
        // an auto parent and a read-only child: the write asks, and an allowed write fits inside
        // the sandbox the child inherits (the parent's, a hard ceiling)
        let h = parent(&env, "auto", write_script("a.txt")).await;
        let (label, task, agent_id, info_label, result_origin, decision): (
            String,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"local result
               C.cordis:on("pipeline/approval-decided", function(ev) result = ev end)
               LISTEN()
               SPAWN({ prompt = "write the a file\nmore detail", label = "explorer", permission = "read-only" })
               local o = ASKED[1].origin
               return o.label, o.task, o.agent_id, ASKED[1].info_origin.label,
                      result.call.origin.label, result.decision"#,
        )
        .await;
        assert_eq!(label, "explorer");
        assert_eq!(task, "write the a file", "the task is the prompt's first line");
        assert!(agent_id.starts_with("agent-"), "{agent_id}");
        assert_eq!(info_label, "explorer");
        assert_eq!(result_origin, "explorer", "the parent's approval decision names the origin");
        assert_eq!(decision, "allow");
        assert_eq!(std::fs::read_to_string(env.cwd.join("a.txt")).unwrap(), "hi", "allowed → the write ran");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn parent_approval_sandbox_constraints_are_enforced_in_the_child() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", write_script("constrained.txt")).await;
        let asked: usize = eval(
            &h,
            r#"
            local n, seen = 0, {}
            C.cordis:get("policy"):voter("tool/approval", function(call)
                if not seen[call.id] then seen[call.id] = true; n = n + 1 end
                return { decision = "allow", sandbox = "read-only" }
            end)
            SPAWN({ prompt = "write constrained" })
            return n"#,
        )
        .await;
        assert_eq!(asked, 1);
        assert!(!env.cwd.join("constrained.txt").exists(), "parent approval restriction must reach the child");
        h.shutdown().await;
    })
    .await
}

/// The parent's sandbox is the child's ceiling, but a call the parent's approval path approves
/// escalates exactly like the main agent's: it
/// runs with the policy it was approved under — the workspace write a read-only preset asks
/// about, or `sandbox_permissions = "all"` outside the sandbox. "Allow once" covers that call
/// only: the identical call asks again.
#[tokio::test]
async fn an_approved_child_call_escalates_like_the_main_agent() {
    local(async {
        let env = TestEnv::new().await;
        let write = json!({ "name": "write", "arguments": { "path": "r.txt", "content": "hi" } });
        let script = json!([
            { "tool_calls": [write] },
            { "tool_calls": [write] },
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "printf hi > b.txt",
                "sandbox_permissions": "all", "justification": "write the b file" } }] },
            { "text": "done" }
        ]);
        let h = parent(&env, "read-only", script).await;
        let (asked, messages): (usize, String) = eval(
            &h,
            r#"LISTEN()
               local a = mare.subagent.start({ prompt = "write r", cwd = CWD, no_session = true })
               local ok, res = pcall(a.wait, a)
               return #ASKED, mare.json.encode(ok and res.messages or res)"#,
        )
        .await;
        assert_eq!(asked, 3, "every escalation reaches the parent; allow once covers one call: {messages}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("r.txt")).unwrap(), "hi", "{messages}");
        assert_eq!(std::fs::read_to_string(env.cwd.join("b.txt")).unwrap(), "hi", "{messages}");
        assert!(!messages.contains("denied"), "{messages}");
        h.shutdown().await;
    })
    .await
}

/// "Allow for this session" in the parent covers the child's identical calls, as it does for the
/// main agent: the repeat runs escalated without asking again.
#[tokio::test]
async fn a_child_session_approval_keeps_the_escalation_for_identical_calls() {
    local(async {
        let env = TestEnv::new().await;
        let write = json!({ "name": "write", "arguments": { "path": "s.txt", "content": "hi" } });
        let script = json!([{ "tool_calls": [write] }, { "tool_calls": [write] }, { "text": "done" }]);
        let h = parent(&env, "read-only", script).await;
        let (asked, messages): (usize, String) = eval(
            &h,
            r#"GRANT = "allow_session" LISTEN()
               local a = mare.subagent.start({ prompt = "write s", cwd = CWD, no_session = true })
               local ok, res = pcall(a.wait, a)
               return #ASKED, mare.json.encode(ok and res.messages or res)"#,
        )
        .await;
        assert_eq!(asked, 1, "{messages}");
        assert!(env.cwd.join("s.txt").exists());
        assert!(!messages.contains("denied"), "the repeat ran escalated too: {messages}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_denied_child_approval_does_not_run() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", write_script("d.txt")).await;
        let (ok, asked): (bool, usize) =
            eval(&h, r#"ANSWER = "deny" LISTEN() local ok = SPAWN({ prompt = "write d" }) return ok, #ASKED"#).await;
        assert!(ok, "the child finishes its turn after the denial");
        assert_eq!(asked, 1);
        assert!(!env.cwd.join("d.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn the_parent_dialog_shows_the_child_and_its_answer_is_honoured() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", write_script("t.txt")).await;
        let title: String = eval(
            &h,
            r#"local title
               mare.ui.connected = function() return true end
               mare.ui.approve = function(o)
                   title = (o.wait_title or "") .. " " .. (o.approval_arguments and o.approval_arguments.path or "")
                   return "allow"
               end -- "Allow once"
               SPAWN({ prompt = "write t", label = "explorer", run_id = "7", permission = "read-only" })
               return title"#,
        )
        .await;
        assert!(title.starts_with("[explorer · run 7 · write t] Allow write?") && title.contains("t.txt"), "{title}");
        assert!(env.cwd.join("t.txt").exists());
        h.shutdown().await;
    })
    .await
}

/// Deny in the parent's `mare.ui.approve` dialog asks why via `mare.ui.input` (visible TUI
/// option labels and keys are verified in `suite::tui::dialogs`); the reason reaches the child's
/// tool result, and `nil` (Esc) on the reason prompt leaves the plain "denied by the user".
#[tokio::test]
async fn a_child_approval_denied_with_a_reason_tells_the_child_why() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "auto", write_script("w.txt")).await;
        let (items, prompt, messages): (String, String, String) = eval(
            &h,
            r#"local items, prompt
               mare.ui.connected = function() return true end
               mare.ui.approve = function(o) items = table.concat(o.choices, "|") return "deny" end
               mare.ui.input = function(o) prompt = o.title return "  use the scratch dir \n" end
               local o = { prompt = "write w", label = "explorer", permission = "read-only",
                           cwd = CWD, no_session = true }
               local a = mare.subagent.start(o)
               local res = a:wait()
               return items, prompt, mare.json.encode(res.messages)"#,
        )
        .await;
        assert_eq!(items, "allow|allow_session|allow_amend|deny|abort", "exact child approval choices");
        assert!(prompt.starts_with("Tell the sub-agent why"), "{prompt}");
        assert!(messages.contains("denied by the user: use the scratch dir. Do not retry"), "{messages}");
        assert!(!env.cwd.join("w.txt").exists());
        // skipping the reason (Esc / empty): the plain decline
        let messages: String = eval(
            &h,
            r#"mare.ui.input = function() return nil end
               local a = mare.subagent.start({ prompt = "write w", permission = "read-only", cwd = CWD, no_session = true })
               return mare.json.encode(a:wait().messages)"#,
        )
        .await;
        assert!(messages.contains("Tool call denied by the user. Do not retry"), "{messages}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn headless_with_nobody_to_answer_denies_a_child_approval() {
    local(async {
        let env = TestEnv::new().await;
        let h = parent(&env, "read-only", write_script("h.txt")).await;
        let (decision, origin): (String, String) = eval(
            &h,
            r#"local result
               C.cordis:on("pipeline/approval-decided", function(ev) result = ev end)
               SPAWN({ prompt = "write h", label = "explorer" })
               return result.decision, result.origin.label"#,
        )
        .await;
        assert_eq!(decision, "unavailable", "headless frontends count this like the parent's own");
        assert_eq!(origin, "explorer");
        assert!(!env.cwd.join("h.txt").exists());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn concurrent_child_approvals_are_queued_not_lost() {
    local(async {
        let env = TestEnv::new().await;
        // Distinct targets keep one child's write from invalidating the other's snapshot.
        let h = parent(&env, "auto", write_script("unused.txt")).await;
        let (asked, max_open, labels): (usize, usize, String) = eval(
            &h,
            r#"local open, max_open, labels = 0, 0, {}
               C.cordis:get("policy"):voter("tool/approval-host", function(call)
                   open = open + 1
                   max_open = math.max(max_open, open)
                   labels[#labels + 1] = call.origin.label
                   mare.task.sleep(100)
                   open = open - 1
                   return { decision = "allow" }
               end)
               local function spawn(label)
                   local script = {
                       { tool_calls = { { name = "write", arguments = { path = label .. ".txt", content = "hi" } } } },
                       { text = "done" },
                   }
                   return mare.subagent.start({ prompt = label, label = label, cwd = CWD, no_session = true,
                       permission = "read-only", config = { plugins = {
                           ["provider-faux"] = { script_json = mare.json.encode(script) },
                       } } })
               end
               local a, b = spawn("a"), spawn("b")
               a:wait() b:wait()
               table.sort(labels)
               return #labels, max_open, table.concat(labels, ",")"#,
        )
        .await;
        assert_eq!(asked, 2, "both requests were answered");
        assert_eq!(max_open, 1, "one at a time");
        assert_eq!(labels, "a,b");
        assert!(env.cwd.join("a.txt").exists());
        assert!(env.cwd.join("b.txt").exists());
        h.shutdown().await;
    })
    .await
}

/// Child subagents receive their narrowed permission preset and approval policy through the
/// child's `cli` config layer (visible in `/config` and `mare.config_sources()`) with no
/// `mare.args.permission` or `mare.args.approval_policy` bypass, while retaining ceiling checks.
#[tokio::test]
async fn child_receives_narrowed_preset_and_policy_in_cli_config_without_args_bypass() {
    local(async {
        let env = TestEnv::new().await;
        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("child-config-probe.lua"),
            r#"
            return {
              name = "child-config-probe",
              inject = { "bus", "permissions", "approvals", "commands" },
              contracts = {
                probe = {
                  owner = "child-config-probe",
                  events = {
                    ["child-config"] = {
                      doc = "probe",
                      mode = "emit",
                      expose = "both",
                      delivery = "required",
                      ignorable = false,
                      data = { type = "object", additionalProperties = true },
                      example = {},
                    },
                  },
                },
              },
              apply = function(ctx)
                if mare.mode ~= "child" then return end
                ctx:on("turn/started", function()
                  local overlays = {}
                  local orig_overlay, orig_connected = mare.ui.overlay, mare.ui.connected
                  mare.ui.connected = function() return false end
                  mare.ui.overlay = function(_, node)
                    overlays[#overlays + 1] = mare.json.encode(node)
                  end
                  ctx.commands:run("/config plugins.permissions.preset")
                  ctx.commands:run("/config plugins.approvals.policy")
                  mare.ui.overlay, mare.ui.connected = orig_overlay, orig_connected
                  local sources = mare.config_sources and mare.config_sources() or {}
                  local psrc = sources["plugins.permissions.preset"]
                  local asrc = sources["plugins.approvals.policy"]
                  local widen_ok, widen_err = pcall(ctx.permissions.set, ctx.permissions, "auto")
                  ctx.bus:emit("probe/child-config", {
                    arg_permission = mare.args and mare.args.permission or nil,
                    arg_approval_policy = mare.args and mare.args.approval_policy or nil,
                    cfg_preset = mare.config.plugins and mare.config.plugins.permissions and mare.config.plugins.permissions.preset,
                    cfg_policy = mare.config.plugins and mare.config.plugins.approvals and mare.config.plugins.approvals.policy,
                    src_preset = psrc and psrc.layer or nil,
                    src_policy = asrc and asrc.layer or nil,
                    live_preset = ctx.permissions:preset(),
                    live_policy = ctx.approvals:policy(),
                    slash_preset = overlays[1] or "",
                    slash_policy = overlays[2] or "",
                    widen_rejected = not widen_ok,
                    widen_err = type(widen_err) == "table" and tostring(widen_err.message or "") or tostring(widen_err or ""),
                  })
                end)
              end,
            }
            "#,
        )
        .unwrap();

        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(json!([{ "text": "done" }]))
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "read-only" },
                    "approvals": { "policy": "never" }
                }
            }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        let report_raw: String = eval(
            &h,
            r#"
            local child = mare.subagent.start({
              prompt = "inspect config",
              no_session = true,
              permission = "full-access",
              approval_policy = "ask",
              config = {
                plugins = {
                  permissions = { preset = "full-access" },
                  approvals = { policy = "ask" },
                },
              },
            })
            local seen
            while true do
              local ev = child:next_event()
              if not ev then break end
              if ev.name == "probe/child-config" then seen = ev.payload; break end
            end
            child:wait()
            assert(seen, "child config probe did not emit")
            return mare.json.encode(seen)
            "#,
        )
        .await;
        let r: Value = serde_json::from_str(&report_raw).unwrap();
        assert!(r["arg_permission"].is_null(), "child must not receive mare.args.permission: {r}");
        assert!(r["arg_approval_policy"].is_null(), "child must not receive mare.args.approval_policy: {r}");
        assert_eq!(r["cfg_preset"], "read-only", "child config must show narrowed preset: {r}");
        assert_eq!(r["cfg_policy"], "never", "child config must show narrowed approval policy: {r}");
        assert_eq!(r["src_preset"], "cli", "{r}");
        assert_eq!(r["src_policy"], "cli", "{r}");
        assert_eq!(r["live_preset"], "read-only", "{r}");
        assert_eq!(r["live_policy"], "never", "{r}");
        assert!(
            r["slash_preset"].as_str().unwrap().contains("read-only"),
            "child /config must display narrowed preset: {r}"
        );
        assert!(
            r["slash_policy"].as_str().unwrap().contains("never"),
            "child /config must display narrowed approval policy: {r}"
        );
        assert_eq!(r["widen_rejected"], true, "child ceiling must still reject widening: {r}");
        assert!(r["widen_err"].as_str().unwrap().contains("can't widen"), "{r}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn subagent_cwd_respects_parent_sandbox_write_and_deny_read_ceilings() {
    local(async {
        let env = TestEnv::new().await;
        let outside_dir = tempfile::tempdir().unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        std::fs::write(secret_dir.path().join("AGENTS.md"), "SECRET_AGENTS_PAYLOAD_XYZ\n").unwrap();
        std::fs::write(secret_dir.path().join("secret.txt"), "SECRET_FILE_PAYLOAD_XYZ\n").unwrap();

        let plugins = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("child_request_probe.lua"),
            r#"
            return {
              name = "child-request-probe",
              inject = { "bus" },
              contracts = {
                probe = {
                  owner = "child-request-probe",
                  events = {
                    ["child-req"] = {
                      doc = "probe",
                      mode = "emit",
                      expose = "both",
                      delivery = "required",
                      ignorable = false,
                      data = { type = "object", additionalProperties = true },
                      example = {},
                    },
                  },
                },
              },
              apply = function(ctx)
                if mare.mode ~= "child" then return end
                ctx:on("request/build", function(req)
                  ctx.bus:emit("probe/child-req", {
                    system = tostring(req.system or ""),
                    encoded = mare.json.encode(req),
                  })
                  return req
                end)
              end,
            }
            "#,
        )
        .unwrap();

        let script = json!([
            { "tool_calls": [{ "name": "write", "arguments": { "path": "escape.txt", "content": "escaped" } }] },
            { "text": "write-done" }
        ]);
        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(script)
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "auto" },
                    "approvals": { "policy": "never" }
                }
            }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "OUTSIDE_CWD", outside_dir.path().to_string_lossy().to_string());
        global(&h, "SECRET_CWD", secret_dir.path().to_string_lossy().to_string());

        // Narrow parent sandbox to parent workspace only (excluding temp roots) and deny reading secret_dir.
        let write_msgs: String = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current({
              level = "workspace-write",
              exclude_slash_tmp = true,
              exclude_tmpdir_env_var = true,
              deny_read = { SECRET_CWD },
              accept_partial = { "fs_deny_read" },
            })
            local child = mare.subagent.start({
              prompt = "write outside",
              cwd = OUTSIDE_CWD,
              no_session = true,
            })
            local res = child:wait()
            return mare.json.encode(res.messages or res)
            "#,
        )
        .await;
        assert!(
            !outside_dir.path().join("escape.txt").exists(),
            "subagent cwd outside parent workspace must not become writable: {write_msgs}"
        );

        // Second child points cwd at a parent deny_read directory containing AGENTS.md and secret.txt.
        let read_script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": "secret.txt" } }] },
            { "text": "read-done" }
        ]);
        let mut b2 = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(read_script)
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "auto" },
                    "approvals": { "policy": "never" }
                }
            }))
            .args(json!({ "no_session": true }));
        b2.opts.project_trusted = true;
        let h2 = b2.build().await;
        expose(&h2);
        global(&h2, "SECRET_CWD", secret_dir.path().to_string_lossy().to_string());
        let (req_dump, read_msgs): (String, String) = eval(
            &h2,
            r#"
            mare.proc.sandbox.set_current({
              level = "workspace-write",
              exclude_slash_tmp = true,
              exclude_tmpdir_env_var = true,
              deny_read = { SECRET_CWD },
              accept_partial = { "fs_deny_read" },
            })
            local child = mare.subagent.start({
              prompt = "read secret",
              cwd = SECRET_CWD,
              no_session = true,
            })
            local req_seen = ""
            while true do
              local ev = child:next_event()
              if not ev then break end
              if ev.name == "probe/child-req" and req_seen == "" then
                req_seen = tostring(ev.payload.encoded or "")
              end
            end
            local res = child:wait()
            return req_seen, mare.json.encode(res.messages or res)
            "#,
        )
        .await;
        assert!(
            !req_dump.contains("SECRET_AGENTS_PAYLOAD_XYZ"),
            "parent deny_read must block instruction file ingestion from subagent cwd: {req_dump}"
        );
        assert!(
            !read_msgs.contains("SECRET_FILE_PAYLOAD_XYZ"),
            "parent deny_read must block file reads inside subagent cwd: {read_msgs}"
        );
        h2.shutdown().await;
        h.shutdown().await;
    })
    .await
}

/// Built-in `explore` and `plan` subagents have `bash` available for read-only inspection, but run
/// inside an enforced read-only sandbox ceiling so their shell commands cannot create or modify
/// files even when the parent session is `auto`, `full-access`, or has permissions turned off.
#[tokio::test]
async fn explore_and_plan_subagents_run_bash_read_only_and_cannot_write_files() {
    local(async {
        for (parent_preset, perms_enabled) in [("auto", true), ("full-access", true), ("auto", false)] {
            for agent_type in ["explore", "plan"] {
                let env = TestEnv::new().await;
                let seed_path = env.cwd.join("seed.txt");
                std::fs::write(&seed_path, "SENTINEL_READ_ONLY_CHECK\n").unwrap();
                let new_file = format!("created-{agent_type}.txt");
                let escalated_file = format!("escalated-{agent_type}.txt");

                let script = json!([
                    {
                        "tool_calls": [
                            { "name": "bash", "arguments": { "command": "cat seed.txt" } }
                        ]
                    },
                    {
                        "tool_calls": [
                            { "name": "bash", "arguments": { "command": format!("printf hacked > {new_file} && printf bad >> seed.txt") } }
                        ]
                    },
                    {
                        "tool_calls": [
                            {
                                "name": "bash",
                                "arguments": {
                                    "command": format!("printf escalated > {escalated_file}"),
                                    "sandbox_permissions": "all",
                                    "justification": "attempt write escalation"
                                }
                            }
                        ]
                    },
                    { "text": "inspection-finished" }
                ]);

                let mut b = env
                    .host()
                    .builtins(SUBAGENT_BUILTINS)
                    .faux(script)
                    .config(json!({
                        "plugins": {
                            "permissions": { "enabled": perms_enabled, "preset": parent_preset },
                            "approvals": { "policy": "never" }
                        }
                    }))
                    .args(json!({ "no_session": true }));
                b.opts.project_trusted = true;
                let h = b.build().await;
                expose(&h);
                // On Windows' default tier (`windows-acl`), the sandbox cannot block network
                // access. To uphold `read-only`'s no-uncontrolled-network promise, every command
                // tool requires approval (`permissions/vote.lua`), so under `approvals.policy =
                // "never"` the child's bash calls are denied (and Git Bash cannot run under the
                // write-restricted token anyway). Verify the `read-only` ceiling still holds and
                // refuses the commands instead of inheriting the parent's `full-access` / disabled
                // permissions.
                let network_partial: bool = eval(
                    &h,
                    r#"
                    local caps = mare.proc.sandbox.capabilities()
                    if caps.mechanism ~= "windows-acl" then
                        return false
                    end
                    for _, c in ipairs(caps.caps or {}) do
                        if c == "net_block" or c == "net_proxy" then
                            return false
                        end
                    end
                    return true
                    "#,
                )
                .await;
                global(&h, "AGENT_TYPE", agent_type.to_string());
                let raw: String = eval(
                    &h,
                    r#"
                    local svc = C.cordis:get("subagent")
                    local child = svc:start({
                        type = AGENT_TYPE,
                        prompt = "inspect and try to write",
                        config = {
                            plugins = {
                                permissions = { enabled = false, preset = "full-access" }
                            }
                        }
                    })
                    local done = svc:wait(child.subagent_id, { timeout_ms = 15000 })
                    return mare.json.encode(done)
                    "#,
                )
                .await;
                let done: Value = serde_json::from_str(&raw).unwrap();
                assert_eq!(
                    done["status"], "completed",
                    "parent={parent_preset} (enabled={perms_enabled}) type={agent_type}: {done}"
                );
                let msgs = serde_json::to_string(&done["messages"]).unwrap();
                if network_partial {
                    assert!(
                        msgs.contains("Tool call denied"),
                        "on windows-acl without net_block, {agent_type} read-only ceiling must gate bash under approvals.policy=never (parent={parent_preset}, enabled={perms_enabled}): {msgs}"
                    );
                } else {
                    assert!(
                        msgs.contains("SENTINEL_READ_ONLY_CHECK"),
                        "bash must be callable and able to read files in {agent_type} (parent={parent_preset}, enabled={perms_enabled}): {msgs}"
                    );
                }
                assert!(
                    !env.cwd.join(&new_file).exists(),
                    "{agent_type} bash must not create files in workspace (parent={parent_preset}, enabled={perms_enabled}): {msgs}"
                );
                assert!(
                    !env.cwd.join(&escalated_file).exists(),
                    "{agent_type} bash escalation must not create files (parent={parent_preset}, enabled={perms_enabled}): {msgs}"
                );
                assert_eq!(
                    std::fs::read_to_string(&seed_path).unwrap(),
                    "SENTINEL_READ_ONLY_CHECK\n",
                    "{agent_type} bash must not modify existing files (parent={parent_preset}, enabled={perms_enabled}): {msgs}"
                );
                h.shutdown().await;
            }
        }
    })
    .await
}

/// Installs a compact child-side probe plugin that reports live permission/sandbox state on
/// `turn/started` (`probe/state`) and provides `wait_gate` / `report_state` tools for synchronized
/// parent/child permission tests.
fn install_perm_probe_plugin(env: &TestEnv, try_widen_on_start: bool) {
    let plugins = env.config_dir().join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let try_widen = if try_widen_on_start { "true" } else { "false" };
    std::fs::write(
        plugins.join("child-perm-probe.lua"),
        format!(
            r#"
            return {{
              name = "child-perm-probe",
              inject = {{ "bus", "tools", "permissions" }},
              contracts = {{
                probe = {{
                  owner = "child-perm-probe",
                  events = {{
                    state = {{
                      doc = "probe", mode = "emit", expose = "both", delivery = "required",
                      ignorable = false, data = {{ type = "object", additionalProperties = true }}, example = {{}},
                    }},
                  }},
                }},
              }},
              apply = function(ctx)
                local function snapshot(tag, widen_ok)
                  local sb = mare.proc.sandbox.current()
                  return {{
                    tag = tag,
                    preset = ctx.permissions:preset(),
                    ceiling = mare.args and mare.args.permission_ceiling or nil,
                    sandbox_nil = sb == nil,
                    sandbox_level = sb and sb.level or nil,
                    widen_ok = widen_ok,
                  }}
                end
                if mare.mode == "child" then
                  ctx:on("turn/started", function()
                    local widen_ok
                    if {try_widen} then
                      widen_ok = pcall(ctx.permissions.set, ctx.permissions, "full-access")
                    end
                    ctx.bus:emit("probe/state", snapshot("start", widen_ok))
                  end)
                end
                ctx.tools:register("wait_gate", {{
                  description = "signal readiness and wait on gate file",
                  side_effect = "read",
                  parameters = {{ type = "object", properties = {{ gate = {{ type = "string" }} }}, required = {{ "gate" }} }},
                  execute = function(a)
                    ctx.bus:emit("probe/state", snapshot("ready"))
                    for _ = 1, 1000 do
                      if mare.fs.stat(a.gate) then return {{ content = "released" }} end
                      mare.task.sleep(5)
                    end
                    return {{ content = "timed out", is_error = true }}
                  end,
                }})
                ctx.tools:register("report_state", {{
                  description = "emit current child preset and sandbox state",
                  side_effect = "read",
                  parameters = {{ type = "object", properties = {{}} }},
                  execute = function()
                    ctx.bus:emit("probe/state", snapshot("after"))
                    return {{ content = "reported" }}
                  end,
                }})
              end,
            }}
            "#
        ),
    )
    .unwrap();
}

/// Regression for parent switching to `full-access` (FULL) before spawning a `general` subagent:
/// stale pre-FULL sandbox policies must not leak into the new child's ceiling. Both direct writes
/// and repeated `request_permissions` calls succeed with zero parent prompts.
#[tokio::test]
async fn general_subagent_after_parent_switches_to_full_writes_and_repeats_request_permissions_without_prompt() {
    local(async {
        for initial_preset in ["read-only", "auto"] {
            let env = TestEnv::new().await;
            let tests_dir = env.cwd.join("tests");
            std::fs::create_dir_all(&tests_dir).unwrap();
            let outside_base = tempfile::tempdir().unwrap();
            let outside_dir = dunce::canonicalize(outside_base.path()).unwrap();
            install_perm_probe_plugin(&env, false);

            let script = json!([
                { "tool_calls": [{ "name": "write", "arguments": { "path": tests_dir.join("repo-test.txt"), "content": "ok-tests" } }] },
                { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [tests_dir] }, "scope": "session", "justification": "write repo tests" } }] },
                { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [tests_dir] }, "scope": "turn", "justification": "repeat repo tests" } }] },
                { "tool_calls": [{ "name": "write", "arguments": { "path": outside_dir.join("outside.txt"), "content": "ok-outside" } }] },
                { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [outside_dir] }, "scope": "session", "justification": "write outside dir" } }] },
                { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [outside_dir] }, "scope": "turn", "justification": "repeat outside dir" } }] },
                { "text": "all-done" }
            ]);

            let mut b = env
                .host()
                .builtins(SUBAGENT_BUILTINS)
                .user_plugins()
                .faux(script)
                .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": initial_preset } } }))
                .args(json!({ "no_session": true }));
            b.opts.project_trusted = true;
            let h = b.build().await;
            expose(&h);
            global(&h, "P", h.service_table("permissions").unwrap());
            let (asked, state_raw, done_raw): (usize, String, String) = eval(
                &h,
                r#"
                local asked = 0
                C.cordis:get("policy"):voter("tool/approval", function()
                    asked = asked + 1
                    return { decision = "deny", reason = "unexpected parent prompt under FULL" }
                end)
                assert(P:set("full-access"))
                local svc, raw_start, handle = C.cordis:get("subagent"), mare.subagent.start, nil
                mare.subagent.start = function(opts) handle = raw_start(opts); return handle end
                local info = svc:start({ type = "general", prompt = "write repo tests and outside dir" })
                mare.subagent.start = raw_start
                local state
                while handle do
                    local ev = handle:next_event()
                    if not ev then break end
                    if ev.name == "probe/state" and ev.payload.tag == "start" then state = ev.payload; break end
                end
                local done = svc:wait(info.subagent_id, { timeout_ms = 15000 })
                return asked, mare.json.encode(state or {}), mare.json.encode(done)
                "#,
            )
            .await;

            let state: Value = serde_json::from_str(&state_raw).unwrap();
            let done: Value = serde_json::from_str(&done_raw).unwrap();
            assert_eq!(done["status"], "completed", "initial_preset={initial_preset}: {done}");
            assert_eq!(asked, 0, "initial_preset={initial_preset}: state={state} done={done}");
            assert_eq!(state["preset"], "full-access", "{state}");
            assert_eq!(state["ceiling"], "full-access", "{state}");
            assert_eq!(state["sandbox_nil"], true, "{state}");
            assert_eq!(std::fs::read_to_string(tests_dir.join("repo-test.txt")).unwrap(), "ok-tests");
            assert_eq!(std::fs::read_to_string(outside_dir.join("outside.txt")).unwrap(), "ok-outside");
            h.shutdown().await;
        }
    })
    .await
}

/// Existing unpinned `general` subagent started while parent is `read-only` → parent switches to
/// `full-access` (`FULL`) → continuing the subagent via `steer` resynchronizes its preset and
/// sandbox ceiling to `full-access`, so writes and repeated `request_permissions` succeed with
/// zero parent prompts.
#[tokio::test]
async fn existing_general_subagent_steered_after_parent_switches_to_full_resyncs_and_writes_without_prompt() {
    local(async {
        let env = TestEnv::new().await;
        let tests_dir = env.cwd.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let outside_base = tempfile::tempdir().unwrap();
        let outside_dir = dunce::canonicalize(outside_base.path()).unwrap();
        let gate_dir = tempfile::tempdir().unwrap();
        let gate_file = gate_dir.path().join("parent-switched-and-steered");
        install_perm_probe_plugin(&env, false);

        let child_script = json!([
            { "tool_calls": [{ "name": "wait_gate", "arguments": { "gate": gate_file } }] },
            { "tool_calls": [{ "name": "report_state", "arguments": {} }] },
            { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [tests_dir] }, "scope": "session", "justification": "write repo tests after steer resync" } }] },
            { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [tests_dir] }, "scope": "turn", "justification": "repeat permission request after steer resync" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": tests_dir.join("steered.txt"), "content": "steered-ok" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": outside_dir.join("steered-outside.txt"), "content": "outside-ok" } }] },
            { "text": "steered-done" }
        ]);

        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(child_script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        global(&h, "GATE_FILE", gate_file.to_string_lossy().to_string());

        let (asked, ready_preset, resynced_raw): (usize, String, String) = eval(
            &h,
            r#"
            local asked = 0
            C.cordis:get("policy"):voter("tool/approval", function()
                asked = asked + 1
                return { decision = "deny", reason = "unexpected parent prompt after steer resync" }
            end)
            local svc, raw_start, handle = C.cordis:get("subagent"), mare.subagent.start, nil
            mare.subagent.start = function(opts) handle = raw_start(opts); return handle end
            local running = svc:start({ type = "general", prompt = "start while read-only" })
            mare.subagent.start = raw_start

            local ready_preset = ""
            while handle do
                local ev = handle:next_event()
                if not ev then break end
                if ev.name == "probe/state" and ev.payload.tag == "ready" then
                    ready_preset = tostring(ev.payload.preset or ""); break
                end
            end

            assert(P:set("full-access"))
            svc:steer(running.subagent_id, "continue and write the test files")
            local saw_perm_changed = false
            while handle do
                local ev = handle:next_event()
                if not ev then break end
                if ev.name == "permission/changed" and ev.payload and ev.payload.preset == "full-access" then
                    saw_perm_changed = true; break
                end
            end
            assert(saw_perm_changed, "expected steered child to emit permission/changed=full-access")
            assert(mare.fs.write(GATE_FILE, "go"))

            local resynced = {}
            while handle do
                local ev = handle:next_event()
                if not ev then break end
                if ev.name == "probe/state" and ev.payload.tag == "after" then resynced = ev.payload; break end
            end
            local done = svc:wait(running.subagent_id, { timeout_ms = 15000 })
            assert(done.status == "completed", mare.json.encode(done))
            return asked, ready_preset, mare.json.encode(resynced)
            "#,
        )
        .await;

        let resynced: Value = serde_json::from_str(&resynced_raw).unwrap();
        assert_eq!(ready_preset, "read-only");
        assert_eq!(resynced["preset"], "full-access", "{resynced}");
        assert_eq!(resynced["ceiling"], "full-access", "{resynced}");
        assert_eq!(resynced["sandbox_nil"], true, "{resynced}");
        assert_eq!(asked, 0, "steered general subagent after parent FULL must not prompt");
        assert_eq!(std::fs::read_to_string(tests_dir.join("steered.txt")).unwrap(), "steered-ok");
        assert_eq!(std::fs::read_to_string(outside_dir.join("steered-outside.txt")).unwrap(), "outside-ok");
        h.shutdown().await;
    })
    .await
}

/// Without being steered again, an already-running unpinned `general` subagent whose approval
/// reaches a parent that switched to `FULL` is auto-allowed by the parent's current preset with
/// zero UI dialogs for workspace writes and `request_permissions`, while dangerous commands
/// (`rm -f`) still trigger the parent approval dialog.
#[tokio::test]
async fn in_flight_general_subagent_approval_auto_allows_under_parent_full_while_dangerous_still_prompts() {
    local(async {
        let env = TestEnv::new().await;
        let tests_dir = env.cwd.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        let outside_base = tempfile::tempdir().unwrap();
        let outside_dir = dunce::canonicalize(outside_base.path()).unwrap();
        let gate_dir = tempfile::tempdir().unwrap();
        let gate_file = gate_dir.path().join("parent-full-inflight");
        let victim = tests_dir.join("keep-me.txt");
        std::fs::write(&victim, "precious").unwrap();
        install_perm_probe_plugin(&env, false);

        let child_script = json!([
            { "tool_calls": [{ "name": "wait_gate", "arguments": { "gate": gate_file } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": tests_dir.join("inflight.txt"), "content": "inflight-ok" } }] },
            { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [outside_dir] }, "scope": "session", "justification": "write outside workspace from in-flight subagent" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": outside_dir.join("inflight-outside.txt"), "content": "inflight-outside-ok" } }] },
            { "tool_calls": [{ "name": "bash", "arguments": { "command": format!("rm -f {}", victim.display()) } }] },
            { "text": "inflight-done" }
        ]);

        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(child_script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "read-only" } } }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        global(&h, "GATE_FILE", gate_file.to_string_lossy().to_string());

        let (dialogs_raw, ready_preset): (String, String) = eval(
            &h,
            r#"
            local dialogs = {}
            mare.ui.connected = function() return true end
            mare.ui.approve = function(o) dialogs[#dialogs + 1] = tostring(o.wait_title or ""); return "deny" end
            mare.ui.input = function() return nil end

            local svc, raw_start, handle = C.cordis:get("subagent"), mare.subagent.start, nil
            mare.subagent.start = function(opts) handle = raw_start(opts); return handle end
            local running = svc:start({ type = "general", prompt = "start while read-only" })
            mare.subagent.start = raw_start

            local ready_preset = ""
            while handle do
                local ev = handle:next_event()
                if not ev then break end
                if ev.name == "probe/state" and ev.payload.tag == "ready" then
                    ready_preset = tostring(ev.payload.preset or ""); break
                end
            end

            assert(P:set("full-access"))
            assert(mare.fs.write(GATE_FILE, "go"))
            local done = svc:wait(running.subagent_id, { timeout_ms = 15000 })
            assert(done.status == "completed", mare.json.encode(done))
            return mare.json.encode(dialogs), ready_preset
            "#,
        )
        .await;

        let dialogs: Vec<String> = serde_json::from_str(&dialogs_raw).unwrap();
        assert_eq!(ready_preset, "read-only");
        assert_eq!(std::fs::read_to_string(tests_dir.join("inflight.txt")).unwrap(), "inflight-ok");
        assert_eq!(std::fs::read_to_string(outside_dir.join("inflight-outside.txt")).unwrap(), "inflight-outside-ok");
        assert_eq!(dialogs.len(), 1, "only dangerous rm -f should prompt under FULL: {dialogs:?}");
        assert!(dialogs[0].contains("Allow bash?"), "expected dangerous bash dialog: {dialogs:?}");
        assert!(victim.exists(), "denied dangerous rm -f must not delete file");
        h.shutdown().await;
    })
    .await
}

/// Explicit read-only subagents (`permission = "read-only"`, `explore`, `plan`) enforce a hard
/// read-only ceiling (even if child config tries to disable `permissions` or widen itself, and
/// even after the parent switches to `FULL` and steers the child).
#[tokio::test]
async fn explicit_read_only_subagents_remain_restricted_after_parent_switches_to_full() {
    local(async {
        let env = TestEnv::new().await;
        let tests_dir = env.cwd.join("tests");
        std::fs::create_dir_all(&tests_dir).unwrap();
        install_perm_probe_plugin(&env, true);

        let ro_script = json!([
            { "tool_calls": [{ "name": "request_permissions", "arguments": { "additional_permissions": { "write": [tests_dir] }, "scope": "session", "justification": "attempt write under explicit read-only" } }] },
            { "tool_calls": [{ "name": "write", "arguments": { "path": tests_dir.join("ro-general.txt"), "content": "nope" } }] },
            { "tool_calls": [{ "name": "bash", "arguments": { "command": format!("printf nope > {}", tests_dir.join("ro-bash.txt").display()) } }] },
            { "text": "ro-done" }
        ]);
        let explore_script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": format!("printf nope > {}", tests_dir.join("ro-explore.txt").display()) } }] },
            { "text": "explore-done" }
        ]);

        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .user_plugins()
            .faux(ro_script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "auto" } } }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        global(&h, "EXPLORE_SCRIPT", serde_json::to_string(&explore_script).unwrap());

        let (asked, probe_raw, explore_status, plan_status): (usize, String, String, String) = eval(
            &h,
            r#"
            local asked = 0
            local ro
            C.cordis:get("policy"):voter("tool/approval", function()
                asked = asked + 1
                if asked == 1 and ro then ro:steer("keep going under parent full-access") end
                return { decision = "deny", reason = "explicit read-only" }
            end)
            assert(P:set("full-access"))
            ro = mare.subagent.start({
                permission = "read-only",
                no_session = true,
                prompt = "try to write under explicit read-only",
                config = { plugins = { permissions = { enabled = false, preset = "full-access" } } },
            })
            local seen = {}
            while true do
                local ev = ro:next_event()
                if not ev then break end
                if ev.name == "probe/state" and ev.payload.tag == "start" then seen = ev.payload; break end
            end
            ro:wait()

            local svc = C.cordis:get("subagent")
            local function run_ro(kind)
                local c = svc:start({
                    type = kind,
                    prompt = "try to write under " .. kind,
                    config = { plugins = { ["provider-faux"] = { script_json = EXPLORE_SCRIPT } } },
                })
                return tostring(svc:wait(c.subagent_id, { timeout_ms = 15000 }).status)
            end
            return asked, mare.json.encode(seen), run_ro("explore"), run_ro("plan")
            "#,
        )
        .await;

        let seen: Value = serde_json::from_str(&probe_raw).unwrap();
        assert_eq!(seen["ceiling"], "read-only", "{seen}");
        assert_eq!(seen["preset"], "read-only", "{seen}");
        assert_eq!(seen["sandbox_level"], "read-only", "{seen}");
        assert_eq!(seen["widen_ok"], false, "explicit read-only child must not widen itself: {seen}");
        assert_eq!(explore_status, "completed");
        assert_eq!(plan_status, "completed");
        assert!(asked >= 2, "explicit read-only child must prompt on request_permissions and write: asked={asked}");
        assert!(!tests_dir.join("ro-general.txt").exists());
        assert!(!tests_dir.join("ro-bash.txt").exists());
        assert!(!tests_dir.join("ro-explore.txt").exists());
        h.shutdown().await;
    })
    .await
}

/// Deterministic concurrency barrier test: if an ancestor tightens the parent VM's sandbox (e.g.
/// adding `deny_read`) AFTER an in-flight tool call has frozen its `exec_ctx` and BEFORE that tool
/// reserves/spawns a descendant, `apply_inheritance` must intersect the frozen tool policy with the
/// parent VM's live policy/ceiling so the new child inherits the tightening and cannot read the
/// newly denied path.
#[tokio::test]
async fn ancestor_tightening_after_tool_freeze_before_child_reserve_reaches_new_child() {
    local(async {
        let env = TestEnv::new().await;
        let secret_dir = tempfile::tempdir().unwrap();
        let secret_file = secret_dir.path().join("secret.txt");
        std::fs::write(&secret_file, "TOP_SECRET_BARRIER_PAYLOAD\n").unwrap();

        let child_script = json!([
            { "tool_calls": [{ "name": "read", "arguments": { "path": secret_file } }] },
            { "text": "done" }
        ]);
        let mut b = env
            .host()
            .builtins(SUBAGENT_BUILTINS)
            .faux(child_script)
            .config(json!({
                "plugins": {
                    "permissions": { "enabled": true, "preset": "auto" },
                    "approvals": { "policy": "never" }
                }
            }))
            .args(json!({ "no_session": true }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);

        // Install a Lua callback that synchronizes via std::sync::Barrier with a background thread
        // tightening the parent VM's PolicyHandle while the tool's exec_ctx is already frozen.
        let handle = mare_binding::ossandbox::policy_handle(h.lua()).expect("policy_handle installed");
        let deny_spec_val: mlua::Value = h
            .lua()
            .load(format!(
                r#"return {{ level = "workspace-write", network = true, deny_read = {{ {:?} }}, accept_partial = {{ "fs_protect", "fs_deny_read" }} }}"#,
                secret_dir.path().to_string_lossy()
            ))
            .eval()
            .unwrap();
        let tightened_policy = mare_binding::ossandbox::Policy::from_lua(h.lua(), &deny_spec_val, &env.cwd)
            .unwrap()
            .unwrap();

        let at_freeze = std::sync::Arc::new(std::sync::Barrier::new(2));
        let after_tighten = std::sync::Arc::new(std::sync::Barrier::new(2));
        let b1 = at_freeze.clone();
        let b2 = after_tighten.clone();
        let tightener = std::thread::spawn(move || {
            b1.wait();
            assert!(handle.tighten_ceiling(&tightened_policy));
            b2.wait();
        });

        let wait_tighten = h
            .lua()
            .create_function(move |_, ()| {
                at_freeze.wait();
                after_tighten.wait();
                Ok(())
            })
            .unwrap();
        h.lua().globals().set("WAIT_ANCESTOR_TIGHTEN", wait_tighten).unwrap();

        let msgs: String = eval(
            &h,
            r#"
            -- Enter a frozen tool execution context under the pre-tightening workspace-write policy.
            local res = mare.exec.run({ cwd = mare.cwd }, function()
                -- Ancestor tightens parent VM's PolicyHandle after this exec_ctx is frozen and
                -- BEFORE the child is reserved.
                WAIT_ANCESTOR_TIGHTEN()
                local child = mare.subagent.start({
                    prompt = "read secret",
                    no_session = true,
                })
                return child:wait()
            end)
            return mare.json.encode(res.messages or res)
            "#,
        )
        .await;
        tightener.join().unwrap();

        assert!(
            !msgs.contains("TOP_SECRET_BARRIER_PAYLOAD"),
            "child spawned from frozen tool context after ancestor deny_read tightening must not read secret: {msgs}"
        );
        h.shutdown().await;
    })
    .await
}
