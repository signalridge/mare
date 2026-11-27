//! Shell commands: dangerous ones, network control tiers and denied writes.

use super::*;
use crate::suite::sandbox_expect::require_backend;

#[tokio::test]
async fn targeted_hint_only_with_sandbox_evidence() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        let (with, without, win, uncertain): (String, String, String, String) = eval(
            &h,
            r#"
            local call = { name = "bash", arguments = { command = "touch /etc/x" } }
            local a = C.cordis:get("bus"):waterfall("tool/after", { result = { content = "touch: /etc/x: Operation not permitted", is_error = true,
                details = { sandbox_denials = { { sandbox_denied = "write", path = "/etc/x" } } } }, call = call, operation = {} }).result
            local b = C.cordis:get("bus"):waterfall("tool/after", { result = { content = "make: *** [all] Error 2", is_error = true }, call = call, operation = {} }).result
            local w = C.cordis:get("bus"):waterfall("tool/after", { result = { content = "Access to the path 'C:\\Users\\me\\x.txt' is denied.", is_error = true,
                details = { sandbox_denials = { { sandbox_denied = "write", path = "C:\\Users\\me\\x.txt" } } } }, call = call, operation = {} }).result
            local u = C.cordis:get("bus"):waterfall("tool/after", { result = { content = "Access is denied.", is_error = true }, call = call, operation = {} }).result
            return a.content, b.content, w.content, u.content
            "#,
        )
        .await;
        // the hint names what was refused and the narrowest scope to ask for
        assert!(with.contains("[sandbox: write to /etc/x denied under"), "{with}");
        assert!(with.contains(r#"sandbox_permissions = "write:/etc""#), "{with}");
        assert!(with.contains("check the actual state"), "{with}");
        assert!(!without.contains("sandbox"), "{without}");
        assert!(win.contains(r"write to C:\Users\me\x.txt denied"), "{win}");
        assert!(win.contains(r#"sandbox_permissions = "write:C:\\Users\\me""#), "{win}");
        // Windows' own wording without a path: only the uncertain note
        assert!(uncertain.contains("the cause is uncertain"), "{uncertain}");
        h.shutdown().await;
    })
    .await
}

/// The bash tool's persistent shell (the default) reports a sandbox refusal like a one-shot run:
/// the result names the denied path.
#[tokio::test]
async fn a_denied_write_in_the_persistent_shell_names_the_path() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        if !require_backend(
            &h,
            "a_denied_write_in_the_persistent_shell_names_the_path",
            mare_binding::ossandbox::expect::ANY,
        )
        .await
            || cfg!(windows)
        {
            return h.shutdown().await;
        }
        let outside = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(format!(".mare-sbx-hint-{}", std::process::id()));
        global(&h, "OUTSIDE", outside.to_string_lossy().to_string());
        let content: String = eval(
            &h,
            r#"assert(P:set("auto"))
               local call = { id = "1", name = "bash", arguments = { command = "touch '" .. OUTSIDE .. "'" } }
               local r = T:execute(call, {
                 approve = function() return { decision = "allow", source = "user" } end,
               })
               -- what the agent loop does with every result
               r = (C.cordis:get("bus"):waterfall("tool/after", { result = r, call = call, operation = {} }) or {}).result or r
               return tostring(r.content)"#,
        )
        .await;
        let leaked = outside.exists();
        let _ = std::fs::remove_file(&outside);
        assert!(!leaked, "the write escaped the sandbox: {content}");
        assert!(content.contains(&format!("[sandbox: write to {} denied under", outside.display())), "{content}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_tier_without_network_control_runs_partial_unless_strict_is_configured() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        // the Windows default tier's capability set: writes (and sockets) only, no network control
        env.set_var("MARE_SANDBOX_CAPS", "fs_write,fs_protect,unix_socket");
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        if !require_backend(
            &h,
            "a_tier_without_network_control_runs_partial_unless_strict_is_configured",
            mare_binding::ossandbox::expect::ANY,
        )
        .await
        {
            return h.shutdown().await;
        }
        exec(&h, VOTE).await;
        let (code, enforcement, label): (i64, String, String) = eval(
            &h,
            r#"local label
               C.cordis:on("permission/changed", function(ev) label = ev.label end)
               P:set("read-only")
               local r = mare.proc.run(TRUE({ sandbox = "current" }))
               return r.code, r.sandbox.enforcement, tostring(label)"#,
        )
        .await;
        assert_eq!((code, enforcement.as_str(), label.as_str()), (0, "partial", "PARTIAL"));
        h.shutdown().await;
        // an explicit empty accept_partial is strict: refused, not silently weaker
        let h = host(&env, true, json!({ "preset": "read-only", "accept_partial": [] }), json!({ "no_session": true }))
            .await;
        exec(&h, VOTE).await;
        let (refused, label): (String, String) = eval(
            &h,
            r#"local label
               C.cordis:on("permission/changed", function(ev) label = ev.label end)
               P:set("read-only")
               local _, e = pcall(mare.proc.run, TRUE({ sandbox = "current" }))
               return tostring(e), tostring(label)"#,
        )
        .await;
        assert!(refused.contains("SANDBOX_INSUFFICIENT") && refused.contains("net_block"), "{refused}");
        assert_eq!(label, "READ");
        h.shutdown().await;
    })
    .await
}

/// Accepting the protected-path gap is visible even when network access is unrestricted.
#[tokio::test]
async fn accepted_protected_path_gap_is_badged_partial() {
    local(async {
        let mut env = TestEnv::new().await;
        std::fs::create_dir(env.cwd.join(".git")).unwrap();
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let h = host(
            &env,
            true,
            json!({ "network": true, "accept_partial": ["fs_protect"] }),
            json!({ "no_session": true }),
        )
        .await;
        let (missing, label): (bool, String) = eval(
            &h,
            r#"
            local label
            C.cordis:on("permission/changed", function(ev) label = ev.label end)
            P:set("auto")
            local cur, missing = mare.proc.sandbox.current(), false
            for _, name in ipairs(cur.unavailable or {}) do if name == "fs_protect" then missing = true end end
            return missing, tostring(label)"#,
        )
        .await;
        if missing {
            assert_eq!(label, "PARTIAL");
        } else {
            assert_eq!(label, "AUTO");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn read_only_without_network_control_asks_before_every_shell_command() {
    local(async {
        let mut env = TestEnv::new().await;
        // The default Windows tier implicitly accepts the network gap; keep this assertion
        // separate from the metadata-gap fixture needed for the auto preset below.
        env.set_var("MARE_SANDBOX_CAPS", "fs_write,fs_protect,unix_socket");
        let h = host(&env, true, json!({ "preset": "read-only" }), json!({ "no_session": true })).await;
        if !require_backend(
            &h,
            "read_only_without_network_control_asks_before_every_shell_command",
            mare_binding::ossandbox::expect::ANY,
        )
        .await
        {
            return h.shutdown().await;
        }
        let (ro, reason, implicit_network): (String, String, bool) = eval(
            &h,
            &format!(
                r#"{VOTE}
                P:set("read-only")
                local current = mare.proc.sandbox.current()
                local accepted = {{}}
                for _, cap in ipairs(current.accept_partial or {{}}) do accepted[cap] = true end
                local v = VOTE(BASH("ls"), {{ side_effect = "read" }})
                return v.decision, tostring(v.reason), accepted.net_block == true and accepted.net_proxy == true"#
            ),
        )
        .await;
        assert_eq!(ro, "ask");
        assert!(reason.contains("network is not controlled"), "{reason}");
        assert!(implicit_network, "the default tier must accept the network gap implicitly");
        h.shutdown().await;

        let h = host(
            &env,
            true,
            json!({ "preset": "auto", "accept_partial": ["fs_protect", "network"] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let auto: String = eval(&h, r#"return VOTE(BASH("ls"), { side_effect = "read" }).decision"#).await;
        assert_eq!(auto, "allow", "auto permits network, and this fixture accepts the separate metadata gap");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn dangerous_commands_ask_in_every_preset() {
    local(async {
        let env = TestEnv::new().await;
        let opts = json!({
            "preset": "full-access",
            "rules": [{ "argv": ["rm", "-rf", "build"], "decision": "allow" }],
            "dangerous": { "rules": [["git", "push", "--force"]] },
        });
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (rm, cached, exact, git, plain): (String, String, String, String, String) = eval(
            &h,
            r#"
            local c = BASH("cd /tmp && rm -rf dist")
            local rm = VOTE(c)
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = c, decision = "allow_session" })
            local cached = VOTE(BASH("cd /tmp && rm -rf dist")).decision
            local exact = VOTE(BASH("rm -rf build")).decision
            local git = VOTE(BASH("git push --force origin main")).decision
            local plain = VOTE(BASH("rm -r dist")).decision
            return rm.decision .. "|" .. tostring(rm.reason), cached, exact, git, plain
            "#,
        )
        .await;
        assert!(rm.starts_with("ask|") && rm.contains("forced_rm"), "{rm}");
        assert_eq!(cached, "ask", "the session cache never passes a dangerous command");
        assert_eq!(exact, "allow", "an allow rule naming the exact command passes it");
        assert_eq!(git, "ask", "an added rule");
        assert_eq!(plain, "allow", "not dangerous: full-access allows");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn dangerous_builtins_can_be_turned_off() {
    local(async {
        let env = TestEnv::new().await;
        let opts = json!({ "preset": "full-access", "dangerous": { "disable": ["forced_rm"] } });
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let d: String = eval(&h, r#"return VOTE(BASH("rm -rf dist")).decision"#).await;
        assert_eq!(d, "allow");
        h.shutdown().await;
    })
    .await
}

/// Headless with nobody to answer: a dangerous command is denied even with --yolo.
#[tokio::test]
async fn headless_denies_dangerous_commands_even_in_full_access() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join("dist")).unwrap();
        let script = json!([
            { "tool_calls": [{ "name": "bash", "arguments": { "command": "rm -rf dist" } }] },
            { "text": "done" }
        ]);
        let b = env
            .host()
            .faux(script)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "full-access" } } }))
            .args(json!({ "no_session": true }))
            .project_trusted(true);
        let h = b.build().await;
        expose(&h);
        h.prompt("clean up", json!({})).await.unwrap();
        let results = tool_results(&h).await;
        assert!(results.contains("denied"), "{results}");
        assert!(env.cwd.join("dist").exists(), "the directory survived");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn network_partial_reuses_only_explicit_full_access_approvals() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_write,fs_protect,unix_socket");
        let h = host(
            &env,
            true,
            json!({ "preset": "read-only", "rules": [
                { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
                { "argv": ["echo", "generic"], "decision": "allow" }
            ] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (first, cached, wider, explicit_rule, unscoped_rule, sandbox): (
            String,
            String,
            String,
            String,
            String,
            String,
        ) = eval(
            &h,
            r#"
            local call = BASH("pwd", "all", "inspect without sandbox")
            local first = VOTE(call)
            C.cordis:get("bus"):emit("pipeline/approval-decided", { call = call, decision = "allow_session" })
            local cached = VOTE(BASH("pwd", "all", "inspect without sandbox"))
            local wider = VOTE(BASH("pwd -P", "all", "inspect without sandbox"))
            local rule = VOTE(BASH("git status", "all", "inspect without sandbox"))
            local generic = VOTE(BASH("echo generic", "all", "inspect without sandbox"))
            return first.decision, cached.decision, wider.decision, rule.decision, generic.decision,
                tostring(rule.sandbox_grant)
            "#,
        )
        .await;
        assert_eq!(first, "ask", "the initial unrestricted request still needs approval");
        assert_eq!(cached, "allow", "only the exact approved unrestricted call is reused");
        assert_eq!(wider, "ask", "a different command still needs approval");
        assert_eq!(explicit_rule, "allow", "a rule explicitly granting all is reusable");
        assert_eq!(sandbox, "danger-full-access");
        assert_eq!(unscoped_rule, "ask", "an ordinary allow rule cannot grant unrestricted access");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn read_only_without_network_control_is_not_bypassed_by_rules_session_cache_or_command_tools() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        env.set_var("MARE_SANDBOX_CAPS", "fs_write,fs_protect,unix_socket");
        let opts = json!({
            "preset": "read-only",
            "rules": [
                { "argv": ["ls"], "decision": "allow" },
                { "tool": "cmd_probe", "decision": "allow" },
            ],
        });
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        let mech: String = eval(&h, "return mare.proc.sandbox.capabilities().mechanism").await;
        assert_ne!(mech, "none", "DF15 regression test requires a non-none sandbox mechanism");
        let (
            rule_prompts,
            rule_reason,
            rule_err,
            sess_prompts,
            sess_out,
            cmd_prompts,
            cmd_info_flag,
            cmd_reason,
            cmd_out,
            cmd_denied_err,
        ): (i64, String, bool, i64, String, i64, bool, String, String, bool) = eval(
            &h,
            &format!(
                r#"{VOTE}
                P:set("read-only")
                T:command({{
                    name = "cmd_probe",
                    description = "probe command tool",
                    readonly = true,
                    command = mare.os == "windows"
                        and {{ "cmd.exe", "/d", "/c", "echo cmd-probe-ok" }}
                        or {{ "echo", "cmd-probe-ok" }},
                }})

                local rule_prompts, rule_reason = 0, ""
                local rule_res = T:execute(BASH("ls"), {{
                    approve = function(_, _, verdict)
                        rule_prompts = rule_prompts + 1
                        rule_reason = tostring(verdict and verdict.reason or "")
                        return {{ decision = "deny", reason = "blocked" }}
                    end,
                }})

                local sess_prompts = 0
                local function sess_approve()
                    sess_prompts = sess_prompts + 1
                    return {{ decision = "allow_session", source = "user" }}
                end
                T:execute(BASH("echo hi"), {{ approve = sess_approve }})
                local sess_res = T:execute(BASH("echo hi"), {{ approve = sess_approve }})

                local cmd_prompts, cmd_info_flag, cmd_reason = 0, false, ""
                local cmd_call = {{ id = "cmd-1", name = "cmd_probe", arguments = {{}} }}
                local cmd_res = T:execute(cmd_call, {{
                    approve = function(_, info, verdict)
                        cmd_prompts = cmd_prompts + 1
                        cmd_info_flag = info and info.command_tool == true
                        cmd_reason = tostring(verdict and verdict.reason or "")
                        return {{ decision = "allow_session", source = "user" }}
                    end,
                }})
                local cmd_call_2 = {{ id = "cmd-2", name = "cmd_probe", arguments = {{}} }}
                local cmd_denied = T:execute(cmd_call_2, {{
                    approve = function()
                        cmd_prompts = cmd_prompts + 1
                        return {{ decision = "deny", reason = "blocked" }}
                    end,
                }})

                return rule_prompts, rule_reason, rule_res.is_error == true,
                    sess_prompts, tostring(sess_res.content),
                    cmd_prompts, cmd_info_flag, cmd_reason, tostring(cmd_res.content), cmd_denied.is_error == true"#
            ),
        )
        .await;
        assert_eq!(rule_prompts, 1, "allow rule must not bypass read-only network-partial mitigation");
        assert!(rule_reason.contains("network is not controlled"), "{rule_reason}");
        assert!(rule_err, "denied rule call must not run");
        assert_eq!(sess_prompts, 2, "session approval must not bypass read-only network-partial mitigation");
        assert!(sess_out.contains("hi"), "approved bash command should execute: {sess_out}");
        assert_eq!(cmd_prompts, 2, "command tool must not bypass read-only network-partial mitigation");
        assert!(cmd_info_flag, "real command tool must propagate command_tool = true");
        assert!(cmd_reason.contains("network is not controlled"), "{cmd_reason}");
        assert!(cmd_out.contains("cmd-probe-ok"), "real command tool should execute when approved: {cmd_out}");
        assert!(cmd_denied_err, "denied command tool call must not run");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn bash_erofs_populates_details_sandbox_denials() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        if !require_backend(&h, "bash_erofs_populates_details_sandbox_denials", mare_binding::ossandbox::expect::ANY).await || cfg!(windows) { return h.shutdown().await; }
        let outside = std::path::PathBuf::from(std::env::var("HOME").unwrap())
            .join(format!(".mare-sbx-erofs-{}", std::process::id()));
        global(&h, "OUTSIDE", outside.to_string_lossy().to_string());
        let (pr_denied, r1_json, r2_json, r3_json): (String, String, String, String) = eval(
            &h,
            r#"assert(P:set("auto"))
               local pr = mare.proc.run({ cmd = "sh", args = { "-c", "echo x > '" .. OUTSIDE .. "'" }, sandbox = "workspace-write" })
               local approve = function() return { decision = "allow", source = "user" } end
               local r1 = T:execute({ id = "1", name = "bash", arguments = { command = "echo x > '" .. OUTSIDE .. "'" } }, { approve = approve })
               local r2 = T:execute({ id = "2", name = "bash", arguments = { command = "echo x > '" .. OUTSIDE .. "'", workdir = mare.cwd } }, { approve = approve })
               assert(P:set("read-only"))
               local r3 = T:execute({ id = "3", name = "bash", arguments = { command = "echo x > rel.txt" } }, { approve = approve })
               return tostring(pr.sandbox_denied), mare.json.encode(r1.details), mare.json.encode(r2.details), mare.json.encode(r3.details)"#,
        )
        .await;
        let _ = std::fs::remove_file(&outside);
        assert_eq!(pr_denied, "write");
        let d1: serde_json::Value = serde_json::from_str(&r1_json).unwrap();
        let d2: serde_json::Value = serde_json::from_str(&r2_json).unwrap();
        let d3: serde_json::Value = serde_json::from_str(&r3_json).unwrap();
        assert_eq!(d1["sandbox_denials"][0]["sandbox_denied"], "write", "r1={r1_json}");
        assert_eq!(d2["sandbox_denials"][0]["sandbox_denied"], "write", "r2={r2_json}");
        assert_eq!(d3["sandbox_denials"][0]["sandbox_denied"], "write", "r3={r3_json}");
        assert!(
            d3["sandbox_denials"][0]["path"].as_str().is_some_and(|p| p.ends_with("rel.txt")),
            "r3={r3_json}"
        );
        h.shutdown().await;
    })
    .await
}

/// A replacement shell tool that declares `sandbox_permissions` but not `justification` gets the
/// missing one: the vote reads the two as a pair (permissions/vote.lua), so each is added on its
/// own and only a definition carrying both is left alone.
#[tokio::test]
async fn a_shell_tool_declaring_one_escalation_parameter_gets_the_other() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "auto" }), json!({ "no_session": true })).await;
        let (own, justification): (String, bool) = eval(
            &h,
            r#"
            T:register("bash", { description = "replacement", side_effect = "external", command_tool = true,
                command = { shell = "bash", arg = "command" },
                parameters = { type = "object", properties = { command = { type = "string" },
                    sandbox_permissions = { type = "string", description = "the replacement's own" } } },
                execute = function() return { content = "" } end })
            local kernel = C.cordis:get("kernel")
            kernel:reload("permissions")
            kernel:reload_barrier()
            local props = T:get("bash").parameters.properties
            return props.sandbox_permissions.description, props.justification ~= nil
            "#,
        )
        .await;
        assert_eq!((own.as_str(), justification), ("the replacement's own", true));
        h.shutdown().await;
    })
    .await
}

/// dangerous commands, rules, and "always allow" prefixes read command-tool
/// declarations (`command_tool`, `command = { shell, arg } | { argv, defaults }`) across real
/// `bash`, `powershell`, template `T:command`, and unanalyzable command tools without name tables.
#[tokio::test]
async fn command_tool_declarations_drive_rules_danger_and_unanalyzable_refusals() {
    local(async {
        let env = TestEnv::new().await;
        let opts = json!({
            "preset": "auto",
            "rules": [
                { "argv": ["git", "push"], "decision": "forbidden" },
                { "argv": ["git", "status"], "decision": "allow" },
                { "tool": "opaque_cmd", "decision": "allow" },
                { "tool": "zsh_cmd", "decision": "allow" },
            ],
        });
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        let result_json: String = eval(
            &h,
            r#"
            local out = {}
            -- 1. Built-in declarations
            local bash_def = T:get("bash")
            out.bash_decl = bash_def and bash_def.command_tool == true
                and type(bash_def.command) == "table"
                and bash_def.command.shell == "bash"
                and bash_def.command.arg == "command"
            local ps_def = T:get("powershell")
            out.ps_decl_ok = (ps_def == nil) or (
                ps_def.command_tool == true
                and type(ps_def.command) == "table"
                and ps_def.command.shell == "powershell"
                and ps_def.command.arg == "command"
            )

            -- 1b. Real bash via T:execute: forbidden rule denies, dangerous asks in full-access
            local bash_forbid = T:execute({ id = "b1", name = "bash", arguments = { command = "git push" } })
            out.bash_forbid_err = bash_forbid.is_error == true
            assert(P:set("full-access"))
            local bash_danger_asks = 0
            local bash_danger = T:execute({ id = "b2", name = "bash", arguments = { command = "rm -rf missing-dir" } }, {
                approve = function()
                    bash_danger_asks = bash_danger_asks + 1
                    return { decision = "deny", reason = "blocked" }
                end,
            })
            out.bash_danger_asked = bash_danger_asks == 1 and bash_danger.is_error == true
            assert(P:set("auto"))

            -- 2. Custom PowerShell-declared command tool (custom parameter `script`, non-"powershell" name)
            local ps_ran = 0
            T:register("ps_runner", {
                description = "custom powershell command tool",
                side_effect = "external",
                command_tool = true,
                command = { shell = "powershell", arg = "script" },
                parameters = {
                    type = "object",
                    properties = {
                        script = { type = "string" },
                        sandbox_permissions = { type = "string" },
                        justification = { type = "string" },
                    },
                    required = { "script" },
                },
                execute = function(a)
                    ps_ran = ps_ran + 1
                    return { content = "ran:" .. tostring(a.script) }
                end,
            })
            local ps_forbid = T:execute({ id = "p1", name = "ps_runner", arguments = { script = "git push" } })
            local ps_allow = T:execute({ id = "p2", name = "ps_runner", arguments = { script = "git status" } })
            assert(P:set("full-access"))
            local ps_danger_asks, ps_danger_reason = 0, ""
            local ps_danger = T:execute({ id = "p3", name = "ps_runner", arguments = { script = "Remove-Item missing -Force" } }, {
                approve = function(_, _, v)
                    ps_danger_asks = ps_danger_asks + 1
                    ps_danger_reason = tostring(v and v.reason or "")
                    return { decision = "deny", reason = "blocked" }
                end,
            })
            out.ps_forbid = ps_forbid.is_error == true
            out.ps_allow = ps_allow.content == "ran:git status" and ps_ran == 1
            out.ps_danger = ps_danger_asks == 1 and ps_danger.is_error == true and ps_danger_reason:find("windows_force_delete", 1, true) ~= nil
            assert(P:set("auto"))

            -- 3. Template command tool (`T:command`) without `arguments.command`
            T:command({ name = "rm_tpl", description = "rm template", command = { "rm", "-rf", "{target}" }, params = { target = { type = "string", required = true } } })
            T:command({ name = "git_tpl", description = "git template", command = { "git", "{sub}", "{arg}" }, params = { sub = { type = "string", required = true }, arg = { type = "string", default = "origin" } } })
            local tpl_forbid = T:execute({ id = "t1", name = "git_tpl", arguments = { sub = "push" } })
            local tpl_escalate_prefix = ""
            local tpl_esc = T:execute({
                id = "t2",
                name = "git_tpl",
                arguments = { sub = "diff", arg = "HEAD", sandbox_permissions = "all", justification = "inspect" },
            }, {
                approve = function(_, _, v)
                    tpl_escalate_prefix = v and v.amendment and v.amendment.argv and table.concat(v.amendment.argv, " ") or ""
                    return { decision = "deny", reason = "stop" }
                end,
            })
            assert(P:set("full-access"))
            local tpl_danger_asks, tpl_danger_reason = 0, ""
            local tpl_danger = T:execute({ id = "t3", name = "rm_tpl", arguments = { target = "missing-dir" } }, {
                approve = function(_, _, v)
                    tpl_danger_asks = tpl_danger_asks + 1
                    tpl_danger_reason = tostring(v and v.reason or "")
                    return { decision = "deny", reason = "blocked" }
                end,
            })
            out.tpl_forbid = tpl_forbid.is_error == true
            out.tpl_prefix = tpl_escalate_prefix
            out.tpl_danger = tpl_danger_asks == 1 and tpl_danger.is_error == true and tpl_danger_reason:find("forced_rm", 1, true) ~= nil
            assert(P:set("auto"))

            -- 4. Unanalyzable command tools: hard-deny only when the command text names a forbidden
            -- executable (`zsh_cmd` with `git status`); opaque/unrelated text downgrades to `ask`
            -- without an allow-rule amendment.
            local opaque_ran = 0
            T:register("opaque_cmd", {
                description = "opaque command tool",
                side_effect = "external",
                command_tool = true,
                parameters = { type = "object", properties = { payload = { type = "string" } } },
                execute = function() opaque_ran = opaque_ran + 1 return { content = "ok" } end,
            })
            T:register("zsh_cmd", {
                description = "unsupported shell tool",
                side_effect = "external",
                command_tool = true,
                command = { shell = "zsh", arg = "cmd" },
                parameters = { type = "object", properties = { cmd = { type = "string" } } },
                execute = function() opaque_ran = opaque_ran + 1 return { content = "ok" } end,
            })
            local opaque_forbid_asks, opaque_forbid_amend = 0, false
            local opaque_forbid = T:execute({ id = "u1f", name = "opaque_cmd", arguments = { payload = "rm -rf /" } }, {
                approve = function(_, _, v)
                    opaque_forbid_asks = opaque_forbid_asks + 1
                    opaque_forbid_amend = opaque_forbid_amend or (v and v.amendment ~= nil)
                    return { decision = "deny", reason = "deny opaque" }
                end,
            })
            local zsh_forbid_asks = 0
            local zsh_forbid = T:execute({ id = "u2f", name = "zsh_cmd", arguments = { cmd = "git status" } }, {
                approve = function() zsh_forbid_asks = zsh_forbid_asks + 1 return { decision = "allow" } end,
            })
            local zsh_glob_asks, zsh_glob_amend = 0, false
            local zsh_glob = T:execute({ id = "u3f", name = "zsh_cmd", arguments = { cmd = "ls *.rs" } }, {
                approve = function(_, _, v)
                    zsh_glob_asks = zsh_glob_asks + 1
                    zsh_glob_amend = zsh_glob_amend or (v and v.amendment ~= nil)
                    return { decision = "deny", reason = "deny glob" }
                end,
            })
            out.opaque_forbid_asked = opaque_forbid_asks == 1 and not opaque_forbid_amend and opaque_forbid.is_error == true and opaque_ran == 0
            out.zsh_forbid_denied = zsh_forbid_asks == 0 and zsh_forbid.is_error == true and opaque_ran == 0
            out.zsh_glob_asked = zsh_glob_asks == 1 and not zsh_glob_amend and zsh_glob.is_error == true and opaque_ran == 0

            return mare.json.encode(out)
            "#,
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(v["bash_decl"], json!(true), "{v}");
        assert_eq!(v["ps_decl_ok"], json!(true), "{v}");
        assert_eq!(v["bash_forbid_err"], json!(true), "{v}");
        assert_eq!(v["bash_danger_asked"], json!(true), "{v}");
        assert_eq!(v["ps_forbid"], json!(true), "{v}");
        assert_eq!(v["ps_allow"], json!(true), "{v}");
        assert_eq!(v["ps_danger"], json!(true), "{v}");
        assert_eq!(v["tpl_forbid"], json!(true), "{v}");
        assert_eq!(v["tpl_prefix"], json!("git diff"), "{v}");
        assert_eq!(v["tpl_danger"], json!(true), "{v}");
        assert_eq!(v["opaque_forbid_asked"], json!(true), "{v}");
        assert_eq!(v["zsh_forbid_denied"], json!(true), "{v}");
        assert_eq!(v["zsh_glob_asked"], json!(true), "{v}");
        h.shutdown().await;

        // Without a forbidden argv rule, unanalyzable command tools still ask (never auto-allowed, no amendment).
        let opts_ask = json!({
            "preset": "auto",
            "rules": [
                { "tool": "opaque_cmd", "decision": "allow" },
                { "tool": "zsh_cmd", "decision": "allow" },
            ],
        });
        let h_ask = host(&env, true, opts_ask, json!({ "no_session": true })).await;
        let ask_json: String = eval(
            &h_ask,
            r#"
            local out = {}
            local opaque_ran = 0
            T:register("opaque_cmd", {
                description = "opaque command tool",
                side_effect = "external",
                command_tool = true,
                parameters = { type = "object", properties = { payload = { type = "string" } } },
                execute = function() opaque_ran = opaque_ran + 1 return { content = "ok" } end,
            })
            T:register("zsh_cmd", {
                description = "unsupported shell tool",
                side_effect = "external",
                command_tool = true,
                command = { shell = "zsh", arg = "cmd" },
                parameters = { type = "object", properties = { cmd = { type = "string" } } },
                execute = function() opaque_ran = opaque_ran + 1 return { content = "ok" } end,
            })
            local opaque_asks, opaque_amend = 0, false
            local opaque_res = T:execute({ id = "u1", name = "opaque_cmd", arguments = { payload = "rm -rf /" } }, {
                approve = function(_, _, v)
                    opaque_asks = opaque_asks + 1
                    opaque_amend = opaque_amend or (v and v.amendment ~= nil)
                    return { decision = "deny", reason = "deny opaque" }
                end,
            })
            local zsh_asks, zsh_amend = 0, false
            local zsh_res = T:execute({ id = "u2", name = "zsh_cmd", arguments = { cmd = "git status" } }, {
                approve = function(_, _, v)
                    zsh_asks = zsh_asks + 1
                    zsh_amend = zsh_amend or (v and v.amendment ~= nil)
                    return { decision = "deny", reason = "deny zsh" }
                end,
            })
            out.opaque_denied = opaque_asks == 1 and not opaque_amend and opaque_res.is_error == true and opaque_ran == 0
            out.zsh_denied = zsh_asks == 1 and not zsh_amend and zsh_res.is_error == true and opaque_ran == 0
            return mare.json.encode(out)
            "#,
        )
        .await;
        let va: serde_json::Value = serde_json::from_str(&ask_json).unwrap();
        assert_eq!(va["opaque_denied"], json!(true), "{va}");
        assert_eq!(va["zsh_denied"], json!(true), "{va}");
        h_ask.shutdown().await;
    })
    .await
}

/// Regression tests for the `command_tools` base should-fixes and edge cases:
/// - `opaque_cmd` / `zsh_cmd` ask even under `full-access` and key session cache on full args;
/// - `T:command` works with `params = nil`, `defaults`, and omitted optional params;
/// - `{argv}` runs through `shell_parse(argv)` so unwrapped `{ "env", "rm", "-rf", ... }`,
///   `{ "env", "git", "push" }`, `{ "sh", "-c", "rm -rf $(echo x)" }` (word_scan fallback), and
///   `{ "pwsh", "-Command", "Remove-Item {target} -Force" }` fail closed or enforce rules.
#[tokio::test]
async fn command_tool_review_edge_cases_and_fail_closed_fallbacks() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        let opts = json!({
            "preset": "auto",
            "rules": [
                { "argv": ["git", "status"], "decision": "allow" },
                { "tool": "zsh_extra", "decision": "prompt" },
                { "tool": "env_sh_split", "decision": "prompt" },
            ],
        });
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        let result_json: String = eval(
            &h,
            r#"
            local out = {}
            local T = C.cordis:get("tools")
            local P = C.cordis:get("permissions")

            local opaque_ran, zsh_ran = 0, 0
            T:register("opaque_cmd", {
                description = "opaque command tool",
                side_effect = "external",
                command_tool = true,
                parameters = { type = "object", properties = { payload = { type = "string" } } },
                execute = function(a) opaque_ran = opaque_ran + 1 return { content = "ok:" .. tostring(a.payload) } end,
            })
            T:register("zsh_cmd", {
                description = "unsupported shell tool",
                side_effect = "external",
                command_tool = true,
                command = { shell = "zsh", arg = "cmd" },
                parameters = { type = "object", properties = { cmd = { type = "string" } } },
                execute = function(a) zsh_ran = zsh_ran + 1 return { content = "ok:" .. tostring(a.cmd) } end,
            })

            -- 1. Unanalyzable command tools still ask under full-access
            assert(P:set("full-access"))
            local fa_opaque_asks, fa_zsh_asks = 0, 0
            local fa_opaque = T:execute({ id = "fa1", name = "opaque_cmd", arguments = { payload = "one" } }, {
                approve = function() fa_opaque_asks = fa_opaque_asks + 1 return { decision = "deny", reason = "ask" } end,
            })
            local fa_zsh = T:execute({ id = "fa2", name = "zsh_cmd", arguments = { cmd = "git status" } }, {
                approve = function() fa_zsh_asks = fa_zsh_asks + 1 return { decision = "deny", reason = "ask" } end,
            })
            out.fa_asks = fa_opaque_asks == 1 and fa_zsh_asks == 1 and fa_opaque.is_error == true and fa_zsh.is_error == true

            -- 2. Unanalyzable session cache keys on full args (approving payload="a" does not approve "b")
            assert(P:set("auto"))
            local o_asks = 0
            local oa1 = T:execute({ id = "o1", name = "opaque_cmd", arguments = { payload = "a" } }, {
                approve = function() o_asks = o_asks + 1 return { decision = "allow_session" } end,
            })
            local oa2 = T:execute({ id = "o2", name = "opaque_cmd", arguments = { payload = "a" } }, {
                approve = function() o_asks = o_asks + 1 return { decision = "allow_session" } end,
            })
            local ob = T:execute({ id = "o3", name = "opaque_cmd", arguments = { payload = "b" } }, {
                approve = function() o_asks = o_asks + 1 return { decision = "deny", reason = "diff payload" } end,
            })
            out.opaque_cache_ok = o_asks == 2 and oa1.content == "ok:a" and oa2.content == "ok:a" and ob.is_error == true

            local z_asks = 0
            local za1 = T:execute({ id = "z1", name = "zsh_cmd", arguments = { cmd = "ls" } }, {
                approve = function() z_asks = z_asks + 1 return { decision = "allow_session" } end,
            })
            local zb = T:execute({ id = "z2", name = "zsh_cmd", arguments = { cmd = "pwd" } }, {
                approve = function() z_asks = z_asks + 1 return { decision = "deny", reason = "diff cmd" } end,
            })
            T:command({
                name = "zsh_extra",
                description = "zsh argv with same primary cmd and extra arg",
                command = { "zsh", "-c", "{cmd}", "{extra}" },
                params = { cmd = { type = "string", required = true }, extra = { type = "string", required = true } },
            })
            local ze_asks = 0
            T:execute({ id = "ze1", name = "zsh_extra", arguments = { cmd = "echo hi", extra = "a" } }, {
                approve = function() ze_asks = ze_asks + 1 return { decision = "allow_session" } end,
            })
            local ze2 = T:execute({ id = "ze2", name = "zsh_extra", arguments = { cmd = "echo hi", extra = "b" } }, {
                approve = function() ze_asks = ze_asks + 1 return { decision = "deny", reason = "diff extra" } end,
            })
            T:command({
                name = "env_sh_split",
                description = "unanalyzable argv multi-placeholder cache key check",
                command = { "env", "sh", "-c", "{a}", "{b}" },
                params = { a = { type = "string", required = true }, b = { type = "string", required = true } },
            })
            local split_asks = 0
            T:execute({ id = "sp1", name = "env_sh_split", arguments = { a = "true x", b = "y" } }, {
                approve = function() split_asks = split_asks + 1 return { decision = "allow_session" } end,
            })
            local split2 = T:execute({ id = "sp2", name = "env_sh_split", arguments = { a = "true", b = "x y" } }, {
                approve = function() split_asks = split_asks + 1 return { decision = "deny", reason = "split diff" } end,
            })
            out.zsh_cache_ok = z_asks == 2 and za1.content == "ok:ls" and zb.is_error == true
                and ze_asks == 2 and ze2.is_error == true and split_asks == 2 and split2.is_error == true

            -- 3. `T:command` with params = nil, omitted optional params, and defaults
            -- Windows' Git-Bash `true`/`echo` are MSYS binaries blocked inside windows-acl; use native cmd.exe.
            local true_cmd = mare.os == "windows" and { "cmd", "/d", "/c", "exit", "0" } or { "true" }
            T:command({ name = "true_nil_params", description = "no params table", readonly = true, command = true_cmd })
            local nil_p = T:execute({ id = "np1", name = "true_nil_params", arguments = {} })
            out.nil_params_ok = nil_p.is_error ~= true

            local echo_cmd = mare.os == "windows" and { "cmd", "/d", "/c", "echo", "{first}", "{opt}" }
                or { "echo", "{first}", "{opt}" }
            T:command({
                name = "opt_echo",
                description = "optional placeholder omitted",
                command = echo_cmd,
                params = {
                    first = { type = "string", required = true },
                    opt = { type = "string" },
                },
            })
            local opt_asks = 0
            local opt_res = T:execute({ id = "opt1", name = "opt_echo", arguments = { first = "hi" } }, {
                approve = function() opt_asks = opt_asks + 1 return { decision = "allow_once" } end,
            })
            out.opt_omitted_ok = opt_asks == 0 and opt_res.is_error ~= true and (opt_res.content or ""):find("hi", 1, true) ~= nil

            -- 4. argv template unwrapping (`env rm -rf`, `env git push`) & nested shell / word_scan / pwsh
            assert(P:set("full-access"))
            T:command({ name = "env_rm", description = "env rm", command = { "env", "rm", "-rf", "{target}" }, params = { target = { type = "string", default = "nope" } } })
            local env_rm_asks = 0
            local env_rm = T:execute({ id = "er1", name = "env_rm", arguments = {} }, {
                approve = function(_, _, v)
                    if tostring(v and v.reason or ""):find("forced_rm", 1, true) then env_rm_asks = env_rm_asks + 1 end
                    return { decision = "deny", reason = "blocked" }
                end,
            })
            out.env_rm_danger = env_rm_asks == 1 and env_rm.is_error == true

            T:command({ name = "env_git", description = "env git", command = { "env", "git", "{sub}" }, params = { sub = { type = "string", required = true } } })
            T:command({ name = "sh_tpl", description = "sh -c template", command = { "sh", "-c", "{script}" }, params = { script = { type = "string", required = true } } })
            local sh_wordscan_asks = 0
            local sh_ws = T:execute({ id = "sw1", name = "sh_tpl", arguments = { script = "rm -rf missing; )" } }, {
                approve = function(_, _, v)
                    if tostring(v and v.reason or ""):find("forced_rm", 1, true) then sh_wordscan_asks = sh_wordscan_asks + 1 end
                    return { decision = "deny", reason = "blocked" }
                end,
            })

            T:command({ name = "pwsh_rm", description = "pwsh rm", command = { "pwsh", "-Command", "Remove-Item {target} -Force" }, params = { target = { type = "string", required = true } } })
            local pwsh_asks = 0
            local pwsh_res = T:execute({ id = "pw1", name = "pwsh_rm", arguments = { target = "missing" } }, {
                approve = function(_, _, v)
                    if tostring(v and v.reason or ""):find("windows_force_delete", 1, true) then pwsh_asks = pwsh_asks + 1 end
                    return { decision = "deny", reason = "blocked" }
                end,
            })
            T:command({ name = "env_pwsh_rm", description = "env pwsh rm", command = { "env", "pwsh", "-Command", "Remove-Item {target} -Force" }, params = { target = { type = "string", required = true } } })
            local env_pwsh_asks = 0
            local env_pwsh_res = T:execute({ id = "pw2", name = "env_pwsh_rm", arguments = { target = "missing" } }, {
                approve = function(_, _, v)
                    if tostring(v and v.reason or ""):find("windows_force_delete", 1, true) then env_pwsh_asks = env_pwsh_asks + 1 end
                    return { decision = "deny", reason = "blocked" }
                end,
            })
            out.pwsh_danger = pwsh_asks == 1 and pwsh_res.is_error == true
                and env_pwsh_asks == 1 and env_pwsh_res.is_error == true

            assert(P:set("auto"))
            -- With a prompt argv rule on the host, an unanalyzable command fails closed to ask
            -- rather than silently bypassing the rule under auto.
            P:add_rules({ { argv = { "git", "push" }, decision = "prompt" } })
            T:command({ name = "pwsh_file", description = "pwsh -File before -Command", command = { "pwsh", "-File", "{file}", "-Command", "git status" }, params = { file = { type = "string", required = true } } })
            local pf_asks = 0
            local pf_res = T:execute({ id = "pf1", name = "pwsh_file", arguments = { file = "evil.ps1" } }, {
                approve = function() pf_asks = pf_asks + 1 return { decision = "deny", reason = "blocked" } end,
            })
            out.pwsh_file_blocked = pf_asks == 1 and pf_res.is_error == true

            -- Adding forbidden argv rules enforces exact `env git push` and nested `sh -c "true forbid-probe"`.
            assert(P:set("full-access"))
            P:add_rules({
                { argv = { "git", "push" }, decision = "forbidden" },
                { argv = { "true", "forbid-probe" }, decision = "forbidden" },
            })
            local env_git_forbid = T:execute({ id = "eg1", name = "env_git", arguments = { sub = "push" } })
            out.env_git_forbid = env_git_forbid.is_error == true and env_git_forbid.refused == true
            local sh_forbid = T:execute({ id = "sf1", name = "sh_tpl", arguments = { script = "true forbid-probe" } })
            out.sh_nested_ok = sh_wordscan_asks == 1 and sh_ws.is_error == true
                and sh_forbid.is_error == true and sh_forbid.refused == true

            out.placeholder_exe_rejected = not pcall(T.command, T, {
                name = "bad_ph_exe",
                description = "placeholder exe",
                command = { "{bin}", "arg" },
                params = { bin = { type = "string", required = true } },
            })
            out.mixed_rejected = not pcall(T.register, T, "bad_mix", {
                description = "bad mix",
                command = { shell = "bash", arg = "c", argv = { "echo" } },
                execute = function() return { content = "no" } end,
            })

            return mare.json.encode(out)
            "#,
        )
        .await;
        let v: serde_json::Value = serde_json::from_str(&result_json).unwrap();
        assert_eq!(v["fa_asks"], json!(true), "{v}");
        assert_eq!(v["opaque_cache_ok"], json!(true), "{v}");
        assert_eq!(v["zsh_cache_ok"], json!(true), "{v}");
        assert_eq!(v["nil_params_ok"], json!(true), "{v}");
        assert_eq!(v["opt_omitted_ok"], json!(true), "{v}");
        assert_eq!(v["env_rm_danger"], json!(true), "{v}");
        assert_eq!(v["env_git_forbid"], json!(true), "{v}");
        assert_eq!(v["sh_nested_ok"], json!(true), "{v}");
        assert_eq!(v["pwsh_danger"], json!(true), "{v}");
        assert_eq!(v["pwsh_file_blocked"], json!(true), "{v}");
        assert_eq!(v["placeholder_exe_rejected"], json!(true), "{v}");
        assert_eq!(v["mixed_rejected"], json!(true), "{v}");
        h.shutdown().await;
    })
    .await
}
