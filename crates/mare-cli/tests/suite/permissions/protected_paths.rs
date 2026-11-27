//! Paths that stay protected: git and mare metadata, credentials, missing roots.

use super::*;
#[cfg(target_os = "linux")]
use crate::suite::sandbox_expect::require_backend;

#[cfg(target_os = "linux")]
#[tokio::test]
async fn partial_protection_does_not_accept_deny_read() {
    local(async {
        for accepted in [false, true] {
            let mut env = TestEnv::new().await;
            env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
            std::fs::write(env.cwd.join("secret"), "private").unwrap();
            let h = host(&env, true, json!({ "network": true }), json!({ "no_session": true })).await;
            global(&h, "ACCEPT_READ", accepted);
            let content: String = eval(&h, r#"
              local spec = { level = "workspace-write", network = true, deny_read = { "secret" },
                accept_partial = ACCEPT_READ and { "fs_protect", "fs_deny_read" } or { "fs_protect" } }
              local ok, r = pcall(T.execute, T, { id = "combined", name = "bash", arguments = { command = "printf ok > combined-marker" } }, { sandbox = spec })
              return tostring(ok and r.content or r)
            "#).await;
            assert!(content.contains("fs_deny_read"), "{content}");
            if accepted {
                assert!(content.contains("PARTIAL") && env.cwd.join("combined-marker").exists(), "{content}");
            } else {
                assert!(!env.cwd.join("combined-marker").exists(), "{content}");
            }
            h.shutdown().await;
        }
    }).await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn changed_tool_definition_still_blocks_an_approved_call() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let h = host(&env, true, json!({ "network": true }), json!({ "no_session": true })).await;
        let (refused, ran): (bool, bool) = eval(
            &h,
            r#"
          C.cordis:get("policy"):voter("tool/before", function() return { decision = "ask", reason = "review" } end)
          local ran = false
          local r = T:execute({ id = "definition", name = "bash", arguments = { command = "printf old" } }, {
            approve = function(_, _, v)
              T:register("bash", { description = "replacement", parameters = { type = "object" },
                execute = function() ran = true; return "unexpected" end })
              return { decision = "allow_amend", amendment = v.amendment }
            end,
          })
          return r.refused == true, ran
        "#,
        )
        .await;
        assert!(refused && !ran);
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn git_and_mare_metadata_stay_read_only_until_write_git_is_granted() {
    local(async {
        let env = TestEnv::new().await;
        std::fs::create_dir_all(env.cwd.join(".git/hooks")).unwrap();
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        global(&h, "ROOT", env.cwd.to_string_lossy().to_string());
        let (git, src, mare_dir, granted, why): (bool, bool, bool, bool, String) = eval(
            &h,
            r#"local S, spec = mare.proc.sandbox, { level = "workspace-write" }
               local git, why = S.check("write", ROOT .. "/.git/config", spec)
               return git, S.check("write", ROOT .. "/src/a.txt", spec),
                      S.check("write", ROOT .. "/.mare/init.lua", spec),
                      S.check("write", ROOT .. "/.git/config", { level = "workspace-write", writable = { ROOT .. "/.git" } }),
                      tostring(why)"#,
        )
        .await;
        assert!(!git, ".git is read-only inside the workspace");
        assert!(why.contains("write:.git"), "the denial names the scope to request: {why}");
        assert!(src, "the rest of the workspace stays writable");
        assert!(!mare_dir, "an absent .mare is still protected");
        assert!(granted, "an explicit write:.git grant makes it writable");

        // the kernel profile agrees (Seatbelt / bubblewrap carve-outs), not just the policy layer
        let mech: String = eval(&h, "return mare.proc.sandbox.capabilities().mechanism").await;
        if mech == "seatbelt" || mech == "bwrap" {
            let (blocked, allowed): (i64, i64) = eval(
                &h,
                r#"local a = mare.proc.run({ cmd = "sh", args = { "-c", "echo x > .git/hooks/pre-commit" }, sandbox = { level = "workspace-write", accept_partial = { "fs_protect" } } })
                   local b = mare.proc.run({ cmd = "sh", args = { "-c", "echo x > .git/hooks/pre-commit" },
                                            sandbox = { level = "workspace-write", writable = { ROOT .. "/.git" } } })
                   return a.code, b.code"#,
            )
            .await;
            assert_ne!(blocked, 0, "a sandboxed process can't write git hooks");
            assert_eq!(allowed, 0, "with write:.git it can");
        }
        h.shutdown().await;
    })
    .await
}

/// A missing metadata-protection capability runs as partial without changing policy votes.
#[tokio::test]
async fn unavailable_protection_is_partial_without_a_separate_approval() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let h = host(
            &env,
            true,
            json!({ "network": true, "rules": [
                { "argv": ["printf", "ordinary"], "decision": "allow" },
            { "argv": ["git", "push"], "decision": "prompt" },
            { "argv": ["rm", "-rf"], "decision": "forbidden" }
        ] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        global(&h, "J", h.service_table("jobs").unwrap());
        let (allow, ask, deny): (String, String, String) = eval(
            &h,
            r#"
            return VOTE(BASH("printf ordinary")).decision, VOTE(BASH("git push")).decision,
                   VOTE(BASH("rm -rf")).decision"#,
        )
        .await;
        assert_eq!((allow.as_str(), ask.as_str(), deny.as_str()), ("allow", "ask", "deny"));
        let (content, enforcement, prompts, background, background_enforcement): (String, String, i64, String, String) =
            eval(
                &h,
                r#"
            local tool = mare.os == "windows" and "powershell" or "bash"
            local foreground = mare.os == "windows"
              and "Set-Content -NoNewline -Path partial-marker -Value ordinary"
              or "printf ordinary > partial-marker"
            local background = mare.os == "windows"
              and "Set-Content -NoNewline -Path partial-background-marker -Value background"
              or "printf background > partial-background-marker"
            local prompts = 0
            local r = T:execute({ id = "partial", name = tool, arguments = { command = foreground } }, {
              approve = function() prompts = prompts + 1; return { decision = "deny" } end,
            })
            local bg = T:execute({ id = "partial-bg", name = tool, arguments = {
              command = background, run_in_background = true
            } }, { approve = function() prompts = prompts + 1; return { decision = "deny" } end })
            assert(r.details.code == 0, tostring(r.content))
            local job = assert(J:get(bg.details.job_id))
            local done = job.handle:wait({ timeout_ms = 10000 })
            assert(done.state == "exited" and done.code == 0, tostring(bg.content))
            return tostring(r.content), tostring(r.details.sandbox.enforcement), prompts,
              tostring(bg.content), tostring(bg.details.sandbox.enforcement)"#,
            )
            .await;
        assert_eq!(enforcement, "partial");
        assert!(content.contains("PARTIAL") && content.contains("fs_protect"), "{content}");
        assert_eq!(prompts, 0, "a protection gap does not request approval");
        assert_eq!(std::fs::read_to_string(env.cwd.join("partial-marker")).unwrap(), "ordinary", "{content}");
        assert_eq!(
            std::fs::read_to_string(env.cwd.join("partial-background-marker")).unwrap(),
            "background",
            "{background}"
        );
        assert!(background.contains("PARTIAL") && background.contains("fs_protect"), "{background}");
        assert_eq!(background_enforcement, "partial", "{background}");
        h.shutdown().await;
    })
    .await
}

/// Explicitly accepting fs_protect is an informed standing choice, so no extra prompt is needed.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn explicit_protected_path_downgrade_does_not_ask_and_reports_partial() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        std::fs::create_dir(env.cwd.join(".git")).unwrap();
        let h = host(&env, true, json!({ "network": true, "accept_partial": ["fs_protect"], "rules": [
                { "argv": ["git", "push"], "decision": "prompt" }
            ] }),
            json!({ "no_session": true })).await;
        let (content, partial): (String, String) = eval(&h, r#"
            local r = T:execute({ id = "accepted", name = "bash", arguments = { command = "printf ok > accepted.txt" } }, {})
            return tostring(r.content), tostring(r.details and r.details.sandbox and r.details.sandbox.enforcement)"#).await;
        assert_eq!(std::fs::read_to_string(env.cwd.join("accepted.txt")).unwrap(), "ok", "{content}");
        assert_eq!(partial, "partial", "{content}");
        assert!(content.contains("[sandbox: PARTIAL") && content.contains("fs_protect"), "{content}");
        exec(&h, VOTE).await;
        let still_asked: String = eval(&h, r#"return VOTE(BASH("git push")).decision"#).await;
        assert_eq!(still_asked, "ask", "accepting fs_protect must not bypass other approval rules");
        h.shutdown().await;
    }).await
}

/// The Windows backend has no fs_protect; explicit acceptance must preserve other asks.
#[cfg(windows)]
#[tokio::test]
async fn windows_explicit_metadata_gap_is_partial_without_an_extra_prompt() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_HELPER", sandbox_helper_bin());
        std::fs::create_dir(env.cwd.join(".git")).unwrap();
        let h = host(
            &env,
            true,
            json!({ "network": true, "accept_partial": ["fs_protect"], "rules": [
            { "argv": ["git", "push"], "decision": "prompt" }
        ] }),
            json!({ "no_session": true }),
        )
        .await;
        let (content, partial): (String, String) = eval(
            &h,
            r#"
            local r = T:execute({ id = "accepted", name = "powershell", arguments = {
                command = "Set-Content -NoNewline -Path accepted.txt -Value ok" } }, {})
            return tostring(r.content), tostring(r.details and r.details.sandbox and r.details.sandbox.enforcement)"#,
        )
        .await;
        assert_eq!(std::fs::read_to_string(env.cwd.join("accepted.txt")).unwrap(), "ok", "{content}");
        assert_eq!(partial, "partial", "{content}");
        assert!(content.contains("[sandbox: PARTIAL") && content.contains("fs_protect"), "{content}");
        exec(&h, VOTE).await;
        let still_asked: String = eval(&h, r#"return VOTE(BASH("git push")).decision"#).await;
        assert_eq!(still_asked, "ask", "accepting fs_protect must not bypass other approval rules");
        h.shutdown().await;
    })
    .await
}

/// Ordinary asks and denies remain independent from the automatic fs_protect partial policy.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn partial_protection_does_not_bypass_asks_or_denies() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        std::fs::create_dir(env.cwd.join(".git")).unwrap();
        let h = host(
            &env,
            true,
            json!({ "network": true, "rules": [
                { "argv": ["printf", "ok"], "decision": "allow" },
                { "argv": ["git", "push"], "decision": "prompt" },
                { "argv": ["rm", "-rf"], "decision": "forbidden" }
            ] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (decision, asks, denied, denied_prompts, ordinary, enforcement): (String, i64, bool, i64, String, String) =
            eval(&h, r#"
            local asks = 0
            local gated = T:execute({ id = "ask", name = "bash", arguments = { command = "git push" } }, {
              approve = function() asks = asks + 1; return { decision = "deny" } end,
            })
            local forbidden_prompts = 0
            local forbidden = T:execute({ id = "deny", name = "bash", arguments = { command = "rm -rf" } }, {
              approve = function() forbidden_prompts = forbidden_prompts + 1; return { decision = "allow" } end,
            })
            local allowed = T:execute({ id = "ok", name = "bash", arguments = { command = "printf ok > allowed.txt" } }, {})
            return VOTE(BASH("git push")).decision, asks, gated.refused == true, forbidden_prompts,
              tostring(allowed.content), tostring(allowed.details.sandbox.enforcement)"#).await;
        assert_eq!(decision, "ask");
        assert_eq!(asks, 1, "the ordinary ask still reaches approval");
        assert!(denied, "the ask is denied");
        assert_eq!(denied_prompts, 0, "a forbidden rule still bypasses approval");
        assert_eq!(std::fs::read_to_string(env.cwd.join("allowed.txt")).unwrap(), "ok", "{ordinary}");
        assert_eq!(enforcement, "partial", "{ordinary}");
        assert!(ordinary.contains("[sandbox: PARTIAL") && ordinary.contains("fs_protect"), "{ordinary}");
        h.shutdown().await;
    }).await
}

#[tokio::test]
async fn write_git_in_a_worktree_grants_the_gitdir_it_points_to() {
    local(async {
        let env = TestEnv::new().await;
        // a worktree: `.git` is a pointer file to a valid gitdir (here inside the workspace)
        std::fs::create_dir_all(env.cwd.join(".gitstore/wt/objects")).unwrap();
        std::fs::create_dir_all(env.cwd.join(".gitstore/wt/refs")).unwrap();
        std::fs::write(env.cwd.join(".gitstore/wt/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(env.cwd.join(".git"), "gitdir: .gitstore/wt\n").unwrap();
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        global(&h, "ROOT", env.cwd.to_string_lossy().to_string());
        exec(&h, VOTE).await;
        let (protected, scope_dir): (bool, String) = eval(
            &h,
            r#"local ok = mare.proc.sandbox.check("write", ROOT .. "/.gitstore/wt/HEAD", { level = "workspace-write" })
               local v = VOTE(BASH("git commit -m x", "write:.git", "commit the change"))
               local w = type(v.sandbox_grant) == "table" and (v.sandbox_grant.writable or v.sandbox_grant.write) or {}
               return ok, tostring(w[#w])"#,
        )
        .await;
        assert!(!protected, "the gitdir a worktree points to is protected too");
        assert!(
            std::path::Path::new(&scope_dir).ends_with(".gitstore/wt"),
            "write:.git grants the gitdir: {scope_dir}"
        );
        h.shutdown().await;
    })
    .await
}

/// Unix only: the check reads POSIX mode bits (`st.mode & 0o077`, llm/auth_store.lua); Windows
/// files carry ACLs, not mode bits, so there is nothing for this check to read there.
#[cfg(unix)]
#[tokio::test]
async fn credential_files_readable_by_others_are_refused() {
    local(async {
        use std::os::unix::fs::PermissionsExt;
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({}), json!({ "no_session": true })).await;
        global(&h, "L", h.service_table("llm").unwrap());
        let auth: String = eval(&h, "return mare.paths.auth").await;
        let auth = std::path::PathBuf::from(auth);
        std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
        std::fs::write(&auth, r#"{"version": 1, "openai": {"key": "sk-test"}}"#).unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (ok, err): (bool, String) =
            eval(&h, r#"local ok, e = pcall(L.auth_get, L, "openai") return ok, tostring(e)"#).await;
        assert!(!ok && err.contains("readable by other users") && err.contains("mare doctor --fix"), "{err}");
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
        let ok: bool = eval(&h, r#"return L:auth_get("openai") ~= nil"#).await;
        assert!(ok, "0600 is fine");
        h.shutdown().await;
    })
    .await
}

#[tokio::test]
async fn a_missing_writable_root_is_reported_once_by_name() {
    local(async {
        let env = TestEnv::new().await;
        let present = env.cwd.join("present");
        std::fs::create_dir(&present).unwrap();
        let missing = env.cwd.join("not-there");
        let b = env.host().config(json!({
            "plugins": { "permissions": { "enabled": true, "writable": [present, missing] } }
        }));
        let mut opts = b.opts;
        opts.mode = Mode::Tui;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        // the notice waits for the first frame; a preset switch re-reads the roots without repeating it
        h.kernel.emit_json("app/ready", json!({})).await;
        exec(&h, r#"P:set("read-only"); P:set("auto")"#).await;
        let mut notices = vec![];
        while let Ok(cmd) = rx.try_recv() {
            if let mare_shared::ui::UiCmd::Notify { text, level, .. } = cmd
                && text.contains("Writable root")
            {
                notices.push((text, level));
            }
        }
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].0.contains(missing.to_str().unwrap()), "{notices:?}");
        assert!(!notices[0].0.contains("present"), "{notices:?}");
        assert_eq!(notices[0].1, "warning");
        let roots: Vec<String> = eval(&h, "return mare.proc.sandbox.current().roots").await;
        assert!(roots.iter().any(|r| r.ends_with("present")), "{roots:?}");
        assert!(!roots.iter().any(|r| r.ends_with("not-there")), "{roots:?}");
        h.shutdown().await;
    })
    .await
}

/// Real bubblewrap asks neither for an empty workspace nor for existing metadata.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn bubblewrap_existing_metadata_does_not_add_an_approval() {
    local(async {
        let env = TestEnv::new().await;
        let h = host(&env, true, json!({ "network": true, "rules": [
            { "argv": ["printf", "ordinary"], "decision": "allow" }
        ] }), json!({ "no_session": true })).await;
        if !require_backend(&h, "bubblewrap_existing_metadata_does_not_add_an_approval", &["bwrap"]).await { return h.shutdown().await; }
        exec(&h, VOTE).await;
        let empty: String = eval(&h, r#"return VOTE(BASH("printf ordinary")).decision"#).await;
        assert_eq!(empty, "allow");
        std::fs::create_dir(env.cwd.join(".git")).unwrap();
        let (vote, calls, enforcement): (String, i64, String) = eval(&h, r#"
            local vote = VOTE(BASH("printf ordinary")).decision
            local calls = 0
            local result = T:execute({ id = "s7-existing", name = "bash", arguments = { command = "printf ordinary" } }, {
                approve = function() calls = calls + 1; return { decision = "deny" } end,
            })
            return vote, calls, tostring(result.details and result.details.sandbox and result.details.sandbox.enforcement)
        "#).await;
        assert_eq!(vote, "allow");
        assert_eq!(calls, 0);
        assert_eq!(enforcement, "full");
        h.shutdown().await;
    }).await
}

#[cfg(target_os = "linux")]
struct CrashSandboxCleanup(std::path::PathBuf);

#[cfg(target_os = "linux")]
impl Drop for CrashSandboxCleanup {
    fn drop(&mut self) {
        use std::os::unix::ffi::OsStrExt;
        let Ok(entries) = std::fs::read_dir("/proc") else { return };
        let root = self.0.as_os_str().as_bytes();
        for entry in entries.flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Ok(args) = std::fs::read(entry.path().join("cmdline")) else { continue };
            let mut args = args.split(|b| *b == 0);
            if args.next() == Some(b"bwrap".as_slice()) && args.any(|arg| arg == root) {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
    }
}

/// An abandoned target is retained and protected as an existing read-only path.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn abandoned_target_is_protected_without_recovery_prompt() {
    use std::os::unix::process::CommandExt;

    local(async {
        let mut env = TestEnv::new().await;
        let workspace = tempfile::Builder::new().prefix("mare-s7-orphan-test-").tempdir().unwrap();
        env.cwd = dunce::canonicalize(workspace.path()).unwrap();
        let _cleanup = CrashSandboxCleanup(env.cwd.clone());
        let usable = std::process::Command::new("bwrap")
            .args([
                "--ro-bind",
                "/",
                "/",
                "--dev",
                "/dev",
                "--unshare-pid",
                "--proc",
                "/proc",
                "--die-with-parent",
                "--",
                "true",
            ])
            .status()
            .is_ok_and(|s| s.success());
        if !usable {
            return;
        }
        // The helper intentionally SIGKILLs itself, so its temporary home/work belong
        // to this parent instead of relying on the killed process's destructors.
        let temporary = env.home.path().join("tmp");
        std::fs::create_dir(&temporary).unwrap();
        let mut child = ChildGroup(std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "suite::sandbox::protection::separate_hosts_keep_concurrent_sandboxes_protected",
                "--nocapture",
            ])
            .env("MARE_TEST_SANDBOX_SHARED_WORKSPACE", &env.cwd)
            .env("MARE_TEST_SANDBOX_HOST_LABEL", "crash")
            .env("TMPDIR", &temporary).env("TEMP", &temporary).env("TMP", &temporary)
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap());
        let status = child.0.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(env.cwd.join(".mare").is_dir());
        let h = host(
            &env,
            true,
            json!({ "network": true, "rules": [
            { "argv": ["printf", "ordinary"], "decision": "allow" }
        ] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, VOTE).await;
        let (decision, enforcement, unavailable, write_code): (String, String, bool, i64) = eval(&h, r#"
            local policy = mare.proc.sandbox.policy({level = "workspace-write", network = true})
            local unavailable = false
            for _, cap in ipairs(policy.unavailable or {}) do if cap == "fs_protect" then unavailable = true end end
            local v = VOTE(BASH("printf ordinary"))
            local result = T:execute({ id = "orphan", name = "bash", arguments = { command = "printf ordinary" } }, {})
            local write = mare.proc.run({cmd = "sh", args = {"-c", "echo denied > .mare/escape"}, sandbox = policy})
            return v.decision, tostring(result.details and result.details.sandbox and result.details.sandbox.enforcement), unavailable, write.code
        "#).await;
        assert_eq!(decision, "allow");
        assert!(!unavailable, "an abandoned target remains protected");
        assert_eq!(enforcement, "full");
        assert_ne!(write_code, 0, "the preserved target must remain read-only");
        assert!(!env.cwd.join(".mare/escape").exists());
        assert!(env.cwd.join(".mare").is_dir(), "the abandoned target must remain present");
        h.shutdown().await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    })
    .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn explicit_empty_accept_partial_keeps_fs_protect_strict() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let h = host(
            &env,
            true,
            json!({ "network": true, "accept_partial": [] }),
            json!({ "no_session": true }),
        )
        .await;
        exec(&h, r#"
            N = {}
            mare.ui.connected = function() return true end
            mare.ui.notify = function(text) N[#N + 1] = tostring(text) end
        "#).await;
        h.kernel.emit_json("app/ready", json!({})).await;
        let (error, notice_count): (String, i64) = eval(&h, r#"
            local command, args
            if mare.os == "windows" then
              command, args = "cmd.exe", { "/d", "/c", "echo ran> strict-partial-marker" }
            else
              command, args = "sh", { "-c", "printf ran > strict-partial-marker" }
            end
            local ok, result = pcall(mare.proc.run, { cmd = command, args = args, sandbox = mare.proc.sandbox.current() })
            local n = 0
            for _, text in ipairs(N) do
              if text:find("Sandbox file protection is PARTIAL", 1, true) then n = n + 1 end
            end
            return tostring(ok and "unexpected success" or result), n
        "#).await;
        let partial: bool = eval(&h, "return P:is_partial()").await;
        assert!(error.contains("SANDBOX_INSUFFICIENT") && error.contains("fs_protect"), "{error}");
        assert!(!partial, "an explicit empty list remains strict");
        assert_eq!(notice_count, 0, "a refused gap is not reported as partial");
        assert!(!env.cwd.join("strict-partial-marker").exists());
        h.shutdown().await;
    }).await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn protection_partial_warns_once_on_session_start_and_no_plugin_stays_quiet() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let b = env.host().config(json!({ "plugins": { "permissions": { "enabled": true, "network": true } } }));
        let mut opts = b.opts;
        opts.project_trusted = true;
        opts.mode = mare_shared::ui::Mode::Tui;
        let ui = mare_shared::ui::UiShared::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        *ui.tx.write().unwrap() = Some(tx);
        let h = mare_host::Host::with_ui(opts, ui).await.unwrap();
        expose(&h);
        global(&h, "P", h.service_table("permissions").unwrap());
        exec(
            &h,
            r#"
            N = {}
            mare.ui.connected = function() return true end
            mare.ui.notify = function(text) N[#N + 1] = tostring(text) end
        "#,
        )
        .await;
        h.kernel.emit_json("app/ready", json!({})).await;
        let notices: i64 = eval(
            &h,
            r#"
            local function count()
              local n = 0
              for _, text in ipairs(N) do
                if text:find("Sandbox file protection is PARTIAL", 1, true) then n = n + 1 end
              end
              return n
            end
            P:set("read-only")
            P:set("auto")
            return count()
        "#,
        )
        .await;
        assert_eq!(notices, 1, "preset changes do not repeat the session warning");
        h.shutdown().await;
        drop(env);

        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let mut b = env.host().no_session().config(json!({
            "plugins": { "permissions": { "enabled": false } }
        }));
        b.opts.project_trusted = true;
        let h = b.build().await;
        expose(&h);
        exec(
            &h,
            r#"
            PARTIAL_COUNT = 0
            mare.ui.connected = function() return true end
            mare.ui.notify = function(text)
              if tostring(text):find("Sandbox file protection is PARTIAL", 1, true) then PARTIAL_COUNT = PARTIAL_COUNT + 1 end
            end
        "#,
        )
        .await;
        h.kernel.emit_json("app/ready", json!({})).await;
        let notices: i64 = eval(&h, "return PARTIAL_COUNT").await;
        assert_eq!(notices, 0, "the core does not emit permission-plugin notices by itself");
        h.shutdown().await;
    })
    .await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn rule_change_during_approval_rechecks_the_call_against_the_new_rule() {
    local(async {
        let mut env = TestEnv::new().await;
        env.set_var("MARE_SANDBOX_CAPS", "fs_read,fs_write,net_block,net_proxy,unix_socket");
        let h = host(&env, true, json!({ "network": true }), json!({ "no_session": true })).await;
        exec(&h, VOTE).await;
        let (asks, refused, decision): (i64, bool, String) = eval(&h, r#"
            C.cordis:get("policy"):voter("tool/before", function(call)
              if call.name == "bash" then return { decision = "ask", reason = "review this command" } end
            end)
            local asks = 0
            local result = T:execute({ id = "rule-change", name = "bash", arguments = { command = "touch changed-rule-marker" } }, {
              approve = function()
                asks = asks + 1
                P:add_rules({ { argv = { "touch" }, decision = "forbidden" } })
                return { decision = "allow" }
              end,
            })
            return asks, result.refused == true, VOTE(BASH("touch another-marker")).decision
        "#).await;
        assert_eq!(asks, 1, "the original prompt was shown once");
        assert!(refused, "the in-flight approval is void after the rule changes");
        assert_eq!(decision, "deny");
        assert!(!env.cwd.join("changed-rule-marker").exists());
        h.shutdown().await;
    }).await
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linked_worktree_write_git_grants_commondir_for_real_sandbox_commit_and_rejects_escapes() {
    use std::process::Command;

    local(async {
        let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dunce::canonicalize(base.path()).unwrap();
        let main_repo = root.join("main-repo");
        let worktree = root.join("linked-wt");
        std::fs::create_dir_all(&main_repo).unwrap();

        let git_ok = |dir: &std::path::Path, args: &[&str]| {
            let out = Command::new("git")
                .args(["-C", dir.to_str().unwrap()])
                .args(args)
                .env("GIT_AUTHOR_NAME", "Mare Test")
                .env("GIT_AUTHOR_EMAIL", "mare@example.test")
                .env("GIT_COMMITTER_NAME", "Mare Test")
                .env("GIT_COMMITTER_EMAIL", "mare@example.test")
                .output()
                .expect("git installed");
            assert!(
                out.status.success(),
                "git {:?} failed: {}\n{}",
                args,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        git_ok(&main_repo, &["init", "-q"]);
        std::fs::write(main_repo.join("tracked.txt"), "v1\n").unwrap();
        git_ok(&main_repo, &["add", "tracked.txt"]);
        git_ok(&main_repo, &["commit", "-q", "-m", "initial commit"]);
        git_ok(&main_repo, &["worktree", "add", "-q", "-b", "wt-branch", worktree.to_str().unwrap()]);

        let mut env = TestEnv::new().await;
        env.cwd = worktree.clone();
        let h = host(&env, true, json!({ "network": true }), json!({ "no_session": true })).await;
        if !require_backend(
            &h,
            "linked_worktree_write_git_grants_commondir_for_real_sandbox_commit_and_rejects_escapes",
            &["bwrap"],
        )
        .await
        {
            return h.shutdown().await;
        }
        exec(&h, VOTE).await;
        global(&h, "MAIN_OUTSIDE", main_repo.join("outside.txt").to_string_lossy().to_string());
        global(&h, "WT_DOT_GIT", worktree.join(".git").to_string_lossy().to_string());

        // Stage a file inside the linked worktree.
        std::fs::write(worktree.join("wt-file.txt"), "worktree change\n").unwrap();
        git_ok(&worktree, &["add", "wt-file.txt"]);

        // 1. Without `write:.git`, git commit inside the linked worktree fails under sandbox confinement.
        let before_count = git_ok(&worktree, &["rev-list", "--count", "HEAD"]).trim().to_string();
        assert_eq!(before_count, "1");
        let (denied_err, denied_msg): (bool, String) = eval(
            &h,
            r#"
            local r = T:execute({
                id = "commit-no-scope",
                name = "bash",
                arguments = {
                    command = "git -c user.name=Mare -c user.email=mare@example.test commit -q -m denied"
                }
            }, {})
            return r.is_error == true, tostring(r.content)
            "#,
        )
        .await;
        assert!(denied_err, "git commit without write:.git must fail: {denied_msg}");
        assert_eq!(git_ok(&worktree, &["rev-list", "--count", "HEAD"]).trim(), "1");

        // 2. With approved `write:.git`, git commit succeeds inside the real bwrap sandbox!
        //    Meanwhile, writing to main_repo/outside.txt or wt/.git pointer file remains strictly denied!
        let (commit_err, commit_msg, outside_err, outside_msg, dotgit_err, dotgit_msg): (
            bool,
            String,
            bool,
            String,
            bool,
            String,
        ) = eval(
            &h,
            r#"
            local approve_git = function()
                return { decision = "allow" }
            end
            local r1 = T:execute({
                id = "commit-with-scope",
                name = "bash",
                arguments = {
                    command = "git -c user.name=Mare -c user.email=mare@example.test commit -q -m 'worktree commit'",
                    sandbox_permissions = "write:.git",
                    justification = "commit worktree changes"
                }
            }, { approve = approve_git })
            local r2 = T:execute({
                id = "outside-write",
                name = "bash",
                arguments = {
                    command = string.format("printf bad > %q", MAIN_OUTSIDE),
                    sandbox_permissions = "write:.git",
                    justification = "test confinement"
                }
            }, { approve = approve_git })
            local r3 = T:execute({
                id = "dotgit-ptr-write",
                name = "bash",
                arguments = {
                    command = string.format("printf evil > %q", WT_DOT_GIT),
                    sandbox_permissions = "write:.git",
                    justification = "test pointer confinement"
                }
            }, { approve = approve_git })
            return r1.is_error == true, tostring(r1.content),
                   r2.is_error == true, tostring(r2.content),
                   r3.is_error == true, tostring(r3.content)
            "#,
        )
        .await;
        assert!(!commit_err, "git commit with write:.git must succeed: {commit_msg}");
        assert_eq!(git_ok(&worktree, &["rev-list", "--count", "HEAD"]).trim(), "2");
        assert_eq!(git_ok(&main_repo, &["log", "-1", "--pretty=%s", "wt-branch"]).trim(), "worktree commit");
        assert!(outside_err, "main repo outside .git must stay non-writable: {outside_msg}");
        assert!(!main_repo.join("outside.txt").exists());
        assert!(dotgit_err, "worktree .git pointer file must stay protected: {dotgit_msg}");

        // 3. Read-only preset keeps workspace non-writable even when write:.git is evaluated.
        let (ro_err, ro_msg): (bool, String) = eval(
            &h,
            r#"
            P:set("read-only")
            local r = T:execute({
                id = "ro-ws-write",
                name = "bash",
                arguments = {
                    command = "printf bad > ro-escape.txt",
                    sandbox_permissions = "write:.git",
                    justification = "test read-only"
                }
            }, { approve = function() return { decision = "allow" } end })
            return r.is_error == true, tostring(r.content)
            "#,
        )
        .await;
        assert!(ro_err, "read-only workspace write must stay denied: {ro_msg}");
        assert!(!worktree.join("ro-escape.txt").exists());
        h.shutdown().await;
        drop(env);

        // 4. Malicious / symlinked commondir and symlinked .git pointer rejections.
        let victim_repo = root.join("victim-repo");
        std::fs::create_dir_all(&victim_repo).unwrap();
        git_ok(&victim_repo, &["init", "-q"]);
        std::fs::write(victim_repo.join("v.txt"), "v\n").unwrap();
        git_ok(&victim_repo, &["add", "v.txt"]);
        git_ok(&victim_repo, &["commit", "-q", "-m", "victim"]);

        let evil_dir = root.join("evil-escape");
        std::fs::create_dir_all(&evil_dir).unwrap();

        let bad_cases = ["traversal_outside_git", "unrelated_git_repo", "symlink_commondir", "symlink_dotgit"];
        for case in bad_cases {
            let mut env_bad = TestEnv::new().await;
            let case_ws = root.join(format!("bad-{case}"));
            std::fs::create_dir_all(&case_ws).unwrap();
            let wt_meta = case_ws.join(".gitstore/worktrees/wt1");
            std::fs::create_dir_all(&wt_meta).unwrap();
            std::fs::write(wt_meta.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(wt_meta.join("gitdir"), format!("{}\n", case_ws.join(".git").display())).unwrap();
            match case {
                "traversal_outside_git" => {
                    std::fs::write(case_ws.join(".git"), format!("gitdir: {}\n", wt_meta.display())).unwrap();
                    std::fs::write(wt_meta.join("commondir"), format!("{}\n", evil_dir.display())).unwrap();
                }
                "unrelated_git_repo" => {
                    std::fs::write(case_ws.join(".git"), format!("gitdir: {}\n", wt_meta.display())).unwrap();
                    std::fs::write(wt_meta.join("commondir"), format!("{}\n", victim_repo.join(".git").display()))
                        .unwrap();
                }
                "symlink_commondir" => {
                    let sym_target = root.join("sym-commondir-target");
                    std::fs::write(&sym_target, "../..\n").unwrap();
                    std::fs::write(case_ws.join(".git"), format!("gitdir: {}\n", wt_meta.display())).unwrap();
                    std::os::unix::fs::symlink(&sym_target, wt_meta.join("commondir")).unwrap();
                }
                "symlink_dotgit" => {
                    let sym_git = root.join("sym-git-file");
                    std::fs::write(&sym_git, format!("gitdir: {}\n", wt_meta.display())).unwrap();
                    std::os::unix::fs::symlink(&sym_git, case_ws.join(".git")).unwrap();
                }
                _ => unreachable!(),
            }
            env_bad.cwd = case_ws.clone();
            let hb = host(&env_bad, true, json!({ "network": true }), json!({ "no_session": true })).await;
            exec(&hb, VOTE).await;
            let (bad_err, bad_msg): (bool, String) = eval(
                &hb,
                r#"
                local r = T:execute({
                    id = "bad-git-scope",
                    name = "bash",
                    arguments = {
                        command = "true",
                        sandbox_permissions = "write:.git",
                        justification = "bad"
                    }
                }, { approve = function() return { decision = "allow" } end })
                return r.is_error == true, tostring(r.content)
                "#,
            )
            .await;
            assert!(bad_err, "{case} must reject unsafe write:.git scope: {bad_msg}");
            hb.shutdown().await;
            drop(env_bad);
        }

        // 5. Symlinked gitdir, non-git gitdir without commondir, and SHA-256 64-hex detached HEAD.
        let mut env_sha = TestEnv::new().await;
        let sha_ws = root.join("sha256-standalone-wt");
        let sha_gitdir = sha_ws.join(".gitstore/standalone");
        std::fs::create_dir_all(sha_gitdir.join("objects")).unwrap();
        std::fs::create_dir_all(sha_gitdir.join("refs")).unwrap();
        std::fs::write(sha_gitdir.join("HEAD"), format!("{}\n", "a".repeat(64))).unwrap();
        std::fs::write(sha_ws.join(".git"), format!("gitdir: {}\n", sha_gitdir.display())).unwrap();
        env_sha.cwd = sha_ws.clone();
        let hs = host(&env_sha, true, json!({ "network": true }), json!({ "no_session": true })).await;
        exec(&hs, VOTE).await;
        let (sha_ok, sha_scope): (bool, String) = eval(
            &hs,
            r#"
            local v = VOTE(BASH("git status", "write:.git", "inspect standalone sha256 repo"))
            local w = type(v.sandbox_grant) == "table" and (v.sandbox_grant.writable or v.sandbox_grant.write) or {}
            return v.decision == "ask", tostring(w[#w])
            "#,
        )
        .await;
        assert!(sha_ok && std::path::Path::new(&sha_scope).ends_with(".gitstore/standalone"), "{sha_scope}");
        hs.shutdown().await;
        drop(env_sha);

        for strict_case in ["symlink_gitdir", "non_git_gitdir"] {
            let mut env_s = TestEnv::new().await;
            let case_ws = root.join(format!("strict-{strict_case}"));
            std::fs::create_dir_all(&case_ws).unwrap();
            match strict_case {
                "symlink_gitdir" => {
                    let target_gd = root.join("real-gd-target");
                    std::fs::create_dir_all(target_gd.join("objects")).unwrap();
                    std::fs::create_dir_all(target_gd.join("refs")).unwrap();
                    std::fs::write(target_gd.join("HEAD"), "ref: refs/heads/main\n").unwrap();
                    let sym_gd = case_ws.join("sym-gd");
                    std::os::unix::fs::symlink(&target_gd, &sym_gd).unwrap();
                    std::fs::write(case_ws.join(".git"), format!("gitdir: {}\n", sym_gd.display())).unwrap();
                }
                "non_git_gitdir" => {
                    let not_git = case_ws.join("plain-dir");
                    std::fs::create_dir_all(&not_git).unwrap();
                    std::fs::write(case_ws.join(".git"), format!("gitdir: {}\n", not_git.display())).unwrap();
                }
                _ => unreachable!(),
            }
            env_s.cwd = case_ws.clone();
            let hst = host(&env_s, true, json!({ "network": true }), json!({ "no_session": true })).await;
            let (strict_err, strict_msg): (bool, String) = eval(
                &hst,
                r#"
                local r = T:execute({
                    id = "strict-git-scope",
                    name = "bash",
                    arguments = {
                        command = "true",
                        sandbox_permissions = "write:.git",
                        justification = "reject"
                    }
                }, { approve = function() return { decision = "allow" } end })
                return r.is_error == true, tostring(r.content)
                "#,
            )
            .await;
            assert!(strict_err, "{strict_case} must reject unsafe write:.git scope: {strict_msg}");
            hst.shutdown().await;
            drop(env_s);
        }
    })
    .await
}
