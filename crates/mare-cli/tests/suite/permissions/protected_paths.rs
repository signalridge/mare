//! Paths that stay protected: git and mare metadata, credentials, missing roots.

use super::*;
#[cfg(unix)]
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

#[cfg(unix)]
#[tokio::test]
async fn approved_git_grants_reach_real_sandbox_git_fetch_across_repos_workdir_symlinks_and_worktrees() {
    local(async {
        let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dunce::canonicalize(base.path()).unwrap();

        let git_ok = |dir: &std::path::Path, args: &[&str]| {
            let out = fixture_git(dir)
                .args(args)
                .env("GIT_AUTHOR_NAME", "Mare Test")
                .env("GIT_AUTHOR_EMAIL", "mare@example.test")
                .env("GIT_COMMITTER_NAME", "Mare Test")
                .env("GIT_COMMITTER_EMAIL", "mare@example.test")
                .output()
                .expect("git installed");
            assert!(
                out.status.success(),
                "git {:?} in {} failed: {}\n{}",
                args,
                dir.display(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).into_owned()
        };

        let make_repo_with_upstream_update = |name: &str| {
            let remote = root.join(format!("{name}-remote.git"));
            let seed = root.join(format!("{name}-seed"));
            let clone = root.join(name);
            std::fs::create_dir_all(&remote).unwrap();
            std::fs::create_dir_all(&seed).unwrap();
            git_ok(&remote, &["init", "--bare", "-q"]);
            git_ok(&seed, &["init", "-q"]);
            std::fs::write(seed.join("f.txt"), format!("{name}-v1\n")).unwrap();
            git_ok(&seed, &["add", "f.txt"]);
            git_ok(&seed, &["commit", "-q", "-m", "v1"]);
            git_ok(&seed, &["push", "-q", remote.to_str().unwrap(), "HEAD:refs/heads/main"]);
            let out = fixture_git(&root)
                .args(["clone", "-q", remote.to_str().unwrap(), clone.to_str().unwrap()])
                .output()
                .expect("git clone");
            assert!(out.status.success(), "git clone failed: {}", String::from_utf8_lossy(&out.stderr));
            // Push a second commit to the bare remote so `git fetch` must update FETCH_HEAD + refs + objects.
            std::fs::write(seed.join("f.txt"), format!("{name}-v2\n")).unwrap();
            git_ok(&seed, &["add", "f.txt"]);
            git_ok(&seed, &["commit", "-q", "-m", "v2"]);
            git_ok(&seed, &["push", "-q", remote.to_str().unwrap(), "HEAD:refs/heads/main"]);
            let head_v2 = git_ok(&seed, &["rev-parse", "HEAD"]).trim().to_string();
            (clone, seed, remote, head_v2)
        };

        let (ref_a, seed_a, remote_a, head_a_v2) = make_repo_with_upstream_update("ref-a");
        let (ref_b, _seed_b, _remote_b, head_b_v2) = make_repo_with_upstream_update("ref-b");
        let (ref_c, _seed_c, _remote_c, head_c_v2) = make_repo_with_upstream_update("ref-c");
        let ext_wt = root.join("ext-wt");
        git_ok(&ref_a, &["worktree", "add", "-q", "-b", "wt-fetch", ext_wt.to_str().unwrap()]);
        let sym_parent = root.join("ghq-link");
        std::os::unix::fs::symlink(&root, &sym_parent).unwrap();

        let ws = root.join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        git_ok(&ws, &["init", "-q"]);

        let mut env = TestEnv::new().await;
        env.cwd = ws.clone();
        let h = host(&env, true, json!({ "network": true }), json!({ "no_session": true })).await;
        if !require_backend(
            &h,
            "approved_git_grants_reach_real_sandbox_git_fetch_across_repos_workdir_symlinks_and_worktrees",
            &["bwrap", "seatbelt"],
        )
        .await
        {
            return h.shutdown().await;
        }
        global(&h, "REF_A", ref_a.to_string_lossy().to_string());
        global(&h, "REF_B", ref_b.to_string_lossy().to_string());
        global(&h, "REF_C", ref_c.to_string_lossy().to_string());
        global(&h, "EXT_WT", ext_wt.to_string_lossy().to_string());
        global(&h, "SYM_PARENT", sym_parent.to_string_lossy().to_string());

        // 1. Without approval, git fetch fails under the real OS sandbox (bwrap on Linux, Seatbelt on macOS);
        //    warm the persistent shell too.
        let (warm_ok, denied_err, denied_msg): (bool, bool, String) = eval(
            &h,
            r#"
            local warm = T:execute({ id = "warm", name = "bash", arguments = { command = "echo warm" } }, {})
            local r = T:execute({
                id = "fetch-denied",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch", REF_A) }
            }, {})
            return warm.is_error ~= true, r.is_error == true, tostring(r.content)
            "#,
        )
        .await;
        assert!(warm_ok);
        assert!(denied_err, "unapproved git fetch must fail: {denied_msg}");

        // 1b. Approving only `write:<ref-c>` (repo root, not `.git`) warns that `.git` stays
        //     read-only and `git fetch` still fails; requesting `write:<ref-c>/.git` afterwards
        //     must NOT be deduplicated away by the ancestor `<ref-c>` grant, must prompt, and
        //     once approved must lift `.git` read-only protection and write `FETCH_HEAD`.
        let (repo_only_err, repo_only_reason, git_prompted, repo_plus_git_ok, repo_plus_git_msg, after_turn_err): (
            bool,
            String,
            bool,
            bool,
            String,
            bool,
        ) = eval(
            &h,
            r#"
            local seen_reason = ""
            local r_repo = T:execute({
                id = "fetch-repo-only",
                name = "bash",
                arguments = {
                    command = string.format("git -C %q fetch", REF_C),
                    sandbox_permissions = "write:" .. REF_C,
                    justification = "write repo root only",
                }
            }, { approve = function(_, _, v)
                seen_reason = tostring(v and v.reason or "")
                return { decision = "allow" }
            end })
            -- Store ancestor repo root grant first, then separately request `<repo>/.git`.
            local r_anc = P:request({ write = { REF_C } }, {
                justification = "allow repo root first",
                scope = "turn",
                approve = function() return { decision = "allow", scope = "turn" } end,
            })
            local prompted_for_git = false
            local r_git = P:request({ write = { REF_C .. "/.git" } }, {
                justification = "now explicitly allow repo .git",
                scope = "turn",
                approve = function()
                    prompted_for_git = true
                    return { decision = "allow", scope = "turn" }
                end,
            })
            local r_fetch = T:execute({
                id = "fetch-after-repo-and-git",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch", REF_C) }
            }, {})
            -- End the turn so turn grants expire; fetching ref-c must fail again.
            C.cordis:emit("turn/completed", {})
            local r_expired = T:execute({
                id = "fetch-after-turn-expired",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch", REF_C) }
            }, {})
            return r_repo.is_error == true, seen_reason,
                   prompted_for_git,
                   r_anc.refused ~= true and r_git.refused ~= true and r_fetch.is_error ~= true,
                   tostring(r_fetch.content),
                   r_expired.is_error == true
            "#,
        )
        .await;
        assert!(repo_only_err, "write:<repo> alone must keep <repo>/.git read-only");
        assert!(
            repo_only_reason.contains(".git inside granted directory stays read-only")
                && repo_only_reason.contains("Allow once = this call only"),
            "approval reason must explain .git protection and scope/expiry: {repo_only_reason}"
        );
        assert!(git_prompted, "ancestor repo grant must NOT deduplicate away a subsequent .git write request");
        assert!(
            repo_plus_git_ok,
            "explicitly approving <repo>/.git after <repo> must lift read-only protection: {repo_plus_git_msg}"
        );
        let fetch_head_c = std::fs::read_to_string(ref_c.join(".git/FETCH_HEAD")).unwrap_or_default();
        assert!(fetch_head_c.contains(&head_c_v2), "FETCH_HEAD must record fetched commit {head_c_v2}: {fetch_head_c}");
        assert!(after_turn_err, "turn grant must expire after turn/completed so subsequent git fetch fails");

        // 2. Approve ref-a/.git and ref-b/.git via request_permissions (session scope), verify
        //    scope/expiry in both approval reason and tool result, then run real git fetch:
        //    (a) directly after persistent shell, (b) with sandbox_permissions="network",
        //    and (c) with sandbox_permissions="write:<ref-b>/.git". None may drop prior grants,
        //    and unapproved ref-c must remain denied.
        let (
            req_a_ok,
            req_b_ok,
            req_reason_has_expiry,
            req_result_has_expiry,
            direct_ok,
            direct_msg,
            net_ok,
            net_msg,
            reused_ok,
            reused_msg,
            unapproved_err,
        ): (bool, bool, bool, bool, bool, String, bool, String, bool, String, bool) = eval(
            &h,
            r#"
            local req_reason = ""
            local approve_session = function(_, _, v)
                if v and v.reason and req_reason == "" then
                    req_reason = tostring(v.reason)
                end
                return { decision = "allow_session" }
            end
            local ra = P:request({ write = { REF_A .. "/.git" } }, {
                justification = "fetch ref-a", scope = "session", approve = approve_session,
            })
            local rb = P:request({ write = { REF_B .. "/.git" } }, {
                justification = "fetch ref-b", scope = "session", approve = approve_session,
            })
            local parsed_ra = mare.json.decode(ra.content or "{}")
            local direct = T:execute({
                id = "fetch-both-direct",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch && git -C %q fetch", REF_A, REF_B) }
            }, {})
            local with_net = T:execute({
                id = "fetch-both-with-net",
                name = "bash",
                arguments = {
                    command = string.format("git -C %q fetch && git -C %q fetch", REF_A, REF_B),
                    sandbox_permissions = "network",
                    justification = "fetch both repos",
                }
            }, { approve = approve_session })
            local with_one = T:execute({
                id = "fetch-both-with-ref-b-scope",
                name = "bash",
                arguments = {
                    command = string.format("git -C %q fetch && git -C %q fetch", REF_A, REF_B),
                    sandbox_permissions = "write:" .. REF_B .. "/.git",
                    justification = "fetch both repos with one scope named",
                }
            }, { approve = approve_session })
            local unapproved = T:execute({
                id = "fetch-ref-c-unapproved",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch", REF_C) }
            }, {})
            return ra.refused ~= true and ra.is_error ~= true,
                   rb.refused ~= true and rb.is_error ~= true,
                   req_reason:find("maximum grant scope: session", 1, true) ~= nil
                       and req_reason:find("expires when this session ends", 1, true) ~= nil,
                   parsed_ra.scope == "session" and parsed_ra.expires == "session_or_policy_change",
                   direct.is_error ~= true, tostring(direct.content),
                   with_net.is_error ~= true, tostring(with_net.content),
                   with_one.is_error ~= true, tostring(with_one.content),
                   unapproved.is_error == true
            "#,
        )
        .await;
        assert!(req_a_ok && req_b_ok, "request_permissions for .git must succeed");
        assert!(req_reason_has_expiry, "request_permissions approval reason must show scope and expiry");
        assert!(req_result_has_expiry, "request_permissions tool result must report scope and expiry");
        assert!(direct_ok, "git fetch after request_permissions must succeed: {direct_msg}");
        assert!(net_ok, "sandbox_permissions='network' must preserve granted .git writes: {net_msg}");
        assert!(reused_ok, "sandbox_permissions for ref-b/.git must preserve ref-a/.git grant: {reused_msg}");
        assert!(unapproved_err, "unapproved ref-c must stay protected");
        assert_eq!(git_ok(&ref_a, &["rev-parse", "refs/remotes/origin/main"]).trim(), head_a_v2);
        assert_eq!(git_ok(&ref_b, &["rev-parse", "refs/remotes/origin/main"]).trim(), head_b_v2);
        assert!(!ref_a.join(".git/.mare").exists(), "must never create .mare inside external .git");
        assert!(!ref_b.join(".git/.mare").exists(), "must never create .mare inside external .git");

        // 3. workdir + relative `write:.git` must resolve against workdir (not mare.cwd),
        //    and workspace's own `.git` must remain protected.
        let (wd_ok, wd_msg, ws_git_still_blocked): (bool, String, bool) = eval(
            &h,
            r#"
            local r = T:execute({
                id = "fetch-workdir-rel-git",
                name = "bash",
                arguments = {
                    workdir = REF_C,
                    command = "git fetch",
                    sandbox_permissions = "write:.git",
                    justification = "fetch ref-c via workdir",
                }
            }, { approve = function() return { decision = "allow" } end })
            local ws_probe = T:execute({
                id = "ws-git-still-ro",
                name = "bash",
                arguments = {
                    workdir = REF_C,
                    command = string.format("touch %q", mare.cwd .. "/.git/escape-probe"),
                    sandbox_permissions = "write:.git",
                    justification = "verify workspace .git is not granted",
                }
            }, { approve = function() return { decision = "allow" } end })
            return r.is_error ~= true, tostring(r.content), ws_probe.is_error == true
            "#,
        )
        .await;
        assert!(wd_ok, "workdir + write:.git must grant workdir/.git: {wd_msg}");
        assert!(ws_git_still_blocked, "workdir + write:.git must not grant workspace/.git");
        assert!(!ws.join(".git/escape-probe").exists());
        assert_eq!(git_ok(&ref_c, &["rev-parse", "refs/remotes/origin/main"]).trim(), head_c_v2);

        // 4. Symlinked ancestor path in sandbox_permissions="write:<symlink>/ref-c/.git" canonicalizes cleanly.
        let (sym_ok, sym_msg): (bool, String) = eval(
            &h,
            r#"
            local r = T:execute({
                id = "fetch-symlink-ancestor",
                name = "bash",
                arguments = {
                    command = string.format("git -C %q fetch", SYM_PARENT .. "/ref-c"),
                    sandbox_permissions = "write:" .. SYM_PARENT .. "/ref-c/.git",
                    justification = "fetch through symlinked ancestor",
                }
            }, { approve = function() return { decision = "allow" } end })
            return r.is_error ~= true, tostring(r.content)
            "#,
        )
        .await;
        assert!(sym_ok, "symlinked ancestor in write:<path> must canonicalize for sandbox: {sym_msg}");

        // 4b. Approving `sandbox_permissions="write:<ref-c>/.git"` for this session merges into
        //     subsequent bash calls without repeating `sandbox_permissions`.
        let (sp_sess_ok, sp_next_ok, sp_next_msg): (bool, bool, String) = eval(
            &h,
            r#"
            local r1 = T:execute({
                id = "fetch-ref-c-sp-session",
                name = "bash",
                arguments = {
                    command = string.format("git -C %q fetch", REF_C),
                    sandbox_permissions = "write:" .. REF_C .. "/.git",
                    justification = "fetch ref-c for this session",
                }
            }, { approve = function() return { decision = "allow_session" } end })
            local r2 = T:execute({
                id = "fetch-ref-c-subsequent-no-sp",
                name = "bash",
                arguments = {
                    command = string.format("git -C %q fetch && git -C %q fetch", REF_A, REF_C),
                }
            }, {})
            return r1.is_error ~= true, r2.is_error ~= true, tostring(r2.content)
            "#,
        )
        .await;
        assert!(sp_sess_ok, "sandbox_permissions session approval must succeed");
        assert!(sp_next_ok, "session-approved sandbox_permissions must merge into later bash calls: {sp_next_msg}");

        // 5. Worktree `.git` pointer file works via `request_permissions` (including under symlinked ancestor),
        //    while a symlinked `.git` pointer file is still refused.
        std::fs::write(seed_a.join("f.txt"), "ref-a-v3\n").unwrap();
        git_ok(&seed_a, &["add", "f.txt"]);
        git_ok(&seed_a, &["commit", "-q", "-m", "v3"]);
        git_ok(&seed_a, &["push", "-q", remote_a.to_str().unwrap(), "HEAD:refs/heads/main"]);
        let head_a_v3 = git_ok(&seed_a, &["rev-parse", "HEAD"]).trim().to_string();

        let bad_sym_wt = root.join("bad-sym-wt");
        std::fs::create_dir_all(&bad_sym_wt).unwrap();
        std::os::unix::fs::symlink(ext_wt.join(".git"), bad_sym_wt.join(".git")).unwrap();
        global(&h, "BAD_SYM_WT", bad_sym_wt.to_string_lossy().to_string());

        let (wt_req_ok, wt_fetch_ok, wt_fetch_msg, bad_sym_refused): (bool, bool, String, bool) = eval(
            &h,
            r#"
            local r_bad = P:request({ write = { BAD_SYM_WT .. "/.git" } }, {
                justification = "must reject symlinked .git pointer",
                approve = function() error("must not ask for symlinked .git pointer") end,
            })
            local r_wt = P:request({ write = { SYM_PARENT .. "/ext-wt/.git" } }, {
                justification = "fetch linked worktree",
                scope = "turn",
                approve = function() return { decision = "allow", scope = "turn" } end,
            })
            local fetch = T:execute({
                id = "fetch-ext-wt",
                name = "bash",
                arguments = { command = string.format("git -C %q fetch", EXT_WT) }
            }, {})
            return r_wt.refused ~= true and r_wt.is_error ~= true,
                   fetch.is_error ~= true, tostring(fetch.content),
                   r_bad.refused == true
            "#,
        )
        .await;
        assert!(bad_sym_refused, "request_permissions must still refuse a symlinked .git pointer file");
        assert!(wt_req_ok, "request_permissions must resolve valid worktree .git pointer files");
        assert!(wt_fetch_ok, "git fetch in linked worktree after request_permissions must succeed: {wt_fetch_msg}");
        assert_eq!(git_ok(&ext_wt, &["rev-parse", "refs/remotes/origin/main"]).trim(), head_a_v3);

        // 6. Subagent under `auto` preset requesting `.git` write with turn scope:
        //    - Turn 1: next ordinary `bash` executes `git fetch` -> verifies FETCH_HEAD content;
        //      unapproved sibling `ref-b` stays non-writable.
        //    - Turn 2: after turn 1 completes, the turn grant has expired -> subsequent fetch fails.
        std::fs::write(seed_a.join("f.txt"), "ref-a-v4\n").unwrap();
        git_ok(&seed_a, &["add", "f.txt"]);
        git_ok(&seed_a, &["commit", "-q", "-m", "v4"]);
        git_ok(&seed_a, &["push", "-q", remote_a.to_str().unwrap(), "HEAD:refs/heads/main"]);
        let head_a_v4 = git_ok(&seed_a, &["rev-parse", "HEAD"]).trim().to_string();

        let sub_res: String = eval(
            &h,
            r#"
            C.cordis:get("policy"):voter("tool/approval", function(call)
                if call.arguments and call.arguments.additional_permissions then
                    return { decision = "allow", scope = "turn" }
                end
                return { decision = "deny", reason = "unexpected ask" }
            end)
            local script = {
                { tool_calls = {{ name = "request_permissions", arguments = {
                    additional_permissions = { write = { SYM_PARENT .. "/ref-a/.git" } },
                    justification = "fetch ref-a in subagent",
                    scope = "turn",
                } }} },
                { tool_calls = {{ name = "bash", arguments = {
                    command = string.format("git -C %q fetch", REF_A),
                } }} },
                { tool_calls = {{ name = "bash", arguments = {
                    command = string.format("touch %q", REF_B .. "/.git/sibling-denied-probe"),
                } }} },
                { text = "turn1-done" }
            }
            local child = mare.subagent.start({
                prompt = "fetch ref-a",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = { ["provider-faux"] = { script_json = mare.json.encode_strict(script) } },
                },
            })
            local r1 = child:wait()
            return mare.json.encode(r1)
            "#,
        )
        .await;
        assert!(
            !ref_b.join(".git/sibling-denied-probe").exists(),
            "unapproved sibling repo .git must remain non-writable during subagent turn"
        );
        let fetch_head_a_v4 = std::fs::read_to_string(ref_a.join(".git/FETCH_HEAD")).unwrap_or_default();
        assert!(
            fetch_head_a_v4.contains(&head_a_v4),
            "subagent git fetch must update FETCH_HEAD with {head_a_v4}: {fetch_head_a_v4} (res={sub_res})"
        );
        assert_eq!(
            git_ok(&ref_a, &["rev-parse", "refs/remotes/origin/main"]).trim(),
            head_a_v4,
            "subagent git fetch after request_permissions failed: {sub_res}"
        );

        // Push v5 and verify a fresh subagent turn without a new grant cannot fetch ref-a
        // (subagent turn grants never leak past their turn or to another subagent).
        std::fs::write(seed_a.join("f.txt"), "ref-a-v5\n").unwrap();
        git_ok(&seed_a, &["add", "f.txt"]);
        git_ok(&seed_a, &["commit", "-q", "-m", "v5"]);
        git_ok(&seed_a, &["push", "-q", remote_a.to_str().unwrap(), "HEAD:refs/heads/main"]);
        let head_a_v5 = git_ok(&seed_a, &["rev-parse", "HEAD"]).trim().to_string();

        let _sub_turn2: String = eval(
            &h,
            r#"
            local script2 = {
                { tool_calls = {{ name = "bash", arguments = {
                    command = string.format("git -C %q fetch", REF_A),
                } }} },
                { text = "turn2-done" }
            }
            local child2 = mare.subagent.start({
                prompt = "fetch ref-a again after turn grant expired",
                no_session = true,
                config = {
                    model = "faux/echo",
                    plugins = { ["provider-faux"] = { script_json = mare.json.encode_strict(script2) } },
                },
            })
            return mare.json.encode(child2:wait())
            "#,
        )
        .await;
        let fetch_head_after_turn2 = std::fs::read_to_string(ref_a.join(".git/FETCH_HEAD")).unwrap_or_default();
        assert!(
            !fetch_head_after_turn2.contains(&head_a_v5),
            "turn grant must expire after turn 1; turn 2 fetch must NOT update FETCH_HEAD to {head_a_v5}: {fetch_head_after_turn2}"
        );

        // 7. Nominal `full-access` preset under an active host sandbox ceiling (parent switches
        //    auto -> full-access while agent tree still enforces historical auto sandbox cap):
        //    child requests physical `.git` write -> next ordinary `bash` must NOT drop the grant
        //    via the `full-access` early return and must update FETCH_HEAD to head_a_v5.
        let sub_full_res: String = eval(
            &h,
            r#"
            P:set("full-access")
            local script = {
                { tool_calls = {{ name = "request_permissions", arguments = {
                    additional_permissions = { write = { REF_A .. "/.git" } },
                    justification = "fetch ref-a under nominal full-access with sandbox cap",
                    scope = "turn",
                } }} },
                { tool_calls = {{ name = "bash", arguments = {
                    command = string.format("git -C %q fetch", REF_A),
                } }} },
                { tool_calls = {{ name = "bash", arguments = {
                    command = string.format("touch %q", REF_B .. "/.git/full-sibling-denied"),
                } }} },
                { text = "done" }
            }
            local child = mare.subagent.start({
                prompt = "fetch ref-a under nominal full-access",
                no_session = true,
                sandbox = { level = "workspace-write", network = true },
                config = {
                    model = "faux/echo",
                    plugins = { ["provider-faux"] = { script_json = mare.json.encode_strict(script) } },
                },
            })
            return mare.json.encode(child:wait())
            "#,
        )
        .await;
        assert!(
            !ref_b.join(".git/full-sibling-denied").exists(),
            "unapproved sibling repo .git must remain non-writable under capped full-access child"
        );
        let fetch_head_a_v5 = std::fs::read_to_string(ref_a.join(".git/FETCH_HEAD")).unwrap_or_default();
        assert!(
            fetch_head_a_v5.contains(&head_a_v5),
            "nominal full-access child with active sandbox ceiling must pass granted .git to next bash and update FETCH_HEAD to {head_a_v5}: {fetch_head_a_v5} (res={sub_full_res})"
        );
        assert_eq!(
            git_ok(&ref_a, &["rev-parse", "refs/remotes/origin/main"]).trim(),
            head_a_v5,
            "nominal full-access child git fetch failed: {sub_full_res}"
        );

        h.shutdown().await;
    })
    .await
}

/// Regression tests for the three Lua permission-grant boundaries kept from review:
/// 1. Baseline `/tmp` (`wider == false`) is never promoted into an explicit host bind mount when
///    another grant is active.
/// 2. Session cache does not bypass `needs == true` when a symlinked scope is retargeted to a
///    different physical repo.
/// 3. Partial approval excluding `.git` never resurrects `R/.git` via ancestor grant `R`.
#[cfg(unix)]
#[tokio::test]
async fn permission_grant_boundaries_tmp_symlink_cache_and_partial_approval() {
    local(async {
        let base = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let root = dunce::canonicalize(base.path()).unwrap();
        let init_repo = |dir: &std::path::Path| {
            std::fs::create_dir_all(dir).unwrap();
            let out = fixture_git(dir).args(["init", "-q", "-b", "main"]).output().expect("git installed");
            assert!(out.status.success(), "git init: {}", String::from_utf8_lossy(&out.stderr));
        };

        let ws = root.join("ws");
        let repo_a = root.join("repo-a");
        let repo_b = root.join("repo-b");
        let other_dir = root.join("other");
        let link_repo = root.join("repo-link");
        init_repo(&ws);
        init_repo(&repo_a);
        init_repo(&repo_b);
        std::fs::create_dir_all(&other_dir).unwrap();
        std::os::unix::fs::symlink(&repo_a, &link_repo).unwrap();

        let mut env = TestEnv::new().await;
        env.cwd = ws.clone();
        let h = host(&env, true, json!({ "network": true }), json!({ "no_session": true })).await;
        if !require_backend(&h, "permission_grant_boundaries_tmp_symlink_cache_and_partial_approval", &["bwrap", "seatbelt"]).await {
            return h.shutdown().await;
        }
        exec(&h, VOTE).await;
        global(&h, "REPO_B", repo_b.to_string_lossy().to_string());
        global(&h, "OTHER", other_dir.to_string_lossy().to_string());
        global(&h, "LINK_REPO", link_repo.to_string_lossy().to_string());

        // 1. Baseline `/tmp` (`wider == false`) must not be promoted into explicit writable roots.
        let host_tmp_probe = std::path::PathBuf::from("/tmp").join(format!("mare-tmp-probe-{}", std::process::id()));
        let _ = std::fs::remove_file(&host_tmp_probe);
        global(&h, "TMP_PROBE", host_tmp_probe.to_string_lossy().to_string());
        let (tmp_not_promoted, mech): (bool, String) = eval(
            &h,
            r#"
            local ra = P:request({ write = { OTHER } }, {
                justification = "active grant", scope = "turn",
                approve = function() return { decision = "allow", scope = "turn" } end,
            })
            assert(ra.refused ~= true)
            local v = VOTE(BASH("touch " .. TMP_PROBE, "write:/tmp", "use temp"))
            local has_tmp = false
            for _, w in ipairs((type(v.sandbox_grant) == "table" and (v.sandbox_grant.writable or v.sandbox_grant.write)) or {}) do
                if w == "/tmp" or w == "/private/tmp" then has_tmp = true end
            end
            local r = T:execute({
                id = "tmp-no-promote", name = "bash",
                arguments = { command = "touch " .. TMP_PROBE, sandbox_permissions = "write:/tmp", justification = "temp" },
            }, {})
            assert(r.is_error ~= true, tostring(r.content))
            C.cordis:emit("turn/completed", {})
            return not has_tmp, mare.proc.sandbox.capabilities().mechanism
            "#,
        )
        .await;
        assert!(tmp_not_promoted, "baseline /tmp must not be promoted into explicit writable roots");
        if mech == "bwrap" {
            assert!(!host_tmp_probe.exists(), "bwrap must keep /tmp on private tmpfs, not host /tmp");
        }
        let _ = std::fs::remove_file(&host_tmp_probe);

        // 2. Session cache must not bypass `needs == true` when symlink ancestor retargets A -> B.
        let first_ok: bool = eval(
            &h,
            r#"
            local r = T:execute({
                id = "sym-cache-1", name = "bash",
                arguments = {
                    command = string.format("touch %q", LINK_REPO .. "/.git/cache-ok"),
                    sandbox_permissions = "write:" .. LINK_REPO .. "/.git",
                    justification = "write repo-a git",
                },
            }, { approve = function() return { decision = "allow_session" } end })
            return r.is_error ~= true
            "#,
        )
        .await;
        assert!(first_ok && repo_a.join(".git/cache-ok").exists());
        std::fs::remove_file(&link_repo).unwrap();
        std::os::unix::fs::symlink(&repo_b, &link_repo).unwrap();
        let (reprompted, refused_b): (bool, bool) = eval(
            &h,
            r#"
            local asked = false
            local r = T:execute({
                id = "sym-cache-2", name = "bash",
                arguments = {
                    command = string.format("touch %q", LINK_REPO .. "/.git/cache-ok"),
                    sandbox_permissions = "write:" .. LINK_REPO .. "/.git",
                    justification = "write repo-a git",
                },
            }, { approve = function() asked = true; return { decision = "deny" } end })
            return asked, r.refused == true
            "#,
        )
        .await;
        assert!(reprompted && refused_b, "retargeted symlink must not hit session cache when needs == true");
        assert!(!repo_b.join(".git/cache-ok").exists());

        // 3. Partial approval excluding `.git` must not resurrect `R/.git` via stored ancestor `R`.
        let (partial_json, other_ok, git_blocked): (String, bool, bool) = eval(
            &h,
            r#"
            C.cordis:emit("permission/changed", {})
            local r_anc = P:request({ write = { REPO_B } }, {
                justification = "repo-b root", scope = "turn",
                approve = function() return { decision = "allow", scope = "turn" } end,
            })
            assert(r_anc.refused ~= true)
            local r_part = P:request({ write = { REPO_B .. "/.git", OTHER } }, {
                justification = "git and other", scope = "turn",
                approve = function()
                    return { decision = "allow", scope = "turn", permissions = { write = { OTHER } } }
                end,
            })
            assert(r_part.refused ~= true)
            local r_other = T:execute({ id = "part-other", name = "bash", arguments = { command = string.format("touch %q", OTHER .. "/partial-ok") } }, {})
            local r_git = T:execute({ id = "part-git", name = "bash", arguments = { command = string.format("touch %q", REPO_B .. "/.git/partial-escape") } }, {})
            C.cordis:emit("turn/completed", {})
            return tostring(r_part.content), r_other.is_error ~= true, r_git.is_error == true
            "#,
        )
        .await;
        assert!(!partial_json.contains(".git"), "partial approval must not resurrect excluded .git: {partial_json}");
        assert!(other_ok && other_dir.join("partial-ok").exists());
        assert!(git_blocked && !repo_b.join(".git/partial-escape").exists());

        h.shutdown().await;
    })
    .await
}
