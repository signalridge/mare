use super::*;

/// A shell behind a command wrapper still runs the forbidden command (PERM-ESCAPE-b, b54 review):
/// `env sh -c 'git push'` and the like must ask or deny, never fall back to the preset's allow
/// (control: without the rule they are allowed under full-access).
#[tokio::test]
async fn a_forbidden_command_in_a_shell_behind_a_wrapper_is_not_allowed() {
    local(async {
        let env = TestEnv::new().await;
        let cmds = r#"{ "env sh -c 'git push'", "nohup bash -c 'git push'", "env FOO=1 bash -lc 'git push'",
                       "timeout 5 sh -c 'git push origin'", "command sh -c 'git push'" }"#;
        let votes = |rules: Value| {
            let env = &env;
            async move {
                let h = host(env, true, json!({ "preset": "full-access", "rules": rules }), json!({ "no_session": true })).await;
                exec(&h, VOTE).await;
                let v: Vec<String> = eval(
                    &h,
                    &format!(
                        r#"local out = {{}}
                           for _, s in ipairs({cmds}) do out[#out + 1] = s .. " => " .. tostring(VOTE(BASH(s)).decision) end
                           return out"#
                    ),
                )
                .await;
                h.shutdown().await;
                v
            }
        };
        let free = votes(json!([])).await;
        assert!(free.iter().all(|v| v.ends_with("=> allow")), "control: full-access allows them without a rule: {free:?}");
        let ruled = votes(json!([{ "argv": ["git", "push"], "decision": "forbidden" }])).await;
        let allowed: Vec<_> = ruled.iter().filter(|v| v.ends_with("=> allow")).collect();
        assert!(allowed.is_empty(), "a forbidden command ran through a wrapped shell: {allowed:?} (all: {ruled:?})");
    })
    .await
}

#[tokio::test]
async fn deny_reasons_from_parse_hook_refuses_wrapped_shell_in_full_access() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "preset": "full-access", "rules": [] }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let decision: String = eval(
            &h,
            r#"
            local off = C.cordis:on("permission/parse", function(data, ev) data.parsed = { deny_reasons = { "org: no curl" } }; return ev.next(data) end)
            local d = VOTE(BASH('env sh -c "curl https://example.com"')).decision
            off()
            return d
            "#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(decision, "deny", "permission/parse deny_reasons must refuse even wrapped shells");
    })
    .await
}

#[tokio::test]
async fn forbidden_outer_segment_stays_deny_when_paired_with_wrapped_shell() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([{ "argv": ["git", "push"], "decision": "forbidden" }]);
        let h =
            host(&env, true, json!({ "preset": "full-access", "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let decision: String =
            eval(&h, r#"return VOTE(BASH('git push && timeout 60 sh -c "echo ok"')).decision"#).await;
        h.shutdown().await;
        assert_eq!(decision, "deny", "a definite forbidden outer segment must stay deny, not downgrade to ask");
    })
    .await
}

#[tokio::test]
async fn busybox_and_ash_wrapped_shells_do_not_bypass_forbidden_rules() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([{ "argv": ["rm", "-rf"], "decision": "forbidden" }]);
        let h =
            host(&env, true, json!({ "preset": "full-access", "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let decisions: Vec<String> = eval(
            &h,
            r#"
            local cmds = { 'busybox sh -c "rm -rf dist"', 'env ash -c "rm -rf dist"', 'busybox ash -c "rm -rf dist"' }
            local out = {}
            for _, s in ipairs(cmds) do out[#out + 1] = s .. " => " .. tostring(VOTE(BASH(s)).decision) end
            return out
            "#,
        )
        .await;
        h.shutdown().await;
        let allowed: Vec<_> = decisions.iter().filter(|v| v.ends_with("=> allow")).collect();
        assert!(allowed.is_empty(), "busybox/ash wrapped shell bypassed forbidden rule: {decisions:?}");
    })
    .await
}

#[tokio::test]
async fn busybox_direct_applet_hits_forbidden_and_danger_without_allowing_rm() {
    local(async {
        let env = TestEnv::new().await;
        // These are votes only; explicitly requesting network lets the test isolate rule matching
        // from the separate network-partial warning on the default Windows tier.
        // 1. With forbidden {"rm", "-rf"}, busybox rm -rf dist must deny (including path-qualified / .exe).
        let h_forbid = host(
            &env,
            true,
            json!({ "preset": "full-access", "rules": [{ "argv": ["rm", "-rf"], "decision": "forbidden" }] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h_forbid, VOTE).await;
        let forbid_decisions: Vec<String> = eval(
            &h_forbid,
            r#"
            local cmds = { "busybox rm -rf dist", "/bin/busybox rm -rf dist", "busybox.exe rm -rf dist" }
            local out = {}
            for _, s in ipairs(cmds) do out[#out + 1] = VOTE(BASH(s)).decision end
            return out
            "#,
        )
        .await;
        h_forbid.shutdown().await;
        assert_eq!(forbid_decisions, vec!["deny", "deny", "deny"]);

        // 2. Without a forbidden rule, busybox rm -rf dist still triggers dangerous-command detection (ask even in full-access),
        //    and allow {"rm"} does NOT allow busybox rm (in read-only preset, `rm dist` is allowed by the rule while `busybox rm dist` still asks).
        let h_allow = host(
            &env,
            true,
            json!({ "preset": "read-only", "rules": [{ "argv": ["rm"], "decision": "allow", "scope": "network" }] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h_allow, VOTE).await;
        let (rm_d, danger_d, non_danger_d): (String, String, String) = eval(
            &h_allow,
            r#"return VOTE(BASH("rm dist", "network", "x")).decision, VOTE(BASH("busybox rm -rf dist", "network", "x")).decision, VOTE(BASH("busybox rm dist", "network", "x")).decision"#,
        )
        .await;
        h_allow.shutdown().await;
        assert_eq!(rm_d, "allow", "control: allow {{rm}} allows plain rm dist");
        assert_eq!(danger_d, "ask", "busybox rm -rf dist must ask as dangerous");
        assert_eq!(non_danger_d, "ask", "allow {{rm}} must NOT allow busybox rm");

        let h_full =
            host(&env, true, json!({ "preset": "full-access", "rules": [] }), json!({ "no_session": true })).await;
        exec(&h_full, VOTE).await;
        let (full_danger, full_list): (String, String) = eval(
            &h_full,
            r#"return VOTE(BASH("busybox rm -rf dist")).decision, VOTE(BASH("busybox --list")).decision"#,
        )
        .await;
        h_full.shutdown().await;
        assert_eq!(full_danger, "ask", "busybox rm -rf dist must ask in full-access");
        assert_eq!(full_list, "allow", "busybox --list is not dangerous in full-access");
    })
    .await
}

#[tokio::test]
async fn deny_reasons_and_forbidden_outer_segment_hold_with_mid_word_hash() {
    local(async {
        let env = TestEnv::new().await;
        // 1. permission/parse hook deny_reasons keeps deny on `echo foo#bar`
        let h_hook =
            host(&env, true, json!({ "preset": "full-access", "rules": [] }), json!({ "no_session": true })).await;
        exec(&h_hook, VOTE).await;
        let hook_decision: String = eval(
            &h_hook,
            r#"
            local off = C.cordis:on("permission/parse", function(data, ev) data.parsed = { deny_reasons = { "org: blocked" } }; return ev.next(data) end)
            local d = VOTE(BASH("echo foo#bar")).decision
            off()
            return d
            "#,
        )
        .await;
        h_hook.shutdown().await;
        assert_eq!(hook_decision, "deny", "permission/parse deny_reasons must keep deny on echo foo#bar");

        // 2. `git push && echo foo#bar` (with `git push` forbidden) keeps deny, not downgraded to ask
        let h_forbid = host(
            &env,
            true,
            json!({ "preset": "full-access", "rules": [{ "argv": ["git", "push"], "decision": "forbidden" }] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h_forbid, VOTE).await;
        let forbid_decision: String =
            eval(&h_forbid, r#"return VOTE(BASH("git push && echo foo#bar")).decision"#).await;
        h_forbid.shutdown().await;
        assert_eq!(forbid_decision, "deny", "git push && echo foo#bar must keep deny");
    })
    .await
}

/// A program named by a path never borrows an allow rule written for a command name
/// (permissions corpus; Codex execpolicy/src/policy.rs:462 match_host_executable_rules,
/// execpolicy/tests/basic.rs:958, core/src/exec_policy_tests.rs:1102): the model can write an
/// executable `./git` in the workspace, and `{argv = {"git","status"}, allow, scope = "all"}`
/// must not run it outside the sandbox. Bare `git status` still matches; forbidden rules keep
/// matching the basename (the conservative direction).
#[cfg(unix)]
#[tokio::test]
async fn a_path_named_program_does_not_borrow_a_command_name_allow_rule() {
    local(async {
        let env = TestEnv::new().await;
        for p in ["git", "bin/git"] {
            let f = env.cwd.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, "#!/bin/sh\necho escaped\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let rules = json!([
            { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
            { "argv": ["git", "push"], "decision": "forbidden" },
            { "argv": ["sudo"], "decision": "prompt" },
        ]);
        let h = host(&env, true, json!({ "preset": "auto", "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let abs = env.cwd.join("git").display().to_string();
        let votes: Vec<String> = eval(
            &h,
            &format!(
                r#"local out = {{}}
                   for _, s in ipairs({{ "git status", "./git status", "bin/git status", {abs:?} .. " status", "./git push", "./sudo id" }}) do
                       local v = VOTE(BASH(s))
                       out[#out + 1] = s .. " => " .. tostring(v.decision) .. "/" .. (v.sandbox_grant == "danger-full-access" and "unsandboxed" or "sandboxed")
                   end
                   return out"#
            ),
        )
        .await;
        h.shutdown().await;
        assert!(votes[0].ends_with("=> allow/unsandboxed"), "control: bare git status uses the rule (scope all): {votes:?}");
        let borrowed: Vec<_> = votes[1..4].iter().filter(|v| v.ends_with("allow/unsandboxed")).collect();
        assert!(borrowed.is_empty(), "a path-named program used the git allow rule: {borrowed:?} (all: {votes:?})");
        assert!(votes[4].contains("=> deny"), "forbidden rules still match the basename: {votes:?}");
        assert!(votes[5].contains("=> ask"), "prompt rules still match the basename: {votes:?}");
    })
    .await
}

/// PowerShell / Windows backslash paths: `\git.exe` and `C:\tools\git.exe` are path-qualified
/// and must not borrow a bare-name `allow` rule, while `forbidden` and `prompt` rules still
/// match the basename across both `/` and `\`. `.exe` stripping and case-folding apply only on
/// Windows (and case-folding on macOS), not on Linux `pwsh` (`git.exe` != `git`).
#[tokio::test]
async fn powershell_backslash_path_does_not_borrow_allow_and_still_matches_deny_prompt() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([
            { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
            { "argv": ["git", "push"], "decision": "forbidden" },
            { "argv": ["sudo"], "decision": "prompt" },
        ]);
        let h = host(&env, true, json!({ "preset": "auto", "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let votes: Vec<String> = eval(
            &h,
            r#"local function PWSH(cmd)
                   return { id = "c" .. math.random(1e9), name = "powershell", arguments = { command = cmd } }
               end
               local out = {}
               for _, s in ipairs({
                   "git status",
                   "git.exe status",
                   "Git status",
                   "\\git status",
                   "\\git.exe status",
                   "C:\\tools\\git.exe status",
                   "C:\\tools\\git push",
                   "C:\\tools\\git.exe push",
                   ".\\sudo id",
               }) do
                   local v = VOTE(PWSH(s), { command_tool = true, command = { shell = "powershell", arg = "command" } })
                   out[#out + 1] = s .. " => " .. tostring(v.decision) .. "/" .. (v.sandbox_grant == "danger-full-access" and "unsandboxed" or "sandboxed")
               end
               return out"#,
        )
        .await;
        h.shutdown().await;
        assert!(votes[0].ends_with("=> allow/unsandboxed"), "control: bare git status uses the rule (scope all): {votes:?}");
        assert_eq!(
            votes[1].ends_with("=> allow/unsandboxed"),
            cfg!(windows),
            "git.exe only borrows git allow on Windows: {votes:?}"
        );
        assert_eq!(
            votes[2].ends_with("=> allow/unsandboxed"),
            cfg!(any(target_os = "macos", windows)),
            "Git status only case-folds on macOS/Windows: {votes:?}"
        );
        let borrowed: Vec<_> = votes[3..6].iter().filter(|v| v.ends_with("allow/unsandboxed")).collect();
        assert!(borrowed.is_empty(), "a backslash-path program used the git allow rule: {borrowed:?} (all: {votes:?})");
        assert!(votes[6].contains("=> deny"), "forbidden rules still match the basename across backslashes: {votes:?}");
        assert_eq!(
            votes[7].contains("=> deny"),
            cfg!(windows),
            "C:\\tools\\git.exe push only strips .exe on Windows: {votes:?}"
        );
        assert!(votes[8].contains("=> ask"), "prompt rules still match the basename across backslashes: {votes:?}");
    })
    .await
}

/// POSIX bash backslash escaping (`\git`, `./\git`) is character escaping rather than a path
/// separator: `\git status` still hits the bare `git` allow rule and `\git push` hits `forbidden`,
/// while `./git status` and `./\git status` remain path-qualified and do not borrow `allow`.
#[tokio::test]
async fn bash_backslash_escaped_command_matches_bare_name_while_path_does_not() {
    local(async {
        let env = TestEnv::new().await;
        let rules = json!([
            { "argv": ["git", "status"], "decision": "allow", "scope": "all" },
            { "argv": ["git", "push"], "decision": "forbidden" },
        ]);
        let h = host(&env, true, json!({ "preset": "auto", "rules": rules }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let votes: Vec<String> = eval(
            &h,
            r#"local out = {}
               for _, s in ipairs({ "\\git status", "\\git push", "./git status", "./\\git status" }) do
                   local v = VOTE(BASH(s))
                   out[#out + 1] = s .. " => " .. tostring(v.decision) .. "/" .. (v.sandbox_grant == "danger-full-access" and "unsandboxed" or "sandboxed")
               end
               return out"#,
        )
        .await;
        h.shutdown().await;
        assert!(votes[0].ends_with("=> allow/unsandboxed"), "\\git status should hit bare git allow rule: {votes:?}");
        assert!(votes[1].contains("=> deny"), "\\git push should hit forbidden rule: {votes:?}");
        assert!(!votes[2].ends_with("allow/unsandboxed"), "./git status must not borrow allow rule: {votes:?}");
        assert!(!votes[3].ends_with("allow/unsandboxed"), "./\\git status must not borrow allow rule: {votes:?}");
    })
    .await
}

/// Item 1 (self-written): PowerShell and bash `workdir` must enter permission rules, approval
/// cache keys, and process launch cwd.
#[tokio::test]
async fn powershell_and_bash_workdir_enter_permission_rules_and_approval_cache() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        let dir_a = env.cwd.join("dir_a");
        let dir_b = env.cwd.join("dir_b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let rules = json!([
            { "argv": ["mytool", "run"], "cwd": dir_a.to_string_lossy(), "decision": "allow", "scope": "all" },
            { "argv": ["mytool", "check"], "decision": "prompt" }
        ]);
        #[cfg(not(windows))]
        let h = {
            let fake_pwsh = env.cwd.join("fake_pwsh.sh");
            std::fs::write(
                &fake_pwsh,
                "#!/bin/sh\nfor arg do cmd=\"$arg\"; done\ncase \"$cmd\" in\n  *pwd*) pwd -P ;;\n  *) printf 'ok\\n' ;;\nesac\n",
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_pwsh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let cfg_dir = env.config_dir();
            std::fs::create_dir_all(&cfg_dir).unwrap();
            std::fs::write(
                cfg_dir.join("init.lua"),
                format!(
                    "mare.proc.powershell = function() return {{ path = {:?}, args = {{ \"-NoProfile\", \"-NonInteractive\", \"-Command\" }}, kind = \"powershell\", transport = \"argv\" }} end\n",
                    fake_pwsh.to_string_lossy()
                ),
            )
            .unwrap();
            let mut b = env
                .host()
                .user_config(true)
                .config(json!({
                    "plugins": {
                        "tools": { "powershell": true },
                        "permissions": { "enabled": true, "rules": rules }
                    }
                }))
                .args(json!({ "no_session": true }));
            b.opts.project_trusted = true;
            let h = b.build().await;
            expose(&h);
            global(&h, "P", h.service_table("permissions").unwrap());
            h
        };
        #[cfg(windows)]
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        global(&h, "DIR_A", dir_a.to_string_lossy().to_string());
        global(&h, "DIR_B", dir_b.to_string_lossy().to_string());
        let (bash_a_asked, bash_b_asked, bash_b_cwd, ps_a_first, ps_c1_asked, ps_c1_out, ps_c1_err, ps_c1_again_asked, ps_b_after_a, ps_b_cwd): (
            bool,
            bool,
            String,
            String,
            bool,
            String,
            bool,
            bool,
            String,
            String,
        ) = eval(
            &h,
            r#"
            -- Test `bash` with `workdir` through T:execute (which runs tools/pipeline.lua).
            local asked_a2 = false
            T:execute({ id = "b-a2", name = "bash", arguments = {
                command = "mytool run", workdir = DIR_A, sandbox_permissions = "all", justification = "test"
            } }, {
                approve = function()
                    asked_a2 = true
                    return { decision = "deny", source = "user" }
                end,
            })

            local asked_b = false
            local seen_cwd_b
            T:execute({ id = "b-b", name = "bash", arguments = {
                command = "mytool run", workdir = DIR_B, sandbox_permissions = "all", justification = "test"
            } }, {
                approve = function(ev)
                    asked_b = true
                    seen_cwd_b = ev.cwd
                    return { decision = "deny", source = "user" }
                end,
            })

            assert(T:get("powershell") ~= nil, "powershell tool must be enabled in test")
            local ps_asked_a = false
            T:execute({ id = "ps-a", name = "powershell", arguments = { command = "mytool run", workdir = DIR_A } }, {
                approve = function() ps_asked_a = true; return { decision = "deny", source = "user" } end,
            })
            local ps_a_first = ps_asked_a and "ask" or "allow"

            -- Approve `mytool check; pwd` in DIR_A for session, assert it actually ran in DIR_A and cached the approval.
            local ps_c1_asked = false
            local ps_c1_res = T:execute({ id = "ps-c1", name = "powershell", arguments = { command = "mytool check; pwd", workdir = DIR_A } }, {
                approve = function()
                    ps_c1_asked = true
                    return { decision = "allow_session", source = "user" }
                end,
            })
            local ps_c1_again_asked = false
            T:execute({ id = "ps-c1-again", name = "powershell", arguments = { command = "mytool check; pwd", workdir = DIR_A } }, {
                approve = function()
                    ps_c1_again_asked = true
                    return { decision = "deny", source = "user" }
                end,
            })

            -- Verify DIR_B still asks and sees DIR_B as ev.cwd.
            local ps_asked_b = false
            local ps_b_cwd = ""
            T:execute({ id = "ps-c2", name = "powershell", arguments = { command = "mytool check; pwd", workdir = DIR_B } }, {
                approve = function(ev)
                    ps_asked_b = true
                    ps_b_cwd = tostring(ev.cwd)
                    return { decision = "deny", source = "user" }
                end,
            })
            local ps_b_after_a = ps_asked_b and "ask" or "allow"
            return asked_a2, asked_b, tostring(seen_cwd_b), ps_a_first, ps_c1_asked, tostring(ps_c1_res and ps_c1_res.content or ""), ps_c1_res and ps_c1_res.is_error == true, ps_c1_again_asked, ps_b_after_a, ps_b_cwd
            "#,
        )
        .await;
        assert!(!bash_a_asked, "bash with workdir=DIR_A must match DIR_A cwd rule");
        assert!(bash_b_asked, "bash with workdir=DIR_B must NOT match DIR_A cwd rule");
        assert_eq!(bash_b_cwd, dir_b.to_string_lossy());
        assert_eq!(ps_a_first, "allow");
        assert!(ps_c1_asked, "first powershell call in DIR_A must prompt for session approval");
        assert!(!ps_c1_err, "approved powershell pwd in DIR_A must succeed: {ps_c1_out}");
        let expected_a = dunce::canonicalize(&dir_a).unwrap();
        let matched_a = ps_c1_out.lines().any(|line| {
            dunce::canonicalize(line.trim()).ok().as_ref() == Some(&expected_a)
        });
        assert!(matched_a, "powershell with workdir=DIR_A must run in DIR_A, got: {ps_c1_out}");
        assert!(!ps_c1_again_asked, "second powershell call in DIR_A must use cached session approval");
        assert_eq!(ps_b_after_a, "ask", "session approval in DIR_A must not apply to DIR_B");
        assert_eq!(ps_b_cwd, dir_b.to_string_lossy());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn bash_workdir_launches_shell_in_frozen_cwd_not_mare_cwd() {
    local(async {
        let env = TestEnv::new().await;
        let dir_a = env.cwd.join("dir_a");
        let dir_b = env.cwd.join("dir_b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();
        let rules = json!([
            { "argv": ["pwd"], "cwd": dir_a.to_string_lossy(), "decision": "allow", "scope": "all" }
        ]);
        let h = host(&env, true, json!({ "rules": rules }), json!({ "no_session": true })).await;
        global(&h, "DIR_A", dir_a.to_string_lossy().to_string());
        global(&h, "DIR_B", dir_b.to_string_lossy().to_string());
        let (asked_a, out_a, asked_b, seen_cwd_b): (bool, String, bool, String) = eval(
            &h,
            r#"
            local asked_a = false
            local res_a = T:execute({ id = "pwd-a", name = "bash", arguments = {
                command = mare.os == "windows" and "pwd -W" or "pwd", workdir = DIR_A, sandbox_permissions = "all", justification = "test"
            } }, {
                approve = function()
                    asked_a = true
                    return { decision = "deny", source = "user" }
                end,
            })

            local asked_b = false
            local seen_cwd_b = ""
            T:execute({ id = "pwd-b", name = "bash", arguments = {
                command = "pwd", workdir = DIR_B, sandbox_permissions = "all", justification = "test"
            } }, {
                approve = function(ev)
                    asked_b = true
                    seen_cwd_b = tostring(ev.cwd)
                    return { decision = "deny", source = "user" }
                end,
            })
            return asked_a, tostring(res_a and res_a.content or ""), asked_b, seen_cwd_b
            "#,
        )
        .await;
        assert!(!asked_a, "pwd in DIR_A must match DIR_A cwd rule");
        let expected_a = dunce::canonicalize(&dir_a).unwrap();
        let actual_a = dunce::canonicalize(out_a.trim()).unwrap_or_else(|_| std::path::PathBuf::from(out_a.trim()));
        assert_eq!(
            actual_a, expected_a,
            "bash with workdir=DIR_A must launch in frozen.cwd ({expected_a:?}), not mare.cwd ({:?})",
            env.cwd
        );
        assert!(asked_b, "pwd in DIR_B must not match DIR_A cwd rule");
        assert_eq!(seen_cwd_b, dir_b.to_string_lossy());
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn persistent_bash_refuses_when_shell_cwd_changes_while_waiting_for_lock() {
    local(async {
        let env = TestEnv::new().await;
        let dir_b = env.cwd.join("dir_b");
        std::fs::create_dir_all(&dir_b).unwrap();
        let ready = env.cwd.join("ready.flag");
        let go = env.cwd.join("go.flag");
        // On Windows, `sandbox_permissions = "all"` forces bash into a temporary non-persistent shell,
        // while sandboxed Git Bash fails with SANDBOX_MSYS_UNSUPPORTED; run unconfined under full-access
        // so both calls exercise the persistent shell lock.
        let opts = if cfg!(windows) { json!({ "preset": "full-access" }) } else { json!({}) };
        let h = host(&env, true, opts, json!({ "no_session": true })).await;
        global(&h, "DIR_B", dir_b.to_string_lossy().to_string());
        global(&h, "READY_FILE", ready.to_string_lossy().to_string());
        global(&h, "GO_FILE", go.to_string_lossy().to_string());
        let (res2_err, res2_content, res2_ran_in_b): (bool, String, bool) = eval(
            &h,
            r#"
            local res1
            local done1, done2 = false, false
            -- Git Bash accepts drive paths with forward slashes; Lua's %q escaping of a
            -- Windows backslash path is not a shell path conversion.
            local function bash_path(path)
                return mare.os == "windows" and path:gsub("\\", "/") or path
            end
            mare.task.spawn(function()
                res1 = T:execute({
                    id = "c1",
                    name = "bash",
                    arguments = {
                        command = string.format('touch %q; while [ ! -f %q ]; do sleep 0.02; done; cd %q', bash_path(READY_FILE), bash_path(GO_FILE), bash_path(DIR_B)),
                    },
                }, {
                    approve = function() return { decision = "allow_once", source = "user" } end,
                })
                done1 = true
            end)

            local dl = mare.task.clock() + 15000
            while not mare.fs.exists(READY_FILE) and mare.task.clock() < dl do
                mare.task.sleep(10)
            end
            assert(mare.fs.exists(READY_FILE), "first bash call must hold persistent shell lock")

            local prepared2 = false
            local res2
            mare.task.spawn(function()
                res2 = T:execute({
                    id = "c2",
                    name = "bash",
                    arguments = {
                        command = "printf 'RAN_IN:%s' \"$(pwd -P)\"",
                    },
                }, {
                    approve = function() return { decision = "allow_once", source = "user" } end,
                    on_prepared = function() prepared2 = true end,
                })
                done2 = true
            end)

            dl = mare.task.clock() + 15000
            while not prepared2 and mare.task.clock() < dl do
                mare.task.sleep(10)
            end
            assert(prepared2, "second bash call must pass approval in original cwd")
            -- Let call 2 reach shell_rx:recv(), then release call 1 to `cd DIR_B` and drop the lock.
            mare.task.sleep(50)
            mare.fs.write(GO_FILE, "go")

            dl = mare.task.clock() + 15000
            while not (done1 and done2) and mare.task.clock() < dl do
                mare.task.sleep(10)
            end
            assert(done1 and done2, "both bash calls must finish")
            assert(res1 and not res1.is_error, "first call must succeed: " .. tostring(res1 and res1.content))
            local content = tostring(res2 and res2.content or "")
            return res2 and res2.is_error == true, content, content:find("RAN_IN:", 1, true) ~= nil
            "#,
        )
        .await;
        assert!(res2_err, "second call must be marked is_error: {res2_content}");
        assert!(
            res2_content.contains("working directory changed"),
            "second call must be refused when shell cwd changed while waiting for lock: {res2_content}"
        );
        assert!(!res2_ran_in_b, "second call must not execute pwd in DIR_B: {res2_content}");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn forbidden_segment_after_mid_word_hash_or_non_analyzable_stays_deny() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(
            &env,
            true,
            json!({ "preset": "full-access", "rules": [{ "argv": ["git", "push"], "decision": "forbidden" }] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (echo_hash, hash_then_push, var_then_push, wrapped_then_push): (String, String, String, String) = eval(
            &h,
            r#"
            return VOTE(BASH("echo foo#bar")).decision,
                   VOTE(BASH("echo foo#bar && git push")).decision,
                   VOTE(BASH("echo ${foo} && git push")).decision,
                   VOTE(BASH("timeout 60 sh -c 'echo ok' && git push")).decision
            "#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(echo_hash, "allow", "echo foo#bar is analyzable in bash and allowed in full-access");
        assert_eq!(hash_then_push, "deny", "echo foo#bar && git push must deny");
        assert_eq!(var_then_push, "deny", "echo ${{foo}} && git push must deny, not downgrade to ask");
        assert_eq!(wrapped_then_push, "deny", "timeout 60 sh -c 'echo ok' && git push must deny, not downgrade to ask");
    })
    .await
}

#[tokio::test]
async fn command_v_lookup_is_allowed_while_command_sh_c_triggers_fallback() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(
            &env,
            true,
            json!({ "preset": "full-access", "rules": [{ "argv": ["git", "push"], "decision": "forbidden" }] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (cmd_v, cmd_cap_v, cmd_sh_c): (String, String, String) = eval(
            &h,
            r#"
            return VOTE(BASH("command -v sh git")).decision,
                   VOTE(BASH("command -V sh git")).decision,
                   VOTE(BASH("command sh -c \"rm -rf dist\"")).decision
            "#,
        )
        .await;
        h.shutdown().await;
        assert_eq!(cmd_v, "allow", "command -v sh git is a benign lookup, not a wrapped shell");
        assert_eq!(cmd_cap_v, "allow", "command -V sh git is a benign lookup, not a wrapped shell");
        assert_eq!(
            cmd_sh_c, "ask",
            "command sh -c \"rm -rf dist\" does not name forbidden git so it downgrades to ask"
        );
    })
    .await
}

#[tokio::test]
async fn mistyped_deny_args_rejects_rule_instead_of_allowing_bypass() {
    local(async {
        let env = TestEnv::new().await;
        // Startup: full-access + valid forbidden (`git push`) + invalid allow (`deny_args = {"-c", 0}`)
        // must fail closed on startup rather than dropping `rules` and allowing `git push`.
        let rules = json!([
            { "argv": ["git", "push"], "decision": "forbidden" },
            { "argv": ["npm"], "decision": "allow", "scope": "all", "deny_args": ["-c", 0] }
        ]);
        let b = env
            .host()
            .user_config(false)
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "full-access", "rules": rules } } }))
            .args(json!({ "no_session": true }));
        match mare_host::Host::new(b.opts).await {
            Err(_) => {}
            Ok(h) => {
                let diag: String =
                    eval(&h, "return mare.json.encode({ failures = mare.plugins.failures(), warnings = mare.config_warnings() })")
                        .await;
                assert!(!h.kernel.plugin_active("permissions"), "permissions must not start when rules are invalid: {diag}");
                h.shutdown().await;
            }
        }

        drop(env);

        // Reload: full-access + valid forbidden (`git push`), followed by a reload with an invalid
        // allow rule (`deny_args = {"-c", 0}`), must preserve the previous safe `rules` so `git push`
        // remains denied.
        let env2 = TestEnv::new().await;
        std::fs::create_dir_all(env2.config_dir()).unwrap();
        std::fs::write(
            env2.config_dir().join("init.lua"),
            r#"mare.setup { model = "faux/echo" }
mare.plugin("permissions", { preset = "full-access", rules = { { argv = { "git", "push" }, decision = "forbidden" } } })"#,
        )
        .unwrap();
        let h2 = env2.host().user_config(false).no_session().build().await;
        expose(&h2);
        exec(&h2, VOTE).await;
        std::fs::write(
            env2.config_dir().join("init.lua"),
            r#"mare.setup { model = "faux/echo" }
mare.plugin("permissions", { preset = "full-access", rules = { { argv = { "git", "push" }, decision = "forbidden" }, { argv = { "npm" }, decision = "allow", scope = "all", deny_args = { "-c", 0 } } } })"#,
        )
        .unwrap();
        h2.reload_config().await.unwrap();
        let decision: String = eval(&h2, r#"return VOTE(BASH("git push", "all", "x")).decision"#).await;
        h2.shutdown().await;
        assert_eq!(decision, "deny", "reload with mistyped deny_args must preserve the previous forbidden rule");
    })
    .await
}
