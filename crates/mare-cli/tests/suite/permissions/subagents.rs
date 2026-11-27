//! Sub-agents inherit the parent policy and can only narrow it.

use super::*;

#[tokio::test]
async fn sub_agent_cannot_widen_the_inherited_preset() {
    local(async {
        let env = TestEnv::new().await;
        let mut b = env
            .host()
            .config(json!({ "plugins": { "permissions": { "enabled": true, "preset": "full-access" } } }))
            .args(json!({ "no_session": true, "permission_ceiling": "read-only" }));
        b.opts.mode = Mode::Child;
        b.opts.project_trusted = true;
        let h = b.build().await;
        global(&h, "P", h.service_table("permissions").unwrap());
        let (preset, err): (String, String) = eval(
            &h,
            r#"local ok, e = pcall(P.set, P, "auto"); assert(not ok); return P:preset(), type(e) == "table" and tostring(e.message) or tostring(e)"#,
        )
        .await;
        assert_eq!(preset, "read-only");
        assert!(err.contains("can't widen"), "{err}");
        h.shutdown().await;
    })
    .await
}

/// Real child VMs on separate threads do not mutate the parent's or sibling VMs' policies.
#[tokio::test]
async fn concurrent_child_policies_do_not_change_parent_or_siblings() {
    local(async {
        let env = TestEnv::new().await;
        let first_dir = env.cwd.join("child-one");
        let second_dir = env.cwd.join("child-two");
        std::fs::create_dir(&first_dir).unwrap();
        std::fs::create_dir(&second_dir).unwrap();
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        global(&h, "DIR1", first_dir.to_string_lossy().to_string());
        global(&h, "DIR2", second_dir.to_string_lossy().to_string());
        exec(&h, SHELL_WRITE).await;
        let (parent, child1, child2, parent_file_denied, parent_process): (String, String, String, bool, String) = eval(
            &h,
            r#"assert(P:set("auto"))
               local script = {
                 { text = string.rep("waiting ", 20), delay_ms = 20,
                   tool_calls = {
                     { name = "write", arguments = { path = "direct.txt", content = "direct" } },
                     SHELL_WRITE,
                   } },
                 { text = "done" },
               }
               local one = mare.subagent.start({ prompt = "one", cwd = DIR1, no_session = true, permission = "read-only",
                 config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode_strict(script) }, permissions = { enabled = true } } } })
               local two = mare.subagent.start({ prompt = "two", cwd = DIR2, no_session = true, permission = "full-access",
                 permission_ceiling = "full-access",
                 config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode_strict(script) }, permissions = { enabled = false } } } })
               local a, b = one:wait(), two:wait()
               -- Tightening the parent afterward still enforces read-only on parent primitives,
               -- unaffected by child two having disabled its own permissions plugin.
               assert(P:set("read-only"))
               local file_ok = pcall(function() return mare.exec.run({ cwd = mare.cwd }, function()
                 assert(mare.fs.write(DIR1 .. "/parent-direct.txt", "bad"))
               end) end)
               local process_ok, process = pcall(function() return mare.exec.run({ cwd = mare.cwd }, function()
                 -- the platform's own shell (cmd.exe on Windows: no Git Bash needed)
                 local spec = mare.os == "windows"
                     and { cmd = "cmd.exe", args = { "/d", "/c", "echo bad> parent-shell.txt" } }
                     or { cmd = "sh", args = { "-c", "printf bad > parent-shell.txt" } }
                 spec.cwd = DIR1
                 return mare.proc.run(spec)
               end) end)
               return P:preset(), mare.json.encode(a.messages), mare.json.encode(b.messages), not file_ok,
                 process_ok and mare.json.encode(process) or tostring(process)"#,
        )
        .await;
        assert_eq!(parent, "read-only");
        assert!(parent_file_denied, "the parent's file access must be denied");
        #[cfg(any(target_os = "macos", windows))]
        {
            let result: Value = serde_json::from_str(&parent_process).expect("the parent's sandboxed shell ran");
            assert_ne!(result["code"], 0, "the parent's shell write must fail: {result}");
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = parent_process; // an unavailable Linux sandbox also fails closed
        assert!(!first_dir.join("direct.txt").exists() && !first_dir.join("shell.txt").exists(), "{child1}");
        assert!(!first_dir.join("parent-direct.txt").exists() && !first_dir.join("parent-shell.txt").exists());
        assert_eq!(std::fs::read_to_string(second_dir.join("direct.txt")).unwrap(), "direct", "{child2}");
        // Windows: the sibling's shell is PowerShell inside the default-tier sandbox, asserted under
        // the same condition as win_sandbox.rs (CI or MARE_WIN_SANDBOX_TEST): elsewhere, e.g. over
        // SSH, a sandboxed powershell.exe can die with 0xC0000142 before it runs
        let sandboxed_powershell = std::env::var("CI").is_ok() || std::env::var("MARE_WIN_SANDBOX_TEST").is_ok();
        if !cfg!(windows) || sandboxed_powershell {
            assert_eq!(std::fs::read_to_string(second_dir.join("shell.txt")).unwrap(), "shell", "{child2}");
        }
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn child_cannot_remove_parent_sandbox_by_disabling_permissions() {
    local(async {
        let env = TestEnv::new().await;
        let child_dir = env.cwd.join("child");
        std::fs::create_dir(&child_dir).unwrap();
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        global(&h, "CHILD_DIR", child_dir.to_string_lossy().to_string());
        exec(&h, SHELL_WRITE).await;
        let (parent, child): (String, String) = eval(
            &h,
            r#"assert(P:set("read-only"))
               local script = {
                 { tool_calls = {
                     { name = "write", arguments = { path = "direct.txt", content = "direct" } },
                     SHELL_WRITE,
                   } },
                 { text = "done" },
               }
               local child = mare.subagent.start({ prompt = "try writes", cwd = CHILD_DIR, no_session = true,
                 permission = "full-access", permission_ceiling = "full-access",
                 config = { model = "faux/echo", plugins = { ["provider-faux"] = { script_json = mare.json.encode_strict(script) }, permissions = { enabled = false } } } })
               local result = child:wait()
               return P:preset(), mare.json.encode(result.messages)"#,
        )
        .await;
        assert_eq!(parent, "read-only");
        assert!(!child_dir.join("direct.txt").exists() && !child_dir.join("shell.txt").exists(), "{child}");
        assert!(child.contains("sandbox") || child.contains("denied"), "the denial should be visible: {child}");
        h.shutdown().await;
    })
    .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn child_default_fs_protect_partial_does_not_widen_strict_parent_policy() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let plugin_dir = env.config_dir().join("plugins");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("partial-ceiling-probe.lua"),
            r#"
            return { name = "partial-ceiling-probe", inject = { "tools" }, apply = function(ctx)
              ctx.tools:register("spawn_probe", { description = "check inherited process policy",
                side_effect = "write", parameters = { type = "object", properties = {} },
                execute = function()
                  local ok, result = pcall(mare.proc.run, { cmd = "sh", args = { "-c", "printf child > child-ceiling-marker" } })
                  return tostring(ok) .. ":" .. tostring(result)
                end })
            end }
            "#,
        )
        .unwrap();
        let h = host(&env, true, json!({ "network": true }), json!({ "no_session": true })).await;
        let (messages, parent_is_strict): (String, bool) = eval(
            &h,
            r#"
            mare.proc.sandbox.set_current({ level = "workspace-write", network = true, accept_partial = {} })
            local current = mare.proc.sandbox.current()
            local script = {
              { tool_calls = { { name = "spawn_probe", arguments = {} } } },
              { text = "done" },
            }
            local agent = mare.subagent.start({ prompt = "write a marker", no_session = true,
              config = { model = "faux/echo", plugins = {
                ["provider-faux"] = { script_json = mare.json.encode_strict(script) },
              } } })
            local result = agent:wait()
            return mare.json.encode(result.messages), #(current.accept_partial or {}) == 0
            "#,
        )
        .await;
        assert!(parent_is_strict, "the parent policy explicitly accepts no missing capability");
        assert!(messages.contains("SANDBOX_INSUFFICIENT") && messages.contains("fs_protect"), "{messages}");
        assert!(!env.cwd.join("child-ceiling-marker").exists(), "{messages}");
        h.shutdown().await;
    })
    .await
}
